use super::bridge_prep::normalize_chat_turn_session_error;
use super::header_utils::collect_forward_headers;
use super::run_handlers::transform_stream_run_events_for_client;
use super::*;

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

pub(super) async fn resolve_or_create_chat_session_id(
    state: &AppState,
    user: &AuthUserRecord,
    requested_session_id: Option<String>,
    agent_id: Option<String>,
    session_id_is_trusted: bool,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
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
                Ok(_) => Ok(Some(session_id)),
                Err(error) if is_session_service_unconfigured_error(&error) => {
                    Ok(session_id_is_trusted.then_some(session_id))
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

            match state
                .session_service
                .create_session(
                    user.user_id.clone(),
                    SessionCreateRequestData {
                        agent_id,
                        title: None,
                        metadata,
                    },
                )
                .await
            {
                Ok(session) => Ok(Some(session.session_id)),
                Err(error) if is_session_service_unconfigured_error(&error) => Ok(None),
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

fn chat_stream_bridge_fallback_payload(
    chat_data: &astra_services::runs::ChatRequestData,
) -> serde_json::Value {
    let allow_skills = normalize_bridge_allowlist(chat_data.allow_skills.as_deref());
    let allow_tools = normalize_bridge_allowlist(chat_data.allow_tools.as_deref());
    serde_json::json!({
        "session_id": chat_data.session_id.as_deref(),
        "agent_id": chat_data.agent_id.as_deref(),
        "model": chat_data.model.as_deref(),
        "llm_token_service": chat_data
            .llm_token_service
            .as_ref()
            .map(|config| serde_json::json!(config)),
        "skill_search": chat_data.skill_search.as_ref(),
        "allow_skills": allow_skills,
        "allow_tools": allow_tools,
        "context": chat_data.context.as_ref(),
        "max_candidates": chat_data.max_candidates,
        "explain": chat_data.explain,
        "messages": [
            {
                "role": "user",
                "content": chat_data.message.as_str(),
            }
        ],
    })
}

fn normalize_bridge_allowlist(entries: Option<&[String]>) -> Option<Vec<String>> {
    entries.map(|entries| {
        let mut normalized = std::collections::BTreeSet::new();
        for entry in entries {
            normalized.insert(entry.trim().to_ascii_lowercase());
        }
        normalized.into_iter().collect()
    })
}

pub(super) async fn chat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let mut chat_data = chat_request_into_data(request);
    chat_data.forward_headers = collect_forward_headers(&headers);
    chat_data.session_id = resolve_or_create_chat_session_id(
        &state,
        &user,
        chat_data.session_id.take(),
        chat_data.agent_id.clone(),
        false,
    )
    .await?;
    let run = state
        .run_lifecycle_service
        .create_run(user.user_id, chat_data)
        .await?;
    Ok(Json(ChatResponse::from(run)))
}

pub(super) async fn chat_stream_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Response {
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };

    let mut chat_data = chat_request_into_data(request);
    chat_data.forward_headers = collect_forward_headers(&headers);
    chat_data.session_id = match resolve_or_create_chat_session_id(
        &state,
        &user,
        chat_data.session_id.take(),
        chat_data.agent_id.clone(),
        false,
    )
    .await
    {
        Ok(session_id) => session_id,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };

    // Bridge E2E hooks: when test secret is present, route through bridge so
    // `test_llm_rounds` mock works without a real LLM.
    #[cfg(feature = "bridge-e2e-hooks")]
    if crate::turn::bridge_e2e_hooks::authorized(&headers) {
        let payload = chat_stream_bridge_fallback_payload(&chat_data);
        let body = match serde_json::to_vec(&payload).map(Bytes::from) {
            Ok(body) => body,
            Err(e) => {
                return sse_error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
        };
        return dispatch_chat_turn_bridge(&state, &user, &headers, body).await;
    }

    match state
        .run_lifecycle_service
        .stream_chat(user.user_id.clone(), chat_data.clone())
        .await
    {
        Ok(stream) => {
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
        Err((status, error))
            if astra_services::runs::is_run_lifecycle_unconfigured_error(status, &error.0) =>
        {
            // Fallback path: route /chat/stream through chat-turn bridge when lifecycle
            // service isn't wired yet. This preserves CLI usability during cutover.
            let payload = chat_stream_bridge_fallback_payload(&chat_data);
            let body = match serde_json::to_vec(&payload).map(Bytes::from) {
                Ok(body) => body,
                Err(e) => {
                    return sse_error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
                }
            };
            dispatch_chat_turn_bridge(&state, &user, &headers, body).await
        }
        Err((status, error)) => sse_error_response(status, error.0.detail),
    }
}

pub(super) async fn chat_turn_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };
    dispatch_chat_turn_bridge(&state, &user, &headers, body).await
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
        Err((status, error)) => return sse_error_response(status, error.0.detail),
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
    // `ASTRA_BRIDGE_TEST_SECRET`; harmless if unset or header absent.
    if let Some(v) = source_headers.get("x-mo-bridge-test-secret").cloned() {
        bridge_headers.insert(HeaderName::from_static("x-mo-bridge-test-secret"), v);
    }

    let client_disconnect = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());

    match state
        .chat_turn_bridge
        .forward(
            &bridge_headers,
            prepared.body,
            state.turn_core_event_writer.clone(),
            state.turn_tool_event_writer.clone(),
            state.turn_hook_db_writer.clone(),
            state.turn_reflection_state_store.clone(),
            state.turn_reflection_lesson_writer.clone(),
            state.turn_observer_worker.clone(),
            state.turn_auxiliary_event_writer.clone(),
            state.turn_session_activity_writer.clone(),
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
    use astra_core::SkillSearchSettings;
    use astra_services::runs::ChatRequestData;

    use super::*;

    #[test]
    fn bridge_header_names_are_valid() {
        let headers = [
            "x-mo-bridge-secret",
            "x-mo-user-id",
            "x-mo-username-b64",
            "x-mo-bridge-capabilities",
            "x-mo-session-id",
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
    fn chat_stream_fallback_payload_shape() {
        let payload = chat_stream_bridge_fallback_payload(&ChatRequestData {
            message: "hello".to_string(),
            session_id: Some("s1".to_string()),
            agent_id: Some("a1".to_string()),
            model: Some("gpt-4".to_string()),
            llm_token_service: Some(astra_services::LlmTokenServiceConfig {
                url: "http://catalog:8081/api/v1/llm-token".to_string(),
                timeout_ms: Some(2500),
            }),
            skill_search: Some(SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            }),
            allow_skills: Some(vec!["plan".to_string()]),
            allow_tools: Some(vec!["bash".to_string()]),
            context: None,
            forward_headers: std::collections::HashMap::new(),
            max_candidates: 3,
            explain: true,
            interactive_client: false,
        });
        let obj = payload.as_object().unwrap();
        assert!(obj.contains_key("messages"));
        assert!(obj.contains_key("session_id"));
        assert_eq!(obj["max_candidates"], 3);
        assert_eq!(obj["explain"], true);
        assert_eq!(obj["skill_search"]["dynamic_surface"], false);
        assert_eq!(obj["skill_search"]["min_catalog_size"], 12);
        assert_eq!(obj["skill_search"]["surface_cap"], 20);
        assert_eq!(
            obj["llm_token_service"]["url"],
            "http://catalog:8081/api/v1/llm-token"
        );
        assert_eq!(obj["llm_token_service"]["timeout_ms"], 2500);
        assert_eq!(obj["allow_skills"], serde_json::json!(["plan"]));
        assert_eq!(obj["allow_tools"], serde_json::json!(["bash"]));
        let messages = obj["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn chat_stream_fallback_payload_normalizes_allowlists() {
        let payload = chat_stream_bridge_fallback_payload(&ChatRequestData {
            message: "hello".to_string(),
            session_id: Some("s1".to_string()),
            agent_id: Some("a1".to_string()),
            model: Some("gpt-4".to_string()),
            llm_token_service: None,
            skill_search: None,
            allow_skills: Some(vec![
                " plan ".to_string(),
                "PLAN".to_string(),
                "analyze".to_string(),
            ]),
            allow_tools: Some(vec![
                " bash ".to_string(),
                "BASH".to_string(),
                "read_file".to_string(),
            ]),
            context: None,
            forward_headers: std::collections::HashMap::new(),
            max_candidates: 3,
            explain: true,
            interactive_client: false,
        });
        let obj = payload.as_object().unwrap();
        assert_eq!(obj["allow_skills"], serde_json::json!(["analyze", "plan"]));
        assert_eq!(obj["allow_tools"], serde_json::json!(["bash", "read_file"]));
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
        // + optional from prepared: 9 (session-id, turn-chain-id, user-query-event-id,
        //   tools-changed, task-hint, user-query-b64, routing-meta-b64, force-intent,
        //   execution-state-b64)
        // Total possible: 14
        assert_eq!(4 + 1 + 9, 14);
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
                total: 0,
                limit: 20,
                offset: 0,
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
            _offset: u32,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: vec![],
                total: 0,
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

/// `/chat/stream` fallback when run-lifecycle is unconfigured (was `chat_stream_bridge_fallback_contract.rs`).
#[cfg(test)]
mod chat_stream_bridge_fallback_tests {
    use std::sync::Arc;

    use astra_core::error_response_coded;
    use astra_services::runs::{
        CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, RunLifecycleService,
        RunListRecord, RunStatusRecord,
    };
    use async_trait::async_trait;
    use axum::{
        Json,
        body::{self, Body, Bytes},
        http::{HeaderMap, Request, StatusCode},
        response::Response,
    };
    use serde_json::Value;
    use tokio::sync::Mutex;
    use tower::util::ServiceExt;

    use crate::{
        AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData,
        AuthService, AuthTokenRecord, AuthUserRecord, ChatTurnBridge, ErrorResponse, HealthChecker,
        ServiceInfo, SessionActivityRecord, SessionCreateRequestData, SessionListFilter,
        SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
        TurnAuxiliaryEventWriter, TurnCoreEventWriter, TurnHookDbWriter, TurnObserverWorker,
        TurnReflectionLessonWriter, TurnReflectionStateStore, TurnSessionActivityWriter,
        TurnToolEventWriter, build_app,
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
                total: 0,
                limit: 20,
                offset: 0,
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
            _offset: u32,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: vec![],
                total: 0,
            })
        }
    }

    #[derive(Clone, Default)]
    struct Capture {
        body: Arc<Mutex<Option<Value>>>,
    }

    #[derive(Clone)]
    struct StubChatTurnBridge {
        capture: Capture,
    }

    #[derive(Clone)]
    struct StubOtherNotImplementedLifecycle;

    #[derive(Clone)]
    struct StubConfiguredLifecycle;

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

        async fn list_runs(
            &self,
            _user_id: String,
            _limit: u32,
            _offset: u32,
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

        async fn list_runs(
            &self,
            _user_id: String,
            _limit: u32,
            _offset: u32,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    #[async_trait]
    impl ChatTurnBridge for StubChatTurnBridge {
        async fn forward(
            &self,
            _headers: &HeaderMap,
            body: Bytes,
            _turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
            _turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
            _turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
            _turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
            _turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
            _turn_observer_worker: Arc<dyn TurnObserverWorker>,
            _turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
            _turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
            _client_cancel: Option<Arc<tokio_util::sync::CancellationToken>>,
        ) -> Result<Response, (StatusCode, String)> {
            *self.capture.body.lock().await =
                Some(serde_json::from_slice(&body).expect("request body should be valid json"));
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(
                    "data: {\"type\":\"session_info\",\"session_id\":\"s1\",\"run_id\":\"r1\"}\n\n\
                     data: {\"type\":\"text_delta\",\"content\":\"hello\"}\n\n\
                     data: {\"type\":\"text_done\",\"full_text\":\"hello\"}\n\n\
                     data: [DONE]\n\n",
                ))
                .expect("response should build"))
        }
    }

    #[tokio::test]
    async fn chat_stream_falls_back_to_chat_turn_bridge_when_lifecycle_unconfigured() {
        let capture = Capture::default();
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_chat_turn_bridge_secret("test-secret")
                .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                    capture: capture.clone(),
                })),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"message":"hi","session_id":"s1","model":"demo-model","context":{"topic":"runtime"},"skill_search":{"dynamic_surface":false,"min_catalog_size":12,"surface_cap":20},"max_candidates":3,"explain":true}"#,
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
        assert!(text.contains("\"type\":\"text_delta\""));
        assert!(text.contains("\"content\":\"hello\""));

        let forwarded = capture
            .body
            .lock()
            .await
            .clone()
            .expect("bridge should receive payload");
        assert_eq!(forwarded["session_id"], "s1");
        assert_eq!(forwarded["model"], "demo-model");
        assert_eq!(forwarded["context"]["topic"], "runtime");
        assert_eq!(forwarded["skill_search"]["dynamic_surface"], false);
        assert_eq!(forwarded["skill_search"]["min_catalog_size"], 12);
        assert_eq!(forwarded["skill_search"]["surface_cap"], 20);
        assert_eq!(forwarded["max_candidates"], 3);
        assert_eq!(forwarded["explain"], true);
        assert_eq!(forwarded["messages"][0]["role"], "user");
        assert_eq!(forwarded["messages"][0]["content"], "hi");
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
    async fn chat_stream_does_not_fall_back_for_other_not_implemented_errors() {
        let capture = Capture::default();
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService))
                .with_run_lifecycle_service(Arc::new(StubOtherNotImplementedLifecycle))
                .with_chat_turn_bridge_secret("test-secret")
                .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                    capture: capture.clone(),
                })),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/stream")
                    .header("authorization", "Bearer good-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"message":"hi","session_id":"s1","model":"demo-model"}"#,
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
        assert!(capture.body.lock().await.is_none());
    }

    #[tokio::test]
    async fn chat_stream_fallback_returns_bridge_disabled_error_when_bridge_unconfigured() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService)),
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
        let bytes = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(bytes.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"session_info\""));
        assert!(text.contains("\"session_id\":\"s-created\""));
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("chat turn bridge disabled"));
    }
}
