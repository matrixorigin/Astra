use std::sync::Arc;

use astra_core::ErrorResponse;
use astra_services::{
    AdmittedModelExecution, ModelService,
    runs::{
        ResolvedModelSelection, RuntimeAuthRequest, RuntimeCapabilityDescriptorRequest,
        RuntimeCapabilityDescriptorsRequest,
    },
};
use astra_turn_types::ModelSelection;
use axum::{Json, body::Bytes, http::StatusCode};
use serde::{Deserialize, Deserializer, de::IgnoredAny};

use crate::{error_response_coded, internal_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelExecutionAdmissionAuthority {
    Catalog,
    ProviderRuntime,
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

/// Parse only the model-admission fields from a chat payload and materialize
/// the one execution contract used by the bridge and durable run paths.
///
/// The surrounding chat payload deliberately remains owned by its transport
/// adapter. This boundary neither persists credentials nor rewrites the
/// prompt-facing body.
pub(crate) async fn admit_model_execution_from_body(
    model_service: &Arc<dyn ModelService>,
    body: &Bytes,
    authority: ModelExecutionAdmissionAuthority,
) -> Result<AdmittedModelExecution, (StatusCode, Json<ErrorResponse>)> {
    let fields: ModelExecutionAdmissionFields = serde_json::from_slice(body).map_err(|error| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("invalid chat request JSON: {error}"),
            "chat_request_invalid",
        )
    })?;
    if let Some(field) = fields.direct_execution_field() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!(
                "client field `{field}` cannot select an execution endpoint, credential, or placement"
            ),
            "client_execution_override_forbidden",
        ));
    }
    let selection = fields.model_selection.ok_or_else(|| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "model_selection is required",
            "model_selection_missing",
        )
    })?;

    let gateway = fields
        .capability_descriptors
        .as_ref()
        .and_then(|descriptors| descriptors.model_gateway.as_ref());
    match authority {
        ModelExecutionAdmissionAuthority::Catalog => {
            if fields.resolved_model_selection.is_some()
                || fields.capability_descriptors.is_some()
                || fields.runtime_auth.is_some()
            {
                return Err(error_response_coded(
                    StatusCode::FORBIDDEN,
                    "provider execution context requires provider-authorized request authentication",
                    "provider_runtime_context_required",
                ));
            }
            admit_model_execution(model_service, &selection, None, None, None).await
        }
        ModelExecutionAdmissionAuthority::ProviderRuntime => {
            let resolved = fields.resolved_model_selection.as_ref().ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "provider-authorized model execution requires a resolved model identity",
                    "provider_runtime_context_invalid",
                )
            })?;
            let gateway = gateway.ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "provider-authorized model execution requires a model gateway descriptor",
                    "provider_runtime_context_invalid",
                )
            })?;
            admit_model_execution(
                model_service,
                &selection,
                Some(resolved),
                Some(gateway),
                fields.runtime_auth.as_ref(),
            )
            .await
        }
    }
}

/// Sparse transport view used to admit execution before any session mutation.
/// Unknown chat fields are skipped by Serde rather than materialized, so large
/// message/tool payloads do not get allocated a second time at this boundary.
#[derive(Deserialize)]
struct ModelExecutionAdmissionFields {
    #[serde(default)]
    model_selection: Option<ModelSelection>,
    #[serde(default)]
    resolved_model_selection: Option<ResolvedModelSelection>,
    #[serde(default)]
    capability_descriptors: Option<RuntimeCapabilityDescriptorsRequest>,
    #[serde(default)]
    runtime_auth: Option<RuntimeAuthRequest>,
    #[serde(default, deserialize_with = "field_is_present")]
    runtime_bindings: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    api_key: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    authorization: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    base_url: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    provider: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    gateway: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    gateway_id: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    connection_id: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    execution_placement: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    endpoint: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    endpoint_url: bool,
    #[serde(default, deserialize_with = "field_is_present")]
    request_headers: bool,
}

impl ModelExecutionAdmissionFields {
    fn direct_execution_field(&self) -> Option<&'static str> {
        astra_turn_types::CLIENT_DIRECT_EXECUTION_FIELDS
            .into_iter()
            .find(|&field| self.is_field_present(field))
    }

    fn is_field_present(&self, field: &str) -> bool {
        match field {
            "runtime_bindings" => self.runtime_bindings,
            "api_key" => self.api_key,
            "authorization" => self.authorization,
            "base_url" => self.base_url,
            "provider" => self.provider,
            "gateway" => self.gateway,
            "gateway_id" => self.gateway_id,
            "connection_id" => self.connection_id,
            "execution_placement" => self.execution_placement,
            "endpoint" => self.endpoint,
            "endpoint_url" => self.endpoint_url,
            "request_headers" => self.request_headers,
            // Fail-closed: any field in CLIENT_DIRECT_EXECUTION_FIELDS that
            // lacks a match arm here is treated as present, so admission
            // rejects unknown additions rather than silently passing them.
            _unknown => true,
        }
    }
}

fn field_is_present<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{
        ModelCreateRequestData, ModelListItem, ModelRecord, ModelUpdateRequestData,
        ResolvedActiveLlmModel, ResolvedModelOffering,
    };
    use async_trait::async_trait;
    use serde_json::{Value, json};

    struct StaticModelService;

    fn unsupported_model_service_call<T>() -> Result<T, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "operation is outside the admission test service contract",
            "test_model_service_operation_unsupported",
        ))
    }

    #[async_trait]
    impl ModelService for StaticModelService {
        async fn create_model(
            &self,
            _: String,
            _: ModelCreateRequestData,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported_model_service_call()
        }

        async fn list_models(
            &self,
            _: String,
            _: bool,
        ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
            unsupported_model_service_call()
        }

        async fn get_model(
            &self,
            _: String,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported_model_service_call()
        }

        async fn resolve_model_offering(
            &self,
            offering_id: String,
        ) -> Result<ResolvedModelOffering, (StatusCode, Json<ErrorResponse>)> {
            assert_eq!(offering_id, "offer-server");
            Ok(ResolvedModelOffering {
                offering_id,
                model: ResolvedActiveLlmModel {
                    model_name: "server-model".to_string(),
                    wire_model_name: Some("wire-model".to_string()),
                    api_key: "server-secret".to_string(),
                    base_url: "https://models.example/v1".to_string(),
                    provider: "openai".to_string(),
                    fallback_chain: vec!["must-not-survive".to_string()],
                    tags: Vec::new(),
                    request_body_overrides: None,
                    prompt_cache_capability: None,
                    thinking_capability: None,
                    context_window: Some(128_000),
                    max_completion_tokens: Some(16_384),
                    request_headers: Some(serde_json::Map::from_iter([(
                        "x-model-mode".to_string(),
                        Value::String("coding".to_string()),
                    )])),
                },
            })
        }

        async fn update_model(
            &self,
            _: String,
            _: ModelUpdateRequestData,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported_model_service_call()
        }

        async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unsupported_model_service_call()
        }

        async fn check_model(
            &self,
            _: String,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            unsupported_model_service_call()
        }
    }

    #[tokio::test]
    async fn server_and_endpoint_admission_produce_one_execution_contract() {
        let server_service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        let server = admit_model_execution_from_body(
            &server_service,
            &Bytes::from_static(br#"{"model_selection":{"offering_id":"offer-server"}}"#),
            ModelExecutionAdmissionAuthority::Catalog,
        )
        .await
        .expect("Server Offering admission");
        assert_eq!(server.offering_id, "offer-server");
        assert_eq!(server.model_name, "server-model");
        assert_eq!(server.api_key, "server-secret");
        assert_eq!(
            server
                .header_overrides
                .get("x-model-mode")
                .map(String::as_str),
            Some("coding")
        );

        let unconfigured: Arc<dyn ModelService> =
            Arc::new(astra_services::UnconfiguredModelService);
        let endpoint = admit_model_execution_from_body(
            &unconfigured,
            &Bytes::from(
                json!({
                    "model_selection": {"offering_id": "offer-edge"},
                    "resolved_model_selection": {
                        "offering_id": "offer-edge",
                        "model_name": "edge-model"
                    },
                    "capability_descriptors": {
                        "model_gateway": {
                            "id": "edge-model-endpoint",
                            "type": "model_gateway",
                            "transport": "http",
                            "endpoint_url": "http://127.0.0.1:8181/chat/completions",
                            "protocol": "openai_chat_completions"
                        }
                    },
                    "runtime_auth": {"authorization": "Bearer endpoint-secret"}
                })
                .to_string(),
            ),
            ModelExecutionAdmissionAuthority::ProviderRuntime,
        )
        .await
        .expect("endpoint admission");
        assert_eq!(endpoint.offering_id, "offer-edge");
        assert_eq!(endpoint.model_name, "edge-model");
        assert_eq!(endpoint.api_key, "");
        assert_eq!(
            endpoint.completions_url_override.as_deref(),
            Some("http://127.0.0.1:8181/chat/completions")
        );
        assert_eq!(
            endpoint
                .header_overrides
                .get("authorization")
                .map(String::as_str),
            Some("Bearer endpoint-secret")
        );
    }

    #[tokio::test]
    async fn admission_rejects_unbound_resolved_identity() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        let error = admit_model_execution_from_body(
            &service,
            &Bytes::from_static(
                br#"{
                    "model_selection":{"offering_id":"offer-server"},
                    "resolved_model_selection":{
                        "offering_id":"offer-server",
                        "model_name":"attacker-model"
                    }
                }"#,
            ),
            ModelExecutionAdmissionAuthority::Catalog,
        )
        .await
        .expect_err("resolved identities require an authenticated endpoint descriptor");
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("provider_runtime_context_required")
        );
    }

    #[tokio::test]
    async fn admission_rejects_every_direct_execution_field_before_catalog_access() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        for field in astra_turn_types::CLIENT_DIRECT_EXECUTION_FIELDS {
            let body = Bytes::from(
                json!({
                    "model_selection": {"offering_id": "offer-server"},
                    (field): null,
                })
                .to_string(),
            );
            let error = admit_model_execution_from_body(
                &service,
                &body,
                ModelExecutionAdmissionAuthority::Catalog,
            )
            .await
            .expect_err("direct execution fields must fail before catalog resolution");

            assert_eq!(error.0, StatusCode::BAD_REQUEST, "field={field}");
            assert_eq!(
                error.1.error_code.as_deref(),
                Some("client_execution_override_forbidden"),
                "field={field}"
            );
        }
    }

    #[tokio::test]
    async fn admission_rejects_malformed_selection_before_catalog_access() {
        let service: Arc<dyn ModelService> = Arc::new(StaticModelService);
        let error = admit_model_execution_from_body(
            &service,
            &Bytes::from_static(br#"{"model_selection":"offer-server"}"#),
            ModelExecutionAdmissionAuthority::Catalog,
        )
        .await
        .expect_err("model selection must retain its typed wire shape");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1.error_code.as_deref(), Some("chat_request_invalid"));
    }

    #[tokio::test]
    async fn endpoint_admission_rejects_offering_identity_drift() {
        let service: Arc<dyn ModelService> = Arc::new(astra_services::UnconfiguredModelService);
        let error = admit_model_execution_from_body(
            &service,
            &Bytes::from(
                json!({
                    "model_selection": {"offering_id": "offer-requested"},
                    "resolved_model_selection": {
                        "offering_id": "offer-other",
                        "model_name": "edge-model"
                    },
                    "capability_descriptors": {
                        "model_gateway": {
                            "id": "edge-model-endpoint",
                            "type": "model_gateway",
                            "transport": "http",
                            "endpoint_url": "http://127.0.0.1:8181/chat/completions",
                            "protocol": "openai_chat_completions"
                        }
                    },
                    "runtime_auth": {"authorization": "Bearer endpoint-secret"}
                })
                .to_string(),
            ),
            ModelExecutionAdmissionAuthority::ProviderRuntime,
        )
        .await
        .expect_err("resolved Offering must remain exact");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("provider_runtime_context_invalid")
        );
    }

    #[tokio::test]
    async fn endpoint_admission_rejects_protocol_without_an_execution_adapter() {
        let service: Arc<dyn ModelService> = Arc::new(astra_services::UnconfiguredModelService);
        let error = admit_model_execution_from_body(
            &service,
            &Bytes::from(
                json!({
                    "model_selection": {"offering_id": "offer-edge"},
                    "resolved_model_selection": {
                        "offering_id": "offer-edge",
                        "model_name": "edge-model"
                    },
                    "capability_descriptors": {
                        "model_gateway": {
                            "id": "edge-model-endpoint",
                            "type": "model_gateway",
                            "transport": "http",
                            "endpoint_url": "http://127.0.0.1:8181/responses",
                            "protocol": "openai_responses"
                        }
                    },
                    "runtime_auth": {"authorization": "Bearer endpoint-secret"}
                })
                .to_string(),
            ),
            ModelExecutionAdmissionAuthority::ProviderRuntime,
        )
        .await
        .expect_err("an admitted endpoint must have a concrete execution adapter");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("model_execution_protocol_unsupported")
        );
    }

    #[test]
    fn test_all_client_direct_execution_fields_have_explicit_match_arms() {
        // Every field in CLIENT_DIRECT_EXECUTION_FIELDS MUST have an explicit
        // match arm in is_field_present(). The fail-closed fallback
        // (_unknown => true) returns true on an empty struct (all bools false),
        // so we assert false for every known field. If a new field is added to
        // the constant without a match arm, this test FAILS.
        let fields: ModelExecutionAdmissionFields =
            serde_json::from_str("{}").expect("empty JSON should deserialize");

        for &field in astra_turn_types::CLIENT_DIRECT_EXECUTION_FIELDS.iter() {
            assert!(
                !fields.is_field_present(field),
                "field '{field}' has no explicit match arm in is_field_present() — \
                 the fail-closed fallback returned true on an empty struct. \
                 Add an explicit match arm for this field."
            );
        }
    }
}
