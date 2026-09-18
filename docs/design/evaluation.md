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

## Generic comparison contract

Evaluation is a reusable controlled-comparison capability. A prompt, Skill,
tool policy, model/provider binding, memory policy, or workflow is only a
comparison input; none of them owns trial identity, assessment, or report
semantics. Historical quality-tracker data is observational context and cannot
stand in for a paired evaluation.

The initial planned isolation profile is `prompt_only_private`: the task input
and declared read-only resources are frozen, external side effects are
rejected, and production ranking, reflection, and learning writes are
disabled. When a task needs memory, each trial starts from the same Memoria
base snapshot and writes to its own memory branch; a branch is never merged by
evaluation implicitly. When a task needs structured data writes, each trial
uses a MatrixOne data snapshot/branch from the same base. These branches are
part of the composite snapshot for the trial; they do not isolate model
context, files, provider cache, or external APIs by themselves. Branch
creation or snapshot drift makes that trial unavailable rather than silently
changing its context. A Skillify flow supplies candidate Skill revisions to
this profile, but the same evaluation contract must work without a Skill
identity.

The baseline is the normal configuration or an immutable prior revision. The
candidate is loaded by its exact content hash. The object under comparison is
recorded in the experiment specification; it is not inferred later from tool
frequency or from a report filename.

Before dispatch, the specification freezes the cases, rubric/verifier
versions, model/provider configuration, fixed companion skills, repetition
and ordering plan, total budget, and composite snapshot references. The
scheduler persists a trial identity before creating a run. Run creation and
settlement are idempotent; an unknown provider outcome remains an uncertain
attempt rather than being silently retried as a new success.

The current contract records and validates these requirements; it does not yet
create branches or enforce access at execution time. The durable executor must
fail closed when a required branch, snapshot, or isolation receipt is missing.
`Disabled` means the corresponding memory or data capability is prohibited for
the trial, not that it may be used without isolation.

Each trial input is addressed by a snapshot envelope. The envelope has a
UUIDv7 `snapshot_id` for lookup and idempotency, wraps the existing composite
snapshot references, and stores a canonical `snapshot_fingerprint` over its
owner, experiment/trial scope, context hash, policy hash, and component refs.
The UUID and the creation timestamp are metadata; the fingerprint and the
component references are the evidence of what was frozen. A materialization
receipt must still prove that an owner/trial-scoped Memory or MatrixOne branch
was created. A complete-looking envelope without that receipt is unavailable,
not an isolated execution.

The report must preserve all samples, including failures, cancellations,
timeouts, unavailable infrastructure, and missing measurements. It may state
that evidence is insufficient, but must not infer equivalence or causal credit
from a small successful-only sample. A report links each conclusion to the
trial facts that support it and distinguishes observed correlation from a
controlled version effect. The report renderer is separate from execution, so
disconnects or rendering failures do not lose completed trials. Missing usage
remains missing, and a partial or unavailable trial is never converted to zero
or silently excluded. The report distinguishes paired comparison support from
mechanism evidence; a trace being available does not by itself prove a causal
effect.
