# Background work user journey

Status: implementation contract

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

## Default journey: structured fan-in

`agent.spawn` and `agent_fanout.start` are foreground by default. Foreground
describes the logical relationship to the parent, not whether the terminal UI
can process input.

```text
accepted -> dispatching -> running children -> fan-in -> synthesizing -> answered
                                |                 |
                                |                 +-> partial terminal result
                                +-> actionable wait -> resumed or explicitly stopped
```

The runtime launches independent children concurrently and keeps the client
interactive. It emits deterministic lifecycle projections while the parent
tool call waits. It does not call the parent model merely because a child
started, emitted progress, or completed before its siblings. After the whole
group settles, the canonical bounded aggregate becomes one tool result and
the parent gets one synthesis boundary.

This gives the user the clarity of synchronous waiting without freezing the
UI or serializing the children.

## Explicit background handoff

The user may move foreground work to the background. In the terminal this is
an explicit control such as Ctrl+B; other clients may expose the same typed
control. A model must not silently weaken the default join contract.

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
| CLI local | CLI process | local session journal and workspace projection | the live foreground future performs fan-in; an explicit detached group is rediscovered from the journal on restart |
| CLI + Server / bridge | edge may execute tools and children | server session/run rows plus edge outbox | live fan-in stays in the owning turn; lost edge facts replay through the outbox; a new session turn can recover result-ready work |
| Server only | server run owner | MatrixOne run, child-run, event, and transcript rows | SSE disconnect never changes execution state; replay reconstructs progress; server restart must expose an honest continuation or failure |
| Edge + Server | selected edge executes workspace-bound work | cloud is C0 lifecycle authority | edge offline becomes visible waiting; reconnect/outbox replay is idempotent; cloud never infers completion from transport loss |

Client disconnect is not cancellation. Explicit cancel is a durable control
that converges the root and descendants. A slow live stream may drop a
non-terminal presentation event, but durable replay must reconstruct the
current projection.

## Unhappy-path obligations

- Validate the complete fanout before spawning any slot. A partial launch has
  a fixed target count, explicit rejected slots, and no automatic replacements.
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

Unit and property tests establish the state machine:

- no parent model opportunity exists between fanout launch and group fan-in;
- children start concurrently and one early completion does not settle the
  parent tool;
- status is monotonic under duplicate, missing, and reordered events;
- a group version obtains at most one synthesis lease;
- cancellation and foreground-to-background promotion wake every waiter;
- partial and oversized results preserve provenance and recovery references.

The terminal PTY journey uses an adversarial mock model, not a cooperative
script. It attempts to claim completion immediately after launch. The test
must prove that request is never sent to the model. While children are gated,
the test verifies the runtime launch receipt, responsive composer,
Shift+Down task navigation, stable selection across refreshes, explicit
backgrounding, and cancellation.

The online CI gate uses the real Axum routes and real MatrixOne with a mock LLM
or no LLM. It asserts actual root/child run rows, non-null ownership,
transcript contents, event order, disconnect/replay behavior, partial fan-in,
and exactly one terminal synthesis. A second lane exercises Edge + Server
transport loss and outbox replay. These tests are part of `make test-online`;
an in-memory harness or a PTY-only cooperative mock is not sufficient.

The minimum journey service levels are:

- launch projection visible within 500 ms of accepted child identities;
- zero parent LLM calls while a structured group is active;
- exactly one parent synthesis boundary after a terminal group;
- stable task selection for every refresh generation;
- no terminal contradiction between task projection, transcript, and durable
  run rows;
- no empty visible assistant messages;
- no rerun of children when synthesis or delivery is retried.
- zero writes under the real user data directory from a Cargo test process;
- recovery transaction count grows with source batches, not source count, and
  degraded backlog draining has a non-zero duty-cycle cooldown.
