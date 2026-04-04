use astra_core::ErrorResponse;
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub username: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdminTokenFilter {
    pub token_type: Option<String>,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminTokenRecord {
    pub token_id: String,
    pub token_type: String,
    pub provider: Option<String>,
    pub scope: String,
    pub scope_id: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminTokenCreateRequestData {
    pub token_type: String,
    pub provider: Option<String>,
    pub scope: String,
    pub scope_id: Option<String>,
    pub token_value: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdminAuditFilter {
    pub user_id: Option<String>,
    pub since: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdminAuditRecord {
    pub log_id: String,
    pub user_id: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub timestamp: String,
    pub details: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdminFeedbackStatsFilter {
    pub agent_id: Option<String>,
    pub since: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdminFeedbackStatsRecord {
    pub total_feedback: i64,
    pub positive_feedback: i64,
    pub negative_feedback: i64,
    pub avg_rating: Option<f64>,
    pub feedback_by_type: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminInitRecord {
    pub message: String,
    pub tables_created: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminUserRoleRequestData {
    pub username: String,
    pub role_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminUserRoleRecord {
    pub username: String,
    pub role_name: String,
    pub message: String,
}

#[async_trait]
pub trait AdminAuthorizer: Send + Sync {
    async fn require_admin(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, Json<ErrorResponse>)>;
}

#[async_trait]
pub trait AdminInitializer: Send + Sync {
    async fn initialize(&self) -> Result<AdminInitRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[async_trait]
pub trait AdminTokenReader: Send + Sync {
    async fn list_tokens(
        &self,
        filter: AdminTokenFilter,
    ) -> Result<Vec<AdminTokenRecord>, (StatusCode, Json<ErrorResponse>)>;
}

#[async_trait]
pub trait AdminTokenWriter: Send + Sync {
    async fn create_token(
        &self,
        request: AdminTokenCreateRequestData,
    ) -> Result<AdminTokenRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[async_trait]
pub trait AdminAuditReader: Send + Sync {
    async fn list_audit_logs(
        &self,
        filter: AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRecord>, (StatusCode, Json<ErrorResponse>)>;
}

#[async_trait]
pub trait AdminFeedbackStatsReader: Send + Sync {
    async fn read_feedback_stats(
        &self,
        filter: AdminFeedbackStatsFilter,
    ) -> Result<AdminFeedbackStatsRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[async_trait]
pub trait AdminUserRoleManager: Send + Sync {
    async fn grant_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn revoke_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)>;
}
