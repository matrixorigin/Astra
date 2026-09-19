use astra_turn_types::{
    LlmTransportConfig, ThinkingConfig,
    auxiliary_execution::{AUXILIARY_GENERATION_POLICY_VERSION, AuxiliaryGenerationPolicy},
    context_execution::ContextBudget,
    prompt_sections::StaticSections,
    summary_prompts::{SUMMARY_PROMPT_RENDERER_VERSION, SummaryPromptTemplates},
};
use services::{
    evaluation::{
        EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION, EVALUATION_RUNTIME_CONTRACT_VERSION,
        EvaluationExecutionConfig, FrozenRoundBudget, InstructionOnlyRuntimeConfig,
    },
    models::ModelExecutionProjection,
};
use std::{collections::BTreeMap, num::NonZeroUsize};

pub fn execution_config(model: &str, provider: &str, case_id: &str) -> EvaluationExecutionConfig {
    use astra_turn_types::{
        InferencePurpose,
        auxiliary_execution::{AuxiliaryPolicyProvenance, AuxiliaryTemperatureEmission},
        context_execution::{CompactConfig, CompactionTier},
    };
    EvaluationExecutionConfig {
        schema_version: EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION,
        runtime_contract_version: EVALUATION_RUNTIME_CONTRACT_VERSION,
        model: ModelExecutionProjection {
            schema_version: 1,
            offering_id: model.into(),
            model_name: model.into(),
            provider: provider.into(),
            access_kind: services::models::ModelAccessKind::SelfHosted,
            execution_placement: services::models::ModelExecutionPlacement::Server,
            wire_model_name: None,
            cache_capability: None,
            thinking_capability: None,
            fixed_temperature: None,
            thinking_protocol: None,
            context_window: Some(32_768),
            max_completion_tokens: Some(4_096),
            request_timeout_ms: Some(30_000),
            private_route_and_overrides_digest: "test-private-route-digest".into(),
        },
        transport: LlmTransportConfig {
            policy_version: astra_turn_types::LLM_TRANSPORT_POLICY_VERSION,
            connect_timeout_ms: 30_000,
            nonstream_timeout_ms: 120_000,
            total_budget_ms: 300_000,
            introspection_budget_ms: 8_000,
            stream_idle_ms: 120_000,
            stream_idle_after_progress_ms: 120_000,
            semantic_progress_ms: 120_000,
            retry_base_ms: 1_000,
            pool_max_idle_per_host: 4,
        },
        private_proxy_binding_digest: "test-private-proxy-digest".into(),
        primary_thinking: ThinkingConfig::Off,
        prompt_cache_enabled: true,
        pre_turn_compaction_gate: astra_turn_types::auxiliary_execution::AuxiliaryCallGate::Allowed,
        work_admission_gate: astra_turn_types::auxiliary_execution::WorkAdmissionGate::Allowed,
        context_budget: ContextBudget::resolve(
            Some(32_768),
            Some(4_096),
            0.75,
            4,
            4_000,
            CompactConfig {
                enable_summary: true,
                summary_token_budget: 2_048,
                max_ptl_retries: 2,
                summary_min_tier: CompactionTier::CompactHistory,
            },
        ),
        static_sections: StaticSections::test_default(),
        session_current_date: "2026-09-19".into(),
        summary_templates: SummaryPromptTemplates {
            renderer_version: SUMMARY_PROMPT_RENDERER_VERSION,
            standalone_system: "Summarize.".into(),
            standalone_user_prefix: "History:".into(),
            standalone_user_suffix: "Keep facts.".into(),
            inline_instruction: "Summarize history.".into(),
        },
        auxiliary_policies: vec![AuxiliaryGenerationPolicy {
            schema_version: AUXILIARY_GENERATION_POLICY_VERSION,
            operation_id: "pre_turn_compaction".into(),
            purpose: InferencePurpose::RequiredCompaction,
            thinking: ThinkingConfig::Off,
            temperature: AuxiliaryTemperatureEmission::ProviderDefault,
            temperature_provenance: AuxiliaryPolicyProvenance::ExistingPurposePolicy,
            configured_temperature: None,
            max_output_tokens: 4_096,
        }],
        runtime: InstructionOnlyRuntimeConfig {
            max_turn_input_tokens: 26_214,
            max_identical_tool_calls: 3,
            max_tools_per_turn: 10,
            repeated_cache_hit_suppression: 2,
            max_consecutive_empty_name: 2,
            round_budget_by_case: BTreeMap::from([(
                case_id.into(),
                FrozenRoundBudget {
                    initial_turns: 4,
                    extension_turns: 12,
                    hard_turn_limit: NonZeroUsize::new(12),
                },
            )]),
        },
    }
}
