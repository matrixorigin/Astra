# Behavioral Audit Round 8 — Complex Workflow Capabilities & Failure Modes
# Date: 2026-04-24
# Branch: fix/behavioral-audit-round8
# Focus: Multi-step run lifecycle, durable tasks, cancellation, state machines

## Audit Methodology
4 parallel tracks: run-lifecycle, durable-tasks, cloud-sync, error-boundaries.
37 raw findings → verified top P0/P1 against source → 9 confirmed actionable.

## Verified Findings (sorted by severity)

### P1-A: `Waiting` outcome mapped to `Failed` — runs needing input permanently killed
- File: rust/crates/runtime/src/server/run_lifecycle.rs (finalize_run_events ~L1117)
- `AgenticLoopOutcome::Waiting(w)` → `RunStatus::Failed` + `run_error` event
- `Failed` is terminal — no transition out. Run is permanently dead.
- Sub-run path correctly uses `STATUS_WAITING` but main path does not.
- Fix: Add `RunStatus::Waiting` variant, update `try_transition`, fix `finalize_run_events`.
- Impact: Any run requiring external input (tool approval, webhook) is killed.

### P1-B: `cancel_run` does not cascade to delegation sub-runs
- File: rust/crates/runtime/src/server/run_lifecycle.rs (cancel_run ~L2778-2837)
- `pause_run` cascades via `de.pause_children_of()`, `resume_run` via `de.resume_children_of()`
- `cancel_run` has ZERO delegation engine calls. No `cancel_children_of` method exists.
- Fix: Add `cancel_children_of` to DelegationEngine, call from `cancel_run`.
- Impact: Cancelled parent leaves sub-runs consuming tokens until natural completion.

### P1-C: Skill sub-runs have no cancel token — unresponsive to parent cancellation
- File: rust/crates/runtime/src/server/run_lifecycle.rs (build_server_skill_executor ~L391)
- Builder never calls `.with_cancel_token()`. Field defaults to `None`.
- Root cause: executor built before `configure_loop_state_runtime_controls` wires parent token.
- Fix: Wire parent cancel token into skill sub-run executor after runtime controls configured.
- Impact: Skill sub-runs run to completion even after parent cancelled.

### P1-D: `DeltaEngine::apply` not atomic — partial state on error
- File: rust/crates/services/src/protocol.rs (~L650-659)
- Iterates ops, mutates `&mut Value` in place. Op N failure leaves 0..N-1 applied.
- `ApplyOptions::atomic` field exists (default true) but is NEVER IMPLEMENTED.
- Fix: Clone state before apply, swap on success. Or implement atomic flag.
- Impact: Failed delta batch leaves corrupted partial state.

### P1-E: `persist_contract` no optimistic locking — lost updates under concurrency
- File: rust/crates/services/src/durable_task.rs (~L2549-2581)
- INSERT ON DUPLICATE KEY UPDATE with no `WHERE version = ?`.
- `contract.version` incremented in app code but never checked at DB level.
- All 3 persist sites have same issue.
- Fix: Add `WHERE version = ?` to UPDATE, return conflict error on mismatch.
- Impact: Concurrent contract updates silently overwrite each other.

### P1-F: Cancelled `stream_chat` runs lose usage data in durable store
- File: rust/crates/runtime/src/server/run_lifecycle.rs (~L2625-2650)
- `persist_usage` inside `if persist_terminal_state` block.
- When run already cancelled, `persist_terminal_state = false`, usage never persisted.
- `create_run` path calls `persist_usage` unconditionally — inconsistency.
- Fix: Move `persist_usage` outside the `persist_terminal_state` guard.
- Impact: Cancelled streaming runs show 0 tokens in durable store. Billing/audit wrong.

### P2-A: Stuck `Executing` subtasks after crash — no watchdog
- File: rust/crates/services/src/durable_task.rs (~L2830-2860)
- `begin_subtask` persists `Executing` then returns. Crash = stuck forever.
- `can_start()` only allows `Pending | VerificationFailed`, not `Executing`.
- Fix: Add timeout-based recovery or allow `Executing` → retry after threshold.

### P2-B: `TaskOrchestrator::update_status` no state transition validation
- File: rust/crates/services/src/task_orchestrator.rs (~L601-635)
- Blindly writes any status. `Completed` → `Pending` allowed. No guard.
- Fix: Add transition matrix validation before DB write.

### P2-C: `progress_pct` counts Failed/Cancelled subtasks as "done"
- File: rust/crates/services/src/task_orchestrator.rs (~L119-128)
- `is_terminal()` includes Failed/Cancelled. 3 failed + 2 completed = 100%.
- Fix: Only count `Completed` + `Verified` in numerator.

## Rejected / Downgraded Findings

### Token double-counting (originally P0) → FALSE for production
- Only occurs in test-only `execute_mock_turn` behind `#[cfg(feature = "bridge-e2e-hooks")]`.
- Production `execute_turn` does NOT accumulate tokens — `ingest_agentic_turn_stream` is sole site.

### Journal non-atomic writes (originally P0) → DEFERRED
- Real issue but requires filesystem-level fix (flock or write-to-temp-then-rename).
- Complex fix with many callers. Defer to dedicated PR.

## Fix Order
1. P1-D: DeltaEngine::apply atomicity (self-contained, services crate only)
2. P1-E: persist_contract optimistic locking (self-contained, services crate only)
3. P1-F: stream_chat usage persistence (small, targeted fix)
4. P1-A: Waiting→Failed state machine (cross-cutting but well-scoped)
5. P1-B + P1-C: Cancel cascade (related, fix together)
6. P2-C: progress_pct (small fix)
7. P2-B: update_status validation (small fix)
8. P2-A: Stuck Executing recovery (needs design decision)
