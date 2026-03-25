use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use mo_agent_core::{ErrorResponse, error_response};

#[async_trait]
pub trait RunLifecycleService: Send + Sync {
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)>;

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequestData {
    pub message: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub max_candidates: u32,
    pub explain: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRunRecord {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub explain: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatStreamRecord {
    pub session_id: String,
    pub run_id: String,
    pub events: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunStatusRecord {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub events_count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRunRecord {
    pub run_id: String,
    pub status: String,
}

pub fn transform_run_event_for_client(event: serde_json::Value) -> serde_json::Value {
    let event_type = event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let data = event
        .get("data")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    match event_type {
        "text_delta" => serde_json::json!({
            "type": "text_delta",
            "content": data.get("chunk").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "text_done" => serde_json::json!({
            "type": "text_done",
            "full_text": data.get("full_text").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "reasoning_message_content" => serde_json::json!({
            "type": "reasoning_message_content",
            "content": data.get("content").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "thinking_delta" => serde_json::json!({
            "type": "thinking_delta",
            "content": data.get("chunk").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "thinking_done" => serde_json::json!({ "type": "thinking_done" }),
        "tool_call_start" => serde_json::json!({
            "type": "tool_call_start",
            "tool": data.get("tool").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "call_id": data.get("call_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "tool_result" => serde_json::json!({
            "type": "tool_result",
            "call_id": data.get("call_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "run_started" => serde_json::json!({ "type": "run_started" }),
        "run_finished" => serde_json::json!({ "type": "run_finished" }),
        "run_error" => serde_json::json!({
            "type": "error",
            "message": data.get("error").cloned().unwrap_or(serde_json::Value::String("Unknown error".to_string())),
            "code": "RUN_ERROR",
        }),
        "plan_created" => serde_json::json!({
            "type": "plan_created",
            "plan": data.get("plan").cloned().unwrap_or(serde_json::json!({})),
        }),
        "plan_step_start" => serde_json::json!({
            "type": "plan_step_start",
            "step": data.get("step").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "plan_step_done" => serde_json::json!({
            "type": "plan_step_done",
            "step": data.get("step").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "plan_revised" => serde_json::json!({
            "type": "plan_revised",
            "plan": data.get("plan").cloned().unwrap_or(serde_json::json!({})),
        }),
        "agent_delegated" => serde_json::json!({
            "type": "agent_delegated",
            "agent_id": data.get("agent_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "task": data.get("task").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "agent_progress" => serde_json::json!({
            "type": "agent_progress",
            "agent_id": data.get("agent_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "progress": data.get("progress").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "agent_completed" => serde_json::json!({
            "type": "agent_completed",
            "agent_id": data.get("agent_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "keepalive" => serde_json::json!({ "type": "ping" }),
        _ => serde_json::json!({ "type": event_type }),
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredRunLifecycleService;

#[async_trait]
impl RunLifecycleService for UnconfiguredRunLifecycleService {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
        ))
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
        ))
    }

    async fn get_run_status(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
        ))
    }

    async fn stream_run(
        &self,
        _run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
        ))
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
        ))
    }
}
