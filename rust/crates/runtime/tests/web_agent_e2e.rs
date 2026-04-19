#![cfg(feature = "bridge-e2e-hooks")]
//! Web agent mode E2E tests — incremental SSE streaming, edge tool delivery via ledger.
//!
//! These tests exercise the `/chat/stream` → `ServerAgenticLoopHost` path (NOT the bridge),
//! using `test_llm_rounds` injected into the host to mock LLM responses.
//!
//! ```text
//! cargo test -p astra-runtime --test web_agent_e2e --features bridge-e2e-hooks
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use astra_runtime::{
    AgenticRunLifecycleService, AppState, AuthLoginRequestData, AuthRefreshRequestData,
    AuthRegisterRequestData, AuthService, AuthTokenRecord, AuthUserRecord, ErrorResponse,
    FernetTokenEncryptor, HealthChecker, MatrixOneSettings, ServiceInfo, SessionActivityRecord,
    SessionCreateRequestData, SessionListFilter, SessionListRecord, SessionRecord, SessionService,
    SessionUpdateRequestData, build_app,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::util::ServiceExt;

// ── Env setup ────────────────────────────────────────────────────────────────

const SECRET: &str = "web-agent-e2e-secret";
const TOKEN: &str = "Bearer web-agent-e2e-token";
const USER_ID: &str = "web-agent-e2e-user";

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
    async fn current_user(
        &self,
        _headers: &axum::http::HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(AuthUserRecord {
            user_id: USER_ID.into(),
            username: "web-e2e".into(),
            email: "web-e2e@test.com".into(),
            display_name: None,
        })
    }
    async fn register(
        &self,
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[derive(Clone)]
struct StubSession;
#[async_trait]
impl SessionService for StubSession {
    async fn create_session(
        &self,
        _: String,
        _: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id: format!("web-e2e-{}", uuid::Uuid::new_v4()),
            user_id: String::new(),
            agent_id: None,
            title: None,
            status: "active".into(),
            metadata: Default::default(),
            event_count: 0,
            created_at: String::new(),
            updated_at: None,
            ended_at: None,
        })
    }
    async fn get_session(
        &self,
        session_id: String,
        _: String,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id,
            user_id: String::new(),
            agent_id: None,
            title: None,
            status: "active".into(),
            metadata: Default::default(),
            event_count: 0,
            created_at: String::new(),
            updated_at: None,
            ended_at: None,
        })
    }
    async fn update_session(
        &self,
        _: String,
        _: String,
        _: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn list_sessions(
        &self,
        _: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn delete_session(
        &self,
        _: String,
        _: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionActivityRecord {
            session_id: "stub".into(),
            activities: vec![],
            total: 0,
        })
    }
}

// ── App builder ──────────────────────────────────────────────────────────────

fn build_test_app() -> (Router, Arc<tokio::sync::Mutex<HashMap<String, Value>>>) {
    let enc =
        Arc::new(FernetTokenEncryptor::new("web-e2e-fernet-key-32-chars!!!").expect("fernet key"));
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();

    let lifecycle = AgenticRunLifecycleService::new(
        MatrixOneSettings {
            host: "127.0.0.1".into(),
            port: 1,
            user: "x".into(),
            password: "x".into(),
            database: "x".into(),
        },
        enc,
        ledger.clone(),
    );

    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (build_app(state), ledger)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

#[allow(dead_code)]
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

#[allow(dead_code)]
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

/// Send a POST /chat/stream request and collect all SSE events from the stream.
async fn chat_stream_collect(app: &Router, payload: Value) -> Vec<Value> {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Collect the SSE body. Each line has format: "data: {json}\n\n"
    let body_bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    parse_sse_events(&body_str)
}

/// Send a POST /chat/stream and return the streaming body as a stream of bytes.
/// This is used for tests that need to read events incrementally while posting
/// tool results concurrently.
async fn chat_stream_start(app: &Router, payload: Value) -> axum::response::Response {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(payload.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

/// Parse SSE events from a body string.
fn parse_sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter(|line| line.starts_with("data: "))
        .filter_map(|line| serde_json::from_str(line.strip_prefix("data: ").unwrap()).ok())
        .collect()
}

/// POST /tools/result
async fn post_tool_result(
    app: &Router,
    request_id: &str,
    output: &str,
    status: &str,
) -> StatusCode {
    let body = json!({
        "request_id": request_id,
        "status": status,
        "output": output,
        "duration_ms": 10,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/tools/result")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// POST /approval/respond
async fn post_approval_respond(app: &Router, request_id: &str, decision: &str) -> StatusCode {
    let body = json!({
        "request_id": request_id,
        "decision": decision,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/approval/respond")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[allow(dead_code)]
/// POST /approval/respond with session_id and tool_name
async fn post_approval_respond_full(
    app: &Router,
    request_id: &str,
    decision: &str,
    tool_name: &str,
) -> StatusCode {
    let body = json!({
        "request_id": request_id,
        "decision": decision,
        "tool_name": tool_name,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/approval/respond")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// Read SSE events incrementally from a streaming response body.
async fn read_sse_events_from_body(body: Body) -> Vec<Value> {
    let bytes = body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&bytes);
    parse_sse_events(&body_str)
}

#[allow(dead_code)]
fn find_event<'a>(events: &'a [Value], event_type: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some(event_type))
}

fn find_events<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some(event_type))
        .collect()
}

#[allow(dead_code)]
fn find_event_type<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| {
            e.get("type").and_then(Value::as_str) == Some(event_type)
                || e.get("event_type").and_then(Value::as_str) == Some(event_type)
        })
        .collect()
}

// ══════════════════════════════════════════════════════════════════════════════
// TESTS
// ══════════════════════════════════════════════════════════════════════════════

// ── Basic streaming: text-only response ──────────────────────────────────────

#[tokio::test]
async fn text_only_response_streams_session_info_and_text() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Hello",
        "context": {
            "test_llm_rounds": [
                { "full_text": "Hi there!" }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;

    // First event should be session_info.
    assert!(
        events.len() >= 2,
        "expected session_info + text events, got {}",
        events.len()
    );
    let session_info = &events[0];
    assert_eq!(session_info["type"], "session_info");
    assert!(session_info.get("session_id").is_some());
    assert!(session_info.get("run_id").is_some());

    // Should have text_delta event.
    let text_events = find_events(&events, "text_delta");
    assert!(
        !text_events.is_empty(),
        "expected at least one text_delta event"
    );
    assert_eq!(text_events[0]["content"], "Hi there!");
}

#[tokio::test]
async fn text_with_reasoning_streams_both() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Think step by step",
        "context": {
            "test_llm_rounds": [
                {
                    "full_text": "The answer is 42.",
                    "reasoning": "Let me think about this...",
                }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;

    let reasoning = find_events(&events, "reasoning_delta");
    assert!(!reasoning.is_empty(), "expected reasoning_delta events");
    assert_eq!(reasoning[0]["content"], "Let me think about this...");

    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
    assert_eq!(text[0]["content"], "The answer is 42.");
}

#[tokio::test]
async fn usage_event_emitted() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "hello",
        "context": {
            "test_llm_rounds": [
                {
                    "full_text": "hi",
                    "usage": { "prompt_tokens": 100, "completion_tokens": 50 }
                }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    let usage = find_events(&events, "usage");
    assert!(!usage.is_empty(), "expected usage event");
    assert_eq!(usage[0]["prompt_tokens"], 100);
    assert_eq!(usage[0]["completion_tokens"], 50);
}

// ── Edge tool delivery via ledger ────────────────────────────────────────────

#[tokio::test]
async fn edge_tool_delivery_emits_tool_request_and_waits_for_result() {
    init_env();
    let (app, _ledger) = build_test_app();

    let payload = json!({
        "message": "Read the file",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-read-1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/tmp/test.txt\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "The file contains: hello world"
                }
            ],
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read a file",
                        "parameters": {
                            "type": "object",
                            "properties": { "path": { "type": "string" } }
                        }
                    }
                }
            ]
        }
    });

    // Start the stream in a background task — it will block waiting for tool result.
    let app_clone = app.clone();
    let stream_task = tokio::spawn(async move {
        let resp = chat_stream_start(&app_clone, payload).await;
        assert_eq!(resp.status(), StatusCode::OK);
        read_sse_events_from_body(resp.into_body()).await
    });

    // Wait a bit for the server to start processing and emit tool_request.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Post tool result to the ledger.
    let status = post_tool_result(&app, "tc-read-1", "hello world", "ok").await;
    assert_eq!(status, StatusCode::OK);

    // Wait for the stream to complete.
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), stream_task)
        .await
        .expect("stream timed out")
        .expect("stream task failed");

    // Verify session_info is present.
    assert_eq!(events[0]["type"], "session_info");

    // Verify we got tool_call events.
    let tool_calls = find_events(&events, "tool_call");
    assert!(!tool_calls.is_empty(), "expected tool_call events");

    // Verify we got text at the end.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "expected text_delta after tool round");
}

#[tokio::test]
async fn multiple_tool_calls_in_single_round() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Read two files",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/tmp/a.txt\"}"
                            }
                        },
                        {
                            "id": "tc-2",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/tmp/b.txt\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "Both files read successfully"
                }
            ],
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read a file",
                        "parameters": {
                            "type": "object",
                            "properties": { "path": { "type": "string" } }
                        }
                    }
                }
            ]
        }
    });

    let app_clone = app.clone();
    let stream_task = tokio::spawn(async move {
        let resp = chat_stream_start(&app_clone, payload).await;
        read_sse_events_from_body(resp.into_body()).await
    });

    // Wait for processing.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Post results for both tool calls.
    let s1 = post_tool_result(&app, "tc-1", "content of a.txt", "ok").await;
    assert_eq!(s1, StatusCode::OK);
    let s2 = post_tool_result(&app, "tc-2", "content of b.txt", "ok").await;
    assert_eq!(s2, StatusCode::OK);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), stream_task)
        .await
        .expect("stream timed out")
        .expect("stream task failed");

    // Verify tool_call events for both.
    let tool_calls = find_events(&events, "tool_call");
    assert!(
        tool_calls.len() >= 2,
        "expected 2 tool_call events, got {}",
        tool_calls.len()
    );

    // Verify final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
}

// ── Multi-round: tools → results → more LLM → final text ────────────────────

#[tokio::test]
async fn multi_round_tool_execution() {
    init_env();
    let (app, _) = build_test_app();

    // Round 1: LLM calls read_file (no approval required)
    // Round 2: LLM calls list_dir (no approval required)
    // Round 3: LLM returns final text
    let payload = json!({
        "message": "List source files",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-read",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/src/main.rs\"}"
                            }
                        }
                    ]
                },
                {
                    "tool_calls": [
                        {
                            "id": "tc-list",
                            "type": "function",
                            "function": {
                                "name": "list_dir",
                                "arguments": "{\"path\": \"/src\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "Found 3 source files."
                }
            ],
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read file contents",
                        "parameters": { "type": "object", "properties": {} }
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "list_dir",
                        "description": "List directory",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let app_clone = app.clone();
    let app_for_post = app.clone();
    let stream_task = tokio::spawn(async move {
        let resp = chat_stream_start(&app_clone, payload).await;
        read_sse_events_from_body(resp.into_body()).await
    });

    // Post results as tool_request events are emitted.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let st = post_tool_result(&app_for_post, "tc-read", "fn main() {}", "ok").await;
    assert_eq!(st, 200, "tc-read POST failed");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let st = post_tool_result(&app_for_post, "tc-list", "main.rs\nlib.rs\nmod.rs", "ok").await;
    assert_eq!(st, 200, "tc-list POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(15), stream_task)
        .await
        .expect("stream timed out")
        .expect("stream task failed");

    // Should have 2 tool_call events (one per round).
    let tool_calls = find_events(&events, "tool_call");
    assert!(
        tool_calls.len() >= 2,
        "expected >= 2 tool_call events, got {}",
        tool_calls.len()
    );

    // Should have final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
    assert!(
        text.iter()
            .any(|t| t["content"].as_str().unwrap_or("").contains("Found"))
    );
}

// ── Server-side tools (no edge tools → auto-populated) ──────────────────────

#[tokio::test]
async fn server_side_tools_no_edge_tools_auto_populated() {
    init_env();
    let (app, _) = build_test_app();

    // No edge_tools in context → server_side_tools = true.
    // The mock LLM returns tool calls, but server_tool_executor would handle them.
    // With mock LLM, tool calls with no edge tools won't go through the ledger.
    let payload = json!({
        "message": "Hello server mode",
        "context": {
            "test_llm_rounds": [
                { "full_text": "I'm running in server mode." }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    assert_eq!(events[0]["type"], "session_info");
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
    assert_eq!(text[0]["content"], "I'm running in server mode.");
}

// ── Session ID preservation ──────────────────────────────────────────────────

#[tokio::test]
async fn custom_session_id_preserved() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Hello",
        "session_id": "custom-web-session-123",
        "context": {
            "test_llm_rounds": [
                { "full_text": "Hello!" }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    let session_info = &events[0];
    assert_eq!(session_info["session_id"], "custom-web-session-123");
}

// ── Error scenario: empty test_llm_rounds ────────────────────────────────────

#[tokio::test]
async fn empty_test_llm_rounds_completes_gracefully() {
    init_env();
    let (app, _) = build_test_app();

    // No rounds → loop should complete immediately (no LLM to call).
    // This may result in an error event since model resolution will fail
    // (no real DB), but the stream should still complete.
    let payload = json!({
        "message": "Hello",
        "context": {
            "test_llm_rounds": []
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    // The response should complete (not hang forever).
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_sse_events_from_body(resp.into_body()),
    )
    .await
    .expect("stream should not hang on empty rounds");

    // Should at least have session_info.
    assert!(!events.is_empty(), "expected at least session_info event");
    assert_eq!(events[0]["type"], "session_info");
}

// ── Event ordering ───────────────────────────────────────────────────────────

#[tokio::test]
async fn events_arrive_in_correct_order() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Ordered test",
        "context": {
            "test_llm_rounds": [
                {
                    "full_text": "The answer.",
                    "reasoning": "Thinking...",
                    "usage": { "prompt_tokens": 20, "completion_tokens": 10 }
                }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;

    // Expected order: session_info, reasoning_delta, reasoning_done, text_delta, usage
    let types: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str))
        .collect();

    assert_eq!(types[0], "session_info");

    // Reasoning should come before text.
    let reasoning_idx = types.iter().position(|&t| t == "reasoning_delta");
    let text_idx = types.iter().position(|&t| t == "text_delta");
    if let (Some(r), Some(t)) = (reasoning_idx, text_idx) {
        assert!(r < t, "reasoning_delta should come before text_delta");
    }

    // Usage should be present.
    assert!(types.contains(&"usage"));
}

// ── Concurrent streams don't interfere ───────────────────────────────────────

#[tokio::test]
async fn concurrent_streams_isolated() {
    init_env();
    let (app, _) = build_test_app();

    let payload1 = json!({
        "message": "Stream 1",
        "context": {
            "test_llm_rounds": [
                { "full_text": "Response for stream 1" }
            ]
        }
    });

    let payload2 = json!({
        "message": "Stream 2",
        "context": {
            "test_llm_rounds": [
                { "full_text": "Response for stream 2" }
            ]
        }
    });

    let app1 = app.clone();
    let app2 = app.clone();

    let (events1, events2) = tokio::join!(
        chat_stream_collect(&app1, payload1),
        chat_stream_collect(&app2, payload2),
    );

    // Both should have their own session_ids.
    let sid1 = events1[0]["session_id"].as_str().unwrap();
    let sid2 = events2[0]["session_id"].as_str().unwrap();
    assert_ne!(
        sid1, sid2,
        "concurrent streams should have different session IDs"
    );

    // Each should have its own text.
    let text1 = find_events(&events1, "text_delta");
    let text2 = find_events(&events2, "text_delta");
    assert_eq!(text1[0]["content"], "Response for stream 1");
    assert_eq!(text2[0]["content"], "Response for stream 2");
}

// ── Tool call with error result ──────────────────────────────────────────────

#[tokio::test]
async fn tool_call_with_error_result_continues() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Try reading",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-err-1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/nonexistent\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "Sorry, the file was not found."
                }
            ],
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let app_clone = app.clone();
    let stream_task = tokio::spawn(async move {
        let resp = chat_stream_start(&app_clone, payload).await;
        read_sse_events_from_body(resp.into_body()).await
    });

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    post_tool_result(&app, "tc-err-1", "status=error: file not found", "error").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), stream_task)
        .await
        .expect("stream timed out")
        .expect("stream task failed");

    // Should still get final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "LLM should continue after tool error");
}

// ── Approval flow test ──────────────────────────────────────────────────────

#[tokio::test]
async fn tool_requiring_approval_emits_approval_event_and_waits() {
    init_env();
    let (app, _) = build_test_app();

    // write_file requires approval before tool_request is emitted.
    let payload = json!({
        "message": "Write a file",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-approve-1",
                            "type": "function",
                            "function": {
                                "name": "write_file",
                                "arguments": "{\"path\": \"/tmp/out.txt\", \"content\": \"hello\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "File written."
                }
            ],
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "description": "Write file",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let app_clone = app.clone();
    let app_for_post = app.clone();
    let stream_task = tokio::spawn(async move {
        let resp = chat_stream_start(&app_clone, payload).await;
        read_sse_events_from_body(resp.into_body()).await
    });

    // Wait for the approval_required SSE, then approve, then post tool result.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let st = post_approval_respond(&app_for_post, "tc-approve-1", "allow").await;
    assert_eq!(st, 200, "approval POST failed");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let st = post_tool_result(&app_for_post, "tc-approve-1", "written", "ok").await;
    assert_eq!(st, 200, "tool result POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), stream_task)
        .await
        .expect("stream timed out")
        .expect("stream task failed");

    // Should have approval_required event.
    let approval_events = find_events(&events, "approval_required");
    assert!(
        !approval_events.is_empty(),
        "expected approval_required event for write_file"
    );

    // Should have tool_request event (after approval granted).
    let tool_requests = find_events(&events, "tool_request");
    assert!(
        !tool_requests.is_empty(),
        "expected tool_request after approval"
    );

    // Should have final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "expected final text");
}

// ── Approval denied → error result ──────────────────────────────────────────

#[tokio::test]
async fn approval_denied_skips_tool_and_continues() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Write a file",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-deny-1",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\": \"rm -rf /\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "Operation was denied."
                }
            ],
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "description": "Run command",
                        "parameters": { "type": "object", "properties": {} }
                    }
                }
            ]
        }
    });

    let app_clone = app.clone();
    let app_for_post = app.clone();
    let stream_task = tokio::spawn(async move {
        let resp = chat_stream_start(&app_clone, payload).await;
        read_sse_events_from_body(resp.into_body()).await
    });

    // Deny the approval.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let st = post_approval_respond(&app_for_post, "tc-deny-1", "deny").await;
    assert_eq!(st, 200, "approval deny POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), stream_task)
        .await
        .expect("stream timed out")
        .expect("stream task failed");

    // Should have approval_required event.
    let approval_events = find_events(&events, "approval_required");
    assert!(
        !approval_events.is_empty(),
        "expected approval_required event"
    );

    // Should NOT have tool_request event (denied before execution).
    let tool_requests = find_events(&events, "tool_request");
    assert!(
        tool_requests.is_empty(),
        "denied tool should not emit tool_request"
    );

    // LLM should still continue with final text.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "expected final text after denial");
}
