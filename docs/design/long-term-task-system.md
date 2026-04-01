# Long-Term Task System Design

## Overview

A **durable, verifiable, multi-session task system** that leverages MatrixOne's
unique capabilities (git4data branching, stage/DataLink artifacts, HTAP queries)
alongside the existing edge-cloud architecture and memory/learning pipeline.

### Why This Exists

Current agent systems (including claudecode) treat tasks as single-session
affairs. When a task is complex — spanning hours, requiring multiple agents,
crossing session boundaries — there is no system to:

1. **Guarantee completion** — tasks can be abandoned mid-way with no recovery
2. **Verify outcomes** — "done" means the agent stopped, not that criteria passed
3. **Learn across tasks** — success/failure patterns aren't systematically captured
4. **Isolate parallel work** — multiple agents can't safely work on the same project

This design addresses all four with MatrixOne as the execution substrate.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     USER / CLIENT                             │
│  "Refactor auth module to use JWT, add tests, update docs"   │
└────────────────────────┬─────────────────────────────────────┘
                         │
┌────────────────────────▼─────────────────────────────────────┐
│              TASK CONTRACT LAYER                              │
│  TaskContract { goal, scope, subtasks[], verification[] }    │
│  Persisted: cloud task_contracts table                        │
│  Versioned: plan_version_history (rollback support)          │
└────────────────────────┬─────────────────────────────────────┘
                         │
┌────────────────────────▼─────────────────────────────────────┐
│          DURABLE TASK LIFECYCLE ENGINE                        │
│                                                               │
│  States: Draft → Contracted → Executing → Verifying          │
│          → Verified → Delivered                               │
│          (+ Paused, Failed, Abandoned branches)               │
│                                                               │
│  Persistence: MatrixOne agent_tasks + task_contracts          │
│  Leasing: task_leases (TTL, row-lock, auto-reclaim)          │
│  Checkpoint: per-subtask state (edge + cloud)                │
└────┬──────────────┬──────────────┬───────────────────────────┘
     │              │              │
┌────▼────┐  ┌──────▼──────┐  ┌───▼────────────────────────────┐
│ GIT4DATA │  │ VERIFICATION│  │ MEMORY / LEARNING              │
│ ISOLATION│  │ ENGINE      │  │                                 │
│          │  │             │  │ EntityGraph ← task entities     │
│ Per-task │  │ Command     │  │ PatternLibrary ← tool chains   │
│ snapshot │  │ Build/Test  │  │ Calibrator ← verification stats│
│ branches │  │ Grep/File   │  │ Templates ← successful plans   │
│ Diff     │  │ LLM Judge   │  │                                 │
│ Merge    │  │ Composite   │  │ Sync: edge ↔ cloud (delta)     │
│ Rollback │  │             │  │                                 │
└──────────┘  └─────────────┘  └────────────────────────────────┘
```

---

## 1. Task Contract

Every durable task begins with a **contract** — a structured commitment that
defines what "done" means before execution starts.

```rust
/// A durable task contract with verifiable acceptance criteria.
pub struct TaskContract {
    pub contract_id: String,
    pub task_id: String,
    pub goal: String,
    pub scope: TaskScope,
    pub global_verification: Vec<VerificationCriterion>,
    pub version: u32,
    pub status: ContractStatus,    // draft | active | amended | completed | abandoned
    pub created_at: String,
    pub updated_at: String,
}

pub struct TaskScope {
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub assumptions: Vec<String>,
}

/// Per-subtask, verifiable acceptance criterion.
pub struct VerificationCriterion {
    pub id: String,
    pub description: String,       // Human-readable
    pub verifier: VerifierKind,    // Machine-executable
    pub required: bool,            // Must-pass vs advisory
    pub timeout_sec: u32,          // Max verification time
}

pub enum VerifierKind {
    /// Run command, check exit code
    Command { cmd: String, expected_exit: i32 },
    /// Run command, check stdout content
    CommandOutput { cmd: String, contains: Vec<String>, not_contains: Vec<String> },
    /// Check files exist
    FileExists { paths: Vec<String> },
    /// Grep pattern in file
    GrepCheck { file: String, pattern: String, should_match: bool },
    /// Build must pass
    BuildPass { cmd: String },
    /// Tests must pass with min rate
    TestPass { cmd: String, min_pass_rate: f64 },
    /// LLM-based semantic judgment (can run on cloud)
    LlmJudge { prompt: String, pass_threshold: f64 },
    /// Composite (AND/OR of sub-criteria)
    Composite { criteria: Vec<VerificationCriterion>, require_all: bool },
}
```

### Cloud Storage

```sql
CREATE TABLE IF NOT EXISTS task_contracts (
    contract_id    VARCHAR(36) PRIMARY KEY,
    task_id        VARCHAR(36) NOT NULL,
    session_id     VARCHAR(36) NOT NULL,
    user_id        VARCHAR(36) NOT NULL,
    goal           TEXT NOT NULL,
    scope_json     JSON,
    criteria_json  JSON NOT NULL,
    version        INT NOT NULL DEFAULT 1,
    status         VARCHAR(20) NOT NULL DEFAULT 'draft',
    created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_tc_task (task_id),
    INDEX idx_tc_user_status (user_id, status)
);
```

---

## 2. Durable Subtask with Verification Gate

Each subtask extends the existing `SubtaskPlan` with a verification gate:

```rust
pub struct DurableSubtask {
    // From existing SubtaskPlan
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub depends_on: Vec<String>,
    pub effort: Option<String>,
    pub files: Vec<String>,

    // Durable extensions
    pub stage: SubtaskStage,
    pub criteria: Vec<VerificationCriterion>,
    pub max_retries: u32,
    pub retry_count: u32,

    // Git4data isolation
    pub data_branch: Option<String>,     // MatrixOne branch name
    pub snapshot_name: Option<String>,   // Pre-execution snapshot
}

pub enum SubtaskStage {
    Pending,
    Blocked { reason: String },
    Executing,
    ExecutionFailed { error: String },
    AwaitingVerification,
    Verifying,
    VerificationFailed { results: Vec<VerificationResult> },
    Verified,
    Completed,
    Skipped { reason: String },
    Abandoned { reason: String },
}
```

### State Machine

```
Pending ──[deps met]──→ Executing
    ──[snapshot]──→ CREATE SNAPSHOT task_{id}_pre

Executing ──[done]──→ AwaitingVerification ──[auto]──→ Verifying
Executing ──[error]──→ ExecutionFailed
    ──[retry ≤ max]──→ RESTORE SNAPSHOT → Executing
    ──[retry > max]──→ Abandoned

Verifying ──[all pass]──→ Verified ──→ Completed
Verifying ──[fail]──→ VerificationFailed
    ──[retry ≤ max]──→ Executing (re-attempt)
    ──[retry > max]──→ Abandoned
```

---

## 3. Git4Data Integration

MatrixOne's git4data provides **zero-cost data branching** for task isolation.

### Per-Task Isolation

```sql
-- Before task execution: snapshot current state
CREATE SNAPSHOT task_{task_id}_v{version} FOR ACCOUNT;

-- For parallel subtasks: branch per agent
DATA BRANCH CREATE DATABASE agent_{agent_id}_workspace
    FROM workspace{snapshot="task_{task_id}_v{version}"};

-- After subtask: diff changes
SELECT * FROM mo_diff('task_{task_id}_v{version}', 'agent_{agent_id}_workspace');

-- On success: merge back
DATA BRANCH MERGE agent_{agent_id}_workspace INTO workspace;

-- On failure: instant rollback
RESTORE ACCOUNT FROM SNAPSHOT task_{task_id}_v{version};
```

### Benefits vs File-Based Branching

| Aspect | Git (files) | Git4Data (MatrixOne) |
|--------|-------------|---------------------|
| Scope | Source code only | All agent state (tasks, events, learning) |
| Isolation | git worktree (filesystem) | Database-level (zero-copy snapshot) |
| Diff | Text-based | Row-level semantic diff |
| Merge | 3-way text merge | Row-level merge with conflict detection |
| Rollback | git checkout (lossy) | RESTORE SNAPSHOT (instant, complete) |
| Parallel agents | Need separate worktrees | Each gets a data branch, no filesystem cost |

### Implementation

```rust
pub struct TaskBranchService {
    pool: SharedPool,
}

impl TaskBranchService {
    /// Create pre-execution snapshot for a task
    pub async fn snapshot_before_task(&self, task_id: &str, version: u32)
        -> Result<String, String>;

    /// Create isolated branch for an agent's work
    pub async fn create_agent_branch(&self, task_id: &str, agent_id: &str,
        snapshot: &str) -> Result<String, String>;

    /// Diff agent's branch against snapshot
    pub async fn diff_agent_work(&self, snapshot: &str, branch: &str)
        -> Result<DiffSummary, String>;

    /// Merge agent's work back (after verification passes)
    pub async fn merge_verified_work(&self, branch: &str, target: &str)
        -> Result<MergeResult, String>;

    /// Rollback to snapshot (on failure)
    pub async fn rollback_to_snapshot(&self, snapshot: &str)
        -> Result<(), String>;
}
```

---

## 4. Verification Engine

Verification runs on **edge** (for filesystem/command checks) or **cloud**
(for LLM judge / cross-reference checks).

```rust
pub struct VerificationRunner {
    edge_tools: Arc<dyn ToolExecutor>,    // bash, fs, grep
}

pub struct VerificationResult {
    pub criterion_id: String,
    pub passed: bool,
    pub evidence: String,          // Actual output
    pub expected: String,          // What we checked for
    pub duration_ms: u64,
    pub error: Option<String>,     // If verification itself failed
}

pub struct SubtaskVerificationReport {
    pub subtask_id: String,
    pub all_required_passed: bool,
    pub results: Vec<VerificationResult>,
    pub timestamp: String,
}

impl VerificationRunner {
    /// Verify all criteria for a subtask
    pub async fn verify_subtask(&self, subtask: &DurableSubtask)
        -> SubtaskVerificationReport;

    /// Run a single criterion
    async fn run_criterion(&self, criterion: &VerificationCriterion)
        -> VerificationResult;
}
```

### Cloud Storage for Verification Audit

```sql
CREATE TABLE IF NOT EXISTS task_verification_results (
    result_id      VARCHAR(36) PRIMARY KEY,
    contract_id    VARCHAR(36) NOT NULL,
    task_id        VARCHAR(36) NOT NULL,
    subtask_id     VARCHAR(64) NOT NULL,
    criterion_id   VARCHAR(64) NOT NULL,
    session_id     VARCHAR(36) NOT NULL,
    passed         SMALLINT NOT NULL,
    evidence       LONGTEXT,
    expected       TEXT,
    duration_ms    INT,
    error_message  TEXT,
    attempt        INT NOT NULL DEFAULT 1,
    created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_tvr_task_subtask (task_id, subtask_id),
    INDEX idx_tvr_contract (contract_id, created_at)
);
```

---

## 5. Memory / Learning Integration

Task outcomes feed back into the existing learning pipeline:

### Entity Graph
- Each task introduces entities (files, modules, APIs, concepts)
- Successful task completion → entities get confidence boost
- Failed tasks → entity-tool associations get penalty

### Pattern Library
- Tool chains used in successful subtasks → recorded as patterns
- Verification-related patterns (which verifiers work for which task types)
- Template extraction: completed task plan → reusable template

### Progressive Calibrator
- Verification pass/fail rates → calibration data
- Per-project-type thresholds for auto-fix confidence

### Integration Points

```rust
impl DurableTaskLifecycle {
    /// After task completes: feed learning
    async fn record_task_outcome(&self, task: &TaskRecord, contract: &TaskContract) {
        // 1. Update EntityGraph with task entities
        // 2. Record tool chain patterns from execution timeline
        // 3. Update calibrator with verification stats
        // 4. Extract template if high-quality completion
        // 5. Push learning snapshot to cloud (delta sync)
    }
}
```

---

## 6. Stage / Artifact Integration (Future)

For long-running tasks that produce artifacts (reports, generated code,
documentation), MatrixOne Stage provides persistent storage:

```sql
-- Mount artifact storage
CREATE STAGE task_artifacts URL = 's3://mo-agent-artifacts/';

-- Store task deliverables as DataLink references
CREATE TABLE IF NOT EXISTS task_artifacts (
    artifact_id   VARCHAR(36) PRIMARY KEY,
    task_id       VARCHAR(36) NOT NULL,
    subtask_id    VARCHAR(64),
    artifact_type VARCHAR(50) NOT NULL,    -- report, code, test, doc
    file_ref      DATALINK,               -- Pointer to Stage file
    description   TEXT,
    created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_ta_task (task_id),
    FULLTEXT INDEX ft_content (file_ref)   -- Search inside artifacts
);
```

This enables **cross-task artifact search**: "Find all test files generated
for JWT authentication tasks in the last month."

---

## 7. DurableTaskLifecycle Service

The main orchestration trait:

```rust
#[async_trait]
pub trait DurableTaskLifecycle: Send + Sync {
    // ── Contract Phase ──
    async fn create_contract(&self, user_id: &str, session_id: &str,
        goal: &str, plan: &TaskPlan) -> Result<TaskContract, String>;
    async fn amend_contract(&self, contract_id: &str,
        amendments: ContractAmendment) -> Result<TaskContract, String>;

    // ── Execution Phase ──
    async fn begin_subtask(&self, task_id: &str, subtask_id: &str)
        -> Result<SubtaskExecutionContext, String>;
    async fn complete_subtask_execution(&self, task_id: &str, subtask_id: &str)
        -> Result<(), String>;
    async fn fail_subtask(&self, task_id: &str, subtask_id: &str,
        error: &str) -> Result<(), String>;

    // ── Verification Phase ──
    async fn verify_subtask(&self, task_id: &str, subtask_id: &str)
        -> Result<SubtaskVerificationReport, String>;
    async fn verify_global(&self, task_id: &str)
        -> Result<Vec<VerificationResult>, String>;

    // ── Resume / Recovery ──
    async fn pause_task(&self, task_id: &str) -> Result<(), String>;
    async fn resume_task(&self, task_id: &str, session_id: &str)
        -> Result<TaskResumeContext, String>;

    // ── Delivery ──
    async fn deliver_task(&self, task_id: &str)
        -> Result<TaskDeliveryReport, String>;

    // ── Git4Data ──
    async fn snapshot_task_state(&self, task_id: &str)
        -> Result<String, String>;
    async fn rollback_task(&self, task_id: &str, snapshot: &str)
        -> Result<(), String>;
}
```

---

## 8. Event Flow

Every lifecycle transition produces a cloud event:

```
task_contract_created    → contract stored
subtask_started          → snapshot created, stage = Executing
subtask_execution_done   → stage = AwaitingVerification
verification_started     → stage = Verifying
verification_passed      → result stored, stage = Verified
verification_failed      → result stored, retry or Abandoned
subtask_completed        → stage = Completed, learning recorded
global_verification_run  → all subtasks verified
task_delivered           → delivery report generated
task_paused              → checkpoint saved
task_resumed             → checkpoint loaded, continue
task_abandoned           → rollback snapshot, cleanup
```

---

## 9. Implementation Plan

### Phase 1: Core Types + Verification (This PR)
- `DurableSubtask`, `VerificationCriterion`, `VerifierKind` types
- `SubtaskStage` state machine
- `VerificationRunner` (Command, Build, Test, File, Grep verifiers)
- `TaskContract` type + cloud DDL
- `DurableTaskLifecycle` trait
- Unit tests for verification + state machine

### Phase 2: Git4Data + Cloud Integration
- `TaskBranchService` (snapshot, branch, diff, merge, rollback)
- Cloud DDL: `task_contracts`, `task_verification_results`
- Integration with event ingestion pipeline
- Checkpoint enhancement (include verification state)

### Phase 3: Memory + Learning
- Task outcome → EntityGraph, PatternLibrary, Calibrator
- Template extraction from successful tasks
- Verification stats → calibration data
- Delta sync for task learning

### Phase 4: Stage + Artifacts (Future)
- Stage mount + DataLink columns
- Artifact storage and fulltext search
- Cross-task artifact discovery
