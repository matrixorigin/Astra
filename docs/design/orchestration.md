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
The scheduler does not parse task output into a second findings store or copy
it into ancestor state rows. Sequential execution stops at a paused or waiting
stage instead of launching a successor without a settled predecessor. Active
stages observe cancellation and deadlines even if an executor ignores its token;
the existing short publication grace and bounded, owner-fenced reconciliation
preserve actual terminal outcomes or explicitly unfinished recovery state.

## Delegation model

### Team configuration ownership

Team definitions have one owner-scoped persistence contract. CLI commands use
the existing HTTP adapter; their registry is a display projection, not a second
configuration store or template authority. An empty roster is a valid draft,
but execution rejects it before admitting children. Configuration edits retain
the complete definition, including member profiles, capabilities and shared context.
The accepted save response supplies the persisted identity without a follow-up
read. Failed reads or writes must not publish success or a locally committed
configuration, and standalone commands must return a failing exit status.
Snapshot restore uses the complete saved configuration and preserves the current
Team identity. It requires the exact returned snapshot ID, publishes only the
accepted save response, and never checks out Git or changes running tasks.

### Team user journey (target, not yet a proven runtime guarantee)

Native CLI and TUI Team tasks must enter the ordinary root-turn owner with a
selected lead and an authorized member set. Configuration commands and fixed
fanout alone do not establish the interactive lead journey below; acceptance
requires the same session, guidance, callbacks and child observations across
successive turns.

The native entrypoints use the same configuration:

```text
astra team create delivery
astra team add-member delivery lead --can-delegate -- Describe the lead's responsibilities
astra team add-member delivery developer -- Describe the member's responsibilities
astra team run delivery <task>
```

In the TUI, use `/team run delivery <task>`;
configuration remains in the CLI. Subsequent ordinary input retains that selection. Canonical turn commit atomically
retains the admitted Team/lead selection as intent for the next root.
Authenticated CLI resume reads the Server-owned generation even when a local
replica exists; local-only restore is reserved for an unauthenticated session.
Resume returns it only at the matching canonical cursor; a new root reauthorizes the
current owner-scoped configuration. Switching or clearing a session drops the
previous selection, while an explicit Team launch overrides restored intent.
Same-run recovery continues to use its frozen admitted profiles.
Native entrypoints select the sole delegation-capable member from the already
loaded configuration. With zero or multiple such members, use `team info` and
`--lead-agent-id <agent_id>` to choose explicitly; role names and member order
never choose a lead or grant permission. Server admission authorizes and freezes
the explicit resolved identity. This UI default does not change the protocol's
`lead_agent_id: null` meaning: an ordinary root with an admitted member directory.
Native `team run --json` reuses the ordinary turn's terminal JSON; its hidden
`--stream-events <path>` flag writes the same structured event stream as `chat`.
For an isolated one-shot task, `team run --no-resume` uses ordinary Chat routing
to create a new conversation instead of attaching recent history. In the TUI,
start a fresh conversation with `/clear` before `/team run`; `--no-resume` is
only applicable to one-shot CLI execution.
Role names do not grant delegation permission. `--model <available-model-name>`
on `add-member` is optional; an explicit root model selection takes precedence
over the lead's configured default. These entrypoints alone do not prove the
full acceptance criteria below.

Built-in templates declare one delegation-capable coordinator:
`dev/planner`, `research/synthesizer`, and `review/reviewer`. Each can start
direct members at depth one; other members retain non-delegating authority.
Selecting a different lead does not silently promote its permissions.

A Team is a reusable collaboration configuration, not a second execution
engine. The user addresses one accountable lead with an objective; they need
not manually invoke every member or understand transport contracts. The lead
clarifies missing requirements, maintains the plan, assigns work, integrates
observed results and checks acceptance. Product manager and developer are
ordinary configured responsibilities, not hard-coded runtime roles.

Profile admission belongs to ordinary root and child execution. Built-in agent
definitions and owner-authorized Team members normalize to the same effective
profile, preserving prompt, model intent, skill and MCP selection, tool limits,
read-only restrictions, initial turn limits and delegation scope. Skills are
not tool permission aliases. A profile can narrow current execution authority;
it cannot grant capabilities that the parent or selected provider lacks.

Explicit profile MCP selection is not yet wired to the shared provider binding
and dispatch owner. Run admission rejects nonempty `mcp_servers` rather than
silently ignoring it. An empty selection inherits the currently authorized
parent MCP scope; it does not establish new connections or credentials.

The admitted member set is immutable and scoped to the run, never installed
into a session-global registry. Ordinary spawn, fanout and coordination consume
the same preflight facts; all fanout slots must pass before any child starts.
Protected `run_started` facts retain the effective configuration and its
owner/source identity. Recovery preserves those facts rather than resolving a
subsequently edited Team, while revalidating current capability and model
authorization. Public request metadata cannot establish child-run authority.

Children execute through the existing same-session run tree. Client-side
ordinary root admissions are not a replacement for internal child admission:
sharing a parent's session writer causes contention, while opening unrelated
sessions loses authoritative lineage. Native CLI/TUI must retain the ordinary
root's authenticated Edge delivery, callback, cancellation and observation
channels. Team execution uses this ordinary root-turn entrypoint rather than a
separate batch executor.

Multi-step collaboration uses the existing Work dependency and attempt owner.
Member execution, messages, user guidance, cancellation, pause and recovery use
the same child-agent backbone as ordinary delegation. Sequential/FanOut/Fork
are coordination policies, not independent lifecycle authorities. A member's
claimed completion is insufficient to settle an attempt or finish the Team.
Blocked dependencies, questions and failed acceptance may require another
step; neither a one-shot aggregate nor a transport acknowledgement proves the
objective has been achieved.

Explicit model choices remain authoritative. Optional model selection may
choose among authorized candidates within the existing budget and capability
constraints; it cannot relax a hard requirement. Model choice is not required
for communication, step tracking or lifecycle correctness.

When a child request omits a model policy, its admitted profile's model default
is resolved before falling back to the parent binding. An explicit `inherit`
request selects the parent binding; explicit fixed choices and admitted user
requirements remain authoritative. This normalization precedes shared model
admission for both single spawn and fanout, so launch receipts and execution
use the same prepared Offering rather than changing models after launch.
An HTTP request with an explicit Offering and omitted model policy has fixed
selection intent; only explicit inheritance can replace it with the lead default.

The user can inspect progress, blockers and artifacts, change requirements,
pause or cancel, and drill down into member execution without flooding the
lead transcript. Execution identity must connect Team, lead, child, Work
attempt and physical provider calls in existing Trace/Explain/Audit facts.
CLI root history retains exact-run durable received messages in the existing
transcript journal, including interrupted turns and one-shot submissions.
Replayed evidence is deduplicated by typed identity, not message text; it does
not enter provider-facing conversation history or authorize completion.
Team summaries are aggregates, not additional billable provider usage;
unknown usage, cache or price coverage must remain explicit. Acceptance must
exercise a real multi-step exchange and recovery, not only fixed fanout.

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
worktree implicitly. Native Team execution uses this same root-turn and child
execution boundary.
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

When a user names a delegated model in natural language, one candidate-aware
judgment resolves the request against the current authorized Chat catalog.
It must preserve the requested family, version, variant, namespace and source;
an unavailable or ambiguous model stops the child before execution. A family
plus version can identify a unique model in that snapshot when the user omitted
a variant; multiple matching variants or sources require clarification. Runtime
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
database write. The validated ambiguity reason is returned to the caller, while
Explain retains only its bounded structured summary. A non-retryable delegation
rejection blocks that operation, not independent parent work. Frozen admission
results, stall limits and turn budgets prevent repeated paid interpretation;
active Work attempts retain their existing typed settlement boundary.

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
