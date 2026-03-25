use super::*;

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct RootResponse {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) docs: String,
}

#[derive(Deserialize)]
pub(super) struct AuthRegisterRequest {
    pub(super) username: String,
    pub(super) email: String,
    pub(super) password: String,
    pub(super) display_name: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AuthLoginRequest {
    pub(super) username: String,
    pub(super) password: String,
}

#[derive(Deserialize)]
pub(super) struct AuthRefreshRequest {
    pub(super) refresh_token: String,
}

#[derive(Deserialize, Default)]
pub(super) struct ChatRouteRequest {
    #[serde(default)]
    pub(super) query: String,
}

#[derive(Deserialize)]
pub(super) struct ChatRequest {
    pub(super) message: String,
    pub(super) session_id: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) model: Option<String>,
    pub(super) context: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default = "default_max_candidates")]
    pub(super) max_candidates: u32,
    #[serde(default)]
    pub(super) explain: bool,
}

#[derive(Deserialize, Default)]
pub(super) struct RunStreamQuery {
    #[serde(default)]
    pub(super) last_index: u32,
}

#[derive(Deserialize)]
pub(super) struct SessionCreateRequest {
    pub(super) agent_id: Option<String>,
    pub(super) title: Option<String>,
    pub(super) metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub(super) struct SessionUpdateRequest {
    pub(super) title: Option<String>,
    pub(super) metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub(super) status: Option<String>,
}

#[derive(Deserialize, Default)]
pub(super) struct SessionListQuery {
    pub(super) agent_id: Option<String>,
    pub(super) session_status: Option<String>,
    #[serde(default = "default_session_limit")]
    pub(super) limit: u32,
    #[serde(default)]
    pub(super) offset: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AuthUserResponse {
    pub(super) user_id: String,
    pub(super) username: String,
    pub(super) email: String,
    pub(super) display_name: Option<String>,
}

/// Returned by POST /auth/register — includes the user record plus ready-to-use tokens
/// so callers don't need a separate login round-trip.
#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AuthRegisterResponse {
    pub(super) user_id: String,
    pub(super) username: String,
    pub(super) email: String,
    pub(super) display_name: Option<String>,
    pub(super) access_token: String,
    pub(super) refresh_token: String,
    pub(super) token_type: String,
    pub(super) expires_in: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AuthTokenResponse {
    pub(super) access_token: String,
    pub(super) refresh_token: String,
    pub(super) token_type: String,
    pub(super) expires_in: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AuthLogoutResponse {
    pub(super) message: String,
}

#[derive(Serialize, PartialEq)]
pub(super) struct SessionResponse {
    pub(super) session_id: String,
    pub(super) user_id: String,
    pub(super) agent_id: Option<String>,
    pub(super) title: Option<String>,
    pub(super) metadata: serde_json::Map<String, serde_json::Value>,
    pub(super) status: String,
    pub(super) event_count: i64,
    pub(super) created_at: String,
    pub(super) updated_at: Option<String>,
    pub(super) ended_at: Option<String>,
}

#[derive(Serialize, PartialEq)]
pub(super) struct SessionListResponse {
    pub(super) sessions: Vec<SessionResponse>,
    pub(super) total: i64,
    pub(super) limit: u32,
    pub(super) offset: u32,
}

#[derive(Serialize, PartialEq)]
pub(super) struct ChatResponse {
    pub(super) session_id: String,
    pub(super) run_id: String,
    pub(super) status: String,
    pub(super) explain: Option<serde_json::Value>,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct RunStatusResponse {
    pub(super) run_id: String,
    pub(super) session_id: String,
    pub(super) status: String,
    pub(super) waiting_for: Option<String>,
    pub(super) events_count: i64,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct CancelRunResponse {
    pub(super) run_id: String,
    pub(super) status: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct HealthResponse {
    pub(super) status: String,
    pub(super) database: String,
    pub(super) persist_ok: u64,
    pub(super) persist_fail: u64,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct LearningHealthResponse {
    pub(super) status: String,
    pub(super) service: String,
    pub(super) version: String,
    pub(super) timestamp: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct LearningSignalsResponse {
    pub(super) signal_types: Vec<&'static str>,
    pub(super) descriptions: LearningSignalDescriptions,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct LearningSignalDescriptions {
    pub(super) wrong_skill: &'static str,
    pub(super) slow_execution: &'static str,
    pub(super) high_cost: &'static str,
    pub(super) low_satisfaction: &'static str,
}

#[derive(Serialize, PartialEq)]
pub(super) struct LearningStatsResponse {
    pub(super) total_learnings: i32,
    pub(super) high_confidence: i32,
    pub(super) low_confidence: i32,
    pub(super) avg_confidence: f64,
    pub(super) by_signal_type: serde_json::Map<String, serde_json::Value>,
    pub(super) weights: serde_json::Map<String, serde_json::Value>,
    pub(super) weights_per_signal: serde_json::Map<String, serde_json::Value>,
    pub(super) decay: serde_json::Map<String, serde_json::Value>,
    pub(super) total_gates: i32,
    pub(super) passed_gates: i32,
    pub(super) failed_gates: i32,
    pub(super) pass_rate: f64,
    pub(super) avg_improvement_pct: f64,
    pub(super) per_skill: serde_json::Map<String, serde_json::Value>,
    pub(super) last_learning_time: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct LearningTriggerRequest {
    #[serde(default = "default_days")]
    pub(super) days: i32,
    #[serde(default)]
    pub(super) force: bool,
    #[serde(default = "default_signal_types")]
    pub(super) signal_types: Vec<String>,
    #[serde(default)]
    pub(super) weights: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize, PartialEq)]
pub(super) struct LearningTriggerResponse {
    pub(super) status: &'static str,
    pub(super) learned: i32,
    pub(super) signals_by_type: Option<serde_json::Value>,
    pub(super) gate_verdict: Option<String>,
    pub(super) improvement_pct: Option<serde_json::Value>,
    pub(super) test_count: Option<i32>,
    pub(super) error: Option<&'static str>,
    pub(super) message: Option<serde_json::Value>,
    pub(super) model_version: &'static str,
}

#[derive(Deserialize, Default)]
pub(super) struct AdminTokenListQuery {
    pub(super) token_type: Option<String>,
    pub(super) scope: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AdminTokenCreateRequest {
    pub(super) token_type: String,
    pub(super) provider: Option<String>,
    #[serde(default = "default_admin_scope")]
    pub(super) scope: String,
    pub(super) scope_id: Option<String>,
    pub(super) token_value: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct PromptOptimizeRequest {
    pub(super) agent_id: String,
    #[serde(default = "default_prompt_optimization_type")]
    pub(super) optimization_type: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct PromptOptimizeResponse {
    pub(super) job_id: String,
    pub(super) status: &'static str,
    pub(super) message: String,
}

#[derive(Deserialize)]
pub(super) struct FeedbackExportRequest {
    pub(super) agent_id: Option<String>,
    #[serde(default = "default_feedback_export_format")]
    pub(super) format: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct FeedbackExportResponse {
    pub(super) job_id: String,
    pub(super) status: &'static str,
    pub(super) download_url: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AdminFeedbackStatsQuery {
    pub(super) agent_id: Option<String>,
    pub(super) since: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AdminAuditListQuery {
    pub(super) user_id: Option<String>,
    pub(super) since: Option<String>,
    #[serde(default = "default_admin_audit_limit")]
    pub(super) limit: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AdminTokenResponse {
    pub(super) token_id: String,
    pub(super) token_type: String,
    pub(super) provider: Option<String>,
    pub(super) scope: String,
    pub(super) scope_id: Option<String>,
    pub(super) created_at: String,
}

#[derive(Serialize, PartialEq)]
pub(super) struct AdminAuditResponse {
    pub(super) log_id: String,
    pub(super) user_id: String,
    pub(super) action: String,
    pub(super) resource_type: String,
    pub(super) resource_id: Option<String>,
    pub(super) timestamp: String,
    pub(super) details: Option<serde_json::Value>,
}

#[derive(Serialize, PartialEq)]
pub(super) struct AdminFeedbackStatsResponse {
    pub(super) total_feedback: i64,
    pub(super) positive_feedback: i64,
    pub(super) negative_feedback: i64,
    pub(super) avg_rating: Option<f64>,
    pub(super) feedback_by_type: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AdminInitResponse {
    pub(super) message: String,
    pub(super) tables_created: i64,
}

#[derive(Deserialize)]
pub(super) struct AdminUserRoleRequest {
    pub(super) username: String,
    pub(super) role_name: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct AdminUserRoleResponse {
    pub(super) username: String,
    pub(super) role_name: String,
    pub(super) message: String,
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

pub(super) fn chat_request_into_data(request: ChatRequest) -> ChatRequestData {
    ChatRequestData {
        message: request.message,
        session_id: request.session_id,
        agent_id: request.agent_id,
        model: request.model,
        context: request.context,
        max_candidates: request.max_candidates,
        explain: request.explain,
    }
}

pub(super) fn default_days() -> i32 {
    7
}

pub(super) fn default_admin_scope() -> String {
    "global".to_string()
}

pub(super) fn default_max_candidates() -> u32 {
    5
}

pub(super) fn default_session_limit() -> u32 {
    50
}

pub(super) fn default_prompt_optimization_type() -> String {
    "compression".to_string()
}

pub(super) fn default_feedback_export_format() -> String {
    "jsonl".to_string()
}

pub(super) fn default_admin_audit_limit() -> u32 {
    100
}

pub(super) fn default_signal_types() -> Vec<String> {
    vec!["wrong_skill".to_string()]
}
