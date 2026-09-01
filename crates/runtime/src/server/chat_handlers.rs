use super::header_utils::collect_forward_headers;
use super::*;
use crate::server::{
    provider_runtime_context::inject_effective_runtime_context,
    run::handlers::transform_stream_run_events_for_client,
};
use axum::Extension;

fn parse_chat_request_body(body: &Bytes) -> Result<ChatRequest, (StatusCode, Json<ErrorResponse>)> {
    serde_json::from_slice(body).map_err(|error| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("invalid chat request JSON: {error}"),
            "chat_request_invalid",
        )
    })
}

pub(super) async fn validate_conversation_authority(
    state: &AppState,
    authenticated_user_id: &str,
    session_id: Option<&str>,
    authority: Option<&astra_turn_types::ConversationAuthorityEnvelopeV1>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(authority) = authority else {
        // `/chat` and `/chat/stream` are Server-owned loops. They may start
        // without a client grant; the run lifecycle acquires authority
        // internally. A supplied grant, however, always fails closed.
        return Ok(());
    };
    authority.validate_shape().map_err(|error| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("invalid conversation authority envelope: {error}"),
            "conversation_authority_invalid",
        )
    })?;
    if authority.key.owner_user_id != authenticated_user_id
        || session_id != Some(authority.key.session_id.as_str())
    {
        return Err(error_response_coded(
            StatusCode::FORBIDDEN,
            "conversation authority does not match the authenticated owner and session",
            "conversation_authority_owner_mismatch",
        ));
    }
    let coordinator = state.session_context_coordinator.as_ref().ok_or_else(|| {
        error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "canonical session coordinator is unavailable",
            "session_coordinator_unavailable",
        )
    })?;
    let signer = state.execution_grant_signer.as_ref().ok_or_else(|| {
        error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "execution grant verifier is unavailable",
            "execution_grant_verifier_unavailable",
        )
    })?;
    let active_lease = coordinator
        .load_active_writer(&authority.key)
        .await
        .map_err(|error| {
            error_response_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("failed to load current session authority: {error}"),
                "session_authority_unavailable",
            )
        })?
        .ok_or_else(|| {
            error_response_coded(
                StatusCode::CONFLICT,
                "conversation authority no longer owns the active writer lease",
                "conversation_authority_fenced",
            )
        })?;
    let now_unix_ms = chrono::Utc::now().timestamp_millis();
    let claims = signer
        .verify(
            &authority.execution_grant,
            &active_lease,
            &authority.run_id,
            authority.run_generation,
            authority.provider_binding_id.as_deref(),
            authority.provider_generation,
            now_unix_ms,
        )
        .map_err(|error| {
            error_response_coded(
                StatusCode::CONFLICT,
                format!("conversation execution grant was rejected: {error}"),
                "conversation_authority_fenced",
            )
        })?;
    if claims.writer_epoch != authority.writer_epoch
        || claims.actor_id != authority.actor_id
        || authority.execution_grant.claims.lease_id != active_lease.lease_id
    {
        return Err(error_response_coded(
            StatusCode::CONFLICT,
            "conversation authority generations do not match the signed grant",
            "conversation_authority_fenced",
        ));
    }
    let head = coordinator
        .load_head(&authority.key)
        .await
        .map_err(|error| {
            error_response_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("failed to load canonical conversation head: {error}"),
                "session_head_unavailable",
            )
        })?;
    if head.as_ref().map(|head| &head.cursor) != authority.expected_cursor.as_ref()
        || authority.prompt_manifest_root.as_deref()
            != head.as_ref().map(|head| head.latest_manifest_root.as_str())
    {
        return Err(error_response_coded(
            StatusCode::CONFLICT,
            "conversation cursor or prompt manifest is stale",
            "conversation_cursor_conflict",
        ));
    }
    Ok(())
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
            validate_requested_chat_session_id(&session_id)?;

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
                Err(error) => Err(normalize_chat_session_error(error)),
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

fn validate_requested_chat_session_id(
    session_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    astra_services::validate_persisted_session_id(session_id).map_err(|error| {
        tracing::warn!(
            target: "astra_runtime::chat",
            session_id_bytes = session_id.len(),
            validation_error = %error,
            "rejected invalid requested session id"
        );
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "session_id is invalid",
            "session_id_invalid",
        )
    })
}

pub(super) fn is_session_service_unconfigured_error(
    error: &(StatusCode, Json<ErrorResponse>),
) -> bool {
    error.0 == StatusCode::NOT_IMPLEMENTED && error.1.0.detail == "Session service not configured"
}

fn normalize_chat_session_error(
    error: (StatusCode, Json<ErrorResponse>),
) -> (StatusCode, Json<ErrorResponse>) {
    let (status, detail) = error;
    if status == StatusCode::NOT_FOUND {
        error_response(StatusCode::NOT_FOUND, "Session not found")
    } else {
        (status, detail)
    }
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
    chat_data.agent_binding_owner_scope = Some(
        astra_services::AgentBindingOwnerScope::from_principal(&principal),
    );
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
    validate_conversation_authority(
        &state,
        &user.user_id,
        chat_data.session_id.as_deref(),
        chat_data.conversation_authority.as_ref(),
    )
    .await?;
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
    trace: Option<Extension<RequestTrace>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = trace
        .as_ref()
        .map(|Extension(trace)| trace.request_id.clone());
    let principal = match state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/chat/stream", &body),
        )
        .await
    {
        Ok(principal) => principal,
        Err((status, error)) => {
            return sse_error_response_from_error_with_request_id(
                status,
                error.0,
                request_id.as_deref(),
            );
        }
    };
    let request = match parse_chat_request_body(&body) {
        Ok(request) => request,
        Err((status, error)) => {
            return sse_error_response_from_error_with_request_id(
                status,
                error.0,
                request_id.as_deref(),
            );
        }
    };
    let user = principal.user.clone();

    let mut chat_data = chat_request_into_data(request);
    chat_data.agent_binding_owner_scope = Some(
        astra_services::AgentBindingOwnerScope::from_principal(&principal),
    );
    chat_data.forward_headers = collect_forward_headers(&headers);
    if let Some(Extension(trace)) = trace {
        chat_data
            .forward_headers
            .entry("x-request-id".to_string())
            .or_insert(trace.request_id);
    }
    let requested_session_id = chat_data.session_id.clone();
    let requested_session_id_for_diagnostics = requested_session_id
        .as_deref()
        .filter(|session_id| astra_services::validate_persisted_session_id(session_id).is_ok());
    let requested_agent_id = chat_data.agent_id.clone();
    let (resolved, runtime_context) = tokio::join!(
        resolve_or_create_chat_session(
            &state,
            &user,
            requested_session_id.clone(),
            requested_agent_id,
            false,
        ),
        inject_effective_runtime_context(&state, &principal, &mut chat_data),
    );
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err((status, error)) => {
            return sse_error_response_from_error_with_context(
                status,
                error.0,
                SseErrorContext {
                    request_id: request_id.as_deref(),
                    session_id: requested_session_id_for_diagnostics,
                    ..SseErrorContext::default()
                },
            );
        }
    };
    chat_data.session_id = resolved.session_id;
    chat_data.full_llm_capture = resolved.full_llm_capture;
    if let Err((status, error)) = validate_conversation_authority(
        &state,
        &user.user_id,
        chat_data.session_id.as_deref(),
        chat_data.conversation_authority.as_ref(),
    )
    .await
    {
        return sse_error_response_from_error_with_context(
            status,
            error.0,
            SseErrorContext {
                request_id: request_id.as_deref(),
                session_id: chat_data.session_id.as_deref(),
                ..SseErrorContext::default()
            },
        );
    }
    if let Err((status, error)) = runtime_context {
        return sse_error_response_from_error_with_context(
            status,
            error.0,
            SseErrorContext {
                request_id: request_id.as_deref(),
                session_id: chat_data.session_id.as_deref(),
                ..SseErrorContext::default()
            },
        );
    }
    let error_session_id = chat_data.session_id.clone();
    match state
        .execution
        .run_lifecycle_service
        .stream_chat(user.user_id.clone(), chat_data)
        .await
    {
        Ok(mut stream) => {
            if let Some(event_rx) = stream.event_rx.take() {
                // Incremental SSE streaming: convert channel into SSE body.
                let session_id = stream.session_id.clone();
                let run_id = stream.run_id.clone();
                sse_streaming_response(session_id, run_id, request_id.clone(), event_rx)
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
        Err((status, error)) => sse_error_response_from_error_with_context(
            status,
            error.0,
            SseErrorContext {
                request_id: request_id.as_deref(),
                session_id: error_session_id.as_deref(),
                ..SseErrorContext::default()
            },
        ),
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
        looked_up_session_ids: Arc<Mutex<Vec<String>>>,
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

        async fn looked_up_session_ids(&self) -> Vec<String> {
            self.looked_up_session_ids.lock().await.clone()
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
            self.looked_up_session_ids
                .lock()
                .await
                .push(session_id.clone());
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
    async fn resolve_or_create_chat_session_id_rejects_invalid_id_before_lookup() {
        let session_service = RecordingSessionService::default();
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_session_service(Arc::new(session_service.clone()));
        let invalid_session_id = "x".repeat(4 * 1024 * 1024);

        let error = resolve_or_create_chat_session_id(
            &state,
            &test_user(),
            Some(invalid_session_id),
            None,
            false,
        )
        .await
        .expect_err("oversized session id must be rejected");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1.0.error_code.as_deref(), Some("session_id_invalid"));
        assert!(session_service.looked_up_session_ids().await.is_empty());
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

    fn sse_json_events(body: &str) -> Vec<serde_json::Value> {
        body.split("\n\n")
            .filter_map(|frame| frame.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).expect("SSE data must be JSON"))
            .collect()
    }

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
            let agent_binding_ids = if request.agent_bindings.is_empty() {
                request
                    .agent_binding
                    .iter()
                    .map(|binding| binding.id.clone())
                    .collect::<Vec<_>>()
            } else {
                request
                    .agent_bindings
                    .iter()
                    .map(|binding| binding.id.clone())
                    .collect::<Vec<_>>()
            };
            let _ =
                crate::server::agent_binding_skill_runtime::prepare_agent_binding_skill_resolver(
                    "skills",
                    &self.endpoint_url,
                    &runtime_auth.authorization,
                    &agent_binding_ids,
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
    async fn chat_stream_model_selection_shape_error_returns_typed_sse_error() {
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
                    .body(Body::from(
                        r#"{"message":"hi","model_selection":"offer-gpt-4"}"#,
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
        assert!(text.contains("\"error_code\":\"chat_request_invalid\""));
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
                            "model_selection": {"offering_id": "offer-gpt-4o-mini"},
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
        assert!(
            text.contains("\"error_code\":\"agent_binding_discovery_failed\""),
            "unexpected SSE body: {text}"
        );
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
        let events = sse_json_events(&text);
        assert!(
            events
                .iter()
                .any(|event| { event["type"] == "session_info" && event["run_id"] == "run-live" })
        );
        let usage = events
            .iter()
            .find(|event| event["type"] == "usage")
            .expect("terminal usage event");
        assert_eq!(usage["input_tokens"], 7);
        assert_eq!(usage["output_tokens"], 3);
        assert_eq!(usage["total_tokens"], 10);
        assert!(usage.get("prompt_tokens").is_none());
        assert!(usage.get("completion_tokens").is_none());
        assert!(events.iter().any(|event| {
            event["type"] == "run_finished"
                && event["status"] == "failed"
                && event["error"] == "boom"
        }));
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
        let response_request_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("request trace middleware must return x-request-id")
            .to_string();
        let requests = lifecycle.recorded_requests().await;
        assert_eq!(requests.len(), 1, "one stream request expected");
        assert!(requests[0].full_llm_capture);
        assert_eq!(requests[0].session_id.as_deref(), Some("capture-session"));
        assert_eq!(
            requests[0]
                .forward_headers
                .get("x-request-id")
                .map(String::as_str),
            Some(response_request_id.as_str()),
            "generated request id must reach the background runtime request"
        );
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
        let events = sse_json_events(&text);
        assert!(events.iter().any(|event| {
            event["type"] == "session_info" && event["run_id"] == "run-live-stream"
        }));
        let usage = events
            .iter()
            .find(|event| event["type"] == "usage")
            .expect("terminal usage event");
        assert_eq!(usage["input_tokens"], 11);
        assert_eq!(usage["output_tokens"], 4);
        assert_eq!(usage["total_tokens"], 15);
        assert!(usage.get("prompt_tokens").is_none());
        assert!(usage.get("completion_tokens").is_none());
        assert!(events.iter().any(|event| {
            event["type"] == "run_finished"
                && event["status"] == "failed"
                && event["error"] == "live boom"
        }));
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
                        r#"{"message":"hi","session_id":"s1","model_selection":{"offering_id":"offer-demo-model"}}"#,
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
