//! Full path: `POST /chat/turn` (mock LLM via `test_llm_rounds`) → SSE `tool_request` →
//! `POST /tools/result` → shared ledger → bridge consumes and persists tool result.
//!
//! Requires crate feature `bridge-e2e-hooks` and env `ASTRA_BRIDGE_TEST_SECRET` (set below).

use std::sync::Arc;
use std::sync::OnceLock;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, FernetTokenEncryptor, HealthChecker,
    MatrixOneSettings, ServiceInfo, SessionActivityRecord, SessionCreateRequestData,
    SessionListFilter, SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    TurnToolEventPersistPlan, TurnToolEventWriter, build_app,
    turn::bridge_inprocess::InProcessChatTurnBridge, turn::edge_ledger::MSG_TOOL_LEDGER_TIMEOUT,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::Mutex;
use tower::util::ServiceExt;

static BRIDGE_TEST_SECRET_INIT: OnceLock<()> = OnceLock::new();

fn ensure_bridge_test_secret_env() {
    BRIDGE_TEST_SECRET_INIT.get_or_init(|| {
        // SAFETY: `set_var` is `unsafe` in Rust 2024; this integration test binary sets the secret
        // once (before test threads read it) so `bridge_e2e_hooks::authorized` matches the client header.
        unsafe {
            std::env::set_var("ASTRA_BRIDGE_TEST_SECRET", "ledger-inject-e2e-secret");
        }
    });
}

const E2E_TOOL_OUTPUT: &str = "e2e-ledger-injected-tool-body-7f3a";

#[derive(Clone)]
struct StubHealth;

#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct LedgerE2eAuth;

#[async_trait]
impl AuthService for LedgerE2eAuth {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some("Bearer ledger-e2e-token")
        {
            Ok(AuthUserRecord {
                user_id: "ledger-e2e-user".to_string(),
                username: "ledger-e2e".to_string(),
                email: "ledger@e2e.test".to_string(),
                display_name: None,
            })
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("bad token")),
            ))
        }
    }
}

#[derive(Clone)]
struct LedgerE2eSession;

#[async_trait]
impl SessionService for LedgerE2eSession {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id: "s-ledger-e2e".to_string(),
            user_id,
            agent_id: request.agent_id,
            title: Some("e2e".to_string()),
            metadata: request.metadata.unwrap_or_default(),
            status: "active".to_string(),
            event_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            ended_at: None,
        })
    }

    async fn list_sessions(
        &self,
        _filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id,
            user_id,
            agent_id: None,
            title: None,
            metadata: serde_json::Map::new(),
            status: "active".to_string(),
            event_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: None,
            ended_at: None,
        })
    }

    async fn update_session(
        &self,
        session_id: String,
        user_id: String,
        _request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        self.get_session(session_id, user_id).await
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(())
    }

    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
}

#[derive(Clone, Default)]
struct ToolPersistCapture {
    plans: Arc<Mutex<Vec<TurnToolEventPersistPlan>>>,
}

#[derive(Clone)]
struct CapturingTurnToolWriter {
    capture: ToolPersistCapture,
}

#[async_trait]
impl TurnToolEventWriter for CapturingTurnToolWriter {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
        self.capture.plans.lock().await.push(plan);
        Ok(())
    }
}

fn matrixone_dummy() -> MatrixOneSettings {
    MatrixOneSettings {
        host: "127.0.0.1".into(),
        port: 1,
        user: "x".into(),
        password: "x".into(),
        database: "x".into(),
    }
}

fn ledger_inject_app(capture: ToolPersistCapture) -> Router {
    let encryptor =
        Arc::new(FernetTokenEncryptor::new("ledger-e2e-fernet-key").expect("test fernet key"));
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(LedgerE2eAuth))
        .with_session_service(Arc::new(LedgerE2eSession))
        .with_turn_tool_event_writer(Arc::new(CapturingTurnToolWriter {
            capture: capture.clone(),
        }));
    let ledger = base.edge_callback_ledger();
    let bridge = InProcessChatTurnBridge::new(matrixone_dummy(), encryptor)
        .with_edge_callback_ledger(ledger);
    let state = base
        .with_chat_turn_bridge(Arc::new(bridge))
        .with_chat_turn_bridge_secret("ledger-e2e-bridge-secret");
    build_app(state)
}

async fn post_json(app: &Router, path: &str, payload: serde_json::Value) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", "Bearer ledger-e2e-token")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn chat_turn_tool_request_tools_result_ledger_injects_before_second_round() {
    ensure_bridge_test_secret_env();
    let capture = ToolPersistCapture::default();
    let app = ledger_inject_app(capture.clone());

    let read_file_tool = json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "read a file",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }
    });

    let payload = json!({
        "agent_id": "ledger-e2e-agent",
        "messages": [{ "role": "user", "content": "read README" }],
        "edge_tools": [read_file_tool],
        "test_llm_rounds": [
            {
                "tool_calls": [{
                    "id": "call-ledger-e2e-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}"
                    }
                }]
            },
            { "full_text": "round-2-after-ledger" }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", "Bearer ledger-e2e-token")
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", "ledger-inject-e2e-secret")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = response.into_body().into_data_stream();
    let mut posted = false;
    let mut acc = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted && s.contains("\"type\":\"tool_request\"") && s.contains("call-ledger-e2e-1") {
            let (st, body) = post_json(
                &app,
                "/tools/result",
                json!({
                    "request_id": "call-ledger-e2e-1",
                    "status": "ok",
                    "output": E2E_TOOL_OUTPUT
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            assert!(body.contains("\"ok\":true"), "tools/result body: {body}");
            posted = true;
        }
    }

    assert!(posted, "expected tool_request in SSE before stream end");

    let full = String::from_utf8_lossy(&acc);
    assert!(
        full.contains("\"type\":\"tool_request\""),
        "missing tool_request: {full}"
    );
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "ledger timed out: {full}"
    );
    assert!(
        full.contains("round-2-after-ledger"),
        "second mock round should stream as text_delta: {full}"
    );

    // Tool events are persisted in a spawned task on the bridge path.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let plans = capture.plans.lock().await;
    let mut saw_injected = false;
    for plan in plans.iter() {
        for ev in &plan.events {
            if ev.event_type == "tool_result" && ev.content.contains(E2E_TOOL_OUTPUT) {
                saw_injected = true;
            }
        }
    }
    assert!(
        saw_injected,
        "expected tool_result persist to contain POST /tools/result output"
    );
}
