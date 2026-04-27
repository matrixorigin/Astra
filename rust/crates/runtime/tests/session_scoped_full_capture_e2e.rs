use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord,
    ErrorResponse, HealthChecker, RunLifecycleService, RunListRecord, RunStatusRecord, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, build_app,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

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
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer test-capture-token") => Ok(AuthUserRecord {
                user_id: "test-user-1".to_string(),
                username: "capture-user".to_string(),
                email: "capture@test.local".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new("bad token".to_string())),
            )),
        }
    }
}

#[derive(Clone)]
struct CaptureEnabledSessionService;

#[async_trait]
impl SessionService for CaptureEnabledSessionService {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id: "capture-created".to_string(),
            user_id,
            agent_id: request.agent_id,
            title: Some("Created".to_string()),
            metadata: serde_json::Map::from_iter([("full_llm_capture".to_string(), json!(true))]),
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
            metadata: serde_json::Map::from_iter([("full_llm_capture".to_string(), json!(true))]),
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
struct RecordingLifecycle {
    create_requests: Arc<Mutex<Vec<ChatRequestData>>>,
}

impl RecordingLifecycle {
    async fn recorded_create_requests(&self) -> Vec<ChatRequestData> {
        self.create_requests.lock().await.clone()
    }
}

#[async_trait]
impl RunLifecycleService for RecordingLifecycle {
    async fn create_run(
        &self,
        _user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| "capture-session".to_string());
        self.create_requests.lock().await.push(request);
        Ok(ChatRunRecord {
            session_id,
            run_id: "run-capture-ws".to_string(),
            status: "queued".to_string(),
            explain: None,
        })
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(ChatStreamRecord {
            session_id: "capture-session".to_string(),
            run_id: "run-capture-http".to_string(),
            events: vec![json!({
                "event_type": "run_finished",
                "data": {"status": "completed"}
            })],
            event_rx: None,
        })
    }

    async fn get_run_status(
        &self,
        run_id: String,
        _user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(RunStatusRecord {
            run_id,
            session_id: "capture-session".to_string(),
            status: "completed".to_string(),
            waiting_for: None,
            events_count: 1,
        })
    }

    async fn stream_run(
        &self,
        run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        Ok(vec![json!({
            "event_type": "run_finished",
            "data": {"run_id": run_id, "status": "completed"}
        })])
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<astra_runtime::CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
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

async fn spawn_test_server() -> (
    std::net::SocketAddr,
    RecordingLifecycle,
    tokio::task::JoinHandle<()>,
) {
    let lifecycle = RecordingLifecycle::default();
    let state = AppState::new(
        ServiceInfo::new("capture-e2e-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_session_service(Arc::new(CaptureEnabledSessionService))
    .with_run_lifecycle_service(Arc::new(lifecycle.clone()));

    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to ephemeral port");
    let addr = listener.local_addr().expect("listener addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve app");
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, lifecycle, handle)
}

#[tokio::test]
async fn browser_ws_chat_propagates_session_scoped_full_capture_over_real_websocket() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    let url = format!("ws://{addr}/chat/ws");
    let (mut ws, _) = connect_async(&url).await.expect("WS connect");

    ws.send(Message::Text(
        json!({"type": "auth", "token": "Bearer test-capture-token"})
            .to_string()
            .into(),
    ))
    .await
    .expect("auth send should succeed");

    let auth_ok = ws.next().await.expect("auth response").expect("auth frame");
    let auth_json: serde_json::Value =
        serde_json::from_str(&auth_ok.into_text().expect("auth text")).expect("auth json");
    assert_eq!(auth_json["type"], "auth_ok");
    assert_eq!(auth_json["user_id"], "test-user-1");

    ws.send(Message::Text(
        json!({
            "type": "message",
            "content": "hello over websocket",
            "session_id": "capture-session"
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("chat send should succeed");

    let mut seen_session_info = false;
    let mut seen_run_started = false;
    let mut seen_run_finished = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline
        && !(seen_session_info && seen_run_started && seen_run_finished)
    {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("WS response should arrive before timeout");
        let message = next.expect("server frame").expect("valid ws frame");
        match message {
            Message::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                let payload: serde_json::Value = serde_json::from_str(&text).expect("ws json");
                match payload.get("type").and_then(serde_json::Value::as_str) {
                    Some("session_info") => seen_session_info = true,
                    Some("run_started") => seen_run_started = true,
                    Some("run_finished") => seen_run_finished = true,
                    other => panic!("unexpected WS payload type: {other:?} payload={payload}"),
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(frame) => {
                panic!("unexpected WS close before terminal messages: {frame:?}")
            }
            other => panic!("unexpected WS frame: {other:?}"),
        }
    }

    assert!(seen_session_info);
    assert!(seen_run_started);
    assert!(seen_run_finished);

    let requests = lifecycle.recorded_create_requests().await;
    assert_eq!(requests.len(), 1, "one WS run request expected");
    assert_eq!(requests[0].session_id.as_deref(), Some("capture-session"));
    assert!(requests[0].full_llm_capture);

    server.abort();
}

#[tokio::test]
async fn http_chat_propagates_session_scoped_full_capture_over_real_http() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build no-proxy client");
    let response = client
        .post(format!("http://{addr}/chat"))
        .bearer_auth("test-capture-token")
        .json(&json!({
            "session_id": "capture-session",
            "message": "hello over http"
        }))
        .send()
        .await
        .expect("http request should succeed");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["session_id"], "capture-session");
    assert_eq!(body["run_id"], "run-capture-ws");

    let requests = lifecycle.recorded_create_requests().await;
    assert_eq!(requests.len(), 1, "one HTTP run request expected");
    assert_eq!(requests[0].session_id.as_deref(), Some("capture-session"));
    assert!(requests[0].full_llm_capture);

    server.abort();
}
