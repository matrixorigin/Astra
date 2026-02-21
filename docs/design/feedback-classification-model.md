# Feedback Classification Model — Design

> **Status**: Design — native Agent OS capability for implicit feedback detection  
> **Last Updated**: 2026-02-21  
> **Dependencies**: implicit_feedback.py (heuristic layer), llm_feedback table, training data pipeline

---

## 1. Motivation

Users rarely give explicit feedback. But their next message is rich with implicit signals:

- "不对，我要的不是这个" → correction (rating: 1)
- "太啰嗦了，说重点" → frustration (rating: 1)
- "我再说一遍" → rephrasing (rating: 2)
- "具体一点" → clarification (rating: 3)
- "谢谢" → positive (rating: 5)

**Current state**: Regex heuristics catch ~70% of high-confidence signals. LLM classification (GPT-4o-mini / DeepSeek) catches ~80%. Both have limitations:
- Heuristics miss nuanced dissatisfaction ("嗯...那换个方式试试？")
- LLM classification costs money and adds latency per turn

**Goal**: A small, fast, cheap model that runs locally and achieves >90% accuracy.

---

## 2. Why This Is a Native Agent OS Capability

The platform already has every piece of the pipeline:

| Component | Status | Role |
|-----------|--------|------|
| `conversation_events` | ✅ Exists | Raw conversation data with causal chains |
| `llm_feedback` | ✅ Exists | Labeled feedback (explicit + implicit) |
| `context_snapshots` | ✅ Exists | Full context at each decision point |
| Heuristic detector | ✅ Implemented | Auto-labels high-confidence cases |
| `/rate` command | ✅ Implemented | Explicit user labels |
| `mo-admin prompt mine-feedback --use-llm` | ✅ Implemented | LLM teacher labels |
| Training data pipeline | ✅ Designed (§6 eval-and-evolution) | Versioned datasets with lineage |
| Regression gate | ✅ Implemented | Validates model changes before deployment |
| Skill registry | ✅ Implemented | Deploy model as a platform skill |

No external infrastructure needed. The model trains on platform data, deploys as a platform skill, and improves from platform feedback.

---

## 3. Data Pipeline

### 3.1 Label Sources (Teacher Ensemble)

```
┌─────────────────────────────────────────────────────────────┐
│  Source 1: Heuristic Detector (high-confidence only)        │
│  - Regex patterns for CN+EN                                 │
│  - confidence ≥ 0.7 → auto-label                            │
│  - Volume: every conversation turn, zero cost               │
├─────────────────────────────────────────────────────────────┤
│  Source 2: Explicit User Feedback                            │
│  - /rate N [comment] in chat                                │
│  - Highest quality, lowest volume                            │
│  - Maps: rating 1-2 → negative, 4-5 → positive              │
├─────────────────────────────────────────────────────────────┤
│  Source 3: LLM Teacher (batch, on-demand)                    │
│  - mo-admin prompt mine-feedback --use-llm                   │
│  - Classifies ambiguous cases heuristic couldn't resolve     │
│  - Highest coverage, moderate cost                           │
├─────────────────────────────────────────────────────────────┤
│  Source 4: Platform-Internal Signals                         │
│  - Session abandonment (user exits after agent response)     │
│  - Immediate rephrasing (same intent, different words)       │
│  - Response regeneration requests                            │
│  - Tool call failures followed by user correction            │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 Training Data Format

```json
{
  "id": "feedback-00001",
  "user_query": "帮我写个排序函数",
  "agent_response": "排序有很多种方法，比如冒泡排序、快速排序...(500字)",
  "user_followup": "太啰嗦了，直接给代码",
  "label": "frustration",
  "confidence": 0.95,
  "source": "heuristic",
  "session_id": "sess-xxx",
  "timestamp": "2026-02-21T10:00:00Z"
}
```

### 3.3 Data Accumulation Milestones

| Phase | Volume | Action |
|-------|--------|--------|
| Cold start | 0-500 | Heuristic + LLM teacher only |
| Minimum viable | 500-2K | Can train BERT-base, expect ~80% accuracy |
| Production ready | 2K-10K | Fine-tune with confidence, expect ~90% |
| Mature | 10K+ | Continuous learning, domain adaptation |

### 3.4 Data Export

```bash
# Export training data from llm_feedback table
mo-admin feedback export --output train.jsonl --min-confidence 0.7

# Output format (JSONL):
{"query": "...", "response": "...", "followup": "...", "label": "correction", "weight": 1.0}
{"query": "...", "response": "...", "followup": "...", "label": "positive", "weight": 0.8}

# Weight by source quality:
# - LLM teacher: 1.0
# - Explicit /rate: 0.9
# - Heuristic: 0.7
# - Platform signals: 0.6
```

### 3.4 Data Quality Controls

- **Deduplication**: Same (query, followup) pair from same session counted once
- **Label agreement**: When heuristic and LLM disagree, flag for review or use LLM label
- **Confidence weighting**: Training loss weighted by label confidence
- **Temporal split**: Train on older data, validate on recent (no future leakage)
- **Versioning**: Every training dataset is a versioned snapshot in the training data pipeline

---

## 4. Model Architecture

### 4.1 Recommended: Encoder-based Classifier

```
Input: [CLS] user_query [SEP] agent_response_truncated [SEP] user_followup [CLS]
  │
  ▼
Encoder (BERT-base-multilingual / chinese-roberta-base)
  │
  ▼
Classification Head (6-class: correction, frustration, rephrasing, clarification, positive, neutral)
  │
  ▼
Output: {label, confidence}
```

**Why encoder, not decoder (LLM)?**
- 10-50x faster inference (~5ms vs ~200ms)
- 100x smaller (110M vs 7B+)
- Classification is a discriminative task — encoders excel here
- Multilingual BERT handles CN+EN natively

### 4.2 Alternative: Distilled Small LLM (1-3B)

If encoder accuracy is insufficient:
- Qwen2-1.5B or similar small LLM
- LoRA fine-tune on classification task
- Still fast enough for inline use (~30ms)
- Better at nuanced cases due to generative understanding

### 4.3 Model Selection Criteria

| Criterion | Encoder (BERT) | Small LLM (1-3B) |
|-----------|---------------|-------------------|
| Latency | <10ms | 20-50ms |
| Memory | ~500MB | 2-6GB |
| Training data needed | 2K+ | 5K+ |
| Nuance handling | Moderate | Good |
| Deployment complexity | Low (ONNX) | Moderate (vLLM/llama.cpp) |

**Recommendation**: Start with BERT-base-multilingual. Upgrade to small LLM only if accuracy plateaus below 85%.

---

## 5. Training Pipeline

### 5.1 Skill-Based Training

Training is implemented as a **platform skill** (`FeedbackTrainerSkill`) to leverage existing infrastructure:

```python
# skills/feedback_trainer/skill.py
class FeedbackTrainerSkill(BaseSkill):
    skill_id = "feedback_trainer"
    
    requirements = SkillRequirement(
        conda_env="agent-engine-train",  # Isolated heavy deps
        gpu_required=True,
        min_memory_gb=8,
        packages=["torch>=2.1.0", "transformers>=4.36.0", "accelerate>=0.25.0"]
    )
    
    async def execute(
        self,
        dataset_id: str,
        base_model: str = "bert-base-multilingual-cased",
        epochs: int = 3,
        batch_size: int = 16,
        learning_rate: float = 2e-5,
        export_onnx: bool = True
    ) -> dict:
        """
        Train feedback classifier from llm_feedback data.
        
        Returns:
            {
                "artifact_id": "art_abc123",
                "metrics": {"accuracy": 0.87, "f1": 0.85},
                "onnx_path": "~/.mo-agent/models/art_abc123.onnx"
            }
        """
```

**Why as a skill?**
- Reuses skill registry, versioning, audit trail
- Executor handles conda env switching automatically
- GPU scheduling via skill requirements
- Regression gate validates before deployment

### 5.2 Environment Isolation

**Problem**: Training needs PyTorch (2GB+), but inference only needs ONNX Runtime (50MB). Other skills shouldn't be forced to install ML deps.

**Solution**: Separate conda environments

```yaml
# environment-train.yml (training only)
name: agent-engine-train
dependencies:
  - pytorch::pytorch=2.1.0
  - pytorch::pytorch-cuda=12.1
  - transformers=4.36.0
  - datasets=2.16.0
  - accelerate=0.25.0
  - onnx=1.15.0

# environment.yml (base, inference)
name: agent-engine
dependencies:
  - python=3.11
  - onnxruntime=1.16.0  # Lightweight (50MB)
  - numpy=1.24.0
  # NO torch, transformers training deps
```

**Executor behavior**:
```python
# core/agent/executor.py
class SkillExecutor:
    async def execute_skill(self, skill_id: str, inputs: dict):
        skill = self.registry.get(skill_id)
        
        if skill.requirements.conda_env:
            # Switch to training env for this skill only
            result = await self._execute_in_env(
                skill, inputs, env=skill.requirements.conda_env
            )
        else:
            # Run in current env
            result = await skill.execute(**inputs)
```

### 5.3 Data Export

```sql
-- Extract labeled conversation triples for training
SELECT
    e_user.content AS user_query,
    e_agent.content AS agent_response,
    e_followup.content AS user_followup,
    f.rating,
    f.comment AS label_detail,
    JSON_EXTRACT(f.metadata, '$.signal_type') AS label,
    JSON_EXTRACT(f.metadata, '$.confidence') AS label_confidence,
    JSON_EXTRACT(f.metadata, '$.source') AS label_source
FROM llm_feedback f
JOIN context_snapshots cs ON f.llm_request_id = cs.llm_request_id
JOIN conversation_events e_user ON cs.event_id = e_user.event_id
LEFT JOIN conversation_events e_agent
    ON e_agent.parent_event_id = e_user.event_id
    AND e_agent.event_type = 'llm_response'
LEFT JOIN conversation_events e_followup
    ON e_followup.session_id = e_user.session_id
    AND e_followup.event_type = 'user_query'
    AND e_followup.created_at > e_agent.created_at
WHERE JSON_EXTRACT(f.metadata, '$.confidence') >= 0.7
ORDER BY f.created_at DESC;
```

### 5.4 GPU Scheduling

**Development**: Local GPU (if available)
```python
# core/agent/executor.py
class SkillExecutor:
    def _select_device(self, skill: BaseSkill) -> str:
        if not skill.requirements.gpu_required:
            return "cpu"
        
        try:
            import torch
            if torch.cuda.is_available():
                return "cuda:0"
        except ImportError:
            pass
        
        logger.warning("GPU required but not available, using CPU (slow)")
        return "cpu"
```

**Production**: Remote GPU cluster (Ray / Kubernetes)
```python
# skills/feedback_trainer/remote_executor.py
class RemoteGPUExecutor:
    """Offload training to remote GPU cluster"""
    
    async def execute(self, skill_id: str, inputs: dict):
        # Submit job to Ray cluster
        job_id = await self.ray_client.submit(
            skill_id=skill_id,
            inputs=inputs,
            resources={"num_gpus": 1, "memory": "8GB"}
        )
        
        # Poll for completion
        result = await self._wait_for_job(job_id, timeout=3600)
        return result
```

### 5.5 Distributed Training (Future)

For large datasets (>100K samples):
```python
# skills/feedback_trainer/distributed.py
from accelerate import Accelerator

class DistributedTrainer:
    def __init__(self):
        self.accelerator = Accelerator()  # Auto-detects multi-GPU/TPU
    
    def train(self, model, train_loader):
        model, train_loader = self.accelerator.prepare(model, train_loader)
        
        for batch in train_loader:
            outputs = model(**batch)
            loss = outputs.loss
            self.accelerator.backward(loss)  # Automatic DDP
```

### 5.6 ONNX Export (Lightweight Inference)

```python
# Export trained model to ONNX for fast CPU inference
def export_onnx(model, tokenizer, output_path: str):
    dummy_input = tokenizer(
        "帮我写个函数", "这是一个很长的回复...", "太啰嗦了",
        return_tensors="pt", padding=True, truncation=True
    )
    
    torch.onnx.export(
        model,
        (dummy_input["input_ids"], dummy_input["attention_mask"]),
        output_path,
        input_names=["input_ids", "attention_mask"],
        output_names=["logits"],
        dynamic_axes={
            "input_ids": {0: "batch", 1: "sequence"},
            "attention_mask": {0: "batch", 1: "sequence"}
        },
        opset_version=14
    )
```

**Why ONNX?**
- 10x faster inference than PyTorch on CPU
- 50MB runtime vs 2GB PyTorch
- Cross-platform (Linux/Mac/Windows)
- No GPU needed for inference
        text = f"{example['user_query']} [SEP] {example['agent_response'][:256]} [SEP] {example['user_followup']}"
        return tokenizer(text, truncation=True, max_length=512)

    # ... standard HuggingFace Trainer setup
    # Weighted loss by label_confidence
    # Temporal train/val split
```

### 5.3 Evaluation

- **Offline**: Precision/Recall/F1 per class on held-out set
- **Online**: A/B test against heuristic layer — compare feedback quality fed to PromptOptimizer
- **Regression gate**: Before deploying new model version, replay historical conversations and verify no regression

---

## 6. Deployment as Platform Skill

### 6.1 Inference Skill

```python
# skills/feedback_classifier/skill.py
class FeedbackClassifierSkill(BaseSkill):
    """Classify implicit feedback from conversation turns."""
    skill_id = "feedback_classifier"
    
    requirements = SkillRequirement(
        optional_packages=["onnxruntime>=1.16.0", "numpy>=1.24.0"],
        fallback_mode="heuristic"  # Degrade to regex if deps missing
    )
    
    def __init__(self):
        self.engine = None  # Lazy init on first call
        self.batch_queue = asyncio.Queue()
        self.batch_processor = None
    
    async def execute(
        self,
        user_query: str,
        agent_response: str,
        followup_query: str
    ) -> dict:
        """
        Returns:
            {
                "signal_type": "correction" | "frustration" | ...,
                "confidence": 0.89,
                "reasoning": "User explicitly corrected agent's output"
            }
        """
        if self.engine is None:
            self._init_engine()
        
        # Async batch inference for efficiency
        result = await self._infer_async(user_query, agent_response, followup_query)
        return result
```

### 6.2 Lazy Loading + Fallback

```python
# skills/feedback_classifier/inference.py
class ONNXInferenceEngine:
    def __init__(self):
        self._session = None
        self._tokenizer = None
    
    def _ensure_loaded(self):
        if self._session is not None:
            return
        
        try:
            import onnxruntime as ort
            from transformers import AutoTokenizer
        except ImportError as e:
            raise SkillDependencyError(
                f"Missing {e.name}. Install: pip install onnxruntime transformers"
            )
        
        # Load active model from model_artifacts table
        artifact = self.db.execute(
            "SELECT storage_path FROM model_artifacts "
            "WHERE skill_id = 'feedback_classifier' AND is_active = TRUE"
        ).fetchone()
        
        if not artifact:
            raise SkillError("No active model found")
        
        model_path = self._download_if_remote(artifact["storage_path"])
        self._session = ort.InferenceSession(model_path)
        self._tokenizer = AutoTokenizer.from_pretrained("bert-base-multilingual-cased")
```

### 6.3 Batch Inference Optimization

**Problem**: Each chat turn calls classifier → 1 inference/turn → underutilizes CPU/GPU

**Solution**: Async batch queue

```python
# skills/feedback_classifier/batch_processor.py
class BatchProcessor:
    def __init__(self, engine, max_batch_size=32, max_wait_ms=50):
        self.engine = engine
        self.max_batch_size = max_batch_size
        self.max_wait_ms = max_wait_ms
        self.queue = asyncio.Queue()
    
    async def infer(self, inputs: dict) -> dict:
        """Submit inference request, returns when batch completes"""
        future = asyncio.Future()
        await self.queue.put((inputs, future))
        return await future
    
    async def _batch_loop(self):
        """Background task: collect requests, batch infer, resolve futures"""
        while True:
            batch = []
            futures = []
            
            # Collect batch with timeout
            deadline = asyncio.get_event_loop().time() + self.max_wait_ms / 1000
            while len(batch) < self.max_batch_size:
                timeout = max(0, deadline - asyncio.get_event_loop().time())
                try:
                    inputs, future = await asyncio.wait_for(
                        self.queue.get(), timeout=timeout
                    )
                    batch.append(inputs)
                    futures.append(future)
                except asyncio.TimeoutError:
                    break
            
            if not batch:
                continue
            
            # Batch inference (10x faster than sequential)
            results = self.engine.infer_batch(batch)
            
            # Resolve futures
            for future, result in zip(futures, results):
                future.set_result(result)
```

### 6.4 Model Artifact Management

**Database schema**:
```sql
CREATE TABLE model_artifacts (
    artifact_id VARCHAR(36) PRIMARY KEY,
    skill_id VARCHAR(100) NOT NULL,
    model_type VARCHAR(50),      -- "onnx", "pytorch"
    storage_path TEXT,            -- Local path or S3 URL
    version VARCHAR(20),
    metrics JSON,                 -- {"accuracy": 0.87, "f1": 0.85}
    metadata JSON,                -- Training config, dataset info
    is_active BOOLEAN DEFAULT FALSE,
    created_at DATETIME,
    INDEX idx_skill_active (skill_id, is_active)
);
```

**Storage strategy**:
- **Development**: `~/.mo-agent/models/art_abc123.onnx`
- **Production**: S3 + CloudFront CDN for fast download

### 6.5 Multi-Process Deployment

**Problem**: Multiple `mo-agent chat` processes each load model (110MB × N)

**Solution**: Shared model server

```python
# core/models/model_server.py
class ModelServer:
    """Shared inference server for multi-process deployment"""
    
    def __init__(self, port=9527):
        self.app = FastAPI()
        self.engine = None
        
        @self.app.post("/infer")
        async def infer(request: InferRequest):
            if self.engine is None:
                self.engine = ONNXInferenceEngine()
            return self.engine.infer(request.inputs)
    
    def start(self):
        uvicorn.run(self.app, host="127.0.0.1", port=9527)
```

```bash
# Terminal 1: Start model server
mo-agent model serve --port 9527

# Terminal 2-N: Chat processes use HTTP client
mo-agent chat  # Auto-detects model server at localhost:9527
```
    category = "platform"
    
    class Input(SkillInput):
        user_query: str
        agent_response: str
        user_followup: str
    
    class Output(BaseModel):
        label: str  # correction | frustration | rephrasing | clarification | positive | neutral
        confidence: float
    
    async def execute(self, input: Input) -> Output:
        # Load model (cached in memory)
        result = self._model.predict(input.user_query, input.agent_response, input.user_followup)
        return self.Output(label=result.label, confidence=result.confidence)
```

### 6.2 Integration Points

Replace heuristic layer in `ImplicitFeedbackDetector`:

```python
class ImplicitFeedbackDetector:
    def __init__(self, model_path: str | None = None):
        if model_path:
            self._model = load_classifier(model_path)  # Fine-tuned model
        else:
            self._model = None  # Fall back to regex heuristics
    
    def detect(self, user_input, prev_response=None) -> ImplicitSignal:
        if self._model:
            return self._model_detect(user_input, prev_response)
        return self._heuristic_detect(user_input, prev_response)
```

### 6.3 CLI

```bash
# Export training data
mo-admin feedback export --format jsonl --output feedback_train.jsonl

# Train model
mo-admin feedback train --data feedback_train.jsonl --output models/feedback_v1/

# Evaluate
mo-admin feedback eval --model models/feedback_v1/ --test feedback_test.jsonl

# Deploy (registers as platform skill, replaces heuristic layer)
mo-admin feedback deploy --model models/feedback_v1/
```

---

## 7. Continuous Learning Loop

```
Platform conversations
    ↓
Heuristic + current model labels new data
    ↓
---

## 7. Continuous Learning Loop

```
User conversations → Implicit signals detected (heuristic + model)
    ↓
Write to llm_feedback table
    ↓
Monitor: dataset size, label distribution, model accuracy
    ↓
Trigger: dataset grows 20%+ OR accuracy drops 5%+ OR 1 month elapsed
    ↓
mo-admin feedback retrain --validate
    ↓
FeedbackTrainerSkill: export data → train → export ONNX → register artifact
    ↓
Regression gate: test new model on golden set (100 samples)
    ↓ (PASS: accuracy ≥ current - 2%)
A/B test: 10% traffic to new model for 1 week
    ↓ (metrics stable)
Full deployment: activate new model
    ↓
FeedbackClassifierSkill auto-reloads on next inference
    ↓
Better labels → PromptOptimizer gets better signal → better prompts
    ↓
(flywheel: better prompts → better responses → more users → more data)
```

### 7.1 Retraining Trigger

```python
# core/context/feedback_retraining.py
class RetrainingMonitor:
    def __init__(self, db):
        self.db = db
        self.last_train_count = self._get_last_train_count()
    
    async def check_trigger(self) -> bool:
        current_count = self.db.execute(
            "SELECT COUNT(*) FROM llm_feedback "
            "WHERE JSON_EXTRACT(metadata, '$.confidence') >= 0.7"
        ).scalar()
        
        growth_rate = (current_count - self.last_train_count) / self.last_train_count
        
        if growth_rate >= 0.20:
            logger.info(f"Dataset grew {growth_rate:.1%}, triggering retrain")
            return True
        
        # Also check accuracy degradation (compare recent vs historical)
        recent_accuracy = await self._eval_recent_accuracy()
        if recent_accuracy < self.baseline_accuracy - 0.05:
            logger.warning(f"Accuracy dropped to {recent_accuracy:.2%}, retraining")
            return True
        
        return False
```

### 7.2 Automated Retraining

```bash
# Cron job: weekly check
0 2 * * 0  conda run -n agent-engine mo-admin feedback retrain --auto-activate

# Manual trigger
mo-admin feedback retrain --validate --dry-run  # Preview metrics
mo-admin feedback retrain --validate            # Train + gate + activate if pass
```

### 7.3 A/B Testing

```python
# core/models/ab_test.py
class ABTestRouter:
    def __init__(self, db, test_ratio=0.1):
        self.db = db
        self.test_ratio = test_ratio
    
    def select_model(self, session_id: str) -> str:
        # Deterministic routing by session_id hash
        if hash(session_id) % 100 < self.test_ratio * 100:
            return self._get_test_model()
        else:
            return self._get_prod_model()
```

---

## 8. Cost Analysis

| Component | Development | Production (10K users, 1M inferences/day) |
|-----------|-------------|-------------------------------------------|
| Training (1x/week) | Local GPU (free) | AWS g4dn.xlarge ($0.50/hr × 2hr) = $1/week |
| Inference | CPU (free) | t3.medium ($0.04/hr) = $30/month |
| Storage | Local disk | S3 (50MB model) = $0.001/month |
| **Total** | **$0** | **~$34/month** |

**Compare to LLM-based detection**:
- 1M calls/day × $0.0001/call (DeepSeek) = $100/day = $3000/month
- **Savings: 99% cost reduction**

**Latency comparison**:
- Heuristic: <1ms
- ONNX model: 5-10ms (batch), 20-30ms (single)
- LLM (DeepSeek): 200-500ms

---

## 9. Research Alignment

| Paper | Relevance | Our Advantage |
|-------|-----------|---------------|
| Liu et al. 2025 (NYU) | Taxonomy + GPT-4o-mini classification | We go further: distill to small model, deploy as native skill |
| Don-Yehiya et al. 2024 | First to extract implicit feedback at scale | We close the loop: feedback → prompt evolution → deployment |
| Meta RLUF 2025 | Production implicit signals for RL | We use for prompt optimization (lighter than RL fine-tuning) |
| DSPy (Stanford) | Prompt optimization | We add feedback mining + regression gate |

**Our unique contribution**: End-to-end pipeline from implicit signal detection → labeled dataset → fine-tuned classifier → prompt auto-evolution, all within a single auditable platform. No external tooling needed.

---

## 10. Implementation Phases

| Phase | Milestone | Effort |
|-------|-----------|--------|
| 0 (Current) | Heuristic + LLM teacher, `/rate` command | ✅ Done |
| 1 | Data export + training environment setup | 1 day |
| 2 | FeedbackTrainerSkill + ONNX export | 2 days |
| 3 | FeedbackClassifierSkill + chat loop integration | 2 days |
| 4 | Continuous learning + A/B test framework | 2 days |

**Total**: ~1 week from design to production-ready feedback classifier.

**Timeline**:
- Week 1: Accumulate data (users chat, heuristic labels)
- Week 2: Phase 1-3 implementation
- Week 3: Phase 4 + A/B test
- Week 4: Full deployment

---

## References

- Liu, Zhang, Choi. "User Feedback in Human-LLM Dialogues: A Lens to Understand Users But Noisy as a Learning Signal." 2025. https://arxiv.org/abs/2507.23158
- Don-Yehiya, Choshen, Abend. "Naturally Occurring Feedback is Common, Extractable and Useful." 2024.
- Meta. "Reinforcement Learning from User Feedback (RLUF)." 2025. https://arxiv.org/abs/2505.14946
- Khattab et al. "DSPy: Compiling Declarative Language Model Calls into Self-Improving Pipelines." Stanford, 2024.
