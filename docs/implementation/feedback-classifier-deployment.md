# Feedback Classification Model — Deployment Architecture

> **Last Updated**: 2026-02-21  
> **Related**: `feedback-classification-model.md` (design), `plan-2026-02-21-feedback-classifier.md`

Complete engineering design for training, deploying, and operating the feedback classification model as a production skill.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Training Pipeline                         │
│  (Heavy deps: transformers, torch, GPU)                     │
│                                                              │
│  astra-admin feedback train                                     │
│      → FeedbackTrainerSkill (isolated conda env)            │
│      → Export ONNX model                                     │
│      → Register to model_artifacts table                    │
└─────────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────────┐
│                   Inference Pipeline                         │
│  (Light deps: onnxruntime, numpy, CPU)                      │
│                                                              │
│  Chat loop → FeedbackClassifierSkill                        │
│      → Load ONNX model (lazy init)                          │
│      → Batch inference (async queue)                        │
│      → Fallback to heuristic on error                       │
└─────────────────────────────────────────────────────────────┘
```

## Dependency Isolation Strategy

### Problem
- **Training**: Needs PyTorch (2GB+), transformers, CUDA toolkit
- **Inference**: Only needs ONNX Runtime (50MB), numpy
- **Other skills**: Should not be forced to install ML dependencies

### Solution: Conditional Imports + Environment Detection

#### 1. Skill Metadata Declaration

```python
# skills/feedback_classifier/skill.py
class FeedbackClassifierSkill(BaseSkill):
    skill_id = "feedback_classifier"
    
    requirements = SkillRequirement(
        optional_packages=["onnxruntime>=1.16.0", "numpy>=1.24.0"],
        fallback_mode="heuristic"  # Degrade gracefully if deps missing
    )
```

#### 2. Lazy Import Pattern

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
            import numpy as np
            from transformers import AutoTokenizer
        except ImportError as e:
            raise SkillDependencyError(
                f"Missing dependency: {e.name}. "
                f"Install with: pip install onnxruntime transformers"
            )
        
        # Load model from model_artifacts table
        model_path = self._get_model_path()
        self._session = ort.InferenceSession(model_path)
        self._tokenizer = AutoTokenizer.from_pretrained("bert-base-multilingual-cased")
```

#### 3. Executor Dependency Check

```python
# core/agent/executor.py
class SkillExecutor:
    async def execute_skill(self, skill_id: str, inputs: dict):
        skill = self.registry.get(skill_id)
        
        # Check optional dependencies
        if skill.requirements.optional_packages:
            missing = self._check_packages(skill.requirements.optional_packages)
            if missing:
                if skill.requirements.fallback_mode:
                    logger.warning(f"Missing {missing}, using fallback: {skill.requirements.fallback_mode}")
                    return await self._execute_fallback(skill, inputs)
                else:
                    raise SkillDependencyError(f"Missing required packages: {missing}")
        
        return await skill.execute(**inputs)
```

### Environment Setup

#### Base Environment (agent-engine)
```yaml
# environment.yml
name: agent-engine
dependencies:
  - python=3.11
  - sqlalchemy
  - pydantic
  - httpx
  # NO torch, transformers
```

#### Training Environment (agent-engine-train)
```yaml
# environment-train.yml
name: agent-engine-train
dependencies:
  - python=3.11
  - pytorch::pytorch=2.1.0
  - pytorch::torchvision
  - pytorch::pytorch-cuda=12.1  # GPU support
  - transformers=4.36.0
  - datasets=2.16.0
  - accelerate=0.25.0
  - onnx=1.15.0
```

#### Inference-Only Environment (agent-engine-infer)
```yaml
# environment-infer.yml (optional, for production)
name: agent-engine-infer
dependencies:
  - python=3.11
  - onnxruntime=1.16.0  # CPU-only, 50MB
  - numpy=1.24.0
  - transformers=4.36.0  # Only for tokenizer
```

## Training Pipeline

### Skill: FeedbackTrainerSkill

**Executor**: Runs in `agent-engine-train` conda env (GPU-enabled)

```python
# skills/feedback_trainer/skill.py
class FeedbackTrainerSkill(BaseSkill):
    skill_id = "feedback_trainer"
    
    requirements = SkillRequirement(
        conda_env="agent-engine-train",  # Executor switches env
        gpu_required=True,
        min_memory_gb=8
    )
    
    async def execute(
        self,
        dataset_id: str,
        base_model: str = "bert-base-multilingual-cased",
        epochs: int = 3,
        batch_size: int = 16,
        learning_rate: float = 2e-5
    ) -> dict:
        # 1. Export training data
        train_data = await self._export_data(dataset_id)
        
        # 2. Train model (uses PyTorch + Transformers)
        model, metrics = await self._train(
            train_data, base_model, epochs, batch_size, learning_rate
        )
        
        # 3. Export to ONNX (for lightweight inference)
        onnx_path = await self._export_onnx(model)
        
        # 4. Register model artifact
        artifact_id = await self._register_artifact(
            model_path=onnx_path,
            metrics=metrics,
            metadata={
                "base_model": base_model,
                "training_samples": len(train_data),
                "accuracy": metrics["test_accuracy"]
            }
        )
        
        return {
            "artifact_id": artifact_id,
            "metrics": metrics,
            "onnx_path": onnx_path
        }
```

### GPU Scheduling

#### Option 1: Local GPU (Development)
```python
# core/agent/executor.py
class SkillExecutor:
    def _select_device(self, skill: BaseSkill) -> str:
        if not skill.requirements.gpu_required:
            return "cpu"
        
        import torch
        if torch.cuda.is_available():
            return "cuda:0"
        else:
            logger.warning("GPU required but not available, using CPU")
            return "cpu"
```

#### Option 2: Remote GPU (Production)
```python
# skills/feedback_trainer/remote_executor.py
class RemoteGPUExecutor:
    """Offload training to remote GPU cluster"""
    
    async def execute(self, skill_id: str, inputs: dict):
        # Submit job to Ray cluster / Kubernetes GPU pod
        job_id = await self.ray_client.submit(
            skill_id=skill_id,
            inputs=inputs,
            resources={"num_gpus": 1, "memory": "8GB"}
        )
        
        # Poll for completion
        result = await self._wait_for_job(job_id)
        return result
```

### Distributed Training (Future)

```python
# skills/feedback_trainer/distributed.py
from accelerate import Accelerator

class DistributedTrainer:
    def __init__(self):
        self.accelerator = Accelerator()  # Auto-detects multi-GPU
    
    def train(self, model, train_loader):
        model, train_loader = self.accelerator.prepare(model, train_loader)
        
        for batch in train_loader:
            # Automatic gradient accumulation + DDP
            outputs = model(**batch)
            loss = outputs.loss
            self.accelerator.backward(loss)
```

## Inference Pipeline

### Skill: FeedbackClassifierSkill

**Executor**: Runs in base `agent-engine` env (CPU-only)

```python
# skills/feedback_classifier/skill.py
class FeedbackClassifierSkill(BaseSkill):
    skill_id = "feedback_classifier"
    
    requirements = SkillRequirement(
        optional_packages=["onnxruntime>=1.16.0"],
        fallback_mode="heuristic"
    )
    
    def __init__(self):
        self.engine = None  # Lazy init
        self.batch_queue = asyncio.Queue()
        self.batch_processor = None
    
    async def execute(
        self,
        user_query: str,
        agent_response: str,
        followup_query: str
    ) -> dict:
        if self.engine is None:
            self._init_engine()
        
        # Async batch inference
        result = await self._infer_async(user_query, agent_response, followup_query)
        return result
    
    def _init_engine(self):
        try:
            self.engine = ONNXInferenceEngine()
            self.batch_processor = asyncio.create_task(self._batch_loop())
        except SkillDependencyError:
            logger.warning("ONNX not available, using heuristic fallback")
            self.engine = HeuristicEngine()
```

### Batch Inference Optimization

```python
# skills/feedback_classifier/batch_processor.py
class BatchProcessor:
    def __init__(self, engine, max_batch_size=32, max_wait_ms=50):
        self.engine = engine
        self.max_batch_size = max_batch_size
        self.max_wait_ms = max_wait_ms
        self.queue = asyncio.Queue()
    
    async def infer(self, inputs: dict) -> dict:
        future = asyncio.Future()
        await self.queue.put((inputs, future))
        return await future
    
    async def _batch_loop(self):
        while True:
            batch = []
            futures = []
            
            # Collect batch
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
            
            # Batch inference
            results = self.engine.infer_batch(batch)
            
            # Resolve futures
            for future, result in zip(futures, results):
                future.set_result(result)
```

## Model Artifact Management

### Database Schema

```sql
CREATE TABLE model_artifacts (
    artifact_id VARCHAR(36) PRIMARY KEY,
    skill_id VARCHAR(100) NOT NULL,
    model_type VARCHAR(50),  -- "onnx", "pytorch", "tensorflow"
    storage_path TEXT,       -- S3/local path
    version VARCHAR(20),
    metrics JSON,            -- {"accuracy": 0.87, "f1": 0.85}
    metadata JSON,
    is_active BOOLEAN DEFAULT FALSE,
    created_at DATETIME,
    INDEX idx_skill_active (skill_id, is_active)
);
```

### Storage Strategy

#### Development: Local Filesystem
```python
# core/models/artifact_manager.py
class ArtifactManager:
    def save(self, artifact_id: str, model_path: str):
        storage_dir = Path("~/.astra/models").expanduser()
        storage_dir.mkdir(parents=True, exist_ok=True)
        
        dest = storage_dir / f"{artifact_id}.onnx"
        shutil.copy(model_path, dest)
        
        self.db.execute(
            "INSERT INTO model_artifacts (artifact_id, storage_path, ...) VALUES (...)",
            {"artifact_id": artifact_id, "storage_path": str(dest)}
        )
```

#### Production: S3 + CDN
```python
class S3ArtifactManager(ArtifactManager):
    def save(self, artifact_id: str, model_path: str):
        s3_key = f"models/{artifact_id}.onnx"
        self.s3_client.upload_file(model_path, self.bucket, s3_key)
        
        # Generate CloudFront URL for fast download
        cdn_url = f"https://cdn.astra.local/models/{artifact_id}.onnx"
        
        self.db.execute(
            "INSERT INTO model_artifacts (...) VALUES (...)",
            {"storage_path": cdn_url}
        )
```

## Deployment Workflow

### 1. Train Model

```bash
# Activate training environment
conda activate agent-engine-train

# Train via skill
astra skill execute feedback_trainer \
  --dataset-id ds_20260221 \
  --epochs 3 \
  --batch-size 16

# Output:
# ✅ Training complete
# 📊 Accuracy: 0.87, F1: 0.85
# 💾 Artifact ID: art_abc123
# 📦 ONNX exported: ~/.astra/models/art_abc123.onnx
```

### 2. Validate Model

```bash
# Regression gate: compare with current active model
astra-admin model validate --artifact-id art_abc123

# Output:
# 🔍 Testing on golden set (100 samples)
# Current model: 0.82 accuracy
# New model:     0.87 accuracy (+5%)
# ✅ PASS: No regression detected
```

### 3. Activate Model

```bash
# Switch to new model
astra-admin model activate --artifact-id art_abc123

# DB update:
# UPDATE model_artifacts SET is_active = FALSE WHERE skill_id = 'feedback_classifier';
# UPDATE model_artifacts SET is_active = TRUE WHERE artifact_id = 'art_abc123';
```

### 4. Inference (Automatic)

```bash
# Switch back to base environment
conda activate agent-engine

# Chat loop automatically uses new model
astra chat

# First inference triggers lazy load:
# 📥 Loading model art_abc123 from ~/.astra/models/art_abc123.onnx
# ✅ Model loaded (50ms)
# 🔮 Inference: signal_type=correction, confidence=0.89
```

## Multi-Process Deployment

### Problem
- Multiple `astra chat` processes
- Each loads model into memory (110MB × N processes)

### Solution: Shared Model Server

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
astra model serve --port 9527

# Terminal 2-N: Chat processes use HTTP client
astra chat  # Auto-detects model server at localhost:9527
```

## Monitoring & Observability

### Metrics to Track

```python
# skills/feedback_classifier/metrics.py
class InferenceMetrics:
    latency_histogram = Histogram("feedback_classifier_latency_ms")
    prediction_counter = Counter("feedback_classifier_predictions", ["signal_type"])
    fallback_counter = Counter("feedback_classifier_fallbacks", ["reason"])
    batch_size_histogram = Histogram("feedback_classifier_batch_size")
```

### Logging

```python
# Every inference logs to skill_execution_logs
{
    "skill_id": "feedback_classifier",
    "inputs": {"query": "...", "response": "...", "followup": "..."},
    "output": {"signal_type": "correction", "confidence": 0.89},
    "latency_ms": 45,
    "model_version": "art_abc123",
    "fallback_used": false
}
```

## Continuous Learning Loop

```
User feedback → llm_feedback table
    ↓ (data growth 20%)
Trigger retraining
    ↓
FeedbackTrainerSkill (GPU)
    ↓
Export ONNX → model_artifacts
    ↓
Regression gate validation
    ↓ (PASS)
Activate new model
    ↓
FeedbackClassifierSkill auto-reloads
```

Automated via cron job:
```bash
# crontab
0 2 * * 0  conda run -n agent-engine-train astra-admin feedback retrain --auto-activate
```

## Cost Analysis

| Component | Development | Production |
|-----------|-------------|------------|
| Training (1x/week) | Local GPU (free) | AWS g4dn.xlarge ($0.50/hr × 2hr) = $1/week |
| Inference (1M calls/day) | CPU (free) | t3.medium ($0.04/hr) = $30/month |
| Storage | Local disk | S3 (50MB model) = $0.001/month |
| **Total** | **$0** | **~$34/month** |

Compare to LLM-based detection: 1M calls × $0.0001/call = $100/day = $3000/month

**Savings: 99% cost reduction**
