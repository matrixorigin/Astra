use crate::storage::database_user_from_row;
use astra_core::{
    ErrorResponse, JwtSettings, MatrixOneSettings, ProviderRequestAuthConfig, SharedPool,
    bearer_token, error_response, error_response_coded, internal_error, is_duplicate_key_error,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bcrypt::{hash as bcrypt_hash, verify as bcrypt_verify};
use hmac::{Hmac, Mac};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Resolve bcrypt cost from `ASTRA_BCRYPT_COST`, falling back to `bcrypt::DEFAULT_COST` (12).
/// Tests set a low cost (e.g. `4`) to avoid multi-hundred-millisecond hashing in debug builds;
/// production leaves the env var unset. Cached after first read via OnceLock.
fn bcrypt_cost_from_env() -> u32 {
    static COST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *COST.get_or_init(|| {
        std::env::var("ASTRA_BCRYPT_COST")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|c| (4..=bcrypt::DEFAULT_COST).contains(c))
            .unwrap_or(bcrypt::DEFAULT_COST)
    })
}
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;
use sha2::Sha256;
use sqlx::{MySql, Row, query};
use tracing::warn;
use uuid::Uuid;

mod admin;
mod encryption;
mod jwt;
pub mod provider_request;
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
pub use provider_request::{ProviderAuthorizedRequest, ProviderRequestDescriptor};
pub use session::UnconfiguredSessionService;
pub use session::{
    DatabaseSessionService, SessionActivityCursor, SessionActivityRecord, SessionCreateRequestData,
    SessionListCursor, SessionListFilter, SessionListRecord, SessionRecord, SessionService,
    SessionUpdateRequestData,
};
use validation::validate_register_request;

type AuthHttpError = (StatusCode, Json<ErrorResponse>);
type ExternalRequestAuthHeaders = Option<(String, String, String)>;
type ParsedTokenSession = (String, String, Option<String>);
type HmacSha256 = Hmac<Sha256>;
const PROVIDER_REQUEST_TOKEN_PREFIX: &str = "moi-provider-v1";
const PROVIDER_REQUEST_MAX_TTL_SECONDS: i64 = 300;
const PROVIDER_REQUEST_CLOCK_SKEW_SECONDS: i64 = 60;

#[derive(Debug, Deserialize)]
struct ProviderRequestClaims {
    sub: String,
    scope: String,
    provider: String,
    method: String,
    path: String,
    route: String,
    request_id: String,
    body_digest: String,
    nonce: String,
    iat: i64,
    exp: i64,
}

fn verify_provider_request_hmac_token(
    expected_provider: &str,
    key: &str,
    bearer_token: &str,
    request: &ProviderRequestDescriptor,
) -> Result<ProviderAuthorizedRequest, AuthHttpError> {
    if key.is_empty() || key.trim() != key {
        return Err(provider_request_auth_error(
            "Provider request HMAC key is not configured",
        ));
    }
    let token = bearer_token.strip_prefix("Bearer ").ok_or_else(|| {
        error_response_coded(
            StatusCode::UNAUTHORIZED,
            "Provider request token must use Bearer authorization",
            "provider_request_auth_invalid",
        )
    })?;
    if token.is_empty() || token.trim() != token {
        return Err(provider_request_auth_error(
            "Provider request token must not be empty or padded",
        ));
    }
    let mut parts = token.split('.');
    let Some(prefix) = parts.next() else {
        return Err(provider_request_auth_error(
            "Provider request token is malformed",
        ));
    };
    let Some(encoded_claims) = parts.next() else {
        return Err(provider_request_auth_error(
            "Provider request token is malformed",
        ));
    };
    let Some(encoded_signature) = parts.next() else {
        return Err(provider_request_auth_error(
            "Provider request token is malformed",
        ));
    };
    if parts.next().is_some() || prefix != PROVIDER_REQUEST_TOKEN_PREFIX {
        return Err(provider_request_auth_error(
            "Provider request token is malformed",
        ));
    }
    if !verify_provider_request_signature(key.as_bytes(), encoded_claims, encoded_signature) {
        return Err(provider_request_auth_error(
            "Provider request token signature is invalid",
        ));
    }
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(encoded_claims)
        .map_err(|_| provider_request_auth_error("Provider request token claims are malformed"))?;
    let claims: ProviderRequestClaims = serde_json::from_slice(&claims_bytes)
        .map_err(|_| provider_request_auth_error("Provider request token claims are malformed"))?;
    validate_provider_request_claims(expected_provider, &claims, request)?;
    Ok(ProviderAuthorizedRequest {
        provider_id: expected_provider.to_string(),
        external_subject: claims.sub,
        provider_scope_id: claims.scope,
        request_authorization_id: format!(
            "{PROVIDER_REQUEST_TOKEN_PREFIX}:{}:{}:{}",
            claims.provider, claims.request_id, claims.nonce
        ),
        expires_at_unix: claims.exp,
    })
}

fn validate_provider_request_claims(
    expected_provider: &str,
    claims: &ProviderRequestClaims,
    request: &ProviderRequestDescriptor,
) -> Result<(), AuthHttpError> {
    for (field, value) in [
        ("sub", claims.sub.as_str()),
        ("scope", claims.scope.as_str()),
        ("provider", claims.provider.as_str()),
        ("method", claims.method.as_str()),
        ("path", claims.path.as_str()),
        ("route", claims.route.as_str()),
        ("request_id", claims.request_id.as_str()),
        ("body_digest", claims.body_digest.as_str()),
        ("nonce", claims.nonce.as_str()),
    ] {
        if value.is_empty() || value.trim() != value {
            return Err(provider_request_auth_error(format!(
                "Provider request claim {field} must not be empty or padded"
            )));
        }
    }
    if claims.provider != expected_provider {
        return Err(provider_request_auth_error(
            "Provider request token provider does not match request provider",
        ));
    }
    if claims.method != request.method {
        return Err(provider_request_auth_error(
            "Provider request token method does not match request method",
        ));
    }
    if claims.path != request.path {
        return Err(provider_request_auth_error(
            "Provider request token path does not match request path",
        ));
    }
    if request
        .route
        .as_deref()
        .is_none_or(|route| route != claims.route)
    {
        return Err(provider_request_auth_error(
            "Provider request token route does not match request descriptor",
        ));
    }
    if request
        .request_id
        .as_deref()
        .is_none_or(|request_id| request_id != claims.request_id)
    {
        return Err(provider_request_auth_error(
            "Provider request token request_id does not match request descriptor",
        ));
    }
    if request
        .body_digest
        .as_deref()
        .is_none_or(|body_digest| body_digest != claims.body_digest)
    {
        return Err(provider_request_auth_error(
            "Provider request token body_digest does not match request body",
        ));
    }
    if claims.iat <= 0 || claims.exp <= 0 || claims.exp <= claims.iat {
        return Err(provider_request_auth_error(
            "Provider request token time claims are invalid",
        ));
    }
    if claims.exp - claims.iat > PROVIDER_REQUEST_MAX_TTL_SECONDS {
        return Err(provider_request_auth_error(
            "Provider request token TTL exceeds the maximum allowed window",
        ));
    }
    let now = Utc::now().timestamp();
    if claims.iat > now + PROVIDER_REQUEST_CLOCK_SKEW_SECONDS {
        return Err(provider_request_auth_error(
            "Provider request token was issued in the future",
        ));
    }
    if now >= claims.exp {
        return Err(provider_request_auth_error(
            "Provider request token has expired",
        ));
    }
    Ok(())
}

fn provider_request_auth_error(message: impl Into<String>) -> AuthHttpError {
    error_response_coded(
        StatusCode::UNAUTHORIZED,
        message.into(),
        "provider_request_auth_invalid",
    )
}

#[cfg(test)]
fn provider_request_signature(key: &[u8], encoded_claims: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
    mac.update(encoded_claims.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn verify_provider_request_signature(
    key: &[u8],
    encoded_claims: &str,
    encoded_signature: &str,
) -> bool {
    let Ok(signature) = URL_SAFE_NO_PAD.decode(encoded_signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return false;
    };
    mac.update(encoded_claims.as_bytes());
    mac.verify_slice(&signature).is_ok()
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

    async fn current_principal(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
        self.current_user(headers)
            .await
            .map(AuthPrincipal::internal)
    }

    async fn current_principal_for_request(
        &self,
        headers: &HeaderMap,
        _request: ProviderRequestDescriptor,
    ) -> Result<AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
        self.current_principal(headers).await
    }
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
pub struct AuthPrincipal {
    pub user: AuthUserRecord,
    pub session_id: Option<String>,
    pub origin: AuthPrincipalOrigin,
}

impl AuthPrincipal {
    pub fn internal(user: AuthUserRecord) -> Self {
        Self {
            user,
            session_id: None,
            origin: AuthPrincipalOrigin::Internal,
        }
    }

    pub fn is_provider_authorized_request(&self) -> bool {
        matches!(
            self.origin,
            AuthPrincipalOrigin::ProviderAuthorizedRequest(_)
        )
    }

    pub fn provider_authorized_request_context(
        &self,
    ) -> Option<&AuthProviderAuthorizedRequestContext> {
        match &self.origin {
            AuthPrincipalOrigin::ProviderAuthorizedRequest(context) => Some(context),
            AuthPrincipalOrigin::Internal => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPrincipalOrigin {
    Internal,
    ProviderAuthorizedRequest(AuthProviderAuthorizedRequestContext),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthProviderAuthorizedRequestContext {
    pub provider_id: String,
    pub external_subject: String,
    pub provider_scope_id: String,
    pub request_authorization_id: String,
    pub allowed_capability_descriptors: Vec<astra_core::ProviderCapabilityDescriptorConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthTokenRecord {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u32,
}

#[derive(Clone)]
pub struct DatabaseAuthService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
    jwt: JwtSettings,
    encryptor: Option<FernetTokenEncryptor>,
    provider_request_auth: Vec<ProviderRequestAuthConfig>,
    provider_request_replay_cache: Arc<Mutex<HashMap<String, i64>>>,
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

impl fmt::Debug for DatabaseAuthService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseAuthService")
            .field("matrixone", &self.matrixone)
            .field("pool_configured", &self.pool.is_some())
            .field("jwt", &self.jwt)
            .field("encryptor_configured", &self.encryptor.is_some())
            .field(
                "provider_request_auth_count",
                &self.provider_request_auth.len(),
            )
            .finish()
    }
}

trait RefreshTokenRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &'static str) -> Result<Option<String>, sqlx::Error>;
}

impl RefreshTokenRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_string_column(&self, column: &'static str) -> Result<Option<String>, sqlx::Error> {
        self.try_get(column)
    }
}

fn decode_refresh_token_row(
    row: &impl RefreshTokenRow,
) -> Result<(String, String, Option<String>), sqlx::Error> {
    Ok((
        row.string_column("user_id")?,
        row.string_column("expires_at")?,
        row.optional_string_column("session_id")?,
    ))
}

impl DatabaseAuthService {
    pub fn new(matrixone: MatrixOneSettings, jwt: JwtSettings) -> Self {
        Self {
            matrixone,
            jwt,
            pool: None,
            encryptor: None,
            provider_request_auth: Vec::new(),
            provider_request_replay_cache: Arc::new(Mutex::new(HashMap::new())),
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
        .and_then(|row| row.map(database_user_from_row).transpose())
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
        .and_then(|row| row.map(database_user_from_row).transpose())
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
            .and_then(|row| row.map(database_user_from_row).transpose())
        } else {
            query(
                "SELECT user_id, username, email, password_hash, display_name, is_active \
                 FROM auth_users WHERE user_id = ? LIMIT 1",
            )
            .bind(user_id)
            .fetch_optional(executor)
            .await
            .and_then(|row| row.map(database_user_from_row).transpose())
        }
    }

    async fn fetch_refresh_token(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        token_hash: &str,
    ) -> Result<Option<(String, String, Option<String>)>, sqlx::Error> {
        query(
            "SELECT user_id, DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at, session_id \
             FROM auth_refresh_tokens WHERE token_hash = ? AND is_revoked = 0 LIMIT 1",
        )
        .bind(token_hash)
        .fetch_optional(executor)
        .await
        .and_then(|row| row.map(|row| decode_refresh_token_row(&row)).transpose())
    }

    fn create_access_token(
        &self,
        user_id: &str,
        username: &str,
        session_id: &str,
        origin: &str,
    ) -> Result<String, String> {
        create_jwt_token(
            &self.jwt,
            JwtTokenClaims {
                sub: user_id.to_string(),
                username: Some(username.to_string()),
                token_type: "access".to_string(),
                sid: Some(session_id.to_string()),
                origin: Some(origin.to_string()),
                exp: 0,
                iat: 0,
                jti: String::new(),
            },
            ChronoDuration::minutes(i64::from(self.jwt.access_token_expire_minutes)),
        )
    }

    fn create_refresh_token(
        &self,
        user_id: &str,
        session_id: &str,
        origin: &str,
    ) -> Result<String, String> {
        create_jwt_token(
            &self.jwt,
            JwtTokenClaims {
                sub: user_id.to_string(),
                username: None,
                token_type: "refresh".to_string(),
                sid: Some(session_id.to_string()),
                origin: Some(origin.to_string()),
                exp: 0,
                iat: 0,
                jti: String::new(),
            },
            ChronoDuration::days(i64::from(self.jwt.refresh_token_expire_days)),
        )
    }

    fn access_token_expires_in_seconds(&self) -> u32 {
        self.jwt.access_token_expire_minutes.saturating_mul(60)
    }

    fn refresh_token_expires_at_string(&self, now: chrono::DateTime<Utc>) -> String {
        (now + ChronoDuration::days(i64::from(self.jwt.refresh_token_expire_days)))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    pub fn with_encryptor(mut self, encryptor: FernetTokenEncryptor) -> Self {
        self.encryptor = Some(encryptor);
        self
    }

    pub fn with_provider_request_auth(mut self, auth: Vec<ProviderRequestAuthConfig>) -> Self {
        self.provider_request_auth = auth;
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(self.pool.as_ref(), "DatabaseAuthService", &self.matrixone)
    }

    fn provider_config(
        &self,
        provider_id: &str,
    ) -> Result<&ProviderRequestAuthConfig, (StatusCode, Json<ErrorResponse>)> {
        self.provider_request_auth
            .iter()
            .find(|provider| provider.provider == provider_id)
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    format!("Unknown provider id '{provider_id}'"),
                    "provider_unknown",
                )
            })
    }

    fn record_provider_request_authorization(
        &self,
        authorization_id: &str,
        expires_at_unix: i64,
    ) -> Result<(), AuthHttpError> {
        let now = Utc::now().timestamp();
        let mut guard = self.provider_request_replay_cache.lock().map_err(|_| {
            provider_request_auth_error("Provider request replay cache is unavailable")
        })?;
        guard.retain(|_, expiry| *expiry > now);
        if guard.contains_key(authorization_id) {
            return Err(provider_request_auth_error(
                "Provider request token has already been used",
            ));
        }
        guard.insert(authorization_id.to_string(), expires_at_unix);
        Ok(())
    }

    fn external_request_auth_headers(
        headers: &HeaderMap,
    ) -> Result<ExternalRequestAuthHeaders, AuthHttpError> {
        let provider = header_exact(headers, "x-astra-provider")?;
        let action = header_exact(headers, "x-astra-provider-action")?;
        match (provider, action) {
            (None, None) => Ok(None),
            (Some(provider), Some(action)) => {
                if action != "authorize_request" {
                    return Err(error_response_coded(
                        StatusCode::BAD_REQUEST,
                        "X-Astra-Provider-Action must be authorize_request",
                        "provider_action_invalid",
                    ));
                }
                let token = bearer_token(headers)?;
                Ok(Some((provider, action, format!("Bearer {token}"))))
            }
            _ => Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "X-Astra-Provider and X-Astra-Provider-Action must be sent together",
                "provider_request_auth_invalid",
            )),
        }
    }

    async fn refresh_session_is_active(
        &self,
        pool: &sqlx::Pool<MySql>,
        user_id: &str,
        session_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let row = query(
            "SELECT COUNT(*) AS count FROM auth_refresh_tokens \
             WHERE user_id = ? AND session_id = ? AND is_revoked = 0 AND expires_at > NOW()",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_one(pool)
        .await?;
        Ok(row.try_get::<i64, _>("count").unwrap_or(0) > 0)
    }

    fn parse_token_session(
        &self,
        claims: &jwt::JwtClaims,
    ) -> Result<ParsedTokenSession, AuthHttpError> {
        let session_id = claims
            .sid
            .clone()
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Invalid token session"))?;
        let origin = claims
            .origin
            .clone()
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Invalid token origin"))?;
        match origin.as_str() {
            "internal" => Ok((session_id, origin, None)),
            _ => Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid token origin",
            )),
        }
    }
}

fn header_exact(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{name} must be visible ASCII"),
            "provider_request_auth_invalid",
        )
    })?;
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{name} must be a non-empty exact string"),
            "provider_request_auth_invalid",
        ));
    }
    Ok(Some(value.to_string()))
}

/// Map SQLx failures to stable auth DB errors; pool exhaustion is retryable.
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
        return error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "pool timed out while waiting for an open connection",
            "database_error",
        );
    }
    error_response_coded(
        StatusCode::INTERNAL_SERVER_ERROR,
        err.to_string(),
        "database_error",
    )
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

        let password_hash = bcrypt_hash(request.password.as_str(), bcrypt_cost_from_env())
            .map_err(internal_error)?;
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
            "INSERT IGNORE INTO auth_user_roles (user_id, role_id) \
             SELECT ?, r.role_id FROM auth_roles r \
             WHERE r.role_name = 'astra_user'",
        )
        .bind(&user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_auth_sqlx(e, "register.assign_user_role", Some(&pool)))?;

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

        let session_id = Uuid::new_v4().to_string();
        let access_token = self
            .create_access_token(&user.user_id, &user.username, &session_id, "internal")
            .map_err(internal_error)?;
        let refresh_token = self
            .create_refresh_token(&user.user_id, &session_id, "internal")
            .map_err(internal_error)?;
        let refresh_token_hash = sha256_hex(&refresh_token);
        let expires_at = self.refresh_token_expires_at_string(Utc::now());

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
            "INSERT INTO auth_refresh_tokens (token_id, user_id, session_id, token_hash, expires_at, is_revoked) \
             VALUES (?, ?, ?, ?, ?, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&user.user_id)
        .bind(&session_id)
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
            expires_in: self.access_token_expires_in_seconds(),
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
        let (session_id, origin, _) = self.parse_token_session(&claims)?;

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

        if stored.0 != user_id || stored.2.as_deref() != Some(session_id.as_str()) {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Token session mismatch",
            ));
        }
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
            .create_access_token(&user.user_id, &user.username, &session_id, &origin)
            .map_err(internal_error)?;
        let new_refresh_token = self
            .create_refresh_token(&user.user_id, &session_id, &origin)
            .map_err(internal_error)?;
        let new_refresh_token_hash = sha256_hex(&new_refresh_token);
        let expires_at = self.refresh_token_expires_at_string(Utc::now());

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
            "INSERT INTO auth_refresh_tokens (token_id, user_id, session_id, token_hash, expires_at, is_revoked) \
             VALUES (?, ?, ?, ?, ?, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&user.user_id)
        .bind(&session_id)
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
            expires_in: self.access_token_expires_in_seconds(),
        })
    }

    async fn logout(
        &self,
        request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
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
        let (session_id, _, _) = self.parse_token_session(&claims)?;

        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        let refresh_token_hash = sha256_hex(&request.refresh_token);
        let stored = self
            .fetch_refresh_token(&pool, &refresh_token_hash)
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.fetch_refresh_token", Some(&pool)))?
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Token expired or revoked"))?;

        if stored.0 != user_id || stored.2.as_deref() != Some(session_id.as_str()) {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Token session mismatch",
            ));
        }
        if stored.1 < Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string() {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Token expired or revoked",
            ));
        }

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.begin_tx", Some(&pool)))?;
        query("UPDATE auth_refresh_tokens SET is_revoked = 1 WHERE token_hash = ? AND user_id = ?")
            .bind(&refresh_token_hash)
            .bind(&user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.revoke_submitted_token", Some(&pool)))?;

        tx.commit()
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.commit_tx", Some(&pool)))?;

        Ok(())
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        self.current_principal(headers)
            .await
            .map(|principal| principal.user)
    }

    async fn current_principal(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
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
        let (session_id, _, _) = self.parse_token_session(&claims)?;
        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        if !self
            .refresh_session_is_active(&pool, &user_id, &session_id)
            .await
            .map_err(|e| map_auth_sqlx(e, "current_principal.refresh_session", Some(&pool)))?
        {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Session expired or revoked",
            ));
        }

        let user = self
            .fetch_user_by_id_or_username(&pool, &user_id, None)
            .await
            .map_err(|e| map_auth_sqlx(e, "current_user.fetch_user", Some(&pool)))?
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "User not found"))?;

        Ok(AuthPrincipal {
            user: AuthUserRecord {
                user_id: user.user_id,
                username: user.username,
                email: user.email,
                display_name: user.display_name,
            },
            session_id: Some(session_id),
            origin: AuthPrincipalOrigin::Internal,
        })
    }

    async fn current_principal_for_request(
        &self,
        headers: &HeaderMap,
        request: ProviderRequestDescriptor,
    ) -> Result<AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
        let Some((provider_id, _action, token)) = Self::external_request_auth_headers(headers)?
        else {
            return self.current_principal(headers).await;
        };
        let provider = self.provider_config(&provider_id)?.clone();
        if provider.auth_type != "hmac" {
            return Err(provider_request_auth_error(
                "Provider request auth configuration is unsupported",
            ));
        }
        let authorized = verify_provider_request_hmac_token(
            &provider.provider,
            &provider.key,
            &token,
            &request,
        )?;
        self.record_provider_request_authorization(
            &authorized.request_authorization_id,
            authorized.expires_at_unix,
        )?;
        let user_id = format!(
            "provider_authorized:{}:{}",
            authorized.provider_id, authorized.external_subject
        );
        Ok(AuthPrincipal {
            user: AuthUserRecord {
                user_id,
                username: authorized.external_subject.clone(),
                email: String::new(),
                display_name: None,
            },
            session_id: None,
            origin: AuthPrincipalOrigin::ProviderAuthorizedRequest(
                AuthProviderAuthorizedRequestContext {
                    provider_id: authorized.provider_id,
                    external_subject: authorized.external_subject,
                    provider_scope_id: authorized.provider_scope_id,
                    request_authorization_id: authorized.request_authorization_id,
                    allowed_capability_descriptors: provider.allowed_capability_descriptors.clone(),
                },
            ),
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
    use astra_core::ProviderRequestAuthConfig;
    use chrono::Utc;
    use serde_json::json;

    const TEST_PROVIDER_HMAC_KEY: &str = "test-provider-hmac-key";
    const GOLDEN_PROVIDER_HMAC_KEY: &str = "WrE2irtwGEq1Ih2stZUtgFfLFNv2gVhOAwsBD999QLI";
    const GOLDEN_PROVIDER_TOKEN: &str = "moi-provider-v1.eyJzdWIiOiJ1c2VyXzEiLCJzY29wZSI6IndvcmtzcGFjZV8xIiwicHJvdmlkZXIiOiJtb2kiLCJpYXQiOjQxMDI0NDQ1MDAsImV4cCI6NDEwMjQ0NDgwMH0.wZS9mRKasIEmreqhzGheguYYPbq1URTYkJoSK3nfW3M";

    fn hmac_provider_token(
        subject: &str,
        scope: &str,
        provider: &str,
        request: &ProviderRequestDescriptor,
        nonce: &str,
        iat: i64,
        exp: i64,
    ) -> String {
        let claims = json!({
            "sub": subject,
            "scope": scope,
            "provider": provider,
            "method": request.method.as_str(),
            "path": request.path.as_str(),
            "route": request.route.as_deref().expect("route"),
            "request_id": request.request_id.as_deref().expect("request_id"),
            "body_digest": request.body_digest.as_deref().expect("body_digest"),
            "nonce": nonce,
            "iat": iat,
            "exp": exp,
        });
        let encoded_claims =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims json"));
        let signature =
            provider_request_signature(TEST_PROVIDER_HMAC_KEY.as_bytes(), &encoded_claims);
        format!("{PROVIDER_REQUEST_TOKEN_PREFIX}.{encoded_claims}.{signature}")
    }

    fn hmac_provider_service() -> DatabaseAuthService {
        DatabaseAuthService::new(
            astra_core::MatrixOneSettings::mock(),
            JwtSettings {
                secret_key: "test-secret-key-for-unit-tests".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 60,
                refresh_token_expire_days: 7,
            },
        )
        .with_provider_request_auth(vec![ProviderRequestAuthConfig {
            provider: "moi".to_string(),
            auth_type: "hmac".to_string(),
            key: TEST_PROVIDER_HMAC_KEY.to_string(),
            allowed_capability_descriptors: Vec::new(),
        }])
    }

    #[test]
    fn provider_request_hmac_golden_vector_uses_encoded_key_bytes() {
        let mut parts = GOLDEN_PROVIDER_TOKEN.split('.');
        assert_eq!(parts.next(), Some(PROVIDER_REQUEST_TOKEN_PREFIX));
        let encoded_claims = parts.next().expect("encoded claims");
        let encoded_signature = parts.next().expect("encoded signature");
        assert!(parts.next().is_none());

        assert!(verify_provider_request_signature(
            GOLDEN_PROVIDER_HMAC_KEY.as_bytes(),
            encoded_claims,
            encoded_signature,
        ));
        let decoded_key = URL_SAFE_NO_PAD
            .decode(GOLDEN_PROVIDER_HMAC_KEY)
            .expect("golden key should be base64url decodable");
        assert!(!verify_provider_request_signature(
            &decoded_key,
            encoded_claims,
            encoded_signature,
        ));
    }

    fn provider_headers(token: &str, provider: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .expect("authorization header"),
        );
        headers.insert(
            "x-astra-provider",
            provider.parse().expect("provider header"),
        );
        headers.insert(
            "x-astra-provider-action",
            "authorize_request".parse().expect("action header"),
        );
        headers
    }

    fn chat_stream_descriptor() -> ProviderRequestDescriptor {
        ProviderRequestDescriptor {
            method: "POST".to_string(),
            path: "/chat/stream".to_string(),
            route: Some("/chat/stream".to_string()),
            request_id: Some("request-1".to_string()),
            body_digest: Some("sha256-body".to_string()),
        }
    }

    #[tokio::test]
    async fn current_principal_for_request_verifies_provider_hmac_token() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now,
            now + 300,
        );
        let principal = hmac_provider_service()
            .current_principal_for_request(&provider_headers(&token, "moi"), descriptor)
            .await
            .expect("hmac provider request should resolve");

        assert_eq!(principal.user.user_id, "provider_authorized:moi:user_1");
        assert_eq!(principal.user.username, "user_1");
        match principal.origin {
            AuthPrincipalOrigin::ProviderAuthorizedRequest(context) => {
                assert_eq!(context.provider_id, "moi");
                assert_eq!(context.external_subject, "user_1");
                assert_eq!(context.provider_scope_id, "workspace_1");
                assert!(
                    context
                        .request_authorization_id
                        .starts_with(PROVIDER_REQUEST_TOKEN_PREFIX)
                );
            }
            other => panic!("expected provider-authorized request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn current_principal_for_request_preserves_hmac_subjects_per_user() {
        let now = Utc::now().timestamp();
        let service = hmac_provider_service();
        let descriptor = chat_stream_descriptor();
        let first = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now,
            now + 300,
        );
        let second = hmac_provider_token(
            "user_2",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-2",
            now,
            now + 300,
        );

        let first_principal = service
            .current_principal_for_request(&provider_headers(&first, "moi"), descriptor.clone())
            .await
            .expect("first hmac provider request should resolve");
        let second_principal = service
            .current_principal_for_request(&provider_headers(&second, "moi"), descriptor)
            .await
            .expect("second hmac provider request should resolve");

        assert_eq!(
            first_principal.user.user_id,
            "provider_authorized:moi:user_1"
        );
        assert_eq!(
            second_principal.user.user_id,
            "provider_authorized:moi:user_2"
        );
        assert_ne!(first_principal.user.user_id, second_principal.user.user_id);
    }

    #[tokio::test]
    async fn current_principal_for_request_rejects_expired_hmac_token() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now - 600,
            now - 300,
        );
        let (status, body) = hmac_provider_service()
            .current_principal_for_request(&provider_headers(&token, "moi"), descriptor)
            .await
            .expect_err("expired hmac provider request should be rejected");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("provider_request_auth_invalid")
        );
        assert!(body.0.detail.contains("expired"));
    }

    #[tokio::test]
    async fn current_principal_for_request_rejects_bad_hmac_signature() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now,
            now + 300,
        );
        let replacement = if token.ends_with('x') { "y" } else { "x" };
        let tampered = format!("{}{}", &token[..token.len() - 1], replacement);
        let (status, body) = hmac_provider_service()
            .current_principal_for_request(&provider_headers(&tampered, "moi"), descriptor)
            .await
            .expect_err("tampered hmac provider request should be rejected");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("provider_request_auth_invalid")
        );
        assert!(body.0.detail.contains("signature"));
    }

    #[tokio::test]
    async fn current_principal_for_request_rejects_provider_mismatch() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "other",
            &descriptor,
            "nonce-1",
            now,
            now + 300,
        );
        let (status, body) = hmac_provider_service()
            .current_principal_for_request(&provider_headers(&token, "moi"), descriptor)
            .await
            .expect_err("mismatched hmac provider request should be rejected");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("provider_request_auth_invalid")
        );
        assert!(body.0.detail.contains("provider"));
    }

    #[tokio::test]
    async fn current_principal_for_request_rejects_body_digest_mismatch() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now,
            now + 300,
        );
        let mut tampered_descriptor = descriptor;
        tampered_descriptor.body_digest = Some("sha256:tampered".to_string());

        let (status, body) = hmac_provider_service()
            .current_principal_for_request(&provider_headers(&token, "moi"), tampered_descriptor)
            .await
            .expect_err("token must be bound to the body digest");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.0.detail.contains("body_digest"));
    }

    #[tokio::test]
    async fn current_principal_for_request_rejects_replayed_hmac_token() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now,
            now + 300,
        );
        let service = hmac_provider_service();

        service
            .current_principal_for_request(&provider_headers(&token, "moi"), descriptor.clone())
            .await
            .expect("first use should succeed");
        let (status, body) = service
            .current_principal_for_request(&provider_headers(&token, "moi"), descriptor)
            .await
            .expect_err("second use of same request token must be rejected");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.0.detail.contains("already been used"));
    }

    #[tokio::test]
    async fn current_principal_for_request_rejects_hmac_token_ttl_above_maximum() {
        let now = Utc::now().timestamp();
        let descriptor = chat_stream_descriptor();
        let token = hmac_provider_token(
            "user_1",
            "workspace_1",
            "moi",
            &descriptor,
            "nonce-1",
            now,
            now + PROVIDER_REQUEST_MAX_TTL_SECONDS + 1,
        );

        let (status, body) = hmac_provider_service()
            .current_principal_for_request(&provider_headers(&token, "moi"), descriptor)
            .await
            .expect_err("long-lived provider request token must be rejected");

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.0.detail.contains("TTL"));
    }

    #[test]
    fn map_auth_sqlx_pool_timeout_is_retryable_database_error() {
        let (status, body) =
            map_auth_sqlx(sqlx::Error::PoolTimedOut, "auth.test_pool_timeout", None);

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0.error_code.as_deref(), Some("database_error"));
        assert!(body.0.detail.contains("pool timed out"));
    }

    #[test]
    fn map_auth_sqlx_other_sqlx_error_is_database_error() {
        let (status, body) = map_auth_sqlx(
            sqlx::Error::Protocol("database connection closed".into()),
            "auth.test_protocol",
            None,
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0.error_code.as_deref(), Some("database_error"));
        assert!(body.0.detail.contains("database connection closed"));
    }

    struct FakeRefreshTokenRow {
        failed_column: Option<&'static str>,
    }

    impl FakeRefreshTokenRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
            }
        }
    }

    impl RefreshTokenRow for FakeRefreshTokenRow {
        fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "user_id" => "user-1",
                "expires_at" => "2026-06-26T10:00:00",
                _ => unreachable!("unexpected refresh token column: {column}"),
            }
            .to_string())
        }

        fn optional_string_column(
            &self,
            column: &'static str,
        ) -> Result<Option<String>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "session_id" => Some("session-1".to_string()),
                _ => unreachable!("unexpected optional refresh token column: {column}"),
            })
        }
    }

    #[test]
    fn refresh_token_row_decode_preserves_database_values() {
        let (user_id, expires_at, session_id) =
            decode_refresh_token_row(&FakeRefreshTokenRow::complete())
                .expect("complete refresh token row should decode");

        assert_eq!(user_id, "user-1");
        assert_eq!(expires_at, "2026-06-26T10:00:00");
        assert_eq!(session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn refresh_token_row_decode_fails_loudly_on_any_missing_column() {
        for column in ["user_id", "expires_at", "session_id"] {
            match decode_refresh_token_row(&FakeRefreshTokenRow::fail_on(column)).unwrap_err() {
                sqlx::Error::ColumnNotFound(name) => assert_eq!(name, column),
                err => panic!("expected ColumnNotFound({column}), got {err:?}"),
            }
        }
    }

    #[test]
    fn database_auth_service_expires_in_uses_configured_access_ttl() {
        let service = DatabaseAuthService::new(
            astra_core::MatrixOneSettings::mock(),
            JwtSettings {
                secret_key: "test-secret-key-for-unit-tests".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 90,
                refresh_token_expire_days: 21,
            },
        );

        assert_eq!(service.access_token_expires_in_seconds(), 5_400);
    }

    #[test]
    fn database_auth_service_refresh_expiry_uses_configured_days() {
        use chrono::TimeZone;

        let service = DatabaseAuthService::new(
            astra_core::MatrixOneSettings::mock(),
            JwtSettings {
                secret_key: "test-secret-key-for-unit-tests".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 60,
                refresh_token_expire_days: 12,
            },
        );
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 0).unwrap();

        assert_eq!(
            service.refresh_token_expires_at_string(now),
            "2026-05-26 10:00:00"
        );
    }
}
