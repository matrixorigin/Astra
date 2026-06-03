use astra_core::ErrorResponse;
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};

pub const ADMIN_TOKEN_SCOPE_GLOBAL: &str = "global";
pub const ADMIN_TOKEN_SCOPE_REPO: &str = "repo";
pub const ADMIN_TOKEN_SCOPE_USER: &str = "user";
pub const ADMIN_TOKEN_TYPE_API_KEY: &str = "api_key";
pub const ADMIN_TOKEN_PROVIDER_TAAS: &str = "taas";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub username: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdminTokenFilter {
    pub token_type: Option<String>,
    pub provider: Option<String>,
    pub scope: Option<String>,
    pub scope_id: Option<String>,
}

impl AdminTokenFilter {
    pub fn taas_user_key(scope_id: Option<String>) -> Self {
        Self {
            token_type: Some(ADMIN_TOKEN_TYPE_API_KEY.to_string()),
            provider: Some(ADMIN_TOKEN_PROVIDER_TAAS.to_string()),
            scope: Some(ADMIN_TOKEN_SCOPE_USER.to_string()),
            scope_id,
        }
    }
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

impl AdminTokenCreateRequestData {
    pub fn taas_user_key(user_id: String, token_value: String) -> Self {
        Self {
            token_type: ADMIN_TOKEN_TYPE_API_KEY.to_string(),
            provider: Some(ADMIN_TOKEN_PROVIDER_TAAS.to_string()),
            scope: ADMIN_TOKEN_SCOPE_USER.to_string(),
            scope_id: Some(user_id),
            token_value: Some(token_value),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.token_type.trim().is_empty() {
            return Err("token_type is required".to_string());
        }

        match self.scope.as_str() {
            ADMIN_TOKEN_SCOPE_GLOBAL => {
                if match self.scope_id.as_deref() {
                    Some(scope_id) => !scope_id.trim().is_empty(),
                    None => false,
                } {
                    return Err("scope_id must be omitted for global tokens".to_string());
                }
            }
            ADMIN_TOKEN_SCOPE_USER | ADMIN_TOKEN_SCOPE_REPO => {
                if match self.scope_id.as_deref() {
                    Some(scope_id) => scope_id.trim().is_empty(),
                    None => true,
                } {
                    return Err(format!("scope_id is required for {} tokens", self.scope));
                }
            }
            _ => {
                return Err("scope must be one of global, user, or repo".to_string());
            }
        }

        if self.provider.as_deref() == Some(ADMIN_TOKEN_PROVIDER_TAAS) {
            if self.token_type != ADMIN_TOKEN_TYPE_API_KEY {
                return Err("TAAS token bindings must use token_type api_key".to_string());
            }
            if self.scope != ADMIN_TOKEN_SCOPE_USER {
                return Err("TAAS token bindings must use user scope".to_string());
            }
            if match self.token_value.as_deref() {
                Some(token_value) => token_value.trim().is_empty(),
                None => true,
            } {
                return Err("TAAS token bindings require token_value".to_string());
            }
        }

        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taas_user_key_filter_uses_binding_convention() {
        let filter = AdminTokenFilter::taas_user_key(Some("u123".to_string()));

        assert_eq!(filter.token_type.as_deref(), Some(ADMIN_TOKEN_TYPE_API_KEY));
        assert_eq!(filter.provider.as_deref(), Some(ADMIN_TOKEN_PROVIDER_TAAS));
        assert_eq!(filter.scope.as_deref(), Some(ADMIN_TOKEN_SCOPE_USER));
        assert_eq!(filter.scope_id.as_deref(), Some("u123"));
    }

    #[test]
    fn taas_user_key_request_is_valid() {
        let request =
            AdminTokenCreateRequestData::taas_user_key("u123".to_string(), "taas-key".to_string());

        assert_eq!(request.token_type, ADMIN_TOKEN_TYPE_API_KEY);
        assert_eq!(request.provider.as_deref(), Some(ADMIN_TOKEN_PROVIDER_TAAS));
        assert_eq!(request.scope, ADMIN_TOKEN_SCOPE_USER);
        assert_eq!(request.scope_id.as_deref(), Some("u123"));
        assert_eq!(request.token_value.as_deref(), Some("taas-key"));
        assert!(request.validate().is_ok());
    }

    #[test]
    fn global_token_without_scope_id_is_valid() {
        let request = AdminTokenCreateRequestData {
            token_type: "llm".to_string(),
            provider: Some("openai".to_string()),
            scope: ADMIN_TOKEN_SCOPE_GLOBAL.to_string(),
            scope_id: None,
            token_value: None,
        };

        assert!(request.validate().is_ok());
    }

    #[test]
    fn user_token_requires_scope_id() {
        let request = AdminTokenCreateRequestData {
            token_type: "api_key".to_string(),
            provider: Some("github".to_string()),
            scope: ADMIN_TOKEN_SCOPE_USER.to_string(),
            scope_id: None,
            token_value: Some("key".to_string()),
        };

        assert_eq!(
            request.validate().unwrap_err(),
            "scope_id is required for user tokens"
        );
    }

    #[test]
    fn global_token_rejects_scope_id() {
        let request = AdminTokenCreateRequestData {
            token_type: "llm".to_string(),
            provider: Some("openai".to_string()),
            scope: ADMIN_TOKEN_SCOPE_GLOBAL.to_string(),
            scope_id: Some("u123".to_string()),
            token_value: None,
        };

        assert_eq!(
            request.validate().unwrap_err(),
            "scope_id must be omitted for global tokens"
        );
    }

    #[test]
    fn taas_token_requires_user_scope_and_value() {
        let missing_value = AdminTokenCreateRequestData {
            token_type: ADMIN_TOKEN_TYPE_API_KEY.to_string(),
            provider: Some(ADMIN_TOKEN_PROVIDER_TAAS.to_string()),
            scope: ADMIN_TOKEN_SCOPE_USER.to_string(),
            scope_id: Some("u123".to_string()),
            token_value: None,
        };
        assert_eq!(
            missing_value.validate().unwrap_err(),
            "TAAS token bindings require token_value"
        );

        let repo_scoped = AdminTokenCreateRequestData {
            token_type: ADMIN_TOKEN_TYPE_API_KEY.to_string(),
            provider: Some(ADMIN_TOKEN_PROVIDER_TAAS.to_string()),
            scope: ADMIN_TOKEN_SCOPE_REPO.to_string(),
            scope_id: Some("repo".to_string()),
            token_value: Some("key".to_string()),
        };
        assert_eq!(
            repo_scoped.validate().unwrap_err(),
            "TAAS token bindings must use user scope"
        );
    }
}
