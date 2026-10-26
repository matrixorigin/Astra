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

For live multi-agent capability tests, execution identity and original result
custody are deterministic prerequisites; natural-language answer correctness is
a separate required semantic judgment. An omitted child `expected_result` must
be paired with a required judge. Both built-in and external judges receive the
same complete, verified child bodies and identities, separately from parent
claims. Missing, oversized, or contradictory evidence cannot authorize a pass.
The built-in judge streams its complete JSON request through CLI stdin, rather
than imposing an OS argument-size budget on valid child evidence. The existing
Server request and Offering admission boundaries still apply; rejection or
missing evidence never becomes a passing verdict. The external-command protocol
retains its own 64 KiB serialized-envelope limit.
User-visible response requirements apply to the assistant response in its labelled
exchange, while user prompts and tool/diagnostic records establish context and
execution facts. They must not be attributed to the assistant's answer.
Explicit text and JSON expectations remain available for actual output-format
contracts; they must not be imposed accidentally on general collaboration tests.

Public child-adoption diagnostics use closed reason codes from the canonical
custody verifier, distinguishing a failed condition from unavailable evidence.
Neither category authorizes semantic judging without verified child evidence.
Public progress and CI summaries omit raw results, identities, arguments and
provider error text; diagnostic changes preserve grading and failure classification.

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

The Server may evaluate the complete settled tool and child evidence once.
`turn_evaluation` is optional diagnostic evidence; canonical Run completion and
cold replay do not depend on persisting it. A live evaluation can therefore be
absent from cold replay. When supplied, it carries the original journal event,
session/run identity, execution generation, frozen thresholds and final status.
Missing evaluation remains unknown; conflicting or incorrectly scoped evidence
is a protocol error. CLI journals and feedback do not manufacture an evaluation
from partial stream records or reload local thresholds to fill that gap.

The CLI sidecar journal adds only its existing projection ID/index for retry
reconciliation. The Server timestamp, canonical turn, producer scope and
evaluation metadata remain unchanged; projection retries never re-evaluate.

Child-result adoption checks use the canonical Run tree for completion, model
identity and parentage, terminal transcript content for the actual result, and
parent-owned adoption facts for content digests, complete result windows and
adoption-before-finalization ordering. Provider request diagnostics are optional
observations, not an additional execution authority or prerequisite for adoption
verification. Missing canonical result or adoption evidence still fails closed.
