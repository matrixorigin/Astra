use super::encryption::FernetTokenEncryptor;
use super::jwt::decode_jwt_claims;
use crate::admin::AdminAuditReader;
use crate::admin::{
    AdminAuditFilter, AdminAuditRecord, AdminAuthorizer, AdminFeedbackStatsFilter,
    AdminFeedbackStatsReader, AdminFeedbackStatsRecord, AdminInitRecord, AdminInitializer,
    AdminTokenCreateRequestData, AdminTokenFilter, AdminTokenReader, AdminTokenRecord,
    AdminTokenWriter, AdminUserRoleManager, AdminUserRoleRecord, AdminUserRoleRequestData,
    AuthenticatedUser,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::{
    ErrorResponse, JwtSettings, MatrixOneSettings, SharedPool, bearer_token, connect_matrixone,
    error_response, internal_error,
};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct DatabaseAdminAuthorizer {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    jwt: JwtSettings,
}

impl DatabaseAdminAuthorizer {
    pub fn new(matrixone: MatrixOneSettings, jwt: JwtSettings) -> Self {
        Self {
            matrixone,
            jwt,
            pool: None,
        }
    }

    async fn query_user_exists(
        &self,
        user_id: &str,
        username: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let pool = self.get_pool().await?;
        let exists = if let Some(username) = username {
            query("SELECT 1 FROM auth_users WHERE user_id = ? OR username = ? LIMIT 1")
                .bind(user_id)
                .bind(username)
                .fetch_optional(&pool)
                .await?
                .is_some()
        } else {
            query("SELECT 1 FROM auth_users WHERE user_id = ? LIMIT 1")
                .bind(user_id)
                .fetch_optional(&pool)
                .await?
                .is_some()
        };
        Ok(exists)
    }

    async fn query_has_role(
        &self,
        user_id: &str,
        username: Option<&str>,
        role_name: &str,
    ) -> Result<bool, sqlx::Error> {
        let pool = self.get_pool().await?;
        let has_role = if let Some(username) = username {
            query(
                r#"
                SELECT 1
                FROM auth_user_roles ur
                JOIN auth_roles r ON ur.role_id = r.role_id
                JOIN auth_users u ON ur.user_id = u.user_id
                WHERE r.role_name = ? AND (u.user_id = ? OR u.username = ?)
                LIMIT 1
                "#,
            )
            .bind(role_name)
            .bind(user_id)
            .bind(username)
            .fetch_optional(&pool)
            .await?
            .is_some()
        } else {
            query(
                r#"
                SELECT 1
                FROM auth_user_roles ur
                JOIN auth_roles r ON ur.role_id = r.role_id
                JOIN auth_users u ON ur.user_id = u.user_id
                WHERE r.role_name = ? AND u.user_id = ?
                LIMIT 1
                "#,
            )
            .bind(role_name)
            .bind(user_id)
            .fetch_optional(&pool)
            .await?
            .is_some()
        };
        Ok(has_role)
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl AdminAuthorizer for DatabaseAdminAuthorizer {
    async fn require_admin(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, Json<ErrorResponse>)> {
        let token = bearer_token(headers)?;
        let claims = decode_jwt_claims(token, &self.jwt)?;

        if claims.token_type.as_deref() != Some("access") {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid token type",
            ));
        }

        let user_id = claims
            .sub
            .clone()
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Invalid token"))?;
        let username = claims.username.clone();

        if !self
            .query_user_exists(&user_id, username.as_deref())
            .await
            .map_err(internal_error)?
        {
            return Err(error_response(StatusCode::UNAUTHORIZED, "User not found"));
        }

        let has_admin = self
            .query_has_role(&user_id, username.as_deref(), "mo_agent_admin")
            .await
            .map_err(internal_error)?;

        if !has_admin {
            return Err(error_response(StatusCode::FORBIDDEN, "Admin role required"));
        }

        Ok(AuthenticatedUser { user_id, username })
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseAdminTokenReader {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAdminTokenReader {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[derive(Clone)]
pub struct DatabaseAdminTokenWriter {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    encryptor: FernetTokenEncryptor,
}

impl DatabaseAdminTokenWriter {
    pub fn from_env(matrixone: MatrixOneSettings) -> Result<Self, String> {
        Ok(Self {
            matrixone,
            encryptor: FernetTokenEncryptor::from_env()?,
            pool: None,
        })
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseAdminAuditReader {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAdminAuditReader {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseAdminFeedbackStatsReader {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAdminFeedbackStatsReader {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseAdminInitializer {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAdminInitializer {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseAdminUserRoleManager {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseAdminUserRoleManager {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    async fn lookup_user_id(
        &self,
        pool: &sqlx::Pool<MySql>,
        username: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        query("SELECT user_id FROM auth_users WHERE username = ? LIMIT 1")
            .bind(username)
            .fetch_optional(pool)
            .await
            .map(|row| row.and_then(|row| row.try_get("user_id").ok()))
    }

    async fn lookup_role_id(
        &self,
        pool: &sqlx::Pool<MySql>,
        role_name: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        query("SELECT role_id FROM auth_roles WHERE role_name = ? LIMIT 1")
            .bind(role_name)
            .fetch_optional(pool)
            .await
            .map(|row| row.and_then(|row| row.try_get("role_id").ok()))
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl AdminTokenReader for DatabaseAdminTokenReader {
    async fn list_tokens(
        &self,
        filter: AdminTokenFilter,
    ) -> Result<Vec<AdminTokenRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let mut query_builder = QueryBuilder::<MySql>::new(
            "SELECT token_id, type, provider, scope_user_id, scope_repo, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM auth_tokens",
        );

        let mut has_where = false;
        if let Some(token_type) = filter.token_type {
            query_builder.push(" WHERE type = ");
            query_builder.push_bind(token_type);
            has_where = true;
        }

        if let Some(scope) = filter.scope.as_deref() {
            let clause = match scope {
                "user" => Some("scope_user_id IS NOT NULL"),
                "repo" => Some("scope_repo IS NOT NULL"),
                "global" => Some("scope_user_id IS NULL AND scope_repo IS NULL"),
                _ => None,
            };
            if let Some(clause) = clause {
                query_builder.push(if has_where { " AND " } else { " WHERE " });
                query_builder.push(clause);
            }
        }

        query_builder.push(" ORDER BY created_at DESC");
        let rows = query_builder
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        rows.into_iter()
            .map(|row| {
                let scope_user_id = row.try_get::<Option<String>, _>("scope_user_id")?;
                let scope_repo = row.try_get::<Option<String>, _>("scope_repo")?;
                Ok(AdminTokenRecord {
                    token_id: row.try_get("token_id")?,
                    token_type: row.try_get("type")?,
                    provider: Some(row.try_get::<String, _>("provider")?),
                    scope: if scope_user_id.is_some() {
                        "user".to_string()
                    } else if scope_repo.is_some() {
                        "repo".to_string()
                    } else {
                        "global".to_string()
                    },
                    scope_id: scope_user_id.or(scope_repo),
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(internal_error)
    }
}

#[async_trait]
impl AdminTokenWriter for DatabaseAdminTokenWriter {
    async fn create_token(
        &self,
        request: AdminTokenCreateRequestData,
    ) -> Result<AdminTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        let token_id = Uuid::new_v4().to_string();
        let provider = request.provider.unwrap_or_else(|| "unknown".to_string());
        let encrypted_value = request
            .token_value
            .as_deref()
            .map(|value| self.encryptor.encrypt(value))
            .transpose()
            .map_err(internal_error)?;
        let scope_user_id = if request.scope == "user" {
            request.scope_id.clone()
        } else {
            None
        };
        let scope_repo = if request.scope == "repo" {
            request.scope_id.clone()
        } else {
            None
        };
        let scope = request.scope;

        let pool = self.get_pool().await.map_err(internal_error)?;

        query(
            r#"
            INSERT INTO auth_tokens
            (token_id, type, provider, encrypted_value, is_active, scope_user_id, scope_repo, metadata)
            VALUES (?, ?, ?, ?, 1, ?, ?, ?)
            "#,
        )
        .bind(&token_id)
        .bind(&request.token_type)
        .bind(&provider)
        .bind(&encrypted_value)
        .bind(&scope_user_id)
        .bind(&scope_repo)
        .bind(serde_json::json!({ "scope": scope.clone() }).to_string())
        .execute(&pool)
        .await
        .map_err(internal_error)?;

        let row = query(
            "SELECT token_id, type, provider, scope_user_id, scope_repo, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM auth_tokens WHERE token_id = ? LIMIT 1",
        )
        .bind(&token_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let persisted_scope_user_id = row
            .try_get::<Option<String>, _>("scope_user_id")
            .map_err(internal_error)?;
        let persisted_scope_repo = row
            .try_get::<Option<String>, _>("scope_repo")
            .map_err(internal_error)?;

        Ok(AdminTokenRecord {
            token_id: row.try_get("token_id").map_err(internal_error)?,
            token_type: row.try_get("type").map_err(internal_error)?,
            provider: Some(
                row.try_get::<String, _>("provider")
                    .map_err(internal_error)?,
            ),
            scope: if persisted_scope_user_id.is_some() {
                "user".to_string()
            } else if persisted_scope_repo.is_some() {
                "repo".to_string()
            } else {
                "global".to_string()
            },
            scope_id: persisted_scope_user_id.or(persisted_scope_repo),
            created_at: row.try_get("created_at").map_err(internal_error)?,
        })
    }
}

#[async_trait]
impl AdminAuditReader for DatabaseAdminAuditReader {
    async fn list_audit_logs(
        &self,
        filter: AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let mut query_builder = QueryBuilder::<MySql>::new(
            "SELECT log_id, user_id, action, resource_type, resource_id, \
             IFNULL(CAST(details AS CHAR), 'null') AS details_json, \
             DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM auth_audit_logs",
        );

        let mut has_where = false;
        if let Some(user_id) = filter.user_id {
            query_builder.push(" WHERE user_id = ");
            query_builder.push_bind(user_id);
            has_where = true;
        }
        if let Some(since) = filter.since {
            query_builder.push(if has_where {
                " AND created_at >= "
            } else {
                " WHERE created_at >= "
            });
            query_builder.push_bind(since);
        }

        query_builder.push(" ORDER BY created_at DESC LIMIT ");
        query_builder.push_bind(i64::from(filter.limit));

        let rows = query_builder
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut logs = Vec::with_capacity(rows.len());
        for row in rows {
            let details_json: String = row.try_get("details_json").map_err(internal_error)?;
            let details =
                serde_json::from_str::<serde_json::Value>(&details_json).map_err(internal_error)?;
            logs.push(AdminAuditRecord {
                log_id: row.try_get("log_id").map_err(internal_error)?,
                user_id: row.try_get("user_id").map_err(internal_error)?,
                action: row.try_get("action").map_err(internal_error)?,
                resource_type: row.try_get("resource_type").map_err(internal_error)?,
                resource_id: row.try_get("resource_id").map_err(internal_error)?,
                timestamp: row.try_get("created_at").map_err(internal_error)?,
                details: match details {
                    serde_json::Value::Null => None,
                    value => Some(value),
                },
            });
        }

        Ok(logs)
    }
}

#[async_trait]
impl AdminFeedbackStatsReader for DatabaseAdminFeedbackStatsReader {
    async fn read_feedback_stats(
        &self,
        filter: AdminFeedbackStatsFilter,
    ) -> Result<AdminFeedbackStatsRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let mut summary_query = QueryBuilder::<MySql>::new(
            "SELECT COUNT(feedback_id) AS total_feedback, \
             SUM(IF(rating >= 4, 1, 0)) AS positive_feedback, \
             SUM(IF(rating <= 2, 1, 0)) AS negative_feedback, \
             AVG(rating) AS avg_rating \
             FROM eval_user_feedback",
        );
        let mut type_query = QueryBuilder::<MySql>::new(
            "SELECT feedback_type, COUNT(feedback_id) AS type_count \
             FROM eval_user_feedback WHERE feedback_type IS NOT NULL",
        );

        let has_summary_where = filter.agent_id.is_some();
        if let Some(agent_id) = filter.agent_id {
            summary_query.push(" WHERE agent_id = ");
            summary_query.push_bind(agent_id.clone());

            type_query.push(" AND agent_id = ");
            type_query.push_bind(agent_id);
        }
        if let Some(since) = filter.since {
            summary_query.push(if has_summary_where {
                " AND created_at >= "
            } else {
                " WHERE created_at >= "
            });
            summary_query.push_bind(since.clone());
            type_query.push(" AND created_at >= ");
            type_query.push_bind(since);
        }

        type_query.push(" GROUP BY feedback_type");

        let summary_row = summary_query
            .build()
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let type_rows = type_query
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let mut feedback_by_type = serde_json::Map::new();
        for row in type_rows {
            let feedback_type: String = row.try_get("feedback_type").map_err(internal_error)?;
            let count: i64 = row.try_get("type_count").map_err(internal_error)?;
            feedback_by_type.insert(feedback_type, serde_json::Value::from(count));
        }

        Ok(AdminFeedbackStatsRecord {
            total_feedback: summary_row.try_get::<i64, _>("total_feedback").unwrap_or(0),
            positive_feedback: summary_row
                .try_get::<Option<i64>, _>("positive_feedback")
                .unwrap_or(None)
                .unwrap_or(0),
            negative_feedback: summary_row
                .try_get::<Option<i64>, _>("negative_feedback")
                .unwrap_or(None)
                .unwrap_or(0),
            avg_rating: summary_row
                .try_get::<Option<f64>, _>("avg_rating")
                .unwrap_or(None),
            feedback_by_type,
        })
    }
}

#[async_trait]
impl AdminInitializer for DatabaseAdminInitializer {
    async fn initialize(&self) -> Result<AdminInitRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query("SELECT 1")
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        for (role_id, role_name, description) in [
            (
                "role-admin",
                "mo_agent_admin",
                "Administrator with full system access",
            ),
            (
                "role-user",
                "mo_agent_user",
                "Regular user with limited access",
            ),
        ] {
            query(
                "INSERT IGNORE INTO auth_roles (role_id, role_name, description) VALUES (?, ?, ?)",
            )
            .bind(role_id)
            .bind(role_name)
            .bind(description)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        }

        Ok(AdminInitRecord {
            message: "Database initialized successfully".to_string(),
            tables_created: 0,
        })
    }
}

#[async_trait]
impl AdminUserRoleManager for DatabaseAdminUserRoleManager {
    async fn grant_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let user_id = self
            .lookup_user_id(&pool, &request.username)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User not found"))?;
        let role_id = self
            .lookup_role_id(&pool, &request.role_name)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Role not found"))?;

        let existing =
            query("SELECT 1 FROM auth_user_roles WHERE user_id = ? AND role_id = ? LIMIT 1")
                .bind(&user_id)
                .bind(&role_id)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?
                .is_some();

        if existing {
            return Ok(AdminUserRoleRecord {
                username: request.username,
                role_name: request.role_name,
                message: "User already has this role".to_string(),
            });
        }

        query("INSERT INTO auth_user_roles (user_id, role_id) VALUES (?, ?)")
            .bind(&user_id)
            .bind(&role_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(AdminUserRoleRecord {
            username: request.username,
            role_name: request.role_name,
            message: "Role granted successfully".to_string(),
        })
    }

    async fn revoke_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let user_id = self
            .lookup_user_id(&pool, &request.username)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User not found"))?;
        let role_id = self
            .lookup_role_id(&pool, &request.role_name)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Role not found"))?;

        let result = query("DELETE FROM auth_user_roles WHERE user_id = ? AND role_id = ?")
            .bind(&user_id)
            .bind(&role_id)
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(AdminUserRoleRecord {
            username: request.username,
            role_name: request.role_name,
            message: if result.rows_affected() > 0 {
                "Role revoked successfully".to_string()
            } else {
                "User does not have this role".to_string()
            },
        })
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminAuthorizer;

#[async_trait]
impl AdminAuthorizer for UnconfiguredAdminAuthorizer {
    async fn require_admin(
        &self,
        _headers: &HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin auth not configured",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminInitializer;

#[async_trait]
impl AdminInitializer for UnconfiguredAdminInitializer {
    async fn initialize(&self) -> Result<AdminInitRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin initializer not configured",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminTokenReader;

#[async_trait]
impl AdminTokenReader for UnconfiguredAdminTokenReader {
    async fn list_tokens(
        &self,
        _filter: AdminTokenFilter,
    ) -> Result<Vec<AdminTokenRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin token reader not configured",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminTokenWriter;

#[async_trait]
impl AdminTokenWriter for UnconfiguredAdminTokenWriter {
    async fn create_token(
        &self,
        _request: AdminTokenCreateRequestData,
    ) -> Result<AdminTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin token writer not configured",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminAuditReader;

#[async_trait]
impl AdminAuditReader for UnconfiguredAdminAuditReader {
    async fn list_audit_logs(
        &self,
        _filter: AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin audit reader not configured",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminFeedbackStatsReader;

#[async_trait]
impl AdminFeedbackStatsReader for UnconfiguredAdminFeedbackStatsReader {
    async fn read_feedback_stats(
        &self,
        _filter: AdminFeedbackStatsFilter,
    ) -> Result<AdminFeedbackStatsRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin feedback stats reader not configured",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAdminUserRoleManager;

#[async_trait]
impl AdminUserRoleManager for UnconfiguredAdminUserRoleManager {
    async fn grant_role(
        &self,
        _request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin user role manager not configured",
        ))
    }

    async fn revoke_role(
        &self,
        _request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin user role manager not configured",
        ))
    }
}
