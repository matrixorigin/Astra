# Evaluation

> Status: target design contract.
> Last updated: 2026-07-07.

Evaluation defines how Astra measures agent quality, safety, reliability, and regression risk across prompts, tools, providers, memory, and orchestration.

## Goals

- Make changes testable before activation.
- Evaluate complete agent behavior, not only final text.
- Include tool correctness, provider routing, context quality, and safety.
- Support replay with versioned inputs and clear non-replayable dependencies.
- Feed tuning jobs with trustworthy labels.

## Evaluation dimensions

| Dimension | Measures |
| --- | --- |
| Task success | Did the agent solve the user objective. |
| Tool validity | Were tool calls valid, necessary, and well-routed. |
| Context quality | Was the right memory/artifact/provider state included. |
| Safety | Were policies, permissions, and side-effect boundaries respected. |
| Robustness | Did the system handle failures and degraded providers. |
| Efficiency | Token cost, latency, retry waste, tool fanout waste. |
| User experience | Clear status, recoverability, useful diagnostics. |

## Case structure

```text
case_id
objective
input_transcript
context_snapshot_refs
provider_bindings
expected_behavior
forbidden_behavior
rubric
fixtures
privacy_scope
```

## Replay modes

| Mode | Meaning |
| --- | --- |
| Exact replay | Same context, tool facts, model config, no external live calls. |
| Simulated provider replay | External providers replaced by fixtures. |
| Live integration eval | Calls real providers under controlled policy. |
| Human review | Human judges output, trace, or behavior. |

## Regression gates

A change should not activate if it causes material regression in:

- safety;
- data loss risk;
- tool-call validity;
- provider fallback correctness;
- task success on critical workflows;
- cost/latency beyond policy budget.

## Relationship to learning

Evaluation produces labels and quality signals. It is not itself a training pipeline. Learning artifacts require the additional consent/redaction/lineage rules in [evaluation-and-learning.md](evaluation-and-learning.md).
