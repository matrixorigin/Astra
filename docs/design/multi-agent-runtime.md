# Multi-agent runtime

> Status: target design contract.
> Last updated: 2026-07-07.

The multi-agent runtime defines how multiple agents cooperate under the same backbone without creating independent untraceable execution islands.

## Principles

- Every child agent run is a durable run with lineage.
- Parent and child share observation and audit semantics.
- Delegation has explicit objective, scope, provider constraints, and result contract.
- Parallelism is bounded and observable.
- Child failure should be isolated unless parent policy requires fail-fast.

Model-authored child sizing (`complexity` and `initial_turns`) selects only an
initial, renewable execution slice. It is not a user-owned hard limit: a parent
model cannot stop a child after an arbitrary number of rounds. Explicit
request/runtime limits and inherited wall-clock deadlines remain authoritative.
The child must retain a path from a successful action to a truthful terminal
answer while it is still making progress.

## Foreground deadline ownership

A foreground child must not consume the parent's final-answer reserve. The
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
runtime policy. In a wildcard `read_only` delegation, `read_only` blocks
workspace mutation; it does not by itself block network reads whose canonical
tool effects declare no workspace writes, credentials, process spawning, or
external mutation. Those tools remain subject to the parent's enabled-tool
constraints and the child's actual provider/runtime admission. When the parent
has an explicit enabled-capability set, a child allowlist may include registered
core tools or capabilities explicitly enabled by the parent; unknown names are
rejected before dispatch. Dynamic capabilities must be present in that explicit
parent set. A legacy unrestricted parent context retains its existing behavior.

## Result integration

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
