use astra_core::{ErrorResponse, error_response};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};

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

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)>;

    /// Pause an active run. Default: NOT_IMPLEMENTED.
    async fn pause_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Pause not supported",
        ))
    }

    /// Resume a paused run. Default: NOT_IMPLEMENTED.
    async fn resume_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Resume not supported",
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequestData {
    pub message: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub skill_search: Option<astra_core::SkillSearchSettings>,
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

/// Generic record for run mutations (pause, resume, etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMutationRecord {
    pub run_id: String,
    pub status: String,
    pub previous_status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunListRecord {
    pub runs: Vec<RunStatusRecord>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

// ─── Durable Run State Store ─────────────────────────────────────────────────

/// Persistent record for a durable agent run.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunRecord {
    pub run_id: String,
    pub user_id: String,
    pub session_id: String,
    /// Parent run ID for delegation sub-runs.
    pub parent_run_id: Option<String>,
    /// Delegation ID this run belongs to.
    pub delegation_id: Option<String>,
    /// Agent profile ID executing this run.
    pub agent_id: Option<String>,
    pub status: String,
    pub waiting_for: Option<String>,
    pub checkpoint_json: Option<String>,
    pub error_message: Option<String>,
    /// Number of verification-gate retry attempts.
    pub retry_count: u32,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    pub events: Vec<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// Abstraction for durable run persistence.
///
/// Implementations:
/// - `InMemoryRunStateStore` — for tests and single-process deployments
/// - (future) `DatabaseRunStateStore` — MatrixOne-backed persistence
#[async_trait]
pub trait RunStateStore: Send + Sync {
    /// Insert a new run record.
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String>;

    /// Load a run by ID.
    async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String>;

    /// Update run status and optional fields.
    async fn update_run_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String>;

    /// Update token/tool counts.
    async fn update_run_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String>;

    /// Save checkpoint JSON for crash recovery.
    async fn save_checkpoint(&self, run_id: &str, checkpoint_json: &str) -> Result<bool, String>;

    /// Append an event to the run's event log.
    async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String>;

    /// List runs for a user with pagination.
    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String>;

    /// Find runs in WAITING status (for resume engine).
    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find all sub-runs belonging to a delegation.
    async fn find_sub_runs(&self, delegation_id: &str) -> Result<Vec<DurableRunRecord>, String>;

    /// Update the retry count for a run (verification gate retries).
    async fn update_retry_count(&self, run_id: &str, retry_count: u32) -> Result<bool, String>;
}

/// In-memory run state store for tests and single-process deployments.
pub struct InMemoryRunStateStore {
    runs: tokio::sync::RwLock<std::collections::HashMap<String, DurableRunRecord>>,
}

impl InMemoryRunStateStore {
    pub fn new() -> Self {
        Self {
            runs: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryRunStateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RunStateStore for InMemoryRunStateStore {
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
        let mut runs = self.runs.write().await;
        runs.insert(record.run_id.clone(), record);
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs.get(run_id).cloned())
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.status = status.to_string();
            run.waiting_for = waiting_for.map(ToString::to_string);
            if let Some(msg) = error_message {
                run.error_message = Some(msg.to_string());
            }
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_run_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.total_prompt_tokens = prompt_tokens;
            run.total_completion_tokens = completion_tokens;
            run.total_tool_calls = tool_calls;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn save_checkpoint(&self, run_id: &str, checkpoint_json: &str) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.checkpoint_json = Some(checkpoint_json.to_string());
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.events.push(event);
            run.updated_at = chrono::Utc::now().to_rfc3339();
        }
        Ok(())
    }

    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
        let runs = self.runs.read().await;
        let mut user_runs: Vec<_> = runs
            .values()
            .filter(|r| r.user_id == user_id)
            .cloned()
            .collect();
        user_runs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let total = user_runs.len() as i64;
        let page = user_runs
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        Ok((page, total))
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.status == "waiting")
            .cloned()
            .collect())
    }

    async fn find_sub_runs(&self, delegation_id: &str) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.delegation_id.as_deref() == Some(delegation_id))
            .cloned()
            .collect())
    }

    async fn update_retry_count(&self, run_id: &str, retry_count: u32) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.retry_count = retry_count;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }
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

    async fn list_runs(
        &self,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(event_type: &str, data: serde_json::Value) -> serde_json::Value {
        json!({"event_type": event_type, "data": data})
    }

    #[test]
    fn text_delta() {
        let out = transform_run_event_for_client(make_event("text_delta", json!({"chunk": "hi"})));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "hi");
    }

    #[test]
    fn text_delta_missing_chunk() {
        let out = transform_run_event_for_client(make_event("text_delta", json!({})));
        assert_eq!(out["content"], "");
    }

    #[test]
    fn text_done() {
        let out =
            transform_run_event_for_client(make_event("text_done", json!({"full_text": "all"})));
        assert_eq!(out["type"], "text_done");
        assert_eq!(out["full_text"], "all");
    }

    #[test]
    fn reasoning_message_content() {
        let out = transform_run_event_for_client(make_event(
            "reasoning_message_content",
            json!({"content": "think"}),
        ));
        assert_eq!(out["type"], "reasoning_message_content");
        assert_eq!(out["content"], "think");
    }

    #[test]
    fn thinking_delta() {
        let out =
            transform_run_event_for_client(make_event("thinking_delta", json!({"chunk": "t"})));
        assert_eq!(out["type"], "thinking_delta");
        assert_eq!(out["content"], "t");
    }

    #[test]
    fn thinking_done() {
        let out = transform_run_event_for_client(make_event("thinking_done", json!({})));
        assert_eq!(out["type"], "thinking_done");
    }

    #[test]
    fn tool_call_start() {
        let out = transform_run_event_for_client(make_event(
            "tool_call_start",
            json!({"tool": "bash", "call_id": "c1"}),
        ));
        assert_eq!(out["type"], "tool_call_start");
        assert_eq!(out["tool"], "bash");
        assert_eq!(out["call_id"], "c1");
    }

    #[test]
    fn tool_result() {
        let out = transform_run_event_for_client(make_event(
            "tool_result",
            json!({"call_id": "c1", "result": "ok"}),
        ));
        assert_eq!(out["type"], "tool_result");
        assert_eq!(out["call_id"], "c1");
    }

    #[test]
    fn run_started_and_finished() {
        let started = transform_run_event_for_client(make_event("run_started", json!({})));
        assert_eq!(started["type"], "run_started");
        let finished = transform_run_event_for_client(make_event("run_finished", json!({})));
        assert_eq!(finished["type"], "run_finished");
    }

    #[test]
    fn run_error_maps_to_error_type() {
        let out = transform_run_event_for_client(make_event("run_error", json!({"error": "boom"})));
        assert_eq!(out["type"], "error");
        assert_eq!(out["message"], "boom");
        assert_eq!(out["code"], "RUN_ERROR");
    }

    #[test]
    fn run_error_default_message() {
        let out = transform_run_event_for_client(make_event("run_error", json!({})));
        assert_eq!(out["message"], "Unknown error");
    }

    #[test]
    fn plan_events() {
        let created = transform_run_event_for_client(make_event(
            "plan_created",
            json!({"plan": {"steps": []}}),
        ));
        assert_eq!(created["type"], "plan_created");
        let step_start =
            transform_run_event_for_client(make_event("plan_step_start", json!({"step": "s1"})));
        assert_eq!(step_start["type"], "plan_step_start");
        let step_done = transform_run_event_for_client(make_event(
            "plan_step_done",
            json!({"step": "s1", "result": "ok"}),
        ));
        assert_eq!(step_done["type"], "plan_step_done");
        let revised =
            transform_run_event_for_client(make_event("plan_revised", json!({"plan": {}})));
        assert_eq!(revised["type"], "plan_revised");
    }

    #[test]
    fn agent_events() {
        let delegated = transform_run_event_for_client(make_event(
            "agent_delegated",
            json!({"agent_id": "a1", "task": "t"}),
        ));
        assert_eq!(delegated["type"], "agent_delegated");
        let progress = transform_run_event_for_client(make_event(
            "agent_progress",
            json!({"agent_id": "a1", "progress": "50%"}),
        ));
        assert_eq!(progress["type"], "agent_progress");
        let completed = transform_run_event_for_client(make_event(
            "agent_completed",
            json!({"agent_id": "a1", "result": "done"}),
        ));
        assert_eq!(completed["type"], "agent_completed");
    }

    #[test]
    fn keepalive_maps_to_ping() {
        let out = transform_run_event_for_client(make_event("keepalive", json!({})));
        assert_eq!(out["type"], "ping");
    }

    #[test]
    fn unknown_event_type_passthrough() {
        let out = transform_run_event_for_client(make_event("custom_event", json!({})));
        assert_eq!(out["type"], "custom_event");
    }

    #[test]
    fn missing_event_type() {
        let out = transform_run_event_for_client(json!({"data": {}}));
        assert_eq!(out["type"], "");
    }

    #[test]
    fn missing_data_object() {
        let out = transform_run_event_for_client(json!({"event_type": "text_delta"}));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "");
    }
}
