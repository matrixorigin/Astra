use super::encryption::FernetTokenEncryptor;
use super::jwt::decode_jwt_claims;
use crate::admin::{
    AdminAuditFilter, AdminAuditReader, AdminAuditRecord, AdminAuthorizer, AdminInitRecord,
    AdminInitializer, AdminTokenCreateRequestData, AdminTokenFilter, AdminTokenReader,
    AdminTokenRecord, AdminTokenWriter, AdminUserRoleManager, AdminUserRoleRecord,
    AdminUserRoleRequestData, AuthenticatedUser,
};
use crate::pagination::clamp_admin_audit_limit;
use astra_core::{
    ErrorResponse, JwtSettings, MatrixOneSettings, SharedPool, bearer_token, error_response,
    internal_error,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use sqlx::{MySql, QueryBuilder, Row, query};
use uuid::Uuid;

fn required_admin_string(
    row: &sqlx::mysql::MySqlRow,
    table: &'static str,
    column: &'static str,
) -> Result<String, sqlx::Error> {
    let value: String = row.try_get(column)?;
    if value.trim().is_empty() {
        return Err(sqlx::Error::ColumnDecode {
            index: column.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid {table}.{column}: value is empty"),
            )
            .into(),
        });
    }
    Ok(value)
}

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

    async fn admin_role_for_user(
        &self,
        user_id: &str,
        username: Option<&str>,
    ) -> Result<Option<i16>, sqlx::Error> {
        let pool = self.get_pool().await?;
        sqlx::query_scalar(
            "SELECT is_admin FROM auth_users
             WHERE user_id = ? OR username = ?
             ORDER BY is_admin DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(username)
        .fetch_optional(&pool)
        .await
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseAdminAuthorizer",
            &self.matrixone,
        )
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

        let is_admin = self
            .admin_role_for_user(&user_id, username.as_deref())
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "User not found"))?;
        if is_admin != 1 {
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseAdminTokenReader",
            &self.matrixone,
        )
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseAdminTokenWriter",
            &self.matrixone,
        )
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseAdminAuditReader",
            &self.matrixone,
        )
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseAdminInitializer",
            &self.matrixone,
        )
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
            .and_then(|row| {
                row.map(|row| required_admin_string(&row, "auth_users", "user_id"))
                    .transpose()
            })
    }

    async fn set_role(
        &self,
        request: AdminUserRoleRequestData,
        grant: bool,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let user_id = self
            .lookup_user_id(&pool, &request.username)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User not found"))?;
        let column = role_column(&request.role_name)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Role not found"))?;
        let sql = format!("UPDATE auth_users SET {column} = ? WHERE user_id = ? AND {column} <> ?");
        let changed = query(sqlx::AssertSqlSafe(sql))
            .bind(i16::from(grant))
            .bind(user_id)
            .bind(i16::from(grant))
            .execute(&pool)
            .await
            .map_err(internal_error)?
            .rows_affected()
            == 1;
        let message = match (grant, changed) {
            (true, true) => "Role granted successfully",
            (true, false) => "User already has this role",
            (false, true) => "Role revoked successfully",
            (false, false) => "User does not have this role",
        };
        Ok(AdminUserRoleRecord {
            username: request.username,
            role_name: request.role_name,
            message: message.to_string(),
        })
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseAdminUserRoleManager",
            &self.matrixone,
        )
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
        mut filter: AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRecord>, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        filter.limit = clamp_admin_audit_limit(filter.limit);
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
impl AdminInitializer for DatabaseAdminInitializer {
    async fn initialize(&self) -> Result<AdminInitRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        query("SELECT 1")
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        Ok(AdminInitRecord {
            message: "Database initialized successfully".to_string(),
            tables_created: 0,
        })
    }
}

fn role_column(role_name: &str) -> Option<&'static str> {
    match role_name {
        "astra_admin" => Some("is_admin"),
        "astra_user" => Some("has_user_role"),
        _ => None,
    }
}

#[async_trait]
impl AdminUserRoleManager for DatabaseAdminUserRoleManager {
    async fn has_role_members(
        &self,
        role_name: &str,
    ) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
        let Some(column) = role_column(role_name) else {
            return Ok(false);
        };
        let pool = self.get_pool().await.map_err(internal_error)?;
        let sql = format!("SELECT 1 FROM auth_users WHERE {column} = 1 LIMIT 1");
        Ok(query(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?
            .is_some())
    }

    async fn grant_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        self.set_role(request, true).await
    }

    async fn revoke_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, Json<ErrorResponse>)> {
        self.set_role(request, false).await
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
pub struct UnconfiguredAdminUserRoleManager;

#[async_trait]
impl AdminUserRoleManager for UnconfiguredAdminUserRoleManager {
    async fn has_role_members(
        &self,
        _role_name: &str,
    ) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Admin user role manager not configured",
        ))
    }

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
