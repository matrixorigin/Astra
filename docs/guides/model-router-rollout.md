# Model-router live shadow and canary

Stage 6 provides an authenticated operator API for a qualified, per-owner learned
router. It is disabled until an admin publishes evidence. The existing Server
`[model_routing]` pair must already be configured, and clients must opt into Auto.
Explicit model choices keep their existing behavior.

The deployment registry requires fresh schema contract `2026-09-27-v90`. Astra's
existing bootstrap policy rejects older schema markers; there is no ALTER-based
migration. Provision a fresh database according to the repository deployment
procedure. Do not point test/bootstrap commands at an existing production database.

All routes below use the normal admin bearer token and API base URL. Evidence is
uploaded as JSON; the Server never reads paths on an operator's computer. Keep
reviewed datasets, grants and requests outside the repository. The ordinary HTTP
body limit still applies; oversized corpora need a deliberately configured limit.

## Publish a reviewed candidate in shadow mode

Use the evidence, authorization, training config and protocol from
[offline qualification](model-router-offline.md). Build a request with these fields:

```json
{
  "expected_revision": 0,
  "input": "REPLACE with the evidence JSON object",
  "authorization": "REPLACE with the current authorization JSON object",
  "config": "REPLACE with the training-config JSON object",
  "protocol": "REPLACE with the registered protocol JSON object",
  "review": {
    "online_consent_reference": "reviewed-online-consent-001",
    "verifier_review_reference": "verifier-calibration-001",
    "safety_review_reference": "critical-task-review-001",
    "expires_at": "2026-12-01T00:00:00Z",
    "minimum_shadow_sessions": 1000,
    "maximum_routing_overhead_ms": 100
  }
}
```

The four placeholder fields must be JSON objects, not filenames or strings. Use
sample counts and overhead limits justified for your workload; the values above
are examples. Review references are opaque identifiers, not free-text explanations,
private URLs or credentials. They attest separate online consent, verifier and
critical-task reviews. Offline authorization alone does not imply online consent.

`POST /admin/model-router/{owner}` recomputes qualification, checks owner, expiry,
current Offering identities/contracts, and atomically creates a shadow deployment.
The response includes the new revision and deployment/candidate identity. A failed
gate publishes nothing. Replacing an unexpired deployment requires rollback first;
a replacement gets a new assignment salt and needs fresh shadow observations.

Live Auto requests now score the candidate without changing selection or calling
another provider. `GET /admin/model-router/{owner}` returns the operational
dashboard. Check abstentions, admission failures, feature scope and p95 routing
overhead. The dashboard exposes missing outcome coverage and bounded-scan
truncation; neither counts as success. Overhead covers registry/scoring/admission,
while full-episode latency and cost must also include the earlier auxiliary judge.

## Begin a limited canary

After reviewing shadow behavior, send:

```text
POST /admin/model-router/{owner}/canary
{"expected_revision": 1, "basis_points": 100}
```

Use the actual revision from the previous response/dashboard. The server enforces
shadow support, zero recorded admission/critical failures, and the reviewed p95
routing-overhead budget. Allowed fractions are 1–1000 basis points (0.01%–10%).
The fraction is fixed; increasing exposure requires a new reviewed deployment.
Sessions are assigned consistently to control or treatment. Control retains
stage-3 Auto, including its ordinary economy use; it is not an always-strong arm.
The durable decision records the cohort probability, not a model-action propensity.

Treatment stays within primary, read-only compatible execution. Unsupported
features abstain to strong. Child execution from treatment is currently rejected.
Every primary authorization and recovery rechecks deployment validity and current
Offering access. A rollback stops pinned treatment rather than rerouting an
in-progress task or replaying tools. Already authorized/in-flight work is not
recalled. No production deployment is enabled merely by running these tests.

## Record and compare reviewed outcomes

For a completed, failed or cancelled routed run, send:

```text
POST /admin/model-router/{owner}/outcomes/{run_id}
```

```json
{
  "rubric_version": "SAME_RUBRIC_AS_QUALIFICATION",
  "evidence_reference": "reviewed-verifier-result-001",
  "acceptable": null,
  "corrected": null,
  "full_episode_cost_usd": null,
  "episode_latency_ms": null,
  "critical_violation": false
}
```

Replace unknowns only with reviewed evidence. Transport success or user silence
is not correctness; anger is not necessarily a model error. Complete episode cost
includes unsuccessful attempts, primary/auxiliary calls and user-visible task
rework within the defined episode. The API records operator attestations and does
not compute prices from missing usage or independently certify a verifier.
It binds evidence to the owner, run, deployment and rubric. Repeating the same
outcome succeeds; changing an immutable outcome conflicts. Critical violations
stop the deployment before the report is acknowledged. This stop checks the
deployment identity under the database lock, so concurrent revision updates
cannot suppress it and an older report cannot stop a replacement deployment.

The dashboard uses the first admitted run per session in each shadow/canary epoch,
selected without inspecting its outcome. It shows quality, corrections, price and
latency coverage separately. Cost per acceptable task requires full coverage and
includes spending on failures. These comparisons are descriptive. Predeclare an
online analysis horizon, sample-size/power calculation, quality margin, cost and
correction/latency limits before collecting results; the API does not claim
statistical significance or automatically promote a successful-looking canary.
Operational admission failures, critical violations and routing overhead include
every observed run, including later turns in a session.

## Stop or revoke

```text
POST /admin/model-router/{owner}/rollback
{"expected_revision": 2, "reason": "quality_regression"}

POST /admin/model-router/{owner}/revoke
{"expected_revision": 3, "source_ids": ["withdrawn-source-001"]}
```

Rollback also serves as the kill switch and online-consent withdrawal. New turns
return to deterministic Auto. Revocations cover all lineage IDs from the offline
artifact, including referenced executions, verifier evidence and snapshots. They
persist across deployments and prevent reusing the withdrawn source. Independently
delete already exported offline artifacts according to their lineage.

Updates use compare-and-swap revisions. On HTTP 409, fetch current status and
review the newer state before retrying. Each successful change shares a database
transaction with its authenticated admin audit entry, available through the
existing admin audit API. Storage failures and unavailable pinned treatment state
fail closed. Neither editing an offline report nor restarting a host reactivates a
rolled-back deployment.

Routing-budget failures retain their cohort, measured overhead and explicit failure
reason in the canonical routing decision before the run fails. They remain in
the dashboard and can receive reviewed outcomes; recovery cannot dispatch them.
The decision also pins the review rubric. Delayed outcomes and identical retries
remain valid after deployment replacement, using the original run's deployment
and rubric. A historical critical report never stops a replacement deployment.
