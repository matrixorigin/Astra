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

#[derive(Debug)]
pub(crate) struct PreparedModelAdmissionSlot {
    wire: ModelAdmissionSlotV1,
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
            astra_services::validate_model_offering_id(&wire.offering_id).map_err(|_| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "model_selection.offering_id is invalid",
                    "model_selection_invalid",
                )
            })?;
            let reasoning = serde_json::from_value::<ReasoningSelection>(wire.reasoning.clone())
                .map_err(|_| {
                    error_response_coded(
                        StatusCode::BAD_REQUEST,
                        "child reasoning selection is invalid",
                        "model_reasoning_invalid",
                    )
                })?;
            reasoning
                .config()
                .validate_output_budget(wire.max_output_tokens.map_or(u64::MAX, u64::from))
                .map_err(|error| {
                    error_response_coded(StatusCode::BAD_REQUEST, error, "model_reasoning_invalid")
                })?;
            Ok(PreparedModelAdmissionSlot { wire, reasoning })
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
        .admit_model_offerings(
            user_id,
            slots
                .iter()
                .map(|slot| slot.wire.offering_id.clone())
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
        if execution.offering_id != slot.wire.offering_id {
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
        validate_reasoning_control(&execution, &slot.reasoning.config()).map_err(|error| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                error,
                "model_reasoning_unsupported",
            )
        })?;
        if let Some(limit) = slot.wire.max_output_tokens {
            slot.reasoning
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
            offering_id: slot.wire.offering_id,
            reasoning: slot.wire.reasoning,
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
    use astra_core::model_wire::thinking::ThinkingProtocol;
    use astra_services::models::ThinkingCapability;
    use astra_turn_core::thinking_config::ThinkingConfig;

    let capability = execution.thinking_capability;
    let budget = crate::prompts::budget_for_model_with_metadata(
        Some(&execution.model_name),
        execution.context_window,
        execution.max_completion_tokens,
    );
    thinking.validate_output_budget(crate::prompts::capped_output_tokens(&budget) as u64)?;
    let protocol = execution.thinking_protocol.unwrap_or_default();
    let supported = match thinking {
        ThinkingConfig::ModelDefault => true,
        ThinkingConfig::Off => matches!(
            capability,
            Some(ThinkingCapability::Both | ThinkingCapability::None)
        ),
        ThinkingConfig::Enabled { budget_tokens } => {
            *budget_tokens >= 1024
                && capability == Some(ThinkingCapability::Both)
                && matches!(execution.provider.as_str(), "anthropic" | "bedrock")
        }
        ThinkingConfig::Adaptive { .. } => {
            matches!(
                capability,
                Some(ThinkingCapability::Both | ThinkingCapability::EffortOnly)
            ) && (matches!(execution.provider.as_str(), "anthropic" | "bedrock")
                || matches!(
                    protocol,
                    ThinkingProtocol::ReasoningEffort | ThinkingProtocol::ThinkingObject
                ))
        }
    };
    if supported {
        Ok(())
    } else {
        Err(format!(
            "Offering '{}' cannot execute requested reasoning control '{}' (capability: {}, protocol: {:?})",
            execution.offering_id,
            thinking,
            capability.map_or("unknown", ThinkingCapability::as_str),
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

    struct StaticModelService;

    fn unsupported<T>() -> Result<T, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "outside the admission test contract",
            "test_operation_unsupported",
        ))
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
                    model_name: "server-model".into(),
                    wire_model_name: Some("wire-model".into()),
                    api_key: "server-secret".into(),
                    base_url: "https://models.example/v1".into(),
                    provider: provider.into(),
                    fallback_chain: Vec::new(),
                    tags: Vec::new(),
                    request_body_overrides: None,
                    fixed_temperature: None,
                    thinking_protocol: None,
                    prompt_cache_capability: None,
                    thinking_capability: None,
                    context_window: Some(128_000),
                    max_completion_tokens: Some(16_384),
                    request_headers: Some(serde_json::Map::from_iter([(
                        "x-model-mode".into(),
                        Value::String("coding".into()),
                    )])),
                },
            })
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
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        let slot = |id: &str, reasoning: serde_json::Value| ModelAdmissionSlotV1 {
            max_output_tokens: None,
            offering_id: id.to_string(),
            reasoning,
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

    #[test]
    fn child_model_slot_shape_is_rejected_before_admission() {
        let error = prepare_child_model_slots(vec![
            ModelAdmissionSlotV1 {
                max_output_tokens: None,
                offering_id: "offer-a".into(),
                reasoning: serde_json::json!({"mode":"model_default"}),
            },
            ModelAdmissionSlotV1 {
                max_output_tokens: None,
                offering_id: "offer-b".into(),
                reasoning: serde_json::json!({"mode":"unknown"}),
            },
        ])
        .expect_err("the final malformed slot must fail in the pure pre-auth phase");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0.error_code.as_deref(),
            Some("model_reasoning_invalid")
        );
    }

    #[tokio::test]
    async fn explicit_offering_cannot_bypass_chat_purpose_but_remains_a_typed_judge() {
        use astra_core::model_wire::purpose::ModelRequestPurpose;
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
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
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
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
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
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
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
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
