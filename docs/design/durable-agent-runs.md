# Durable agent runs

> Status: target design contract.
> Last updated: 2026-09-06.

Durable agent runs define how agent execution survives long tasks, reconnects, cancellation, owner changes, provider failures, and process crashes.

## Principles

- A run is durable control state, not an HTTP request.
- Ownership is leased and recoverable.
- Checkpoints are correctness artifacts, not only performance optimizations.
- Terminal outcomes must be explicit.
- Sub-runs and delegation preserve lineage.

## Run record

A run should capture:

```text
run_id
session_id
parent_run_id
agent_id
status
stage
owner
lease_expires_at
checkpoint_ref
current_turn
waiting_for
terminal_outcome
created_at
updated_at
```

## Checkpoint contract

Checkpoint must include enough information to resume safely:

- current stage;
- pending tool calls;
- provider decisions relevant to pending work;
- task state refs;
- transcript cursor;
- artifact refs;
- last durable event cursor;
- cancellation/resume policy.

## Lease and ownership

- Only the owner may advance active execution.
- Lease expiry enables recovery.
- Recovery must avoid double execution of non-idempotent actions.
- Session execution slots prevent conflicting root runs when required by product semantics.
- Recovery discovery is only a candidate list. A claim returns the new run
  snapshot and the previous generation observed under the write lock. Database
  claims keep canonical session/slot/run locks through batch update, frontier
  readback and commit; neither a pod identifier nor a lease timestamp identifies
  the winning invocation. Collision retries exclude identities already examined
  by that invocation. Generation exhaustion must not partially update a batch.
  A claim receipt is not permission to execute: restoration and current
  authorization still have to succeed before a provider or tool is called.
- Local client session execution leases use one stable, never-rotated file
  authority in the owner-local state directory. Linux adds a kernel-named
  abstract Unix socket so path replacement cannot admit a second executor;
  macOS rejects symlink authorities and verifies path-to-inode continuity
  around its advisory lock. Unsupported platforms fail closed rather than
  running without an execution owner.

## Terminal outcomes

Newly accepted guidance fences an older inference snapshot, not its run owner.
Both logical inference admission and the final pre-HTTP attempt admission retain
that typed distinction. After a guidance fence the shared loop applies durable
guidance through its normal acknowledgement path and prepares a fresh request;
it must not retry the stale wire request or terminalize the Work attempt solely
because guidance arrived. Missing guidance, failed acknowledgement, cancellation,
and owner loss remain fail-closed. Ambiguous admission is reconciled before
continuation, preserving exact settlement custody.

Turn-evaluation journals report settled `run_status` and separate
`tool_evaluation_success` from overall `success`. Healthy tools do not promote
a failed, cancelled, paused, or still-running execution to successful completion.
Internal inference-ledger rejections are not provider HTTP-400 errors.

Terminal states should distinguish:

- completed;
- failed;
- cancelled;
- interrupted partial;
- blocked terminal;
- expired;
- superseded.

## Resume

Resume should validate:

- run status;
- checkpoint integrity;
- session/task projection;
- provider availability;
- pending side-effect safety;
- user intent.

Buffered completion may finalize without resuming execution when the answer is already durable.

### Process restart is not a new user turn

The target for unattended, multi-day Work is to reconstruct execution of the
same durable run under a new owner generation. A process restart does not create
a new user instruction, Work item, attempt, or resource allowance. User-driven
session continuation remains a distinct operation; it is not the implementation
of automatic restart recovery.

Reconstruction must use the shared lifecycle preparation and execution path,
without repeating initial run admission, appending the original user message,
or emitting another `run_started`. Before execution begins, the canonical run
owner must atomically validate session exclusivity, cancellation, the expected
generation, and the checkpoint frontier. Duplicate recovery attempts must not
create another executor. A superseded owner cannot publish new execution facts.
The atomic claim binds the durable frontier identity and version; credential
resolution and history loading happen outside that transaction. Execution must
still verify its owner generation after loading.

The existing run-start and checkpoint contracts own reconstruction inputs:

- Persist stable capability and authorization references plus explicit execution
  restrictions; reauthorize and resolve credentials at recovery time. Never
  persist request-local credentials or silently substitute Server capabilities
  for an unavailable client or Edge binding.
- Preserve consumed round budget and the original explicit wall-clock deadline,
  including time spent stopped. Restart must not replenish either; an absent
  time limit remains unbounded. A renewable scheduling slice is not an implicit
  lifetime limit on Work.
- Restore conversation and task state at the same durable frontier, reconciling
  unresolved tool outcomes before deciding whether any action may execute again.
  A checkpoint marker alone does not prove an interrupted side effect is safe
  to repeat.

Automatic recovery applies to previously executing Work interrupted by process
loss. Deferred activation, user pause, approval/input waits, cancellation, and
recorded terminal outcomes are not authorization to execute. An unavailable
required binding produces an explicit recoverable wait, not broader permissions.
The existing recovery service owns discovery and retry; no parallel scheduler
or continuation ledger is required.

Implementation boundary: current orphan recovery exposes user-driven session
continuation. It does not yet reconstruct an executor for the same run.
New admission persists versioned execution restrictions and shares one time
budget anchor with the execution host. Nullable restriction fields must be
present, so an incomplete record cannot become an unrestricted configuration.
Run-start events also record versioned admission provenance: catalog or
provider-authorized model admission, and registry, request-scoped, bound-executor,
or server-managed capability origin. This is not an authorization grant.
Offering identities, ordered binding identities, and executor routing remain in
their existing event fields. Request-scoped capabilities take precedence over
the bound-executor label; their accompanying executor is still recorded, so a
mixed route does not lose a required dependency. Missing or unknown provenance
must not be interpreted as permission to use Server defaults. Credentials are
not persisted in this descriptor.
This record is not complete reconstruction authorization; restoring consumed
rounds and automatically reconstructing the same run remain unimplemented.
Execution heavy checkpoints additionally carry versioned run-budget facts:
producer run and owner generation, actual charged iterations, current grant,
remaining iterations, and an explicitly present nullable effective hard limit.
Finalization, warning-policy, and failed-subrun writers share the same projection.
Renewal does not reset consumption. Session-only checkpoints without a run owner
omit this projection rather than borrowing an older run's allowance. Recovery
passes these facts through without authorizing execution: owner takeover,
frontier reconciliation, and settlement-only control restoration remain required.
Execution checkpoints also carry versioned control facts using the canonical
completion-settlement type: terminal action windows, their consumed attempts,
retry counters, pagination obligations, and settlement-only restrictions, plus
the loop's wrap-up flag and ignored-round count. Control V2 also requires both
sets of stop-hook obligations and their consumed execution counts. It contains
no request headers or model credentials. Missing obligations cannot decode as
an empty set, and V1 is not upgraded by supplying defaults. All execution writers use one
projection; restore passes it through without granting execution. These controls
must be validated at the same frontier as budget consumption before a recovery
owner may authorize another action. Missing control state must not default to
ordinary execution. Only a validated reconstructable snapshot proceeds
to current authorization and atomic takeover; an unavailable snapshot continues
through the existing session-continuation path without unnecessary credential
requests. Preparation must precede any release of the original session slot.
Deferred Work surviving restart and later explicit activation verifies only
that existing contract, not unattended multi-day recovery.

### Cooperative execution handoff

A shutdown request is not user cancellation or completion of the current turn.
At a settled tool boundary, before charging another iteration, the executor
captures fresh execution state and persists it under its live owner generation.
An acknowledged handoff freezes that executor until shutdown releases it; normal
terminal settlement must not run merely because the process is stopping.
An unresolved tool, failed snapshot construction, or unconfirmed write cannot
produce an exact handoff. A previously cached snapshot is not a substitute.

The handoff's runtime payload pairs the heavy snapshot with the original turn
reservation and a canonical WAL continuation transition. The existing transition
already identifies its committed base and parent result; these are not duplicated
in another log. Its pending suffix includes the last settled tool results. Writing
this payload neither invents an inference attempt nor commits a completed user
turn. In particular, an absent committed conversation cursor must not be replaced
with a fabricated cursor. Recovery validates and replays the existing WAL plus
this continuation, and compares the reconstructed durable history with the heavy
snapshot. The old reservation is identity evidence, not renewed authority.

Original execution facts also retain the original TurnGuard. Its owning module
validates both checkpoint serialization and restoration; recovery does not import
it as fresh-session health or reset correction outcomes, warning watermarks,
adaptive thresholds, or the workspace observation epoch. Retained stall keys
contain tool names and opaque digests, not complete arguments. Restoring the epoch
preserves observation grouping only: it never authorizes reuse of process-local
cached results. Malformed or contradictory facts reject execution restoration
rather than being clamped into a healthy-looking state.

Skill execution facts preserve the instructions already delivered, re-entry and
auto-route attempt history, pinned/discovered skills, effort and effective sandbox
constraints. They are separate from freshly authorized resolvers and request
constraints. Reassembly installs those facts without activating the skill again
or replacing delivered instructions with a newer catalog version. A saved sandbox
does not authorize a different workspace or restore environment variable values.

The shared loop owns an explicit entry cursor: before its preamble, or at an
iteration boundary with the next round identity and consumed harness recovery
count. Neither the last tool step nor charged budget can reconstruct this cursor.
A restored iteration boundary skips SessionStart effects and preserves compression
tracking; every new host still installs its local skill schema. A returned loop
resets the entry for a subsequent user turn; a frozen handoff does not return.
This does not authorize a preamble checkpoint with no current step or with a step
left over from another logical turn; actual recovery must verify the reservation
and frontier together.

Pending runtime context retains its original order, payload and round, excluding
only the existing telemetry-only delivery class. Any unresolved attempt lease
prevents capture, including a lease on telemetry. Restored items are pending, not
consumed; normal preparation can replace singleton snapshots before delivery.
They never become user messages, executable hook configuration or new authority.
Consumed items are not reconstructed from history.

Tool/session hook continuations bind execution facts to the canonical digest of
the currently authorized, ordered definitions. They retain original-index once,
failure and circuit cooldown state, not commands, headers or environment values.
Asynchronous tool hooks register before spawning; normal completion drains the
obligation, while cancellation or panic remains unconfirmed. A checkpoint with
pending/unconfirmed asynchronous effects or an applied hook environment preserves
custody but is explicitly unavailable for execution. A process-global environment
overlay is not an authorized reconstruction source for another user or session.
Actual recovery activation must restore these facts before starting the executor;
the production recovery activation path remains unfinished. The runtime replay
entrypoint is currently exercised only by contract tests, including real
database adoption across owner generations. The atomic adoption API returns the
locked checkpoint with its committed receipt, binding both producer and current
owner generations; a serialized receipt alone is not fresh execution authority.

Completion evidence is summarized by one incremental frontier. Explicit
verification retains the latest source mutation and at most one subsequent
successful proof per authoritative hook. General workspace observation retains
its latest mutation barrier, observation proof and latest positive writer; a
literal-artifact observation also retains the preceding typed delivery it used.
These predicates are distinct: a successful verifier need not add an observation
barrier, but can still invalidate an older artifact delivery. Failed or unknown
writers never supply successful completion proof. Hook or workspace-root changes
rebuild from complete local history; with a recovered prefix they fail explicitly
instead of reinterpreting an empty local suffix.

The runtime handoff requires an explicit verification-evidence disposition.
`Bound` carries the original canonical turn-chain identity, exact authoritative
hook contract, workspace-root binding, execution ordinal, and bounded invocation references including
outcome digests. `Unavailable` carries a typed reason, such as a necessary
unbound mutation or missing turn-chain identity. Both preserve the heavy snapshot
and WAL continuation; unavailable evidence never becomes an empty proof set.
Display output and persisted journal projections cannot establish these bindings.
Decoding checks scope, contract, order and completion shape. Restoration additionally
checks every distinct retained reference against the exact invocation ledger row;
neither decoding nor restoring these facts grants execution ownership. General
observation uses constant-size evidence, not a serialized tool-history scan.
Weak mutation receipts may preserve negative invalidation, never artifact delivery
or positive mutation-completion authority. An independent subsequent observation
can discharge observation debt without upgrading the writer's authority.

The handoff also records exact primary Work custody using the existing Work
binding and item-attempt identifiers. Its explicit variants distinguish no
binding, a binding with no active attempt, an active attempt, and unavailable
custody. Active requires an item identity and revision; lock failure, a missing
binding, or a mismatched executor scope cannot be interpreted as no task.
The producer checks the executor against the handoff's user, session and run.
No objective text or task-selection policy is duplicated in this reference.
Restoration must read this exact attempt and verify its current ownership and
unsettled state, not select the next task or take over another run's attempt.

The canonical checkpoint store fences exact handoffs by run, owner generation,
lease and cancellation lineage. Ordinary control-plane markers cannot overwrite
them. Repeating a handoff identity accepts only the same decoded content; newer
authorized generations preserve older historical snapshots. These storage
guarantees do not themselves authorize recovery execution.

Cross-placement session handoff watermarks carry a paired run identity and
producer generation when an execution is involved; an idle session need not
invent a run. Graceful handoff transitions cannot replace that run/generation/
checkpoint reference after `Checkpointed`, including blocked-step retries.
Non-identity delivery watermarks may still advance. At graceful checkpoint,
hydration and activation boundaries, the service checks the exact stored
checkpoint's owner, session, run, kind and producer generation against the
current run generation. Another active execution in the session prevents these
transitions; ordinary paused continuations do not occupy execution authority.
Checks share the canonical session/execution fence and use current reads, not
an earlier transaction snapshot. They lock the referenced run rather than all
historical continuations. Direct and ancestor cancellation are checked before
handoff locks; the complete run/generation/checkpoint reference is rechecked
after those locks are acquired. Storage failures remain server failures, not
invalid user requests.

A recovery claim atomically advances ownership generation and records exact
checkpoint custody in the existing run-event stream. A repeated claim may
inherit custody only from the current tail's matching claim event; intervening
activity invalidates that proof. Recovery reconciliation atomically records the
final association and pauses the run. Its retry returns the persisted event
without appending another one. Cross-generation handoff accepts only that exact
current-tail association for a paused run with no waiting obligation. It does
not search history for older matching proofs. These guarantees do not prove
source quiescence, materialize the destination workspace, or authorize executing
the checkpoint payload.

Implementation boundary: owner-fenced storage, fresh snapshot construction and
cooperative Server shutdown are connected. The recovery scanner preserves valid
handoff material through the atomic recovery-association path; it does
not launch an executor. Full replay currently has deterministic test coverage,
not a production recovery consumer. It requires the original generation,
reservation, committed history and real WAL receipts. Automatic execution takeover
and restoration of all execution obligations remain unimplemented. After shutdown
begins, the shared request preparation entry rejects new chat admission with a
structured `server_shutting_down` response instead of creating a run that would
immediately freeze.

Custody and execution decoding are separate: an owner-fenced handoff whose
inner control schema is unsupported still retains its original bytes and the
explicit continuation path. Recovery reports whether the runtime can decode
that state, without claiming it reconstructed execution. Cancellation still
wins. Control V2 is not yet a complete reconstruction contract: ledger-verified
tool evidence, intent and remaining execution obligations must be restored
before an automatic executor may run.

## Test obligations

- Crash after model output but before final event.
- Crash during tool execution.
- Owner lease expiry and takeover.
- Duplicate resume attempts.
- Sub-run lineage recovery.
- Provider offline during resume.
- Same-run reconstruction preserves attempt identity, consumed budgets, and
  transcript identity before the first provider request.
- Repeated recovery, recovery-owner loss, and concurrent user control admit at
  most one executor; deferred activation and blocking waits do not auto-start.
- Native client release targets acquire, conflict, release, and reacquire the
  session execution authority before they are packaged.
