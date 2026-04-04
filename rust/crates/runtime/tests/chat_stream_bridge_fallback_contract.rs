use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ChatTurnBridge, ErrorResponse, HealthChecker, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, TurnAuxiliaryEventWriter,
    TurnCoreEventWriter, TurnHookDbWriter, TurnObserverWorker, TurnReflectionLessonWriter,
    TurnReflectionStateStore, TurnSessionActivityWriter, TurnToolEventWriter, build_app,
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
                Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
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
    assert_eq!(forwarded["messages"][0]["role"], "user");
    assert_eq!(forwarded["messages"][0]["content"], "hi");
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
    assert!(text.contains("\"type\":\"error\""));
    assert!(text.contains("chat turn bridge disabled"));
}
