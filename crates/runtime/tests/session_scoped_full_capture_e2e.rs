mod test_support;

use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord,
    ErrorResponse, HealthChecker, RunLifecycleService, RunListRecord, RunStatusRecord, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, build_app,
};
use astra_services::runs::{
    DurableRunInteractionKind, DurableRunInteractionResolveOutcome, RunListCursor,
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
            Some("Bearer test-other-token") => Ok(AuthUserRecord {
                user_id: "other-user".to_string(),
                username: "other".to_string(),
                email: "other@test.local".to_string(),
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
    publication_after_terminal: Arc<Mutex<bool>>,
    publication_in_live_replay: Arc<Mutex<bool>>,
    waiting_for: Arc<Mutex<Option<String>>>,
    resolved_interactions: Arc<Mutex<Vec<String>>>,
    live_attach_cursors: Arc<Mutex<Vec<u32>>>,
    replay_event_at_index: Arc<Mutex<Option<u32>>>,
}

impl RecordingLifecycle {
    async fn recorded_create_requests(&self) -> Vec<ChatRequestData> {
        self.create_requests.lock().await.clone()
    }

    async fn recorded_cancel_calls(&self) -> Vec<String> {
        self.cancel_calls.lock().await.clone()
    }
}

fn mock_explain_publication() -> serde_json::Value {
    json!({
        "index": 1,
        "event_type": "artifact_publication",
        "data": {
            "schema_version": 1,
            "run_id": "run-capture-ws",
            "turn_id": "turn-1",
            "execution_owner_generation": 1,
            "artifact_type": "explain_analyze_snapshot",
            "recorded": true,
            "status": "unavailable",
            "reason_code": "report_missing",
            "message": "Report unavailable."
        }
    })
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
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        if run_id != "run-capture-ws" || user_id != "test-user-1" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("Run not found")),
            ));
        }
        let active = *self.keep_run_active.lock().await;
        Ok(RunStatusRecord {
            artifact_publication: None,
            root_run_id: Some(run_id.clone()),
            run_id,
            session_id: "capture-session".to_string(),
            parent_run_id: None,
            depth: 0,
            status: if active { "running" } else { "completed" }.to_string(),
            waiting_for: self.waiting_for.lock().await.clone(),
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
        last_index: u32,
    ) -> Result<astra_services::runs::DurableRunEventDelta, (StatusCode, Json<ErrorResponse>)> {
        if let Some(status) = *self.stream_error.lock().await {
            return Err((status, Json(ErrorResponse::new("observer failed"))));
        }
        let active = *self.keep_run_active.lock().await;
        let replay_at = *self.replay_event_at_index.lock().await;
        Ok(astra_services::runs::DurableRunEventDelta {
            session_id: String::new(),
            status: if active { "running" } else { "completed" }.into(),
            last_event_idx: 0,
            events: if active && replay_at.is_some_and(|index| last_index <= index) {
                vec![json!({
                    "index": replay_at.unwrap(),
                    "event_type": "text_delta",
                    "data": {"content": "replayed after reconnect"}
                })]
            } else if active || last_index > 0 {
                vec![]
            } else {
                vec![json!({
                    "index": 0,
                    "event_type": "run_finished",
                    "data": {"run_id": run_id, "status": "completed"}
                })]
            },
        })
    }

    async fn stream_run_live(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        self.live_attach_cursors.lock().await.push(last_index);
        if *self.publication_in_live_replay.lock().await {
            return Ok(ChatStreamRecord {
                session_id: "capture-session".into(),
                run_id,
                events: vec![mock_explain_publication()],
                event_rx: None,
            });
        }
        if *self.publication_after_terminal.lock().await {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = tx.send(mock_explain_publication()).await;
            });
            return Ok(ChatStreamRecord {
                session_id: "capture-session".into(),
                run_id,
                events: vec![],
                event_rx: Some(rx),
            });
        }
        let delta = self.stream_run(run_id.clone(), user_id, last_index).await?;
        Ok(ChatStreamRecord {
            session_id: "capture-session".into(),
            run_id,
            events: delta.events,
            event_rx: None,
        })
    }

    async fn get_run_interaction_event(
        &self,
        _run_id: String,
        _user_id: String,
        request_id: String,
        event_type: String,
    ) -> Result<Option<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        let event = match (request_id.as_str(), event_type.as_str()) {
            ("approval-1", "approval_required") => json!({
                "data": { "tool": "shell", "approval_kind": "standard" }
            }),
            ("prompt-1", "ask_user_prompted") => json!({
                "data": { "prompt": {
                    "context": null,
                    "questions": [{
                        "header": "Continue", "question": "Continue?",
                        "options": [
                            {"label": "yes", "description": null, "preview": null},
                            {"label": "no", "description": null, "preview": null}
                        ],
                        "multi_select": false, "allow_freeform": false
                    }]
                }}
            }),
            _ => return Ok(None),
        };
        Ok(Some(event))
    }

    async fn resolve_run_interaction(
        &self,
        _run_id: String,
        _user_id: String,
        _expected_session_id: String,
        request_id: String,
        _kind: DurableRunInteractionKind,
        _response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, (StatusCode, Json<ErrorResponse>)> {
        self.resolved_interactions.lock().await.push(request_id);
        Ok(DurableRunInteractionResolveOutcome::Resolved(json!({})))
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
    let mut ws = connect_authenticated_ws(addr, "test-capture-token").await;
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

async fn connect_authenticated_ws(
    addr: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut ws, _) = connect_async(format!("ws://{addr}/chat/ws"))
        .await
        .expect("WS connect");
    ws.send(Message::Text(
        json!({
            "type": "auth",
            "token": format!("Bearer {token}"),
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
    ws
}

async fn next_ws_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .expect("WS event timeout")
            .expect("WS frame")
            .expect("valid WS frame");
        match frame {
            Message::Text(text) => return serde_json::from_str(&text).expect("WS JSON"),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected WS frame: {other:?}"),
        }
    }
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
async fn browser_ws_replays_publication_committed_between_head_and_tail_reads() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    // The captured tail exposes only index 0; the publication at index 1 is
    // already present when live attach starts its reconciliation read.
    *lifecycle.publication_in_live_replay.lock().await = true;
    let mut ws = start_browser_ws_run(addr, "explain").await;
    assert_eq!(next_ws_json(&mut ws).await["type"], "session_info");
    assert_eq!(next_ws_json(&mut ws).await["type"], "run_started");
    assert_eq!(next_ws_json(&mut ws).await["type"], "artifact_publication");
    assert_eq!(next_ws_json(&mut ws).await["type"], "run_finished");
    assert_eq!(*lifecycle.live_attach_cursors.lock().await, vec![1]);
    server.abort();
}

#[tokio::test]
async fn browser_ws_reconciles_publication_appended_after_terminal_tail_read() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    *lifecycle.publication_after_terminal.lock().await = true;
    let mut ws = start_browser_ws_run(addr, "explain").await;
    assert_eq!(next_ws_json(&mut ws).await["type"], "session_info");
    assert_eq!(next_ws_json(&mut ws).await["type"], "run_started");
    let publication = next_ws_json(&mut ws).await;
    assert_eq!(publication["type"], "artifact_publication");
    assert_eq!(publication["index"], 1);
    assert_eq!(next_ws_json(&mut ws).await["type"], "run_finished");
    assert_eq!(*lifecycle.live_attach_cursors.lock().await, vec![1]);
    server.abort();
}

#[tokio::test]
async fn browser_ws_reconnect_replays_cursor_and_resolves_waiting_interactions() {
    let (addr, lifecycle, server) = spawn_test_server().await;
    *lifecycle.keep_run_active.lock().await = true;
    let mut first = start_browser_ws_run(addr, "leave the run active").await;
    assert_eq!(next_ws_json(&mut first).await["type"], "session_info");
    assert_eq!(next_ws_json(&mut first).await["type"], "run_started");
    first.close(None).await.expect("close first observer");

    let mut wrong_owner = connect_authenticated_ws(addr, "test-other-token").await;
    wrong_owner
        .send(Message::Text(
            json!({
                "type": "attach_run", "run_id": "run-capture-ws", "last_index": 0
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send other-user attach");
    assert_eq!(next_ws_json(&mut wrong_owner).await["type"], "error");
    wrong_owner.close(None).await.expect("close other owner");

    let mut ws = connect_authenticated_ws(addr, "test-capture-token").await;
    *lifecycle.replay_event_at_index.lock().await = Some(7);
    ws.send(Message::Text(
        json!({
            "type": "attach_run", "run_id": "run-capture-ws", "last_index": 7
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send attach");
    let attached = next_ws_json(&mut ws).await;
    assert_eq!(attached["type"], "session_info");
    assert_eq!(attached["run_id"], "run-capture-ws");
    let replay = next_ws_json(&mut ws).await;
    assert_eq!(replay["type"], "text_delta");
    assert_eq!(replay["index"], 7);
    *lifecycle.waiting_for.lock().await = Some("tool_approval".into());
    ws.send(Message::Text(
        json!({
            "type": "tool_approval", "request_id": "approval-1", "approved": true
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send approval");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if lifecycle
                .resolved_interactions
                .lock()
                .await
                .contains(&"approval-1".to_string())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approval resolved");

    *lifecycle.waiting_for.lock().await = Some("user_input".into());
    ws.send(Message::Text(
        json!({
            "type": "user_prompt", "request_id": "prompt-1", "cancelled": true
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send prompt response");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if lifecycle
                .resolved_interactions
                .lock()
                .await
                .contains(&"prompt-1".to_string())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt resolved");
    assert!(lifecycle.recorded_cancel_calls().await.is_empty());
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
                Some(Err(_)) => break,
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
