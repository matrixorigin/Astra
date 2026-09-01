use std::sync::Arc;

use astra_core::ErrorResponse;
use astra_services::{
    AdmittedModelExecution, ModelService,
    runs::{ResolvedModelSelection, RuntimeAuthRequest, RuntimeCapabilityDescriptorRequest},
};
use astra_turn_types::ModelSelection;
use axum::{Json, http::StatusCode};

use crate::{error_response_coded, internal_error};

/// Admit one Offering into the single execution-material contract consumed by
/// every agent and inference adapter.
///
/// An endpoint descriptor and its resolved identity are trusted provider
/// context injected before this boundary. Without one, the Offering is
/// materialized by the Server catalog. Both paths return the same
/// non-serializable value.
pub(crate) async fn admit_model_execution(
    model_service: &Arc<dyn ModelService>,
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
        return Ok(AdmittedModelExecution::from_endpoint(
            selection.offering_id.clone(),
            resolved.model_name.clone(),
            provider.to_string(),
            gateway.endpoint_url.clone(),
            authorization.to_string(),
            None,
            gateway
                .model_context_window
                .expect("validated model_gateway capability must carry a positive context window"),
        ));
    }

    if resolved.is_some() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "resolved_model_selection is trusted provider context and cannot select a Server route",
            "model_selection_invalid",
        ));
    }
    let offering = model_service
        .revalidate_model_offering(selection.offering_id.clone())
        .await?;
    AdmittedModelExecution::from_offering(offering).map_err(internal_error)
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
            Ok(ResolvedModelOffering {
                offering_id,
                model: ResolvedActiveLlmModel {
                    model_name: "server-model".into(),
                    wire_model_name: Some("wire-model".into()),
                    api_key: "server-secret".into(),
                    base_url: "https://models.example/v1".into(),
                    provider: "openai".into(),
                    fallback_chain: Vec::new(),
                    tags: Vec::new(),
                    request_body_overrides: None,
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
    async fn catalog_and_provider_context_materialize_the_same_execution_type() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        let catalog = admit_model_execution(
            &service,
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
    }

    #[tokio::test]
    async fn provider_context_requires_positive_model_context_window() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        for context_window in [None, Some(0)] {
            let error = admit_model_execution(
                &service,
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
