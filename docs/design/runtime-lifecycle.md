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
and explicit execution limits remain authoritative. Historical guard verdicts remain
audit and recovery advice, not sticky execution vetoes. Continuation
does not make unexecuted requests successful or grant them completion evidence.

Repeated tool signatures establish a repeated request pattern, not unchanged
results or lack of progress. Signature-only advice must preserve legitimate
verification, pagination, evidence recovery and authorized waiting. It must
not claim that earlier content remains in context, prescribe workspace edits,
escalate to task termination because prior advice was repeated, or add a tool
to retry caution solely because its name recurs. Independent tool-health
evidence remains available to the existing recovery policy.

The runtime harness follows the same boundary: signature repetition is a
warning observation, never a Fatal/Block or Pause condition. Actual configured
budgets, safety invariants and critical-verifier failures retain their existing
enforcement. Empty tool-call sets do not constitute repeated tool activity.

Progress does not discharge completion obligations. Required validation must
still apply to the final mutation state; a renewed slice must not be reported
as successful completion or bypass provider and permission boundaries.

### Execution failures and task resolution

Semantic request classification distinguishes valid decisions, unresolved
necessary facts, conflicting facts, malformed responses, and provider failures.
A successful provider invocation is not proof of a usable admission decision.
Uncertainty retains bounded field evidence and may receive one clarification
on the explicitly configured judgment Offering within the existing admission
deadline. This per-turn budget is not reset by trusted skill loading. The
complete clarification must satisfy the same confidence and dependency rules
and preserve already determined necessary facts; partial evidence is never
merged into execution authority. No implicit main-model fallback is permitted.

Mutation completion scope describes the resources the current request actually
requires changing, not its subject, referenced paths, executor location, or
previously completed work. Workspace and external scopes put all required
changes on the corresponding side of the bound-workspace effect boundary;
mixed requires changes on both sides, and an unclear target/boundary remains
unknown. Effect-owner domain is independent of this scope. Read-only references
create no mutation obligation. All judgment carriers share this semantic
contract; the existing typed receipt checks still own completion evidence.

Mutation classification concerns requested task-resource effects, not runtime
checkpoint, audit, trace, usage or scheduling writes. Explicit Astra Work
tracking, board and graph changes remain obligations of the Work lifecycle and
plan, verified by their existing Work owners; they do not by themselves require
a workspace or external task-resource mutation. Read-only verification plus a
requested Work board can therefore be both `read_only` and Work `required`.
Separately requested workspace or external changes retain their mutation and
scope obligations, with or without Work. This producer distinction does not
change unresolved-scope completion handling or any authorization/receipt gate.

The auxiliary classifier is optional. With no configured judgment Offering,
the primary model uses normal typed admission carriers without an auxiliary
call. An explicitly configured ordinary LLM or Jev uses the same existing
`judgment_model` contract and routing; none is not an implicit ordinary-LLM
configuration. No new router or fallback model call is introduced.

Absent or unreliable auxiliary output supplies no admission authority. Timeout,
abstention, malformed output, internal contradictory classifications and other
auxiliary failures therefore preserve the same primary typed-carrier baseline.
The runtime retains their diagnostic cause, bounded evidence and clarification
state; it does not invent a complete decision, mutation scope or completion
obligation. A valid negative topology decision remains distinct from unavailable
auxiliary evidence. A conflict with trusted runtime workflow/Work state is also
distinct from internal auxiliary conflict and remains a rejection.

Tool visibility and lifecycle admission share the canonical fanout-proposal
predicate. Proposing a carrier is not authorization: existing permissions,
effect boundaries, readiness/capacity, active Work custody, trusted topology,
canonical delegation, cancellation and completion gates remain authoritative.
Direct parallel spawn batches do not become an alternative fanout carrier.
Missing auxiliary facts that these existing carriers and gates can establish
must not become classifier-only prerequisites. Unresolved mutation locus and
receipt obligations retain their existing owners; baseline admission does not
default a target to workspace or erase an outstanding completion obligation.

Diagnostics must not claim that no work happened earlier in the turn merely
because the current dependent call was rejected. Raw classification payloads
remain opt-in debug material rather than ordinary lifecycle facts.

Tool results retain their source status, exit semantics, output, and invocation
identity. Classification uses supported evidence; an unknown cause stays unknown.
The Agent interprets the impact on the user's task. A later unrelated success
must not clear a failure, and changing a command does not prove equivalence.

At completion, an unresolved failure with a later-round observation candidate,
or the existing repeated-failure signal, permits one evidence-linked
`submit_task_resolution` proposal through the existing `invoke_tool` carrier
when the task has a real execution contract. The completion path first
projects the retained failures into terminally relevant obligations: possible
bound-workspace mutation, a recognized validation command, declared external
state, unfinished child/fanout execution, or an invocation that cannot be
rejoined to durable authority. Ordinary diagnostic probes remain in the full
ledger and final explanation, but do not turn the answer into
`ExecutionIncomplete` merely because a later observation exists. This keeps
the rule semantic rather than tied to one command name. If the runtime cannot
classify a failed invocation because its authority is missing, it fails closed
and retains the strict path. The full schema is supplied only in that
boundary's hint, not added to resident tools. The proposal identifies the
verification target, failed and later evidence calls,
`supported`/`partial`/`unknown`, rationale, and remaining gaps. This is a model
assessment, not a verification receipt.
The wire contract bounds the target and each gap to 256 characters, rationale to
1024 characters, and each evidence list to 32 call IDs; runtime validation uses
the same character-count limits as the provider schema.
Candidate existence does not establish semantic relevance. Same-round sibling
results are not later evidence, and exact-operation recovery needs no proposal.
The transient hint supplies bounded, source-owned execution IDs and typed
status from the retained policy window, so the Agent can reference evidence
without inventing identities or performing discovery. Submission transport and
evidence validation have distinct feedback; neither replaces final coverage.
Scheduling pressure may decrease after healthy progress; once this bounded
assessment starts, final coverage is checked against the remaining failure facts,
not the scheduling stage or whether the proposal tool itself returned success.

If the provider schema rejects this submission before dispatch, the runtime
records that typed pre-dispatch stage separately from an executed tool failure.
The same reconciliation boundary may receive one argument correction; that
budget survives checkpoint recovery and cannot be reopened by handler failures,
unknown tools, a different boundary, or another invalid submission.

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
Task-level resolution requires coverage of all current terminally relevant
unresolved failures and must not reuse stale workspace evidence after a later
writer. Advisory failures remain observable evidence and are not silently
dropped from the journal or answer context. Missing, ambiguous, foreign, or
unavailable evidence remains unresolved.
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

Reply-only plan drafting is an informational outcome, not a request to change
the workspace or establish durable Work. Quoted goals remain data. Explicit
execution, saving, tracking, or graph edits retain their effects even when the
requested response is a plan or JSON. A request to establish tracked Work without
execution may defer activation; merely returning a plan does not establish Work.

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
An optimistic Work-context mismatch is a retryable non-execution rejection:
the proposed graph mutation has not started and carries no possible side
effect. Refreshing the canonical context may safely retry without leaving an
unresolved execution failure in the turn outcome.

Task identities are not execution-order authority. Admission retains explicit
`after_initial_tasks` prerequisites as dependency edges; omitted prerequisites
leave tasks independent. Replacement inherits the replaced task's precedence.
The same field on a graph mutation is a separate application trigger: its
referenced initial tasks must be delivered before the mutation is committed.
The immutable establishment decision retains these triggers beyond establishment
completion. Scheduling, including settlement's automatic successor allocation,
must apply due mutations before selecting another task or declaring completion.
Accepted graph revisions mark applied mutations, so recovery replays the same
operation and item identities without repeating semantic admission even after
terminal proposal rows leave the bounded proposal queue. Settlement and resume
receipts publish the durable graph revision and canonical task states, including
retired declarations, even when a mutation committed before replay. A failed
post-commit receipt read resumes through `run_next_work_item`; its receipt must
restore the board even when the graph is already complete. These receipts read
one canonical snapshot after settlement or task allocation so the live board
observes cancellation and replacement together with successor assignment. The
accepted `propose_work_plan` receipt also exposes bounded `applied_mutations`
arrays for added items, revised source revisions and declaration states, and
dependency additions or removals. Those arrays describe the accepted proposal
at its recorded graph revision; they do not guess a revised item's target
revision because shared item identities may allocate different successors on
sibling branches.
`start_work` result separately reports declared task count and any already
applied admission graph changes, so a server-applied addition or revision is
visible as a completed change and is not proposed again by the model.
The `settle_work_item` receipt narrows that field to the admission changes
whose trigger was the exact settled attempt and item revision. That association
is committed under the branch lock before reconciliation, so a crash between
settlement and proposal application replays the same receipt instead of
claiming an empty change set or attributing a later trigger's change to an
earlier attempt. Resume and `run_next_work_item` receipts expose the bounded
cumulative accepted set, while the settlement receipt remains an exact
per-attempt explanation. If a pre-association accepted graph revision is
recovered, the receipt says
`applied_admission_mutation_attribution: "unavailable"` and publishes the
cumulative set with `applied_admission_mutations_scope: "cumulative_recovery"`;
it never guesses
which current attempt caused that historical change.
Terminal-cut recovery uses the same exact marker; a legacy accepted revision
without one stays recoverable only through an explicit repair path and cannot
silently assign terminal ownership to a current attempt.
Initial-candidate references are not aliases for arbitrary later replacements;
conflicting retirement/prerequisite lifetimes are rejected before establishment.

The canonical schema now includes the durable trigger-association table and a
composite `work_graph_revisions(owner_id, work_id, patch_ref, revision)` index.
Fresh installations receive both from the schema manifest. Existing databases
must provision both through the repository's schema migration process (or use a
fresh-schema cutover) before deploying a binary that verifies this contract.
Bootstrap may create a newly absent table from the manifest, but it fails closed
with an explicit cutover error when an existing Work table has an incompatible
shape, such as a missing mandatory index. `CREATE TABLE IF NOT EXISTS` and the
manifest's index declaration are not an in-place upgrade mechanism for an
existing Work table.

Before the next model call, an owner/session/run-validated active primary Work
binding receives a bounded `work_evidence_context.v1` snapshot through the
existing required runtime-context lane. It carries the delivered-settlement
requirement and at most eight recent journal call identities, dispositions and
success flags, stopping at a Work lifecycle carrier. It does not copy arguments
or result bodies, invoke a judge, read storage, or change tool authority. The
snapshot is rebuilt each boundary and removed when the active binding cannot be
validated; hosts without an executor or an optional semantic judge still run.

This first stage deliberately reports settlement readiness as unknown. Journal
execution identity does not bind each result to an exact Work attempt; a missing
lifecycle boundary after recovery or window truncation cannot prove absence of
evidence. Even a successful result cannot prove semantic coverage of
`expected_result`. Recent calls are therefore explicitly a journal suffix, not
attempt-attributed proof. The existing Work start/continuation context owns the
expected result, and settlement admission remains the only owner of its gate.
The projection makes the requirement and reusable observations available before
a rejected settlement or another inspection; it neither auto-settles nor promises
that the model will avoid all redundant calls.

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

### Session cancellation convergence

`POST /sessions/{id}/cancel` accepts cancellation through the canonical Run
control path and spends a bounded interval converging its execution. Its JSON
retains the Session fields and adds required `execution_settled` and `runs`
fields. Each run entry contains `run_id`, its observed `status`, and
`execution_settled`. HTTP 202 with `status: "cancellation_requested"` and
`execution_settled: false` is pending, including when the run list is empty.
Only HTTP 200 with `status: "cancelled"` and `execution_settled: true` reports
convergence. Pending responses do not write a cancelled Session projection.
Clients may repeat the same owner-scoped endpoint; a deadline, missing proof,
or storage failure must not become cancellation success.
Pending responses may include `workspace_blocker`, the typed result of the
latest canonical idle proof. Missing proof is not an inferred blocker or
success. The database Run store publishes all currently active Runs' existing
cancellation markers in one owner/Session-scoped atomic write, with a bounded,
cancellation-safe database attempt. An ambiguous acknowledgement fails closed;
retry is idempotent. This precedes settlement discovery, so slow pages cannot
starve later intent. The subsequent one-second settlement budget is cooperative:
checks occur between completed operations, never by dropping an in-flight SQL
transaction. An individual operation can exceed that budget; CLI callers enforce
their own hard deadline.

Active-run enumeration is insufficient: terminal runs can retain live
executors, owner leases, tool invocations or canonical turn authority. Retries
rediscover that retained authority and preserve the targets observed while
waiting. The coordinator owns one fenced idle proof shared with checkout
reuse, including slots, active execution, generation-scoped durable settlement
fences (started without finished or accounting-finalized), writer/reservation authority,
unresolved invocations and unfinished execution-binding switches. Run statuses
remain truthful when completion wins a cancellation race.

An orphan's terminal transition does not itself retire its canonical writer.
Cleanup rechecks the terminal Run generation and owner under the Session
execution fence, then releases only the exact internally acquired writer and
matching reservation. A newer writer, recovered generation or externally
supplied controller lease is not released. A writer acquired before its Run
exists remains pending. Cleanup is idempotent and can resume after a failed or
unacknowledged attempt without replaying execution.

Cancellation invokes the existing terminal tool reconciler promptly. A
prepared invocation with no dispatch attempts becomes a durable rejection;
dispatched or unknown outcomes retain their safety obligations. Lease expiry,
disconnect, terminal status and cancellation intent cannot prove external
effects stopped. `execution_settled: true` requires this Session no longer to
block checkout reuse; it does not reserve the checkout against another Session
or prohibit a later explicitly admitted turn. Session history and execution
bindings are retained, and new-conversation admission performs its own claim
check.

## Recovery

Recovery uses:

- latest durable run status;
- checkpoint;
- transcript;
- C2/C3 facts;
- artifact manifest;
- provider binding projection.

Prompt cache artifacts are not recovery correctness inputs.

### Provider chat session lookup

`POST /sessions` accepts authenticated provider requests with a required
`client_session_ref` (an exact, nonempty string, at most 255 bytes), plus the
existing optional `agent_id`, `title` and `metadata`. Authentication covers the
request method, route and exact body. Provider requests create an empty session;
they do not start a model or import history. First-party creation without a
client reference retains the ordinary creation path.

The canonical session owner derives a 64-byte session ID from a domain-separated,
length-delimited SHA-256 of the verified provider, subject, scope, user and client
reference. The existing `(user_id, session_id)` primary key and session write
fence serialize creation across replicas. An immutable `provider_creation_hash`
column compares the canonical original creation payload: matching repeats return
the same session (HTTP 200), while a new session returns HTTP 201 and a changed
payload returns HTTP 409 `session_creation_conflict`. Mutable titles/metadata and
run state never replace that original hash. Both creation paths share the same
insert/read-before-commit implementation; a replay bypasses new-session quota
denial and is not counted as another creation. The existing durable deletion
fence remains authoritative and cannot be cleared by idempotent creation.

For authenticated provider requests, a confirmed missing session before run
creation is reported as HTTP 404 with `error_code: session_not_found` (including
the SSE error envelope for `/chat/stream`). The session owner distinguishes
absence from an owner-hidden 404; only a failed owner lookup requires the
additional existence check. A healthy lookup retains its original query path.
Foreign sessions, generic 404s and storage failures do not grant permission to
recreate a session. First-party session APIs retain their hidden-not-found
contract. This error does not assert that an earlier run did not execute: the
provider must also preserve its own task/run execution identity when deciding
whether the current operation can be retried.

For an authenticated provider cancellation request, a confirmed lifecycle 404
is returned with `error_code: run_not_found`. The lifecycle owner continues to
hide whether the run is absent or belongs to another user. A provider may use
this exact code to settle an already-requested local cancellation because the
remote run cannot be cancelled; it must not infer the same result from a generic
404, an authorization failure, timeout, or malformed response. This code never
authorizes replaying the original run.

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
