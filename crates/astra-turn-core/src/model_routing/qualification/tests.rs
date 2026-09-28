use super::*;
use astra_turn_types::{AssessmentConfidence, TaskDifficulty, model_routing::ModelRoutingReason};

fn config() -> RouterTrainingConfig {
    RouterTrainingConfig {
        minimum_training_groups: 4,
        minimum_validation_groups: 4,
        maximum_quality_regression: 0.01,
        quality_thresholds: vec![0.5],
    }
}
fn data() -> RouterDatasetInput {
    let mut input: RouterDatasetInput = serde_json::from_str(include_str!(
        "../../../../../fixtures/contracts/model_router_offline.json"
    ))
    .unwrap();
    let template = input.sources.remove(0);
    for (split, day, count) in [
        ("train", "2024-01-01", 8),
        ("validation", "2024-02-01", 8),
        ("test", "2024-03-01", 1200),
    ] {
        for i in 0..count {
            let mut s = template.clone();
            let id = format!("{split}-{i}");
            s.source_id = id.clone();
            s.decision.run_id = id.clone();
            s.decision.session_id = id.clone();
            s.group_keys = vec![id.clone()];
            s.decision.input_reference.as_mut().unwrap().prefix_root = id.clone();
            s.decision.assessment.as_mut().unwrap().difficulty = TaskDifficulty::Moderate;
            let f = s.decision.features.as_mut().unwrap();
            f.difficulty = TaskDifficulty::Moderate;
            f.difficulty_confidence = AssessmentConfidence::High;
            s.decision.selected_offering_id = input.manifest.strong.offering_id.clone();
            s.decision.selected_contract_root = input.manifest.strong.contract_root.clone();
            s.decision.reason = ModelRoutingReason::StrongRequired;
            s.decision_at = format!("{day}T00:00:00Z").parse().unwrap();
            let pair = s.paired.as_mut().unwrap();
            for (name, arm) in [("economy", &mut pair.economy), ("strong", &mut pair.strong)] {
                arm.input_reference = s.decision.input_reference.clone().unwrap();
                arm.episode.execution_id = format!("{id}-{name}");
                let replay_at =
                    s.decision_at + chrono::Duration::days(i64::from(split == "test") * 2);
                arm.episode.started_at = replay_at + chrono::Duration::seconds(1);
                arm.episode.completed_at = replay_at + chrono::Duration::seconds(5);
                let q = arm.episode.quality.as_mut().unwrap();
                q.target_execution_id = arm.episode.execution_id.clone();
                q.assessed_at = replay_at + chrono::Duration::seconds(6);
            }
            input.sources.push(s);
        }
    }
    input
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
fn protocol(input: &RouterDatasetInput) -> RouterQualificationProtocol {
    RouterQualificationProtocol {
        schema_version: 1,
        job_id: "qualification-1".into(),
        owner_id: input.manifest.owner_id.clone(),
        dataset_id: input.manifest.dataset_id.clone(),
        registered_at: "2024-03-02T00:00:00Z".parse().unwrap(),
        training_config_sha256: content_sha256(&config()).unwrap(),
        evaluation_plan_sha256: router_evaluation_plan_sha256(input).unwrap(),
        minimum_test_groups: 1000,
        minimum_stratum_groups: 1000,
        minimum_pair_coverage: 0.95,
        maximum_quality_regression: 0.1,
        minimum_cost_saving_fraction: 0.2,
        maximum_episode_cost_usd: 0.1,
        maximum_p95_latency_ratio: 1.1,
        confidence: 0.9,
        required_strata: vec![input.sources[0].decision.features.unwrap()],
    }
}
fn qualify(input: RouterDatasetInput) -> RouterQualificationOutput {
    qualify_router(
        input.clone(),
        &auth(&input),
        Utc::now(),
        config(),
        protocol(&input),
    )
    .unwrap()
}
fn shadow_data(training: &RouterDatasetInput) -> RouterDatasetInput {
    let mut shadow = training.clone();
    shadow.manifest.dataset_id = "shadow-1".into();
    shadow.manifest.created_at = training.manifest.created_at + chrono::Duration::days(3);
    shadow.sources.truncate(1);
    let s = &mut shadow.sources[0];
    s.source_id = "shadow-source".into();
    s.decision.run_id = "shadow-run".into();
    s.decision.session_id = "shadow-session".into();
    s.decision.input_reference.as_mut().unwrap().prefix_root = "shadow-prefix".into();
    s.group_keys = vec!["shadow-group".into()];
    s.decision_at = training.manifest.created_at + chrono::Duration::days(1);
    s.paired = None;
    shadow
}
#[test]
fn collected_trace_roster_can_be_sealed_before_held_out_replay() {
    let mut planned = data();
    planned.manifest.created_at = "2024-03-05T00:00:00Z".parse().unwrap();
    let mut replays = BTreeMap::new();
    for source in &mut planned.sources {
        if source.decision_at >= planned.manifest.validation_before {
            replays.insert(source.source_id.clone(), source.paired.take().unwrap());
        }
    }
    // March 1: collect immutable decisions. March 2: seal their actual roster,
    // with no held-out replay or outcomes yet. March 3: run paired replays.
    let sealed = protocol(&planned);
    assert!(
        planned
            .sources
            .iter()
            .all(|s| s.decision_at < sealed.registered_at)
    );
    for source in &mut planned.sources {
        if let Some(pair) = replays.remove(&source.source_id) {
            assert!(pair.economy.episode.started_at > sealed.registered_at);
            assert!(pair.strong.episode.started_at > sealed.registered_at);
            source.paired = Some(pair);
        }
    }
    assert_eq!(
        router_evaluation_plan_sha256(&planned).unwrap(),
        sealed.evaluation_plan_sha256
    );
    // March 5: both replay outcome horizons have matured; renew source grants.
    let result = qualify_router(
        planned.clone(),
        &auth(&planned),
        planned.manifest.created_at,
        config(),
        sealed,
    )
    .unwrap();
    assert_eq!(
        result.tuning.status,
        RouterQualificationStatus::ReadyForShadow
    );
}

#[test]
fn seal_rejects_future_decisions_and_preexisting_held_out_evidence() {
    for mutation in 0..5 {
        let mut input = data();
        let mut sealed = protocol(&input);
        let source = input.sources.last_mut().unwrap();
        match mutation {
            0 => sealed.registered_at = source.decision_at - chrono::Duration::seconds(1),
            1 => sealed.registered_at = source.paired.as_ref().unwrap().economy.episode.started_at,
            2 | 3 => {
                if mutation == 3 {
                    source.group_keys = vec!["test-0".into()];
                }
                let pair = source.paired.as_mut().unwrap();
                pair.strong.episode.started_at = sealed.registered_at;
                pair.strong.episode.quality = None;
            }
            _ => {
                let mut observed = source.paired.as_ref().unwrap().strong.episode.clone();
                observed.execution_id = "observed-before-seal".into();
                observed.started_at = source.decision_at;
                observed.completed_at = sealed.registered_at;
                observed.quality = None;
                source.observed = Some(observed);
            }
        }
        sealed.evaluation_plan_sha256 = router_evaluation_plan_sha256(&input).unwrap();
        let result = qualify_router(input.clone(), &auth(&input), Utc::now(), config(), sealed);
        assert!(
            result.unwrap_err().contains(if mutation == 0 {
                "decisions after its registration seal"
            } else {
                "must follow the registration seal"
            }),
            "mutation {mutation}"
        );
    }
}

#[test]
fn qualified_gate_is_reproducible_and_only_ready_for_shadow() {
    let input = data();
    let output = qualify(input.clone());
    assert_eq!(
        output.tuning.status,
        RouterQualificationStatus::ReadyForShadow
    );
    assert!(!output.tuning.production_qualified);
    assert_eq!(output.candidate.activation, "offline_only");
    assert!(
        output.cohorts["overall"]
            .comparisons
            .values()
            .all(|c| c.quality_passed && c.cost_passed)
    );
    let mut reversed = input;
    reversed.sources.reverse();
    let other = qualify(reversed);
    assert_eq!(
        output.tuning.candidate_sha256,
        other.tuning.candidate_sha256
    );
    assert_eq!(
        serde_json::to_value(output.cohorts).unwrap(),
        serde_json::to_value(other.cohorts).unwrap()
    );
}
#[test]
fn qualification_uses_recorded_auto_fallback_for_paired_comparison() {
    let mut input = data();
    for source in &mut input.sources {
        source.decision.assessment.as_mut().unwrap().difficulty = TaskDifficulty::Easy;
        source.decision.features.as_mut().unwrap().difficulty = TaskDifficulty::Easy;
        source.decision.reason = ModelRoutingReason::EconomyUnavailable;
    }
    let output = qualify(input);
    assert_eq!(
        output.tuning.status,
        RouterQualificationStatus::ReadyForShadow
    );
    let overall = &output.cohorts["overall"];
    assert_eq!(overall.policies["deterministic_auto"].economy_choices, 0);
    assert!(
        overall.comparisons["deterministic_auto"]
            .saving_lower_bound_usd
            .unwrap()
            > 0.0
    );
}
#[test]
fn rejects_small_biased_unbounded_regressed_and_slow_cohorts() {
    for mutation in 0..6 {
        let mut input = data();
        for s in input
            .sources
            .iter_mut()
            .filter(|s| s.source_id.starts_with("test"))
        {
            let pair = s.paired.as_mut().unwrap();
            match mutation {
                0 => s.group_keys = vec!["same-test-group".into()],
                1 => s.paired = None,
                2 => pair.economy.episode.cost.as_mut().unwrap().total_usd = 0.11,
                3 => {
                    pair.economy.episode.quality.as_mut().unwrap().verdict =
                        Acceptability::Unacceptable
                }
                4 => {
                    pair.economy.episode.completed_at += chrono::Duration::seconds(10);
                    pair.economy.episode.quality.as_mut().unwrap().assessed_at +=
                        chrono::Duration::seconds(10);
                }
                _ => {
                    pair.economy.episode.cost.as_mut().unwrap().total_usd = 0.09;
                }
            }
        }
        assert_eq!(
            qualify(input).tuning.status,
            RouterQualificationStatus::Rejected,
            "mutation {mutation}"
        );
    }
}
#[test]
fn protocol_is_bound_to_scope_config_cutoff_and_required_strata() {
    let input = data();
    for mutation in 0..7 {
        let mut p = protocol(&input);
        match mutation {
            0 => p.registered_at = input.manifest.created_at,
            1 => p.training_config_sha256 = "wrong".into(),
            2 => p.owner_id = "wrong".into(),
            3 => p.required_strata.clear(),
            4 => p.confidence = f64::NAN,
            5 => p.maximum_episode_cost_usd = f64::INFINITY,
            _ => p.maximum_episode_cost_usd = f64::MAX,
        }
        assert!(qualify_router(input.clone(), &auth(&input), Utc::now(), config(), p).is_err());
    }
    let mut p = protocol(&input);
    p.required_strata[0].difficulty = TaskDifficulty::Easy;
    let result = qualify_router(input.clone(), &auth(&input), Utc::now(), config(), p).unwrap();
    assert_eq!(result.tuning.status, RouterQualificationStatus::Rejected);
}
#[test]
fn shadow_scores_later_features_without_outcomes_and_abstains_outside_scope() {
    let input = data();
    let shadow = shadow_data(&input);
    let result = shadow_router(
        input.clone(),
        &auth(&input),
        shadow.clone(),
        &auth(&shadow),
        Utc::now(),
        config(),
        protocol(&input),
    )
    .unwrap();
    assert_eq!(result.decisions[0].choice, CandidateChoice::Economy);
    assert_eq!(result.disagreements, 1);
    assert!(result.source_ids.contains(&"shadow-source".into()));
    assert_eq!(result.mode, "offline_shadow");
    let mut novel = shadow;
    novel.sources[0].decision.features = None;
    let result = shadow_router(
        input.clone(),
        &auth(&input),
        novel.clone(),
        &auth(&novel),
        Utc::now(),
        config(),
        protocol(&input),
    )
    .unwrap();
    assert_eq!(result.abstentions, 1);
    assert_eq!(
        result.decisions[0].proposed_profile_id,
        input.manifest.strong.profile_id
    );
}
#[test]
fn shadow_rejects_leakage_scope_drift_and_cross_snapshot_revocation() {
    let input = data();
    for mutation in 0..5 {
        let mut shadow = shadow_data(&input);
        match mutation {
            0 => shadow.sources[0].group_keys = input.sources[0].group_keys.clone(),
            1 => shadow.sources[0].decision_at = input.manifest.created_at,
            2 => shadow.manifest.policy_revision = "new-revision".into(),
            3 => shadow.manifest.owner_id = "another-owner".into(),
            _ => {}
        }
        let mut shadow_auth = auth(&shadow);
        if mutation == 4 {
            shadow_auth
                .revoked_source_ids
                .push("economy-verification".into());
        }
        assert!(
            shadow_router(
                input.clone(),
                &auth(&input),
                shadow,
                &shadow_auth,
                Utc::now(),
                config(),
                protocol(&input)
            )
            .is_err(),
            "mutation {mutation}"
        );
    }
}
#[test]
fn revoked_evidence_invalidates_previously_passing_qualification() {
    let input = data();
    let mut authorization = auth(&input);
    authorization
        .revoked_source_ids
        .push("strong-verification".into());
    assert!(
        qualify_router(
            input.clone(),
            &authorization,
            Utc::now(),
            config(),
            protocol(&input)
        )
        .is_err()
    );
    let mut p = protocol(&input);
    p.maximum_quality_regression = 0.01;
    // Perfect point estimates with inadequate statistical power stay rejected.
    assert_eq!(
        qualify_router(input.clone(), &auth(&input), Utc::now(), config(), p)
            .unwrap()
            .tuning
            .status,
        RouterQualificationStatus::Rejected
    );
}

#[test]
fn later_training_labels_cannot_qualify_against_earlier_test_turns() {
    let mut input = data();
    let episode = &mut input.sources[0].paired.as_mut().unwrap().economy.episode;
    episode.started_at = input.manifest.validation_before;
    episode.completed_at = episode.started_at + chrono::Duration::seconds(5);
    episode.quality.as_mut().unwrap().assessed_at =
        episode.completed_at + chrono::Duration::seconds(1);
    let result = qualify(input);
    assert_eq!(result.tuning.status, RouterQualificationStatus::Rejected);
    assert!(
        result
            .failures
            .contains(&"fitting_labels_cross_test_cutoff".into())
    );
}

#[test]
fn registered_plan_rejects_changed_splits_roster_groups_and_observation_scope() {
    let input = data();
    let registered = protocol(&input);
    for mutation in 0..11 {
        let mut changed = input.clone();
        match mutation {
            0 => changed.manifest.validation_before += chrono::Duration::days(31),
            1 => changed.manifest.train_before += chrono::Duration::days(1),
            2 => changed.manifest.outcome_horizon_seconds += 1,
            3 => changed.manifest.created_at += chrono::Duration::days(1),
            4 => {
                changed.sources.pop();
            }
            5 => changed.sources[0].group_keys.push("another-group".into()),
            6 => changed.sources[0].decision_at += chrono::Duration::seconds(1),
            7 => changed.sources[0].decision.features = None,
            8 => changed.manifest.strong.contract_root = "another-contract".into(),
            9 => changed.sources[0].decision.selected_offering_id = "economy".into(),
            _ => changed.sources[0].decision.selected_contract_root = "another-contract".into(),
        }
        // Even renewed source authorization cannot amend a registered protocol.
        let result = qualify_router(
            changed.clone(),
            &auth(&changed),
            Utc::now(),
            config(),
            registered.clone(),
        );
        assert_eq!(
            result.unwrap_err(),
            "Router evaluation plan differs from the registered protocol",
            "mutation {mutation}"
        );
    }
}

#[test]
fn qualification_rejects_hidden_missing_and_out_of_scope_features() {
    for missing in [false, true] {
        let mut input = data();
        let source = input.sources.last_mut().unwrap();
        // test-0 represents this group, hiding this row from cohort metrics.
        source.group_keys = vec!["test-0".into()];
        if missing {
            source.decision.features = None;
        } else {
            source.decision.features.as_mut().unwrap().difficulty = TaskDifficulty::Easy;
            source.decision.assessment.as_mut().unwrap().difficulty = TaskDifficulty::Easy;
            source.decision.reason = ModelRoutingReason::EconomyUnavailable;
        }
        let output = qualify(input);
        assert_eq!(output.tuning.status, RouterQualificationStatus::Rejected);
        assert!(
            output
                .failures
                .contains(&"test_contains_missing_or_out_of_scope_features".into())
        );
        assert!(output.cohorts["overall"].failures.is_empty());
    }
}

#[test]
fn renewed_authorization_cannot_change_the_registered_auto_baseline() {
    let mut input = data();
    for source in &mut input.sources {
        source.decision.features.as_mut().unwrap().difficulty = TaskDifficulty::Easy;
        source.decision.assessment.as_mut().unwrap().difficulty = TaskDifficulty::Easy;
        source.decision.reason = ModelRoutingReason::EconomyUnavailable;
    }
    let registered = protocol(&input);
    assert_eq!(
        qualify(input.clone()).tuning.status,
        RouterQualificationStatus::ReadyForShadow
    );
    for source in &mut input.sources {
        source.decision.selected_offering_id = input.manifest.economy.offering_id.clone();
        source.decision.selected_contract_root = input.manifest.economy.contract_root.clone();
        source.decision.selected_model = "synthetic-economy".into();
        source.decision.reason = ModelRoutingReason::EasyReadOnly;
    }
    let renewed = auth(&input);
    let result = qualify_router(input, &renewed, Utc::now(), config(), registered);
    assert_eq!(
        result.unwrap_err(),
        "Router evaluation plan differs from the registered protocol"
    );
}

#[test]
fn known_over_ceiling_costs_reject_incomplete_and_nonrepresentative_pairs() {
    let original = data();
    let registered = protocol(&original);
    for mutation in 0..7 {
        let mut input = original.clone();
        let source = input.sources.last_mut().unwrap();
        let pair = source.paired.as_mut().unwrap();
        pair.economy.episode.cost.as_mut().unwrap().total_usd = 100.0;
        match mutation {
            0 => pair.economy.episode.quality = None,
            1 => pair.economy.episode.quality.as_mut().unwrap().verdict = Acceptability::Unknown,
            2 => pair.economy.episode.status = EpisodeStatus::ProviderFailure,
            3 => {
                pair.economy
                    .episode
                    .cost
                    .as_mut()
                    .unwrap()
                    .covers_full_episode = false
            }
            4 => pair.strong.episode.cost = None,
            5 => pair.both_eligible = false,
            _ => {
                // This ID sorts after test-0, so the expensive row will not be
                // picked as a group representative. Its known cost still counts.
                source.group_keys = vec!["test-0".into()];
            }
        }
        let p = if mutation == 6 {
            protocol(&input)
        } else {
            registered.clone()
        };
        let result = qualify_router(input.clone(), &auth(&input), Utc::now(), config(), p).unwrap();
        assert_eq!(
            result.tuning.status,
            RouterQualificationStatus::Rejected,
            "mutation {mutation}"
        );
        let cohort = &result.cohorts["overall"];
        assert!(cohort.pair_coverage >= registered.minimum_pair_coverage);
        assert!(
            cohort
                .failures
                .contains(&"episode_cost_exceeds_prespecified_bound".into())
        );
        for comparison in cohort.comparisons.values() {
            assert!(!comparison.cost_passed);
            assert!(comparison.saving_lower_bound_usd.is_none());
        }
    }
}

#[test]
fn live_rollout_requires_qualification_and_preserves_scope_and_session_assignment() {
    use crate::model_routing::rollout::*;
    use astra_services::tuning::rollout::*;
    let input = data();
    let now = Utc::now();
    let owner = input.manifest.owner_id.clone();
    let request = RouterPublishRequest {
        expected_revision: 0,
        authorization: auth(&input),
        config: config(),
        protocol: protocol(&input),
        review: RolloutReview {
            online_consent_reference: "consent-1".into(),
            verifier_review_reference: "verifier-1".into(),
            safety_review_reference: "safety-1".into(),
            expires_at: input.manifest.expires_at,
            minimum_shadow_sessions: 10,
            maximum_routing_overhead_ms: 100,
        },
        input: input.clone(),
    };
    assert!(prepare_shadow(request.clone(), "another-owner", now).is_err());
    let mut rejected = request.clone();
    rejected.protocol.minimum_test_groups = 100_000;
    assert!(prepare_shadow(rejected, &owner, now).is_err());
    let d = prepare_shadow(request, &owner, now).unwrap();
    let policy = input.sources[0].decision.policy.clone();
    let features = input.sources[0].decision.features.unwrap();
    let mut state = transition(
        RouterRolloutState::default(),
        &owner,
        RolloutChange::Publish(Box::new(d)),
        now,
    )
    .unwrap();
    let shadow = score_live(&state, &owner, "session-1", &policy, features, now)
        .unwrap()
        .unwrap();
    assert_eq!(shadow.cohort, RolloutCohort::Shadow);
    assert_eq!(shadow.proposed_offering_id, policy.economy_offering_id);
    assert!(score_live(&state, "other-owner", "session-1", &policy, features, now).is_err());
    assert!(
        transition(
            state.clone(),
            &owner,
            RolloutChange::Canary { basis_points: 1001 },
            now
        )
        .is_err()
    );
    state = transition(
        state,
        &owner,
        RolloutChange::Canary { basis_points: 1000 },
        now,
    )
    .unwrap();
    let restarted: RouterRolloutState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    let mut treatments = Vec::new();
    let mut controls = 0;
    for i in 0..2000 {
        let session = format!("session-{i}");
        let a = score_live(&state, &owner, &session, &policy, features, now)
            .unwrap()
            .unwrap();
        let b = score_live(&restarted, &owner, &session, &policy, features, now)
            .unwrap()
            .unwrap();
        assert_eq!(a, b);
        if a.cohort == RolloutCohort::Treatment {
            treatments.push(a);
        } else {
            controls += 1;
        }
    }
    assert!(!treatments.is_empty() && controls > treatments.len());
    let pinned = &treatments[0];
    validate_pinned_treatment(&state, pinned, now).unwrap();
    let mut unsupported = features;
    unsupported.supported_input = false;
    let abstain = score_live(&state, &owner, "session-1", &policy, unsupported, now)
        .unwrap()
        .unwrap();
    assert!(abstain.abstained);
    assert_eq!(abstain.proposed_offering_id, policy.strong_offering_id);
    let revoked = state.deployment.as_ref().unwrap().tuning.source_ids[0].clone();
    state = transition(
        state,
        &owner,
        RolloutChange::Revoke {
            source_ids: vec![revoked],
        },
        now,
    )
    .unwrap();
    assert!(validate_pinned_treatment(&state, pinned, now).is_err());
    assert!(
        score_live(&state, &owner, "session-1", &policy, features, now)
            .unwrap()
            .is_none()
    );
    let expired = restarted.deployment.as_ref().unwrap().expires_at;
    assert!(validate_pinned_treatment(&restarted, pinned, expired).is_err());
    let mut tampered = restarted;
    tampered
        .deployment
        .as_mut()
        .unwrap()
        .candidate_json
        .push(' ');
    assert!(score_live(&tampered, &owner, "session-1", &policy, features, now).is_err());
}
