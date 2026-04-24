use crate::storage::database_user_from_row;
use astra_core::{
    ErrorResponse, JwtSettings, MatrixOneSettings, SharedPool, bearer_token, connect_matrixone,
    error_response, internal_error, is_duplicate_key_error,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use bcrypt::{hash as bcrypt_hash, verify as bcrypt_verify};
use chrono::{Duration as ChronoDuration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;
use sqlx::{MySql, Row, query};
use tracing::warn;
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

use std::fmt;

#[derive(Clone)]
struct TrustedMoiJwtSettings {
    secret_key: String,
    algorithm: Algorithm,
    expected_issuer: Option<String>,
    expected_audience: Option<String>,
    leeway_seconds: u64,
}

impl fmt::Debug for TrustedMoiJwtSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedMoiJwtSettings")
            .field("secret_key", &"[REDACTED]")
            .field("algorithm", &self.algorithm)
            .field("expected_issuer", &self.expected_issuer)
            .field("expected_audience", &self.expected_audience)
            .field("leeway_seconds", &self.leeway_seconds)
            .finish()
    }
}

#[derive(Clone)]
pub struct TrustedMoiAuthService {
    settings: TrustedMoiJwtSettings,
}

impl fmt::Debug for TrustedMoiAuthService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedMoiAuthService")
            .field("settings", &self.settings)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
struct TrustedMoiClaims {
    sub: Option<String>,
    uid: Option<String>,
    user_id: Option<String>,
    username: Option<String>,
    email: Option<String>,
    display_name: Option<String>,
    nickname: Option<String>,
    name: Option<String>,
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

impl TrustedMoiAuthService {
    pub fn new(
        secret_key: &str,
        algorithm: &str,
        expected_issuer: Option<String>,
        expected_audience: Option<String>,
        leeway_seconds: u64,
    ) -> Result<Self, String> {
        Ok(Self {
            settings: TrustedMoiJwtSettings {
                secret_key: normalize_jwt_secret_for_trusted_mode(secret_key),
                algorithm: parse_trusted_moi_algorithm(algorithm)?,
                expected_issuer,
                expected_audience,
                leeway_seconds,
            },
        })
    }

    pub fn from_env() -> Result<Self, String> {
        let raw_secret = std::env::var("TRUSTED_MOI_JWT_SECRET_KEY")
            .or_else(|_| std::env::var("JWT_SECRET_KEY"))
            .map_err(|_| {
                "TRUSTED_MOI_JWT_SECRET_KEY (or JWT_SECRET_KEY fallback) must be set".to_string()
            })?;

        let algorithm = std::env::var("TRUSTED_MOI_JWT_ALGORITHM")
            .or_else(|_| std::env::var("JWT_ALGORITHM"))
            .unwrap_or_else(|_| "HS256".to_string());

        let expected_issuer = std::env::var("TRUSTED_MOI_JWT_ISSUER")
            .ok()
            .filter(|v| !v.is_empty());
        let expected_audience = std::env::var("TRUSTED_MOI_JWT_AUDIENCE")
            .ok()
            .filter(|v| !v.is_empty());
        let leeway_seconds = std::env::var("TRUSTED_MOI_JWT_LEEWAY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);

        Self::new(
            &raw_secret,
            &algorithm,
            expected_issuer,
            expected_audience,
            leeway_seconds,
        )
    }

    fn auth_disabled_error() -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::FORBIDDEN,
            "Local auth endpoints are disabled in trusted_moi mode",
        )
    }

    fn decode_trusted_claims(
        &self,
        token: &str,
    ) -> Result<TrustedMoiClaims, (StatusCode, Json<ErrorResponse>)> {
        let mut validation = Validation::new(self.settings.algorithm);
        validation.leeway = self.settings.leeway_seconds;
        validation.validate_exp = true;
        if let Some(expected_issuer) = &self.settings.expected_issuer {
            validation.set_issuer(&[expected_issuer]);
        }
        if let Some(expected_audience) = &self.settings.expected_audience {
            validation.set_audience(&[expected_audience]);
        }

        decode::<TrustedMoiClaims>(
            token,
            &DecodingKey::from_secret(self.settings.secret_key.as_bytes()),
            &validation,
        )
        .map(|data| data.claims)
        .map_err(|_| error_response(StatusCode::UNAUTHORIZED, "Invalid trusted_moi token"))
    }
}

fn parse_trusted_moi_algorithm(algorithm: &str) -> Result<Algorithm, String> {
    match algorithm {
        "HS256" => Ok(Algorithm::HS256),
        "HS384" => Ok(Algorithm::HS384),
        "HS512" => Ok(Algorithm::HS512),
        _ => Err(format!(
            "unsupported TRUSTED_MOI_JWT_ALGORITHM: {algorithm}"
        )),
    }
}

fn normalize_jwt_secret_for_trusted_mode(secret: &str) -> String {
    if secret.len() >= 32 {
        secret.to_string()
    } else {
        let mut padded = secret.to_string();
        padded.extend(std::iter::repeat_n('0', 32 - secret.len()));
        padded
    }
}

/// Map SQLx failures to HTTP errors; PoolTimedOut → 503 (retryable), others → 500.
fn map_auth_sqlx(
    err: sqlx::Error,
    operation: &'static str,
    pool: Option<&sqlx::Pool<MySql>>,
) -> (StatusCode, Json<ErrorResponse>) {
    if matches!(&err, sqlx::Error::PoolTimedOut) {
        match pool {
            Some(p) => {
                warn!(
                    target: "astra_services::auth",
                    operation,
                    pool_size = p.size(),
                    pool_idle = p.num_idle(),
                    "auth database pool acquire timed out"
                );
            }
            None => {
                warn!(
                    target: "astra_services::auth",
                    operation,
                    "auth database pool acquire timed out (no pool handle for size/idle)"
                );
            }
        }
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "pool timed out while waiting for an open connection",
        );
    }
    internal_error(err)
}

#[async_trait]
impl AuthService for DatabaseAuthService {
    async fn register(
        &self,
        request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        validate_register_request(&request)?;
        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        self.ensure_default_roles(&pool)
            .await
            .map_err(|e| map_auth_sqlx(e, "register.ensure_default_roles", Some(&pool)))?;
        if self
            .fetch_user_by_username(&pool, &request.username)
            .await
            .map_err(|e| map_auth_sqlx(e, "register.fetch_user_by_username", Some(&pool)))?
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
            .map_err(|e| map_auth_sqlx(e, "register.fetch_user_by_email", Some(&pool)))?
            .is_some()
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Email already exists",
            ));
        }

        let bcrypt_cost = std::env::var("ASTRA_BCRYPT_COST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(bcrypt::DEFAULT_COST);
        let password_hash =
            bcrypt_hash(request.password.as_str(), bcrypt_cost).map_err(internal_error)?;
        let user_id = Uuid::new_v4().to_string();
        let display_name = request.display_name.clone();

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| map_auth_sqlx(e, "register.begin_tx", Some(&pool)))?;
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
                    .map_err(|e| {
                        map_auth_sqlx(e, "register.fetch_user_by_username_dup", Some(&pool))
                    })?
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
                    .map_err(|e| map_auth_sqlx(e, "register.fetch_user_by_email_dup", Some(&pool)))?
                    .is_some()
                {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "Email already exists",
                    ));
                }
            }
            return Err(map_auth_sqlx(
                error,
                "register.insert_auth_users",
                Some(&pool),
            ));
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
        .map_err(|e| map_auth_sqlx(e, "register.assign_initial_roles", Some(&pool)))?;

        tx.commit()
            .await
            .map_err(|e| map_auth_sqlx(e, "register.commit_tx", Some(&pool)))?;

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
        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        let user = self
            .fetch_user_by_username(&pool, &request.username)
            .await
            .map_err(|e| map_auth_sqlx(e, "login.fetch_user_by_username", Some(&pool)))?
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

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| map_auth_sqlx(e, "login.begin_tx", Some(&pool)))?;
        query("UPDATE auth_users SET last_login_at = NOW() WHERE user_id = ?")
            .bind(&user.user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "login.update_last_login_at", Some(&pool)))?;
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
        .map_err(|e| map_auth_sqlx(e, "login.insert_refresh_token", Some(&pool)))?;
        tx.commit()
            .await
            .map_err(|e| map_auth_sqlx(e, "login.commit_tx", Some(&pool)))?;

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

        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        let refresh_token_hash = sha256_hex(&request.refresh_token);
        let stored = self
            .fetch_refresh_token(&pool, &refresh_token_hash)
            .await
            .map_err(|e| map_auth_sqlx(e, "refresh.fetch_refresh_token", Some(&pool)))?
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
            .map_err(|e| map_auth_sqlx(e, "refresh.fetch_user_by_id", Some(&pool)))?
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

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| map_auth_sqlx(e, "refresh.begin_tx", Some(&pool)))?;
        query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE token_hash = ?")
            .bind(&refresh_token_hash)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "refresh.revoke_old_token", Some(&pool)))?;
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
        .map_err(|e| map_auth_sqlx(e, "refresh.insert_new_refresh_token", Some(&pool)))?;
        tx.commit()
            .await
            .map_err(|e| map_auth_sqlx(e, "refresh.commit_tx", Some(&pool)))?;

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

        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;

        // Revoke the specific token by hash first (handles even expired tokens gracefully)
        query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE token_hash = ?")
            .bind(sha256_hex(&request.refresh_token))
            .execute(&pool)
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.revoke_submitted_token", Some(&pool)))?;

        // Also revoke ALL active sessions for this user
        if let Some(uid) = user_id {
            query(
                "UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE user_id = ? AND is_revoked = 0",
            )
            .bind(uid)
            .execute(&pool)
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.revoke_all_user_tokens", Some(&pool)))?;
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
        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        let user = self
            .fetch_user_by_id_or_username(&pool, &user_id, claims.username.as_deref())
            .await
            .map_err(|e| map_auth_sqlx(e, "current_user.fetch_user", Some(&pool)))?
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "User not found"))?;

        Ok(AuthUserRecord {
            user_id: user.user_id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
        })
    }
}

#[async_trait]
impl AuthService for TrustedMoiAuthService {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(Self::auth_disabled_error())
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(Self::auth_disabled_error())
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(Self::auth_disabled_error())
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(Self::auth_disabled_error())
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        let token = bearer_token(headers)?;
        let claims = self.decode_trusted_claims(token)?;

        let user_id = claims
            .sub
            .or(claims.uid)
            .or(claims.user_id)
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Missing user id claim"))?;

        let preferred_name = claims.name.or(claims.nickname).or(claims.display_name);
        let username = claims
            .username
            .or_else(|| preferred_name.clone())
            .unwrap_or_else(|| user_id.clone());
        let display_name = preferred_name;

        Ok(AuthUserRecord {
            user_id,
            username,
            email: claims.email.unwrap_or_default(),
            display_name,
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

/// Stub auth service that accepts any Bearer token and returns a fixed test user.
/// Intended for integration/contract tests where JWT validation is not under test.
pub struct StubAuthService;

#[async_trait]
impl AuthService for StubAuthService {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(StatusCode::NOT_IMPLEMENTED, "stub"))
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(StatusCode::NOT_IMPLEMENTED, "stub"))
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(StatusCode::NOT_IMPLEMENTED, "stub"))
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(StatusCode::NOT_IMPLEMENTED, "stub"))
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        let _token = bearer_token(headers)?;
        Ok(AuthUserRecord {
            user_id: "test-user".into(),
            username: "test-user".into(),
            email: "test@test.com".into(),
            display_name: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde::Serialize;

    #[derive(Serialize)]
    struct TrustedMoiTestClaims<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        sub: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        uid: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_name: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        iss: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        aud: Option<&'a str>,
        iat: usize,
        exp: usize,
    }

    fn make_token(secret: &str, claims: TrustedMoiTestClaims<'_>) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(normalize_jwt_secret_for_trusted_mode(secret).as_bytes()),
        )
        .expect("encode token")
    }

    fn auth_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {token}").parse().expect("valid header"),
        );
        headers
    }

    fn now_exp_pair() -> (usize, usize) {
        let now = Utc::now().timestamp() as usize;
        (now, now + 3600)
    }

    #[tokio::test]
    async fn trusted_moi_current_user_from_sub_username_email() {
        let service = TrustedMoiAuthService::new("secret", "HS256", None, None, 30).unwrap();
        let (iat, exp) = now_exp_pair();
        let token = make_token(
            "secret",
            TrustedMoiTestClaims {
                sub: Some("moi-user-1"),
                uid: None,
                user_id: None,
                username: Some("alice"),
                email: Some("alice@example.com"),
                display_name: Some("Alice"),
                name: None,
                iss: None,
                aud: None,
                iat,
                exp,
            },
        );
        let user = service
            .current_user(&auth_headers(&token))
            .await
            .expect("current_user");
        assert_eq!(user.user_id, "moi-user-1");
        assert_eq!(user.username, "alice");
        assert_eq!(user.email, "alice@example.com");
        assert_eq!(user.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn trusted_moi_current_user_fallback_to_uid_and_name() {
        let service = TrustedMoiAuthService::new("secret", "HS256", None, None, 30).unwrap();
        let (iat, exp) = now_exp_pair();
        let token = make_token(
            "secret",
            TrustedMoiTestClaims {
                sub: None,
                uid: Some("moi-user-2"),
                user_id: None,
                username: None,
                email: None,
                display_name: None,
                name: Some("Bob"),
                iss: None,
                aud: None,
                iat,
                exp,
            },
        );
        let user = service
            .current_user(&auth_headers(&token))
            .await
            .expect("current_user");
        assert_eq!(user.user_id, "moi-user-2");
        assert_eq!(user.username, "Bob");
        assert_eq!(user.email, "");
        assert_eq!(user.display_name.as_deref(), Some("Bob"));
    }

    #[tokio::test]
    async fn trusted_moi_rejects_missing_identity_claim() {
        let service = TrustedMoiAuthService::new("secret", "HS256", None, None, 30).unwrap();
        let (iat, exp) = now_exp_pair();
        let token = make_token(
            "secret",
            TrustedMoiTestClaims {
                sub: None,
                uid: None,
                user_id: None,
                username: Some("nobody"),
                email: None,
                display_name: None,
                name: None,
                iss: None,
                aud: None,
                iat,
                exp,
            },
        );
        let err = service
            .current_user(&auth_headers(&token))
            .await
            .expect_err("should reject");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        assert_eq!(err.1.detail, "Missing user id claim");
    }

    #[tokio::test]
    async fn trusted_moi_rejects_wrong_issuer_or_audience() {
        let service = TrustedMoiAuthService::new(
            "secret",
            "HS256",
            Some("issuer-good".to_string()),
            Some("astra".to_string()),
            30,
        )
        .unwrap();
        let (iat, exp) = now_exp_pair();
        let token = make_token(
            "secret",
            TrustedMoiTestClaims {
                sub: Some("moi-user-3"),
                uid: None,
                user_id: None,
                username: Some("charlie"),
                email: None,
                display_name: None,
                name: None,
                iss: Some("issuer-bad"),
                aud: Some("not-astra"),
                iat,
                exp,
            },
        );
        let err = service
            .current_user(&auth_headers(&token))
            .await
            .expect_err("should reject");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        assert_eq!(err.1.detail, "Invalid trusted_moi token");
    }

    #[tokio::test]
    async fn trusted_moi_disables_local_auth_endpoints() {
        let service = TrustedMoiAuthService::new("secret", "HS256", None, None, 30).unwrap();

        let login = service
            .login(AuthLoginRequestData {
                username: "alice".to_string(),
                password: "pw".to_string(),
            })
            .await
            .expect_err("login should be disabled");
        assert_eq!(login.0, StatusCode::FORBIDDEN);

        let register = service
            .register(AuthRegisterRequestData {
                username: "alice".to_string(),
                email: "alice@example.com".to_string(),
                password: "pw".to_string(),
                display_name: Some("Alice".to_string()),
            })
            .await
            .expect_err("register should be disabled");
        assert_eq!(register.0, StatusCode::FORBIDDEN);

        let refresh = service
            .refresh(AuthRefreshRequestData {
                refresh_token: "refresh".to_string(),
            })
            .await
            .expect_err("refresh should be disabled");
        assert_eq!(refresh.0, StatusCode::FORBIDDEN);

        let logout = service
            .logout(AuthRefreshRequestData {
                refresh_token: "refresh".to_string(),
            })
            .await
            .expect_err("logout should be disabled");
        assert_eq!(logout.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn trusted_moi_jwt_settings_debug_redacts_secret() {
        let service = TrustedMoiAuthService::new("supersecret", "HS256", None, None, 30)
            .expect("construct service");
        let debug_str = format!("{:?}", service);
        assert!(
            !debug_str.contains("supersecret"),
            "secret should be redacted: {debug_str}"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "should show [REDACTED]: {debug_str}"
        );
    }
}
