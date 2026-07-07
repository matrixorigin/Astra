# Learning pipeline

> Status: target design contract.
> Last updated: 2026-07-07.

Learning pipeline defines how Astra converts approved runtime evidence into durable learning artifacts. Evaluation, feedback, and tuning are upstream systems; learning owns consent, redaction, quality, lineage, dataset construction, and deletion propagation.

## Ownership

This document owns:

- C5 learning artifact semantics;
- consent and privacy boundary;
- redaction and quality gate requirements;
- dataset lineage;
- deletion propagation;
- activation boundary between data and behavior.

It does not own:

- evaluation case execution, owned by [evaluation.md](evaluation.md);
- feedback collection and classification, owned by [feedback-control-loop.md](feedback-control-loop.md);
- candidate improvement lifecycle, owned by [tuning-jobs.md](tuning-jobs.md);
- raw debug bundles, owned by [observation-plane.md](observation-plane.md).

## Principle

```text
Runtime traces are evidence. They become learning data only after consent, redaction, quality, and lineage gates.
```

## Allowed sources

Allowed by default when policy permits:

- explicit user feedback;
- redacted transcript excerpts;
- C2 audit facts;
- C3 trace facts;
- eval labels;
- tool-result quality annotations.

Opt-in only:

- raw debug bundles;
- private workspace snippets;
- sensitive external API results;
- raw tool output containing user data;
- personally identifying or regulated data.

## Learning artifact

A learning artifact should record:

```text
artifact_id
source_refs
consent_scope
redaction_version
quality_score
dataset_id
target_use
lineage
created_at
expires_at
```

## Dataset lifecycle

```text
candidate -> redacted -> quality_checked -> approved -> active -> deprecated -> deleted
```

A dataset must be invalidated or rebuilt when source consent is revoked or source data is deleted.

## Quality gate

Learning data must be filtered for:

- task relevance;
- correctness or label confidence;
- safety compliance;
- duplication;
- provider/tool failure contamination;
- private data leakage;
- prompt injection contamination.

## Activation boundary

Datasets do not change runtime behavior by themselves. Behavior changes go through tuning jobs, evaluation gates, and rollout/rollback policy.
