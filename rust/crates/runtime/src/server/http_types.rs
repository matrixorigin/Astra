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

#[derive(Deserialize, Default)]
pub(super) struct SessionActivityQuery {
    #[serde(default = "default_session_activity_limit")]
    pub(super) limit: u32,
    #[serde(default)]
    pub(super) offset: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct SessionActivityEntry {
    pub(super) log_id: String,
    pub(super) action: String,
    pub(super) details: serde_json::Value,
    pub(super) created_at: String,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct SessionActivityResponse {
    pub(super) session_id: String,
    pub(super) activities: Vec<SessionActivityEntry>,
    pub(super) total: i64,
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

#[derive(Deserialize, Default)]
pub(super) struct RunListQuery {
    #[serde(default = "default_run_list_limit")]
    pub(super) limit: u32,
    #[serde(default)]
    pub(super) offset: u32,
}

#[derive(Serialize, PartialEq, Eq)]
pub(super) struct RunListResponse {
    pub(super) runs: Vec<RunStatusResponse>,
    pub(super) total: i64,
    pub(super) limit: u32,
    pub(super) offset: u32,
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

pub(super) fn default_run_list_limit() -> u32 {
    50
}

pub(super) fn default_session_activity_limit() -> u32 {
    100
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};

    // ── default functions ───────────────────────────────────────────

    #[test]
    fn default_days_returns_seven() {
        assert_eq!(default_days(), 7);
    }

    #[test]
    fn default_admin_scope_returns_global() {
        assert_eq!(default_admin_scope(), "global");
    }

    #[test]
    fn default_max_candidates_returns_five() {
        assert_eq!(default_max_candidates(), 5);
    }

    #[test]
    fn default_session_limit_returns_fifty() {
        assert_eq!(default_session_limit(), 50);
    }

    #[test]
    fn default_prompt_optimization_type_returns_compression() {
        assert_eq!(default_prompt_optimization_type(), "compression");
    }

    #[test]
    fn default_feedback_export_format_returns_jsonl() {
        assert_eq!(default_feedback_export_format(), "jsonl");
    }

    #[test]
    fn default_admin_audit_limit_returns_hundred() {
        assert_eq!(default_admin_audit_limit(), 100);
    }

    #[test]
    fn default_signal_types_returns_wrong_skill() {
        assert_eq!(default_signal_types(), vec!["wrong_skill".to_string()]);
    }

    // ── deserialization with defaults ───────────────────────────────

    #[test]
    fn chat_request_defaults_applied() {
        let req: ChatRequest = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(req.message, "hi");
        assert_eq!(req.max_candidates, 5);
        assert!(!req.explain);
        assert!(req.session_id.is_none());
        assert!(req.agent_id.is_none());
        assert!(req.model.is_none());
        assert!(req.context.is_none());
    }

    #[test]
    fn session_list_query_defaults_applied() {
        let q: SessionListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 50);
        assert_eq!(q.offset, 0);
        assert!(q.agent_id.is_none());
        assert!(q.session_status.is_none());
    }

    #[test]
    fn run_stream_query_defaults_applied() {
        let q: RunStreamQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.last_index, 0);
    }

    #[test]
    fn chat_route_request_defaults_applied() {
        let q: ChatRouteRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(q.query, "");
    }

    #[test]
    fn learning_trigger_request_defaults_applied() {
        let req: LearningTriggerRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.days, 7);
        assert!(!req.force);
        assert_eq!(req.signal_types, vec!["wrong_skill".to_string()]);
        assert!(req.weights.is_none());
    }

    #[test]
    fn admin_token_create_request_defaults_applied() {
        let req: AdminTokenCreateRequest =
            serde_json::from_str(r#"{"token_type":"api_key"}"#).unwrap();
        assert_eq!(req.token_type, "api_key");
        assert_eq!(req.scope, "global");
        assert!(req.provider.is_none());
        assert!(req.scope_id.is_none());
        assert!(req.token_value.is_none());
    }

    #[test]
    fn admin_audit_list_query_defaults_applied() {
        let q: AdminAuditListQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.limit, 100);
        assert!(q.user_id.is_none());
        assert!(q.since.is_none());
    }

    #[test]
    fn prompt_optimize_request_defaults_applied() {
        let req: PromptOptimizeRequest = serde_json::from_str(r#"{"agent_id":"a1"}"#).unwrap();
        assert_eq!(req.optimization_type, "compression");
    }

    #[test]
    fn feedback_export_request_defaults_applied() {
        let req: FeedbackExportRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.format, "jsonl");
        assert!(req.agent_id.is_none());
    }

    // ── deserialization with all fields ─────────────────────────────

    #[test]
    fn chat_request_all_fields() {
        let input = json!({
            "message": "hello",
            "session_id": "s1",
            "agent_id": "a1",
            "model": "gpt-4",
            "context": {"key": "value"},
            "max_candidates": 10,
            "explain": true
        });
        let req: ChatRequest = serde_json::from_value(input).unwrap();
        assert_eq!(req.message, "hello");
        assert_eq!(req.session_id.as_deref(), Some("s1"));
        assert_eq!(req.agent_id.as_deref(), Some("a1"));
        assert_eq!(req.model.as_deref(), Some("gpt-4"));
        assert_eq!(req.max_candidates, 10);
        assert!(req.explain);
        let ctx = req.context.unwrap();
        assert_eq!(ctx.get("key").unwrap(), "value");
    }

    #[test]
    fn session_create_request_with_metadata() {
        let input = json!({
            "agent_id": "a1",
            "title": "My Session",
            "metadata": {"foo": 42}
        });
        let req: SessionCreateRequest = serde_json::from_value(input).unwrap();
        assert_eq!(req.agent_id.as_deref(), Some("a1"));
        assert_eq!(req.title.as_deref(), Some("My Session"));
        let meta = req.metadata.unwrap();
        assert_eq!(meta.get("foo").unwrap(), 42);
    }

    #[test]
    fn auth_register_request_with_display_name() {
        let input = json!({
            "username": "alice",
            "email": "alice@example.com",
            "password": "secret",
            "display_name": "Alice W."
        });
        let req: AuthRegisterRequest = serde_json::from_value(input).unwrap();
        assert_eq!(req.username, "alice");
        assert_eq!(req.email, "alice@example.com");
        assert_eq!(req.password, "secret");
        assert_eq!(req.display_name.as_deref(), Some("Alice W."));
    }

    #[test]
    fn auth_register_request_without_display_name() {
        let input = json!({
            "username": "bob",
            "email": "bob@example.com",
            "password": "pass"
        });
        let req: AuthRegisterRequest = serde_json::from_value(input).unwrap();
        assert!(req.display_name.is_none());
    }

    #[test]
    fn learning_trigger_request_all_fields() {
        let input = json!({
            "days": 14,
            "force": true,
            "signal_types": ["slow_execution", "high_cost"],
            "weights": {"alpha": 0.5}
        });
        let req: LearningTriggerRequest = serde_json::from_value(input).unwrap();
        assert_eq!(req.days, 14);
        assert!(req.force);
        assert_eq!(req.signal_types, vec!["slow_execution", "high_cost"]);
        let w = req.weights.unwrap();
        assert_eq!(w.get("alpha").unwrap(), &json!(0.5));
    }

    #[test]
    fn session_list_query_all_fields() {
        let input = json!({
            "agent_id": "a1",
            "session_status": "active",
            "limit": 20,
            "offset": 5
        });
        let q: SessionListQuery = serde_json::from_value(input).unwrap();
        assert_eq!(q.agent_id.as_deref(), Some("a1"));
        assert_eq!(q.session_status.as_deref(), Some("active"));
        assert_eq!(q.limit, 20);
        assert_eq!(q.offset, 5);
    }

    #[test]
    fn admin_token_create_request_all_fields() {
        let input = json!({
            "token_type": "api_key",
            "provider": "openai",
            "scope": "user",
            "scope_id": "u123",
            "token_value": "sk-xxx"
        });
        let req: AdminTokenCreateRequest = serde_json::from_value(input).unwrap();
        assert_eq!(req.scope, "user");
        assert_eq!(req.provider.as_deref(), Some("openai"));
        assert_eq!(req.scope_id.as_deref(), Some("u123"));
        assert_eq!(req.token_value.as_deref(), Some("sk-xxx"));
    }

    // ── deserialization missing required fields ─────────────────────

    #[test]
    fn chat_request_missing_message_errors() {
        let result = serde_json::from_str::<ChatRequest>(r#"{"max_candidates":3}"#);
        assert!(result.is_err());
    }

    #[test]
    fn auth_login_request_missing_username_errors() {
        let result = serde_json::from_str::<AuthLoginRequest>(r#"{"password":"x"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn auth_login_request_missing_password_errors() {
        let result = serde_json::from_str::<AuthLoginRequest>(r#"{"username":"x"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn auth_register_request_missing_email_errors() {
        let result =
            serde_json::from_str::<AuthRegisterRequest>(r#"{"username":"u","password":"p"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn auth_register_request_missing_username_errors() {
        let result =
            serde_json::from_str::<AuthRegisterRequest>(r#"{"email":"e@e.com","password":"p"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn prompt_optimize_request_missing_agent_id_errors() {
        let result = serde_json::from_str::<PromptOptimizeRequest>("{}");
        assert!(result.is_err());
    }

    #[test]
    fn admin_user_role_request_missing_fields_errors() {
        let result = serde_json::from_str::<AdminUserRoleRequest>(r#"{"username":"u"}"#);
        assert!(result.is_err());
    }

    // ── serialization of response types ─────────────────────────────

    #[test]
    fn root_response_serializes() {
        let resp = RootResponse {
            name: "mo-agent".into(),
            version: "1.0.0".into(),
            docs: "https://docs.example.com".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["name"], "mo-agent");
        assert_eq!(v["version"], "1.0.0");
        assert_eq!(v["docs"], "https://docs.example.com");
    }

    #[test]
    fn health_response_serializes() {
        let resp = HealthResponse {
            status: "ok".into(),
            database: "connected".into(),
            persist_ok: 42,
            persist_fail: 1,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["database"], "connected");
        assert_eq!(v["persist_ok"], 42);
        assert_eq!(v["persist_fail"], 1);
    }

    #[test]
    fn auth_user_response_serializes_with_none_display_name() {
        let resp = AuthUserResponse {
            user_id: "u1".into(),
            username: "alice".into(),
            email: "a@b.com".into(),
            display_name: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["user_id"], "u1");
        assert_eq!(v["display_name"], Value::Null);
    }

    #[test]
    fn auth_user_response_serializes_with_some_display_name() {
        let resp = AuthUserResponse {
            user_id: "u1".into(),
            username: "alice".into(),
            email: "a@b.com".into(),
            display_name: Some("Alice".into()),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["display_name"], "Alice");
    }

    #[test]
    fn session_response_serializes_all_fields() {
        let mut meta = Map::new();
        meta.insert("k".into(), json!("v"));
        let resp = SessionResponse {
            session_id: "s1".into(),
            user_id: "u1".into(),
            agent_id: Some("a1".into()),
            title: Some("T".into()),
            metadata: meta,
            status: "active".into(),
            event_count: 5,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: Some("2024-01-02T00:00:00Z".into()),
            ended_at: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["agent_id"], "a1");
        assert_eq!(v["metadata"]["k"], "v");
        assert_eq!(v["event_count"], 5);
        assert_eq!(v["ended_at"], Value::Null);
    }

    #[test]
    fn chat_response_serializes_with_explain() {
        let resp = ChatResponse {
            session_id: "s1".into(),
            run_id: "r1".into(),
            status: "running".into(),
            explain: Some(json!({"routing": "skill_a"})),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["explain"]["routing"], "skill_a");
    }

    #[test]
    fn run_status_response_serializes() {
        let resp = RunStatusResponse {
            run_id: "r1".into(),
            session_id: "s1".into(),
            status: "waiting".into(),
            waiting_for: Some("tool_call".into()),
            events_count: 3,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["waiting_for"], "tool_call");
        assert_eq!(v["events_count"], 3);
    }

    #[test]
    fn cancel_run_response_serializes() {
        let resp = CancelRunResponse {
            run_id: "r1".into(),
            status: "cancelled".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["run_id"], "r1");
        assert_eq!(v["status"], "cancelled");
    }

    #[test]
    fn auth_token_response_serializes() {
        let resp = AuthTokenResponse {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["access_token"], "at");
        assert_eq!(v["token_type"], "Bearer");
        assert_eq!(v["expires_in"], 3600);
    }

    #[test]
    fn auth_register_response_serializes() {
        let resp = AuthRegisterResponse {
            user_id: "u1".into(),
            username: "alice".into(),
            email: "a@b.com".into(),
            display_name: Some("Alice".into()),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            token_type: "Bearer".into(),
            expires_in: 3600,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["user_id"], "u1");
        assert_eq!(v["access_token"], "at");
        assert_eq!(v["display_name"], "Alice");
    }

    #[test]
    fn auth_logout_response_serializes() {
        let resp = AuthLogoutResponse {
            message: "bye".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["message"], "bye");
    }

    #[test]
    fn session_list_response_serializes() {
        let resp = SessionListResponse {
            sessions: vec![],
            total: 0,
            limit: 50,
            offset: 0,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["sessions"], json!([]));
        assert_eq!(v["total"], 0);
    }

    #[test]
    fn admin_token_response_serializes() {
        let resp = AdminTokenResponse {
            token_id: "t1".into(),
            token_type: "api_key".into(),
            provider: Some("openai".into()),
            scope: "global".into(),
            scope_id: None,
            created_at: "2024-01-01T00:00:00Z".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["token_id"], "t1");
        assert_eq!(v["provider"], "openai");
        assert_eq!(v["scope_id"], Value::Null);
    }

    #[test]
    fn admin_audit_response_serializes() {
        let resp = AdminAuditResponse {
            log_id: "l1".into(),
            user_id: "u1".into(),
            action: "login".into(),
            resource_type: "session".into(),
            resource_id: Some("s1".into()),
            timestamp: "2024-01-01T00:00:00Z".into(),
            details: Some(json!({"ip": "127.0.0.1"})),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["details"]["ip"], "127.0.0.1");
    }

    #[test]
    fn admin_init_response_serializes() {
        let resp = AdminInitResponse {
            message: "done".into(),
            tables_created: 5,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["tables_created"], 5);
    }

    #[test]
    fn admin_feedback_stats_response_serializes() {
        let mut by_type = Map::new();
        by_type.insert("thumbs_up".into(), json!(10));
        let resp = AdminFeedbackStatsResponse {
            total_feedback: 20,
            positive_feedback: 15,
            negative_feedback: 5,
            avg_rating: Some(4.2),
            feedback_by_type: by_type,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["total_feedback"], 20);
        assert_eq!(v["avg_rating"], 4.2);
        assert_eq!(v["feedback_by_type"]["thumbs_up"], 10);
    }

    #[test]
    fn admin_user_role_response_serializes() {
        let resp = AdminUserRoleResponse {
            username: "alice".into(),
            role_name: "admin".into(),
            message: "ok".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["role_name"], "admin");
    }

    #[test]
    fn prompt_optimize_response_serializes() {
        let resp = PromptOptimizeResponse {
            job_id: "j1".into(),
            status: "queued",
            message: "started".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["status"], "queued");
    }

    #[test]
    fn feedback_export_response_serializes() {
        let resp = FeedbackExportResponse {
            job_id: "j1".into(),
            status: "ready",
            download_url: Some("https://example.com/f.jsonl".into()),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["download_url"], "https://example.com/f.jsonl");
    }

    // ── From trait implementations ──────────────────────────────────

    #[test]
    fn admin_token_record_to_response() {
        let record = AdminTokenRecord {
            token_id: "t1".into(),
            token_type: "api_key".into(),
            provider: Some("openai".into()),
            scope: "global".into(),
            scope_id: None,
            created_at: "2024-01-01T00:00:00Z".into(),
        };
        let resp: AdminTokenResponse = record.into();
        assert_eq!(resp.token_id, "t1");
        assert_eq!(resp.token_type, "api_key");
        assert_eq!(resp.provider.as_deref(), Some("openai"));
        assert_eq!(resp.scope, "global");
        assert!(resp.scope_id.is_none());
        assert_eq!(resp.created_at, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn admin_audit_record_to_response_with_details() {
        let details = json!({"reason": "suspicious"});
        let record = AdminAuditRecord {
            log_id: "l1".into(),
            user_id: "u1".into(),
            action: "delete".into(),
            resource_type: "token".into(),
            resource_id: Some("t1".into()),
            timestamp: "2024-06-01T12:00:00Z".into(),
            details: Some(details.clone()),
        };
        let resp: AdminAuditResponse = record.into();
        assert_eq!(resp.log_id, "l1");
        assert_eq!(resp.user_id, "u1");
        assert_eq!(resp.action, "delete");
        assert_eq!(resp.resource_type, "token");
        assert_eq!(resp.resource_id.as_deref(), Some("t1"));
        assert_eq!(resp.timestamp, "2024-06-01T12:00:00Z");
        assert_eq!(resp.details, Some(details));
    }

    #[test]
    fn admin_audit_record_to_response_without_details() {
        let record = AdminAuditRecord {
            log_id: "l2".into(),
            user_id: "u2".into(),
            action: "login".into(),
            resource_type: "user".into(),
            resource_id: None,
            timestamp: "2024-06-01T12:00:00Z".into(),
            details: None,
        };
        let resp: AdminAuditResponse = record.into();
        assert!(resp.resource_id.is_none());
        assert!(resp.details.is_none());
    }

    #[test]
    fn admin_feedback_stats_record_to_response() {
        let mut by_type = Map::new();
        by_type.insert("rating".into(), json!(10));
        let record = AdminFeedbackStatsRecord {
            total_feedback: 100,
            positive_feedback: 80,
            negative_feedback: 20,
            avg_rating: Some(4.5),
            feedback_by_type: by_type.clone(),
        };
        let resp: AdminFeedbackStatsResponse = record.into();
        assert_eq!(resp.total_feedback, 100);
        assert_eq!(resp.positive_feedback, 80);
        assert_eq!(resp.negative_feedback, 20);
        assert_eq!(resp.avg_rating, Some(4.5));
        assert_eq!(resp.feedback_by_type, by_type);
    }

    #[test]
    fn admin_feedback_stats_record_to_response_none_avg() {
        let record = AdminFeedbackStatsRecord {
            total_feedback: 0,
            positive_feedback: 0,
            negative_feedback: 0,
            avg_rating: None,
            feedback_by_type: Map::new(),
        };
        let resp: AdminFeedbackStatsResponse = record.into();
        assert!(resp.avg_rating.is_none());
    }

    #[test]
    fn admin_init_record_to_response() {
        let record = AdminInitRecord {
            message: "Initialized".into(),
            tables_created: 12,
        };
        let resp: AdminInitResponse = record.into();
        assert_eq!(resp.message, "Initialized");
        assert_eq!(resp.tables_created, 12);
    }

    #[test]
    fn admin_user_role_record_to_response() {
        let record = AdminUserRoleRecord {
            username: "bob".into(),
            role_name: "editor".into(),
            message: "Role assigned".into(),
        };
        let resp: AdminUserRoleResponse = record.into();
        assert_eq!(resp.username, "bob");
        assert_eq!(resp.role_name, "editor");
        assert_eq!(resp.message, "Role assigned");
    }

    #[test]
    fn session_record_to_response_all_fields() {
        let mut meta = Map::new();
        meta.insert("env".into(), json!("prod"));
        let record = SessionRecord {
            session_id: "s1".into(),
            user_id: "u1".into(),
            agent_id: Some("a1".into()),
            title: Some("Test Session".into()),
            metadata: meta.clone(),
            status: "active".into(),
            event_count: 42,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: Some("2024-01-02T00:00:00Z".into()),
            ended_at: Some("2024-01-03T00:00:00Z".into()),
        };
        let resp: SessionResponse = record.into();
        assert_eq!(resp.session_id, "s1");
        assert_eq!(resp.user_id, "u1");
        assert_eq!(resp.agent_id.as_deref(), Some("a1"));
        assert_eq!(resp.title.as_deref(), Some("Test Session"));
        assert_eq!(resp.metadata, meta);
        assert_eq!(resp.status, "active");
        assert_eq!(resp.event_count, 42);
        assert_eq!(resp.created_at, "2024-01-01T00:00:00Z");
        assert_eq!(resp.updated_at.as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(resp.ended_at.as_deref(), Some("2024-01-03T00:00:00Z"));
    }

    #[test]
    fn session_record_to_response_optional_none() {
        let record = SessionRecord {
            session_id: "s2".into(),
            user_id: "u2".into(),
            agent_id: None,
            title: None,
            metadata: Map::new(),
            status: "ended".into(),
            event_count: 0,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: None,
            ended_at: None,
        };
        let resp: SessionResponse = record.into();
        assert!(resp.agent_id.is_none());
        assert!(resp.title.is_none());
        assert!(resp.updated_at.is_none());
        assert!(resp.ended_at.is_none());
    }

    #[test]
    fn session_list_record_to_response() {
        let sess = SessionRecord {
            session_id: "s1".into(),
            user_id: "u1".into(),
            agent_id: None,
            title: None,
            metadata: Map::new(),
            status: "active".into(),
            event_count: 1,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: None,
            ended_at: None,
        };
        let record = SessionListRecord {
            sessions: vec![sess],
            total: 1,
            limit: 50,
            offset: 0,
        };
        let resp: SessionListResponse = record.into();
        assert_eq!(resp.sessions.len(), 1);
        assert_eq!(resp.sessions[0].session_id, "s1");
        assert_eq!(resp.total, 1);
        assert_eq!(resp.limit, 50);
        assert_eq!(resp.offset, 0);
    }

    #[test]
    fn session_list_record_to_response_empty() {
        let record = SessionListRecord {
            sessions: vec![],
            total: 0,
            limit: 20,
            offset: 10,
        };
        let resp: SessionListResponse = record.into();
        assert!(resp.sessions.is_empty());
        assert_eq!(resp.total, 0);
        assert_eq!(resp.limit, 20);
        assert_eq!(resp.offset, 10);
    }

    #[test]
    fn chat_run_record_to_response_with_explain() {
        let explain = json!({"candidates": [{"skill": "math", "score": 0.9}]});
        let record = ChatRunRecord {
            session_id: "s1".into(),
            run_id: "r1".into(),
            status: "completed".into(),
            explain: Some(explain.clone()),
        };
        let resp: ChatResponse = record.into();
        assert_eq!(resp.session_id, "s1");
        assert_eq!(resp.run_id, "r1");
        assert_eq!(resp.status, "completed");
        assert_eq!(resp.explain, Some(explain));
    }

    #[test]
    fn chat_run_record_to_response_without_explain() {
        let record = ChatRunRecord {
            session_id: "s1".into(),
            run_id: "r1".into(),
            status: "running".into(),
            explain: None,
        };
        let resp: ChatResponse = record.into();
        assert!(resp.explain.is_none());
    }

    #[test]
    fn run_status_record_to_response_with_waiting_for() {
        let record = RunStatusRecord {
            run_id: "r1".into(),
            session_id: "s1".into(),
            status: "waiting".into(),
            waiting_for: Some("tool_call".into()),
            events_count: 7,
        };
        let resp: RunStatusResponse = record.into();
        assert_eq!(resp.run_id, "r1");
        assert_eq!(resp.session_id, "s1");
        assert_eq!(resp.status, "waiting");
        assert_eq!(resp.waiting_for.as_deref(), Some("tool_call"));
        assert_eq!(resp.events_count, 7);
    }

    #[test]
    fn run_status_record_to_response_without_waiting_for() {
        let record = RunStatusRecord {
            run_id: "r2".into(),
            session_id: "s2".into(),
            status: "completed".into(),
            waiting_for: None,
            events_count: 10,
        };
        let resp: RunStatusResponse = record.into();
        assert!(resp.waiting_for.is_none());
    }

    #[test]
    fn cancel_run_record_to_response() {
        let record = CancelRunRecord {
            run_id: "r1".into(),
            status: "cancelled".into(),
        };
        let resp: CancelRunResponse = record.into();
        assert_eq!(resp.run_id, "r1");
        assert_eq!(resp.status, "cancelled");
    }

    #[test]
    fn auth_user_record_to_response_with_display_name() {
        let record = AuthUserRecord {
            user_id: "u1".into(),
            username: "alice".into(),
            email: "alice@example.com".into(),
            display_name: Some("Alice W.".into()),
        };
        let resp: AuthUserResponse = record.into();
        assert_eq!(resp.user_id, "u1");
        assert_eq!(resp.username, "alice");
        assert_eq!(resp.email, "alice@example.com");
        assert_eq!(resp.display_name.as_deref(), Some("Alice W."));
    }

    #[test]
    fn auth_user_record_to_response_without_display_name() {
        let record = AuthUserRecord {
            user_id: "u2".into(),
            username: "bob".into(),
            email: "bob@example.com".into(),
            display_name: None,
        };
        let resp: AuthUserResponse = record.into();
        assert!(resp.display_name.is_none());
    }

    #[test]
    fn auth_token_record_to_response() {
        let record = AuthTokenRecord {
            access_token: "access123".into(),
            refresh_token: "refresh456".into(),
            token_type: "Bearer".into(),
            expires_in: 7200,
        };
        let resp: AuthTokenResponse = record.into();
        assert_eq!(resp.access_token, "access123");
        assert_eq!(resp.refresh_token, "refresh456");
        assert_eq!(resp.token_type, "Bearer");
        assert_eq!(resp.expires_in, 7200);
    }

    // ── chat_request_into_data ──────────────────────────────────────

    #[test]
    fn chat_request_into_data_maps_all_fields() {
        let mut ctx = Map::new();
        ctx.insert("tool".into(), json!("calc"));
        let req = ChatRequest {
            message: "hello".into(),
            session_id: Some("s1".into()),
            agent_id: Some("a1".into()),
            model: Some("gpt-4".into()),
            context: Some(ctx.clone()),
            max_candidates: 3,
            explain: true,
        };
        let data = chat_request_into_data(req);
        assert_eq!(data.message, "hello");
        assert_eq!(data.session_id.as_deref(), Some("s1"));
        assert_eq!(data.agent_id.as_deref(), Some("a1"));
        assert_eq!(data.model.as_deref(), Some("gpt-4"));
        assert_eq!(data.context, Some(ctx));
        assert_eq!(data.max_candidates, 3);
        assert!(data.explain);
    }

    #[test]
    fn chat_request_into_data_maps_defaults() {
        let req: ChatRequest = serde_json::from_str(r#"{"message":"test"}"#).unwrap();
        let data = chat_request_into_data(req);
        assert_eq!(data.message, "test");
        assert!(data.session_id.is_none());
        assert!(data.agent_id.is_none());
        assert!(data.model.is_none());
        assert!(data.context.is_none());
        assert_eq!(data.max_candidates, 5);
        assert!(!data.explain);
    }
}
