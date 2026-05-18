# Three-Tier Task System — Design Plan

**Status**: Draft for review — no code changes yet.
**Model**: Claude Code's task/agent system, **UX parity required**, implementation adapted to astra's edge-cloud architecture + MatrixOne.
**Branch context**: `enhance_ui_1`. Unrelated uncommitted changes for `memory_extraction_runner` are **not** in scope here.

---

## Guiding principle

> **UX must not be worse than Claude Code. Implementation leans into our architecture.**

Claude Code is a single-process local CLI. We are:
- Edge + cloud — a session's turn can run on either side; tools can dispatch cross-node.
- MatrixOne-bound — MySQL-protocol HTAP with vector, full-text, **git4data**, **stage**, **pubsub**, datalink. These are first-class, not afterthoughts.
- Dual frontend — CLI agent and cloud agent must see the same task state.

Several Claude-Code shortcuts (pure in-memory todos, local-only spill files, `tokio::sync::Notify` for completion) break UX the moment an operation crosses the edge/cloud boundary. We pay the MatrixOne integration cost upfront so the multi-node UX works from day one.

---

## Scope

In scope:
- **Tier 1** — agent-facing todo scratchpad.
- **Tier 2** — background execution handles for `bash` and `spawn_agent` with post-turn notification injection.
- **Tier 3** — subagent orchestration via `spawn_agent`, optional model override, optional git-worktree isolation, location-aware routing.
- **Cross-node consistency** — the CLI and the cloud see the same todo list, the same background-task registry, and the same completion notifications for a given session.

Out of scope:
- `services::task_orchestrator::MatrixOneTaskService` / `services::durable_task` — user-initiated long tasks with checkpoint+resume; different product surface.
- Edge ↔ cloud task lease negotiation (`task_lease_service`) — unaffected.
- Adaptive tuning driven off task completion (future).
- Uncommitted `memory_extraction_runner` work on this branch.

## UX parity table (not worse than Claude Code)

| UX | Claude Code | This design |
|---|---|---|
| Same-session todo visible inside current process | ✅ | ✅ |
| **Cross-client / cross-device todo visible** | ❌ (single-process) | ✅ MO-backed |
| **CLI-created task visible from cloud agent and vice versa** | N/A | ✅ MO + pubsub |
| Background bash, poll later | ✅ local | ✅ cross-node via MO registry + stage |
| **Stop a cloud-launched background task from the CLI** | N/A | ✅ cancel propagated via pubsub |
| Next-turn auto-injection of completion notice | ✅ local only | ✅ regardless of where the turn continues |
| Subagent parallel spawn | ✅ | ✅ + location routing (`local` / `remote` / `auto`) |
| Turn-level snapshot/rollback of todos | ✅ | ✅ snapshot cache, MO is source of truth |
| Durability across server restart | ✅ (local state survives) | ✅ MO survives any node restart |

---

## Dependency map

```
Tier 1 (session_todos table + cached manager)
    └─ schema shared by Tier 2 notification bodies
Tier 2 (bg_tasks registry + stage spill + pubsub)
    ├─ depends on: Tier 1 for auto-todo linkage
    ├─ reuses: astra-sandbox::process_isolation::execute_isolated (kill_on_drop)
    ├─ reuses: runtime::orchestration::spawner::DynamicAgentSpawner (wraps as BgTaskKind::Agent)
    └─ new: MatrixOne tables, stage upload, per-session pubsub channel
Tier 3 (spawn_agent with location routing)
    ├─ depends on: Tier 2 for background mode
    ├─ reuses: ServerSkillSubRunExecutor core + AgentRegistry built-ins
    ├─ reuses: existing edge↔cloud delegation routing
    └─ new: GenericSubAgentRunner extraction + location resolver
Turn-flow integration
    └─ new: pubsub subscription on AgenticLoopState init → drains into volatile_pending
```

---

## 1. Current astra baseline

All references file:line, verified.

### 1.1 Tool registry and dispatch
- Schemas centrally defined in `rust/crates/astra-tools/src/schemas.rs::all_tool_schemas_core`.
- Tool visibility is capability-driven via `astra-turn-core::tool_surface`; legacy edge/cloud tool-name allowlists have been removed.
- Server dispatch: `runtime/src/server/server_tool_executor.rs::execute_local_with_metadata`, match arm on tool name.
- Edge dispatch: `astra-tools/src/executor.rs::DefaultToolExecutor::dispatch`.
- Unified `task` tool already registered with `action: create|update|list|get|stop`.

### 1.2 Tier 1 — todo scratchpad (already ~95% present, but wrong storage for our arch)
`astra-tools/src/task_mgmt.rs`:
- `pub struct TaskManager` + `pub struct TaskManagerSnapshot` (lines 52, 58). In-memory `Mutex<Vec<SessionTask>>` with snapshot/restore.
- Field set matches Claude Code (`id, title, description, active_form, status, owner, metadata, blocks, blocked_by, subtasks, created_at, updated_at`).
- Cycle detection on `add_blocks` / `add_blocked_by`.
- Auto-complete-on-all-subtasks-complete.

**Duplication**: `runtime/src/server/server_tool_executor.rs:183` has `ServerTaskManager`, `Arc<ServerTaskManager>` stored at 987/1096, duplicate `task_create`/`task_list` at 2207/2222.

**Storage gap**: both implementations are process-local. CLI-created todos are invisible to the cloud agent and vice versa.

### 1.3 Tier 2 — background execution (mostly absent)
- `server_bash` is synchronous. `execute_isolated` uses `kill_on_drop: true` — cancellation is free once we own the `JoinHandle`.
- No task registry, no spill, no poll endpoint.
- `AgenticRunLifecycleService::background_task_count` counts `JoinSet` entries for local drain only, not model-visible.
- Background *agents* work via `DynamicAgentSpawner` (`runtime/src/orchestration/spawner.rs`) — completion_notifiers `HashMap<String, Arc<Notify>>`, `wait_for_agent`, `get_agent_result`. Local-only.

**Gap**: unified cross-node registry with `task_output(task_id, wait_ms?)` and `task_stop(task_id)` surface.

### 1.4 Tier 3 — subagents (already ~90% for local)
- `SubRunExecutor` trait + `SubRunConfig` in `runtime/src/server/delegation_engine.rs`.
- `ServerSubRunExecutor` (`run_lifecycle.rs`) — multi-agent delegation (FanOut / Pipeline / Adversarial).
- `ServerSkillSubRunExecutor` (`server_skill_subrun.rs`) — skill forks; fresh messages; tool allowlist via `restricted_tools`; `SUBRUN_MAX_TURNS = 30`; workspace under `ASTRA_SERVER_WORKSPACES`.
- Built-ins: `orchestration_builtin_agents::get_builtin_agent_types` → `explore`, `code-review`, `task`, `general-purpose`.
- Custom types: `AgentRegistry::discover(project_root)` over project team config.
- Recursion guard: `checked_child_recursion_depth`, `MAX_AGENT_RECURSION_DEPTH`.
- Worktree isolation: CLI-only (`astra-cli/src/edge_tools/worktree.rs::WorktreeManager`, `git_gix::worktree_add_with_metadata`). Server does directory isolation only.
- Two tool surfaces: legacy unified `agent` tool + dedicated `spawn_agent` schema (`orchestration_spawn_tool.rs`; `SpawnAgentInput` field order load-bearing for prompt-cache).
- Edge↔cloud delegation already has routing plumbing in `delegation_engine` — we extend, not rebuild.

### 1.5 Session storage + MatrixOne capability inventory
- `astra_services::SessionArtifactStore`: `LocalSessionArtifactStore` (files), `DatabaseSessionArtifactStore` (MatrixOne).
- `services::session_journal::JournalEventType` — append-only per-session event log including `PlanProgress`. Natural cold-write companion.
- MatrixOne relevant features:
  - **Standard tables** — `session_todos`, `bg_tasks` will go here.
  - **Stage** — object storage for large blobs (`>= N MB` task outputs). Accessible by any node via stage path.
  - **Pubsub** — per-session channel for completion notices; both edge and cloud subscribe.
  - **Full-text + vector** — not used by task system v1.

### 1.6 Turn lifecycle + injection points
- `AgenticLoopState::volatile_pending: Vec<VolatileSnapshotEntry>`.
- `introspect::render_volatile_pending` + `injection_tracking::VolatilePending` fingerprint.
- Exact hook for `<task-notification>` injection.

### 1.7 Bash execution path
`bash` tool call → server/edge dispatch → `execute_isolated(config, cmd) → IsolatedOutput { stdout, stderr, exit_code, duration, timed_out }` → `format_server_bash_output`. All synchronous. Tier 2 wraps step 3.

---

## 2. Data model

### 2.1 Tier 1 — todo scratchpad (MO-backed, cached)

**Decision**: MatrixOne is source of truth; each loop host holds a write-through cache. Both CLI and cloud read/write the same table for a given session_id. Eliminates the cross-client visibility gap.

Schema:

```sql
CREATE TABLE session_todos (
    session_id       VARCHAR(64)   NOT NULL,
    todo_id          VARCHAR(64)   NOT NULL,
    title            VARCHAR(512)  NOT NULL,
    description      TEXT          NULL,
    active_form      VARCHAR(512)  NULL,
    status           VARCHAR(16)   NOT NULL,     -- pending|in_progress|completed|failed|cancelled|deleted
    owner            VARCHAR(128)  NULL,
    metadata         JSON          NULL,
    blocks           JSON          NULL,         -- array of todo_id
    blocked_by       JSON          NULL,
    subtasks         JSON          NULL,
    created_at       TIMESTAMP     NOT NULL,
    updated_at       TIMESTAMP     NOT NULL,
    created_by_node  VARCHAR(64)   NULL,         -- 'edge:<id>' | 'cloud:<id>'
    PRIMARY KEY (session_id, todo_id),
    INDEX idx_session_status_updated (session_id, status, updated_at)
);
```

Design notes (CLAUDE.md §3, §5):
- Primary key matches retrieval pattern (always filter by `session_id`).
- Single secondary index covers "list open todos in this session ordered by recency" — the only non-PK query the manager makes.
- `status` is an explicit enum column; no implicit flags.
- `created_at` / `updated_at` on every row.
- No `WHERE` on the JSON columns (`metadata` / `blocks`) — they're read-only payload. CLAUDE.md §5 compliance.
- No `SELECT *`. Every read is column-scoped.

`TaskManager` becomes a trait:

```rust
#[async_trait]
pub trait TaskManager: Send + Sync {
    async fn create(&self, session_id: &str, args: Value) -> Result<Value>;
    async fn list(&self, session_id: &str, args: Value) -> Result<Value>;
    async fn get(&self, session_id: &str, todo_id: &str) -> Result<Value>;
    async fn update(&self, session_id: &str, todo_id: &str, args: Value) -> Result<Value>;
    async fn stop(&self, session_id: &str, todo_id: &str) -> Result<Value>;
    fn snapshot(&self, session_id: &str) -> TaskManagerSnapshot;
    fn restore(&self, session_id: &str, snapshot: TaskManagerSnapshot);
}
```

Implementations:
- `InMemoryTaskManager` — unit tests, offline CLI mode.
- `MatrixOneTaskManager` — production. Write-through: mutations hit MO first, then update per-session in-memory cache. Reads hit cache; cache invalidation on pubsub `todos_changed` event (optional; MVP can read-through on cache miss only).

Snapshot/restore for turn rollback:
- Snapshot captures the in-memory cache state (small; one session's todos).
- Restore writes the captured state back to MO in a single transaction. Rollback is rare — turn abort, not normal flow — so the DB cost is acceptable.

Status transitions same as current code. `stop` → `cancelled`. `update { status: "deleted" }` → `deleted`.

### 2.2 Tier 2 — background task registry (MO + stage + pubsub)

**Decision**: metadata + tail-64 KiB in MO; full logs to stage.

Two tables (append-only, per CLAUDE.md §4):

```sql
-- Current-state projection. One row per task. Updated on each state transition.
-- UPDATE frequency is low (O(5) per task lifecycle) — acceptable.
CREATE TABLE bg_tasks (
    task_id            VARCHAR(64)   NOT NULL,      -- "bg-<session>-<nanos>"
    session_id         VARCHAR(64)   NOT NULL,
    kind               VARCHAR(16)   NOT NULL,      -- 'bash' | 'agent'
    command_or_prompt  TEXT          NOT NULL,      -- bash command, or spawn_agent prompt summary
    status             VARCHAR(16)   NOT NULL,      -- starting|running|completed|failed|stopped|timed_out
    executor_node_id   VARCHAR(64)   NOT NULL,      -- node that owns the spill & process
    exit_code          INT           NULL,
    started_at         TIMESTAMP     NOT NULL,
    finished_at        TIMESTAMP     NULL,
    timeout_ms         BIGINT        NULL,
    tail_stdout        MEDIUMTEXT    NULL,          -- last 64 KiB, updated on terminal
    tail_stderr        MEDIUMTEXT    NULL,
    stage_stdout_path  VARCHAR(512)  NULL,          -- MO stage path, populated on finalize
    stage_stderr_path  VARCHAR(512)  NULL,
    linked_todo_id     VARCHAR(64)   NULL,          -- optional auto-todo
    created_by_node    VARCHAR(64)   NOT NULL,
    parent_task_id     VARCHAR(64)   NULL,          -- for agent→agent spawn
    PRIMARY KEY (task_id),
    INDEX idx_session_status (session_id, status),
    INDEX idx_executor_node_status (executor_node_id, status)
);

-- Append-only audit trail of state transitions + error bodies.
-- Never UPDATE / DELETE this table. Retention via partition/ttl, out of scope here.
CREATE TABLE bg_task_events (
    task_id        VARCHAR(64)   NOT NULL,
    seq            INT           NOT NULL,
    event_type     VARCHAR(24)   NOT NULL,          -- state_transition | stderr_chunk | error
    status         VARCHAR(16)   NULL,
    body           TEXT          NULL,
    occurred_at    TIMESTAMP     NOT NULL,
    PRIMARY KEY (task_id, seq)
);
```

**Why this split:** CLAUDE.md §4 says "prefer append + soft-delete". `bg_tasks` is mutated at most 4-5 times per task across its whole life (Starting → Running → Completed/Failed/Stopped/TimedOut), not per-chunk — that's not a hot table. `bg_task_events` is strictly append-only; it's where any high-frequency writes (future stderr chunking) would go.

Output storage tiers:
1. **Live tail buffer** — ring buffer in executor-node memory, flushed to local spill file.
2. **Tail-64 KiB snapshot** — written to `bg_tasks.tail_stdout / tail_stderr` on terminal transition. This is what `task_output` returns on the fast path.
3. **Full log archive** — on terminal transition, local spill file uploaded to MO stage at path `tasks/<session_id>/<task_id>/{stdout,stderr}.log`; path written back to `bg_tasks.stage_stdout_path`. Caller requesting full output (`task_output { full: true }`) reads from stage — works across nodes.

State machine, forward-only, `try_transition` pattern from `RunStatus::try_transition`:

```
Starting → Running → Completed
                   → Failed
                   → Stopped      (via task_stop)
                   → TimedOut     (sandbox timeout)
```

Live task entry (executor-node memory; wraps MO row):

```rust
pub struct BgTaskEntry {
    task_id: String,
    session_id: String,
    kind: BgTaskKind,
    status: AtomicU8,
    cancel: CancellationToken,
    stdout_spill: PathBuf,       // local only; finalized to stage
    stderr_spill: PathBuf,
    join_handle: Mutex<Option<JoinHandle<()>>>,
    // MO-sync bookkeeping
    last_synced_status: AtomicU8,
}

pub enum BgTaskKind {
    Bash { command: String, timeout: Duration },
    Agent { agent_id: String },   // wraps DynamicAgentSpawner handle
}
```

Cross-node `task_output`:
1. Resolve `executor_node_id` by `task_id` PK lookup.
2. If `executor_node_id == self`: return live ring-buffer tail.
3. Else if terminal + `stage_stdout_path` set: read from stage.
4. Else if terminal + no stage path: return `tail_stdout` column (safety net for small outputs).
5. Else (remote non-terminal): return MO row snapshot (status + partial tail). Non-terminal remote live streaming is **not** MVP — documented limitation.

Cross-node `task_stop`:
1. PK lookup → `executor_node_id`.
2. Publish `astra.tasks.<session_id>` pubsub message `{ op: "cancel", task_id }`.
3. Executor node's registry listener receives → triggers `entry.cancel.cancel()` → `kill_on_drop` for bash / cancellation token check for agent.
4. Transition writes back to `bg_tasks` + `bg_task_events`.

### 2.3 Tier 3 — subagents with location routing

Reuse `AgentRegistry` + `get_builtin_agent_types()` as-is. `AgentTypeDefinition { agent_type, system_prompt, allowed_tools, model }` is sufficient.

**New**: `SpawnAgentInput.run_location: Option<RunLocation>` appended at the tail (cache-safe).

```rust
pub enum RunLocation {
    Local,        // run on the same node as the caller
    Remote,       // force the other node (cloud for CLI callers; edge for cloud callers)
    Auto,         // resolved by the location resolver (default)
}
```

Location resolver order:
1. Explicit `run_location` wins.
2. Tool-permission heuristic — if `allowed_tools` includes tools only available on one side (e.g., cloud-only MCP), route there.
3. Workload heuristic — if executor-node queue depth above threshold, route to the other side.
4. Default: same side as caller.

Existing edge↔cloud delegation routing in `delegation_engine` is the execution substrate. We add the resolver in front and surface it as `run_location`.

Tool-scope enforcement: `AgenticLoopState::restricted_tools` (already live in skill path) receives `AgentTypeDefinition.allowed_tools`.

Context isolation (unchanged from prior plan):
- Fresh `messages` (no parent bleed).
- Model: `SpawnAgentInput.model` > agent-type default > inherit parent.
- Opt-in prompt-cache prefix via `inherit_prefix`.

**Worktree isolation**: promote `astra-cli/src/edge_tools/worktree.rs::WorktreeManager` to shared `astra-workspace` (or fold into `astra-services::workspace`) so cloud-side `spawn_agent { isolated: true }` runs `git worktree add` too. Non-git projects fall back to directory isolation.

Background subagents route through Tier 2 registry: a `spawn_agent { run_in_background: true }` call creates both a `bg_tasks` row (kind=agent) and a `DynamicAgentSpawner` handle; `task_output(task_id)` resolves to the agent's cumulative output; `task_stop(task_id)` cancels the loop via MO pubsub.

---

## 3. Tool-layer exposure

### 3.1 Registration changes (`astra-tools/src/schemas.rs`)

| Tier | Change |
|---|---|
| Tier 1 | Keep unified `task` as alias; additionally register `task_create`, `task_list`, `task_get`, `task_update`, `task_stop`. |
| Tier 2 | New top-level `task_output`. `task_stop` already exists (Tier 1); dispatcher branches on id prefix. Add `run_in_background: bool` + optional `timeout_ms: u64` to `bash`. Add `run_in_background: bool` + `run_location` enum to `spawn_agent`. |
| Tier 3 | `spawn_agent` already registered; append `run_location` at the tail. Retire `agent.spawn` action one release after. |

Dispatcher:
- `DefaultToolExecutor::dispatch` (edge) — add `task_output`; branch `bash` on `run_in_background`.
- `ServerToolExecutor::execute_local_with_metadata` (cloud) — same; route `task_output` to `Arc<BackgroundTaskRegistry>`.

### 3.2 Foreground vs background bash

```json
"run_in_background": {
  "type": "boolean",
  "description": "If true, returns immediately with a task_id. Use task_output(task_id) to collect results.",
  "default": false
},
"timeout_ms": {
  "type": "integer",
  "description": "Absolute cap; default 600000 (10 min) for background, current behavior for foreground.",
  "minimum": 0
}
```

### 3.3 `task_output` schema

```json
{
  "name": "task_output",
  "parameters": {
    "type": "object",
    "properties": {
      "task_id":     { "type": "string" },
      "block":       { "type": "boolean", "default": false },
      "wait_ms":     { "type": "integer", "default": 0, "maximum": 600000 },
      "full":        { "type": "boolean", "default": false,
                       "description": "If true, returns full stage-backed log. Otherwise tail-64 KiB." }
    },
    "required": ["task_id"]
  }
}
```

`block=true, wait_ms>0`: blocks on the per-session pubsub channel until the task terminates or timeout expires.

### 3.4 Cache-break discipline

`SpawnAgentInput`, `SpawnAgentTooBashInput`, and any extended struct must append new fields at the tail — existing load-bearing comment in `orchestration_spawn_tool.rs:32`. Same rule for `bash`.

### 3.5 Inheritance rules (parent → child)

Unchanged vs. prior plan. Key points:
- Fresh `messages`.
- `allowed_tools` overrides agent-type default (not intersect).
- Cancellation token is hierarchical; parent cancel propagates via pubsub to child executor node.
- `RequestConstraints` + `forward_headers` inherited.
- Skill executor not inherited (prevents Fork→Fork).

---

## 4. Lifecycle and notification

### 4.1 Completion notification — MO pubsub, per-session channel

Channel naming: `astra.tasks.<session_id>`.

Publisher (executor node): on terminal transition,
1. Write `bg_tasks` row + `bg_task_events` entry.
2. Upload spill files to stage; update `stage_*_path`.
3. Publish JSON: `{ kind: "task_completed", task_id, status, exit_code, duration_ms, summary_tail: "<last 2KB>" }`.

Subscribers (every loop host):
1. `AgenticLoopState` init hooks a per-session subscription via a new `TaskCompletionInbox` struct.
2. `TaskCompletionInbox` holds a `mpsc::UnboundedReceiver<TaskCompletionNotice>` fed by the pubsub handler.
3. Between turn rounds (round-begin phase), drain the receiver into `state.volatile_pending` as a `VolatileSnapshotEntry { kind: "task_notification", body }`.
4. `injection_tracking::VolatilePending` dedupes.

Rendered body:
```
<task-notification>
task: bg-42 (bash) -> completed (exit 0, 1.3s)
tail:
  <...last N KB of stdout...>
</task-notification>
```

**Cross-node:** if the turn resumes on a different node than the one that ran the task, the notification still arrives — both nodes subscribe to the same per-session channel. This is the core property that makes our UX strictly better than Claude Code's single-process local story.

Subscription lifecycle:
- Subscribe when the loop host binds a session.
- Unsubscribe when the session terminates (`SessionEndState` signal in `session_journal`) or when the loop host drops.
- Leak-proof: any leaked subscription is reaped by `AgenticRunLifecycleService::drain_background_tasks_impl` extension.

### 4.2 Does the main turn block?

Same as Claude Code: no. `run_in_background: true` returns a task_id immediately. Voluntary block via `task_output { block: true, wait_ms: N }`. Non-block polling is default.

### 4.3 Integration with existing layers

- **Compaction** (`cloud_compact_prompt`, `microcompact`): `task_notification` entries are standard `volatile_pending` items. One-turn lifespan via existing dedup/render rules.
- **Prompt cache**: notifications render in the user-message tail after the cacheable prefix. No invalidation.
- **Turn finalization** (`agentic_loop_finalization.rs`): hook point drains remaining inbox as a final `<task-notification>` block so late-arriving completions aren't lost.
- **Harness snapshots** (feature `harness`): tasks surface as child snapshots via the existing `observe_only` sink.
- **Cloud sync** (`services::state_sync`, `sync_engine`): no new sync contract — Tier 1/2 state lives in MO directly, replication is the DB's job.

---

## 5. Failure modes and safety

| Failure | Mitigation |
|---|---|
| Runaway subagent recursion | `checked_child_recursion_depth` + `SUBRUN_MAX_TURNS` + `SUBRUN_MAX_CUMULATIVE_TOKENS` (skill path); add analogous cumulative-token cap to `DynamicAgentSpawner` when it feeds Tier 2. |
| Budget blowup across tasks | Per-session `Arc<AtomicU64>` budget tracker; reject new spawns at threshold; publish "budget exhausted" notice. |
| Executor-node restart | `bg_tasks.executor_node_id` is authoritative. On startup each node scans `WHERE executor_node_id = self AND status IN (starting, running)`; transitions them to `Failed` with `body = "executor restart"`; publishes completion. No in-flight task is silently orphaned. |
| Cloud DB hiccup | Pubsub is best-effort; MO row is source of truth. Loop host refreshes state on reconnect by polling `bg_tasks WHERE session_id = ? AND status NOT IN terminal AND updated_at > last_seen`. Gives at-least-once completion delivery. |
| Permission scope | Children inherit no tools automatically; `allowed_tools` + `AgentRegistry` gate. MCP servers: `request_constraints` flows down. |
| Cancellation propagation | `task_stop` → MO pubsub `{op: cancel}` → executor-node registry → `CancellationToken::cancel()` → bash `kill_on_drop` / agent loop checkpoint. |
| Circular todo edges | Existing BFS `add_blocks`/`add_blocked_by` check in `TaskManager::update`. Preserve in both impls. |
| Bash sandbox escape | Existing `astra-sandbox` ns isolation. Unchanged. |
| Worktree leak | Rollback entry in worktree rollback journal + `session_reaper` on session end. |
| Orphan tokio tasks on host drop | Extend `drain_background_tasks_impl` to also drain registry via `AbortHandle`. |
| Stage upload failure on finalize | Retry once; on persistent failure, keep `tail_*` in MO and set `stage_*_path = NULL`. `task_output { full: true }` degrades gracefully to "tail only — full log unavailable". |

---

## 6. Phased rollout

### Phase 0+1 — Consolidation + Tier 1 MO-backed (single PR, ~5-7 days)

Deliberately merged: a Phase 0 that ships `TaskMirror` as a seam would encode
the wrong invariant (in-memory as SoT). We go straight to MO-as-SoT in one PR
so the codebase never sits in a half-designed intermediate state.

Revert / delete first:
- Delete `ServerTaskManager` in `server_tool_executor.rs:183-586` and its
  duplicate `task_create` / `task_list` arms.
- Revert `TaskMirror` trait, `TaskManager::set_mirror`, `TaskManager::sync_mirror`,
  and the `RecordingMirror` test that were added on this branch — they model a
  "mirror" semantics that §10 no longer uses.

Refactor:
- `TaskManager` becomes `#[async_trait] pub trait TaskManager` (signature from §2.1)
  with `InMemoryTaskManager` (tests, offline CLI) and `MatrixOneTaskManager` impls.
- All callsites in `astra-tools/executor.rs`, `server_tool_executor.rs`,
  `server/state_builder.rs`, `app_state.rs` take `Arc<dyn TaskManager>` instead.
- Split tool names (`task_create / task_list / task_get / task_update / task_stop`),
  keep unified `task` as alias for one release.

New:
- `session_todos` migration (§2.1 schema).
- `MatrixOneTaskManager` write-through cache impl.
- Production wiring picks `MatrixOneTaskManager` when the MO pool is configured,
  `InMemoryTaskManager` only when CLI runs fully offline.

Contract tests (CLAUDE.md §6):
- CLI `task.create` → cloud `task.list` in the same `session_id` sees the row.
- Turn rollback via `TaskManager::snapshot` / `restore` round-trips through MO.
- `status=deleted` vs `status=cancelled` transitions write distinct event types.

### Phase 2 — Tier 2 bash background + pubsub (~5-7 days)
- Migrations: `bg_tasks`, `bg_task_events`.
- `BackgroundTaskRegistry` implementation, bash-only.
- `run_in_background: true` on `bash`; stage upload finalizer.
- Per-session pubsub channel `astra.tasks.<session_id>`; subscription in loop host.
- `TaskCompletionInbox` → `volatile_pending` injection.
- Cross-node `task_output` / `task_stop` routing via MO lookups + pubsub.
- Contract tests: edge-launched bash, completion seen on cloud-side resumed turn.

### Phase 3 — Tier 3 `spawn_agent` + background agent wrap + location routing (~5-7 days)
- Extract `GenericSubAgentRunner` from `ServerSkillSubRunExecutor` core.
- `SpawnAgentInput.run_location` (Local | Remote | Auto); resolver on top of existing edge↔cloud delegation.
- Background agent wrap as `BgTaskKind::Agent` → surfaces through `task_output / task_stop`.
- Promote `WorktreeManager` to shared crate; enable server-side `isolated: true`.
- Retire `agent.spawn` action (alias, deprecation notice).

### Phase 4 — Safety & polish (~2-3 days)
- Cross-task per-session budget ceiling.
- Executor-node startup reaper for in-flight-on-restart.
- Session-reaper spill cleanup on `SessionEndState`.
- Full contract suite under `runtime/tests/`.

**MVP = Phase 0 + 1 + 2.**
- Crates touched: `astra-tools`, `runtime`, `astra-cli`, `astra-turn-core`, `astra-services` (new migrations + pubsub handle).
- LOC: ~2,500-4,000 production (Phase 0 nets ~−400; MO manager + migration + wiring ~800; registry + stage + pubsub ~1,500; inbox + injection ~300; CLI plumbing ~200; tests 600-1,200).
- Engineer-days: ~10-14 for MVP.

---

## 7. Open questions / tradeoffs

### 7.1 Unified `task` tool vs split `task_*`
**Recommendation: split, keep unified as alias.** Claude Code parity + clearer tool-search, at ~60 extra schema tokens/turn (~0.1% cache impact).

### 7.2 ✅ RESOLVED — Tier 1 storage
MO-backed, not in-memory. Required for cross-client UX parity. See §2.1.

**Relationship to `docs/design/long-term-task-system.md` §10:** the "TaskService mirror"
language in §10 is superseded. There is no mirror, and `TaskManager` is not a
separate authoritative surface that gets replicated into MO. `session_todos` **is**
the authoritative store; in-memory is just a per-host cache. The durable worker
(executing an `agent_tasks` row queued via `astra task queue`) reads from
`session_todos` and **projects** progress one-way into its `agent_tasks.plan_json` /
`progress_pct`. Normal chat sessions — no `agent_tasks` row, no projection, no
mirror. This removes the `TaskMirror` trait and `set_mirror` / `sync_mirror` hooks
entirely; Phase 0 must revert the stub wiring those pieces already added to
`astra-tools/src/task_mgmt.rs`.

### 7.3 ✅ RESOLVED — Tier 2 output
Tail in MO, full log in stage. Metadata + 64 KiB tail live in `bg_tasks`; stage upload on finalize covers cross-node full-log access. See §2.2.

### 7.4 Worktree isolation rollout
Promote `WorktreeManager` in Phase 3 (not MVP). Git worktree for git repos; directory fallback otherwise.

### 7.5 Reuse `SubRunExecutor` vs new runner
Extract `GenericSubAgentRunner` from `ServerSkillSubRunExecutor`; leave `ServerSubRunExecutor` alone for multi-agent delegation. Phase 3.

### 7.6 ✅ RESOLVED — Pubsub channel granularity
Per-session channel `astra.tasks.<session_id>`. Clear authorization boundary, multi-tenant isolation trivial, dedup via `injection_tracking::VolatilePending`.

### 7.7 Tool-name collision — unified `task` vs `task_stop` top-level
Keep both; add deprecation note to unified. Delete unified once usage drops.

### 7.8 Pubsub reliability + at-least-once completion
Pubsub is best-effort. MO row is source of truth. Loop host reconciles on reconnect by polling non-terminal `bg_tasks` rows newer than `last_seen`. At-least-once guaranteed via MO read; de-dup via `VolatilePending` fingerprint.

### 7.9 ✅ RESOLVED — `run_location` default
**`Auto`**. Resolver order (repeated from §2.3):
1. Explicit `run_location` argument wins.
2. Tool-scope heuristic — `allowed_tools` containing node-specific tools pins the location.
3. Workload heuristic — executor queue depth above threshold diverts to the other side.
4. Default tiebreak: same side as caller.

Rationale: location is an implementation detail the agent rarely has signal to reason about. `Auto` lets the resolver pick without forcing every `spawn_agent` call to include a flag, and the four-step ordering keeps behavior predictable when the agent *does* care.

---

## Critical files for implementation

- `rust/crates/astra-tools/src/task_mgmt.rs`
- `rust/crates/astra-tools/src/schemas.rs`
- `rust/crates/astra-tools/src/executor.rs`
- `rust/crates/runtime/src/server/server_tool_executor.rs`
- `rust/crates/runtime/src/orchestration/spawner.rs`
- `rust/crates/runtime/src/server/server_skill_subrun.rs`
- `rust/crates/runtime/src/server/run_lifecycle.rs`
- `rust/crates/runtime/src/server/delegation_engine.rs`
- `rust/crates/runtime/src/turn/agentic_loop_host.rs`
- `rust/crates/runtime/src/turn/agentic_loop_finalization.rs`
- `rust/crates/astra-turn-core/src/orchestration_builtin_agents.rs`
- `rust/crates/astra-turn-core/src/orchestration_team_config.rs`
- `rust/crates/astra-turn-core/src/orchestration_spawn_tool.rs`
- `rust/crates/services/` — new migrations + pubsub handle module
- `rust/crates/astra-cli/src/edge_tools/worktree.rs` (promote to shared crate Phase 3)

---

## Top-3 architectural decisions (now user-confirmed)

1. **MO is source of truth** for Tier 1 todos and Tier 2 task metadata. In-memory is cache. Enables cross-client / cross-node UX that Claude Code structurally cannot provide.
2. **Tier 2 output: tail-in-MO + full-log-in-stage.** Spec-aligned with MatrixOne idioms, CLAUDE.md §4 compliant, cross-node reads solved.
3. **Completion notification via per-session pubsub.** Loop hosts subscribe on session bind; drain into `volatile_pending`. At-least-once via MO reconciliation fallback.

All major decisions resolved. Ready to start Phase 0.
