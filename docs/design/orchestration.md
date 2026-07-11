# Orchestration

> Status: target design contract.
> Last updated: 2026-07-07.

Orchestration owns multi-agent coordination, delegation, fanout/fanin, model choice per agent, and result integration. It does not own provider routing or lifecycle state machines.

## Principles

- Delegated agents share the same backbone semantics.
- Sub-agent execution must preserve parent trace and task lineage.
- Fanout should be explicit, bounded, and observable.
- Delegation failure should degrade the relevant branch, not corrupt the parent run.

## Delegation model

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

## Model selection

Per-agent model override is an orchestration decision, but it must still respect budget, policy, and trace requirements.

## Failure handling

- Child failure is recorded as branch failure.
- Parent may continue if aggregation policy allows partial results.
- Cancellation propagates according to delegation policy.
- Missing `action` or malformed delegation calls should produce targeted diagnostics and retry guidance.

## Test obligations

- Child run lineage survives resume.
- Parent cancellation handles children deterministically.
- Partial fanin is explicit.
- Tool/provider failures inside child runs preserve provider diagnostics.
