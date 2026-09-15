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

## Comparable benchmark runs

The `astra-test` harness persists a typed manifest with each JSON suite
report. It records the case digest, effective model matrix, effective working
directory, profile and repeat/concurrency settings, full capture and judger
configuration, executor kind, and the embedded build identity of the process
that actually ran the cases. External command text is represented only by a
digest; the manifest does not contain credentials or command stderr.

The report also contains one canonical aggregate keyed by `(case, model)`.
Planned, executed, passed, failed, cancelled, unavailable, and incomplete
evidence rows remain separate. Token, duration, and provider-round samples
are retained as p50/p95 summaries for all executed rows, complete successful
rows, and incomplete successful rows. Only the complete-success bucket is
eligible for efficiency scoring. Execution attribution (including rejected,
reused, suppressed, and deferred calls) is counted only when durable evidence
is present and only for rows the runner says actually executed.

`astra-test --baseline <report.json>` compares two manifests before producing
quality or efficiency deltas. A case/model/configuration mismatch is
`incomparable`. Efficiency is considered only with at least three successful,
complete observations in each report and only when the quality rate has not
decreased; an earlier failure, cancellation, or unavailable row therefore
cannot masquerade as a cheaper run. Missing case-executor identity makes
performance comparison unavailable while keeping the current product result
visible.

## Relationship to learning

Evaluation produces labels and quality signals. It is not itself a training pipeline. Learning artifacts require the additional consent/redaction/lineage rules in [evaluation-and-learning.md](evaluation-and-learning.md).
