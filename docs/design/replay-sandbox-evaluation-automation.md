# Replay, Sandbox, Evaluation & Evolution: Engineering Validation

**Status**: Engineering Specification  
**Version**: 2.0  
**Last Updated**: 2026-02-11

## Purpose

This document bridges the gap between design vision and operational completeness by defining:
- **Automated replay gating** with quality metrics
- **Side-effect isolation** for safe replay (critical)
- **Sandbox-based validation** workflows
- **Skill/Prompt evolution** closed-loop automation
- **Acceptance criteria** for production readiness
- **Regression Gate with data lineage** for automated quality gates
- **Prompt Evolution Pipeline** with branch-based experimentation
- **Sandbox-as-CI** for automated testing of every skill/prompt change
- **Training Data Pipeline** with versioned datasets

**Goal**: Make "replay + sandbox + evaluation + evolution" provably operational, not just aspirational.

**Critical addition**: This revision addresses the **fatal gap** of side-effect isolation—ensuring replay doesn't trigger real-world actions (merge PRs, delete repos, send emails).

---

## 1. Side-Effect Isolation (Critical)

### 1.1 The Problem

**Scenario**: Production session contains `github.merge_pr(id=123)`. When replayed in sandbox:
- ❌ **Without isolation**: Agent calls real GitHub API → merges PR again (or fails if already merged)
- ❌ **Catastrophic cases**: `delete_repo`, `send_email`, `deploy_to_prod`

**Root cause**: Data sandbox (MatrixOne DB isolation) ≠ Execution sandbox (external API isolation).

### 1.2 Solution: Execution Sandbox with Tool Mocking

**Three-layer isolation**:

| Layer | Scope | Mechanism |
|-------|-------|-----------|
| **Data Sandbox** | MatrixOne database | Separate DB (e.g., `replay_gate_123`) |
| **Execution Sandbox** | Tool/Skill invocations | Mock mode + recorded responses |
| **Code Sandbox** | Generated code execution | Docker container with resource limits |

### 1.3 Tool Mocking Architecture

**Recording phase** (production):
```python
# When skill is invoked in production
skill_result = github.merge_pr(id=123)

# Store in conversation_events
event = {
    "event_type": "skill_invocation",
    "skill_id": "github_merge_pr",
    "skill_params": {"id": 123},
    "skill_result": skill_result,  # ← Record actual result
    "skill_side_effects": {         # ← Record what changed
        "api_calls": [{"method": "POST", "url": "/repos/.../pulls/123/merge", "status": 200}],
        "external_state": {"pr_123_status": "merged"}
    }
}
```

**Replay phase** (sandbox):
```python
class ToolMockingLayer:
    def __init__(self, mode: str = "replay"):
        self.mode = mode  # "replay" | "production" | "dry_run"
        self.recorded_results = {}  # Loaded from conversation_events
    
    def invoke_skill(self, skill_id: str, params: dict) -> Any:
        if self.mode == "production":
            # Real execution
            return self._execute_real(skill_id, params)
        
        elif self.mode == "replay":
            # Return recorded result
            key = (skill_id, json.dumps(params, sort_keys=True))
            if key in self.recorded_results:
                return self.recorded_results[key]
            else:
                raise ReplayError(f"No recorded result for {skill_id}({params})")
        
        elif self.mode == "dry_run":
            # Validate but don't execute
            return self._validate_and_mock(skill_id, params)
```

### 1.4 Skill Classification

**All skills must declare their side-effect profile**:

```python
# skills_registry table
{
    "skill_id": "github_merge_pr",
    "side_effect_profile": {
        "category": "write",           # "read" | "write" | "destructive"
        "external_apis": ["github"],
        "idempotent": false,
        "reversible": false,
        "mock_strategy": "recorded"    # "recorded" | "noop" | "error"
    }
}
```

**Enforcement in replay**:

| Category | Replay Behavior | Example |
|----------|----------------|---------|
| **read** | Allow real calls (safe) | `github.get_pr`, `search_code` |
| **write** | Mock with recorded result | `github.merge_pr`, `create_issue` |
| **destructive** | Block + error (never replay) | `delete_repo`, `force_push` |

### 1.5 Implementation

**Skill wrapper** (`core/skills/executor.py`):
```python
class SkillExecutor:
    def __init__(self, db: Database, mode: str = "production"):
        self.db = db
        self.mode = mode
        self.mock_layer = ToolMockingLayer(mode)
    
    def execute(self, skill_id: str, params: dict, event_id: str = None) -> dict:
        # Load skill definition
        skill = self._load_skill(skill_id)
        
        # Check side-effect profile
        if self.mode == "replay":
            if skill["side_effect_profile"]["category"] == "destructive":
                raise ReplayError(f"Skill {skill_id} is destructive, cannot replay")
            
            if skill["side_effect_profile"]["category"] == "write":
                # Use recorded result
                recorded = self._load_recorded_result(event_id, skill_id, params)
                return {"result": recorded, "mocked": True}
        
        # Real execution
        result = skill["implementation"](params)
        
        # Record if in production
        if self.mode == "production":
            self._record_result(event_id, skill_id, params, result)
        
        return {"result": result, "mocked": False}
```

### 1.6 Read-Only Mode

**For skills without recorded results**:
```python
class ReadOnlySkillProxy:
    """Wraps skills to prevent writes during replay"""
    
    def __init__(self, skill: Skill):
        self.skill = skill
    
    def __call__(self, **params):
        if self.skill.side_effect_profile["category"] in ["write", "destructive"]:
            # Return mock response
            return {
                "status": "mocked",
                "message": f"Skill {self.skill.id} blocked in read-only mode",
                "original_params": params
            }
        else:
            # Allow read operations
            return self.skill(**params)
```

### 1.7 Mock GitHub Server (Optional)

**For testing skills themselves** (not replay):
```yaml
# docker-compose.test.yml
services:
  mock-github:
    image: mockserver/mockserver
    ports:
      - "1080:1080"
    environment:
      MOCKSERVER_INITIALIZATION_JSON_PATH: /config/github-mocks.json
```

**Mock configuration**:
```json
{
  "httpRequest": {
    "method": "POST",
    "path": "/repos/.*/pulls/.*/merge"
  },
  "httpResponse": {
    "statusCode": 200,
    "body": {"merged": true, "sha": "abc123"}
  }
}
```

---

## 2. Code Execution Sandbox

### 2.1 The Problem

**Scenario**: Agent generates code via `GenerateTestsSkill` and executes it.
- ❌ **Without sandbox**: Code runs on host → infinite loop, file deletion, network access
- ❌ **Security risk**: Malicious code injection

### 2.2 Solution: Docker-Based Code Sandbox

**Architecture**:
```
┌─────────────────────────────────────┐
│ Agent (Host)                        │
│  ├─ Skill: GenerateTestsSkill       │
│  └─ Executor: CodeSandboxExecutor   │
└──────────────┬──────────────────────┘
               │ Docker API
               ▼
┌─────────────────────────────────────┐
│ Code Sandbox Container              │
│  ├─ Python 3.11 (isolated)          │
│  ├─ Resource limits (CPU/Memory)    │
│  ├─ Network: disabled               │
│  ├─ Filesystem: read-only + tmpfs   │
│  └─ Timeout: 30s                    │
└─────────────────────────────────────┘
```

**Implementation** (`core/sandbox/code_executor.py`):
```python
import docker
from typing import Optional

class CodeSandboxExecutor:
    def __init__(self):
        self.client = docker.from_env()
        self.image = "python:3.11-slim"
    
    def execute(self, code: str, timeout: int = 30) -> dict:
        """Execute code in isolated Docker container"""
        try:
            # Create container with strict limits
            container = self.client.containers.run(
                image=self.image,
                command=["python", "-c", code],
                detach=True,
                network_disabled=True,  # No network access
                mem_limit="256m",       # 256MB RAM limit
                cpu_quota=50000,        # 50% CPU
                read_only=True,         # Read-only filesystem
                tmpfs={"/tmp": "size=10m"},  # 10MB temp space
                remove=True
            )
            
            # Wait with timeout
            result = container.wait(timeout=timeout)
            logs = container.logs().decode("utf-8")
            
            return {
                "status": "success" if result["StatusCode"] == 0 else "error",
                "exit_code": result["StatusCode"],
                "output": logs,
                "timeout": False
            }
        
        except docker.errors.ContainerError as e:
            return {"status": "error", "output": str(e), "timeout": False}
        
        except Exception as e:
            return {"status": "timeout", "output": str(e), "timeout": True}
```

### 2.3 Resource Limits

**Enforced limits**:
- **CPU**: 50% of one core (prevents CPU exhaustion)
- **Memory**: 256MB (prevents memory bombs)
- **Timeout**: 30 seconds (prevents infinite loops)
- **Network**: Disabled (prevents data exfiltration)
- **Filesystem**: Read-only + 10MB tmpfs (prevents file attacks)

### 2.4 Skill Integration

**Example skill** (`skills/generate_tests.py`):
```python
class GenerateTestsSkill(Skill):
    def __init__(self):
        self.code_executor = CodeSandboxExecutor()
    
    def execute(self, code: str) -> dict:
        # Generate test code (LLM call)
        test_code = self._generate_tests(code)
        
        # Execute in sandbox
        result = self.code_executor.execute(test_code)
        
        if result["status"] == "success":
            return {"tests": test_code, "passed": True, "output": result["output"]}
        else:
            return {"tests": test_code, "passed": False, "error": result["output"]}
```

---

## 3. MatrixOne-Specific Optimizations

### 3.1 Snapshot-Based Sandbox Creation

**Leverage MatrixOne's snapshot feature for instant sandbox creation**:

```sql
-- Create snapshot of production database
CREATE SNAPSHOT prod_snapshot_20260211 FOR ACCOUNT sys;

-- Create sandbox from snapshot (instant, zero-copy)
CREATE DATABASE replay_gate_123 FROM SNAPSHOT prod_snapshot_20260211;
```

**Benefits**:
- **Speed**: Seconds instead of minutes (no data copy)
- **Storage**: Zero additional storage (copy-on-write)
- **Consistency**: Point-in-time consistency guaranteed

**Implementation** (`core/sandbox/matrixone_sandbox.py`):
```python
class MatrixOneSandbox:
    def create_from_snapshot(self, sandbox_name: str, snapshot_id: str):
        """Create sandbox using MatrixOne snapshot (instant)"""
        self.db.execute(f"""
            CREATE DATABASE {sandbox_name} 
            FROM SNAPSHOT {snapshot_id}
        """)
        return {"created_at": time.time(), "storage_overhead": 0}
```

### 3.2 Columnar Storage for Metrics

**Leverage MatrixOne's OLAP capabilities for large-scale log analysis**:

```sql
-- Metrics table optimized for columnar storage
CREATE TABLE replay_metrics (
    run_id VARCHAR(64),
    session_id VARCHAR(64),
    metric_name VARCHAR(64),
    metric_value FLOAT,
    timestamp TIMESTAMP,
    metadata JSON
) WITH (storage_format = 'columnar');  -- MatrixOne columnar optimization

-- Fast aggregation queries
SELECT 
    metric_name,
    AVG(metric_value) as avg_value,
    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY metric_value) as p95
FROM replay_metrics
WHERE run_id = 'gate-2026-02-11'
GROUP BY metric_name;
```

**Benefits**:
- **Compression**: 10x better compression for metrics data
- **Query speed**: 100x faster for aggregation queries
- **Cost**: Lower storage costs for long-term retention

---

## 5. Integration with Replay Gate

### 5.1 Updated Replay Workflow

**With side-effect isolation**:

```python
class ReplayGate:
    def run(self, baseline_id: str, new_config: Dict, sandbox_name: str) -> GateResult:
        # 1. Load golden sessions
        sessions = self._load_golden_sessions(baseline_id)
        
        # 2. Create data sandbox (MatrixOne)
        snapshot_id = self._create_snapshot()
        self.sandbox.create_from_snapshot(sandbox_name, snapshot_id)
        
        # 3. Initialize execution sandbox (Tool Mocking)
        executor = SkillExecutor(self.db, mode=ExecutionMode.REPLAY)
        for session in sessions:
            executor.mock_layer.load_recorded_results(session.session_id)
        
        # 4. Replay in parallel with mocking enabled
        results = []
        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [
                pool.submit(self._replay_session, s, new_config, executor)
                for s in sessions
            ]
            results = [f.result() for f in futures]
        
        # 5. Compute metrics
        metrics = self._compute_metrics(results)
        baseline_metrics = self._load_baseline_metrics(baseline_id)
        
        # 6. Compare and decide
        passed = self._compare_metrics(metrics, baseline_metrics)
        
        # 7. Generate report
        report = self._generate_report(metrics, baseline_metrics, passed)
        
        # 8. Cleanup
        self.sandbox.delete(sandbox_name)
        
        return GateResult(passed=passed, metrics=metrics, report=report)
```

### 5.2 Validation Checks

**Before replay starts**:
```python
def _validate_replay_safety(self, session_id: str) -> List[str]:
    """Check if session can be safely replayed"""
    warnings = []
    
    # Check for destructive skills
    destructive = self.db.query("""
        SELECT DISTINCT skill_id, skill_params
        FROM conversation_events
        WHERE session_id = ?
          AND event_type = 'skill_invocation'
          AND skill_id IN (
              SELECT skill_id FROM skills_registry
              WHERE JSON_EXTRACT(side_effect_profile, '$.category') = 'destructive'
          )
    """, (session_id,))
    
    if destructive:
        warnings.append(
            f"Session contains {len(destructive)} destructive skill invocations. "
            f"These will be blocked during replay."
        )
    
    # Check for missing recorded results
    missing = self.db.query("""
        SELECT skill_id, COUNT(*) as count
        FROM conversation_events
        WHERE session_id = ?
          AND event_type = 'skill_invocation'
          AND (skill_result IS NULL OR skill_result = '{}')
        GROUP BY skill_id
    """, (session_id,))
    
    if missing:
        warnings.append(
            f"Session has {sum(m['count'] for m in missing)} skill invocations "
            f"without recorded results. Replay may fail."
        )
    
    return warnings
```

---

## 6. Acceptance Criteria

### 6.1 Side-Effect Isolation

**Deliverables**:
- [ ] `ToolMockingLayer` class with replay mode
- [ ] `side_effect_profile` field in `skills_registry`
- [ ] All existing skills classified (read/write/destructive)
- [ ] `skill_result` and `skill_side_effects` fields in `conversation_events`
- [ ] Replay validation checks

**Acceptance test**:
```bash
# 1. Record a session with write operations
mo-agent chat --user-id test_user
> "Merge PR #123"  # Triggers github.merge_pr skill

# 2. Replay in sandbox mode
mo-agent replay --session-id <session_id> --mode replay

# Expected: 
# - No real API calls made
# - Recorded result returned
# - Metrics computed correctly
```

**Success criteria**:
- Zero real API calls during replay (verified by network monitoring)
- Destructive skills blocked with clear error message
- Recorded results match original execution

### 6.2 Code Execution Sandbox

**Deliverables**:
- [ ] `CodeSandboxExecutor` class
- [ ] Docker container configuration
- [ ] Resource limit enforcement
- [ ] Integration with code generation skills

**Acceptance test**:
```python
# Test 1: Infinite loop protection
code = "while True: pass"
result = executor.execute(code, timeout=5)
assert result["timeout"] == True
assert result["elapsed_seconds"] <= 6  # 5s + 1s grace

# Test 2: Memory bomb protection
code = "x = [0] * (10**9)"  # Try to allocate 8GB
result = executor.execute(code, mem_limit="256m")
assert result["status"] == "error"

# Test 3: Network isolation
code = "import urllib.request; urllib.request.urlopen('http://google.com')"
result = executor.execute(code)
assert "Network is unreachable" in result["output"]
```

**Success criteria**:
- All resource limits enforced
- No network access possible
- Malicious code contained

### 6.3 MatrixOne Optimizations

**Deliverables**:
- [ ] Snapshot-based sandbox creation
- [ ] Columnar metrics table
- [ ] Performance benchmarks

**Acceptance test**:
```bash
# Test snapshot creation speed
time mo-agent sandbox create-from-snapshot test-sandbox prod-snapshot

# Expected: < 10 seconds for 100GB database
```

**Success criteria**:
- Sandbox creation < 10 seconds regardless of data size
- Zero storage overhead until writes
- Metrics queries 10x faster than baseline

---

## 7. Risk Mitigation

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Recorded results missing** | Replay fails | Validation check before replay; fallback to read-only mode |
| **Skill parameters changed** | Key mismatch, no recorded result | Fuzzy matching on params; warn user |
| **Docker not available** | Code sandbox fails | Graceful degradation; skip code execution tests |
| **Snapshot creation fails** | Can't create sandbox | Fallback to table clone (slower but works) |
| **Mocking layer bypassed** | Real API calls in replay | Enforce at executor level; integration tests |

---

## 9. Regression Gate with Data Lineage

Every skill/prompt change must pass through an automated regression gate before reaching production.

Workflow:
1. Create snapshot of current production state
2. Load golden sessions (quality_score >= 4.0, training_eligible = TRUE, last 50)
3. Replay golden sessions against the snapshot
4. Compute quality metrics (error rate, score delta)
5. Pass/fail decision (error rate < 5%)
6. Record gate result with full data lineage (which snapshot, which sessions, what metrics)

Schema:
```sql
CREATE TABLE gate_results (
  gate_id VARCHAR(64) PRIMARY KEY,
  change_type VARCHAR(50) NOT NULL,
  change_id VARCHAR(255) NOT NULL,
  snapshot_used VARCHAR(255) NOT NULL,
  sessions_tested INT NOT NULL,
  error_rate DECIMAL(5,4),
  passed BOOLEAN NOT NULL,
  metrics JSON,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_change (change_type, change_id),
  INDEX idx_created (created_at)
);
```

---

## 10. Prompt Evolution Pipeline

Data-versioned prompt experimentation follows a Git-like workflow:

1. **Propose change**: Create new prompt variant
2. **Create data branch**: Branch conversation_events table
3. **Evaluate on branch**: Run experiments in isolated environment
4. **Merge if better**: Promote successful variants to production

Each experiment maintains full lineage:
- Which baseline prompt was used
- What changes were made
- Which sessions were tested
- Performance comparison metrics
- Decision rationale

Implementation:
```python
class PromptEvolution:
    def experiment(self, prompt_id: str, variant: str) -> ExperimentResult:
        # Create data branch
        branch_name = f"prompt_exp_{prompt_id}_{timestamp}"
        self.branch.create(branch_name, "conversation_events")
        
        # Run evaluation
        results = self.evaluate_prompt(variant, branch_name)
        
        # Compare with baseline
        baseline = self.load_baseline_metrics(prompt_id)
        improvement = self.compare_metrics(results, baseline)
        
        # Record experiment
        self.record_experiment(prompt_id, variant, results, improvement)
        
        return ExperimentResult(
            improved=improvement > 0.05,
            metrics=results,
            recommendation="merge" if improvement > 0.05 else "reject"
        )
```

---

## 11. Sandbox-as-CI

Automatically trigger sandbox-based regression tests on every skill/prompt change:

1. **Change detection**: Monitor skill registry and prompt updates
2. **Sandbox creation**: Spin up isolated test environment
3. **Test execution**: Run regression suite against change
4. **Result recording**: Store test outcomes with full context
5. **Cleanup**: Destroy sandbox after test completion

Workflow:
```python
class SandboxCI:
    def on_change(self, change_event: ChangeEvent):
        # Create test sandbox
        sandbox_id = f"ci_{change_event.id}_{uuid4()}"
        self.sandbox.create(sandbox_id)
        
        try:
            # Load test suite
            tests = self.load_regression_tests(change_event.type)
            
            # Execute in sandbox
            results = []
            for test in tests:
                result = self.execute_test(test, sandbox_id)
                results.append(result)
            
            # Aggregate results
            summary = self.aggregate_results(results)
            
            # Record outcome
            self.record_ci_run(change_event.id, summary)
            
            # Block deployment if failed
            if not summary.passed:
                self.block_deployment(change_event.id, summary.failures)
                
        finally:
            # Always cleanup
            self.sandbox.delete(sandbox_id)
```

---

## 12. Training Data Pipeline

Automated training data extraction with versioned datasets:

1. **Quality filtering**: Extract high-quality conversation events (score >= 4.0)
2. **SFT pair creation**: Build supervised fine-tuning pairs from causal chains
3. **Dataset versioning**: Snapshot each dataset with metadata
4. **Cross-version comparison**: Track data drift and quality changes
5. **Contamination detection**: Ensure test/train separation

Schema:
```sql
CREATE TABLE training_datasets (
  dataset_id VARCHAR(64) PRIMARY KEY,
  version VARCHAR(32) NOT NULL,
  source_snapshot VARCHAR(255) NOT NULL,
  quality_threshold DECIMAL(3,2),
  record_count INT NOT NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  metadata JSON,
  INDEX idx_version (version),
  INDEX idx_created (created_at)
);

CREATE TABLE sft_pairs (
  pair_id VARCHAR(64) PRIMARY KEY,
  dataset_id VARCHAR(64) NOT NULL,
  input_text TEXT NOT NULL,
  target_text TEXT NOT NULL,
  source_session VARCHAR(64),
  source_event VARCHAR(64),
  quality_score DECIMAL(3,2),
  FOREIGN KEY (dataset_id) REFERENCES training_datasets(dataset_id)
);
```

Implementation:
```python
class TrainingDataPipeline:
    def extract_dataset(self, version: str, quality_threshold: float = 4.0) -> Dataset:
        # Create snapshot for reproducibility
        snapshot_id = f"training_data_{version}_{timestamp}"
        self.db.execute(f"CREATE SNAPSHOT {snapshot_id} FOR ACCOUNT sys")
        
        # Extract high-quality events
        events = self.db.query("""
            SELECT session_id, event_id, content, quality_score
            FROM conversation_events
            WHERE quality_score >= ?
              AND training_eligible = TRUE
            ORDER BY created_at DESC
        """, (quality_threshold,))
        
        # Build SFT pairs
        pairs = []
        for event in events:
            if event.event_type == "llm_response":
                input_event = self.get_parent_event(event.parent_event_id)
                pairs.append({
                    "input": input_event.content,
                    "target": event.content,
                    "quality": event.quality_score
                })
        
        # Store dataset
        dataset = self.create_dataset(version, snapshot_id, pairs)
        return dataset
```

---

## 8. Implementation Priority

**Critical (Week 1-2)**:
1. Tool mocking layer
2. Skill classification system
3. Replay mode enforcement
4. Regression gate with data lineage

**High (Week 3-4)**:
5. Code execution sandbox
6. Snapshot-based sandbox creation
7. Validation checks
8. Sandbox-as-CI automation

**Medium (Week 5-6)**:
9. Columnar metrics optimization
10. Mock GitHub server
11. Prompt evolution pipeline
12. Training data pipeline

**Low (Week 7-8)**:
13. Comprehensive tests
14. Performance optimizations
15. Advanced analytics

---

## Conclusion

This document addresses the **fatal gap** of side-effect isolation, making the replay/evaluation system **safe and usable**:

1. **Execution Sandbox**: Tool mocking prevents real API calls during replay
2. **Code Sandbox**: Docker isolation prevents malicious code execution
3. **MatrixOne Optimizations**: Snapshots enable instant sandbox creation
4. **Regression Gate**: Automated quality gates with full data lineage
5. **Prompt Evolution**: Branch-based experimentation with version control
6. **Sandbox-as-CI**: Continuous integration for every change
7. **Training Pipeline**: Versioned datasets with contamination detection

**Without these additions, the entire replay system would be dangerous and unusable.** With them, it becomes a **production-ready quality gate** with comprehensive automation.

**Next steps**:
1. Implement tool mocking layer (Week 1-2)
2. Build regression gate with data lineage
3. Add code execution sandbox
4. Deploy sandbox-as-CI automation
5. Create training data pipeline
6. Comprehensive testing

**Key insight**: Side-effect isolation is not optional—it's **critical infrastructure** for any replay/evaluation system. The additional automation capabilities transform this from a manual tool into a complete MLOps platform.
