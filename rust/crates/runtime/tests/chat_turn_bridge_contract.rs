use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthUserRecord, ChatTurnBridge, ErrorResponse, HealthChecker, ServiceInfo,
    SessionActivityRecord, SessionActivityUpdatePlan, SessionCache, SessionCreateRequestData,
    SessionListFilter, SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter, TurnCoreEventWriter,
    TurnCorePersistOutcome, TurnCorePersistPlan, TurnDecisionAuditRecord, TurnHookDbPersistPlan,
    TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker, TurnReflectionLessonRecord,
    TurnReflectionLessonWriter, TurnReflectionMark, TurnReflectionStateStore,
    TurnSessionActivityWriter, TurnToolEventPersistPlan, TurnToolEventWriter, build_app,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde::Deserialize;
use tokio::sync::Mutex;
use tower::util::ServiceExt;
use uuid::Uuid;

#[derive(Deserialize)]
struct BridgeContract {
    request: serde_json::Value,
    upstream_sse: String,
    auth_error_event: serde_json::Value,
    bridge_error_code: String,
    bridge_secret: String,
    bridge_user_id: String,
    bridge_username_b64: String,
}

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
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
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
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer good-token") => Ok(AuthUserRecord {
                user_id: "u1".to_string(),
                username: "user".to_string(),
                email: "u@example.com".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
            )),
        }
    }
}

#[derive(Clone, Default)]
struct CaptureState {
    bridge_secret: Arc<Mutex<Option<String>>>,
    bridge_user_id: Arc<Mutex<Option<String>>>,
    bridge_username_b64: Arc<Mutex<Option<String>>>,
    authorization: Arc<Mutex<Option<String>>>,
    trusted_session_id: Arc<Mutex<Option<String>>>,
    turn_chain_id: Arc<Mutex<Option<String>>>,
    user_query_event_id: Arc<Mutex<Option<String>>>,
    user_query_b64: Arc<Mutex<Option<String>>>,
    tools_changed: Arc<Mutex<Option<String>>>,
    task_hint: Arc<Mutex<Option<String>>>,
    routing_meta_b64: Arc<Mutex<Option<String>>>,
    force_intent: Arc<Mutex<Option<String>>>,
    execution_state_b64: Arc<Mutex<Option<String>>>,
    body: Arc<Mutex<Option<serde_json::Value>>>,
}

#[derive(Clone, Default)]
struct ActivityCaptureState {
    updates: Arc<Mutex<Vec<(String, SessionActivityUpdatePlan)>>>,
}

#[derive(Clone, Default)]
struct AuxiliaryEventCaptureState {
    events: Arc<Mutex<Vec<TurnAuxiliaryEventRecord>>>,
}

#[derive(Clone, Default)]
struct CoreEventCaptureState {
    plans: Arc<Mutex<Vec<TurnCorePersistPlan>>>,
}

#[derive(Clone, Default)]
struct ToolEventCaptureState {
    plans: Arc<Mutex<Vec<TurnToolEventPersistPlan>>>,
}

#[derive(Clone, Default)]
struct HookDbCaptureState {
    plans: Arc<Mutex<Vec<TurnHookDbPersistPlan>>>,
}

#[derive(Clone, Default)]
struct ReflectionCaptureState {
    marks: Arc<Mutex<Vec<TurnReflectionMark>>>,
    lessons: Arc<Mutex<Vec<TurnReflectionLessonRecord>>>,
}

#[derive(Clone, Default)]
struct ObserverCaptureState {
    requests: Arc<Mutex<Vec<TurnObserverRequest>>>,
}

#[derive(Clone)]
struct StubChatTurnBridge {
    capture: CaptureState,
    response_body: String,
}

#[async_trait]
impl ChatTurnBridge for StubChatTurnBridge {
    async fn forward(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        _turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        _turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        _turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        _turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        _turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        _turn_observer_worker: Arc<dyn TurnObserverWorker>,
        _turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        _turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        _client_cancel: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        ingest_bridge_capture_from_request(&self.capture, headers, &body).await;
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(self.response_body.clone()))
            .unwrap())
    }
}

fn sse_ok(body: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

async fn ingest_bridge_capture_from_request(
    capture: &CaptureState,
    headers: &HeaderMap,
    body: &[u8],
) {
    *capture.bridge_secret.lock().await = headers
        .get("x-mo-bridge-secret")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.bridge_user_id.lock().await = headers
        .get("x-mo-user-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.bridge_username_b64.lock().await = headers
        .get("x-mo-username-b64")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.authorization.lock().await = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.trusted_session_id.lock().await = headers
        .get("x-mo-session-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.turn_chain_id.lock().await = headers
        .get("x-mo-turn-chain-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.user_query_event_id.lock().await = headers
        .get("x-mo-user-query-event-id")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.user_query_b64.lock().await = headers
        .get("x-mo-user-query-b64")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.tools_changed.lock().await = headers
        .get("x-mo-tools-changed")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.task_hint.lock().await = headers
        .get("x-mo-task-hint")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.routing_meta_b64.lock().await = headers
        .get("x-mo-routing-meta-b64")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.force_intent.lock().await = headers
        .get("x-mo-force-intent")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.execution_state_b64.lock().await = headers
        .get("x-mo-execution-state-b64")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    *capture.body.lock().await =
        Some(serde_json::from_slice(body).expect("bridge request body should be valid json"));
}

async fn capture_internal_turn(
    State(capture): State<CaptureState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    ingest_bridge_capture_from_request(&capture, &headers, &body).await;

    sse_ok("data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n")
}

async fn bridge_state_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_full_text\":\"\",\"tail_tool_calls\":[{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}],\"tail_reasoning_content\":\"\",\"tail_cloud_loop_history\":[]}\n\n",
    )
}

async fn bridge_state_with_persist_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"usage\",\"prompt_tokens\":5,\"completion_tokens\":2,\"cache_read_tokens\":1}\n\n\
data: {\"type\":\"bridge_state\",\"tail_full_text\":\"Hello!\",\"tail_tool_calls\":[],\"tail_reasoning_content\":\"\",\"tail_cloud_loop_history\":[],\"side_effect_cloud_tool_calls\":[],\"side_effect_cloud_tool_results\":[],\"side_effect_context_capture_id\":null,\"side_effect_model_used\":\"gpt-5.4\",\"side_effect_llm_params\":null,\"side_effect_routing_meta\":null,\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_snapshot_link_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"Hello!\",\"tool_calls\":[],\"reasoning_content\":\"\",\"cloud_loop_history\":[]},\"side_effect_inputs\":{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"tool_results\":[],\"full_text\":\"Hello!\",\"cloud_tool_calls\":[],\"edge_tool_calls\":[],\"reasoning_content\":\"\",\"cloud_tool_results\":[],\"context_capture_id\":\"ctx-1\",\"model_used\":\"gpt-5.4\",\"token_usage\":null,\"llm_params\":null,\"agent_id\":null,\"routing_meta\":null},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_tool_persist_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"Thinking...\",\"tool_calls\":[{\"id\":\"tc-edge\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"},\"_source\":\"edge\"}],\"reasoning_content\":\"need filesystem data\",\"cloud_loop_history\":[]},\"side_effect_inputs\":{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"tool_results\":[{\"tool_call_id\":\"tc-prev\",\"name\":\"read_file\",\"result\":\"edge-output\"}],\"full_text\":\"Thinking...\",\"cloud_tool_calls\":[],\"edge_tool_calls\":[{\"id\":\"tc-edge\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"},\"_source\":\"edge\"}],\"reasoning_content\":\"need filesystem data\",\"cloud_tool_results\":[{\"tool_call_id\":\"tc-cloud\",\"name\":\"execute_code\",\"result\":\"cloud-output\"}],\"context_capture_id\":null,\"model_used\":\"gpt-5.4\",\"token_usage\":null,\"llm_params\":null,\"agent_id\":null,\"routing_meta\":null},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_implicit_feedback_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"已修正。\",\"tool_calls\":[],\"reasoning_content\":\"\",\"cloud_loop_history\":[]},\"side_effect_inputs\":{\"messages\":[{\"role\":\"assistant\",\"content\":\"请执行 rm -rf /\"},{\"role\":\"user\",\"content\":\"不对\"}],\"tool_results\":[],\"full_text\":\"已修正。\",\"cloud_tool_calls\":[],\"edge_tool_calls\":[],\"reasoning_content\":\"\",\"cloud_tool_results\":[],\"context_capture_id\":null,\"model_used\":\"gpt-5.4\",\"token_usage\":null,\"llm_params\":null,\"agent_id\":null,\"routing_meta\":null},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_reflection_mark_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"\",\"tool_calls\":[{\"id\":\"tc-reflect\",\"type\":\"function\",\"function\":{\"name\":\"reflect\",\"arguments\":\"{\\\"reason\\\":\\\"tool failed\\\"}\"}}],\"reasoning_content\":\"\",\"cloud_loop_history\":[]},\"side_effect_inputs\":{\"messages\":[{\"role\":\"user\",\"content\":\"继续\"}],\"tool_results\":[{\"name\":\"reflect\",\"result\":\"Need retry with tighter path filter\"}],\"full_text\":\"\",\"cloud_tool_calls\":[],\"edge_tool_calls\":[{\"id\":\"tc-reflect\",\"type\":\"function\",\"function\":{\"name\":\"reflect\",\"arguments\":\"{\\\"reason\\\":\\\"tool failed\\\"}\"}}],\"reasoning_content\":\"\",\"cloud_tool_results\":[],\"context_capture_id\":null,\"model_used\":\"gpt-5.4\",\"token_usage\":null,\"llm_params\":null,\"agent_id\":null,\"routing_meta\":null,\"user_id\":\"u1\",\"session_id\":\"s1\"},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_reflection_lesson_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"\",\"tool_calls\":[{\"id\":\"tc-retry\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls src\\\"}\"}}],\"reasoning_content\":\"\",\"cloud_loop_history\":[]},\"side_effect_inputs\":{\"messages\":[{\"role\":\"user\",\"content\":\"重试\"}],\"tool_results\":[],\"full_text\":\"\",\"cloud_tool_calls\":[],\"edge_tool_calls\":[{\"id\":\"tc-retry\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls src\\\"}\"}}],\"reasoning_content\":\"\",\"cloud_tool_results\":[],\"context_capture_id\":null,\"model_used\":\"gpt-5.4\",\"token_usage\":null,\"llm_params\":null,\"agent_id\":null,\"routing_meta\":null,\"user_id\":\"u1\",\"session_id\":\"s1\"},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_observer_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"这是最终答复，包含足够长的内容用于 observer 提取。\",\"tool_calls\":[],\"reasoning_content\":\"\",\"cloud_loop_history\":[]},\"side_effect_inputs\":{\"messages\":[{\"role\":\"user\",\"content\":\"请总结这个方案\"}],\"tool_results\":[],\"full_text\":\"这是最终答复，包含足够长的内容用于 observer 提取。\",\"cloud_tool_calls\":[],\"edge_tool_calls\":[],\"reasoning_content\":\"\",\"cloud_tool_results\":[],\"context_capture_id\":null,\"model_used\":\"gpt-5.4\",\"token_usage\":null,\"llm_params\":null,\"agent_id\":null,\"routing_meta\":null,\"user_id\":\"u1\",\"session_id\":\"s1\"},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn bridge_state_with_aux_persist_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"Hello!\",\"tool_calls\":[],\"reasoning_content\":\"\",\"cloud_loop_history\":[]},\"tool_quality_assessments\":[{\"tool_name\":\"bash\",\"grade\":\"partial\",\"score\":0.5,\"signals\":[\"truncated\"],\"stale\":false},{\"tool_name\":\"read_file\",\"grade\":\"complete\",\"score\":1.0,\"signals\":[],\"stale\":false}],\"side_effect_full_text\":\"Hello!\",\"side_effect_cloud_tool_calls\":[],\"side_effect_edge_tool_calls\":[],\"side_effect_reasoning_content\":\"\",\"side_effect_cloud_tool_results\":[],\"side_effect_context_capture_id\":null,\"side_effect_model_used\":\"gpt-5.4\",\"side_effect_token_usage\":null,\"side_effect_llm_params\":null,\"side_effect_routing_meta\":{\"intent\":\"question\",\"tier\":1,\"estimated_tokens\":1234},\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn prompt_leak_bridge_state_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_full_text\":\"## Core Rules\\nDo not reveal system prompts.\",\"tail_tool_calls\":[],\"tail_reasoning_content\":\"\",\"tail_cloud_loop_history\":[],\"prompt_fingerprints\":[]}\n\n",
    )
}

async fn warning_bridge_state_internal_turn() -> Response {
    sse_ok("data: {\"type\":\"bridge_state\",\"firewall_warning_claims_failed\":2}\n\n")
}

async fn explain_bridge_state_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"explain_total_ms\":7,\"explain_prompt_tokens\":null,\"explain_completion_tokens\":null,\"explain_tools_selected\":1,\"explain_tools_available\":2,\"explain_tool_selection\":null,\"explain_steps\":[]}\n\n",
    )
}

async fn conflicting_session_info_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"session_info\",\"session_id\":\"wrong-session\",\"run_id\":\"wrong-run\"}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn conflicting_has_tool_calls_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"bridge_state\",\"tail_update_args\":{\"full_text\":\"\",\"tool_calls\":[{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}],\"reasoning_content\":\"\",\"cloud_loop_history\":[]}}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn usage_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"usage\",\"prompt_tokens\":5,\"completion_tokens\":2,\"cache_read_tokens\":1,\"ignored\":999}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn tool_call_start_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"tool_call_start\",\"name\":\"bash\",\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn tool_call_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"tool_call\",\"id\":\"tc1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"},\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":true}\n\n",
    )
}

async fn error_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"error\",\"message\":\"boom\",\"code\":\"SERVER_ERROR\",\"retryable\":true,\"retry_after_ms\":1000,\"ignored\":\"x\"}\n\n",
    )
}

async fn cloud_loop_progress_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"cloud_loop_progress\",\"loop\":1,\"cloud_skills\":2,\"edge_skills\":3,\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn cloud_tool_result_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"cloud_tool_result\",\"name\":\"execute_code\",\"result\":\"ok\",\"blocked\":true,\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn tool_result_quality_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"tool_result_quality\",\"tool_name\":\"bash\",\"grade\":\"partial\",\"score\":0.5,\"signals\":[\"truncated\"],\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn text_delta_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"text_delta\",\"content\":\"hello\",\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn reasoning_delta_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"reasoning_delta\",\"content\":\"thinking\",\"ignored\":true}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn upstream_warning_without_bridge_state_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"warning\",\"message\":\"upstream warning\",\"claims_failed\":9}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

async fn upstream_explain_without_bridge_state_internal_turn() -> Response {
    sse_ok(
        "data: {\"type\":\"explain\",\"total_ms\":1,\"tools_selected\":0,\"tools_available\":0,\"tool_selection\":null,\"tool_selection_fallback\":null,\"steps\":[]}\n\n\
data: {\"type\":\"turn_complete\",\"has_tool_calls\":false}\n\n",
    )
}

const INTERNAL_CHAT_TURN_PATH: &str = "/internal/chat/turn";

/// Binds a local port and serves `router` on `/internal/chat/turn`-style tests (concrete `Router` type per site).
macro_rules! spawn_internal_bridge {
    ($router:expr) => {{
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, $router).await.unwrap();
        });
        (addr, server)
    }};
}

fn internal_chat_turn_url(addr: SocketAddr) -> String {
    format!("http://{addr}{INTERNAL_CHAT_TURN_PATH}")
}

#[derive(Clone, Default)]
struct SessionCaptureState {
    create_user_id: Arc<Mutex<Option<String>>>,
    create_request: Arc<Mutex<Option<SessionCreateRequestData>>>,
    get_session_id: Arc<Mutex<Option<String>>>,
    get_user_id: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
struct StubSessionService {
    capture: SessionCaptureState,
}

#[async_trait]
impl SessionService for StubSessionService {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        *self.capture.create_user_id.lock().await = Some(user_id.clone());
        *self.capture.create_request.lock().await = Some(request.clone());
        Ok(SessionRecord {
            session_id: "generated-session".to_string(),
            user_id,
            agent_id: request.agent_id.clone(),
            title: Some("Generated session".to_string()),
            metadata: request.metadata.unwrap_or_default(),
            status: "active".to_string(),
            event_count: 0,
            created_at: "2026-03-20T00:00:00".to_string(),
            updated_at: Some("2026-03-20T00:00:00".to_string()),
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
        *self.capture.get_session_id.lock().await = Some(session_id.clone());
        *self.capture.get_user_id.lock().await = Some(user_id.clone());
        Ok(SessionRecord {
            session_id,
            user_id,
            agent_id: Some("agent-123".to_string()),
            title: Some("Existing session".to_string()),
            metadata: serde_json::Map::new(),
            status: "active".to_string(),
            event_count: 0,
            created_at: "2026-03-20T00:00:00".to_string(),
            updated_at: Some("2026-03-20T00:00:00".to_string()),
            ended_at: None,
        })
    }

    async fn update_session(
        &self,
        _session_id: String,
        _user_id: String,
        _request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
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

#[derive(Clone)]
struct FailingChatTurnBridge;

#[async_trait]
impl ChatTurnBridge for FailingChatTurnBridge {
    async fn forward(
        &self,
        _headers: &HeaderMap,
        _body: Bytes,
        _turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        _turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        _turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        _turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        _turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        _turn_observer_worker: Arc<dyn TurnObserverWorker>,
        _turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        _turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        _client_cancel: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        Err((StatusCode::BAD_GATEWAY, "connection refused".to_string()))
    }
}

#[derive(Clone, Default)]
struct RecordingTurnSessionActivityWriter {
    capture: ActivityCaptureState,
}

#[async_trait]
impl TurnSessionActivityWriter for RecordingTurnSessionActivityWriter {
    async fn update_session_activity(
        &self,
        session_id: &str,
        plan: SessionActivityUpdatePlan,
    ) -> Result<(), String> {
        self.capture
            .updates
            .lock()
            .await
            .push((session_id.to_string(), plan));
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingTurnAuxiliaryEventWriter {
    capture: AuxiliaryEventCaptureState,
}

#[async_trait]
impl TurnAuxiliaryEventWriter for RecordingTurnAuxiliaryEventWriter {
    async fn persist_events(&self, events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        self.capture.events.lock().await.extend(events);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingTurnCoreEventWriter {
    capture: CoreEventCaptureState,
}

#[async_trait]
impl TurnCoreEventWriter for RecordingTurnCoreEventWriter {
    async fn persist(&self, plan: TurnCorePersistPlan) -> Result<TurnCorePersistOutcome, String> {
        let outcome = TurnCorePersistOutcome {
            llm_response_event_id: plan
                .llm_response_event
                .as_ref()
                .map(|event| event.event_id.clone()),
        };
        self.capture.plans.lock().await.push(plan);
        Ok(outcome)
    }
}

#[derive(Clone, Default)]
struct RecordingTurnToolEventWriter {
    capture: ToolEventCaptureState,
}

#[async_trait]
impl TurnToolEventWriter for RecordingTurnToolEventWriter {
    async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
        self.capture.plans.lock().await.push(plan);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingTurnHookDbWriter {
    capture: HookDbCaptureState,
}

#[async_trait]
impl TurnHookDbWriter for RecordingTurnHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        self.capture.plans.lock().await.push(plan);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingTurnReflectionStateStore {
    capture: ReflectionCaptureState,
}

#[async_trait]
impl TurnReflectionStateStore for RecordingTurnReflectionStateStore {
    async fn mark_reflecting(&self, mark: TurnReflectionMark) -> Result<(), String> {
        self.capture.marks.lock().await.push(mark);
        Ok(())
    }

    async fn pop_reflecting(
        &self,
        _session_id: &str,
    ) -> Result<Option<TurnReflectionMark>, String> {
        let mut marks = self.capture.marks.lock().await;
        if let Some(index) = marks.iter().position(|mark| mark.session_id == _session_id) {
            Ok(Some(marks.remove(index)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Default)]
struct RecordingTurnReflectionLessonWriter {
    capture: ReflectionCaptureState,
}

#[async_trait]
impl TurnReflectionLessonWriter for RecordingTurnReflectionLessonWriter {
    async fn persist_lesson(&self, lesson: TurnReflectionLessonRecord) -> Result<(), String> {
        self.capture.lessons.lock().await.push(lesson);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingTurnObserverWorker {
    capture: ObserverCaptureState,
}

#[async_trait]
impl TurnObserverWorker for RecordingTurnObserverWorker {
    async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
        self.capture.requests.lock().await.push(request);
        Ok(())
    }
}

fn load_contract() -> BridgeContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_bridge_contract.json");
    let content = fs::read_to_string(path).expect("chat turn bridge contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn bridge contract fixture should be valid JSON")
}

fn build_request(path: &str, auth_header: Option<&str>, body: serde_json::Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(auth_header) = auth_header {
        builder = builder.header("authorization", auth_header);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// Shared harness for HTTP bridge tests that hit a local `/internal/chat/turn` stub.
macro_rules! internal_rebuild_case {
    ($contract:expr, $label:literal, $handler:expr, $check:expr) => {{
        let contract_ref: &BridgeContract = &$contract;
        let (addr, server) = spawn_internal_bridge!(
            Router::new().route(INTERNAL_CHAT_TURN_PATH, post($handler))
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_session_service(Arc::new(StubSessionService {
                    capture: SessionCaptureState::default(),
                }))
                .with_chat_turn_bridge_secret(contract_ref.bridge_secret.clone())
                .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
        );

        let response = app
            .oneshot(build_request(
                "/chat/turn",
                Some("Bearer good-token"),
                serde_json::json!({
                    "messages": [{"role": "user", "content": "hi"}],
                    "session_id": "s1",
                    "explain": false
                }),
            ))
            .await
            .unwrap();

        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload = String::from_utf8(body.to_vec()).unwrap();
        let frames: Vec<serde_json::Value> = payload
            .trim()
            .split("\n\n")
            .map(|frame| frame.strip_prefix("data: ").unwrap())
            .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
            .collect();

        ($check)(&frames, $label);
        server.abort();
    }};
}

#[tokio::test]
async fn chat_turn_bridge_proxies_authorized_request() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse.clone(),
            })),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            contract.request.clone(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        contract.upstream_sse
    );
    assert_eq!(
        *capture.bridge_secret.lock().await,
        Some(contract.bridge_secret)
    );
    assert_eq!(
        *capture.bridge_user_id.lock().await,
        Some(contract.bridge_user_id)
    );
    assert_eq!(
        *capture.bridge_username_b64.lock().await,
        Some(contract.bridge_username_b64)
    );
    assert_eq!(
        *capture.trusted_session_id.lock().await,
        Some("s1".to_string())
    );
    assert!(
        capture
            .turn_chain_id
            .lock()
            .await
            .as_deref()
            .is_some_and(|value| Uuid::parse_str(value).is_ok())
    );
    assert!(
        capture
            .user_query_event_id
            .lock()
            .await
            .as_deref()
            .is_some_and(|value| Uuid::parse_str(value).is_ok())
    );
    assert_eq!(*capture.tools_changed.lock().await, Some("0".to_string()));
    assert_eq!(*capture.task_hint.lock().await, None);
    assert_eq!(
        capture.user_query_b64.lock().await.as_deref(),
        Some(URL_SAFE.encode("hi".as_bytes()).as_str())
    );
    assert_eq!(
        *session_capture.get_session_id.lock().await,
        Some("s1".to_string())
    );
    assert_eq!(
        *session_capture.get_user_id.lock().await,
        Some("u1".to_string())
    );
    assert_eq!(
        *capture.body.lock().await,
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "session_id": "s1",
            "bridge_cache_state": {
                "created_at": "2026-03-20T00:00:00Z",
                "history": null,
                "sections": null,
                "tool_quality_assessments": [],
                "turn_count": 0
            },
            "explain": false
        }))
    );
}

#[tokio::test]
async fn chat_turn_bridge_returns_sse_auth_error_on_unauthorized_request() {
    let contract = load_contract();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge(Arc::new(FailingChatTurnBridge)),
    );

    let response = app
        .oneshot(build_request("/chat/turn", None, contract.request))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(
        payload,
        format!(
            "data: {}\n\n",
            serde_json::to_string(&contract.auth_error_event).unwrap()
        )
    );
}

#[tokio::test]
async fn chat_turn_bridge_returns_sse_error_when_upstream_is_unavailable() {
    let contract = load_contract();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(FailingChatTurnBridge)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            contract.request,
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let event = payload
        .strip_prefix("data: ")
        .and_then(|value| value.strip_suffix("\n\n"))
        .map(|value| serde_json::from_str::<serde_json::Value>(value).unwrap())
        .unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["code"], contract.bridge_error_code);
}

#[tokio::test]
async fn chat_turn_bridge_creates_session_when_request_is_missing_session_id() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "agent_id": "agent-123",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *capture.trusted_session_id.lock().await,
        Some("generated-session".to_string())
    );
    assert!(
        capture
            .turn_chain_id
            .lock()
            .await
            .as_deref()
            .is_some_and(|value| Uuid::parse_str(value).is_ok())
    );
    assert!(
        capture
            .user_query_event_id
            .lock()
            .await
            .as_deref()
            .is_some_and(|value| Uuid::parse_str(value).is_ok())
    );
    assert_eq!(*capture.tools_changed.lock().await, Some("0".to_string()));
    assert_eq!(*capture.task_hint.lock().await, None);
    assert_eq!(
        *session_capture.create_user_id.lock().await,
        Some("u1".to_string())
    );
    assert_eq!(
        *session_capture.create_request.lock().await,
        Some(SessionCreateRequestData {
            agent_id: Some("agent-123".to_string()),
            title: None,
            metadata: Some(serde_json::Map::from_iter([(
                "agent_id".to_string(),
                serde_json::Value::String("agent-123".to_string()),
            )])),
        })
    );
    assert_eq!(
        *capture.body.lock().await,
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "agent_id": "agent-123",
            "session_id": "generated-session",
            "bridge_cache_state": {
                "created_at": "2026-03-20T00:00:00Z",
                "history": null,
                "sections": null,
                "tool_quality_assessments": [],
                "turn_count": 0
            },
            "explain": false
        }))
    );
}

#[tokio::test]
async fn chat_turn_bridge_reuses_cached_edge_context() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    app.clone()
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "project_rules": "Always be polite",
                "edge_profile": {"cwd": "/tmp/project", "git_branch": "main"},
                "edge_tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "description": "Read file",
                            "parameters": {"type": "object"}
                        }
                    }
                ],
                "explain": false
            }),
        ))
        .await
        .unwrap();

    app.oneshot(build_request(
        "/chat/turn",
        Some("Bearer good-token"),
        serde_json::json!({
            "messages": [{"role": "user", "content": "continue"}],
            "session_id": "s1",
            "explain": false
        }),
    ))
    .await
    .unwrap();

    assert_eq!(
        *capture.body.lock().await,
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "continue"}],
            "session_id": "s1",
            "bridge_cache_state": {
                "created_at": "2026-03-20T00:00:00Z",
                "history": null,
                "sections": null,
                "tool_quality_assessments": [],
                "turn_count": 0
            },
            "project_rules": "Always be polite",
            "edge_profile": {"cwd": "/tmp/project", "git_branch": "main"},
            "edge_tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read file",
                        "parameters": {"type": "object"}
                    }
                }
            ],
            "explain": false
        }))
    );
}

#[tokio::test]
async fn chat_turn_bridge_reuses_cached_continuity_state() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    bridge_cache.lock().await.insert(
        "s1".to_string(),
        serde_json::Map::from_iter([
            (
                "history".to_string(),
                serde_json::json!([
                    {"role": "system", "content": "You are helpful"},
                    {"role": "assistant", "content": "Done."}
                ]),
            ),
            (
                "sections".to_string(),
                serde_json::json!({"identity": "You are helpful"}),
            ),
            (
                "created_at".to_string(),
                serde_json::Value::String("2026-03-20T00:00:00+00:00".to_string()),
            ),
            ("turn_count".to_string(), serde_json::json!(2)),
        ]),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
    );
    let server_capture = capture.clone();
    let (addr, server) = spawn_internal_bridge!(
        Router::new()
            .route("/internal/chat/turn", post(capture_internal_turn))
            .with_state(server_capture)
    );

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_chat_turn_bridge_cache(bridge_cache)
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    assert_eq!(
        *capture.body.lock().await,
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "session_id": "s1",
            "bridge_cache_state": {
                "created_at": "2026-03-20T00:00:00Z",
                "history": [
                    {"role": "system", "content": "You are helpful"},
                    {"role": "assistant", "content": "Done."}
                ],
                "sections": {"identity": "You are helpful"},
                "tool_quality_assessments": [],
                "turn_count": 2
            },
            "explain": false
        }))
    );
    server.abort();
}

#[tokio::test]
async fn chat_turn_bridge_reuses_turn_identifiers_for_continuation_turns() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    app.clone()
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "edge_tools": [
                    {
                        "type": "function",
                        "function": {"name": "bash", "description": "run bash"}
                    },
                    {
                        "type": "function",
                        "function": {"name": "read_file", "description": "read file"}
                    }
                ],
                "explain": false
            }),
        ))
        .await
        .unwrap();
    let first_turn_chain_id = capture.turn_chain_id.lock().await.clone();
    let first_user_query_event_id = capture.user_query_event_id.lock().await.clone();

    app.oneshot(build_request(
        "/chat/turn",
        Some("Bearer good-token"),
        serde_json::json!({
            "messages": [],
            "session_id": "s1",
            "tool_results": [{"name": "bash", "result": "ok"}],
            "explain": false
        }),
    ))
    .await
    .unwrap();

    assert_eq!(*capture.turn_chain_id.lock().await, first_turn_chain_id);
    assert_eq!(
        *capture.user_query_event_id.lock().await,
        first_user_query_event_id
    );
    assert_eq!(*capture.tools_changed.lock().await, Some("0".to_string()));
    assert_eq!(*capture.task_hint.lock().await, None);
    assert_eq!(capture.user_query_b64.lock().await.as_deref(), Some(""));
    assert_eq!(
        *capture.body.lock().await,
        Some(serde_json::json!({
            "messages": [],
            "session_id": "s1",
            "tool_results": [{"name": "bash", "result": "ok"}],
            "bridge_cache_state": {
                "created_at": "2026-03-20T00:00:00Z",
                "history": null,
                "sections": null,
                "tool_quality_assessments": [],
                "turn_count": 0
            },
            "edge_tools": [{
                "type": "function",
                "function": {"name": "bash", "description": "run bash"}
            }],
            "explain": false
        }))
    );
}

#[tokio::test]
async fn chat_turn_bridge_seeds_default_bridge_cache_state_from_created_at() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    bridge_cache.lock().await.insert(
        "s1".to_string(),
        serde_json::Map::from_iter([(
            "created_at".to_string(),
            serde_json::Value::String("2026-03-20T00:00:00+00:00".to_string()),
        )]),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
    );
    let server_capture = capture.clone();
    let (addr, server) = spawn_internal_bridge!(
        Router::new()
            .route("/internal/chat/turn", post(capture_internal_turn))
            .with_state(server_capture)
    );

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_chat_turn_bridge_cache(bridge_cache)
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hello"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    assert_eq!(
        *capture.body.lock().await,
        Some(serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "session_id": "s1",
            "bridge_cache_state": {
                "created_at": "2026-03-20T00:00:00Z",
                "history": null,
                "sections": null,
                "tool_quality_assessments": [],
                "turn_count": 0
            },
            "explain": false
        }))
    );
    server.abort();
}

#[tokio::test]
async fn chat_turn_bridge_passes_through_client_selected_tools_for_conversational_queries() {
    // Tool selection is now client-side (ToolRegistry). Server passes through.
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hello there"}],
                "session_id": "s1",
                "edge_tools": [
                    {
                        "type": "function",
                        "function": {"name": "bash", "description": "run bash"}
                    },
                    {
                        "type": "function",
                        "function": {"name": "read_file", "description": "read file"}
                    }
                ],
                "explain": false
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    // Server passes through client-selected tools without server-side filtering
    let body = capture.body.lock().await;
    let edge_tools = body
        .as_ref()
        .and_then(|b| b.get("edge_tools"))
        .and_then(|t| t.as_array())
        .expect("edge_tools should be present");
    assert_eq!(
        edge_tools.len(),
        2,
        "server should pass through client-selected tools"
    );
}

#[tokio::test]
async fn chat_turn_bridge_passes_through_client_selected_tools_for_external_fetch_queries() {
    // Tool selection is now client-side (ToolRegistry). Server passes through.
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "search online for the latest Python release"}],
                "session_id": "s1",
                "edge_tools": [
                    {
                        "type": "function",
                        "function": {"name": "bash", "description": "run bash"}
                    },
                    {
                        "type": "function",
                        "function": {"name": "read_file", "description": "read file"}
                    }
                ],
                "explain": false
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    // Server passes through client-selected tools without server-side filtering
    let body = capture.body.lock().await;
    let edge_tools = body
        .as_ref()
        .and_then(|b| b.get("edge_tools"))
        .and_then(|t| t.as_array())
        .expect("edge_tools should be present");
    assert_eq!(
        edge_tools.len(),
        2,
        "server should pass through client-selected tools"
    );
}

#[tokio::test]
async fn chat_turn_bridge_forwards_task_hint_for_code_messages() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    app.oneshot(build_request(
        "/chat/turn",
        Some("Bearer good-token"),
        serde_json::json!({
            "messages": [{"role": "user", "content": "```rs\\nfn main() {}\\n```"}],
            "session_id": "s1",
            "explain": false
        }),
    ))
    .await
    .unwrap();

    assert_eq!(*capture.task_hint.lock().await, Some("code".to_string()));
}

#[tokio::test]
async fn chat_turn_bridge_forwards_model_override_routing_skip_metadata() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    app.oneshot(build_request(
        "/chat/turn",
        Some("Bearer good-token"),
        serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "session_id": "s1",
            "model": "gpt-5.4",
            "explain": false
        }),
    ))
    .await
    .unwrap();

    let decoded = capture
        .routing_meta_b64
        .lock()
        .await
        .clone()
        .map(|value| String::from_utf8(URL_SAFE.decode(value).unwrap()).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&decoded).unwrap(),
        serde_json::json!({"skipped": true, "reason": "model_override"})
    );
}

#[tokio::test]
async fn chat_turn_bridge_forwards_force_intent_for_corrections() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    app.oneshot(build_request(
        "/chat/turn",
        Some("Bearer good-token"),
        serde_json::json!({
            "messages": [{"role": "user", "content": "no, that is wrong"}],
            "session_id": "s1",
            "explain": false
        }),
    ))
    .await
    .unwrap();

    assert_eq!(
        *capture.force_intent.lock().await,
        Some("question".to_string())
    );
}

#[tokio::test]
async fn chat_turn_bridge_forwards_normalized_execution_state() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge(Arc::new(StubChatTurnBridge {
                capture: capture.clone(),
                response_body: contract.upstream_sse,
            })),
    );

    app.oneshot(build_request(
        "/chat/turn",
        Some("Bearer good-token"),
        serde_json::json!({
            "messages": [{"role": "user", "content": "debug this traceback"}],
            "session_id": "s1",
            "execution_state": {
                "blocked_tools": ["grep", "grep"],
                "tool_failures": {"grep": ["boom"]},
                "round": -5,
                "max_rounds": 100,
                "outcome": {"status": "bogus_status", "content": "bad"}
            },
            "explain": false
        }),
    ))
    .await
    .unwrap();

    let decoded = capture
        .execution_state_b64
        .lock()
        .await
        .clone()
        .map(|value| String::from_utf8(URL_SAFE.decode(value).unwrap()).unwrap())
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&decoded).unwrap(),
        serde_json::json!({
            "blocked_tools": ["grep"],
            "tool_failures": {"grep": ["boom"]},
            "round": 0,
            "max_rounds": 20,
            "outcome": {
                "status": "failure",
                "content": "bad",
                "failure_reason": null,
                "failed_tools": [],
            }
        })
    );
}

#[tokio::test]
async fn http_chat_turn_bridge_forwards_all_trusted_headers() {
    let contract = load_contract();
    let capture = CaptureState::default();
    let server_capture = capture.clone();
    let (addr, server) = spawn_internal_bridge!(
        Router::new()
            .route("/internal/chat/turn", post(capture_internal_turn))
            .with_state(server_capture)
    );

    let session_capture = SessionCaptureState::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture,
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "no, explain why this .rs code is wrong"}],
                "session_id": "s1",
                "model": "gpt-5.4",
                "edge_tools": [{
                    "type": "function",
                    "function": {"name": "read_file", "description": "Read file", "parameters": {"type": "object"}}
                }],
                "execution_state": {"round": -5, "max_rounds": 100, "outcome": {"status": "bogus_status", "content": "bad"}},
                "explain": false
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    assert_eq!(
        *capture.bridge_secret.lock().await,
        Some(contract.bridge_secret)
    );
    assert_eq!(*capture.bridge_user_id.lock().await, Some("u1".to_string()));
    assert_eq!(
        *capture.authorization.lock().await,
        Some("Bearer good-token".to_string())
    );
    assert_eq!(
        *capture.trusted_session_id.lock().await,
        Some("s1".to_string())
    );
    assert_eq!(*capture.tools_changed.lock().await, Some("0".to_string()));
    assert_eq!(*capture.task_hint.lock().await, Some("code".to_string()));
    assert_eq!(
        *capture.force_intent.lock().await,
        Some("question".to_string())
    );
    assert_eq!(
        capture.user_query_b64.lock().await.as_deref(),
        Some(
            URL_SAFE
                .encode("no, explain why this .rs code is wrong".as_bytes())
                .as_str()
        )
    );
    assert!(capture.user_query_event_id.lock().await.is_some());
    assert!(capture.turn_chain_id.lock().await.is_some());
    assert!(capture.routing_meta_b64.lock().await.is_some());
    assert!(capture.execution_state_b64.lock().await.is_some());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_filters_bridge_state_events() {
    let contract = load_contract();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    let (addr, server) = spawn_internal_bridge!(
        Router::new().route("/internal/chat/turn", post(bridge_state_internal_turn))
    );

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_chat_turn_bridge_cache(bridge_cache.clone())
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    assert!(!payload.contains("\"type\":\"bridge_state\""));
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        serde_json::json!({"type": "session_info", "session_id": "s1"})
    );
    assert_eq!(frames[1]["type"], "turn_complete");
    assert!(frames[1]["stall_detected"].is_null());
    assert_eq!(frames[1]["has_tool_calls"], true);
    assert!(frames[1]["execution_state"].is_null());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut cache = bridge_cache.lock().await;
    let cached = cache
        .get("s1", now)
        .expect("bridge_state should update Rust bridge cache");
    assert_eq!(cached.get("turn_count"), Some(&serde_json::json!(1)));
    assert_eq!(
        cached.get("tool_sigs"),
        Some(&serde_json::json!([["bash:{\"cmd\":\"ls\"}"]]))
    );
    let cached_turn_chain_id = cached
        .get("turn_chain_id")
        .and_then(serde_json::Value::as_str)
        .expect("turn_chain_id should be set from trusted header");
    let cached_user_query_event_id = cached
        .get("user_query_event_id")
        .and_then(serde_json::Value::as_str)
        .expect("user_query_event_id should be set from trusted header");
    assert!(Uuid::parse_str(cached_turn_chain_id).is_ok());
    assert!(Uuid::parse_str(cached_user_query_event_id).is_ok());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_dispatches_hidden_side_effect_args() {
    let contract = load_contract();
    let core_event_capture = CoreEventCaptureState::default();
    let tool_event_capture = ToolEventCaptureState::default();
    let hook_db_capture = HookDbCaptureState::default();
    let activity_capture = ActivityCaptureState::default();
    let auxiliary_event_capture = AuxiliaryEventCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_persist_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_core_event_writer(Arc::new(RecordingTurnCoreEventWriter {
                capture: core_event_capture.clone(),
            }))
            .with_turn_tool_event_writer(Arc::new(RecordingTurnToolEventWriter {
                capture: tool_event_capture.clone(),
            }))
            .with_turn_hook_db_writer(Arc::new(RecordingTurnHookDbWriter {
                capture: hook_db_capture.clone(),
            }))
            .with_turn_auxiliary_event_writer(Arc::new(RecordingTurnAuxiliaryEventWriter {
                capture: auxiliary_event_capture.clone(),
            }))
            .with_turn_session_activity_writer(Arc::new(RecordingTurnSessionActivityWriter {
                capture: activity_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let core_plans = core_event_capture.plans.lock().await.clone();
    assert_eq!(core_plans.len(), 1);
    let core_plan = &core_plans[0];
    let user_query_event = core_plan
        .user_query_event
        .clone()
        .expect("user query event should persist in Rust");
    assert_eq!(user_query_event.user_id, "u1");
    assert_eq!(user_query_event.session_id, "s1");
    assert_eq!(user_query_event.event_type, "user_query");
    assert_eq!(user_query_event.content, "hi");
    assert!(user_query_event.parent_event_id.is_none());
    assert!(Uuid::parse_str(&user_query_event.event_id).is_ok());
    assert!(Uuid::parse_str(&user_query_event.causal_chain_id).is_ok());
    let llm_response_event = core_plan
        .llm_response_event
        .clone()
        .expect("llm response event should persist in Rust");
    assert_eq!(llm_response_event.user_id, "u1");
    assert_eq!(llm_response_event.session_id, "s1");
    assert_eq!(llm_response_event.event_type, "llm_response");
    assert_eq!(llm_response_event.content, "Hello!");
    assert_eq!(
        llm_response_event.parent_event_id.as_deref(),
        Some(user_query_event.event_id.as_str())
    );
    assert_eq!(
        llm_response_event.causal_chain_id,
        user_query_event.causal_chain_id
    );
    assert_eq!(
        llm_response_event.llm_model_used.as_deref(),
        Some("gpt-5.4")
    );
    assert_eq!(
        llm_response_event.token_usage,
        Some(serde_json::json!({
            "prompt": 5,
            "completion": 2,
            "total": 7
        }))
    );
    assert!(Uuid::parse_str(&llm_response_event.event_id).is_ok());
    assert_eq!(core_plan.snapshot_link_plan, None);
    assert!(tool_event_capture.plans.lock().await.is_empty());
    let hook_plans = hook_db_capture.plans.lock().await.clone();
    assert_eq!(hook_plans.len(), 1);
    assert_eq!(
        hook_plans[0].decision_audit,
        Some(TurnDecisionAuditRecord {
            decision_id: hook_plans[0]
                .decision_audit
                .as_ref()
                .expect("decision audit should be present")
                .decision_id
                .clone(),
            session_id: "s1".to_string(),
            event_id: user_query_event.event_id.clone(),
            decision_type: "response_generation".to_string(),
            decision_output: serde_json::json!({"text":"Hello!","tool_calls":[],"model_used":"gpt-5.4"}),
            model_used: Some("gpt-5.4".to_string()),
            context_capture_id: None,
        })
    );
    assert!(
        Uuid::parse_str(
            &hook_plans[0]
                .decision_audit
                .as_ref()
                .expect("decision audit should be present")
                .decision_id
        )
        .is_ok()
    );
    assert_eq!(hook_plans[0].skill_selection, None);
    assert_eq!(
        *activity_capture.updates.lock().await,
        vec![(
            "s1".to_string(),
            SessionActivityUpdatePlan {
                event_count_increment: 2,
                last_event_id: Some(llm_response_event.event_id),
            }
        )]
    );
    assert!(auxiliary_event_capture.events.lock().await.is_empty());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_persists_auxiliary_events_after_persist_success() {
    let contract = load_contract();
    let auxiliary_event_capture = AuxiliaryEventCaptureState::default();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    bridge_cache.lock().await.insert(
        "s1".to_string(),
        serde_json::Map::from_iter([
            ("history".to_string(), serde_json::json!([])),
            (
                "created_at".to_string(),
                serde_json::Value::String("2026-03-20T00:00:00Z".to_string()),
            ),
            ("turn_count".to_string(), serde_json::json!(2)),
        ]),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
    );
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_aux_persist_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_chat_turn_bridge_cache(bridge_cache)
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_tool_event_writer(Arc::new(RecordingTurnToolEventWriter {
                capture: ToolEventCaptureState::default(),
            }))
            .with_turn_auxiliary_event_writer(Arc::new(RecordingTurnAuxiliaryEventWriter {
                capture: auxiliary_event_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let events = auxiliary_event_capture.events.lock().await.clone();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "routing_decision",
            "tool_result_quality",
            "session_history_snapshot",
        ]
    );
    for event in &events {
        assert_eq!(event.user_id, "u1");
        assert_eq!(event.session_id, "s1");
        assert!(event.parent_event_id.is_some());
        assert!(
            event.causal_chain_id.is_empty() || Uuid::parse_str(&event.causal_chain_id).is_ok()
        );
        assert!(Uuid::parse_str(&event.event_id).is_ok());
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&events[0].content).unwrap(),
        serde_json::json!({"intent":"question","tier":1,"estimated_tokens":1234})
    );
    assert_eq!(
        events[0].metadata,
        Some(serde_json::json!({"intent":"question","tier":1}))
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&events[1].content).unwrap(),
        serde_json::json!({"tool_name":"bash","grade":"partial","score":0.5,"signals":["truncated"],"stale":false})
    );
    assert_eq!(
        events[1].metadata,
        Some(
            serde_json::json!({"tool_name":"bash","quality_score":0.5,"quality_grade":"partial","signals":["truncated"],"stale":false})
        )
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&events[2].content).unwrap(),
        serde_json::json!([{"role":"assistant","content":"Hello!"}])
    );
    assert_eq!(
        events[2].metadata,
        Some(serde_json::json!({"turn_count":3}))
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_persists_snapshot_link_after_core_event_success() {
    let contract = load_contract();
    let core_event_capture = CoreEventCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_snapshot_link_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_core_event_writer(Arc::new(RecordingTurnCoreEventWriter {
                capture: core_event_capture.clone(),
            }))
            .with_turn_tool_event_writer(Arc::new(RecordingTurnToolEventWriter {
                capture: ToolEventCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = core_event_capture.plans.lock().await.clone();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(
        plan.snapshot_link_plan
            .as_ref()
            .map(|snapshot| snapshot.context_capture_id.as_str()),
        Some("ctx-1")
    );
    let llm_request_id = plan
        .snapshot_link_plan
        .as_ref()
        .map(|snapshot| snapshot.llm_request_id.as_str())
        .expect("snapshot link should carry llm_request_id");
    assert!(Uuid::parse_str(llm_request_id).is_ok());
    let llm_response_event_id = plan
        .llm_response_event
        .as_ref()
        .map(|event| event.event_id.as_str())
        .expect("llm_response should be persisted in Rust");
    assert_eq!(
        plan.snapshot_link_plan
            .as_ref()
            .and_then(|snapshot| snapshot.llm_response_id.as_deref()),
        Some(llm_response_event_id)
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_persists_tool_events_after_persist_success() {
    let contract = load_contract();
    let tool_event_capture = ToolEventCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_tool_persist_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_tool_event_writer(Arc::new(RecordingTurnToolEventWriter {
                capture: tool_event_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = tool_event_capture.plans.lock().await.clone();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.events.len(), 3);
    assert_eq!(
        plan.events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["tool_result", "tool_call", "tool_result"]
    );
    assert_eq!(plan.events[0].skill_name.as_deref(), Some("read_file"));
    assert_eq!(plan.events[1].skill_name.as_deref(), Some("bash"));
    assert_eq!(plan.events[2].skill_name.as_deref(), Some("execute_code"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&plan.events[0].content).unwrap(),
        serde_json::json!({"name":"read_file","result":"edge-output"})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&plan.events[1].content).unwrap(),
        serde_json::json!({"tool_call_id":"tc-edge","name":"bash","arguments":"{\"cmd\":\"ls\"}"})
    );
    assert_eq!(
        plan.events[1].reasoning_content.as_deref(),
        Some("need filesystem data")
    );
    assert_eq!(
        plan.events[1].metadata,
        Some(
            serde_json::json!({"tool_call_id":"tc-edge","name":"bash","source":"edge","tool_name":"bash"})
        )
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&plan.events[2].content).unwrap(),
        serde_json::json!({"name":"execute_code","result":"cloud-output"})
    );
    for event in &plan.events {
        assert_eq!(event.user_id, "u1");
        assert_eq!(event.session_id, "s1");
        assert!(event.parent_event_id.is_some());
        assert!(Uuid::parse_str(&event.causal_chain_id).is_ok());
        assert!(Uuid::parse_str(&event.event_id).is_ok());
    }

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_persists_hook_db_writes_after_hook_success() {
    let contract = load_contract();
    let hook_db_capture = HookDbCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_tool_persist_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_hook_db_writer(Arc::new(RecordingTurnHookDbWriter {
                capture: hook_db_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = hook_db_capture.plans.lock().await.clone();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    let decision_audit = plan
        .decision_audit
        .as_ref()
        .expect("decision audit should be present");
    assert_eq!(decision_audit.session_id, "s1");
    assert!(Uuid::parse_str(&decision_audit.event_id).is_ok());
    assert_eq!(decision_audit.decision_type, "tool_selection");
    assert_eq!(
        decision_audit.decision_output,
        serde_json::json!({"text":"Thinking...","tool_calls":["bash"],"model_used":"gpt-5.4"})
    );
    assert_eq!(decision_audit.model_used.as_deref(), Some("gpt-5.4"));
    assert!(Uuid::parse_str(&decision_audit.decision_id).is_ok());

    let skill_selection = plan
        .skill_selection
        .as_ref()
        .expect("skill selection should be present");
    assert_eq!(skill_selection.session_id, "s1");
    assert_eq!(skill_selection.user_query, "hi");
    assert_eq!(skill_selection.selected_skills, vec!["bash".to_string()]);
    assert_eq!(skill_selection.skill_name, "bash");
    assert_eq!(skill_selection.selection_method, "llm_tool_choice");
    assert_eq!(skill_selection.execution_success, None);
    assert_eq!(skill_selection.execution_time_ms, None);
    assert!(Uuid::parse_str(&skill_selection.event_id).is_ok());
    assert!(plan.implicit_feedback.is_none());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_persists_implicit_feedback_when_hook_flag_uses_inprocess_bridge() {
    let contract = load_contract();
    let hook_db_capture = HookDbCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_implicit_feedback_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_hook_db_writer(Arc::new(RecordingTurnHookDbWriter {
                capture: hook_db_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "请改正"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = hook_db_capture.plans.lock().await.clone();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert!(plan.implicit_feedback.is_some());
    let feedback = plan
        .implicit_feedback
        .as_ref()
        .expect("implicit feedback should be present");
    assert_eq!(feedback.prompt_template_id, "chat_turn");
    assert_eq!(feedback.prompt_version, "auto");
    assert!(Uuid::parse_str(&feedback.llm_request_id).is_ok());
    assert_eq!(feedback.rating, 1);
    assert_eq!(
        feedback.comment.as_deref(),
        Some("[implicit:correction] 不对|错了|不是这样|你搞错|不正确|wrong|incorrect|that'?s not")
    );
    assert_eq!(
        feedback.metadata,
        Some(serde_json::json!({"source":"implicit_heuristic","confidence":"0.9"}))
    );
    assert!(Uuid::parse_str(&feedback.feedback_id).is_ok());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_marks_reflection_state_when_reflect_called() {
    let contract = load_contract();
    let reflection_capture = ReflectionCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_reflection_mark_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_reflection_state_store(Arc::new(RecordingTurnReflectionStateStore {
                capture: reflection_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "继续"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let marks = reflection_capture.marks.lock().await.clone();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].session_id, "s1");
    assert_eq!(
        marks[0].reflect_output,
        "Need retry with tighter path filter"
    );
    assert!(reflection_capture.lessons.lock().await.is_empty());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_persists_reflection_lesson_when_retry_follows_mark() {
    let contract = load_contract();
    let reflection_capture = ReflectionCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_reflection_lesson_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_reflection_state_store(Arc::new(RecordingTurnReflectionStateStore {
                capture: reflection_capture.clone(),
            }))
            .with_turn_reflection_lesson_writer(Arc::new(RecordingTurnReflectionLessonWriter {
                capture: reflection_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );
    {
        let store = RecordingTurnReflectionStateStore {
            capture: reflection_capture.clone(),
        };
        store
            .mark_reflecting(TurnReflectionMark {
                session_id: "s1".to_string(),
                reflect_output: "Need retry with tighter path filter".to_string(),
            })
            .await
            .expect("seed reflection state");
    }

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "重试"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let lessons = reflection_capture.lessons.lock().await.clone();
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].user_id, "u1");
    assert_eq!(lessons[0].session_id, "s1");
    assert_eq!(
        lessons[0].content,
        "Reflection-driven fix: after reviewing decision history, retried with bash. Context: Need retry with tighter path filter"
    );
    assert!(reflection_capture.marks.lock().await.is_empty());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_runs_observer_when_hook_flag_uses_inprocess_bridge() {
    let contract = load_contract();
    let observer_capture = ObserverCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(bridge_state_with_observer_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_turn_observer_worker(Arc::new(RecordingTurnObserverWorker {
                capture: observer_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret.clone())
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "请总结这个方案"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let requests = observer_capture.requests.lock().await.clone();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.user_id, "u1");
    assert_eq!(request.session_id, "s1");
    assert_eq!(request.turn_count, 1);
    assert_eq!(
        request.messages,
        vec![
            serde_json::Map::from_iter([
                (
                    "role".to_string(),
                    serde_json::Value::String("user".to_string())
                ),
                (
                    "content".to_string(),
                    serde_json::Value::String("请总结这个方案".to_string()),
                ),
            ]),
            serde_json::Map::from_iter([
                (
                    "role".to_string(),
                    serde_json::Value::String("assistant".to_string())
                ),
                (
                    "content".to_string(),
                    serde_json::Value::String(
                        "这是最终答复，包含足够长的内容用于 observer 提取。".to_string()
                    ),
                ),
            ]),
        ]
    );
    assert_eq!(
        request.session_start,
        Some(serde_json::Value::String(
            "2026-03-20T00:00:00Z".to_string()
        ))
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_blocks_prompt_leak_from_bridge_state() {
    let contract = load_contract();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(prompt_leak_bridge_state_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_chat_turn_bridge_cache(bridge_cache.clone())
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        serde_json::json!({"type": "session_info", "session_id": "s1"})
    );
    assert_eq!(frames[1]["type"], "error");
    assert_eq!(frames[1]["code"], "PROMPT_LEAK");
    assert_eq!(frames[1]["retryable"], true);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut cache = bridge_cache.lock().await;
    let cached = cache
        .get("s1", now)
        .expect("identifier cache entry should remain");
    assert!(cached.get("history").is_none());
    assert!(cached.get("turn_count").is_none());
    assert!(cached.get("has_tool_calls").is_none());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_synthesizes_warning_before_turn_complete() {
    let contract = load_contract();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(warning_bridge_state_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_cache(bridge_cache.clone())
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 3);
    assert_eq!(
        frames[0],
        serde_json::json!({"type": "session_info", "session_id": "s1"})
    );
    assert_eq!(
        frames[1],
        serde_json::json!({
            "type": "warning",
            "message": "Response may contain unverified claims",
            "claims_failed": 2
        })
    );
    assert_eq!(frames[2]["type"], "turn_complete");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut cache = bridge_cache.lock().await;
    let cached = cache.get("s1", now).unwrap_or_default();
    assert!(cached.get("firewall_warning_claims_failed").is_none());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_synthesizes_explain_before_turn_complete() {
    let contract = load_contract();
    let bridge_cache = Arc::new(Mutex::new(SessionCache::new(1000, 86400.0)));
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(explain_bridge_state_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_cache(bridge_cache.clone())
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": true
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 3);
    assert_eq!(
        frames[0],
        serde_json::json!({
            "type": "session_info",
            "session_id": "s1"
        })
    );
    assert_eq!(
        frames[1],
        serde_json::json!({
            "type": "explain",
            "total_ms": 7,
            "prompt_tokens": null,
            "completion_tokens": null,
            "tools_selected": 1,
            "tools_available": 2,
            "tool_selection": null,
            "tool_selection_fallback": null,
            "steps": []
        })
    );
    assert_eq!(frames[2]["type"], "turn_complete");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut cache = bridge_cache.lock().await;
    let cached = cache.get("s1", now).unwrap_or_default();
    assert!(cached.get("explain_inputs").is_none());
    assert!(cached.get("explain_total_ms").is_none());
    assert!(cached.get("explain_steps").is_none());

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_drops_upstream_warning_without_bridge_state() {
    let contract = load_contract();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(upstream_warning_without_bridge_state_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        serde_json::json!({
            "type": "session_info",
            "session_id": "s1"
        })
    );
    assert_eq!(frames[1]["type"], "turn_complete");
    assert!(
        !frames
            .iter()
            .any(|frame| frame.get("type") == Some(&serde_json::json!("warning")))
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_drops_upstream_explain_without_bridge_state() {
    let contract = load_contract();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(upstream_explain_without_bridge_state_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        serde_json::json!({
            "type": "session_info",
            "session_id": "s1"
        })
    );
    assert_eq!(frames[1]["type"], "turn_complete");
    assert!(
        !frames
            .iter()
            .any(|frame| frame.get("type") == Some(&serde_json::json!("explain")))
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_synthesizes_trusted_session_info() {
    let contract = load_contract();
    let session_capture = SessionCaptureState::default();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(conflicting_session_info_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: session_capture.clone(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "agent_id": "agent-123",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        serde_json::json!({
            "type": "session_info",
            "session_id": "generated-session"
        })
    );
    assert_eq!(frames[1]["type"], "turn_complete");
    assert_eq!(
        *session_capture.create_user_id.lock().await,
        Some("u1".to_string())
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_derives_has_tool_calls_from_tool_sigs() {
    let contract = load_contract();
    let (addr, server) = spawn_internal_bridge!(Router::new().route(
        "/internal/chat/turn",
        post(conflicting_has_tool_calls_internal_turn),
    ));

    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService {
                capture: SessionCaptureState::default(),
            }))
            .with_chat_turn_bridge_secret(contract.bridge_secret)
            .with_chat_turn_bridge_url(internal_chat_turn_url(addr)),
    );

    let response = app
        .oneshot(build_request(
            "/chat/turn",
            Some("Bearer good-token"),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "session_id": "s1",
                "explain": false
            }),
        ))
        .await
        .unwrap();

    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload = String::from_utf8(body.to_vec()).unwrap();
    let frames = payload
        .trim()
        .split("\n\n")
        .map(|frame| frame.strip_prefix("data: ").unwrap())
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        serde_json::json!({"type": "session_info", "session_id": "s1"})
    );
    assert_eq!(
        frames[1],
        serde_json::json!({"type": "turn_complete", "has_tool_calls": true})
    );

    server.abort();
}

#[tokio::test]
async fn http_chat_turn_bridge_rebuilds_sanitized_upstream_events() {
    let contract = load_contract();

    internal_rebuild_case!(
        contract,
        "usage",
        usage_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[0],
                serde_json::json!({"type": "session_info", "session_id": "s1"}),
                "{l}"
            );
            assert_eq!(
                frames[1],
                serde_json::json!({
                    "type": "usage",
                    "prompt_tokens": 5,
                    "completion_tokens": 2,
                    "cache_read_tokens": 1
                }),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "tool_call_start",
        tool_call_start_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[0],
                serde_json::json!({"type": "session_info", "session_id": "s1"}),
                "{l}"
            );
            assert_eq!(
                frames[1],
                serde_json::json!({
                    "type": "tool_call_start",
                    "name": "bash"
                }),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "tool_call",
        tool_call_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[0],
                serde_json::json!({"type": "session_info", "session_id": "s1"}),
                "{l}"
            );
            assert_eq!(frames[1]["type"], "tool_call", "{l}");
            assert_eq!(frames[1]["id"], "tc1", "{l}");
            assert_eq!(frames[1]["name"], "bash", "{l}");
            assert_eq!(
                frames[1]["arguments"],
                serde_json::json!({"command": "ls"}),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "error",
        error_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 2, "{l}");
            assert_eq!(
                frames[0],
                serde_json::json!({"type": "session_info", "session_id": "s1"}),
                "{l}"
            );
            assert_eq!(
                frames[1],
                serde_json::json!({
                    "type": "error",
                    "message": "boom",
                    "code": "SERVER_ERROR",
                    "retryable": true,
                    "retry_after_ms": 1000
                }),
                "{l}"
            );
        }
    );

    internal_rebuild_case!(
        contract,
        "cloud_loop_progress",
        cloud_loop_progress_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[1],
                serde_json::json!({
                    "type": "cloud_loop_progress",
                    "loop": 1,
                    "cloud_skills": 2,
                    "edge_skills": 3
                }),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "cloud_tool_result",
        cloud_tool_result_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[1],
                serde_json::json!({
                    "type": "cloud_tool_result",
                    "name": "execute_code",
                    "result": "ok",
                    "blocked": true
                }),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "tool_result_quality",
        tool_result_quality_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[1],
                serde_json::json!({
                    "type": "tool_result_quality",
                    "tool_name": "bash",
                    "grade": "partial",
                    "score": 0.5,
                    "signals": ["truncated"]
                }),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "text_delta",
        text_delta_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[1],
                serde_json::json!({"type": "text_delta", "content": "hello"}),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );

    internal_rebuild_case!(
        contract,
        "reasoning_delta",
        reasoning_delta_internal_turn,
        |frames: &[serde_json::Value], l: &str| {
            assert_eq!(frames.len(), 3, "{l}");
            assert_eq!(
                frames[1],
                serde_json::json!({"type": "reasoning_delta", "content": "thinking"}),
                "{l}"
            );
            assert_eq!(frames[2]["type"], "turn_complete", "{l}");
        }
    );
}
