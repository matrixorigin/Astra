use std::sync::Arc;

use astra_core::ErrorResponse;
use astra_server_types::{ModelAdmissionResponseV1, ModelAdmissionResultV1, ModelAdmissionSlotV1};
use astra_services::{
    AdmittedModelExecution, ModelService,
    runs::{ResolvedModelSelection, RuntimeAuthRequest, RuntimeCapabilityDescriptorRequest},
};
use astra_turn_types::ModelSelection;
use axum::{Json, http::StatusCode};

use crate::error_response_coded;

/// Resolve the baseline before any model-dependent prompt construction.
pub(crate) async fn admit_auto_model_request(
    service: &Arc<dyn ModelService>,
    user_id: &str,
    mut request: astra_services::runs::ChatRequestData,
    policy: Option<astra_turn_types::model_routing::AutoModelRoutingPolicy>,
) -> Result<astra_services::runs::ChatRequestData, (StatusCode, Json<ErrorResponse>)> {
    use astra_services::runs::{ModelSelectionMode, TurnIntentExecutionPolicy};
    if request.provider_runtime_authorized
        || request
            .capability_descriptors
            .as_ref()
            .is_some_and(|descriptors| descriptors.model_gateway.is_some())
        || request.model_selection_mode != ModelSelectionMode::ExplicitOffering
        || request.model_selection.is_some()
        || request.resolved_model_selection.is_some()
        || request.admitted_model_execution.is_some()
        || request.expected_model_name.is_some()
        || request.model.is_some()
        || request.work_binding.is_some()
        || request.execution_policy.turn_intent != TurnIntentExecutionPolicy::Auto
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "Auto requires a primary Server-catalog turn with no explicit model or provider model binding",
            "model_routing_unsupported",
        ));
    }
    let policy = policy.ok_or_else(|| {
        error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "Auto model routing has no configured policy",
            "model_routing_unavailable",
        )
    })?;
    if policy.revision.trim().is_empty()
        || policy.revision.len() > 128
        || policy.revision.trim() != policy.revision
        || policy.revision.chars().any(char::is_control)
        || policy.economy_offering_id == policy.strong_offering_id
        || astra_services::validate_model_offering_id(&policy.economy_offering_id).is_err()
        || astra_services::validate_model_offering_id(&policy.strong_offering_id).is_err()
    {
        return Err(error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "Auto model routing policy is invalid",
            "model_routing_unavailable",
        ));
    }
    let selection = ModelSelection {
        offering_id: policy.strong_offering_id.clone(),
    };
    let admitted = admit_model_execution(
        service,
        astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
        user_id,
        &selection,
        None,
        None,
        None,
    )
    .await?;
    if admitted.execution_placement != astra_services::ModelExecutionPlacement::Server {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "Auto requires Server-catalog model execution",
            "model_routing_unsupported",
        ));
    }
    request.model = Some(admitted.model_name.clone());
    request.resolved_model_selection = Some(ResolvedModelSelection {
        offering_id: admitted.offering_id.clone(),
        model_name: admitted.model_name.clone(),
        source_identity: admitted.source_identity.clone(),
    });
    request.model_selection = Some(selection);
    request.admitted_model_execution = Some(admitted);
    request.model_selection_mode = ModelSelectionMode::Auto(policy);
    Ok(request)
}

#[derive(Debug)]
pub(crate) struct PreparedModelAdmissionSlot {
    wire: ModelAdmissionSlotV1,
    reasoning: astra_turn_core::orchestration_spawn_tool::ReasoningSelection,
    inherited_reasoning: Option<PreparedInheritedReasoning>,
}

#[derive(Debug)]
struct PreparedInheritedReasoning {
    offering_id: String,
    reasoning: astra_turn_core::orchestration_spawn_tool::ReasoningSelection,
}

pub(crate) fn prepare_child_model_slots(
    slots: Vec<ModelAdmissionSlotV1>,
) -> Result<Vec<PreparedModelAdmissionSlot>, (StatusCode, Json<ErrorResponse>)> {
    use astra_turn_core::orchestration_spawn_tool::ReasoningSelection;
    slots
        .into_iter()
        .map(|wire| {
            if wire.max_output_tokens == Some(0) {
                return Err(error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "output-token limit must be positive",
                    "model_reasoning_invalid",
                ));
            }
            wire.selector.validate().map_err(|detail| {
                error_response_coded(StatusCode::BAD_REQUEST, detail, "model_selection_invalid")
            })?;
            let reasoning = serde_json::from_value::<ReasoningSelection>(wire.reasoning.clone())
                .map_err(|_| {
                    error_response_coded(
                        StatusCode::BAD_REQUEST,
                        "child reasoning selection is invalid",
                        "model_reasoning_invalid",
                    )
                })?;
            let inherited_reasoning = wire
                .inherited_reasoning
                .as_ref()
                .map(|inheritance| {
                    if !matches!(
                        wire.selector,
                        astra_turn_types::ModelSelector::ConfiguredName { .. }
                    ) {
                        return Err(error_response_coded(
                            StatusCode::BAD_REQUEST,
                            "conditional reasoning inheritance is valid only for configured-name selectors",
                            "model_reasoning_invalid",
                        ));
                    }
                    astra_services::validate_model_offering_id(&inheritance.offering_id).map_err(
                        |_| {
                            error_response_coded(
                                StatusCode::BAD_REQUEST,
                                "inherited reasoning Offering ID is invalid",
                                "model_selection_invalid",
                            )
                        },
                    )?;
                    let reasoning = serde_json::from_value::<ReasoningSelection>(
                        inheritance.reasoning.clone(),
                    )
                    .map_err(|_| {
                        error_response_coded(
                            StatusCode::BAD_REQUEST,
                            "inherited reasoning selection is invalid",
                            "model_reasoning_invalid",
                        )
                    })?;
                    Ok(PreparedInheritedReasoning {
                        offering_id: inheritance.offering_id.clone(),
                        reasoning,
                    })
                })
                .transpose()?;
            reasoning
                .config()
                .validate_output_budget(wire.max_output_tokens.map_or(u64::MAX, u64::from))
                .map_err(|error| {
                    error_response_coded(StatusCode::BAD_REQUEST, error, "model_reasoning_invalid")
                })?;
            Ok(PreparedModelAdmissionSlot {
                wire,
                reasoning,
                inherited_reasoning,
            })
        })
        .collect()
}

/// All-or-error child-model preflight. Execution material stays on Server;
/// only a safe display projection crosses back to CLI.
pub(crate) async fn admit_child_model_slots(
    model_service: &Arc<dyn ModelService>,
    user_id: String,
    slots: Vec<PreparedModelAdmissionSlot>,
) -> Result<ModelAdmissionResponseV1, (StatusCode, Json<ErrorResponse>)> {
    use astra_core::model_wire::purpose::ModelRequestPurpose;
    let executions = model_service
        .admit_model_selectors(
            user_id,
            slots
                .iter()
                .map(|slot| slot.wire.selector.clone())
                .collect(),
        )
        .await?;
    if executions.len() != slots.len() {
        return Err(error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "batch model admission returned incomplete results",
            "model_catalog_unavailable",
        ));
    }
    let mut admitted = Vec::with_capacity(slots.len());
    for (slot, execution) in slots.into_iter().zip(executions) {
        if matches!(
            &slot.wire.selector,
            astra_turn_types::ModelSelector::OfferingId { offering_id }
                if execution.offering_id != *offering_id
        ) {
            return Err(error_response_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "batch model admission returned mismatched Offering",
                "model_catalog_unavailable",
            ));
        }
        astra_services::models::validate_model_execution_purpose(
            &execution,
            ModelRequestPurpose::Chat,
        )?;
        let effective_reasoning = slot
            .inherited_reasoning
            .as_ref()
            .filter(|inherited| inherited.offering_id == execution.offering_id)
            .map_or(&slot.reasoning, |inherited| &inherited.reasoning);
        validate_reasoning_control(&execution, &effective_reasoning.config()).map_err(|error| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                error,
                "model_reasoning_unsupported",
            )
        })?;
        if let Some(limit) = slot.wire.max_output_tokens {
            effective_reasoning
                .config()
                .validate_output_budget(u64::from(limit))
                .map_err(|error| {
                    error_response_coded(
                        StatusCode::BAD_REQUEST,
                        error,
                        "model_reasoning_unsupported",
                    )
                })?;
        }
        admitted.push(ModelAdmissionResultV1 {
            max_output_tokens: slot.wire.max_output_tokens,
            offering_id: execution.offering_id.clone(),
            reasoning: serde_json::to_value(effective_reasoning).map_err(|error| {
                error_response_coded(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to encode admitted reasoning: {error}"),
                    "model_reasoning_invalid",
                )
            })?,
            model_name: execution.model_name,
            context_window: execution.context_window,
        });
    }
    Ok(ModelAdmissionResponseV1 { slots: admitted })
}

/// Validate one exact reasoning control against already-admitted execution
/// material. This is deliberately pure: callers perform it after Offering
/// admission and before dispatch, including ordinary chat, single spawn, and
/// future batch admission.
pub(crate) fn validate_reasoning_control(
    execution: &AdmittedModelExecution,
    thinking: &astra_turn_core::thinking_config::ThinkingConfig,
) -> Result<(), String> {
    let capability = execution.thinking_capability;
    let budget = crate::prompts::budget_for_model_with_metadata(
        Some(&execution.model_name),
        execution.context_window,
        execution.max_completion_tokens,
    );
    thinking.validate_output_budget(crate::prompts::capped_output_tokens(&budget) as u64)?;
    let protocol = execution.thinking_protocol.unwrap_or_default();
    let supported = thinking.is_supported_by(
        &execution.provider,
        capability.map(|capability| capability.as_str()),
        protocol,
    );
    if supported {
        Ok(())
    } else {
        Err(format!(
            "Offering '{}' cannot execute requested reasoning control '{}' (capability: {}, protocol: {:?}). If the task permits the Offering default instead of this explicit control, retry /chat with {{\"context\":{{\"thinking\":{{\"mode\":\"model_default\"}}}}}}; for agent/fanout use {{\"reasoning\":{{\"mode\":\"model_default\"}}}} to suppress parent reasoning inheritance. The requested control was not changed or executed.",
            execution.offering_id,
            thinking,
            capability.map_or("unknown", |capability| capability.as_str()),
            protocol
        ))
    }
}

/// Admit one Offering into the single execution-material contract consumed by
/// every agent and inference adapter.
///
/// An endpoint descriptor and its resolved identity are trusted provider
/// context injected before this boundary. Without one, the Offering is
/// materialized by the Server catalog. Both paths return the same
/// non-serializable value.
pub(crate) async fn admit_model_execution(
    model_service: &Arc<dyn ModelService>,
    purpose: astra_core::model_wire::purpose::ModelRequestPurpose,
    user_id: &str,
    selection: &ModelSelection,
    resolved: Option<&ResolvedModelSelection>,
    gateway: Option<&RuntimeCapabilityDescriptorRequest>,
    runtime_auth: Option<&RuntimeAuthRequest>,
) -> Result<AdmittedModelExecution, (StatusCode, Json<ErrorResponse>)> {
    astra_services::validate_model_offering_id(&selection.offering_id).map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "model_selection.offering_id is invalid",
            "model_selection_invalid",
        )
    })?;
    if let Some(gateway) = gateway {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor(
            gateway,
            "model_gateway",
        )?;
        let resolved = resolved.ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "provider model gateway requires a trusted resolved model identity",
                "provider_runtime_context_invalid",
            )
        })?;
        if resolved.offering_id != selection.offering_id
            || !is_exact_runtime_identity(&resolved.model_name)
        {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "provider model identity does not match model_selection.offering_id",
                "provider_runtime_context_invalid",
            ));
        }
        let authorization = runtime_auth
            .map(|auth| auth.authorization.as_str())
            .filter(|authorization| is_exact_runtime_identity(authorization))
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "runtime_auth.authorization is required for model execution",
                    "agent_binding_runtime_auth_missing",
                )
            })?;
        let provider = execution_provider_for_protocol(&gateway.protocol)?;
        let mut execution = AdmittedModelExecution::from_endpoint(
            selection.offering_id.clone(),
            resolved.model_name.clone(),
            provider.to_string(),
            gateway.endpoint_url.clone(),
            authorization.to_string(),
            None,
            gateway
                .model_context_window
                .expect("validated model_gateway capability must carry a positive context window"),
        );
        execution.source_identity = resolved.source_identity.clone();
        // This execution material exists only after the provider-authorized
        // model_gateway descriptor and runtime identity have both been
        // validated above. Negotiate MOI's safe structured error envelope at
        // that trust boundary; TUI and ordinary provider routes never receive
        // this header and retain the existing redaction behavior.
        execution.header_overrides.insert(
            astra_services::models::MOI_MODEL_GATEWAY_ERROR_CONTRACT_HEADER.to_string(),
            astra_services::models::MOI_MODEL_GATEWAY_ERROR_CONTRACT_V1.to_string(),
        );
        astra_services::models::validate_model_execution_purpose(&execution, purpose)?;
        return Ok(execution);
    }

    if resolved.is_some() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "resolved_model_selection is trusted provider context and cannot select a Server route",
            "model_selection_invalid",
        ));
    }
    let execution = model_service
        .admit_model_offering(user_id.to_string(), selection.offering_id.clone())
        .await?;
    astra_services::models::validate_model_execution_purpose(&execution, purpose)?;
    Ok(execution)
}

fn is_exact_runtime_identity(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

fn execution_provider_for_protocol(
    protocol: &str,
) -> Result<&'static str, (StatusCode, Json<ErrorResponse>)> {
    match protocol {
        "openai_chat_completions" => Ok("openai"),
        _ => Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("model execution protocol '{protocol}' is not supported"),
            "model_execution_protocol_unsupported",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{
        ModelCreateRequestData, ModelListItem, ModelRecord, ModelUpdateRequestData,
        ResolvedActiveLlmModel, ResolvedModelOffering,
    };
    use async_trait::async_trait;
    use serde_json::Value;

    struct StaticModelService {
        configured_name_offering_id: Option<String>,
        support_reasoning_inheritance: bool,
    }

    impl Default for StaticModelService {
        fn default() -> Self {
            Self {
                configured_name_offering_id: None,
                support_reasoning_inheritance: false,
            }
        }
    }

    impl StaticModelService {
        fn for_configured_name(offering_id: &str) -> Self {
            Self {
                configured_name_offering_id: Some(offering_id.to_string()),
                support_reasoning_inheritance: true,
            }
        }
    }

    fn unsupported<T>() -> Result<T, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "outside the admission test contract",
            "test_operation_unsupported",
        ))
    }

    #[test]
    fn explicit_off_requires_an_actual_disabling_contract_for_thinking_models() {
        use astra_core::model_wire::thinking::ThinkingProtocol;
        use astra_services::models::ThinkingCapability;
        use astra_turn_core::thinking_config::ThinkingConfig;

        let mut execution = AdmittedModelExecution::from_endpoint(
            "offering".into(),
            "model".into(),
            "openai".into(),
            "http://127.0.0.1:1/chat/completions".into(),
            "Bearer fixture".into(),
            None,
            128_000,
        );
        execution.thinking_capability = Some(ThinkingCapability::Both);
        let rejection = validate_reasoning_control(&execution, &ThinkingConfig::Off).unwrap_err();
        let chat_hint = rejection
            .split("retry /chat with ")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let mut chat_json: serde_json::Value = serde_json::from_str(chat_hint).unwrap();
        chat_json["message"] = serde_json::json!("retry");
        let chat: astra_server_types::ChatRequest = serde_json::from_value(chat_json).unwrap();
        let thinking: ThinkingConfig =
            serde_json::from_value(chat.context.unwrap()["thinking"].clone()).unwrap();
        assert!(validate_reasoning_control(&execution, &thinking).is_ok());
        let child_hint = rejection
            .split("for agent/fanout use ")
            .nth(1)
            .unwrap()
            .split(" to suppress")
            .next()
            .unwrap();
        let child_json: serde_json::Value = serde_json::from_str(child_hint).unwrap();
        let reasoning: astra_turn_core::orchestration_spawn_tool::ReasoningSelection =
            serde_json::from_value(child_json["reasoning"].clone()).unwrap();
        assert!(validate_reasoning_control(&execution, &reasoning.config()).is_ok());
        for protocol in [ThinkingProtocol::Unknown, ThinkingProtocol::ReasoningEffort] {
            execution.thinking_protocol = Some(protocol);
            assert!(validate_reasoning_control(&execution, &ThinkingConfig::Off).is_err());
            assert!(validate_reasoning_control(&execution, &ThinkingConfig::ModelDefault).is_ok());
        }
        for protocol in [
            ThinkingProtocol::EnableThinking,
            ThinkingProtocol::ThinkingObject,
            ThinkingProtocol::Moonshot,
        ] {
            execution.thinking_protocol = Some(protocol);
            assert!(validate_reasoning_control(&execution, &ThinkingConfig::Off).is_ok());
        }
        execution.thinking_protocol = Some(ThinkingProtocol::Unknown);
        for provider in ["anthropic", "bedrock"] {
            execution.provider = provider.into();
            assert!(validate_reasoning_control(&execution, &ThinkingConfig::Off).is_ok());
        }
        execution.provider = "openai".into();
        execution.thinking_capability = Some(ThinkingCapability::None);
        assert!(validate_reasoning_control(&execution, &ThinkingConfig::Off).is_ok());
        for capability in [
            None,
            Some(ThinkingCapability::EffortOnly),
            Some(ThinkingCapability::NativeOnly),
        ] {
            execution.thinking_capability = capability;
            assert!(validate_reasoning_control(&execution, &ThinkingConfig::Off).is_err());
        }
    }

    #[async_trait]
    impl ModelService for StaticModelService {
        async fn resolve_model_offering(
            &self,
            offering_id: String,
        ) -> Result<ResolvedModelOffering, (StatusCode, Json<ErrorResponse>)> {
            let provider = if offering_id == "offer-judgment" {
                "typesafe"
            } else {
                "openai"
            };
            Ok(ResolvedModelOffering {
                offering_id,
                model: ResolvedActiveLlmModel {
                    price_snapshot: None,
                    model_name: "server-model".into(),
                    wire_model_name: Some("wire-model".into()),
                    api_key: "server-secret".into(),
                    base_url: "https://models.example/v1".into(),
                    provider: provider.into(),
                    fallback_chain: Vec::new(),
                    tags: Vec::new(),
                    request_body_overrides: None,
                    fixed_temperature: None,
                    thinking_protocol: self.support_reasoning_inheritance.then_some(
                        astra_core::model_wire::thinking::ThinkingProtocol::ThinkingObject,
                    ),
                    prompt_cache_capability: None,
                    thinking_capability: self
                        .support_reasoning_inheritance
                        .then_some(astra_services::models::ThinkingCapability::Both),
                    context_window: Some(128_000),
                    max_completion_tokens: Some(16_384),
                    request_headers: Some(serde_json::Map::from_iter([(
                        "x-model-mode".into(),
                        Value::String("coding".into()),
                    )])),
                },
            })
        }

        async fn admit_model_selectors(
            &self,
            _user_id: String,
            selectors: Vec<astra_turn_types::ModelSelector>,
        ) -> Result<Vec<AdmittedModelExecution>, (StatusCode, Json<ErrorResponse>)> {
            let mut executions = Vec::with_capacity(selectors.len());
            for selector in selectors {
                let offering_id = match selector {
                    astra_turn_types::ModelSelector::OfferingId { offering_id } => offering_id,
                    astra_turn_types::ModelSelector::ConfiguredName { model_name, .. }
                        if model_name.eq_ignore_ascii_case("server-model") =>
                    {
                        self.configured_name_offering_id.clone().ok_or_else(|| {
                            error_response_coded(
                                StatusCode::NOT_FOUND,
                                "configured model is unavailable in this fixture",
                                "model_not_available",
                            )
                        })?
                    }
                    astra_turn_types::ModelSelector::ConfiguredName { .. } => {
                        return Err(error_response_coded(
                            StatusCode::NOT_FOUND,
                            "configured model is unavailable in this fixture",
                            "model_not_available",
                        ));
                    }
                };
                let offering = self.resolve_model_offering(offering_id).await?;
                executions.push(AdmittedModelExecution::from_offering(offering).map_err(
                    |error| {
                        error_response_coded(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            error,
                            "model_catalog_unavailable",
                        )
                    },
                )?);
            }
            Ok(executions)
        }

        async fn create_model(
            &self,
            _: String,
            _: ModelCreateRequestData,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported()
        }
        async fn list_models(
            &self,
            _: String,
            _: bool,
        ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
            unsupported()
        }
        async fn get_model(
            &self,
            _: String,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported()
        }
        async fn update_model(
            &self,
            _: String,
            _: ModelUpdateRequestData,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported()
        }
        async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unsupported()
        }
        async fn check_model(
            &self,
            _: String,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported()
        }
    }

    #[tokio::test]
    async fn child_model_batch_preflight_is_all_or_error_and_redacts_execution_material() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService::default());
        let slot = |id: &str, reasoning: serde_json::Value| ModelAdmissionSlotV1 {
            max_output_tokens: None,
            selector: astra_turn_types::ModelSelector::OfferingId {
                offering_id: id.to_string(),
            },
            reasoning,
            inherited_reasoning: None,
        };
        let admitted = admit_child_model_slots(
            &service,
            "user-a".into(),
            prepare_child_model_slots(vec![
                slot("offer-a", serde_json::json!({"mode":"model_default"})),
                slot("offer-b", serde_json::json!({"mode":"model_default"})),
            ])
            .unwrap(),
        )
        .await
        .expect("both slots admitted");
        assert_eq!(admitted.slots.len(), 2);
        assert_eq!(admitted.slots[1].offering_id, "offer-b");
        let wire = serde_json::to_string(&admitted).unwrap();
        assert!(!wire.contains("server-secret"));
        assert!(!wire.contains("models.example"));

        let error = admit_child_model_slots(
            &service,
            "user-a".into(),
            prepare_child_model_slots(vec![
                slot("offer-a", serde_json::json!({"mode":"model_default"})),
                slot("offer-b", serde_json::json!({"mode":"off"})),
            ])
            .unwrap(),
        )
        .await
        .expect_err("invalid final slot rejects the entire batch");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some("model_reasoning_unsupported")
        );
    }

    #[tokio::test]
    async fn configured_name_inherits_reasoning_only_for_the_exact_parent_offering() {
        let selector = astra_turn_types::ModelSelector::ConfiguredName {
            model_name: "server-model".into(),
            source: None,
        };
        let inherited = astra_server_types::ModelAdmissionReasoningInheritanceV1 {
            offering_id: "offer-parent".into(),
            reasoning: serde_json::json!({"mode":"adaptive","effort":"high"}),
        };
        let request_slot = |inherited_reasoning| ModelAdmissionSlotV1 {
            max_output_tokens: None,
            selector: selector.clone(),
            reasoning: serde_json::json!({"mode":"model_default"}),
            inherited_reasoning,
        };
        let parent_service: Arc<dyn ModelService> =
            Arc::new(StaticModelService::for_configured_name("offer-parent"));
        let inherited_result = admit_child_model_slots(
            &parent_service,
            "user-a".into(),
            prepare_child_model_slots(vec![request_slot(Some(inherited.clone()))]).unwrap(),
        )
        .await
        .expect("same Offering accepts the inherited reasoning control");
        assert_eq!(inherited_result.slots[0].offering_id, "offer-parent");
        assert_eq!(
            inherited_result.slots[0].reasoning, inherited.reasoning,
            "Server must return the effective inherited setting to CLI"
        );

        let different_service: Arc<dyn ModelService> =
            Arc::new(StaticModelService::for_configured_name("offer-other"));
        let different_result = admit_child_model_slots(
            &different_service,
            "user-a".into(),
            prepare_child_model_slots(vec![request_slot(Some(inherited))]).unwrap(),
        )
        .await
        .expect("a different Offering keeps its own model default");
        assert_eq!(different_result.slots[0].offering_id, "offer-other");
        assert_eq!(
            different_result.slots[0].reasoning,
            serde_json::json!({"mode":"model_default"})
        );
    }

    #[test]
    fn child_model_slot_shape_is_rejected_before_admission() {
        let error = prepare_child_model_slots(vec![
            ModelAdmissionSlotV1 {
                max_output_tokens: None,
                selector: astra_turn_types::ModelSelector::OfferingId {
                    offering_id: "offer-a".into(),
                },
                reasoning: serde_json::json!({"mode":"model_default"}),
                inherited_reasoning: None,
            },
            ModelAdmissionSlotV1 {
                max_output_tokens: None,
                selector: astra_turn_types::ModelSelector::OfferingId {
                    offering_id: "offer-b".into(),
                },
                reasoning: serde_json::json!({"mode":"unknown"}),
                inherited_reasoning: None,
            },
        ])
        .expect_err("the final malformed slot must fail in the pure pre-auth phase");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some("model_reasoning_invalid")
        );

        let error = prepare_child_model_slots(vec![ModelAdmissionSlotV1 {
            max_output_tokens: None,
            selector: astra_turn_types::ModelSelector::OfferingId {
                offering_id: "offer-a".into(),
            },
            reasoning: serde_json::json!({"mode":"model_default"}),
            inherited_reasoning: Some(astra_server_types::ModelAdmissionReasoningInheritanceV1 {
                offering_id: "offer-parent".into(),
                reasoning: serde_json::json!({"mode":"model_default"}),
            }),
        }])
        .expect_err("conditional inheritance cannot be attached to an exact Offering selector");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some("model_reasoning_invalid")
        );
    }

    #[tokio::test]
    async fn explicit_offering_cannot_bypass_chat_purpose_but_remains_a_typed_judge() {
        use astra_core::model_wire::purpose::ModelRequestPurpose;
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService::default());
        let selection = ModelSelection {
            offering_id: "offer-judgment".into(),
        };
        let error = admit_model_execution(
            &service,
            ModelRequestPurpose::Chat,
            "user-1",
            &selection,
            None,
            None,
            None,
        )
        .await
        .expect_err("a directly supplied judgment Offering must fail before dispatch");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some("model_purpose_unsupported")
        );
        let judgment = admit_model_execution(
            &service,
            ModelRequestPurpose::TypedJudgment,
            "user-1",
            &selection,
            None,
            None,
            None,
        )
        .await
        .expect("explicit typed judgments retain TypeSafe access");
        assert_eq!(judgment.provider, "typesafe");
        let ordinary = ModelSelection {
            offering_id: "offer-server".into(),
        };
        assert!(
            admit_model_execution(
                &service,
                ModelRequestPurpose::TypedJudgment,
                "user-1",
                &ordinary,
                None,
                None,
                None
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn catalog_and_provider_context_materialize_the_same_execution_type() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService::default());
        let catalog = admit_model_execution(
            &service,
            astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
            "user-1",
            &ModelSelection {
                offering_id: "offer-server".into(),
            },
            None,
            None,
            None,
        )
        .await
        .expect("catalog admission");
        assert_eq!(catalog.model_name, "server-model");
        assert_eq!(catalog.api_key, "server-secret");
        assert_eq!(catalog.context_window, Some(128_000));

        let endpoint = admit_model_execution(
            &service,
            astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
            "user-1",
            &ModelSelection {
                offering_id: "offer-edge".into(),
            },
            Some(&ResolvedModelSelection {
                offering_id: "offer-edge".into(),
                model_name: "edge-model".into(),
                source_identity: None,
            }),
            Some(&RuntimeCapabilityDescriptorRequest {
                id: "edge-model-endpoint".into(),
                descriptor_type: "model_gateway".into(),
                transport: "http".into(),
                endpoint_url: "http://127.0.0.1:8181/chat/completions".into(),
                protocol: "openai_chat_completions".into(),
                semantic_read: None,
                model_context_window: Some(128_000),
                metadata: serde_json::Map::new(),
            }),
            Some(&RuntimeAuthRequest {
                authorization: "Bearer endpoint-secret".into(),
            }),
        )
        .await
        .expect("provider admission");
        assert_eq!(endpoint.model_name, "edge-model");
        assert_eq!(endpoint.context_window, Some(128_000));
        assert_eq!(
            endpoint.completions_url_override.as_deref(),
            Some("http://127.0.0.1:8181/chat/completions")
        );
        assert_eq!(
            endpoint
                .header_overrides
                .get(astra_services::models::MOI_MODEL_GATEWAY_ERROR_CONTRACT_HEADER),
            Some(&astra_services::models::MOI_MODEL_GATEWAY_ERROR_CONTRACT_V1.to_string())
        );
    }

    #[tokio::test]
    async fn provider_context_requires_positive_model_context_window() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService::default());
        for context_window in [None, Some(0)] {
            let error = admit_model_execution(
                &service,
                astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
                "user-1",
                &ModelSelection {
                    offering_id: "offer-edge".into(),
                },
                Some(&ResolvedModelSelection {
                    offering_id: "offer-edge".into(),
                    model_name: "edge-model".into(),
                    source_identity: None,
                }),
                Some(&RuntimeCapabilityDescriptorRequest {
                    id: "edge-model-endpoint".into(),
                    descriptor_type: "model_gateway".into(),
                    transport: "http".into(),
                    endpoint_url: "http://127.0.0.1:8181/chat/completions".into(),
                    protocol: "openai_chat_completions".into(),
                    semantic_read: None,
                    model_context_window: context_window,
                    metadata: serde_json::Map::new(),
                }),
                Some(&RuntimeAuthRequest {
                    authorization: "Bearer endpoint-secret".into(),
                }),
            )
            .await
            .expect_err("missing or zero context capacity must fail closed");
            assert_eq!(
                error.1.error_code.as_deref(),
                Some("provider_runtime_context_invalid")
            );
        }
    }

    #[tokio::test]
    async fn provider_identity_drift_fails_closed() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService::default());
        let error = admit_model_execution(
            &service,
            astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
            "user-1",
            &ModelSelection {
                offering_id: "offer-requested".into(),
            },
            Some(&ResolvedModelSelection {
                offering_id: "offer-other".into(),
                model_name: "edge-model".into(),
                source_identity: None,
            }),
            Some(&RuntimeCapabilityDescriptorRequest {
                id: "edge-model-endpoint".into(),
                descriptor_type: "model_gateway".into(),
                transport: "http".into(),
                endpoint_url: "http://127.0.0.1:8181/chat/completions".into(),
                protocol: "openai_chat_completions".into(),
                semantic_read: None,
                model_context_window: Some(128_000),
                metadata: serde_json::Map::new(),
            }),
            Some(&RuntimeAuthRequest {
                authorization: "Bearer endpoint-secret".into(),
            }),
        )
        .await
        .expect_err("identity drift");
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("provider_runtime_context_invalid")
        );
    }
}

pub(crate) fn model_execution_contract_root(execution: &AdmittedModelExecution) -> String {
    use sha2::{Digest, Sha256};
    let contract = serde_json::json!({
        "offering": execution.offering_id, "model": execution.model_name,
        "wire_model": execution.wire_model_name, "provider": execution.provider,
        "access": execution.access_kind, "placement": execution.execution_placement,
        "context": execution.context_window, "output": execution.max_completion_tokens,
        "thinking": execution.thinking_capability, "protocol": execution.thinking_protocol,
        "temperature": execution.fixed_temperature, "cache": execution.cache_capability,
        "parameters": execution.request_body_overrides,
        "endpoint": execution.completions_url_override.as_deref().unwrap_or(&execution.base_url),
        "headers": execution.header_overrides.iter().filter(|(key, _)| !matches!(key.to_ascii_lowercase().as_str(), "authorization" | "x-api-key" | "api-key")).collect::<std::collections::BTreeMap<_, _>>(),
    });
    format!(
        "{:x}",
        Sha256::digest(astra_core::canonical_json_string(&contract).as_bytes())
    )
}

/// Process-local executors may have been built before Auto committed. Resolve
/// inheritance from the same immutable run fact rather than their old baseline.
pub(crate) async fn inherit_routed_execution<F, Fut>(
    engine: &crate::server::run::engine::RunEngine,
    user_id: &str,
    session_id: &str,
    parent_run_id: &str,
    inherited: Option<&AdmittedModelExecution>,
    resolve: F,
) -> Result<Option<AdmittedModelExecution>, String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<AdmittedModelExecution, String>>,
{
    use astra_services::model_routing::{DECISION_KEY, EVENT_TYPE, ModelRoutingDecision};
    let Some(event) = engine
        .load_run_event_by_idempotency_key(user_id, parent_run_id, EVENT_TYPE, DECISION_KEY)
        .await?
    else {
        return Ok(inherited.cloned());
    };
    let decision: ModelRoutingDecision = serde_json::from_value(event["data"].clone())
        .map_err(|_| "Invalid durable Auto routing decision".to_string())?;
    decision.validate_identity(parent_run_id, session_id)?;
    if decision
        .rollout
        .as_ref()
        .is_some_and(|r| r.cohort == astra_services::tuning::rollout::RolloutCohort::Treatment)
    {
        return Err(
            "Learned canary is limited to primary execution; child admission is unavailable".into(),
        );
    }
    let execution = resolve(decision.selected_offering_id).await?;
    if model_execution_contract_root(&execution) != decision.selected_contract_root {
        return Err("The inherited Auto model contract changed; start a new turn".into());
    }
    Ok(Some(execution))
}

#[cfg(test)]
#[path = "model_execution_admission_test_support.rs"]
pub(crate) mod inheritance_test_support;
