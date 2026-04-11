//! Edge↔Cloud round-trip E2E tests.
//!
//! Realistic scenarios that exercise the full `POST /chat/turn` → SSE stream →
//! `POST /tools/result` → next LLM round path using `bridge-e2e-hooks` (mock LLM).
//!
//! Each test simulates what a real edge CLI does: consume SSE events, post tool
//! results back, and verify the cloud continues correctly.
//!
//! ```text
//! cargo test -p astra-runtime --test edge_cloud_round_trip_e2e --features bridge-e2e-hooks
//! ```

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

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
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::util::ServiceExt;

// ── Env setup ────────────────────────────────────────────────────────────────

const SECRET: &str = "round-trip-e2e-secret";
const TOKEN: &str = "Bearer rt-e2e-token";
const USER_ID: &str = "rt-e2e-user";

static SECRET_INIT: OnceLock<()> = OnceLock::new();

fn init_env() {
    SECRET_INIT.get_or_init(|| unsafe {
        std::env::set_var("ASTRA_BRIDGE_TEST_SECRET", SECRET);
    });
}

// ── Stubs ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealth;
#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuth;
#[async_trait]
impl AuthService for StubAuth {
    async fn register(
        &self,
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(TOKEN) {
            Ok(AuthUserRecord {
                user_id: USER_ID.into(),
                username: "rt-e2e".into(),
                email: "rt@e2e.test".into(),
                display_name: None,
            })
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("bad")),
            ))
        }
    }
}

#[derive(Clone)]
struct StubSession;
#[async_trait]
impl SessionService for StubSession {
    async fn create_session(
        &self,
        user_id: String,
        req: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id: "s-rt-e2e".into(),
            user_id,
            agent_id: req.agent_id,
            title: Some("e2e".into()),
            metadata: req.metadata.unwrap_or_default(),
            status: "active".into(),
            event_count: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: None,
            ended_at: None,
        })
    }
    async fn list_sessions(
        &self,
        _: SessionListFilter,
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
            status: "active".into(),
            event_count: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: None,
            ended_at: None,
        })
    }
    async fn update_session(
        &self,
        sid: String,
        uid: String,
        _: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        self.get_session(sid, uid).await
    }
    async fn delete_session(
        &self,
        _: String,
        _: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(())
    }
    async fn get_session_activity(
        &self,
        _: String,
        _: String,
        _: u32,
        _: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
}

#[derive(Clone, Default)]
struct Capture {
    plans: Arc<Mutex<Vec<TurnToolEventPersistPlan>>>,
}

#[derive(Clone)]
struct CapturingWriter(Capture);
#[async_trait]
impl TurnToolEventWriter for CapturingWriter {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
        self.0.plans.lock().await.push(plan);
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn tool_schema(name: &str) -> Value {
    json!({ "type": "function", "function": { "name": name, "description": name, "parameters": { "type": "object", "properties": { "path": { "type": "string" }, "command": { "type": "string" }, "content": { "type": "string" }, "old_str": { "type": "string" }, "new_str": { "type": "string" } } } } })
}

fn tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({ "id": id, "type": "function", "function": { "name": name, "arguments": serde_json::to_string(&args).unwrap() } })
}

fn build_app_with_capture(capture: Capture) -> Router {
    let enc = Arc::new(FernetTokenEncryptor::new("rt-e2e-fernet-key-32chars!!").expect("fernet"));
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession))
        .with_turn_tool_event_writer(Arc::new(CapturingWriter(capture.clone())));
    let ledger = base.edge_callback_ledger();
    let bridge = InProcessChatTurnBridge::new(
        MatrixOneSettings {
            host: "127.0.0.1".into(),
            port: 1,
            user: "x".into(),
            password: "x".into(),
            database: "x".into(),
        },
        enc,
    )
    .with_edge_callback_ledger(ledger);
    let state = base
        .with_chat_turn_bridge(Arc::new(bridge))
        .with_chat_turn_bridge_secret("rt-e2e-bridge-secret");
    build_app(state)
}

async fn chat_turn(app: &Router, payload: Value) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (st, bytes.to_vec())
}

async fn post_json(app: &Router, path: &str, payload: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (st, v)
}

/// Consume SSE stream, posting tool results when tool_request events appear.
/// Returns the full accumulated SSE text.
async fn consume_sse_posting_results(
    app: &Router,
    payload: Value,
    results: HashMap<String, Value>,
) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted: std::collections::HashSet<String> = std::collections::HashSet::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        // Check for tool_request events and post results
        for (call_id, output) in &results {
            if !posted.contains(call_id.as_str())
                && s.contains("\"type\":\"tool_request\"")
                && s.contains(call_id.as_str())
            {
                let (st, _) = post_json(
                    app,
                    "/tools/result",
                    json!({
                        "request_id": call_id,
                        "status": "ok",
                        "output": output,
                    }),
                )
                .await;
                assert_eq!(st, StatusCode::OK, "POST /tools/result for {call_id}");
                posted.insert(call_id.clone());
            }
        }
    }
    String::from_utf8_lossy(&acc).into_owned()
}

/// Parse SSE text into a list of typed events.
fn parse_sse_events(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

fn events_of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some(ty))
        .collect()
}

async fn wait_persist() {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 1: Multi-tool parallel delivery — 3 tool_calls in one round
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_tool_parallel_delivery_and_result_collection() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "analyze the project" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep"), tool_schema("bash")],
        "test_llm_rounds": [
            {
                "tool_calls": [
                    tool_call("tc-rf-1", "read_file", json!({"path": "README.md"})),
                    tool_call("tc-grep-1", "grep", json!({"path": ".", "command": "TODO"})),
                    tool_call("tc-bash-1", "bash", json!({"command": "ls"})),
                ]
            },
            { "full_text": "Analysis complete: project has 3 files." }
        ]
    });

    // bash requires approval — post approval + result; read_file/grep only need result
    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_results: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut posted_approvals: std::collections::HashSet<String> = std::collections::HashSet::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        // Handle approval for bash
        if !posted_approvals.contains("tc-bash-1")
            && s.contains("\"type\":\"approval_required\"")
            && s.contains("tc-bash-1")
        {
            let (st, _) = post_json(
                &app,
                "/approval/respond",
                json!({
                    "request_id": "tc-bash-1", "decision": "allow"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            posted_approvals.insert("tc-bash-1".into());
        }

        // Post tool results when tool_request appears
        for (id, output) in [
            ("tc-rf-1", "# README\nProject docs"),
            ("tc-grep-1", "src/main.rs:10:// TODO: refactor"),
            ("tc-bash-1", "README.md\nsrc/\ntests/"),
        ] {
            if !posted_results.contains(id)
                && s.contains("\"type\":\"tool_request\"")
                && s.contains(id)
            {
                let (st, _) = post_json(
                    &app,
                    "/tools/result",
                    json!({
                        "request_id": id, "status": "ok", "output": output
                    }),
                )
                .await;
                assert_eq!(st, StatusCode::OK);
                posted_results.insert(id.into());
            }
        }
    }

    let full = String::from_utf8_lossy(&acc);

    // All 3 tool_request events appeared
    assert!(full.contains("tc-rf-1"), "missing read_file tool_request");
    assert!(full.contains("tc-grep-1"), "missing grep tool_request");
    assert!(full.contains("tc-bash-1"), "missing bash tool_request");

    // All 3 results were posted
    assert_eq!(posted_results.len(), 3, "should have posted 3 tool results");

    // Round 2 text appeared
    assert!(
        full.contains("Analysis complete"),
        "missing round 2 text: {full}"
    );

    // No timeout
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "unexpected timeout"
    );

    // Persistence check
    wait_persist().await;
    let plans = capture.plans.lock().await;
    let all_events: Vec<_> = plans.iter().flat_map(|p| &p.events).collect();
    let result_events: Vec<_> = all_events
        .iter()
        .filter(|e| e.event_type == "tool_result")
        .collect();
    assert!(
        result_events.len() >= 3,
        "expected ≥3 persisted tool_result events, got {}",
        result_events.len()
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 2: Approval gate — write_file requires approval, then proceeds
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn approval_gate_allow_then_tool_request() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "create hello.txt" }],
        "edge_tools": [tool_schema("write_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-wf-1", "write_file", json!({"path": "hello.txt", "content": "hello"}))] },
            { "full_text": "File created successfully." }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut approved = false;
    let mut result_posted = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        if !approved && s.contains("\"type\":\"approval_required\"") && s.contains("tc-wf-1") {
            let (st, _) = post_json(
                &app,
                "/approval/respond",
                json!({
                    "request_id": "tc-wf-1", "decision": "allow"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            approved = true;
        }

        if approved
            && !result_posted
            && s.contains("\"type\":\"tool_request\"")
            && s.contains("tc-wf-1")
        {
            let (st, _) = post_json(
                &app,
                "/tools/result",
                json!({
                    "request_id": "tc-wf-1", "status": "ok", "output": "{\"success\":true}"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            result_posted = true;
        }
    }

    let full = String::from_utf8_lossy(&acc);
    assert!(approved, "approval_required event never appeared");
    assert!(
        result_posted,
        "tool_request event never appeared after approval"
    );

    let events = parse_sse_events(&full);
    let approval_idx = events
        .iter()
        .position(|e| e["type"] == "approval_required")
        .unwrap();
    let request_idx = events
        .iter()
        .position(|e| e["type"] == "tool_request" && e.to_string().contains("tc-wf-1"))
        .unwrap();
    assert!(
        approval_idx < request_idx,
        "approval_required must come before tool_request"
    );

    assert!(
        full.contains("File created successfully"),
        "missing round 2 text"
    );
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "unexpected timeout"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 3: Approval denied — cloud continues with denied result, no tool_request
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn approval_denied_skips_tool_execution() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "delete everything" }],
        "edge_tools": [tool_schema("bash")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-deny-1", "bash", json!({"command": "rm -rf /"}))] },
            { "full_text": "Understood, I won't delete anything." }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut denied = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        if !denied && s.contains("\"type\":\"approval_required\"") && s.contains("tc-deny-1") {
            let (st, _) = post_json(
                &app,
                "/approval/respond",
                json!({
                    "request_id": "tc-deny-1", "decision": "deny", "reason": "too dangerous"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            denied = true;
        }
    }

    let full = String::from_utf8_lossy(&acc);
    assert!(denied, "approval_required never appeared");

    // tool_request should NOT appear for a denied tool
    let events = parse_sse_events(&full);
    let tool_requests: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "tool_request" && e.to_string().contains("tc-deny-1"))
        .collect();
    assert!(
        tool_requests.is_empty(),
        "denied tool should not get tool_request, got: {tool_requests:?}"
    );

    // Round 2 text should still appear (LLM continues with denied result in context)
    assert!(
        full.contains("won't delete"),
        "missing round 2 text after denial: {full}"
    );

    // Persistence: denied result should be persisted
    wait_persist().await;
    let plans = capture.plans.lock().await;
    let all_events: Vec<_> = plans.iter().flat_map(|p| &p.events).collect();
    let denied_events: Vec<_> = all_events
        .iter()
        .filter(|e| e.event_type == "tool_result" && e.content.contains("user_denied"))
        .collect();
    assert!(
        !denied_events.is_empty(),
        "denied tool_result should be persisted"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 4: Multi-round conversation — 3 tool rounds + final answer
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_round_three_tool_calls_then_final_answer() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "fix the bug in auth.go" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("str_replace"), tool_schema("bash")],
        "test_llm_rounds": [
            // Round 1: read the file
            { "tool_calls": [tool_call("tc-r1", "read_file", json!({"path": "auth.go"}))] },
            // Round 2: edit the file (requires approval)
            { "tool_calls": [tool_call("tc-r2", "str_replace", json!({"path": "auth.go", "old_str": "bug", "new_str": "fix"}))] },
            // Round 3: run tests (requires approval)
            { "tool_calls": [tool_call("tc-r3", "bash", json!({"command": "go test ./..."}))] },
            // Round 4: final answer
            { "full_text": "Bug fixed and tests pass." }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut approved: std::collections::HashSet<String> = std::collections::HashSet::new();

    let tool_outputs: HashMap<&str, &str> = HashMap::from([
        ("tc-r1", "package auth\n\nfunc Login() { /* bug here */ }"),
        ("tc-r2", "Replaced 1 occurrence in auth.go"),
        ("tc-r3", "ok  \tauth\t0.003s"),
    ]);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        // Handle approvals for str_replace and bash
        for id in ["tc-r2", "tc-r3"] {
            if !approved.contains(id)
                && s.contains("\"type\":\"approval_required\"")
                && s.contains(id)
            {
                let (st, _) = post_json(
                    &app,
                    "/approval/respond",
                    json!({
                        "request_id": id, "decision": "allow"
                    }),
                )
                .await;
                assert_eq!(st, StatusCode::OK);
                approved.insert(id.into());
            }
        }

        // Post tool results
        for (id, output) in &tool_outputs {
            if !posted.contains(*id) && s.contains("\"type\":\"tool_request\"") && s.contains(*id) {
                let (st, _) = post_json(
                    &app,
                    "/tools/result",
                    json!({
                        "request_id": id, "status": "ok", "output": output
                    }),
                )
                .await;
                assert_eq!(st, StatusCode::OK);
                posted.insert(id.to_string());
            }
        }
    }

    let full = String::from_utf8_lossy(&acc);

    // All 3 tool rounds completed
    assert_eq!(
        posted.len(),
        3,
        "should have posted 3 tool results, got: {posted:?}"
    );
    assert!(full.contains("tc-r1"), "missing round 1 tool_request");
    assert!(full.contains("tc-r2"), "missing round 2 tool_request");
    assert!(full.contains("tc-r3"), "missing round 3 tool_request");

    // Final answer appeared
    assert!(
        full.contains("Bug fixed and tests pass"),
        "missing final answer: {full}"
    );
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "unexpected timeout"
    );

    // Persistence: 3 tool_result events
    wait_persist().await;
    let plans = capture.plans.lock().await;
    let result_count = plans
        .iter()
        .flat_map(|p| &p.events)
        .filter(|e| e.event_type == "tool_result")
        .count();
    assert!(
        result_count >= 3,
        "expected ≥3 persisted tool_results, got {result_count}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 5: Tool result timeout — edge never responds
//
// Run separately: MO_TURN_TIMEOUT_S=3 cargo test ... -- tool_result_timeout --ignored
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "requires MO_TURN_TIMEOUT_S=3 to avoid 300s wait; run: MO_TURN_TIMEOUT_S=3 cargo test --manifest-path rust/Cargo.toml -p astra-runtime --test edge_cloud_round_trip_e2e --features bridge-e2e-hooks -- tool_result_timeout --ignored --nocapture"]
async fn tool_result_timeout_when_edge_never_responds() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    // read_file doesn't need approval, so the bridge will emit tool_request
    // and then wait on the ledger. We never post a result → timeout.
    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "read something" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-timeout-1", "read_file", json!({"path": "gone.txt"}))] },
            { "full_text": "Could not read the file." }
        ]
    });

    // Don't post any tool result — let it timeout
    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let full = String::from_utf8_lossy(&raw);

    // The tool_request event should still appear (bridge emits it before waiting)
    assert!(
        full.contains("tc-timeout-1"),
        "tool_request should appear before timeout wait"
    );

    // The LLM should still get a second round (with timeout as tool result in context)
    // The bridge passes MSG_TOOL_LEDGER_TIMEOUT as the tool result content to the LLM,
    // which then generates round 2 text.
    assert!(
        full.contains("Could not read the file"),
        "round 2 should still appear after timeout: {full}"
    );

    // Persistence: timeout should be recorded in tool_result events
    wait_persist().await;
    let plans = capture.plans.lock().await;
    let all_events: Vec<_> = plans.iter().flat_map(|p| &p.events).collect();
    let timeout_events: Vec<_> = all_events
        .iter()
        .filter(|e| e.event_type == "tool_result" && e.content.contains("timed out"))
        .collect();
    assert!(
        !timeout_events.is_empty(),
        "timeout tool_result should be persisted, events: {:?}",
        all_events
            .iter()
            .map(|e| format!("{}:{}", e.event_type, &e.content[..e.content.len().min(80)]))
            .collect::<Vec<_>>()
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 6: SSE event ordering contract
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn sse_event_ordering_contract() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "hello" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-ord-1", "read_file", json!({"path": "x.txt"}))] },
            { "full_text": "Done." }
        ]
    });

    let results = HashMap::from([("tc-ord-1".to_string(), json!("file content"))]);
    let full = consume_sse_posting_results(&app, payload, results).await;
    let events = parse_sse_events(&full);

    assert!(!events.is_empty(), "should have SSE events");

    // turn_complete must be the last application event (pings may follow but are transport-level)
    let app_events: Vec<_> = events
        .iter()
        .filter(|e| e["type"].as_str() != Some("ping"))
        .collect();
    let last = app_events.last().expect("should have app events");
    assert_eq!(
        last["type"], "turn_complete",
        "last application event must be turn_complete, got: {last}"
    );

    // session_info should appear before any tool_request or text_delta
    let session_info_idx = events.iter().position(|e| e["type"] == "session_info");
    let first_tool_or_text = events
        .iter()
        .position(|e| e["type"] == "tool_request" || e["type"] == "text_delta");
    if let (Some(si), Some(ft)) = (session_info_idx, first_tool_or_text) {
        assert!(
            si < ft,
            "session_info (idx {si}) must come before first tool/text (idx {ft})"
        );
    }

    // usage event should appear before turn_complete
    let usage_idx = events.iter().position(|e| e["type"] == "usage");
    let complete_idx = events.iter().position(|e| e["type"] == "turn_complete");
    if let (Some(u), Some(c)) = (usage_idx, complete_idx) {
        assert!(
            u < c,
            "usage (idx {u}) must come before turn_complete (idx {c})"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 7: Single tool round-trip baseline (also verifies pings are harmless)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_tool_round_trip_baseline() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    // Simple round-trip: tool call → result → final text
    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "list files" }],
        "edge_tools": [tool_schema("glob")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-ping-1", "glob", json!({"path": "."}))] },
            { "full_text": "Found 5 files." }
        ]
    });

    let results = HashMap::from([("tc-ping-1".to_string(), json!("a.rs\nb.rs\nc.rs"))]);
    let full = consume_sse_posting_results(&app, payload, results).await;
    let events = parse_sse_events(&full);

    // Ping events (if any) should not interfere with the flow
    let pings = events_of_type(&events, "ping");
    let tool_requests = events_of_type(&events, "tool_request");
    let _text_deltas = events_of_type(&events, "text_delta");
    let turn_completes = events_of_type(&events, "turn_complete");

    // Core flow must work regardless of pings
    assert!(!tool_requests.is_empty(), "should have tool_request events");
    assert!(!turn_completes.is_empty(), "should have turn_complete");
    assert!(full.contains("Found 5 files"), "final text missing");

    // If pings are present, they should have a timestamp
    for ping in &pings {
        assert!(
            ping.get("ts").is_some(),
            "ping should have ts field: {ping}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 8: Simple text-only turn (no tools) — baseline sanity
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn text_only_turn_no_tools() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "what is 2+2?" }],
        "edge_tools": [],
        "test_llm_rounds": [
            { "full_text": "2+2 equals 4." }
        ]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let full = String::from_utf8_lossy(&raw);
    let events = parse_sse_events(&full);

    assert!(full.contains("2+2 equals 4"), "missing LLM response text");

    // No tool_request events
    let tool_requests = events_of_type(&events, "tool_request");
    assert!(
        tool_requests.is_empty(),
        "text-only turn should have no tool_requests"
    );

    // turn_complete with has_tool_calls: false
    let completes = events_of_type(&events, "turn_complete");
    assert!(!completes.is_empty(), "should have turn_complete");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 9: Edge returns tool error — cloud passes error to LLM, LLM continues
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tool_error_result_propagates_to_next_round() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "read secret.key" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-err-1", "read_file", json!({"path": "secret.key"}))] },
            { "full_text": "The file could not be read due to a permission error." }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        if !posted && s.contains("\"type\":\"tool_request\"") && s.contains("tc-err-1") {
            // Edge returns an error result (like a real read_file permission denied)
            let (st, _) = post_json(
                &app,
                "/tools/result",
                json!({
                    "request_id": "tc-err-1",
                    "status": "error",
                    "output": "Error: permission denied (os error 13). Cannot read secret.key"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            posted = true;
        }
    }

    let full = String::from_utf8_lossy(&acc);
    assert!(posted, "tool_request never appeared");

    // LLM should still get round 2 (with error as tool result content)
    assert!(
        full.contains("permission error"),
        "round 2 should appear with LLM acknowledging the error: {full}"
    );
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "should not timeout"
    );

    // Persistence: error result should be persisted
    wait_persist().await;
    let plans = capture.plans.lock().await;
    let error_events: Vec<_> = plans
        .iter()
        .flat_map(|p| &p.events)
        .filter(|e| e.event_type == "tool_result" && e.content.contains("permission denied"))
        .collect();
    assert!(
        !error_events.is_empty(),
        "error tool_result should be persisted"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 10: Duplicate tool result — second POST is accepted but doesn't break flow
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn duplicate_tool_result_does_not_break_flow() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "check status" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-dup-1", "read_file", json!({"path": "status.txt"}))] },
            { "full_text": "Status is healthy." }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut post_count = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        if s.contains("\"type\":\"tool_request\"") && s.contains("tc-dup-1") && post_count < 2 {
            // Post the result (first time: consumed by bridge; second time: goes to ledger but nobody reads it)
            let (st, _) = post_json(
                &app,
                "/tools/result",
                json!({
                    "request_id": "tc-dup-1",
                    "status": "ok",
                    "output": "all systems operational"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            post_count += 1;
        }
    }

    let full = String::from_utf8_lossy(&acc);

    // The flow should complete normally despite the duplicate
    assert!(post_count >= 1, "should have posted at least once");
    assert!(
        full.contains("Status is healthy"),
        "round 2 text should appear: {full}"
    );
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "should not timeout"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 11: Mixed approval + read-only in same round — approval sequential,
//          read-only concurrent, both complete correctly
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mixed_approval_and_readonly_in_same_round() {
    init_env();
    let capture = Capture::default();
    let app = build_app_with_capture(capture.clone());

    // write_file needs approval (sequential), read_file + grep don't (concurrent)
    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "update config and verify" }],
        "edge_tools": [tool_schema("write_file"), tool_schema("read_file"), tool_schema("grep")],
        "test_llm_rounds": [
            {
                "tool_calls": [
                    tool_call("tc-mix-wf", "write_file", json!({"path": "config.yaml", "content": "key: value"})),
                    tool_call("tc-mix-rf", "read_file", json!({"path": "config.yaml"})),
                    tool_call("tc-mix-gr", "grep", json!({"path": ".", "command": "key"})),
                ]
            },
            { "full_text": "Config updated and verified." }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut approved: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut posted: std::collections::HashSet<String> = std::collections::HashSet::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);

        // Approve write_file
        if !approved.contains("tc-mix-wf")
            && s.contains("\"type\":\"approval_required\"")
            && s.contains("tc-mix-wf")
        {
            let (st, _) = post_json(
                &app,
                "/approval/respond",
                json!({
                    "request_id": "tc-mix-wf", "decision": "allow"
                }),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            approved.insert("tc-mix-wf".into());
        }

        // Post results for all tools
        for (id, output) in [
            ("tc-mix-wf", "{\"success\":true,\"bytes_written\":10}"),
            ("tc-mix-rf", "key: value"),
            ("tc-mix-gr", "config.yaml:1:key: value"),
        ] {
            if !posted.contains(id) && s.contains("\"type\":\"tool_request\"") && s.contains(id) {
                let (st, _) = post_json(
                    &app,
                    "/tools/result",
                    json!({
                        "request_id": id, "status": "ok", "output": output
                    }),
                )
                .await;
                assert_eq!(st, StatusCode::OK);
                posted.insert(id.into());
            }
        }
    }

    let full = String::from_utf8_lossy(&acc);

    // All 3 tools completed
    assert_eq!(posted.len(), 3, "all 3 tools should complete: {posted:?}");

    // Verify ordering: approval_required for write_file comes before its tool_request
    let events = parse_sse_events(&full);
    let appr_idx = events
        .iter()
        .position(|e| e["type"] == "approval_required" && e.to_string().contains("tc-mix-wf"));
    let wf_req_idx = events
        .iter()
        .position(|e| e["type"] == "tool_request" && e.to_string().contains("tc-mix-wf"));
    if let (Some(a), Some(r)) = (appr_idx, wf_req_idx) {
        assert!(
            a < r,
            "approval_required must precede tool_request for write_file"
        );
    }

    // read_file and grep should NOT have approval_required events
    let rf_approvals: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "approval_required" && e.to_string().contains("tc-mix-rf"))
        .collect();
    assert!(
        rf_approvals.is_empty(),
        "read_file should not need approval"
    );

    assert!(
        full.contains("Config updated and verified"),
        "final text missing: {full}"
    );
    assert!(
        !full.contains(MSG_TOOL_LEDGER_TIMEOUT),
        "unexpected timeout"
    );

    // Persistence
    wait_persist().await;
    let plans = capture.plans.lock().await;
    let result_count = plans
        .iter()
        .flat_map(|p| &p.events)
        .filter(|e| e.event_type == "tool_result")
        .count();
    assert!(
        result_count >= 3,
        "expected ≥3 persisted tool_results, got {result_count}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 12: turn_complete.has_tool_calls reflects actual tool presence
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_complete_has_tool_calls_field_accuracy() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    // Case A: turn WITH tool calls
    let payload_tools = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "read x" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-htc-1", "read_file", json!({"path": "x"}))] },
            { "full_text": "done" }
        ]
    });
    let results_a = HashMap::from([("tc-htc-1".to_string(), json!("content"))]);
    let full_a = consume_sse_posting_results(&app, payload_tools, results_a).await;
    let events_a = parse_sse_events(&full_a);
    let complete_a = events_a
        .iter()
        .find(|e| e["type"] == "turn_complete")
        .expect("turn_complete");
    assert_eq!(
        complete_a["has_tool_calls"], true,
        "turn with tools should have has_tool_calls:true"
    );

    // Case B: turn WITHOUT tool calls
    let payload_text = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "hello" }]
    });
    let (_, raw_b) = chat_turn(&app, payload_text).await;
    let full_b = String::from_utf8_lossy(&raw_b);
    let events_b = parse_sse_events(&full_b);
    let complete_b = events_b
        .iter()
        .find(|e| e["type"] == "turn_complete")
        .expect("turn_complete");
    // Note: has_tool_calls may be true if the bridge always sets it based on
    // all_round_tool_calls (which includes tools from prior rounds in the same
    // /chat/turn call). For a pure text turn, it should be false.
    assert_eq!(
        complete_b["has_tool_calls"], false,
        "text-only turn should have has_tool_calls:false, got: {complete_b}"
    );
}
