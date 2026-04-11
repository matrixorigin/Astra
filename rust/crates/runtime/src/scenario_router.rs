//! Scenario router — selects an [`ExecutionProfile`] per task based on detected
//! scenario, pattern-library suggestions, and active A/B experiments.
//!
//! The router is stateless: it reads from `PatternLibrary` and `ExperimentStore`
//! but does not mutate them. Mutation (recording outcomes, creating experiments)
//! is handled by the agentic loop and [`ExplorationEngine`].

use crate::ab_testing::{ExperimentStatus, ExperimentStore};
use crate::adaptive_baselines::AdaptiveBaselineStore;
use crate::execution_profile::ExecutionProfile;
use crate::pipeline::pattern::PatternLibrary;
use crate::pipeline::routing::RoutingDecision;
use crate::runtime_config::RuntimeConfig;
use crate::user_profile::{Scenario, ScenarioDetector};

/// Produces an [`ExecutionProfile`] for each task.
#[derive(Default)]
pub struct ScenarioRouter;

impl ScenarioRouter {
    pub fn new() -> Self {
        Self
    }

    /// Select an execution profile for the current task.
    ///
    /// Steps:
    /// 1. Start from base config
    /// 2. Detect scenario → apply strategy adjustments
    /// 3. Merge pattern-library boost terms
    /// 4. Check active experiments → assign variant if enrolled
    pub fn select(
        &self,
        base_config: &RuntimeConfig,
        routing: &RoutingDecision,
        detector: &ScenarioDetector,
        adaptive_baselines: Option<&AdaptiveBaselineStore>,
        pattern_library: Option<&PatternLibrary>,
        experiment_store: Option<&ExperimentStore>,
        user_id: &str,
    ) -> ExecutionProfile {
        let mut profile = ExecutionProfile::from_base(base_config.clone());

        // 1. Scenario detection
        if let Some((scenario, confidence)) = detector.detect() {
            profile.apply_scenario(scenario);
            profile.confidence = profile.confidence.min(confidence);
        } else if let Some(scenario) = scenario_from_task_type(routing.task_type) {
            profile.apply_scenario(scenario);
        }

        // 2. Apply promoted adaptive baselines for this task/domain.
        if let Some(store) = adaptive_baselines {
            if store
                .apply_to_config(routing.task_type, routing.domain_hint, &mut profile.config)
                .is_some()
            {
                profile.baseline_applied = true;
            }
        }

        // 3. Pattern library boost terms
        let mut boosts = routing.boost_terms.clone();
        if let Some(library) = pattern_library {
            boosts.extend(library.boost_terms_for(routing.task_type, routing.domain_hint));
        }
        if !boosts.is_empty() {
            boosts.sort();
            boosts.dedup();
            profile.merge_boosts(boosts);
        }

        // 4. Experiment variant assignment
        if let Some(store) = experiment_store {
            self.try_assign_experiment(&mut profile, store, user_id);
        }

        // 5. Fold routing confidence into profile
        profile.confidence = profile.confidence.min(routing.confidence);

        profile
    }

    /// Try to assign the user to an active experiment variant.
    fn try_assign_experiment(
        &self,
        profile: &mut ExecutionProfile,
        store: &ExperimentStore,
        user_id: &str,
    ) {
        let mut experiments = store.list();
        experiments.sort_by(|a, b| a.id.cmp(&b.id));
        for experiment in experiments {
            if experiment.status != ExperimentStatus::Running {
                continue;
            }
            if let Some(variant) = experiment.assign_variant(user_id) {
                profile.apply_variant(&experiment.id, variant);
                // Only enroll in one experiment at a time
                return;
            }
        }
    }
}

// ─── Convenience: scenario from routing decision ─────────────────────────────

/// Map a task type to a likely scenario (fallback when ScenarioDetector has
/// insufficient signal).
pub fn scenario_from_task_type(task_type: crate::pipeline::routing::TaskType) -> Option<Scenario> {
    use crate::pipeline::routing::TaskType;
    match task_type {
        TaskType::Code => Some(Scenario::Implementation),
        TaskType::Reasoning => Some(Scenario::Exploration),
        TaskType::Fetch => Some(Scenario::Exploration),
        TaskType::Mutate => Some(Scenario::Implementation),
        TaskType::Memory => None,
        TaskType::Conversational => None,
        TaskType::Compound => None,
        TaskType::Unknown => None,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab_testing::{Experiment, MetricDefinition, Variant};
    use crate::pipeline::routing::{TaskType, ToolFilter};
    use crate::tool_registry::state::ConversationState;
    use crate::turn::routing_metrics::{DisambiguationAction, IntentDisambiguation};

    fn dummy_routing(task_type: TaskType) -> RoutingDecision {
        RoutingDecision {
            conversation_state: ConversationState::default(),
            task_type,
            memory_hints: vec![],
            domain_hint: None,
            boost_terms: vec![],
            confidence: 0.9,
            tool_filter: ToolFilter::Wide,
            estimated_rounds: 1,
            disambiguation: IntentDisambiguation {
                primary_intent: "test".to_string(),
                secondary_intent: None,
                conflict_score: 0.0,
                recommendation: DisambiguationAction::Proceed,
            },
        }
    }

    #[test]
    fn select_baseline_no_scenario() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let routing = dummy_routing(TaskType::Unknown);

        let profile = router.select(&config, &routing, &detector, None, None, None, "user-1");

        assert!(profile.scenario.is_none());
        assert!(profile.boost_terms.is_empty());
        assert!(!profile.is_in_experiment());
    }

    #[test]
    fn select_falls_back_to_task_type_scenario() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let routing = dummy_routing(TaskType::Code);

        let profile = router.select(&config, &routing, &detector, None, None, None, "user-1");

        assert_eq!(profile.scenario, Some(Scenario::Implementation));
        assert_eq!(profile.config.tool_selection.max_tools, 4);
    }

    #[test]
    fn select_with_scenario_detection() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let mut detector = ScenarioDetector::new();
        // Feed debugging signals to the detector
        for _ in 0..5 {
            detector.observe_query("fix the bug in the code");
            detector.observe_tool("bash");
            detector.observe_tool("view");
        }
        let routing = dummy_routing(TaskType::Code);

        let profile = router.select(&config, &routing, &detector, None, None, None, "user-1");

        // Scenario should be detected (Debugging or Implementation depending on signals)
        if let Some(scenario) = profile.scenario {
            // Verify config was adjusted
            let strategy = scenario.strategy_hints();
            assert_eq!(
                profile.config.tool_selection.max_tools,
                strategy.max_tools_per_turn as u32
            );
        }
    }

    #[test]
    fn select_with_experiment_enrollment() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let routing = dummy_routing(TaskType::Code);

        let store = ExperimentStore::new();
        let mut exp = Experiment::new("exp-1")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("compression.max_history_tokens", serde_json::json!(25_000)),
            )
            .with_metric(MetricDefinition::success_rate())
            .with_min_samples(10)
            .build();
        exp.start();
        store.register(exp);

        let profile = router.select(
            &config,
            &routing,
            &detector,
            None,
            None,
            Some(&store),
            "user-1",
        );

        // User should be enrolled in some variant
        assert!(profile.is_in_experiment());
        assert_eq!(profile.experiment_id.as_deref(), Some("exp-1"));
    }

    #[test]
    fn select_skips_completed_experiments() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let routing = dummy_routing(TaskType::Code);

        let store = ExperimentStore::new();
        let mut exp = Experiment::new("exp-done")
            .with_variant(Variant::control())
            .with_variant(Variant::new("t1").with_traffic(0.5))
            .build();
        exp.start();
        exp.stop();
        store.register(exp);

        let profile = router.select(
            &config,
            &routing,
            &detector,
            None,
            None,
            Some(&store),
            "user-1",
        );
        assert!(!profile.is_in_experiment());
    }

    #[test]
    fn scenario_from_task_type_mappings() {
        assert_eq!(
            scenario_from_task_type(TaskType::Code),
            Some(Scenario::Implementation)
        );
        assert_eq!(
            scenario_from_task_type(TaskType::Fetch),
            Some(Scenario::Exploration)
        );
        assert_eq!(scenario_from_task_type(TaskType::Conversational), None);
    }

    #[test]
    fn select_folds_routing_confidence() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let mut routing = dummy_routing(TaskType::Code);
        routing.confidence = 0.4;

        let profile = router.select(&config, &routing, &detector, None, None, None, "user-1");
        // Profile confidence should be at most the routing confidence
        assert!(profile.confidence <= 0.4 + f64::EPSILON);
    }

    #[test]
    fn select_merges_routing_and_library_boosts() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let mut routing = dummy_routing(TaskType::Fetch);
        routing.boost_terms = vec!["view".into(), "grep".into()];

        let mut library = PatternLibrary::default();
        library.record_outcome(
            &["view".to_string(), "glob".to_string()],
            TaskType::Fetch,
            None,
            true,
            0.9,
            None,
        );
        library.record_outcome(
            &["view".to_string(), "glob".to_string()],
            TaskType::Fetch,
            None,
            true,
            0.8,
            None,
        );

        let profile = router.select(
            &config,
            &routing,
            &detector,
            None,
            Some(&library),
            None,
            "user-1",
        );

        assert!(profile.boost_terms.contains(&"view".to_string()));
        assert!(profile.boost_terms.contains(&"grep".to_string()));
        assert!(profile.boost_terms.contains(&"glob".to_string()));
    }

    #[test]
    fn select_applies_promoted_baseline() {
        let config = RuntimeConfig::default();
        let router = ScenarioRouter::new();
        let detector = ScenarioDetector::new();
        let routing = dummy_routing(TaskType::Fetch);
        let baselines = AdaptiveBaselineStore::new();
        let experiment = Experiment::new("exp-baseline")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("winner")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(9)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        baselines.promote_winner(&experiment, "winner").unwrap();

        let profile = router.select(
            &config,
            &routing,
            &detector,
            Some(&baselines),
            None,
            None,
            "user-1",
        );

        assert_eq!(profile.config.memory.retrieval_top_k, 9);
    }
}
