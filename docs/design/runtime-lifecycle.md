# Runtime lifecycle

> Status: target design contract.
> Last updated: 2026-07-07.

Runtime lifecycle owns sessions, runs, turns, tasks, plan mode, cancellation, resume, recovery, and durable projections. It does not own tool routing or provider selection; those belong to the capability system.

This document defines the target lifecycle contract. Current code can be evaluated against it, but should not redefine it.

## Principles

- Lifecycle state is durable, not UI-only.
- Web, CLI, Edge, and Server share the same lifecycle semantics.
- Plan mode changes execution policy, not agent intelligence.
- Cancel, delete, archive, pause, blocked, and resume must have explicit transitions.
- Recovery correctness depends on durable state, checkpoints, transcript, and events.

## State hierarchy

```text
session
  run
    turn
      model round
      tool call
    tasks
    checkpoints
    events
```

## Session

A session is the continuity boundary for user-visible conversation, context, memory references, provider bindings, and task board projection.

A session may span Web, CLI, Edge, and multiple devices. Surface changes do not create a new backbone.

## Run

A run is a durable execution attempt inside a session. It owns status, owner lease, checkpoint lineage, current stage, and terminal outcome.

Common statuses:

```text
queued
running
waiting
paused
blocked
cancelling
cancelled
completed
failed
archived
```

A run may be resumed when its state and checkpoint indicate resumability. Resume must not guess from UI state.

## Turn

A turn is the user/agent interaction unit used for context, prompt, trace, and tool sequencing. Tool calls inside a turn inherit provider decisions from the capability system.

## Tasks

Tasks are durable work items projected into UI boards.

```text
created -> active -> completed -> archived
created -> active -> blocked -> active
created -> active -> waiting -> active
created -> active -> cancelled -> deleted
cancelled -> archived
deleted -> archived
```

`deleted` hides a task from active projection but preserves audit lineage.

Required invariants:

- Cancelled tasks do not remain forever in the active board.
- Resume cannot resurrect deleted tasks as active.
- UI cannot invent transitions not accepted by the durable state machine.
- Terminal runs must not leave non-resumable active tasks.

## Plan mode

Plan mode is a policy overlay.

Allowed by default:

- read-only context and status;
- introspect and reflect;
- task planning and non-mutating plan edits;
- provider/status diagnostics.

Blocked by default unless explicitly approved:

- file writes;
- shell mutation;
- git mutation;
- external side effects;
- write-shaped MCP calls.

A denial must explain policy and continuation options. It must not pretend the tool does not exist.

## Cancellation

Cancellation is a state transition with cleanup obligations:

- stop new unsafe tool dispatch;
- settle in-flight tool results as cancelled, failed, or ignored according to provider semantics;
- update task projection;
- persist cancellation reason;
- expose resumability status.

Hard stop is reserved for safety or consistency boundaries. Prefer precise degraded states when possible.

## Recovery

Recovery uses:

- latest durable run status;
- checkpoint;
- transcript;
- C2/C3 facts;
- artifact manifest;
- provider binding projection.

Prompt cache artifacts are not recovery correctness inputs.

## Migration roadmap

Runtime lifecycle migration should proceed in stages:

1. Define canonical lifecycle states and transition table.
2. Ensure all surfaces consume durable projections rather than local UI state.
3. Make cancellation/delete/archive idempotent and projection-safe.
4. Ensure checkpoint/resume correctness for model, tool, and provider boundaries.
5. Add recovery tests for browser disconnect, Edge offline, owner lease expiry, and cancelled task cleanup.

## Lifecycle unhappy paths

| Path | Required behavior |
| --- | --- |
| Browser disconnect | Preserve run unless explicit cancel. |
| User cancel during tool call | Stop new dispatch, settle in-flight call, update task projection. |
| Resume after compaction | Rebuild from checkpoint, transcript, tasks, provider state. |
| Deleted task in old UI cache | Durable projection wins; task remains hidden. |
| Owner lease expired | New owner may recover without double-executing non-idempotent side effects. |
| Buffered completion exists | Finalize without resuming execution when safe. |
