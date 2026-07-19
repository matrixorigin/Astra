//! Lightweight `/v1/chat/completions` proxy endpoint.
//!
//! Admits an opaque Offering selection (or a governed Server default), forwards
//! the request through the same execution-material boundary as agent runs, and
//! returns a compact OpenAI-compatible response.

use super::*;
use serde::Deserialize;
use std::time::Duration;

/// OpenAI-compatible chat completion request (subset).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompletionRequest {
    /// Semantic reason for this inference. The server uses this typed value
    /// for attribution and policy; it is never inferred from prompt text.
    pub purpose: astra_turn_types::InferencePurpose,
    /// Optional explicit Offering. Absence means the governed Server default;
    /// model names, URLs, and credentials are never accepted as selectors.
    #[serde(default)]
    pub model_selection: Option<astra_services::runs::ModelSelectionRequest>,
    pub messages: Vec<serde_json::Value>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
}

fn default_max_tokens() -> u32 {
    512
}
fn default_temperature() -> f64 {
    0.1
}

fn completion_response_id(response_id: Option<&str>) -> String {
    response_id.unwrap_or("chatcmpl-proxy").to_string()
}

/// OpenAI-compatible chat completion response (subset).
#[derive(Debug, Serialize)]
pub(super) struct CompletionResponse {
    pub id: String,
    pub object: String,
    pub offering_id: String,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<CompletionUsage>,
}

#[derive(Debug, Serialize)]
pub(super) struct CompletionChoice {
    pub index: u32,
    pub message: CompletionMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CompletionMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// `POST /v1/chat/completions` — lightweight LLM proxy.
///
/// Authenticates via bearer token, admits one effective Offering, and forwards
/// a non-streaming request to the admitted provider route.
pub(super) async fn completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CompletionRequest>,
) -> Result<Json<CompletionResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Authenticate
    let _user = state.auth_service.current_user(&headers).await?;

    // 2. Admit one Offering. Explicit selections use the same catalog boundary
    // as durable chat runs; omission invokes the Server-owned default policy.
    let admitted = if let Some(selection) = request.model_selection.as_ref() {
        super::model_execution_admission::admit_model_execution(
            &state.model_service,
            selection,
            None,
            None,
            None,
        )
        .await?
    } else {
        let matrixone =
            crate::matrix_cloud_runtime::matrix_settings_from_env().map_err(|error| {
                crate::error_response_coded(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("MatrixOne configuration unavailable: {error}"),
                    "model_catalog_unavailable",
                )
            })?;
        let offering = astra_services::resolve_reasoning_offering(
            &matrixone,
            &state.fernet_encryptor,
            state.admin.config_service.as_ref(),
            state.shared_pool.as_ref().map(|pool| pool.get()),
        )
        .await
        .map_err(|error| {
            crate::error_response_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Default Offering resolution failed: {error}"),
                "model_default_unavailable",
            )
        })?;
        astra_services::AdmittedModelExecution::from_offering(offering).map_err(|error| {
            crate::error_response_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Default Offering execution configuration is invalid: {error}"),
                "model_execution_configuration_invalid",
            )
        })?
    };

    // 3. Execute through the same typed provider boundary as agent turns.
    let mut messages = request.messages;
    crate::turn::llm::client::strip_empty_assistant_tool_calls(&mut messages);
    let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
    let parsed = crate::turn::llm::client::call_llm_nonstream(
        &state.http_client,
        crate::turn::llm::client::LlmCall {
            purpose: request.purpose,
            messages: &messages,
            tools: &[],
            route: crate::turn::llm::client::LlmExecutionRoute::from_admitted(&admitted),
            max_output_tokens: Some(request.max_tokens as usize),
            temperature: Some(request.temperature),
            has_fallback: false,
            thinking: &thinking,
        },
        Duration::from_secs(120),
    )
    .await
    .map_err(|error| {
        let detail = crate::turn::llm::client::redact_provider_secrets(&error.message);
        let detail = astra_text_utils::str_preview::truncate_str(&detail, 500);
        crate::error_response_coded(
            StatusCode::BAD_GATEWAY,
            format!("Upstream LLM request failed ({}): {detail}", error.kind),
            "model_provider_request_failed",
        )
    })?;

    // 4. Build the stable OpenAI-compatible response surface.

    let response_id = completion_response_id(parsed.response_id.as_deref());
    let content = parsed.full_text;
    let finish_reason = parsed.finish_reason.unwrap_or_else(|| "stop".to_string());
    let usage = completion_usage(&parsed.usage);

    Ok(Json(CompletionResponse {
        id: response_id,
        object: "chat.completion".to_string(),
        offering_id: admitted.offering_id,
        model: admitted.model_name,
        choices: vec![CompletionChoice {
            index: 0,
            message: CompletionMessage {
                role: "assistant".to_string(),
                content,
            },
            finish_reason,
        }],
        usage,
    }))
}

fn completion_usage(raw: &serde_json::Map<String, serde_json::Value>) -> Option<CompletionUsage> {
    if raw.is_empty() {
        return None;
    }
    let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(raw);
    let prompt_tokens =
        usage.input_tokens + usage.cached_input_tokens + usage.cache_creation_tokens;
    Some(CompletionUsage {
        prompt_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{
        ModelCreateRequestData, ModelListItem, ModelRecord, ModelService, ModelUpdateRequestData,
        ResolvedActiveLlmModel, ResolvedModelOffering,
    };
    use async_trait::async_trait;
    use axum::http::{HeaderValue, header::AUTHORIZATION};
    use serde_json::json;
    use std::sync::Arc;

    struct Healthy;

    #[async_trait]
    impl crate::app_state::HealthChecker for Healthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    struct CompletionModelService {
        base_url: String,
    }

    fn unsupported_model_service_call<T>() -> Result<T, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "operation is outside the completion test contract",
        ))
    }

    #[async_trait]
    impl ModelService for CompletionModelService {
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
            if offering_id != "offer-completion" {
                return Err(crate::error_response_coded(
                    StatusCode::NOT_FOUND,
                    "Offering not found",
                    "model_offering_not_found",
                ));
            }
            Ok(ResolvedModelOffering {
                offering_id,
                model: ResolvedActiveLlmModel {
                    model_name: "catalog-display-model".into(),
                    wire_model_name: Some("provider-wire-model".into()),
                    api_key: "provider-secret".into(),
                    base_url: self.base_url.clone(),
                    provider: "openai".into(),
                    fallback_chain: Vec::new(),
                    tags: Vec::new(),
                    request_body_overrides: None,
                    prompt_cache_capability: None,
                    thinking_capability: None,
                    context_window: Some(32_000),
                    request_headers: Some(serde_json::Map::from_iter([(
                        "x-offering-route".into(),
                        serde_json::Value::String("admitted".into()),
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

    fn explicit_completion_request(offering_id: &str) -> CompletionRequest {
        CompletionRequest {
            purpose: astra_turn_types::InferencePurpose::MemoryExtraction,
            model_selection: Some(astra_services::runs::ModelSelectionRequest {
                offering_id: offering_id.to_string(),
            }),
            messages: vec![json!({"role": "user", "content": "extract"})],
            max_tokens: 64,
            temperature: 0.0,
        }
    }

    fn completion_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer user-token"));
        headers
    }

    #[test]
    fn completion_request_defaults() {
        let json = r#"{
            "purpose": "verification_judge",
            "messages": [{"role": "user", "content": "hello"}]
        }"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            req.purpose,
            astra_turn_types::InferencePurpose::VerificationJudge
        );
        assert_eq!(req.max_tokens, 512);
        assert!((req.temperature - 0.1).abs() < f64::EPSILON);
        assert!(req.model_selection.is_none());
    }

    #[test]
    fn completion_response_serializes() {
        let resp = CompletionResponse {
            id: "test".into(),
            object: "chat.completion".into(),
            offering_id: "offer-test".into(),
            model: "gpt-4o-mini".into(),
            choices: vec![CompletionChoice {
                index: 0,
                message: CompletionMessage {
                    role: "assistant".into(),
                    content: r#"{"score": 0.85}"#.into(),
                },
                finish_reason: "stop".into(),
            }],
            usage: Some(CompletionUsage {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
            }),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["choices"][0]["message"]["content"],
            r#"{"score": 0.85}"#
        );
    }

    #[test]
    fn completions_handler_strips_empty_assistant_tool_calls_before_forwarding() {
        let mut messages = vec![
            serde_json::json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        crate::turn::llm::client::strip_empty_assistant_tool_calls(&mut messages);
        assert!(messages[0].get("tool_calls").is_none(), "{messages:?}");
    }

    #[test]
    fn completion_request_requires_a_known_inference_purpose() {
        let missing = r#"{"messages": []}"#;
        let unknown = r#"{"purpose": "other", "messages": []}"#;
        assert!(serde_json::from_str::<CompletionRequest>(missing).is_err());
        assert!(serde_json::from_str::<CompletionRequest>(unknown).is_err());
    }

    #[test]
    fn completion_request_rejects_model_name_selection() {
        let legacy = r#"{
            "purpose": "verification_judge",
            "model": "gpt-4o-mini",
            "messages": []
        }"#;
        assert!(serde_json::from_str::<CompletionRequest>(legacy).is_err());
    }

    #[test]
    fn completion_request_accepts_only_typed_offering_selection() {
        let json = r#"{
            "purpose": "memory_extraction",
            "model_selection": {"offering_id": "offer-memory"},
            "messages": []
        }"#;
        let request = serde_json::from_str::<CompletionRequest>(json).expect("typed request");
        assert_eq!(
            request
                .model_selection
                .expect("explicit Offering")
                .offering_id,
            "offer-memory"
        );
    }

    #[test]
    fn completion_response_does_not_claim_zero_usage_when_provider_omits_usage() {
        let raw = serde_json::Map::new();
        assert!(completion_usage(&raw).is_none());
    }

    #[tokio::test]
    async fn explicit_completion_executes_only_the_admitted_offering_route() {
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let request_tx = Arc::new(std::sync::Mutex::new(Some(request_tx)));
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post({
                let request_tx = Arc::clone(&request_tx);
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let request_tx = Arc::clone(&request_tx);
                    async move {
                        let observed = json!({
                            "authorization": headers
                                .get(AUTHORIZATION)
                                .and_then(|value| value.to_str().ok()),
                            "route_header": headers
                                .get("x-offering-route")
                                .and_then(|value| value.to_str().ok()),
                            "body": body,
                        });
                        request_tx
                            .lock()
                            .expect("request capture lock")
                            .take()
                            .expect("one provider request")
                            .send(observed)
                            .expect("capture provider request");
                        Json(json!({
                            "id": "provider-response",
                            "choices": [{
                                "message": {"role": "assistant", "content": "memory"},
                                "finish_reason": "stop"
                            }]
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider");
        let address = listener.local_addr().expect("provider address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve provider");
        });
        let state = AppState::new(Default::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
            .with_model_service(Arc::new(CompletionModelService {
                base_url: format!("http://{address}/v1"),
            }));

        let response = completions_handler(
            State(state),
            completion_headers(),
            Json(explicit_completion_request("offer-completion")),
        )
        .await
        .expect("completion through admitted Offering")
        .0;
        let observed = request_rx.await.expect("provider request captured");

        assert_eq!(response.offering_id, "offer-completion");
        assert_eq!(response.model, "catalog-display-model");
        assert!(response.usage.is_none());
        assert_eq!(observed["authorization"], "Bearer provider-secret");
        assert_eq!(observed["route_header"], "admitted");
        assert_eq!(observed["body"]["model"], "provider-wire-model");
        assert!(observed["body"].get("purpose").is_none());

        server.abort();
        assert!(
            server
                .await
                .expect_err("provider server should be cancelled")
                .is_cancelled()
        );
    }

    #[tokio::test]
    async fn completion_preserves_unknown_offering_as_a_typed_admission_failure() {
        let state = AppState::new(Default::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
            .with_model_service(Arc::new(CompletionModelService {
                base_url: "http://127.0.0.1:1/v1".into(),
            }));

        let error = completions_handler(
            State(state),
            completion_headers(),
            Json(explicit_completion_request("offer-unknown")),
        )
        .await
        .expect_err("unknown Offering must fail before provider execution");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("model_offering_not_found")
        );
    }

    #[tokio::test]
    async fn provider_failure_is_typed_and_does_not_change_the_admitted_identity() {
        let state = AppState::new(Default::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
            .with_model_service(Arc::new(CompletionModelService {
                base_url: "http://127.0.0.1:1/v1".into(),
            }));

        let error = completions_handler(
            State(state),
            completion_headers(),
            Json(explicit_completion_request("offer-completion")),
        )
        .await
        .expect_err("unreachable provider must fail");

        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("model_provider_request_failed")
        );
        assert!(!error.1.detail.contains("provider-secret"));
    }
}
