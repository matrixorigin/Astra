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

## Implemented local model-router qualification

`astra_services::tuning` owns the versioned router qualification protocol and
review record. `astra-test router-qualify` rebuilds authorized sources and produces
an immutable record ending in `rejected` or `ready_for_shadow`; `router-shadow`
rechecks qualification and current source authorization before observational
scoring. Records pin the dataset, candidate, protocol, expiry and deletion lineage.
They are local review artifacts, not authenticated approvals or rows in an active
policy registry. The production lifecycle above remains a target contract:
passing this offline gate cannot create an `approved` or `active` policy.

See [model routing](model-routing.md#qualification-and-offline-shadow-scoring-stage-5)
for gate semantics and [the operator workflow](../guides/model-router-offline.md)
for invocation and interpretation.

Router protocols require an outcome-free evaluation-plan digest, computed by the
services tuning owner and exposed as `astra-test router-plan-hash`. It seals the
manifest, source population, grouping and decision features. Changed splits or
sample membership cannot reuse a registered protocol even if the source grants
remain valid. Later outcomes require source approval but do not change the plan.
The registration timestamp records the actual roster seal after source decisions
and before held-out replay starts or supplied outcomes arrive. The decision split
remains a separate cutoff: fitting labels must mature before test decisions.

## Implemented online router rollout

Stage 6 extends this owner with a durable per-owner deployment registry and admin
API. It recomputes offline qualification and records separate online review/consent
attestations before live shadow, then gates a capped canary on observed shadow
support/admission/overhead. Atomic revisions and audit records own publication,
canary, rollback and persistent lineage revocation. Run decisions and reviewed
outcomes remain in the existing run-event ledger. There is no parallel trace store
or automatic unrestricted production activation. See the
[rollout contract](model-routing.md#controlled-online-rollout-stage-6).
