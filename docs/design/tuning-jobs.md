# Tuning jobs

> Status: target design contract.
> Last updated: 2026-07-07.

Tuning jobs are controlled improvement workflows for prompts, skills, routing, memory loading, and provider policy. They are not ad hoc background scripts and not raw training jobs over production traces.

## Goals

- Turn feedback and evaluation results into safe candidate improvements.
- Keep every candidate reproducible and attributable.
- Gate activation through offline and online evaluation.
- Support rollback and comparison.
- Preserve consent, redaction, lineage, and deletion propagation.

## Non-goals

- Do not train on raw debug bundles by default.
- Do not auto-activate changes without regression gates.
- Do not treat tuning output as globally valid across users, workspaces, or policies.
- Do not bypass provider, safety, or permission contracts.

## Job types

| Job type | Target |
| --- | --- |
| Prompt tuning | System/developer prompt sections, examples, and policy wording. |
| Skill tuning | Skill instructions, invocation heuristics, schemas, and examples. |
| Tool routing tuning | Provider priority, fallback policy, deferred-tool search, and admission diagnostics. |
| Memory loading tuning | Retrieval filters, ranking, compaction, and conflict handling. |
| Model routing tuning | Model selection, escalation, and cost/quality thresholds. |
| Evaluation tuning | Test set generation, labels, rubrics, and scoring thresholds. |

## Lifecycle

```text
proposed -> collecting_data -> building_candidate -> evaluating -> approved -> active
proposed -> rejected
building_candidate -> failed
evaluating -> rejected
active -> rolled_back
```

A tuning job should not mutate active behavior until it reaches `approved` and activation policy allows it.

## Inputs

Allowed by default:

- explicit user feedback;
- redacted transcript excerpts;
- C2 audit facts;
- C3 trace facts;
- eval labels and regression failures;
- tool-result quality annotations;
- provider fallback/degraded metrics.

Opt-in only:

- raw debug bundle data;
- private workspace snippets;
- sensitive external API responses;
- raw tool output containing user data.

## Candidate artifact

Every candidate must record:

```text
candidate_id
job_id
target_domain
target_version_base
change_summary
input_dataset_id
redaction_policy
quality_gate
created_by
created_at
rollback_target
```

## Evaluation gates

Activation requires:

- replay on relevant historical/eval cases;
- regression tests for known failure modes;
- safety checks for policy and provider boundaries;
- cost/token impact estimate;
- failure-mode review;
- rollback plan.

Online rollout should support staged activation and fast rollback.

## Lineage and deletion

Tuning data lineage must be explicit:

- source event ids;
- redaction version;
- consent scope;
- dataset version;
- candidate id;
- activated version.

If source data is deleted or consent is revoked, derived datasets and candidates must be invalidated or rebuilt according to policy.

## Observability

Track:

- candidate success rate;
- regression failure rate;
- activation/rollback count;
- token and latency deltas;
- provider fallback changes;
- tool-call validity;
- user correction rate after activation.
