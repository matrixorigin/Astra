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

### Bounded continuation and completion

An execution slice is a capacity checkpoint, not a task-completion boundary.
Task profiles choose the initial slice and renewal step, not an implicit
terminal cutoff or a fixed number of renewals. Root and child runs resolve the
same optional hard boundary from administrator configuration and any explicit
caller limit. Without either limit, there is no built-in total-round cutoff;
execution continues in renewable slices, including across long tasks. With a
limit, renewal cannot exceed it. A bounded closing allowance is
separate from ordinary execution capacity and cannot reopen exploration.

Fresh executor-confirmed workspace changes, new authoritative observations, or
recovery of a failed operation can justify another bounded slice, including
during an implementation task. A successful edit need not be followed by full
validation before another edit is allowed.

Missing progress receipts do not establish a stall: capabilities have different
evidence coverage. In the absence of an authoritative stop condition, another
bounded slice is allowed without a separate progress-credit ledger or mandatory
reflection call. Explicit hard limits, cancellation, workspace quarantine,
and repetition controls remain authoritative. Historical guard verdicts remain
audit and recovery advice, not sticky execution vetoes. Continuation
does not make unexecuted requests successful or grant them completion evidence.

Progress does not discharge completion obligations. Required validation must
still apply to the final mutation state; a renewed slice must not be reported
as successful completion or bypass provider and permission boundaries.

### Execution failures and task resolution

Tool results retain their source status, exit semantics, output, and invocation
identity. Classification uses supported evidence; an unknown cause stays unknown.
The Agent interprets the impact on the user's task. A later unrelated success
must not clear a failure, and changing a command does not prove equivalence.

At completion, an unresolved failure with a later-round observation candidate,
or the existing repeated-failure signal, permits
one evidence-linked `submit_task_resolution` proposal through the existing
`invoke_tool` carrier. Its full schema is supplied only in that boundary's hint,
not added to resident tools. The proposal identifies the verification target,
failed and later evidence calls, `supported`/`partial`/`unknown`, rationale, and
remaining gaps. This is a model assessment, not a verification receipt.
Candidate existence does not establish semantic relevance. Same-round sibling
results are not later evidence, and exact-operation recovery needs no proposal.
The transient hint supplies bounded, source-owned execution IDs and typed
status from the retained policy window, so the Agent can reference evidence
without inventing identities or performing discovery. Submission transport and
evidence validation have distinct feedback; neither replaces final coverage.
Scheduling pressure may decrease after healthy progress; once this bounded
assessment starts, final coverage is checked against the remaining failure facts,
not the scheduling stage or whether the proposal tool itself returned success.

Admission binds the call to the current run, turn chain, Work subject and user
intent. The model supplies only the interpretation; the handler binds scope and
boundary from current invocation authority. Model-supplied control fields are
rejected, not silently overridden. Acceptance resolves exact, authority-tagged completion references in the
existing owner-scoped invocation ledger or Edge dispatch store. Edge references
bind the selected executor and canonical result hash, and are attached by the
Server only after durable acceptance; local-only delivery grants no such proof.
Durable result bodies use lossless text storage: database JSON normalization
must not change the numeric representation covered by the accepted hash.
With a configured durable Edge owner, direct tool delivery admits and claims the
dispatch after guarded run admission and before publishing the request. Existing
in-flight or terminal dispatches are observed, not re-executed. A durable
admission failure must not silently downgrade to local-only delivery; an
ambiguous outcome remains unknown. Explicit local-only hosts can still execute
through guarded callback delivery without gaining durable assessment authority.
HTTP callback replay compares original callback content, not the Server's added
provenance, and never upgrades or overwrites the first delivered reference.
Task-level resolution requires coverage of all current
unresolved failures and must not reuse stale workspace evidence after a later
writer. Missing, ambiguous, foreign, or unavailable evidence remains unresolved.
The bounded policy window retains original references across checkpoint recovery;
local display text and absence from a recovered suffix are not authority.
Edge workspace evidence without a retained trustworthy ordering relative to a
known writer remains unresolved after recovery. This does not expand the
authority of deterministic invocation-backed verification contracts.

Raw execution failures remain in accounting even when another approach satisfies
the task. Explicit deterministic checks and canonical Work settlement keep their
own authority. A rejected submission or unavailable capability produces an honest
partial/unknown report, not another unrestricted execution loop.

## Tasks

Work admission counts user acceptance units, not execution phases. Observation,
verification, reporting, and settlement for one result belong to that task;
a separately requested report deliverable may itself be a task. Initial tasks
and admitted graph mutations together must respect explicit user task-count
constraints unless the user explicitly revises them. Graph mutations represent
requested additions, cancellations, or replacements, not merely steps described
as happening later. Do not duplicate a requested mutation in the initial tasks.
Semantic admission rejections identify the field path, violated rule and
observed size or index without copying field contents into the diagnostic.
These diagnostics do not relax validation or change the repair policy.

Task identities are not execution-order authority. Admission retains explicit
`after_initial_tasks` prerequisites as dependency edges; omitted prerequisites
leave tasks independent. Replacement inherits the replaced task's precedence.
The same field on a graph mutation is a separate application trigger: its
referenced initial tasks must be delivered before the mutation is committed.
The immutable establishment decision retains these triggers beyond establishment
completion. Scheduling, including settlement's automatic successor allocation,
must apply due mutations before selecting another task or declaring completion.
Accepted proposals mark applied mutations, so recovery replays the same operation
and item identities without repeating semantic admission.
Initial-candidate references are not aliases for arbitrary later replacements;
conflicting retirement/prerequisite lifetimes are rejected before establishment.

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
