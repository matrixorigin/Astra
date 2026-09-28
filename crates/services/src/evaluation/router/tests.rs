use super::*;
fn fixture() -> RouterDatasetInput {
    serde_json::from_str(include_str!(
        "../../../../../fixtures/contracts/model_router_offline.json"
    ))
    .unwrap()
}
fn auth(input: &RouterDatasetInput) -> RouterDataAuthorization {
    RouterDataAuthorization {
        dataset_id: input.manifest.dataset_id.clone(),
        owner_id: input.manifest.owner_id.clone(),
        target_use: TARGET_USE.into(),
        redaction_version: input.manifest.redaction_version.clone(),
        expires_at: input.manifest.expires_at,
        approved_sources: input
            .sources
            .iter()
            .map(|s| (s.source_id.clone(), content_sha256(s).unwrap()))
            .collect(),
        revoked_source_ids: vec![],
    }
}
fn build(input: RouterDatasetInput) -> Result<RouterDataset, String> {
    let a = auth(&input);
    build_router_dataset(input, &a, Utc::now())
}
#[test]
fn builds_allowlisted_dataset_and_preserves_unknown_legacy_records() {
    let input = fixture();
    let dataset = build(input.clone()).unwrap();
    assert!(dataset.examples[0].paired_training_eligible);
    let json = serde_json::to_value(&dataset.examples[0]).unwrap();
    assert!(json.get("work_admission").is_none());
    assert!(json.get("assessment").is_none());
    assert_eq!(dataset.examples[0].selected_action_probability, 1.0);
    let mut historical = input;
    historical.sources[0].decision.features = None;
    let dataset = build(historical).unwrap();
    assert!(!dataset.examples[0].paired_training_eligible);
    assert!(dataset.examples[0].features.is_none());
}
#[test]
fn rollout_provenance_keeps_treatment_out_of_deterministic_training_evidence() {
    use crate::tuning::rollout::{RolloutCohort, RouterRolloutDecision};
    let mut input = fixture();
    let decision = &mut input.sources[0].decision;
    decision.rollout = Some(RouterRolloutDecision {
        deployment_id: "deployment".into(),
        revision: 1,
        candidate_sha256: "a".repeat(64),
        rubric_version: "rubric-1".into(),
        routing_failure: None,
        cohort: RolloutCohort::Shadow,
        cohort_probability_basis_points: 10_000,
        proposed_offering_id: decision.policy.economy_offering_id.clone(),
        abstained: false,
        admission_rejected: false,
        routing_overhead_us: 100,
    });
    build(input.clone()).unwrap();
    let rollout = input.sources[0].decision.rollout.as_mut().unwrap();
    rollout.cohort = RolloutCohort::Control;
    rollout.cohort_probability_basis_points = 9000;
    build(input.clone()).unwrap();

    let rollout = input.sources[0].decision.rollout.as_mut().unwrap();
    rollout.cohort = RolloutCohort::Treatment;
    rollout.cohort_probability_basis_points = 1000;
    assert_eq!(
        build(input.clone()).unwrap_err(),
        "Routing algorithm differs from pinned rollout provenance"
    );
    input.sources[0].decision.policy_version =
        astra_turn_types::model_routing::LEARNED_CANARY_ROUTING_POLICY_VERSION.into();
    assert_eq!(
        build(input.clone()).unwrap_err(),
        "Routing policy revision mismatch"
    );
    input.manifest.policy_version = input.sources[0].decision.policy_version.clone();
    assert_eq!(
        build(input).unwrap_err(),
        "Unsupported logging policy; action probability is unknown"
    );
}
#[test]
fn consent_scope_hash_revocation_expiration_and_deletion_fail_closed() {
    let input = fixture();
    let approved = auth(&input);
    for case in ["scope", "owner", "expiry", "revoked", "deleted", "changed"] {
        let mut a = approved.clone();
        let mut i = input.clone();
        match case {
            "scope" => a.target_use = "other".into(),
            "owner" => a.owner_id = "other".into(),
            "expiry" => a.expires_at = i.manifest.created_at,
            "revoked" => a.revoked_source_ids.push(i.sources[0].source_id.clone()),
            "deleted" => a.approved_sources.clear(),
            "changed" => i.sources[0].group_keys.push("new-group".into()),
            _ => unreachable!(),
        }
        assert!(build_router_dataset(i, &a, Utc::now()).is_err(), "{case}");
    }
    let dataset = build(input).unwrap();
    validate_router_dataset(&dataset, &approved, Utc::now()).unwrap();
    let mut revoked = approved;
    revoked.approved_sources.clear();
    assert!(validate_router_dataset(&dataset, &revoked, Utc::now()).is_err());
}
#[test]
fn rejects_cross_owner_duplicate_and_future_evidence() {
    for case in [
        "owner",
        "duplicate",
        "cutoff",
        "feature",
        "quality-target",
        "rubric",
        "cost",
        "private-id",
    ] {
        let mut input = fixture();
        let source = &mut input.sources[0];
        match case {
            "owner" => source.owner_id = "other".into(),
            "duplicate" => {
                let copy = source.clone();
                input.sources.push(copy);
            }
            "cutoff" => source.decision_at = input.manifest.created_at,
            "feature" => {
                source.decision.features.as_mut().unwrap().difficulty = TaskDifficulty::Difficult
            }
            "quality-target" => {
                source
                    .paired
                    .as_mut()
                    .unwrap()
                    .economy
                    .episode
                    .quality
                    .as_mut()
                    .unwrap()
                    .target_execution_id = "other".into()
            }
            "rubric" => {
                source
                    .paired
                    .as_mut()
                    .unwrap()
                    .strong
                    .episode
                    .quality
                    .as_mut()
                    .unwrap()
                    .rubric_version = "other".into()
            }
            "cost" => {
                source
                    .paired
                    .as_mut()
                    .unwrap()
                    .strong
                    .episode
                    .cost
                    .as_mut()
                    .unwrap()
                    .total_usd = -1.0
            }
            "private-id" => source.group_keys.push("https://private.test/task".into()),
            _ => unreachable!(),
        }
        assert!(build(input).is_err(), "{case}");
    }
}
#[test]
fn rejects_mismatched_inputs_contracts_and_shared_mutable_replays() {
    for case in ["input", "snapshot", "model", "execution", "sandbox"] {
        let mut input = fixture();
        let pair = input.sources[0].paired.as_mut().unwrap();
        match case {
            "input" => pair.strong.input_reference.prefix_root = "future-user-input".into(),
            "snapshot" => pair.strong.snapshot_root = "different-tools".into(),
            "model" => pair.strong.episode.contract_root = "new-model-version".into(),
            "execution" => {
                pair.strong.episode.execution_id = pair.economy.episode.execution_id.clone()
            }
            "sandbox" => pair.isolation = ReplayIsolation::IsolatedSandbox,
            _ => unreachable!(),
        }
        assert!(build(input).is_err(), "{case}");
    }
}
#[test]
fn related_groups_and_prefixes_cannot_cross_time_splits() {
    for case in ["session", "prefix", "group"] {
        let mut input = fixture();
        let mut later = input.sources[0].clone();
        later.source_id = "later".into();
        later.decision.run_id = "later-run".into();
        later.decision_at = input.manifest.train_before;
        later.observed = None;
        later.paired = None;
        if case != "session" {
            later.decision.session_id = "later-session".into();
        }
        if case != "prefix" {
            later.decision.input_reference.as_mut().unwrap().prefix_root = "later-prefix".into();
        }
        if case != "group" {
            later.group_keys = vec!["later-task".into()];
        }
        input.sources.push(later);
        assert!(
            build(input).unwrap_err().contains("cross dataset splits"),
            "{case}"
        );
    }
}
#[test]
fn incomplete_and_failed_pairs_are_retained_without_training_labels() {
    for case in [
        "no-pair",
        "provider",
        "cancelled",
        "cost",
        "partial-cost",
        "unknown",
        "late-label",
        "ineligible",
    ] {
        let mut input = fixture();
        let source = &mut input.sources[0];
        let pair = source.paired.as_mut().unwrap();
        match case {
            "no-pair" => source.paired = None,
            "provider" => pair.economy.episode.status = EpisodeStatus::ProviderFailure,
            "cancelled" => pair.economy.episode.status = EpisodeStatus::Cancelled,
            "cost" => pair.economy.episode.cost = None,
            "partial-cost" => {
                pair.economy
                    .episode
                    .cost
                    .as_mut()
                    .unwrap()
                    .covers_full_episode = false
            }
            "unknown" => {
                pair.economy.episode.quality.as_mut().unwrap().verdict = Acceptability::Unknown
            }
            "late-label" => {
                pair.economy.episode.quality.as_mut().unwrap().assessed_at =
                    input.manifest.created_at
            }
            "ineligible" => pair.both_eligible = false,
            _ => unreachable!(),
        }
        let d = build(input).unwrap();
        assert_eq!(d.examples.len(), 1);
        assert!(!d.examples[0].paired_training_eligible, "{case}");
    }
}
#[test]
fn feedback_is_bound_to_actual_response_and_never_changes_features_or_quality() {
    let mut input = fixture();
    let source = &mut input.sources[0];
    let mut observed = source.paired.as_ref().unwrap().economy.episode.clone();
    observed.execution_id = "original-execution".into();
    observed.started_at = source.decision_at - Duration::seconds(5);
    observed.quality = None;
    source.followup = Some(RouterFollowup {
        source_id: "followup".into(),
        observed_at: observed.completed_at + Duration::seconds(1),
        response_reference: observed.response_reference.clone().unwrap(),
        assessment: astra_turn_types::TurnAssessment {
            feedback_relation: FeedbackResponseRelation::PreviousResponse,
            satisfaction: astra_turn_types::ResponseSatisfaction::Satisfied,
            difficulty: TaskDifficulty::Difficult,
            ..Default::default()
        },
    });
    source.observed = Some(observed);
    let dataset = build(input.clone()).unwrap();
    assert_eq!(
        dataset.examples[0].features.unwrap().difficulty,
        TaskDifficulty::Easy
    );
    assert!(
        dataset.examples[0]
            .observed
            .as_ref()
            .unwrap()
            .quality
            .is_none()
    );
    input.sources[0]
        .followup
        .as_mut()
        .unwrap()
        .response_reference
        .prefix_root = "wrong-branch".into();
    assert!(build(input).is_err());
}

#[test]
fn late_isolated_replay_uses_its_own_matured_outcome_window() {
    let mut input = fixture();
    let pair = input.sources[0].paired.as_mut().unwrap();
    for arm in [&mut pair.economy, &mut pair.strong] {
        arm.episode.started_at += Duration::days(40);
        arm.episode.completed_at += Duration::days(40);
        arm.episode.quality.as_mut().unwrap().assessed_at += Duration::days(40);
    }
    assert!(build(input).unwrap().examples[0].paired_training_eligible);
}
#[test]
fn unknown_logging_policy_does_not_invent_assignment_probability() {
    let mut input = fixture();
    input.manifest.policy_version = "unknown-randomized-policy".into();
    input.sources[0].decision.policy_version = input.manifest.policy_version.clone();
    assert!(
        build(input)
            .unwrap_err()
            .contains("action probability is unknown")
    );
}

fn observed_feedback_fixture() -> RouterDatasetInput {
    let mut input = fixture();
    let source = &mut input.sources[0];
    let mut observed = source.paired.as_ref().unwrap().economy.episode.clone();
    observed.execution_id = "original-execution".into();
    observed.started_at = source.decision_at - Duration::seconds(5);
    let quality = observed.quality.as_mut().unwrap();
    quality.target_execution_id = observed.execution_id.clone();
    quality.evidence_ids = vec!["observed-verification".into()];
    source.followup = Some(RouterFollowup {
        source_id: "referenced-feedback".into(),
        observed_at: observed.completed_at + Duration::seconds(5),
        response_reference: observed.response_reference.clone().unwrap(),
        assessment: astra_turn_types::TurnAssessment {
            feedback_relation: FeedbackResponseRelation::PreviousResponse,
            satisfaction: astra_turn_types::ResponseSatisfaction::Dissatisfied,
            ..Default::default()
        },
    });
    source.observed = Some(observed);
    input
}

#[test]
fn lineage_revocation_covers_all_retained_dependencies_on_build_and_revalidation() {
    let mut input = observed_feedback_fixture();
    // Even an example excluded from training retains evidence that must be revocable.
    input.sources[0].decision.features = None;
    let approved = auth(&input);
    let dataset = build(input.clone()).unwrap();
    assert!(!dataset.examples[0].paired_training_eligible);
    let expected = BTreeSet::from([
        "source-1",
        "referenced-feedback",
        "original-execution",
        "observed-verification",
        "economy-execution",
        "strong-execution",
        "economy-verification",
        "strong-verification",
        "synthetic-environment-root",
    ]);
    assert_eq!(dataset.source_ids(), expected);
    let restored: RouterDataset =
        serde_json::from_value(serde_json::to_value(&dataset).unwrap()).unwrap();
    assert_eq!(restored.source_ids(), expected);
    validate_router_dataset(&restored, &approved, Utc::now()).unwrap();
    for id in expected {
        let mut withdrawn = approved.clone();
        withdrawn.revoked_source_ids.push(id.into());
        assert!(
            build_router_dataset(input.clone(), &withdrawn, Utc::now()).is_err(),
            "{id}"
        );
        assert!(
            validate_router_dataset(&restored, &withdrawn, Utc::now()).is_err(),
            "{id}"
        );
    }
    let mut unrelated = approved;
    unrelated
        .revoked_source_ids
        .push("unrelated-feedback".into());
    build_router_dataset(input, &unrelated, Utc::now()).unwrap();
    validate_router_dataset(&restored, &unrelated, Utc::now()).unwrap();
}

#[test]
fn observed_episode_preserves_judge_time_while_replays_remain_after_cutoff() {
    let input = observed_feedback_fixture();
    let expected = input.sources[0].observed.clone().unwrap();
    let dataset = build(input.clone()).unwrap();
    assert_eq!(dataset.examples[0].observed.as_ref(), Some(&expected));
    assert_eq!(
        (expected.completed_at - expected.started_at).num_seconds(),
        10
    );
    assert!(dataset.examples[0].followup.is_some());
    for case in [
        "observed-start-after",
        "observed-end-before",
        "observed-reversed",
        "observed-future-end",
        "observed-early-quality",
        "replay-start-before",
        "replay-reversed",
        "replay-future-end",
    ] {
        let mut invalid = input.clone();
        let source = &mut invalid.sources[0];
        let cutoff = source.decision_at;
        let observed = source.observed.as_mut().unwrap();
        let replay = &mut source.paired.as_mut().unwrap().economy.episode;
        match case {
            "observed-start-after" => observed.started_at = cutoff + Duration::seconds(1),
            "observed-end-before" => observed.completed_at = cutoff - Duration::seconds(1),
            "observed-reversed" => {
                observed.started_at = observed.completed_at + Duration::seconds(1)
            }
            "observed-future-end" => {
                observed.completed_at = invalid.manifest.created_at + Duration::seconds(1)
            }
            "observed-early-quality" => observed.quality.as_mut().unwrap().assessed_at = cutoff,
            "replay-start-before" => replay.started_at = cutoff - Duration::seconds(1),
            "replay-reversed" => replay.completed_at = replay.started_at - Duration::seconds(1),
            "replay-future-end" => {
                replay.completed_at = invalid.manifest.created_at + Duration::seconds(1)
            }
            _ => unreachable!(),
        }
        assert!(build(invalid).is_err(), "{case}");
    }
    let mut boundary = input;
    let source = &mut boundary.sources[0];
    source.observed.as_mut().unwrap().started_at = source.decision_at;
    source.observed.as_mut().unwrap().completed_at = source.decision_at;
    source.paired.as_mut().unwrap().economy.episode.started_at = source.decision_at;
    build(boundary).unwrap();
}
