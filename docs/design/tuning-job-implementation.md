# Tuning Job MVP Implementation Plan

**Date**: 2026-06-24  
**Status**: Implementation Design  
**Audience**: Astra runtime, CLI, server, storage, observability, and SDK maintainers

## Executive Summary

`/tuning` should be implemented as a durable state-machine job, not as a
long-running slash command.

The closest existing Astra patterns are:

- `/plan`: versioned durable state, active session binding, append-only step
  runs.
- `/task`: durable status, checkpoint, progress, pause/resume/cancel.
- Session journal: append-only progress events visible across clients.

MVP should prove this loop:

```text
Signal -> FailureCluster -> Hypothesis -> Candidate -> Evaluation -> Decision
```

Apply, measurement, rollback, promotion, finalizers, and multi-controller
decomposition are later phases.

## Product Behavior

The user-facing command:

```text
/tuning goal="Improve NL2SQL exact-match accuracy to 88%"
        benchmark="nl2sql_eval_v3"
        constraints="prompt_tokens <= 3500, p95_latency_ms <= 5000"
        allowed_changes="prompt_patch,skill_patch,context_policy_patch"
```

means:

```text
create durable TuningJob
-> persist spec/status/checkpoint
-> start or enqueue JobController
-> return job_id and current status
```

It must not run the whole optimization loop synchronously in the current chat
turn.

## Plan-Like Intent Flow

`/tuning` is a specialized plan mode plus a durable optimization loop.

It should not require the user to provide a complete `TuningSpec` up front.
The first phase is a lightweight planning/spec-drafting interaction:

```text
user intent
-> recognize tuning request
-> draft TuningSpec
-> map intent into target, harness, constraints, budget, allowed changes
-> ask user only for missing blocking fields
-> confirm spec
-> create durable TuningJob
-> start JobController
```

This is similar to `/plan`: the system helps turn a fuzzy goal into structured
work. The difference is that `/tuning` must land the plan into measurable
tuning elements.

### Spec Elements

The spec drafting step maps user intent into these fields:

| Element | Example | Required for Start |
| --- | --- | --- |
| Domain | `nl2sql` | Yes |
| Objective | improve SQL generation accuracy | Yes |
| Metric | `exact_match_accuracy`, `execution_accuracy`, qualitative review | Yes, or explicitly qualitative |
| Harness | `nl2sql_eval_v3` | Yes for benchmark tuning |
| Baseline | current prompt/skill/context policy | Yes |
| Constraints | prompt size, latency, cost, safety | Optional but recommended |
| Allowed changes | prompt, skill, context policy | Yes |
| Search budget | rounds, candidates, cost | Yes, defaultable |
| Source signals | adaptation signal refs or selected failure evidence | Optional |

### Ask-User Policy

The system should ask the user only when a missing field blocks safe execution.
The implementation can use the existing ask-user/confirmation UI path for these
questions.

Ask when:

- No measurable or explicitly qualitative objective is available.
- No harness exists for a benchmark-driven request.
- Allowed change types are ambiguous or risky.
- Budget or scope would otherwise default to something expensive.
- Constraints conflict, such as impossible target and strict prompt limit.
- Applying changes is requested but approval scope is unclear.

Do not ask when:

- A conservative default is safe.
- The missing field only affects display.
- The job can start in diagnosis-only mode.

Example:

```text
I can draft this tuning job, but I need one choice before starting:
Which benchmark should measure NL2SQL quality?
1. nl2sql_eval_v3
2. create diagnosis-only job
```

If the user does not answer, the job should remain in `draft` or `blocked`, not
silently choose an unsafe benchmark.

### Draft Lifecycle

Before a job starts, the lifecycle is:

| Phase | Meaning |
| --- | --- |
| `drafting` | Parse user intent and infer spec fields |
| `needs_input` | Ask-user is required for blocking fields |
| `ready_to_start` | Spec is complete enough to create/start job |

MVP can keep draft state in the current session until the user confirms. If the
draft should survive session boundaries, persist it as a `TuningJob` with
`status = created` and `phase = drafting`.

## Loop and Trace Model

Yes: a tuning job is a durable loop.

| Concept | Tuning Entity | Meaning |
| --- | --- | --- |
| Target | `TuningSpec.objective` and constraints | What the loop is trying to improve and what it must not violate |
| Loop instance | `TuningJob` | One durable optimization run |
| Cursor | `phase` plus `checkpoint_json` | Where the loop can resume |
| Step | `TuningStep` or material phase handler | One unit of work such as baseline, cluster, evaluate, decide |
| Step attempt | `tuning_events` row with `step_ref` and `attempt` | One try at a step |
| State | `status`, `phase`, `progress_pct`, `current_step` | User-visible current state |
| Trace | `trace_ref` plus ordered `tuning_events` | Full explanation of how the loop moved |
| Evidence | `evidence_refs_json` and artifact refs | Why a transition or decision happened |

The loop is not an infinite retry loop. It stops when it reaches a terminal
decision, exhausts budget, becomes blocked, fails, or is cancelled.

Recommended trace shape:

```text
TuningJob(trace_ref)
  -> step: spec_accepted
  -> step: baseline
  -> step: failure_clustering
  -> step: hypothesis_generation
  -> step: candidate_generation
  -> step: candidate_evaluation
  -> step: decision
```

Each step should answer:

- What input refs did it consume?
- What output refs did it produce?
- Which budget did it spend?
- Was it skipped, reused, successful, blocked, or failed?
- Which event explains the transition?

## MVP Commands

| Command | Behavior |
| --- | --- |
| `/tuning draft ...` | Draft a spec from intent without starting |
| `/tuning create ...` | Create a durable tuning job |
| `/tuning confirm` | Confirm the current draft and create/start job |
| `/tuning start <job>` | Start or resume controller execution |
| `/tuning status [job]` | Show phase, progress, budget, selected refs, decision |
| `/tuning list` | List recent jobs for the user/session |
| `/tuning dashboard` | Open multi-job dashboard |
| `/tuning events <job>` | Show recent audit/progress events |
| `/tuning trace <job>` | Show ordered step attempts with inputs, outputs, and decisions |
| `/tuning cancel <job>` | Request terminal cancellation |
| `/tuning edit <job>` | Create a new generation or replacement job from edited spec |

Shortcut:

```text
/tuning goal="..." benchmark="..."
```

is equivalent to `draft + confirm + start` only when required fields are clear.
If key fields are missing, it enters draft mode and asks the smallest blocking
question.

## MVP Architecture

```text
CLI / Web / API
  -> TuningJobService
  -> TuningJobRepository
  -> JobController
  -> HarnessAdapter
  -> ArtifactStore
  -> Observation / Event sinks
```

Responsibilities:

| Component | Responsibility |
| --- | --- |
| `TuningJobService` | Validate requests, owner-check jobs, expose create/start/status/list/cancel/events |
| `TuningJobRepository` | Durable spec/status/checkpoint/events with optimistic concurrency |
| `JobController` | Advance one state-machine phase at a time |
| `HarnessAdapter` | Run baseline/candidate/holdout evaluations |
| `ArtifactStore` | Store prompt/skill/context-policy patches and benchmark outputs |
| Observation sink | Expose tuning events/evaluations as graph evidence |

## Proposed Module Layout

The first implementation should follow existing runtime/service layering.

| Module | Contents |
| --- | --- |
| `rust/crates/services/src/tuning_job.rs` | Core structs, enums, repository trait, service logic |
| `rust/crates/services/src/tuning_job_repository.rs` | SQL-backed repository and in-memory repository |
| `rust/crates/services/src/tuning_job_controller.rs` | MVP `JobController` state-machine runner |
| `rust/crates/services/src/tuning_harness.rs` | Harness adapter trait and first benchmark adapter |
| `rust/crates/runtime/src/server/tuning_handlers.rs` | HTTP handlers and request/response validation |
| `rust/crates/astra-cli/src/cli/slash/slash_tuning.rs` | `/tuning` command parsing and rendering |
| `rust/crates/services/src/introspection/...` | Observation provider for tuning events/status |

The module names can move to match crate conventions, but the boundaries should
stay: repository owns persistence, service owns user/API semantics, controller
owns state transitions, harness owns benchmark execution.

## Storage Model

Start with two tables. Add normalized candidate/evaluation tables later only
when query patterns require them.

### `tuning_jobs`

Current-state projection for UI, CLI, API, and resume.

```sql
CREATE TABLE tuning_jobs (
    job_id                 VARCHAR(64) PRIMARY KEY,
    trace_ref              VARCHAR(256) NOT NULL,
    user_id                VARCHAR(128) NOT NULL,
    session_id             VARCHAR(128) NULL,
    status                 VARCHAR(32) NOT NULL,
    phase                  VARCHAR(32) NOT NULL,
    spec_json              JSON NOT NULL,
    checkpoint_json        JSON NULL,
    progress_pct           INT NOT NULL DEFAULT 0,
    current_step           VARCHAR(256) NULL,
    selected_candidate_ref VARCHAR(256) NULL,
    decision_ref           VARCHAR(256) NULL,
    budget_used_json       JSON NULL,
    error_message          TEXT NULL,
    version                BIGINT NOT NULL DEFAULT 1,
    created_at             TIMESTAMP NOT NULL,
    updated_at             TIMESTAMP NOT NULL,
    completed_at           TIMESTAMP NULL,
    INDEX idx_tuning_user_updated (user_id, updated_at),
    INDEX idx_tuning_session_updated (session_id, updated_at),
    INDEX idx_tuning_status_updated (status, updated_at)
);
```

Column semantics:

| Column | Meaning |
| --- | --- |
| `job_id` | Durable user-facing id, short enough for CLI prefixes |
| `trace_ref` | Stable trace id for ordered step events |
| `user_id` | Owner; required for every read/write |
| `session_id` | Optional session that created or is watching the job |
| `status` | Broad lifecycle: `created`, `running`, `completed`, `rejected`, `blocked`, `failed`, `cancelled` |
| `phase` | Current controller cursor such as `baseline` or `evaluating` |
| `spec_json` | Immutable desired state for this generation |
| `checkpoint_json` | Compact resume state containing refs and budget counters |
| `progress_pct` | UI projection only; not a correctness signal |
| `current_step` | Human/agent-readable current operation |
| `selected_candidate_ref` | Filled after decision when applicable |
| `decision_ref` | Terminal decision artifact ref |
| `budget_used_json` | Denormalized budget counters for fast status rendering |
| `version` | Optimistic concurrency token |

Status values:

- `created`
- `running`
- `completed`
- `rejected`
- `blocked`
- `failed`
- `cancelled`
- `superseded`
- `not_converged`

Phase values are finer-grained and match the controller phases. Terminal status
may keep the last meaningful phase for debugging.

### `tuning_events`

Append-only audit and replay log.

```sql
CREATE TABLE tuning_events (
    event_id           VARCHAR(64) PRIMARY KEY,
    job_id             VARCHAR(64) NOT NULL,
    sequence           BIGINT NOT NULL,
    trace_ref          VARCHAR(256) NOT NULL,
    step_ref           VARCHAR(256) NULL,
    attempt            INT NULL,
    event_type         VARCHAR(64) NOT NULL,
    payload_json       JSON NOT NULL,
    evidence_refs_json JSON NULL,
    created_at         TIMESTAMP NOT NULL,
    UNIQUE KEY uniq_tuning_event_seq (job_id, sequence),
    INDEX idx_tuning_events_job_created (job_id, created_at),
    INDEX idx_tuning_events_trace_seq (trace_ref, sequence)
);
```

Event types:

| Event Type | Purpose |
| --- | --- |
| `job_created` | Initial job persisted |
| `job_started` | Controller execution requested |
| `step_started` | Material phase began |
| `step_reused` | Existing output ref reused during resume |
| `step_succeeded` | Material phase produced output refs |
| `step_blocked` | Phase cannot continue without data, harness, budget, or approval |
| `step_failed` | Infrastructure or unrecoverable handler failure |
| `decision_recorded` | Terminal or iterate decision created |
| `job_cancelled` | User/policy cancellation requested |

`sequence` is scoped to `job_id`. It is the canonical ordering key for trace
rendering and replay.

Design rules:

- `tuning_jobs` is mutable current state.
- `tuning_events` is append-only evidence.
- `checkpoint_json` stores compact refs and counters, not full benchmark output.
- Large outputs are stored as artifact refs.
- Updates use optimistic concurrency through `version`.

## Repository Contract

Mirror the shape of `PlanRepository` and `DurableTaskStore`, but keep tuning
domain entities first-class.

```rust
#[async_trait]
pub trait TuningJobRepository: Send + Sync {
    async fn create(&self, user_id: &str, session_id: Option<&str>, spec: TuningSpec)
        -> Result<TuningJob, TuningError>;

    async fn get(&self, user_id: &str, job_id: &str)
        -> Result<Option<TuningJob>, TuningError>;

    async fn list(&self, user_id: &str, filter: TuningJobFilter)
        -> Result<Vec<TuningJobSummary>, TuningError>;

    async fn checkpoint(
        &self,
        job_id: &str,
        expected_version: u64,
        phase: TuningPhase,
        checkpoint: serde_json::Value,
        progress_pct: u8,
        current_step: Option<&str>,
    ) -> Result<TuningJob, TuningError>;

    async fn transition(
        &self,
        job_id: &str,
        expected_version: u64,
        status: TuningStatus,
        phase: TuningPhase,
        error_message: Option<&str>,
    ) -> Result<TuningJob, TuningError>;

    async fn append_event(
        &self,
        job_id: &str,
        event_type: &str,
        payload: serde_json::Value,
        evidence_refs: Vec<String>,
    ) -> Result<String, TuningError>;

    async fn cancel(&self, user_id: &str, job_id: &str)
        -> Result<TuningJob, TuningError>;
}
```

Owner checks should follow plan/task APIs: return not found for non-owned jobs.

Repository transaction rules:

- `create` inserts `tuning_jobs` and appends `job_created` in one transaction.
- `checkpoint` updates `tuning_jobs.version = version + 1`; reject if
  `expected_version` does not match.
- `transition` updates status/phase and terminal timestamps in the same
  optimistic-concurrency write.
- `append_event` allocates the next `sequence` for `(job_id)` and inserts one
  event. Cloud SQL implementation should do this inside a transaction or with a
  sequence-safe query.
- Controller code should prefer `append_event + checkpoint/transition` as one
  repository helper once the SQL implementation exists, so an event and state
  transition cannot drift.

Minimal domain structs:

```rust
pub struct TuningJob {
    pub job_id: String,
    pub trace_ref: String,
    pub user_id: String,
    pub session_id: Option<String>,
    pub status: TuningStatus,
    pub phase: TuningPhase,
    pub spec: TuningSpec,
    pub checkpoint: Option<TuningCheckpoint>,
    pub progress_pct: u8,
    pub current_step: Option<String>,
    pub selected_candidate_ref: Option<String>,
    pub decision_ref: Option<String>,
    pub budget_used: BudgetUsed,
    pub version: u64,
}

pub struct TuningCheckpoint {
    pub active_phase: TuningPhase,
    pub round: u32,
    pub baseline_run_ref: Option<String>,
    pub failure_cluster_refs: Vec<String>,
    pub hypothesis_refs: Vec<String>,
    pub candidate_refs: Vec<String>,
    pub evaluation_refs: Vec<String>,
    pub budget_used: BudgetUsed,
}
```

## State Machine

MVP phases:

| Phase | Handler | Output |
| --- | --- | --- |
| `created` | Validate/default spec | `spec_accepted` event |
| `baseline` | Run baseline harness | baseline evaluation ref |
| `diagnosing` | Build failure clusters | failure cluster refs |
| `hypothesizing` | Generate hypotheses | hypothesis refs |
| `generating_candidates` | Materialize bounded candidates | candidate artifact refs |
| `evaluating` | Run candidate benchmark | candidate evaluation refs |
| `deciding` | Rank, gate, and choose outcome | tuning decision ref |
| `completed` | Terminal success | decision summary |
| `rejected` | No acceptable candidate | rejection reason |
| `not_converged` | Search made no meaningful progress before stop condition | convergence summary |
| `blocked` | Missing approval, harness, or data | blocked reason |
| `failed` | Infrastructure or unrecoverable error | error message |
| `cancelled` | User or policy cancelled | cancellation event |
| `superseded` | Replaced by a newer generation or job | replacement ref |

Terminal statuses:

- `completed`
- `rejected`
- `not_converged`
- `blocked`
- `failed`
- `cancelled`
- `superseded`

`blocked` is terminal for controller execution until explicit resume or spec
change. It is not a hidden retry loop.

## Step Attempt Events

Every material step should write at least one event. Long steps may write
`started` and `finished` events.

Event payload shape:

```json
{
  "step": {
    "step_ref": "urn:astra:reconcile:local:tune_01H_step_eval_01",
    "name": "candidate_evaluation",
    "attempt": 1,
    "state": "succeeded",
    "input_refs": [
      "urn:astra:candidate:local:cand_prompt_01H..."
    ],
    "output_refs": [
      "urn:astra:evaluation:local:eval_cand_01H..."
    ],
    "budget_delta": {
      "cases": 300,
      "cost_usd": 4.25
    },
    "decision": null,
    "message": "Candidate evaluation completed on development split."
  }
}
```

Allowed step states:

- `started`
- `succeeded`
- `failed`
- `blocked`
- `skipped`
- `reused`
- `cancelled`

`reused` is important for idempotent resume: it means the controller found a
valid existing output ref and advanced without repeating work.

## JobController Rules

The MVP has one `JobController`.

Each tick:

1. Load job by id.
2. If terminal, stop.
3. Check cancellation.
4. Read compact checkpoint.
5. Append a `started` event for the selected step, unless the step is reused.
6. Execute exactly one material phase.
7. Append `succeeded`, `reused`, `blocked`, or `failed` with evidence refs.
8. Save checkpoint and phase/status transition with expected version.
9. Return.

Idempotence requirements:

- If a baseline evaluation ref already exists, do not rerun baseline.
- If failure cluster refs already exist, reuse them.
- If candidate refs already exist for the round, do not regenerate them.
- If candidate evaluation refs already exist, do not rerun unless explicitly
  invalidated.
- If decision ref exists, treat the job as terminal.

This makes crash/retry/resume safe enough for MVP.

Traceability requirements:

- Every phase transition has an event.
- Every event has `trace_ref`.
- Every material step should have a stable `step_ref`.
- Every generated artifact, evaluation, cluster, hypothesis, or decision appears
  in `output_refs`.
- Every decision cites input refs and evidence refs.

Controller pseudocode:

```rust
async fn tick(job_id: &str) -> Result<TuningJob, TuningError> {
    let job = repo.get_for_update(job_id).await?;
    if job.status.is_terminal() {
        return Ok(job);
    }
    if job.status == TuningStatus::Cancelled {
        return repo.transition_terminal(job, TuningStatus::Cancelled).await;
    }

    let step = select_step(&job);
    if step_output_exists(&job, step).await? {
        repo.append_step_event(&job, step, StepState::Reused, refs).await?;
        return repo.advance_reused(job, step).await;
    }

    repo.append_step_event(&job, step, StepState::Started, vec![]).await?;
    match run_step(&job, step).await {
        Ok(output) => {
            repo.append_step_event(&job, step, StepState::Succeeded, output.refs).await?;
            repo.checkpoint_and_advance(job, output).await
        }
        Err(TuningError::Blocked(reason)) => {
            repo.append_step_event(&job, step, StepState::Blocked, reason.refs).await?;
            repo.transition_blocked(job, reason).await
        }
        Err(err) => {
            repo.append_step_event(&job, step, StepState::Failed, err.refs()).await?;
            repo.transition_failed(job, err).await
        }
    }
}
```

## Checkpoint Shape

Keep checkpoint small:

```json
{
  "active_phase": "evaluating",
  "round": 1,
  "baseline_run_ref": "urn:astra:evaluation:local:eval_base_01H...",
  "failure_cluster_refs": [
    "urn:astra:failure_cluster:graph:fc_schema_linking_01H..."
  ],
  "hypothesis_refs": [
    "urn:astra:hypothesis:local:hyp_schema_linking_01H..."
  ],
  "candidate_refs": [
    "urn:astra:candidate:local:cand_prompt_01H..."
  ],
  "evaluation_refs": [
    "urn:astra:evaluation:local:eval_cand_01H..."
  ],
  "budget_used": {
    "rounds": 1,
    "candidates": 3,
    "cost_usd": 4.25
  }
}
```

## API Surface

```text
POST /tuning/jobs
GET  /tuning/jobs
GET  /tuning/jobs/{job_id}
POST /tuning/jobs/{job_id}/start
POST /tuning/jobs/{job_id}/cancel
GET  /tuning/jobs/{job_id}/events
GET  /tuning/jobs/{job_id}/trace
```

`POST /tuning/jobs/{job_id}/start` should enqueue/resume controller execution.
It should be safe to call repeatedly.

### API Payloads

Create request:

```json
{
  "session_id": "sess_01H00000000000000000000001",
  "spec": {
    "domain": "nl2sql",
    "mode": "benchmark_tuning",
    "objective": {
      "primary": {
        "metric": "exact_match_accuracy",
        "direction": "maximize",
        "target": 0.88,
        "min_delta": 0.02
      },
      "hard_constraints": {
        "prompt_tokens_max": 3500,
        "p95_latency_ms_max": 5000
      }
    },
    "benchmark": {
      "id": "nl2sql_eval_v3",
      "primary_split": "dev",
      "holdout_required": true
    },
    "allowed_change_types": [
      "prompt_patch",
      "skill_patch",
      "context_policy_patch"
    ],
    "search_budget": {
      "max_rounds": 3,
      "max_total_candidates": 12,
      "max_cost_usd": 20
    }
  },
  "source_signal_refs": [
    "urn:astra:signal:graph:sig_context_policy_01H00000000000000000000001"
  ]
}
```

Create/status response:

```json
{
  "job": {
    "job_id": "tune_01H00000000000000000000001",
    "trace_ref": "urn:astra:trace:local:tune_01H00000000000000000000001",
    "status": "running",
    "phase": "baseline",
    "progress_pct": 10,
    "current_step": "Running baseline on nl2sql_eval_v3.",
    "selected_candidate_ref": null,
    "decision_ref": null,
    "budget_used": {
      "rounds": 0,
      "candidates": 0,
      "cost_usd": 0
    },
    "version": 1
  }
}
```

Trace response:

```json
{
  "trace_ref": "urn:astra:trace:local:tune_01H00000000000000000000001",
  "events": [
    {
      "sequence": 1,
      "event_type": "step_succeeded",
      "step_ref": "urn:astra:reconcile:local:tune_01H_step_baseline_01",
      "attempt": 1,
      "phase": "baseline",
      "output_refs": [
        "urn:astra:evaluation:local:eval_base_01H00000000000000000000001"
      ]
    }
  ]
}
```

## CLI Behavior

`/tuning` should render from durable state.

Example initial output:

```text
Created tuning job tune_01H...
phase: baseline
status: running
next: run baseline on nl2sql_eval_v3
```

Example status output:

```text
tune_01H...  running · round 1 · evaluating
baseline: eval_base_01H...
clusters: 3
candidates: 5
metric: baseline available · candidates pending
trace: urn:astra:trace:local:tune_01H...
budget: rounds=1/3 candidates=5/12 cost=$7.14/$20
```

Trace output should be compact by default:

```text
1 spec_accepted        succeeded  event_01H...
2 baseline             succeeded  eval_base_01H...
3 failure_clustering   succeeded  3 clusters
4 hypothesis_generation succeeded  4 hypotheses
5 candidate_generation succeeded  5 candidates
6 candidate_evaluation running    2/5 complete
```

## TUI Mode

`/tuning` should stay lightweight, closer to `/plan` than to a full task board.
The default mode is a compact view of one active tuning loop. A separate
dashboard is available when the user wants to browse multiple jobs.

```text
/tuning            -> lightweight active tuning mode
/tuning dashboard  -> multi-job dashboard
```

### Entry and Exit

| User Action | Behavior |
| --- | --- |
| `/tuning` | Toggle lightweight tuning mode for the active or most recent job |
| `/tuning goal="..."` | Create + start job, then enter lightweight mode |
| `/tuning dashboard` | Open dashboard for recent jobs |
| `/tuning status <job>` | Render one-shot status in chat, no persistent mode |
| `/tuning trace <job>` | Render compact trace in chat or details pane |
| `/tuning cancel <job>` | Ask for confirmation if job is running |
| `/tuning edit <job>` | Open or prompt for a spec edit; creates replacement job in MVP |
| `Esc` or `/tuning` while active | Close mode; job keeps running |

Tuning mode should never own execution. It observes durable state and sends
start/cancel/resume requests.

### Lightweight Mode

The default `/tuning` mode should be one compact loop card, not a tabbed
workspace. It should show reliable facts, not fake precision.

User-friendly principles:

- Show what is known now, not what the system hopes to know later.
- Prefer `round + phase + latest event` over invented percentages.
- Keep normal progress out of the chat transcript.
- Ask for attention only on blocked, failed, completed, or approval-needed
  states.
- Keep one obvious next action.

Draft mode:

```text
Tuning draft
Goal    Improve NL2SQL generation
Need    benchmark or diagnosis-only mode

Choose  [1] nl2sql_eval_v3   [2] diagnosis only   [q] cancel
```

Running mode:

```text
Tuning  tune_01H...  running · round 1 · evaluating
Goal    Improve NL2SQL accuracy to target
Metric  exact_match_accuracy unavailable until harness completes
Budget  rounds 1/3   candidates 5/12   cost $7.14/$20

Steps   baseline ✓  clusters ✓  hypotheses ✓  candidates ✓  eval 2/5

Latest  candidate_evaluation running · 2 of 5 complete
Next    wait for evaluation results

r refresh   t trace   d dashboard   c cancel   q close
```

This view should fit in the bottom pane or a compact modal. It should not
replace the whole conversation unless the user opens the dashboard.

Do not show `54%` unless progress is derived from a deterministic step counter.
For many tuning jobs, phase and round are more honest than a percentage.

Use:

```text
running · round 1 · evaluating
```

instead of:

```text
evaluating 54%
```

unless the job can explain exactly how the percentage was computed.

Metric display should be capability-aware:

| Metric State | Render |
| --- | --- |
| Not computable yet | `metric unavailable until harness completes` |
| No harness | `metric unavailable · missing harness` |
| Baseline only | `baseline 0.81 · candidates pending` |
| Candidate evaluated | `baseline 0.81 · best candidate 0.86` |
| Non-numeric objective | `goal check pending` or `qualitative review required` |

### Dashboard

The dashboard is the task-board-like view for multiple jobs.

```text
┌ Tuning Dashboard ───────────────────────────────────────────────────────┐
│ tune_01H  running    r1 evaluating   NL2SQL accuracy target             │
│ tune_01G  blocked    r0 diagnosing   Missing benchmark harness          │
│ tune_01F  completed  r1 deciding     prompt_patch selected              │
└────────────────────────────────────────────────────────────────────────┘
```

Dashboard columns:

| Column | Meaning |
| --- | --- |
| Job | Short job id |
| Status | `running`, `blocked`, `completed`, `failed`, etc. |
| Round/Phase | Current round and loop phase |
| Target | Short objective summary |
| Last | Latest event or blocker |

Dashboard actions:

- `enter`: open lightweight mode for selected job
- `t`: show trace
- `c`: cancel selected running job
- `r`: refresh
- `q`: close

### Dynamic Updates

Lightweight mode and dashboard should poll durable state and diff snapshots,
similar to the task board.

Recommended behavior:

- Poll `/tuning/jobs/{job_id}` every 3-5 seconds while the mode is visible.
- Poll `/tuning/jobs/{job_id}/events?after_sequence=N` for incremental trace
  updates.
- Dashboard polls `GET /tuning/jobs` every 5 seconds.
- Refresh immediately after local actions such as start/cancel.
- Highlight changed job rows or newly appended trace rows for a short TTL.
- Highlight phase changes in the header and step strip.
- Stop polling when the mode/dashboard is closed, unless a global status chip needs a
  lightweight active-job count.
- Do not inject chat messages for normal phase progress.
- Inject or surface attention only for `blocked`, `failed`, `completed`, or
  user-confirmation-required states.

Diff keys:

- Job row: `job_id`
- Trace event: `(job_id, sequence)`

### Status Rendering

Use stable status colors consistent with task/background views:

| Status | Color Intent | TUI Meaning |
| --- | --- | --- |
| `created` | neutral | Job exists but has not started |
| `running` | cyan | Controller is progressing |
| `blocked` | yellow | User/data/harness action required |
| `rejected` | dark gray | Completed with no acceptable candidate |
| `not_converged` | yellow | Search plateaued or exhausted improvement attempts |
| `completed` | green | Decision produced successfully |
| `failed` | red | Infrastructure or unrecoverable error |
| `cancelled` | dark gray | User/policy stopped the job |
| `superseded` | dark gray | Replaced by a newer job or generation |

Phase should be shown separately from status. Example: `running · evaluating`.

### Header and Status Chip

When a tuning job is active, the main status line can show a compact chip:

```text
tuning: r1 evaluating
```

Attention order:

1. `blocked`
2. `failed`
3. `running`
4. `created`
5. `completed` / `rejected` / `cancelled`

If multiple jobs are active, show the most recent non-terminal job and use
`/tuning dashboard` for the full view.

The chip should be quiet. It should not animate or steal focus. Use attention
color only when the job is blocked or failed.

### Trace Details

Trace is a drill-down, not the default view.

Trace event detail should show:

- `step_ref`
- `attempt`
- `state`
- input refs
- output refs
- budget delta
- evidence refs
- error/blocker message

Candidate/evaluation detail can be added later. MVP can show only refs and
short metric summaries.

Compact trace:

```text
1 spec_accepted          succeeded
2 baseline               succeeded  eval_base_01H...
3 failure_clustering     succeeded  3 clusters
4 hypothesis_generation  succeeded  4 hypotheses
5 candidate_generation   succeeded  5 candidates
6 candidate_evaluation   running    2/5 complete
```

### Blocked and Failed States

Blocked jobs should render an action-oriented row:

```text
blocked · missing benchmark harness nl2sql_eval_v3
next: configure harness or cancel job
```

Failed jobs should show:

- failing phase
- last event
- error message
- whether resume is allowed

The TUI should not auto-resume failed jobs. User action should be explicit.

### Unhappy Paths

The UI must make degraded states understandable without interrupting normal
chat.

| Situation | Status | User-Facing Render | Allowed Action |
| --- | --- | --- | --- |
| Metric not computable yet | `running` | `metric unavailable until harness completes` | wait, trace |
| Missing benchmark harness | `blocked` | `missing benchmark harness nl2sql_eval_v3` | configure, edit spec, cancel |
| Harness failed | `blocked` or `failed` | `harness failed · see trace` | retry/resume, edit spec, cancel |
| Candidate generation failed | `failed` | `candidate generation failed in round 1` | view trace, retry if supported |
| All candidates rejected | `rejected` | `no candidate passed gates` | view decision, create new job |
| No convergence | `not_converged` | `no meaningful improvement after 3 rounds` | stop, edit target/budget/search space, inspect trace |
| Budget exhausted | `rejected` or `blocked` | `budget exhausted: candidates 12/12` | increase budget with new generation, cancel |
| User cancels | `cancelled` | `cancelled · last phase evaluating` | reopen trace |
| CLI disconnects | unchanged | status chip disappears locally; job continues server-side | `/tuning status <job>` |
| Server restarts | `running` or recovered | `resuming from checkpoint` event if needed | wait, trace |
| Version conflict | transient error | `state changed; refreshing` | automatic refresh |
| Spec edited while running | `blocked` | `spec changed; restart required` | create new generation/job |
| Approval required | `blocked` | `approval required before apply` | approve, skip apply, cancel |

Spec modification rule:

- MVP should not mutate `spec_json` in place while a job is running.
- A user edit creates a new generation or a new job.
- The old job remains traceable and terminal as `cancelled`, `rejected`, or
  `superseded` once that status exists.

Resume rule:

- `blocked` may resume after the missing dependency is fixed.
- `failed` only resumes when the controller marks the phase as retryable.
- `cancelled`, `completed`, and `rejected` do not resume in MVP.

### No Convergence

No convergence is not an infrastructure failure. It means the loop ran but did
not find a candidate that meaningfully improved the target under the constraints.

Detect `not_converged` when any configured stop condition fires:

- `stop_when_no_improvement_rounds` is reached.
- `max_rounds` is reached without a passing candidate.
- `max_total_candidates` is exhausted without meaningful improvement.
- Best candidate improves one metric only by violating a hard constraint.
- Candidate scores oscillate without improving the Pareto frontier.

UI rendering:

```text
Tuning  tune_01H...  not converged · 3 rounds
Goal    Improve NL2SQL accuracy to target
Best    baseline available · best candidate did not pass gates
Reason  no meaningful improvement after 3 rounds

Next    inspect trace, relax constraints, expand search space, or stop

t trace   e edit target   d dashboard   q close
```

Controller behavior:

- Stop the loop; do not silently launch another round.
- Write a `decision_recorded` event with `decision = "not_converged"`.
- Persist the best candidate ref if one exists, even if rejected.
- Persist a short convergence summary:
  - baseline metrics
  - best candidate metrics
  - failed gates
  - consumed rounds/candidates/cost
  - dominant failure clusters
- Require a new generation or new job to continue with changed budget,
  constraints, allowed changes, or search strategy.

User choices should stay simple:

| Choice | Meaning |
| --- | --- |
| Stop | Accept that no useful candidate was found |
| Edit target | Create replacement job with different metric/threshold |
| Relax constraints | Create replacement job with larger budget or softer constraints |
| Expand search | Create replacement job with more allowed change types |
| Inspect trace | Show why candidates failed |

### TUI Data Model

Suggested UI state:

```rust
pub struct TuningModeState {
    pub active_job_id: Option<String>,
    pub job: Option<TuningJobSummary>,
    pub trace_events: Vec<TuningTraceEvent>,
    pub last_seen_sequence: u64,
    pub recent_event_ttl: Vec<(u64, std::time::Instant)>,
    pub last_refresh_error: Option<String>,
}

pub struct TuningDashboardState {
    pub jobs: Vec<TuningJobSummary>,
    pub selected_row: usize,
    pub recent_job_ttl: Vec<(String, std::time::Instant)>,
    pub last_refresh_error: Option<String>,
}
```

The TUI should store only renderable summaries and refs. It should not store full
benchmark outputs, prompt diffs, or large artifacts.

### Rendering Rules

- Keep rows stable by id/ref so reorder does not create visual noise.
- Truncate long refs but expose full refs in detail view.
- Never render full prompt diffs inline in the main panel.
- Show budget as `used/max`.
- Show constraints as pass/fail/unknown.
- Show trace events newest-last in trace detail so the loop reads in order.
- Use one-line summaries in list rows and detail panes for evidence.

## Edge and Server Execution

Preferred MVP:

- Server is source of truth for durable jobs.
- CLI can create/start/status/cancel through HTTP.
- Local-only fallback may use an in-memory or local JSON repository for
  development, but should keep the same repository contract.

Open scheduling choice:

- Server worker pool executes `JobController`.
- CLI may trigger `/start`, but should not own long-running optimization unless
  explicitly running in local-only mode.

If both Edge and Server can execute jobs, add leases before enabling concurrent
workers.

## Integration With Observation Plane

Every material tuning output should be visible as graph evidence:

| Tuning Output | Evidence Class |
| --- | --- |
| failure cluster | `inferred_evidence` |
| candidate artifact | `experimental_evidence` |
| evaluation run | `experimental_evidence` |
| tuning decision | `audit_evidence` |
| status/event | `audit_evidence` |

`reflect(topic="adaptation", facet="measurements")` should be able to explain:

- current job phase
- last event
- selected candidate
- why a candidate was rejected
- why a job did not converge
- which budget limit or gate blocked progress

## MVP Implementation Order

1. Add `TuningSpec`, `TuningJob`, `TuningPhase`, `TuningStatus`, and checkpoint
   structs.
2. Add `TuningJobRepository` trait and in-memory implementation.
3. Add SQL-backed repository and migrations for `tuning_jobs` and
   `tuning_events`.
4. Add API routes for create/list/get/start/cancel/events.
5. Add trace fields and `/tuning trace` / `GET /trace` rendering.
6. Add CLI `/tuning` parser and renderer.
7. Add lightweight TUI tuning mode and `/tuning dashboard` backed by durable
   status/events.
8. Add `JobController` with only `created -> baseline -> deciding` smoke path.
9. Add failure clustering and hypothesis generation.
10. Add candidate generation for `prompt_patch`, `skill_patch`, and
   `context_policy_patch`.
11. Add harness adapter for one benchmark.
12. Add decision ranking and acceptance gates.
13. Expose job events through the observation provider.

## Suggested PR Slices

1. **Schema and repository**  
   Add structs, repository trait, in-memory repo, SQL migration, SQL repo tests.

2. **API and CLI shell**  
   Add create/list/get/start/cancel/events/trace routes and `/tuning` rendering.
   `start` can only move `created -> baseline` with a stub event.

3. **TUI mode and dashboard**  
   Add lightweight tuning mode, dashboard state, polling, snapshot diff, trace
   drill-down, and status chip.

4. **Controller smoke path**  
   Implement idempotent `created -> baseline -> deciding` with fake harness
   output. This validates durable transitions, checkpointing, trace rendering,
   and restart safety.

5. **Real harness adapter**  
   Wire one benchmark adapter and persist baseline/candidate evaluation refs.

6. **Failure cluster and hypothesis generation**  
   Add failure clustering from observation/evaluation evidence and produce
   auditable hypothesis refs.

7. **Candidate generation and decision**  
   Generate bounded candidates, evaluate, rank, and emit `TuningDecision`.

8. **Observation integration**  
   Add provider support so `reflect(topic="adaptation", facet="measurements")`
   can explain job state and decisions.

## Non-Goals for MVP

- Automatic apply.
- Promotion.
- Finalizers.
- Multi-controller split.
- Tool implementation patches.
- Model routing changes.
- Cross-session production learning.
- Unbounded forensic graph rendering.

## Key Invariants

- `/tuning` always returns a durable job id.
- Every controller phase writes an event.
- Every job has a `trace_ref`.
- Every material step has a step event.
- Every candidate/evaluation/decision has stable refs.
- Controller ticks are idempotent.
- Spec changes create a new generation or new job.
- Apply is outside Phase 3 MVP.
