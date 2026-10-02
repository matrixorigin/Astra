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
[`services::model_routing::offline`](../../crates/services/src/model_routing/offline/types.rs).
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

The `deterministic_auto` comparison uses the immutable recorded Auto selection,
including economy-unavailable and incompatible-candidate fallbacks, to select an
arm from the same paired replay cohort. It reports replay outcomes for that
choice; it does not reconstruct the online choice from feature eligibility alone.

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

## Qualify a candidate for offline shadow scoring

Collect the immutable test routing decisions, then seal their outcome-free roster
and evaluation protocol **before starting any held-out replay or collecting its
outcomes**. Set `registered_at` to the actual seal time, after all source decisions;
it may follow the test decision split (`validation_before`). Keep the protocol in
your review system; the local command checks the supplied timeline but cannot
authenticate the seal or prove omitted outcomes were unseen. Protocol changes or
repeated candidate selection need a fresh holdout. The fitting configuration and
threshold choices are pinned too:

```bash
astra-test router-config-hash --config training-config.json
astra-test router-plan-hash --input planned-evidence.json
```

For default training settings, omit `--config` on every command. Put the returned
configuration and plan hashes in a reviewed `protocol.json`. The plan input uses
the same manifest and source roster as the eventual evidence bundle; replay,
quality, cost and feedback fields may be absent. The digest binds the full
manifest (including split dates, outcome cutoff/horizon and model/rubric scope),
source membership, grouping, canonical input references, frozen features and the
recorded selected Offering/contract used by the Auto baseline.
Reordering sources or group keys does not change it. Complete evidence still
needs independent source authorization when outcomes arrive. Changing the plan
requires a new reviewed protocol and fresh holdout; do not move the split or drop
incomplete cases under the old protocol. The hash command itself grants no access.

For example, mature training/validation labels before March 1, collect test
decisions on March 1, seal the roster on March 2, and start both replay arms on
March 3. Use March 2 as `registered_at`. Plan the final evidence snapshot's
`created_at` for March 5 so the example's one-day replay horizons have matured.
At sealing, omit held-out paired/observed outcomes and feedback; add replay
evidence later under renewed source authorization without changing the plan.
Every supplied test replay must start strictly after the seal, including failed,
incomplete and grouped-out pairs. Supplied test observed completions and follow-up
feedback must also be strictly later. Backdated seals before source decisions
and seals at or after replay start are rejected.

```json
{
  "schema_version": 1,
  "job_id": "router-qualification-001",
  "owner_id": "example-owner",
  "dataset_id": "example-dataset",
  "registered_at": "2026-01-01T00:00:00Z",
  "training_config_sha256": "REPLACE_WITH_ROUTER_CONFIG_HASH",
  "evaluation_plan_sha256": "REPLACE_WITH_ROUTER_PLAN_HASH",
  "minimum_test_groups": 1000,
  "minimum_stratum_groups": 1000,
  "minimum_pair_coverage": 0.95,
  "maximum_quality_regression": 0.01,
  "minimum_cost_saving_fraction": 0.2,
  "maximum_episode_cost_usd": 1.0,
  "maximum_p95_latency_ratio": 1.1,
  "confidence": 0.95,
  "required_strata": [{
    "schema_version": 1,
    "assessment_present": true,
    "difficulty": "moderate",
    "difficulty_confidence": "high",
    "read_only_primary": true,
    "supported_input": true
  }]
}
```

These are example product criteria, not calibrated defaults or a sufficient
sample-size claim. The conservative confidence bounds can require far more than
the specified sample floor, especially for a 1% quality margin. Declare all target
structural feature categories. Missing or unexpected categories anywhere in the
test evidence reject the gate, including sources excluded by group selection.
The episode cost bound must be defensible for your population before observing
test spending; any known held-out replay cost exceeding it fails the gate,
including costs from incomplete pairs, failed episodes, partial prices, and
examples excluded by grouping. Unknown prices remain unknown. Never remove expensive or failed
examples just to pass. The repository's tiny synthetic fixture will be rejected.

```bash
astra-test router-qualify \
  --input reviewed-evidence.json \
  --authorization authorization.json \
  --config training-config.json \
  --protocol protocol.json \
  --output qualification-001
```

`qualification.json` includes the candidate, protocol, tuning record and overall/
stratum metrics, bounds and rejection reasons. `complete.json` pins its digest,
expiry and all evidence dependencies. Successful command execution means the
report was produced, **not** that the gate passed: inspect `tuning.status` for
`rejected` or `ready_for_shadow`. Invalid authorization or malformed protocols fail
before publication. A changed evaluation plan also fails before publication;
protocols missing `evaluation_plan_sha256` are rejected. Existing output
directories are refused.

The gate requires quality non-inferiority and cost improvement against both
always-strong and deterministic Auto overall. Every required category must meet
its quality, coverage, support and p95 latency limits; unchanged selections in
one category need not save money themselves. Both model episodes are priced in
full. The original training/validation labels must have matured before the test
period, including labels obtained by replay.

## Score later traces without changing model selection

Prepare a separate evidence bundle and authorization with the same owner, model
contracts, policy revision and rubric. Use fresh sessions/tasks/input prefixes;
all decisions must follow the qualification dataset's creation timestamp. Obtain
source approvals using the existing `router-source-hashes` workflow. Outcomes and
paired replays may be absent; original decision horizons must still have matured.

```bash
astra-test router-shadow \
  --input reviewed-evidence.json \
  --authorization authorization.json \
  --config training-config.json \
  --protocol protocol.json \
  --shadow-input later-reviewed-evidence.json \
  --shadow-authorization later-authorization.json \
  --output shadow-001
```

The command rebuilds qualification under current authorization instead of trusting
a saved approval. It rejects a failed gate, changed model/scope, overlapping tasks,
old decisions or revoked evidence. A revocation in either authorization also
invalidates dependencies in the other dataset. `shadow.json` records historical
and proposed profiles, abstentions, disagreements and local scorer timings;
`complete.json` contains the union of training and shadow lineage and the earliest
expiry. Use this union when invalidating/deleting derived bundles. Out-of-scope
features abstain to strong. No provider requests or model changes occur.

This is **offline shadow scoring** of later recorded decisions. It measures neither
online routing overhead nor the unchosen model's success. There is no live canary,
activation registry, kill switch or rollback implementation in this stage. All
artifacts remain offline-only and production qualification remains false.
