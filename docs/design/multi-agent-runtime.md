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

For a Server fanout with explicit child models, the runtime validates every
slot before launching any child, admits the distinct non-inherited Offerings as
one bounded user-scoped batch, and binds each admitted execution to its slot.
An inherited parent Offering reuses the parent's admission without another
catalog read. Any invalid or revoked slot fails preparation for the entire
fanout; admission does not authorize a partial launch.
CLI fanout uses the same Server-owned check through one `/model-access/admit`
request when slots explicitly select a model or reasoning control. The response
contains only display name and context-window metadata; it is not a reusable
authorization token, and inference still revalidates the Offering. An
inherited-only CLI batch keeps its existing single catalog lookup. A mixed
batch with inherited slots requires the parent's exact Offering identity;
without it, preparation fails before remote I/O instead of guessing from a
display name.

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
