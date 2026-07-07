---
name: trace-delegation
description: "Trace Astra's current delegation engine: sub-run hierarchy, fan_out/pipeline/sequential/adversarial_review/fork patterns, journal events, verification retries, pause/resume, cancellation, and aggregation. Use when debugging delegated agents, child runs, team orchestration, or delegation progress."
user_invocable: true
when_to_use: "When the user wants to trace, debug, or explain Astra delegation behavior, child sub-runs, agent fanout/fork, verification-gated retries, delegation pause/resume, or parent-child run hierarchy."
arguments:
  - name: TARGET
    description: "Delegation ID, parent run ID, session ID, or 'last'. Omit for the most recent delegation."
    required: false
  - name: DEPTH
    description: "Trace depth: 'summary', 'detail', or 'deep'. Default: detail."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---

# Trace Delegation

Use this skill to reconstruct what the delegation engine actually did. Prefer durable evidence
from journal events and tracker-visible records over inferred architecture diagrams.

## Current Implementation Map

| Concern | Source |
| --- | --- |
| Engine, tracker, state transitions, verification/checkpoint gates | `crates/runtime/src/server/delegation/engine.rs` |
| HTTP handlers for delegate/list/pause/resume | `crates/runtime/src/server/delegation/handlers.rs` |
| Patterns, tiers, request/result types, aggregation | `crates/services/src/coordination.rs` |
| Journal event builders and metadata fields | `crates/services/src/session_journal.rs` |
| Parent/child messaging and broadcast groups | `crates/astra-messaging/src/` |
| Runtime lifecycle delegation hints | `crates/runtime/src/server/server_loop_host.rs` |

Do not use the old path `runtime/src/server/delegation_engine.rs`; the engine lives under
`runtime/src/server/delegation/`.

## Journal Events

These are the stable delegation events to group by `metadata.delegation_id`:

| Event | Key Metadata |
| --- | --- |
| `DelegationStarted` | `delegation_id`, `parent_run_id`, `pattern`, `agent_ids`, `agent_count` |
| `DelegationSubRunStarted` | `delegation_id`, `sub_run_id`, `parent_run_id`, `agent_id`, `status`, `depth`, `retry_of` |
| `DelegationRetry` | `delegation_id`, `original_run_id`, `retry_run_id`, `agent_id`, `attempt`, `reason` |
| `DelegationSubRunCompleted` | `delegation_id`, `sub_run_id`, `agent_id`, `status`, `error`, `output_preview` |
| `DelegationCompleted` | `delegation_id`, `pattern`, `total_sub_runs`, `succeeded`, `failed`, `aggregated_status`, `aggregated_output_preview` |

Important limitation: journal sub-run completion events do not include per-sub-run token counts.
`DelegationResult` and HTTP `DelegationResponse` carry token totals, but a journal-only trace
cannot reliably reconstruct token distribution.

## Locate Delegation Data

```bash
python3 - <<'PY'
import glob, json, os

events = []
names = {
    "DelegationStarted",
    "DelegationSubRunStarted",
    "DelegationRetry",
    "DelegationSubRunCompleted",
    "DelegationCompleted",
}
for path in glob.glob(os.path.expanduser("~/.astra/sessions/*.jsonl")):
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line_no, line in enumerate(fh, 1):
            try:
                event = json.loads(line)
            except Exception:
                continue
            if event.get("type") in names:
                meta = event.get("metadata") or {}
                events.append((event.get("ts", ""), path, line_no, event.get("type"), meta))

for ts, path, line_no, typ, meta in events[-80:]:
    did = str(meta.get("delegation_id", ""))[:16]
    rid = meta.get("sub_run_id") or meta.get("parent_run_id") or meta.get("retry_run_id") or ""
    print(f"{ts[:19]} | {typ:27s} | {did:16s} | {rid} | {path}:{line_no}")
PY
```

Resolve the target:

| Target | Action |
| --- | --- |
| Delegation ID | Filter events where `metadata.delegation_id` matches |
| Parent run ID | Find `DelegationStarted.metadata.parent_run_id`, then collect its delegation IDs |
| Session ID | Read that session journal only |
| `last` or omitted | Use the most recent `DelegationStarted` |

## Patterns And Aggregation

Pattern names emitted by the engine:

| Pattern | Journal string | Notes |
| --- | --- | --- |
| `CoordinationPattern::FanOut` | `fan_out` | Parallel agents, aggregated after all complete |
| `Pipeline` | `pipeline` | Current engine executes through the sequential path using stage agent IDs |
| `Sequential` | `sequential` | Ordered agents, optional stop-on-success |
| `AdversarialReview` | `adversarial_review` | Producer/reviewer rounds with max rounds |
| `Fork` | `fork` | N tasks sharing parent context, one user-tier agent, constrained recursion |

Aggregation strategies in current code are `FirstSuccess`, `AllResults`, `Consensus`, and
`LlmGuided`. Do not report `Merge` or `VoteOnBest`; those are stale names.

## Trace Workflow

1. Build a timeline from journal events sorted by timestamp and line number.
2. For each `DelegationStarted`, verify the expected pattern and agent set.
3. For each expected child, confirm a `DelegationSubRunStarted` event. Missing starts mean the
   engine registered a delegation but the sub-run did not enter `Running`.
4. For each started child, confirm terminal `DelegationSubRunCompleted`. Missing completion means
   pause, cancellation, crash, timeout, or unfinished state.
5. Fold `DelegationRetry` into the original child. A retry should mark the original as
   `verification_failed` and create a `retry_of` relationship on the retry run.
6. Confirm `DelegationCompleted` totals match the completed child set. A mismatch is a journal or
   aggregation bug.
7. If pause/resume is involved, inspect `pause_delegations_handler`, `resume_delegations_handler`,
   `pause_flags`, `cancel_tokens`, and `cleanup_delegation` in `engine.rs`.

## Failure Classification

| Symptom | Check |
| --- | --- |
| Delegation never starts | Engine configured in `AppState`, handler ownership check, request validation |
| No child runs | `record_sub_run`, `transition_state(..., Running)`, request allowlists, recursion depth |
| Child cannot message parent | mailbox router registration and `MessageTarget::Parent` resolution |
| Retry loop | `VerificationGate::verify`, `max_retries`, `DelegationRetry.reason` |
| Pause/resume stuck | `pause_flags`, child loop pause boundaries, `resume_children_of` |
| Cancellation leak | `cancel_tokens`, `cancel_children_of`, `cleanup_delegation` |
| Wrong aggregate | `AggregationStrategy` and `aggregate_results` in `coordination.rs` |
| Missing UI progress | `progress_broadcaster`, `DelegationProgress`, SSE event emission |

## Report Format

```
Delegation Trace: <delegation_id>

Pattern: <pattern>
Parent run: <parent_run_id>
Expected agents/tasks: <n>

Timeline:
- <ts> started: agents=<...>
- <ts> sub-run started: <run_id> agent=<agent_id> depth=<n> retry_of=<...>
- <ts> retry: <old> -> <new> attempt=<n> reason=<...>
- <ts> sub-run completed: <run_id> status=<status>
- <ts> delegation completed: succeeded=<n> failed=<n> status=<status>

Findings:
- <only concrete gaps or mismatches, with file/event evidence>

Limits:
- <state any data not present in the journal, such as per-sub-run token distribution>
```
