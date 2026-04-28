//! Edge↔Cloud round-trip E2E tests — single-call proxy mode.
//!
//! After the bridge performance overhaul, the bridge makes exactly ONE LLM call
//! per HTTP request. Tool calls from the LLM are returned to the CLI via
//! `turn_complete` with `has_tool_calls: true`. The CLI drives tool execution
//! and continuation rounds.
//!
//! ```text
//! cargo test -p astra-runtime --test edge_cloud_round_trip_e2e --features bridge-e2e-hooks
//! ```

use std::sync::{Arc, OnceLock};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, FernetTokenEncryptor, HealthChecker,
    MatrixOneSettings, ServiceInfo, SessionActivityRecord, SessionCreateRequestData,
    SessionListFilter, SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    TurnToolEventPersistPlan, TurnToolEventWriter, build_app,
    turn::bridge_inprocess::InProcessChatTurnBridge,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
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
        .with_turn_tool_event_writer(Arc::new(CapturingWriter(capture)));
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
    .with_edge_callback_ledger(base.edge_callback_ledger());
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

async fn chat_turn_events(app: &Router, payload: Value) -> (String, Vec<Value>) {
    let (st, raw) = chat_turn(app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let full = String::from_utf8_lossy(&raw).into_owned();
    let events = parse_sse_events(&full);
    (full, events)
}

fn assert_single_call(events: &[Value], full: &str) {
    // The bridge is a single-call proxy: it makes one LLM call and emits
    // tool_request SSE events so the CLI can execute tools locally.
    // It does NOT run multi-round tool loops itself.
    let completes = events_of_type(events, "turn_complete");
    assert!(!completes.is_empty(), "should have turn_complete: {full}");

    // If the turn has tool_calls, bridge must emit tool_request events.
    let has_tool_calls = completes.iter().any(|e| {
        e.get("has_tool_calls")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    });
    let tool_requests = events_of_type(events, "tool_request");
    if has_tool_calls {
        assert!(
            !tool_requests.is_empty(),
            "bridge must emit tool_request for each tool_call: {full}"
        );
    }

    assert!(
        events_of_type(events, "approval_required").is_empty(),
        "single-call: no approval_required events: {full}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 1: Multiple tool calls — bridge returns all, does not execute
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_tool_parallel_delivery_and_result_collection() {
    init_env();
    let app = build_app_with_capture(Capture::default());

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
            { "full_text": "This should NOT appear in single-call mode." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(
        !full.contains("should NOT appear"),
        "bridge must not run second LLM round: {full}"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 2: Approval-required tool — bridge does NOT gate on approval
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn approval_gate_allow_then_tool_request() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "create hello.txt" }],
        "edge_tools": [tool_schema("write_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-wf-1", "write_file", json!({"path": "hello.txt", "content": "hello"}))] },
            { "full_text": "File created successfully." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(!full.contains("File created successfully"));
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 2b: Approval journal fallback — irrelevant in single-call mode
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn approval_journal_fallback_survives_ledger_overflow() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "create hello.txt" }],
        "edge_tools": [tool_schema("write_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-wf-journal", "write_file", json!({"path": "hello.txt", "content": "hello"}))] },
            { "full_text": "Should not appear." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(!full.contains("Should not appear"));
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 3: Approval denied — bridge does not handle denial
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn approval_denied_skips_tool_execution() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "delete everything" }],
        "edge_tools": [tool_schema("bash")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-deny-1", "bash", json!({"command": "rm -rf /"}))] },
            { "full_text": "Understood, I won't delete anything." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(!full.contains("won't delete"));
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 4: Multi-round — only first round runs in single-call mode
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_round_three_tool_calls_then_final_answer() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "fix the bug in auth.go" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("str_replace"), tool_schema("bash")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-r1", "read_file", json!({"path": "auth.go"}))] },
            { "tool_calls": [tool_call("tc-r2", "str_replace", json!({"path": "auth.go", "old_str": "bug", "new_str": "fix"}))] },
            { "tool_calls": [tool_call("tc-r3", "bash", json!({"command": "go test ./..."}))] },
            { "full_text": "Bug fixed and tests pass." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(!full.contains("Bug fixed and tests pass"));
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 5: Tool result timeout — irrelevant in single-call mode
// ═══════════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════════
// Test 6: SSE event ordering — session_info before content, turn_complete last
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn sse_event_ordering_contract() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "hello" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-ord-1", "read_file", json!({"path": "x.txt"}))] },
            { "full_text": "Done." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert!(!events.is_empty(), "should have SSE events");

    // turn_complete must be the last application event
    let app_events: Vec<_> = events
        .iter()
        .filter(|e| e["type"].as_str() != Some("ping"))
        .collect();
    let last = app_events.last().expect("should have app events");
    assert_eq!(
        last["type"], "turn_complete",
        "last app event must be turn_complete, got: {last}"
    );

    // session_info should appear before any content
    let session_info_idx = events.iter().position(|e| e["type"] == "session_info");
    let first_content = events
        .iter()
        .position(|e| e["type"] == "text_delta" || e["type"] == "tool_call_start");
    if let (Some(si), Some(fc)) = (session_info_idx, first_content) {
        assert!(
            si < fc,
            "session_info ({si}) must come before content ({fc})"
        );
    }

    assert!(!full.contains("Done."), "second round should not run");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 7: Single tool call — baseline single-call behavior
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_tool_round_trip_baseline() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "list files" }],
        "edge_tools": [tool_schema("glob")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-ping-1", "glob", json!({"path": "."}))] },
            { "full_text": "Found 5 files." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(
        !full.contains("Found 5 files"),
        "second round should not run"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 8: Text-only turn (no tools) — baseline sanity
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn text_only_turn_no_tools() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "what is 2+2?" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "2+2 equals 4." }]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert!(full.contains("2+2 equals 4"), "missing LLM response text");
    assert!(events_of_type(&events, "tool_request").is_empty());
    let completes = events_of_type(&events, "turn_complete");
    assert!(!completes.is_empty());
    assert_eq!(completes[0]["has_tool_calls"], false);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 9: Tool error — bridge does not execute, so error is CLI's concern
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tool_error_result_propagates_to_next_round() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "read secret.key" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-err-1", "read_file", json!({"path": "secret.key"}))] },
            { "full_text": "Permission error." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(
        !full.contains("Permission error"),
        "second round should not run"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 10: Duplicate tool result — irrelevant in single-call mode
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn duplicate_tool_result_does_not_break_flow() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "check status" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-dup-1", "read_file", json!({"path": "status.txt"}))] },
            { "full_text": "Status is healthy." }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(
        !full.contains("Status is healthy"),
        "second round should not run"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 11: Mixed approval + read-only — bridge treats all tool calls equally
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mixed_approval_and_readonly_in_same_round() {
    init_env();
    let app = build_app_with_capture(Capture::default());

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

    let (full, events) = chat_turn_events(&app, payload).await;
    assert_single_call(&events, &full);
    assert!(
        !full.contains("Config updated"),
        "second round should not run"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 11b: empty edge_tools — bridge still returns tool calls, does not execute
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn empty_edge_tools_read_file_executes_on_server_without_tools_result() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "read hello" }],
        "edge_tools": [],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-srv-rf", "read_file", json!({"path": "hello.txt"}))] },
            { "full_text": "round two ok" }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert!(
        !full.contains("round two ok"),
        "second round should not run"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert!(!completes.is_empty());
    assert_eq!(completes[0]["has_tool_calls"], true);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test 11c: empty edge_tools + approval tool — bridge does not fast-fail
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn empty_edge_tools_approval_required_fast_fails() {
    init_env();
    let app = build_app_with_capture(Capture::default());

    let payload = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "write config" }],
        "edge_tools": [],
        "test_llm_rounds": [
            { "tool_calls": [tool_call("tc-srv-wf", "write_file", json!({"path": "out.txt", "content": "x"}))] },
            { "full_text": "round two ok" }
        ]
    });

    let (full, events) = chat_turn_events(&app, payload).await;
    assert!(
        !full.contains("round two ok"),
        "second round should not run"
    );
    let completes = events_of_type(&events, "turn_complete");
    assert!(!completes.is_empty());
    assert_eq!(completes[0]["has_tool_calls"], true);
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

    let (_, events_a) = chat_turn_events(&app, payload_tools).await;
    let complete_a = events_a
        .iter()
        .find(|e| e["type"] == "turn_complete")
        .expect("turn_complete");
    assert_eq!(complete_a["has_tool_calls"], true);

    // Case B: turn WITHOUT tool calls
    let payload_text = json!({
        "agent_id": "rt-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "hello" }]
    });

    let (_, events_b) = chat_turn_events(&app, payload_text).await;
    let complete_b = events_b
        .iter()
        .find(|e| e["type"] == "turn_complete")
        .expect("turn_complete");
    assert_eq!(
        complete_b["has_tool_calls"], false,
        "text-only: {complete_b}"
    );
}
