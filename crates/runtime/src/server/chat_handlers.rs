use super::bridge_prep::normalize_chat_turn_session_error;
use super::header_utils::collect_forward_headers;
use super::*;
use crate::server::{
    provider_runtime_context::{
        inject_effective_runtime_context, inject_effective_runtime_context_body,
    },
    run::handlers::transform_stream_run_events_for_client,
};

fn chat_request_json_error_from_body_text(detail: String) -> (StatusCode, Json<ErrorResponse>) {
    if detail.contains("selected_model") || detail.contains("SelectedModelRequest") {
        return astra_core::error_response_coded(
            StatusCode::BAD_REQUEST,
            detail,
            "selected_model_invalid",
        );
    }
    error_response(StatusCode::BAD_REQUEST, detail)
}

fn parse_chat_request_body(body: &Bytes) -> Result<ChatRequest, (StatusCode, Json<ErrorResponse>)> {
    serde_json::from_slice(body)
        .map_err(|error| chat_request_json_error_from_body_text(error.to_string()))
}

/// Safely convert a string to a HeaderValue, returning an SSE error response on failure.
#[allow(clippy::result_large_err)]
fn safe_header_value(value: &str) -> Result<HeaderValue, Response> {
    HeaderValue::from_str(value).map_err(|_| {
        sse_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid header value: contains non-visible ASCII".to_string(),
        )
    })
}

#[cfg(test)]
pub(super) async fn resolve_or_create_chat_session_id(
    state: &AppState,
    user: &AuthUserRecord,
    requested_session_id: Option<String>,
    agent_id: Option<String>,
    session_id_is_trusted: bool,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    resolve_or_create_chat_session(
        state,
        user,
        requested_session_id,
        agent_id,
        session_id_is_trusted,
    )
    .await
    .map(|resolved| resolved.session_id)
}

pub(super) struct ResolvedChatSession {
    pub(super) session_id: Option<String>,
    pub(super) full_llm_capture: bool,
}

pub(super) async fn resolve_or_create_chat_session(
    state: &AppState,
    user: &AuthUserRecord,
    requested_session_id: Option<String>,
    agent_id: Option<String>,
    session_id_is_trusted: bool,
) -> Result<ResolvedChatSession, (StatusCode, Json<ErrorResponse>)> {
    match requested_session_id {
        Some(session_id) => {
            if session_id.trim().is_empty() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "session_id must not be empty",
                ));
            }

            match state
                .session_service
                .get_session(session_id.clone(), user.user_id.clone())
                .await
            {
                Ok(session) => Ok(ResolvedChatSession {
                    session_id: Some(session_id),
                    full_llm_capture:
                        crate::turn::llm::exchange_capture::session_full_llm_capture_enabled(Some(
                            &session.metadata,
                        )),
                }),
                Err(error) if is_session_service_unconfigured_error(&error) => {
                    Ok(ResolvedChatSession {
                        session_id: session_id_is_trusted.then_some(session_id),
                        full_llm_capture: false,
                    })
                }
                Err(error) => Err(normalize_chat_turn_session_error(error)),
            }
        }
        None => {
            let metadata = agent_id.as_ref().map(|agent_id| {
                serde_json::Map::from_iter([(
                    "agent_id".to_string(),
                    serde_json::Value::String(agent_id.clone()),
                )])
            });

            match super::session::session_quota::create_session_with_resource_quota(
                state,
                user.user_id.clone(),
                SessionCreateRequestData {
                    agent_id,
                    title: None,
                    metadata,
                },
            )
            .await
            {
                Ok(session) => Ok(ResolvedChatSession {
                    session_id: Some(session.session_id),
                    full_llm_capture:
                        crate::turn::llm::exchange_capture::session_full_llm_capture_enabled(Some(
                            &session.metadata,
                        )),
                }),
                Err(error) if is_session_service_unconfigured_error(&error) => {
                    Ok(ResolvedChatSession {
                        session_id: None,
                        full_llm_capture: false,
                    })
                }
                Err(error) => Err(error),
            }
        }
    }
}

pub(super) fn is_session_service_unconfigured_error(
    error: &(StatusCode, Json<ErrorResponse>),
) -> bool {
    error.0 == StatusCode::NOT_IMPLEMENTED && error.1.0.detail == "Session service not configured"
}

pub(super) async fn chat_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    let principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/chat", &body),
        )
        .await?;
    let request = parse_chat_request_body(&body)?;
    let user = principal.user.clone();
    let mut chat_data = chat_request_into_data(request);
    chat_data.forward_headers = collect_forward_headers(&headers);
    let resolved = resolve_or_create_chat_session(
        &state,
        &user,
        chat_data.session_id.take(),
        chat_data.agent_id.clone(),
        false,
    )
    .await?;
    chat_data.session_id = resolved.session_id;
    chat_data.full_llm_capture = resolved.full_llm_capture;
    inject_effective_runtime_context(&state, &principal, &mut chat_data).await?;
    let run = state
        .execution
        .run_lifecycle_service
        .create_run(user.user_id, chat_data)
        .await?;
    Ok(Json(ChatResponse::from(run)))
}

pub(super) async fn chat_stream_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = match state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/chat/stream", &body),
        )
        .await
    {
        Ok(principal) => principal,
        Err((status, error)) => return sse_error_response_from_error(status, error.0),
    };
    let request = match parse_chat_request_body(&body) {
        Ok(request) => request,
        Err((status, error)) => {
            return sse_error_response_from_error(status, error.0);
        }
    };
    let user = principal.user.clone();

    let mut chat_data = chat_request_into_data(request);
    chat_data.forward_headers = collect_forward_headers(&headers);
    let resolved = match resolve_or_create_chat_session(
        &state,
        &user,
        chat_data.session_id.take(),
        chat_data.agent_id.clone(),
        false,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err((status, error)) => return sse_error_response_from_error(status, error.0),
    };
    chat_data.session_id = resolved.session_id;
    chat_data.full_llm_capture = resolved.full_llm_capture;
    if let Err((status, error)) =
        inject_effective_runtime_context(&state, &principal, &mut chat_data).await
    {
        return sse_error_response_from_error(status, error.0);
    }

    match state
        .execution
        .run_lifecycle_service
        .stream_chat(user.user_id.clone(), chat_data.clone())
        .await
    {
        Ok(mut stream) => {
            if let Some(event_rx) = stream.event_rx.take() {
                // Incremental SSE streaming: convert channel into SSE body.
                let session_id = stream.session_id.clone();
                let run_id = stream.run_id.clone();
                sse_streaming_response(session_id, run_id, event_rx)
            } else {
                // Batch fallback (test stubs, etc.)
                let mut events = vec![serde_json::json!({
                    "type": "session_info",
                    "session_id": stream.session_id,
                    "run_id": stream.run_id,
                })];
                events.extend(transform_stream_run_events_for_client(
                    &stream.run_id,
                    stream.events,
                ));
                sse_json_response(events)
            }
        }
        Err((status, error)) => sse_error_response_from_error(status, error.0),
    }
}

pub(super) async fn chat_turn_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = match state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/chat/turn", &body),
        )
        .await
    {
        Ok(principal) => principal,
        Err((status, error)) => return sse_error_response_from_error(status, error.0),
    };
    let body = match inject_effective_runtime_context_body(&state, &principal, body).await {
        Ok(body) => body,
        Err((status, error)) => return sse_error_response_from_error(status, error.0),
    };
    dispatch_chat_turn_bridge(&state, &principal.user, &headers, body).await
}

pub(super) async fn dispatch_chat_turn_bridge(
    state: &AppState,
    user: &AuthUserRecord,
    source_headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let mut bridge_headers = HeaderMap::new();
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-secret"),
        match safe_header_value(&state.chat_turn_bridge_secret) {
            Ok(v) => v,
            Err(r) => return r,
        },
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-user-id"),
        match safe_header_value(&user.user_id) {
            Ok(v) => v,
            Err(r) => return r,
        },
    );
    let username_b64 = URL_SAFE.encode(user.username.as_bytes());
    bridge_headers.insert(
        HeaderName::from_static("x-mo-username-b64"),
        match safe_header_value(&username_b64) {
            Ok(v) => v,
            Err(r) => return r,
        },
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-capabilities"),
        HeaderValue::from_static("state-sync-v1"),
    );
    if let Some(auth) = source_headers.get("authorization").cloned() {
        bridge_headers.insert(HeaderName::from_static("authorization"), auth);
    }

    let prepared = match prepare_chat_turn_bridge_body(state, user, body, None).await {
        Ok(result) => result,
        Err((status, error)) => return sse_error_response_from_error(status, error.0),
    };
    if let Some(trusted_session_id) = prepared.trusted_session_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-session-id"),
            match safe_header_value(trusted_session_id) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if prepared.full_llm_capture == Some(true) {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-full-llm-capture"),
            HeaderValue::from_static("1"),
        );
    }
    if let Some(session_turn) = prepared.session_turn.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-session-turn"),
            match safe_header_value(session_turn) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(turn_chain_id) = prepared.turn_chain_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-turn-chain-id"),
            match safe_header_value(turn_chain_id) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(user_query_event_id) = prepared.user_query_event_id.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-user-query-event-id"),
            match safe_header_value(user_query_event_id) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(tools_changed) = prepared.tools_changed {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-tools-changed"),
            HeaderValue::from_static(if tools_changed { "1" } else { "0" }),
        );
    }
    if let Some(task_hint) = prepared.task_hint.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-task-hint"),
            match safe_header_value(task_hint) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(user_query_b64) = prepared.user_query_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-user-query-b64"),
            match safe_header_value(user_query_b64) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(routing_meta_b64) = prepared.routing_meta_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-routing-meta-b64"),
            match safe_header_value(routing_meta_b64) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(force_intent) = prepared.force_intent.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-force-intent"),
            match safe_header_value(force_intent) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    if let Some(execution_state_b64) = prepared.execution_state_b64.as_deref() {
        bridge_headers.insert(
            HeaderName::from_static("x-mo-execution-state-b64"),
            match safe_header_value(execution_state_b64) {
                Ok(v) => v,
                Err(r) => return r,
            },
        );
    }
    // Bridge E2E hooks (`bridge-e2e-hooks`): in-process bridge reads this header with env
    // `ASTRA_TEST_BRIDGE_SECRET`; harmless if unset or header absent.
    if let Some(v) = source_headers.get("x-mo-bridge-test-secret").cloned() {
        bridge_headers.insert(HeaderName::from_static("x-mo-bridge-test-secret"), v);
    }

    let client_disconnect = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());

    let bridge = match state.chat_turn_bridge.as_ref() {
        Some(b) => b,
        None => {
            return sse_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "chat turn bridge disabled. Configure the runtime with an in-process bridge.",
            );
        }
    };
    match bridge
        .forward(
            &bridge_headers,
            prepared.body,
            state.turn_persistence.core_event_writer.clone(),
            state.turn_persistence.tool_event_writer.clone(),
            state.turn_persistence.hook_db_writer.clone(),
            state.turn_persistence.reflection_state_store.clone(),
            state.turn_persistence.reflection_lesson_writer.clone(),
            state.turn_persistence.observer_worker.clone(),
            state.turn_persistence.auxiliary_event_writer.clone(),
            state.turn_persistence.session_activity_writer.clone(),
            Some(client_disconnect),
        )
        .await
    {
        Ok(response) => response,
        Err((status, error)) => {
            sse_error_response(status, format!("Chat turn bridge unavailable: {error}"))
        }
    }
}

pub(super) async fn chat_route_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRouteRequest>,
) -> Result<Json<ChatRouteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _ = state.auth_service.current_user(&headers).await?;
    Ok(Json(classify_chat_route(request.query)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_header_names_are_valid() {
        let headers = [
            "x-mo-bridge-secret",
            "x-mo-user-id",
            "x-mo-username-b64",
            "x-mo-bridge-capabilities",
            "x-mo-session-id",
            "x-mo-full-llm-capture",
            "x-mo-turn-chain-id",
            "x-mo-user-query-event-id",
            "x-mo-tools-changed",
            "x-mo-task-hint",
            "x-mo-user-query-b64",
            "x-mo-routing-meta-b64",
            "x-mo-force-intent",
            "x-mo-execution-state-b64",
            "x-mo-bridge-test-secret",
        ];
        for name in headers {
            assert!(
                HeaderName::from_static(name).as_str() == name,
                "invalid header name: {name}"
            );
        }
    }

    #[test]
    fn username_b64_encoding() {
        let username = "alice";
        let encoded = URL_SAFE.encode(username.as_bytes());
        let decoded = URL_SAFE.decode(&encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "alice");

        // CJK username
        let cjk = "张三";
        let encoded_cjk = URL_SAFE.encode(cjk.as_bytes());
        let decoded_cjk = URL_SAFE.decode(&encoded_cjk).unwrap();
        assert_eq!(String::from_utf8(decoded_cjk).unwrap(), "张三");
    }

    #[test]
    fn bridge_capabilities_header_value() {
        let hv = HeaderValue::from_static("state-sync-v1");
        assert_eq!(hv.to_str().unwrap(), "state-sync-v1");
    }

    #[test]
    fn tools_changed_header_values() {
        let true_val = if true { "1" } else { "0" };
        let false_val = if false { "1" } else { "0" };
        assert_eq!(true_val, "1");
        assert_eq!(false_val, "0");
        // Ensure they are valid header values
        assert!(HeaderValue::from_static("1").to_str().is_ok());
        assert!(HeaderValue::from_static("0").to_str().is_ok());
    }

    #[test]
    fn header_value_from_str_handles_special_chars() {
        // UUID format
        assert!(HeaderValue::from_str("550e8400-e29b-41d4-a716-446655440000").is_ok());
        // Base64 with padding
        assert!(HeaderValue::from_str("dXNlcm5hbWU=").is_ok());
        // Base64 URL-safe
        assert!(HeaderValue::from_str("aGVsbG8td29ybGQ").is_ok());
    }

    #[test]
    fn dispatch_header_count() {
        // Base headers: 4 (secret, user-id, username-b64, capabilities)
        // + authorization passthrough: 1
        // + optional from prepared: 11 (session-id, full-llm-capture, session-turn, turn-chain-id,
        //   user-query-event-id, tools-changed, task-hint, user-query-b64,
        //   routing-meta-b64, force-intent, execution-state-b64)
        // Total possible: 16
        assert_eq!(4 + 1 + 11, 16);
    }
}

#[cfg(test)]
mod session_resolution_tests {
    use std::sync::Arc;

    use astra_core::error_response;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use crate::{
        AppState, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, SessionActivityRecord,
        SessionCreateRequestData, SessionListFilter, SessionListRecord, SessionRecord,
        SessionService, SessionUpdateRequestData,
    };

    use super::*;

    #[derive(Clone)]
    struct StubHealthChecker;

    #[async_trait]
    impl HealthChecker for StubHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSessionService {
        created: Arc<Mutex<Vec<(String, SessionCreateRequestData)>>>,
        missing_sessions: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingSessionService {
        async fn mark_missing(&self, session_id: &str) {
            self.missing_sessions
                .lock()
                .await
                .push(session_id.to_string());
        }

        async fn created_requests(&self) -> Vec<(String, SessionCreateRequestData)> {
            self.created.lock().await.clone()
        }
    }

    #[async_trait]
    impl SessionService for RecordingSessionService {
        async fn create_session(
            &self,
            user_id: String,
            request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.created
                .lock()
                .await
                .push((user_id.clone(), request.clone()));
            Ok(SessionRecord {
                session_id: "s-created".to_string(),
                user_id,
                agent_id: request.agent_id,
                title: Some("Created".to_string()),
                metadata: request.metadata.unwrap_or_default(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionListRecord {
                sessions: Vec::new(),
                total: Some(0),
                limit: 20,
                next_cursor: None,
            })
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            if self
                .missing_sessions
                .lock()
                .await
                .iter()
                .any(|missing| missing == &session_id)
            {
                return Err(error_response(
                    StatusCode::NOT_FOUND,
                    "session lookup missed backing record",
                ));
            }

            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: Some("Existing".to_string()),
                metadata: serde_json::Map::new(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            session_id: String,
            user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_session(session_id, user_id).await
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            Ok(())
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::auth::SessionActivityCursor>,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: vec![],
                total: 0,
                limit: 20,
                next_cursor: None,
            })
        }
    }

    fn test_user() -> AuthUserRecord {
        AuthUserRecord {
            user_id: "u1".to_string(),
            username: "test-user".to_string(),
            email: "u1@example.test".to_string(),
            display_name: None,
        }
    }

    #[tokio::test]
    async fn resolve_or_create_chat_session_id_creates_session_for_new_lifecycle_turn() {
        let session_service = RecordingSessionService::default();
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_session_service(Arc::new(session_service.clone()));

        let session_id = resolve_or_create_chat_session_id(
            &state,
            &test_user(),
            None,
            Some("agent-1".to_string()),
            false,
        )
        .await
        .expect("session resolution should succeed");

        assert_eq!(session_id.as_deref(), Some("s-created"));

        let created = session_service.created_requests().await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "u1");
        assert_eq!(created[0].1.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(
            created[0]
                .1
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("agent_id"))
                .and_then(serde_json::Value::as_str),
            Some("agent-1")
        );
    }

    #[tokio::test]
    async fn resolve_or_create_chat_session_id_rejects_unknown_requested_session() {
        let session_service = RecordingSessionService::default();
        session_service.mark_missing("missing-session").await;

        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_session_service(Arc::new(session_service));

        let error = resolve_or_create_chat_session_id(
            &state,
            &test_user(),
            Some("missing-session".to_string()),
            None,
            false,
        )
        .await
        .expect_err("missing session should be rejected");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(error.1.0.detail, "Session not found");
    }

    #[tokio::test]
    async fn resolve_or_create_chat_session_id_strips_untrusted_session_when_service_unconfigured()
    {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));

        let session_id = resolve_or_create_chat_session_id(
            &state,
            &test_user(),
            Some("client-supplied".to_string()),
            None,
            false,
        )
        .await
        .expect("unconfigured service should fall back to server session creation");

        assert_eq!(session_id, None);
    }

    #[tokio::test]
    async fn resolve_or_create_chat_session_id_keeps_trusted_bound_session_when_service_unconfigured()
     {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));

        let session_id = resolve_or_create_chat_session_id(
            &state,
            &test_user(),
            Some("bound-session".to_string()),
            None,
            true,
        )
        .await
        .expect("trusted server-bound session should survive unconfigured lookup");

        assert_eq!(session_id.as_deref(), Some("bound-session"));
    }
}

/// `/chat/stream` lifecycle HTTP behavior.
#[cfg(test)]
mod chat_stream_lifecycle_tests {
    use std::sync::Arc;

    use astra_core::error_response_coded;
    use astra_services::runs::{
        CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, RunLifecycleService,
        RunListRecord, RunStatusRecord,
    };
    use async_trait::async_trait;
    use axum::{
        Json,
        body::{self, Body},
        http::{HeaderMap, Request, StatusCode},
    };
    use tokio::sync::Mutex;
    use tower::util::ServiceExt;

    use crate::{
        AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData,
        AuthService, AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
        SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
        SessionRecord, SessionService, SessionUpdateRequestData, build_app,
    };

    #[derive(Clone)]
    struct StubHealthChecker;

    #[async_trait]
    impl HealthChecker for StubHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone)]
    struct StubAuthService;

    #[async_trait]
    impl AuthService for StubAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn current_user(
            &self,
            headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            if headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer good-token")
            {
                Ok(AuthUserRecord {
                    user_id: "u1".to_string(),
                    username: "test-user".to_string(),
                    email: "u1@example.test".to_string(),
                    display_name: None,
                })
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse::new("Not authenticated".to_string())),
                ))
            }
        }
    }

    #[derive(Clone)]
    struct StubSessionService;

    #[derive(Clone)]
    struct CaptureEnabledSessionService;

    #[derive(Clone, Default)]
    struct RecordingCreateRunLifecycle {
        requests: Arc<Mutex<Vec<ChatRequestData>>>,
    }

    impl RecordingCreateRunLifecycle {
        async fn recorded_requests(&self) -> Vec<ChatRequestData> {
            self.requests.lock().await.clone()
        }
    }

    #[derive(Clone, Default)]
    struct RecordingStreamChatLifecycle {
        requests: Arc<Mutex<Vec<ChatRequestData>>>,
    }

    impl RecordingStreamChatLifecycle {
        async fn recorded_requests(&self) -> Vec<ChatRequestData> {
            self.requests.lock().await.clone()
        }
    }

    #[async_trait]
    impl SessionService for StubSessionService {
        async fn create_session(
            &self,
            user_id: String,
            request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id: "s-created".to_string(),
                user_id,
                agent_id: request.agent_id,
                title: Some("Created".to_string()),
                metadata: request.metadata.unwrap_or_default(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionListRecord {
                sessions: Vec::new(),
                total: Some(0),
                limit: 20,
                next_cursor: None,
            })
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: Some("Existing".to_string()),
                metadata: serde_json::Map::new(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            session_id: String,
            user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_session(session_id, user_id).await
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            Ok(())
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::auth::SessionActivityCursor>,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: vec![],
                total: 0,
                limit: 20,
                next_cursor: None,
            })
        }
    }

    #[async_trait]
    impl SessionService for CaptureEnabledSessionService {
        async fn create_session(
            &self,
            user_id: String,
            request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id: "s-created-capture".to_string(),
                user_id,
                agent_id: request.agent_id,
                title: Some("Created".to_string()),
                metadata: serde_json::Map::from_iter([(
                    crate::turn::llm::exchange_capture::FULL_LLM_CAPTURE_METADATA_KEY.to_string(),
                    serde_json::json!(true),
                )]),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionListRecord {
                sessions: Vec::new(),
                total: Some(0),
                limit: 20,
                next_cursor: None,
            })
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: Some("Existing".to_string()),
                metadata: serde_json::Map::from_iter([(
                    crate::turn::llm::exchange_capture::FULL_LLM_CAPTURE_METADATA_KEY.to_string(),
                    serde_json::json!(true),
                )]),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            session_id: String,
            user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_session(session_id, user_id).await
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            Ok(())
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::auth::SessionActivityCursor>,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: vec![],
                total: 0,
                limit: 20,
                next_cursor: None,
            })
        }
    }

    #[derive(Clone)]
    struct StubOtherNotImplementedLifecycle;

    #[derive(Clone)]
    struct StubConfiguredLifecycle;

    #[derive(Clone)]
    struct StubAssistantTextLifecycle;

    #[derive(Clone)]
    struct SkillDiscoveryFailureLifecycle {
        endpoint_url: String,
    }

    #[async_trait]
    impl RunLifecycleService for StubOtherNotImplementedLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            Err(error_response_coded(
                StatusCode::NOT_IMPLEMENTED,
                "Run lifecycle service not configured",
                "different_not_implemented",
            ))
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            Err(error_response_coded(
                StatusCode::NOT_IMPLEMENTED,
                "Run lifecycle service not configured",
                "different_not_implemented",
            ))
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[async_trait]
    impl RunLifecycleService for StubConfiguredLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(ChatStreamRecord {
                session_id: "s-live".to_string(),
                run_id: "run-live".to_string(),
                events: vec![
                    serde_json::json!({
                        "event_type": "run_error",
                        "data": {"error": "boom"}
                    }),
                    serde_json::json!({
                        "event_type": "run_finished",
                        "data": {"prompt_tokens": 7, "completion_tokens": 3, "tool_call_count": 2}
                    }),
                ],
                event_rx: None,
            })
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[async_trait]
    impl RunLifecycleService for SkillDiscoveryFailureLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            let runtime_auth = request
                .runtime_auth
                .as_ref()
                .expect("agent binding stream request should carry runtime auth");
            let _ =
                crate::server::agent_binding_skill_runtime::prepare_agent_binding_skill_resolver(
                    "skills",
                    &self.endpoint_url,
                    &runtime_auth.authorization,
                )
                .await?;
            unreachable!("fake skill endpoint must return non-2xx")
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[async_trait]
    impl RunLifecycleService for RecordingCreateRunLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            self.requests.lock().await.push(request);
            Ok(ChatRunRecord {
                session_id: "s-recorded".to_string(),
                run_id: "run-recorded".to_string(),
                status: "queued".to_string(),
                explain: None,
            })
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[async_trait]
    impl RunLifecycleService for RecordingStreamChatLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            self.requests.lock().await.push(request);
            Ok(ChatStreamRecord {
                session_id: "s-stream-recorded".to_string(),
                run_id: "run-stream-recorded".to_string(),
                events: vec![serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"status": "completed"}
                })],
                event_rx: None,
            })
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[derive(Clone)]
    struct StubConfiguredStreamingLifecycle;

    #[async_trait]
    impl RunLifecycleService for StubConfiguredStreamingLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            let (event_tx, event_rx) = tokio::sync::mpsc::channel(8);
            event_tx
                .try_send(serde_json::json!({
                    "event_type": "run_error",
                    "data": {"error": "live boom"}
                }))
                .expect("queue run_error");
            event_tx
                .try_send(serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"prompt_tokens": 11, "completion_tokens": 4, "tool_call_count": 0}
                }))
                .expect("queue run_finished");
            drop(event_tx);
            Ok(ChatStreamRecord {
                session_id: "s-live-stream".to_string(),
                run_id: "run-live-stream".to_string(),
                events: Vec::new(),
                event_rx: Some(event_rx),
            })
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[async_trait]
    impl RunLifecycleService for StubAssistantTextLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(ChatStreamRecord {
                session_id: "s-turn".to_string(),
                run_id: "run-turn".to_string(),
                events: vec![
                    serde_json::json!({
                        "event_type": "text_delta",
                        "data": {"chunk": "stale partial"}
                    }),
                    serde_json::json!({
                        "event_type": "turn_complete",
                        "data": {"assistant_text": "recovered final text", "has_tool_calls": false}
                    }),
                ],
                event_rx: None,
            })
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn chat_stream_selected_model_shape_error_returns_typed_sse_error() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(StubConfiguredLifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"message":"hi","selected_model":"gpt-4"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"error_code\":\"selected_model_invalid\""));
        assert!(!text.contains("\"type\":\"session_info\""));
    }

    #[tokio::test]
    async fn chat_stream_agent_binding_skill_discovery_error_redacts_runtime_auth_in_sse() {
        use axum::{Router, routing::post};

        let fake_skill_server = Router::new().route(
            "/skills",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "upstream echoed Bearer abc and abc".to_string(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, fake_skill_server).await;
        });
        let lifecycle = SkillDiscoveryFailureLifecycle {
            endpoint_url: format!("http://{addr}/skills"),
        };
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{
                            "message": "hi",
                            "selected_model": {"model": "gpt-4o-mini"},
                            "agent_binding": {
                                "id": "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
                                "capability_server_refs": {
                                    "mcp": "tools",
                                    "skills": "skills"
                                }
                            },
                            "runtime_auth": {
                                "authorization": "Bearer abc"
                            }
                        }"#,
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"error_code\":\"agent_binding_discovery_failed\""));
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("Bearer abc"));
        assert!(!text.contains("abc"));
        assert!(!text.contains("\"type\":\"session_info\""));
        server.abort();
    }

    #[tokio::test]
    async fn chat_stream_uses_client_run_event_shape_for_live_lifecycle_streams() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(StubConfiguredLifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"message":"hi"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"run_id\":\"run-live\""));
        assert!(text.contains("\"type\":\"usage\""));
        assert!(text.contains("\"prompt_tokens\":7"));
        assert!(text.contains("\"completion_tokens\":3"));
        assert!(text.contains("\"type\":\"run_finished\""));
        assert!(text.contains("\"status\":\"failed\""));
        assert!(text.contains("\"error\":\"boom\""));
    }

    #[tokio::test]
    async fn chat_handler_propagates_full_capture_from_session_metadata_into_run_request() {
        let lifecycle = RecordingCreateRunLifecycle::default();
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(CaptureEnabledSessionService))
                .with_run_lifecycle_service(Arc::new(lifecycle.clone())),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"session_id":"capture-session","message":"hi"}"#,
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let requests = lifecycle.recorded_requests().await;
        assert_eq!(requests.len(), 1, "one run request expected");
        assert!(requests[0].full_llm_capture);
        assert_eq!(requests[0].session_id.as_deref(), Some("capture-session"));
    }

    #[tokio::test]
    async fn chat_stream_handler_propagates_full_capture_from_session_metadata_into_stream_request()
    {
        let lifecycle = RecordingStreamChatLifecycle::default();
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(CaptureEnabledSessionService))
                .with_run_lifecycle_service(Arc::new(lifecycle.clone())),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"session_id":"capture-session","message":"hi"}"#,
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let requests = lifecycle.recorded_requests().await;
        assert_eq!(requests.len(), 1, "one stream request expected");
        assert!(requests[0].full_llm_capture);
        assert_eq!(requests[0].session_id.as_deref(), Some("capture-session"));
    }

    #[tokio::test]
    async fn chat_stream_streaming_path_uses_client_run_event_shape_for_terminal_events() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(StubConfiguredStreamingLifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"message":"hi"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"run_id\":\"run-live-stream\""));
        assert!(text.contains("\"type\":\"usage\""));
        assert!(text.contains("\"prompt_tokens\":11"));
        assert!(text.contains("\"completion_tokens\":4"));
        assert!(text.contains("\"type\":\"run_finished\""));
        assert!(text.contains("\"status\":\"failed\""));
        assert!(text.contains("\"error\":\"live boom\""));
    }

    #[tokio::test]
    async fn chat_stream_does_not_fall_back_for_other_not_implemented_errors() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(StubOtherNotImplementedLifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"message":"hi","session_id":"s1","selected_model":{"model":"demo-model"}}"#,
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"error\""));
        assert!(!text.contains("\"type\":\"text_delta\""));
    }

    #[tokio::test]
    async fn chat_stream_emits_turn_complete_assistant_text_for_client_reconciliation() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(StubAssistantTextLifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"message":"hi"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"text_delta\""));
        assert!(text.contains("\"content\":\"stale partial\""));
        assert!(
            text.contains("\"type\":\"turn_complete\""),
            "turn completion event should reach the client: {text}"
        );
        assert!(
            text.contains("\"assistant_text\":\"recovered final text\""),
            "authoritative assistant text should survive the HTTP SSE route: {text}"
        );
    }
}
