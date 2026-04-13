use crate::storage::database_user_from_row;
use astra_core::{
    ErrorResponse, JwtSettings, MatrixOneSettings, SharedPool, bearer_token, connect_matrixone,
    error_response, internal_error,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use bcrypt::{hash as bcrypt_hash, verify as bcrypt_verify};
use chrono::{Duration as ChronoDuration, Utc};
use sqlx::{MySql, Row, query};
use uuid::Uuid;

mod admin;
mod encryption;
mod jwt;
pub mod session;
mod validation;

pub use admin::{
    DatabaseAdminAuditReader, DatabaseAdminAuthorizer, DatabaseAdminFeedbackStatsReader,
    DatabaseAdminInitializer, DatabaseAdminTokenReader, DatabaseAdminTokenWriter,
    DatabaseAdminUserRoleManager,
};
pub use admin::{
    UnconfiguredAdminAuditReader, UnconfiguredAdminAuthorizer,
    UnconfiguredAdminFeedbackStatsReader, UnconfiguredAdminInitializer,
    UnconfiguredAdminTokenReader, UnconfiguredAdminTokenWriter, UnconfiguredAdminUserRoleManager,
};
pub use encryption::FernetTokenEncryptor;
use encryption::sha256_hex;
use jwt::{JwtTokenClaims, create_jwt_token, decode_jwt_claims, decode_jwt_claims_with_detail};
pub use session::UnconfiguredSessionService;
pub use session::{
    DatabaseSessionService, SessionActivityRecord, SessionCreateRequestData, SessionListFilter,
    SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
};
use validation::validate_register_request;

fn is_duplicate_key_error(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => {
            // MySQL error code 1062 = ER_DUP_ENTRY
            if db_err.code().as_deref() == Some("1062") {
                return true;
            }
            // Fallback: check error message for "Duplicate entry" pattern
            let msg = db_err.message();
            msg.contains("Duplicate entry") || msg.contains("ER_DUP_ENTRY")
        }
        // Also check Protocol and other wrapped errors
        _ => {
            let msg = err.to_string();
            msg.contains("1062") && msg.contains("Duplicate entry")
        }
    }
}

#[async_trait]
pub trait AuthService: Send + Sync {
    async fn register(
        &self,
        request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn login(
        &self,
        request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn refresh(
        &self,
        request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn logout(
        &self,
        request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)>;

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthRegisterRequestData {
    pub username: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthLoginRequestData {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthRefreshRequestData {
    pub refresh_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthUserRecord {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthTokenRecord {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u32,
}

#[derive(Clone, Debug)]
pub struct DatabaseAuthService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    jwt: JwtSettings,
}

#[derive(Clone, Debug)]
pub struct DatabaseUserRecord {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub display_name: Option<String>,
    pub is_active: bool,
}

impl DatabaseAuthService {
    pub fn new(matrixone: MatrixOneSettings, jwt: JwtSettings) -> Self {
        Self {
            matrixone,
            jwt,
            pool: None,
        }
    }

    async fn ensure_default_roles(&self, pool: &sqlx::Pool<MySql>) -> Result<(), sqlx::Error> {
        for (role_id, role_name, description) in [
            (
                "role-admin",
                "astra_admin",
                "Administrator with full system access",
            ),
            (
                "role-user",
                "astra_user",
                "Regular user with limited access",
            ),
        ] {
            query(
                "INSERT IGNORE INTO auth_roles (role_id, role_name, description) VALUES (?, ?, ?)",
            )
            .bind(role_id)
            .bind(role_name)
            .bind(description)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    async fn fetch_user_by_username(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        username: &str,
    ) -> Result<Option<DatabaseUserRecord>, sqlx::Error> {
        query(
            "SELECT user_id, username, email, password_hash, display_name, is_active \
             FROM auth_users WHERE username = ? LIMIT 1",
        )
        .bind(username)
        .fetch_optional(executor)
        .await
        .map(|row| row.map(database_user_from_row))
    }

    async fn fetch_user_by_email(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        email: &str,
    ) -> Result<Option<DatabaseUserRecord>, sqlx::Error> {
        query(
            "SELECT user_id, username, email, password_hash, display_name, is_active \
             FROM auth_users WHERE email = ? LIMIT 1",
        )
        .bind(email)
        .fetch_optional(executor)
        .await
        .map(|row| row.map(database_user_from_row))
    }

    async fn fetch_user_by_id_or_username(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        user_id: &str,
        username: Option<&str>,
    ) -> Result<Option<DatabaseUserRecord>, sqlx::Error> {
        if let Some(username) = username {
            query(
                "SELECT user_id, username, email, password_hash, display_name, is_active \
                 FROM auth_users WHERE user_id = ? OR username = ? LIMIT 1",
            )
            .bind(user_id)
            .bind(username)
            .fetch_optional(executor)
            .await
            .map(|row| row.map(database_user_from_row))
        } else {
            query(
                "SELECT user_id, username, email, password_hash, display_name, is_active \
                 FROM auth_users WHERE user_id = ? LIMIT 1",
            )
            .bind(user_id)
            .fetch_optional(executor)
            .await
            .map(|row| row.map(database_user_from_row))
        }
    }

    async fn fetch_refresh_token(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        token_hash: &str,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        query(
            "SELECT user_id, DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at \
             FROM auth_refresh_tokens WHERE token_hash = ? AND is_revoked = 0 LIMIT 1",
        )
        .bind(token_hash)
        .fetch_optional(executor)
        .await
        .map(|row| {
            row.map(|row| {
                (
                    row.try_get("user_id").unwrap_or_default(),
                    row.try_get("expires_at").unwrap_or_default(),
                )
            })
        })
    }

    fn create_access_token(&self, user_id: &str, username: &str) -> Result<String, String> {
        create_jwt_token(
            &self.jwt,
            JwtTokenClaims {
                sub: user_id.to_string(),
                username: Some(username.to_string()),
                token_type: "access".to_string(),
                exp: 0,
                iat: 0,
                jti: String::new(),
            },
            ChronoDuration::minutes(i64::from(self.jwt.access_token_expire_minutes)),
        )
    }

    fn create_refresh_token(&self, user_id: &str) -> Result<String, String> {
        create_jwt_token(
            &self.jwt,
            JwtTokenClaims {
                sub: user_id.to_string(),
                username: None,
                token_type: "refresh".to_string(),
                exp: 0,
                iat: 0,
                jti: String::new(),
            },
            ChronoDuration::days(i64::from(self.jwt.refresh_token_expire_days)),
        )
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
impl AuthService for DatabaseAuthService {
    async fn register(
        &self,
        request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_register_request(&request)?;

        let pool = self.get_pool().await.map_err(internal_error)?;
        self.ensure_default_roles(&pool)
            .await
            .map_err(internal_error)?;

        if self
            .fetch_user_by_username(&pool, &request.username)
            .await
            .map_err(internal_error)?
            .is_some()
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Username already exists",
            ));
        }
        if self
            .fetch_user_by_email(&pool, &request.email)
            .await
            .map_err(internal_error)?
            .is_some()
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Email already exists",
            ));
        }

        let password_hash =
            bcrypt_hash(request.password.as_str(), bcrypt::DEFAULT_COST).map_err(internal_error)?;
        let user_id = Uuid::new_v4().to_string();
        let display_name = request.display_name.clone();

        let mut tx = pool.begin().await.map_err(internal_error)?;
        let insert_result = query(
            "INSERT INTO auth_users (user_id, username, email, password_hash, display_name, is_active) \
             VALUES (?, ?, ?, ?, ?, 1)",
        )
        .bind(&user_id)
        .bind(&request.username)
        .bind(&request.email)
        .bind(&password_hash)
        .bind(&display_name)
        .execute(&mut *tx)
        .await;

        if let Err(error) = insert_result {
            tx.rollback().await.ok();
            if is_duplicate_key_error(&error) {
                if self
                    .fetch_user_by_username(&pool, &request.username)
                    .await
                    .map_err(internal_error)?
                    .is_some()
                {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "Username already exists",
                    ));
                }
                if self
                    .fetch_user_by_email(&pool, &request.email)
                    .await
                    .map_err(internal_error)?
                    .is_some()
                {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "Email already exists",
                    ));
                }
            }
            return Err(internal_error(error));
        }

        query(
            "INSERT INTO auth_user_roles (user_id, role_id) \
             SELECT ?, r.role_id FROM auth_roles r \
             WHERE r.role_name = 'astra_admin' \
             AND NOT EXISTS (SELECT 1 FROM auth_user_roles ur JOIN auth_roles r2 \
             ON ur.role_id = r2.role_id WHERE r2.role_name = 'astra_admin')",
        )
        .bind(&user_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            let message = error.to_string();
            (message, internal_error(error))
        })
        .map_err(|(_, error)| error)?;

        tx.commit().await.map_err(internal_error)?;

        Ok(AuthUserRecord {
            user_id,
            username: request.username,
            email: request.email,
            display_name,
        })
    }

    async fn login(
        &self,
        request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let user = self
            .fetch_user_by_username(&pool, &request.username)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| {
                error_response(StatusCode::UNAUTHORIZED, "Invalid username or password")
            })?;

        if !bcrypt_verify(request.password.as_str(), &user.password_hash).unwrap_or(false) {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid username or password",
            ));
        }
        if !user.is_active {
            return Err(error_response(StatusCode::FORBIDDEN, "User is inactive"));
        }

        let access_token = self
            .create_access_token(&user.user_id, &user.username)
            .map_err(internal_error)?;
        let refresh_token = self
            .create_refresh_token(&user.user_id)
            .map_err(internal_error)?;
        let refresh_token_hash = sha256_hex(&refresh_token);
        let expires_at = (Utc::now()
            + ChronoDuration::days(i64::from(self.jwt.refresh_token_expire_days)))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

        let mut tx = pool.begin().await.map_err(internal_error)?;
        query("UPDATE auth_users SET last_login_at = NOW() WHERE user_id = ?")
            .bind(&user.user_id)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        query(
            "INSERT INTO auth_refresh_tokens (token_id, user_id, token_hash, expires_at, is_revoked) \
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&user.user_id)
        .bind(&refresh_token_hash)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        Ok(AuthTokenRecord {
            access_token,
            refresh_token,
            token_type: "bearer".to_string(),
            expires_in: 3600,
        })
    }

    async fn refresh(
        &self,
        request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        let claims =
            decode_jwt_claims_with_detail(&request.refresh_token, &self.jwt, "Invalid token")?;
        if claims.token_type.as_deref() != Some("refresh") {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid token type",
            ));
        }
        let user_id = claims
            .sub
            .clone()
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Invalid token"))?;

        let pool = self.get_pool().await.map_err(internal_error)?;
        let refresh_token_hash = sha256_hex(&request.refresh_token);
        let stored = self
            .fetch_refresh_token(&pool, &refresh_token_hash)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Token expired or revoked"))?;

        if stored.1 < Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string() {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Token expired or revoked",
            ));
        }

        let user = self
            .fetch_user_by_id_or_username(&pool, &user_id, None)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User not found"))?;

        let access_token = self
            .create_access_token(&user.user_id, &user.username)
            .map_err(internal_error)?;
        let new_refresh_token = self
            .create_refresh_token(&user.user_id)
            .map_err(internal_error)?;
        let new_refresh_token_hash = sha256_hex(&new_refresh_token);
        let expires_at = (Utc::now() + ChronoDuration::days(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        let mut tx = pool.begin().await.map_err(internal_error)?;
        query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE token_hash = ?")
            .bind(&refresh_token_hash)
            .execute(&mut *tx)
            .await
            .map_err(internal_error)?;
        query(
            "INSERT INTO auth_refresh_tokens (token_id, user_id, token_hash, expires_at, is_revoked) \
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&user.user_id)
        .bind(&new_refresh_token_hash)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;

        Ok(AuthTokenRecord {
            access_token,
            refresh_token: new_refresh_token,
            token_type: "bearer".to_string(),
            expires_in: 3600,
        })
    }

    async fn logout(
        &self,
        request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        // Decode to get user_id — revoke ALL tokens for this user, not just the submitted one.
        // This ensures all active sessions are invalidated on logout.
        let user_id =
            decode_jwt_claims_with_detail(&request.refresh_token, &self.jwt, "Invalid token")
                .ok()
                .and_then(|c| c.sub);

        let pool = self.get_pool().await.map_err(internal_error)?;

        // Revoke the specific token by hash first (handles even expired tokens gracefully)
        query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE token_hash = ?")
            .bind(sha256_hex(&request.refresh_token))
            .execute(&pool)
            .await
            .map_err(internal_error)?;

        // Also revoke ALL active sessions for this user
        if let Some(uid) = user_id {
            query(
                "UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE user_id = ? AND is_revoked = 0",
            )
            .bind(uid)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        }

        Ok(())
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
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
        let pool = self.get_pool().await.map_err(internal_error)?;
        let user = self
            .fetch_user_by_id_or_username(&pool, &user_id, claims.username.as_deref())
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "User not found"))?;

        Ok(AuthUserRecord {
            user_id: user.user_id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredAuthService;

#[async_trait]
impl AuthService for UnconfiguredAuthService {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Auth service not configured",
        ))
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Auth service not configured",
        ))
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Auth service not configured",
        ))
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Auth service not configured",
        ))
    }

    async fn current_user(
        &self,
        _headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Auth service not configured",
        ))
    }
}
