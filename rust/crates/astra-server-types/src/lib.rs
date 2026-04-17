pub mod agent_mailbox;
pub mod agent_mcp;
mod chat_route;
pub mod conflict_resolver;
pub mod edge_connection_pool;
pub mod edge_ws_protocol;
pub mod worktree_isolation;
pub mod ws_progress_callback;

use astra_services::auth::SessionActivityRecord;
use astra_services::{
    AdminAuditRecord, AdminFeedbackStatsRecord, AdminInitRecord, AdminTokenRecord,
    AdminUserRoleRecord, AuthTokenRecord, AuthUserRecord, CancelRunRecord, ChatRequestData,
    ChatRunRecord, RunListRecord, RunMutationRecord, RunStatusRecord, SessionListRecord,
    SessionRecord,
};
use serde::{Deserialize, Serialize};

pub use chat_route::{ChatRouteResponse, classify_chat_route};
pub use edge_ws_protocol::{
    EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS, EDGE_TOOL_TIMEOUT_SECS,
    EdgeClientMessage, EdgeServerMessage,
};

#[derive(Serialize, PartialEq, Eq)]
pub struct RootResponse {
    pub name: String,
    pub version: String,
    pub docs: String,
}

#[derive(Deserialize)]
pub struct AuthRegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Deserialize)]
pub struct AuthLoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

#[derive(Deserialize, Default)]
pub struct ChatRouteRequest {
    #[serde(default)]
    pub query: String,
}

#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub skill_search: Option<astra_core::SkillSearchSettings>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default = "default_max_candidates")]
    pub max_candidates: u32,
    #[serde(default)]
    pub explain: bool,
    /// Durable plan subtask id — merged into `context` for cloud stop-hooks (`when: task_completed`).
    #[serde(default)]
    pub plan_subtask_id: Option<String>,
    #[serde(default)]
    pub is_plan_subtask: Option<bool>,
}

#[derive(Deserialize, Default)]
pub struct RunStreamQuery {
    #[serde(default)]
    pub last_index: u32,
}

#[derive(Deserialize)]
pub struct SessionCreateRequest {
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub struct SessionUpdateRequest {
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub status: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct SessionListQuery {
    pub agent_id: Option<String>,
    pub session_status: Option<String>,
    #[serde(default = "default_session_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Deserialize, Default)]
pub struct SessionActivityQuery {
    #[serde(default = "default_session_activity_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct SessionActivityEntry {
    pub log_id: String,
    pub action: String,
    pub details: serde_json::Value,
    pub created_at: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct SessionActivityResponse {
    pub session_id: String,
    pub activities: Vec<SessionActivityEntry>,
    pub total: i64,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct AuthUserResponse {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
}

/// Returned by POST /auth/register — includes the user record plus ready-to-use tokens
/// so callers don't need a separate login round-trip.
#[derive(Serialize, PartialEq, Eq)]
pub struct AuthRegisterResponse {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct AuthTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct AuthLogoutResponse {
    pub message: String,
}

#[derive(Serialize, PartialEq)]
pub struct SessionResponse {
    pub session_id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub status: String,
    pub event_count: i64,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Serialize, PartialEq)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionResponse>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Serialize, PartialEq)]
pub struct ChatResponse {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub explain: Option<serde_json::Value>,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct RunStatusResponse {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub events_count: i64,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct CancelRunResponse {
    pub run_id: String,
    pub status: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct RunMutationResponse {
    pub run_id: String,
    pub status: String,
    pub previous_status: String,
}

#[derive(Deserialize, Default)]
pub struct RunListQuery {
    #[serde(default = "default_run_list_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct RunListResponse {
    pub runs: Vec<RunStatusResponse>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
    pub database: String,
    pub persist_ok: u64,
    pub persist_fail: u64,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct LearningHealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub timestamp: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct LearningSignalsResponse {
    pub signal_types: Vec<&'static str>,
    pub descriptions: LearningSignalDescriptions,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct LearningSignalDescriptions {
    pub wrong_skill: &'static str,
    pub slow_execution: &'static str,
    pub high_cost: &'static str,
    pub low_satisfaction: &'static str,
}

#[derive(Serialize, PartialEq)]
pub struct LearningStatsResponse {
    pub total_learnings: i32,
    pub high_confidence: i32,
    pub low_confidence: i32,
    pub avg_confidence: f64,
    pub by_signal_type: serde_json::Map<String, serde_json::Value>,
    pub weights: serde_json::Map<String, serde_json::Value>,
    pub weights_per_signal: serde_json::Map<String, serde_json::Value>,
    pub decay: serde_json::Map<String, serde_json::Value>,
    pub total_gates: i32,
    pub passed_gates: i32,
    pub failed_gates: i32,
    pub pass_rate: f64,
    pub avg_improvement_pct: f64,
    pub per_skill: serde_json::Map<String, serde_json::Value>,
    pub last_learning_time: Option<String>,
}

#[derive(Deserialize)]
pub struct LearningTriggerRequest {
    #[serde(default = "default_days")]
    pub days: i32,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_signal_types")]
    pub signal_types: Vec<String>,
    #[serde(default)]
    pub weights: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize, PartialEq)]
pub struct LearningTriggerResponse {
    pub status: &'static str,
    pub learned: i32,
    pub signals_by_type: Option<serde_json::Value>,
    pub gate_verdict: Option<String>,
    pub improvement_pct: Option<serde_json::Value>,
    pub test_count: Option<i32>,
    pub error: Option<&'static str>,
    pub message: Option<serde_json::Value>,
    pub model_version: &'static str,
}

#[derive(Deserialize, Default)]
pub struct AdminTokenListQuery {
    pub token_type: Option<String>,
    pub scope: Option<String>,
}

#[derive(Deserialize)]
pub struct AdminTokenCreateRequest {
    pub token_type: String,
    pub provider: Option<String>,
    #[serde(default = "default_admin_scope")]
    pub scope: String,
    pub scope_id: Option<String>,
    pub token_value: Option<String>,
}

#[derive(Deserialize)]
pub struct PromptOptimizeRequest {
    pub agent_id: String,
    #[serde(default = "default_prompt_optimization_type")]
    pub optimization_type: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct PromptOptimizeResponse {
    pub job_id: String,
    pub status: &'static str,
    pub message: String,
}

#[derive(Deserialize)]
pub struct FeedbackExportRequest {
    pub agent_id: Option<String>,
    #[serde(default = "default_feedback_export_format")]
    pub format: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct FeedbackExportResponse {
    pub job_id: String,
    pub status: &'static str,
    pub download_url: Option<String>,
}

#[derive(Deserialize)]
pub struct AdminFeedbackStatsQuery {
    pub agent_id: Option<String>,
    pub since: Option<String>,
}

#[derive(Deserialize)]
pub struct AdminAuditListQuery {
    pub user_id: Option<String>,
    pub since: Option<String>,
    #[serde(default = "default_admin_audit_limit")]
    pub limit: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct AdminTokenResponse {
    pub token_id: String,
    pub token_type: String,
    pub provider: Option<String>,
    pub scope: String,
    pub scope_id: Option<String>,
    pub created_at: String,
}

#[derive(Serialize, PartialEq)]
pub struct AdminAuditResponse {
    pub log_id: String,
    pub user_id: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub timestamp: String,
    pub details: Option<serde_json::Value>,
}

#[derive(Serialize, PartialEq)]
pub struct AdminFeedbackStatsResponse {
    pub total_feedback: i64,
    pub positive_feedback: i64,
    pub negative_feedback: i64,
    pub avg_rating: Option<f64>,
    pub feedback_by_type: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct AdminInitResponse {
    pub message: String,
    pub tables_created: i64,
}

#[derive(Deserialize)]
pub struct AdminUserRoleRequest {
    pub username: String,
    pub role_name: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct AdminUserRoleResponse {
    pub username: String,
    pub role_name: String,
    pub message: String,
}

#[doc(hidden)]
pub fn default_ws_max_candidates() -> u32 {
    25
}

/// Messages sent from browser client to server.
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WsClientMessage {
    /// Authenticate with a Bearer token (must be first message).
    #[serde(rename = "auth")]
    Auth { token: String },

    /// Send a chat message to the agent.
    #[serde(rename = "message")]
    ChatMessage {
        content: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        skill_search: Option<astra_core::SkillSearchSettings>,
        #[serde(default)]
        context: Option<serde_json::Map<String, serde_json::Value>>,
        #[serde(default = "default_ws_max_candidates")]
        max_candidates: u32,
        #[serde(default)]
        explain: bool,
        #[serde(default)]
        plan_subtask_id: Option<String>,
        #[serde(default)]
        is_plan_subtask: Option<bool>,
    },

    /// Cancel an active run.
    #[serde(rename = "cancel_run")]
    CancelRun { run_id: String },

    /// Pause an active run.
    #[serde(rename = "pause_run")]
    PauseRun { run_id: String },

    /// Resume a paused run.
    #[serde(rename = "resume_run")]
    ResumeRun { run_id: String },

    /// Respond to a tool approval request.
    #[serde(rename = "tool_approval")]
    ToolApproval {
        request_id: String,
        approved: bool,
        #[serde(default)]
        reason: Option<String>,
    },

    /// Client heartbeat.
    #[serde(rename = "ping")]
    Ping,
}

/// Messages sent from server to browser client.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WsServerMessage {
    /// Authentication succeeded.
    #[serde(rename = "auth_ok")]
    AuthOk { user_id: String, username: String },

    /// Authentication failed.
    #[serde(rename = "auth_error")]
    AuthError { message: String },

    /// Session/run identifiers for the active websocket chat stream.
    #[serde(rename = "session_info")]
    SessionInfo {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },

    /// Agentic run started — client should track this run_id.
    #[serde(rename = "run_started")]
    RunStarted {
        run_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        explain: Option<serde_json::Value>,
    },

    /// Agentic run finished (completed or failed).
    #[serde(rename = "run_finished")]
    RunFinished {
        run_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Run was cancelled by client request.
    #[serde(rename = "run_cancelled")]
    RunCancelled { run_id: String },

    /// Run was paused.
    #[serde(rename = "run_paused")]
    RunPaused { run_id: String },

    /// Run was resumed.
    #[serde(rename = "run_resumed")]
    RunResumed { run_id: String },

    /// Tool requires user approval before execution.
    #[serde(rename = "tool_approval_request")]
    ToolApprovalRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
    },

    /// Tool execution started on server.
    #[serde(rename = "tool_execution_started")]
    ToolExecutionStarted { call_id: String, tool: String },

    /// Incremental output from a running tool.
    #[serde(rename = "tool_output_delta")]
    ToolOutputDelta { call_id: String, content: String },

    /// Tool execution completed on server.
    #[serde(rename = "tool_execution_completed")]
    ToolExecutionCompleted { call_id: String, success: bool },

    /// Error during processing.
    #[serde(rename = "error")]
    Error {
        message: String,
        code: String,
        retryable: bool,
    },

    /// Server heartbeat response.
    #[serde(rename = "pong")]
    Pong,

    /// Connection is being closed.
    #[serde(rename = "closing")]
    Closing { reason: String },
}

/// Query params for WebSocket upgrade — allows token in URL for browser compat.
#[derive(Deserialize, Default)]
pub struct WsUpgradeQuery {
    /// Optional Bearer token (alternative to sending auth message).
    pub token: Option<String>,
    /// Optional session ID to request on the first chat turn.
    pub session_id: Option<String>,
}

#[doc(hidden)]
pub fn merge_plan_subtask_context(
    mut context: Option<serde_json::Map<String, serde_json::Value>>,
    plan_subtask_id: Option<String>,
    is_plan_subtask: Option<bool>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if plan_subtask_id.is_some() || is_plan_subtask == Some(true) {
        let ctx = context.get_or_insert_with(serde_json::Map::new);
        if let Some(id) = plan_subtask_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            ctx.entry("plan_subtask_id".to_string())
                .or_insert(serde_json::Value::String(id));
        }
        if is_plan_subtask == Some(true) {
            ctx.entry("is_plan_subtask".to_string())
                .or_insert(serde_json::Value::Bool(true));
        }
    }
    context
}

#[doc(hidden)]
pub fn default_days() -> i32 {
    7
}

#[doc(hidden)]
pub fn default_admin_scope() -> String {
    "global".to_string()
}

#[doc(hidden)]
pub fn default_max_candidates() -> u32 {
    5
}

#[doc(hidden)]
pub fn default_session_limit() -> u32 {
    50
}

#[doc(hidden)]
pub fn default_run_list_limit() -> u32 {
    50
}

#[doc(hidden)]
pub fn default_session_activity_limit() -> u32 {
    100
}

#[doc(hidden)]
pub fn default_prompt_optimization_type() -> String {
    "compression".to_string()
}

#[doc(hidden)]
pub fn default_feedback_export_format() -> String {
    "jsonl".to_string()
}

#[doc(hidden)]
pub fn default_admin_audit_limit() -> u32 {
    100
}

#[doc(hidden)]
pub fn default_signal_types() -> Vec<String> {
    vec!["wrong_skill".to_string()]
}

pub fn sse_error_code_for_status(status: u16) -> &'static str {
    match status {
        401 | 403 => "AUTH_ERROR",
        404 => "NOT_FOUND",
        422 => "VALIDATION_ERROR",
        _ => "INTERNAL_ERROR",
    }
}

pub fn sse_retryable_for_status(status: u16) -> bool {
    status >= 500 || status == 429
}

pub fn build_sse_error_event_payload(status: u16, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "message": message.into(),
        "code": sse_error_code_for_status(status),
        "retryable": sse_retryable_for_status(status),
    })
}

impl From<AdminTokenRecord> for AdminTokenResponse {
    fn from(value: AdminTokenRecord) -> Self {
        Self {
            token_id: value.token_id,
            token_type: value.token_type,
            provider: value.provider,
            scope: value.scope,
            scope_id: value.scope_id,
            created_at: value.created_at,
        }
    }
}

impl From<AdminAuditRecord> for AdminAuditResponse {
    fn from(value: AdminAuditRecord) -> Self {
        Self {
            log_id: value.log_id,
            user_id: value.user_id,
            action: value.action,
            resource_type: value.resource_type,
            resource_id: value.resource_id,
            timestamp: value.timestamp,
            details: value.details,
        }
    }
}

impl From<AdminFeedbackStatsRecord> for AdminFeedbackStatsResponse {
    fn from(value: AdminFeedbackStatsRecord) -> Self {
        Self {
            total_feedback: value.total_feedback,
            positive_feedback: value.positive_feedback,
            negative_feedback: value.negative_feedback,
            avg_rating: value.avg_rating,
            feedback_by_type: value.feedback_by_type,
        }
    }
}

impl From<AdminInitRecord> for AdminInitResponse {
    fn from(value: AdminInitRecord) -> Self {
        Self {
            message: value.message,
            tables_created: value.tables_created,
        }
    }
}

impl From<AdminUserRoleRecord> for AdminUserRoleResponse {
    fn from(value: AdminUserRoleRecord) -> Self {
        Self {
            username: value.username,
            role_name: value.role_name,
            message: value.message,
        }
    }
}

impl From<SessionRecord> for SessionResponse {
    fn from(value: SessionRecord) -> Self {
        Self {
            session_id: value.session_id,
            user_id: value.user_id,
            agent_id: value.agent_id,
            title: value.title,
            metadata: value.metadata,
            status: value.status,
            event_count: value.event_count,
            created_at: value.created_at,
            updated_at: value.updated_at,
            ended_at: value.ended_at,
        }
    }
}

impl From<SessionListRecord> for SessionListResponse {
    fn from(value: SessionListRecord) -> Self {
        Self {
            sessions: value
                .sessions
                .into_iter()
                .map(SessionResponse::from)
                .collect(),
            total: value.total,
            limit: value.limit,
            offset: value.offset,
        }
    }
}

impl From<SessionActivityRecord> for SessionActivityResponse {
    fn from(value: SessionActivityRecord) -> Self {
        Self {
            session_id: value.session_id,
            activities: value
                .activities
                .into_iter()
                .map(|e| SessionActivityEntry {
                    log_id: e.log_id,
                    action: e.action,
                    details: e.details,
                    created_at: e.created_at,
                })
                .collect(),
            total: value.total,
        }
    }
}

impl From<ChatRunRecord> for ChatResponse {
    fn from(value: ChatRunRecord) -> Self {
        Self {
            session_id: value.session_id,
            run_id: value.run_id,
            status: value.status,
            explain: value.explain,
        }
    }
}

impl From<RunStatusRecord> for RunStatusResponse {
    fn from(value: RunStatusRecord) -> Self {
        Self {
            run_id: value.run_id,
            session_id: value.session_id,
            status: value.status,
            waiting_for: value.waiting_for,
            events_count: value.events_count,
        }
    }
}

impl From<CancelRunRecord> for CancelRunResponse {
    fn from(value: CancelRunRecord) -> Self {
        Self {
            run_id: value.run_id,
            status: value.status,
        }
    }
}

impl From<RunMutationRecord> for RunMutationResponse {
    fn from(value: RunMutationRecord) -> Self {
        Self {
            run_id: value.run_id,
            status: value.status,
            previous_status: value.previous_status,
        }
    }
}

impl From<RunListRecord> for RunListResponse {
    fn from(value: RunListRecord) -> Self {
        Self {
            runs: value
                .runs
                .into_iter()
                .map(RunStatusResponse::from)
                .collect(),
            total: value.total,
            limit: value.limit,
            offset: value.offset,
        }
    }
}

impl From<AuthUserRecord> for AuthUserResponse {
    fn from(value: AuthUserRecord) -> Self {
        Self {
            user_id: value.user_id,
            username: value.username,
            email: value.email,
            display_name: value.display_name,
        }
    }
}

impl From<AuthTokenRecord> for AuthTokenResponse {
    fn from(value: AuthTokenRecord) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            token_type: value.token_type,
            expires_in: value.expires_in,
        }
    }
}

#[doc(hidden)]
pub fn chat_request_into_data(mut request: ChatRequest) -> ChatRequestData {
    let context = merge_plan_subtask_context(
        request.context.take(),
        request.plan_subtask_id.take(),
        request.is_plan_subtask,
    );
    ChatRequestData {
        message: request.message,
        session_id: request.session_id,
        agent_id: request.agent_id,
        model: request.model,
        skill_search: request.skill_search,
        context,
        max_candidates: request.max_candidates,
        explain: request.explain,
    }
}

#[cfg(test)]
mod sse_error_payload_tests {
    use super::*;

    #[test]
    fn sse_error_code_maps_common_statuses() {
        assert_eq!(sse_error_code_for_status(401), "AUTH_ERROR");
        assert_eq!(sse_error_code_for_status(404), "NOT_FOUND");
        assert_eq!(sse_error_code_for_status(422), "VALIDATION_ERROR");
        assert_eq!(sse_error_code_for_status(418), "INTERNAL_ERROR");
    }

    #[test]
    fn sse_error_event_payload_includes_code_and_retryable() {
        let payload = build_sse_error_event_payload(503, "upstream unavailable");
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["code"], "INTERNAL_ERROR");
        assert_eq!(payload["retryable"], true);
        assert_eq!(payload["message"], "upstream unavailable");
    }
}
