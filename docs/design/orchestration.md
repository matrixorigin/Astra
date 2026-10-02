# Orchestration

> Status: target design contract.
> Last updated: 2026-09-29.

Orchestration owns multi-agent coordination, delegation, fanout/fanin, model choice per agent, and result integration. It does not own provider routing or lifecycle state machines.

## Principles

- Delegated agents share the same backbone semantics.
- Sub-agent execution must preserve parent trace and task lineage.
- Fanout should be explicit, bounded, and observable.
- Delegation failure should degrade the relevant branch, not corrupt the parent run.

Agent profiles are explicitly selected; they do not expose keyword-based
auto-activation triggers. Team coordination has two execution strategies:
`Sequential` passes each output to the next agent with optional early exit;
`FanOut` runs independent agents with explicit aggregation. A context-sharing
`Fork` additionally has its own bounded child contract. There is no separate
pipeline strategy or producer/reviewer revision loop. Reviewing, producing and
revising are ordinary tasks, not runtime roles or acceptance heuristics.
Default topology uses agent count and explicit dependency facts; neither task
prose nor scenario labels choose an execution strategy. Production delegation
requires an explicitly wired executor; a stub is a test fixture, not a default
execution mode.

Child results and failures remain in the canonical lifecycle and journal.
The scheduler does not parse task output into a second findings store or copy
it into ancestor state rows. Sequential execution stops at a paused or waiting
stage instead of launching a successor without a settled predecessor. Active
stages observe cancellation and deadlines even if an executor ignores its token;
the existing short publication grace and bounded, owner-fenced reconciliation
preserve actual terminal outcomes or explicitly unfinished recovery state.

## Delegation model

The agent executing an objective owns its relevant skill selection, evidence
gathering, and result. When the user assigns an objective to a child, the parent
hands off the objective and constraints without pre-running its workflow or
copying skill instructions into the child prompt. The parent may separately do
independent work and must integrate only observed child results. An explicitly
requested parent-side prerequisite still runs before handoff. Without child
capacity, the current agent owns the objective and its matching skills. This
ownership rule does not bypass Work admission, tool policy, or cancellation.

A delegation should record:

- parent run id;
- child run id;
- parent task id when applicable;
- child agent profile;
- model override if any;
- provider/capability constraints;
- expected result contract;
- timeout and cancellation policy.

## Fanout/fanin

Fanout creates multiple child runs or work branches. Fanin merges results through a declared aggregation step.

Ordinary `agent.spawn` creates one independent child. A parent may propose several
spawns in one model response when the admitted execution topology permits
parallel children; each call retains its own identity, admission result, and
terminal state. A failed sibling does not roll back an already accepted child.
Use `agent_fanout.start` when the parent needs a fixed group with all-slot
preflight and group-level control. Preflight prevents launching a group with
an invalid slot, but provider or child execution can still fail after launch.
Neither carrier may override an authoritative primary-only Work topology.
Provider-authored parallel calls do not cancel a user-requested durable Work
graph or its deferred activation; a conflicting proposal is rejected.
An explicit Required Work decision is a turn-wide establishment fence:
neither a one-slot nor a multi-slot fanout proposal, valid or malformed, can
start work or authorize sibling calls before the `start_work` receipt. An
already-bound WorkItem retains its own execution authority.

An ordinary child running, waiting, or asking its parent a question does not
assign a canonical WorkItem attempt. `settle_work_item` is admitted only for a
runtime-assigned primary or delegated WorkItem attempt; an unassigned call is
rejected before settlement storage is accessed. The database remains the
authority for valid attempt settlement and replay.
Rejecting an invalid call does not close the parent's existing communication
authority while its own child result or correlated reply is still pending;
the invalid call remains non-retryable, and normal deadline, cancellation,
budget, and tool admission still apply.

Required fields:

```text
fanout_id
branch_id
parent_run_id
child_run_id
objective
result_contract
status
summary
```

## Asynchronous agent messages

The collaboration contract has two layers. The execution core owns run
identity, authorized routing, message custody, correlation, input adoption,
completion dependencies, cancellation, parking/wake, recovery, and trace. It
must be testable with scripted model outputs and controlled transports, without
a live LLM. Model prompts, tool schemas, candidate metadata, and tool results
form the driving layer: they help the model choose an action and understand the
runtime's authoritative outcome, but cannot manufacture lifecycle facts.

For a send attempt, tool results distinguish `queued`, definite `rejected`,
and `delivery_unknown`. An unknown attempt retains its original message ID and
must not invite a new-ID retry. A correlated answer settles only the exact
run-owned request from its authorized responder; unrelated text, wrong IDs,
and wrong senders cannot clear a completion dependency. A proposed final answer
with unresolved dependencies parks the execution under the same logical run.
Parking releases execution capacity, and an authorized wake re-enters normal
admission with the same budget. Accepted input must survive detachment and
restart, be adopted once, and be visible in the next model request before the
dependency is considered resolved. These are target invariants, not guarantees
established by an in-process mailbox test alone.

`agent.send_message` returns `queued` with a message ID after the routing/transport
path accepts the envelope. The sender continues without waiting for the receiver
or an application receipt. Parent, child, and peer messages use the same mailbox
consumer at safe execution boundaries, including while waiting for child results.
Semantic messages can wake the waiting parent before a child completes; transient
progress does not require another model round.
Model-authored coordination messages are limited to 3,000 characters; larger
content belongs in an artifact rather than an accepted-but-truncated message.
The receiver sees a question's exact message ID. An `answer` must carry that
ID as `request_id` and travels as a correlated response, not generic text.
The completion obligation belongs to the run's execution state, not to the
message envelope or the model's promise to wait.

Communication evidence distinguishes acceptance (`Sent`) from target-runtime
observation (`Received`). Neither proves model inclusion, compliance, or task
completion. Missing receiver evidence is unknown. Correlated requests and
responses remain semantic exchanges, including permission decisions; child
completion remains owned by the child lifecycle, not a model-authored result
message. Trace and Explain project these facts without inventing an applied state.

Transport owns consumption acknowledgement, claims, and redelivery. There is no
application ACK/NACK envelope, sender retry tracker, or in-memory dead-letter
queue. Immediate routing/transport errors remain visible to the sender. Durable
transport failure records belong to the transport; `/messaging` exposes observed
metrics, without a separate application delivery/retry status.
In-process direct and parent messages share one bounded inbox per canonical
address. The inbox retains original envelopes and capacity charges until ACK;
detaching a stream returns unacknowledged deliveries to its head. A second
consumer cannot replace an attached owner. A parent can receive messages while
between turns, but only at an address that has already been registered; an
unknown parent fails explicitly. A bound turn alias takes precedence over a
turn-addressed registration when resolving the parent's sender and delivery
identity. Delegation requires that caller-owned consumer; it does not create
an unread parent inbox just to make `queued` succeed. Ending a turn detaches; ending a session or
terminal child retires its address and turn aliases only after its existing
durable outcome write or authoritative reread confirms a terminal status.
A projected failure without durable evidence retains the mailbox for recovery.
Terminal cleanup owns the whole unregister-and-retire transition even if its
caller is cancelled. A mailbox-bearing child transfers reconciliation and its
exact retirement capability to an owned task before the parent's shared wait
deadline is checked. Even when that deadline is exhausted, the parent receives
a recoverable Waiting result while durable authority can still be established.
A recoverable Waiting child only detaches. The volatile transport rejects an
oversized direct message or an exhausted per-inbox/global byte budget before
acceptance (128 KiB/message, 4 MiB/inbox, 64 MiB/process, plus 4,096 envelopes
per inbox and 8,192 retained addresses). Charges include unacknowledged
deliveries and are released by ACK or terminal retirement; payloads remain
shared in-process, with size measured without a second payload allocation.
Broadcast remains a
best-effort notification, not a durable direct-message receipt.
Durable messages keep their original database envelope and claim authority instead of
being resent as a new message.

Acceptance is not an end-to-end delivery guarantee. In-process messages are
volatile; durable transport recovery does not establish crash-safe model adoption,
exactly-once processing, or receipt by every broadcast recipient. This contract
adds no receipt persistence, sender sweep, or status-query database I/O.

## Model selection

Per-agent model override is an orchestration decision, but it must still respect budget, policy, and trace requirements.

When a user names a delegated model in natural language, one candidate-aware
judgment resolves the request against the current authorized Chat catalog.
It must preserve the requested family, version, variant, namespace and source;
an unavailable or ambiguous model stops the child before execution. Runtime
checks exact quoted text, candidate membership, canonical IDs/names, scope,
authorization and capability, but does not run a second lexical parser over
model aliases. Semantic alias interpretation can still be wrong; its accuracy
needs live evaluation and the chosen identity remains visible in Explain.
Explicit structured selectors retain their exact-match contract. The judgment
reuses the admission catalog snapshot without a second catalog read.

The auxiliary interpretation of one user turn covers all proposed children in
one bounded judgment. Its compact output identifies exact user evidence,
eligible candidate IDs, optional task scopes, and reasoning controls. Runtime
verifies the deterministic facts and binds them to the original user text and
catalog snapshot; semantic accuracy is evaluated separately. An invalid or
uncertain judgment cannot silently choose a nearby model, relax a hard
requirement, or authorize Auto. A tool's proposed selector is matching context,
never user authority. This interpretation adds no separate catalog query or
database write.

A model-only request does not imply a reasoning level. The same judgment must
distinguish a request to *use* a reasoning control from a phrase the child is
asked to explain, compare, quote or output, and must account for negations and
later corrections. Its typed reasoning and exact source quote travel together.
Runtime checks that pairing, positive budgets, slot conflicts and provider
support; it no longer guesses semantic intent from a fixed phrase, negation or
sentence-separator list. Ambiguous intent is unresolved. A plausible but wrong
semantic judgment remains a measurable risk, not a deterministic guarantee.
The runtime never downgrades an unsupported exact effort into generic thinking.

Dynamic spawn and fanout resolve reasoning separately from the Offering.
After per-slot and shared defaults, an omitted reasoning control inherits the
effective parent setting only when the Offering identity matches. An explicit
`model_default` suppresses inheritance; a different Offering starts with its own
default. The parent snapshot carries the effective control, including per-turn
adjustments, rather than reconstructing it from a display model name.

Batch admission, prefix compatibility and child execution consume the same
resolver. Unsupported inherited controls fail admission just like unsupported
explicit controls. Fixed token budgets remain exact and must fit the output
limit; they are not translated into effort levels. This resolution is in-memory
and requires no parent-run lookup or additional persistence.
User-authored delegation requirements retain their default or hard strength in
the frozen invocation. An explicit slot choice may override a default but not a
hard requirement; applicable hard requirements are resolved before defaults,
independent of extraction order. A CLI child without a trusted task binder
rejects new nested delegation when it inherits unresolved or constrained
descendant requirements rather than silently dropping them.
`max_output_tokens` is a ceiling for the first child model round, including its
retries. CLI carries it as validated `context.max_output_tokens`; internal
delegation carries the same typed cap. Catalog output limits remain separate.
Final request assembly cannot enlarge that cap or replace an exact reasoning
control through route defaults, convergence, or settlement heuristics.

## Failure handling

- Child failure is recorded as branch failure.
- Parent may continue if aggregation policy allows partial results.
- A launched `spawn` receipt or running `get_result` response is nonterminal,
  not a failed child. It stops blocking final reconciliation only after the
  same child's successful producer-owned terminal result has been observed by
  the parent; final quality evaluation consumes the same typed proof.
  Failed or unobserved terminal results still block completion. When the
  parent has no independent work, it proposes a final answer; the existing
  completion barrier waits for direct children and resumes synthesis with
  their results, without shell sleeps, polling, or an extra database read.
  Observation timeouts do not cancel children. Completion waiting uses the
  host's authorized execution deadline, if present, rather than inventing a
  deadline; cancellation and user guidance remain active while parked.
- A committed child `paused` status is nonterminal, unlike an interrupted
  partial result. At either an explicit wait or the final-answer barrier, the
  parent yields a resumable waiting outcome without another model request or
  cancelling siblings. Available terminal sibling results remain staged and
  unresolved child/question obligations remain pending. A presentation-only
  `waiting` signal cannot establish this authority; durable resume clears it.
  Clearing a pause requires newer committed evidence for the same child run:
  a later event frontier within its generation, or a newer execution generation.
  A cached running snapshot or absent local executor is not resume authority.
  This propagation does not by itself provide cross-process parent recovery
  or durable capacity parking; those remain owned by the run continuation and
  admission contracts.
- Cancellation propagates according to delegation policy.
- Missing `action` or malformed delegation calls should produce targeted diagnostics and retry guidance.

## Test obligations

- Child run lineage survives resume.
- Parent cancellation handles children deterministically.
- Partial fanin is explicit.
- Tool/provider failures inside child runs preserve provider diagnostics.
- Sending and receiving semantic messages preserves asynchronous wakeups and
  request/response correlation without automatic application reply traffic.
- Message evidence and public tool descriptions distinguish acceptance and
  runtime observation from model inclusion and child completion.
