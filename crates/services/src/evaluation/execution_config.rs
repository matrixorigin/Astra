//! Persisted projection of the exact inputs handed to Evaluation execution.
//! Runtime owns resolution and private keyed digests; this module never resolves defaults.
//!
//! Validation here checks persisted identity and shape, not transport construction or
//! runtime operation coverage. ExperimentSpec owns exact case-budget coverage.

use std::{collections::BTreeMap, num::NonZeroUsize};

use astra_turn_types::{
    LlmTransportConfig, ThinkingConfig,
    auxiliary_execution::{
        AUXILIARY_GENERATION_POLICY_VERSION, AuxiliaryGenerationPolicy,
        AuxiliaryTemperatureEmission,
    },
    context_execution::ContextBudget,
    prompt_sections::StaticSections,
    summary_prompts::{SUMMARY_PROMPT_RENDERER_VERSION, SummaryPromptTemplates},
};
use serde::{Deserialize, Serialize};

use crate::models::ModelExecutionProjection;

pub const EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION: u32 = 2;
/// Pins fixed prompt assembly, turn-budget derivation and output-cap retry rules.
pub const EVALUATION_RUNTIME_CONTRACT_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationExecutionConfig {
    pub schema_version: u32,
    pub runtime_contract_version: u32,
    pub model: ModelExecutionProjection,
    pub transport: LlmTransportConfig,
    pub private_proxy_binding_digest: String,
    pub primary_thinking: ThinkingConfig,
    pub prompt_cache_enabled: bool,
    pub pre_turn_compaction_gate: astra_turn_types::auxiliary_execution::AuxiliaryCallGate,
    pub work_admission_gate: astra_turn_types::auxiliary_execution::WorkAdmissionGate,
    pub context_budget: ContextBudget,
    pub static_sections: StaticSections,
    pub session_current_date: String,
    pub summary_templates: SummaryPromptTemplates,
    /// Canonically ordered by (operation_id, purpose.as_str()), with no duplicate keys.
    /// The runtime resolver additionally requires every operation it can execute.
    pub auxiliary_policies: Vec<AuxiliaryGenerationPolicy>,
    pub runtime: InstructionOnlyRuntimeConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionOnlyRuntimeConfig {
    pub max_turn_input_tokens: u64,
    pub max_identical_tool_calls: u32,
    pub max_tools_per_turn: u32,
    pub repeated_cache_hit_suppression: u32,
    pub max_consecutive_empty_name: u32,
    pub parallel_batching_force_streak: u32,
    pub cache_waste_midloop_threshold: u32,
    pub round_budget_by_case: BTreeMap<String, FrozenRoundBudget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRoundBudget {
    pub initial_turns: usize,
    pub extension_turns: usize,
    /// None means explicitly uncapped, not missing resolution.
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    pub hard_turn_limit: Option<NonZeroUsize>,
}

impl EvaluationExecutionConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION
            || self.runtime_contract_version != EVALUATION_RUNTIME_CONTRACT_VERSION
            || self.model.schema_version != 1
            || self.transport.policy_version != astra_turn_types::LLM_TRANSPORT_POLICY_VERSION
            || self.summary_templates.renderer_version != SUMMARY_PROMPT_RENDERER_VERSION
        {
            return Err("unsupported evaluation execution configuration version".into());
        }
        if self.model.offering_id.trim().is_empty()
            || self.model.model_name.trim().is_empty()
            || self.model.provider.trim().is_empty()
            || self
                .model
                .private_route_and_overrides_digest
                .trim()
                .is_empty()
            || self.private_proxy_binding_digest.trim().is_empty()
        {
            return Err(
                "evaluation execution configuration requires model and private routing identities"
                    .into(),
            );
        }
        let date = chrono::NaiveDate::parse_from_str(&self.session_current_date, "%Y-%m-%d")
            .map_err(|_| "evaluation session_current_date must be YYYY-MM-DD".to_string())?;
        if date.format("%Y-%m-%d").to_string() != self.session_current_date {
            return Err("evaluation session_current_date must be YYYY-MM-DD".into());
        }
        if self
            .model
            .fixed_temperature
            .is_some_and(|value| !value.is_finite())
            || !self.context_budget.compact_threshold.is_finite()
        {
            return Err("evaluation execution configuration contains non-finite values".into());
        }
        let mut previous = None;
        for policy in &self.auxiliary_policies {
            let key = (policy.operation_id.as_str(), policy.purpose.as_str());
            if policy.schema_version != AUXILIARY_GENERATION_POLICY_VERSION
                || policy.operation_id.trim().is_empty()
                || policy.max_output_tokens == 0
                || previous.is_some_and(|prior| prior >= key)
            {
                return Err("evaluation auxiliary policies require supported, ordered unique operation/purpose identities and output budgets".into());
            }
            if policy
                .configured_temperature
                .is_some_and(|value| !value.is_finite())
                || matches!(policy.temperature, AuxiliaryTemperatureEmission::Explicit(value) if !value.is_finite())
            {
                return Err("evaluation auxiliary policy contains non-finite temperature".into());
            }
            previous = Some(key);
        }
        for (case_id, budget) in &self.runtime.round_budget_by_case {
            if case_id.trim().is_empty()
                || budget.initial_turns == 0
                || budget.extension_turns == 0
                || budget
                    .hard_turn_limit
                    .is_some_and(|limit| budget.initial_turns > limit.get())
            {
                return Err("evaluation case round budget is invalid".into());
            }
        }
        if self.runtime.parallel_batching_force_streak == 0
            || self.runtime.cache_waste_midloop_threshold == 0
        {
            return Err("evaluation mid-loop guard thresholds must be positive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::evaluation::test_support::execution_config as fixture;

    #[test]
    fn round_budget_requires_explicit_hard_limit_or_null() {
        assert!(
            serde_json::from_value::<FrozenRoundBudget>(
                serde_json::json!({"initial_turns": 4, "extension_turns": 12})
            )
            .is_err()
        );
        let uncapped: FrozenRoundBudget = serde_json::from_value(
            serde_json::json!({"initial_turns": 4, "extension_turns": 12, "hard_turn_limit": null}),
        )
        .unwrap();
        assert_eq!(uncapped.hard_turn_limit, None);
        assert!(
            serde_json::from_value::<FrozenRoundBudget>(
                serde_json::json!({"initial_turns": 4, "hard_turn_limit": null})
            )
            .is_err()
        );
        let capped = FrozenRoundBudget {
            initial_turns: 4,
            extension_turns: 12,
            hard_turn_limit: NonZeroUsize::new(12),
        };
        assert_eq!(
            serde_json::from_value::<FrozenRoundBudget>(serde_json::to_value(&capped).unwrap())
                .unwrap(),
            capped
        );
    }

    #[test]
    fn execution_config_roundtrip_requires_resolved_fields() {
        let config = fixture("model", "openai", "case");
        config.validate().unwrap();
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(
            serde_json::from_value::<EvaluationExecutionConfig>(encoded.clone()).unwrap(),
            config
        );
        for field in [
            "pre_turn_compaction_gate",
            "work_admission_gate",
            "prompt_cache_enabled",
            "model",
            "transport",
            "context_budget",
            "static_sections",
            "session_current_date",
            "summary_templates",
            "auxiliary_policies",
            "runtime",
            "private_proxy_binding_digest",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<EvaluationExecutionConfig>(missing).is_err(),
                "missing {field}"
            );
        }
        let mut unknown = encoded;
        unknown["runtime_config"] = serde_json::json!({});
        assert!(serde_json::from_value::<EvaluationExecutionConfig>(unknown).is_err());
    }

    #[test]
    fn execution_config_rejects_unsupported_or_ambiguous_inputs() {
        let config = fixture("model", "openai", "case");
        let mut changed = config.clone();
        changed.runtime_contract_version += 1;
        assert!(changed.validate().is_err());
        let mut changed = config.clone();
        changed
            .auxiliary_policies
            .push(changed.auxiliary_policies[0].clone());
        assert!(changed.validate().is_err());
        let mut changed = config.clone();
        changed.context_budget.compact_threshold = f64::NAN;
        assert!(changed.validate().is_err());
        let mut changed = config.clone();
        changed
            .runtime
            .round_budget_by_case
            .get_mut("case")
            .unwrap()
            .initial_turns = 13;
        assert!(changed.validate().is_err());
        let mut changed = config;
        changed.session_current_date = "2026-02-30".into();
        assert!(changed.validate().is_err());
    }
}
