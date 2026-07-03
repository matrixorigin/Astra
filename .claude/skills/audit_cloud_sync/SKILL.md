---
name: audit-cloud-sync
description: "Audit Astra edge/cloud synchronization using local journals/checkpoints and current MatrixOne projection tables. Use for event ingestion gaps, restore/checkpoint mismatch, task projection drift, and skill/learning visibility issues."
user_invocable: true
when_to_use: "When the user wants to audit edge-cloud sync, event ingestion, restore behavior, checkpoint projection, task state, learning/preferences sync, or cloud/local data drift."
arguments:
  - name: TARGET
    description: "Session ID, run ID, task ID, or 'last'. Omit for most recent session."
    required: false
  - name: ASPECT
    description: "Audit focus: events, learning, checkpoints, tasks, skills, restore, or all. Default: all."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---

# Audit Cloud Sync

Audit current evidence, not a presumed sync diagram. Confirm the live schema and
service owner first, then compare local artifacts with MatrixOne projections.

## Task

$ARGUMENTS

## Phase 1: Confirm Current Owners

Before querying data, confirm table names and owner modules from the repo:

```bash
rg -n "CREATE TABLE IF NOT EXISTS (agent_events|agent_sessions|session_checkpoints|run_checkpoints|agent_tasks|task_contracts|verification_results|user_preferences|skills_registry)" rust/crates/services/src/storage.rs
rg -n "HybridRestoreService|RestoredSession|restore_recent_tools|session_checkpoints|run_checkpoints|learning_snapshots|skills_registry" rust/crates/services rust/crates/runtime rust/crates/astra-cli --glob '!target/**'
```

Reference document when the issue involves restore, checkpoints, or skill paths:
`rust/docs/edge-cloud-sync-architecture.md`, especially section 8.

Important current facts:

- `agent_events` and `agent_sessions` are core projection tables.
- `session_checkpoints` stores session checkpoints; `run_checkpoints` stores run checkpoint payloads.
- `agent_tasks`, `task_contracts`, and `verification_results` cover durable task state.
- `skills_registry` is the database-backed skill catalog for web/runtime skills.
- `user_preferences` is current; `session_sync_log` is intentionally dropped in schema setup and must not be used as proof of current sync health.

## Phase 2: Locate Local Evidence

Resolve the target:

```bash
ls -lt ~/.astra/sessions/*.jsonl 2>/dev/null | head
astra journal digest last --format json
astra journal digest <SESSION_ID> --format json
find ~/.astra/sessions/<SESSION_ID> -maxdepth 3 -type f | sort
```

Local evidence to collect:

| Evidence | Path |
| --- | --- |
| Event stream | `~/.astra/sessions/<session_id>.jsonl` |
| Digest summary | `astra journal digest <session_id> --format json` |
| Step checkpoints | `~/.astra/sessions/<session_id>/step_checkpoints/` |
| Composite snapshot index | `~/.astra/sessions/<session_id>/step_checkpoints/composite_snapshots.json` |
| Local workspace restore metadata | `~/.astra/sessions/<session_id>/workspace.yaml` when present |

## Phase 3: Query Cloud Evidence Only When Configured

Use the project's configured MatrixOne connection. Do not hardcode database names;
the service code resolves them through `astra_core::resolve_database_name`.

Suggested query dimensions:

| Aspect | Tables / predicates |
| --- | --- |
| Events | `agent_events` by `user_id`, `session_id`, `event_type`, `turn_id`, `run_id` |
| Session projection | `agent_sessions` by `user_id`, `session_id`, `event_count`, status fields |
| Session checkpoints | `session_checkpoints` by `user_id`, `session_id`, `number`, `turn` |
| Run checkpoints | `run_checkpoints` by `user_id`, `session_id`, `run_id`, `checkpoint_kind` |
| Durable tasks | `agent_tasks`, `task_contracts`, `verification_results` |
| Skills | `skills_registry` plus runtime/API `GET /skills` path if testing web visibility |
| Preferences | `user_preferences` |

If DB access is unavailable, produce a local-only audit and explicitly label cloud
evidence as skipped.

## Phase 4: Compare Invariants

Events:

- Local journal event count should explain `agent_sessions.event_count`; account for projection filtering before calling it loss.
- Critical trace events should appear in `agent_events` with owner-bound identity (`user_id`, event/run/session fields).
- Repeated local events missing in cloud usually indicate ingestion, ownership, or idempotency issues.

Checkpoints:

- Local checkpoint files and `session_checkpoints` should agree on session, turn, and summary/state availability.
- `run_checkpoints` should be used for run-scoped recovery, not confused with session rewind checkpoints.
- For restore bugs, compare `astra_services::session_restore::RestoredSession` with runtime step restore data; they are distinct layers.

Tasks:

- `agent_tasks.status`, task plan JSON, task contract status, and verification results must describe the same lifecycle.
- A completed task without verification evidence is a task lifecycle issue, not just sync lag.

Skills:

- Filesystem skills in `.claude/skills` or `.agent/skills` are local/catalog inputs.
- Web/runtime database-visible skills flow through `skills_registry` and capability selection.
- Do not assume a local filesystem skill is visible to remote runtime unless the code path loads that source.

## Output Contract

```text
Scope:
- target=<session/run/task>, aspect=<aspect>, mode=<local-only|local+cloud>

Evidence:
- local: <journal/checkpoint/digest facts>
- cloud: <table/query facts or skipped reason>

Mismatches:
- <only concrete mismatches with owner table/module>

Likely owner:
- <storage/service/runtime/cli file>

Next fix:
- <one actionable change or verification command>
```
