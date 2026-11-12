//! Owner-scoped ChatGPT subscription connections. OAuth is a model credential,
//! not an Astra login; renewable credentials never cross the control API.

use crate::auth::FernetTokenEncryptor;
use crate::{CancellationSafePoolConnection, CancellationSafeTransaction};
use astra_core::model_wire::responses::ResponsesStreamError;
use astra_core::{ErrorResponse, SharedPool, error_response_coded, internal_error};
use astra_credentials::chatgpt::protocol::{self, AuthorizationRequest, Identity};
use axum::{Json, http::StatusCode};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use jsonwebtoken::jwk::JwkSet;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{MySqlPool, Row, query};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const PROVIDER: &str = "chatgpt";
#[cfg(test)]
mod tests;
pub use astra_credentials::chatgpt::API_BASE;
use astra_credentials::chatgpt::{ISSUER, PLAN_SCOPE};
const AUTH_LIFETIME: i64 = 900;
const REFRESH_BUDGET: Duration = Duration::from_secs(25);
const REFRESH_LEASE_SECONDS: i64 = 30;
/// Hard bound on the in-memory access-token cache. Entries hold plaintext
/// bearer tokens, so historical connections must never accumulate without
/// limit; expired entries are reclaimed before any live one is evicted.
const MAX_CACHED_TOKENS: usize = 64;
type ApiResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

fn fail(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(status, message, code)
}
fn not_found() -> (StatusCode, Json<ErrorResponse>) {
    fail(
        StatusCode::NOT_FOUND,
        "model_connection_not_found",
        "Model connection not found",
    )
}
fn reauth() -> (StatusCode, Json<ErrorResponse>) {
    fail(
        StatusCode::CONFLICT,
        "model_connection_reauthentication_required",
        "Reconnect the selected ChatGPT account",
    )
}
fn unavailable() -> (StatusCode, Json<ErrorResponse>) {
    fail(
        StatusCode::SERVICE_UNAVAILABLE,
        "chatgpt_connection_unavailable",
        "OpenAI connection failed",
    )
}
fn unix_now() -> i64 {
    Utc::now().timestamp()
}
fn connected_generation(row: &ConnectionFence, generation: i64) -> ApiResult<()> {
    if row.status != "connected" || row.generation != generation {
        return Err(reauth());
    }
    Ok(())
}

/// Preserve only documented machine codes; upstream prose may contain secrets.
pub fn provider_error_code(status: u16, body: &Value) -> &'static str {
    let raw = body
        .pointer("/error/code")
        .and_then(Value::as_str)
        .or_else(|| body["error"].as_str());
    if let Some((code, _)) = raw.and_then(astra_core::model_wire::responses::provider_error) {
        return code;
    }
    if matches!(raw, Some("invalid_grant" | "invalid_token")) {
        return "model_connection_reauthentication_required";
    }
    match status {
        401 => "model_connection_reauthentication_required",
        403 => "subscription_provider_forbidden",
        429 => "provider_rate_limited",
        _ => "subscription_provider_rejected",
    }
}
fn provider_failure(status: StatusCode, bytes: &[u8]) -> (StatusCode, Json<ErrorResponse>) {
    let body = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let code = provider_error_code(status.as_u16(), &body);
    fail(
        if code == "model_connection_reauthentication_required" {
            StatusCode::CONFLICT
        } else {
            status
        },
        code,
        "OpenAI rejected the subscription request; inspect the selected connection and account usage",
    )
}

async fn provider_body(mut response: reqwest::Response) -> ApiResult<Vec<u8>> {
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| internal_error("OpenAI response was interrupted"))?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(internal_error("OpenAI response exceeded its size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(provider_failure(status, &bytes));
    }
    Ok(bytes)
}

/// Typed failure for a probe whose stream never completed. Recognized protocol
/// codes keep their own status, so eligibility, quota, and length rejections
/// stay distinguishable from a transport/format failure. Upstream prose is
/// never echoed into the public detail; the verbatim provider code is
/// server-side evidence, length-capped because it is untrusted input.
fn model_check_failure(failure: &ResponsesStreamError) -> (StatusCode, Json<ErrorResponse>) {
    let mapped = failure.mapped();
    let upstream_code: String = match failure {
        ResponsesStreamError::Provider(Some(code)) => code.chars().take(128).collect(),
        ResponsesStreamError::Provider(None) => "provider_terminal_event_without_code".into(),
        ResponsesStreamError::Codec(reason) => (*reason).into(),
    };
    tracing::warn!(
        upstream_code,
        mapped_status = mapped.map(|(_, status)| status),
        "ChatGPT subscription model check did not complete"
    );
    match mapped {
        Some((code, status)) => {
            let detail = match code {
                "subscription_sharing_user_not_eligible" => {
                    "This ChatGPT subscription is not eligible for Astra"
                }
                "subscription_sharing_usage_limit_exceeded" => {
                    "ChatGPT subscription usage limit reached"
                }
                "subscription_sharing_usage_unavailable" => {
                    "ChatGPT subscription usage is temporarily unavailable"
                }
                _ => "The subscription model rejected the check request",
            };
            fail(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                code,
                detail,
            )
        }
        None => fail(
            StatusCode::BAD_GATEWAY,
            "chatgpt_model_check_failed",
            "Subscription model check did not complete",
        ),
    }
}

#[derive(Clone, Debug)]
pub struct ChatgptConfig {
    pub enabled: bool,
    pub host_id: String,
    issuer: String,
    api_base: String,
    authorize: String,
    token: String,
    discovery: String,
    test_endpoints: bool,
}
impl ChatgptConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_settings(
            std::env::var("ASTRA_CHATGPT_SUBSCRIPTION_ENABLED")
                .ok()
                .as_deref(),
            std::env::var("ASTRA_CHATGPT_HOST_ID").unwrap_or_default(),
        )
    }
    fn from_settings(enabled: Option<&str>, host_id: String) -> Result<Self, String> {
        let enabled = match enabled {
            None | Some("" | "1" | "true") => true,
            Some("0" | "false") => false,
            _ => return Err("ASTRA_CHATGPT_SUBSCRIPTION_ENABLED must be 0 or 1".into()),
        };
        if enabled
            && !host_id.is_empty()
            && (!host_id.starts_with("urn:uuid:") || Uuid::parse_str(&host_id[9..]).is_err())
        {
            return Err("ASTRA_CHATGPT_HOST_ID must be urn:uuid:<UUID> when explicitly set".into());
        }
        Ok(Self {
            enabled,
            host_id,
            issuer: ISSUER.into(),
            api_base: API_BASE.into(),
            authorize: format!("{ISSUER}/api/accounts/authorize"),
            token: format!("{ISSUER}/api/accounts/oauth/token"),
            discovery: format!("{ISSUER}/.well-known/openid-configuration"),
            test_endpoints: false,
        })
    }
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn for_test(base: &str) -> Self {
        Self {
            enabled: true,
            host_id: Uuid::new_v4().urn().to_string(),
            issuer: base.into(),
            api_base: format!("{base}/v1"),
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            discovery: format!("{base}/.well-known/openid-configuration"),
            test_endpoints: true,
        }
    }
    fn provider_url(&self, value: &str) -> ApiResult<Url> {
        let url = Url::parse(value).map_err(|_| internal_error("Invalid OpenAI metadata URL"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || (!self.test_endpoints
                && (url.scheme() != "https"
                    || url.host_str() != Some("auth.openai.com")
                    || url.port_or_known_default() != Some(443)))
        {
            return Err(internal_error("OpenAI metadata changed its trusted origin"));
        }
        Ok(url)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartAuthorization {
    pub retry_authorization_id: Option<String>,
    pub redirect_uri: String,
    pub label: Option<String>,
    pub connection_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteAuthorization {
    pub state: String,
    pub code: Option<String>,
    pub client_id: Option<String>,
    pub error: Option<String>,
}
impl std::fmt::Debug for CompleteAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompleteAuthorization")
            .finish_non_exhaustive()
    }
}
pub enum ConnectionOperation {
    List,
    Start(StartAuthorization),
    Complete(String, CompleteAuthorization),
    Cancel(String),
    Models(String),
    Disconnect(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConnectionView {
    pub remote_revocation_confirmed: Option<bool>,
    pub connection_id: String,
    pub provider: String,
    pub label: String,
    pub email: Option<String>,
    pub status: String,
    pub expires_at: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Credentials {
    access_token: String,
    refresh_token: Option<String>,
    scopes: Vec<String>,
    expires_at: i64,
}
#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
    #[serde(default)]
    expires_in: i64,
}

#[derive(sqlx::FromRow)]
struct AuthorizationRow {
    state_digest: String,
    status: String,
    callback_digest: Option<String>,
    completed_connection_id: Option<String>,
    expires_at: i64,
    client_id: String,
    verifier_encrypted: Option<String>,
    redirect_uri: String,
    nonce_digest: String,
    expected_subject: Option<String>,
    connection_id: Option<String>,
    label: String,
}

#[derive(sqlx::FromRow)]
struct ConnectionFence {
    status: String,
    generation: i64,
}

#[derive(sqlx::FromRow)]
struct CredentialRow {
    #[sqlx(flatten)]
    fence: ConnectionFence,
    credential_version: i64,
    credentials_encrypted: Option<String>,
    token_expires_at: Option<i64>,
    refresh_owner: Option<String>,
    refresh_until: Option<i64>,
    client_id: String,
    subject: String,
}

type ConnectionKey = (String, String);
type RefreshKey = (String, String, i64);
/// Refresh flights settle into (token, credential_version, expires_at) so
/// every waiter can replace the stale cache entry, not just bypass it once.
type RefreshWaiter = tokio::sync::watch::Receiver<Option<ApiResult<(String, i64, i64)>>>;
#[derive(Default)]
struct ActiveRegistry {
    connections: HashMap<ConnectionKey, ActiveConnection>,
    monitor: Option<ConnectionMonitor>,
}
impl ActiveRegistry {
    fn apply_observations(
        &mut self,
        targets: &[(ConnectionKey, Uuid)],
        rows: Option<&[Option<ConnectionRow>]>,
    ) {
        for (index, (key, identity)) in targets.iter().enumerate() {
            let Some(entry) = self
                .connections
                .get_mut(key)
                .filter(|entry| entry.monitor == *identity)
            else {
                continue;
            };
            let row = rows.and_then(|rows| rows[index].as_ref());
            for (_, generation, cancel) in &entry.requests {
                if row.is_none_or(|row| {
                    *generation < row.generation
                        || (*generation == row.generation && row.status != "connected")
                }) {
                    cancel.cancel();
                }
            }
            entry
                .requests
                .retain(|(_, _, cancel)| !cancel.is_cancelled());
            if entry.requests.is_empty() {
                self.connections.remove(key);
            }
        }
    }
}
struct ConnectionMonitor {
    id: Uuid,
    stop: CancellationToken,
}
struct MonitorGuard {
    service: Weak<ChatgptConnections>,
    id: Uuid,
    armed: bool,
}
impl Drop for MonitorGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(service) = self.service.upgrade() else {
            return;
        };
        let connections = {
            let mut active = service.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.monitor.as_ref().map(|monitor| monitor.id) != Some(self.id) {
                return;
            }
            active.monitor = None;
            std::mem::take(&mut active.connections)
        };
        for entry in connections.into_values() {
            for (_, _, cancel) in entry.requests {
                cancel.cancel();
            }
        }
    }
}
#[derive(sqlx::FromRow)]
struct ConnectionRow {
    status: String,
    generation: i64,
    host_id: String,
    label: String,
}
const CONNECTION_READ_BATCH: usize = 256;
const CONNECTION_READ_CONCURRENCY: usize = 4;

struct ActiveConnection {
    monitor: Uuid,
    requests: Vec<(Uuid, i64, CancellationToken)>,
}
struct CachedAccessToken {
    token: String,
    version: i64,
    generation: i64,
    expires_at: i64,
}
/// Result of adjudicating a row or refresh result while its guard is pinned.
enum CacheVerdict {
    Serve(String, i64, i64),
    /// The observed row needs a refresh; carry the effective rejection floor.
    Refresh(Option<i64>),
    /// The observed row predates a concurrent rejection of this version.
    Retry(i64),
    Invalidated,
}

#[derive(Default)]
struct ConnectionGuard {
    rejected_floor: Option<(i64, i64)>,
    invalidated_through: Option<i64>,
    pins: usize,
}

#[derive(Default)]
struct TokenCache {
    entries: HashMap<ConnectionKey, CachedAccessToken>,
    guards: HashMap<ConnectionKey, ConnectionGuard>,
}

impl TokenCache {
    /// Record a 401 if its guard is resident. If it is not, prevent another
    /// hot lookup from serving this generation while the caller waits for a
    /// producer slot. An entry from a newer generation is left alone.
    fn note_rejected_version(
        &mut self,
        key: &ConnectionKey,
        generation: i64,
        version: i64,
    ) -> bool {
        let Some(guard) = self.guards.get_mut(key) else {
            if self
                .entries
                .get(key)
                .is_some_and(|entry| entry.generation <= generation)
            {
                self.entries.remove(key);
            }
            return false;
        };
        let rejected = (generation, version);
        let floor = guard
            .rejected_floor
            .map_or(rejected, |old| old.max(rejected));
        guard.rejected_floor = Some(floor);
        if self
            .entries
            .get(key)
            .is_some_and(|entry| (entry.generation, entry.version) <= floor)
        {
            self.entries.remove(key);
        }
        true
    }

    /// Merge caller and connection evidence without carrying an old
    /// generation's rejection into a newer generation.
    fn rejection_floor(
        &self,
        key: &ConnectionKey,
        generation: i64,
        rejected_version: Option<i64>,
    ) -> ApiResult<Option<i64>> {
        let Some(guard) = self.guards.get(key) else {
            return Ok(rejected_version);
        };
        if guard
            .invalidated_through
            .is_some_and(|invalidated| generation <= invalidated)
        {
            return Err(reauth());
        }
        match guard.rejected_floor {
            Some((rejected_generation, _)) if rejected_generation > generation => Err(reauth()),
            Some((rejected_generation, version)) if rejected_generation == generation => Ok(Some(
                rejected_version.map_or(version, |old| old.max(version)),
            )),
            _ => Ok(rejected_version),
        }
    }
}

/// One cold producer owns one permit and one guard pin. The Arc is shared
/// with its detached refresh worker; only the last owner releases the pin.
struct CachePin {
    service: Weak<ChatgptConnections>,
    key: ConnectionKey,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for CachePin {
    fn drop(&mut self) {
        if let Some(service) = self.service.upgrade() {
            let mut cache = service.tokens.lock().unwrap_or_else(|e| e.into_inner());
            let guard = cache
                .guards
                .get_mut(&self.key)
                .expect("a pinned token guard cannot be evicted");
            guard.pins = guard
                .pins
                .checked_sub(1)
                .expect("token guard pin underflow");
        }
        // The permit field is dropped after this method, so another producer
        // cannot acquire it before the guard becomes evictable.
    }
}
struct SigningKeys {
    fetched: std::time::Instant,
    keys: JwkSet,
}
pub struct ChatgptConnections {
    pool: Option<SharedPool>,
    encryptor: Arc<FernetTokenEncryptor>,
    config: Result<ChatgptConfig, String>,
    active: Mutex<ActiveRegistry>,
    refreshes: Mutex<HashMap<RefreshKey, RefreshWaiter>>,
    tokens: Mutex<TokenCache>,
    token_producers: Arc<tokio::sync::Semaphore>,
    signing_keys: tokio::sync::Mutex<Option<SigningKeys>>,
    inference_client: OnceLock<reqwest::Client>,
    #[cfg(any(test, feature = "e2e-hooks"))]
    fence_reads: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "e2e-hooks"))]
    connection_read_queries: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "e2e-hooks"))]
    credential_row_reads: std::sync::atomic::AtomicUsize,
}
impl std::fmt::Debug for ChatgptConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatgptConnections(<protected>)")
    }
}

#[derive(Clone)]
pub struct ExecutionCredential {
    service: Arc<ChatgptConnections>,
    owner: String,
    connection_id: String,
    generation: i64,
    label: String,
}
impl PartialEq for ExecutionCredential {
    fn eq(&self, rhs: &Self) -> bool {
        self.owner == rhs.owner
            && self.connection_id == rhs.connection_id
            && self.generation == rhs.generation
    }
}
impl std::fmt::Debug for ExecutionCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionCredential")
            .field("connection_id", &self.connection_id)
            .finish_non_exhaustive()
    }
}
pub struct CredentialLease {
    owner: String,
    token: String,
    version: i64,
    pub cancel: CancellationToken,
    service: Arc<ChatgptConnections>,
    connection_id: String,
    request_id: Uuid,
}
impl CredentialLease {
    pub fn version(&self) -> i64 {
        self.version
    }
    pub fn bearer(&self) -> &str {
        &self.token
    }
}
impl Drop for CredentialLease {
    fn drop(&mut self) {
        let mut active = self
            .service
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(connection) = active
            .connections
            .get_mut(&(self.owner.clone(), self.connection_id.clone()))
        {
            connection
                .requests
                .retain(|(id, _, _)| *id != self.request_id);
            if connection.requests.is_empty() {
                active
                    .connections
                    .remove(&(self.owner.clone(), self.connection_id.clone()));
                if active.connections.is_empty()
                    && let Some(monitor) = &active.monitor
                {
                    monitor.stop.cancel();
                }
            }
        }
    }
}
impl ExecutionCredential {
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn connection_read_queries_for_test(&self) -> usize {
        self.service.connection_read_queries_for_test()
    }
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn fence_reads_for_test(&self) -> usize {
        self.service
            .fence_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Fixed-origin transport. Redirects cannot move private history off origin.
    /// The runtime's canonical request/stream deadlines bound each operation.
    pub fn http_client(&self) -> ApiResult<reqwest::Client> {
        if let Some(client) = self.service.inference_client.get() {
            return Ok(client.clone());
        }
        let client = astra_core::net::client_builder_for_target(self.api_base()?)
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| internal_error("Cannot configure subscription inference transport"))?;
        let _ = self.service.inference_client.set(client.clone());
        Ok(self
            .service
            .inference_client
            .get()
            .cloned()
            .unwrap_or(client))
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    pub fn api_base(&self) -> ApiResult<&str> {
        self.service.api_base()
    }
    pub async fn acquire(&self, rejected_version: Option<i64>) -> ApiResult<CredentialLease> {
        let (token, version) = self
            .service
            .access_token(
                &self.owner,
                &self.connection_id,
                self.generation,
                rejected_version,
            )
            .await?;
        let request_id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.service.register_request(
            &self.owner,
            &self.connection_id,
            request_id,
            self.generation,
            cancel.clone(),
        );
        let lease = CredentialLease {
            owner: self.owner.clone(),
            token,
            version,
            cancel,
            service: self.service.clone(),
            connection_id: self.connection_id.clone(),
            request_id,
        };
        self.ensure_current().await?;
        Ok(lease)
    }
    pub async fn ensure_current(&self) -> ApiResult<()> {
        self.service.enabled()?;
        let pool = self.service.pool()?;
        #[cfg(any(test, feature = "e2e-hooks"))]
        self.service
            .fence_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let row = sqlx::query_as::<_, ConnectionFence>("SELECT status, generation FROM user_llm_connections WHERE user_id = ? AND connection_id = ?")
            .bind(&self.owner).bind(&self.connection_id).fetch_optional(connection.connection_mut()).await.map_err(internal_error)?;
        connection.release();
        let row = row.ok_or_else(not_found)?;
        connected_generation(&row, self.generation)
    }

    /// Serialize model activation with disconnect/reauthorization. Call only
    /// after provider I/O, before touching model rows in the same transaction.
    pub(crate) async fn lock_current(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    ) -> ApiResult<()> {
        self.service.enabled()?;
        let row = sqlx::query_as::<_, ConnectionFence>("SELECT status,generation FROM user_llm_connections WHERE user_id=? AND connection_id=? FOR UPDATE")
            .bind(&self.owner).bind(&self.connection_id).fetch_optional(&mut **tx).await.map_err(internal_error)?.ok_or_else(not_found)?;
        connected_generation(&row, self.generation)
    }
}

pub use astra_credentials::chatgpt::protocol::validate_redirect;

impl ChatgptConnections {
    pub fn new(
        pool: Option<SharedPool>,
        encryptor: Arc<FernetTokenEncryptor>,
        config: Result<ChatgptConfig, String>,
    ) -> Self {
        let config = config.map(|mut config| {
            if config.host_id.is_empty() {
                // The existing deployment key is stable across restarts and replicas.
                // Derive a public, purpose-separated identity without new storage or setup.
                let digest = Sha256::digest(
                    encryptor
                        .keyed_digest("astra.chatgpt.host-id.v1", "installation")
                        .as_bytes(),
                );
                let mut bytes = [0; 16];
                bytes.copy_from_slice(&digest[..16]);
                config.host_id = uuid::Builder::from_random_bytes(bytes)
                    .into_uuid()
                    .urn()
                    .to_string();
            }
            config
        });
        Self {
            pool,
            encryptor,
            config,
            active: Mutex::new(ActiveRegistry::default()),
            refreshes: Mutex::new(HashMap::new()),
            tokens: Mutex::new(TokenCache::default()),
            token_producers: Arc::new(tokio::sync::Semaphore::new(MAX_CACHED_TOKENS)),
            signing_keys: tokio::sync::Mutex::new(None),
            inference_client: OnceLock::new(),
            #[cfg(any(test, feature = "e2e-hooks"))]
            fence_reads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "e2e-hooks"))]
            connection_read_queries: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "e2e-hooks"))]
            credential_row_reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn config(&self) -> ApiResult<&ChatgptConfig> {
        self.config.as_ref().map_err(|_| {
            fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "chatgpt_subscription_unconfigured",
                "ChatGPT subscription configuration is invalid; contact the administrator",
            )
        })
    }
    fn enabled(&self) -> ApiResult<&ChatgptConfig> {
        let config = self.config()?;
        if !config.enabled {
            return Err(fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "chatgpt_subscription_disabled",
                "ChatGPT subscriptions are disabled on this Server",
            ));
        }
        Ok(config)
    }
    pub fn availability(&self) -> Value {
        match self.enabled() {
            Ok(_) => json!({"available": true}),
            Err((_, Json(error))) => json!({
                "available": false,
                "error_code": error.error_code,
                "detail": error.detail,
            }),
        }
    }
    fn pool(&self) -> ApiResult<MySqlPool> {
        self.pool
            .as_ref()
            .map(|p| p.get().clone())
            .ok_or_else(|| internal_error("ChatGPT connections require the shared database pool"))
    }
    fn client(&self) -> ApiResult<reqwest::Client> {
        let config = self.config()?;
        astra_core::net::client_builder_for_target(&config.token)
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| internal_error("Cannot configure ChatGPT HTTP transport"))
    }
    pub async fn operate(
        self: &Arc<Self>,
        owner: &str,
        operation: ConnectionOperation,
    ) -> ApiResult<Value> {
        match operation {
            ConnectionOperation::List => Ok(json!({"items":self.list(owner).await?})),
            ConnectionOperation::Start(request) => self.start(owner, request).await,
            ConnectionOperation::Complete(id, request) => {
                // Once a code is submitted, its settlement outlives a dropped HTTP waiter.
                let service = self.clone();
                let owner = owner.to_owned();
                tokio::spawn(async move {
                    let claimed = std::sync::atomic::AtomicBool::new(false);
                    match tokio::time::timeout(Duration::from_secs(90), service.complete(&owner, &id, request, &claimed)).await {
                        Ok(result) => result,
                        Err(_) => {
                            // Before an acknowledged claim, another callback may own
                            // this attempt. Leave uncertain claims to explicit cancel.
                            if !claimed.load(std::sync::atomic::Ordering::Acquire) { return Err(unavailable()); }
                            // An uncertain commit may already be completed; only settle
                            // an exchanging attempt. Never exchange its code a second time.
                            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                                let pool = service.pool()?;
                                let mut connection = CancellationSafePoolConnection::acquire(&pool).await.map_err(internal_error)?;
                                query("UPDATE user_llm_authorizations SET status='failed',verifier_encrypted=NULL WHERE user_id=? AND authorization_id=? AND status='exchanging'")
                                    .bind(&owner).bind(&id).execute(connection.connection_mut()).await.map_err(internal_error)?;
                                connection.release();
                                Ok::<_, (StatusCode, Json<ErrorResponse>)>(())
                            }).await;
                            Err(unavailable())
                        }
                    }
                }).await.map_err(internal_error)?
            }
            ConnectionOperation::Cancel(id) => {
                let status:String=sqlx::query_scalar("SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?")
                    .bind(owner).bind(&id).fetch_optional(&self.pool()?).await.map_err(internal_error)?.ok_or_else(not_found)?;
                let changed=query("UPDATE user_llm_authorizations SET status = 'cancelled', verifier_encrypted = NULL WHERE user_id = ? AND authorization_id = ? AND status IN ('pending','exchanging')")
                    .bind(owner).bind(&id).execute(&self.pool()?).await.map_err(internal_error)?;
                Ok(json!({"cancelled":changed.rows_affected()==1 || status=="cancelled"}))
            }
            ConnectionOperation::Models(id) => self.models(owner, &id).await,
            ConnectionOperation::Disconnect(id) => self.disconnect(owner, &id).await,
        }
    }
    pub async fn list(&self, owner: &str) -> ApiResult<Vec<ConnectionView>> {
        let pool = self.pool()?;
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let rows = sqlx::query_as("SELECT connection_id,provider,label,email,status,token_expires_at AS expires_at,revocation_confirmed <> 0 AS remote_revocation_confirmed FROM user_llm_connections WHERE user_id=? ORDER BY created_at,connection_id")
            .bind(owner).fetch_all(connection.connection_mut()).await.map_err(internal_error)?;
        connection.release();
        Ok(rows)
    }
    pub async fn catalog_labels(&self, owner: &str) -> ApiResult<HashMap<String, String>> {
        let Ok(config) = self.enabled() else {
            return Ok(HashMap::new());
        };
        let rows = sqlx::query_as::<_, (String, String)>("SELECT connection_id,label FROM user_llm_connections WHERE user_id=? AND host_id=? AND status='connected'")
            .bind(owner).bind(&config.host_id).fetch_all(&self.pool()?).await.map_err(internal_error)?;
        Ok(rows.into_iter().collect())
    }
    async fn start(&self, owner: &str, mut request: StartAuthorization) -> ApiResult<Value> {
        let config = self.enabled()?;
        validate_redirect(&request.redirect_uri)
            .map_err(|e| fail(StatusCode::BAD_REQUEST, "oauth_redirect_invalid", e))?;
        let pool = self.pool()?;
        let mut label = request
            .label
            .unwrap_or_else(|| "ChatGPT subscription".into());
        if label.trim().is_empty() || label.len() > 100 || label.chars().any(char::is_control) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "model_connection_label_invalid",
                "Use a non-empty label of at most 100 bytes",
            ));
        }
        if request.retry_authorization_id.is_some() && request.connection_id.is_some() {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "oauth_attempt_invalid",
                "Choose either a connection or a failed authorization",
            ));
        }
        let mut need_consent = false;
        let (client_id, subject) = if let Some(previous) = &request.retry_authorization_id {
            let (client,subject,connection,previous_label,status):(String,Option<String>,Option<String>,String,String)=sqlx::query_as("SELECT client_id,expected_subject,connection_id,label,status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?")
                .bind(owner).bind(previous).fetch_optional(&pool).await.map_err(internal_error)?.ok_or_else(not_found)?;
            if client == "dynamic_agent_client" || status != "failed" {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "oauth_attempt_not_retryable",
                    "Inspect your connections or start a new registration",
                ));
            }
            request.connection_id = connection;
            label = previous_label;
            (client, subject)
        } else if let Some(id) = &request.connection_id {
            let (client,subject,previous_label,status):(String,String,String,String)=sqlx::query_as("SELECT client_id,subject,label,status FROM user_llm_connections WHERE user_id=? AND connection_id=?")
                .bind(owner).bind(id).fetch_optional(&pool).await.map_err(internal_error)?.ok_or_else(not_found)?;
            label = previous_label;
            need_consent = status == "plan_disabled";
            (client, Some(subject))
        } else {
            ("dynamic_agent_client".into(), None)
        };
        let state = Uuid::new_v4().simple().to_string();
        let nonce = Uuid::new_v4().simple().to_string();
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let id = Uuid::new_v4().to_string();
        let expires = unix_now() + AUTH_LIFETIME;
        let url = AuthorizationRequest {
            client_id: client_id.clone(),
            host_id: config.host_id.clone(),
            redirect_uri: request.redirect_uri.clone(),
            state: state.clone(),
            nonce: nonce.clone(),
            verifier: verifier.clone(),
            consent: need_consent,
        }
        .authorization_url(config.provider_url(&config.authorize)?);
        query("DELETE FROM user_llm_authorizations WHERE user_id = ? AND expires_at < ?")
            .bind(owner)
            .bind(unix_now() - 86400)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        let mut tx = CancellationSafeTransaction::begin(&pool)
            .await
            .map_err(internal_error)?;
        let account: Option<String> =
            sqlx::query_scalar("SELECT user_id FROM auth_users WHERE user_id=? FOR UPDATE")
                .bind(owner)
                .fetch_optional(tx.connection_mut())
                .await
                .map_err(internal_error)?;
        if account.is_none() {
            tx.rollback().await.map_err(internal_error)?;
            return Err(fail(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Your account is no longer available. Sign in again.",
            ));
        }
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_llm_authorizations WHERE user_id = ? AND expires_at > ? AND status = 'pending'")
            .bind(owner).bind(unix_now()).fetch_one(tx.connection_mut()).await.map_err(internal_error)?;
        if pending >= 8 {
            tx.rollback().await.map_err(internal_error)?;
            return Err(fail(
                StatusCode::TOO_MANY_REQUESTS,
                "oauth_attempt_limit",
                "Finish or cancel a pending authorization",
            ));
        }
        // Stable parameter types preserve nullable fences across cached executions.
        query("INSERT INTO user_llm_authorizations (authorization_id,user_id,connection_id,label,redirect_uri,state_digest,nonce_digest,verifier_encrypted,client_id,expected_subject,status,expires_at) VALUES (?,?,CASE WHEN CAST(? AS SIGNED) <> 0 THEN CAST(? AS CHAR(255)) ELSE CAST(NULL AS CHAR(255)) END,?,?,?,?,?,?,CASE WHEN CAST(? AS SIGNED) <> 0 THEN CAST(? AS CHAR(255)) ELSE CAST(NULL AS CHAR(255)) END,'pending',?)")
            .bind(&id).bind(owner).bind(if request.connection_id.is_some() {"1"} else {"0"}).bind(request.connection_id.as_deref().unwrap_or("")).bind(label).bind(request.redirect_uri)
            .bind(self.encryptor.keyed_digest("chatgpt-state",&state)).bind(self.encryptor.keyed_digest("chatgpt-nonce",&nonce))
            .bind(self.encryptor.encrypt(&verifier).map_err(internal_error)?).bind(client_id).bind(if subject.is_some() {"1"} else {"0"}).bind(subject.as_deref().unwrap_or("")).bind(expires)
            .execute(tx.connection_mut()).await.map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        Ok(
            json!({"authorization_id":id,"authorization_url":url.as_str(),"state":state,"expires_at":expires}),
        )
    }
    async fn read_json(&self, request: reqwest::RequestBuilder) -> ApiResult<Value> {
        let response = request.send().await.map_err(|_| unavailable())?;
        let bytes = provider_body(response).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| internal_error("OpenAI returned an invalid JSON response"))
    }
    async fn signing_keys(&self, kid: Option<&str>) -> ApiResult<JwkSet> {
        let mut cached = self.signing_keys.lock().await;
        if let Some(value) = cached.as_ref()
            && value.fetched.elapsed() < Duration::from_secs(300)
            && kid.is_none_or(|kid| value.keys.find(kid).is_some())
        {
            return Ok(value.keys.clone());
        }
        let config = self.config()?;
        let client = self.client()?;
        let discovery = self
            .read_json(client.get(config.provider_url(&config.discovery)?))
            .await?;
        if discovery["issuer"].as_str() != Some(config.issuer.as_str()) {
            return Err(reauth());
        }
        let url = discovery["jwks_uri"].as_str().ok_or_else(reauth)?;
        let keys: JwkSet = serde_json::from_value(
            self.read_json(client.get(config.provider_url(url)?))
                .await?,
        )
        .map_err(|_| reauth())?;
        *cached = Some(SigningKeys {
            fetched: std::time::Instant::now(),
            keys: keys.clone(),
        });
        Ok(keys)
    }

    fn refresh(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        generation: i64,
        rejected_version: Option<i64>,
        pin: &Arc<CachePin>,
    ) -> RefreshWaiter {
        let mut refreshes = self.refreshes.lock().unwrap_or_else(|e| e.into_inner());
        let key = (owner.to_owned(), id.to_owned(), generation);
        if let Some(waiter) = refreshes.get(&key)
            && waiter.has_changed().is_ok()
        {
            return waiter.clone();
        }
        let (result, waiter) = tokio::sync::watch::channel(None);
        refreshes.insert(key.clone(), waiter.clone());
        let service = self.clone();
        let pin = Arc::clone(pin);
        // Rotation belongs to the connection, not an inference's cancellation
        // or deadline. Always finish its bounded exchange and persistence even
        // if every waiter goes away; disconnect still fences its database CAS.
        tokio::spawn(async move {
            let outcome = service
                .refresh_access_token(&key.0, &key.1, generation, rejected_version, &pin)
                .await;
            service
                .refreshes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            result.send_replace(Some(outcome));
            drop(pin);
        });
        waiter
    }

    fn register_request(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        request: Uuid,
        generation: i64,
        cancel: CancellationToken,
    ) {
        let launch;
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            active
                .connections
                .entry((owner.into(), id.into()))
                .or_insert_with(|| ActiveConnection {
                    monitor: Uuid::new_v4(),
                    requests: Vec::new(),
                })
                .requests
                .push((request, generation, cancel));
            launch = self.reserve_monitor(&mut active, None);
        }
        if let Some(launch) = launch {
            Self::spawn_monitor(launch);
        }
    }

    // Reserve under the registry lock; the returned guard must outlive that
    // critical section so rejected spawn/unwind cleanup can safely take it.
    fn reserve_monitor(
        self: &Arc<Self>,
        active: &mut ActiveRegistry,
        predecessor: Option<Uuid>,
    ) -> Option<(MonitorGuard, CancellationToken)> {
        if active.monitor.as_ref().map(|monitor| monitor.id) != predecessor {
            return None;
        }
        if active.connections.is_empty() {
            active.monitor = None;
            return None;
        }
        let id = Uuid::new_v4();
        let stop = CancellationToken::new();
        let mut guard = MonitorGuard {
            service: Arc::downgrade(self),
            id,
            armed: false,
        };
        active.monitor = Some(ConnectionMonitor {
            id,
            stop: stop.clone(),
        });
        guard.armed = true;
        Some((guard, stop))
    }

    fn finish_monitor(self: &Arc<Self>, guard: &mut MonitorGuard) {
        let launch;
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            launch = self.reserve_monitor(&mut active, Some(guard.id));
            guard.armed = false;
        }
        if let Some(launch) = launch {
            Self::spawn_monitor(launch);
        }
    }

    fn spawn_monitor((mut guard, stop): (MonitorGuard, CancellationToken)) {
        let service = guard.service.clone();
        tokio::spawn(async move {
            let period = Duration::from_secs(2);
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let tick = tokio::select! { biased; _ = stop.cancelled() => { if let Some(service) = service.upgrade() { service.finish_monitor(&mut guard); } return; }, tick = ticks.tick() => tick };
                let Some(service) = service.upgrade() else {
                    return;
                };
                let snapshot: Vec<_> = service
                    .active
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .connections
                    .iter()
                    .map(|(key, entry)| (key.clone(), entry.monitor))
                    .collect();
                let deadline = tick + period;
                let chunks: Vec<_> = snapshot
                    .chunks(CONNECTION_READ_BATCH)
                    .map(|chunk| chunk.to_vec())
                    .collect();
                let mut batches = stream::iter(chunks.into_iter().map(|targets| {
                    let service = service.clone();
                    let stop = stop.clone();
                    async move {
                        let keys: Vec<_> = targets.iter().map(|(key, _)| key.clone()).collect();
                        if tokio::time::Instant::now() >= deadline {
                            return (targets, None);
                        }
                        let rows = service
                            .lookup_connection_rows(&keys, Some((deadline, stop.clone())))
                            .await
                            .ok();
                        (targets, rows)
                    }
                }))
                .buffer_unordered(CONNECTION_READ_CONCURRENCY);
                while let Some((targets, rows)) = batches.next().await {
                    let mut active = service.active.lock().unwrap_or_else(|e| e.into_inner());
                    active.apply_observations(&targets, rows.as_deref());
                    if active.connections.is_empty() {
                        stop.cancel();
                    }
                }
            }
        });
    }

    /// One bounded, owner-scoped lookup; no credentials leave this query.
    async fn lookup_connection_rows(
        &self,
        keys: &[ConnectionKey],
        control: Option<(tokio::time::Instant, CancellationToken)>,
    ) -> ApiResult<Vec<Option<ConnectionRow>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if keys.len() > CONNECTION_READ_BATCH {
            return Err(unavailable());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT requested.ordinal, c.status, c.generation, c.host_id, c.label FROM (VALUES ",
        );
        for (ordinal, (owner, id)) in keys.iter().enumerate() {
            if ordinal != 0 {
                query.push(",");
            }
            query
                .push("ROW(")
                .push(ordinal)
                .push(",")
                .push_bind(owner)
                .push(",")
                .push_bind(id)
                .push(")");
        }
        query.push(") requested(ordinal, requested_owner, requested_id) LEFT JOIN user_llm_connections c ON c.user_id=requested_owner AND c.connection_id=requested_id AND (c.user_id,c.connection_id) IN (");
        for (ordinal, (owner, id)) in keys.iter().enumerate() {
            if ordinal != 0 {
                query.push(",");
            }
            query
                .push("(")
                .push_bind(owner)
                .push(",")
                .push_bind(id)
                .push(")");
        }
        query.push(")");
        let pool = self.pool()?;
        let mut connection = if let Some((deadline, stop)) = &control {
            tokio::select! { biased;
                _ = stop.cancelled() => return Err(unavailable()),
                _ = tokio::time::sleep_until(*deadline) => return Err(unavailable()),
                result = CancellationSafePoolConnection::acquire(&pool) => result.map_err(internal_error)?,
            }
        } else {
            CancellationSafePoolConnection::acquire(&pool)
                .await
                .map_err(internal_error)?
        };
        #[cfg(any(test, feature = "e2e-hooks"))]
        self.connection_read_queries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let rows = if let Some((deadline, stop)) = &control {
            tokio::select! { biased;
                _ = stop.cancelled() => Err(unavailable()),
                _ = tokio::time::sleep_until(*deadline) => Err(unavailable()),
                result = query.build().fetch_all(connection.connection_mut()) => result.map_err(internal_error),
            }
        } else {
            query
                .build()
                .fetch_all(connection.connection_mut())
                .await
                .map_err(internal_error)
        };
        let rows = match rows {
            Ok(rows) => {
                connection.release();
                rows
            }
            Err(error) => {
                connection.discard();
                return Err(error);
            }
        };
        let mut result: Vec<Option<ConnectionRow>> = (0..keys.len()).map(|_| None).collect();
        let mut seen = vec![false; keys.len()];
        for row in rows {
            let ordinal =
                usize::try_from(row.try_get::<i64, _>("ordinal").map_err(internal_error)?)
                    .map_err(internal_error)?;
            let Some(slot) = seen.get_mut(ordinal) else {
                return Err(unavailable());
            };
            if *slot {
                return Err(unavailable());
            }
            *slot = true;
            if let Some(status) = row
                .try_get::<Option<String>, _>("status")
                .map_err(internal_error)?
            {
                result[ordinal] = Some(ConnectionRow {
                    status,
                    generation: row.try_get("generation").map_err(internal_error)?,
                    host_id: row.try_get("host_id").map_err(internal_error)?,
                    label: row.try_get("label").map_err(internal_error)?,
                });
            }
        }
        if seen.iter().any(|seen| !seen) {
            return Err(unavailable());
        }
        if control.as_ref().is_some_and(|(deadline, stop)| {
            stop.is_cancelled() || tokio::time::Instant::now() >= *deadline
        }) {
            return Err(unavailable());
        }
        Ok(result)
    }

    async fn identity(
        &self,
        id_token: &str,
        client_id: &str,
        nonce: Option<&str>,
    ) -> ApiResult<Identity> {
        let config = self.config()?;
        let kid = protocol::identity_key_id(id_token).map_err(|_| reauth())?;
        let jwks = self.signing_keys(Some(&kid)).await?;
        let identity = protocol::verify_identity(id_token, client_id, &config.issuer, &jwks)
            .map_err(|_| reauth())?;
        if let Some(expected) = nonce
            && !identity.nonce.as_deref().is_some_and(|n| {
                self.encryptor
                    .verify_keyed_digest("chatgpt-nonce", n, expected)
            })
        {
            return Err(reauth());
        }
        Ok(identity)
    }
    async fn complete(
        &self,
        owner: &str,
        id: &str,
        request: CompleteAuthorization,
        claimed: &std::sync::atomic::AtomicBool,
    ) -> ApiResult<Value> {
        let config = self.enabled()?;
        let pool = self.pool()?;
        let mut checkout = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let row = sqlx::query_as::<_, AuthorizationRow>(
            "SELECT * FROM user_llm_authorizations WHERE user_id = ? AND authorization_id = ?",
        )
        .bind(owner)
        .bind(id)
        .fetch_optional(checkout.connection_mut())
        .await
        .map_err(internal_error)?
        .ok_or_else(not_found)?;
        checkout.release();
        if !self
            .encryptor
            .verify_keyed_digest("chatgpt-state", &request.state, &row.state_digest)
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "oauth_state_invalid",
                "OAuth state did not match this attempt",
            ));
        }
        let callback_digest = self.encryptor.keyed_digest(
            "chatgpt-callback",
            &serde_json::to_string(&json!([request.code, request.client_id, request.error]))
                .map_err(internal_error)?,
        );
        if row.status == "completed" {
            if row.callback_digest.as_deref() != Some(callback_digest.as_str()) {
                return Err(reauth());
            }
            let connection = row
                .completed_connection_id
                .as_deref()
                .ok_or_else(|| internal_error("Completed authorization has no connection"))?;
            return Ok(json!({"connection":self.view(owner,connection).await?}));
        }
        if row.status != "pending" || row.expires_at < unix_now() {
            return Err(fail(
                StatusCode::CONFLICT,
                "oauth_attempt_expired",
                "Start a fresh ChatGPT authorization",
            ));
        }
        if request.error.is_some() {
            let mut connection = CancellationSafePoolConnection::acquire(&pool)
                .await
                .map_err(internal_error)?;
            query("UPDATE user_llm_authorizations SET status='declined', verifier_encrypted=NULL WHERE user_id=? AND authorization_id=? AND status='pending'")
                .bind(owner).bind(id).execute(connection.connection_mut()).await.map_err(internal_error)?;
            connection.release();
            return Err(fail(
                StatusCode::FORBIDDEN,
                "chatgpt_consent_declined",
                "ChatGPT authorization was declined",
            ));
        }
        let client_id = request.client_id.as_deref().unwrap_or(&row.client_id);
        if client_id == "dynamic_agent_client"
            || client_id.is_empty()
            || client_id.len() > 255
            || !client_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            || (row.client_id != "dynamic_agent_client" && row.client_id != client_id)
        {
            return Err(reauth());
        }
        let code = request
            .code
            .as_deref()
            .filter(|c| !c.is_empty() && c.len() < 16384)
            .ok_or_else(reauth)?;
        let verifier = self
            .encryptor
            .decrypt(row.verifier_encrypted.as_deref().ok_or_else(reauth)?)
            .map_err(internal_error)?;
        let mut checkout = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let changed = query("UPDATE user_llm_authorizations SET status='exchanging', client_id=? WHERE user_id=? AND authorization_id=? AND status='pending' AND expires_at>=?")
            .bind(client_id).bind(owner).bind(id).bind(unix_now()).execute(checkout.connection_mut()).await.map_err(internal_error)?;
        checkout.release();
        if changed.rows_affected() != 1 {
            return Err(fail(
                StatusCode::CONFLICT,
                "oauth_attempt_consumed",
                "Authorization is already being exchanged",
            ));
        }
        claimed.store(true, std::sync::atomic::Ordering::Release);
        let result = async {
            let value = self.read_json(self.client()?.post(&config.token).form(&[
                ("grant_type","authorization_code"),("client_id",client_id),("code",code),
                ("code_verifier",verifier.as_str()),("redirect_uri",row.redirect_uri.as_str()),("resource",API_BASE),
            ])).await?;
            let token: TokenResponse = serde_json::from_value(value).map_err(|_| reauth())?;
            let id_token = token.id_token.as_deref().ok_or_else(reauth)?;
            let identity = self.identity(id_token,client_id,Some(&row.nonce_digest)).await?;
            if row.expected_subject.as_deref().is_some_and(|expected| expected!=identity.sub) { return Err(reauth()); }
            if token.expires_in<=0 || token.expires_in>86400*30 || token.access_token.is_empty() { return Err(reauth()); }
            let scopes: Vec<String> = token.scope.as_deref().unwrap_or("").split_whitespace().map(str::to_owned).collect();
            let state = if scopes.iter().any(|s| s==PLAN_SCOPE) { "connected" } else { "plan_disabled" };
            let credentials = Credentials { access_token:token.access_token,refresh_token:token.refresh_token,
                scopes,expires_at:unix_now()+token.expires_in };
            let encrypted = self.encryptor.encrypt(&serde_json::to_string(&credentials).map_err(internal_error)?).map_err(internal_error)?;
            let mut existing = row.connection_id.clone();
            if existing.is_none() {
                existing = self.registration_id(owner, client_id, &identity.sub).await?;
            }
            // Resolve registration conflicts before taking the authorization lock.
            // Only a definite duplicate is retried; the consumed OAuth code is not.
            for attempt in 0..2 {
                let connection_id = existing.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
                let mut tx = CancellationSafeTransaction::begin(&pool).await.map_err(internal_error)?;
                let replaced_generation = if existing.is_some() {
                    let registration: Option<(String,String,i64)> = sqlx::query_as("SELECT client_id,subject,generation FROM user_llm_connections WHERE user_id=? AND connection_id=? FOR UPDATE")
                        .bind(owner).bind(&connection_id).fetch_optional(&mut *tx).await.map_err(internal_error)?;
                    let Some((registered_client, registered_subject, old_generation)) = registration else { return Err(reauth()); };
                    if registered_client != client_id || registered_subject != identity.sub { return Err(reauth()); }
                    Some(old_generation)
                } else {
                    let inserted = query("INSERT INTO user_llm_connections (connection_id,user_id,provider,label,client_id,subject,email,host_id,status,credentials_encrypted,token_expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)")
                        .bind(&connection_id).bind(owner).bind(PROVIDER).bind(&row.label).bind(client_id).bind(&identity.sub).bind(&identity.email)
                        .bind(&config.host_id).bind(state).bind(&encrypted).bind(credentials.expires_at).execute(&mut *tx).await;
                    if let Err(error) = inserted {
                        let duplicate = error.as_database_error().is_some_and(|e| e.is_unique_violation());
                        tx.rollback().await.map_err(internal_error)?;
                        if !duplicate || attempt != 0 { return Err(internal_error(error)); }
                        existing = self.registration_id(owner, client_id, &identity.sub).await?;
                        if existing.is_none() { return Err(internal_error(error)); }
                        continue;
                    }
                    None
                };
                let pending: String = sqlx::query_scalar("SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=? FOR UPDATE")
                    .bind(owner).bind(id).fetch_one(&mut *tx).await.map_err(internal_error)?;
                if pending!="exchanging" { return Err(fail(StatusCode::CONFLICT,"oauth_attempt_cancelled","Authorization was cancelled before linking completed")); }
                if existing.is_some() {
                    query("UPDATE user_llm_connections SET credentials_encrypted=?,token_expires_at=?,status=?,revocation_confirmed=NULL,email=?,host_id=?,generation=generation+1,credential_version=credential_version+1,refresh_owner=NULL,refresh_until=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
                        .bind(&encrypted).bind(credentials.expires_at).bind(state).bind(&identity.email).bind(&config.host_id).bind(owner).bind(&connection_id)
                        .execute(&mut *tx).await.map_err(internal_error)?;
                }
                if state!="connected" {
                    query("UPDATE user_llm_models SET is_active=0,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
                        .bind(owner).bind(&connection_id).execute(&mut *tx).await.map_err(internal_error)?;
                }
                query("UPDATE user_llm_authorizations SET status='completed',verifier_encrypted=NULL,callback_digest=?,completed_connection_id=? WHERE user_id=? AND authorization_id=? AND status='exchanging'")
                    .bind(&callback_digest).bind(&connection_id).bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
                tx.commit().await.map_err(internal_error)?;
                if let Some(old_generation) = replaced_generation {
                    self.invalidate_local_connection(owner, &connection_id, old_generation);
                }
                return Ok(json!({"connection":self.view(owner,&connection_id).await?}));
            }
            Err(unavailable())
        }.await;
        if result.is_err() {
            let mut connection = CancellationSafePoolConnection::acquire(&pool)
                .await
                .map_err(internal_error)?;
            query("UPDATE user_llm_authorizations SET status='failed',verifier_encrypted=NULL WHERE user_id=? AND authorization_id=? AND status='exchanging'")
                .bind(owner).bind(id).execute(connection.connection_mut()).await.map_err(internal_error)?;
            connection.release();
        }
        result
    }
    async fn registration_id(
        &self,
        owner: &str,
        client: &str,
        subject: &str,
    ) -> ApiResult<Option<String>> {
        let pool = self.pool()?;
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let result = sqlx::query_scalar("SELECT connection_id FROM user_llm_connections WHERE user_id=? AND client_id=? AND subject=?")
            .bind(owner).bind(client).bind(subject).fetch_optional(connection.connection_mut()).await.map_err(internal_error)?;
        connection.release();
        Ok(result)
    }
    async fn view(&self, owner: &str, id: &str) -> ApiResult<ConnectionView> {
        self.list(owner)
            .await?
            .into_iter()
            .find(|item| item.connection_id == id)
            .ok_or_else(not_found)
    }
    pub async fn binding(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
    ) -> ApiResult<ExecutionCredential> {
        self.bindings(owner, &[id.to_owned()])
            .await?
            .remove(id)
            .ok_or_else(not_found)?
    }

    pub(crate) async fn bindings(
        self: &Arc<Self>,
        owner: &str,
        ids: &[String],
    ) -> ApiResult<HashMap<String, ApiResult<ExecutionCredential>>> {
        let config = self.enabled()?;
        let unique: std::collections::BTreeSet<_> = ids.iter().cloned().collect();
        let keys: Vec<_> = unique
            .into_iter()
            .map(|id| (owner.to_owned(), id))
            .collect();
        let rows = self.lookup_connection_rows(&keys, None).await?;
        Ok(keys
            .into_iter()
            .zip(rows)
            .map(|((_, id), row)| {
                let value = row.ok_or_else(not_found).and_then(|row| {
                    if row.status == "plan_disabled" {
                        return Err(fail(
                            StatusCode::FORBIDDEN,
                            "chatgpt_plan_usage_disabled",
                            "Authorize ChatGPT plan usage for this connection",
                        ));
                    }
                    if row.status != "connected" || row.host_id != config.host_id {
                        return Err(reauth());
                    }
                    Ok(ExecutionCredential {
                        service: self.clone(),
                        owner: owner.into(),
                        connection_id: id.clone(),
                        generation: row.generation,
                        label: row.label,
                    })
                });
                (id, value)
            })
            .collect())
    }
    pub fn api_base(&self) -> ApiResult<&str> {
        Ok(&self.enabled()?.api_base)
    }
    fn decode_credentials(&self, encrypted: &str) -> ApiResult<Credentials> {
        serde_json::from_str(&self.encryptor.decrypt(encrypted).map_err(internal_error)?)
            .map_err(|_| internal_error("Stored subscription credentials are invalid"))
    }

    /// A warm lookup and a 401 record share the cache lock. A missing guard
    /// makes the caller take the cold path before it can serve the old entry.
    fn cached_access_token(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
        rejected_version: Option<i64>,
    ) -> ApiResult<Option<(String, i64)>> {
        let mut cache = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let key = (owner.to_owned(), id.to_owned());
        if let Some(version) = rejected_version
            && !cache.note_rejected_version(&key, generation, version)
        {
            return Ok(None);
        }
        let floor = cache.rejection_floor(&key, generation, rejected_version)?;
        let Some(cached) = cache.entries.get(&key) else {
            return Ok(None);
        };
        if cached.generation > generation {
            return Err(reauth());
        }
        if cached.generation == generation
            && cached.expires_at > unix_now() + 60
            && floor.is_none_or(|version| cached.version > version)
        {
            return Ok(Some((cached.token.clone(), cached.version)));
        }
        cache.entries.remove(&key);
        Ok(None)
    }

    /// Admit one cold producer. A held permit guarantees that at most 63
    /// other distinct guards can be pinned, leaving a victim if the guard
    /// table is full. The permit is retained by the pin, including while a
    /// detached refresh worker is still running.
    async fn pin_token_guard(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        generation: i64,
        rejected_version: Option<i64>,
    ) -> ApiResult<Arc<CachePin>> {
        let permit = self
            .token_producers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| unavailable())?;
        let key = (owner.to_owned(), id.to_owned());
        let mut cache = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.guards.contains_key(&key) {
            if cache.guards.len() >= MAX_CACHED_TOKENS {
                let victim = cache
                    .guards
                    .iter()
                    .find(|(_, guard)| guard.pins == 0)
                    .map(|(key, _)| key.clone())
                    .ok_or_else(unavailable)?;
                let guard = cache.guards.remove(&victim).ok_or_else(unavailable)?;
                let invalid_entry = guard.invalidated_through.is_some_and(|generation| {
                    cache
                        .entries
                        .get(&victim)
                        .is_some_and(|entry| entry.generation <= generation)
                });
                if guard.rejected_floor.is_some() || invalid_entry {
                    cache.entries.remove(&victim);
                }
            }
            cache.guards.insert(key.clone(), ConnectionGuard::default());
        }
        if let Some(version) = rejected_version {
            cache.note_rejected_version(&key, generation, version);
        }
        cache.rejection_floor(&key, generation, rejected_version)?;
        cache.guards.get_mut(&key).expect("guard was inserted").pins += 1;
        drop(cache);
        Ok(Arc::new(CachePin {
            service: Arc::downgrade(self),
            key,
            _permit: permit,
        }))
    }

    /// Adjudicate a row or flight result under the same lock that records
    /// 401s and lifecycle invalidation. A pin prevents guard eviction until
    /// after this decision, including when a refresh has lost all waiters.
    fn adjudicate_candidate(
        &self,
        pin: &CachePin,
        generation: i64,
        rejected_version: Option<i64>,
        observed_version: i64,
        candidate: Option<(String, i64, i64)>,
    ) -> CacheVerdict {
        let mut cache = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        if cache
            .guards
            .get(&pin.key)
            .is_none_or(|guard| guard.pins == 0)
        {
            return CacheVerdict::Invalidated;
        }
        let floor = match cache.rejection_floor(&pin.key, generation, rejected_version) {
            Ok(floor) => floor,
            Err(_) => return CacheVerdict::Invalidated,
        };
        let now = unix_now();
        let cutoff = now + 60;
        if let Some(cached) = cache.entries.get(&pin.key) {
            if cached.generation > generation {
                return CacheVerdict::Invalidated;
            }
            if cached.generation < generation
                || cached.expires_at <= cutoff
                || floor.is_some_and(|version| cached.version <= version)
            {
                cache.entries.remove(&pin.key);
            } else if cached.version > observed_version {
                return CacheVerdict::Serve(
                    cached.token.clone(),
                    cached.version,
                    cached.expires_at,
                );
            }
        }
        if let Some(rejected) = floor.filter(|version| observed_version < *version) {
            // This row or flight settled before a newer version was rejected.
            // Re-read the committed row rather than rotating an older token.
            return CacheVerdict::Retry(rejected);
        }
        let Some((token, version, expires_at)) = candidate else {
            return CacheVerdict::Refresh(floor);
        };
        if version != observed_version {
            return CacheVerdict::Invalidated;
        }
        if floor.is_some_and(|rejected| version <= rejected) || expires_at <= now {
            return CacheVerdict::Refresh(floor);
        }
        if expires_at > cutoff {
            if !cache.entries.contains_key(&pin.key) {
                cache.entries.retain(|_, entry| entry.expires_at > cutoff);
                while cache.entries.len() >= MAX_CACHED_TOKENS {
                    let Some(oldest) = cache
                        .entries
                        .iter()
                        .min_by_key(|(_, entry)| entry.expires_at)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    cache.entries.remove(&oldest);
                }
            }
            cache.entries.insert(
                pin.key.clone(),
                CachedAccessToken {
                    token: token.clone(),
                    version,
                    generation,
                    expires_at,
                },
            );
        } else {
            // A newly rotated short-lived token can serve this request, but
            // must not leave an older token available through the hot path.
            cache.entries.remove(&pin.key);
        }
        CacheVerdict::Serve(token, version, expires_at)
    }

    /// Drop every local trace of a connection whose database credentials were
    /// cleared: cancel in-flight leases fenced at or before `generation` and
    /// evict the cached plaintext token so it cannot outlive its credentials.
    fn invalidate_local_connection(&self, owner: &str, id: &str, generation: i64) {
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            let key = (owner.to_owned(), id.to_owned());
            if let Some(connection) = active.connections.get_mut(&key) {
                for (_, version, cancel) in &connection.requests {
                    if *version <= generation {
                        cancel.cancel();
                    }
                }
                connection
                    .requests
                    .retain(|(_, version, _)| *version > generation);
                if connection.requests.is_empty() {
                    active.connections.remove(&key);
                    if active.connections.is_empty()
                        && let Some(monitor) = &active.monitor
                    {
                        monitor.stop.cancel();
                    }
                }
            }
        }
        let mut cache = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let key = (owner.to_owned(), id.to_owned());
        if let Some(guard) = cache.guards.get_mut(&key) {
            guard.invalidated_through = Some(
                guard
                    .invalidated_through
                    .map_or(generation, |old| old.max(generation)),
            );
        }
        if cache
            .entries
            .get(&key)
            .is_some_and(|entry| entry.generation <= generation)
        {
            cache.entries.remove(&key);
        }
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn cached_token_version_for_test(&self, owner: &str, id: &str) -> Option<i64> {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get(&(owner.to_owned(), id.to_owned()))
            .map(|entry| entry.version)
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn connection_read_queries_for_test(&self) -> usize {
        self.connection_read_queries
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn credential_row_reads_for_test(&self) -> usize {
        self.credential_row_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test-only cross-instance expiry signal: rewriting the database row
    /// cannot reach this instance's memory, so integration tests clear the
    /// entry explicitly to exercise the rediscovery path.
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub fn clear_cached_token_for_test(&self, owner: &str, id: &str) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .remove(&(owner.to_owned(), id.to_owned()));
    }

    async fn access_token(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        generation: i64,
        mut rejected_version: Option<i64>,
    ) -> ApiResult<(String, i64)> {
        if let Some(cached) = self.cached_access_token(owner, id, generation, rejected_version)? {
            return Ok(cached);
        }
        let pin = self
            .pin_token_guard(owner, id, generation, rejected_version)
            .await?;
        let mut stale_row = None;
        loop {
            // Another producer may have populated the cache while admission
            // was pending. Duplicate rejection records are idempotent.
            if let Some(cached) =
                self.cached_access_token(owner, id, generation, rejected_version)?
            {
                return Ok(cached);
            }
            let row = self.credential_row(owner, id, generation).await?;
            let observed = row.credential_version;
            let candidate = self.usable_access_token(&row, rejected_version)?;
            match self.adjudicate_candidate(&pin, generation, rejected_version, observed, candidate)
            {
                CacheVerdict::Serve(token, version, _) => return Ok((token, version)),
                CacheVerdict::Invalidated => return Err(reauth()),
                CacheVerdict::Retry(rejected) => {
                    if stale_row == Some((observed, rejected)) {
                        return Err(unavailable());
                    }
                    stale_row = Some((observed, rejected));
                    rejected_version = Some(rejected);
                    continue;
                }
                CacheVerdict::Refresh(floor) => {
                    stale_row = None;
                    rejected_version = floor;
                }
            }
            let mut refresh = self.refresh(owner, id, generation, rejected_version, &pin);
            let (token, version, expires_at) = loop {
                if let Some(result) = refresh.borrow_and_update().clone() {
                    break result?;
                }
                refresh.changed().await.map_err(|_| {
                    fail(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "chatgpt_refresh_unavailable",
                        "Subscription refresh did not settle",
                    )
                })?;
            };
            // A 401 or disconnect can arrive after the worker's decision but
            // before this waiter receives the result, so adjudicate again.
            match self.adjudicate_candidate(
                &pin,
                generation,
                rejected_version,
                version,
                Some((token, version, expires_at)),
            ) {
                CacheVerdict::Serve(token, version, _) => return Ok((token, version)),
                CacheVerdict::Invalidated => return Err(reauth()),
                CacheVerdict::Retry(rejected) => rejected_version = Some(rejected),
                CacheVerdict::Refresh(floor) => rejected_version = floor,
            }
        }
    }

    async fn credential_row(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
    ) -> ApiResult<CredentialRow> {
        self.enabled()?;
        let pool = self.pool()?;
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        #[cfg(any(test, feature = "e2e-hooks"))]
        self.credential_row_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let row = sqlx::query_as::<_, CredentialRow>(
            "SELECT * FROM user_llm_connections WHERE user_id=? AND connection_id=?",
        )
        .bind(owner)
        .bind(id)
        .fetch_optional(connection.connection_mut())
        .await
        .map_err(internal_error)?;
        connection.release();
        let row = row.ok_or_else(not_found)?;
        connected_generation(&row.fence, generation)?;
        Ok(row)
    }

    fn usable_access_token(
        &self,
        row: &CredentialRow,
        rejected_version: Option<i64>,
    ) -> ApiResult<Option<(String, i64, i64)>> {
        // This prefilter avoids decryption of a caller-rejected row. The
        // pinned guard is still authoritative when the candidate is served.
        if rejected_version.is_none_or(|version| row.credential_version > version)
            && row.refresh_owner.is_none()
            && row
                .token_expires_at
                .is_some_and(|value| value > unix_now() + 60)
        {
            let credentials =
                self.decode_credentials(row.credentials_encrypted.as_deref().ok_or_else(reauth)?)?;
            if credentials.expires_at > unix_now() + 60 {
                let expires_at = row
                    .token_expires_at
                    .map_or(credentials.expires_at, |value| {
                        value.min(credentials.expires_at)
                    });
                return Ok(Some((
                    credentials.access_token,
                    row.credential_version,
                    expires_at,
                )));
            }
        }
        Ok(None)
    }

    async fn refresh_access_token(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
        mut rejected_version: Option<i64>,
        pin: &CachePin,
    ) -> ApiResult<(String, i64, i64)> {
        let config = self.enabled()?;
        let pool = self.pool()?;
        let mut metadata_ready = false;
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        let mut wait = Duration::from_millis(100);
        let mut stale_row = None;
        loop {
            let row = tokio::time::timeout_at(deadline, self.credential_row(owner, id, generation))
                .await
                .map_err(|_| unavailable())??;
            let version = row.credential_version;
            let candidate = self.usable_access_token(&row, rejected_version)?;
            match self.adjudicate_candidate(pin, generation, rejected_version, version, candidate) {
                CacheVerdict::Serve(token, version, expires_at) => {
                    return Ok((token, version, expires_at));
                }
                CacheVerdict::Invalidated => return Err(reauth()),
                CacheVerdict::Retry(rejected) => {
                    if stale_row == Some((version, rejected)) {
                        return Err(unavailable());
                    }
                    stale_row = Some((version, rejected));
                    rejected_version = Some(rejected);
                    continue;
                }
                CacheVerdict::Refresh(floor) => {
                    stale_row = None;
                    rejected_version = floor;
                }
            }
            let encrypted = row.credentials_encrypted.as_deref().ok_or_else(reauth)?;
            if let Some(refresh_owner) = row.refresh_owner.as_deref() {
                if row.refresh_until.is_none_or(|until| until < unix_now()) {
                    if tokio::time::timeout_at(
                        deadline,
                        self.mark_reauth(owner, id, generation, refresh_owner),
                    )
                    .await
                    .map_err(|_| unavailable())??
                    {
                        return Err(reauth());
                    }
                    continue;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(fail(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "chatgpt_refresh_pending",
                        "Subscription refresh is already in progress",
                    ));
                }
                // Only the connection-owned worker polls cross-replica state;
                // local waiters share its result without SQL or decryption.
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(1));
                continue;
            }
            if !metadata_ready {
                // Discovery is independent of token rotation. Warm its cache
                // before claiming the refresh lease, then re-read the winner.
                tokio::time::timeout_at(deadline, self.signing_keys(None))
                    .await
                    .map_err(|_| unavailable())??;
                metadata_ready = true;
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                continue;
            }
            let credentials = self.decode_credentials(encrypted)?;
            let client_id = &row.client_id;
            let refresh_id = Uuid::new_v4().to_string();
            let settlement_deadline = tokio::time::Instant::now() + REFRESH_BUDGET;
            let exchange_deadline = settlement_deadline - Duration::from_secs(5);
            let claim_result = tokio::time::timeout_at(exchange_deadline.min(deadline), async {
                let mut claim = CancellationSafeTransaction::begin(&pool).await.map_err(internal_error)?;
                let lease=query("UPDATE user_llm_connections SET refresh_owner=?,refresh_until=? WHERE user_id=? AND connection_id=? AND generation=? AND credential_version=? AND refresh_owner IS NULL AND status='connected'")
                .bind(&refresh_id).bind(unix_now()+REFRESH_LEASE_SECONDS).bind(owner).bind(id).bind(generation).bind(version).execute(claim.connection_mut()).await.map_err(internal_error)?;
                claim.commit().await.map_err(internal_error)?;
                Ok::<_, (StatusCode, Json<ErrorResponse>)>(lease)
            }).await;
            let lease = match claim_result {
                Ok(Ok(lease)) => lease,
                Ok(Err(_)) | Err(_) => {
                    tokio::time::timeout_at(
                        settlement_deadline,
                        self.release_refresh_lease(owner, id, generation, version, &refresh_id),
                    )
                    .await
                    .map_err(|_| unavailable())??;
                    return Err(unavailable());
                }
            };
            if lease.rows_affected() != 1 {
                continue;
            }
            let mut credentials_unchanged = false;
            // Keep the SQL guard outside the deadline future: discard an
            // interrupted checkout before cleanup needs another pool permit.
            let mut persistence_connection = None;
            let refresh_result=tokio::time::timeout_at(exchange_deadline,async {
                let refresh=credentials.refresh_token.as_deref().ok_or_else(reauth)?;
                let request=self.client().inspect_err(|_|{credentials_unchanged=true;})?.post(&config.token).form(&[
                    ("grant_type","refresh_token"),("client_id",client_id.as_str()),("refresh_token",refresh),("resource",API_BASE),
                ]);
                let response=request.send().await.map_err(|error|{
                    if error.is_connect() || error.is_builder() {credentials_unchanged=true;}
                    unavailable()
                })?;
                // A rate-limited grant was rejected, not rotated. Keep the
                // current token set even if its error body is interrupted.
                credentials_unchanged = response.status() == StatusCode::TOO_MANY_REQUESTS;
                let token:TokenResponse=serde_json::from_slice(&provider_body(response).await?).map_err(|_|reauth())?;
                if token.access_token.is_empty()||token.expires_in<=0||token.expires_in>86400*30 {return Err(reauth());}
                if let Some(id_token)=&token.id_token {
                    let identity=self.identity(id_token,client_id,None).await?;
                    if identity.sub!=row.subject {return Err(reauth());}
                }
                let scopes=token.scope.map(|value|value.split_whitespace().map(str::to_owned).collect()).unwrap_or(credentials.scopes);
                if !scopes.iter().any(|value|value==PLAN_SCOPE) {return Err(reauth());}
                let replacement=Credentials {access_token:token.access_token,refresh_token:token.refresh_token.or(credentials.refresh_token),scopes,expires_at:unix_now()+token.expires_in};
                let encrypted=self.encryptor.encrypt(&serde_json::to_string(&replacement).map_err(internal_error)?).map_err(internal_error)?;
                persistence_connection = Some(CancellationSafePoolConnection::acquire(&pool).await.map_err(internal_error)?);
                let changed=query("UPDATE user_llm_connections SET credentials_encrypted=?,token_expires_at=?,credential_version=credential_version+1,refresh_owner=NULL,refresh_until=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=? AND generation=? AND credential_version=? AND refresh_owner=? AND status='connected'")
                    .bind(encrypted).bind(replacement.expires_at).bind(owner).bind(id).bind(generation).bind(version).bind(&refresh_id).execute(persistence_connection.as_mut().expect("refresh checkout acquired").connection_mut()).await.map_err(internal_error)?;
                persistence_connection.take().expect("refresh SQL acknowledged").release();
                if changed.rows_affected()!=1 {return Err(reauth());}
                Ok((replacement.access_token,version+1,replacement.expires_at))
            }).await.unwrap_or_else(|_|Err(fail(StatusCode::SERVICE_UNAVAILABLE,"chatgpt_refresh_deadline","Subscription refresh timed out")));
            if let Some(connection) = persistence_connection {
                connection.discard();
            }
            if refresh_result.is_err() {
                if !credentials_unchanged {
                    if !tokio::time::timeout_at(
                        settlement_deadline,
                        self.mark_reauth(owner, id, generation, &refresh_id),
                    )
                    .await
                    .map_err(|_| unavailable())??
                    {
                        continue;
                    }
                } else {
                    // Release only our lease; another generation may have won.
                    tokio::time::timeout_at(
                        settlement_deadline,
                        self.release_refresh_lease(owner, id, generation, version, &refresh_id),
                    )
                    .await
                    .map_err(|_| unavailable())??;
                }
            }
            match refresh_result {
                Ok((token, version, expires_at)) => {
                    match self.adjudicate_candidate(
                        pin,
                        generation,
                        rejected_version,
                        version,
                        Some((token, version, expires_at)),
                    ) {
                        CacheVerdict::Serve(token, version, expires_at) => {
                            return Ok((token, version, expires_at));
                        }
                        CacheVerdict::Invalidated => return Err(reauth()),
                        CacheVerdict::Retry(rejected) => rejected_version = Some(rejected),
                        CacheVerdict::Refresh(floor) => rejected_version = floor,
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    async fn release_refresh_lease(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
        version: i64,
        refresh_id: &str,
    ) -> ApiResult<()> {
        let pool = self.pool()?;
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        query("UPDATE user_llm_connections SET refresh_owner=NULL,refresh_until=NULL WHERE user_id=? AND connection_id=? AND generation=? AND credential_version=? AND refresh_owner=?")
            .bind(owner).bind(id).bind(generation).bind(version).bind(refresh_id).execute(connection.connection_mut()).await.map_err(internal_error)?;
        connection.release();
        Ok(())
    }
    async fn mark_reauth(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
        refresh_owner: &str,
    ) -> ApiResult<bool> {
        let pool = self.pool()?;
        let mut tx = CancellationSafeTransaction::begin(&pool)
            .await
            .map_err(internal_error)?;
        let changed=query("UPDATE user_llm_connections SET status='reauth_required',credentials_encrypted=NULL,token_expires_at=NULL,generation=generation+1,refresh_owner=NULL,refresh_until=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=? AND generation=? AND refresh_owner=? AND status='connected'")
            .bind(owner).bind(id).bind(generation).bind(refresh_owner).execute(&mut *tx).await.map_err(internal_error)?;
        if changed.rows_affected() == 1 {
            query("UPDATE user_llm_models SET is_active=0,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
                .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        }
        tx.commit().await.map_err(internal_error)?;
        if changed.rows_affected() == 1 {
            // Credentials are cleared: evict the plaintext cache entry now
            // instead of waiting for a generation-mismatched read.
            self.invalidate_local_connection(owner, id, generation);
        }
        Ok(changed.rows_affected() == 1)
    }
    async fn authenticated_body(
        &self,
        binding: &ExecutionCredential,
        request: reqwest::RequestBuilder,
    ) -> ApiResult<Vec<u8>> {
        let mut rejected = None;
        loop {
            let lease = binding.acquire(rejected).await?;
            let response = request
                .try_clone()
                .ok_or_else(|| internal_error("Subscription probe body cannot be replayed"))?
                .bearer_auth(lease.bearer())
                .send()
                .await
                .map_err(|_| unavailable())?;
            let status = response.status();
            let body = provider_body(response).await;
            if status == StatusCode::UNAUTHORIZED && rejected.is_none() {
                rejected = Some(lease.version());
                continue;
            }
            let body = body?;
            binding.ensure_current().await?;
            return Ok(body);
        }
    }
    pub async fn models(self: &Arc<Self>, owner: &str, id: &str) -> ApiResult<Value> {
        let mut catalog = self.catalog(&self.binding(owner, id).await?).await?;
        let pool = self.pool()?;
        let mut connection = CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let configured: Vec<(String, String)> = sqlx::query_as("SELECT model_id,model_alias FROM user_llm_models WHERE user_id=? AND connection_id=? ORDER BY model_alias,model_id")
            .bind(owner).bind(id).fetch_all(connection.connection_mut()).await.map_err(internal_error)?;
        connection.release();
        catalog["configured_models"] = json!(
            configured
                .into_iter()
                .map(|(model_id, name)| json!({"model_id":model_id,"name":name}))
                .collect::<Vec<_>>()
        );
        Ok(catalog)
    }
    async fn catalog(&self, binding: &ExecutionCredential) -> ApiResult<Value> {
        let bytes = self
            .authenticated_body(
                binding,
                self.client()?
                    .get(format!("{}/models", binding.api_base()?)),
            )
            .await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| internal_error("OpenAI returned an invalid JSON response"))?;
        let items = astra_core::model_wire::responses::subscription_catalog(&value)
            .map_err(internal_error)?;
        Ok(json!({"connection_id":binding.connection_id(),"items":items}))
    }
    pub async fn probe(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        model: &str,
    ) -> ApiResult<(ExecutionCredential, Vec<String>)> {
        let binding = self.binding(owner, id).await?;
        let catalog = self.catalog(&binding).await?;
        let selected = catalog["items"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["slug"] == model))
            .ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    "chatgpt_model_unavailable",
                    "Choose a model from this connection's current catalog",
                )
            })?;
        let efforts: Vec<String> =
            serde_json::from_value(selected["supported_reasoning_efforts"].clone())
                .map_err(internal_error)?;
        let body = astra_core::model_wire::responses::request(
            model,
            &[json!({"role":"user","content":"Reply briefly with OK."})],
            None,
            id,
            true,
            None,
        )
        .map_err(internal_error)?;
        let bytes = self
            .authenticated_body(
                &binding,
                self.client()?
                    .post(format!("{}/responses", binding.api_base()?))
                    .json(&body),
            )
            .await?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| internal_error("Invalid subscription response encoding"))?;
        let output = astra_core::model_wire::responses::decode_sse(text)
            .map_err(|reason| model_check_failure(&reason))?;
        if output.finish_reason.as_deref() == Some("length") || output.text.trim().is_empty() {
            tracing::warn!("ChatGPT subscription model check did not complete with usable text");
            return Err(fail(
                StatusCode::BAD_GATEWAY,
                "chatgpt_model_check_failed",
                "Subscription model check did not complete with usable text",
            ));
        }
        Ok((binding, efforts))
    }
    async fn disconnect(&self, owner: &str, id: &str) -> ApiResult<Value> {
        let pool = self.pool()?;
        let mut tx = pool.begin().await.map_err(internal_error)?;
        let (status,client_id,encrypted,generation,confirmed):(String,String,Option<String>,i64,Option<i16>)=sqlx::query_as("SELECT status,client_id,credentials_encrypted,generation,revocation_confirmed FROM user_llm_connections WHERE user_id=? AND connection_id=? FOR UPDATE")
            .bind(owner).bind(id).fetch_optional(&mut *tx).await.map_err(internal_error)?.ok_or_else(not_found)?;
        // Cancel consent even when this connection was already disconnected.
        query("UPDATE user_llm_authorizations SET status='cancelled',verifier_encrypted=NULL WHERE user_id=? AND connection_id=? AND status IN ('pending','exchanging')")
            .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        if status == "disconnected" && encrypted.is_none() {
            tx.commit().await.map_err(internal_error)?;
            self.invalidate_local_connection(owner, id, generation);
            return Ok(
                json!({"connection_id":id,"status":"disconnected","remote_revocation_confirmed":confirmed==Some(1)}),
            );
        }
        query("UPDATE user_llm_connections SET status='disconnected',credentials_encrypted=NULL,token_expires_at=NULL,generation=generation+1,refresh_owner=NULL,refresh_until=NULL,revocation_confirmed=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
            .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        query("UPDATE user_llm_models SET is_active=0,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
            .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        // Credentials are gone: cached plaintext tokens must not outlive them.
        self.invalidate_local_connection(owner, id, generation);
        let confirmed = if let Some(encrypted) = encrypted {
            self.revoke(&client_id, &encrypted).await.is_ok()
        } else {
            false
        };
        query("UPDATE user_llm_connections SET revocation_confirmed=? WHERE user_id=? AND connection_id=? AND status='disconnected' AND generation=?")
            .bind(if confirmed {1_i16} else {0_i16}).bind(owner).bind(id).bind(generation+1).execute(&pool).await.map_err(internal_error)?;
        Ok(
            json!({"connection_id":id,"status":"disconnected","remote_revocation_confirmed":confirmed}),
        )
    }
    async fn revoke(&self, client_id: &str, encrypted: &str) -> ApiResult<()> {
        let credentials = self.decode_credentials(encrypted)?;
        let refresh = credentials.refresh_token.as_deref().ok_or_else(reauth)?;
        let config = self.config()?;
        let discovery = self
            .read_json(self.client()?.get(config.provider_url(&config.discovery)?))
            .await?;
        let endpoint = discovery
            .get("revocation_endpoint")
            .and_then(Value::as_str)
            .ok_or_else(reauth)?;
        let response = self
            .client()?
            .post(config.provider_url(endpoint)?)
            .form(&[
                ("token", refresh),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(|_| reauth())?;
        if response.status() != StatusCode::OK {
            return Err(reauth());
        }
        Ok(())
    }
}

pub(crate) const CONNECTIONS_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS user_llm_connections (
        connection_id VARCHAR(64) NOT NULL, user_id VARCHAR(128) NOT NULL,
        provider VARCHAR(50) NOT NULL, label VARCHAR(100) NOT NULL,
        client_id VARCHAR(255) NOT NULL, subject VARCHAR(255) NOT NULL,
        email VARCHAR(255) NULL, host_id VARCHAR(100) NOT NULL,
        status VARCHAR(32) NOT NULL, credentials_encrypted TEXT NULL,
        token_expires_at BIGINT NULL, revocation_confirmed SMALLINT NULL, generation BIGINT NOT NULL DEFAULT 1,
        credential_version BIGINT NOT NULL DEFAULT 1,
        refresh_owner VARCHAR(64) NULL, refresh_until BIGINT NULL,
        created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
        updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
        PRIMARY KEY (user_id, connection_id),
        UNIQUE KEY uq_user_llm_connection_registration (user_id, client_id, subject)
    )";

pub(crate) const AUTHORIZATIONS_TABLE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS user_llm_authorizations (
        authorization_id VARCHAR(64) NOT NULL, user_id VARCHAR(128) NOT NULL,
        connection_id VARCHAR(64) NULL, label VARCHAR(100) NOT NULL,
        redirect_uri VARCHAR(255) NOT NULL, state_digest VARCHAR(64) NOT NULL,
        nonce_digest VARCHAR(64) NOT NULL, verifier_encrypted TEXT NULL,
        client_id VARCHAR(255) NOT NULL, expected_subject VARCHAR(255) NULL,
        status VARCHAR(32) NOT NULL, expires_at BIGINT NOT NULL,
        callback_digest VARCHAR(64) NULL, completed_connection_id VARCHAR(64) NULL,
        PRIMARY KEY (user_id, authorization_id)
    )";
