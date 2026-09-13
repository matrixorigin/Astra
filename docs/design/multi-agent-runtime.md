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
constraints and the child's actual provider/runtime admission.

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
