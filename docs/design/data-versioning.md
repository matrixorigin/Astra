# Data Versioning (Git for Data)

> **Status**: Core Design — single source of truth for time travel, sandbox, branching, and data lineage  
> **Last Updated**: 2026-02-22

> **Data ownership note**: Data versioning operations (sandbox, time-travel, branching) apply to both the platform DB and the user's BYOD database. Enhanced operations (zero-copy clone, native time-travel) require MatrixOne. See [skill-as-package.md](skill-as-package.md) for the BYOD architecture.

---

## Why Data Versioning Is the Moat

Every capability that makes mo-agent-engine unique depends on one thing: **the ability to query historical data states, branch data cheaply, and track causal relationships natively.**

| Platform Capability | Required Data Capability |
|---------------------|-------------------------|
| Decision audit | Query exact data state at decision time |
| Regression gate | Run tests on full production data instantly |
| Knowledge regression detection | Compare agent behavior across two data versions |
| Training data pipeline | Trace data lineage across dataset versions |
| Memory experimentation | Fix memory in sandbox, replay to verify |
| Prompt evolution | Branch-based A/B testing with quality gates |

MatrixOne provides time-travel queries, zero-copy branching, snapshots, and HTAP natively. This is the architectural foundation — not a pluggable database choice.

---

## 1. Time Travel

### What It Enables

Query any historical data state without manual snapshots:

```sql
-- What did the agent see 3 days ago?
SELECT * FROM conversation_events
  {SNAPSHOT = 'checkpoint_20260211'}
WHERE session_id = 'session_123'
ORDER BY created_at;

-- Compare context selection logic over time
SELECT DATE(created_at) as date,
       AVG(JSON_EXTRACT(context_snapshot, '$.token_budget.total')) as avg_tokens
FROM conversation_events
WHERE event_type = 'llm_request'
GROUP BY DATE(created_at);
```

### Use Cases

- **Decision reconstruction**: "What was the exact input that produced this output?"
- **Hallucination verification**: Verify claims against the same data the LLM saw
- **A/B comparison**: Compare agent behavior at two different time points
- **Data recovery**: View data before accidental deletion

### Checkpoints

Named snapshots for important states:

```python
time_machine.create_checkpoint("before_prompt_v3", description="Baseline before prompt upgrade")
time_machine.create_checkpoint("training_data_v2", description="Dataset extraction point")
```

---

## 2. Sandbox (Zero-Copy Branching)

### What It Enables

Create full production data copies in seconds with zero storage overhead (copy-on-write):

```python
# Create sandbox from current state
sandbox.create("experiment_1", description="Test new prompt")

# Or from a specific checkpoint
sandbox.create("experiment_2", from_snapshot="checkpoint_20260211")

# Sandbox is a separate database — complete isolation
sandbox.use("experiment_1")
# All reads/writes hit the sandbox, not production
```

### Use Cases

| Scenario | How |
|----------|-----|
| **Regression testing** | Snapshot → sandbox → replay golden sessions → compare |
| **Prompt experimentation** | Sandbox → modify prompt_templates → replay → measure quality delta |
| **Memory correction** | Sandbox → fix wrong knowledge entries → replay affected conversations → verify |
| **What-if analysis** | Sandbox → modify data → observe agent behavior changes |
| **Training data extraction** | Snapshot → extract high-quality events → build dataset |
| **Incident replay** | Snapshot at incident time → replay exact sequence → diagnose |

### Sandbox Lifecycle

```
Create (from snapshot or current state)
  → Use (all operations isolated)
  → Checkpoint (save intermediate states within sandbox)
  → Evaluate (compare with production)
  → Merge or Discard
  → Cleanup (DROP DATABASE)
```

### Table-Level Operations

Fine-grained control:

```python
sandbox.clone_table("experiment_1", "conversation_events")  # Clone specific table
sandbox.add_table("experiment_1", "prompt_templates")        # Add another table
sandbox.remove_table("experiment_1", "llm_call_logs")        # Remove unneeded table
```

---

## 3. Branching for Experiments

### Prompt Evolution Pipeline

```
1. PROPOSE: Create new prompt variant
2. BRANCH: Create sandbox from production snapshot
3. MODIFY: Write candidate prompt to sandbox's prompt_templates
4. REPLAY: Run golden sessions against the candidate
5. MEASURE: Compute quality delta vs baseline
6. DECIDE: Merge if improvement > threshold, discard otherwise
7. RECORD: Store experiment results with full lineage
```

```sql
-- Experiment tracking
CREATE TABLE prompt_experiments (
  experiment_id     VARCHAR(64) PRIMARY KEY,
  template_id       VARCHAR(64) NOT NULL,
  branch_name       VARCHAR(255) NOT NULL,
  hypothesis        TEXT,
  candidate_content TEXT NOT NULL,
  baseline_metrics  JSON,
  experiment_metrics JSON,
  quality_delta     DECIMAL(5,4),
  status            VARCHAR(50) DEFAULT 'running',
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  completed_at      TIMESTAMP
);
```

### Knowledge Regression Detection

When knowledge changes:

```
1. Identify past decisions that depended on the old knowledge
   (via context_snapshot.semantic_entries)
2. Create sandbox with updated knowledge
3. Replay affected decisions in sandbox
4. Compare outputs: did the answer change? Is the new answer better?
5. Flag regressions for human review
```

---

## 4. Training Data Pipeline

### Versioned Datasets

```sql
CREATE TABLE training_datasets (
  dataset_id      VARCHAR(64) PRIMARY KEY,
  name            VARCHAR(255) NOT NULL,
  snapshot_name   VARCHAR(255) NOT NULL,  -- Snapshot = dataset version
  event_count     INT NOT NULL,
  pair_count      INT NOT NULL,
  criteria        JSON NOT NULL,          -- Selection criteria
  quality_stats   JSON,
  created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### Pipeline

```
1. FILTER: Select high-quality events (quality_score >= 4.0, training_eligible = TRUE)
2. EXTRACT: Build SFT pairs from causal chains (user_query → llm_response)
3. SNAPSHOT: Create named snapshot as dataset version
4. VALIDATE: Check for contamination across train/test splits
5. EXPORT: JSONL/Parquet for training pipeline
```

### Contamination Detection

```sql
-- Find overlap between training and test datasets
SELECT t.event_id, t.content
FROM training_datasets_v3 t
JOIN test_datasets_v1 e ON t.content_hash = e.content_hash
-- Any matches = contamination
```

### Lineage

Every training example traces back to:
- Source conversation event
- The data state when that event was generated (via context_snapshot)
- The prompt and skill versions used
- The quality score and evaluation source

---

## 5. Event Lineage Graph

### Causal Chain Tracking

All events share `causal_chain_id` linking the full request chain:

```
user_query → llm_request → llm_response → tool_call → tool_result → llm_response
     ↑            ↑              ↑             ↑            ↑             ↑
  chain_001   chain_001      chain_001     chain_001    chain_001     chain_001
```

In multi-agent workflows, the chain extends across agents:

```
user_query → orchestrator_plan → delegation(code_agent) → code_agent_tool_call → 
code_agent_result → orchestrator_synthesis
  All share chain_001
```

### Upstream/Downstream Tracing

```python
# What led to this decision?
lineage.trace_upstream(event_id="evt_response_123")
# → [user_query, context_assembly, skill_selection, tool_calls...]

# What was affected by this knowledge change?
lineage.trace_downstream(event_id="evt_knowledge_update_456")
# → [decisions that used this knowledge, training data that included it...]
```

---

## 6. MatrixOne-Native Workflows

The point is not "we use MatrixOne features." The point is that MatrixOne's capabilities **collapse entire categories of infrastructure into single operations**, creating workflows that are impossible or prohibitively expensive on traditional stacks.

These workflows fall into two categories:
- **Platform-internal**: Operate on the platform's own state DB (always available)
- **Enhanced service**: Operate on the user's data via passed `db` handle (available when user data is on MatrixOne)

### Workflow 1: "Clone-Test-Merge" — Zero-Risk Agent Evolution (Platform-internal)

**Industry pain point**: Changing a prompt, skill, or model is terrifying in production. No one knows if it will break existing behavior. Most teams ship and pray.

**Our solution**: Every change goes through a clone-test-merge cycle that costs near-zero time and storage.

```
Developer changes prompt_v3
  │
  ▼
CREATE CLONE experiment FROM production;     -- <5 seconds, 0 extra storage
  │
  ▼
UPDATE experiment.prompt_templates SET content = '...' WHERE id = 'code_review';
  │
  ▼
Replay 50 golden sessions in experiment DB   -- same data, new prompt
  │
  ▼
DIFF BRANCH experiment vs production         -- what changed in outputs?
  │
  ▼
Quality improved? → MERGE BRANCH experiment  -- promote to production
Quality regressed? → DROP DATABASE experiment -- discard, zero cleanup
```

**What this replaces**: CI/CD pipelines with staging environments, manual QA, A/B testing infrastructure, feature flags, rollback procedures. All of that collapses into clone → test → merge/discard.

### Workflow 2: "Snapshot-as-Ground-Truth" — Auditable Decisions (Platform-internal)

**Industry pain point**: "Why did the agent say that?" is unanswerable because the data has changed since the decision was made.

**Our solution**: Every decision binds to a snapshot. The snapshot IS the ground truth.

```
Agent makes decision at T1
  │
  ▼
context_snapshot records: snapshot_name = "snap_T1"
  │
  ▼
... days pass, data changes ...
  │
  ▼
Auditor asks: "What did the agent see?"
  │
  ▼
RESTORE ACCOUNT sys FROM SNAPSHOT snap_T1;   -- or query with {SNAPSHOT = 'snap_T1'}
  │
  ▼
Exact data state at T1 — including vector indexes, knowledge entries, event history
```

**What this replaces**: Application-level snapshot logic, separate audit databases, manual data archival, compliance reporting tools. The database IS the audit system.

### Workflow 3: "Hybrid Memory Recall" — One Query, Three Signals (Platform-internal)

**Industry pain point**: Memory retrieval requires stitching together a vector DB (semantic), a search engine (keyword), and a relational DB (structured filters). Three systems, three sync problems, three failure modes.

**Our solution**: MatrixOne does vector + fulltext + SQL in a single query.

```sql
SELECT event_id, content, quality_score,
  l2_distance(embedding, @query_vec) AS semantic_score,
  MATCH(content) AGAINST(@keywords IN BOOLEAN MODE) AS keyword_score
FROM conversation_events
WHERE user_id = @user_id
  AND created_at > NOW() - INTERVAL 7 DAY
  AND quality_score > 3.0
ORDER BY (0.5 * semantic_score + 0.3 * keyword_score + 0.2 * quality_score) DESC
LIMIT 10;
```

**What this replaces**: Pinecone + Elasticsearch + PostgreSQL. Three deployments, three bills, three sync jobs, eventual consistency bugs. Gone.

**New concept this enables: "Memory with opinions."** Because quality_score lives next to the embedding, retrieval naturally prefers high-quality memories. Bad experiences decay not just by time, but by quality. The agent's memory is self-curating.

### Workflow 4: "Publication-as-Marketplace" — Skill Distribution Without Infrastructure (Platform-internal)

**Industry pain point**: Sharing reusable agent capabilities across teams requires building a registry, an API, a distribution mechanism, version management, and access control.

**Our solution**: MatrixOne Publication IS the marketplace.

```sql
-- Publisher account
CREATE PUBLICATION my_skills DATABASE skill_db TABLE skills_registry ACCOUNT ALL;

-- Consumer account
CREATE DATABASE team_skills FROM publisher_acct PUBLICATION my_skills;
-- Done. Skills available. Updates automatic. Read-only. Isolated.

-- Want to pin a version instead?
CREATE CLONE pinned_skills FROM publisher_acct.skill_db;
-- Writable copy, frozen in time. Upgrade when ready.
```

**What this replaces**: npm/pip-style package registries, API gateways, webhook-based update notifications, entitlement management. The database IS the distribution channel.

### Workflow 5: "Clone-per-Agent" — Isolated Parallel Exploration (Enhanced service)

**Industry pain point**: When multiple agents work in parallel, they can step on each other's data. Locking is complex. Isolation is expensive.

**Our solution**: Each agent in a team gets its own clone. Zero-cost. Full isolation.

```
Team lead decomposes task into 4 subtasks
  │
  ├── CREATE CLONE agent_a_workspace FROM production;
  ├── CREATE CLONE agent_b_workspace FROM production;
  ├── CREATE CLONE agent_c_workspace FROM production;
  └── CREATE CLONE agent_d_workspace FROM production;
  │
  ▼
Each agent works in its own clone — full read/write, no conflicts
  │
  ▼
DIFF BRANCH agent_a_workspace vs production  -- what did agent A change?
MERGE BRANCH agent_a_workspace              -- accept agent A's changes
  │
  ▼
DROP DATABASE agent_b_workspace, agent_c_workspace, agent_d_workspace;
```

**New concept: "Speculative Execution for Agents."** Like CPU branch prediction — run multiple approaches in parallel, keep the best one, discard the rest. Only possible when branching is free.

### Workflow 6: "UDF-as-Guardrail" — Safety at the Data Layer (Platform-internal)

**Industry pain point**: Guardrails are application-level middleware. They can be bypassed, they add latency, they're hard to audit.

**Our solution**: Push safety checks into the database as Python UDFs. They run WHERE the data lives.

```sql
CREATE FUNCTION check_pii(content TEXT) RETURNS BOOLEAN
LANGUAGE PYTHON AS $$
import re
def check_pii(content):
    patterns = [r'\b\d{3}-\d{2}-\d{4}\b', r'\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}\b']
    return not any(re.search(p, content) for p in patterns)
$$;

-- Every event insertion automatically checked
CREATE DYNAMIC TABLE pii_violations AS
SELECT event_id, content, created_at
FROM conversation_events
WHERE NOT check_pii(content);
-- Violations detected in real-time, no application code needed
```

**What this replaces**: Middleware guardrail layers, post-hoc scanning jobs, separate compliance databases. The guardrail IS the database constraint.

### Workflow 7: "Stage-as-Training-Pipeline" — Data to Model Without ETL (Platform-internal)

**Industry pain point**: Getting training data from production to a fine-tuning pipeline requires ETL jobs, data warehouses, export scripts, format conversion.

**Our solution**: Stage (S3 integration) + External Table = bidirectional data flow with zero ETL.

```sql
-- Export: production → S3 (one statement)
SELECT event_id, content, quality_score INTO OUTFILE
  's3://training-data/sft_v3.jsonl' FIELDS TERMINATED BY '\n'
FROM conversation_events
WHERE quality_score >= 4.0 AND training_eligible = TRUE;

-- Import: S3 → queryable (no data movement)
CREATE EXTERNAL TABLE training_v3 (...)
  INFILE 's3://training-data/sft_v3.jsonl' FORMAT 'jsonline';

-- Compare: current production vs training snapshot
SELECT p.event_id FROM conversation_events p
LEFT JOIN training_v3 t ON p.event_id = t.event_id
WHERE t.event_id IS NULL AND p.quality_score >= 4.0;
-- New high-quality events not yet in training set
```

### The Pattern

Every workflow above follows the same principle: **what traditionally requires a separate system collapses into a database operation.**

For the platform's own state (workflows 1-4, 6-7): this is always available — the platform runs on MatrixOne.

For user business data (workflow 5, and workflows 1-3 applied to user data): this activates when the user's data is also on MatrixOne. The service accepts a `db` handle — like `Sandbox(db=user_db, source_db="user_database")` — and operates on the user's database. The agent code doesn't change; only the db handle determines what data the service operates on.

---

## 7. Cost-Aware Branching (Implemented)

> **Implementation**: `core/sandbox/cost_predictor.py` — `BranchCostPredictor` class, 12 unit tests passing.

Before creating branches or running experiments:

```python
# Predict cost of replaying 50 sessions
estimated_cost = cost_predictor.estimate(
    operation="replay",
    session_count=50,
    model="gpt-4o",
    historical_avg_tokens=3000
)

if estimated_cost > budget_remaining:
    suggest_alternatives(cheaper_model="gpt-4o-mini", reduced_sessions=20)
```

Historical cost data from `llm_call_logs` enables accurate prediction.
