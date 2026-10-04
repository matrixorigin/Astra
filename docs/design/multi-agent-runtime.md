# Multi-agent runtime

> Status: target design contract.
> Last updated: 2026-07-07.

The multi-agent runtime defines how multiple agents cooperate under the same backbone without creating independent untraceable execution islands.

## Principles

- Every executing child agent run has durable lineage; an accepted child stopped
  before run creation retains an exact durable parent-owned terminal receipt.
- Parent and child share observation and audit semantics.
- Delegation has explicit objective, scope, provider constraints, and result contract.
- Parallelism is bounded and observable.
- Child failure should be isolated unless parent policy requires fail-fast.

## Coordination receipts and child outcomes

Agent-tool results distinguish accepted coordination from child execution
outcomes through one shared, typed result-family contract. Starting concurrent
work, admitting a wait, queuing a message, or listing children does not prove
that a child completed. An accepted control receipt is neutral coordination:
it neither creates a failed-child obligation nor supplies positive validation
evidence. Child results retain their terminal status and output requirements.

Malformed, unknown, rejected, or uncertain-delivery receipts must not gain
accepted-control authority. Existing run/call ownership checks, child completion
barriers, reply obligations, and Work settlement remain authoritative. Evaluation,
recovery, and Server result projection consume the same classification; a later
control receipt cannot erase an unrelated failed verification.
An observation error does not establish child termination or release ownership.
Presentation windows preserve the typed child envelope and pagination authority.

Model-authored child sizing (`complexity` and `initial_turns`) selects only an
initial, renewable execution slice. It is not a user-owned hard limit: a parent
model cannot stop a child after an arbitrary number of rounds. Explicit
request/runtime limits and inherited wall-clock deadlines remain authoritative.
The child must retain a path from a successful action to a truthful terminal
answer while it is still making progress.

## Child deadline ownership

A delegated child must not consume the parent's final-answer reserve. The
parent retains the runtime's existing text-only final-convergence window after
the child settles. Optional parent verification can use time left when a child
finishes early; it is not reserved by shortening every child's deadline. The
child receives the earlier absolute deadline and must retain one ordinary
provider-action window plus its own final-convergence window. Fanout siblings
share that child deadline; the runtime returns completed and usable partial
results together rather than discarding them when another slot times out.

Delegation starts only when the remaining deadline can support one ordinary
child work window, the child's final-convergence window, and the parent's final
answer window. If that minimum is unavailable, delegation is skipped before
child admission. This is a degraded optimization, not a failure of the user's
task: the parent continues with available evidence and reports any work it
could not verify. For terminal fanouts with recoverable issues, the parent may
continue the unfulfilled task directly while its ordinary work budget remains;
once the final-answer window begins, it synthesizes the available evidence and
marks gaps. It must not retry the same delegation or force the user to restart
solely because a child could not be started.

Tool admission, provider budget and the model-visible deadline use the same
remaining wall-clock budget. A process-backed Edge action must fit its selected
command timeout, process cleanup/receipt time and a following final answer;
direct callbacks use their shorter receipt allowance. A typed completion
action is not exempt: it still needs a subsequent answer or Work settlement.
At the final-answer boundary, the runtime reports unfinished verification
truthfully instead of inviting a completion action that cannot be settled.
Time allowances are snapshots that shrink during model generation, and tool
admission remains authoritative. No extra database read is required to make
these process-local deadline decisions.

## Agent profile

For a run with admitted profiles, `agent.agent_type` and fanout slot/default
`agent_type` select an exact profile ID from the frozen run directory. That
directory replaces the builtin persona list; omitting the selector does not
implicitly choose a Team member. Tool schemas validate the selector's string
shape, while the shared child admission owns identity, delegation permission,
ancestry and depth checks before any child starts. This uses the already-loaded
snapshot, not a fresh catalog query.

An agent profile may specify:

```text
agent_id
role
system_contract_ref
skills
model_policy
provider_constraints
memory_scope
permission_scope
result_contract
```

## Delegation record

```text
delegation_id
parent_run_id
child_run_id
parent_task_id
objective
scope
expected_output
model_policy
provider_policy
status
summary
```

## Coordination patterns

| Pattern | Use |
| --- | --- |
| Sequential delegation | Specialist follows parent plan. |
| Fanout/fanin | Multiple branches explore alternatives. |
| Review delegation | Independent critique or security review. |
| Monitor agent | Watches long-running task or external condition. |
| Repair agent | Attempts recovery after structured failure. |

## Safety

Delegated agents do not inherit unlimited authority. Capabilities are bounded
by the delegation request, parent authorization, provider availability, and
runtime policy. A wildcard `read_only` delegation inherits the parent's tool
scope. `read_only` is workspace authority, not a second tool-name allowlist:
Edge tool visibility and dispatch both require the registered tool's read-only
execution capability. Shell execution is excluded even when a proposed command
appears observational. Discovery, coordination, and separately routed services
remain subject to their own provider/runtime admission and enabled-tool
constraints. Tool selection must disclose profile defaults and their execution
limits before a parent delegates work. When the parent
has an explicit enabled-capability set, a child allowlist may include registered
core tools or capabilities explicitly enabled by the parent; unknown names are
rejected before dispatch. Dynamic capabilities must be present in that explicit
parent set. Parents without an explicit enabled-capability set still use their
execution binding and authorization.

Before launch, preparation binds each slot's model selection to its parent,
Offering, reasoning, and slot identity. It grants no reusable inference
authorization; each inference revalidates current authorization. Invalid
preparation rejects the batch before any child starts. Server batching rules
and the catalog-read contract belong to
[orchestration model selection](orchestration.md#model-selection).

## Result integration

Fanout cancellation closes admission, but is not proof that an accepted child
has stopped. The existing cancellation event records the unassigned slot indexes
under the admission lock. Recovery settles only those explicitly unassigned
slots; a child absent from a recovery page remains unknown until exact durable
evidence arrives. If an accepted child is stopped before its run row exists,
the existing cancellation retry owner first waits for the exact aborted executor
to finish, retaining its execution-generation binding across retries. It then
commits one immutable parent-owned child terminal receipt. That exceptional
transaction takes the same session execution
fence as child creation, checks the exact child identity and row absence, and
only then settles local durability. Recovery seeks these receipts by child run
identity across pages; a missing page or row alone never settles a slot.
The same no-row fence is read-only for a child without a fanout slot.
Proven terminal state is published under the group lock before
eviction is possible; rejected recovery must not mutate a different owner.
After exact owner and group validation, a recovered cancellation closes the
execution-owned parent admission fence before optional projection admission;
projection capacity cannot reopen that parent. Spawn reservation rechecks the
same fence after asynchronous preparation, so either admission or closure wins.
Durable and workspace recovery apply each available group's child evidence as
one batch; a child already archived without group membership stays eligible for
later repair. Missing child pages never manufacture terminal slots.
Nonterminal durable rows prove acceptance, not that a remote executor stopped;
only terminal child evidence (a row or pre-durable receipt) settles a recovered slot.

Each executing parent (including outstanding tool calls) owns its fanout admission
fence and, after session-cache eviction, its complete terminal group receipt and
rendered result cache.
This preserves failed/unstarted slots and result addresses without reopening
completed groups. Parent ownership expires with execution; historical cancellations
must not permanently disable unrelated future parents. The session index is weak,
and the live projection and persistence backlog remain bounded. Subtree cancellation
uses a shared insertion fence but retains no historical run-ID table; closure
lives in the same execution-owned admission used by outstanding preparations. No additional
database operation is required for admission or reading an evicted receipt.
Result observation may use another turn in the same session while the group is
live or its owner receipt survives. Spawn/replay and stop actions still require
the exact parent run. Historical reads use the owner's cache and do not emit a
new parent collection event; a reused bare group ID is rejected when multiple
owners remain visible, and evicted receipts are not made permanent.
An explicit slot read may report all slots delivered only when it covers every
successful slot and returns the complete byte window. A suffix, truncated page,
or slot subset cannot make that claim. This does not change historical observation
mode, parent ownership, or pagination accounting.
When a later durable child state corrects a terminal result, it refines the same
parent-owned group receipt (live or evicted) and invalidates the rendered result;
ordinary executor callbacks cannot overturn that durable terminal truth. Auto-ID
result recovery and replay read the receipt through the same parent ownership.

Parent should receive:

- child summary;
- evidence refs;
- unresolved risks;
- tool/provider failures;
- confidence;
- recommended next action.

## Test obligations

- Child cancellation from parent.
- Parent cancellation with active children.
- Partial fanin.
- Deep delegation depth limit.
- Provider constraints inherited correctly.
- Child result survives parent resume.
