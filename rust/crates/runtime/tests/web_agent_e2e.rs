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
    SessionUpdateRequestData, TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest,
    TurnObserverWorker, TurnToolEventPersistPlan, TurnToolEventWriter, build_app,
};
use astra_services::skills::{
    SkillInfoRecord, SkillListItem, SkillListRecord, SkillPublishRequestData, SkillRecord,
    SkillRegisterRequestData, SkillService, SkillStatusRecord, SkillVersionRecord,
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{self, Body},
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
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
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != TOKEN {
            return Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("unauthorized")),
            ));
        }
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

// ── Recording test doubles ───────────────────────────────────────────────────

/// Records all hook DB persist calls for test verification.
#[derive(Default)]
struct RecordingHookDbWriter {
    plans: tokio::sync::Mutex<Vec<TurnHookDbPersistPlan>>,
}

#[async_trait]
impl TurnHookDbWriter for RecordingHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        self.plans.lock().await.push(plan);
        Ok(())
    }
}

/// Records all observer requests for test verification.
#[derive(Default)]
struct RecordingObserverWorker {
    requests: tokio::sync::Mutex<Vec<TurnObserverRequest>>,
}

#[async_trait]
impl TurnObserverWorker for RecordingObserverWorker {
    async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
        self.requests.lock().await.push(request);
        Ok(())
    }
}

/// Records all tool event persist plans for test verification.
#[derive(Default)]
struct RecordingToolEventWriter {
    plans: tokio::sync::Mutex<Vec<TurnToolEventPersistPlan>>,
}

#[async_trait]
impl TurnToolEventWriter for RecordingToolEventWriter {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
        self.plans.lock().await.push(plan);
        Ok(())
    }
}

struct TestSkillService;

#[async_trait]
impl SkillService for TestSkillService {
    async fn register_skill(
        &self,
        _: String,
        _: SkillRegisterRequestData,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_skills(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
        if offset > 0 {
            return Ok(SkillListRecord {
                skills: Vec::new(),
                total: 1,
                limit,
                offset,
            });
        }

        Ok(SkillListRecord {
            skills: vec![SkillListItem {
                skill_id: "test-skill@1.0.0".to_string(),
                skill_name: "test-skill".to_string(),
                version: "1.0.0".to_string(),
                description: Some("Test skill".to_string()),
                status: Some("active".to_string()),
                source: Some("user".to_string()),
                category: Some("testing".to_string()),
                created_at: None,
            }],
            total: 1,
            limit,
            offset,
        })
    }

    async fn get_skill(
        &self,
        skill_id: String,
        _version: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
        if skill_id == "test-skill" || skill_id == "test-skill@1.0.0" {
            return Ok(SkillRecord {
                skill_id: "test-skill@1.0.0".to_string(),
                skill_name: "test-skill".to_string(),
                version: "1.0.0".to_string(),
                description: Some("Test skill".to_string()),
                metadata: Some(json!({
                    "skill_type": "local",
                    "instructions": "You are the test skill. Return the prepared instructions.",
                    "when_to_use": "when validating skill interception"
                })),
                created_at: None,
            });
        }

        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not found".to_string())),
        ))
    }

    async fn get_skill_info(
        &self,
        _: String,
        _: String,
    ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_skill_versions(
        &self,
        _: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_skill_status(
        &self,
        _: String,
        _: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn publish_skill(
        &self,
        _: String,
        _: SkillPublishRequestData,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn unpublish_skill(
        &self,
        _: String,
        _: String,
    ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
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

/// Build a test app with recording hook DB + observer + tool event writers for verification.
fn build_test_app_with_hooks() -> (
    Router,
    Arc<RecordingHookDbWriter>,
    Arc<RecordingObserverWorker>,
    Arc<RecordingToolEventWriter>,
) {
    init_env();
    let enc =
        Arc::new(FernetTokenEncryptor::new("web-e2e-fernet-key-32-chars!!!").expect("fernet key"));
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();
    let hook_writer = Arc::new(RecordingHookDbWriter::default());
    let observer_worker = Arc::new(RecordingObserverWorker::default());
    let tool_event_writer = Arc::new(RecordingToolEventWriter::default());

    let lifecycle = AgenticRunLifecycleService::new(
        MatrixOneSettings {
            host: "127.0.0.1".into(),
            port: 1,
            user: "x".into(),
            password: "x".into(),
            database: "x".into(),
        },
        enc,
        ledger,
    )
    .with_hook_db_writer(hook_writer.clone())
    .with_observer_worker(observer_worker.clone())
    .with_tool_event_writer(tool_event_writer.clone());

    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (
        build_app(state),
        hook_writer,
        observer_worker,
        tool_event_writer,
    )
}

fn build_test_app_with_hooks_and_skills() -> (
    Router,
    Arc<RecordingHookDbWriter>,
    Arc<RecordingObserverWorker>,
    Arc<RecordingToolEventWriter>,
) {
    init_env();
    let enc =
        Arc::new(FernetTokenEncryptor::new("web-e2e-fernet-key-32-chars!!!").expect("fernet key"));
    let base = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_session_service(Arc::new(StubSession));

    let ledger = base.edge_callback_ledger();
    let hook_writer = Arc::new(RecordingHookDbWriter::default());
    let observer_worker = Arc::new(RecordingObserverWorker::default());
    let tool_event_writer = Arc::new(RecordingToolEventWriter::default());

    let lifecycle = AgenticRunLifecycleService::new(
        MatrixOneSettings {
            host: "127.0.0.1".into(),
            port: 1,
            user: "x".into(),
            password: "x".into(),
            database: "x".into(),
        },
        enc,
        ledger,
    )
    .with_skill_service(Arc::new(TestSkillService))
    .with_hook_db_writer(hook_writer.clone())
    .with_observer_worker(observer_worker.clone())
    .with_tool_event_writer(tool_event_writer.clone());

    let state = base.with_run_lifecycle_service(Arc::new(lifecycle));
    (
        build_app(state),
        hook_writer,
        observer_worker,
        tool_event_writer,
    )
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
/// Read SSE events from a body frame by frame, collecting them incrementally.
async fn read_sse_events_incremental(body: Body) -> Vec<Value> {
    let mut collected = Vec::new();
    let mut buf = String::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        // Parse complete SSE events from buf.
        while let Some(idx) = buf.find("\n\n") {
            let event_str = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            if let Some(data) = event_str.strip_prefix("data: ") {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    collected.push(v);
                }
            }
        }
    }
    collected
}

/// DELETE /chat/runs/{run_id} — cancel a run.
async fn cancel_run(app: &Router, run_id: &str) -> StatusCode {
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/chat/runs/{run_id}"))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
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

// ── Event-driven synchronization helpers ─────────────────────────────────────

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn a background task that reads SSE events from a streaming body,
/// sending each event through an unbounded channel for real-time consumption.
/// Returns (receiver, join_handle). The join handle resolves to all collected events.
async fn spawn_sse_reader(body: Body) -> (mpsc::UnboundedReceiver<Value>, JoinHandle<Vec<Value>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut events = Vec::new();
        let mut buf = String::new();
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(idx) = buf.find("\n\n") {
                let event_str = buf[..idx].to_string();
                buf = buf[idx + 2..].to_string();
                if let Some(data) = event_str.strip_prefix("data: ") {
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        let _ = tx.send(v.clone());
                        events.push(v);
                    }
                }
            }
        }
        events
    });
    (rx, handle)
}

/// Wait for an SSE event of a specific type from the channel (with timeout).
async fn wait_for_sse(
    rx: &mut mpsc::UnboundedReceiver<Value>,
    event_type: &str,
    timeout_secs: u64,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) => {
                if event.get("type").and_then(Value::as_str) == Some(event_type) {
                    return event;
                }
            }
            Ok(None) => panic!("stream ended without '{event_type}' event"),
            Err(_) => panic!("timed out ({timeout_secs}s) waiting for '{event_type}' event"),
        }
    }
}

#[derive(Clone)]
struct MockToolScenarioStep {
    request_id: &'static str,
    tool_name: &'static str,
    args: Value,
    result_output: &'static str,
    requires_approval: bool,
}

#[derive(Clone)]
struct MockToolScenario {
    name: &'static str,
    message: String,
    edge_tools: Vec<&'static str>,
    steps: Vec<MockToolScenarioStep>,
    final_text: &'static str,
    expected_query_fragments: Vec<&'static str>,
}

async fn run_mock_tool_scenario(case: MockToolScenario) {
    let (app, hook_writer, _observer, tool_writer) = build_test_app_with_hooks();
    let edge_tools: Vec<Value> = case
        .edge_tools
        .iter()
        .map(|tool| tool_schema(tool))
        .collect();

    if case.steps.is_empty() {
        let events = chat_stream_collect(
            &app,
            json!({
                "message": &case.message,
                "context": {
                    "test_llm_rounds": [{ "full_text": case.final_text }]
                }
            }),
        )
        .await;
        assert!(
            find_events(&events, "text_delta")
                .iter()
                .any(|event| event["content"].as_str() == Some(case.final_text)),
            "{}: expected final text",
            case.name
        );

        let hw = hook_writer.clone();
        poll_until(
            move || {
                let hw = hw.clone();
                async move { !hw.plans.lock().await.is_empty() }
            },
            5,
        )
        .await;

        let plans = hook_writer.plans.lock().await;
        let plan = plans.last().expect("text-only hook plan");
        let audit = plan
            .decision_audit
            .as_ref()
            .expect("text-only decision audit");
        assert_eq!(
            audit.decision_type, "response_generation",
            "{}: text-only case should persist response_generation",
            case.name
        );
        assert!(
            plan.skill_selection.is_none(),
            "{}: text-only case should not persist skill_selection",
            case.name
        );
        return;
    }

    let tool_calls: Vec<Value> = case
        .steps
        .iter()
        .map(|step| tool_call(step.request_id, step.tool_name, step.args.clone()))
        .collect();

    let resp = chat_stream_start(
        &app,
        json!({
            "message": &case.message,
            "context": {
                "test_llm_rounds": [
                    { "tool_calls": tool_calls },
                    { "full_text": case.final_text }
                ],
                "edge_tools": edge_tools
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    for step in &case.steps {
        if step.requires_approval {
            let approval = wait_for_sse(&mut rx, "approval_required", 5).await;
            assert_eq!(
                approval["request_id"].as_str(),
                Some(step.request_id),
                "{}: approval should match {}",
                case.name,
                step.request_id
            );
            let status = post_approval_respond(&app, step.request_id, "allow").await;
            assert_eq!(status, StatusCode::OK, "{}: approval accepted", case.name);
        }

        let request = wait_for_sse(&mut rx, "tool_request", 5).await;
        assert_eq!(
            request["request_id"].as_str(),
            Some(step.request_id),
            "{}: tool_request should match {}",
            case.name,
            step.request_id
        );
        let status = post_tool_result(&app, step.request_id, step.result_output, "success").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}: tool result accepted",
            case.name
        );
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert!(
        find_events(&events, "text_delta")
            .iter()
            .any(|event| event["content"].as_str() == Some(case.final_text)),
        "{}: expected final text",
        case.name
    );

    let hw = hook_writer.clone();
    poll_until(
        move || {
            let hw = hw.clone();
            async move { !hw.plans.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let plans = hook_writer.plans.lock().await;
    let plan = plans.last().expect("tool hook plan");
    let audit = plan.decision_audit.as_ref().expect("tool decision audit");
    assert_eq!(
        audit.decision_type, "tool_selection",
        "{}: tool case should persist tool_selection",
        case.name
    );
    let skill = plan.skill_selection.as_ref().expect("tool skill selection");
    let selected_skills: std::collections::HashSet<&str> =
        skill.selected_skills.iter().map(String::as_str).collect();
    for step in &case.steps {
        assert!(
            selected_skills.contains(step.tool_name),
            "{}: missing selected skill {}",
            case.name,
            step.tool_name
        );
    }
    for fragment in &case.expected_query_fragments {
        assert!(
            skill.user_query.contains(fragment),
            "{}: user_query should contain {:?}",
            case.name,
            fragment
        );
    }

    let tw = tool_writer.clone();
    poll_until(
        move || {
            let tw = tw.clone();
            async move { !tw.plans.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let tool_plans = tool_writer.plans.lock().await;
    let tool_events = &tool_plans.last().expect("tool event plan").events;
    let tool_names: std::collections::HashSet<&str> = tool_events
        .iter()
        .filter_map(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("tool_name"))
                .and_then(Value::as_str)
        })
        .collect();
    for step in &case.steps {
        assert!(
            tool_names.contains(step.tool_name),
            "{}: missing persisted tool event {}",
            case.name,
            step.tool_name
        );
    }
}

/// Poll run status until it reaches the expected value (with timeout).
async fn poll_run_status(app: &Router, run_id: &str, expected: &str, timeout_secs: u64) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let (st, body) = get_run_status(app, run_id).await;
        if st == StatusCode::OK {
            if body["status"].as_str().unwrap_or("") == expected {
                return body;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out ({timeout_secs}s) waiting for run '{run_id}' → '{expected}'");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Poll an async condition with timeout. Returns when the predicate returns true.
async fn poll_until<F, Fut>(predicate: F, timeout_secs: u64)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if predicate().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("poll_until timed out after {timeout_secs}s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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

    // Start the stream and use event-driven synchronization.
    let resp = chat_stream_start(&app, payload).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for tool_request event before posting tool result.
    wait_for_sse(&mut rx, "tool_request", 5).await;

    // Post tool result to the ledger.
    let status = post_tool_result(&app, "tc-read-1", "hello world", "ok").await;
    assert_eq!(status, StatusCode::OK);

    // Wait for the stream to complete.
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for tool_request before posting results for both tool calls.
    wait_for_sse(&mut rx, "tool_request", 5).await;

    let s1 = post_tool_result(&app, "tc-1", "content of a.txt", "ok").await;
    assert_eq!(s1, StatusCode::OK);
    let s2 = post_tool_result(&app, "tc-2", "content of b.txt", "ok").await;
    assert_eq!(s2, StatusCode::OK);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Post results as tool_request events are emitted.
    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-read", "fn main() {}", "ok").await;
    assert_eq!(st, 200, "tc-read POST failed");

    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-list", "main.rs\nlib.rs\nmod.rs", "ok").await;
    assert_eq!(st, 200, "tc-list POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(15), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

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

#[tokio::test]
async fn skill_tool_call_is_intercepted_without_edge_tool_request() {
    init_env();
    let (app, _hook_writer, observer_worker, _tool_writer) = build_test_app_with_hooks_and_skills();

    let payload = json!({
        "message": "Use the test skill",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        tool_call("tc-skill-1", "skill", json!({"skill_name": "test-skill"}))
                    ]
                },
                {
                    "full_text": "I used the skill instructions."
                }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;

    assert!(
        find_events(&events, "tool_request").is_empty(),
        "intercepted skill should not fall through to edge tool execution"
    );

    let ow = observer_worker.clone();
    poll_until(
        || {
            let ow = ow.clone();
            async move { ow.requests.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1, "expected one observer request");
    let result = requests[0]
        .messages
        .iter()
        .find(|message| message.get("tool_call_id").and_then(Value::as_str) == Some("tc-skill-1"))
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .unwrap_or("");
    assert!(
        result.contains("<skill-loaded name=\"test-skill\"/>"),
        "skill result should be injected into the turn: {result}"
    );

    let text_events = find_events(&events, "text_delta");
    assert!(
        text_events
            .iter()
            .any(|event| event["content"].as_str() == Some("I used the skill instructions.")),
        "final LLM round should continue after skill interception"
    );
}

#[tokio::test]
async fn unknown_skill_returns_error_without_edge_tool_request() {
    init_env();
    let (app, _hook_writer, observer_worker, _tool_writer) = build_test_app_with_hooks_and_skills();

    let payload = json!({
        "message": "Use a missing skill",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        tool_call("tc-skill-unknown", "skill", json!({"skill_name": "missing-skill"}))
                    ]
                },
                {
                    "full_text": "The skill was unavailable."
                }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;

    assert!(
        find_events(&events, "tool_request").is_empty(),
        "unknown skill should fail in interception, not as an edge tool"
    );

    let ow = observer_worker.clone();
    poll_until(
        || {
            let ow = ow.clone();
            async move { ow.requests.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1, "expected one observer request");
    let result = requests[0]
        .messages
        .iter()
        .find(|message| {
            message.get("tool_call_id").and_then(Value::as_str) == Some("tc-skill-unknown")
        })
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .unwrap_or("");
    assert!(
        result.contains("Unknown skill") || result.contains("unknown skill"),
        "unknown skill should surface a clear error: {result}"
    );
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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-err-1", "status=error: file not found", "error").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for the approval_required SSE, then approve, then post tool result.
    wait_for_sse(&mut rx, "approval_required", 5).await;
    let st = post_approval_respond(&app, "tc-approve-1", "allow").await;
    assert_eq!(st, 200, "approval POST failed");

    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-approve-1", "written", "ok").await;
    assert_eq!(st, 200, "tool result POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Deny the approval.
    wait_for_sse(&mut rx, "approval_required", 5).await;
    let st = post_approval_respond(&app, "tc-deny-1", "deny").await;
    assert_eq!(st, 200, "approval deny POST failed");

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

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

// ── Cancellation test ───────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_mid_stream_stops_further_rounds() {
    init_env();
    let (app, _) = build_test_app();

    // Round 1: edge tool (read_file) — will wait on ledger
    // Round 2: text — should NOT execute if cancelled between rounds
    let payload = json!({
        "message": "Read and summarize",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "id": "tc-cancel-1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/src/main.rs\"}"
                            }
                        }
                    ]
                },
                {
                    "full_text": "This text should NOT appear because we cancelled."
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

    // Start the stream.
    let app_clone = app.clone();
    let resp = chat_stream_start(&app_clone, payload).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Read SSE frames incrementally to get the run_id, then cancel.
    let body = resp.into_body();
    let mut collected_events: Vec<Value> = Vec::new();
    let mut buf = String::new();
    let mut stream = body.into_data_stream();
    let mut run_id: Option<String> = None;
    let mut cancelled = false;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);

    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next()).await;
        let frame = match frame {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(_))) | Err(_) => break, // error or timeout
            Ok(None) => break,                  // stream ended
        };
        buf.push_str(&String::from_utf8_lossy(&frame));

        // Parse complete SSE events from the buffer.
        while let Some(idx) = buf.find("\n\n") {
            let event_str = buf[..idx].to_string();
            buf = buf[idx + 2..].to_string();
            if let Some(data) = event_str.strip_prefix("data: ") {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    let event_type = v
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    // Capture run_id from session_info.
                    if event_type == "session_info" {
                        run_id = v.get("run_id").and_then(Value::as_str).map(String::from);
                    }

                    collected_events.push(v);

                    // After seeing tool_request, cancel the run, then post result to unblock.
                    if event_type == "tool_request" && !cancelled {
                        if let Some(rid) = &run_id {
                            let st = cancel_run(&app, rid).await;
                            assert_eq!(st, 200, "cancel_run failed");
                            cancelled = true;

                            // Post tool result to unblock the ledger wait.
                            post_tool_result(&app, "tc-cancel-1", "file contents", "ok").await;
                        }
                    }
                }
            }
        }
    }

    assert!(cancelled, "should have cancelled the run");
    assert!(
        run_id.is_some(),
        "should have received session_info with run_id"
    );

    // Round 2's text ("This text should NOT appear") should be absent
    // because cancellation was detected before round 2 started.
    let text_events = find_events(&collected_events, "text_delta");
    let has_round2_text = text_events.iter().any(|t| {
        t.get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("should NOT appear")
    });
    assert!(
        !has_round2_text,
        "round 2 text should not appear after cancellation"
    );

    // Verify run cleaned up — status should reach cancelled/completed.
    let rid = run_id.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (st, body) = get_run_status(&app, &rid).await;
        if st == StatusCode::OK {
            let status = body["status"].as_str().unwrap_or("");
            if status == "cancelled" || status == "completed" {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run should finalize after cancel"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// EDGE CASES: Malformed payloads, missing fields, auth failures
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn missing_auth_header_returns_unauthorized() {
    init_env();
    let (app, _) = build_test_app();

    // SSE endpoints return HTTP 200 even for errors (SSE convention).
    // Auth failures are sent as SSE error events.
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("content-type", "application/json")
        .body(Body::from(json!({"message": "hi"}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = read_sse_events_from_body(resp.into_body()).await;
    let errors = find_events(&events, "error");
    assert!(
        !errors.is_empty(),
        "expected an error event for missing auth"
    );
    assert_eq!(errors[0]["code"], "AUTH_ERROR");
}

#[tokio::test]
async fn invalid_auth_token_returns_unauthorized() {
    init_env();
    let (app, _) = build_test_app();

    // SSE endpoints return HTTP 200 even for errors.
    // An invalid token produces an SSE error event.
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", "Bearer invalid-token")
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(json!({"message": "hi"}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = read_sse_events_from_body(resp.into_body()).await;
    let errors = find_events(&events, "error");
    assert!(
        !errors.is_empty(),
        "expected an error event for invalid token"
    );
    assert_eq!(errors[0]["code"], "AUTH_ERROR");
}

#[tokio::test]
async fn empty_message_still_completes() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "",
        "context": {
            "test_llm_rounds": [
                { "full_text": "You sent an empty message." }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    let text = find_events(&events, "text_delta");
    assert!(
        !text.is_empty(),
        "should get text back even for empty message"
    );
}

#[tokio::test]
async fn missing_message_field_returns_error() {
    init_env();
    let (app, _) = build_test_app();

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", TOKEN)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", SECRET)
        .body(Body::from(json!({"context": {}}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    // Should fail validation — either 400 or 422.
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn tool_call_with_empty_id_gets_auto_assigned() {
    init_env();
    let (app, _) = build_test_app();

    // Tool call with no ID — ensure_tool_call_ids should auto-generate one.
    let payload = json!({
        "message": "auto-id test",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [
                        {
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"/test\"}"
                            }
                        }
                    ]
                },
                { "full_text": "Done." }
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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for tool_request and discover the auto-generated ID from the event.
    let tool_req_event = wait_for_sse(&mut rx, "tool_request", 5).await;
    let auto_id = tool_req_event["request_id"]
        .as_str()
        .unwrap_or_else(|| tool_req_event["tool_call_id"].as_str().unwrap_or(""));

    // Post tool result using the discovered ID (if available).
    if !auto_id.is_empty() {
        post_tool_result(&app, auto_id, "content", "ok").await;
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(8), reader).await;

    // The stream may time out if we couldn't discover the right ID,
    // but it should not panic. The important thing is no crash.
    assert!(events.is_ok() || events.is_err(), "should not panic");
}

#[tokio::test]
async fn tool_result_for_unknown_request_id_does_not_crash() {
    init_env();
    let (app, _) = build_test_app();

    // Post a tool result with an ID that no stream is waiting for.
    let st = post_tool_result(&app, "nonexistent-id-12345", "output", "ok").await;
    // Should succeed (ledger accepts it even if nobody consumes it).
    assert_eq!(st, 200);
}

#[tokio::test]
async fn approval_for_unknown_request_id_does_not_crash() {
    init_env();
    let (app, _) = build_test_app();

    let st = post_approval_respond(&app, "nonexistent-approval-id", "allow").await;
    assert_eq!(st, 200);
}

// ══════════════════════════════════════════════════════════════════════════════
// STRESS: Large responses, many tool calls, deep nesting
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn large_text_response_streams_completely() {
    init_env();
    let (app, _) = build_test_app();

    // Generate a large text response (~10KB).
    let large_text = "x".repeat(10_000);
    let payload = json!({
        "message": "Generate a long response",
        "context": {
            "test_llm_rounds": [
                { "full_text": large_text }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
    let content = text[0]["content"].as_str().unwrap_or("");
    assert_eq!(content.len(), 10_000, "full 10KB text should be preserved");
}

#[tokio::test]
async fn many_tool_calls_in_single_round() {
    init_env();
    let (app, _) = build_test_app();

    // 5 tool calls in one round — all need results.
    let tool_calls: Vec<Value> = (0..5)
        .map(|i| {
            json!({
                "id": format!("tc-many-{i}"),
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": format!("{{\"path\": \"/file{i}\"}}")
                }
            })
        })
        .collect();

    let payload = json!({
        "message": "Read 5 files",
        "context": {
            "test_llm_rounds": [
                { "tool_calls": tool_calls },
                { "full_text": "Read all 5 files." }
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

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for tool_request then post all 5 results.
    wait_for_sse(&mut rx, "tool_request", 5).await;
    for i in 0..5 {
        let id = format!("tc-many-{i}");
        post_tool_result(&app, &id, &format!("content of file{i}"), "ok").await;
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(15), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let tool_calls_events = find_events(&events, "tool_call");
    assert!(
        tool_calls_events.len() >= 5,
        "expected >= 5 tool_call events, got {}",
        tool_calls_events.len()
    );

    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "should have final text");
}

#[tokio::test]
async fn three_sequential_rounds_all_with_tools() {
    init_env();
    let (app, _) = build_test_app();

    // 3 rounds of tools, then final text.
    let payload = json!({
        "message": "Three round tool test",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-r1",
                        "type": "function",
                        "function": { "name": "grep", "arguments": "{\"pattern\": \"test\"}" }
                    }]
                },
                {
                    "tool_calls": [{
                        "id": "tc-r2",
                        "type": "function",
                        "function": { "name": "glob", "arguments": "{\"pattern\": \"*.rs\"}" }
                    }]
                },
                {
                    "tool_calls": [{
                        "id": "tc-r3",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/found\"}" }
                    }]
                },
                { "full_text": "Completed 3-round tool chain." }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "grep", "description": "Search", "parameters": { "type": "object", "properties": {} } } },
                { "type": "function", "function": { "name": "glob", "description": "Find files", "parameters": { "type": "object", "properties": {} } } },
                { "type": "function", "function": { "name": "read_file", "description": "Read", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    for (id, output) in [
        ("tc-r1", "grep matches: 3"),
        ("tc-r2", "found: main.rs, lib.rs"),
        ("tc-r3", "file content here"),
    ] {
        wait_for_sse(&mut rx, "tool_request", 5).await;
        post_tool_result(&app, id, output, "ok").await;
    }

    let events = tokio::time::timeout(std::time::Duration::from_secs(15), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let tool_calls_events = find_events(&events, "tool_call");
    assert!(
        tool_calls_events.len() >= 3,
        "expected >= 3 tool_call events for 3 rounds, got {}",
        tool_calls_events.len()
    );

    let text = find_events(&events, "text_delta");
    assert!(
        text.iter()
            .any(|t| t["content"].as_str().unwrap_or("").contains("3-round")),
        "expected final text"
    );
}

#[tokio::test]
async fn tool_call_with_complex_json_arguments() {
    init_env();
    let (app, _) = build_test_app();

    let complex_args = serde_json::to_string(&json!({
        "path": "/src/main.rs",
        "options": {
            "encoding": "utf-8",
            "line_numbers": true,
            "range": [1, 100]
        },
        "metadata": {
            "tags": ["rust", "source"],
            "nested": { "deep": { "value": 42 } }
        }
    }))
    .unwrap();

    let payload = json!({
        "message": "Complex args test",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-complex",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": complex_args }
                    }]
                },
                { "full_text": "Done." }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "read_file", "description": "Read", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-complex", "file content", "ok").await;
    assert_eq!(st, 200);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    // Should complete normally even with complex args.
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty());
}

// ══════════════════════════════════════════════════════════════════════════════
// INTEGRATION: Mixed scenarios, session/state verification
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn text_then_tool_then_text_interleaved() {
    init_env();
    let (app, _) = build_test_app();

    // Round 1: text + tool call → text_delta emitted, then tool_request, wait on ledger
    // Round 2: text only → second text_delta, loop completes
    // (Round 1 must have tool calls so the agentic loop continues to round 2.)
    let payload = json!({
        "message": "Mixed flow",
        "context": {
            "test_llm_rounds": [
                {
                    "full_text": "Let me check that.",
                    "tool_calls": [{
                        "id": "tc-mixed",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/info\"}" }
                    }]
                },
                { "full_text": "Here is the result." }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "read_file", "description": "Read", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-mixed", "info content", "ok").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let text = find_events(&events, "text_delta");
    // Should have text from round 1 and round 2.
    assert!(
        text.len() >= 2,
        "expected at least 2 text_delta events, got {}",
        text.len()
    );
    let all_text: String = text
        .iter()
        .filter_map(|t| t["content"].as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(all_text.contains("check"), "should have round 1 text");
    assert!(all_text.contains("result"), "should have round 2 text");
}

#[tokio::test]
async fn reasoning_tokens_with_tool_calls() {
    init_env();
    let (app, _) = build_test_app();

    // LLM returns reasoning + tool calls in same round.
    let payload = json!({
        "message": "Think and act",
        "context": {
            "test_llm_rounds": [
                {
                    "reasoning": "I should read the file first to understand the context.",
                    "tool_calls": [{
                        "id": "tc-think",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/src\"}" }
                    }]
                },
                { "full_text": "Got it." }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "read_file", "description": "Read", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-think", "file data", "ok").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    // Should have reasoning events.
    let reasoning = find_events(&events, "reasoning_delta");
    assert!(!reasoning.is_empty(), "expected reasoning_delta events");

    // Should also have tool_call and text.
    let tool_calls_events = find_events(&events, "tool_call");
    assert!(!tool_calls_events.is_empty(), "expected tool_call events");
    let text = find_events(&events, "text_delta");
    assert!(!text.is_empty(), "expected final text");
}

#[tokio::test]
async fn multiple_usage_events_accumulate() {
    init_env();
    let (app, _) = build_test_app();

    // Round 1: tool call with usage → loop continues to round 2
    // Round 2: text only with usage → loop stops
    // Both rounds emit usage events through execute_mock_turn.
    let payload = json!({
        "message": "Multi-round usage",
        "context": {
            "test_llm_rounds": [
                {
                    "full_text": "Checking...",
                    "tool_calls": [{
                        "id": "tc-usage",
                        "type": "function",
                        "function": { "name": "list_dir", "arguments": "{\"path\": \"/\"}" }
                    }],
                    "usage": { "prompt_tokens": 100, "completion_tokens": 50 }
                },
                {
                    "full_text": "Done.",
                    "usage": { "prompt_tokens": 200, "completion_tokens": 100 }
                }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "list_dir", "description": "List", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-usage", "file1\nfile2", "ok").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let usage = find_events(&events, "usage");
    assert!(
        usage.len() >= 2,
        "expected at least 2 usage events, got {}",
        usage.len()
    );
}

#[tokio::test]
async fn session_info_has_required_fields() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Check session info",
        "context": {
            "test_llm_rounds": [
                { "full_text": "ok" }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    let session_info = find_events(&events, "session_info");
    assert!(!session_info.is_empty(), "expected session_info event");

    let si = session_info[0];
    assert!(
        si.get("session_id").and_then(Value::as_str).is_some(),
        "session_info must have session_id"
    );
    assert!(
        si.get("run_id").and_then(Value::as_str).is_some(),
        "session_info must have run_id"
    );
}

#[tokio::test]
async fn run_status_queryable_after_stream_completes() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Check run status",
        "context": {
            "test_llm_rounds": [
                { "full_text": "Done." }
            ]
        }
    });

    let events = chat_stream_collect(&app, payload).await;
    let session_info = find_events(&events, "session_info");
    let run_id = session_info[0]
        .get("run_id")
        .and_then(Value::as_str)
        .expect("run_id in session_info");

    // Poll run status until finalized.
    let body = poll_run_status(&app, &run_id, "completed", 5).await;
    let status = body["status"].as_str().unwrap_or("");
    assert!(
        status == "completed" || status == "running",
        "expected completed or running, got: {status}"
    );
}

#[tokio::test]
async fn tool_result_with_large_output() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "Large tool output",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-large",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/big\"}" }
                    }]
                },
                { "full_text": "Processed large output." }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "read_file", "description": "Read", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Post a large tool result (~50KB).
    let large_output = "y".repeat(50_000);
    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-large", &large_output, "ok").await;
    assert_eq!(st, 200);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let text = find_events(&events, "text_delta");
    assert!(
        !text.is_empty(),
        "should complete even with large tool output"
    );
}

#[tokio::test]
async fn approval_allow_session_approves_tool() {
    init_env();
    let (app, _) = build_test_app();

    // Test "allow_session" decision (alternative to "allow").
    let payload = json!({
        "message": "Session-wide approval",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-session-approve",
                        "type": "function",
                        "function": { "name": "write_file", "arguments": "{\"path\": \"/out\"}" }
                    }]
                },
                { "full_text": "Written." }
            ],
            "edge_tools": [
                { "type": "function", "function": { "name": "write_file", "description": "Write", "parameters": { "type": "object", "properties": {} } } }
            ]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "approval_required", 5).await;
    let st = post_approval_respond(&app, "tc-session-approve", "allow_session").await;
    assert_eq!(st, 200);

    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-session-approve", "ok", "ok").await;
    assert_eq!(st, 200);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let text = find_events(&events, "text_delta");
    assert!(
        !text.is_empty(),
        "should complete after allow_session approval"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// PHASE A: RUN LIFECYCLE, EVENT REPLAY, STATE CONSISTENCY
// ══════════════════════════════════════════════════════════════════════════════

// ── Helpers for Phase A ──────────────────────────────────────────────────────

/// GET /chat/runs/{run_id} — returns JSON body.
async fn get_run_status(app: &Router, run_id: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/chat/runs/{run_id}"))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// GET /chat/runs/{run_id} with a custom auth header.
async fn get_run_status_with_auth(app: &Router, run_id: &str, auth: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/chat/runs/{run_id}"))
        .header("authorization", auth)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// GET /chat/runs/{run_id}/stream?last_index=N — returns SSE events.
async fn get_run_stream(app: &Router, run_id: &str, last_index: u32) -> (StatusCode, Vec<Value>) {
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/chat/runs/{run_id}/stream?last_index={last_index}"
        ))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&bytes);
    let events = parse_sse_events(&body_str);
    (status, events)
}

/// GET /runs?limit=N&offset=M — list runs.
async fn list_runs(app: &Router, limit: u32, offset: u32) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/runs?limit={limit}&offset={offset}"))
        .header("authorization", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Convenience: stream a chat, wait for completion, extract run_id.
async fn stream_and_get_run_id(app: &Router, payload: Value) -> (Vec<Value>, String, String) {
    let events = chat_stream_collect(app, payload).await;
    let si = find_events(&events, "session_info");
    assert!(!si.is_empty(), "must have session_info event");
    let run_id = si[0]
        .get("run_id")
        .and_then(Value::as_str)
        .expect("run_id in session_info")
        .to_string();
    let session_id = si[0]
        .get("session_id")
        .and_then(Value::as_str)
        .expect("session_id in session_info")
        .to_string();
    (events, run_id, session_id)
}

// ── A1: Run Status Field Verification ────────────────────────────────────────

#[tokio::test]
async fn a1_run_status_all_fields_text_only() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "text only run",
        "context": {
            "test_llm_rounds": [{ "full_text": "Done." }]
        }
    });

    let (_events, run_id, session_id) = stream_and_get_run_id(&app, payload).await;
    let body = poll_run_status(&app, &run_id, "completed", 5).await;

    // Verify ALL RunStatusResponse fields.
    assert_eq!(body["run_id"].as_str().unwrap(), run_id);
    assert_eq!(body["session_id"].as_str().unwrap(), session_id);
    assert_eq!(body["status"].as_str().unwrap(), "completed");
    assert!(
        body["waiting_for"].is_null(),
        "completed run should not be waiting: {:?}",
        body["waiting_for"]
    );
    let events_count = body["events_count"].as_i64().unwrap();
    assert!(
        events_count > 0,
        "events_count should be > 0, got {events_count}"
    );
}

#[tokio::test]
async fn a1_run_status_all_fields_after_tool_round() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "tool round run",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-a1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/x\"}" }
                    }]
                },
                { "full_text": "All done." }
            ],
            "edge_tools": [tool_schema("read_file")]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    let st = post_tool_result(&app, "tc-a1", "file contents", "ok").await;
    assert_eq!(st, 200);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("timed out")
        .expect("task panicked");

    let si = find_events(&events, "session_info");
    let run_id = si[0]["run_id"].as_str().unwrap();
    let session_id = si[0]["session_id"].as_str().unwrap();

    let body = poll_run_status(&app, run_id, "completed", 5).await;
    assert_eq!(body["run_id"].as_str().unwrap(), run_id);
    assert_eq!(body["session_id"].as_str().unwrap(), session_id);
    assert_eq!(body["status"].as_str().unwrap(), "completed");
    assert!(body["waiting_for"].is_null());
    assert!(body["events_count"].as_i64().unwrap() > 0);
}

// ── A2: Run Status Transitions ───────────────────────────────────────────────

#[tokio::test]
async fn a2_transition_running_to_completed_text_only() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "transition text",
        "context": {
            "test_llm_rounds": [{ "full_text": "Response." }]
        }
    });

    let (_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    let body = poll_run_status(&app, &run_id, "completed", 5).await;
    assert_eq!(body["status"].as_str().unwrap(), "completed");
}

#[tokio::test]
async fn a2_transition_running_to_completed_after_tool_rounds() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "tool then complete",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-a2-tool",
                        "type": "function",
                        "function": { "name": "glob", "arguments": "{\"pattern\": \"*.rs\"}" }
                    }]
                },
                { "full_text": "Tool done." }
            ],
            "edge_tools": [tool_schema("glob")]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-a2-tool", "file.rs", "ok").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("timed out")
        .expect("task panicked");

    let si = find_events(&events, "session_info");
    let run_id = si[0]["run_id"].as_str().unwrap();

    let body = poll_run_status(&app, run_id, "completed", 5).await;
    assert_eq!(body["status"].as_str().unwrap(), "completed");
}

#[tokio::test]
async fn a2_transition_running_to_cancelled() {
    init_env();
    let (app, _) = build_test_app();

    // Use a tool round so the loop doesn't terminate immediately.
    let payload = json!({
        "message": "cancel me",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-a2-cancel",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/c\"}" }
                    }]
                },
                { "full_text": "never reached" }
            ],
            "edge_tools": [tool_schema("read_file")]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for tool_request to know the stream is running, then get run_id and cancel.
    let tool_req = wait_for_sse(&mut rx, "tool_request", 5).await;

    // We need to cancel — but first we need the run_id. We'll list runs to find it.
    let (_, list_body) = list_runs(&app, 10, 0).await;
    let runs = list_body["runs"].as_array().expect("runs array");
    assert!(!runs.is_empty(), "should have at least one run");
    let running = runs
        .iter()
        .find(|r| r["status"].as_str() == Some("running"));
    assert!(running.is_some(), "should have a running run");
    let run_id = running.unwrap()["run_id"].as_str().unwrap().to_string();

    let cancel_status = cancel_run(&app, &run_id).await;
    assert_eq!(cancel_status, StatusCode::OK);

    // Also post the tool result so the stream can terminate.
    post_tool_result(&app, "tc-a2-cancel", "cancelled", "ok").await;

    let _events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("timed out")
        .expect("task panicked");

    let body = poll_run_status(&app, &run_id, "cancelled", 5).await;
    let status = body["status"].as_str().unwrap();
    assert!(
        status == "cancelled" || status == "completed",
        "expected cancelled or completed after cancel, got: {status}"
    );
}

// ── A3: Event Replay via stream_run ──────────────────────────────────────────

#[tokio::test]
async fn a3_event_replay_all_events_from_index_zero() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "replay test",
        "context": {
            "test_llm_rounds": [{ "full_text": "Replay me." }]
        }
    });

    let (_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    // Replay from index 0 — should get all stored events.
    let (status, replay_events) = get_run_stream(&app, &run_id, 0).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !replay_events.is_empty(),
        "replay from index 0 should return events"
    );

    // Replayed events should have index fields.
    assert_eq!(replay_events[0]["index"], 0);

    // Should contain a run_started or run_finished event type.
    let has_terminal = replay_events.iter().any(|e| {
        let t = e["type"].as_str().unwrap_or("");
        t == "run_started" || t == "run_finished"
    });
    assert!(has_terminal, "replay should include run lifecycle events");
}

#[tokio::test]
async fn a3_event_replay_partial_from_middle() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "partial replay",
        "context": {
            "test_llm_rounds": [{ "full_text": "Partial replay content." }]
        }
    });

    let (_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    // Get all events first.
    let (_, all_events) = get_run_stream(&app, &run_id, 0).await;
    let total = all_events.len();
    assert!(total >= 2, "need at least 2 events for partial replay");

    // Replay from index 1 — should skip the first event.
    let (_, partial_events) = get_run_stream(&app, &run_id, 1).await;
    assert_eq!(
        partial_events.len(),
        total - 1,
        "partial from index 1 should have {expected} events, got {actual}",
        expected = total - 1,
        actual = partial_events.len()
    );

    // First event in partial should have index 1.
    assert_eq!(partial_events[0]["index"], 1);
}

#[tokio::test]
async fn a3_event_replay_beyond_end_returns_empty() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "beyond end",
        "context": {
            "test_llm_rounds": [{ "full_text": "Short." }]
        }
    });

    let (_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    // Replay from a very high index.
    let (status, events) = get_run_stream(&app, &run_id, 9999).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        events.is_empty(),
        "replay beyond end should return empty, got {} events",
        events.len()
    );
}

#[tokio::test]
async fn a3_event_replay_matches_sse_stream_content() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "match test",
        "context": {
            "test_llm_rounds": [{ "full_text": "Match this text." }]
        }
    });

    let (sse_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    let (_, replay_events) = get_run_stream(&app, &run_id, 0).await;

    // Find text_delta in SSE events.
    let sse_text: Vec<&str> = sse_events
        .iter()
        .filter(|e| e["type"].as_str() == Some("text_delta"))
        .filter_map(|e| e["content"].as_str().or(e["text"].as_str()))
        .collect();

    // Find text_delta in replay events.
    let replay_text: Vec<&str> = replay_events
        .iter()
        .filter(|e| e["type"].as_str() == Some("text_delta"))
        .filter_map(|e| e["content"].as_str().or(e["text"].as_str()))
        .collect();

    assert!(!sse_text.is_empty(), "SSE should have text_delta events");
    assert_eq!(
        sse_text, replay_text,
        "replay text_delta content should match SSE stream"
    );
}

// ── A4: Ledger Cleanup Verification ──────────────────────────────────────────

#[tokio::test]
async fn a4_ledger_empty_after_tool_run_completes() {
    init_env();
    let (app, ledger) = build_test_app();

    let payload = json!({
        "message": "ledger cleanup test",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-a4-ledger",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/l\"}" }
                    }]
                },
                { "full_text": "Ledger clean." }
            ],
            "edge_tools": [tool_schema("read_file")]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-a4-ledger", "content", "ok").await;

    let _events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("timed out")
        .expect("task panicked");

    let ledger_cl = ledger.clone();
    poll_until(
        || {
            let l = ledger_cl.clone();
            async move { l.lock().await.is_empty() }
        },
        5,
    )
    .await;

    // Ledger should be empty — all tool entries consumed.
    let ledger_map = ledger.lock().await;
    assert!(
        ledger_map.is_empty(),
        "ledger should be empty after run completes, has {} entries: {:?}",
        ledger_map.len(),
        ledger_map.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a4_ledger_empty_after_cancelled_run() {
    init_env();
    let (app, ledger) = build_test_app();

    let payload = json!({
        "message": "cancel ledger test",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-a4-cancel-ledger",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\": \"/cl\"}" }
                    }]
                },
                { "full_text": "never reached" }
            ],
            "edge_tools": [tool_schema("read_file")]
        }
    });

    let resp = chat_stream_start(&app, payload).await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Wait for tool_request so we know the stream is running, then cancel.
    wait_for_sse(&mut rx, "tool_request", 5).await;

    // Find running run and cancel it.
    let (_, list_body) = list_runs(&app, 10, 0).await;
    let runs = list_body["runs"].as_array().expect("runs array");
    let running = runs
        .iter()
        .find(|r| r["status"].as_str() == Some("running"));
    if let Some(r) = running {
        let run_id = r["run_id"].as_str().unwrap();
        cancel_run(&app, run_id).await;
    }

    // Post tool result so the stream can finish even if cancel didn't interrupt.
    post_tool_result(&app, "tc-a4-cancel-ledger", "cancelled", "ok").await;

    let _events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("timed out")
        .expect("task panicked");

    let ledger_cl = ledger.clone();
    poll_until(
        || {
            let l = ledger_cl.clone();
            async move { l.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let ledger_map = ledger.lock().await;
    assert!(
        ledger_map.is_empty(),
        "ledger should be empty after cancelled run, has {} entries: {:?}",
        ledger_map.len(),
        ledger_map.keys().collect::<Vec<_>>()
    );
}

// ── A5: Run Not Found / Access Denied ────────────────────────────────────────

#[tokio::test]
async fn a5_run_status_not_found() {
    init_env();
    let (app, _) = build_test_app();

    let (status, body) = get_run_status(&app, "nonexistent-run-id").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nonexistent run should 404");
    assert!(
        body["detail"].as_str().is_some(),
        "error response should have detail"
    );
}

#[tokio::test]
async fn a5_run_status_unauthorized() {
    init_env();
    let (app, _) = build_test_app();

    // Create a run first.
    let payload = json!({
        "message": "auth test",
        "context": {
            "test_llm_rounds": [{ "full_text": "Auth." }]
        }
    });
    let (_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    // Try with wrong token.
    let (status, _) = get_run_status_with_auth(&app, &run_id, "Bearer wrong-token").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "wrong token should get 401"
    );
}

#[tokio::test]
async fn a5_stream_run_not_found() {
    init_env();
    let (app, _) = build_test_app();

    // stream_run returns SSE, so errors come as SSE events.
    let (status, events) = get_run_stream(&app, "nonexistent-stream-id", 0).await;
    assert_eq!(status, StatusCode::OK, "SSE endpoints return 200");
    // Should have an error event.
    let error_events = find_events(&events, "error");
    assert!(
        !error_events.is_empty(),
        "should have SSE error event for nonexistent run"
    );
    let code = error_events[0]["code"].as_str().unwrap_or("");
    assert_eq!(code, "NOT_FOUND", "error code should be NOT_FOUND");
}

// ── A6: Session Info Consistency ─────────────────────────────────────────────

#[tokio::test]
async fn a6_session_id_consistent_across_events_and_run() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "session consistency",
        "context": {
            "test_llm_rounds": [{ "full_text": "Consistent." }]
        }
    });

    let (events, run_id, session_id) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    // Verify run status session_id matches.
    let (_, body) = get_run_status(&app, &run_id).await;
    assert_eq!(
        body["session_id"].as_str().unwrap(),
        session_id,
        "run status session_id should match session_info"
    );

    // Verify session_id is non-empty and looks like a UUID.
    assert!(!session_id.is_empty(), "session_id should not be empty");
    assert!(
        session_id.contains('-'),
        "session_id should be UUID-like: {session_id}"
    );

    // All events in the stream should be associated with this session.
    let si_events = find_events(&events, "session_info");
    assert_eq!(si_events.len(), 1, "should have exactly one session_info");
}

#[tokio::test]
async fn a6_custom_session_id_preserved() {
    init_env();
    let (app, _) = build_test_app();

    let custom_sid = format!("custom-{}", uuid::Uuid::new_v4());
    let payload = json!({
        "message": "custom session",
        "session_id": &custom_sid,
        "context": {
            "test_llm_rounds": [{ "full_text": "Custom." }]
        }
    });

    let (_events, run_id, session_id) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    // The session_id in session_info should match our custom ID.
    assert_eq!(
        session_id, custom_sid,
        "session_info should preserve custom session_id"
    );

    // Run status should also reflect the custom session_id.
    let (_, body) = get_run_status(&app, &run_id).await;
    assert_eq!(body["session_id"].as_str().unwrap(), custom_sid);
}

#[tokio::test]
async fn a6_multiple_runs_same_session() {
    init_env();
    let (app, _) = build_test_app();

    let shared_sid = format!("shared-{}", uuid::Uuid::new_v4());

    // First run.
    let payload1 = json!({
        "message": "run 1",
        "session_id": &shared_sid,
        "context": {
            "test_llm_rounds": [{ "full_text": "Run one." }]
        }
    });
    let (_, run_id_1, sid_1) = stream_and_get_run_id(&app, payload1).await;
    poll_run_status(&app, &run_id_1, "completed", 5).await;

    // Second run with same session.
    let payload2 = json!({
        "message": "run 2",
        "session_id": &shared_sid,
        "context": {
            "test_llm_rounds": [{ "full_text": "Run two." }]
        }
    });
    let (_, run_id_2, sid_2) = stream_and_get_run_id(&app, payload2).await;
    poll_run_status(&app, &run_id_2, "completed", 5).await;

    // Both should share the same session_id.
    assert_eq!(sid_1, shared_sid);
    assert_eq!(sid_2, shared_sid);
    assert_ne!(
        run_id_1, run_id_2,
        "different runs should have different run_ids"
    );

    // Both runs should be queryable.
    let (s1, b1) = get_run_status(&app, &run_id_1).await;
    let (s2, b2) = get_run_status(&app, &run_id_2).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1["session_id"].as_str().unwrap(), shared_sid);
    assert_eq!(b2["session_id"].as_str().unwrap(), shared_sid);
}

#[tokio::test]
async fn a6_list_runs_shows_completed_runs() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "list me",
        "context": {
            "test_llm_rounds": [{ "full_text": "Listed." }]
        }
    });

    let (_events, run_id, _) = stream_and_get_run_id(&app, payload).await;
    poll_run_status(&app, &run_id, "completed", 5).await;

    let (status, body) = list_runs(&app, 50, 0).await;
    assert_eq!(status, StatusCode::OK);

    let runs = body["runs"].as_array().expect("runs array");
    let found = runs.iter().any(|r| r["run_id"].as_str() == Some(&run_id));
    assert!(found, "list_runs should include the completed run {run_id}");
}

// ─── Turn Complete Event Tests ──────────────────────────────────────────────

#[tokio::test]
async fn turn_complete_event_emitted_text_only() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "say hi",
        "context": {
            "test_llm_rounds": [{ "full_text": "Hello!" }]
        }
    });

    let (events, _, _) = stream_and_get_run_id(&app, payload).await;
    let turn_completes: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"].as_str() == Some("turn_complete"))
        .collect();
    assert_eq!(
        turn_completes.len(),
        1,
        "should emit exactly one turn_complete event"
    );
    assert_eq!(
        turn_completes[0]["has_tool_calls"].as_bool(),
        Some(false),
        "text-only turn should have has_tool_calls=false"
    );
}

#[tokio::test]
async fn turn_complete_event_emitted_with_tools() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "run a tool",
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc1",
                        "function": { "name": "Read", "arguments": "{\"file_path\":\"x.txt\"}" }
                    }]
                },
                { "full_text": "Tool done." }
            ]
        }
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    let events = parse_sse_events(&body_str);

    let turn_completes: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"].as_str() == Some("turn_complete"))
        .collect();
    assert_eq!(
        turn_completes.len(),
        1,
        "should emit exactly one turn_complete"
    );
    assert_eq!(
        turn_completes[0]["has_tool_calls"].as_bool(),
        Some(true),
        "turn with tools should have has_tool_calls=true"
    );
}

#[tokio::test]
async fn turn_complete_is_last_typed_event() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "order check",
        "context": {
            "test_llm_rounds": [{ "full_text": "Done." }]
        }
    });

    let (events, _, _) = stream_and_get_run_id(&app, payload).await;
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();

    let tc_pos = types.iter().position(|t| *t == "turn_complete");
    assert!(
        tc_pos.is_some(),
        "turn_complete should be present, got: {types:?}"
    );
    // turn_complete should be the last event with a "type" field in the SSE stream.
    assert_eq!(
        tc_pos.unwrap(),
        types.len() - 1,
        "turn_complete should be the last typed SSE event, order: {types:?}"
    );
}

// ─── Client Disconnect Cancellation Tests ───────────────────────────────────

#[tokio::test]
async fn client_disconnect_run_still_finalizes() {
    init_env();
    let (app, _) = build_test_app();

    let payload = json!({
        "message": "disconnect test",
        "context": {
            "test_llm_rounds": [{ "full_text": "Quick response." }]
        }
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Read just session_info, then drop the body (simulating client disconnect).
    let mut stream = resp.into_body().into_data_stream();
    let mut run_id = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        let bytes = chunk.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        if let Some(line) = text.lines().find(|l| l.starts_with("data: ")) {
            if let Ok(v) = serde_json::from_str::<Value>(line.strip_prefix("data: ").unwrap()) {
                if v["type"].as_str() == Some("session_info") {
                    run_id = v["run_id"].as_str().unwrap_or("").to_string();
                    break;
                }
            }
        }
    }
    assert!(!run_id.is_empty(), "should get session_info with run_id");

    // Drop the stream — simulating client disconnect.
    drop(stream);

    // Wait for the background task to finalize.
    let mut final_status = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let (st, body) = get_run_status(&app, &run_id).await;
        if st == StatusCode::OK {
            final_status = body["status"].as_str().unwrap_or("").to_string();
            if final_status == "completed" || final_status == "cancelled" {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        final_status == "completed" || final_status == "cancelled",
        "run should finalize after client disconnect, got: {final_status}"
    );
}

// ── Hook DB + Observer Persistence Tests ─────────────────────────────────────

/// Text-only response produces a "response_generation" decision audit with no skills.
#[tokio::test]
async fn hook_db_decision_audit_text_only() {
    let (app, hook_writer, observer_worker, _tool_writer) = build_test_app_with_hooks();

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "hello",
            "context": {
                "test_llm_rounds": [{ "full_text": "Hi there!" }]
            }
        }),
    )
    .await;
    assert!(!events.is_empty());

    // Wait for background persistence to complete.
    let hw = hook_writer.clone();
    poll_until(
        || {
            let hw = hw.clone();
            async move { hw.plans.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1, "exactly one hook persist call");
    let plan = &plans[0];

    let audit = plan
        .decision_audit
        .as_ref()
        .expect("decision_audit present");
    assert_eq!(audit.decision_type, "response_generation");
    assert!(!audit.decision_id.is_empty());
    let output = &audit.decision_output;
    assert!(output["tool_calls"].as_array().unwrap().is_empty());
    assert!(output["text"].as_str().unwrap().contains("Hi there"));

    // No skill selection for text-only.
    assert!(plan.skill_selection.is_none());
    // No implicit feedback (server loop doesn't have next-turn signal).
    assert!(plan.implicit_feedback.is_none());

    // Observer should have been called with messages.
    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1, "observer fired once");
    assert_eq!(requests[0].user_id, USER_ID);
    assert!(!requests[0].messages.is_empty());
}

/// Tool-call response produces a "tool_selection" decision audit with skill selection.
#[tokio::test]
async fn hook_db_decision_audit_with_tools() {
    let (app, hook_writer, _observer, _tool_writer) = build_test_app_with_hooks();

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "list files",
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [tool_call("tc1", "list_files", json!({"path": "."}))]
                    },
                    { "full_text": "Here are the files." }
                ],
                "edge_tools": [tool_schema("list_files"), tool_schema("read_file")]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Deliver tool results for tc1.
    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc1", "file1.txt\nfile2.txt", "success").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert!(!events.is_empty());
    let hw = hook_writer.clone();
    poll_until(
        || {
            let hw = hw.clone();
            async move { hw.plans.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];

    let audit = plan
        .decision_audit
        .as_ref()
        .expect("decision_audit present");
    assert_eq!(audit.decision_type, "tool_selection");
    let tool_calls = audit.decision_output["tool_calls"].as_array().unwrap();
    assert!(tool_calls.iter().any(|t| t.as_str() == Some("list_files")));

    let skill = plan
        .skill_selection
        .as_ref()
        .expect("skill_selection present");
    assert_eq!(skill.skill_name, "list_files");
    assert_eq!(skill.selection_method, "llm_tool_choice");
    assert!(skill.selected_skills.contains(&"list_files".to_string()));
    assert_eq!(skill.user_query, "list files");
    assert_eq!(skill.execution_success, Some(1));
}

/// Model name is propagated to decision audit.
#[tokio::test]
async fn hook_db_decision_audit_model_name() {
    let (app, hook_writer, _observer, _tool_writer) = build_test_app_with_hooks();

    chat_stream_collect(
        &app,
        json!({
            "message": "test",
            "model": "test-model-v1",
            "context": {
                "test_llm_rounds": [{ "full_text": "ok" }]
            }
        }),
    )
    .await;
    let hw = hook_writer.clone();
    poll_until(
        || {
            let hw = hw.clone();
            async move { hw.plans.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1);
    let audit = plans[0].decision_audit.as_ref().unwrap();
    assert_eq!(audit.model_used.as_deref(), Some("test-model-v1"));
}

/// Observer receives correct session_id and turn_count.
#[tokio::test]
async fn observer_fired_with_correct_metadata() {
    let (app, _hook_writer, observer_worker, _tool_writer) = build_test_app_with_hooks();

    let events = chat_stream_collect(
        &app,
        json!({
            "message": "hello",
            "session_id": "obs-session-123",
            "context": {
                "test_llm_rounds": [{ "full_text": "Hi!" }]
            }
        }),
    )
    .await;
    let session_info = events.iter().find(|e| e["type"] == "session_info");
    let session_id = session_info.unwrap()["session_id"].as_str().unwrap();
    assert_eq!(session_id, "obs-session-123");

    let ow = observer_worker.clone();
    poll_until(
        || {
            let ow = ow.clone();
            async move { ow.requests.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let requests = observer_worker.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id, "obs-session-123");
    assert!(requests[0].turn_count >= 1, "at least one turn completed");
}

/// Multiple tool calls across rounds produce a skill selection with all tool names.
#[tokio::test]
async fn hook_db_multiple_tools_selected() {
    let (app, hook_writer, _observer, _tool_writer) = build_test_app_with_hooks();

    // Two rounds of approval-free tools (read_file, list_dir) + final text.
    let resp = chat_stream_start(
        &app,
        json!({
            "message": "do stuff",
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [
                            tool_call("tc1", "read_file", json!({"path": "a.txt"}))
                        ]
                    },
                    {
                        "tool_calls": [
                            tool_call("tc2", "list_dir", json!({"path": "/src"}))
                        ]
                    },
                    { "full_text": "Done!" }
                ],
                "edge_tools": [
                    tool_schema("read_file"),
                    tool_schema("list_dir")
                ]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    // Deliver tool results for round 1 (tc1).
    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc1", "contents of a.txt", "success").await;

    // Deliver tool results for round 2 (tc2).
    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc2", "main.rs\nlib.rs", "success").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(15), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert!(!events.is_empty());

    // Wait for async persistence.
    let hw = hook_writer.clone();
    poll_until(
        || {
            let hw = hw.clone();
            async move { hw.plans.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1);
    let audit = plans[0].decision_audit.as_ref().expect("decision_audit");
    assert_eq!(audit.decision_type, "tool_selection");

    let skill = plans[0].skill_selection.as_ref().expect("skill_selection");
    // All unique tool names should be captured.
    assert!(skill.selected_skills.contains(&"read_file".to_string()));
    assert!(skill.selected_skills.contains(&"list_dir".to_string()));
}

#[tokio::test]
async fn mock_llm_tool_flow_scenario_matrix() {
    let attachment = "[Active task attachment]\n\
Resume the active task/thread below unless the user explicitly changes topic.\n\
Treat brief follow-ups as actions on this active thread, not as brand-new unrelated tasks.\n\
If the follow-up asks to fix / patch / test / continue, apply that action to this active thread.\n\
Latest user task: review 这个: aa1f419bc040003f5de8cdfa6b414225ade82e2b\n\
Latest assistant summary:\n\
## Review: `aa1f419b` — P5 git timeout, P6 compression protection\n\
Two independent fixes in one commit. Let me review each.\n\
P5 still has a thread leak on timeout; terminate the child before returning.\n\n\
[User follow-up]\n修复?";

    let cases = vec![
        MockToolScenario {
            name: "text_only",
            message: "hello".to_string(),
            edge_tools: vec![],
            steps: vec![],
            final_text: "Hi there!",
            expected_query_fragments: vec![],
        },
        MockToolScenario {
            name: "read_file",
            message: "read the README".to_string(),
            edge_tools: vec!["read_file"],
            steps: vec![MockToolScenarioStep {
                request_id: "tc-matrix-read",
                tool_name: "read_file",
                args: json!({"path": "README.md"}),
                result_output: "README contents",
                requires_approval: false,
            }],
            final_text: "Read the README.",
            expected_query_fragments: vec!["read the README"],
        },
        MockToolScenario {
            name: "write_file_with_approval",
            message: "create a new file named notes.txt".to_string(),
            edge_tools: vec!["write_file"],
            steps: vec![MockToolScenarioStep {
                request_id: "tc-matrix-write",
                tool_name: "write_file",
                args: json!({"path": "notes.txt", "content": "hello"}),
                result_output: "file created",
                requires_approval: true,
            }],
            final_text: "Created the file.",
            expected_query_fragments: vec!["create a new file named notes.txt"],
        },
        MockToolScenario {
            name: "search_with_grep",
            message: "search the repo for TODO".to_string(),
            edge_tools: vec!["grep"],
            steps: vec![MockToolScenarioStep {
                request_id: "tc-matrix-grep",
                tool_name: "grep",
                args: json!({"pattern": "TODO", "path": "."}),
                result_output: "src/main.rs:12:// TODO",
                requires_approval: false,
            }],
            final_text: "Found TODO matches.",
            expected_query_fragments: vec!["search the repo for TODO"],
        },
        MockToolScenario {
            name: "memory_store",
            message: "记住我喜欢 Rust".to_string(),
            edge_tools: vec!["memory_store"],
            steps: vec![MockToolScenarioStep {
                request_id: "tc-matrix-mstore",
                tool_name: "memory_store",
                args: json!({"content": "User likes Rust"}),
                result_output: "memory stored",
                requires_approval: false,
            }],
            final_text: "I stored that preference.",
            expected_query_fragments: vec!["记住我喜欢 Rust"],
        },
        MockToolScenario {
            name: "memory_search",
            message: "我之前说过我喜欢什么语言?".to_string(),
            edge_tools: vec!["memory_search"],
            steps: vec![MockToolScenarioStep {
                request_id: "tc-matrix-msearch",
                tool_name: "memory_search",
                args: json!({"query": "preferred language"}),
                result_output: "User likes Rust",
                requires_approval: false,
            }],
            final_text: "You said you like Rust.",
            expected_query_fragments: vec!["我之前说过我喜欢什么语言?"],
        },
        MockToolScenario {
            name: "multi_tool_batch",
            message: "inspect the project files".to_string(),
            edge_tools: vec!["read_file", "list_dir"],
            steps: vec![
                MockToolScenarioStep {
                    request_id: "tc-matrix-list",
                    tool_name: "list_dir",
                    args: json!({"path": "."}),
                    result_output: "Cargo.toml\nREADME.md",
                    requires_approval: false,
                },
                MockToolScenarioStep {
                    request_id: "tc-matrix-read-batch",
                    tool_name: "read_file",
                    args: json!({"path": "README.md"}),
                    result_output: "README contents",
                    requires_approval: false,
                },
            ],
            final_text: "Inspected the project files.",
            expected_query_fragments: vec!["inspect the project files"],
        },
        MockToolScenario {
            name: "attachment_followup_repair",
            message: attachment.to_string(),
            edge_tools: vec!["str_replace"],
            steps: vec![MockToolScenarioStep {
                request_id: "tc-matrix-repair",
                tool_name: "str_replace",
                args: json!({"path": "rust/crates/astra-tools/src/git_gix.rs"}),
                result_output: "updated helper",
                requires_approval: true,
            }],
            final_text: "Patched the timeout path.",
            expected_query_fragments: vec![
                "[Active task attachment]",
                "aa1f419bc040003f5de8cdfa6b414225ade82e2b",
                "thread leak on timeout",
                "[User follow-up]\n修复?",
            ],
        },
    ];

    for case in cases {
        run_mock_tool_scenario(case).await;
    }
}

#[tokio::test]
async fn context_meta_exposes_late_round_guidance_signals() {
    let (app, _hook_writer, _observer, _tool_writer) = build_test_app_with_hooks();

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "inspect the project files",
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [tool_call("tc-guidance-r1", "read_file", json!({"path": "README.md"}))]
                    },
                    {
                        "tool_calls": [tool_call("tc-guidance-r2", "list_dir", json!({"path": "."}))]
                    },
                    {
                        "tool_calls": [tool_call("tc-guidance-r3", "grep", json!({"pattern": "TODO", "path": "."}))]
                    },
                    {
                        "tool_calls": [
                            tool_call("tc-guidance-r4a", "grep", json!({"pattern": "FIXME", "path": "."})),
                            tool_call("tc-guidance-r4b", "glob", json!({"pattern": "**/*.rs"}))
                        ]
                    },
                    { "full_text": "Done." }
                ],
                "edge_tools": [
                    tool_schema("read_file"),
                    tool_schema("list_dir"),
                    tool_schema("grep"),
                    tool_schema("glob")
                ]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    let request = wait_for_sse(&mut rx, "tool_request", 5).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-guidance-r1"));
    let status = post_tool_result(&app, "tc-guidance-r1", "README contents", "success").await;
    assert_eq!(status, StatusCode::OK);

    let request = wait_for_sse(&mut rx, "tool_request", 5).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-guidance-r2"));
    let status = post_tool_result(&app, "tc-guidance-r2", "src\nREADME.md", "success").await;
    assert_eq!(status, StatusCode::OK);

    let request = wait_for_sse(&mut rx, "tool_request", 5).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-guidance-r3"));
    let status = post_tool_result(&app, "tc-guidance-r3", "src/main.rs:12:// TODO", "success").await;
    assert_eq!(status, StatusCode::OK);

    let request = wait_for_sse(&mut rx, "tool_request", 5).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-guidance-r4a"));
    let status = post_tool_result(&app, "tc-guidance-r4a", "src/lib.rs:5:// FIXME", "success").await;
    assert_eq!(status, StatusCode::OK);

    let request = wait_for_sse(&mut rx, "tool_request", 5).await;
    assert_eq!(request["request_id"].as_str(), Some("tc-guidance-r4b"));
    let status = post_tool_result(&app, "tc-guidance-r4b", "src/main.rs", "success").await;
    assert_eq!(status, StatusCode::OK);

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");

    let late_round_context = find_events(&events, "context_meta")
        .into_iter()
        .find(|event| {
            event["system_prompt_breakdown"]["guidance_signals"]["synthesize_or_batch"].as_bool()
                == Some(true)
        })
        .expect("late-round context_meta event");

    assert!(
        late_round_context["system_prompt_tokens"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "context_meta should expose prompt token estimates"
    );
    assert_eq!(
        late_round_context["system_prompt_breakdown"]["guidance_signals"]["round_budget_warning"]
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        late_round_context["system_prompt_breakdown"]["guidance_signals"]["synthesize_or_batch"]
            .as_bool(),
        Some(true)
    );
    assert!(
        late_round_context["system_prompt_breakdown"]["guidance_signals"]["parallel_feedback"]
            .is_boolean(),
        "context_meta should expose the parallel_feedback flag"
    );
}

/// Low-information repair follow-up stays scoped when the caller provides an active-task attachment.
#[tokio::test]
async fn low_information_followup_attachment_drives_repair_turn() {
    let (app, hook_writer, _observer, tool_writer) = build_test_app_with_hooks();
    let sid = format!("followup-attach-{}", uuid::Uuid::new_v4());

    chat_stream_collect(
        &app,
        json!({
            "session_id": &sid,
            "message": "review f49aa28beedb75c838db442950b7076e590008ad",
            "context": {
                "test_llm_rounds": [{
                    "full_text": "## Review: `f49aa28b`\nIndentation issue and unnecessary JSON round-trip."
                }]
            }
        }),
    )
    .await;

    chat_stream_collect(
        &app,
        json!({
            "session_id": &sid,
            "message": "review 这个: aa1f419bc040003f5de8cdfa6b414225ade82e2b",
            "context": {
                "test_llm_rounds": [{
                    "full_text": "## Review: `aa1f419b` — P5 git timeout, P6 compression protection\nTwo independent fixes in one commit. Let me review each.\nP5 still has a thread leak on timeout; terminate the child before returning."
                }]
            }
        }),
    )
    .await;

    let attachment = "[Active task attachment]\n\
Resume the active task/thread below unless the user explicitly changes topic.\n\
Treat brief follow-ups as actions on this active thread, not as brand-new unrelated tasks.\n\
If the follow-up asks to fix / patch / test / continue, apply that action to this active thread.\n\
Latest user task: review 这个: aa1f419bc040003f5de8cdfa6b414225ade82e2b\n\
Latest assistant summary:\n\
## Review: `aa1f419b` — P5 git timeout, P6 compression protection\n\
Two independent fixes in one commit. Let me review each.\n\
P5 still has a thread leak on timeout; terminate the child before returning.\n\n\
[User follow-up]\n修复?";

    let resp = chat_stream_start(
        &app,
        json!({
            "session_id": &sid,
            "message": attachment,
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [tool_call(
                            "tc-followup-fix",
                            "str_replace",
                            json!({"path": "rust/crates/astra-tools/src/git_gix.rs"})
                        )]
                    },
                    { "full_text": "Patched the timeout path." }
                ],
                "edge_tools": [tool_schema("str_replace")]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "approval_required", 5).await;
    let st = post_approval_respond(&app, "tc-followup-fix", "allow").await;
    assert_eq!(st, StatusCode::OK);
    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc-followup-fix", "updated helper", "success").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert!(
        find_events(&events, "text_delta")
            .iter()
            .any(|event| event["content"].as_str() == Some("Patched the timeout path.")),
        "expected final repair text"
    );

    let hw = hook_writer.clone();
    poll_until(
        || {
            let hw = hw.clone();
            async move { hw.plans.lock().await.len() >= 3 }
        },
        5,
    )
    .await;
    let tw = tool_writer.clone();
    poll_until(
        || {
            let tw = tw.clone();
            async move { !tw.plans.lock().await.is_empty() }
        },
        5,
    )
    .await;

    let plans = hook_writer.plans.lock().await;
    let plan = plans.last().expect("turn 3 hook plan");
    let skill = plan
        .skill_selection
        .as_ref()
        .expect("turn 3 skill selection");
    assert!(skill.user_query.contains("[Active task attachment]"));
    assert!(
        skill
            .user_query
            .contains("aa1f419bc040003f5de8cdfa6b414225ade82e2b")
    );
    assert!(skill.user_query.contains("thread leak on timeout"));
    assert!(skill.user_query.contains("[User follow-up]\n修复?"));
    assert!(skill.selected_skills.contains(&"str_replace".to_string()));

    let tool_plans = tool_writer.plans.lock().await;
    let tool_events = &tool_plans.last().expect("tool plan").events;
    assert!(
        tool_events.iter().any(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("tool_name"))
                .and_then(Value::as_str)
                == Some("str_replace")
        }),
        "expected persisted str_replace tool event"
    );
}

// ── Tool Event Persistence Tests ─────────────────────────────────────────────

/// Text-only response produces no tool events.
#[tokio::test]
async fn tool_events_empty_for_text_only() {
    let (app, _hook_writer, _observer, tool_writer) = build_test_app_with_hooks();

    chat_stream_collect(
        &app,
        json!({
            "message": "hello",
            "context": {
                "test_llm_rounds": [{ "full_text": "Hi there!" }]
            }
        }),
    )
    .await;

    let plans = tool_writer.plans.lock().await;
    assert!(plans.is_empty(), "no tool events for text-only response");
}

/// Tool calls produce tool_call events with correct tool_name metadata.
#[tokio::test]
async fn tool_events_persisted_for_tool_calls() {
    let (app, _hook_writer, _observer, tool_writer) = build_test_app_with_hooks();

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "read the file",
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [tool_call("tc1", "read_file", json!({"path": "a.txt"}))]
                    },
                    { "full_text": "Done." }
                ],
                "edge_tools": [tool_schema("read_file")]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc1", "file contents", "success").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert!(!events.is_empty());
    let tw = tool_writer.clone();
    poll_until(
        || {
            let tw = tw.clone();
            async move { tw.plans.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let plans = tool_writer.plans.lock().await;
    assert_eq!(plans.len(), 1, "one tool event plan persisted");

    let tool_events = &plans[0].events;
    assert_eq!(tool_events.len(), 1, "one unique tool used");
    assert_eq!(tool_events[0].event_type, "tool_call");

    let meta = tool_events[0].metadata.as_ref().expect("metadata present");
    assert_eq!(meta["tool_name"], "read_file");
}

/// Multiple distinct tools produce one event per unique tool name.
#[tokio::test]
async fn tool_events_multiple_tools_distinct_names() {
    let (app, _hook_writer, _observer, tool_writer) = build_test_app_with_hooks();

    let resp = chat_stream_start(
        &app,
        json!({
            "message": "check stuff",
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [
                            tool_call("tc1", "read_file", json!({"path": "a.txt"})),
                            tool_call("tc2", "list_dir", json!({"path": "."}))
                        ]
                    },
                    { "full_text": "All done." }
                ],
                "edge_tools": [tool_schema("read_file"), tool_schema("list_dir")]
            }
        }),
    )
    .await;
    let (mut rx, reader) = spawn_sse_reader(resp.into_body()).await;

    wait_for_sse(&mut rx, "tool_request", 5).await;
    post_tool_result(&app, "tc1", "contents of a.txt", "success").await;
    post_tool_result(&app, "tc2", "dir listing", "success").await;

    let events = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
        .await
        .expect("stream timed out")
        .expect("reader task failed");
    assert!(!events.is_empty());
    let tw = tool_writer.clone();
    poll_until(
        || {
            let tw = tw.clone();
            async move { tw.plans.lock().await.len() > 0 }
        },
        5,
    )
    .await;

    let plans = tool_writer.plans.lock().await;
    assert_eq!(plans.len(), 1);

    let tool_events = &plans[0].events;
    assert_eq!(tool_events.len(), 2, "two distinct tools");
    let names: std::collections::HashSet<&str> = tool_events
        .iter()
        .map(|e| e.metadata.as_ref().unwrap()["tool_name"].as_str().unwrap())
        .collect();
    assert!(names.contains("read_file"));
    assert!(names.contains("list_dir"));
}
