//! Resolve execution settings from captured admission inputs.
//!
//! Admission captures ambient configuration once. Frozen execution consumes those
//! values and revalidates current model, proxy and admission gates before use.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use crate::turn::llm::client::{LlmTransport, OwnedLlmExecutionRoute};
use astra_services::evaluation::{
    EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION, EVALUATION_RUNTIME_CONTRACT_VERSION,
    EvaluationExecutionConfig, FrozenRoundBudget, InstructionOnlyRuntimeConfig,
};
use astra_services::{AdmittedModelExecution, auth::FernetTokenEncryptor};
use astra_turn_types::InferencePurpose;
use astra_turn_types::auxiliary_execution::{AuxiliaryCallGate, WorkAdmissionGate};

/// Normal resolution and frozen Evaluation consumption are mutually exclusive.
#[derive(Clone, Debug)]
pub(crate) enum PreparedExecutionPolicy {
    Normal(Box<astra_config::runtime_config::RuntimeConfig>),
    Evaluation(Box<FrozenEvaluationInputs>),
}

/// Non-serializable authorized material paired with its persisted behavior identity.
#[derive(Clone, Debug)]
pub(crate) struct FrozenEvaluationInputs {
    pub config: Arc<EvaluationExecutionConfig>,
    pub transport: Arc<LlmTransport>,
    pub admitted: AdmittedModelExecution,
    pub case_budget: astra_turn_core::chat_turn_heuristics::AgenticTurnBudget,
}

/// Existing auxiliary operation limits, shared by admission and host consumers.
pub(crate) const SKILL_AUTO_ROUTE_MAX_OUTPUT_TOKENS: usize = 64;
pub(crate) const PRE_TURN_COMPACTION_MAX_OUTPUT_TOKENS: usize = 4_096;

fn proxy_binding_digest(
    proxy: &astra_core::net::ResolvedProxyConfig,
    encryptor: &FernetTokenEncryptor,
) -> Result<String, String> {
    proxy.private_binding_digest(|bytes| {
        let encoded =
            serde_json::to_string(bytes).map_err(|_| "cannot encode proxy binding".to_string())?;
        Ok(encryptor.keyed_digest("evaluation-proxy-binding.v1", &encoded))
    })
}

fn admitted_execution_route(admitted: &AdmittedModelExecution) -> OwnedLlmExecutionRoute {
    OwnedLlmExecutionRoute {
        model_name: admitted.model_name.clone(),
        wire_model_name: admitted.wire_model_name.clone(),
        api_key: admitted.api_key.clone(),
        base_url: admitted.base_url.clone(),
        provider: admitted.provider.clone(),
        thinking_capability: admitted.thinking_capability,
        fixed_temperature: admitted.fixed_temperature,
        thinking_protocol: admitted.thinking_protocol,
        header_overrides: admitted.header_overrides.clone(),
        request_body_overrides: admitted.request_body_overrides.clone(),
        completions_url_override: admitted.completions_url_override.clone(),
        request_timeout: admitted.request_timeout_ms.map(Duration::from_millis),
    }
}

fn validate_auxiliary_coverage(config: &EvaluationExecutionConfig) -> Result<(), String> {
    let expected = [
        ("pre_turn_compaction", InferencePurpose::RequiredCompaction),
        ("required_compaction", InferencePurpose::RequiredCompaction),
        ("skill_auto_route", InferencePurpose::Introspection),
        ("turn_intent", InferencePurpose::Introspection),
    ];
    if config.auxiliary_policies.len() != expected.len()
        || expected.iter().any(|(operation, purpose)| {
            !config
                .auxiliary_policies
                .iter()
                .any(|policy| policy.operation_id == *operation && policy.purpose == *purpose)
        })
    {
        return Err(
            "evaluation requires exactly the four admitted auxiliary operation policies".into(),
        );
    }
    Ok(())
}

pub(crate) fn resolve_context_budget(
    runtime: &astra_config::runtime_config::RuntimeConfig,
    context_window: Option<u32>,
    max_completion_tokens: Option<u32>,
    compact: astra_turn_types::context_execution::CompactConfig,
) -> astra_turn_types::context_execution::ContextBudget {
    astra_turn_types::context_execution::ContextBudget::resolve(
        context_window,
        max_completion_tokens,
        runtime.compression.compression_threshold,
        runtime.compression.preserve_recent_turns as usize,
        (runtime.memory.max_memory_tokens as usize).saturating_mul(4),
        compact,
    )
}

/// Process-local material captured during root Run preparation. Persisted
/// Evaluation identity must be projected from these exact consumer inputs.
#[derive(Clone, Debug)]
pub(crate) struct PreparedExecutionInputs {
    pub policy: PreparedExecutionPolicy,
    pub pre_turn_compaction_gate: AuxiliaryCallGate,
    pub work_admission_gate: WorkAdmissionGate,
    pub prompt_cache_enabled: bool,
    pub static_sections: std::sync::Arc<astra_turn_types::prompt_sections::StaticSections>,
    pub summary_templates: astra_turn_types::summary_prompts::SummaryPromptTemplates,
    pub session_current_date: String,
}

impl PreparedExecutionInputs {
    pub(crate) fn capture(
        runtime: astra_config::runtime_config::RuntimeConfig,
        user_id: &str,
        session_id: &str,
    ) -> Self {
        Self {
            policy: PreparedExecutionPolicy::Normal(Box::new(runtime)),
            prompt_cache_enabled: crate::turn::prompt_cache::PromptCacheConfig::capture_enablement(
            ),
            work_admission_gate: crate::server::server_loop_host::resolve_work_admission_gate(),
            pre_turn_compaction_gate:
                crate::server::server_loop_host::resolve_optional_auxiliary_call_gate(),
            static_sections: std::sync::Arc::new(crate::prompts::build_pipeline_static_sections()),
            summary_templates: astra_turn_core::cloud_summary::canonical_summary_prompt_templates(),
            session_current_date:
                crate::turn::session_current_date::resolve_session_current_date_for_user(
                    user_id, session_id,
                ),
        }
    }

    /// Freeze one case from this exact capture and the already-admitted model.
    /// This is the prepare boundary; downstream consumers never call its resolvers.
    pub(crate) fn freeze_for_prepare(
        admitted: &AdmittedModelExecution,
        encryptor: &FernetTokenEncryptor,
        case_id: &str,
        case_message: &str,
    ) -> Result<EvaluationExecutionConfig, String> {
        // Experiment preparation precedes trial sessions. Capture one shared
        // date here; start/recovery must never replace it with a trial date.
        let captured = Self::capture(astra_config::RuntimeConfig::load(), "", "");
        captured.freeze_captured_evaluation(admitted, encryptor, case_id, case_message)
    }

    fn freeze_captured_evaluation(
        &self,
        admitted: &AdmittedModelExecution,
        encryptor: &FernetTokenEncryptor,
        case_id: &str,
        case_message: &str,
    ) -> Result<EvaluationExecutionConfig, String> {
        let PreparedExecutionPolicy::Normal(runtime) = &self.policy else {
            return Err("cannot re-resolve an already frozen evaluation".into());
        };
        if case_id.trim().is_empty() || case_message.trim().is_empty() {
            return Err("evaluation freeze requires a case identity and message".into());
        }
        let model = admitted.freeze_projection(encryptor)?;
        let proxy = astra_core::net::ResolvedProxyConfig::capture();
        let private_proxy_binding_digest = proxy_binding_digest(&proxy, encryptor)?;
        let transport =
            LlmTransport::build(crate::turn::llm::client::capture_transport_config(), proxy)?;
        let context_budget = resolve_context_budget(
            runtime,
            admitted.context_window,
            admitted.max_completion_tokens,
            astra_turn_types::context_execution::CompactConfig::default(),
        );
        let primary_thinking =
            astra_turn_core::thinking_config::resolve_model_thinking(&admitted.model_name).1;
        let route = admitted_execution_route(admitted);
        let mut auxiliary_policies = Vec::new();
        for (operation, purpose, output) in [
            (
                "turn_intent",
                InferencePurpose::Introspection,
                astra_services::WORK_ADMISSION_MAX_OUTPUT_TOKENS,
            ),
            (
                "skill_auto_route",
                InferencePurpose::Introspection,
                SKILL_AUTO_ROUTE_MAX_OUTPUT_TOKENS,
            ),
            (
                "required_compaction",
                InferencePurpose::RequiredCompaction,
                context_budget.compact_config.summary_token_budget,
            ),
            (
                "pre_turn_compaction",
                InferencePurpose::RequiredCompaction,
                PRE_TURN_COMPACTION_MAX_OUTPUT_TOKENS,
            ),
        ] {
            auxiliary_policies.push(
                crate::turn::llm::summary_client::resolve_auxiliary_generation_policy(
                    operation, output, purpose, &route,
                )?,
            );
        }
        auxiliary_policies.sort_by(|a, b| {
            (a.operation_id.as_str(), a.purpose.as_str())
                .cmp(&(b.operation_id.as_str(), b.purpose.as_str()))
        });
        let budget = astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
                case_message.trim(),
            ),
            runtime.runtime_limits.resolve_turn_ceiling(false)?,
            None,
        );
        let tools = runtime
            .tool_selection
            .resolve_for_model(Some(&admitted.model_name));
        let config = EvaluationExecutionConfig {
            schema_version: EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION,
            runtime_contract_version: EVALUATION_RUNTIME_CONTRACT_VERSION,
            model,
            transport: transport.config().clone(),
            private_proxy_binding_digest,
            primary_thinking,
            context_budget,
            static_sections: self.static_sections.as_ref().clone(),
            session_current_date: self.session_current_date.clone(),
            summary_templates: self.summary_templates.clone(),
            pre_turn_compaction_gate: self.pre_turn_compaction_gate,
            work_admission_gate: self.work_admission_gate,
            prompt_cache_enabled: self.prompt_cache_enabled,
            auxiliary_policies,
            runtime: InstructionOnlyRuntimeConfig {
                max_turn_input_tokens: astra_core::RuntimeLimits::global()
                    .effective_max_turn_input_tokens_with_context_window(
                        Some(&admitted.model_name),
                        admitted.context_window,
                    ),
                max_identical_tool_calls: tools.max_identical_tool_calls,
                max_tools_per_turn: tools.max_tools_per_turn,
                repeated_cache_hit_suppression: tools.repeated_cache_hit_suppression,
                max_consecutive_empty_name: tools.max_consecutive_empty_name,
                round_budget_by_case: BTreeMap::from([(
                    case_id.to_string(),
                    FrozenRoundBudget {
                        initial_turns: budget.initial_turns,
                        hard_turn_limit: budget.hard_turn_limit,
                        extension_turns: budget.extension_turns,
                    },
                )]),
            },
        };
        config.validate()?;
        validate_auxiliary_coverage(&config)?;
        Ok(config)
    }

    /// Rebind current authorized private material to a persisted behavior snapshot.
    /// No runtime config, date, prompt, thinking, budget or auxiliary resolver runs.
    pub(crate) fn from_frozen(
        frozen: &EvaluationExecutionConfig,
        admitted: &AdmittedModelExecution,
        encryptor: &FernetTokenEncryptor,
        case_id: &str,
    ) -> Result<Self, String> {
        frozen.validate()?;
        validate_auxiliary_coverage(frozen)?;
        let case_budget = frozen
            .runtime
            .round_budget_by_case
            .get(case_id)
            .ok_or_else(|| "evaluation freeze does not contain the trial case".to_string())?;
        let case_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: case_budget.initial_turns,
            hard_turn_limit: case_budget.hard_turn_limit,
            extension_turns: case_budget.extension_turns,
        };
        if admitted.freeze_projection(encryptor)? != frozen.model {
            return Err("current admitted model differs from the evaluation freeze".into());
        }
        let proxy = astra_core::net::ResolvedProxyConfig::capture();
        if proxy_binding_digest(&proxy, encryptor)? != frozen.private_proxy_binding_digest {
            return Err("current proxy binding differs from the evaluation freeze".into());
        }
        if crate::server::server_loop_host::resolve_optional_auxiliary_call_gate()
            != frozen.pre_turn_compaction_gate
        {
            return Err("current auxiliary call gate differs from the evaluation freeze".into());
        }
        if crate::server::server_loop_host::resolve_work_admission_gate()
            != frozen.work_admission_gate
        {
            return Err("current Work admission gate differs from the evaluation freeze".into());
        }
        let transport = Arc::new(LlmTransport::build(frozen.transport.clone(), proxy)?);
        Ok(Self {
            policy: PreparedExecutionPolicy::Evaluation(Box::new(FrozenEvaluationInputs {
                config: Arc::new(frozen.clone()),
                transport,
                admitted: admitted.clone(),
                case_budget,
            })),
            static_sections: Arc::new(frozen.static_sections.clone()),
            summary_templates: frozen.summary_templates.clone(),
            session_current_date: frozen.session_current_date.clone(),
            pre_turn_compaction_gate: frozen.pre_turn_compaction_gate,
            work_admission_gate: frozen.work_admission_gate,
            prompt_cache_enabled: frozen.prompt_cache_enabled,
        })
    }

    pub(crate) fn new_pipeline_session(
        &self,
    ) -> astra_turn_core::pipeline_session::PipelineSession {
        let mut session = astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
            self.session_current_date.clone(),
        );
        session.static_sections_or_init(|| self.static_sections.as_ref().clone());
        session
    }
}

#[cfg(test)]
mod tests {
    use super::PreparedExecutionInputs;
    use astra_services::session_journal::{
        JournalDirGuard, JournalEvent, JournalWriter, journal_file_path_for_user,
    };
    use astra_turn_types::prompt_sections::{CacheScope, PromptTokenBucket};

    fn evaluation_fixture() -> (
        astra_services::evaluation::EvaluationExecutionConfig,
        astra_services::AdmittedModelExecution,
        astra_services::auth::FernetTokenEncryptor,
    ) {
        let mut runtime = astra_config::RuntimeConfig::default();
        runtime.runtime_limits.max_turns = 7;
        runtime.tool_selection.max_identical_tool_calls = 7;
        runtime.compression.compression_threshold = 0.6;
        let mut captured = PreparedExecutionInputs::capture(runtime, "freeze-owner", "");
        captured.session_current_date = "1999-12-31".into();
        std::sync::Arc::make_mut(&mut captured.static_sections)
            .core_rules
            .text = "frozen rules".into();
        let mut admitted = astra_services::AdmittedModelExecution::from_endpoint(
            "offering".into(),
            "model".into(),
            "openai".into(),
            "https://example.invalid/v1/chat/completions".into(),
            "Bearer original".into(),
            Some(30_000),
            128_000,
        );
        admitted.max_completion_tokens = Some(8_000);
        let encryptor = astra_services::auth::FernetTokenEncryptor::new("test-freeze-key").unwrap();
        let config = captured
            .freeze_captured_evaluation(
                &admitted,
                &encryptor,
                "case",
                "review the current implementation",
            )
            .unwrap();
        (config, admitted, encryptor)
    }

    #[test]
    #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
    fn frozen_material_accepts_credential_rotation_and_preserves_exact_inputs() {
        let (mut config, mut admitted, encryptor) = evaluation_fixture();
        // Frozen transport knobs must be used rather than captured again at start.
        config.transport.retry_base_ms = 17;
        // Both explicit cache decisions are valid frozen inputs, regardless of
        // the process environment. No ambient-equality check belongs at start.
        for enabled in [false, true] {
            config.prompt_cache_enabled = enabled;
            let inputs =
                PreparedExecutionInputs::from_frozen(&config, &admitted, &encryptor, "case")
                    .unwrap();
            assert_eq!(inputs.prompt_cache_enabled, enabled);
        }
        config
            .runtime
            .round_budget_by_case
            .get_mut("case")
            .unwrap()
            .extension_turns = 11;
        admitted
            .header_overrides
            .insert("authorization".into(), "Bearer rotated".into());
        let inputs =
            PreparedExecutionInputs::from_frozen(&config, &admitted, &encryptor, "case").unwrap();
        let super::PreparedExecutionPolicy::Evaluation(material) = &inputs.policy else {
            panic!("frozen branch must never construct a normal runtime config")
        };
        assert_eq!(material.config.as_ref(), &config);
        assert_eq!(material.transport.config(), &config.transport);
        assert_eq!(material.admitted, admitted);
        assert_eq!(
            material.case_budget,
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
                initial_turns: config.runtime.round_budget_by_case["case"].initial_turns,
                hard_turn_limit: config.runtime.round_budget_by_case["case"].hard_turn_limit,
                extension_turns: 11,
            }
        );
        assert_eq!(material.config.runtime.max_identical_tool_calls, 7);
        assert_eq!(material.config.context_budget.compact_threshold, 0.6);
        assert_eq!(inputs.session_current_date, "1999-12-31");
        assert_eq!(inputs.static_sections.core_rules.text, "frozen rules");
        assert_eq!(inputs.summary_templates, config.summary_templates);
        assert_eq!(inputs.work_admission_gate, config.work_admission_gate);
        assert_eq!(inputs.prompt_cache_enabled, config.prompt_cache_enabled);
        assert_eq!(
            inputs.pre_turn_compaction_gate,
            config.pre_turn_compaction_gate
        );
        let policies = &material.config.auxiliary_policies;
        assert_eq!(
            policies
                .iter()
                .map(|p| (p.operation_id.as_str(), p.max_output_tokens))
                .collect::<Vec<_>>(),
            vec![
                ("pre_turn_compaction", 4_096),
                (
                    "required_compaction",
                    astra_turn_types::context_execution::CompactConfig::default()
                        .summary_token_budget
                ),
                ("skill_auto_route", 64),
                (
                    "turn_intent",
                    astra_services::WORK_ADMISSION_MAX_OUTPUT_TOKENS
                )
            ]
        );
        let encoded = serde_json::to_string(&config).unwrap();
        for private in [
            "Bearer original",
            "Bearer rotated",
            "https://example.invalid",
        ] {
            assert!(!encoded.contains(private));
        }
    }

    #[test]
    #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
    fn frozen_material_rejects_behavior_proxy_gate_case_and_policy_drift() {
        let (config, admitted, encryptor) = evaluation_fixture();
        let mut changed = admitted.clone();
        changed.max_completion_tokens = Some(16_000);
        assert!(
            PreparedExecutionInputs::from_frozen(&config, &changed, &encryptor, "case").is_err()
        );
        assert!(
            PreparedExecutionInputs::from_frozen(&config, &admitted, &encryptor, "missing-case")
                .is_err()
        );
        let mut changed = config.clone();
        changed.private_proxy_binding_digest = "wrong-binding".into();
        assert!(
            PreparedExecutionInputs::from_frozen(&changed, &admitted, &encryptor, "case").is_err()
        );
        let mut changed = config.clone();
        changed.auxiliary_policies.pop();
        assert!(
            PreparedExecutionInputs::from_frozen(&changed, &admitted, &encryptor, "case").is_err()
        );
        let mut changed = config.clone();
        changed.pre_turn_compaction_gate = match config.pre_turn_compaction_gate {
            super::AuxiliaryCallGate::Allowed => super::AuxiliaryCallGate::Disabled,
            _ => super::AuxiliaryCallGate::Allowed,
        };
        assert!(
            PreparedExecutionInputs::from_frozen(&changed, &admitted, &encryptor, "case").is_err()
        );
        let mut changed = config.clone();
        changed.work_admission_gate = match config.work_admission_gate {
            super::WorkAdmissionGate::Allowed => super::WorkAdmissionGate::Disabled,
            _ => super::WorkAdmissionGate::Allowed,
        };
        let error = PreparedExecutionInputs::from_frozen(&changed, &admitted, &encryptor, "case")
            .expect_err("Work gate drift must fail independently of optional compaction gate");
        assert!(error.contains("Work admission gate"), "{error}");
    }

    #[test]
    fn captured_date_and_sections_survive_owner_journal_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let user = "captured-input-owner";
        let session_id = "00000000-0000-0000-0000-000000000193";
        let writer = JournalWriter::for_user(user, session_id).unwrap();
        let mut event = JournalEvent::session_start(Some(session_id), None);
        event.ts = "2026-05-24T23:59:50Z".into();
        writer.append(&event).unwrap();
        drop(writer);

        let captured = PreparedExecutionInputs::capture(
            astra_config::runtime_config::RuntimeConfig::default(),
            user,
            session_id,
        );
        assert_eq!(captured.session_current_date, "2026-05-24");
        let expected_sections = captured.static_sections.as_ref().clone();

        // Replacing the first event makes a fresh resolver observably disagree;
        // merely appending an event would still return the original anchor.
        event.ts = "2026-05-26T00:10:00Z".into();
        std::fs::write(
            journal_file_path_for_user(user, session_id).unwrap(),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        assert_eq!(
            crate::turn::session_current_date::resolve_session_current_date_for_user(
                user, session_id,
            ),
            "2026-05-26"
        );

        // Each reconstruction must consume the capture, not reread the journal.
        for _ in 0..2 {
            let mut session = captured.new_pipeline_session();
            assert_eq!(session.current_date(), "2026-05-24");
            let sections = session.static_sections_or_init(|| {
                panic!("captured sections must already populate the pipeline cache")
            });
            assert_eq!(sections.as_ref(), &expected_sections);
        }
        assert_eq!(captured.session_current_date, "2026-05-24");
    }

    #[test]
    fn supplied_capture_populates_pipeline_after_override_source_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path().join("sessions"));
        let mut captured = PreparedExecutionInputs::capture(
            astra_config::runtime_config::RuntimeConfig::default(),
            "override-owner",
            "00000000-0000-0000-0000-000000000194",
        );
        let overrides_dir = temp.path().join("prompts");
        std::fs::create_dir(&overrides_dir).unwrap();
        let override_path = overrides_dir.join("core_rules.txt");
        let exact_text = "captured rules\n保留原文及空白  \n";
        std::fs::write(&override_path, exact_text).unwrap();
        let overrides = crate::prompts::load_overrides(&overrides_dir);
        let section = &mut std::sync::Arc::make_mut(&mut captured.static_sections).core_rules;
        section.text = overrides["core_rules"].clone();
        section.scope = CacheScope::Session;
        section.token_bucket = PromptTokenBucket::UserPreferences;
        section.trace_signals.context_signals.system_prompt_override = true;
        captured.session_current_date = "1999-12-31".into();
        let expected_sections = captured.static_sections.as_ref().clone();
        std::fs::remove_file(override_path).unwrap();
        assert!(crate::prompts::load_overrides(&overrides_dir).is_empty());

        let mut session = captured.new_pipeline_session();
        assert_eq!(session.current_date(), "1999-12-31");
        let sections = session.static_sections_or_init(|| {
            panic!("supplied capture must prepopulate sections without loading overrides")
        });
        assert_eq!(sections.core_rules.text, exact_text);
        assert_eq!(sections.as_ref(), &expected_sections);
    }
}
