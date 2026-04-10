//! Exploration engine — turns low-confidence pattern-library areas into A/B
//! experiments and concludes mature experiments once enough evidence exists.

use std::time::Duration;

use crate::ab_testing::{
    Experiment, ExperimentAnalyzer, ExperimentStatus, ExperimentStore, MetricDefinition,
    Recommendation, Variant,
};
use crate::pipeline::pattern::{ExplorationOpportunity, ExplorationReason, PatternLibrary};
use crate::pipeline::routing::{DomainHint, TaskType, domain_hint_to_label};

/// Result of concluding a mature experiment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentConclusion {
    pub experiment_id: String,
    pub winner_variant_id: Option<String>,
}

/// Creates and concludes exploration experiments.
#[derive(Debug, Clone)]
pub struct ExplorationEngine {
    confidence_threshold: f64,
    max_concurrent_experiments: usize,
    min_samples_per_variant: u32,
}

impl Default for ExplorationEngine {
    fn default() -> Self {
        Self {
            confidence_threshold: 0.5,
            max_concurrent_experiments: 3,
            min_samples_per_variant: 20,
        }
    }
}

impl ExplorationEngine {
    pub fn new(
        confidence_threshold: f64,
        max_concurrent_experiments: usize,
        min_samples_per_variant: u32,
    ) -> Self {
        Self {
            confidence_threshold,
            max_concurrent_experiments,
            min_samples_per_variant,
        }
    }

    /// Create experiments for low-confidence opportunities not already covered
    /// by an active experiment.
    pub fn check_and_create_experiments(
        &self,
        pattern_library: &PatternLibrary,
        store: &ExperimentStore,
    ) -> Vec<Experiment> {
        let active = store
            .list()
            .into_iter()
            .filter(|exp| exp.status == ExperimentStatus::Running)
            .collect::<Vec<_>>();
        if active.len() >= self.max_concurrent_experiments {
            return Vec::new();
        }

        let mut opportunities = pattern_library.exploration_opportunities();
        opportunities.sort_by(|a, b| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let remaining = self.max_concurrent_experiments - active.len();
        let mut created = Vec::new();
        for opportunity in opportunities {
            if created.len() >= remaining {
                break;
            }
            if opportunity.confidence >= self.confidence_threshold {
                continue;
            }
            if active
                .iter()
                .chain(created.iter())
                .any(|exp| experiment_matches_opportunity(exp, &opportunity))
            {
                continue;
            }

            let mut experiment = self.build_experiment(&opportunity);
            experiment.start();
            store.register(experiment.clone());
            created.push(experiment);
        }

        created
    }

    /// Conclude running experiments that have enough samples and return the
    /// winner, if any. The winning config is not applied here; callers decide
    /// how to promote it.
    pub fn conclude_mature_experiments(
        &self,
        store: &ExperimentStore,
    ) -> Vec<ExperimentConclusion> {
        let mut conclusions = Vec::new();

        for mut experiment in store.list() {
            if experiment.status != ExperimentStatus::Running {
                continue;
            }

            let samples = store.sample_counts(&experiment.id);
            if !experiment.has_sufficient_samples(&samples) {
                continue;
            }

            let outcomes = store.get_outcomes(&experiment.id);
            let analysis = ExperimentAnalyzer::analyze(&experiment, &outcomes);
            let winner_variant_id = match analysis.recommendation {
                Recommendation::RolloutTreatment { variant_id } => Some(variant_id),
                Recommendation::KeepControl => {
                    experiment.control().map(|variant| variant.id.clone())
                }
                Recommendation::InsufficientData
                | Recommendation::NoSignificantDifference
                | Recommendation::NeedsManualReview => None,
            };

            experiment.stop();
            store.register(experiment.clone());
            conclusions.push(ExperimentConclusion {
                experiment_id: experiment.id,
                winner_variant_id,
            });
        }

        conclusions
    }

    fn build_experiment(&self, opportunity: &ExplorationOpportunity) -> Experiment {
        let experiment_id = experiment_id_for(opportunity);
        let task_tag = task_type_tag(opportunity.task_type);
        let domain_tag = domain_tag(opportunity.domain);
        let reason_tag = reason_tag(&opportunity.reason);

        let treatment = treatment_variant_for(opportunity);
        Experiment::new(experiment_id.clone())
            .with_name(format!(
                "Explore {} / {}",
                task_tag,
                domain_tag.as_deref().unwrap_or("any")
            ))
            .with_description(format!(
                "Auto-created exploration for {:?} (confidence {:.2})",
                opportunity.reason, opportunity.confidence
            ))
            .with_variant(Variant::control())
            .with_variant(treatment)
            .with_metric(MetricDefinition::success_rate())
            .with_metric(MetricDefinition::latency())
            .with_metric(MetricDefinition::token_usage())
            .with_min_samples(self.min_samples_per_variant)
            .with_max_duration(Duration::from_secs(60 * 60))
            .with_tag(format!("task_type:{task_tag}"))
            .with_tag(format!(
                "domain:{}",
                domain_tag.unwrap_or_else(|| "any".to_string())
            ))
            .with_tag(format!("reason:{reason_tag}"))
            .build()
    }
}

fn treatment_variant_for(opportunity: &ExplorationOpportunity) -> Variant {
    match opportunity.reason {
        ExplorationReason::ColdStart => Variant::new("treatment-cold-start")
            .with_traffic(0.5)
            .with_config_diff("learning.exploration_rate", serde_json::json!(0.25))
            .with_config_diff("memory.retrieval_top_k", serde_json::json!(7)),
        ExplorationReason::Drift => Variant::new("treatment-drift")
            .with_traffic(0.5)
            .with_config_diff(
                "tool_selection.confidence_threshold",
                serde_json::json!(0.2),
            )
            .with_config_diff("learning.exploration_rate", serde_json::json!(0.2)),
        ExplorationReason::LowSuccess => Variant::new("treatment-low-success")
            .with_traffic(0.5)
            .with_config_diff("compression.max_history_tokens", serde_json::json!(60_000))
            .with_config_diff("memory.retrieval_top_k", serde_json::json!(8)),
    }
}

fn experiment_matches_opportunity(
    experiment: &Experiment,
    opportunity: &ExplorationOpportunity,
) -> bool {
    let expected_task = format!("task_type:{}", task_type_tag(opportunity.task_type));
    let expected_domain = format!(
        "domain:{}",
        domain_tag(opportunity.domain).unwrap_or_else(|| "any".to_string())
    );
    experiment.tags.iter().any(|tag| tag == &expected_task)
        && experiment.tags.iter().any(|tag| tag == &expected_domain)
}

fn experiment_id_for(opportunity: &ExplorationOpportunity) -> String {
    format!(
        "explore-{}-{}",
        task_type_tag(opportunity.task_type),
        domain_tag(opportunity.domain).unwrap_or_else(|| "any".to_string())
    )
}

fn task_type_tag(task_type: TaskType) -> &'static str {
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

fn domain_tag(domain: Option<DomainHint>) -> Option<String> {
    domain.map(|value| domain_hint_to_label(value).to_string())
}

fn reason_tag(reason: &ExplorationReason) -> &'static str {
    match reason {
        ExplorationReason::ColdStart => "cold_start",
        ExplorationReason::Drift => "drift",
        ExplorationReason::LowSuccess => "low_success",
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab_testing::ExperimentOutcome;

    fn low_confidence_library() -> PatternLibrary {
        let mut library = PatternLibrary::default();
        library.record_outcome(
            &["view".to_string()],
            TaskType::Fetch,
            None,
            false,
            0.2,
            None,
        );
        library.record_outcome(
            &["view".to_string()],
            TaskType::Fetch,
            None,
            false,
            0.3,
            None,
        );
        library
    }

    #[test]
    fn creates_experiment_for_low_confidence_opportunity() {
        let library = low_confidence_library();
        let store = ExperimentStore::new();
        let engine = ExplorationEngine::new(0.5, 3, 5);

        let created = engine.check_and_create_experiments(&library, &store);

        assert_eq!(created.len(), 1);
        assert_eq!(store.list().len(), 1);
        assert_eq!(created[0].status, ExperimentStatus::Running);
        assert!(created[0].tags.iter().any(|tag| tag == "task_type:fetch"));
    }

    #[test]
    fn skips_creation_when_matching_active_experiment_exists() {
        let library = low_confidence_library();
        let store = ExperimentStore::new();
        let engine = ExplorationEngine::new(0.5, 3, 5);

        let existing = Experiment::new("explore-fetch-any")
            .with_variant(Variant::control())
            .with_variant(Variant::new("treatment").with_traffic(0.5))
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        let mut existing = existing;
        existing.start();
        store.register(existing);

        let created = engine.check_and_create_experiments(&library, &store);

        assert!(created.is_empty());
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn concludes_mature_experiment_and_marks_completed() {
        let store = ExperimentStore::new();
        let engine = ExplorationEngine::new(0.5, 3, 1);

        let mut experiment = Experiment::new("exp-mature")
            .with_variant(Variant::control())
            .with_variant(Variant::new("treatment").with_traffic(0.5))
            .with_metric(MetricDefinition::success_rate())
            .with_min_samples(1)
            .build();
        experiment.start();
        store.register(experiment);

        store.record_outcome(
            "exp-mature",
            ExperimentOutcome::new("u1", "control")
                .with_metric("success_rate", 0.0)
                .with_success(false),
        );
        store.record_outcome(
            "exp-mature",
            ExperimentOutcome::new("u2", "treatment")
                .with_metric("success_rate", 1.0)
                .with_success(true),
        );

        let conclusions = engine.conclude_mature_experiments(&store);

        assert_eq!(conclusions.len(), 1);
        assert_eq!(conclusions[0].experiment_id, "exp-mature");
        assert_eq!(
            store.get("exp-mature").map(|exp| exp.status),
            Some(ExperimentStatus::Completed)
        );
    }

    #[test]
    fn respects_max_concurrent_limit() {
        let library = low_confidence_library();
        let store = ExperimentStore::new();
        let engine = ExplorationEngine::new(0.5, 1, 5);

        let mut existing = Experiment::new("already-running")
            .with_variant(Variant::control())
            .with_variant(Variant::new("treatment").with_traffic(0.5))
            .build();
        existing.start();
        store.register(existing);

        let created = engine.check_and_create_experiments(&library, &store);

        assert!(created.is_empty());
    }
}
