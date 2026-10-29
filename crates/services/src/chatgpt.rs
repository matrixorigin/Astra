//! Owner-scoped ChatGPT subscription connections. OAuth is a model credential,
//! not an Astra login; renewable credentials never cross the control API.

use crate::auth::FernetTokenEncryptor;
use astra_core::{ErrorResponse, SharedPool, error_response_coded, internal_error};
use axum::{Json, http::StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use jsonwebtoken::{Algorithm, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{MySqlPool, Row, query};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const PROVIDER: &str = "chatgpt";
#[cfg(test)]
mod tests;
pub const API_BASE: &str = "https://api.openai.com/v1";
const ISSUER: &str = "https://auth.openai.com";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const AUTH_LIFETIME: i64 = 900;
const REFRESH_BUDGET: Duration = Duration::from_secs(25);
const REFRESH_LEASE_SECONDS: i64 = 30;
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
fn connected_generation(row: &sqlx::mysql::MySqlRow, generation: i64) -> ApiResult<()> {
    if row.try_get::<String, _>("status").map_err(internal_error)? != "connected"
        || row
            .try_get::<i64, _>("generation")
            .map_err(internal_error)?
            != generation
    {
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
        let enabled = match std::env::var("ASTRA_CHATGPT_SUBSCRIPTION_ENABLED").as_deref() {
            Err(_) | Ok("") | Ok("0") | Ok("false") => false,
            Ok("1") | Ok("true") => true,
            _ => return Err("ASTRA_CHATGPT_SUBSCRIPTION_ENABLED must be 0 or 1".into()),
        };
        let host_id = std::env::var("ASTRA_CHATGPT_HOST_ID").unwrap_or_default();
        if enabled && (!host_id.starts_with("urn:uuid:") || Uuid::parse_str(&host_id[9..]).is_err())
        {
            return Err("Enabled ChatGPT connections require a stable ASTRA_CHATGPT_HOST_ID=urn:uuid:<UUID>".into());
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Deserialize)]
struct Identity {
    sub: String,
    nonce: Option<String>,
    email: Option<String>,
}

type ConnectionKey = (String, String);
type RefreshKey = (String, String, i64);
type RefreshWaiter = tokio::sync::watch::Receiver<Option<ApiResult<(String, i64)>>>;
struct ActiveConnection {
    monitor: Uuid,
    requests: Vec<(Uuid, i64, CancellationToken)>,
}
struct SigningKeys {
    fetched: std::time::Instant,
    keys: JwkSet,
}
pub struct ChatgptConnections {
    pool: Option<SharedPool>,
    encryptor: Arc<FernetTokenEncryptor>,
    config: Result<ChatgptConfig, String>,
    active: Mutex<HashMap<ConnectionKey, ActiveConnection>>,
    refreshes: Mutex<HashMap<RefreshKey, RefreshWaiter>>,
    signing_keys: tokio::sync::Mutex<Option<SigningKeys>>,
    inference_client: OnceLock<reqwest::Client>,
    #[cfg(any(test, feature = "e2e-hooks"))]
    fence_reads: std::sync::atomic::AtomicUsize,
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
        if let Some(connection) = active.get_mut(&(self.owner.clone(), self.connection_id.clone()))
        {
            connection
                .requests
                .retain(|(id, _, _)| *id != self.request_id);
            if connection.requests.is_empty() {
                active.remove(&(self.owner.clone(), self.connection_id.clone()));
            }
        }
    }
}
impl ExecutionCredential {
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
        let row = query("SELECT status, generation FROM user_llm_connections WHERE user_id = ? AND connection_id = ?")
            .bind(&self.owner).bind(&self.connection_id).fetch_optional(&pool).await.map_err(internal_error)?.ok_or_else(not_found)?;
        connected_generation(&row, self.generation)
    }

    /// Serialize model activation with disconnect/reauthorization. Call only
    /// after provider I/O, before touching model rows in the same transaction.
    pub(crate) async fn lock_current(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    ) -> ApiResult<()> {
        self.service.enabled()?;
        let row = query("SELECT status,generation FROM user_llm_connections WHERE user_id=? AND connection_id=? FOR UPDATE")
            .bind(&self.owner).bind(&self.connection_id).fetch_optional(&mut **tx).await.map_err(internal_error)?.ok_or_else(not_found)?;
        connected_generation(&row, self.generation)
    }
}

pub fn validate_redirect(value: &str) -> Result<(), &'static str> {
    let url = Url::parse(value).map_err(|_| "Invalid OAuth redirect URI")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/auth/callback"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("OAuth redirect must be http://127.0.0.1:<port>/auth/callback");
    }
    Ok(())
}

impl ChatgptConnections {
    pub fn new(
        pool: Option<SharedPool>,
        encryptor: Arc<FernetTokenEncryptor>,
        config: Result<ChatgptConfig, String>,
    ) -> Self {
        Self {
            pool,
            encryptor,
            config,
            active: Mutex::new(HashMap::new()),
            refreshes: Mutex::new(HashMap::new()),
            signing_keys: tokio::sync::Mutex::new(None),
            inference_client: OnceLock::new(),
            #[cfg(any(test, feature = "e2e-hooks"))]
            fence_reads: std::sync::atomic::AtomicUsize::new(0),
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
            ConnectionOperation::Complete(id, request) => self.complete(owner, &id, request).await,
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
        let rows = query("SELECT connection_id, provider, label, email, status, token_expires_at, revocation_confirmed FROM user_llm_connections WHERE user_id = ? ORDER BY created_at, connection_id")
            .bind(owner).fetch_all(&self.pool()?).await.map_err(internal_error)?;
        rows.iter()
            .map(|row| {
                Ok(ConnectionView {
                    remote_revocation_confirmed: row
                        .try_get::<Option<i16>, _>("revocation_confirmed")
                        .map_err(internal_error)?
                        .map(|value| value != 0),
                    connection_id: row.try_get("connection_id").map_err(internal_error)?,
                    provider: row.try_get("provider").map_err(internal_error)?,
                    label: row.try_get("label").map_err(internal_error)?,
                    email: row.try_get("email").map_err(internal_error)?,
                    status: row.try_get("status").map_err(internal_error)?,
                    expires_at: row.try_get("token_expires_at").map_err(internal_error)?,
                })
            })
            .collect()
    }
    pub async fn catalog_labels(&self, owner: &str) -> ApiResult<HashMap<String, String>> {
        let Ok(config) = self.enabled() else {
            return Ok(HashMap::new());
        };
        let rows=query("SELECT connection_id,label FROM user_llm_connections WHERE user_id=? AND host_id=? AND status='connected'")
            .bind(owner).bind(&config.host_id).fetch_all(&self.pool()?).await.map_err(internal_error)?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("connection_id").map_err(internal_error)?,
                    row.try_get("label").map_err(internal_error)?,
                ))
            })
            .collect()
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
            let row=query("SELECT client_id,expected_subject,connection_id,label,status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=?")
                .bind(owner).bind(previous).fetch_optional(&pool).await.map_err(internal_error)?.ok_or_else(not_found)?;
            let client: String = row.try_get("client_id").map_err(internal_error)?;
            let status: String = row.try_get("status").map_err(internal_error)?;
            if client == "dynamic_agent_client" || status != "failed" {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "oauth_attempt_not_retryable",
                    "Inspect your connections or start a new registration",
                ));
            }
            request.connection_id = row.try_get("connection_id").map_err(internal_error)?;
            label = row.try_get("label").map_err(internal_error)?;
            (
                client,
                row.try_get::<Option<String>, _>("expected_subject")
                    .map_err(internal_error)?,
            )
        } else if let Some(id) = &request.connection_id {
            let row=query("SELECT client_id,subject,label,status FROM user_llm_connections WHERE user_id=? AND connection_id=?")
                .bind(owner).bind(id).fetch_optional(&pool).await.map_err(internal_error)?.ok_or_else(not_found)?;
            label = row.try_get("label").map_err(internal_error)?;
            need_consent =
                row.try_get::<String, _>("status").map_err(internal_error)? == "plan_disabled";
            (
                row.try_get("client_id").map_err(internal_error)?,
                Some(row.try_get("subject").map_err(internal_error)?),
            )
        } else {
            ("dynamic_agent_client".into(), None)
        };
        let state = Uuid::new_v4().simple().to_string();
        let nonce = Uuid::new_v4().simple().to_string();
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let id = Uuid::new_v4().to_string();
        let expires = unix_now() + AUTH_LIFETIME;
        let mut url = config.provider_url(&config.authorize)?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("client_id", &client_id)
                .append_pair("ext_agent_host_id", &config.host_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &request.redirect_uri)
                .append_pair("scope", SCOPES)
                .append_pair("resource", API_BASE)
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge);
            if client_id == "dynamic_agent_client" {
                pairs.append_pair("agent_name_hint", "Astra");
            }
            if need_consent {
                pairs.append_pair("prompt", "consent");
            }
        }
        query("DELETE FROM user_llm_authorizations WHERE user_id = ? AND expires_at < ?")
            .bind(owner)
            .bind(unix_now() - 86400)
            .execute(&pool)
            .await
            .map_err(internal_error)?;
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_llm_authorizations WHERE user_id = ? AND expires_at > ? AND status = 'pending'")
            .bind(owner).bind(unix_now()).fetch_one(&pool).await.map_err(internal_error)?;
        if pending >= 8 {
            return Err(fail(
                StatusCode::TOO_MANY_REQUESTS,
                "oauth_attempt_limit",
                "Finish or cancel a pending authorization",
            ));
        }
        query("INSERT INTO user_llm_authorizations (authorization_id,user_id,connection_id,label,redirect_uri,state_digest,nonce_digest,verifier_encrypted,client_id,expected_subject,status,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,'pending',?)")
            .bind(&id).bind(owner).bind(request.connection_id).bind(label).bind(request.redirect_uri)
            .bind(self.encryptor.keyed_digest("chatgpt-state",&state)).bind(self.encryptor.keyed_digest("chatgpt-nonce",&nonce))
            .bind(self.encryptor.encrypt(&verifier).map_err(internal_error)?).bind(client_id).bind(subject).bind(expires)
            .execute(&pool).await.map_err(internal_error)?;
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
        // Rotation belongs to the connection, not an inference's cancellation
        // or deadline. Always finish its bounded exchange and persistence even
        // if every waiter goes away; disconnect still fences its database CAS.
        tokio::spawn(async move {
            let outcome = service
                .refresh_access_token(&key.0, &key.1, generation, rejected_version)
                .await;
            service
                .refreshes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            result.send_replace(Some(outcome));
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
        let key = (owner.to_owned(), id.to_owned());
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let first = !active.contains_key(&key);
        let connection = active
            .entry(key.clone())
            .or_insert_with(|| ActiveConnection {
                monitor: Uuid::new_v4(),
                requests: Vec::new(),
            });
        connection.requests.push((request, generation, cancel));
        if first {
            let monitor = connection.monitor;
            let service = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let Some(service) = service.upgrade() else {
                        return;
                    };
                    if !service
                        .active
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&key)
                        .is_some_and(|value| value.monitor == monitor)
                    {
                        return;
                    }
                    #[cfg(any(test, feature = "e2e-hooks"))]
                    service
                        .fence_reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let row=tokio::time::timeout(Duration::from_secs(2),async {
                        query("SELECT status,generation FROM user_llm_connections WHERE user_id=? AND connection_id=?")
                            .bind(&key.0).bind(&key.1).fetch_optional(&service.pool()?).await.map_err(internal_error)
                    }).await;
                    let observed = match row {
                        Ok(Ok(Some(row))) => row
                            .try_get::<String, _>("status")
                            .ok()
                            .zip(row.try_get::<i64, _>("generation").ok()),
                        _ => None,
                    };
                    let mut active = service.active.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(connection) = active
                        .get_mut(&key)
                        .filter(|value| value.monitor == monitor)
                    else {
                        return;
                    };
                    for (_, generation, cancel) in &connection.requests {
                        if observed.as_ref().is_none_or(|(status, current)| {
                            *generation < *current
                                || (status != "connected" && generation == current)
                        }) {
                            cancel.cancel();
                        }
                    }
                    connection
                        .requests
                        .retain(|(_, _, cancel)| !cancel.is_cancelled());
                    if connection.requests.is_empty() {
                        active.remove(&key);
                        return;
                    }
                }
            });
        }
    }

    async fn identity(
        &self,
        id_token: &str,
        client_id: &str,
        nonce: Option<&str>,
    ) -> ApiResult<Identity> {
        let config = self.config()?;
        let header = decode_header(id_token).map_err(|_| reauth())?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(reauth());
        }
        let kid = header.kid.as_deref().ok_or_else(reauth)?;
        let jwks = self.signing_keys(Some(kid)).await?;
        let key = jwks.find(kid).ok_or_else(reauth)?;
        let key = jsonwebtoken::DecodingKey::from_jwk(key).map_err(|_| reauth())?;
        let mut validation = Validation::new(header.alg);
        validation.leeway = 0;
        validation.set_issuer(&[config.issuer.as_str()]);
        validation.set_audience(&[client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
        let identity = decode::<Identity>(id_token, &key, &validation)
            .map_err(|_| reauth())?
            .claims;
        if identity.sub.is_empty()
            || identity.sub.len() > 255
            || identity.email.as_ref().is_some_and(|e| e.len() > 255)
        {
            return Err(reauth());
        }
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
    ) -> ApiResult<Value> {
        let config = self.enabled()?;
        let pool = self.pool()?;
        let row = query(
            "SELECT * FROM user_llm_authorizations WHERE user_id = ? AND authorization_id = ?",
        )
        .bind(owner)
        .bind(id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?
        .ok_or_else(not_found)?;
        let expected: String = row.try_get("state_digest").map_err(internal_error)?;
        if !self
            .encryptor
            .verify_keyed_digest("chatgpt-state", &request.state, &expected)
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "oauth_state_invalid",
                "OAuth state did not match this attempt",
            ));
        }
        let status: String = row.try_get("status").map_err(internal_error)?;
        let callback_digest = self.encryptor.keyed_digest(
            "chatgpt-callback",
            &serde_json::to_string(&json!([request.code, request.client_id, request.error]))
                .map_err(internal_error)?,
        );
        if status == "completed" {
            if row
                .try_get::<Option<String>, _>("callback_digest")
                .map_err(internal_error)?
                .as_deref()
                != Some(callback_digest.as_str())
            {
                return Err(reauth());
            }
            let connection: String = row
                .try_get("completed_connection_id")
                .map_err(internal_error)?;
            return Ok(json!({"connection":self.view(owner,&connection).await?}));
        }
        if status != "pending"
            || row
                .try_get::<i64, _>("expires_at")
                .map_err(internal_error)?
                < unix_now()
        {
            return Err(fail(
                StatusCode::CONFLICT,
                "oauth_attempt_expired",
                "Start a fresh ChatGPT authorization",
            ));
        }
        if request.error.is_some() {
            query("UPDATE user_llm_authorizations SET status='declined', verifier_encrypted=NULL WHERE user_id=? AND authorization_id=? AND status='pending'")
                .bind(owner).bind(id).execute(&pool).await.map_err(internal_error)?;
            return Err(fail(
                StatusCode::FORBIDDEN,
                "chatgpt_consent_declined",
                "ChatGPT authorization was declined",
            ));
        }
        let saved_client: String = row.try_get("client_id").map_err(internal_error)?;
        let client_id = request.client_id.as_deref().unwrap_or(&saved_client);
        if client_id == "dynamic_agent_client"
            || client_id.is_empty()
            || client_id.len() > 255
            || !client_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            || (saved_client != "dynamic_agent_client" && saved_client != client_id)
        {
            return Err(reauth());
        }
        let code = request
            .code
            .as_deref()
            .filter(|c| !c.is_empty() && c.len() < 16384)
            .ok_or_else(reauth)?;
        let verifier: String = row
            .try_get::<Option<String>, _>("verifier_encrypted")
            .map_err(internal_error)?
            .ok_or_else(reauth)?;
        let verifier = self.encryptor.decrypt(&verifier).map_err(internal_error)?;
        let redirect: String = row.try_get("redirect_uri").map_err(internal_error)?;
        let changed = query("UPDATE user_llm_authorizations SET status='exchanging', client_id=? WHERE user_id=? AND authorization_id=? AND status='pending' AND expires_at>=?")
            .bind(client_id).bind(owner).bind(id).bind(unix_now()).execute(&pool).await.map_err(internal_error)?;
        if changed.rows_affected() != 1 {
            return Err(fail(
                StatusCode::CONFLICT,
                "oauth_attempt_consumed",
                "Authorization is already being exchanged",
            ));
        }
        let result = async {
            let value = self.read_json(self.client()?.post(&config.token).form(&[
                ("grant_type","authorization_code"),("client_id",client_id),("code",code),
                ("code_verifier",verifier.as_str()),("redirect_uri",redirect.as_str()),("resource",API_BASE),
            ])).await?;
            let token: TokenResponse = serde_json::from_value(value).map_err(|_| reauth())?;
            let id_token = token.id_token.as_deref().ok_or_else(reauth)?;
            let nonce: String = row.try_get("nonce_digest").map_err(internal_error)?;
            let identity = self.identity(id_token,client_id,Some(&nonce)).await?;
            let subject: Option<String> = row.try_get("expected_subject").map_err(internal_error)?;
            if subject.as_deref().is_some_and(|expected| expected!=identity.sub) { return Err(reauth()); }
            if token.expires_in<=0 || token.expires_in>86400*30 || token.access_token.is_empty() { return Err(reauth()); }
            let scopes: Vec<String> = token.scope.as_deref().unwrap_or("").split_whitespace().map(str::to_owned).collect();
            let state = if scopes.iter().any(|s| s==PLAN_SCOPE) { "connected" } else { "plan_disabled" };
            let credentials = Credentials { access_token:token.access_token,refresh_token:token.refresh_token,
                scopes,expires_at:unix_now()+token.expires_in };
            let encrypted = self.encryptor.encrypt(&serde_json::to_string(&credentials).map_err(internal_error)?).map_err(internal_error)?;
            let requested_id: Option<String> = row.try_get("connection_id").map_err(internal_error)?;
            let existing = query("SELECT connection_id FROM user_llm_connections WHERE user_id=? AND client_id=? AND subject=?")
                .bind(owner).bind(client_id).bind(&identity.sub).fetch_optional(&pool).await.map_err(internal_error)?;
            let connection_id = requested_id.or_else(|| existing.and_then(|r| r.try_get("connection_id").ok())).unwrap_or_else(|| Uuid::new_v4().to_string());
            let label: String = row.try_get("label").map_err(internal_error)?;
            let mut tx = pool.begin().await.map_err(internal_error)?;
            let pending: String = query("SELECT status FROM user_llm_authorizations WHERE user_id=? AND authorization_id=? FOR UPDATE")
                .bind(owner).bind(id).fetch_one(&mut *tx).await.map_err(internal_error)?.try_get("status").map_err(internal_error)?;
            if pending!="exchanging" { return Err(fail(StatusCode::CONFLICT,"oauth_attempt_cancelled","Authorization was cancelled before linking completed")); }
            query("INSERT INTO user_llm_connections (connection_id,user_id,provider,label,client_id,subject,email,host_id,status,credentials_encrypted,token_expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON DUPLICATE KEY UPDATE credentials_encrypted=VALUES(credentials_encrypted),token_expires_at=VALUES(token_expires_at),status=VALUES(status),revocation_confirmed=NULL,email=VALUES(email),host_id=VALUES(host_id),generation=generation+1,credential_version=credential_version+1,refresh_owner=NULL,refresh_until=NULL,updated_at=NOW(6)")
                .bind(&connection_id).bind(owner).bind(PROVIDER).bind(label).bind(client_id).bind(identity.sub).bind(identity.email)
                .bind(&config.host_id).bind(state).bind(encrypted).bind(credentials.expires_at).execute(&mut *tx).await.map_err(internal_error)?;
            if state!="connected" {
                query("UPDATE user_llm_models SET is_active=0,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
                    .bind(owner).bind(&connection_id).execute(&mut *tx).await.map_err(internal_error)?;
            }
            query("UPDATE user_llm_authorizations SET status='completed',verifier_encrypted=NULL,callback_digest=?,completed_connection_id=? WHERE user_id=? AND authorization_id=? AND status='exchanging'")
                .bind(callback_digest).bind(&connection_id).bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
            tx.commit().await.map_err(internal_error)?;
            Ok(json!({"connection":self.view(owner,&connection_id).await?}))
        }.await;
        if result.is_err() {
            query("UPDATE user_llm_authorizations SET status='failed',verifier_encrypted=NULL WHERE user_id=? AND authorization_id=? AND status='exchanging'")
                .bind(owner).bind(id).execute(&pool).await.map_err(internal_error)?;
        }
        result
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
        self.enabled()?;
        let row = query("SELECT status,generation,host_id,label FROM user_llm_connections WHERE user_id=? AND connection_id=?")
            .bind(owner).bind(id).fetch_optional(&self.pool()?).await.map_err(internal_error)?.ok_or_else(not_found)?;
        let status: String = row.try_get("status").map_err(internal_error)?;
        if status == "plan_disabled" {
            return Err(fail(
                StatusCode::FORBIDDEN,
                "chatgpt_plan_usage_disabled",
                "Authorize ChatGPT plan usage for this connection",
            ));
        }
        if status != "connected" {
            return Err(reauth());
        }
        if row
            .try_get::<String, _>("host_id")
            .map_err(internal_error)?
            != self.enabled()?.host_id
        {
            return Err(reauth());
        }
        Ok(ExecutionCredential {
            service: self.clone(),
            owner: owner.into(),
            connection_id: id.into(),
            generation: row.try_get("generation").map_err(internal_error)?,
            label: row.try_get("label").map_err(internal_error)?,
        })
    }
    pub fn api_base(&self) -> ApiResult<&str> {
        Ok(&self.enabled()?.api_base)
    }
    fn decode_credentials(&self, encrypted: &str) -> ApiResult<Credentials> {
        serde_json::from_str(&self.encryptor.decrypt(encrypted).map_err(internal_error)?)
            .map_err(|_| internal_error("Stored subscription credentials are invalid"))
    }

    async fn access_token(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        generation: i64,
        rejected_version: Option<i64>,
    ) -> ApiResult<(String, i64)> {
        loop {
            let row = self.credential_row(owner, id, generation).await?;
            if let Some(token) = self.usable_access_token(&row, rejected_version)? {
                return Ok(token);
            }
            let mut refresh = self.refresh(owner, id, generation, rejected_version);
            let (token, version) = loop {
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
            // A caller rejecting the next version may have joined an older
            // flight that only reused it. That caller must refresh that version.
            if rejected_version != Some(version) {
                return Ok((token, version));
            }
        }
    }

    async fn credential_row(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
    ) -> ApiResult<sqlx::mysql::MySqlRow> {
        self.enabled()?;
        let row = query("SELECT * FROM user_llm_connections WHERE user_id=? AND connection_id=?")
            .bind(owner)
            .bind(id)
            .fetch_optional(&self.pool()?)
            .await
            .map_err(internal_error)?
            .ok_or_else(not_found)?;
        connected_generation(&row, generation)?;
        Ok(row)
    }

    fn usable_access_token(
        &self,
        row: &sqlx::mysql::MySqlRow,
        rejected_version: Option<i64>,
    ) -> ApiResult<Option<(String, i64)>> {
        let version: i64 = row.try_get("credential_version").map_err(internal_error)?;
        let expires: Option<i64> = row.try_get("token_expires_at").map_err(internal_error)?;
        let refreshing: Option<String> = row.try_get("refresh_owner").map_err(internal_error)?;
        if rejected_version != Some(version)
            && refreshing.is_none()
            && expires.is_some_and(|value| value > unix_now() + 60)
        {
            let encrypted: Option<String> = row
                .try_get("credentials_encrypted")
                .map_err(internal_error)?;
            let credentials = self.decode_credentials(&encrypted.ok_or_else(reauth)?)?;
            if credentials.expires_at > unix_now() + 60 {
                return Ok(Some((credentials.access_token, version)));
            }
        }
        Ok(None)
    }

    async fn refresh_access_token(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
        rejected_version: Option<i64>,
    ) -> ApiResult<(String, i64)> {
        let config = self.enabled()?;
        let pool = self.pool()?;
        let mut metadata_ready = false;
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        let mut wait = Duration::from_millis(100);
        loop {
            let row = self.credential_row(owner, id, generation).await?;
            if let Some(token) = self.usable_access_token(&row, rejected_version)? {
                return Ok(token);
            }
            let version: i64 = row.try_get("credential_version").map_err(internal_error)?;
            let encrypted: Option<String> = row
                .try_get("credentials_encrypted")
                .map_err(internal_error)?;
            let encrypted = encrypted.ok_or_else(reauth)?;
            let refresh_owner: Option<String> =
                row.try_get("refresh_owner").map_err(internal_error)?;
            if let Some(refresh_owner) = refresh_owner {
                let until: Option<i64> = row.try_get("refresh_until").map_err(internal_error)?;
                if until.is_none_or(|until| until < unix_now()) {
                    if self
                        .mark_reauth(owner, id, generation, &refresh_owner)
                        .await?
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
                self.signing_keys(None).await?;
                metadata_ready = true;
                deadline = tokio::time::Instant::now() + Duration::from_secs(35);
                continue;
            }
            let credentials = self.decode_credentials(&encrypted)?;
            let client_id: String = row.try_get("client_id").map_err(internal_error)?;
            let refresh_id = Uuid::new_v4().to_string();
            let lease=query("UPDATE user_llm_connections SET refresh_owner=?,refresh_until=? WHERE user_id=? AND connection_id=? AND generation=? AND credential_version=? AND refresh_owner IS NULL AND status='connected'")
                .bind(&refresh_id).bind(unix_now()+REFRESH_LEASE_SECONDS).bind(owner).bind(id).bind(generation).bind(version).execute(&pool).await.map_err(internal_error)?;
            if lease.rows_affected() != 1 {
                continue;
            }
            let mut credentials_unchanged = false;
            let refresh_result=tokio::time::timeout(REFRESH_BUDGET,async {
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
                    let identity=self.identity(id_token,&client_id,None).await?;
                    if identity.sub!=row.try_get::<String,_>("subject").map_err(internal_error)? {return Err(reauth());}
                }
                let scopes=token.scope.map(|value|value.split_whitespace().map(str::to_owned).collect()).unwrap_or(credentials.scopes);
                if !scopes.iter().any(|value|value==PLAN_SCOPE) {return Err(reauth());}
                let replacement=Credentials {access_token:token.access_token,refresh_token:token.refresh_token.or(credentials.refresh_token),scopes,expires_at:unix_now()+token.expires_in};
                let encrypted=self.encryptor.encrypt(&serde_json::to_string(&replacement).map_err(internal_error)?).map_err(internal_error)?;
                let changed=query("UPDATE user_llm_connections SET credentials_encrypted=?,token_expires_at=?,credential_version=credential_version+1,refresh_owner=NULL,refresh_until=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=? AND generation=? AND credential_version=? AND refresh_owner=? AND status='connected'")
                    .bind(encrypted).bind(replacement.expires_at).bind(owner).bind(id).bind(generation).bind(version).bind(&refresh_id).execute(&pool).await.map_err(internal_error)?;
                if changed.rows_affected()!=1 {return Err(reauth());}
                Ok((replacement.access_token,version+1))
            }).await.unwrap_or_else(|_|Err(fail(StatusCode::SERVICE_UNAVAILABLE,"chatgpt_refresh_deadline","Subscription refresh timed out")));
            if refresh_result.is_err() {
                if !credentials_unchanged {
                    if !self.mark_reauth(owner, id, generation, &refresh_id).await? {
                        continue;
                    }
                } else {
                    // Release only our lease; another generation may have won.
                    query("UPDATE user_llm_connections SET refresh_owner=NULL,refresh_until=NULL WHERE user_id=? AND connection_id=? AND generation=? AND credential_version=? AND refresh_owner=?")
                        .bind(owner).bind(id).bind(generation).bind(version).bind(&refresh_id).execute(&pool).await.map_err(internal_error)?;
                }
            }
            return refresh_result;
        }
    }
    async fn mark_reauth(
        &self,
        owner: &str,
        id: &str,
        generation: i64,
        refresh_owner: &str,
    ) -> ApiResult<bool> {
        let pool = self.pool()?;
        let mut tx = pool.begin().await.map_err(internal_error)?;
        let changed=query("UPDATE user_llm_connections SET status='reauth_required',credentials_encrypted=NULL,token_expires_at=NULL,generation=generation+1,refresh_owner=NULL,refresh_until=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=? AND generation=? AND refresh_owner=? AND status='connected'")
            .bind(owner).bind(id).bind(generation).bind(refresh_owner).execute(&mut *tx).await.map_err(internal_error)?;
        if changed.rows_affected() == 1 {
            query("UPDATE user_llm_models SET is_active=0,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
                .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        }
        tx.commit().await.map_err(internal_error)?;
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
        self.catalog(&self.binding(owner, id).await?).await
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
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| internal_error("OpenAI subscription catalog is invalid"))?;
        let mut items = Vec::new();
        for model in models.iter().filter(|model| model["visibility"] == "list") {
            let slug = model["slug"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 255)
                .ok_or_else(|| {
                    internal_error("OpenAI subscription catalog has an invalid model slug")
                })?;
            items.push(
                json!({"slug":slug,"display_name":model["display_name"].as_str().unwrap_or(slug)}),
            );
        }
        Ok(json!({"connection_id":binding.connection_id(),"items":items}))
    }
    pub async fn probe(
        self: &Arc<Self>,
        owner: &str,
        id: &str,
        model: &str,
    ) -> ApiResult<ExecutionCredential> {
        let binding = self.binding(owner, id).await?;
        let catalog = self.catalog(&binding).await?;
        if !catalog["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["slug"] == model))
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "chatgpt_model_unavailable",
                "Choose a model from this connection's current catalog",
            ));
        }
        let body = astra_core::model_wire::responses::request(
            model,
            &[json!({"role":"user","content":"Reply briefly with OK."})],
            None,
            id,
            true,
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
        let output = astra_core::model_wire::responses::decode_sse(text).map_err(|_| {
            fail(
                StatusCode::BAD_GATEWAY,
                "chatgpt_model_check_failed",
                "Subscription model check did not complete",
            )
        })?;
        if output.text.trim().is_empty() {
            return Err(fail(
                StatusCode::BAD_GATEWAY,
                "chatgpt_model_check_failed",
                "Subscription model check returned no text",
            ));
        }
        Ok(binding)
    }
    async fn disconnect(&self, owner: &str, id: &str) -> ApiResult<Value> {
        let pool = self.pool()?;
        let mut tx = pool.begin().await.map_err(internal_error)?;
        let row=query("SELECT status,client_id,credentials_encrypted,generation,revocation_confirmed FROM user_llm_connections WHERE user_id=? AND connection_id=? FOR UPDATE")
            .bind(owner).bind(id).fetch_optional(&mut *tx).await.map_err(internal_error)?.ok_or_else(not_found)?;
        let encrypted: Option<String> = row
            .try_get("credentials_encrypted")
            .map_err(internal_error)?;
        if row.try_get::<String, _>("status").map_err(internal_error)? == "disconnected"
            && encrypted.is_none()
        {
            let confirmed = row
                .try_get::<Option<i16>, _>("revocation_confirmed")
                .map_err(internal_error)?
                == Some(1);
            tx.commit().await.map_err(internal_error)?;
            return Ok(
                json!({"connection_id":id,"status":"disconnected","remote_revocation_confirmed":confirmed}),
            );
        }
        let generation: i64 = row.try_get("generation").map_err(internal_error)?;
        query("UPDATE user_llm_connections SET status='disconnected',credentials_encrypted=NULL,token_expires_at=NULL,generation=generation+1,refresh_owner=NULL,refresh_until=NULL,revocation_confirmed=NULL,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
            .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        query("UPDATE user_llm_authorizations SET status='cancelled',verifier_encrypted=NULL WHERE user_id=? AND connection_id=? AND status IN ('pending','exchanging')")
            .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        query("UPDATE user_llm_models SET is_active=0,updated_at=NOW(6) WHERE user_id=? AND connection_id=?")
            .bind(owner).bind(id).execute(&mut *tx).await.map_err(internal_error)?;
        tx.commit().await.map_err(internal_error)?;
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            let key = (owner.to_owned(), id.to_owned());
            if let Some(connection) = active.get_mut(&key) {
                for (_, version, cancel) in &connection.requests {
                    if *version <= generation {
                        cancel.cancel();
                    }
                }
                connection
                    .requests
                    .retain(|(_, version, _)| *version > generation);
                if connection.requests.is_empty() {
                    active.remove(&key);
                }
            }
        }
        let confirmed = if let Some(encrypted) = encrypted {
            self.revoke(
                &row.try_get::<String, _>("client_id")
                    .map_err(internal_error)?,
                &encrypted,
            )
            .await
            .is_ok()
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

pub async fn bootstrap(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    let row = query("SELECT IS_NULLABLE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'user_llm_models' AND COLUMN_NAME = 'api_key_encrypted'")
        .fetch_one(pool).await?;
    if row.try_get::<String, _>("IS_NULLABLE")? != "YES" {
        query("ALTER TABLE user_llm_models MODIFY COLUMN api_key_encrypted TEXT NULL")
            .execute(pool)
            .await?;
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'user_llm_models' AND COLUMN_NAME = 'connection_id'")
        .fetch_one(pool).await?;
    if count == 0
        && let Err(error) =
            query("ALTER TABLE user_llm_models ADD COLUMN connection_id VARCHAR(64) NULL")
                .execute(pool)
                .await
    {
        let exists:i64=sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='user_llm_models' AND COLUMN_NAME='connection_id'").fetch_one(pool).await?;
        if exists != 1 {
            return Err(error);
        }
    }
    Ok(())
}
