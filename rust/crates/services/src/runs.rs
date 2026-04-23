use astra_core::{ErrorResponse, error_response, error_response_coded};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

pub const RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE: &str = "run_lifecycle_unconfigured";

pub fn is_run_lifecycle_unconfigured_error(status: StatusCode, error: &ErrorResponse) -> bool {
    status == StatusCode::NOT_IMPLEMENTED
        && error.error_code.as_deref() == Some(RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE)
}

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

    /// Drain pending tool approval requests for a run.
    ///
    /// Returns JSON objects with `request_id`, `tool`, `args` fields.
    /// The WS handler calls this during its polling loop to forward
    /// approval requests to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_approval_requests(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Drain pending ask_user prompt requests for a run.
    ///
    /// Returns JSON objects with `request_id`, `question`, `choices`, `default`,
    /// and `context` fields. The WS handler calls this during its polling loop to
    /// forward prompts to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_user_prompt_requests(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Drain pending tool progress events for a run.
    ///
    /// Returns JSON objects with `kind` field (`started`, `delta`, `completed`).
    /// The WS handler calls this during its polling loop to forward
    /// progress events to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_progress_events(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenServiceConfig {
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenServiceRequest {
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl From<LlmTokenServiceRequest> for LlmTokenServiceConfig {
    fn from(value: LlmTokenServiceRequest) -> Self {
        Self {
            url: value.url,
            timeout_ms: value.timeout_ms,
        }
    }
}

impl From<LlmTokenServiceConfig> for LlmTokenServiceRequest {
    fn from(value: LlmTokenServiceConfig) -> Self {
        Self {
            url: value.url,
            timeout_ms: value.timeout_ms,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct ChatRequestData {
    pub message: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub llm_token_service: Option<LlmTokenServiceConfig>,
    pub skill_search: Option<astra_core::SkillSearchSettings>,
    pub allow_skills: Option<Vec<String>>,
    pub allow_tools: Option<Vec<String>>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub forward_headers: std::collections::HashMap<String, String>,
    pub max_candidates: u32,
    pub explain: bool,
    pub interactive_client: bool,
}

fn redacted_forward_header_names(headers: &std::collections::HashMap<String, String>) -> Vec<&str> {
    let mut names = headers
        .keys()
        .filter(|name| !name.starts_with("__astra_"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

struct RedactedForwardHeadersDebug<'a>(&'a std::collections::HashMap<String, String>);

impl std::fmt::Debug for RedactedForwardHeadersDebug<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = redacted_forward_header_names(self.0);
        f.debug_struct("RedactedForwardHeaders")
            .field("count", &names.len())
            .field("names", &names)
            .finish()
    }
}

impl std::fmt::Debug for ChatRequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRequestData")
            .field("message", &self.message)
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("model", &self.model)
            .field("llm_token_service", &self.llm_token_service)
            .field("skill_search", &self.skill_search)
            .field("allow_skills", &self.allow_skills)
            .field("allow_tools", &self.allow_tools)
            .field("context", &self.context)
            .field(
                "forward_headers",
                &RedactedForwardHeadersDebug(&self.forward_headers),
            )
            .field("max_candidates", &self.max_candidates)
            .field("explain", &self.explain)
            .field("interactive_client", &self.interactive_client)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRunRecord {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub explain: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct ChatStreamRecord {
    pub session_id: String,
    pub run_id: String,
    /// Batch events (populated after loop completes for persistence).
    pub events: Vec<serde_json::Value>,
    /// When present, SSE events are streamed incrementally through this
    /// channel. The HTTP handler converts this into a streaming response.
    pub event_rx: Option<tokio::sync::mpsc::Receiver<serde_json::Value>>,
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
    /// If this run is a verification-gate retry, links to the original run.
    pub retry_of: Option<String>,
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
    /// Maximum number of runs kept in memory. When exceeded, the oldest
    /// completed/failed runs are evicted on insert.
    pub const MAX_RUNS: usize = 10_000;

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

        // Evict oldest completed/failed runs when over capacity
        if runs.len() > Self::MAX_RUNS {
            let terminal = ["completed", "failed", "cancelled"];
            let mut evictable: Vec<_> = runs
                .iter()
                .filter(|(_, r)| terminal.contains(&r.status.as_str()))
                .map(|(id, r)| (id.clone(), r.updated_at.clone()))
                .collect();
            evictable.sort_by(|a, b| a.1.cmp(&b.1));
            let to_remove = runs.len() - Self::MAX_RUNS;
            for (id, _) in evictable.into_iter().take(to_remove) {
                runs.remove(&id);
            }
        }
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
    if event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .is_none()
        && event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some()
    {
        return event;
    }

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
        "thinking_done" | "reasoning_done" => serde_json::json!({ "type": event_type }),
        "tool_call_start" => serde_json::json!({
            "type": "tool_call_start",
            "tool": data.get("tool").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "call_id": data.get("call_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "arguments": data.get("arguments").cloned().unwrap_or(serde_json::Value::Null),
        }),
        "tool_result" => serde_json::json!({
            "type": "tool_call_end",
            "call_id": data.get("call_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "run_started" => {
            let mut out = serde_json::json!({ "type": "run_started" });
            if let Some(obj) = out.as_object_mut() {
                if let Some(run_id) = data.get("run_id").cloned() {
                    obj.insert("run_id".to_string(), run_id);
                }
                if let Some(session_id) = data.get("session_id").cloned() {
                    obj.insert("session_id".to_string(), session_id);
                }
            }
            out
        }
        "run_finished" => {
            let mut out = serde_json::json!({ "type": "run_finished" });
            if let Some(obj) = out.as_object_mut() {
                if let Some(run_id) = data.get("run_id").cloned() {
                    obj.insert("run_id".to_string(), run_id);
                }
                if let Some(status) = data.get("status").cloned() {
                    obj.insert("status".to_string(), status);
                }
                if let Some(error) = data.get("error").cloned() {
                    obj.insert("error".to_string(), error);
                }
            }
            out
        }
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
        "agent_spawned" => {
            let mut out = serde_json::json!({ "type": "agent_spawned" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "agent_progress" => {
            let mut out = serde_json::json!({ "type": "agent_progress" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "agent_completed" => {
            let mut out = serde_json::json!({ "type": "agent_completed" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "keepalive" => serde_json::json!({ "type": "ping" }),
        _ => {
            let mut out = serde_json::json!({ "type": event_type });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
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
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn get_run_status(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn stream_run(
        &self,
        _run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn list_runs(
        &self,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
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
    fn reasoning_done() {
        let out = transform_run_event_for_client(make_event("reasoning_done", json!({})));
        assert_eq!(out["type"], "reasoning_done");
    }

    #[test]
    fn tool_call_start() {
        let out = transform_run_event_for_client(make_event(
            "tool_call_start",
            json!({"tool": "bash", "call_id": "c1", "arguments": "{\"command\":\"ls\"}"}),
        ));
        assert_eq!(out["type"], "tool_call_start");
        assert_eq!(out["tool"], "bash");
        assert_eq!(out["call_id"], "c1");
        assert_eq!(out["arguments"], "{\"command\":\"ls\"}");
    }

    #[test]
    fn tool_result() {
        let out = transform_run_event_for_client(make_event(
            "tool_result",
            json!({"call_id": "c1", "result": "ok"}),
        ));
        assert_eq!(out["type"], "tool_call_end");
        assert_eq!(out["call_id"], "c1");
    }

    #[test]
    fn run_started_and_finished() {
        let started = transform_run_event_for_client(make_event(
            "run_started",
            json!({"run_id": "run-1", "session_id": "sess-1"}),
        ));
        assert_eq!(started["type"], "run_started");
        assert_eq!(started["run_id"], "run-1");
        assert_eq!(started["session_id"], "sess-1");

        let finished = transform_run_event_for_client(make_event(
            "run_finished",
            json!({"run_id": "run-1", "status": "failed", "error": "boom"}),
        ));
        assert_eq!(finished["type"], "run_finished");
        assert_eq!(finished["run_id"], "run-1");
        assert_eq!(finished["status"], "failed");
        assert_eq!(finished["error"], "boom");
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
    fn unknown_event_type_preserves_data_fields() {
        let out = transform_run_event_for_client(make_event(
            "team_prepare",
            json!({"delegation_id": "d1", "phase": "prepare"}),
        ));
        assert_eq!(out["type"], "team_prepare");
        assert_eq!(out["delegation_id"], "d1");
        assert_eq!(out["phase"], "prepare");
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

    #[test]
    fn already_shaped_client_event_passthrough() {
        let event = json!({"type": "reasoning_delta", "content": "thinking", "index": 7});
        let out = transform_run_event_for_client(event.clone());
        assert_eq!(out, event);
    }

    #[test]
    fn chat_request_data_debug_redacts_forward_header_values() {
        let mut forward_headers = std::collections::HashMap::new();
        forward_headers.insert(
            "authorization".to_string(),
            "Bearer secret-token".to_string(),
        );
        forward_headers.insert("x-workspace-id".to_string(), "ws-123".to_string());
        forward_headers.insert("__astra_connection_tokens".to_string(), "x-hop".to_string());

        let request = ChatRequestData {
            message: "hi".to_string(),
            session_id: Some("sess-1".to_string()),
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: None,
            forward_headers,
            max_candidates: 10,
            explain: false,
            interactive_client: false,
        };

        let rendered = format!("{request:?}");
        assert!(rendered.contains("authorization"));
        assert!(rendered.contains("x-workspace-id"));
        assert!(!rendered.contains("Bearer secret-token"));
        assert!(!rendered.contains("ws-123"));
        assert!(!rendered.contains("__astra_connection_tokens"));
    }

    #[tokio::test]
    async fn unconfigured_service_uses_stable_error_code() {
        let service = UnconfiguredRunLifecycleService;
        let err = service
            .create_run(
                "u1".to_string(),
                ChatRequestData {
                    message: "hi".to_string(),
                    session_id: None,
                    agent_id: None,
                    model: None,
                    llm_token_service: None,
                    skill_search: None,
                    allow_skills: None,
                    allow_tools: None,
                    context: None,
                    forward_headers: std::collections::HashMap::new(),
                    max_candidates: 25,
                    explain: false,
                    interactive_client: false,
                },
            )
            .await
            .expect_err("service should be unconfigured");
        assert!(is_run_lifecycle_unconfigured_error(err.0, &err.1.0));
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some(RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE)
        );
    }

    /// U2: InMemoryRunStateStore must evict old completed runs when the
    /// store exceeds its capacity, preventing unbounded memory growth.
    #[tokio::test]
    async fn in_memory_run_store_evicts_completed_runs() {
        let store = InMemoryRunStateStore::new();
        let max = InMemoryRunStateStore::MAX_RUNS;

        // Fill to capacity + 10 with completed runs
        for i in 0..max + 10 {
            let record = DurableRunRecord {
                run_id: format!("run-{i}"),
                session_id: "s1".into(),
                user_id: "u1".into(),
                status: "completed".into(),
                parent_run_id: None,
                delegation_id: None,
                agent_id: None,
                retry_of: None,
                waiting_for: None,
                checkpoint_json: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                events: vec![],
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            store.insert_run(record).await.unwrap();
        }

        // Store must not exceed max capacity
        let runs = store.runs.read().await;
        assert!(
            runs.len() <= max,
            "store has {} runs, expected ≤ {max}",
            runs.len()
        );
    }
}
