use crate::storage::database_user_from_row;
use astra_core::{
    ErrorResponse, ExternalAuthProviderConfig, JwtSettings, MatrixOneSettings, SharedPool,
    bearer_token, error_response, error_response_coded, internal_error, is_duplicate_key_error,
};
use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use bcrypt::{hash as bcrypt_hash, verify as bcrypt_verify};

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
use sqlx::{MySql, Row, query};
use tracing::warn;
use uuid::Uuid;

mod admin;
mod encryption;
pub mod external;
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
pub use external::{
    ExternalAuthorizeRequestData, ExternalAuthorizedRequest, ExternalCatalogResponse,
    ExternalLoginRequestData, ExternalProviderClient, ExternalProviderPublicRecord,
    ExternalRequestDescriptor, ExternalRuntimeContextRequestData, ExternalRuntimeContextResponse,
    ExternalSessionRecord, HttpExternalProviderClient,
};
use external::{
    ExternalProviderSessionHandle, decrypt_provider_session_handle,
    encrypt_provider_session_handle, resolve_selected_scope, validate_provider_runtime_context,
};
use jwt::{JwtTokenClaims, create_jwt_token, decode_jwt_claims, decode_jwt_claims_with_detail};
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
        _request: ExternalRequestDescriptor,
    ) -> Result<AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
        self.current_principal(headers).await
    }

    async fn external_providers(
        &self,
    ) -> Result<Vec<ExternalProviderPublicRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "External auth providers are not configured",
        ))
    }

    async fn external_login(
        &self,
        _request: ExternalLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "External auth providers are not configured",
        ))
    }

    async fn external_catalog(
        &self,
        _principal: &AuthPrincipal,
    ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "External runtime catalog is not configured",
        ))
    }

    async fn external_runtime_context(
        &self,
        _principal: &AuthPrincipal,
        _request: ExternalRuntimeContextRequestData,
    ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "External runtime context is not configured",
        ))
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

    pub fn is_external(&self) -> bool {
        matches!(self.origin, AuthPrincipalOrigin::External(_))
    }

    pub fn is_external_user_session(&self) -> bool {
        matches!(self.origin, AuthPrincipalOrigin::External(_))
    }

    pub fn is_external_authorized_request(&self) -> bool {
        matches!(
            self.origin,
            AuthPrincipalOrigin::ExternalAuthorizedRequest(_)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPrincipalOrigin {
    Internal,
    External(AuthExternalSessionContext),
    ExternalAuthorizedRequest(AuthExternalAuthorizedRequestContext),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthExternalSessionContext {
    pub provider_id: String,
    pub external_subject: String,
    pub external_session_id: String,
    pub provider_scope_id: String,
    pub provider_scope_display_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthExternalAuthorizedRequestContext {
    pub provider_id: String,
    pub external_subject: String,
    pub provider_scope_id: String,
    pub request_authorization_id: String,
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
    external_providers: Vec<ExternalAuthProviderConfig>,
    external_client: std::sync::Arc<dyn ExternalProviderClient>,
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
            .field("external_provider_count", &self.external_providers.len())
            .finish()
    }
}

impl DatabaseAuthService {
    pub fn new(matrixone: MatrixOneSettings, jwt: JwtSettings) -> Self {
        Self {
            matrixone,
            jwt,
            pool: None,
            encryptor: None,
            external_providers: Vec::new(),
            external_client: HttpExternalProviderClient::shared(),
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
    ) -> Result<Option<(String, String, Option<String>)>, sqlx::Error> {
        query(
            "SELECT user_id, DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at, session_id \
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
                    row.try_get("session_id").ok(),
                )
            })
        })
    }

    fn create_access_token(
        &self,
        user_id: &str,
        username: &str,
        session_id: &str,
        origin: &str,
        provider_id: Option<&str>,
    ) -> Result<String, String> {
        create_jwt_token(
            &self.jwt,
            JwtTokenClaims {
                sub: user_id.to_string(),
                username: Some(username.to_string()),
                token_type: "access".to_string(),
                sid: Some(session_id.to_string()),
                origin: Some(origin.to_string()),
                provider_id: provider_id.map(str::to_string),
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
        provider_id: Option<&str>,
    ) -> Result<String, String> {
        create_jwt_token(
            &self.jwt,
            JwtTokenClaims {
                sub: user_id.to_string(),
                username: None,
                token_type: "refresh".to_string(),
                sid: Some(session_id.to_string()),
                origin: Some(origin.to_string()),
                provider_id: provider_id.map(str::to_string),
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

    pub fn with_external_providers(mut self, providers: Vec<ExternalAuthProviderConfig>) -> Self {
        self.external_providers = providers;
        self
    }

    pub fn with_external_provider_client(
        mut self,
        client: std::sync::Arc<dyn ExternalProviderClient>,
    ) -> Self {
        self.external_client = client;
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(self.pool.as_ref(), "DatabaseAuthService", &self.matrixone)
    }

    fn encryptor(&self) -> Result<&FernetTokenEncryptor, (StatusCode, Json<ErrorResponse>)> {
        self.encryptor.as_ref().ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "External auth session encryption is not configured",
            )
        })
    }

    fn provider_config(
        &self,
        provider_id: &str,
    ) -> Result<&ExternalAuthProviderConfig, (StatusCode, Json<ErrorResponse>)> {
        self.external_providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    format!("Unknown external provider id '{provider_id}'"),
                    "external_provider_unknown",
                )
            })
    }

    fn external_request_auth_headers(
        headers: &HeaderMap,
    ) -> Result<ExternalRequestAuthHeaders, AuthHttpError> {
        let provider = header_exact(headers, "x-astra-external-provider")?;
        let action = header_exact(headers, "x-astra-external-action")?;
        match (provider, action) {
            (None, None) => Ok(None),
            (Some(provider), Some(action)) => {
                if action != "authorize_request" {
                    return Err(error_response_coded(
                        StatusCode::BAD_REQUEST,
                        "X-Astra-External-Action must be authorize_request",
                        "external_action_invalid",
                    ));
                }
                let token = bearer_token(headers)?;
                Ok(Some((provider, action, format!("Bearer {token}"))))
            }
            _ => Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "X-Astra-External-Provider and X-Astra-External-Action must be sent together",
                "external_request_auth_invalid",
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

    async fn fetch_external_session(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        session_id: &str,
        user_id: &str,
        provider_id: &str,
    ) -> Result<Option<ExternalSessionDbRecord>, sqlx::Error> {
        query(
            "SELECT s.external_session_id, s.provider_id, s.astra_user_id, s.external_subject, \
                    s.provider_scope_id, s.provider_scope_display_name, \
                    DATE_FORMAT(s.expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at, \
                    s.encrypted_provider_session_handle, \
                    i.username AS external_username, i.email AS external_email, \
                    i.display_name AS external_display_name \
             FROM auth_external_sessions s \
             JOIN auth_external_identities i \
               ON i.provider_id = s.provider_id AND i.external_subject = s.external_subject \
             WHERE s.external_session_id = ? \
               AND s.astra_user_id = ? \
               AND s.provider_id = ? \
               AND s.status = 'active' \
               AND s.expires_at > NOW() \
             LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(provider_id)
        .fetch_optional(executor)
        .await
        .map(|row| {
            row.map(|row| ExternalSessionDbRecord {
                session: ExternalSessionRecord {
                    external_session_id: row.try_get("external_session_id").unwrap_or_default(),
                    provider_id: row.try_get("provider_id").unwrap_or_default(),
                    astra_user_id: row.try_get("astra_user_id").unwrap_or_default(),
                    external_subject: row.try_get("external_subject").unwrap_or_default(),
                    provider_scope_id: row.try_get("provider_scope_id").unwrap_or_default(),
                    provider_scope_display_name: row.try_get("provider_scope_display_name").ok(),
                },
                provider_expires_at: row.try_get("expires_at").unwrap_or_default(),
                encrypted_provider_session_handle: row
                    .try_get("encrypted_provider_session_handle")
                    .unwrap_or_default(),
                external_username: row.try_get("external_username").unwrap_or_default(),
                external_email: row.try_get("external_email").ok(),
                external_display_name: row.try_get("external_display_name").ok(),
            })
        })
    }

    async fn fetch_active_external_session(
        &self,
        executor: impl sqlx::Executor<'_, Database = MySql>,
        session_id: &str,
        user_id: &str,
        provider_id: &str,
    ) -> Result<Option<ExternalSessionDbRecord>, sqlx::Error> {
        query(
            "SELECT s.external_session_id, s.provider_id, s.astra_user_id, s.external_subject, \
                    s.provider_scope_id, s.provider_scope_display_name, \
                    DATE_FORMAT(s.expires_at, '%Y-%m-%dT%H:%i:%s') AS expires_at, \
                    s.encrypted_provider_session_handle, \
                    i.username AS external_username, i.email AS external_email, \
                    i.display_name AS external_display_name \
             FROM auth_external_sessions s \
             JOIN auth_external_identities i \
               ON i.provider_id = s.provider_id AND i.external_subject = s.external_subject \
             WHERE s.external_session_id = ? \
               AND s.astra_user_id = ? \
               AND s.provider_id = ? \
               AND s.status = 'active' \
             LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(provider_id)
        .fetch_optional(executor)
        .await
        .map(|row| {
            row.map(|row| ExternalSessionDbRecord {
                session: ExternalSessionRecord {
                    external_session_id: row.try_get("external_session_id").unwrap_or_default(),
                    provider_id: row.try_get("provider_id").unwrap_or_default(),
                    astra_user_id: row.try_get("astra_user_id").unwrap_or_default(),
                    external_subject: row.try_get("external_subject").unwrap_or_default(),
                    provider_scope_id: row.try_get("provider_scope_id").unwrap_or_default(),
                    provider_scope_display_name: row.try_get("provider_scope_display_name").ok(),
                },
                provider_expires_at: row.try_get("expires_at").unwrap_or_default(),
                encrypted_provider_session_handle: row
                    .try_get("encrypted_provider_session_handle")
                    .unwrap_or_default(),
                external_username: row.try_get("external_username").unwrap_or_default(),
                external_email: row.try_get("external_email").ok(),
                external_display_name: row.try_get("external_display_name").ok(),
            })
        })
    }

    fn external_provider_session_expired(expires_at: &str) -> bool {
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        expires_at <= now.as_str()
    }

    async fn external_session_handle(
        &self,
        principal: &AuthPrincipal,
    ) -> Result<
        (ExternalAuthProviderConfig, ExternalProviderSessionHandle),
        (StatusCode, Json<ErrorResponse>),
    > {
        let AuthPrincipalOrigin::External(external) = &principal.origin else {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "External principal is required",
            ));
        };
        let provider = self.provider_config(&external.provider_id)?.clone();
        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        let session = self
            .fetch_external_session(
                &pool,
                &external.external_session_id,
                &principal.user.user_id,
                &external.provider_id,
            )
            .await
            .map_err(|e| map_auth_sqlx(e, "external.fetch_session", Some(&pool)))?
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::UNAUTHORIZED,
                    "External session expired or revoked",
                    "external_session_invalid",
                )
            })?;
        let handle = decrypt_provider_session_handle(
            self.encryptor()?,
            &session.encrypted_provider_session_handle,
        )?;
        Ok((
            provider,
            ExternalProviderSessionHandle {
                provider_session_handle: handle,
                provider_scope_id: session.session.provider_scope_id,
            },
        ))
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
            "external" => {
                let provider_id = claims.provider_id.clone().ok_or_else(|| {
                    error_response(StatusCode::UNAUTHORIZED, "Invalid token provider")
                })?;
                Ok((session_id, origin, Some(provider_id)))
            }
            _ => Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid token origin",
            )),
        }
    }

    fn parse_provider_expires_at(
        &self,
        raw: &str,
    ) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
        chrono::DateTime::parse_from_rfc3339(raw)
            .map(|dt| {
                dt.with_timezone(&Utc)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .map_err(|error| {
                error_response_coded(
                    StatusCode::BAD_GATEWAY,
                    format!("external provider session expiry is invalid: {error}"),
                    "external_provider_response_invalid",
                )
            })
    }

    async fn complete_external_auth(
        &self,
        provider: &ExternalAuthProviderConfig,
        requested_scope_id: Option<&str>,
        response: external::ExternalProviderAuthResponse,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        let selected_scope = resolve_selected_scope(requested_scope_id, &response)?;
        let encrypted_handle =
            encrypt_provider_session_handle(self.encryptor()?, &response.provider_session_handle)?;
        let external_subject = response.external_subject.id.clone();
        let external_username = response.display_info.username.clone();
        let external_email = response.display_info.email.clone();
        let external_display_name = response.display_info.nickname.clone();
        let pool = self
            .get_pool()
            .await
            .map_err(|e| map_auth_sqlx(e, "auth.get_pool", None))?;
        let now = Utc::now();
        let external_expires_at = self.parse_provider_expires_at(&response.expires_at)?;
        let astra_session_id = Uuid::new_v4().to_string();
        let astra_user_id = Uuid::new_v4().to_string();
        let internal_username = format!("ext_{}", astra_user_id.replace('-', ""));
        let internal_email = format!("{internal_username}@external.astra.invalid");

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| map_auth_sqlx(e, "external.begin_tx", Some(&pool)))?;
        let existing_identity = query(
            "SELECT astra_user_id FROM auth_external_identities \
             WHERE provider_id = ? AND external_subject = ? LIMIT 1",
        )
        .bind(&provider.id)
        .bind(&external_subject)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_auth_sqlx(e, "external.fetch_identity", Some(&pool)))?;

        let astra_user_id = if let Some(row) = existing_identity {
            let existing_user_id: String = row.try_get("astra_user_id").unwrap_or_default();
            query(
                "UPDATE auth_external_identities \
                 SET username = ?, email = ?, display_name = ?, updated_at = NOW() \
                 WHERE provider_id = ? AND external_subject = ?",
            )
            .bind(&external_username)
            .bind(&external_email)
            .bind(&external_display_name)
            .bind(&provider.id)
            .bind(&external_subject)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "external.update_identity", Some(&pool)))?;
            existing_user_id
        } else {
            query(
                "INSERT INTO auth_users \
                 (user_id, username, email, password_hash, display_name, is_active) \
                 VALUES (?, ?, ?, '', ?, 1)",
            )
            .bind(&astra_user_id)
            .bind(&internal_username)
            .bind(&internal_email)
            .bind(&external_display_name)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "external.insert_auth_user", Some(&pool)))?;
            query(
                "INSERT INTO auth_external_identities \
                 (provider_id, external_subject, astra_user_id, username, email, display_name) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&provider.id)
            .bind(&external_subject)
            .bind(&astra_user_id)
            .bind(&external_username)
            .bind(&external_email)
            .bind(&external_display_name)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "external.insert_identity", Some(&pool)))?;
            astra_user_id
        };

        query(
            "INSERT INTO auth_external_sessions \
             (external_session_id, provider_id, astra_user_id, external_subject, \
              provider_scope_id, provider_scope_display_name, encrypted_provider_session_handle, \
              status, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?)",
        )
        .bind(&astra_session_id)
        .bind(&provider.id)
        .bind(&astra_user_id)
        .bind(&external_subject)
        .bind(&selected_scope.id)
        .bind(&selected_scope.name)
        .bind(&encrypted_handle)
        .bind(&external_expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_auth_sqlx(e, "external.insert_session", Some(&pool)))?;

        let access_token = self
            .create_access_token(
                &astra_user_id,
                &external_username,
                &astra_session_id,
                "external",
                Some(&provider.id),
            )
            .map_err(internal_error)?;
        let refresh_token = self
            .create_refresh_token(
                &astra_user_id,
                &astra_session_id,
                "external",
                Some(&provider.id),
            )
            .map_err(internal_error)?;
        let refresh_token_hash = sha256_hex(&refresh_token);
        let refresh_expires_at = self.refresh_token_expires_at_string(now);

        query(
            "INSERT INTO auth_refresh_tokens \
             (token_id, user_id, session_id, token_hash, expires_at, is_revoked) \
             VALUES (?, ?, ?, ?, ?, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&astra_user_id)
        .bind(&astra_session_id)
        .bind(&refresh_token_hash)
        .bind(refresh_expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_auth_sqlx(e, "external.insert_refresh_token", Some(&pool)))?;

        tx.commit()
            .await
            .map_err(|e| map_auth_sqlx(e, "external.commit_tx", Some(&pool)))?;

        Ok(AuthTokenRecord {
            access_token,
            refresh_token,
            token_type: "bearer".to_string(),
            expires_in: self.access_token_expires_in_seconds(),
        })
    }
}

#[derive(Clone, Debug)]
struct ExternalSessionDbRecord {
    session: ExternalSessionRecord,
    provider_expires_at: String,
    encrypted_provider_session_handle: String,
    external_username: String,
    external_email: Option<String>,
    external_display_name: Option<String>,
}

#[derive(Clone, Debug)]
struct ExternalSessionRefreshUpdate {
    encrypted_provider_session_handle: String,
    provider_scope_id: String,
    expires_at: String,
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
            "external_request_auth_invalid",
        )
    })?;
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{name} must be a non-empty exact string"),
            "external_request_auth_invalid",
        ));
    }
    Ok(Some(value.to_string()))
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
            .create_access_token(&user.user_id, &user.username, &session_id, "internal", None)
            .map_err(internal_error)?;
        let refresh_token = self
            .create_refresh_token(&user.user_id, &session_id, "internal", None)
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
        let (session_id, origin, provider_id) = self.parse_token_session(&claims)?;

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
        let mut external_session_refresh: Option<ExternalSessionRefreshUpdate> = None;
        if origin == "external" {
            let provider_id = provider_id.as_deref().ok_or_else(|| {
                error_response(StatusCode::UNAUTHORIZED, "Invalid token provider")
            })?;
            let session = self
                .fetch_active_external_session(&pool, &session_id, &user_id, provider_id)
                .await
                .map_err(|e| map_auth_sqlx(e, "refresh.fetch_external_session", Some(&pool)))?
                .ok_or_else(|| {
                    error_response_coded(
                        StatusCode::UNAUTHORIZED,
                        "External session is revoked or unknown",
                        "external_session_invalid",
                    )
                })?;
            if Self::external_provider_session_expired(&session.provider_expires_at) {
                let provider = self.provider_config(provider_id)?.clone();
                let provider_session_handle = decrypt_provider_session_handle(
                    self.encryptor()?,
                    &session.encrypted_provider_session_handle,
                )?;
                let refreshed = self
                    .external_client
                    .refresh_session(
                        &provider,
                        ExternalProviderSessionHandle {
                            provider_session_handle,
                            provider_scope_id: session.session.provider_scope_id,
                        },
                    )
                    .await?;
                if refreshed.provider_scope_id.is_empty() {
                    return Err(error_response_coded(
                        StatusCode::BAD_GATEWAY,
                        "external provider refresh_session returned empty provider_scope_id",
                        "external_provider_response_invalid",
                    ));
                }
                let encrypted_provider_session_handle = encrypt_provider_session_handle(
                    self.encryptor()?,
                    &refreshed.provider_session_handle,
                )?;
                external_session_refresh = Some(ExternalSessionRefreshUpdate {
                    encrypted_provider_session_handle,
                    provider_scope_id: refreshed.provider_scope_id,
                    expires_at: self.parse_provider_expires_at(&refreshed.expires_at)?,
                });
            }
        }

        let user = self
            .fetch_user_by_id_or_username(&pool, &user_id, None)
            .await
            .map_err(|e| map_auth_sqlx(e, "refresh.fetch_user_by_id", Some(&pool)))?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User not found"))?;

        let access_token = self
            .create_access_token(
                &user.user_id,
                &user.username,
                &session_id,
                &origin,
                provider_id.as_deref(),
            )
            .map_err(internal_error)?;
        let new_refresh_token = self
            .create_refresh_token(&user.user_id, &session_id, &origin, provider_id.as_deref())
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
        if let Some(update) = external_session_refresh {
            query(
                "UPDATE auth_external_sessions \
                 SET encrypted_provider_session_handle = ?, provider_scope_id = ?, expires_at = ?, updated_at = NOW() \
                 WHERE external_session_id = ? AND astra_user_id = ? AND provider_id = ? AND status = 'active'",
            )
            .bind(update.encrypted_provider_session_handle)
            .bind(update.provider_scope_id)
            .bind(update.expires_at)
            .bind(&session_id)
            .bind(&user_id)
            .bind(provider_id.as_deref().unwrap_or_default())
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "refresh.update_external_session", Some(&pool)))?;
        }
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
        let (session_id, origin, provider_id) = self.parse_token_session(&claims)?;

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

        if origin == "external" {
            let provider_id_ref = provider_id.as_deref().ok_or_else(|| {
                error_response(StatusCode::UNAUTHORIZED, "Invalid token provider")
            })?;
            let session = self
                .fetch_active_external_session(&pool, &session_id, &user_id, provider_id_ref)
                .await
                .map_err(|e| map_auth_sqlx(e, "logout.fetch_external_session", Some(&pool)))?
                .ok_or_else(|| {
                    error_response_coded(
                        StatusCode::UNAUTHORIZED,
                        "External session is revoked or unknown",
                        "external_session_invalid",
                    )
                })?;
            let provider = self.provider_config(provider_id_ref)?.clone();
            let provider_session_handle = decrypt_provider_session_handle(
                self.encryptor()?,
                &session.encrypted_provider_session_handle,
            )?;
            self.external_client
                .logout(
                    &provider,
                    ExternalProviderSessionHandle {
                        provider_session_handle,
                        provider_scope_id: session.session.provider_scope_id,
                    },
                )
                .await?;
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

        if origin == "external" {
            let provider_id = provider_id.ok_or_else(|| {
                error_response(StatusCode::UNAUTHORIZED, "Invalid token provider")
            })?;
            query(
                "UPDATE auth_external_sessions \
                 SET status = 'revoked', updated_at = NOW() \
                 WHERE external_session_id = ? AND astra_user_id = ? AND provider_id = ?",
            )
            .bind(&session_id)
            .bind(&user_id)
            .bind(provider_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_auth_sqlx(e, "logout.revoke_external_session", Some(&pool)))?;
        }

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
        let (session_id, origin, provider_id) = self.parse_token_session(&claims)?;
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

        if origin == "external" {
            let provider_id = provider_id.ok_or_else(|| {
                error_response(StatusCode::UNAUTHORIZED, "Invalid token provider")
            })?;
            let session = self
                .fetch_external_session(&pool, &session_id, &user_id, &provider_id)
                .await
                .map_err(|e| map_auth_sqlx(e, "current_principal.fetch_external", Some(&pool)))?
                .ok_or_else(|| {
                    error_response_coded(
                        StatusCode::UNAUTHORIZED,
                        "External session expired or revoked",
                        "external_session_invalid",
                    )
                })?;
            return Ok(AuthPrincipal {
                user: AuthUserRecord {
                    user_id: user_id.clone(),
                    username: session.external_username,
                    email: session.external_email.unwrap_or_default(),
                    display_name: session.external_display_name,
                },
                session_id: Some(session_id),
                origin: AuthPrincipalOrigin::External(AuthExternalSessionContext {
                    provider_id,
                    external_subject: session.session.external_subject,
                    external_session_id: session.session.external_session_id,
                    provider_scope_id: session.session.provider_scope_id,
                    provider_scope_display_name: session.session.provider_scope_display_name,
                }),
            });
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
        request: ExternalRequestDescriptor,
    ) -> Result<AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
        let Some((provider_id, _action, token)) = Self::external_request_auth_headers(headers)?
        else {
            return self.current_principal(headers).await;
        };
        let provider = self.provider_config(&provider_id)?.clone();
        let authorized = self
            .external_client
            .authorize_request(
                &provider,
                ExternalAuthorizeRequestData {
                    provider_id: provider_id.clone(),
                    token,
                    request,
                },
            )
            .await?;
        let user_id = format!(
            "external_authorized:{}:{}",
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
            origin: AuthPrincipalOrigin::ExternalAuthorizedRequest(
                AuthExternalAuthorizedRequestContext {
                    provider_id: authorized.provider_id,
                    external_subject: authorized.external_subject,
                    provider_scope_id: authorized.provider_scope_id,
                    request_authorization_id: authorized.request_authorization_id,
                },
            ),
        })
    }

    async fn external_providers(
        &self,
    ) -> Result<Vec<ExternalProviderPublicRecord>, (StatusCode, Json<ErrorResponse>)> {
        Ok(self
            .external_providers
            .iter()
            .map(ExternalProviderPublicRecord::from)
            .collect())
    }

    async fn external_login(
        &self,
        request: ExternalLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        let provider = self.provider_config(&request.provider_id)?.clone();
        let requested_scope_id = request.scope_id.clone();
        let response = self
            .external_client
            .authenticate(&provider, request)
            .await?;
        self.complete_external_auth(&provider, requested_scope_id.as_deref(), response)
            .await
    }

    async fn external_catalog(
        &self,
        principal: &AuthPrincipal,
    ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)> {
        let (provider, session) = self.external_session_handle(principal).await?;
        self.external_client.list_catalog(&provider, session).await
    }

    async fn external_runtime_context(
        &self,
        principal: &AuthPrincipal,
        request: ExternalRuntimeContextRequestData,
    ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)> {
        let (provider, session) = self.external_session_handle(principal).await?;
        let requested_model_id = request.requested_model_id.clone();
        let context = self
            .external_client
            .issue_runtime_context(&provider, session, request)
            .await?;
        validate_provider_runtime_context(&provider, &requested_model_id, &context)?;
        Ok(context)
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
    use async_trait::async_trait;
    use chrono::Utc;
    struct AuthorizingProviderClient;

    #[async_trait]
    impl ExternalProviderClient for AuthorizingProviderClient {
        async fn authorize_request(
            &self,
            provider: &ExternalAuthProviderConfig,
            request: ExternalAuthorizeRequestData,
        ) -> Result<ExternalAuthorizedRequest, (StatusCode, Json<ErrorResponse>)> {
            assert_eq!(provider.id, "moi");
            assert_eq!(request.token, "Bearer provider-token");
            assert_eq!(request.request.method, "POST");
            assert_eq!(request.request.path, "/chat/stream");
            assert!(request.request.body_digest.is_none());
            Ok(ExternalAuthorizedRequest {
                provider_id: "moi".to_string(),
                external_subject: "moi-user-1".to_string(),
                provider_scope_id: "workspace-1".to_string(),
                request_authorization_id: "authz-1".to_string(),
            })
        }

        async fn authenticate(
            &self,
            _provider: &ExternalAuthProviderConfig,
            _request: ExternalLoginRequestData,
        ) -> Result<external::ExternalProviderAuthResponse, (StatusCode, Json<ErrorResponse>)>
        {
            unimplemented!("not used by header authorization test")
        }

        async fn list_catalog(
            &self,
            _provider: &ExternalAuthProviderConfig,
            _session: ExternalProviderSessionHandle,
        ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used by header authorization test")
        }

        async fn issue_runtime_context(
            &self,
            _provider: &ExternalAuthProviderConfig,
            _session: ExternalProviderSessionHandle,
            _request: ExternalRuntimeContextRequestData,
        ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used by header authorization test")
        }

        async fn refresh_session(
            &self,
            _provider: &ExternalAuthProviderConfig,
            _session: ExternalProviderSessionHandle,
        ) -> Result<external::ExternalRefreshSessionResponse, (StatusCode, Json<ErrorResponse>)>
        {
            unimplemented!("not used by header authorization test")
        }

        async fn logout(
            &self,
            _provider: &ExternalAuthProviderConfig,
            _session: ExternalProviderSessionHandle,
        ) -> Result<external::ExternalLogoutResponse, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used by header authorization test")
        }
    }

    #[tokio::test]
    async fn current_principal_for_request_uses_external_authorization_headers() {
        let service = DatabaseAuthService::new(
            astra_core::MatrixOneSettings::mock(),
            JwtSettings {
                secret_key: "test-secret-key-for-unit-tests".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 60,
                refresh_token_expire_days: 7,
            },
        )
        .with_external_providers(vec![ExternalAuthProviderConfig {
            id: "moi".to_string(),
            display_name: "MOI".to_string(),
            external_auth_endpoint: "http://127.0.0.1/external-auth".to_string(),
        }])
        .with_external_provider_client(std::sync::Arc::new(AuthorizingProviderClient));
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer provider-token"
                .parse()
                .expect("authorization header"),
        );
        headers.insert(
            "x-astra-external-provider",
            "moi".parse().expect("provider header"),
        );
        headers.insert(
            "x-astra-external-action",
            "authorize_request".parse().expect("action header"),
        );

        let principal = service
            .current_principal_for_request(
                &headers,
                ExternalRequestDescriptor {
                    method: "POST".to_string(),
                    path: "/chat/stream".to_string(),
                    route: Some("/chat/stream".to_string()),
                    request_id: None,
                    body_digest: None,
                },
            )
            .await
            .expect("header-authorized request should resolve");

        match principal.origin {
            AuthPrincipalOrigin::ExternalAuthorizedRequest(context) => {
                assert_eq!(context.provider_id, "moi");
                assert_eq!(context.external_subject, "moi-user-1");
                assert_eq!(context.provider_scope_id, "workspace-1");
                assert_eq!(context.request_authorization_id, "authz-1");
            }
            other => panic!("expected external_authorized_request, got {other:?}"),
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
