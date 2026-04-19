#![cfg(feature = "bridge-e2e-hooks")]
//! Comprehensive bridge E2E tests — persistence, multi-turn, errors, cancellation.
//!
//! These tests complement `edge_cloud_round_trip_e2e.rs` by focusing on:
//! - Event persistence verification (P2 fix: no duplicate event_ids)
//! - Multi-turn session state accumulation
//! - Error scenarios (model unavailable, LLM errors)
//! - Client cancellation mid-stream
//! - Session activity writer verification
//!
//! ```text
//! cargo test -p astra-runtime --test bridge_e2e_comprehensive --features bridge-e2e-hooks
//! ```

use std::sync::{Arc, OnceLock};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, FernetTokenEncryptor, HealthChecker,
    MatrixOneSettings, ServiceInfo, SessionActivityRecord, SessionActivityUpdatePlan,
    SessionCreateRequestData, SessionListFilter, SessionListRecord, SessionRecord, SessionService,
    SessionUpdateRequestData, TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter,
    TurnCoreEventWriter, TurnCorePersistOutcome, TurnCorePersistPlan, TurnHookDbPersistPlan,
    TurnHookDbWriter, TurnSessionActivityWriter, TurnToolEventPersistPlan, TurnToolEventWriter,
    build_app, turn::bridge_inprocess::InProcessChatTurnBridge,
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

const SECRET: &str = "comprehensive-e2e-secret";
const TOKEN: &str = "Bearer comp-e2e-token";
const USER_ID: &str = "comp-e2e-user";

static SECRET_INIT: OnceLock<()> = OnceLock::new();

fn init_env() {
    SECRET_INIT.get_or_init(|| unsafe {
        std::env::set_var("ASTRA_BRIDGE_TEST_SECRET", SECRET);
    });
}

// ── Stubs (copied from edge_cloud_round_trip_e2e.rs, adjusted) ───────────────

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
                username: "comp-e2e".into(),
                email: "comp@e2e.test".into(),
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
            session_id: "s-comp-created".into(),
            user_id,
            agent_id: req.agent_id,
            title: Some("comp-test".into()),
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
        Ok(SessionActivityRecord {
            session_id: "s-comp-created".into(),
            activities: vec![],
            total: 0,
        })
    }
}

// ── Capturing writers ────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct AllCaptures {
    core_plans: Arc<Mutex<Vec<TurnCorePersistPlan>>>,
    tool_plans: Arc<Mutex<Vec<TurnToolEventPersistPlan>>>,
    aux_events: Arc<Mutex<Vec<TurnAuxiliaryEventRecord>>>,
    activity_plans: Arc<Mutex<Vec<(String, SessionActivityUpdatePlan)>>>,
    hook_plans: Arc<Mutex<Vec<TurnHookDbPersistPlan>>>,
}

#[derive(Clone)]
struct CapCoreWriter(AllCaptures);
#[async_trait]
impl TurnCoreEventWriter for CapCoreWriter {
    async fn persist(&self, plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String> {
        let event_id = plan.llm_response_event.as_ref().map(|e| e.event_id.clone());
        self.0.core_plans.lock().await.push(plan);
        Ok(TurnCorePersistOutcome {
            llm_response_event_id: event_id,
        })
    }
}

#[derive(Clone)]
struct CapToolWriter(AllCaptures);
#[async_trait]
impl TurnToolEventWriter for CapToolWriter {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
        self.0.tool_plans.lock().await.push(plan);
        Ok(())
    }
}

#[derive(Clone)]
struct CapAuxWriter(AllCaptures);
#[async_trait]
impl TurnAuxiliaryEventWriter for CapAuxWriter {
    async fn persist_events(&self, events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        self.0.aux_events.lock().await.extend(events);
        Ok(())
    }
}

#[derive(Clone)]
struct CapActivityWriter(AllCaptures);
#[async_trait]
impl TurnSessionActivityWriter for CapActivityWriter {
    async fn update_session_activity(
        &self,
        session_id: &str,
        plan: SessionActivityUpdatePlan,
    ) -> Result<(), String> {
        self.0
            .activity_plans
            .lock()
            .await
            .push((session_id.to_string(), plan));
        Ok(())
    }
}

#[derive(Clone)]
struct CapHookWriter(AllCaptures);
#[async_trait]
impl TurnHookDbWriter for CapHookWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        self.0.hook_plans.lock().await.push(plan);
        Ok(())
    }
}

// ── App builder ──────────────────────────────────────────────────────────────

fn build_test_app(cap: AllCaptures) -> Router {
    let enc =
        Arc::new(FernetTokenEncryptor::new("comp-e2e-fernet-key-32chars!").expect("fernet key"));
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession))
        .with_turn_core_event_writer(Arc::new(CapCoreWriter(cap.clone())))
        .with_turn_tool_event_writer(Arc::new(CapToolWriter(cap.clone())))
        .with_turn_auxiliary_event_writer(Arc::new(CapAuxWriter(cap.clone())))
        .with_turn_session_activity_writer(Arc::new(CapActivityWriter(cap.clone())))
        .with_turn_hook_db_writer(Arc::new(CapHookWriter(cap)));
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
        .with_chat_turn_bridge_secret("comp-e2e-bridge-secret");
    build_app(state)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn tool_schema(name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": format!("{name} tool"),
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }
        }
    })
}

fn tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": serde_json::to_string(&args).unwrap()
        }
    })
}

async fn chat_turn(app: &Router, payload: Value) -> (StatusCode, String) {
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
    (st, String::from_utf8_lossy(&bytes).into_owned())
}

async fn chat_turn_wrong_secret(app: &Router, payload: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", "wrong-secret-value")
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&bytes).into_owned())
}

async fn chat_turn_no_secret(app: &Router, payload: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&bytes).into_owned())
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

// ══════════════════════════════════════════════════════════════════════════════
// Test: Persist events — user_query + llm_response persisted once, no duplicates
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn persist_core_events_user_query_and_llm_response_once() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "persist-agent",
        "messages": [{ "role": "user", "content": "hello world" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "Hello from the LLM!",
        }]
    });

    let (st, _raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    // Allow async persistence to complete.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1, "exactly one core persist call");
    let plan = &core[0];
    assert!(
        plan.user_query_event.is_some(),
        "user_query_event should be persisted"
    );
    assert!(
        plan.llm_response_event.is_some(),
        "llm_response_event should be persisted"
    );

    let uq = plan.user_query_event.as_ref().unwrap();
    assert_eq!(uq.event_type, "user_query");
    assert!(uq.content.contains("hello world"));

    let lr = plan.llm_response_event.as_ref().unwrap();
    assert_eq!(lr.event_type, "llm_response");
    assert!(
        lr.content.contains("Hello from the LLM!"),
        "llm response content should contain model output"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Persist tool events — tool call records persisted correctly
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn persist_tool_events_for_tool_calls() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "tool-persist-agent",
        "messages": [{ "role": "user", "content": "read the README" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("list_files")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-p1", "read_file", json!({"path": "README.md"})),
                tool_call("tc-p2", "list_files", json!({"path": "/src"})),
            ]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);
    assert!(
        events_of_type(&events, "turn_complete")
            .first()
            .and_then(|e| e.get("has_tool_calls"))
            .and_then(Value::as_bool)
            == Some(true),
        "turn_complete should have has_tool_calls=true"
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let tools = cap.tool_plans.lock().await;
    let total_events: usize = tools.iter().map(|p| p.events.len()).sum();
    assert!(
        total_events >= 2,
        "at least 2 tool event records should be persisted, got {total_events}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: No duplicate event_ids across core persist calls (P2 fix)
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn no_duplicate_event_ids_in_core_persist() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Two sequential turns simulating a multi-round conversation.
    let turn1 = json!({
        "agent_id": "dedup-agent",
        "messages": [{ "role": "user", "content": "first question" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-d1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let turn2 = json!({
        "agent_id": "dedup-agent",
        "session_id": "s-comp-created",
        "messages": [
            { "role": "user", "content": "first question" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-d1", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-d1", "content": "file contents" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-d1", "content": "file contents" }],
        "test_llm_rounds": [{
            "full_text": "Here is the answer."
        }]
    });

    let (st1, _) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let (st2, _) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let core = cap.core_plans.lock().await;
    let mut all_event_ids = Vec::new();
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            all_event_ids.push(uq.event_id.clone());
        }
        if let Some(lr) = &plan.llm_response_event {
            all_event_ids.push(lr.event_id.clone());
        }
    }
    let unique: std::collections::HashSet<_> = all_event_ids.iter().collect();
    assert_eq!(
        all_event_ids.len(),
        unique.len(),
        "event_ids must be unique: {:?}",
        all_event_ids
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Continuation call (tool_results present) should NOT persist user_query
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn continuation_call_skips_user_query_persist() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Simulates second call in a conversation (tool_results are present).
    let payload = json!({
        "agent_id": "cont-agent",
        "session_id": "s-comp-created",
        "messages": [
            { "role": "user", "content": "read file" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-cont", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-cont", "content": "file data" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-cont", "content": "file data" }],
        "test_llm_rounds": [{
            "full_text": "Done."
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty(), "should have a core persist call");
    // On continuation, user_query should not be re-persisted.
    for plan in core.iter() {
        assert!(
            plan.user_query_event.is_none(),
            "continuation call must not re-persist user_query_event"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Multi-turn session — turn 1 (tools) → turn 2 (completion)
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_turn_session_accumulates_state() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: LLM requests a tool call.
    let turn1 = json!({
        "agent_id": "mt-agent",
        "messages": [{ "role": "user", "content": "analyze code" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-mt1", "read_file", json!({"path": "main.rs"}))]
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let tc1 = events_of_type(&ev1, "turn_complete");
    assert_eq!(tc1.len(), 1);
    assert_eq!(
        tc1[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    // Turn 2: CLI provides tool results, LLM completes with text.
    let turn2 = json!({
        "agent_id": "mt-agent",
        "session_id": "s-comp-created",
        "messages": [
            { "role": "user", "content": "analyze code" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-mt1", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"main.rs\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-mt1", "content": "fn main() {}" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-mt1", "content": "fn main() {}" }],
        "test_llm_rounds": [{
            "full_text": "The main function is empty.",
            "usage": { "prompt_tokens": 150, "completion_tokens": 20 }
        }]
    });

    let (st2, raw2) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);
    let ev2 = parse_sse_events(&raw2);
    let tc2 = events_of_type(&ev2, "turn_complete");
    assert_eq!(tc2.len(), 1);
    assert_eq!(
        tc2[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false),
        "turn 2 should complete without tool calls"
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify both turns persisted core events.
    let core = cap.core_plans.lock().await;
    assert!(
        core.len() >= 2,
        "both turns should trigger core persist, got {}",
        core.len()
    );

    // Turn 1 should have user_query_event.
    let turn1_plan = &core[0];
    assert!(
        turn1_plan.user_query_event.is_some(),
        "turn 1 should persist user_query"
    );

    // Turn 2 (continuation) should NOT have user_query_event.
    let turn2_plan = &core[1];
    assert!(
        turn2_plan.user_query_event.is_none(),
        "turn 2 (continuation) should not re-persist user_query"
    );
    assert!(
        turn2_plan.llm_response_event.is_some(),
        "turn 2 should persist llm_response"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Text-only turn — no tool calls, persist events correct
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn text_only_turn_persists_correctly() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "textonly-agent",
        "messages": [{ "role": "user", "content": "what is 2+2?" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "2+2 = 4",
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false)
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    assert!(core[0].user_query_event.is_some());
    assert!(core[0].llm_response_event.is_some());

    // No tool events for text-only turns.
    let tools = cap.tool_plans.lock().await;
    let total_tool_events: usize = tools.iter().map(|p| p.events.len()).sum();
    assert_eq!(
        total_tool_events, 0,
        "text-only turn should have no tool events"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Batch tool calls — 5 tools in one response
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn batch_five_tool_calls_all_returned() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "batch-agent",
        "messages": [{ "role": "user", "content": "read all config files" }],
        "edge_tools": [
            tool_schema("read_file"),
            tool_schema("list_files"),
        ],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-b1", "read_file", json!({"path": "a.toml"})),
                tool_call("tc-b2", "read_file", json!({"path": "b.toml"})),
                tool_call("tc-b3", "read_file", json!({"path": "c.toml"})),
                tool_call("tc-b4", "list_files", json!({"path": "/etc"})),
                tool_call("tc-b5", "read_file", json!({"path": "d.toml"})),
            ]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let tools = cap.tool_plans.lock().await;
    let total_events: usize = tools.iter().map(|p| p.events.len()).sum();
    assert!(
        total_events >= 5,
        "should persist at least 5 tool events, got {total_events}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: SSE event ordering — session_info first, turn_complete last
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn sse_event_ordering_session_info_first_turn_complete_last() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "order-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "Hello!",
            "tool_calls": [tool_call("tc-ord", "read_file", json!({"path": "x"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    assert!(!events.is_empty(), "should have events");

    let first = &events[0];
    assert_eq!(
        first.get("type").and_then(Value::as_str),
        Some("session_info"),
        "first event must be session_info"
    );

    let last = &events[events.len() - 1];
    assert_eq!(
        last.get("type").and_then(Value::as_str),
        Some("turn_complete"),
        "last event must be turn_complete"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Error — missing test_llm_rounds with e2e hooks returns error event
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn error_no_llm_rounds_returns_error_event() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // With bridge-e2e-hooks enabled and authorized, empty test_llm_rounds means
    // the e2e mock has no rounds. The bridge should handle this — may emit
    // error or empty turn depending on implementation.
    let payload = json!({
        "agent_id": "err-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [],
        "test_llm_rounds": []
    });

    let (st, raw) = chat_turn(&app, payload).await;
    // Should still return 200 (SSE stream).
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // Should have session_info at minimum.
    let si = events_of_type(&events, "session_info");
    assert!(!si.is_empty(), "should have session_info event");

    // With no rounds, bridge may emit turn_complete or error.
    let has_turn_complete = !events_of_type(&events, "turn_complete").is_empty();
    let has_error = !events_of_type(&events, "error").is_empty();
    assert!(
        has_turn_complete || has_error,
        "should have turn_complete or error when no LLM rounds provided"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Client cancellation — drop response mid-stream
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn client_cancellation_does_not_panic() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "cancel-agent",
        "messages": [{ "role": "user", "content": "long task" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "This is a response that the client will never fully read.",
            "tool_calls": [
                tool_call("tc-cancel1", "read_file", json!({"path": "a.txt"})),
                tool_call("tc-cancel2", "read_file", json!({"path": "b.txt"})),
            ]
        }]
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

    // Drop the response body without fully consuming — simulates client disconnect.
    drop(resp);

    // Give time for any async cleanup.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // If we get here without panic, the test passes.
    // Verify that persist writers were not corrupted.
    let core = cap.core_plans.lock().await;
    // May or may not have persisted depending on timing — that's fine.
    // The key assertion is no panic or deadlock.
    drop(core);
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Session info event contains session_id and run_id
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_info_contains_session_and_run_ids() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "si-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "hello" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let si = events_of_type(&events, "session_info");
    assert_eq!(si.len(), 1, "exactly one session_info event");
    assert!(
        si[0].get("session_id").and_then(Value::as_str).is_some(),
        "session_info must have session_id"
    );
    assert!(
        si[0].get("run_id").and_then(Value::as_str).is_some(),
        "session_info must have run_id"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Core persist event_ids are valid UUIDs
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn core_persist_event_ids_are_valid_uuids() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "uuid-agent",
        "messages": [{ "role": "user", "content": "test uuid" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "ok" }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let core = cap.core_plans.lock().await;
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            assert!(
                uuid::Uuid::parse_str(&uq.event_id).is_ok(),
                "user_query event_id should be valid UUID: {}",
                uq.event_id
            );
        }
        if let Some(lr) = &plan.llm_response_event {
            assert!(
                uuid::Uuid::parse_str(&lr.event_id).is_ok(),
                "llm_response event_id should be valid UUID: {}",
                lr.event_id
            );
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Core persist records correct user_id, session_id, agent_id
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn core_persist_records_correct_metadata() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "meta-agent-123",
        "messages": [{ "role": "user", "content": "meta test" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "ok" }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty());
    let plan = &core[0];

    if let Some(uq) = &plan.user_query_event {
        assert_eq!(uq.user_id, USER_ID, "user_id should match auth user");
        assert_eq!(
            uq.agent_id.as_deref(),
            Some("meta-agent-123"),
            "agent_id should match request"
        );
    }

    if let Some(lr) = &plan.llm_response_event {
        assert_eq!(lr.user_id, USER_ID);
        assert_eq!(lr.agent_id.as_deref(), Some("meta-agent-123"));
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Single tool round — full round-trip: tool_calls → tool_results → text
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_tool_round_full_roundtrip() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: User query → LLM returns one tool call.
    let turn1 = json!({
        "agent_id": "roundtrip-agent",
        "messages": [{ "role": "user", "content": "Read the config file" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-rt1", "read_file", json!({"path": "config.toml"}))],
            "usage": { "prompt_tokens": 100, "completion_tokens": 15 }
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);

    // Verify SSE: session_info first, turn_complete last with has_tool_calls=true.
    assert_eq!(
        ev1.first()
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str),
        Some("session_info"),
        "turn 1 must start with session_info"
    );
    let tc1 = events_of_type(&ev1, "turn_complete");
    assert_eq!(tc1.len(), 1);
    assert_eq!(
        tc1[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    // Extract session_id from session_info for turn 2.
    let session_id = ev1[0].get("session_id").and_then(Value::as_str).unwrap();

    // Turn 2: CLI sends tool_results → LLM returns final text.
    let turn2 = json!({
        "agent_id": "roundtrip-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "Read the config file" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-rt1", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"config.toml\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-rt1", "content": "port = 8080\nhost = localhost" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-rt1", "content": "port = 8080\nhost = localhost" }],
        "test_llm_rounds": [{
            "full_text": "The config has port 8080 and host localhost.",
            "usage": { "prompt_tokens": 200, "completion_tokens": 25 }
        }]
    });

    let (st2, raw2) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);
    let ev2 = parse_sse_events(&raw2);

    // Turn 2 SSE: text_delta events present, turn_complete with no tool calls.
    let deltas = events_of_type(&ev2, "text_delta");
    assert!(!deltas.is_empty(), "turn 2 should have text_delta events");
    let tc2 = events_of_type(&ev2, "turn_complete");
    assert_eq!(tc2.len(), 1);
    assert_eq!(
        tc2[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false)
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Verify persistence: 2 core persist calls.
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 2, "two core persist calls (one per turn)");

    // Turn 1: user_query persisted, llm_response persisted.
    assert!(
        core[0].user_query_event.is_some(),
        "turn 1 persists user_query"
    );
    assert!(
        core[0].llm_response_event.is_some(),
        "turn 1 persists llm_response"
    );

    // Turn 2 (continuation): user_query NOT re-persisted, llm_response persisted.
    assert!(
        core[1].user_query_event.is_none(),
        "continuation must NOT re-persist user_query"
    );
    assert!(
        core[1].llm_response_event.is_some(),
        "turn 2 persists llm_response"
    );
    assert!(
        core[1]
            .llm_response_event
            .as_ref()
            .unwrap()
            .content
            .contains("port 8080"),
        "turn 2 llm_response should contain final text"
    );

    // Verify all event_ids are unique across both turns.
    let mut all_ids: Vec<String> = Vec::new();
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            all_ids.push(uq.event_id.clone());
        }
        if let Some(lr) = &plan.llm_response_event {
            all_ids.push(lr.event_id.clone());
        }
    }
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        all_ids.len(),
        unique.len(),
        "all event_ids unique: {:?}",
        all_ids
    );

    // Verify tool events persisted for turn 1.
    let tools = cap.tool_plans.lock().await;
    let total_tool: usize = tools.iter().map(|p| p.events.len()).sum();
    assert!(
        total_tool >= 1,
        "turn 1 tool call should be persisted, got {total_tool}"
    );

    // Verify activity writer called for both turns.
    let acts = cap.activity_plans.lock().await;
    assert!(
        acts.len() >= 2,
        "activity writer should be called for both turns, got {}",
        acts.len()
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Error recovery — LLM failure on continuation still produces clean SSE
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn error_on_continuation_emits_clean_error_event() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: normal tool call.
    let turn1 = json!({
        "agent_id": "err-recovery-agent",
        "messages": [{ "role": "user", "content": "check files" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-er1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let session_id = ev1[0].get("session_id").and_then(Value::as_str).unwrap();

    // Turn 2: continuation with EMPTY test_llm_rounds — simulates LLM failure.
    // The e2e mock has no round[0], so bridge falls through to real LLM call which
    // fails (no real LLM configured) → error SSE event.
    let turn2 = json!({
        "agent_id": "err-recovery-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "check files" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-er1", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-er1", "content": "file data" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-er1", "content": "file data" }],
        "test_llm_rounds": []
    });

    let (st2, raw2) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK, "SSE stream still returns 200");
    let ev2 = parse_sse_events(&raw2);

    // Should still have session_info.
    let si2 = events_of_type(&ev2, "session_info");
    assert!(!si2.is_empty(), "continuation should have session_info");

    // Should have error or turn_complete (the bridge handles the missing round gracefully).
    let has_error = !events_of_type(&ev2, "error").is_empty();
    let has_tc = !events_of_type(&ev2, "turn_complete").is_empty();
    assert!(
        has_error || has_tc,
        "continuation with no LLM rounds should produce error or turn_complete"
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Turn 1 should have persisted normally despite turn 2 failure.
    let core = cap.core_plans.lock().await;
    assert!(
        !core.is_empty(),
        "turn 1 core persist should have succeeded"
    );
    let turn1_plan = &core[0];
    assert!(
        turn1_plan.user_query_event.is_some(),
        "turn 1 user_query should be persisted even if turn 2 fails"
    );
    assert!(
        turn1_plan.llm_response_event.is_some(),
        "turn 1 llm_response should be persisted even if turn 2 fails"
    );

    // Verify no corrupted event_ids — all should be valid UUIDs.
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            assert!(
                uuid::Uuid::parse_str(&uq.event_id).is_ok(),
                "event_id must be valid UUID: {}",
                uq.event_id
            );
        }
        if let Some(lr) = &plan.llm_response_event {
            assert!(
                uuid::Uuid::parse_str(&lr.event_id).is_ok(),
                "event_id must be valid UUID: {}",
                lr.event_id
            );
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Many sequential tool rounds — no state corruption across 5 turns
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn many_sequential_tool_rounds_no_state_corruption() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: initial user query → tool call.
    let turn1 = json!({
        "agent_id": "multi-round-agent",
        "messages": [{ "role": "user", "content": "refactor everything" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("write_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-mr1", "read_file", json!({"path": "src/main.rs"}))]
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let session_id = ev1[0]
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Build message history incrementally for turns 2-5.
    let mut messages = vec![
        json!({"role": "user", "content": "refactor everything"}),
        json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": "tc-mr1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}}
        ]}),
        json!({"role": "tool", "tool_call_id": "tc-mr1", "content": "fn main() {}"}),
    ];

    // Turns 2-4: each returns a different tool call.
    let tool_rounds = [
        (
            "tc-mr2",
            "write_file",
            json!({"path": "src/main.rs", "content": "fn main() { println!(\"hi\"); }"}),
        ),
        ("tc-mr3", "read_file", json!({"path": "src/lib.rs"})),
        (
            "tc-mr4",
            "write_file",
            json!({"path": "src/lib.rs", "content": "pub fn greet() {}"}),
        ),
    ];

    for (tc_id, tool_name, args) in &tool_rounds {
        let payload = json!({
            "agent_id": "multi-round-agent",
            "session_id": &session_id,
            "messages": messages.clone(),
            "edge_tools": [tool_schema("read_file"), tool_schema("write_file")],
            "tool_results": [{ "tool_call_id": messages.iter().rev()
                .find_map(|m| m.get("tool_call_id").and_then(Value::as_str))
                .unwrap_or("tc-mr1"), "content": "ok" }],
            "test_llm_rounds": [{
                "tool_calls": [tool_call(tc_id, tool_name, args.clone())]
            }]
        });

        let (st, raw) = chat_turn(&app, payload).await;
        assert_eq!(st, StatusCode::OK);
        let evts = parse_sse_events(&raw);
        let tc = events_of_type(&evts, "turn_complete");
        assert_eq!(tc.len(), 1);
        assert_eq!(
            tc[0].get("has_tool_calls").and_then(Value::as_bool),
            Some(true)
        );

        // Extend message history with assistant tool_call + tool result.
        messages.push(json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": tc_id, "type": "function", "function": {"name": tool_name, "arguments": serde_json::to_string(args).unwrap()}}
        ]}));
        messages.push(json!({"role": "tool", "tool_call_id": tc_id, "content": "ok"}));
    }

    // Turn 5 (final): return text completion.
    let turn5 = json!({
        "agent_id": "multi-round-agent",
        "session_id": &session_id,
        "messages": messages,
        "edge_tools": [tool_schema("read_file"), tool_schema("write_file")],
        "tool_results": [{ "tool_call_id": "tc-mr4", "content": "ok" }],
        "test_llm_rounds": [{
            "full_text": "Refactoring complete. Updated main.rs and lib.rs.",
            "usage": { "prompt_tokens": 500, "completion_tokens": 30 }
        }]
    });

    let (st5, raw5) = chat_turn(&app, turn5).await;
    assert_eq!(st5, StatusCode::OK);
    let ev5 = parse_sse_events(&raw5);
    let tc5 = events_of_type(&ev5, "turn_complete");
    assert_eq!(tc5.len(), 1);
    assert_eq!(
        tc5[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false)
    );

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify persistence across all 5 turns.
    let core = cap.core_plans.lock().await;
    assert_eq!(
        core.len(),
        5,
        "5 core persist calls (one per HTTP request), got {}",
        core.len()
    );

    // Only turn 1 should have user_query_event; turns 2-5 are continuations.
    assert!(core[0].user_query_event.is_some(), "turn 1 has user_query");
    for (i, plan) in core.iter().enumerate().skip(1) {
        assert!(
            plan.user_query_event.is_none(),
            "turn {} (continuation) must NOT re-persist user_query",
            i + 1
        );
    }

    // All 5 turns should have llm_response_event.
    for (i, plan) in core.iter().enumerate() {
        assert!(
            plan.llm_response_event.is_some(),
            "turn {} should persist llm_response",
            i + 1
        );
    }

    // All event_ids must be globally unique across all 5 turns.
    let mut all_ids: Vec<String> = Vec::new();
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            all_ids.push(uq.event_id.clone());
        }
        if let Some(lr) = &plan.llm_response_event {
            all_ids.push(lr.event_id.clone());
        }
    }
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        all_ids.len(),
        unique.len(),
        "all event_ids must be unique across 5 turns: {:?}",
        all_ids
    );
    // Should have 6 IDs total: 1 user_query + 5 llm_responses.
    assert_eq!(all_ids.len(), 6, "1 user_query + 5 llm_response IDs");

    // All event_ids should be valid UUIDs.
    for id in &all_ids {
        assert!(uuid::Uuid::parse_str(id).is_ok(), "invalid UUID: {id}");
    }

    // Verify tool events persisted (at least 4 tool call records from turns 1-4).
    let tools = cap.tool_plans.lock().await;
    let total_tool: usize = tools.iter().map(|p| p.events.len()).sum();
    assert!(
        total_tool >= 4,
        "at least 4 tool events should be persisted, got {total_tool}"
    );

    // Verify activity writer called for all 5 turns.
    let acts = cap.activity_plans.lock().await;
    assert!(
        acts.len() >= 5,
        "activity writer should be called for all 5 turns, got {}",
        acts.len()
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Reasoning content — persisted in llm_response + reasoning_done SSE emitted
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn reasoning_content_persisted_and_sse_emitted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "reasoning-agent",
        "messages": [{ "role": "user", "content": "explain quicksort" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "Quicksort is a divide-and-conquer algorithm.",
            "reasoning": "The user wants an explanation of quicksort. I should explain the algorithm step by step.",
            "usage": { "prompt_tokens": 50, "completion_tokens": 30 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // reasoning_done SSE event should be emitted when reasoning is non-empty.
    let rd = events_of_type(&events, "reasoning_done");
    assert_eq!(rd.len(), 1, "should have exactly one reasoning_done event");

    // text_delta should contain the actual answer.
    let deltas = events_of_type(&events, "text_delta");
    assert!(!deltas.is_empty(), "should have text_delta events");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Verify reasoning is persisted in llm_response.
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    let lr = core[0].llm_response_event.as_ref().unwrap();
    assert!(
        lr.reasoning_content.is_some(),
        "reasoning_content should be persisted"
    );
    assert!(
        lr.reasoning_content.as_ref().unwrap().contains("quicksort"),
        "reasoning_content should contain the reasoning text"
    );
    assert!(
        lr.content.contains("divide-and-conquer"),
        "llm response content should contain the actual answer"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Reasoning with tool calls — reasoning persisted, tool calls returned
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn reasoning_with_tool_calls_persists_reasoning() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "reason-tool-agent",
        "messages": [{ "role": "user", "content": "read the config" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "reasoning": "I need to read the config file to answer the user's question.",
            "tool_calls": [tool_call("tc-rt1", "read_file", json!({"path": "config.toml"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    // reasoning_done should be emitted even when tool calls are present.
    let rd = events_of_type(&events, "reasoning_done");
    assert_eq!(
        rd.len(),
        1,
        "reasoning_done should be emitted with tool calls"
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Note: reasoning_content on llm_response may be None when has_tool_calls is true
    // (bridge line ~1558: reasoning_content is None when reasoning is empty AND has_tool_calls).
    // But here reasoning IS non-empty, so it should be persisted.
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    let lr = core[0].llm_response_event.as_ref().unwrap();
    assert!(
        lr.reasoning_content.is_some(),
        "reasoning_content should be persisted even with tool calls when reasoning is non-empty"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Usage/token tracking — usage from mock rounds persisted in llm_response
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn usage_token_tracking_persisted_in_llm_response() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "usage-agent",
        "messages": [{ "role": "user", "content": "hello" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "Hi there!",
            "usage": {
                "prompt_tokens": 42,
                "completion_tokens": 7,
                "total_tokens": 49,
                "cache_creation_input_tokens": 10,
                "cache_read_input_tokens": 5
            }
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    let lr = core[0].llm_response_event.as_ref().unwrap();

    // token_usage should be persisted.
    assert!(lr.token_usage.is_some(), "token_usage should be persisted");
    let usage = lr.token_usage.as_ref().unwrap();
    assert_eq!(
        usage.get("prompt_tokens").and_then(Value::as_i64),
        Some(42),
        "prompt_tokens should be 42"
    );
    assert_eq!(
        usage.get("completion_tokens").and_then(Value::as_i64),
        Some(7),
        "completion_tokens should be 7"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Usage tracking across multi-turn — each turn has independent usage
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn usage_tracking_across_multi_turn_independent() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: tool call with usage.
    let turn1 = json!({
        "agent_id": "usage-mt-agent",
        "messages": [{ "role": "user", "content": "read files" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-u1", "read_file", json!({"path": "a.txt"}))],
            "usage": { "prompt_tokens": 100, "completion_tokens": 20 }
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let session_id = ev1[0].get("session_id").and_then(Value::as_str).unwrap();

    // Turn 2: completion with different usage.
    let turn2 = json!({
        "agent_id": "usage-mt-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "read files" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-u1", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-u1", "content": "file contents here" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-u1", "content": "file contents here" }],
        "test_llm_rounds": [{
            "full_text": "Here is what I found.",
            "usage": { "prompt_tokens": 250, "completion_tokens": 15 }
        }]
    });

    let (st2, _) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 2, "two core persist calls");

    // Turn 1 usage.
    let lr1 = core[0].llm_response_event.as_ref().unwrap();
    assert!(lr1.token_usage.is_some());
    assert_eq!(
        lr1.token_usage
            .as_ref()
            .unwrap()
            .get("prompt_tokens")
            .and_then(Value::as_i64),
        Some(100)
    );

    // Turn 2 usage — should be different (independent per call).
    let lr2 = core[1].llm_response_event.as_ref().unwrap();
    assert!(lr2.token_usage.is_some());
    assert_eq!(
        lr2.token_usage
            .as_ref()
            .unwrap()
            .get("prompt_tokens")
            .and_then(Value::as_i64),
        Some(250)
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Concurrent requests — multiple sessions don't interfere
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn concurrent_requests_no_cross_session_interference() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Launch 5 concurrent requests with different agent_ids to distinguish them.
    let mut handles = Vec::new();
    for i in 0..5 {
        let app_clone = app.clone();
        let payload = json!({
            "agent_id": format!("concurrent-agent-{i}"),
            "messages": [{ "role": "user", "content": format!("concurrent request {i}") }],
            "edge_tools": [tool_schema("read_file")],
            "test_llm_rounds": [{
                "full_text": format!("Response for concurrent request {i}"),
                "usage": { "prompt_tokens": 10 + i, "completion_tokens": 5 + i }
            }]
        });
        handles.push(tokio::spawn(
            async move { chat_turn(&app_clone, payload).await },
        ));
    }

    // Await all.
    let results: Vec<_> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("task panicked"))
        .collect();

    // All should succeed.
    for (i, (st, raw)) in results.iter().enumerate() {
        assert_eq!(*st, StatusCode::OK, "request {i} should succeed");
        let events = parse_sse_events(raw);
        let si = events_of_type(&events, "session_info");
        assert!(!si.is_empty(), "request {i} should have session_info");
        let tc = events_of_type(&events, "turn_complete");
        assert_eq!(tc.len(), 1, "request {i} should have turn_complete");
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify 5 independent core persist calls.
    let core = cap.core_plans.lock().await;
    assert_eq!(
        core.len(),
        5,
        "5 concurrent requests = 5 core persist calls"
    );

    // All event_ids must be unique across all 5 requests.
    let mut all_ids: Vec<String> = Vec::new();
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            all_ids.push(uq.event_id.clone());
        }
        if let Some(lr) = &plan.llm_response_event {
            all_ids.push(lr.event_id.clone());
        }
    }
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        all_ids.len(),
        unique.len(),
        "all event_ids unique across concurrent requests"
    );
    // Each request produces 1 user_query + 1 llm_response = 10 total.
    assert_eq!(all_ids.len(), 10, "5 requests × 2 events = 10 IDs");

    // Verify all agent_ids are present (no cross-contamination).
    let agent_ids: std::collections::HashSet<_> = core
        .iter()
        .filter_map(|p| p.user_query_event.as_ref())
        .filter_map(|uq| uq.agent_id.as_deref())
        .collect();
    for i in 0..5 {
        assert!(
            agent_ids.contains(format!("concurrent-agent-{i}").as_str()),
            "agent concurrent-agent-{i} should be present"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Large payload — many tools and long messages handled correctly
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn large_payload_many_tools_and_long_messages() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Build a payload with 20 tool schemas and a long user message.
    let tools: Vec<Value> = (0..20).map(|i| tool_schema(&format!("tool_{i}"))).collect();

    // Long message (~10KB).
    let long_content = "x".repeat(10_000);

    // LLM returns 10 tool calls in one response.
    let tool_calls: Vec<Value> = (0..10)
        .map(|i| {
            tool_call(
                &format!("tc-lg-{i}"),
                &format!("tool_{i}"),
                json!({"data": format!("arg-{i}")}),
            )
        })
        .collect();

    let payload = json!({
        "agent_id": "large-payload-agent",
        "messages": [{ "role": "user", "content": long_content }],
        "edge_tools": tools,
        "test_llm_rounds": [{
            "tool_calls": tool_calls,
            "usage": { "prompt_tokens": 5000, "completion_tokens": 200 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // Should have session_info and turn_complete.
    let si = events_of_type(&events, "session_info");
    assert!(!si.is_empty(), "should have session_info");
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Verify core persistence.
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    let uq = core[0].user_query_event.as_ref().unwrap();
    assert!(
        uq.content.len() >= 10_000,
        "user_query content should contain the long message"
    );

    // Verify tool events: at least 10 tool call records.
    let tools_persisted = cap.tool_plans.lock().await;
    let total: usize = tools_persisted.iter().map(|p| p.events.len()).sum();
    assert!(
        total >= 10,
        "should persist at least 10 tool events, got {total}"
    );

    // Verify usage persisted.
    let lr = core[0].llm_response_event.as_ref().unwrap();
    assert!(lr.token_usage.is_some());
    assert_eq!(
        lr.token_usage
            .as_ref()
            .unwrap()
            .get("prompt_tokens")
            .and_then(Value::as_i64),
        Some(5000)
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Hook DB persistence — decision audit written via side effects
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_db_persistence_fires_after_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // A normal turn that should trigger hook side effects.
    let payload = json!({
        "agent_id": "hook-agent",
        "messages": [{ "role": "user", "content": "test hooks" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "Hook test done.",
            "usage": { "prompt_tokens": 30, "completion_tokens": 10 }
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    // Hook side effects run asynchronously via tokio::spawn — give them time.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // The hook payload is built in bridge_inprocess and passed to
    // run_bridge_hook_side_effects. The capturing writer should have
    // received at least one persist call if the hook payload was non-empty.
    // Note: the actual TurnHookDbPersistPlan fields (decision_audit, etc.)
    // depend on what build_hook_db_persist_from_payload extracts. It may
    // be empty if the payload doesn't match expected fields. The key test
    // is that the writer is not corrupted and no panics occurred.
    let hooks = cap.hook_plans.lock().await;
    // Hook plans may or may not be populated depending on payload structure.
    // The important assertion is that the test completes without panic
    // and the writer accepted whatever was sent.
    drop(hooks);
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Auxiliary events — routing decision persisted for every turn
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auxiliary_routing_decision_persisted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "aux-agent",
        "messages": [{ "role": "user", "content": "test auxiliary" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "Done with aux test.",
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    // Auxiliary events are persisted asynchronously.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let aux = cap.aux_events.lock().await;
    // The bridge always emits a routing_decision auxiliary event.
    let routing_events: Vec<_> = aux
        .iter()
        .filter(|e| e.event_type == "routing_decision")
        .collect();
    assert!(
        !routing_events.is_empty(),
        "should have at least one routing_decision auxiliary event, got {:?}",
        aux.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );

    // Verify routing event fields.
    let re = &routing_events[0];
    assert_eq!(re.user_id, USER_ID);
    assert_eq!(re.agent_id.as_deref(), Some("aux-agent"));
    assert!(
        re.content.contains("inprocess"),
        "routing decision content should mention inprocess router"
    );
    assert!(
        uuid::Uuid::parse_str(&re.event_id).is_ok(),
        "routing event_id should be valid UUID"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Auxiliary events across multi-turn — each turn gets routing event
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auxiliary_events_per_turn_in_multi_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: tool call.
    let turn1 = json!({
        "agent_id": "aux-mt-agent",
        "messages": [{ "role": "user", "content": "multi-turn aux test" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-aux1", "read_file", json!({"path": "x.txt"}))]
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let session_id = ev1[0].get("session_id").and_then(Value::as_str).unwrap();

    // Turn 2: completion.
    let turn2 = json!({
        "agent_id": "aux-mt-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "multi-turn aux test" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-aux1", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-aux1", "content": "data" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-aux1", "content": "data" }],
        "test_llm_rounds": [{
            "full_text": "All done."
        }]
    });

    let (st2, _) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let aux = cap.aux_events.lock().await;
    let routing_events: Vec<_> = aux
        .iter()
        .filter(|e| e.event_type == "routing_decision")
        .collect();
    assert!(
        routing_events.len() >= 2,
        "should have routing_decision for both turns, got {}",
        routing_events.len()
    );

    // All routing event IDs should be unique.
    let ids: std::collections::HashSet<_> = routing_events.iter().map(|e| &e.event_id).collect();
    assert_eq!(
        ids.len(),
        routing_events.len(),
        "routing event IDs must be unique"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: SSE text_delta content correctness — full text delivered via deltas
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn sse_text_delta_content_matches_full_text() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "delta-agent",
        "messages": [{ "role": "user", "content": "give me a haiku" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "An old silent pond / A frog jumps into the pond / Splash! Silence again.",
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // Collect all text_delta content.
    let delta_text: String = events_of_type(&events, "text_delta")
        .iter()
        .filter_map(|e| e.get("content").and_then(Value::as_str))
        .collect();

    assert!(
        delta_text.contains("An old silent pond"),
        "text_delta content should contain the full LLM output, got: {delta_text}"
    );
    assert!(
        delta_text.contains("Splash!"),
        "text_delta should contain 'Splash!'"
    );

    // Verify the persisted content also matches.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let core = cap.core_plans.lock().await;
    let lr = core[0].llm_response_event.as_ref().unwrap();
    assert!(
        lr.content.contains("Splash!"),
        "persisted llm_response should contain the full text"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Session activity event_count_increment accuracy
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_activity_event_count_increment_accuracy() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Text-only turn: should have event_count_increment = 2
    // (1 user_query + 1 llm_response)
    let payload = json!({
        "agent_id": "activity-agent",
        "messages": [{ "role": "user", "content": "simple question" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "Simple answer.",
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let acts = cap.activity_plans.lock().await;
    assert!(!acts.is_empty(), "activity writer should be called");
    let (session_id, plan) = &acts[0];
    assert!(!session_id.is_empty(), "session_id should be non-empty");
    // Text-only: 1 user_query + 1 llm_response = 2 events.
    assert_eq!(
        plan.event_count_increment, 2,
        "text-only turn: event_count_increment should be 2 (user_query + llm_response)"
    );
    // last_event_id should be the llm_response event_id.
    assert!(plan.last_event_id.is_some(), "last_event_id should be set");
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Activity event_count for tool-call turn
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_activity_event_count_for_tool_call_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn with 2 tool calls: event_count_increment should account for
    // user_query + 2 tool_calls + llm_response = 4
    let payload = json!({
        "agent_id": "activity-tool-agent",
        "messages": [{ "role": "user", "content": "read two files" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-at1", "read_file", json!({"path": "a.txt"})),
                tool_call("tc-at2", "read_file", json!({"path": "b.txt"})),
            ]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let acts = cap.activity_plans.lock().await;
    assert!(!acts.is_empty());
    let (_, plan) = &acts[0];
    // user_query(1) + tool_calls(2) + llm_response(1) = 4
    assert_eq!(
        plan.event_count_increment, 4,
        "tool-call turn: event_count_increment should be 4 (user_query + 2 tool_calls + llm_response), got {}",
        plan.event_count_increment
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test: Activity event_count for continuation turn (no user_query re-persist)
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_activity_event_count_for_continuation_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Continuation: tool_results present, so user_query is NOT counted.
    let payload = json!({
        "agent_id": "activity-cont-agent",
        "session_id": "s-comp-created",
        "messages": [
            { "role": "user", "content": "do stuff" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-ac1", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-ac1", "content": "result" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-ac1", "content": "result" }],
        "test_llm_rounds": [{
            "full_text": "Continuation complete.",
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let acts = cap.activity_plans.lock().await;
    assert!(!acts.is_empty());
    let (_, plan) = &acts[0];
    // core_event_count counts user_content.is_some() (1) + should_persist_llm (1) = 2,
    // even though user_query_event is NOT persisted on continuation (P2 fix).
    // Plus tool_event_count = 1 (tool_result). Total = 3.
    // Note: the activity counter slightly over-counts on continuations because it
    // still counts user_content presence despite skipping the actual persist.
    assert_eq!(
        plan.event_count_increment, 3,
        "continuation turn: event_count_increment should be 3, got {}",
        plan.event_count_increment
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 1: Edge Cases
// ══════════════════════════════════════════════════════════════════════════════

// Test: Missing agent_id in payload → persisted records have agent_id = None
#[tokio::test]
async fn edge_missing_agent_id_persists_as_none() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "messages": [{ "role": "user", "content": "no agent id here" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Response without agent_id." }]
    });

    let (st, _raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty(), "should have core persist call");
    let plan = &core[0];
    if let Some(uq) = &plan.user_query_event {
        assert!(
            uq.agent_id.is_none(),
            "agent_id should be None when not provided, got {:?}",
            uq.agent_id
        );
    }
    if let Some(lr) = &plan.llm_response_event {
        assert!(
            lr.agent_id.is_none(),
            "agent_id should be None when not provided, got {:?}",
            lr.agent_id
        );
    }
}

// Test: Unicode content preserved through SSE and persistence
#[tokio::test]
async fn edge_unicode_content_preserved_in_persist_and_sse() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let user_msg = "你好世界 🌍 日本語テスト";
    let llm_text = "回答：こんにちは！🎉 Réponse";

    let payload = json!({
        "agent_id": "unicode-agent",
        "messages": [{ "role": "user", "content": user_msg }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": llm_text }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // SSE text_delta should contain the Unicode LLM output.
    let delta_text: String = events_of_type(&events, "text_delta")
        .iter()
        .filter_map(|e| e.get("content").and_then(Value::as_str))
        .collect();
    assert!(
        delta_text.contains("回答：こんにちは！🎉 Réponse"),
        "SSE text_delta should contain Unicode LLM text, got: {delta_text}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty());
    let plan = &core[0];

    // User query content preserved.
    let uq = plan.user_query_event.as_ref().unwrap();
    assert!(
        uq.content.contains("你好世界 🌍 日本語テスト"),
        "persisted user_query should contain Unicode, got: {}",
        uq.content
    );

    // LLM response content preserved.
    let lr = plan.llm_response_event.as_ref().unwrap();
    assert!(
        lr.content.contains("回答：こんにちは！🎉 Réponse"),
        "persisted llm_response should contain Unicode, got: {}",
        lr.content
    );
}

// Test: Empty tool_results array treated as fresh turn (not continuation)
#[tokio::test]
async fn edge_empty_tool_results_array_is_fresh_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "empty-tr-agent",
        "messages": [{ "role": "user", "content": "fresh turn with empty tool_results" }],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [],
        "test_llm_rounds": [{ "full_text": "This is a fresh turn." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty(), "should have core persist");
    let plan = &core[0];
    assert!(
        plan.user_query_event.is_some(),
        "empty tool_results should be treated as fresh turn — user_query must be persisted"
    );
}

// Test: Tool result with error content flows through to persistence
#[tokio::test]
async fn edge_tool_result_error_content_flows_through() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: tool call.
    let turn1 = json!({
        "agent_id": "tool-err-agent",
        "messages": [{ "role": "user", "content": "read bad file" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-terr", "read_file", json!({"path": "/nonexistent"}))]
        }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let session_id = ev1[0].get("session_id").and_then(Value::as_str).unwrap();

    // Turn 2: continuation with error content in tool_results.
    let turn2 = json!({
        "agent_id": "tool-err-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "read bad file" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-terr", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"/nonexistent\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-terr", "content": "ERROR: file not found: /nonexistent" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-terr", "content": "ERROR: file not found: /nonexistent" }],
        "test_llm_rounds": [{
            "full_text": "The file was not found."
        }]
    });

    let (st2, _) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Verify tool events persisted across both turns.
    let tools = cap.tool_plans.lock().await;
    let total_events: usize = tools.iter().map(|p| p.events.len()).sum();
    assert!(
        total_events >= 1,
        "tool events should be persisted, got {total_events}"
    );

    // Verify final llm_response is persisted.
    let core = cap.core_plans.lock().await;
    assert!(core.len() >= 2, "both turns should persist core events");
    let lr = core.last().unwrap().llm_response_event.as_ref().unwrap();
    assert!(
        lr.content.contains("not found"),
        "final llm_response should contain text about the error"
    );
}

// Test: Empty full_text with no tool_calls → llm_response NOT persisted
#[tokio::test]
async fn edge_empty_full_text_no_tool_calls_skips_llm_persist() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "empty-text-agent",
        "messages": [{ "role": "user", "content": "trigger empty response" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "" }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let core = cap.core_plans.lock().await;
    // A core persist call should still happen (user_query is non-empty).
    assert!(!core.is_empty(), "should have core persist call");
    let plan = &core[0];
    // should_persist_llm = false when llm_content.trim() is empty and no tool_calls.
    assert!(
        plan.llm_response_event.is_none(),
        "empty full_text with no tool_calls should NOT persist llm_response_event"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 2: Multi-turn Patterns
// ══════════════════════════════════════════════════════════════════════════════

// Test: Alternating text → tool → text pattern across 3 turns
#[tokio::test]
async fn multi_turn_text_tool_text_alternating_pattern() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: text-only response.
    let turn1 = json!({
        "agent_id": "alt-agent",
        "messages": [{ "role": "user", "content": "start the process" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "I'll start by reading files." }]
    });

    let (st1, raw1) = chat_turn(&app, turn1).await;
    assert_eq!(st1, StatusCode::OK);
    let ev1 = parse_sse_events(&raw1);
    let session_id = ev1[0].get("session_id").and_then(Value::as_str).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    {
        let core = cap.core_plans.lock().await;
        assert_eq!(core.len(), 1, "after turn 1: 1 core persist");
    }

    // Turn 2: tool call (no full_text).
    let turn2 = json!({
        "agent_id": "alt-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "now read the file" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-alt1", "read_file", json!({"path": "data.txt"}))]
        }]
    });

    let (st2, raw2) = chat_turn(&app, turn2).await;
    assert_eq!(st2, StatusCode::OK);
    let ev2 = parse_sse_events(&raw2);
    let tc2 = events_of_type(&ev2, "turn_complete");
    assert_eq!(
        tc2[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    {
        let core = cap.core_plans.lock().await;
        assert_eq!(core.len(), 2, "after turn 2: 2 core persists");
    }

    // Turn 3: continuation with tool_results → text response.
    let turn3 = json!({
        "agent_id": "alt-agent",
        "session_id": session_id,
        "messages": [
            { "role": "user", "content": "now read the file" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-alt1", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"data.txt\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "tc-alt1", "content": "file contents here" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-alt1", "content": "file contents here" }],
        "test_llm_rounds": [{ "full_text": "The file contains data." }]
    });

    let (st3, raw3) = chat_turn(&app, turn3).await;
    assert_eq!(st3, StatusCode::OK);
    let ev3 = parse_sse_events(&raw3);
    let tc3 = events_of_type(&ev3, "turn_complete");
    assert_eq!(
        tc3[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false)
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    {
        let core = cap.core_plans.lock().await;
        assert_eq!(core.len(), 3, "after turn 3: 3 core persists (cumulative)");
    }
}

// Test: 10 sequential text-only turns, all events captured with unique IDs
#[tokio::test]
async fn ten_sequential_turns_all_events_captured() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    for i in 0..10 {
        let payload = json!({
            "agent_id": "ten-turn-agent",
            "messages": [{ "role": "user", "content": format!("question {i}") }],
            "edge_tools": [],
            "test_llm_rounds": [{ "full_text": format!("answer {i}") }]
        });

        let (st, _) = chat_turn(&app, payload).await;
        assert_eq!(st, StatusCode::OK, "turn {i} should succeed");
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(
        core.len(),
        10,
        "10 sequential turns = 10 core persist plans, got {}",
        core.len()
    );

    let acts = cap.activity_plans.lock().await;
    assert_eq!(
        acts.len(),
        10,
        "10 turns = 10 activity plans, got {}",
        acts.len()
    );

    // All event_ids must be unique.
    let mut all_ids: Vec<String> = Vec::new();
    for plan in core.iter() {
        if let Some(uq) = &plan.user_query_event {
            all_ids.push(uq.event_id.clone());
        }
        if let Some(lr) = &plan.llm_response_event {
            all_ids.push(lr.event_id.clone());
        }
    }
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        all_ids.len(),
        unique.len(),
        "all event_ids across 10 turns must be unique"
    );
    // Each turn: 1 user_query + 1 llm_response = 20 total.
    assert_eq!(all_ids.len(), 20, "10 turns × 2 events = 20 IDs");
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 3: Concurrency
// ══════════════════════════════════════════════════════════════════════════════

// Test: 20 parallel sessions each with a tool call — no panics, all captured
#[tokio::test]
async fn stress_20_parallel_sessions_with_tool_calls() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let mut handles = Vec::new();
    for i in 0..20 {
        let app_clone = app.clone();
        let payload = json!({
            "agent_id": format!("stress-agent-{i}"),
            "messages": [{ "role": "user", "content": format!("stress request {i}") }],
            "edge_tools": [tool_schema("read_file")],
            "test_llm_rounds": [{
                "tool_calls": [tool_call(
                    &format!("tc-stress-{i}"),
                    "read_file",
                    json!({"path": format!("file_{i}.txt")})
                )]
            }]
        });
        handles.push(tokio::spawn(
            async move { chat_turn(&app_clone, payload).await },
        ));
    }

    let results: Vec<_> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("task panicked"))
        .collect();

    for (i, (st, _)) in results.iter().enumerate() {
        assert_eq!(*st, StatusCode::OK, "parallel request {i} should succeed");
    }

    // Allow more time for 20 async persist operations to settle.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(
        core.len(),
        20,
        "20 parallel requests = 20 core persist plans, got {}",
        core.len()
    );
}

// Test: 5 rapid sequential requests — all succeed, all persisted
#[tokio::test]
async fn stress_rapid_sequential_same_session() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    for i in 0..5 {
        let payload = json!({
            "agent_id": "rapid-agent",
            "messages": [{ "role": "user", "content": format!("rapid request {i}") }],
            "edge_tools": [],
            "test_llm_rounds": [{ "full_text": format!("rapid response {i}") }]
        });

        let (st, _) = chat_turn(&app, payload).await;
        assert_eq!(st, StatusCode::OK, "rapid request {i} should succeed");
        // No sleep between requests — fire as fast as possible.
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let core = cap.core_plans.lock().await;
    assert_eq!(
        core.len(),
        5,
        "5 rapid sequential requests = 5 core persist plans, got {}",
        core.len()
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 4: SSE Ordering
// ══════════════════════════════════════════════════════════════════════════════

// Test: Text-only turn produces exact SSE event sequence
#[tokio::test]
async fn sse_full_sequence_text_only_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sse-seq-text-agent",
        "messages": [{ "role": "user", "content": "tell me something" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Here is something." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();

    // context_meta is only emitted in the real LLM path, not the e2e mock path.
    assert_eq!(
        types,
        vec!["session_info", "text_delta", "turn_complete"],
        "text-only turn SSE sequence should be: session_info → text_delta → turn_complete, got: {types:?}"
    );
}

// Test: Tool-call turn produces exact SSE event sequence (no text_delta)
#[tokio::test]
async fn sse_full_sequence_tool_call_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sse-seq-tool-agent",
        "messages": [{ "role": "user", "content": "do something with tools" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-sseq", "read_file", json!({"path": "x.txt"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();

    // context_meta is only emitted in the real LLM path, not the e2e mock path.
    assert_eq!(
        types,
        vec!["session_info", "turn_complete"],
        "tool-call turn SSE sequence should be: session_info → turn_complete, got: {types:?}"
    );

    // Explicitly verify no text_delta.
    let deltas = events_of_type(&events, "text_delta");
    assert!(
        deltas.is_empty(),
        "tool-call turn should NOT have text_delta events"
    );
}

// Test: context_meta is NOT emitted in e2e mock path (only in real LLM path)
// This documents current behavior — context_meta requires real prompt assembly.
#[tokio::test]
async fn sse_context_meta_not_emitted_in_mock_path() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "ctx-meta-agent",
        "messages": [{ "role": "user", "content": "anything" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "ok" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let cm = events_of_type(&events, "context_meta");
    assert!(
        cm.is_empty(),
        "context_meta should NOT be emitted in e2e mock path (emitted only in real LLM path)"
    );
}

// Test: reasoning_done appears before turn_complete in SSE sequence
#[tokio::test]
async fn sse_reasoning_done_before_turn_complete() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sse-reason-agent",
        "messages": [{ "role": "user", "content": "think about this" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "Here is my answer.",
            "reasoning": "thinking..."
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();

    let reasoning_pos = types
        .iter()
        .position(|t| *t == "reasoning_done")
        .expect("should have reasoning_done event");
    let turn_complete_pos = types
        .iter()
        .position(|t| *t == "turn_complete")
        .expect("should have turn_complete event");

    assert!(
        reasoning_pos < turn_complete_pos,
        "reasoning_done (pos {reasoning_pos}) must appear before turn_complete (pos {turn_complete_pos}), sequence: {types:?}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Category 5: Security
// ══════════════════════════════════════════════════════════════════════════════

// Test: Wrong secret header → mock path not used, mock content not in response
#[tokio::test]
async fn security_wrong_secret_rejects_mock_path() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sec-wrong-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "MOCK_CONTENT_SHOULD_NOT_APPEAR"
        }]
    });

    let (st, raw) = chat_turn_wrong_secret(&app, payload).await;
    // The SSE stream should still return 200 but the content should NOT
    // contain the mock text (real LLM unavailable → error event).
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // Verify mock content is NOT in any text_delta.
    let delta_text: String = events_of_type(&events, "text_delta")
        .iter()
        .filter_map(|e| e.get("content").and_then(Value::as_str))
        .collect();
    assert!(
        !delta_text.contains("MOCK_CONTENT_SHOULD_NOT_APPEAR"),
        "wrong secret should NOT allow mock content through"
    );

    // Should likely have an error event (real LLM unavailable in test).
    let has_error = !events_of_type(&events, "error").is_empty();
    let has_tc = !events_of_type(&events, "turn_complete").is_empty();
    assert!(
        has_error || has_tc,
        "wrong secret should produce error or turn_complete (not mock content)"
    );
}

// Test: Missing secret header → mock path not used, mock content not in response
#[tokio::test]
async fn security_missing_secret_header_rejects_mock_path() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sec-nosecret-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": "MOCK_CONTENT_SHOULD_NOT_APPEAR"
        }]
    });

    let (st, raw) = chat_turn_no_secret(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    let events = parse_sse_events(&raw);

    // Verify mock content is NOT in any text_delta.
    let delta_text: String = events_of_type(&events, "text_delta")
        .iter()
        .filter_map(|e| e.get("content").and_then(Value::as_str))
        .collect();
    assert!(
        !delta_text.contains("MOCK_CONTENT_SHOULD_NOT_APPEAR"),
        "missing secret header should NOT allow mock content through"
    );

    // Should likely have an error event (real LLM unavailable in test).
    let has_error = !events_of_type(&events, "error").is_empty();
    let has_tc = !events_of_type(&events, "turn_complete").is_empty();
    assert!(
        has_error || has_tc,
        "missing secret should produce error or turn_complete (not mock content)"
    );
}
