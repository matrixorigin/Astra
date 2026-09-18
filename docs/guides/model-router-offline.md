# Build and evaluate an offline model router

Stage 4 provides a local evidence importer and categorical candidate trainer.
It uses the existing `astra-test` harness binary and requires no running Server,
database, credentials, model calls, or external Python packages.

```bash
cargo build --locked -p astra-test-harness --bin astra-test
./target/debug/astra-test router-source-hashes --input approved-evidence.json
./target/debug/astra-test router-offline \
  --input approved-evidence.json \
  --authorization authorization.json \
  --output router-candidate-v1
```

The output directory must not already exist. The source-hashes command only
computes digests for review; it does not approve the data. Never automatically
turn arbitrary production exports into an authorization file.

## Prepare the evidence

The exact typed schema lives in
[`services::evaluation::router`](../../crates/services/src/evaluation/router/types.rs).
[`model_router_offline.json`](../../fixtures/contracts/model_router_offline.json)
is a synthetic one-example shape fixture, not evidence of model performance.
It is intentionally too small to qualify a threshold under default settings.

1. Select a permitted owner and fixed, evaluated economy/strong model profiles.
   Read routing decisions through the existing owner-scoped run/event boundary.
   Preserve the immutable event timestamp, input reference, policy revision,
   contract digest, and frozen features. Do not guess them from model names or
   terminal diagnostics. Records without features can be included for coverage,
   but cannot train the candidate.
2. Define a versioned task-acceptance rubric and attach reviewed human or
   executable verifier results to exact execution IDs. Keep unknown labels
   unknown. For the original execution, optionally attach next-turn observations
   only when their canonical response reference resolves exactly. Satisfaction
   remains separate from correctness; later-turn difficulty is never a feature
   for the earlier decision. Preserve the original run's full start time,
   including the auxiliary judge: the routing decision must fall between its
   start and completion. Replays must instead start at or after the original
   decision. All episodes must finish by dataset creation, and quality evidence
   must be timestamped at or after episode completion.
3. For paired experiments, start both candidates from identical permitted
   snapshots covering input, environment, tool fixtures and execution budgets.
   Use immutable fixtures or distinct isolated sandboxes under the existing
   harness. Supply actual, independent episode results. The importer does not
   invoke or provision replay environments. Recorded production tool results
   cannot answer a different tool request, and must not cause production writes.
   Mark non-replayable or failed experiments explicitly. Infrastructure failures
   are incomplete episodes; model-caused mistakes in completed episodes receive
   an unacceptable quality label and remain training evidence.
4. Join costs from all actual primary/auxiliary invocations, retries and failed
   attempts, with a pricing revision. Set `cost` to null or
   `covers_full_episode: false` if accounting is incomplete. The importer does
   not estimate missing prices or mistake provider success for task success.
5. Supply opaque related-task/duplicate/workspace/repository group keys and
   predeclare train/validation time boundaries and the outcome horizon. The
   builder also groups sessions and identical input prefixes. It rejects groups
   that cross time splits. Choose boundaries/cohorts accordingly; do not move
   cases after inspecting labels. This stage is per owner and does not perform
   automated semantic duplicate detection.
6. Review the entire source envelope for consent and redaction. Keep production
   datasets and approvals outside the repository. Run `router-source-hashes` and
   independently approve only the reviewed hashes.

An authorization file has this shape (replace every example value):

```json
{
  "dataset_id": "reviewed-dataset-v1",
  "owner_id": "opaque-owner",
  "target_use": "offline_model_routing",
  "redaction_version": "reviewed-structural-v1",
  "expires_at": "2026-12-01T00:00:00Z",
  "approved_sources": {
    "opaque-source": "sha256-returned-by-router-source-hashes"
  },
  "revoked_source_ids": []
}
```

The dataset must expire no later than its authorization. These are local
operator attestations; an authenticated consent service is not introduced by
this command. Changing an approved source invalidates its hash.

## Training and reports

Optionally pass `--config training.json`:

```json
{
  "minimum_training_groups": 20,
  "minimum_validation_groups": 20,
  "maximum_quality_regression": 0.01,
  "quality_thresholds": [0.7, 0.8, 0.9, 0.95, 1.0]
}
```

Set thresholds before reviewing outcomes. The categorical estimator fits only
training groups; validation chooses the threshold, and test groups are reserved
for the final report. No eligible validation improvement yields a candidate
that always abstains. A group representative is chosen before examining label
availability, so additional labeled rounds cannot silently replace an unlabeled
representative. The feature space is deliberately small; it does not yet model
language, domain, urgency or long-term task completion.

Outputs:

| File | Contents |
| --- | --- |
| `manifest.json` | Owner, revisions, profiles, split boundaries, horizon and expiry. |
| `examples.jsonl` | Allowlisted features, immutable source digests, lineage, separate quality/follow-up evidence and missingness. |
| `candidate.json` | Training statistics, validation-selected threshold, dataset hash and `offline_only` activation status. |
| `report.json` | Split coverage, common-cohort policy comparisons, acceptance, total cost, cost per acceptable task, latency, abstention and Brier diagnostic. |
| `complete.json` | Completion marker, dataset hash, expiry and all retained envelope/feedback/execution/verifier/snapshot source IDs. |

Require `complete.json` before using an output bundle. Serialization occurs in a
temporary directory; the completion marker is written after publication. A disk
failure can leave an incomplete directory without the marker. Existing outputs
are never overwritten. On Unix, the output directory is private to its owner
(mode 0700). Inputs are limited to 64 MiB each and 100,000 sources.

Policy cost per acceptable task includes spending on every case in the compared
complete-pair cohort, including unacceptable answers, divided by acceptable
answers. It is null when none passes. Incomplete prices, infrastructure failures
and missing quality labels are reported in coverage and excluded from this
comparison, not assigned zero cost or success. Acquisition cost for collecting
both experimental arms is separate from hypothetical selected-policy cost.
Latency covers the supplied full episode, including auxiliary calls if its
start timestamp is recorded correctly. The input producer owns accounting
completeness and replay isolation attestations.

No report qualifies a production rollout. Check missingness, cohort selection,
verifier calibration, worst-case task categories, and model/cost revisions
before drawing conclusions. The synthetic fixture only verifies mechanics.

## Revocation and deletion

Maintain the current authorization separately from artifacts. Remove an
envelope's `approved_sources` entry to withdraw approval for that envelope. To
withdraw evidence that can be referenced by several envelopes, add its ID to
`revoked_source_ids`: this includes follow-up source IDs, observed/replayed
execution IDs, verifier `evidence_ids`, and replay `snapshot_root` values.
Explicit revocation overrides approval of every containing envelope; removing
another envelope's approval alone does not revoke evidence shared with it.
Every build/train and dataset revalidation checks these dependencies, including
those in examples excluded from training.

`complete.json.source_ids` is the sorted, deduplicated union of all retained
envelope and dependency IDs. Use it to invalidate/delete every derived dataset
and candidate containing withdrawn evidence. Existing bundles produced before
this lineage fix list only envelope IDs and must be rebuilt before relying on
their marker for dependency invalidation. Rebuild from the remaining approved
evidence under a new dataset version; do not reuse a prior candidate as still qualified.
Local artifacts are not a remote managed store: the operator owns deletion of
already exported files. No daemon or automatic production activation consumes
them in this stage. Future activation must revalidate current consent and lineage
through the tuning-job lifecycle.
