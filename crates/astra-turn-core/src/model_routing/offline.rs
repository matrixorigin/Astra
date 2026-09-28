//! Reproducible categorical outcome baseline. This produces an offline candidate
//! only; it cannot authorize an Offering or activate a runtime policy.
use astra_services::evaluation::router::*;
use astra_turn_types::model_routing::ModelRoutingFeatures;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterTrainingConfig {
    pub minimum_training_groups: usize,
    pub minimum_validation_groups: usize,
    pub maximum_quality_regression: f64,
    pub quality_thresholds: Vec<f64>,
}
impl Default for RouterTrainingConfig {
    fn default() -> Self {
        Self {
            minimum_training_groups: 20,
            minimum_validation_groups: 20,
            maximum_quality_regression: 0.01,
            quality_thresholds: vec![0.7, 0.8, 0.9, 0.95, 1.0],
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutcomeEstimate {
    pub groups: usize,
    pub acceptable: usize,
    pub mean_cost_usd: f64,
}
impl OutcomeEstimate {
    pub fn probability(&self) -> f64 {
        self.acceptable as f64 / self.groups.max(1) as f64
    }
    /// Wilson lower bound used as a conservative threshold heuristic, not a
    /// claim of calibrated task-level confidence or a production rollout gate.
    pub fn lower_bound(&self) -> f64 {
        if self.groups == 0 {
            return 0.0;
        }
        let n = self.groups as f64;
        let p = self.probability();
        let z = 1.96;
        (p + z * z / (2.0 * n) - z * ((p * (1.0 - p) + z * z / (4.0 * n)) / n).sqrt())
            / (1.0 + z * z / n)
    }
    fn add(&mut self, episode: &RouterEpisode) {
        self.groups += 1;
        self.acceptable += usize::from(
            episode.quality.as_ref().expect("validated label").verdict == Acceptability::Acceptable,
        );
        self.mean_cost_usd += (episode.cost.as_ref().expect("validated cost").total_usd
            - self.mean_cost_usd)
            / self.groups as f64;
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutcomeBucket {
    pub economy: OutcomeEstimate,
    pub strong: OutcomeEstimate,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterCandidate {
    pub schema_version: u8,
    pub algorithm_version: String,
    pub feature_version: u8,
    pub dataset_sha256: String,
    pub economy: ModelProfile,
    pub strong: ModelProfile,
    pub config: RouterTrainingConfig,
    pub threshold: Option<f64>,
    pub buckets: BTreeMap<String, OutcomeBucket>,
    pub activation: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateChoice {
    Economy,
    Strong,
    Abstain,
}
fn feature_key(features: ModelRoutingFeatures) -> String {
    serde_json::to_string(&features).expect("finite typed feature schema")
}
impl RouterCandidate {
    pub fn choose(&self, features: ModelRoutingFeatures) -> CandidateChoice {
        if features.schema_version != FEATURE_VERSION
            || !features.read_only_primary
            || !features.supported_input
        {
            return CandidateChoice::Abstain;
        }
        let Some(threshold) = self.threshold else {
            return CandidateChoice::Abstain;
        };
        let Some(bucket) = self.buckets.get(&feature_key(features)) else {
            return CandidateChoice::Abstain;
        };
        if bucket.economy.groups < self.config.minimum_training_groups {
            return CandidateChoice::Abstain;
        }
        let economy = bucket.economy.lower_bound() >= threshold;
        let strong = bucket.strong.lower_bound() >= threshold;
        if economy && (!strong || bucket.economy.mean_cost_usd < bucket.strong.mean_cost_usd) {
            CandidateChoice::Economy
        } else if strong {
            CandidateChoice::Strong
        } else {
            CandidateChoice::Abstain
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolicyMetrics {
    pub groups: usize,
    pub acceptable: usize,
    pub economy_choices: usize,
    pub abstentions: usize,
    pub total_cost_usd: f64,
    pub cost_per_acceptable_task: Option<f64>,
    pub acceptable_rate: Option<f64>,
    pub p50_latency_ms: Option<i64>,
    pub p95_latency_ms: Option<i64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub examples: usize,
    pub independent_groups: usize,
    pub complete_paired_groups: usize,
    pub missing_features: usize,
    pub missing_pairs: usize,
    pub unknown_quality: usize,
    pub unknown_full_cost: usize,
    pub ineligible_pairs: usize,
    pub failed_or_nonreplayable_pairs: usize,
    pub followup_observations: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterEvaluationReport {
    pub dataset_sha256: String,
    pub coverage: BTreeMap<DatasetSplit, Coverage>,
    pub validation: BTreeMap<String, PolicyMetrics>,
    pub test: BTreeMap<String, PolicyMetrics>,
    /// Empirical out-of-sample calibration diagnostic, not a confidence label.
    pub test_economy_brier_score: Option<f64>,
    pub calibration_status: String,
    pub production_qualified: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterTrainingOutput {
    pub dataset: RouterDataset,
    pub candidate: RouterCandidate,
    pub report: RouterEvaluationReport,
}

/// Pick the first source by immutable source ID from each connected group before
/// inspecting label availability. Shared sessions, repos or duplicate keys join
/// transitively. This avoids treating correlated rounds as independent evidence.
fn representatives(examples: &[RouterExample]) -> Vec<&RouterExample> {
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        let mut root = i;
        while parent[root] != root {
            root = parent[root];
        }
        while parent[i] != i {
            let next = parent[i];
            parent[i] = root;
            i = next;
        }
        root
    }
    let mut parent: Vec<_> = (0..examples.len()).collect();
    let mut keys = BTreeMap::new();
    for (i, example) in examples.iter().enumerate() {
        for key in &example.group_keys {
            if let Some(previous) = keys.insert(key, i) {
                let a = root(&mut parent, i);
                let b = root(&mut parent, previous);
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut seen = BTreeSet::new();
    examples
        .iter()
        .enumerate()
        .filter_map(|(i, e)| seen.insert(root(&mut parent, i)).then_some(e))
        .collect()
}
fn evaluate(
    examples: &[&RouterExample],
    choice: impl Fn(&RouterExample) -> CandidateChoice,
) -> PolicyMetrics {
    let mut result = PolicyMetrics::default();
    let mut latencies = Vec::new();
    for example in examples {
        let pair = example.paired.as_ref().expect("complete pair");
        let selection = choice(example);
        let episode = match selection {
            CandidateChoice::Economy => {
                result.economy_choices += 1;
                &pair.economy.episode
            }
            CandidateChoice::Strong => &pair.strong.episode,
            CandidateChoice::Abstain => {
                result.abstentions += 1;
                &pair.strong.episode
            }
        };
        result.groups += 1;
        result.acceptable += usize::from(
            episode.quality.as_ref().expect("known verdict").verdict == Acceptability::Acceptable,
        );
        result.total_cost_usd += episode.cost.as_ref().expect("full cost").total_usd;
        latencies.push((episode.completed_at - episode.started_at).num_milliseconds());
    }
    if result.groups > 0 {
        result.acceptable_rate = Some(result.acceptable as f64 / result.groups as f64);
        latencies.sort();
        result.p50_latency_ms = Some(latencies[(latencies.len() - 1) / 2]);
        result.p95_latency_ms = Some(latencies[(latencies.len() * 95).div_ceil(100) - 1]);
    }
    if result.acceptable > 0 {
        result.cost_per_acceptable_task = Some(result.total_cost_usd / result.acceptable as f64);
    }
    result
}
fn comparisons(
    examples: &[&RouterExample],
    candidate: &RouterCandidate,
) -> BTreeMap<String, PolicyMetrics> {
    BTreeMap::from([
        (
            "always_economy".into(),
            evaluate(examples, |_| CandidateChoice::Economy),
        ),
        (
            "always_strong".into(),
            evaluate(examples, |_| CandidateChoice::Strong),
        ),
        (
            "deterministic_auto".into(),
            evaluate(examples, |example| {
                // Preserve the immutable online choice, including catalog and
                // contract fallbacks that cannot be recovered from features.
                if example.selected_profile_id == candidate.economy.profile_id {
                    CandidateChoice::Economy
                } else {
                    CandidateChoice::Strong
                }
            }),
        ),
        (
            "learned_candidate".into(),
            evaluate(examples, |example| {
                candidate.choose(example.features.expect("frozen features"))
            }),
        ),
    ])
}

/// The only training entrypoint builds from authorized source evidence itself;
/// it never trusts a caller-provided eligibility mask or exported feature table.
pub fn train_router(
    input: RouterDatasetInput,
    auth: &RouterDataAuthorization,
    now: DateTime<Utc>,
    config: RouterTrainingConfig,
) -> Result<RouterTrainingOutput, String> {
    if config.minimum_training_groups == 0
        || config.minimum_validation_groups == 0
        || !config.maximum_quality_regression.is_finite()
        || !(0.0..=0.1).contains(&config.maximum_quality_regression)
        || config.quality_thresholds.is_empty()
        || config
            .quality_thresholds
            .iter()
            .any(|t| !t.is_finite() || !(0.5..=1.0).contains(t))
    {
        return Err("Invalid router training configuration".into());
    }
    let dataset = build_router_dataset(input, auth, now)?;
    let representatives = representatives(&dataset.examples);
    let mut candidate = RouterCandidate {
        schema_version: 1,
        algorithm_version: "categorical-paired-outcomes-v1".into(),
        feature_version: FEATURE_VERSION,
        dataset_sha256: dataset.content_sha256.clone(),
        economy: dataset.manifest.economy.clone(),
        strong: dataset.manifest.strong.clone(),
        config,
        threshold: None,
        buckets: BTreeMap::new(),
        activation: "offline_only".into(),
    };
    for example in representatives
        .iter()
        .filter(|e| e.split == DatasetSplit::Train && e.paired_training_eligible)
    {
        let bucket = candidate
            .buckets
            .entry(feature_key(example.features.expect("features")))
            .or_default();
        let pair = example.paired.as_ref().expect("pair");
        bucket.economy.add(&pair.economy.episode);
        bucket.strong.add(&pair.strong.episode);
    }
    let validation: Vec<_> = representatives
        .iter()
        .copied()
        .filter(|e| e.split == DatasetSplit::Validation && e.paired_training_eligible)
        .collect();
    let test: Vec<_> = representatives
        .iter()
        .copied()
        .filter(|e| e.split == DatasetSplit::Test && e.paired_training_eligible)
        .collect();
    let baseline = evaluate(&validation, |_| CandidateChoice::Strong);
    let mut best_cost = baseline.total_cost_usd;
    let mut best = None;
    if validation.len() >= candidate.config.minimum_validation_groups {
        let mut thresholds = candidate.config.quality_thresholds.clone();
        thresholds.sort_by(f64::total_cmp);
        thresholds.dedup();
        // Validation chooses only a threshold. Test outcomes never enter fitting
        // or threshold selection, including when test performance is worse.
        for threshold in thresholds.into_iter().rev() {
            candidate.threshold = Some(threshold);
            let metrics = evaluate(&validation, |example| {
                candidate.choose(example.features.expect("frozen features"))
            });
            if metrics.acceptable_rate.unwrap_or(0.0) + candidate.config.maximum_quality_regression
                >= baseline.acceptable_rate.unwrap_or(0.0)
                && metrics.total_cost_usd < best_cost
            {
                best_cost = metrics.total_cost_usd;
                best = Some(threshold);
            }
        }
    }
    candidate.threshold = best;
    let mut coverage = BTreeMap::new();
    for split in [
        DatasetSplit::Train,
        DatasetSplit::Validation,
        DatasetSplit::Test,
    ] {
        let mut c = Coverage::default();
        for e in dataset.examples.iter().filter(|e| e.split == split) {
            c.examples += 1;
            c.missing_features += usize::from(e.features.is_none());
            c.followup_observations += usize::from(e.followup.is_some());
            if let Some(p) = &e.paired {
                c.ineligible_pairs += usize::from(!p.both_eligible);
                c.failed_or_nonreplayable_pairs += usize::from(
                    [&p.economy.episode, &p.strong.episode]
                        .iter()
                        .any(|a| a.status != EpisodeStatus::Completed),
                );
                c.unknown_quality +=
                    usize::from([&p.economy.episode, &p.strong.episode].iter().any(|a| {
                        verified_acceptability(
                            a,
                            dataset.manifest.outcome_horizon_seconds,
                            dataset.manifest.created_at,
                        )
                        .is_none()
                    }));
                c.unknown_full_cost += usize::from(
                    [&p.economy.episode, &p.strong.episode]
                        .iter()
                        .any(|a| a.cost.as_ref().is_none_or(|cost| !cost.covers_full_episode)),
                );
            } else {
                c.missing_pairs += 1;
            }
        }
        c.independent_groups = representatives.iter().filter(|e| e.split == split).count();
        c.complete_paired_groups = representatives
            .iter()
            .filter(|e| e.split == split && e.paired_training_eligible)
            .count();
        coverage.insert(split, c);
    }
    let mut brier_sum = 0.0;
    let mut brier_count = 0;
    for e in &test {
        if let Some(bucket) = candidate
            .buckets
            .get(&feature_key(e.features.expect("features")))
        {
            let acceptable = e
                .paired
                .as_ref()
                .expect("pair")
                .economy
                .episode
                .quality
                .as_ref()
                .expect("quality")
                .verdict
                == Acceptability::Acceptable;
            brier_sum += (bucket.economy.probability() - f64::from(acceptable)).powi(2);
            brier_count += 1;
        }
    }
    let report = RouterEvaluationReport {
        dataset_sha256: dataset.content_sha256.clone(),
        coverage,
        validation: comparisons(&validation, &candidate),
        test: comparisons(&test, &candidate),
        test_economy_brier_score: (brier_count > 0).then(|| brier_sum / brier_count as f64),
        calibration_status: if best.is_some() {
            "validation_threshold_selected"
        } else {
            "insufficient_evidence_or_no_cost_improvement"
        }
        .into(),
        production_qualified: false,
    };
    Ok(RouterTrainingOutput {
        dataset,
        candidate,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data() -> RouterDatasetInput {
        let mut input: RouterDatasetInput = serde_json::from_str(include_str!(
            "../../../../fixtures/contracts/model_router_offline.json"
        ))
        .unwrap();
        let template = input.sources.remove(0);
        for (split, day) in [
            ("train", "2024-01-01"),
            ("validation", "2024-02-01"),
            ("test", "2024-03-01"),
        ] {
            for index in 0..8 {
                let mut s = template.clone();
                let id = format!("{split}-{index}");
                s.source_id = id.clone();
                s.decision.run_id = id.clone();
                s.decision.session_id = id.clone();
                s.group_keys = vec![id.clone()];
                s.decision.input_reference.as_mut().unwrap().prefix_root = id.clone();
                s.decision_at = format!("{day}T00:00:00Z").parse().unwrap();
                let pair = s.paired.as_mut().unwrap();
                for (name, arm) in [("economy", &mut pair.economy), ("strong", &mut pair.strong)] {
                    arm.input_reference = s.decision.input_reference.clone().unwrap();
                    arm.episode.execution_id = format!("{id}-{name}");
                    arm.episode.started_at = s.decision_at + chrono::Duration::seconds(1);
                    arm.episode.completed_at = s.decision_at + chrono::Duration::seconds(5);
                    let q = arm.episode.quality.as_mut().unwrap();
                    q.target_execution_id = arm.episode.execution_id.clone();
                    q.assessed_at = s.decision_at + chrono::Duration::seconds(6);
                }
                input.sources.push(s);
            }
        }
        input
    }
    fn run(input: RouterDatasetInput) -> RouterTrainingOutput {
        let auth = RouterDataAuthorization {
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
        };
        train_router(
            input,
            &auth,
            Utc::now(),
            RouterTrainingConfig {
                minimum_training_groups: 4,
                minimum_validation_groups: 4,
                maximum_quality_regression: 0.01,
                quality_thresholds: vec![0.5, 0.9],
            },
        )
        .unwrap()
    }
    #[test]
    fn paired_training_calibrates_and_reports_same_cohort_costs() {
        let result = run(data());
        assert_eq!(result.candidate.threshold, Some(0.5));
        assert_eq!(result.report.test["learned_candidate"].economy_choices, 8);
        assert_eq!(result.report.test["deterministic_auto"].economy_choices, 8);
        assert_eq!(result.report.test["always_strong"].groups, 8);
        assert!(
            (result.report.test["learned_candidate"]
                .cost_per_acceptable_task
                .unwrap()
                - 0.01)
                .abs()
                < 1e-10
        );
        assert_eq!(result.report.test_economy_brier_score, Some(0.0));
        assert!(!result.report.production_qualified);
        assert_eq!(result.candidate.activation, "offline_only");
    }
    #[test]
    fn deterministic_auto_preserves_recorded_fallbacks_on_paired_evidence() {
        use astra_turn_types::model_routing::ModelRoutingReason;

        for reason in [
            ModelRoutingReason::IncompatibleCandidate,
            ModelRoutingReason::EconomyUnavailable,
        ] {
            let mut input = data();
            for source in input
                .sources
                .iter_mut()
                .filter(|source| !source.source_id.starts_with("train"))
            {
                source.decision.selected_offering_id = input.manifest.strong.offering_id.clone();
                source.decision.selected_contract_root =
                    input.manifest.strong.contract_root.clone();
                source.decision.selected_model = "synthetic-strong".into();
                source.decision.reason = reason;
                source
                    .paired
                    .as_mut()
                    .unwrap()
                    .economy
                    .episode
                    .quality
                    .as_mut()
                    .unwrap()
                    .verdict = Acceptability::Unacceptable;
            }
            let result = run(input);
            for comparison in [&result.report.validation, &result.report.test] {
                let auto = &comparison["deterministic_auto"];
                assert_eq!(auto.groups, 8);
                assert_eq!(auto.economy_choices, 0);
                assert_eq!(auto.acceptable_rate, Some(1.0));
                assert!((auto.total_cost_usd - 0.4).abs() < 1e-10);
                assert_eq!(
                    auto.total_cost_usd,
                    comparison["always_strong"].total_cost_usd
                );
                assert_eq!(comparison["always_economy"].acceptable_rate, Some(0.0));
            }
        }
    }

    #[test]
    fn held_out_test_labels_never_change_training_or_threshold() {
        let original = run(data());
        let mut changed = data();
        for s in changed
            .sources
            .iter_mut()
            .filter(|s| s.source_id.starts_with("test"))
        {
            s.paired
                .as_mut()
                .unwrap()
                .economy
                .episode
                .quality
                .as_mut()
                .unwrap()
                .verdict = Acceptability::Unacceptable;
        }
        let altered = run(changed);
        assert_eq!(original.candidate.threshold, altered.candidate.threshold);
        assert_eq!(
            serde_json::to_value(original.candidate.buckets).unwrap(),
            serde_json::to_value(altered.candidate.buckets).unwrap()
        );
        assert_eq!(
            altered.report.test["learned_candidate"].acceptable_rate,
            Some(0.0)
        );
        assert_eq!(
            altered.report.test["learned_candidate"].cost_per_acceptable_task,
            None
        );
        assert_eq!(altered.report.test_economy_brier_score, Some(1.0));
    }
    #[test]
    fn validation_regression_and_both_fail_data_cannot_qualify_economy() {
        for split in ["validation", "train"] {
            let mut input = data();
            for s in input
                .sources
                .iter_mut()
                .filter(|s| s.source_id.starts_with(split))
            {
                let pair = s.paired.as_mut().unwrap();
                pair.economy.episode.quality.as_mut().unwrap().verdict =
                    Acceptability::Unacceptable;
                if split == "train" {
                    pair.strong.episode.quality.as_mut().unwrap().verdict =
                        Acceptability::Unacceptable;
                }
            }
            let result = run(input);
            assert!(result.candidate.threshold.is_none());
            assert_eq!(result.report.test["learned_candidate"].abstentions, 8);
        }
    }
    #[test]
    fn repeated_related_tasks_cannot_inflate_sample_support() {
        let mut input = data();
        for s in input
            .sources
            .iter_mut()
            .filter(|s| s.source_id.starts_with("train"))
        {
            s.group_keys = vec!["same-repository".into()];
        }
        let result = run(input);
        assert_eq!(
            result.report.coverage[&DatasetSplit::Train].independent_groups,
            1
        );
        assert_eq!(
            result
                .candidate
                .buckets
                .values()
                .next()
                .unwrap()
                .economy
                .groups,
            1
        );
        assert!(result.candidate.threshold.is_none());
    }
    #[test]
    fn historical_choices_and_sentiment_are_never_training_targets() {
        let mut input = data();
        for s in &mut input.sources {
            s.paired = None;
            s.decision.assessment.as_mut().unwrap().satisfaction =
                astra_turn_types::ResponseSatisfaction::Satisfied;
        }
        let result = run(input);
        assert!(result.candidate.buckets.is_empty());
        assert!(result.candidate.threshold.is_none());
        assert_eq!(result.report.test["learned_candidate"].groups, 0);
        assert_eq!(
            result.report.coverage[&DatasetSplit::Train].missing_pairs,
            8
        );
    }
    #[test]
    fn input_order_does_not_change_artifacts() {
        let input = data();
        let mut reverse = input.clone();
        reverse.sources.reverse();
        assert_eq!(
            serde_json::to_value(run(input)).unwrap(),
            serde_json::to_value(run(reverse)).unwrap()
        );
    }
}
