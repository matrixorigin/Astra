//! Adaptive baseline store — persists promoted experiment winners and reapplies
//! them as durable per-scope baselines.
#![allow(deprecated)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::ab_testing::{Experiment, ExperimentAnalysis, Recommendation, apply_config_diffs};
use crate::evolution::types::ProposalPromotionRecommendation;
use crate::pipeline::routing::{DomainHint, TaskType, domain_hint_to_label};
use crate::runtime_config::RuntimeConfig;
use crate::runtime_promotion_signals::{RuntimePromotionScorecard, RuntimePromotionSignals};

const BASELINE_PROMOTE_CONFIDENCE_THRESHOLD: f64 = 0.85;
const BASELINE_CANARY_CONFIDENCE_THRESHOLD: f64 = 0.75;
const BASELINE_PROMOTE_SCORE_THRESHOLD: f64 = 0.78;
const BASELINE_CANARY_SCORE_THRESHOLD: f64 = 0.60;
const BASELINE_SUPPORT_SCORE_THRESHOLD: f64 = 0.60;
const BASELINE_SAFETY_SCORE_THRESHOLD: f64 = 0.65;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveBaselineScope {
    pub task_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

impl AdaptiveBaselineScope {
    pub fn for_routing(task_type: TaskType, domain: Option<DomainHint>) -> Self {
        Self {
            task_type: task_type_label(task_type).to_string(),
            domain: domain.map(|value| domain_hint_to_label(value).to_string()),
        }
    }

    pub fn from_experiment(experiment: &Experiment) -> Option<Self> {
        let task_type = experiment
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("task_type:"))?
            .to_string();
        let domain = experiment
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("domain:"))
            .and_then(|value| (value != "any").then(|| value.to_string()));
        Some(Self { task_type, domain })
    }

    fn key(&self) -> String {
        format!(
            "{}::{}",
            self.task_type,
            self.domain.as_deref().unwrap_or("any")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveBaseline {
    pub scope: AdaptiveBaselineScope,
    pub experiment_id: String,
    pub variant_id: String,
    pub promoted_at: SystemTime,
    pub config_diff: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AdaptiveBaselineSnapshot {
    active: HashMap<String, AdaptiveBaseline>,
    history: HashMap<String, Vec<AdaptiveBaseline>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveBaselinePromotion {
    pub scope: AdaptiveBaselineScope,
    pub experiment_id: String,
    pub variant_id: String,
    pub replaced_existing: bool,
    pub config_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveBaselineRollback {
    pub scope: AdaptiveBaselineScope,
    pub removed_variant_id: String,
    pub restored_variant_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdaptiveBaselinePromotionVerdict {
    pub recommendation: ProposalPromotionRecommendation,
    pub confidence_score: f64,
    pub support_score: f64,
    pub safety_score: f64,
    pub overall_score: f64,
    pub evidence: Vec<String>,
    pub blockers: Vec<String>,
    pub rollback_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdaptiveBaselinePromotionDecision {
    Promoted {
        promotion: AdaptiveBaselinePromotion,
        verdict: AdaptiveBaselinePromotionVerdict,
    },
    Deferred(AdaptiveBaselinePromotionVerdict),
    Skipped,
}

pub struct AdaptiveBaselineStore {
    baselines: RwLock<AdaptiveBaselineSnapshot>,
    storage_path: Option<PathBuf>,
}

impl Default for AdaptiveBaselineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveBaselineStore {
    pub fn new() -> Self {
        Self {
            baselines: RwLock::new(AdaptiveBaselineSnapshot::default()),
            storage_path: None,
        }
    }

    pub fn with_storage(path: PathBuf) -> Self {
        let snapshot = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|data| serde_json::from_str::<AdaptiveBaselineSnapshot>(&data).ok())
                .unwrap_or_default()
        } else {
            AdaptiveBaselineSnapshot::default()
        };

        Self {
            baselines: RwLock::new(snapshot),
            storage_path: Some(path),
        }
    }

    pub fn promote_winner(
        &self,
        experiment: &Experiment,
        winner_variant_id: &str,
    ) -> Result<Option<AdaptiveBaselinePromotion>, String> {
        let winner = experiment.variant(winner_variant_id).ok_or_else(|| {
            format!(
                "experiment {} missing winner variant {winner_variant_id}",
                experiment.id
            )
        })?;
        if winner.is_control || winner.config_diff.is_empty() {
            return Ok(None);
        }
        let scope = AdaptiveBaselineScope::from_experiment(experiment)
            .ok_or_else(|| format!("experiment {} missing baseline scope tags", experiment.id))?;
        let baseline = AdaptiveBaseline {
            scope: scope.clone(),
            experiment_id: experiment.id.clone(),
            variant_id: winner.id.clone(),
            promoted_at: SystemTime::now(),
            config_diff: winner.config_diff.clone(),
        };

        let mut snapshot = self.baselines.write().unwrap_or_else(|e| e.into_inner());
        let key = scope.key();
        let replaced = snapshot.active.insert(key.clone(), baseline);
        if let Some(previous) = replaced.clone() {
            snapshot.history.entry(key).or_default().push(previous);
        }
        self.persist(&snapshot);

        let mut config_keys = winner.config_diff.keys().cloned().collect::<Vec<_>>();
        config_keys.sort();

        Ok(Some(AdaptiveBaselinePromotion {
            scope,
            experiment_id: experiment.id.clone(),
            variant_id: winner.id.clone(),
            replaced_existing: replaced.is_some(),
            config_keys,
        }))
    }

    pub fn resolve(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
    ) -> Option<AdaptiveBaseline> {
        let scope = AdaptiveBaselineScope::for_routing(task_type, domain);
        let snapshot = self.baselines.read().unwrap_or_else(|e| e.into_inner());
        snapshot.active.get(&scope.key()).cloned().or_else(|| {
            scope.domain.as_ref()?;
            let fallback = AdaptiveBaselineScope {
                task_type: scope.task_type,
                domain: None,
            };
            snapshot.active.get(&fallback.key()).cloned()
        })
    }

    pub fn has_scope(&self, scope: &AdaptiveBaselineScope) -> bool {
        let snapshot = self.baselines.read().unwrap_or_else(|e| e.into_inner());
        snapshot.active.contains_key(&scope.key())
    }

    pub fn apply_to_config(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
        config: &mut RuntimeConfig,
    ) -> Option<AdaptiveBaseline> {
        let baseline = self.resolve(task_type, domain)?;
        apply_config_diffs(config, &baseline.config_diff);
        Some(baseline)
    }

    pub fn rollback(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
    ) -> Option<AdaptiveBaselineRollback> {
        let scope = AdaptiveBaselineScope::for_routing(task_type, domain);
        let key = scope.key();
        let mut snapshot = self.baselines.write().unwrap_or_else(|e| e.into_inner());
        let removed = snapshot.active.remove(&key)?;
        let restored = snapshot
            .history
            .get_mut(&key)
            .and_then(|history| history.pop());
        if let Some(previous) = restored.clone() {
            snapshot.active.insert(key.clone(), previous);
        }
        if snapshot
            .history
            .get(&key)
            .is_some_and(|history| history.is_empty())
        {
            snapshot.history.remove(&key);
        }
        self.persist(&snapshot);

        Some(AdaptiveBaselineRollback {
            scope,
            removed_variant_id: removed.variant_id,
            restored_variant_id: restored.map(|baseline| baseline.variant_id),
        })
    }

    /// Rollback all baselines promoted from a specific experiment.
    ///
    /// Returns the list of rollbacks performed (one per scope where the
    /// experiment had a promoted baseline).
    pub fn rollback_experiment(&self, experiment_id: &str) -> Vec<AdaptiveBaselineRollback> {
        let mut snapshot = self.baselines.write().unwrap_or_else(|e| e.into_inner());
        let mut rollbacks = Vec::new();

        // Find all active baselines belonging to this experiment.
        let keys_to_rollback: Vec<String> = snapshot
            .active
            .iter()
            .filter(|(_, b)| b.experiment_id == experiment_id)
            .map(|(k, _)| k.clone())
            .collect();

        for key in keys_to_rollback {
            let Some(removed) = snapshot.active.remove(&key) else {
                continue;
            };
            let restored = snapshot
                .history
                .get_mut(&key)
                .and_then(|history| history.pop());
            if let Some(previous) = restored.clone() {
                snapshot.active.insert(key.clone(), previous);
            }
            if snapshot
                .history
                .get(&key)
                .is_some_and(|history| history.is_empty())
            {
                snapshot.history.remove(&key);
            }
            rollbacks.push(AdaptiveBaselineRollback {
                scope: removed.scope.clone(),
                removed_variant_id: removed.variant_id,
                restored_variant_id: restored.map(|b| b.variant_id),
            });
        }

        if !rollbacks.is_empty() {
            self.persist(&snapshot);
        }
        rollbacks
    }

    fn persist(&self, snapshot: &AdaptiveBaselineSnapshot) {
        let Some(path) = &self.storage_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!("[adaptive-baselines] failed to create storage directory: {err}");
                return;
            }
        }
        let Ok(data) = serde_json::to_string_pretty(snapshot) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if let Err(err) = std::fs::write(&tmp, data) {
            eprintln!("[adaptive-baselines] failed to write temp file: {err}");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            eprintln!("[adaptive-baselines] failed to rename temp file: {err}");
        }
    }
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::Code => "code",
        TaskType::Reasoning => "reasoning",
        TaskType::Fetch => "fetch",
        TaskType::Mutate => "mutate",
        TaskType::Memory => "memory",
        TaskType::Conversational => "conversational",
        TaskType::Compound => "compound",
        TaskType::Unknown => "unknown",
    }
}

pub fn evaluate_promotion_verdict(
    experiment: &Experiment,
    analysis: &ExperimentAnalysis,
    winner_variant_id: &str,
    replacing_existing: bool,
    promotion_signals: Option<&RuntimePromotionSignals>,
) -> Result<AdaptiveBaselinePromotionVerdict, String> {
    let winner = experiment.variant(winner_variant_id).ok_or_else(|| {
        format!(
            "experiment {} missing winner variant {winner_variant_id}",
            experiment.id
        )
    })?;

    let mut evidence = vec![format!("winner variant '{winner_variant_id}'")];
    let mut blockers = Vec::new();

    if winner.is_control {
        blockers.push("control variant does not produce an adaptive baseline".into());
    }
    if winner.config_diff.is_empty() {
        blockers.push("winner variant has no config diff to promote".into());
    }
    if !matches!(
        &analysis.recommendation,
        Recommendation::RolloutTreatment { variant_id } if variant_id == winner_variant_id
    ) {
        blockers.push(format!(
            "analysis recommendation {:?} does not support '{winner_variant_id}'",
            analysis.recommendation
        ));
    }

    let variant_stats = analysis.variant_stats.get(winner_variant_id).ok_or_else(|| {
        format!(
            "analysis missing variant statistics for winner {winner_variant_id} in experiment {}",
            experiment.id
        )
    })?;
    evidence.push(format!(
        "{} sample(s) for winner",
        variant_stats.sample_count
    ));

    let relevant_comparisons = analysis
        .comparisons
        .iter()
        .filter(|comparison| comparison.treatment_id == winner_variant_id)
        .collect::<Vec<_>>();
    if relevant_comparisons.is_empty() {
        blockers.push("no treatment vs control comparisons available for winner".into());
    }

    let significant_improvements = relevant_comparisons
        .iter()
        .copied()
        .filter(|comparison| comparison.is_significant && comparison.is_improvement)
        .collect::<Vec<_>>();
    let significant_regressions = relevant_comparisons
        .iter()
        .copied()
        .filter(|comparison| comparison.is_significant && !comparison.is_improvement)
        .collect::<Vec<_>>();

    if significant_improvements.is_empty() {
        blockers.push("winner has no significant improving metrics".into());
    } else {
        evidence.push(format!(
            "{} of {} metric comparison(s) improved significantly",
            significant_improvements.len(),
            relevant_comparisons.len()
        ));
    }
    if !significant_regressions.is_empty() {
        blockers.push(format!(
            "winner regresses on {}",
            significant_regressions
                .iter()
                .map(|comparison| comparison.metric.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let confidence_score = if significant_improvements.is_empty() {
        0.0
    } else {
        significant_improvements
            .iter()
            .map(|comparison| 1.0 - (comparison.p_value / 0.05).clamp(0.0, 1.0))
            .sum::<f64>()
            / significant_improvements.len() as f64
    };

    let min_samples = experiment.min_samples_per_variant.max(1) as f64;
    let sample_support = ((variant_stats.sample_count as f64) / (min_samples * 2.0))
        .clamp(0.0, 1.0)
        .max(0.75);
    let improvement_support = if relevant_comparisons.is_empty() {
        0.0
    } else {
        significant_improvements.len() as f64 / relevant_comparisons.len() as f64
    };
    let support_score = (sample_support * 0.7 + improvement_support * 0.3).clamp(0.0, 1.0);

    let mut safety_score: f64 = match winner.config_diff.len() {
        0 => 0.0,
        1 => 0.90,
        2 => 0.80,
        3 => 0.70,
        4 => 0.60,
        _ => 0.50,
    };
    evidence.push(format!(
        "winner changes {} config key(s)",
        winner.config_diff.len()
    ));
    if replacing_existing {
        safety_score = (safety_score - 0.10).max(0.0);
        evidence.push("promotion would replace an existing adaptive baseline".into());
    } else {
        evidence.push("promotion would establish a fresh adaptive baseline".into());
    }

    let mut scorecard = RuntimePromotionScorecard::new(
        confidence_score,
        support_score,
        safety_score,
        evidence,
        blockers,
    );
    scorecard.apply_signals(promotion_signals);
    let RuntimePromotionScorecard {
        confidence_score,
        support_score,
        safety_score,
        evidence,
        blockers,
    } = scorecard;

    let overall_score =
        (confidence_score * 0.40 + support_score * 0.35 + safety_score * 0.25).clamp(0.0, 1.0);
    let no_blockers = blockers.is_empty();
    let recommendation = if no_blockers
        && confidence_score >= BASELINE_PROMOTE_CONFIDENCE_THRESHOLD
        && support_score >= BASELINE_SUPPORT_SCORE_THRESHOLD
        && safety_score >= BASELINE_SAFETY_SCORE_THRESHOLD
        && overall_score >= BASELINE_PROMOTE_SCORE_THRESHOLD
    {
        ProposalPromotionRecommendation::Promote
    } else if no_blockers
        && confidence_score >= BASELINE_CANARY_CONFIDENCE_THRESHOLD
        && overall_score >= BASELINE_CANARY_SCORE_THRESHOLD
    {
        ProposalPromotionRecommendation::Canary
    } else {
        ProposalPromotionRecommendation::Hold
    };

    Ok(AdaptiveBaselinePromotionVerdict {
        recommendation,
        confidence_score,
        support_score,
        safety_score,
        overall_score,
        evidence,
        blockers,
        rollback_hint: Some(format!("rollback_experiment(\"{}\")", experiment.id)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab_testing::{ExperimentAnalyzer, ExperimentOutcome, MetricDefinition, Variant};
    use crate::runtime_promotion_signals::{RuntimePromotionGateSignal, RuntimePromotionSignals};
    use astra_core::confidence::ConfidenceInterval;
    use astra_services::evaluation::types::ValueInterval;

    #[test]
    fn promote_and_resolve_baseline() {
        let store = AdaptiveBaselineStore::new();
        let experiment = Experiment::new("exp-fetch")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(8)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();

        let promotion = store
            .promote_winner(&experiment, "treatment")
            .unwrap()
            .expect("promotion");
        assert_eq!(promotion.scope.task_type, "fetch");

        let baseline = store
            .resolve(TaskType::Fetch, Some(DomainHint::Code))
            .expect("baseline");
        assert_eq!(baseline.variant_id, "treatment");
    }

    #[test]
    fn rollback_restores_previous_baseline() {
        let store = AdaptiveBaselineStore::new();
        let first = Experiment::new("exp-one")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-a")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(7)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        let second = Experiment::new("exp-two")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-b")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(9)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();

        store.promote_winner(&first, "treatment-a").unwrap();
        store.promote_winner(&second, "treatment-b").unwrap();

        let rollback = store
            .rollback(TaskType::Fetch, None)
            .expect("rollback should restore previous baseline");
        assert_eq!(rollback.removed_variant_id, "treatment-b");
        assert_eq!(rollback.restored_variant_id.as_deref(), Some("treatment-a"));
    }

    #[test]
    fn persists_promoted_baselines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("adaptive-baselines.json");
        let experiment = Experiment::new("exp-persist")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("compression.max_history_tokens", serde_json::json!(28000)),
            )
            .with_tag("task_type:code")
            .with_tag("domain:any")
            .build();

        let store = AdaptiveBaselineStore::with_storage(path.clone());
        store.promote_winner(&experiment, "treatment").unwrap();

        let restored = AdaptiveBaselineStore::with_storage(path);
        let baseline = restored.resolve(TaskType::Code, None).expect("restored");
        assert_eq!(baseline.variant_id, "treatment");
        assert_eq!(
            baseline.config_diff.get("compression.max_history_tokens"),
            Some(&serde_json::json!(28000))
        );
    }

    #[test]
    fn rollback_experiment_removes_all_matching_baselines() {
        let store = AdaptiveBaselineStore::new();

        // Two experiments — one promoted for Code, one for Fetch.
        let exp_a = Experiment::new("exp-a")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-a")
                    .with_traffic(0.5)
                    .with_config_diff("max_tools", serde_json::json!(50)),
            )
            .with_tag("task_type:code")
            .with_tag("domain:any")
            .build();
        let _ = store.promote_winner(&exp_a, "treatment-a");

        let exp_b = Experiment::new("exp-b")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-b")
                    .with_traffic(0.5)
                    .with_config_diff("max_tools", serde_json::json!(60)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        let _ = store.promote_winner(&exp_b, "treatment-b");

        // Both are active.
        assert!(store.resolve(TaskType::Code, None).is_some());
        assert!(store.resolve(TaskType::Fetch, None).is_some());

        // Rollback experiment-a only.
        let rollbacks = store.rollback_experiment("exp-a");
        assert_eq!(rollbacks.len(), 1);
        assert_eq!(rollbacks[0].removed_variant_id, "treatment-a");

        // Code baseline is gone, Fetch is untouched.
        assert!(store.resolve(TaskType::Code, None).is_none());
        assert!(store.resolve(TaskType::Fetch, None).is_some());
    }

    #[test]
    fn rollback_experiment_no_match_returns_empty() {
        let store = AdaptiveBaselineStore::new();
        let rollbacks = store.rollback_experiment("no-such-experiment");
        assert!(rollbacks.is_empty());
    }

    #[test]
    fn promotion_verdict_promotes_clean_winner() {
        let experiment = Experiment::new("exp-judge")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(8)),
            )
            .with_metric(MetricDefinition::success_rate())
            .with_min_samples(5)
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        let outcomes = vec![
            ExperimentOutcome::new("c1", "control").with_metric("success_rate", 0.35),
            ExperimentOutcome::new("c2", "control").with_metric("success_rate", 0.40),
            ExperimentOutcome::new("c3", "control").with_metric("success_rate", 0.45),
            ExperimentOutcome::new("c4", "control").with_metric("success_rate", 0.38),
            ExperimentOutcome::new("c5", "control").with_metric("success_rate", 0.42),
            ExperimentOutcome::new("t1", "treatment").with_metric("success_rate", 0.80),
            ExperimentOutcome::new("t2", "treatment").with_metric("success_rate", 0.85),
            ExperimentOutcome::new("t3", "treatment").with_metric("success_rate", 0.88),
            ExperimentOutcome::new("t4", "treatment").with_metric("success_rate", 0.90),
            ExperimentOutcome::new("t5", "treatment").with_metric("success_rate", 0.86),
        ];
        let analysis = ExperimentAnalyzer::analyze(&experiment, &outcomes);

        let verdict = evaluate_promotion_verdict(&experiment, &analysis, "treatment", false, None)
            .expect("promotion verdict");

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Promote
        );
    }

    #[test]
    fn promotion_verdict_holds_mixed_metric_winner() {
        let experiment = Experiment::new("exp-mixed")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(8))
                    .with_config_diff("compression.max_history_tokens", serde_json::json!(28000)),
            )
            .with_metric(MetricDefinition::success_rate())
            .with_metric(MetricDefinition::token_usage())
            .with_min_samples(5)
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        let outcomes = vec![
            ExperimentOutcome::new("c1", "control")
                .with_metric("success_rate", 0.35)
                .with_metric("token_usage", 100.0),
            ExperimentOutcome::new("c2", "control")
                .with_metric("success_rate", 0.40)
                .with_metric("token_usage", 98.0),
            ExperimentOutcome::new("c3", "control")
                .with_metric("success_rate", 0.45)
                .with_metric("token_usage", 102.0),
            ExperimentOutcome::new("c4", "control")
                .with_metric("success_rate", 0.38)
                .with_metric("token_usage", 101.0),
            ExperimentOutcome::new("c5", "control")
                .with_metric("success_rate", 0.42)
                .with_metric("token_usage", 99.0),
            ExperimentOutcome::new("t1", "treatment")
                .with_metric("success_rate", 0.80)
                .with_metric("token_usage", 180.0),
            ExperimentOutcome::new("t2", "treatment")
                .with_metric("success_rate", 0.85)
                .with_metric("token_usage", 185.0),
            ExperimentOutcome::new("t3", "treatment")
                .with_metric("success_rate", 0.88)
                .with_metric("token_usage", 190.0),
            ExperimentOutcome::new("t4", "treatment")
                .with_metric("success_rate", 0.90)
                .with_metric("token_usage", 175.0),
            ExperimentOutcome::new("t5", "treatment")
                .with_metric("success_rate", 0.86)
                .with_metric("token_usage", 188.0),
        ];
        let analysis = ExperimentAnalyzer::analyze(&experiment, &outcomes);

        let verdict = evaluate_promotion_verdict(&experiment, &analysis, "treatment", false, None)
            .expect("promotion verdict");

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Hold
        );
        assert!(
            verdict
                .blockers
                .iter()
                .any(|blocker| blocker.contains("token_usage"))
        );
    }

    #[test]
    fn promotion_verdict_defers_when_global_quality_regresses() {
        let experiment = Experiment::new("exp-regression")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("tool.selection.top_k", serde_json::json!(6)),
            )
            .with_metric(MetricDefinition::success_rate())
            .with_min_samples(5)
            .with_tag("task_type:code")
            .with_tag("domain:any")
            .build();
        let outcomes = vec![
            ExperimentOutcome::new("c1", "control").with_metric("success_rate", 0.35),
            ExperimentOutcome::new("c2", "control").with_metric("success_rate", 0.40),
            ExperimentOutcome::new("c3", "control").with_metric("success_rate", 0.45),
            ExperimentOutcome::new("c4", "control").with_metric("success_rate", 0.38),
            ExperimentOutcome::new("c5", "control").with_metric("success_rate", 0.42),
            ExperimentOutcome::new("t1", "treatment").with_metric("success_rate", 0.80),
            ExperimentOutcome::new("t2", "treatment").with_metric("success_rate", 0.85),
            ExperimentOutcome::new("t3", "treatment").with_metric("success_rate", 0.88),
            ExperimentOutcome::new("t4", "treatment").with_metric("success_rate", 0.90),
            ExperimentOutcome::new("t5", "treatment").with_metric("success_rate", 0.86),
        ];
        let analysis = ExperimentAnalyzer::analyze(&experiment, &outcomes);
        let signals = RuntimePromotionSignals {
            noise_filtered_quality: Some(ConfidenceInterval::new(0.43, 0.43, 0.43)),
            latest_gate: Some(RuntimePromotionGateSignal {
                passed: false,
                score_delta: Some(ValueInterval::exact(-0.11)),
            }),
            calibration_error: Some(ValueInterval::exact(0.24)),
            ..RuntimePromotionSignals::default()
        };

        let verdict =
            evaluate_promotion_verdict(&experiment, &analysis, "treatment", false, Some(&signals))
                .expect("promotion verdict");

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Hold
        );
        assert!(
            verdict
                .blockers
                .iter()
                .any(|blocker| blocker.contains("global quality trend"))
        );
    }
}
