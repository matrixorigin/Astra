mod test_support;

use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord,
    ErrorResponse, HealthChecker, RunLifecycleService, RunListRecord, RunStatusRecord, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, build_app,
};
use astra_services::runs::RunListCursor;
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
        _cursor: Option<astra_services::auth::SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(SessionActivityRecord {
            session_id: String::new(),
            activities: vec![],
            total: 0,
            limit: 1,
            next_cursor: None,
        })
    }
}

#[derive(Clone, Default)]
struct RecordingLifecycle {
    create_requests: Arc<Mutex<Vec<ChatRequestData>>>,
    cancel_calls: Arc<Mutex<Vec<String>>>,
    keep_run_active: Arc<Mutex<bool>>,
    stream_error: Arc<Mutex<Option<StatusCode>>>,
}

impl RecordingLifecycle {
    async fn recorded_create_requests(&self) -> Vec<ChatRequestData> {
        self.create_requests.lock().await.clone()
    }

    async fn recorded_cancel_calls(&self) -> Vec<String> {
        self.cancel_calls.lock().await.clone()
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
        let active = *self.keep_run_active.lock().await;
        Ok(RunStatusRecord {
            artifact_publication: None,
            root_run_id: Some(run_id.clone()),
            run_id,
            session_id: "capture-session".to_string(),
            parent_run_id: None,
            depth: 0,
            status: if active { "running" } else { "completed" }.to_string(),
            waiting_for: None,
            events_count: 1,
            workspace: None,
            executor: None,
            transport: None,
            accounting: None,
        })
    }

    async fn stream_run(
        &self,
        run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<astra_services::runs::DurableRunEventDelta, (StatusCode, Json<ErrorResponse>)> {
        if let Some(status) = *self.stream_error.lock().await {
            return Err((status, Json(ErrorResponse::new("observer failed"))));
        }
        let active = *self.keep_run_active.lock().await;
        Ok(astra_services::runs::DurableRunEventDelta {
            session_id: String::new(),
            status: if active { "running" } else { "completed" }.into(),
            last_event_idx: 0,
            events: if active {
                vec![]
            } else {
                vec![json!({
                    "event_type": "run_finished",
                    "data": {"run_id": run_id, "status": "completed"}
                })]
            },
        })
    }

    async fn cancel_run(
        &self,
        run_id: String,
        _user_id: String,
    ) -> Result<astra_runtime::CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.cancel_calls.lock().await.push(run_id.clone());
        Ok(astra_runtime::CancelRunRecord {
            run_id,
            status: "cancellation_requested".into(),
            execution_settled: false,
        })
    }

    async fn list_runs_cursor(
        &self,
        _user_id: String,
        _limit: u32,
        _cursor: Option<RunListCursor>,
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
    .with_model_service(test_support::test_model_service(
        "offer-test-model",
        "test-model",
    ))
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

async fn start_browser_ws_run(
    addr: std::net::SocketAddr,
    content: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut ws, _) = connect_async(format!("ws://{addr}/chat/ws"))
        .await
        .expect("WS connect");
    ws.send(Message::Text(
        json!({
            "type": "auth",
            "token": "Bearer test-capture-token",
            "interaction_api_major": astra_server_types::AGENT_INTERACTION_API_MAJOR,
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("auth send");
    let auth = ws.next().await.expect("auth response").expect("auth frame");
    let auth_json: serde_json::Value =
        serde_json::from_str(&auth.into_text().unwrap()).expect("auth JSON");
    assert_eq!(auth_json["type"], "auth_ok");
    assert_eq!(auth_json["user_id"], "test-user-1");
    ws.send(Message::Text(
        json!({
            "type": "message",
            "content": content,
            "session_id": "capture-session",
            "model_selection": {"offering_id": "offer-test-model"}
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("chat send");
    ws
}

#[tokio::test]
async fn browser_ws_chat_propagates_session_scoped_full_capture_over_real_websocket() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    let mut ws = start_browser_ws_run(addr, "hello over websocket").await;

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
async fn browser_ws_disconnect_keeps_durable_run_active() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    *lifecycle.keep_run_active.lock().await = true;
    let mut ws = start_browser_ws_run(addr, "leave the run active").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("run start timeout")
            .expect("server frame")
            .expect("valid frame");
        if let Message::Text(text) = frame {
            let payload: serde_json::Value = serde_json::from_str(&text).expect("WS JSON");
            if payload["type"] == "run_started" {
                break;
            }
        }
    }
    ws.close(None).await.expect("close WS");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("WS close failed: {error}"),
            }
        }
    })
    .await
    .expect("server did not finish WS close");
    assert!(lifecycle.recorded_cancel_calls().await.is_empty());
    server.abort();
}

#[tokio::test]
async fn browser_ws_observer_error_closes_without_cancelling_run() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    *lifecycle.stream_error.lock().await = Some(StatusCode::BAD_REQUEST);
    let mut ws = start_browser_ws_run(addr, "observe a failing stream").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut saw_error = false;
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("observer close timeout")
            .expect("server frame")
            .expect("valid frame");
        match frame {
            Message::Text(text) => {
                let payload: serde_json::Value = serde_json::from_str(&text).expect("WS JSON");
                assert_ne!(payload["type"], "run_finished");
                saw_error |= payload["type"] == "error";
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected WS frame: {other:?}"),
        }
    }
    assert!(saw_error, "observer failure must be visible before close");
    assert!(lifecycle.recorded_cancel_calls().await.is_empty());
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
            "message": "hello over http",
            "model_selection": {"offering_id": "offer-test-model"}
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
