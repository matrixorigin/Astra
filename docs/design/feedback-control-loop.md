# Feedback control loop

> Status: target design contract.
> Last updated: 2026-07-07.

The feedback control loop turns user feedback, trace facts, eval results, and operational signals into controlled product improvement. It is the upstream system for tuning jobs and evaluation.

## Principles

- Feedback is evidence, not automatic truth.
- Negative feedback must preserve context and provider decisions.
- Implicit feedback requires conservative interpretation.
- Improvement proposals must pass evaluation before activation.
- Feedback must respect privacy, consent, and deletion.

## Feedback sources

| Source | Examples |
| --- | --- |
| Explicit user feedback | thumbs up/down, correction, comment, bug report. |
| Behavioral feedback | user undoes change, retries prompt, abandons run. |
| Tool feedback | malformed call, repeated failure, fallback, timeout. |
| Evaluation feedback | regression failure, rubric score, human label. |
| Operational feedback | latency, token waste, sync degraded, provider offline. |

## Feedback record

A feedback record should include:

```text
feedback_id
user_id/session_id/run_id/turn_id
source
sentiment_or_score
target_type
target_id
context_refs
provider_decision_refs
privacy_scope
created_at
```

## Interpretation

Feedback interpretation should classify:

- prompt issue;
- tool/capability issue;
- provider routing issue;
- memory/context issue;
- model choice issue;
- product UX issue;
- user/environment issue;
- unsafe or policy issue.

## Control loop

```text
collect -> classify -> aggregate -> propose -> evaluate -> approve -> activate -> monitor
```

Activation is handled through tuning jobs or explicit product changes, not directly from raw feedback.

## Guardrails

- Do not overfit to a single complaint without evidence.
- Do not use sensitive raw data without consent.
- Do not treat low-confidence implicit feedback as a hard label.
- Do not activate a change without rollback.
