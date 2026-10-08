# Orchestration

> Status: target design contract.
> Last updated: 2026-10-03.

Orchestration owns multi-agent coordination, delegation, fanout/fanin, model choice per agent, and result integration. It does not own provider routing or lifecycle state machines.

## Principles

- Delegated agents share the same backbone semantics.
- Sub-agent execution must preserve parent trace and task lineage.
- Fanout should be explicit, bounded, and observable.
- Delegation failure should degrade the relevant branch, not corrupt the parent run.

Agent profiles are explicitly selected; they do not expose keyword-based
auto-activation triggers. Generic delegation has two coordination policies:
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
Server delegate, dynamic-agent and skill children use the runtime's common fresh
loop state, then install their exact identity, authority, context, transport and
execution budget. Entry-specific configuration does not define another loop
lifecycle. Fork context inheritance and recursion restrictions are independent
of result aggregation: Fork applies its declared aggregation through the same
result combiner as FanOut.
Fork children retain a non-delegating restriction in their protected admission
authority, including after recovery and when no prefix was inherited. Ordinary
spawn and fanout enforce that authority before admission; a reusable profile
cannot widen it. Prefix inheritance alone does not impose this Fork restriction.
Child execution deadlines belong to the request, not a reusable executor.
FanOut and Fork admit one absolute deadline before model preparation and pass
it unchanged through queuing into Server loops, tools and nested children.
Sequential retains its explicit per-stage timeout policy; a zero timeout adds
no deadline. Invalid deadline ranges fail before child admission.
Admission freezes separate ordinary-work and total execution cutoffs. Final
synthesis reserves half the admitted interval, capped at 30 seconds; elapsed
time and retries never repartition or renew either cutoff. Ordinary model and
tool admission use the work cutoff, while final synthesis and result draining
use the total cutoff. A foreground child's total cutoff precedes its parent's
work cutoff by the existing delivery grace, and its own work/synthesis split
is frozen once. Short usable child intervals do not require a fixed 60-second
minimum; insufficient intervals reject launch without claiming member work.
Tool dispatch rechecks that same admitted cutoff after asynchronous preparation
and waits. Only exact runtime-owned Work settlement uses the total cutoff;
ordinary calls and completion-action work retain the work cutoff. The selected
invocation-local admission deadline is not serialized or supplied by providers.
A claimed invocation that expires before dispatch still settles once as not
executed. If an MCP call already started, expiry prevents another call without
claiming prior execution was side-effect-free or settled. Acknowledged results
retain their existing completion custody and total-time drain.
The scheduler does not parse task output into a second findings store or copy
it into ancestor state rows. Sequential execution stops at a paused or waiting
stage instead of launching a successor without a settled predecessor. Active
stages observe cancellation and deadlines even if an executor ignores its token;
the existing short publication grace and bounded, owner-fenced reconciliation
preserve actual terminal outcomes or explicitly unfinished recovery state.

## Delegation model

Children use the existing same-session run tree and execution owner. Built-in
profiles may narrow tools, skills, read-only access and budgets; they cannot
grant capabilities the parent or provider lacks. Explicit model requirements
remain authoritative; omitted child model selection inherits the parent binding.

Users can inspect child progress, send guidance, pause, resume or cancel, and
open an exact child transcript without flooding the parent conversation.
Run identity connects launch, Work attempt, messages and physical provider
calls. Summaries are not additional billable usage, and unknown token, cache
or price coverage remains explicit.

Session Audit aggregates physical provider attempts across root and child runs
with `request_usage.scope=session_all_runs`. Missing token, cache or price observations
remain unknown rather than zero. Historical price estimates do not establish
actual provider billing; inference observations and Audit own these facts.

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
Both entrypoints return launch receipts; child results arrive through the
existing result query and completion barrier. Agent runs do not have a
synchronous spawn mode or a foreground-to-background promotion operation.

Restricted built-in read-only profiles retain the default `tool_search` discovery
backbone. Persona defaults, explicit child requests and parent permissions still
intersect; discovery cannot grant execution of a target outside that admitted
scope. An explicit empty allowlist or a parent prohibition remains authoritative.
Shell process detachment remains owned by the shell execution boundary.
Every production launch consumes a one-use `PreparedSpawn`; model admission
and static slot selection do not have an alternate execution path. Launch
registers cancellation controls synchronously without starting I/O. The child
supervisor starts the returned future, and rechecks mutable generation,
cancellation, and deadline authority at execution boundaries.
Dynamic and precreated children share that task owner and panic cleanup. A
settlement or projection panic releases local execution custody and wakes its
parent with a Waiting projection that retains its direct-child obligation.
Only durable reconciliation may publish the winning terminal result; missing
or unreadable durable state also remains unfinished.
A failed preparation or model admission reports a rejected request with an
explicit non-execution fact. CLI and Server carry that same fact into the
execution journal; it remains observable as a blocked request and does not
create an unfinished child obligation. Failure after launch, partially started
fanout, and unknown outcomes retain their execution and settlement obligations.

CLI agent and fanout requests execute through the Server control plane, including
one-shot chat and app-server. The CLI does not launch a second local agent run or
append local child results after the Server stream terminates. These agent
entrypoints reject isolated Git workspaces; they do not provision a local
worktree implicitly.
Interactive CLI state retains local recovery projections for historical child
and fanout queries. It does not install a root delegation engine, capture a
parent prefix for local children, or register a root agent mailbox.

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
Definite routing or delivery rejection reports `success=false` and
`executed=false`; the canonical tool outcome records rejection without an
unfinished execution obligation. An ambiguous delivery reports `executed=null`
and retains unknown execution, even when the transport accepted the envelope.
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
Accepted sends carry typed evidence through the internal execution result, not
public tool output or metadata. Child settlement retains durable communication
in the existing generation-fenced event append, including when paused; progress
remains transient. Replay cannot resend an envelope or create new send authority.

Transport owns consumption acknowledgement, claims, and redelivery. There is no
application ACK/NACK envelope, sender retry tracker, or in-memory dead-letter
queue. Immediate routing/transport errors remain visible to the sender. Durable
transport failure records belong to the transport. The CLI does not expose a local
messaging counter or a separate application delivery/retry status.
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

The primary model interprets natural-language delegation requests and proposes
the child's model through the canonical `requested_model_policy` control.
Both resident and discovered schemas expose that same control. Fixed selectors
use an exact authorized Offering ID or configured name; names inside task
content, quoted text, or requested output do not select an execution model.
Preserve the user's requested family, version, variant and source. Do not
substitute a nearby model when the requested one is unavailable.

The first provider request includes a bounded, structured observation of the
authorized model candidates, outside the stable system/tool prefix.
This context carries observations, not a second selection instruction; the
canonical tool contract owns execution-versus-content interpretation. A cold
request loads this catalog once, including ordinary conversation; subsequent
requests reuse the same principal-isolated cache (60-second freshness, at most
1,024 entries and 64 KiB serialized content per cached entry). Concurrent cold
reads for the same principal coalesce. Request descendants reuse the observed
generation; failures are retained for that request, not retried every round.
The complete authentication principal, including provider/request scope, and
service identities form the cache key. This intentionally does not share a
provider request's observation with a different authorization.

Explicit refresh and pagination use the existing authenticated `model_catalog`
boundary, never workspace configuration or credentials. Oversized catalogs
are not retained across requests. Discovery pages are observations, not
execution grants: execution authorization remains fresh. Incomplete pages
cannot prove a choice unique or absent across the complete catalog.

Runtime validates exact selectors, authorization, capability, lineage, profile,
batch capacity, and independently supplied typed user constraints before any
child starts. A proposal is not user authority and cannot weaken a hard typed
requirement. Ordinary delegation does not invoke a second auxiliary interpreter
to generate hard model or reasoning requirements from the same user text.
Existing scoped typed constraints may still use bounded scope binding; that
judgment only establishes applicability and cannot create execution controls.
Jev/Jev-like assistance is not required for fixed model selection; Auto remains
unavailable until its own evidence and routing contract are implemented.

Frozen invocation decisions retain their original identity and constraints
during retries and replay. A correction is a new proposal, not permission to
rewrite a prepared invocation. Missing, malformed or stale typed authority
fails closed. Existing semantic-derived records are not silently relabelled or
downgraded during replay. Selected model identity, exact control, admission
failure and provider usage remain visible through the existing execution
trace and Explain paths; removed auxiliary calls produce no synthetic usage.

A model-only request does not imply a reasoning level. The primary proposes
reasoning only when requested; runtime validates its supported exact protocol
and effort. It never downgrades unsupported effort into generic thinking.
Natural-language fidelity remains a live-evaluation obligation: exact catalog
membership proves authorization, not that a model correctly understood intent.

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
independent of extraction order.
`max_output_tokens` is exposed by the resident `agent.spawn` schema and is a
ceiling for the first child model round, including its retries. Shared child
admission carries this typed cap. Catalog output limits remain separate.
Final request assembly cannot enlarge that cap or replace an exact reasoning
control through route defaults, convergence, or settlement heuristics.

Server fanout prepares every slot before launching any child and admits distinct
non-inherited Offerings in one bounded user-scoped batch. An inherited-only
batch reuses the parent's exact Offering and model snapshot without a catalog
lookup. Mixed batches require
the exact parent Offering identity for inherited slots and fail before remote
I/O if it is missing. Preparation failure never authorizes a partial launch.

CLI root model or reasoning selections use `/model-access/admit` when Server
validation is required. Its response binds the selected model and limits;
it conveys no reusable authorization token.

## Failure handling

Delegated-model assessment distinguishes a valid semantic refusal from an
invalid service response. With supplied tasks, every resolved requirement
explicitly names its applicable slot indices (an empty array means none);
without supplied tasks, it carries no slot binding. Validation and provider
recovery share one owner and at most two logical calls in total. A configured
fallback, provider deadline retry or invalid-response correction consumes the
same second-call allowance, respecting cancellation and execution budget.
An exhausted invalid response is service unavailability, not evidence that
the user's model reference is ambiguous. The failed decision is reused for
the same authenticated intent; changing spawn parameters or reading workspace
configuration cannot repair it. Physical attempts remain separately accounted.

- Child failure is recorded as branch failure.
- Parent may continue if aggregation policy allows partial results.
- A launched `spawn` receipt or running `get_result` response is nonterminal,
  not a failed child. It stops blocking final reconciliation only after the
  same child's successful producer-owned terminal result has been observed by
  the parent; final quality evaluation consumes the same typed proof.
  Nonterminal `still_running`, `waiting`, and `paused` query observations follow
  the same rule after that child's successful result is observed. This does not
  authorize resuming a paused child or treating an unresolved wait as success.
  Fanout launch and nonterminal result-query receipts follow the same rule for
  the complete canonical group. An early slot query does not require another
  query after all group results have been observed at the completion barrier.
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
