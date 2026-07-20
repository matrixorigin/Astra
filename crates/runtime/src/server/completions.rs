//! Lightweight `/v1/chat/completions` proxy endpoint.
//!
//! Admits an opaque Offering selection (or a governed Server default), forwards
//! the request through the same execution-material boundary as agent runs, and
//! returns a compact OpenAI-compatible response.

use super::*;
use astra_server_types::{
    CompletionChoice, CompletionMessage, CompletionRequest, CompletionResponse, CompletionUsage,
};
use std::time::Duration;

fn completion_response_id(response_id: Option<&str>) -> String {
    response_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("chatcmpl-proxy-{}", uuid::Uuid::new_v4().simple()))
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
    let user = state.auth_service.current_user(&headers).await?;

    // 2. Admit one Offering. Explicit selections use the same catalog boundary
    // as durable chat runs; omission invokes the Server-owned default policy.
    let admitted = if let Some(selection) = request.model_selection.as_ref() {
        let selection = astra_turn_types::ModelSelection {
            offering_id: selection.offering_id.clone(),
        };
        super::model_execution_admission::admit_model_execution(
            &state.model_service,
            &selection,
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

    // 3. Durably admit the logical invocation before provider I/O. Auxiliary
    // work is often session-scoped; callers must not invent an agent run just
    // to satisfy accounting.
    let shared_pool = state.shared_pool.as_ref().ok_or_else(|| {
        crate::error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "Durable inference storage is unavailable",
            "inference_ledger_unavailable",
        )
    })?;
    let durable_ledger = crate::turn::llm::durable::DurableInferenceLedger::new(
        shared_pool.clone(),
        user.user_id,
        admitted.clone(),
    );
    // 4. Execute through the same typed provider boundary as agent turns.
    let mut messages = request.messages;
    crate::turn::llm::client::strip_empty_assistant_tool_calls(&mut messages);
    let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
    let parsed = durable_ledger
        .execute_nonstream(
            &state.http_client,
            request.invocation_scope,
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
        .await;
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) if crate::turn::llm::durable::is_ledger_error(&error) => {
            return Err(inference_ledger_http_error(error));
        }
        Err(error) => {
            let detail = crate::turn::llm::client::redact_provider_secrets(&error.message);
            let detail = astra_text_utils::str_preview::truncate_str(&detail, 500);
            return Err(crate::error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!("Upstream LLM request failed ({}): {detail}", error.kind),
                "model_provider_request_failed",
            ));
        }
    };

    // 5. Build the stable OpenAI-compatible response surface.

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

fn inference_ledger_http_error(
    error: astra_core::ClassifiedError,
) -> (StatusCode, Json<ErrorResponse>) {
    let (status, error_code) = match error.kind {
        astra_core::ErrorKind::InvalidRequest => {
            (StatusCode::BAD_REQUEST, "invalid_inference_scope")
        }
        astra_core::ErrorKind::DatabaseError => (
            StatusCode::SERVICE_UNAVAILABLE,
            "inference_ledger_unavailable",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "inference_ledger_unavailable",
        ),
    };
    crate::error_response_coded(
        status,
        "Durable inference admission or settlement failed",
        error_code,
    )
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
            invocation_scope: astra_turn_types::InferenceInvocationScope::Session {
                session_id: "session-completion".to_string(),
                turn: 1,
                round: 0,
                operation_id: "completion_test".to_string(),
                logical_attempt: 0,
            },
            model_selection: Some(astra_turn_types::ModelSelection {
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
            "invocation_scope": {
                "kind": "session",
                "session_id": "session-1",
                "turn": 1,
                "round": 0,
                "operation_id": "verification",
                "logical_attempt": 0
            },
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
    fn completion_response_id_preserves_provider_identity_and_generates_unique_fallbacks() {
        assert_eq!(
            completion_response_id(Some("provider-response")),
            "provider-response"
        );

        let first = completion_response_id(None);
        let second = completion_response_id(None);
        assert!(first.starts_with("chatcmpl-proxy-"), "{first}");
        assert!(second.starts_with("chatcmpl-proxy-"), "{second}");
        assert_ne!(first, second);
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
            "invocation_scope": {
                "kind": "session",
                "session_id": "session-1",
                "turn": 1,
                "round": 0,
                "operation_id": "verification",
                "logical_attempt": 0
            },
            "model": "gpt-4o-mini",
            "messages": []
        }"#;
        assert!(serde_json::from_str::<CompletionRequest>(legacy).is_err());
    }

    #[test]
    fn completion_request_accepts_only_typed_offering_selection() {
        let json = r#"{
            "purpose": "memory_extraction",
            "invocation_scope": {
                "kind": "session",
                "session_id": "session-1",
                "turn": 1,
                "round": 0,
                "operation_id": "memory_extraction",
                "logical_attempt": 0
            },
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
    async fn completion_without_durable_storage_never_contacts_the_provider() {
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post({
                let request_count = Arc::clone(&request_count);
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let request_count = Arc::clone(&request_count);
                    async move {
                        let _ = (headers, body);
                        request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

        let error = completions_handler(
            State(state),
            completion_headers(),
            Json(explicit_completion_request("offer-completion")),
        )
        .await
        .expect_err("provider I/O requires durable admission");

        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.1.error_code.as_deref(),
            Some("inference_ledger_unavailable")
        );
        assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 0);

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
    #[ignore = "requires live MatrixOne: run with ASTRA_TEST_DB_IT=1"]
    async fn completion_http_boundary_persists_session_scope_attempt_and_usage_before_returning() {
        use axum::response::IntoResponse;
        use sqlx::Row;

        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let mut settings = astra_core::config::MatrixOneSettings::from_env();
        settings.db_pool_max_connections = settings.db_pool_max_connections.min(4);
        settings.db_pool_min_connections = settings.db_pool_min_connections.min(1);
        let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
            .unwrap_or_else(|_| "mysql".to_string());
        astra_services::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure inference schema");
        let shared_pool = astra_core::SharedPool::new(&settings)
            .await
            .expect("connect MatrixOne");
        let pool = shared_pool.get();
        let session_id = format!("completion-ledger-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO agent_sessions
             (session_id, user_id, status, event_count, project_retention_policy,
              created_at, updated_at, last_active_at)
             VALUES (?, 'test-user', 'active', 0, 'session', NOW(6), NOW(6), NOW(6))",
        )
        .bind(&session_id)
        .execute(pool)
        .await
        .expect("seed completion session");

        let provider_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post({
                let provider_requests = Arc::clone(&provider_requests);
                move |Json(body): Json<serde_json::Value>| {
                    let provider_requests = Arc::clone(&provider_requests);
                    async move {
                        provider_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let force_failure = body["messages"].as_array().is_some_and(|messages| {
                            messages.iter().any(|message| {
                                message["content"].as_str() == Some("force-provider-failure")
                            })
                        });
                        if force_failure {
                            return (
                                StatusCode::BAD_GATEWAY,
                                Json(json!({"error": {"message": "provider unavailable"}})),
                            )
                                .into_response();
                        }
                        Json(json!({
                            "id": "provider-session-scope",
                            "choices": [{
                                "message": {"role": "assistant", "content": "durable memory"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 13, "completion_tokens": 5}
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider");
        let provider_address = listener.local_addr().expect("provider address");
        let provider_task = tokio::spawn(async move {
            axum::serve(listener, provider)
                .await
                .expect("serve provider");
        });
        let state = AppState::new(Default::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
            .with_model_service(Arc::new(CompletionModelService {
                base_url: format!("http://{provider_address}/v1"),
            }))
            .with_shared_pool(shared_pool.clone());
        let mut request = explicit_completion_request("offer-completion");
        request.invocation_scope = astra_turn_types::InferenceInvocationScope::Session {
            session_id: session_id.clone(),
            turn: 3,
            round: 1,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 0,
        };

        let response =
            completions_handler(State(state.clone()), completion_headers(), Json(request))
                .await
                .expect("durable completion succeeds")
                .0;
        assert_eq!(response.choices[0].message.content, "durable memory");
        assert_eq!(response.usage.expect("provider usage").total_tokens, 18);
        assert_eq!(
            provider_requests.load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let durable = sqlx::query(
            "SELECT r.scope_kind, r.run_id, i.operation_id, i.status AS invocation_status,
                    i.input_tokens, i.output_tokens, a.status AS attempt_status,
                    a.provider_response_id
             FROM inference_invocations i
             JOIN inference_routes r
               ON r.user_id = i.user_id AND r.route_id = i.route_id
             JOIN inference_provider_attempts a
               ON a.user_id = i.user_id AND a.invocation_id = i.invocation_id
             WHERE i.user_id = 'test-user' AND i.session_id = ?",
        )
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("load durable completion facts");
        assert_eq!(durable.get::<String, _>("scope_kind"), "session");
        assert_eq!(durable.get::<Option<String>, _>("run_id"), None);
        assert_eq!(
            durable.get::<String, _>("operation_id"),
            "memory_extraction"
        );
        assert_eq!(durable.get::<String, _>("invocation_status"), "succeeded");
        assert_eq!(durable.get::<i64, _>("input_tokens"), 13);
        assert_eq!(durable.get::<i64, _>("output_tokens"), 5);
        assert_eq!(durable.get::<String, _>("attempt_status"), "succeeded");
        assert_eq!(
            durable.get::<Option<String>, _>("provider_response_id"),
            Some("provider-session-scope".to_string())
        );

        let mut rejected_request = explicit_completion_request("offer-completion");
        rejected_request.invocation_scope = astra_turn_types::InferenceInvocationScope::Session {
            session_id: format!("not-owned-{session_id}"),
            turn: 3,
            round: 1,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 1,
        };
        let rejected = completions_handler(
            State(state.clone()),
            completion_headers(),
            Json(rejected_request),
        )
        .await
        .expect_err("unknown session scope must fail before provider I/O");
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            rejected.1.error_code.as_deref(),
            Some("invalid_inference_scope")
        );
        assert_eq!(
            provider_requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "invalid owner scope must not contact the provider"
        );

        let provider_requests_before_failure =
            provider_requests.load(std::sync::atomic::Ordering::SeqCst);
        let mut failed_request = explicit_completion_request("offer-completion");
        failed_request.messages = vec![json!({
            "role": "user",
            "content": "force-provider-failure"
        })];
        failed_request.invocation_scope = astra_turn_types::InferenceInvocationScope::Session {
            session_id: session_id.clone(),
            turn: 3,
            round: 1,
            operation_id: "memory_extraction_failure".to_string(),
            logical_attempt: 0,
        };
        let provider_failure =
            completions_handler(State(state), completion_headers(), Json(failed_request))
                .await
                .expect_err("provider failure must remain visible to the caller");
        assert_eq!(provider_failure.0, StatusCode::BAD_GATEWAY);
        assert!(
            provider_requests.load(std::sync::atomic::Ordering::SeqCst)
                > provider_requests_before_failure,
            "the valid failed invocation must reach the provider"
        );

        let failed_invocation = sqlx::query(
            "SELECT status FROM inference_invocations
             WHERE user_id = 'test-user' AND session_id = ? AND operation_id = ?",
        )
        .bind(&session_id)
        .bind("memory_extraction_failure")
        .fetch_one(pool)
        .await
        .expect("load failed logical invocation");
        assert_eq!(failed_invocation.get::<String, _>("status"), "failed");
        let failed_attempts = sqlx::query(
            "SELECT a.status FROM inference_provider_attempts a
             JOIN inference_invocations i
               ON i.user_id = a.user_id AND i.invocation_id = a.invocation_id
             WHERE i.user_id = 'test-user' AND i.session_id = ? AND i.operation_id = ?",
        )
        .bind(&session_id)
        .bind("memory_extraction_failure")
        .fetch_all(pool)
        .await
        .expect("load failed provider attempts");
        assert!(!failed_attempts.is_empty());
        assert!(
            failed_attempts
                .iter()
                .all(|row| row.get::<String, _>("status") == "failed")
        );

        for statement in [
            "DELETE FROM inference_provider_attempts WHERE user_id = 'test-user' AND session_id = ?",
            "DELETE FROM inference_invocations WHERE user_id = 'test-user' AND session_id = ?",
            "DELETE FROM inference_routes WHERE user_id = 'test-user' AND session_id = ?",
            "DELETE FROM agent_sessions WHERE user_id = 'test-user' AND session_id = ?",
        ] {
            sqlx::query(statement)
                .bind(&session_id)
                .execute(pool)
                .await
                .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
        }
        provider_task.abort();
        assert!(
            provider_task
                .await
                .expect_err("provider should be cancelled")
                .is_cancelled()
        );
        shared_pool.close().await;
    }
}
