//! Execution profile — runtime configuration snapshot with scenario + experiment context.
//!
//! **Deprecated**: This module is superseded by `SelfModel` self-awareness.
//! Rather than pre-computing configuration profiles, the LLM receives runtime
//! state via the self-awareness prompt section and reasons about adaptation.
//!
//! An `ExecutionProfile` bundles a [`RuntimeConfig`] with the detected scenario,
//! active experiment variant, and pattern-library boost terms.

use serde::{Deserialize, Serialize};

use crate::runtime_config::RuntimeConfig;
use crate::user_profile::Scenario;

/// A runtime configuration snapshot enriched with adaptive context.
///
/// **Deprecated**: Superseded by `SelfModel` self-awareness reasoning.
#[deprecated(
    since = "0.9.0",
    note = "Superseded by SelfModel + LLM reasoning. See self_model.rs."
)]
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

    /// Whether a durable adaptive baseline was applied to this profile.
    #[serde(default)]
    pub baseline_applied: bool,
}

fn default_confidence() -> f64 {
    1.0
}

#[allow(deprecated)]
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
            baseline_applied: false,
        }
    }

    /// Apply scenario-specific strategy adjustments to the config.
    ///
    /// Maps [`ScenarioStrategy`] hints to concrete `RuntimeConfig` parameters:
    /// - `max_tools_per_turn` → `tool_selection.max_tools_per_turn` (execution limit per headless round)
    /// - `tool_budget_tokens` → `tool_selection.tool_budget_tokens` (controls actual tool selection budget)
    /// - `prefer_read_only` → `tool_selection.confidence_threshold` boost
    /// - Higher detail scenarios get more token budget for history
    /// - `memory_top_k` → `memory.retrieval_top_k` (if set by scenario)
    /// - `verification_strictness` → `verification.strictness` (if set by scenario)
    pub fn apply_scenario(&mut self, scenario: Scenario) {
        self.scenario = Some(scenario);
        let strategy = scenario.strategy_hints();

        // Wire scenario execution limit to the correct config field.
        // `max_tools_per_turn` controls how many tool calls execute per headless round;
        // `max_tools` (selection count) is left at its user/default value.
        self.config.tool_selection.max_tools_per_turn = strategy.max_tools_per_turn as u32;

        // Wire scenario tool budget into config so it reaches the tool selector.
        if strategy.tool_budget_tokens > 0 {
            self.config.tool_selection.tool_budget_tokens = strategy.tool_budget_tokens;
        }

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

        // Scenario-driven memory retrieval
        if let Some(top_k) = strategy.memory_top_k {
            self.config.memory.retrieval_top_k = top_k.clamp(
                self.config.memory_pressure.retrieval_min,
                self.config.memory_pressure.retrieval_max,
            );
        }

        // Scenario-driven verification strictness
        if let Some(strictness) = strategy.verification_strictness {
            self.config.verification.strictness = strictness.clamp(
                self.config.verification.min_strictness,
                self.config.verification.max_strictness,
            );
        }
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
#[allow(deprecated)]
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
        let orig_max_tools = profile.config.tool_selection.max_tools;

        profile.apply_scenario(Scenario::Debugging);

        assert_eq!(profile.scenario, Some(Scenario::Debugging));
        // max_tools (selection count) should remain at its default — scenarios don't touch it.
        assert_eq!(profile.config.tool_selection.max_tools, orig_max_tools);
        // max_tools_per_turn (execution limit) should be set from the scenario.
        assert_eq!(
            profile.config.tool_selection.effective_max_tools_per_turn(),
            15
        );
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
        let orig_max_tools = profile.config.tool_selection.max_tools;

        profile.apply_scenario(Scenario::CodeReview);

        // max_tools (selection count) should remain at default.
        assert_eq!(profile.config.tool_selection.max_tools, orig_max_tools);
        // max_tools_per_turn should be set from scenario.
        assert_eq!(
            profile.config.tool_selection.effective_max_tools_per_turn(),
            10
        );
        assert_eq!(
            profile.config.tool_selection.confidence_threshold,
            orig.max(0.35)
        );
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
            restored
                .config
                .tool_selection
                .effective_max_tools_per_turn(),
            profile.config.tool_selection.effective_max_tools_per_turn()
        );
    }

    #[test]
    fn apply_scenario_sets_memory_top_k_for_exploration() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        profile.apply_scenario(Scenario::Exploration);
        // Exploration has memory_top_k=10
        assert_eq!(profile.config.memory.retrieval_top_k, 10);
    }

    #[test]
    fn apply_scenario_sets_verification_for_code_review() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        profile.apply_scenario(Scenario::CodeReview);
        assert!((profile.config.verification.strictness - 0.7).abs() < 0.01);
    }

    #[test]
    fn apply_scenario_clamps_memory_top_k_to_config_bounds() {
        let mut config = RuntimeConfig::default();
        config.memory_pressure.retrieval_max = 8;
        let mut profile = ExecutionProfile::from_base(config);
        profile.apply_scenario(Scenario::Exploration);
        // Exploration wants 10, but max is 8
        assert_eq!(profile.config.memory.retrieval_top_k, 8);
    }

    #[test]
    fn apply_scenario_clamps_verification_to_config_bounds() {
        let mut config = RuntimeConfig::default();
        config.verification.max_strictness = 0.6;
        let mut profile = ExecutionProfile::from_base(config);
        profile.apply_scenario(Scenario::CodeReview);
        // CodeReview wants 0.7, but max is 0.6
        assert!((profile.config.verification.strictness - 0.6).abs() < 0.01);
    }

    #[test]
    fn apply_scenario_sets_tool_budget_tokens() {
        let mut profile = ExecutionProfile::from_base(RuntimeConfig::default());
        assert_eq!(profile.config.tool_selection.tool_budget_tokens, 0);

        profile.apply_scenario(Scenario::Implementation);
        assert_eq!(profile.config.tool_selection.tool_budget_tokens, 1200);

        let mut profile2 = ExecutionProfile::from_base(RuntimeConfig::default());
        profile2.apply_scenario(Scenario::Planning);
        assert_eq!(profile2.config.tool_selection.tool_budget_tokens, 600);
    }
}
