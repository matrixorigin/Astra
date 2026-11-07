# Background work user journey

Status: target journey contract; launch and observation behavior does not by
itself establish full asynchronous request settlement.

Background execution is a presentation and scheduling choice. It is not
permission for a parent objective to forget its children, report unobserved
results, or require the user to remember how to resume orchestration.

## Product invariant

One user request remains one work unit until the runtime reaches one of these
honest outcomes:

1. an answer grounded in the terminal child results;
2. a visible, actionable wait such as approval, user input, or executor
   reconnect;
3. an explicit user-owned background handoff with a durable result reference;
4. a visible failure, interruption, or cancellation with recoverable state.

The model is never the authority for whether a child started, is still
running, or completed. Those facts come from the runtime projection.

This document separates the public launch/observation contract from the
remaining journey obligations. A passing schema test or a handler returning
`launched` does not validate parent-idle continuation, request settlement,
restart recovery, or cross-surface delivery. Those behaviors require the
runtime and end-to-end evidence described under Verification gates before
they can be claimed as supported.

## Current local shell execution contract

The selected CLI/User Runner executor owns a shell invocation from launch to
settlement. Foreground and background waits share its task id, process, original
deadline, cancellation endpoint, capture and workspace custody. Ctrl+B, an
accepted parent instruction, or Bash's bounded `yield_time_ms` releases a
foreground wait. No process/pipes are adopted by the TUI and no new invocation
is launched. A surface without a retained shell task owner waits for the
command to finish; independently owned agent/fanout controls can remain available. Ordinary
composer input still addresses the parent. Capability introspection reports
`shell_task_controls` separately from agent and fanout task controls.

`task_output` observes a bounded snapshot or waits for a terminal event. For
interactive shells, `wake_on_output=true` also wakes the bounded wait for
output. Pass the last live snapshot's `output_revision` as
`after_output_revision` to wait for subsequent activity; without it, existing
output returns immediately. The revision is an activity signal, not a byte
cursor, and a concurrent write can result in a repeated snapshot. A live shell snapshot gives stdout and stderr separate bounded
tails and has no stable pagination cursor; the tails do not imply ordering
between streams. After completion, the captured output can be paginated;
published workspace files may still be changed by users between reads.
An ambiguous credential or private-key prefix may be withheld from the live
view; this is indicated in the snapshot. A timed-out or cancelled observation
leaves the command running. `task_input` writes only to a task launched with
`stdin=true`; default stdin is closed. Input writes, closing stdin and stop
requests are acknowledged separately from observed terminal execution. The task
panel offers input only when the task supports it. Input arriving during process
startup waits for stdin within the same deadline. A timeout or cancellation
before submission is reported as not sent; after submission, missing
acknowledgement is unconfirmed and is not automatically retried. Agent mailboxes
and native collaborator steering keep their existing protocols and identities.

Ordinary local shell tasks do not hold a workspace-wide exclusive lock for
their lifetime. Independent sessions can therefore use the same workspace;
as with external editors, users must coordinate overlapping writes and worktree
removal. No automatic shell-effect attribution or source rollback is promised.

Capture and queues are bounded. Each executor admits at most 32 simultaneous
shell tasks, with a 50 MiB capture limit per task and 256 MiB retained terminal
capture budget; terminal handle retention is separately bounded. Restored
references remain read-only and cannot authorize deleting user files. Slow consumers
retain terminal notifications under bounded launch backpressure. The existing
safe-output cache has a 64 MiB text budget and a bounded entry count. Cached
reads bypass cold scan admission; small and large cold scans have separate
bounded capacity. Growing output
still requires a new safe projection when its generation changes. Complete or
partial output published under the workspace belongs to the workspace and is
not evicted with private task captures. Private captures are removed when their
session owner closes; a restored task can report a saved terminal status but not
reconstruct its private output. A task whose previous owner was lost is
unavailable to this executor; this does not prove its process stopped or its
effects were rolled back. Published workspace files remain available.
Private cleanup uses the original directory handle; tasks in one namespace
share that handle. A failed eviction keeps its owner for retry, and a full
retention budget applies backpressure rather than dropping cleanup ownership.
Output chunks
do not trigger model calls, and local waits perform no periodic SQL. A restored
plain-shell projection can expose saved output but cannot recreate a lost
process handle. Weak process-group cleanup never certifies authoritative
workspace settlement. These limits are not a claim that one workspace or one
executor can run 1,000 physical commands concurrently.

Repeated observations remain legal within ordinary run budgets. An unchanged
status is a fact for the model, not permission for the application to synthesize
a final answer or declare the user's objective complete.

## Default journey: parent-owned concurrent child

`agent.spawn` accepts one child and promptly returns a `launched` receipt with
its runtime-generated `agent_id` once execution ownership is established.
The parent continues independent work while the child runs; no mode flag or
Ctrl+B is needed. The receipt proves launch, not completion. Before relying
on the child's work, the parent needs its terminal outcome. A launch receipt
or the parent's own final prose cannot establish that outcome.

The public actions have separate purposes on both CLI and Server:

| Action | Inputs | Observation contract |
| --- | --- | --- |
| `agent.spawn` | non-empty `description` and `prompt` | Launch one child and return its receipt while the parent continues. |
| `agent.list` | optional exact returned `agent_id` | Read-only in-memory status of this agent's direct owned children in the current session. No database query, terminal wait, or result collection. |
| `agent.get_result` | exact returned `agent_id` | Collect the child outcome when needed, including after an ordinary spawn. It may briefly wait or reconcile durable state. |

Each call uses the corresponding `action` value. A spawn `name` is a mailbox
label, not an identity for status filtering or result collection. `list` covers
live children and a bounded recent-terminal cache for the same parent agent,
including across its turns in the same session. It does not enumerate peers'
children, all descendants, durable Work, or complete historical runs. Its
`coverage: "in_memory"` and observation timestamp describe that limited view.
An absent or evicted entry yields no matching child; it remains unknown, not
completed or cancelled. Status inspection neither stops a child nor consumes
its result. Use `get_result` when the outcome is needed, not for busy-polling
status; its bounded child wait does not bound any durable reconciliation I/O.

The full journey additionally requires semantic child messages and terminal
outcomes to be shown promptly with their child identity and delivered to the
parent at the next safe model boundary. Notifications must not interrupt an
executing side effect or start one model call per progress event. If the parent
becomes idle with an active child, the intended contract keeps the request
nonterminal and permits at most one serialized continuation for a later child
result. Failed continuation must preserve the result without rerunning the
child. A user cancellation must close descendant admission and converge the
child tree; an ordinary parent status answer must not cancel it. These are
verification obligations, not guarantees established by launch/list support.

An empty parent response while a direct child or requested reply is outstanding
uses that same input wait. It must not spend the bounded empty-final-answer
recovery or enter text-only synthesis before the required input is available.
Once the child result is delivered, an empty synthesis response retains the
ordinary one-retry contract within the existing execution budget.

The launch mode is not an authority change. It preserves the same admitted
model, tool permissions, execution deadline, lineage, and cancellation owner
that a joined child would have. Explicit background handoff below transfers
presentation and continuation ownership; it is not needed for parent/child
concurrency. Launching does not reset or extend the inherited wall-clock
budget, bypass insufficient-deadline admission, or widen the child's tool
allowlist or provider access. Deadline and permission authority remain with
[multi-agent runtime](multi-agent-runtime.md) and
[safety and permissions](safety-and-permissions.md).

## Fixed-group journey: structured fan-in

`agent_fanout.start` retains atomic foreground fan-in. Foreground describes
the logical relationship to the parent, not whether the terminal UI can
process input.

```text
accepted -> dispatching -> running children -> fan-in -> synthesizing -> answered
                                |                 |
                                |                 +-> partial terminal result
                                +-> actionable wait -> resumed or explicitly stopped
```

The runtime launches group slots concurrently and keeps the client
interactive. It emits deterministic lifecycle projections while the parent
tool call waits. It does not call the parent model merely because a slot
started, emitted progress, or completed before its siblings. After the whole
group settles, the canonical bounded aggregate becomes one tool result and
the parent gets one synthesis boundary.

This gives the user the clarity of synchronous waiting without freezing the
UI or serializing the children.

## Explicit background handoff

For shell commands, the supported control releases the foreground wait as
described above. The following fanout handoff remains a target journey. The
user may move foreground work to the background. In the terminal this is
an explicit control such as Ctrl+B; other clients may expose the same typed
control. This changes ownership for a foreground fanout or other work that is
still awaiting a joined result; ordinary single-child concurrency needs no
handoff.

```text
running foreground -> handoff accepted -> running detached -> result ready
                                                   |              |
                                                   |              +-> one continuation lease
                                                   +-> needs attention / failed
```

A handoff is complete only after the runtime returns a stable work-unit id and
the UI confirms where status and output can be found. Individual child events
update the task projection but do not each start analysis. Terminal fanout
causes at most one continuation attempt for the group version.

Detached continuation must be durable and idempotent before it is made the
default anywhere. Its authority tuple is `(session_id, group_id,
terminal_version)`. Acquiring the continuation lease, collecting results, and
recording synthesis settlement must tolerate duplicate and reordered
notifications. If automatic synthesis cannot run, the group remains
`result_ready`; it must not be marked reconciled and must not rerun children.

## Visible states and user actions

The table describes joined fanout and explicit background handoff targets.
Its model-boundary restrictions do not prevent independent parent work after
a single-child launch receipt.

| Runtime state | What the user sees | Valid actions | Model boundary |
| --- | --- | --- | --- |
| dispatching | Starting the named work unit | stop | none |
| running | stable `active / target` progress and inspect shortcut | inspect, guide, stop, background | none |
| waiting for approval/input | exact blocker and owner | resolve, stop | only if a model decision is actually required |
| executor offline | reconnect target and durable ownership | reconnect, reroute when safe, stop | none |
| partial terminal | completion ratio and causes | synthesize available evidence, resume existing work, stop | one |
| terminal | result ready / synthesizing | inspect, retry synthesis | one |
| synthesis failed | children preserved; synthesis retryable | retry synthesis, inspect | retry uses the same results |
| cancelled | what was cancelled and what survived | inspect partial output | none by default |

The task list is a projection, not another lifecycle authority. Its ordering
and selection are stable across progress refreshes. The footer advertises the
management shortcut whenever managed work exists. A launch receipt is a
runtime-owned UI fact, never assistant prose that a cooperative model must
remember to emit.

## Deployment ownership

| Deployment | Execution owner | Durable lifecycle owner | Wake / recovery contract |
| --- | --- | --- | --- |
| CLI local | CLI process | local session journal and workspace projection | joined fanout performs fan-in in the live foreground future; an explicit detached group must be rediscovered from the journal on restart |
| CLI + Server / bridge | edge may execute tools and children | server session/run rows plus edge outbox | joined fanout stays in the owning turn; lost edge facts replay through the outbox; a new session turn must be able to recover result-ready work |
| Server only | server run owner | MatrixOne run, child-run, event, and transcript rows | SSE disconnect never changes execution state; replay reconstructs progress; server restart must expose an honest continuation or failure |
| Edge + Server | selected edge executes workspace-bound work | cloud is C0 lifecycle authority | edge offline becomes visible waiting; reconnect/outbox replay is idempotent; cloud never infers completion from transport loss |

These recovery obligations are separate from the process-local `agent.list`
cache; list does not reconstruct them or prove that they have been validated.

## Evidence for future plan comparison

Model selection and child execution must remain joinable by source intent,
parent/child run IDs, selected Offering ID, and the actual inference route and
request IDs. The candidate-aware assessment records its catalog snapshot
digest, outcome and bounded selection summary in the existing trace/Explain
path; the inference ledger and child lifecycle record actual requests, token
coverage, timings and terminal results. A fixed-model choice, a failed
admission, and a child that never ran are different outcomes. Provider success
or a judge label is not task quality. Future tuning can compare alternate
plans only with a separately defined outcome check and identical cost-coverage
rules, including auxiliary judgments and failed attempts. This contract does
not create a second cost ledger, extra per-child database reads, or an
automatic router before comparable evidence exists.

The comparable unit is one case and one complete root execution, including its
children. Keep the tested case/input digest, plan/configuration digest, runtime
revision, selection method and catalog digest, root and child run IDs, and the
evaluator/rubric version with the evaluated artifact. Follow the existing
selection, tool invocation, child run, inference invocation and physical
request identities rather than matching display model names or timestamps.
Report task quality, execution success, wall-clock latency, provider token
coverage, and cost separately. Sum physical attempts across the task tree,
including selection, retries and failures, without counting replayed receipts
twice; concurrent child durations must not be summed into wall-clock latency.
Historical monetary estimates require a price basis frozen when the route is
admitted. If the price, cache rate, or usage is missing, expose the priced
subtotal and missing coverage instead of zero or a complete-cost claim. A
provider-completed request is not a quality label, and a later price update
must not rewrite an earlier experiment.

Client disconnect is not cancellation. Explicit cancel is a durable control
that converges the root and descendants. A slow live stream may drop a
non-terminal presentation event, but durable replay must reconstruct the
current projection.

When recovery releases a run as paused without a blocking wait, Work carrier
reconciliation also pauses its unsettled primary attempt before the next turn
selects work. Continuation transfers that same attempt to the new run; it does
not create a replacement task or overwrite a recorded outcome. A blocking
user-resume pause does not grant continuation ownership. This is session
continuation, not automatic replay of an interrupted execution.

## Unhappy-path obligations

- A malformed Work admission decision receives at most the existing bounded
  repair, with its parser diagnostic and validated boundary hints; the invalid
  candidate does not gain authority. If admission remains unavailable, repeating
  `start_work` within the same turn cannot retry that cached decision and is
  reported as non-retryable at the carrier boundary. This does not prevent
  reassessment on a new turn or a supported context invalidation, nor change
  retryability of an already admitted durable operation.
- Requested mutations must remain explicit admission actions; goal prose is
  insufficient. Missing cancel/replace targets are malformed, never silently
  removed. The model may choose an initial target when the user delegates that
  choice, but cannot invent an externally bound identity. Mutation delivery
  triggers govern graph changes; nested task prerequisites govern execution,
  so immediate creation does not imply immediate execution.
- Admission task and goal text use the canonical Work domain validators.
  Concise-generation targets are not validity limits: exceeding a brevity
  target alone cannot reject Work or trigger repair. Domain byte bounds,
  non-blank content, graph structure, and authorization remain enforced without
  truncating accepted text. Generation token and deadline budgets remain bounded
  independently of the largest domain-valid object.
- Validate the complete fanout before spawning any slot. A partial launch has
  a fixed target count, explicit rejected slots, and no automatic replacements.
- Fanout admission is bounded to 50 slots. The bound is enforced before any
  child is admitted and is repeated when projecting a slot identity, so an
  oversized request cannot allocate an unbounded group or bypass the runtime
  contract through recovery.
- Fast children may finish before the UI draws the launch receipt; monotonic
  projection must skip directly to terminal without showing a later running
  regression.
- Child failure, interruption, timeout, cancellation, or waiting preserves its
  distinct cause. Parent synthesis discloses the completion ratio.
- User cancel reaches every active descendant and unblocks a foreground wait.
  A cancellation API failure is visible and repairable; it is not reported as
  success.
- Guidance accepted while children run has a visible delivery state. It is
  applied at a safe boundary or returned to the composer; it never disappears.
- Lost, duplicate, or out-of-order progress cannot cause duplicate synthesis.
  Canonical group state, not notification count, decides readiness.
- Oversized results remain valid structured data. Truncation carries byte
  counts and stable continuation/artifact references; nested JSON is never
  corrupted into an unparseable string.
- Empty model output or synthesis failure does not create an empty visible
  assistant transcript item and does not consume the child results.
- Restart never labels an unproven running operation completed. It restores a
  checkpoint, exposes session continuation, or marks the crashed execution
  failed while preserving terminal child evidence.
- Recovery work is resource-bounded. Test processes never write the user's
  journal/outbox, multi-session recovery amortizes durable transactions across
  a batch, and a backlog above the health high-water mark cannot create a
  zero-delay CPU/I/O loop. Sync lag is visible degradation, not permission to
  starve the interactive journey.

## Verification gates

The following are required verification gates, not a record of completed
validation. Schema and handler tests cover only their own boundaries; full
asynchronous settlement needs the parent-loop and deployment journeys too.

Unit and property tests must establish the state machine:

- a gated single child lets the parent make independent model/tool progress
  before the child finishes, with no mode flag or Ctrl+B;
- messages and terminal results update the UI promptly, enter the parent's
  next safe model boundary, and wake an idle dependency wait;
- a parent status answer cannot make an active child an orphan or settle the
  whole request; child completion after parent idle leases one continuation;
- status observation covers only direct owned children in the current session,
  including across parent turns, performs no storage read, terminal wait, or
  result collection, and treats a missing or evicted cache entry as unknown;
- result collection works with the ordinary spawn receipt, and child deadline,
  model admission, permission, and cancellation constraints remain enforced;
- no parent model opportunity exists between fanout launch and group fan-in;
- fanout children start concurrently and one early completion does not settle
  the parent tool;
- status is monotonic under duplicate, missing, and reordered events;
- a group version obtains at most one synthesis lease;
- cancellation and foreground-to-background promotion wake every waiter;
- partial and oversized results preserve provenance and recovery references.

The terminal PTY journey must use an adversarial mock model that attempts to
claim completion immediately after launch. For a single child, the test must
allow independent parent model/tool progress while rejecting premature request
settlement and must validate a later result after parent idle. For a joined
fanout, it must prove there is no parent model request before group fan-in
unless the user explicitly hands it to the background. While children are
gated, the journey should verify the runtime launch receipt, responsive
composer, Shift+Down/Ctrl+B task navigation, stable selection across refreshes,
explicit backgrounding, and cancellation. `/tasks` is the equivalent slash
entry to that same live panel and should be covered by the same adversarial
journey when the PTY route is exercised.

The online CI gate uses the real Axum routes and real MatrixOne with a mock LLM
or no LLM. It must assert actual root/child run rows, non-null ownership,
transcript contents, event order, disconnect/replay behavior, partial fan-in,
and exactly one terminal synthesis, including single-child completion after
parent idle. A second lane must exercise Edge + Server transport loss and
outbox replay. Full journey validation belongs in `make test-online`; existing
passing tests do not establish coverage of an untested journey. An in-memory
harness or a PTY-only cooperative mock is not sufficient.

The minimum journey service levels are:

- launch projection visible within 500 ms of accepted child identities;
- zero parent LLM calls while a joined group is active before explicit handoff;
- exactly one parent synthesis boundary after a terminal group;
- stable task selection for every refresh generation;
- no terminal contradiction between task projection, transcript, and durable
  run rows;
- no empty visible assistant messages;
- no rerun of children when synthesis or delivery is retried.
- zero writes under the real user data directory from a Cargo test process;
- recovery transaction count grows with source batches, not source count, and
  degraded backlog draining has a non-zero duty-cycle cooldown.
