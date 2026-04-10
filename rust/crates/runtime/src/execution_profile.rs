//! Execution profile — runtime configuration snapshot with scenario + experiment context.
//!
//! An `ExecutionProfile` bundles a [`RuntimeConfig`] with the detected scenario,
//! active experiment variant, and pattern-library boost terms. It is produced by
//! [`ScenarioRouter`](crate::scenario_router::ScenarioRouter) before each turn and
//! consumed by the agentic loop to configure tool selection, token budgets, etc.

use serde::{Deserialize, Serialize};

use crate::runtime_config::RuntimeConfig;
use crate::user_profile::Scenario;

/// A runtime configuration snapshot enriched with adaptive context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionProfile {
    /// The tuned runtime configuration for this execution.
    pub config: RuntimeConfig,

    /// Detected work scenario (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<Scenario>,

    /// Active experiment variant ID (if enrolled in an A/B test).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,

    /// Active variant ID within the experiment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,

    /// Tool boost terms from pattern library suggestions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boost_terms: Vec<String>,

    /// Overall confidence in this profile selection (0.0–1.0).
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

fn default_confidence() -> f64 {
    1.0
}

impl ExecutionProfile {
    /// Create a baseline profile from the given config (no scenario, no experiment).
    pub fn from_base(config: RuntimeConfig) -> Self {
        Self {
            config,
            scenario: None,
            experiment_id: None,
            variant_id: None,
            boost_terms: Vec::new(),
            confidence: 1.0,
        }
    }

    /// Apply scenario-specific strategy adjustments to the config.
    ///
    /// Maps [`ScenarioStrategy`] hints to concrete `RuntimeConfig` parameters:
    /// - `max_tools_per_turn` → `tool_selection.max_tools`
    /// - `prefer_read_only` → `tool_selection.confidence_threshold` boost
    /// - Higher detail scenarios get more token budget for history
    pub fn apply_scenario(&mut self, scenario: Scenario) {
        self.scenario = Some(scenario);
        let strategy = scenario.strategy_hints();

        self.config.tool_selection.max_tools = strategy.max_tools_per_turn as u32;

        if strategy.prefer_read_only {
            self.config.tool_selection.confidence_threshold =
                self.config.tool_selection.confidence_threshold.max(0.35);
        }

        match strategy.detail_level {
            crate::user_profile::Verbosity::Debug => {
                self.config.compression.max_history_tokens =
                    self.config.compression.max_history_tokens.max(60_000);
                self.config.token_budget.max_turn_input_tokens =
                    self.config.token_budget.max_turn_input_tokens.max(100_000);
            }
            crate::user_profile::Verbosity::Verbose => {
                self.config.compression.max_history_tokens =
                    self.config.compression.max_history_tokens.max(50_000);
                self.config.token_budget.max_turn_input_tokens =
                    self.config.token_budget.max_turn_input_tokens.max(90_000);
            }
            _ => {}
        }
    }

    /// Apply an experiment variant's config overrides.
    pub fn apply_variant(&mut self, experiment_id: &str, variant: &crate::ab_testing::Variant) {
        self.experiment_id = Some(experiment_id.to_string());
        self.variant_id = Some(variant.id.clone());
        variant.apply_to_config(&mut self.config);
    }

    /// Store pattern library boost terms for tool selection.
    pub fn merge_boosts(&mut self, boost_terms: Vec<String>) {
        self.boost_terms = boost_terms;
    }

    /// Whether this profile has an active experiment enrollment.
    pub fn is_in_experiment(&self) -> bool {
        self.experiment_id.is_some()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_base_has_defaults() {
        let config = RuntimeConfig::default();
        let profile = ExecutionProfile::from_base(config.clone());
        assert!(profile.scenario.is_none());
        assert!(profile.experiment_id.is_none());
        assert!(profile.boost_terms.is_empty());
        assert_eq!(profile.confidence, 1.0);
        assert_eq!(
            profile.config.compression.max_history_tokens,
            config.compression.max_history_tokens
        );
    }

    #[test]
    fn apply_scenario_adjusts_config() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        let orig_threshold = profile.config.tool_selection.confidence_threshold;

        profile.apply_scenario(Scenario::Debugging);

        assert_eq!(profile.scenario, Some(Scenario::Debugging));
        assert_eq!(profile.config.tool_selection.max_tools, 5);
        // Debugging is not prefer_read_only, so threshold unchanged
        assert_eq!(
            profile.config.tool_selection.confidence_threshold,
            orig_threshold
        );
        assert!(profile.config.compression.max_history_tokens >= 60_000);
        assert!(profile.config.token_budget.max_turn_input_tokens >= 100_000);
    }

    #[test]
    fn apply_scenario_read_only_boosts_threshold() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        let orig = profile.config.tool_selection.confidence_threshold;

        profile.apply_scenario(Scenario::CodeReview);

        assert_eq!(profile.config.tool_selection.max_tools, 3);
        assert_eq!(
            profile.config.tool_selection.confidence_threshold,
            orig.max(0.35)
        );
    }

    #[test]
    fn apply_variant_sets_experiment_info() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());

        let mut variant = crate::ab_testing::Variant::new("treatment-1");
        variant.config_diff.insert(
            "compression.max_history_tokens".to_string(),
            serde_json::json!(30_000),
        );

        profile.apply_variant("exp-001", &variant);

        assert_eq!(profile.experiment_id.as_deref(), Some("exp-001"));
        assert_eq!(profile.variant_id.as_deref(), Some("treatment-1"));
        assert_eq!(profile.config.compression.max_history_tokens, 30_000);
        assert!(profile.is_in_experiment());
    }

    #[test]
    fn merge_boosts_stores_terms() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        profile.merge_boosts(vec!["grep".into(), "view".into()]);
        assert_eq!(profile.boost_terms, vec!["grep", "view"]);
    }

    #[test]
    fn serialization_round_trip() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        profile.apply_scenario(Scenario::Testing);
        profile.merge_boosts(vec!["bash".into()]);
        profile.confidence = 0.85;

        let json = serde_json::to_string(&profile).unwrap();
        let restored: ExecutionProfile = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.scenario, Some(Scenario::Testing));
        assert_eq!(restored.boost_terms, vec!["bash"]);
        assert_eq!(restored.confidence, 0.85);
        assert_eq!(
            restored.config.tool_selection.max_tools,
            profile.config.tool_selection.max_tools
        );
    }
}
