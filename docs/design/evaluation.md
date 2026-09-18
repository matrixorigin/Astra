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

## Skill comparison contract

Skill evaluation is a controlled comparison, not a quality-tracker summary.
The first supported profile is `prompt_only_private`: the task input and
declared read-only resources are frozen, external side effects are rejected,
and production memory, ranking, reflection, and learning writes are disabled.
The baseline is either the normal no-target-skill path or an immutable prior
skill revision. The candidate is loaded by its exact content hash.

Before dispatch, the specification freezes the cases, rubric/verifier
versions, model/provider configuration, fixed companion skills, repetition
and ordering plan, and total budget. The scheduler persists a trial identity
before creating a run. Run creation and settlement are idempotent; an unknown
provider outcome remains an uncertain attempt rather than being silently
retried as a new success.

The report must preserve all samples, including failures, cancellations,
timeouts, unavailable infrastructure, and missing measurements. It may state
that evidence is insufficient, but must not infer equivalence or causal credit
from a small successful-only sample. A report links each conclusion to the
trial facts that support it and distinguishes observed correlation from a
controlled version effect. The report renderer is separate from execution, so
disconnects or rendering failures do not lose completed trials.
