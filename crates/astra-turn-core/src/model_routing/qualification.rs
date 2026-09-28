//! Preregistered offline qualification and observational scoring. Every public
//! entrypoint rebuilds from current authorized sources; saved reports are never
//! an authority to bypass consent or to activate a model.
use super::offline::{
    CandidateChoice, PolicyMetrics, RouterCandidate, RouterTrainingConfig, comparisons,
    feature_key, recorded_auto_choice, representatives, train_router,
};
use astra_services::evaluation::router::*;
use astra_services::tuning::{
    RouterQualificationProtocol, RouterQualificationStatus, RouterTuningRecord,
    router_evaluation_plan_sha256,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
pub struct PairedGateComparison {
    pub groups: usize,
    pub quality_difference: Option<f64>,
    pub quality_lower_bound: Option<f64>,
    /// Mean of (1 - required saving) * baseline cost - candidate cost.
    pub saving_surplus_usd: Option<f64>,
    pub saving_lower_bound_usd: Option<f64>,
    pub quality_passed: bool,
    pub cost_passed: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct QualificationCohort {
    pub independent_groups: usize,
    pub complete_paired_groups: usize,
    pub pair_coverage: f64,
    pub policies: BTreeMap<String, PolicyMetrics>,
    pub comparisons: BTreeMap<String, PairedGateComparison>,
    pub failures: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RouterQualificationOutput {
    pub tuning: RouterTuningRecord,
    pub candidate: RouterCandidate,
    pub protocol: RouterQualificationProtocol,
    pub cohorts: BTreeMap<String, QualificationCohort>,
    pub failures: Vec<String>,
}

fn validate_protocol(
    protocol: &RouterQualificationProtocol,
    input: &RouterDatasetInput,
    config: &RouterTrainingConfig,
) -> Result<(), String> {
    if protocol.evaluation_plan_sha256 != router_evaluation_plan_sha256(input)? {
        return Err("Router evaluation plan differs from the registered protocol".into());
    }
    let fraction = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    let opaque = |s: &str| {
        !s.is_empty()
            && s.len() <= 255
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    };
    let keys: BTreeSet<_> = protocol
        .required_strata
        .iter()
        .copied()
        .map(feature_key)
        .collect();
    if protocol.schema_version != 1
        || !opaque(&protocol.job_id)
        || protocol.owner_id != input.manifest.owner_id
        || protocol.dataset_id != input.manifest.dataset_id
        || protocol.registered_at >= input.manifest.validation_before
        || protocol.training_config_sha256 != content_sha256(config)?
        || protocol.minimum_test_groups == 0
        || protocol.minimum_stratum_groups == 0
        || !fraction(protocol.minimum_pair_coverage)
        || protocol.minimum_pair_coverage == 0.0
        || !fraction(protocol.maximum_quality_regression)
        || protocol.maximum_quality_regression > 0.1
        || !fraction(protocol.minimum_cost_saving_fraction)
        || protocol.minimum_cost_saving_fraction == 0.0
        || !protocol.maximum_episode_cost_usd.is_finite()
        || protocol.maximum_episode_cost_usd <= 0.0
        || protocol.maximum_episode_cost_usd > 1_000_000_000.0
        || !protocol.maximum_p95_latency_ratio.is_finite()
        || protocol.maximum_p95_latency_ratio < 1.0
        || !protocol.confidence.is_finite()
        || !(0.9..1.0).contains(&protocol.confidence)
        || keys.is_empty()
        || keys.len() > 64
        || keys.len() != protocol.required_strata.len()
        || protocol
            .required_strata
            .iter()
            .any(|f| f.schema_version != FEATURE_VERSION)
    {
        return Err("Invalid or unregistered router qualification protocol".into());
    }
    Ok(())
}
fn selected_episode(example: &RouterExample, choice: CandidateChoice) -> &RouterEpisode {
    let pair = example.paired.as_ref().expect("validated complete pair");
    match choice {
        CandidateChoice::Economy => &pair.economy.episode,
        CandidateChoice::Strong | CandidateChoice::Abstain => &pair.strong.episode,
    }
}
fn compare(
    examples: &[&RouterExample],
    candidate: &RouterCandidate,
    baseline: fn(&RouterExample, &RouterCandidate) -> CandidateChoice,
    protocol: &RouterQualificationProtocol,
    alpha: f64,
) -> PairedGateComparison {
    let mut result = PairedGateComparison {
        groups: examples.len(),
        quality_difference: None,
        quality_lower_bound: None,
        saving_surplus_usd: None,
        saving_lower_bound_usd: None,
        quality_passed: false,
        cost_passed: false,
    };
    if examples.is_empty() {
        return result;
    }
    let mut quality = 0.0;
    let mut saving = 0.0;
    for example in examples {
        let features = example.features.expect("complete features");
        let chosen = selected_episode(example, candidate.choose(features));
        let baseline = selected_episode(example, baseline(example, candidate));
        let acceptable = |e: &RouterEpisode| {
            f64::from(e.quality.as_ref().expect("label").verdict == Acceptability::Acceptable)
        };
        quality += acceptable(chosen) - acceptable(baseline);
        saving += (1.0 - protocol.minimum_cost_saving_fraction)
            * baseline.cost.as_ref().expect("cost").total_usd
            - chosen.cost.as_ref().expect("cost").total_usd;
    }
    let n = examples.len() as f64;
    // One-sided Hoeffding bounds on paired independent group representatives.
    // Quality lies in [-1, 1]; saving surplus in [-cap, (1-saving)*cap].
    // Bonferroni alpha is fixed across all prespecified comparisons and strata.
    let radius = ((1.0 / alpha).ln() / (2.0 * n)).sqrt();
    let quality_lower = (quality / n - 2.0 * radius).max(-1.0);
    let saving_lower = saving / n
        - (2.0 - protocol.minimum_cost_saving_fraction)
            * protocol.maximum_episode_cost_usd
            * radius;
    result.quality_difference = Some(quality / n);
    result.quality_lower_bound = Some(quality_lower);
    result.saving_surplus_usd = Some(saving / n);
    result.saving_lower_bound_usd = Some(saving_lower);
    result.quality_passed = quality_lower >= -protocol.maximum_quality_regression;
    result.cost_passed = saving_lower > 0.0;
    result
}
fn cohort(
    all: &[&RouterExample],
    evidence: &[&RouterExample],
    candidate: &RouterCandidate,
    protocol: &RouterQualificationProtocol,
    minimum: usize,
    alpha: f64,
    require_cost_improvement: bool,
) -> QualificationCohort {
    let complete: Vec<_> = all
        .iter()
        .copied()
        .filter(|e| e.paired_training_eligible)
        .collect();
    let coverage = complete.len() as f64 / all.len().max(1) as f64;
    let policies = comparisons(&complete, candidate);
    let mut failures = Vec::new();
    if complete.len() < minimum {
        failures.push("insufficient_independent_groups".into());
    }
    if coverage < protocol.minimum_pair_coverage {
        failures.push("insufficient_pair_coverage".into());
    }
    // A known ceiling violation invalidates the bound even when the pair lacks
    // labels, failed, or was not selected as its related group's representative.
    // Absent prices remain unknown; they are never imputed as zero-cost episodes.
    let bounded = evidence
        .iter()
        .filter_map(|e| e.paired.as_ref())
        .all(|pair| {
            [&pair.economy, &pair.strong].iter().all(|arm| {
                arm.episode
                    .cost
                    .as_ref()
                    .is_none_or(|cost| cost.total_usd <= protocol.maximum_episode_cost_usd)
            })
        });
    if !bounded {
        failures.push("episode_cost_exceeds_prespecified_bound".into());
    }
    let mut comparisons = BTreeMap::new();
    let baselines: [(
        &str,
        fn(&RouterExample, &RouterCandidate) -> CandidateChoice,
    ); 2] = [
        ("always_strong", |_, _| CandidateChoice::Strong),
        ("deterministic_auto", recorded_auto_choice),
    ];
    for (name, baseline) in baselines {
        let mut comparison = compare(&complete, candidate, baseline, protocol, alpha);
        if !bounded {
            comparison.quality_lower_bound = None;
            comparison.saving_lower_bound_usd = None;
            comparison.quality_passed = false;
            comparison.cost_passed = false;
        }
        if !comparison.quality_passed || (require_cost_improvement && !comparison.cost_passed) {
            failures.push(format!("{name}:inconclusive_or_regressed"));
        }
        let within_latency = match (
            policies["learned_candidate"].p95_latency_ms,
            policies[name].p95_latency_ms,
        ) {
            (Some(a), Some(b)) => a as f64 <= b as f64 * protocol.maximum_p95_latency_ratio,
            _ => false,
        };
        if !within_latency {
            failures.push(format!("{name}:p95_latency_limit"));
        }
        comparisons.insert(name.into(), comparison);
    }
    QualificationCohort {
        independent_groups: all.len(),
        complete_paired_groups: complete.len(),
        pair_coverage: coverage,
        policies,
        comparisons,
        failures,
    }
}

/// A successful gate qualifies only for observational scoring. The protocol's
/// registration date is an operator attestation, not cryptographic preregistration.
pub fn qualify_router(
    input: RouterDatasetInput,
    auth: &RouterDataAuthorization,
    now: DateTime<Utc>,
    config: RouterTrainingConfig,
    protocol: RouterQualificationProtocol,
) -> Result<RouterQualificationOutput, String> {
    validate_protocol(&protocol, &input, &config)?;
    let trained = train_router(input, auth, now, config)?;
    let reps = representatives(&trained.dataset.examples);
    // Fitting/threshold labels must already have matured before any test turn.
    // Time-splitting the source prompt alone does not prevent label lookahead.
    let fit_labels_ready = reps
        .iter()
        .filter(|e| e.split != DatasetSplit::Test && e.paired_training_eligible)
        .all(|e| {
            let pair = e.paired.as_ref().expect("eligible pair");
            [&pair.economy, &pair.strong].iter().all(|arm| {
                arm.episode.quality.as_ref().expect("label").assessed_at
                    < trained.dataset.manifest.validation_before
                    && arm
                        .episode
                        .started_at
                        .checked_add_signed(chrono::Duration::seconds(
                            trained.dataset.manifest.outcome_horizon_seconds.into(),
                        ))
                        .is_some_and(|horizon| horizon < trained.dataset.manifest.validation_before)
            })
        });
    let test: Vec<_> = reps
        .into_iter()
        .filter(|e| e.split == DatasetSplit::Test)
        .collect();
    let test_evidence: Vec<_> = trained
        .dataset
        .examples
        .iter()
        .filter(|e| e.split == DatasetSplit::Test)
        .collect();
    let keys: BTreeSet<_> = protocol
        .required_strata
        .iter()
        .copied()
        .map(feature_key)
        .collect();
    let alpha = (1.0 - protocol.confidence) / ((keys.len() + 1) * 4) as f64;
    let mut failures = Vec::new();
    if !fit_labels_ready {
        failures.push("fitting_labels_cross_test_cutoff".into());
    }
    if trained.candidate.threshold.is_none() {
        failures.push("candidate_has_no_validation_threshold".into());
    }
    // Group reduction must not hide missing or unexpected deployment strata.
    if test_evidence
        .iter()
        .any(|e| e.features.is_none_or(|f| !keys.contains(&feature_key(f))))
    {
        failures.push("test_contains_missing_or_out_of_scope_features".into());
    }
    let mut cohorts = BTreeMap::from([(
        "overall".into(),
        cohort(
            &test,
            &test_evidence,
            &trained.candidate,
            &protocol,
            protocol.minimum_test_groups,
            alpha,
            true,
        ),
    )]);
    for key in keys {
        let stratum: Vec<_> = test
            .iter()
            .copied()
            .filter(|e| e.features.is_some_and(|f| feature_key(f) == key))
            .collect();
        let stratum_evidence: Vec<_> = test_evidence
            .iter()
            .copied()
            .filter(|e| e.features.is_some_and(|f| feature_key(f) == key))
            .collect();
        cohorts.insert(
            key,
            cohort(
                &stratum,
                &stratum_evidence,
                &trained.candidate,
                &protocol,
                protocol.minimum_stratum_groups,
                alpha,
                false,
            ),
        );
    }
    if cohorts.values().any(|c| !c.failures.is_empty()) {
        failures.push("one_or_more_cohorts_failed".into());
    }
    let tuning = RouterTuningRecord {
        schema_version: 1,
        job_id: protocol.job_id.clone(),
        owner_id: protocol.owner_id.clone(),
        dataset_sha256: trained.dataset.content_sha256.clone(),
        candidate_sha256: content_sha256(&trained.candidate)?,
        protocol_sha256: content_sha256(&protocol)?,
        evaluated_at: now,
        expires_at: trained.dataset.manifest.expires_at,
        source_ids: trained
            .dataset
            .source_ids()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        status: if failures.is_empty() {
            RouterQualificationStatus::ReadyForShadow
        } else {
            RouterQualificationStatus::Rejected
        },
        production_qualified: false,
    };
    Ok(RouterQualificationOutput {
        tuning,
        candidate: trained.candidate,
        protocol,
        cohorts,
        failures,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct RouterShadowDecision {
    pub source_id: String,
    pub run_id: String,
    pub features: Option<astra_turn_types::model_routing::ModelRoutingFeatures>,
    pub historical_profile_id: String,
    pub proposed_profile_id: String,
    pub choice: CandidateChoice,
    pub agrees_with_historical_selection: bool,
    pub scoring_duration_ns: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct RouterShadowOutput {
    pub tuning: RouterTuningRecord,
    pub shadow_dataset_sha256: String,
    pub source_ids: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub decisions: Vec<RouterShadowDecision>,
    pub disagreements: usize,
    pub abstentions: usize,
    pub p95_scoring_duration_ns: Option<u64>,
    pub mode: &'static str,
}

/// Rebuild qualification and shadow evidence under current consent. No artifact
/// file is executable, and no shadow prediction is an admission decision.
pub fn shadow_router(
    training_input: RouterDatasetInput,
    training_auth: &RouterDataAuthorization,
    shadow_input: RouterDatasetInput,
    shadow_auth: &RouterDataAuthorization,
    now: DateTime<Utc>,
    config: RouterTrainingConfig,
    protocol: RouterQualificationProtocol,
) -> Result<RouterShadowOutput, String> {
    // A shared dependency withdrawn in either current snapshot invalidates both.
    let revoked: BTreeSet<_> = training_auth
        .revoked_source_ids
        .iter()
        .chain(&shadow_auth.revoked_source_ids)
        .cloned()
        .collect();
    let mut training_auth = training_auth.clone();
    let mut shadow_auth = shadow_auth.clone();
    training_auth.revoked_source_ids = revoked.iter().cloned().collect();
    shadow_auth.revoked_source_ids = revoked.into_iter().collect();
    let training = build_router_dataset(training_input.clone(), &training_auth, now)?;
    let qualification = qualify_router(training_input, &training_auth, now, config, protocol)?;
    if qualification.tuning.status != RouterQualificationStatus::ReadyForShadow {
        return Err("Router candidate did not pass qualification for shadow scoring".into());
    }
    let shadow = build_router_dataset(shadow_input, &shadow_auth, now)?;
    if training.manifest.owner_id != shadow.manifest.owner_id
        || training.manifest.economy != shadow.manifest.economy
        || training.manifest.strong != shadow.manifest.strong
        || training.manifest.policy_revision != shadow.manifest.policy_revision
        || training.manifest.rubric_version != shadow.manifest.rubric_version
        || shadow.examples.is_empty()
        || shadow
            .examples
            .iter()
            .any(|e| e.decision_at <= training.manifest.created_at)
    {
        return Err("Shadow scope, model contracts or decision cutoff differs".into());
    }
    let groups: BTreeSet<_> = training
        .examples
        .iter()
        .flat_map(|e| &e.group_keys)
        .collect();
    let lineage = training.source_ids();
    if shadow.examples.iter().any(|e| {
        e.group_keys.iter().any(|k| groups.contains(k)) || lineage.contains(e.source_id.as_str())
    }) {
        return Err("Shadow examples overlap the qualification dataset".into());
    }
    let keys: BTreeSet<_> = qualification
        .protocol
        .required_strata
        .iter()
        .copied()
        .map(feature_key)
        .collect();
    let candidate = &qualification.candidate;
    let mut decisions = Vec::new();
    for example in &shadow.examples {
        let start = std::time::Instant::now();
        let choice = match example.features {
            Some(f) if keys.contains(&feature_key(f)) => candidate.choose(f),
            _ => CandidateChoice::Abstain,
        };
        let scoring_duration_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let profile = if choice == CandidateChoice::Economy {
            &candidate.economy
        } else {
            &candidate.strong
        };
        decisions.push(RouterShadowDecision {
            source_id: example.source_id.clone(),
            run_id: example.run_id.clone(),
            features: example.features,
            historical_profile_id: example.selected_profile_id.clone(),
            proposed_profile_id: profile.profile_id.clone(),
            choice,
            agrees_with_historical_selection: example.selected_profile_id == profile.profile_id,
            scoring_duration_ns,
        });
    }
    let mut durations: Vec<_> = decisions.iter().map(|d| d.scoring_duration_ns).collect();
    durations.sort_unstable();
    let source_ids: BTreeSet<_> = training
        .source_ids()
        .into_iter()
        .chain(shadow.source_ids())
        .map(str::to_owned)
        .collect();
    Ok(RouterShadowOutput {
        tuning: qualification.tuning,
        shadow_dataset_sha256: shadow.content_sha256.clone(),
        source_ids: source_ids.into_iter().collect(),
        expires_at: training.manifest.expires_at.min(shadow.manifest.expires_at),
        disagreements: decisions
            .iter()
            .filter(|d| !d.agrees_with_historical_selection)
            .count(),
        abstentions: decisions
            .iter()
            .filter(|d| d.choice == CandidateChoice::Abstain)
            .count(),
        p95_scoring_duration_ns: durations
            .get((durations.len() * 95).div_ceil(100).saturating_sub(1))
            .copied(),
        decisions,
        mode: "offline_shadow",
    })
}

#[cfg(test)]
mod tests;
