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

Semantic judges receive ordered user-request/assistant-response pairs, including
root retry attempts and executed follow-up steps. The evidence stays bounded,
with explicit content or exchange omission markers; deterministic criteria keep
their original aggregate. Missing evidence cannot authorize a passing judgment.

Tool-result identity comes from invocation scope, arguments, outcome, complete
result and typed execution facts. A verified artifact supplies the complete body;
a bounded error preview is presentation, and cannot contradict an identical
complete failure body. Error-only records remain failure evidence, and conflicting
bodies, artifacts, dispositions or typed failure facts are rejected.

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

Execution evidence belongs to the shared turn evaluator and runtime journal.
Reproducible experiments use the test harness; consent-gated routing datasets
belong to `services::model_routing::offline`. These contracts do not require a
separate analytics API or persistence service. Activation belongs to the tuning
owner.

Runtime tool-boundary feedback and terminal evaluation use thresholds resolved
once from the admitted execution configuration. Cooperative handoff carries
those thresholds with the original execution facts; later configuration edits
do not reinterpret an in-flight run or replenish its recovery permissions.
Tool batches perform no configuration file I/O.

The Server terminal owner evaluates the complete settled tool and child evidence
once, before committing `run_finished`. Its `turn_evaluation` carries the
original journal event, session/run identity, execution generation, frozen
thresholds and final status. Live delivery and replay project the same fact.
CLI journals and feedback consume it without re-evaluating partial stream
records or loading local thresholds. Missing evaluation remains unknown;
conflicting or incorrectly scoped evaluation is a protocol error.

The CLI sidecar journal adds only its existing projection ID/index for retry
reconciliation. The Server timestamp, canonical turn, producer scope and
evaluation metadata remain unchanged; projection retries never re-evaluate.
