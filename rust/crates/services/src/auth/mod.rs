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
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use sqlx::{MySql, Row, query};
use std::{
    sync::Arc,
    time::{Duration as StdDuration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, warn};
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
    verifier: TrustedMoiJwtVerifier,
    algorithm: Algorithm,
    expected_issuer: Option<String>,
    expected_audience: Option<String>,
    leeway_seconds: u64,
}

impl fmt::Debug for TrustedMoiJwtSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedMoiJwtSettings")
            .field("verifier", &self.verifier)
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

#[derive(Clone)]
enum TrustedMoiJwtVerifier {
    SharedSecret {
        secret_key: String,
    },
    Jwks {
        jwks_url: String,
        cache_ttl: StdDuration,
        client: reqwest::Client,
        cache: Arc<RwLock<Option<TrustedMoiJwksCache>>>,
    },
}

impl fmt::Debug for TrustedMoiJwtVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SharedSecret { .. } => f
                .debug_struct("SharedSecret")
                .field("secret_key", &"[REDACTED]")
                .finish(),
            Self::Jwks {
                jwks_url,
                cache_ttl,
                ..
            } => f
                .debug_struct("Jwks")
                .field("jwks_url", jwks_url)
                .field("cache_ttl", cache_ttl)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Debug)]
struct TrustedMoiJwksCache {
    jwks: TrustedMoiJwks,
    expires_at: Instant,
}

#[derive(Clone, Debug, Deserialize)]
struct TrustedMoiJwks {
    keys: Vec<TrustedMoiJwk>,
}

#[derive(Clone, Debug, Deserialize)]
struct TrustedMoiJwk {
    kty: String,
    kid: Option<String>,
    #[serde(rename = "use")]
    key_use: Option<String>,
    alg: Option<String>,
    n: String,
    e: String,
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
        let algorithm = parse_trusted_moi_algorithm(algorithm)?;
        if !is_hmac_algorithm(algorithm) {
            return Err(format!(
                "ASTRA_EXTERNAL_JWT_SECRET verifier does not support {algorithm:?}"
            ));
        }
        Ok(Self {
            settings: TrustedMoiJwtSettings {
                verifier: TrustedMoiJwtVerifier::SharedSecret {
                    secret_key: normalize_jwt_secret_for_trusted_mode(secret_key),
                },
                algorithm,
                expected_issuer,
                expected_audience,
                leeway_seconds,
            },
        })
    }

    pub fn new_with_jwks_url(
        jwks_url: &str,
        algorithm: &str,
        expected_issuer: Option<String>,
        expected_audience: Option<String>,
        leeway_seconds: u64,
        cache_ttl_seconds: u64,
    ) -> Result<Self, String> {
        let algorithm = parse_trusted_moi_algorithm(algorithm)?;
        if !matches!(algorithm, Algorithm::RS256) {
            return Err(format!(
                "ASTRA_EXTERNAL_JWT_JWKS_URL verifier does not support {algorithm:?}"
            ));
        }
        let jwks_url = jwks_url.trim();
        if jwks_url.is_empty() {
            return Err("ASTRA_EXTERNAL_JWT_JWKS_URL must not be empty".to_string());
        }
        Ok(Self {
            settings: TrustedMoiJwtSettings {
                verifier: TrustedMoiJwtVerifier::Jwks {
                    jwks_url: jwks_url.to_string(),
                    cache_ttl: StdDuration::from_secs(cache_ttl_seconds.max(1)),
                    client: reqwest::Client::new(),
                    cache: Arc::new(RwLock::new(None)),
                },
                algorithm,
                expected_issuer,
                expected_audience,
                leeway_seconds,
            },
        })
    }

    pub fn from_env() -> Result<Self, String> {
        let algorithm =
            std::env::var("ASTRA_EXTERNAL_JWT_ALGORITHM").unwrap_or_else(|_| "HS256".to_string());

        let expected_issuer = std::env::var("ASTRA_EXTERNAL_JWT_ISSUER")
            .ok()
            .filter(|v| !v.is_empty());
        let expected_audience = std::env::var("ASTRA_EXTERNAL_JWT_AUDIENCE")
            .ok()
            .filter(|v| !v.is_empty());
        let leeway_seconds = std::env::var("ASTRA_EXTERNAL_JWT_LEEWAY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);

        let parsed_algorithm = parse_trusted_moi_algorithm(&algorithm)?;
        if matches!(parsed_algorithm, Algorithm::RS256) {
            let jwks_url = std::env::var("ASTRA_EXTERNAL_JWT_JWKS_URL")
                .map_err(|_| "ASTRA_EXTERNAL_JWT_JWKS_URL must be set for RS256".to_string())?;
            let cache_ttl_seconds = std::env::var("ASTRA_EXTERNAL_JWT_JWKS_CACHE_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(300);
            return Self::new_with_jwks_url(
                &jwks_url,
                &algorithm,
                expected_issuer,
                expected_audience,
                leeway_seconds,
                cache_ttl_seconds,
            );
        }

        let raw_secret = std::env::var("ASTRA_EXTERNAL_JWT_SECRET")
            .map_err(|_| "ASTRA_EXTERNAL_JWT_SECRET must be set".to_string())?;
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

    async fn decode_trusted_claims(
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

        let decoding_key = self.decoding_key(token).await?;
        decode::<TrustedMoiClaims>(token, &decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|_| error_response(StatusCode::UNAUTHORIZED, "Invalid trusted_moi token"))
    }

    async fn decoding_key(
        &self,
        token: &str,
    ) -> Result<DecodingKey, (StatusCode, Json<ErrorResponse>)> {
        match &self.settings.verifier {
            TrustedMoiJwtVerifier::SharedSecret { secret_key } => {
                Ok(DecodingKey::from_secret(secret_key.as_bytes()))
            }
            TrustedMoiJwtVerifier::Jwks { .. } => self.jwks_decoding_key(token).await,
        }
    }

    async fn jwks_decoding_key(
        &self,
        token: &str,
    ) -> Result<DecodingKey, (StatusCode, Json<ErrorResponse>)> {
        let header = decode_header(token)
            .map_err(|_| error_response(StatusCode::UNAUTHORIZED, "Invalid trusted_moi token"))?;
        if header.alg != self.settings.algorithm {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid trusted_moi token",
            ));
        }

        let cached = self.current_jwks(false).await?;
        if let Some(key) = select_jwks_key(&cached, header.kid.as_deref(), self.settings.algorithm)
        {
            return decoding_key_from_jwk(key);
        }

        let refreshed = self.current_jwks(true).await?;
        let key = select_jwks_key(&refreshed, header.kid.as_deref(), self.settings.algorithm)
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Invalid trusted_moi token"))?;
        decoding_key_from_jwk(key)
    }

    async fn current_jwks(
        &self,
        force_refresh: bool,
    ) -> Result<TrustedMoiJwks, (StatusCode, Json<ErrorResponse>)> {
        let TrustedMoiJwtVerifier::Jwks {
            jwks_url,
            cache_ttl,
            client,
            cache,
        } = &self.settings.verifier
        else {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Invalid trusted_moi token",
            ));
        };

        if !force_refresh {
            if let Some(cached) = cache.read().await.as_ref() {
                if Instant::now() < cached.expires_at {
                    return Ok(cached.jwks.clone());
                }
            }
        }

        let jwks = fetch_trusted_moi_jwks(client, jwks_url).await?;
        *cache.write().await = Some(TrustedMoiJwksCache {
            jwks: jwks.clone(),
            expires_at: Instant::now() + *cache_ttl,
        });
        Ok(jwks)
    }
}

fn parse_trusted_moi_algorithm(algorithm: &str) -> Result<Algorithm, String> {
    match algorithm {
        "HS256" => Ok(Algorithm::HS256),
        "HS384" => Ok(Algorithm::HS384),
        "HS512" => Ok(Algorithm::HS512),
        "RS256" => Ok(Algorithm::RS256),
        _ => Err(format!(
            "unsupported ASTRA_EXTERNAL_JWT_ALGORITHM: {algorithm}"
        )),
    }
}

fn is_hmac_algorithm(algorithm: Algorithm) -> bool {
    matches!(
        algorithm,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    )
}

async fn fetch_trusted_moi_jwks(
    client: &reqwest::Client,
    jwks_url: &str,
) -> Result<TrustedMoiJwks, (StatusCode, Json<ErrorResponse>)> {
    let response = client.get(jwks_url).send().await.map_err(|error| {
        warn!(
            target: "astra_services::auth",
            jwks_url,
            error = %error,
            "failed to fetch trusted_moi JWKS"
        );
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "trusted_moi JWKS unavailable",
        )
    })?;
    if !response.status().is_success() {
        warn!(
            target: "astra_services::auth",
            jwks_url,
            status = %response.status(),
            "trusted_moi JWKS endpoint returned non-success status"
        );
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "trusted_moi JWKS unavailable",
        ));
    }
    response.json::<TrustedMoiJwks>().await.map_err(|error| {
        warn!(
            target: "astra_services::auth",
            jwks_url,
            error = %error,
            "trusted_moi JWKS response is invalid"
        );
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "trusted_moi JWKS unavailable",
        )
    })
}

fn select_jwks_key<'a>(
    jwks: &'a TrustedMoiJwks,
    kid: Option<&str>,
    algorithm: Algorithm,
) -> Option<&'a TrustedMoiJwk> {
    let alg = algorithm_name(algorithm);
    let mut matches = jwks.keys.iter().filter(|key| {
        key.kty == "RSA"
            && key.key_use.as_deref().is_none_or(|value| value == "sig")
            && key.alg.as_deref().is_none_or(|value| value == alg)
            && kid.is_none_or(|wanted| key.kid.as_deref() == Some(wanted))
    });
    let first = matches.next()?;
    if kid.is_none() && matches.next().is_some() {
        debug!(
            target: "astra_services::auth",
            "trusted_moi token omitted kid and JWKS has multiple compatible keys"
        );
        return None;
    }
    Some(first)
}

fn decoding_key_from_jwk(
    key: &TrustedMoiJwk,
) -> Result<DecodingKey, (StatusCode, Json<ErrorResponse>)> {
    DecodingKey::from_rsa_components(&key.n, &key.e)
        .map_err(|_| error_response(StatusCode::UNAUTHORIZED, "Invalid trusted_moi token"))
}

fn algorithm_name(algorithm: Algorithm) -> &'static str {
    match algorithm {
        Algorithm::HS256 => "HS256",
        Algorithm::HS384 => "HS384",
        Algorithm::HS512 => "HS512",
        Algorithm::RS256 => "RS256",
        _ => "",
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
            expires_in: self.access_token_expires_in_seconds(),
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
        let claims = self.decode_trusted_claims(token).await?;

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
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const TEST_RSA_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCmvhq07p6Un447
pC8wc4oaEYev8uERKYNFF/wUD74/iaoe6VlGWXyat7rClSi0kiLlv+w2yZ4MVgrE
N4c3GSlXK07/MyZelHqRlbtQ+OLArQrrx72TMiEzdMJLGG6T1DYpOsuCfBVOiwzr
pfSwi4O6xvybvMKnLbUM/2DDHNqvlTag5THpWmeSyyNjIP+qBaepHqly0rKpCB77
pPqSA8vqdVRSDeEntnhxncSCllcawRQgxCr5aPQVjOBoKf1AB7C16TYNkpJcX0BX
+V0qiSnqrz/6vIp36WC6H4MnBiWoFKxYPwfbLB4LpGJK61C7wb8vw1s6VezyvpOA
iC2pg1QZAgMBAAECggEABakY0qvyceeJa73oT4lElsZMkxe4NfMbqTn7YppMN5pO
o6ur+QbrA3zu7VHPUXNP6vkdJZj/nA9EYKKP8mS7cgxJgMFgRpVczy9u7fydBLR7
KpjxIcZHDpj39ZZX6VMqyYiHmx+SQO8t2kbGgUMjOYaC0dinylweCYRr6QPEQByk
ZxCuKopOQaSU42Ax3Hj1TYmM4wmWj7ZDHtAA7jSEWKEipb4EMW5ApwlEsiAfYxRf
zLrWdsWBUmPU1FSp0r36hlffUSnvWgXDinxq88A6PRraU5CBUzSmI9APPv9SNl+o
gFHMnsS+MJPkv6NvHRabdhPwyA0w1pabQ5uHylLlWQKBgQDj/RUCEdLXX1uezcEo
eX0UTpIazMhMYL04QlRfOStAmAbr7CULG8aHMYLYJD90cvQmxhDKcZ3kPZl+Gmu4
fQe5DslsiQtbapp9UnvHjb+BXEpiJPL3qOL/PO+cZlfe+FxNY365+qBIZgHJhZDj
BJRxCqPMQfTQI470zPfEShbFTQKBgQC7OqdU2STIzK6lhB/Rb6++HraLoKx+y4Zt
oPbixzzHaEbLLRZGZPI3Be4KX9FZ6oINlGPOgCoEmTQe/cYm6cys4ZbD0VzK06WN
gboYsLFEEQBOzMYxGCu3ysGV+Hq9xnXb0rcHfO9e0ywK34dVFs8K1Hlh+xYpFtH5
Yt6q3IQz/QKBgQDcTO3i3RQt9r/SeKFAGfyqBa4aZWzamNPerAFZLiXEOeLeT4YP
8NvqQQZdEtGaFYYkfVk2NYlLRdauypryXyZ6RHaQAPDPefgkRvLChg7Z0jMyGOAK
PdBysBAcwawBEV4njY+j6DC/JIpvjzfMld1WSeCy+7yy7tkxZWm465qLNQKBgQCC
y4PQA23uFQdAu59auTJFl8Ego9s9LMM5XMR8QoFUMKWcFGBGRwjqpWrYtn1S2j+G
aw6aWPCBi+FccR53WsdQUrv3ChBP5TD3PRQbYXxEt7fGVMlzzJXl7G/2a8KbRsRZ
D8grI/04+j7/TY6GQ8vZnfs6FqUxiS6gkJBLPofgpQKBgBH3h12VTZ/jDesexlHm
OEmW4j/rA5cj4fi8Be6D8f/xXhF1uTmJovixtl4FIqQcCpDbKtDUpUA9DH0UXH+4
4Zcn3n3fkmvyxVoLvXcsj0wa/CIXY0sBTCAW2n0kavunKjk9f1wHutOYtB0idKeB
4ITcYBKDm5Lxlq0wofuA6ZEp
-----END PRIVATE KEY-----"#;

    const TEST_RSA_N: &str = "pr4atO6elJ-OO6QvMHOKGhGHr_LhESmDRRf8FA--P4mqHulZRll8mre6wpUotJIi5b_sNsmeDFYKxDeHNxkpVytO_zMmXpR6kZW7UPjiwK0K68e9kzIhM3TCSxhuk9Q2KTrLgnwVTosM66X0sIuDusb8m7zCpy21DP9gwxzar5U2oOUx6VpnkssjYyD_qgWnqR6pctKyqQge-6T6kgPL6nVUUg3hJ7Z4cZ3EgpZXGsEUIMQq-Wj0FYzgaCn9QAewtek2DZKSXF9AV_ldKokp6q8_-ryKd-lguh-DJwYlqBSsWD8H2yweC6RiSutQu8G_L8NbOlXs8r6TgIgtqYNUGQ";
    const TEST_RSA_E: &str = "AQAB";

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

    fn make_rs256_token(kid: &str, claims: TrustedMoiTestClaims<'_>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).expect("test rsa key"),
        )
        .expect("encode rs256 token")
    }

    async fn jwks_url_once(body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind jwks listener");
        let addr = listener.local_addr().expect("jwks local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept jwks request");
            let mut buffer = [0_u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write jwks response");
        });
        format!("http://{addr}/oauth/jwks")
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
    async fn trusted_moi_current_user_accepts_rs256_jwks_token() {
        let jwks = format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"moi-key-1","use":"sig","alg":"RS256","n":"{}","e":"{}"}}]}}"#,
            TEST_RSA_N, TEST_RSA_E
        );
        let jwks_url = jwks_url_once(jwks).await;
        let service = TrustedMoiAuthService::new_with_jwks_url(
            &jwks_url,
            "RS256",
            Some("moi".to_string()),
            Some("astra".to_string()),
            30,
            300,
        )
        .unwrap();
        let (iat, exp) = now_exp_pair();
        let token = make_rs256_token(
            "moi-key-1",
            TrustedMoiTestClaims {
                sub: Some("moi-user-rsa"),
                uid: None,
                user_id: None,
                username: Some("rsa-user"),
                email: Some("rsa@example.com"),
                display_name: Some("RSA User"),
                name: None,
                iss: Some("moi"),
                aud: Some("astra"),
                iat,
                exp,
            },
        );
        let user = service
            .current_user(&auth_headers(&token))
            .await
            .expect("current_user");
        assert_eq!(user.user_id, "moi-user-rsa");
        assert_eq!(user.username, "rsa-user");
        assert_eq!(user.email, "rsa@example.com");
        assert_eq!(user.display_name.as_deref(), Some("RSA User"));
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
