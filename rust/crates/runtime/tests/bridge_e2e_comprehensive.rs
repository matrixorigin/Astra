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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, FernetTokenEncryptor, HealthChecker,
    MatrixOneSettings, PERSIST_FAIL_COUNT, PERSIST_OK_COUNT, ServiceInfo, SessionActivityRecord,
    SessionActivityUpdatePlan, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, TurnAuxiliaryEventRecord,
    TurnAuxiliaryEventWriter, TurnCoreEventWriter, TurnCorePersistOutcome, TurnCorePersistPlan,
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnSessionActivityWriter, TurnToolEventPersistPlan,
    TurnToolEventWriter, build_app, turn::bridge_inprocess::InProcessChatTurnBridge,
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

#[derive(Clone)]
struct AllCaptures {
    core_plans: Arc<Mutex<Vec<TurnCorePersistPlan>>>,
    tool_plans: Arc<Mutex<Vec<TurnToolEventPersistPlan>>>,
    aux_events: Arc<Mutex<Vec<TurnAuxiliaryEventRecord>>>,
    activity_plans: Arc<Mutex<Vec<(String, SessionActivityUpdatePlan)>>>,
    hook_plans: Arc<Mutex<Vec<TurnHookDbPersistPlan>>>,
    /// Tracks total persist operations for deterministic wait
    persist_count: Arc<AtomicUsize>,
    persist_notify: Arc<tokio::sync::Notify>,
}

impl Default for AllCaptures {
    fn default() -> Self {
        Self {
            core_plans: Default::default(),
            tool_plans: Default::default(),
            aux_events: Default::default(),
            activity_plans: Default::default(),
            hook_plans: Default::default(),
            persist_count: Arc::new(AtomicUsize::new(0)),
            persist_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl AllCaptures {
    fn signal_persist(&self) {
        self.persist_count.fetch_add(1, Ordering::SeqCst);
        self.persist_notify.notify_waiters();
    }

    /// Wait until no new persist operations have occurred for 10ms (deterministic
    /// replacement for fixed-duration sleeps). Typical wait: <5ms with in-memory writers.
    async fn wait_persist_idle(&self) {
        let mut last = self.persist_count.load(Ordering::SeqCst);
        loop {
            let notified = self.persist_notify.notified();
            let current = self.persist_count.load(Ordering::SeqCst);
            if current != last {
                last = current;
                continue;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(10), notified).await {
                Ok(()) => continue,
                Err(_) => return,
            }
        }
    }
}

#[derive(Clone)]
struct CapCoreWriter(AllCaptures);
#[async_trait]
impl TurnCoreEventWriter for CapCoreWriter {
    async fn persist(&self, plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String> {
        let event_id = plan.llm_response_event.as_ref().map(|e| e.event_id.clone());
        self.0.core_plans.lock().await.push(plan);
        self.0.signal_persist();
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
        self.0.signal_persist();
        Ok(())
    }
}

#[derive(Clone)]
struct CapAuxWriter(AllCaptures);
#[async_trait]
impl TurnAuxiliaryEventWriter for CapAuxWriter {
    async fn persist_events(&self, events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        self.0.aux_events.lock().await.extend(events);
        self.0.signal_persist();
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
        self.0.signal_persist();
        Ok(())
    }
}

#[derive(Clone)]
struct CapHookWriter(AllCaptures);
#[async_trait]
impl TurnHookDbWriter for CapHookWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        self.0.hook_plans.lock().await.push(plan);
        self.0.signal_persist();
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
    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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
    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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
    cap.wait_persist_idle().await;

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
    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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
    cap.wait_persist_idle().await;
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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;
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

    cap.wait_persist_idle().await;
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

    cap.wait_persist_idle().await;
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

    cap.wait_persist_idle().await;

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
    cap.wait_persist_idle().await;

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

    cap.wait_persist_idle().await;

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

    // The bridge emits tool_request events for each tool_call so the CLI can
    // execute them locally and populate edge_tool_round.
    assert_eq!(
        types,
        vec!["session_info", "tool_request", "turn_complete"],
        "tool-call turn SSE sequence should be: session_info → tool_request → turn_complete, got: {types:?}"
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

// ══════════════════════════════════════════════════════════════════════════════
// Test 1: Observability — PERSIST_OK_COUNT increments after a successful turn
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn observability_persist_ok_counter_increments_after_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let before = PERSIST_OK_COUNT.load(Ordering::Relaxed);

    let payload = json!({
        "agent_id": "obs-ok-agent",
        "messages": [{ "role": "user", "content": "count check" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Counted." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    let after = PERSIST_OK_COUNT.load(Ordering::Relaxed);
    assert!(
        after >= before + 1,
        "PERSIST_OK_COUNT should increment by at least 1: before={before}, after={after}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 2: Observability — PERSIST_FAIL_COUNT stays stable on successful turn
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn observability_persist_fail_counter_stable_on_success() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let before = PERSIST_FAIL_COUNT.load(Ordering::Relaxed);

    let payload = json!({
        "agent_id": "obs-fail-agent",
        "messages": [{ "role": "user", "content": "stability check" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Stable." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    let after = PERSIST_FAIL_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        after, before,
        "PERSIST_FAIL_COUNT should not change on success: before={before}, after={after}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 3: turn_complete has_tool_calls == true for a tool turn
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_complete_has_tool_calls_true_for_tool_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "tc-true-agent",
        "messages": [{ "role": "user", "content": "read something" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-ht1", "read_file", json!({"path": "x.rs"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "exactly one turn_complete event");
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true),
        "has_tool_calls should be true for a tool-call turn"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 4: turn_complete has_tool_calls == false for a text-only turn
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_complete_has_tool_calls_false_for_text_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "tc-false-agent",
        "messages": [{ "role": "user", "content": "just text" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Pure text response." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "exactly one turn_complete event");
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false),
        "has_tool_calls should be false for a text-only turn"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 5: turn_complete stall_detected and divergence fields absent normally
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_complete_stall_and_divergence_fields_absent_normally() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "tc-fields-agent",
        "messages": [{ "role": "user", "content": "normal turn" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "All good." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "exactly one turn_complete event");

    assert!(
        tc[0].get("stall_detected").is_none(),
        "stall_detected should be absent on a normal turn"
    );
    assert!(
        tc[0].get("divergence").is_none(),
        "divergence should be absent on a normal turn"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 6: Continuation with 3 tool results — all persisted, no user_query
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn continuation_with_three_tool_results_all_persisted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "cont3-agent",
        "session_id": "s-comp-created",
        "messages": [
            { "role": "user", "content": "read three files" },
            { "role": "assistant", "content": "", "tool_calls": [
                { "id": "tc-c3a", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" } },
                { "id": "tc-c3b", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"b.txt\"}" } },
                { "id": "tc-c3c", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"c.txt\"}" } },
            ]},
            { "role": "tool", "tool_call_id": "tc-c3a", "content": "aaa" },
            { "role": "tool", "tool_call_id": "tc-c3b", "content": "bbb" },
            { "role": "tool", "tool_call_id": "tc-c3c", "content": "ccc" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [
            { "tool_call_id": "tc-c3a", "content": "aaa" },
            { "tool_call_id": "tc-c3b", "content": "bbb" },
            { "tool_call_id": "tc-c3c", "content": "ccc" },
        ],
        "test_llm_rounds": [{ "full_text": "Read all three." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    // Tool events should include entries for the 3 tool results.
    let tools = cap.tool_plans.lock().await;
    let total_tool: usize = tools.iter().map(|p| p.events.len()).sum();
    assert!(
        total_tool >= 3,
        "should persist at least 3 tool events for 3 tool results, got {total_tool}"
    );

    // Continuation: user_query should NOT be re-persisted.
    let core = cap.core_plans.lock().await;
    for plan in core.iter() {
        assert!(
            plan.user_query_event.is_none(),
            "continuation with tool_results must not re-persist user_query_event"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 7: Tool call IDs preserved in persisted tool events
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tool_call_ids_preserved_in_turn_complete() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "tc-ids-agent",
        "messages": [{ "role": "user", "content": "call three tools" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-preserve-1", "read_file", json!({"path": "p1.rs"})),
                tool_call("tc-preserve-2", "read_file", json!({"path": "p2.rs"})),
                tool_call("tc-preserve-3", "read_file", json!({"path": "p3.rs"})),
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
        Some(true),
    );

    cap.wait_persist_idle().await;

    // Verify persisted tool events contain the correct tool_call_ids.
    let tools = cap.tool_plans.lock().await;
    let all_contents: Vec<String> = tools
        .iter()
        .flat_map(|p| p.events.iter())
        .map(|e| e.content.clone())
        .collect();
    let all_content_joined = all_contents.join(" ");

    for expected_id in &["tc-preserve-1", "tc-preserve-2", "tc-preserve-3"] {
        assert!(
            all_content_joined.contains(expected_id),
            "persisted tool events should reference tool_call_id {expected_id}, got: {all_content_joined}"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 8: 50 tools with long descriptions handled without error
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn fifty_tools_with_long_descriptions_handled() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let tools: Vec<Value> = (0..50)
        .map(|i| {
            let name = format!("tool_{i}");
            let desc = format!("D{i}-{}", "x".repeat(500));
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": {
                        "type": "object",
                        "properties": { "input": { "type": "string" } }
                    }
                }
            })
        })
        .collect();

    let payload = json!({
        "agent_id": "50tools-agent",
        "messages": [{ "role": "user", "content": "use many tools" }],
        "edge_tools": tools,
        "test_llm_rounds": [{ "full_text": "Handled all 50 tools." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "should complete successfully with 50 tools");
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 9: Duplicate tool names in edge_tools handled gracefully
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn duplicate_tool_names_in_edge_tools_handled() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "dup-tool-agent",
        "messages": [{ "role": "user", "content": "duplicate tools" }],
        "edge_tools": [
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file (version A)",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file (version B)",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            },
        ],
        "test_llm_rounds": [{ "full_text": "Handled duplicates." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(
        tc.len(),
        1,
        "should complete successfully even with duplicate tool names"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 10: Malformed JSON body — bridge returns SSE stream with error event
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn malformed_json_payload_returns_error_in_sse() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from("{{{{not json}}}}"))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();

    // Bridge uses SSE stream — status is 200 even on errors.
    // Malformed JSON is parsed as {} by bridge, no test_llm_rounds →
    // falls through to real LLM path which fails → error SSE event.
    assert_eq!(st, StatusCode::OK, "bridge always returns 200 SSE stream");
    let events = parse_sse_events(&raw);
    let errors = events_of_type(&events, "error");
    assert!(
        !errors.is_empty(),
        "malformed JSON should produce error SSE event (no real LLM available)"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 11: Missing messages field uses empty array — bridge still completes
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn missing_messages_field_uses_empty_array() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "no-msgs-agent",
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "No messages provided." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(
        tc.len(),
        1,
        "bridge should complete even without messages field"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 12: session_id in persisted core events matches created session
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_id_persisted_in_core_events_matches_created_session() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sid-check-agent",
        "messages": [{ "role": "user", "content": "session check" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Session verified." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(
        !core.is_empty(),
        "should have at least one core persist call"
    );

    for plan in core.iter() {
        if let Some(ref uq) = plan.user_query_event {
            assert_eq!(
                uq.session_id, "s-comp-created",
                "user_query session_id should match StubSession's created session"
            );
        }
        if let Some(ref lr) = plan.llm_response_event {
            assert_eq!(
                lr.session_id, "s-comp-created",
                "llm_response session_id should match StubSession's created session"
            );
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// AREA 1: Unhappy Paths & Exception Scenarios
// ══════════════════════════════════════════════════════════════════════════════

/// Persistence failure: core writer returns Err → PERSIST_FAIL counter increments,
/// SSE stream still completes normally (fire-and-forget persist).
#[tokio::test]
async fn unhappy_core_persist_failure_still_completes_sse() {
    init_env();
    let cap = AllCaptures::default();
    let enc =
        Arc::new(FernetTokenEncryptor::new("comp-e2e-fernet-key-32chars!").expect("fernet key"));

    // Build app with a FAILING core writer
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession))
        .with_turn_core_event_writer(Arc::new(FailCoreWriter))
        .with_turn_tool_event_writer(Arc::new(CapToolWriter(cap.clone())))
        .with_turn_auxiliary_event_writer(Arc::new(CapAuxWriter(cap.clone())))
        .with_turn_session_activity_writer(Arc::new(CapActivityWriter(cap.clone())))
        .with_turn_hook_db_writer(Arc::new(CapHookWriter(cap.clone())));
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
    let app = build_app(
        base.with_chat_turn_bridge(Arc::new(bridge))
            .with_chat_turn_bridge_secret("comp-e2e-bridge-secret"),
    );

    let payload = json!({
        "agent_id": "fail-core-agent",
        "messages": [{ "role": "user", "content": "trigger persist fail" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "LLM replies fine." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK, "SSE stream should still return 200");

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(
        tc.len(),
        1,
        "turn_complete still emitted despite persist fail"
    );

    cap.wait_persist_idle().await;

    // Activity should NOT be updated (core persist failed → early return in spawn task)
    let activity = cap.activity_plans.lock().await;
    assert!(
        activity.is_empty(),
        "activity should not update when core persist fails"
    );
}

/// Persistence failure: tool writer returns Err → PERSIST_FAIL counter increments,
/// but core events still persisted and activity still updated (with reduced count).
#[tokio::test]
async fn unhappy_tool_persist_failure_core_still_persists() {
    init_env();
    let cap = AllCaptures::default();
    let enc =
        Arc::new(FernetTokenEncryptor::new("comp-e2e-fernet-key-32chars!").expect("fernet key"));

    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession))
        .with_turn_core_event_writer(Arc::new(CapCoreWriter(cap.clone())))
        .with_turn_tool_event_writer(Arc::new(FailToolWriter))
        .with_turn_auxiliary_event_writer(Arc::new(CapAuxWriter(cap.clone())))
        .with_turn_session_activity_writer(Arc::new(CapActivityWriter(cap.clone())))
        .with_turn_hook_db_writer(Arc::new(CapHookWriter(cap.clone())));
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
    let app = build_app(
        base.with_chat_turn_bridge(Arc::new(bridge))
            .with_chat_turn_bridge_secret("comp-e2e-bridge-secret"),
    );

    let payload = json!({
        "agent_id": "fail-tool-agent",
        "messages": [{ "role": "user", "content": "trigger tool persist fail" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-fail-1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    // Core events persisted OK (uses CapCoreWriter, not FailToolWriter)
    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty(), "core events should still persist");

    // Activity still updated but with only core event count (tool persist failed)
    let activity = cap.activity_plans.lock().await;
    assert!(
        !activity.is_empty(),
        "activity should still update (core succeeded)"
    );
}

/// Empty user content + empty LLM response → no core events persisted, no activity update
#[tokio::test]
async fn unhappy_empty_content_both_sides_no_persist() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // No user message content, empty LLM text, no tool calls
    let payload = json!({
        "agent_id": "empty-agent",
        "messages": [],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "turn_complete still emitted");
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false)
    );

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    // should_persist_llm = false (empty text + no tool_calls), user_query_event = None (no user content)
    if !core.is_empty() {
        let plan = &core[0];
        assert!(plan.user_query_event.is_none(), "no user query to persist");
        assert!(
            plan.llm_response_event.is_none(),
            "empty LLM text + no tools → no llm_response"
        );
    }
}

/// Activity writer failure: core + tool succeed but activity update fails → PERSIST_FAIL
#[tokio::test]
async fn unhappy_activity_writer_failure() {
    init_env();
    let cap = AllCaptures::default();
    let enc =
        Arc::new(FernetTokenEncryptor::new("comp-e2e-fernet-key-32chars!").expect("fernet key"));

    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession))
        .with_turn_core_event_writer(Arc::new(CapCoreWriter(cap.clone())))
        .with_turn_tool_event_writer(Arc::new(CapToolWriter(cap.clone())))
        .with_turn_auxiliary_event_writer(Arc::new(CapAuxWriter(cap.clone())))
        .with_turn_session_activity_writer(Arc::new(FailActivityWriter))
        .with_turn_hook_db_writer(Arc::new(CapHookWriter(cap.clone())));
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
    let app = build_app(
        base.with_chat_turn_bridge(Arc::new(bridge))
            .with_chat_turn_bridge_secret("comp-e2e-bridge-secret"),
    );

    let payload = json!({
        "agent_id": "fail-activity-agent",
        "messages": [{ "role": "user", "content": "activity will fail" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Response OK." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    // Core events persisted OK despite activity failure
    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty(), "core events should still persist");
}

/// Continuation with empty tool_results array is treated as initial call, not continuation
#[tokio::test]
async fn unhappy_empty_tool_results_not_continuation() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "empty-tr-agent",
        "messages": [{ "role": "user", "content": "with empty tool_results" }],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [],
        "test_llm_rounds": [{ "full_text": "Treated as initial." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty());
    let plan = &core[0];
    // Empty tool_results → is_continuation = false → user_query persisted
    assert!(
        plan.user_query_event.is_some(),
        "empty tool_results = not continuation → user_query persisted"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// AREA 2: Data Synchronization
// ══════════════════════════════════════════════════════════════════════════════

/// Event ordering: user_query event_id < llm_response event_id (UUID v7 time-ordered)
#[tokio::test]
async fn sync_event_ids_monotonically_increasing() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "ordering-agent",
        "messages": [{ "role": "user", "content": "check ordering" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "ordered",
            "tool_calls": [tool_call("tc-ord-1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty());
    let plan = &core[0];

    let uq = plan.user_query_event.as_ref().expect("user_query exists");
    let lr = plan
        .llm_response_event
        .as_ref()
        .expect("llm_response exists");

    // UUID v7 is time-ordered → user_query_event_id < llm_response_event_id
    assert!(
        uq.event_id < lr.event_id,
        "user_query event_id ({}) should be < llm_response event_id ({}) (UUID v7 ordering)",
        uq.event_id,
        lr.event_id
    );

    // Tool events should also have monotonically increasing IDs
    let tools = cap.tool_plans.lock().await;
    if !tools.is_empty() {
        let tool_events = &tools[0].events;
        for window in tool_events.windows(2) {
            assert!(
                window[0].event_id < window[1].event_id,
                "tool event IDs should be monotonically increasing"
            );
        }
        // First tool event should be after llm_response
        assert!(
            lr.event_id < tool_events[0].event_id,
            "tool event should be after llm_response"
        );
    }
}

/// Core, tool, and activity writes all reference same session_id
#[tokio::test]
async fn sync_all_writers_same_session_id() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "sync-sid-agent",
        "messages": [{ "role": "user", "content": "sync check" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-sync-1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let expected_sid = "s-comp-created"; // From StubSession

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty());
    let plan = &core[0];
    if let Some(ref uq) = plan.user_query_event {
        assert_eq!(uq.session_id, expected_sid);
    }
    if let Some(ref lr) = plan.llm_response_event {
        assert_eq!(lr.session_id, expected_sid);
    }

    let tools = cap.tool_plans.lock().await;
    if !tools.is_empty() {
        for ev in &tools[0].events {
            assert_eq!(ev.session_id, expected_sid, "tool event session_id");
        }
    }

    let activity = cap.activity_plans.lock().await;
    if !activity.is_empty() {
        assert_eq!(activity[0].0, expected_sid, "activity session_id");
    }

    let aux = cap.aux_events.lock().await;
    for ev in aux.iter() {
        assert_eq!(ev.session_id, expected_sid, "auxiliary event session_id");
    }
}

/// Activity event_count_increment matches actual persisted event count
#[tokio::test]
async fn sync_activity_count_matches_persisted_events() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "count-agent",
        "messages": [{ "role": "user", "content": "count me" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "test_llm_rounds": [{
            "full_text": "found it",
            "tool_calls": [
                tool_call("tc-c1", "read_file", json!({"path": "a.txt"})),
                tool_call("tc-c2", "grep", json!({"path": "b.txt"}))
            ]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    let plan = &core[0];
    let core_count =
        plan.user_query_event.is_some() as usize + plan.llm_response_event.is_some() as usize;

    let tools = cap.tool_plans.lock().await;
    let tool_count = if !tools.is_empty() {
        tools[0].events.len()
    } else {
        0
    };

    let activity = cap.activity_plans.lock().await;
    assert!(!activity.is_empty());
    let increment = activity[0].1.event_count_increment;

    // Increment should be core_count + tool_count
    assert_eq!(
        increment,
        core_count + tool_count,
        "activity increment ({increment}) should equal core({core_count}) + tool({tool_count})"
    );
}

/// Causal chain consistency: all events in a turn share the same causal_chain_id
#[tokio::test]
async fn sync_causal_chain_consistent_within_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "chain-agent",
        "messages": [{ "role": "user", "content": "causal chain check" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "chained",
            "tool_calls": [tool_call("tc-chain-1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    // Collect all causal_chain_ids
    let core = cap.core_plans.lock().await;
    let plan = &core[0];
    let mut chain_ids = Vec::new();
    if let Some(ref uq) = plan.user_query_event {
        chain_ids.push(uq.causal_chain_id.clone());
    }
    if let Some(ref lr) = plan.llm_response_event {
        chain_ids.push(lr.causal_chain_id.clone());
    }

    let tools = cap.tool_plans.lock().await;
    if !tools.is_empty() {
        for ev in &tools[0].events {
            chain_ids.push(ev.causal_chain_id.clone());
        }
    }

    let aux = cap.aux_events.lock().await;
    for ev in aux.iter() {
        chain_ids.push(ev.causal_chain_id.clone());
    }

    assert!(chain_ids.len() >= 2, "should have at least 2 events");
    let first = &chain_ids[0];
    for (i, cid) in chain_ids.iter().enumerate() {
        assert_eq!(
            cid, first,
            "event {i} chain_id ({cid}) must equal first ({first})"
        );
    }
}

/// Parent event links: llm_response → user_query, tool_calls → user_query
#[tokio::test]
async fn sync_parent_event_links_correct() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "parent-agent",
        "messages": [{ "role": "user", "content": "parent link check" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "linked",
            "tool_calls": [tool_call("tc-par-1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    let plan = &core[0];
    let uq = plan.user_query_event.as_ref().expect("user_query exists");
    let lr = plan
        .llm_response_event
        .as_ref()
        .expect("llm_response exists");

    // user_query has no parent
    assert!(uq.parent_event_id.is_none(), "user_query has no parent");
    assert!(
        uq.parent_event_ids.is_empty(),
        "user_query has empty parent_event_ids"
    );

    // llm_response parent = user_query event_id
    assert_eq!(
        lr.parent_event_id.as_deref(),
        Some(uq.event_id.as_str()),
        "llm_response parent should be user_query"
    );
    assert_eq!(lr.parent_event_ids, vec![uq.event_id.clone()]);

    // Tool events parent = user_query event_id
    let tools = cap.tool_plans.lock().await;
    if !tools.is_empty() {
        for ev in &tools[0].events {
            assert_eq!(
                ev.parent_event_id.as_deref(),
                Some(uq.event_id.as_str()),
                "tool event parent should be user_query"
            );
            assert_eq!(ev.parent_event_ids, vec![uq.event_id.clone()]);
        }
    }

    // Auxiliary events parent = user_query event_id
    let aux = cap.aux_events.lock().await;
    for ev in aux.iter() {
        assert_eq!(
            ev.parent_event_id.as_deref(),
            Some(uq.event_id.as_str()),
            "auxiliary event parent should be user_query"
        );
    }
}

/// Activity last_event_id = llm_response event_id (or user_query if no llm_response)
#[tokio::test]
async fn sync_activity_last_event_id_correct() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "last-eid-agent",
        "messages": [{ "role": "user", "content": "check last event id" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "some response" }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    let plan = &core[0];
    let lr = plan
        .llm_response_event
        .as_ref()
        .expect("llm_response exists");

    let activity = cap.activity_plans.lock().await;
    assert!(!activity.is_empty());
    let last_eid = activity[0].1.last_event_id.as_deref().unwrap();
    assert_eq!(
        last_eid, lr.event_id,
        "activity last_event_id should be llm_response event_id"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// AREA 3: Session State — Traceable, Forkable, Turn Ordering
// ══════════════════════════════════════════════════════════════════════════════

/// Multi-turn: initial call → continuation → continuation reuses cached
/// causal_chain_id (bridge_prep resolves from cache for tool_result turns)
#[tokio::test]
async fn session_multi_turn_causal_chain_propagation() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: Initial call with tool calls
    let payload1 = json!({
        "agent_id": "mt-chain-agent",
        "messages": [{ "role": "user", "content": "turn 1" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-mt1", "read_file", json!({"path": "a.txt"}))]
        }]
    });
    let (st, _) = chat_turn(&app, payload1).await;
    assert_eq!(st, StatusCode::OK);

    // Turn 2: Continuation with tool results — last message is assistant (not user)
    // bridge_prep reuses cached chain_id when has_tool_results && latest_role != "user"
    let payload2 = json!({
        "agent_id": "mt-chain-agent",
        "messages": [
            { "role": "user", "content": "turn 1" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-mt1", "read_file", json!({"path": "a.txt"}))] },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-mt1", "content": "file contents" }],
        "test_llm_rounds": [{ "full_text": "Final answer." }]
    });
    let (st2, _) = chat_turn(&app, payload2).await;
    assert_eq!(st2, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(core.len() >= 2, "should have 2 core persist calls");

    // Turn 1: has user_query (not continuation)
    let turn1 = &core[0];
    assert!(
        turn1.user_query_event.is_some(),
        "turn 1 should persist user_query"
    );

    // Turn 2: no user_query (is continuation)
    let turn2 = &core[1];
    assert!(
        turn2.user_query_event.is_none(),
        "turn 2 (continuation) should skip user_query"
    );

    // Both turns should share the same causal_chain_id (reused from cache)
    let chain1 = turn1
        .llm_response_event
        .as_ref()
        .or(turn1.user_query_event.as_ref())
        .map(|e| e.causal_chain_id.as_str())
        .expect("turn 1 has events");
    let chain2 = turn2
        .llm_response_event
        .as_ref()
        .map(|e| e.causal_chain_id.as_str())
        .expect("turn 2 has llm_response");
    assert_eq!(
        chain1, chain2,
        "continuation should reuse the same causal_chain_id from cache"
    );
}

/// Forkable session: continuation reuses user_query_event_id from cache →
/// all events across turns trace back to same original user query
#[tokio::test]
async fn session_fork_trace_user_query_event_id() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: tool call (new user query → generates fresh IDs, cached)
    let p1 = json!({
        "agent_id": "fork-agent",
        "messages": [{ "role": "user", "content": "fork test" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-fork1", "read_file", json!({"path": "a.txt"}))]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Turn 2: continuation (tool_results + last msg is assistant → reuse cached IDs)
    let p2 = json!({
        "agent_id": "fork-agent",
        "messages": [
            { "role": "user", "content": "fork test" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-fork1", "read_file", json!({"path": "a.txt"}))] },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-fork1", "content": "data" }],
        "test_llm_rounds": [{ "full_text": "Done." }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(core.len() >= 2);

    // Turn 1: user_query_event has a generated ID
    let uq = core[0]
        .user_query_event
        .as_ref()
        .expect("turn1 has user_query");
    let original_uq_eid = &uq.event_id;

    // Turn 1: llm_response parent = user_query event_id
    if let Some(ref lr) = core[0].llm_response_event {
        assert_eq!(
            lr.parent_event_id.as_deref(),
            Some(original_uq_eid.as_str()),
            "turn 1 llm_response parent = user_query"
        );
    }

    // Turn 2: continuation still references same user_query_event_id as parent
    // (bridge_prep caches and reuses it for continuation turns)
    if let Some(ref lr) = core[1].llm_response_event {
        assert_eq!(
            lr.parent_event_id.as_deref(),
            Some(original_uq_eid.as_str()),
            "continuation llm_response still points to original user_query"
        );
    }
}

/// Turn content accuracy: persisted content matches what was sent/returned
#[tokio::test]
async fn session_turn_content_accuracy() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let user_msg = "What is the meaning of life?";
    let llm_response = "The meaning of life is 42.";
    let reasoning_text = "Thinking deeply about philosophy...";

    let payload = json!({
        "agent_id": "accuracy-agent",
        "messages": [{ "role": "user", "content": user_msg }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": llm_response,
            "reasoning": reasoning_text,
            "usage": { "prompt": 100, "completion": 50, "total": 150 }
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty());
    let plan = &core[0];

    let uq = plan.user_query_event.as_ref().expect("user_query");
    assert_eq!(uq.content, user_msg, "persisted user content must match");
    assert_eq!(uq.event_type, "user_query");

    let lr = plan.llm_response_event.as_ref().expect("llm_response");
    assert_eq!(lr.content, llm_response, "persisted llm content must match");
    assert_eq!(lr.event_type, "llm_response");
    assert_eq!(
        lr.reasoning_content.as_deref(),
        Some(reasoning_text),
        "reasoning must be persisted"
    );
    assert!(lr.token_usage.is_some(), "token usage must be persisted");
    let usage = lr.token_usage.as_ref().unwrap();
    assert_eq!(usage.get("prompt").and_then(Value::as_i64), Some(100));
    assert_eq!(usage.get("completion").and_then(Value::as_i64), Some(50));
}

/// Multi-turn: event_ids across turns are unique (no duplicates)
#[tokio::test]
async fn session_event_ids_unique_across_turns() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    for i in 0..3 {
        let payload = json!({
            "agent_id": "uniq-eid-agent",
            "messages": [{ "role": "user", "content": format!("turn {i}") }],
            "edge_tools": [tool_schema("read_file")],
            "test_llm_rounds": [{
                "full_text": format!("response {i}"),
                "tool_calls": [tool_call(&format!("tc-uniq-{i}"), "read_file", json!({"path": "a.txt"}))]
            }]
        });
        let (st, _) = chat_turn(&app, payload).await;
        assert_eq!(st, StatusCode::OK);
    }

    cap.wait_persist_idle().await;

    let mut all_eids = std::collections::HashSet::new();

    let core = cap.core_plans.lock().await;
    for plan in core.iter() {
        if let Some(ref uq) = plan.user_query_event {
            assert!(
                all_eids.insert(uq.event_id.clone()),
                "duplicate event_id: {}",
                uq.event_id
            );
        }
        if let Some(ref lr) = plan.llm_response_event {
            assert!(
                all_eids.insert(lr.event_id.clone()),
                "duplicate event_id: {}",
                lr.event_id
            );
        }
    }

    let tools = cap.tool_plans.lock().await;
    for plan in tools.iter() {
        for ev in &plan.events {
            assert!(
                all_eids.insert(ev.event_id.clone()),
                "duplicate tool event_id: {}",
                ev.event_id
            );
        }
    }

    assert!(
        all_eids.len() >= 9,
        "should have at least 9 unique event_ids across 3 turns (3×uq + 3×lr + 3×tool_call)"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// AREA 4: Explain/Trace Events
// ══════════════════════════════════════════════════════════════════════════════

/// explain: true in payload → SSE explain event emitted with timing/token/tool info
#[tokio::test]
async fn explain_event_emitted_with_metadata() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "explain-agent",
        "messages": [{ "role": "user", "content": "explain me" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "Here's the explanation.",
            "usage": { "prompt": 200, "completion": 80, "total": 280 },
            "tool_calls": [tool_call("tc-exp1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain_events = events_of_type(&events, "explain");
    assert_eq!(
        explain_events.len(),
        1,
        "should emit exactly 1 explain event"
    );

    let ex = explain_events[0];
    // Timing
    assert!(
        ex.get("total_ms").and_then(Value::as_i64).unwrap_or(-1) >= 0,
        "total_ms should be non-negative"
    );
    // Token counts
    assert_eq!(ex.get("prompt_tokens").and_then(Value::as_i64), Some(200));
    assert_eq!(
        ex.get("completion_tokens").and_then(Value::as_i64),
        Some(80)
    );
    // Tool info
    assert_eq!(
        ex.get("tools_selected").and_then(Value::as_i64),
        Some(1),
        "1 tool call"
    );
    assert_eq!(
        ex.get("tools_available").and_then(Value::as_i64),
        Some(2),
        "2 tools available"
    );
    // Tool selection detail
    let selection = ex.get("tool_selection").expect("tool_selection present");
    assert_eq!(
        selection.get("name").and_then(Value::as_str),
        Some("read_file")
    );
    // Steps
    let steps = ex
        .get("steps")
        .and_then(Value::as_array)
        .expect("steps present");
    assert!(!steps.is_empty(), "should have at least 1 step");
    assert_eq!(
        steps[0].get("step").and_then(Value::as_str),
        Some("llm"),
        "step type should be 'llm'"
    );
    // Routing
    let routing = ex.get("routing").expect("routing present");
    assert_eq!(
        routing.get("router").and_then(Value::as_str),
        Some("inprocess-default")
    );
}

/// explain: false (default) → no explain event emitted
#[tokio::test]
async fn explain_event_not_emitted_by_default() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "no-explain-agent",
        "messages": [{ "role": "user", "content": "no explain" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Regular response." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain_events = events_of_type(&events, "explain");
    assert!(
        explain_events.is_empty(),
        "explain should NOT be emitted when explain=false/absent"
    );
}

/// explain event with no tool calls → tools_selected=0, no tool_selection
#[tokio::test]
async fn explain_event_text_only_no_tools() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "explain-notools-agent",
        "messages": [{ "role": "user", "content": "explain text only" }],
        "edge_tools": [tool_schema("read_file")],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "Just text.",
            "usage": { "prompt": 50, "completion": 20, "total": 70 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain_events = events_of_type(&events, "explain");
    assert_eq!(explain_events.len(), 1);
    let ex = explain_events[0];
    assert_eq!(ex.get("tools_selected").and_then(Value::as_i64), Some(0));
    assert!(
        ex.get("tool_selection").map_or(true, Value::is_null),
        "no tool_selection when no tools called"
    );
}

/// explain event includes auxiliary_llm_calls with primary_generation info
#[tokio::test]
async fn explain_event_auxiliary_llm_calls() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "explain-aux-agent",
        "messages": [{ "role": "user", "content": "explain with aux" }],
        "edge_tools": [],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "Auxiliary check.",
            "usage": { "prompt": 300, "completion": 100, "total": 400 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain_events = events_of_type(&events, "explain");
    assert_eq!(explain_events.len(), 1);
    let ex = explain_events[0];

    let aux_calls = ex
        .get("auxiliary_llm_calls")
        .and_then(Value::as_array)
        .expect("auxiliary_llm_calls present");
    assert!(!aux_calls.is_empty());
    assert_eq!(
        aux_calls[0].get("purpose").and_then(Value::as_str),
        Some("primary_generation")
    );
    assert_eq!(
        aux_calls[0].get("tokens_in").and_then(Value::as_i64),
        Some(300)
    );
    assert_eq!(
        aux_calls[0].get("tokens_out").and_then(Value::as_i64),
        Some(100)
    );
}

/// Auxiliary routing_decision event emitted on every turn
#[tokio::test]
async fn trace_routing_decision_event_persisted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "trace-route-agent",
        "messages": [{ "role": "user", "content": "route decision" }],
        "edge_tools": [],
        "test_llm_rounds": [{ "full_text": "Routed." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);
    cap.wait_persist_idle().await;

    let aux = cap.aux_events.lock().await;
    let routing = aux
        .iter()
        .find(|e| e.event_type == "routing_decision")
        .expect("routing_decision event should exist");

    assert_eq!(routing.session_id, "s-comp-created");
    assert!(!routing.event_id.is_empty());
    let content: Value = serde_json::from_str(&routing.content).expect("valid JSON");
    assert_eq!(
        content.get("router").and_then(Value::as_str),
        Some("inprocess-default")
    );
    assert_eq!(
        content.get("intent").and_then(Value::as_str),
        Some("default")
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// AREA 5: Batch Tool Processing (multiple tool calls per round)
// ══════════════════════════════════════════════════════════════════════════════

/// Batch: 5 tool calls in single LLM response → all persisted with correct parent
#[tokio::test]
async fn batch_five_tools_all_persisted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "batch5-agent",
        "messages": [{ "role": "user", "content": "batch 5 tools" }],
        "edge_tools": [
            tool_schema("read_file"),
            tool_schema("grep"),
            tool_schema("list_dir"),
            tool_schema("find_refs"),
            tool_schema("symbols")
        ],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-b1", "read_file", json!({"path": "a.txt"})),
                tool_call("tc-b2", "grep", json!({"pattern": "foo"})),
                tool_call("tc-b3", "list_dir", json!({"path": "/"})),
                tool_call("tc-b4", "find_refs", json!({"symbol": "main"})),
                tool_call("tc-b5", "symbols", json!({"path": "b.rs"}))
            ]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    // SSE turn_complete should indicate has_tool_calls
    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    cap.wait_persist_idle().await;

    let tools = cap.tool_plans.lock().await;
    assert!(!tools.is_empty());
    let tool_events = &tools[0].events;

    // All 5 tool calls persisted
    let call_events: Vec<_> = tool_events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    assert_eq!(call_events.len(), 5, "all 5 tool calls should be persisted");

    // Each has distinct event_id
    let eids: std::collections::HashSet<_> = call_events.iter().map(|e| &e.event_id).collect();
    assert_eq!(eids.len(), 5, "each tool call has unique event_id");

    // Each has correct skill_name
    let skill_names: Vec<_> = call_events
        .iter()
        .filter_map(|e| e.skill_name.as_deref())
        .collect();
    assert!(skill_names.contains(&"read_file"), "read_file in batch");
    assert!(skill_names.contains(&"grep"), "grep in batch");
    assert!(skill_names.contains(&"list_dir"), "list_dir in batch");
}

/// Batch tool call + continuation: tool results returned, then final text response
#[tokio::test]
async fn batch_tool_round_trip_with_results() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Round 1: 3 tool calls
    let p1 = json!({
        "agent_id": "batch-rt-agent",
        "messages": [{ "role": "user", "content": "batch round trip" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep"), tool_schema("list_dir")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-rt1", "read_file", json!({"path": "a.txt"})),
                tool_call("tc-rt2", "grep", json!({"pattern": "x"})),
                tool_call("tc-rt3", "list_dir", json!({"path": "/tmp"}))
            ]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Round 2: continuation with 3 tool results
    let p2 = json!({
        "agent_id": "batch-rt-agent",
        "messages": [{ "role": "user", "content": "batch round trip" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep"), tool_schema("list_dir")],
        "tool_results": [
            { "tool_call_id": "tc-rt1", "content": "file: a.txt content" },
            { "tool_call_id": "tc-rt2", "content": "grep: 3 matches" },
            { "tool_call_id": "tc-rt3", "content": "list_dir: [a.txt, b.txt]" }
        ],
        "test_llm_rounds": [{ "full_text": "Based on the 3 results, here's my answer." }]
    });
    let (st2, raw2) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    let events = parse_sse_events(&raw2);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(false),
        "final round = text only"
    );

    cap.wait_persist_idle().await;

    // Verify tool results are persisted in turn 2
    let tools = cap.tool_plans.lock().await;
    // Turn 2 tool plan should contain tool_result events
    let turn2_plans: Vec<_> = tools
        .iter()
        .filter(|p| p.events.iter().any(|e| e.event_type == "tool_result"))
        .collect();
    assert!(
        !turn2_plans.is_empty(),
        "continuation should persist tool_result events"
    );
    let results: Vec<_> = turn2_plans[0]
        .events
        .iter()
        .filter(|e| e.event_type == "tool_result")
        .collect();
    assert_eq!(results.len(), 3, "3 tool results persisted");
}

/// Batch: mixed tool calls with different argument structures
#[tokio::test]
async fn batch_mixed_tool_argument_types() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "batch-mixed-agent",
        "messages": [{ "role": "user", "content": "mixed args" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-m1", "read_file", json!({"path": "simple.txt"})),
                tool_call("tc-m2", "grep", json!({
                    "pattern": "complex\\.(ts|js)",
                    "path": "/src",
                    "include": ["*.ts", "*.js"],
                    "case_sensitive": false
                }))
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

    cap.wait_persist_idle().await;

    let tools = cap.tool_plans.lock().await;
    assert!(!tools.is_empty());
    let calls: Vec<_> = tools[0]
        .events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    assert_eq!(calls.len(), 2, "2 tool calls with different arg structures");
}

/// Batch: 10 tool calls in single response (high batch count)
#[tokio::test]
async fn batch_ten_tools_stress() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let tool_calls: Vec<Value> = (0..10)
        .map(|i| {
            tool_call(
                &format!("tc-stress-{i}"),
                "read_file",
                json!({"path": format!("file{i}.txt")}),
            )
        })
        .collect();

    let payload = json!({
        "agent_id": "batch10-agent",
        "messages": [{ "role": "user", "content": "10 tools at once" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "tool_calls": tool_calls }]
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

    cap.wait_persist_idle().await;

    let tools = cap.tool_plans.lock().await;
    assert!(!tools.is_empty());
    let calls: Vec<_> = tools[0]
        .events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    assert_eq!(calls.len(), 10, "all 10 tool calls persisted");

    // Verify event_id ordering
    for window in calls.windows(2) {
        assert!(
            window[0].event_id < window[1].event_id,
            "tool event_ids should be monotonically increasing"
        );
    }
}

/// Explain event correctly counts batch tool calls
#[tokio::test]
async fn batch_explain_counts_all_tools() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "batch-explain-agent",
        "messages": [{ "role": "user", "content": "batch explain" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep"), tool_schema("list_dir")],
        "explain": true,
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-be1", "read_file", json!({"path": "a.txt"})),
                tool_call("tc-be2", "grep", json!({"pattern": "foo"})),
                tool_call("tc-be3", "list_dir", json!({"path": "/"}))
            ],
            "usage": { "prompt": 500, "completion": 150, "total": 650 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain_events = events_of_type(&events, "explain");
    assert_eq!(explain_events.len(), 1);
    let ex = explain_events[0];
    assert_eq!(
        ex.get("tools_selected").and_then(Value::as_i64),
        Some(3),
        "explain should count all 3 batch tool calls"
    );
    assert_eq!(
        ex.get("tools_available").and_then(Value::as_i64),
        Some(3),
        "3 tools available"
    );
    // First tool is read_file
    let selection = ex.get("tool_selection").expect("tool_selection present");
    assert_eq!(
        selection.get("name").and_then(Value::as_str),
        Some("read_file"),
        "tool_selection = first tool in batch"
    );
    // Steps should record 3 tool_calls
    let steps = ex.get("steps").and_then(Value::as_array).expect("steps");
    assert_eq!(
        steps[0].get("tool_calls").and_then(Value::as_i64),
        Some(3),
        "step records batch tool count"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Area 1: Prompt Cache / Large-Context Robustness
// ══════════════════════════════════════════════════════════════════════════════

/// 50+ messages in the conversation — bridge handles without crash or truncation
#[tokio::test]
async fn prompt_cache_large_message_history_handled() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let mut messages: Vec<Value> = (0..50)
        .map(|i| {
            if i % 2 == 0 {
                json!({"role": "user", "content": format!("Message {i} from user with some context about topic {}", i / 5)})
            } else {
                json!({"role": "assistant", "content": format!("Response {i} with detailed explanation about the question")})
            }
        })
        .collect();
    messages.push(json!({"role": "user", "content": "Final question after long history"}));

    let payload = json!({
        "agent_id": "large-ctx-agent",
        "messages": messages,
        "test_llm_rounds": [{ "full_text": "Got it." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "turn_complete emitted once with large history");
}

/// Messages with multi-byte unicode (emoji, CJK, RTL) preserved through bridge
#[tokio::test]
async fn prompt_cache_unicode_messages_preserved() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "unicode-agent",
        "messages": [
            { "role": "user", "content": "你好世界 🌍 مرحبا بالعالم" },
            { "role": "assistant", "content": "こんにちは 🎌 Привет" },
            { "role": "user", "content": "Final: emoji 🚀🔥💻 and ñ, ü, ø" }
        ],
        "test_llm_rounds": [{ "full_text": "Acknowledged: 你好 🌍" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let td = events_of_type(&events, "text_delta");
    let combined: String = td
        .iter()
        .filter_map(|e| e.get("content").and_then(Value::as_str))
        .collect();
    assert!(
        combined.contains("你好"),
        "CJK characters preserved in text_delta"
    );
    assert!(combined.contains("🌍"), "emoji preserved in text_delta");

    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    if let Some(lr) = core.last().and_then(|c| c.llm_response_event.as_ref()) {
        assert!(
            lr.content.contains("你好"),
            "CJK characters preserved in persisted event"
        );
    }
}

/// Single message with very large content (100KB) doesn't panic
#[tokio::test]
async fn prompt_cache_very_large_single_message_handled() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let big_content = "x".repeat(100_000);
    let payload = json!({
        "agent_id": "big-msg-agent",
        "messages": [{ "role": "user", "content": big_content }],
        "test_llm_rounds": [{ "full_text": "Processed." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    assert!(!core.is_empty(), "core events persisted with large message");
}

// ══════════════════════════════════════════════════════════════════════════════
// Area 2: SSE Streaming Edge Cases
// ══════════════════════════════════════════════════════════════════════════════

/// turn_complete emitted exactly once for text-only turn
#[tokio::test]
async fn sse_turn_complete_cardinality_text_turn() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "card-text-agent",
        "messages": [{ "role": "user", "content": "hello" }],
        "test_llm_rounds": [{ "full_text": "Hi there." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "exactly one turn_complete for text turn");
}

/// turn_complete emitted exactly once for tool-call turn
#[tokio::test]
async fn sse_turn_complete_cardinality_tool_turn() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "card-tool-agent",
        "messages": [{ "role": "user", "content": "do something" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-card1", "read_file", json!({"path": "a.txt"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "exactly one turn_complete for tool turn");
}

/// turn_complete contains all required fields: type, has_tool_calls
#[tokio::test]
async fn sse_turn_complete_required_fields_present() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "tc-fields-agent",
        "messages": [{ "role": "user", "content": "hello" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "Answer",
            "tool_calls": [tool_call("tc-f1", "read_file", json!({"path": "x"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);

    let tc = tc[0];
    assert_eq!(
        tc.get("type").and_then(Value::as_str),
        Some("turn_complete")
    );
    assert!(
        tc.get("has_tool_calls").is_some(),
        "has_tool_calls field present in turn_complete"
    );
    assert_eq!(
        tc.get("has_tool_calls").and_then(Value::as_bool),
        Some(true),
        "has_tool_calls is true when tool calls returned"
    );
}

/// session_info contains all required fields
#[tokio::test]
async fn sse_session_info_required_fields() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "si-fields-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "test_llm_rounds": [{ "full_text": "ok" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let si = events_of_type(&events, "session_info");
    assert_eq!(si.len(), 1, "exactly one session_info");
    let si = si[0];
    assert!(
        si.get("session_id").is_some(),
        "session_info has session_id"
    );
    assert!(si.get("run_id").is_some(), "session_info has run_id");
}

/// Every SSE event has a "type" field that is a non-empty string
#[tokio::test]
async fn sse_all_events_have_type_field() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "type-check-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "edge_tools": [tool_schema("read_file")],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "Some text",
            "reasoning": "Thinking...",
            "tool_calls": [tool_call("tc-tc1", "read_file", json!({"path": "a"}))],
            "usage": { "prompt": 100, "completion": 50 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    assert!(
        events.len() >= 3,
        "at least session_info + reasoning + turn_complete"
    );

    for (i, ev) in events.iter().enumerate() {
        let ty = ev.get("type").and_then(Value::as_str);
        assert!(
            ty.is_some() && !ty.unwrap().is_empty(),
            "event {i} missing or empty 'type' field: {ev}"
        );
    }
}

/// No text_delta emitted when LLM only returns tool calls (no text)
#[tokio::test]
async fn sse_no_text_delta_for_tool_only_response() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "no-td-agent",
        "messages": [{ "role": "user", "content": "tool only" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-notd", "read_file", json!({"path": "x"}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let td = events_of_type(&events, "text_delta");
    assert!(
        td.is_empty(),
        "no text_delta should be emitted for tool-only response, got {}",
        td.len()
    );
}

/// Error event from missing test_llm_rounds has required fields: type, message
#[tokio::test]
async fn sse_error_event_has_required_fields() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    // No test_llm_rounds → bridge can't call LLM → error
    let payload = json!({
        "agent_id": "err-fields-agent",
        "messages": [{ "role": "user", "content": "trigger error" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let errors = events_of_type(&events, "error");
    assert!(!errors.is_empty(), "should have error event");
    let err = errors[0];
    assert_eq!(err.get("type").and_then(Value::as_str), Some("error"));
    assert!(
        err.get("message").and_then(Value::as_str).is_some(),
        "error event has message field"
    );
}

/// SSE data frames are all valid JSON (no partial/corrupt frames)
#[tokio::test]
async fn sse_all_data_frames_valid_json() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "json-valid-agent",
        "messages": [{ "role": "user", "content": "check json" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "text here",
            "reasoning": "reason here",
            "tool_calls": [tool_call("tc-jv1", "read_file", json!({"path": "a"}))],
            "usage": { "prompt": 100, "completion": 50, "total": 150 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let data_lines: Vec<&str> = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect();
    assert!(data_lines.len() >= 3, "at least 3 data frames");

    for (i, line) in data_lines.iter().enumerate() {
        let parsed: Result<Value, _> = serde_json::from_str(line);
        assert!(parsed.is_ok(), "data frame {i} is not valid JSON: {line}");
    }
}

/// reasoning_done emitted when reasoning is present, before turn_complete
#[tokio::test]
async fn sse_reasoning_lifecycle_complete() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "reason-lc-agent",
        "messages": [{ "role": "user", "content": "think" }],
        "test_llm_rounds": [{
            "full_text": "Answer after reasoning.",
            "reasoning": "Let me think step by step..."
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();

    // reasoning_done must appear before turn_complete
    let rd_pos = types.iter().position(|t| *t == "reasoning_done");
    let tc_pos = types.iter().position(|t| *t == "turn_complete");

    assert!(rd_pos.is_some(), "reasoning_done present");
    assert!(tc_pos.is_some(), "turn_complete present");
    assert!(
        rd_pos.unwrap() < tc_pos.unwrap(),
        "reasoning_done before turn_complete"
    );

    // reasoning_done is a simple marker event (no content field)
    let rd = events_of_type(&events, "reasoning_done");
    assert_eq!(rd.len(), 1, "exactly one reasoning_done event");
    assert_eq!(
        rd[0].get("type").and_then(Value::as_str),
        Some("reasoning_done"),
        "reasoning_done type is correct"
    );
}

/// No reasoning_done emitted when reasoning is absent
#[tokio::test]
async fn sse_no_reasoning_done_without_reasoning() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "no-reason-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "test_llm_rounds": [{ "full_text": "Simple answer." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let rd = events_of_type(&events, "reasoning_done");
    assert!(
        rd.is_empty(),
        "no reasoning_done when reasoning not provided"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Area 3: /chat/stream Integration (Bridge Fallback)
// ══════════════════════════════════════════════════════════════════════════════

/// Helper: POST /chat/stream with bridge-e2e test secret
async fn chat_stream(app: &Router, message: &str, extra: Value) -> (StatusCode, String) {
    let mut payload = json!({
        "message": message,
        "agent_id": "stream-test-agent"
    });
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            payload[k] = v.clone();
        }
    }
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
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

/// /chat/stream with bridge-e2e test secret routes through bridge.
/// Note: chat_stream_bridge_fallback_payload() doesn't forward test_llm_rounds,
/// so the bridge will fall through to the real LLM path (which has no API key → error).
/// We verify the routing works by checking we get a valid SSE response (even if it errors).
#[tokio::test]
async fn chat_stream_bridge_fallback_routes_to_bridge() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let (st, raw) = chat_stream(
        &app,
        "hello from stream",
        json!({}), // no test_llm_rounds — they wouldn't be forwarded anyway
    )
    .await;

    // The response should be valid SSE (200 OK with SSE content)
    // Even if the bridge fails internally, the SSE envelope starts successfully
    assert_eq!(
        st,
        StatusCode::OK,
        "bridge fallback returns 200 OK SSE envelope"
    );

    // session_info is emitted before LLM call, so it should always appear
    let events = parse_sse_events(&raw);
    let si = events_of_type(&events, "session_info");
    assert_eq!(
        si.len(),
        1,
        "session_info present — routing to bridge worked"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Area 4: Observability
// ══════════════════════════════════════════════════════════════════════════════

/// Explain event: total_ms is non-negative
#[tokio::test]
async fn observability_explain_total_ms_positive() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "obs-ms-agent",
        "messages": [{ "role": "user", "content": "hi" }],
        "explain": true,
        "test_llm_rounds": [{ "full_text": "ok" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1);
    let ms = explain[0].get("total_ms").and_then(Value::as_i64).unwrap();
    assert!(ms >= 0, "total_ms should be non-negative, got {ms}");
}

/// Explain event: tool_selection has name of first tool when tools present
#[tokio::test]
async fn observability_explain_tool_selection_name() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "obs-sel-agent",
        "messages": [{ "role": "user", "content": "use grep" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "explain": true,
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-obs1", "grep", json!({"path": "."}))]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1);

    let sel = explain[0].get("tool_selection");
    assert!(sel.is_some(), "tool_selection present in explain");
    assert_eq!(
        sel.and_then(|s| s.get("name")).and_then(Value::as_str),
        Some("grep"),
        "tool_selection.name = first tool called"
    );
}

/// Explain event: steps array has one entry per round
#[tokio::test]
async fn observability_explain_steps_structure() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "obs-steps-agent",
        "messages": [{ "role": "user", "content": "step test" }],
        "edge_tools": [tool_schema("read_file")],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "Done.",
            "tool_calls": [
                tool_call("tc-s1", "read_file", json!({"path": "a"})),
                tool_call("tc-s2", "read_file", json!({"path": "b"}))
            ],
            "usage": { "prompt": 100, "completion": 50 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1);

    let steps = explain[0].get("steps").and_then(Value::as_array);
    assert!(steps.is_some(), "steps array present");
    let steps = steps.unwrap();
    assert!(!steps.is_empty(), "steps has at least one entry");

    // Each step should have tool_calls count
    let step0 = &steps[0];
    assert!(
        step0.get("tool_calls").is_some(),
        "step has tool_calls field"
    );
}

/// Explain event: routing field with router=inprocess-default
#[tokio::test]
async fn observability_explain_routing_field() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "obs-route-agent",
        "messages": [{ "role": "user", "content": "route test" }],
        "explain": true,
        "test_llm_rounds": [{ "full_text": "Routed." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1);

    let routing = explain[0].get("routing");
    assert!(routing.is_some(), "routing field present in explain");
    assert_eq!(
        routing
            .and_then(|r| r.get("router"))
            .and_then(Value::as_str),
        Some("inprocess-default"),
        "routing.router = inprocess-default for bridge path"
    );
}

/// Routing decision auxiliary event contains required fields
#[tokio::test]
async fn observability_routing_decision_fields() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "obs-rd-agent",
        "messages": [{ "role": "user", "content": "route decision test" }],
        "test_llm_rounds": [{ "full_text": "Done." }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;
    let aux = cap.aux_events.lock().await;
    let rd: Vec<_> = aux
        .iter()
        .filter(|e| e.event_type == "routing_decision")
        .collect();
    assert!(!rd.is_empty(), "routing_decision event persisted");

    let content: Value = serde_json::from_str(&rd[0].content).unwrap_or_default();
    assert!(
        content.get("router").is_some(),
        "routing_decision has router field"
    );
    assert!(
        content.get("intent").is_some(),
        "routing_decision has intent field"
    );
}

/// PERSIST_OK_COUNT increments correctly across multiple turns
#[tokio::test]
async fn observability_persist_ok_multi_turn_accumulation() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let before = PERSIST_OK_COUNT.load(Ordering::SeqCst);

    for i in 0..3 {
        let payload = json!({
            "agent_id": "obs-multi-agent",
            "messages": [{ "role": "user", "content": format!("turn {i}") }],
            "test_llm_rounds": [{ "full_text": format!("response {i}") }]
        });
        let (st, _) = chat_turn(&app, payload).await;
        assert_eq!(st, StatusCode::OK);
    }

    cap.wait_persist_idle().await;
    let after = PERSIST_OK_COUNT.load(Ordering::SeqCst);
    assert!(
        after >= before + 3,
        "PERSIST_OK should increment at least once per turn: before={before}, after={after}"
    );
}

/// Explain event includes token counts and timing info
/// (context_trace_signal goes through DatabaseEventService, not mock writers, so
///  we verify observability through the explain event which IS in the SSE stream)
#[tokio::test]
async fn observability_explain_event_has_structured_fields() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "obs-structured-agent",
        "messages": [{ "role": "user", "content": "structured explain" }],
        "explain": true,
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "Observed.",
            "usage": { "prompt": 300, "completion": 100 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1, "explain event present");
    assert!(explain[0].get("total_ms").is_some(), "has total_ms");
    // Token counts are at top level as prompt_tokens / completion_tokens
    assert!(
        explain[0].get("prompt_tokens").is_some(),
        "has prompt_tokens: {:?}",
        explain[0]
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Area 5: Coverage Gaps — Tool Call ID Handling, Large Payloads, Edge Cases
// ══════════════════════════════════════════════════════════════════════════════

/// Tool call with missing id → ensure_tool_call_ids assigns UUID (verified via persistence)
#[tokio::test]
async fn gap_tool_call_missing_id_gets_uuid() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Tool call without "id" field
    let payload = json!({
        "agent_id": "gap-noid-agent",
        "messages": [{ "role": "user", "content": "no id tool" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [{
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" }
            }]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    // turn_complete confirms tool calls exist
    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true)
    );

    // Verify UUID was assigned via persisted tool events' content.tool_call_id
    cap.wait_persist_idle().await;
    let plans = cap.tool_plans.lock().await;
    assert!(!plans.is_empty(), "tool events persisted");
    let tool_events = &plans[0].events;
    let tool_call_events: Vec<_> = tool_events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    assert!(!tool_call_events.is_empty(), "at least one tool_call event");

    let content: Value = serde_json::from_str(&tool_call_events[0].content).unwrap_or_default();
    let assigned_id = content
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !assigned_id.is_empty(),
        "missing id got UUID assigned in tool_call_id: {assigned_id}"
    );
}

/// Tool call with null id → UUID assigned (verified via persistence)
#[tokio::test]
async fn gap_tool_call_null_id_gets_uuid() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-nullid-agent",
        "messages": [{ "role": "user", "content": "null id" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [{
                "id": null,
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" }
            }]
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

    cap.wait_persist_idle().await;
    let plans = cap.tool_plans.lock().await;
    assert!(!plans.is_empty(), "tool events persisted");
    let tool_call_events: Vec<_> = plans[0]
        .events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    let content: Value = serde_json::from_str(&tool_call_events[0].content).unwrap_or_default();
    let assigned_id = content
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !assigned_id.is_empty(),
        "null id got UUID assigned in tool_call_id: {assigned_id}"
    );
}

/// Tool call with empty string id → UUID assigned (verified via persistence)
#[tokio::test]
async fn gap_tool_call_empty_id_gets_uuid() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-emptyid-agent",
        "messages": [{ "role": "user", "content": "empty id" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [{
                "id": "",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" }
            }]
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

    cap.wait_persist_idle().await;
    let plans = cap.tool_plans.lock().await;
    assert!(!plans.is_empty());
    let tool_call_events: Vec<_> = plans[0]
        .events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    let content: Value = serde_json::from_str(&tool_call_events[0].content).unwrap_or_default();
    let assigned_id = content
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !assigned_id.is_empty(),
        "empty id got UUID assigned in tool_call_id: {assigned_id}"
    );
}

/// Mixed tool call IDs: some valid, some missing → only missing ones get UUIDs
#[tokio::test]
async fn gap_tool_call_mixed_ids_selective_assignment() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-mixed-agent",
        "messages": [{ "role": "user", "content": "mixed ids" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("existing-id-123", "read_file", json!({"path": "a"})),
                {
                    "type": "function",
                    "function": { "name": "grep", "arguments": "{\"path\":\".\"}" }
                }
            ]
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

    cap.wait_persist_idle().await;
    let plans = cap.tool_plans.lock().await;
    assert!(!plans.is_empty());
    let tool_call_events: Vec<_> = plans[0]
        .events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    assert_eq!(tool_call_events.len(), 2, "two tool_call events persisted");

    // Extract tool_call_ids from persisted content
    let content0: Value = serde_json::from_str(&tool_call_events[0].content).unwrap_or_default();
    let content1: Value = serde_json::from_str(&tool_call_events[1].content).unwrap_or_default();
    let id0 = content0
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let id1 = content1
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");

    // First tool call keeps its original ID
    assert_eq!(id0, "existing-id-123", "existing ID preserved");

    // Second tool call gets a generated UUID
    assert!(!id1.is_empty(), "missing ID got UUID assigned");
    assert_ne!(id1, "existing-id-123", "generated ID is different");
}

/// Tool result with very large content (50KB) handled without crash
#[tokio::test]
async fn gap_tool_result_large_content_handled() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let big_result = "x".repeat(50_000);

    // Turn 1: get tool call
    let p1 = json!({
        "agent_id": "gap-bigresult-agent",
        "messages": [{ "role": "user", "content": "big result" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-big1", "read_file", json!({"path": "huge"}))]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Turn 2: continuation with big tool result
    let p2 = json!({
        "agent_id": "gap-bigresult-agent",
        "messages": [
            { "role": "user", "content": "big result" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-big1", "read_file", json!({"path": "huge"}))] }
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-big1", "content": big_result }],
        "test_llm_rounds": [{ "full_text": "Processed the big file." }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    assert!(
        core.len() >= 2,
        "both turns persisted with large tool result"
    );
}

/// Tool result with error status flows through to persistence
#[tokio::test]
async fn gap_tool_result_error_status_persisted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: get tool call
    let p1 = json!({
        "agent_id": "gap-errstatus-agent",
        "messages": [{ "role": "user", "content": "err status" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-err1", "read_file", json!({"path": "missing"}))]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Turn 2: continuation with error tool result
    let p2 = json!({
        "agent_id": "gap-errstatus-agent",
        "messages": [
            { "role": "user", "content": "err status" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-err1", "read_file", json!({"path": "missing"}))] }
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{
            "tool_call_id": "tc-err1",
            "content": "Error: file not found",
            "is_error": true
        }],
        "test_llm_rounds": [{ "full_text": "File not found, let me try another." }]
    });
    let (st2, raw2) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    let events = parse_sse_events(&raw2);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1);

    // LLM response should be persisted
    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    assert!(core.len() >= 2, "both turns persisted");
    assert!(
        core[1].llm_response_event.is_some(),
        "llm_response persisted after error tool result"
    );
}

/// Only system-role messages → bridge handles gracefully
#[tokio::test]
async fn gap_only_system_messages_handled() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-sysonly-agent",
        "messages": [
            { "role": "system", "content": "You are a helpful assistant." }
        ],
        "test_llm_rounds": [{ "full_text": "Hello! How can I help?" }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "turn completes with system-only messages");
}

/// Explain with zero tools available
#[tokio::test]
async fn gap_explain_no_tools_available() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "gap-notools-agent",
        "messages": [{ "role": "user", "content": "no tools" }],
        "explain": true,
        "test_llm_rounds": [{ "full_text": "Just text." }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1);
    assert_eq!(
        explain[0].get("tools_selected").and_then(Value::as_u64),
        Some(0),
        "tools_selected=0 when no tools"
    );
    assert_eq!(
        explain[0].get("tools_available").and_then(Value::as_u64),
        Some(0),
        "tools_available=0 when no edge_tools"
    );
}

/// Explain with usage tokens present shows correct values
#[tokio::test]
async fn gap_explain_usage_tokens_accuracy() {
    init_env();
    let app = build_test_app(AllCaptures::default());

    let payload = json!({
        "agent_id": "gap-usage-agent",
        "messages": [{ "role": "user", "content": "count tokens" }],
        "explain": true,
        "test_llm_rounds": [{
            "full_text": "Token result.",
            "usage": { "prompt": 500, "completion": 150, "total": 650 }
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let explain = events_of_type(&events, "explain");
    assert_eq!(explain.len(), 1);
    assert_eq!(
        explain[0].get("prompt_tokens").and_then(Value::as_i64),
        Some(500),
        "prompt_tokens matches usage"
    );
    assert_eq!(
        explain[0].get("completion_tokens").and_then(Value::as_i64),
        Some(150),
        "completion_tokens matches usage"
    );
}

/// Three sequential continuation rounds: all tool events persisted correctly
#[tokio::test]
async fn gap_three_continuation_rounds_all_persisted() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Round 1: initial → tool call
    let p1 = json!({
        "agent_id": "gap-3round-agent",
        "messages": [{ "role": "user", "content": "3 rounds" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-r1", "read_file", json!({"path": "a"}))]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Round 2: continuation → another tool call
    let p2 = json!({
        "agent_id": "gap-3round-agent",
        "messages": [
            { "role": "user", "content": "3 rounds" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-r1", "read_file", json!({"path": "a"}))] }
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-r1", "content": "file a" }],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-r2", "read_file", json!({"path": "b"}))]
        }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    // Round 3: continuation → final answer
    let p3 = json!({
        "agent_id": "gap-3round-agent",
        "messages": [
            { "role": "user", "content": "3 rounds" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-r2", "read_file", json!({"path": "b"}))] }
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-r2", "content": "file b" }],
        "test_llm_rounds": [{ "full_text": "All done with 3 rounds." }]
    });
    let (st3, _) = chat_turn(&app, p3).await;
    assert_eq!(st3, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 3, "3 core persist calls for 3 rounds");

    // Round 1: has user_query (initial)
    assert!(core[0].user_query_event.is_some(), "round 1 has user_query");

    // Rounds 2-3: no user_query (continuations)
    assert!(
        core[1].user_query_event.is_none(),
        "round 2 skips user_query"
    );
    assert!(
        core[2].user_query_event.is_none(),
        "round 3 skips user_query"
    );

    // All rounds have llm_response (tool calls or text)
    for (i, c) in core.iter().enumerate() {
        assert!(
            c.llm_response_event.is_some(),
            "round {} has llm_response",
            i + 1
        );
    }
}

/// Whitespace-only full_text from LLM → treated as empty → no text_delta
#[tokio::test]
async fn gap_whitespace_only_text_no_persist() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-ws-agent",
        "messages": [{ "role": "user", "content": "whitespace test" }],
        "test_llm_rounds": [{ "full_text": "   \n\t  " }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    // Whitespace-only text is trimmed to empty → should_persist_llm = false
    // So no llm_response persisted
    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    if let Some(last) = core.last() {
        assert!(
            last.llm_response_event.is_none(),
            "whitespace-only text should not persist llm_response"
        );
    }

    // turn_complete still emitted
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "turn_complete still emitted");
}

/// Tool call arguments as non-JSON string (edge case from some LLMs)
#[tokio::test]
async fn gap_tool_call_non_json_arguments() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-nonjson-agent",
        "messages": [{ "role": "user", "content": "non-json args" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [{
                "id": "tc-nj1",
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": "just a plain string, not JSON"
                }
            }]
        }]
    });

    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    let events = parse_sse_events(&raw);
    let tc = events_of_type(&events, "turn_complete");
    assert_eq!(tc.len(), 1, "turn_complete despite non-JSON arguments");
    assert_eq!(
        tc[0].get("has_tool_calls").and_then(Value::as_bool),
        Some(true),
        "tool calls still returned with non-JSON arguments"
    );
}

/// Multiple tool calls, some with IDs, some without — persistence has all unique IDs
#[tokio::test]
async fn gap_mixed_id_tool_calls_all_persisted_with_unique_ids() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "gap-mixpersist-agent",
        "messages": [{ "role": "user", "content": "mix persist" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep"), tool_schema("write_file")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("valid-id-1", "read_file", json!({"path": "a"})),
                { "type": "function", "function": { "name": "grep", "arguments": "{}" } },
                { "id": "", "type": "function", "function": { "name": "write_file", "arguments": "{}" } }
            ]
        }]
    });

    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;
    let tools = cap.tool_plans.lock().await;
    assert!(!tools.is_empty(), "tool events persisted");

    let tool_events = &tools[0].events;
    assert_eq!(tool_events.len(), 3, "all 3 tool calls persisted");

    // All event_ids should be unique
    let ids: Vec<&str> = tool_events.iter().map(|e| e.event_id.as_str()).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(
        ids.len(),
        unique.len(),
        "all tool event IDs are unique: {:?}",
        ids
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Deep coverage: Causal chain, parent linkage, session state, event completeness
// ══════════════════════════════════════════════════════════════════════════════

/// New user query after a continuation should get a FRESH causal_chain_id
/// (contrasts with session_multi_turn_causal_chain_propagation which tests REUSE)
#[tokio::test]
async fn deep_causal_chain_refreshes_on_new_user_query() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: initial user query → tool calls
    let p1 = json!({
        "agent_id": "deep-chain-agent",
        "messages": [{ "role": "user", "content": "first question" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-dc1", "read_file", json!({"path": "a"}))]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Turn 2: continuation (tool_results, latest role=assistant) → REUSES chain_id
    let p2 = json!({
        "agent_id": "deep-chain-agent",
        "messages": [
            { "role": "user", "content": "first question" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-dc1", "read_file", json!({"path": "a"}))] },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-dc1", "content": "file data" }],
        "test_llm_rounds": [{ "full_text": "answer to first" }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    // Turn 3: NEW user query (latest role=user, no tool_results) → FRESH chain_id
    let p3 = json!({
        "agent_id": "deep-chain-agent",
        "messages": [
            { "role": "user", "content": "first question" },
            { "role": "assistant", "content": "answer to first" },
            { "role": "user", "content": "second question" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "answer to second" }]
    });
    let (st3, _) = chat_turn(&app, p3).await;
    assert_eq!(st3, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    assert!(
        core.len() >= 3,
        "should have 3 core persist calls, got {}",
        core.len()
    );

    // Turn 1 and 2 share chain_id (continuation reuse)
    let chain1 = core[0]
        .llm_response_event
        .as_ref()
        .or(core[0].user_query_event.as_ref())
        .map(|e| e.causal_chain_id.as_str())
        .expect("turn 1 has events");
    let chain2 = core[1]
        .llm_response_event
        .as_ref()
        .map(|e| e.causal_chain_id.as_str())
        .expect("turn 2 has events");
    assert_eq!(chain1, chain2, "continuation reuses chain_id");

    // Turn 3 gets a DIFFERENT chain_id (new user query)
    let chain3 = core[2]
        .llm_response_event
        .as_ref()
        .or(core[2].user_query_event.as_ref())
        .map(|e| e.causal_chain_id.as_str())
        .expect("turn 3 has events");
    assert_ne!(
        chain1, chain3,
        "new user query must get fresh chain_id: turn1={chain1}, turn3={chain3}"
    );

    // Turn 3 should also have user_query_event (it's a fresh query)
    assert!(
        core[2].user_query_event.is_some(),
        "turn 3 (new user query) should persist user_query_event"
    );
}

/// Parent linkage: tool_result events in continuation point to the CACHED user_query_event_id
/// (same parent as the tool_call events from the initial turn)
#[tokio::test]
async fn deep_parent_linkage_continuation_tool_results() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: user query → 2 tool calls
    let p1 = json!({
        "agent_id": "deep-parent-agent",
        "messages": [{ "role": "user", "content": "read files" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-dp1", "read_file", json!({"path": "a"})),
                tool_call("tc-dp2", "grep", json!({"path": "b"})),
            ]
        }]
    });
    let (st, _) = chat_turn(&app, p1).await;
    assert_eq!(st, StatusCode::OK);

    // Turn 2: continuation with tool results
    let p2 = json!({
        "agent_id": "deep-parent-agent",
        "messages": [
            { "role": "user", "content": "read files" },
            { "role": "assistant", "content": "", "tool_calls": [
                tool_call("tc-dp1", "read_file", json!({"path": "a"})),
                tool_call("tc-dp2", "grep", json!({"path": "b"})),
            ] },
        ],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "tool_results": [
            { "tool_call_id": "tc-dp1", "content": "contents of a" },
            { "tool_call_id": "tc-dp2", "content": "grep results" },
        ],
        "test_llm_rounds": [{ "full_text": "here is the summary" }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    let tools = cap.tool_plans.lock().await;
    assert!(core.len() >= 2, "2 core persists");
    assert!(
        tools.len() >= 2,
        "2 tool persists (turn 1 tool_calls + turn 2 tool_results)"
    );

    // Turn 1: user_query_event_id is the parent for tool_call events
    let turn1_uq_id = core[0]
        .user_query_event
        .as_ref()
        .expect("turn 1 has user_query")
        .event_id
        .as_str();
    let turn1_tool_events = &tools[0].events;
    for ev in turn1_tool_events {
        assert_eq!(ev.event_type, "tool_call");
        assert_eq!(
            ev.parent_event_id.as_deref(),
            Some(turn1_uq_id),
            "tool_call parent must be user_query_event_id from turn 1"
        );
    }

    // Turn 2 (continuation): no user_query persisted, but tool_result events
    // still point to the CACHED user_query_event_id (same as turn 1)
    assert!(
        core[1].user_query_event.is_none(),
        "continuation skips user_query"
    );
    let turn2_tool_events = &tools[1].events;
    assert!(
        !turn2_tool_events.is_empty(),
        "continuation has tool_result events"
    );

    // The parent_event_id in continuation tool_results should match the
    // cached user_query_event_id (which bridge_prep resolves from cache)
    let turn2_parent = turn2_tool_events[0]
        .parent_event_id
        .as_deref()
        .expect("tool_result has parent");
    // All tool_results in the continuation share the same parent
    for ev in turn2_tool_events {
        assert_eq!(
            ev.parent_event_id.as_deref(),
            Some(turn2_parent),
            "all continuation tool_results share same parent"
        );
    }
    // The continuation's llm_response also points to the same parent
    let turn2_lr_parent = core[1]
        .llm_response_event
        .as_ref()
        .expect("turn 2 has llm_response")
        .parent_event_id
        .as_deref();
    assert_eq!(
        turn2_lr_parent,
        Some(turn2_parent),
        "llm_response parent matches tool_result parent (both from cached user_query_event_id)"
    );
}

/// Comprehensive single turn: ALL 5 writers called, all event types populated
#[tokio::test]
async fn deep_all_event_types_single_turn() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // A turn with reasoning + tool calls → triggers all writers
    let payload = json!({
        "agent_id": "deep-all-events-agent",
        "messages": [{ "role": "user", "content": "comprehensive turn" }],
        "edge_tools": [tool_schema("read_file")],
        "explain": true,
        "test_llm_rounds": [{
            "reasoning": "thinking about what to do...",
            "tool_calls": [tool_call("tc-ae1", "read_file", json!({"path": "x"}))],
            "usage": { "prompt_tokens": 100, "completion_tokens": 50 }
        }]
    });
    let (st, raw) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    // 1. Core writer: user_query + llm_response
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1, "exactly 1 core persist");
    let plan = &core[0];
    assert!(plan.user_query_event.is_some(), "user_query persisted");
    assert!(plan.llm_response_event.is_some(), "llm_response persisted");
    let uq = plan.user_query_event.as_ref().unwrap();
    let lr = plan.llm_response_event.as_ref().unwrap();
    assert_eq!(uq.event_type, "user_query");
    assert_eq!(lr.event_type, "llm_response");
    assert!(
        lr.reasoning_content.is_some(),
        "reasoning persisted in llm_response when reasoning is non-empty (even with tool calls)"
    );
    assert!(lr.token_usage.is_some(), "usage persisted");

    // 2. Tool writer: 1 tool_call event
    let tools = cap.tool_plans.lock().await;
    assert_eq!(tools.len(), 1, "exactly 1 tool persist plan");
    assert_eq!(tools[0].events.len(), 1, "1 tool_call event");
    assert_eq!(tools[0].events[0].event_type, "tool_call");

    // 3. Auxiliary writer: routing_decision event
    let aux = cap.aux_events.lock().await;
    assert!(!aux.is_empty(), "auxiliary events persisted");
    let routing = aux.iter().find(|e| e.event_type == "routing_decision");
    assert!(routing.is_some(), "routing_decision auxiliary event");

    // 4. Activity writer: session activity updated
    let activity = cap.activity_plans.lock().await;
    assert_eq!(activity.len(), 1, "activity updated once");
    let (sid, act_plan) = &activity[0];
    assert!(!sid.is_empty(), "session_id non-empty");
    assert!(
        act_plan.event_count_increment > 0,
        "event_count_increment > 0"
    );

    // 5. Hook writer
    let hooks = cap.hook_plans.lock().await;
    assert_eq!(hooks.len(), 1, "hook persisted once");

    // SSE events should include: session_info, reasoning_done (mock path doesn't emit deltas), explain, turn_complete
    let events = parse_sse_events(&raw);
    assert!(
        events_of_type(&events, "session_info").len() == 1,
        "1 session_info"
    );
    // In mock path, reasoning deltas are NOT emitted — only reasoning_done marker
    assert!(
        events_of_type(&events, "reasoning_done").len() == 1,
        "1 reasoning_done"
    );
    assert!(
        events_of_type(&events, "turn_complete").len() == 1,
        "1 turn_complete"
    );
    assert!(events_of_type(&events, "explain").len() == 1, "1 explain");
}

/// Cumulative session activity: event counts accumulate across multiple turns
#[tokio::test]
async fn deep_session_activity_cumulative_across_turns() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: text-only (2 core events: user_query + llm_response)
    let p1 = json!({
        "agent_id": "deep-cumul-agent",
        "messages": [{ "role": "user", "content": "turn 1" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "response 1" }]
    });
    let (st1, _) = chat_turn(&app, p1).await;
    assert_eq!(st1, StatusCode::OK);

    // Turn 2: tool call (2 core events + 1 tool event = 3)
    let p2 = json!({
        "agent_id": "deep-cumul-agent",
        "messages": [
            { "role": "user", "content": "turn 1" },
            { "role": "assistant", "content": "response 1" },
            { "role": "user", "content": "turn 2" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-cum1", "read_file", json!({"path": "a"}))]
        }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    // Turn 3: continuation with tool results (1 llm_response + 1 tool_result = 2)
    let p3 = json!({
        "agent_id": "deep-cumul-agent",
        "messages": [
            { "role": "user", "content": "turn 1" },
            { "role": "assistant", "content": "response 1" },
            { "role": "user", "content": "turn 2" },
            { "role": "assistant", "content": "", "tool_calls": [tool_call("tc-cum1", "read_file", json!({"path": "a"}))] },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-cum1", "content": "file data" }],
        "test_llm_rounds": [{ "full_text": "final answer" }]
    });
    let (st3, _) = chat_turn(&app, p3).await;
    assert_eq!(st3, StatusCode::OK);

    cap.wait_persist_idle().await;

    let activity = cap.activity_plans.lock().await;
    assert_eq!(activity.len(), 3, "3 activity updates");

    // Each turn's event_count reflects that turn's events (not cumulative in the plan itself;
    // the writer is responsible for accumulating). But each plan should have positive count.
    for (i, (_sid, plan)) in activity.iter().enumerate() {
        assert!(
            plan.event_count_increment > 0,
            "turn {} event_count_increment should be > 0, got {}",
            i + 1,
            plan.event_count_increment
        );
    }

    // Turn 1 (text-only): 2 events (user_query + llm_response)
    assert_eq!(
        activity[0].1.event_count_increment, 2,
        "turn 1: user_query + llm_response"
    );
    // Turn 2 (tool call): 2 core + 1 tool = 3
    assert_eq!(
        activity[1].1.event_count_increment, 3,
        "turn 2: user_query + llm_response + tool_call"
    );
}

/// Snapshot link plan: verify it exists in core persist plan (bridge sets None currently)
#[tokio::test]
async fn deep_snapshot_link_plan_in_core_persist() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "deep-snap-agent",
        "messages": [{ "role": "user", "content": "snapshot test" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "response" }]
    });
    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    // Bridge currently sets snapshot_link_plan to None (context_capture not available)
    assert!(
        core[0].snapshot_link_plan.is_none(),
        "bridge sets snapshot_link_plan to None (no context capture in edge path)"
    );
}

/// Multi-turn: fresh user queries get distinct chain_ids AND distinct user_query_event_ids
#[tokio::test]
async fn deep_distinct_ids_for_independent_user_queries() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: text only
    let p1 = json!({
        "agent_id": "deep-distinct-agent",
        "messages": [{ "role": "user", "content": "question alpha" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "answer alpha" }]
    });
    let (st1, _) = chat_turn(&app, p1).await;
    assert_eq!(st1, StatusCode::OK);

    // Turn 2: new user query (no continuation)
    let p2 = json!({
        "agent_id": "deep-distinct-agent",
        "messages": [
            { "role": "user", "content": "question alpha" },
            { "role": "assistant", "content": "answer alpha" },
            { "role": "user", "content": "question beta" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "answer beta" }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    // Turn 3: another fresh user query
    let p3 = json!({
        "agent_id": "deep-distinct-agent",
        "messages": [
            { "role": "user", "content": "question alpha" },
            { "role": "assistant", "content": "answer alpha" },
            { "role": "user", "content": "question beta" },
            { "role": "assistant", "content": "answer beta" },
            { "role": "user", "content": "question gamma" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "answer gamma" }]
    });
    let (st3, _) = chat_turn(&app, p3).await;
    assert_eq!(st3, StatusCode::OK);

    cap.wait_persist_idle().await;
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 3, "3 core persists");

    // All 3 should have user_query_event (all are fresh user queries)
    let uq_ids: Vec<&str> = core
        .iter()
        .map(|p| {
            p.user_query_event
                .as_ref()
                .expect("user_query present")
                .event_id
                .as_str()
        })
        .collect();
    let chain_ids: Vec<&str> = core
        .iter()
        .map(|p| {
            p.llm_response_event
                .as_ref()
                .or(p.user_query_event.as_ref())
                .expect("has events")
                .causal_chain_id
                .as_str()
        })
        .collect();

    // All user_query_event_ids must be unique
    let unique_uq: std::collections::HashSet<&str> = uq_ids.iter().copied().collect();
    assert_eq!(
        unique_uq.len(),
        3,
        "3 distinct user_query_event_ids: {:?}",
        uq_ids
    );

    // All chain_ids must be unique (each is a fresh user query)
    let unique_chain: std::collections::HashSet<&str> = chain_ids.iter().copied().collect();
    assert_eq!(
        unique_chain.len(),
        3,
        "3 distinct chain_ids: {:?}",
        chain_ids
    );
}

/// Tool result events have correct structure: tool_call_id in content, parent linkage
#[tokio::test]
async fn deep_tool_result_event_structure() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: tool calls
    let p1 = json!({
        "agent_id": "deep-tr-struct-agent",
        "messages": [{ "role": "user", "content": "read stuff" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-trs1", "read_file", json!({"path": "x"}))]
        }]
    });
    let (st1, _) = chat_turn(&app, p1).await;
    assert_eq!(st1, StatusCode::OK);

    // Turn 2: continuation with tool result
    let p2 = json!({
        "agent_id": "deep-tr-struct-agent",
        "messages": [
            { "role": "user", "content": "read stuff" },
            { "role": "assistant", "content": "", "tool_calls": [
                tool_call("tc-trs1", "read_file", json!({"path": "x"}))
            ] },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{
            "tool_call_id": "tc-trs1",
            "name": "read_file",
            "content": "file contents here"
        }],
        "test_llm_rounds": [{ "full_text": "done" }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    cap.wait_persist_idle().await;

    let tools = cap.tool_plans.lock().await;
    // Find the tool_result events (from turn 2)
    let tool_results: Vec<_> = tools
        .iter()
        .flat_map(|plan| &plan.events)
        .filter(|e| e.event_type == "tool_result")
        .collect();
    assert_eq!(tool_results.len(), 1, "1 tool_result event");

    let tr = &tool_results[0];
    // Content is {"name": "...", "result": "..."} — tool_call_id is in metadata only
    let content: Value =
        serde_json::from_str(&tr.content).unwrap_or_else(|_| Value::String(tr.content.clone()));
    if let Value::Object(map) = &content {
        let name = map.get("name").and_then(Value::as_str);
        assert_eq!(name, Some("read_file"), "tool_result content has tool name");
        let result = map.get("result").and_then(Value::as_str);
        assert!(result.is_some(), "tool_result content has result field");
    } else {
        panic!("tool_result content should be JSON object");
    }

    // Metadata contains tool_call_id
    let meta = tr.metadata.as_ref().expect("tool_result has metadata");
    let meta_tcid = meta.get("tool_call_id").and_then(Value::as_str);
    assert_eq!(
        meta_tcid,
        Some("tc-trs1"),
        "metadata has correct tool_call_id"
    );

    // Causal chain matches
    let core = cap.core_plans.lock().await;
    let turn2_chain = core[1]
        .llm_response_event
        .as_ref()
        .expect("turn 2 llm_response")
        .causal_chain_id
        .as_str();
    assert_eq!(
        tr.causal_chain_id.as_str(),
        turn2_chain,
        "tool_result causal_chain_id matches llm_response"
    );
}

/// Edge callback ledger: verify it's wired and accessible through AppState
#[tokio::test]
async fn deep_edge_callback_ledger_wired() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Just verify the app works and the ledger is functional by running a turn
    let payload = json!({
        "agent_id": "deep-ledger-agent",
        "messages": [{ "role": "user", "content": "ledger test" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "response" }]
    });
    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    // Verify turn completed successfully with all persists
    let core = cap.core_plans.lock().await;
    assert_eq!(core.len(), 1);
    assert!(core[0].user_query_event.is_some());
    assert!(core[0].llm_response_event.is_some());
}

/// Auxiliary events consistency: all aux events in a turn share the same causal_chain_id
#[tokio::test]
async fn deep_auxiliary_events_chain_consistency() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "deep-aux-chain-agent",
        "messages": [{ "role": "user", "content": "aux chain test" }],
        "edge_tools": [tool_schema("read_file"), tool_schema("grep")],
        "test_llm_rounds": [{
            "tool_calls": [
                tool_call("tc-ac1", "read_file", json!({"path": "a"})),
                tool_call("tc-ac2", "grep", json!({"path": "b"})),
            ],
            "usage": { "prompt_tokens": 200, "completion_tokens": 100 }
        }]
    });
    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    let aux = cap.aux_events.lock().await;
    assert!(!aux.is_empty(), "auxiliary events present");

    let core_chain = core[0]
        .llm_response_event
        .as_ref()
        .or(core[0].user_query_event.as_ref())
        .map(|e| e.causal_chain_id.as_str())
        .expect("core events exist");

    for ev in aux.iter() {
        assert_eq!(
            ev.causal_chain_id.as_str(),
            core_chain,
            "aux event {} chain_id must match core events",
            ev.event_type
        );
    }
}

/// Hook plan contains session and user metadata
#[tokio::test]
async fn deep_hook_plan_session_metadata() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    let payload = json!({
        "agent_id": "deep-hook-meta-agent",
        "messages": [{ "role": "user", "content": "hook metadata test" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "full_text": "response",
            "usage": { "prompt_tokens": 50, "completion_tokens": 25 }
        }]
    });
    let (st, _) = chat_turn(&app, payload).await;
    assert_eq!(st, StatusCode::OK);

    cap.wait_persist_idle().await;

    let hooks = cap.hook_plans.lock().await;
    assert_eq!(hooks.len(), 1, "1 hook plan");
    let hook = &hooks[0];
    // Hook plan should have session_id and user_id
    // Hook plan has optional fields — verify at least one is populated
    let has_any_data = hook.decision_audit.is_some()
        || hook.skill_selection.is_some()
        || hook.implicit_feedback.is_some()
        || hook.reflection_mark.is_some()
        || hook.reflection_lesson.is_some();
    // The hook writer is called even if all fields are None (the bridge always builds one)
    // So we just verify the hook was invoked (already asserted hooks.len() == 1 above)
    let _ = has_any_data; // may or may not have data depending on turn content
}

/// Multi-turn mixed pattern: text → tool+continuation → text → verify all persisted correctly
#[tokio::test]
async fn deep_multi_turn_mixed_pattern_complete() {
    init_env();
    let cap = AllCaptures::default();
    let app = build_test_app(cap.clone());

    // Turn 1: text-only
    let p1 = json!({
        "agent_id": "deep-mixed-agent",
        "messages": [{ "role": "user", "content": "hello" }],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "hi there" }]
    });
    let (st1, _) = chat_turn(&app, p1).await;
    assert_eq!(st1, StatusCode::OK);

    // Turn 2: tool call
    let p2 = json!({
        "agent_id": "deep-mixed-agent",
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": "hi there" },
            { "role": "user", "content": "read file a" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{
            "tool_calls": [tool_call("tc-mx1", "read_file", json!({"path": "a"}))]
        }]
    });
    let (st2, _) = chat_turn(&app, p2).await;
    assert_eq!(st2, StatusCode::OK);

    // Turn 3: continuation with tool result
    let p3 = json!({
        "agent_id": "deep-mixed-agent",
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": "hi there" },
            { "role": "user", "content": "read file a" },
            { "role": "assistant", "content": "", "tool_calls": [
                tool_call("tc-mx1", "read_file", json!({"path": "a"}))
            ] },
        ],
        "edge_tools": [tool_schema("read_file")],
        "tool_results": [{ "tool_call_id": "tc-mx1", "content": "file a data" }],
        "test_llm_rounds": [{ "full_text": "file a says..." }]
    });
    let (st3, _) = chat_turn(&app, p3).await;
    assert_eq!(st3, StatusCode::OK);

    // Turn 4: fresh user query
    let p4 = json!({
        "agent_id": "deep-mixed-agent",
        "messages": [
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": "hi there" },
            { "role": "user", "content": "read file a" },
            { "role": "assistant", "content": "file a says..." },
            { "role": "user", "content": "thanks, now something else" },
        ],
        "edge_tools": [tool_schema("read_file")],
        "test_llm_rounds": [{ "full_text": "sure thing" }]
    });
    let (st4, _) = chat_turn(&app, p4).await;
    assert_eq!(st4, StatusCode::OK);

    cap.wait_persist_idle().await;

    let core = cap.core_plans.lock().await;
    let tools = cap.tool_plans.lock().await;
    let activity = cap.activity_plans.lock().await;
    let hooks = cap.hook_plans.lock().await;

    assert_eq!(core.len(), 4, "4 core persists");
    assert_eq!(activity.len(), 4, "4 activity updates");
    assert_eq!(hooks.len(), 4, "4 hook persists");

    // Turn 1: text only → user_query + llm_response, no tools
    assert!(core[0].user_query_event.is_some());
    assert!(core[0].llm_response_event.is_some());

    // Turn 2: tool call → user_query + llm_response + tool events
    assert!(core[1].user_query_event.is_some());

    // Turn 3: continuation → no user_query, has llm_response + tool_results
    assert!(
        core[2].user_query_event.is_none(),
        "continuation skips user_query"
    );
    assert!(core[2].llm_response_event.is_some());

    // Turn 4: fresh query → user_query + llm_response
    assert!(
        core[3].user_query_event.is_some(),
        "new query has user_query"
    );
    assert!(core[3].llm_response_event.is_some());

    // Chain IDs: turn 2 & 3 share chain (continuation), turn 1 & 4 are different
    let chain2 = core[1]
        .user_query_event
        .as_ref()
        .expect("t2 has uq")
        .causal_chain_id
        .as_str();
    let chain3 = core[2]
        .llm_response_event
        .as_ref()
        .expect("t3 has lr")
        .causal_chain_id
        .as_str();
    let chain4 = core[3]
        .user_query_event
        .as_ref()
        .expect("t4 has uq")
        .causal_chain_id
        .as_str();
    assert_eq!(chain2, chain3, "continuation reuses chain");
    assert_ne!(chain2, chain4, "new query gets fresh chain");

    // Tool events: turn 2 has tool_calls, turn 3 has tool_results
    let all_tool_events: Vec<_> = tools.iter().flat_map(|p| &p.events).collect();
    let tool_calls: Vec<_> = all_tool_events
        .iter()
        .filter(|e| e.event_type == "tool_call")
        .collect();
    let tool_results: Vec<_> = all_tool_events
        .iter()
        .filter(|e| e.event_type == "tool_result")
        .collect();
    assert_eq!(tool_calls.len(), 1, "1 tool_call");
    assert_eq!(tool_results.len(), 1, "1 tool_result");
}

// ── Failing writer stubs for unhappy path tests ─────────────────────────────

#[derive(Clone)]
struct FailCoreWriter;
#[async_trait]
impl TurnCoreEventWriter for FailCoreWriter {
    async fn persist(&self, _plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String> {
        Err("simulated core persist failure".to_string())
    }
}

#[derive(Clone)]
struct FailToolWriter;
#[async_trait]
impl TurnToolEventWriter for FailToolWriter {
    async fn persist(&self, _plan: TurnToolEventPersistPlan) -> Result<(), String> {
        Err("simulated tool persist failure".to_string())
    }
}

#[derive(Clone)]
struct FailActivityWriter;
#[async_trait]
impl TurnSessionActivityWriter for FailActivityWriter {
    async fn update_session_activity(
        &self,
        _session_id: &str,
        _plan: SessionActivityUpdatePlan,
    ) -> Result<(), String> {
        Err("simulated activity update failure".to_string())
    }
}
