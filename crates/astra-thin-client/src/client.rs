//! HTTP client: dispatch to cloud API, consume SSE streams.

use std::pin::Pin;
use std::time::Duration;

use astra_server_types::{
    WORK_API_MAJOR, WORK_API_MAJOR_HEADER, WorkBranchAttachRequestV1,
    WorkBranchControlOperationRequestV1, WorkCreateRequestV1, WorkSessionBindingResponseV1,
    WorkTurnRequestV1,
};
use astra_sync_protocol::{
    SYNC_OUTBOX_SIGNATURE_HEADER, SyncOutboxAck, sync_outbox_request_signature,
};
use async_stream::stream;
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, Response, Url,
    header::{self, HeaderMap, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::edge::ASTRA_EDGE_ID_HEADER;
use crate::error::ThinClientError;
use crate::paths;
use crate::protocol::{
    ApprovalRespondRequest, ChatStreamRequest, EdgeHeartbeatRequest, EdgeHeartbeatResponse,
    EdgeRegisterRequest, ProviderInteractionRespondRequest, RunUserIntentRequest,
    RunUserIntentResponse, SessionCreateRequest, SessionTranscriptPage, SessionTranscriptReadScope,
    SessionUpdateRequest, StreamEvent, ToolResultRequest, UserPromptRespondRequest,
};
use crate::sse::SseParser;
use crate::work::WorkTaskGraphPageV2;

const HTTP_STREAM_CONNECT_TIMEOUT_SECS: u64 = 60;
const AUTHED_TEXT_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Edge callbacks are control-plane acknowledgements, not long-lived streams.
/// A peer that accepts bytes but never returns response headers must not stop
/// the SSE consumer indefinitely. Server handlers are idempotent for an
/// identical request, so one immediate transport retry is safe.
const CONTROL_CALLBACK_TIMEOUT_SECS: u64 = 10;
const CONTROL_CALLBACK_ATTEMPTS: usize = 2;
const HEALTH_REQUEST_TIMEOUT_SECS: u64 = 10;

fn stream_event_is_terminal(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TurnComplete { .. }
            | StreamEvent::RunFinished { .. }
            | StreamEvent::RunCancelled { .. }
            | StreamEvent::RunError { .. }
            | StreamEvent::RunInterrupted { .. }
            | StreamEvent::RunPaused { .. }
            | StreamEvent::RunWaiting { .. }
            | StreamEvent::Done { .. }
            | StreamEvent::Error {
                retryable: false,
                ..
            }
    )
}

#[cfg(test)]
thread_local! {
    /// Test override: when `Some(ms)`, `sleep_between_attempts` uses this flat
    /// value instead of the real `delay_secs`. Lets retry-logic tests run in
    /// <100ms instead of waiting for real backoffs. Production ignores this.
    static TEST_RETRY_SLEEP_OVERRIDE_MS: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
    /// Test probe: the last retry delay selected for developer-loop admission.
    /// wait for. Recorded regardless of the sleep override, so tests can
    /// assert the policy (`Retry-After`, exponential) without relying on
    /// wall-clock timing.
    static TEST_LAST_RETRY_SLEEP_SECS: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
}

/// Sleep `delay_secs` unless a `#[cfg(test)]` TLS override shortens it.
/// Always records the *requested* delay to `TEST_LAST_RETRY_SLEEP_SECS` in
/// test builds so assertions can inspect policy without relying on real time.
async fn sleep_between_attempts(delay_secs: u64) {
    #[cfg(test)]
    {
        TEST_LAST_RETRY_SLEEP_SECS.with(|c| *c.borrow_mut() = Some(delay_secs));
        if let Some(ms) = TEST_RETRY_SLEEP_OVERRIDE_MS.with(|c| *c.borrow()) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            return;
        }
    }
    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
}

/// Parse the `Retry-After` header value into seconds.
/// Supports integer seconds format; ignores HTTP-date format.
/// Clamps to [1, 120] seconds. Returns `None` on missing/unparseable.
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(secs.clamp(1, 120))
}

fn http_stream_connect_timeout() -> Duration {
    Duration::from_secs(HTTP_STREAM_CONNECT_TIMEOUT_SECS)
}

fn authed_text_request_timeout() -> Duration {
    Duration::from_secs(AUTHED_TEXT_REQUEST_TIMEOUT_SECS)
}

fn base_is_loopback(base: &Url) -> bool {
    let Some(host) = base.host_str() else {
        return false;
    };
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .to_ascii_lowercase()
            .strip_suffix(".localhost")
            .is_some_and(|prefix| !prefix.is_empty())
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn client_builder_for_base(base: &Url) -> reqwest::ClientBuilder {
    let builder = Client::builder();
    if base_is_loopback(base) {
        // Loopback is a process-local control-plane boundary, never an
        // outbound destination. Bypass inherited proxy variables even when
        // NO_PROXY is absent so local Astra servers and test harnesses cannot
        // be redirected to an infrastructure proxy.
        builder.no_proxy()
    } else {
        // Remote Astra endpoints retain reqwest's environment-aware proxy
        // behavior, including HTTP(S)_PROXY and NO_PROXY.
        builder
    }
}

fn streaming_http_client(base: &Url) -> Result<Client, reqwest::Error> {
    // Auto-decompression is disabled so that Content-Encoding headers on SSE
    // responses do not cause reqwest to buffer chunks before handing them to
    // the caller, which would break streaming.
    //
    // Remote endpoints remain proxy-aware for OpenShell sandboxes. Loopback
    // endpoints are explicitly direct so inherited proxy variables cannot
    // hijack process-local control-plane traffic.
    client_builder_for_base(base)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .connect_timeout(http_stream_connect_timeout())
        .build()
}

/// Stateless façade over the astra HTTP API (thin client).
#[derive(Debug, Clone)]
pub struct ThinClient {
    http: Client,
    /// Separate client for SSE streams — auto-decompression disabled to prevent
    /// "error decoding response body" when the server sends Content-Encoding on
    /// a streaming response.
    http_stream: Client,
    base: Url,
    /// Default bearer when call sites omit per-request token (optional).
    bearer_token: Option<String>,
}

impl ThinClient {
    /// `base` is the server origin, e.g. `https://api.example.com` (trailing slash optional).
    pub fn new(base: &str, bearer_token: Option<String>) -> Result<Self, ThinClientError> {
        let base =
            Url::parse(base).map_err(|_| ThinClientError::InvalidBaseUrl(base.to_string()))?;
        let http = client_builder_for_base(&base).build()?;
        // Bound TCP/TLS handshakes while leaving response body streaming uncapped.
        // Chat turns can run for many minutes after the connection is established.
        let http_stream = streaming_http_client(&base)?;
        Ok(Self {
            http,
            http_stream,
            base,
            bearer_token,
        })
    }

    /// Shared `reqwest::Client` (TLS / proxy policy aligned with thin API). For optional in-library LLM tool surface and ad-hoc calls to other origins (e.g. Memoria health).
    pub fn http_client(&self) -> &Client {
        &self.http
    }

    /// Base URL without trailing slash — matches legacy `{base}` string in CLI.
    pub fn api_origin(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_string()
    }

    fn url(&self, path: &str) -> Result<Url, ThinClientError> {
        self.base
            .join(path.trim_start_matches('/'))
            .map_err(|_| ThinClientError::InvalidBaseUrl(path.to_string()))
    }

    /// `Authorization: Bearer …` for raw `reqwest` call sites.
    pub fn bearer_headers(token: &str) -> Result<HeaderMap, ThinClientError> {
        let mut h = HeaderMap::new();
        let value = format!("Bearer {token}");
        let hv = HeaderValue::from_str(&value).map_err(|_| ThinClientError::InvalidAuthHeader)?;
        h.insert(header::AUTHORIZATION, hv);
        Ok(h)
    }

    fn auth_headers_for(&self, token_override: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        let token = token_override.or(self.bearer_token.as_deref());
        if let Some(t) = token
            && let Ok(v) = HeaderValue::from_str(&format!("Bearer {t}"))
        {
            h.insert(header::AUTHORIZATION, v);
        }
        h
    }

    fn work_api_headers(token: &str) -> Result<HeaderMap, ThinClientError> {
        let mut headers = Self::bearer_headers(token)?;
        headers.insert(
            WORK_API_MAJOR_HEADER,
            HeaderValue::from_static(WORK_API_MAJOR),
        );
        Ok(headers)
    }

    fn resolved_bearer_token<'a>(&'a self, token_override: Option<&'a str>) -> Option<&'a str> {
        token_override.or(self.bearer_token.as_deref())
    }

    async fn text_or_api(resp: Response) -> Result<String, ThinClientError> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ThinClientError::Api { status, body: text });
        }
        Ok(text)
    }

    async fn json_or_error(resp: Response) -> Result<Value, ThinClientError> {
        let text = Self::text_or_api(resp).await?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// Callback handlers commit the decision before replying and callers do
    /// not consume a response payload. A successful status is therefore the
    /// acknowledgement boundary; waiting for an optional JSON body can turn
    /// an already-committed callback into a false failure when that body is
    /// delayed or dropped.
    async fn callback_ack_or_error(resp: Response) -> Result<Value, ThinClientError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(Value::Null);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ThinClientError::Api { status, body })
    }

    async fn typed_json_or_error<T: DeserializeOwned>(
        resp: Response,
    ) -> Result<T, ThinClientError> {
        let value = Self::json_or_error(resp).await?;
        serde_json::from_value(value).map_err(Into::into)
    }

    // ── Bearer-authenticated CRUD (admin routes, any path under API origin) ─

    /// `GET` with `Authorization: Bearer` and optional query pairs.
    pub async fn get_bearer_path_query_text(
        &self,
        token: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(path)?;
        let mut req = self.http.get(url).headers(Self::bearer_headers(token)?);
        if !query.is_empty() {
            req = req.query(query);
        }
        let resp = req.send().await?;
        Self::text_or_api(resp).await
    }

    /// `GET` with `Authorization: Bearer` and optional query pairs, decoding JSON.
    pub async fn get_bearer_path_query_json<T: DeserializeOwned>(
        &self,
        token: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ThinClientError> {
        let url = self.url(path)?;
        let mut req = self.http.get(url).headers(Self::bearer_headers(token)?);
        if !query.is_empty() {
            req = req.query(query);
        }
        let resp = req.send().await?;
        Self::typed_json_or_error(resp).await
    }

    /// `POST` JSON with bearer.
    pub async fn post_bearer_path_json_text(
        &self,
        token: &str,
        path: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// `POST` JSON with an optional bearer token.
    pub async fn post_path_json_text(
        &self,
        path: &str,
        body: &Value,
        bearer_override: Option<&str>,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path)?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// `PUT` with bearer, JSON body, returns response text.
    pub async fn put_bearer_path_json_text(
        &self,
        token: &str,
        path: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path)?;
        let resp = self
            .http
            .put(url)
            .headers(Self::bearer_headers(token)?)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// `POST` with bearer, empty body.
    pub async fn post_bearer_path_empty_text(
        &self,
        token: &str,
        path: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// `POST` with bearer, empty body, decoding JSON.
    pub async fn post_bearer_path_empty_json<T: DeserializeOwned>(
        &self,
        token: &str,
        path: &str,
    ) -> Result<T, ThinClientError> {
        let url = self.url(path)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::typed_json_or_error(resp).await
    }

    /// `DELETE` with bearer.
    pub async fn delete_bearer_path_text(
        &self,
        token: &str,
        path: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path)?;
        let resp = self
            .http
            .delete(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Public: health & auth (no bearer unless noted) ─────────────────────

    pub async fn get_health_text(&self) -> Result<String, ThinClientError> {
        let url = self.url(paths::HEALTH)?;
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(HEALTH_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// GET an absolute URL on another origin (e.g. Memoria `/health`). Uses the same HTTP client as API calls.
    pub async fn get_url(&self, url: &str) -> Result<Response, ThinClientError> {
        let u = Url::parse(url).map_err(|_| ThinClientError::InvalidBaseUrl(url.to_string()))?;
        Ok(self.http.get(u).send().await?)
    }

    pub async fn post_auth_register_json(&self, body: &Value) -> Result<String, ThinClientError> {
        let url = self.url(paths::AUTH_REGISTER)?;
        let resp = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_auth_login_json(&self, body: &Value) -> Result<String, ThinClientError> {
        let url = self.url(paths::AUTH_LOGIN)?;
        let resp = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_auth_memoria_json(&self, body: &Value) -> Result<String, ThinClientError> {
        let url = self.url(paths::AUTH_MEMORIA)?;
        let resp = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_auth_me_text(&self, token: &str) -> Result<String, ThinClientError> {
        let url = self.url(paths::AUTH_ME)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_auth_me_text_timeout(
        &self,
        token: &str,
        timeout: Duration,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::AUTH_ME)?;
        Ok(self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .timeout(timeout)
            .send()
            .await?)
    }

    pub async fn post_auth_refresh_json(&self, body: &Value) -> Result<String, ThinClientError> {
        let url = self.url(paths::AUTH_REFRESH)?;
        let resp = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_auth_logout_json(&self, body: &Value) -> Result<String, ThinClientError> {
        let url = self.url(paths::AUTH_LOGOUT)?;
        let resp = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Models ─────────────────────────────────────────────────────────────

    pub async fn get_models_response_timeout(
        &self,
        token: &str,
        timeout: Duration,
    ) -> Result<Response, ThinClientError> {
        self.get_models_page_response_timeout(token, timeout, None)
            .await
    }

    /// Fetch one authoritative model-catalog page. The cursor is the complete
    /// `(provider, model_name, offering_id)` seek tuple; partial cursors are
    /// rejected by the server rather than silently changing pagination mode.
    pub async fn get_models_page_response_timeout(
        &self,
        token: &str,
        timeout: Duration,
        cursor: Option<(&str, &str, &str)>,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::MODELS)?;
        tracing::debug!(
            target: "astra_cli::http_proxy",
            url = %url,
            http_proxy_set = std::env::var("HTTP_PROXY").or_else(|_| std::env::var("http_proxy")).is_ok(),
            no_proxy_set = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).is_ok(),
            token_len = token.len(),
            "get_models_response_timeout: sending GET /models via self.http (proxy-aware client)"
        );
        let mut request = self
            .http
            .get(url.clone())
            .headers(Self::bearer_headers(token)?)
            .timeout(timeout);
        if let Some((provider, model_name, model_id)) = cursor {
            request = request.query(&[
                ("after_provider", provider),
                ("after_name", model_name),
                ("after_offering_id", model_id),
            ]);
        }
        let result = request.send().await;
        match &result {
            Ok(resp) => tracing::debug!(
                target: "astra_cli::http_proxy",
                url = %url,
                status = %resp.status(),
                "get_models_response_timeout: response received"
            ),
            Err(e) => tracing::warn!(
                target: "astra_cli::http_proxy",
                url = %url,
                error = %e,
                "get_models_response_timeout: request failed"
            ),
        }
        Ok(result?)
    }

    pub async fn get_model_access_response_timeout(
        &self,
        token: &str,
        timeout: Duration,
    ) -> Result<Response, ThinClientError> {
        self.get_model_access_page_response_timeout(token, timeout, None)
            .await
    }

    pub async fn get_model_access_page_response_timeout(
        &self,
        token: &str,
        timeout: Duration,
        cursor: Option<(&str, &str, &str)>,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::MODEL_ACCESS)?;
        let mut request = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .timeout(timeout);
        if let Some((provider, model_name, model_id)) = cursor {
            request = request.query(&[
                ("after_provider", provider),
                ("after_name", model_name),
                ("after_offering_id", model_id),
            ]);
        }
        Ok(request.send().await?)
    }

    pub async fn get_model_text(
        &self,
        token: &str,
        model_name: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::model(model_name))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Sessions ───────────────────────────────────────────────────────────

    pub async fn post_sessions_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::SESSIONS)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_sessions_query_text(
        &self,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::SESSIONS)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .query(query)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_session_text(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::session(session_id))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_session_close_text(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::session_close(session_id))?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_session_cancel_text(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::session_cancel(session_id))?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn delete_session_text(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::session(session_id))?;
        let resp = self
            .http
            .delete(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_session_artifact_latest_text(
        &self,
        token: &str,
        session_id: &str,
        artifact_kind: &str,
    ) -> Result<String, ThinClientError> {
        let path = paths::session_artifact_latest(session_id, artifact_kind).ok_or_else(|| {
            ThinClientError::InvalidInput(format!("invalid artifact_kind: {artifact_kind}"))
        })?;
        let url = self.url(&path)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn download_session_artifact(
        &self,
        token: &str,
        session_id: &str,
        artifact_id: &str,
    ) -> Result<(Vec<u8>, Option<String>), ThinClientError> {
        let path = paths::session_artifact_download(session_id, artifact_id).ok_or_else(|| {
            ThinClientError::InvalidInput(format!("invalid artifact_id: {artifact_id}"))
        })?;
        let url = self.url(&path)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        let status = resp.status();
        let filename = attachment_filename(resp.headers());
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(ThinClientError::Api {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok((bytes.to_vec(), filename))
    }

    pub async fn post_session_replay_json(
        &self,
        token: &str,
        session_id: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::session_replay(session_id))?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_session_replay_compare_text(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::session_replay_compare(session_id))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Skills ───────────────────────────────────────────────────────────────

    pub async fn get_skills_query_text(
        &self,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::SKILLS)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .query(query)
            .timeout(authed_text_request_timeout())
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_skill_query_text(
        &self,
        token: &str,
        skill_id: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::skill(skill_id))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .query(query)
            .timeout(authed_text_request_timeout())
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_skills_status_query_text(
        &self,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::SKILLS_STATUS)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .query(query)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Memory proxy ───────────────────────────────────────────────────────

    pub async fn post_memory_store_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::MEMORY_STORE)?;
        Ok(self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?)
    }

    pub async fn post_memory_search_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::MEMORY_SEARCH)?;
        Ok(self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?)
    }

    pub async fn post_memory_purge_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::MEMORY_PURGE)?;
        Ok(self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?)
    }

    pub async fn post_memory_retrieve_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::MEMORY_RETRIEVE)?;
        Ok(self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?)
    }
    // ── Context snapshots ──────────────────────────────────────────────────
    /// `POST /v1/chat/completions` — governed non-streaming model invocation.
    ///
    /// The typed request carries an Offering selection and causal scope, never
    /// provider credentials. The typed response keeps SDK consumers from
    /// duplicating response-shape parsing.
    pub async fn post_completions(
        &self,
        token: &str,
        request: &astra_server_types::CompletionRequest,
    ) -> Result<astra_server_types::CompletionResponse, ThinClientError> {
        let url = self.url(paths::COMPLETIONS)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .timeout(
                std::time::Duration::from_millis(request.timeout_ms)
                    .saturating_add(std::time::Duration::from_secs(5)),
            )
            .json(request)
            .send()
            .await?;
        Self::typed_json_or_error(resp).await
    }

    // ── Work ───────────────────────────────────────────────────────────────

    /// Atomically create one Work with a server-owned branch and conversation.
    pub async fn post_work(
        &self,
        token: &str,
        request: &WorkCreateRequestV1,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::WORKS)?;
        let response = self
            .http
            .post(url)
            .headers(Self::work_api_headers(token)?)
            .json(request)
            .send()
            .await?;
        Self::json_or_error(response).await
    }

    /// Read one bounded public Work observation.
    pub async fn get_work(&self, token: &str, work_id: &str) -> Result<Value, ThinClientError> {
        let path = paths::work(work_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid work_id".into()))?;
        let response = self
            .http
            .get(self.url(&path)?)
            .headers(Self::work_api_headers(token)?)
            .send()
            .await?;
        Self::json_or_error(response).await
    }

    /// Resolve one already-known session to the public Work branch that owns
    /// it. A session that is not Work-backed is a typed 404, never an empty or
    /// guessed binding.
    pub async fn get_work_session_binding(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<WorkSessionBindingResponseV1, ThinClientError> {
        let path = paths::work_session_binding(session_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid session_id".into()))?;
        let response = self
            .http
            .get(self.url(&path)?)
            .headers(Self::work_api_headers(token)?)
            .send()
            .await?;
        Self::typed_json_or_error(response).await
    }

    /// Promote an existing active conversation into canonical Work. The
    /// server preserves the session identity and rejects promotion while a
    /// run owns the session, so clients never need a local binding authority.
    pub async fn post_work_session_binding(
        &self,
        token: &str,
        session_id: &str,
        request: &WorkCreateRequestV1,
    ) -> Result<Value, ThinClientError> {
        let path = paths::work_session_binding(session_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid session_id".into()))?;
        let response = self
            .http
            .post(self.url(&path)?)
            .headers(Self::work_api_headers(token)?)
            .json(request)
            .send()
            .await?;
        Self::json_or_error(response).await
    }

    /// Read one bounded Task Graph page for a Work branch. Continuation calls
    /// must carry the exact graph revision returned by the first page so a
    /// client cannot splice pages from concurrent replans.
    pub async fn get_work_branch_task_graph_page(
        &self,
        token: &str,
        work_id: &str,
        branch_id: &str,
        graph_revision: Option<i64>,
        item_offset: u16,
        dependency_offset: u16,
    ) -> Result<WorkTaskGraphPageV2, ThinClientError> {
        if (item_offset > 0 || dependency_offset > 0) && graph_revision.is_none() {
            return Err(ThinClientError::InvalidInput(
                "Task Graph continuation requires graph_revision".into(),
            ));
        }
        if graph_revision.is_some_and(|revision| revision <= 0) {
            return Err(ThinClientError::InvalidInput(
                "invalid Task Graph revision".into(),
            ));
        }
        let path = paths::work_branch_task_graph(work_id, branch_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid work_id or branch_id".into()))?;
        let mut query = Vec::with_capacity(3);
        if let Some(revision) = graph_revision {
            query.push(("graph_revision", revision.to_string()));
        }
        if item_offset > 0 {
            query.push(("item_offset", item_offset.to_string()));
        }
        if dependency_offset > 0 {
            query.push(("dependency_offset", dependency_offset.to_string()));
        }
        let response = self.http.get(self.url(&path)?);
        let response = if query.is_empty() {
            response
        } else {
            response.query(&query)
        }
        .headers(Self::work_api_headers(token)?)
        .send()
        .await?;
        let page: WorkTaskGraphPageV2 = Self::typed_json_or_error(response).await?;
        page.validate().map_err(|reason| {
            ThinClientError::Json(<serde_json::Error as serde::de::Error>::custom(reason))
        })?;
        Ok(page)
    }

    /// Establish a durable attachment to the current Work branch head.
    pub async fn post_work_branch_attachment(
        &self,
        token: &str,
        work_id: &str,
        branch_id: &str,
        request: &WorkBranchAttachRequestV1,
    ) -> Result<Value, ThinClientError> {
        let path = paths::work_branch_attachments(work_id, branch_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid work_id or branch_id".into()))?;
        let response = self
            .http
            .post(self.url(&path)?)
            .headers(Self::work_api_headers(token)?)
            .json(request)
            .send()
            .await?;
        Self::json_or_error(response).await
    }

    /// Submit one typed controller acquire/release/takeover operation.
    pub async fn post_work_branch_control_operation(
        &self,
        token: &str,
        work_id: &str,
        branch_id: &str,
        request: &WorkBranchControlOperationRequestV1,
    ) -> Result<Value, ThinClientError> {
        let path = paths::work_branch_control_operations(work_id, branch_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid work_id or branch_id".into()))?;
        let response = self
            .http
            .post(self.url(&path)?)
            .headers(Self::work_api_headers(token)?)
            .json(request)
            .send()
            .await?;
        Self::json_or_error(response).await
    }

    /// Release one exact read-only attachment after any controller authority
    /// has been released through a control operation.
    pub async fn delete_work_branch_attachment(
        &self,
        token: &str,
        work_id: &str,
        branch_id: &str,
        attachment_id: &str,
    ) -> Result<(), ThinClientError> {
        let path =
            paths::work_branch_attachment(work_id, branch_id, attachment_id).ok_or_else(|| {
                ThinClientError::InvalidInput("invalid work_id, branch_id, or attachment_id".into())
            })?;
        let response = self
            .http
            .delete(self.url(&path)?)
            .headers(Self::work_api_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(response).await.map(|_| ())
    }

    /// Start one server-owned Work turn and return its SSE response. The caller
    /// may stream edge-tool callbacks while the response body remains open.
    pub async fn post_work_branch_turn(
        &self,
        token: &str,
        work_id: &str,
        branch_id: &str,
        request: &WorkTurnRequestV1,
    ) -> Result<Response, ThinClientError> {
        let path = paths::work_branch_turns(work_id, branch_id)
            .ok_or_else(|| ThinClientError::InvalidInput("invalid work_id or branch_id".into()))?;
        let mut headers = Self::work_api_headers(token)?;
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        Ok(self
            .http_stream
            .post(self.url(&path)?)
            .headers(headers)
            .json(request)
            .send()
            .await?)
    }

    // ── Reflect / decision trace ─────────────────────────────────────────────

    /// `path_with_query` is relative to origin, e.g. `chat/session/sid/reflect?topic=execution&facet=trace`.
    pub async fn get_authed_path_text(
        &self,
        token: &str,
        path_with_query: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path_with_query)?;
        // Bounded request timeout: this helper fetches finite text responses,
        // unlike chat turn SSE streams.
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .timeout(authed_text_request_timeout())
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Chat turn (SSE) ─────────────────────────────────────────────────────

    /// Start one Server-owned developer loop and return its raw SSE response.
    ///
    /// The response remains raw because terminal clients execute typed edge
    /// callbacks while the same HTTP body is open. The Server, not the client,
    /// owns model rounds, continuation policy, and terminalization.
    pub async fn post_developer_loop(
        &self,
        token: &str,
        payload: &Value,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::CHAT_STREAM)?;
        tracing::debug!(
            target: "astra_cli::http_proxy",
            url = %url,
            "starting Server-owned developer loop"
        );
        let mut headers = Self::bearer_headers(token)?;
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        let response = self
            .http_stream
            .post(url)
            .headers(headers)
            .json(payload)
            .send()
            .await?;
        if response.status().is_success() {
            let actual = response
                .headers()
                .get(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("missing");
            if actual != astra_server_types::AGENT_INTERACTION_API_MAJOR {
                return Err(ThinClientError::IncompatibleRuntime {
                    expected: astra_server_types::AGENT_INTERACTION_API_MAJOR.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        Ok(response)
    }

    /// Retry Server-owned loop admission on 429 and transport errors.
    ///
    /// A retry is safe only before response headers arrive. Once the response
    /// is returned, durable run identity and stream reconciliation belong to
    /// the Server lifecycle.
    pub async fn post_developer_loop_retry_429(
        &self,
        token: &str,
        payload: &Value,
        max_attempts: u32,
        quiet: bool,
    ) -> Result<Response, ThinClientError> {
        let mut last_err: Option<ThinClientError> = None;
        for attempt in 0..max_attempts {
            match self.post_developer_loop(token, payload).await {
                Ok(resp) => {
                    if resp.status().as_u16() == 429 && attempt + 1 < max_attempts {
                        let delay_secs =
                            parse_retry_after(resp.headers()).unwrap_or(2u64 << attempt);
                        if !quiet {
                            tracing::warn!(
                                target: "astra.thin_client",
                                status = 429u16,
                                delay_secs,
                                attempt = attempt + 1,
                                max_attempts,
                                "developer loop admission rate limited, retrying"
                            );
                            eprintln!("  ⏳ Rate limited (429), retrying in {delay_secs}s…");
                        }
                        sleep_between_attempts(delay_secs).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    if attempt + 1 < max_attempts && e.is_transport() {
                        let delay_secs = 1u64 << attempt;
                        if !quiet {
                            tracing::warn!(
                                target: "astra.thin_client",
                                error = %e,
                                delay_secs,
                                attempt = attempt + 1,
                                max_attempts,
                                "developer loop admission transport error, retrying"
                            );
                            eprintln!("  ⏳ Transport error, retrying in {delay_secs}s… ({e})");
                        }
                        sleep_between_attempts(delay_secs).await;
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| ThinClientError::SseParse("retry exhausted".into())))
    }

    /// `POST /chat/stream` — yields classified SSE events.
    pub fn chat_stream(
        &self,
        body: &ChatStreamRequest,
        bearer_override: Option<&str>,
    ) -> impl Stream<Item = Result<StreamEvent, ThinClientError>> + Send + '_ {
        let url = match self.url(paths::CHAT_STREAM) {
            Ok(u) => u,
            Err(e) => {
                return stream! {
                    yield Err(e);
                }
                .boxed();
            }
        };
        let req = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body);
        let fut = async move {
            let resp = req.send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(ThinClientError::SseParse(format!("HTTP {status}: {text}")));
            }
            Ok(resp)
        };

        stream! {
            let resp = match fut.await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let mut parser = SseParser::new();
            let mut byte_stream = resp.bytes_stream();
            let mut saw_terminal = false;
            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        if saw_terminal {
                            return;
                        }
                        yield Err(e.into());
                        return;
                    }
                };
                match parser.push_bytes(&chunk) {
                    Ok(evs) => {
                        for ev in evs {
                            saw_terminal |= stream_event_is_terminal(&ev);
                            yield Ok(ev);
                        }
                    }
                    Err(e) => {
                        if saw_terminal {
                            return;
                        }
                        yield Err(e);
                        return;
                    }
                }
            }
            match parser.finish() {
                Ok(evs) => {
                    for ev in evs {
                        saw_terminal |= stream_event_is_terminal(&ev);
                        yield Ok(ev);
                    }
                }
                Err(e) => {
                    if saw_terminal {
                        return;
                    }
                    yield Err(e);
                    return;
                }
            }
            if !saw_terminal {
                yield Err(ThinClientError::SseParse(
                    "SSE stream ended before a terminal event (run_finished, turn_complete, or interruption)"
                        .to_string(),
                ));
            }
        }
        .boxed()
    }

    pub async fn chat_stream_collect(
        &self,
        body: &ChatStreamRequest,
        bearer_override: Option<&str>,
    ) -> Result<Vec<StreamEvent>, ThinClientError> {
        let mut out = Vec::new();
        let mut stream = self.chat_stream(body, bearer_override);
        let mut s = Pin::new(&mut stream);
        while let Some(item) = s.next().await {
            out.push(item?);
        }
        Ok(out)
    }

    /// `POST /sessions` (typed body)
    pub async fn create_session(
        &self,
        bearer_override: Option<&str>,
        body: &SessionCreateRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::SESSIONS)?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn get_session(
        &self,
        bearer_override: Option<&str>,
        session_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::session(session_id))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `GET /sessions/{session_id}/runs` — bounded durable run-tree snapshot.
    pub async fn get_session_run_tree(
        &self,
        bearer_override: Option<&str>,
        session_id: &str,
        limit: u32,
    ) -> Result<astra_server_types::SessionRunTreeSnapshot, ThinClientError> {
        let url = self.url(&paths::session_runs(session_id))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .query(&[("limit", limit)])
            .send()
            .await?;
        Self::typed_json_or_error(resp).await
    }

    /// Durable transcript projection with an explicit server-owned scope.
    pub async fn get_session_transcript(
        &self,
        bearer_override: Option<&str>,
        session_id: &str,
        scope: SessionTranscriptReadScope<'_>,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<SessionTranscriptPage, ThinClientError> {
        let url = self.url(&paths::session_transcript(session_id))?;
        let mut query = vec![("limit", limit.to_string())];
        match scope {
            SessionTranscriptReadScope::Session => {}
            SessionTranscriptReadScope::RootConversation => {
                query.push(("scope", "root_conversation".to_string()));
            }
            SessionTranscriptReadScope::Run(run_id) => {
                query.push(("run_id", run_id.to_string()));
            }
        }
        if let Some(before_seq) = before_seq {
            query.push(("before_seq", before_seq.to_string()));
        }
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .query(&query)
            .send()
            .await?;
        Self::typed_json_or_error(resp).await
    }

    pub async fn update_session(
        &self,
        bearer_override: Option<&str>,
        session_id: &str,
        body: &SessionUpdateRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::session(session_id))?;
        let resp = self
            .http
            .put(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body)
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn delete_session(
        &self,
        bearer_override: Option<&str>,
        session_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::session(session_id))?;
        let resp = self
            .http
            .delete(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `GET /chat/runs/{run_id}` — durable run status/metadata.
    pub async fn get_run(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run(run_id))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /chat/runs/{run_id}/intents` — guide the active durable run.
    pub async fn submit_run_user_intent(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
        body: &RunUserIntentRequest,
    ) -> Result<RunUserIntentResponse, ThinClientError> {
        let url = self.url(&paths::chat_run_user_intents(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body)
            .send()
            .await?;
        Self::typed_json_or_error(resp).await
    }

    /// `DELETE /chat/runs/{run_id}` — cancel a durable run.
    pub async fn cancel_run(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run(run_id))?;
        let resp = self
            .http
            .delete(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /chat/runs/{run_id}/pause` — pause a durable run.
    pub async fn pause_run(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_pause(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /chat/runs/{run_id}/resume` — resume a paused durable run.
    pub async fn resume_run(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_resume(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `GET /runs` — list durable runs for the current user.
    pub async fn list_runs(
        &self,
        bearer_override: Option<&str>,
        limit: u32,
        after_updated_at: Option<&str>,
        after_run_id: Option<&str>,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::RUNS)?;
        let mut query = vec![("limit", limit.to_string())];
        if let Some(after_updated_at) = after_updated_at {
            query.push(("after_updated_at", after_updated_at.to_string()));
        }
        if let Some(after_run_id) = after_run_id {
            query.push(("after_run_id", after_run_id.to_string()));
        }
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .query(&query)
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `GET /chat/runs/{run_id}/stream` — yields classified lifecycle SSE events.
    pub fn stream_run(
        &self,
        run_id: &str,
        last_index: u32,
        bearer_override: Option<&str>,
    ) -> impl Stream<Item = Result<StreamEvent, ThinClientError>> + Send + '_ {
        let url = match self.url(&paths::chat_run_stream(run_id)) {
            Ok(u) => u,
            Err(e) => {
                return stream! {
                    yield Err(e);
                }
                .boxed();
            }
        };
        let req = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .query(&[("last_index", last_index)]);
        let fut = async move {
            let resp = req.send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(ThinClientError::SseParse(format!("HTTP {status}: {text}")));
            }
            Ok(resp)
        };

        stream! {
            let resp = match fut.await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let mut parser = SseParser::new();
            let mut byte_stream = resp.bytes_stream();
            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(e.into());
                        return;
                    }
                };
                match parser.push_bytes(&chunk) {
                    Ok(evs) => {
                        for ev in evs {
                            yield Ok(ev);
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            match parser.finish() {
                Ok(evs) => {
                    for ev in evs {
                        yield Ok(ev);
                    }
                }
                Err(e) => yield Err(e),
            }
        }
        .boxed()
    }

    pub async fn stream_run_collect(
        &self,
        run_id: &str,
        last_index: u32,
        bearer_override: Option<&str>,
    ) -> Result<Vec<StreamEvent>, ThinClientError> {
        let mut out = Vec::new();
        let mut stream = self.stream_run(run_id, last_index, bearer_override);
        let mut s = Pin::new(&mut stream);
        while let Some(item) = s.next().await {
            out.push(item?);
        }
        Ok(out)
    }

    /// `POST /chat/runs/{run_id}/delegate` — dispatch a delegated sub-run plan.
    pub async fn delegate_run(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
        body: &Value,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_delegate(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body)
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `GET /chat/runs/{run_id}/delegations` — list delegated child run IDs.
    pub async fn list_run_delegations(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_delegations(run_id))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /chat/runs/{run_id}/delegations/pause` — pause delegated child runs.
    pub async fn pause_run_delegations(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
        expected_session_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_delegations_pause(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(&serde_json::json!({"expected_session_id": expected_session_id}))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /chat/runs/{run_id}/delegations/resume` — resume delegated child runs.
    pub async fn resume_run_delegations(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
        expected_session_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_delegations_resume(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(&serde_json::json!({"expected_session_id": expected_session_id}))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /events` — create or replay an idempotent automation event.
    pub async fn post_event_json(
        &self,
        bearer_override: Option<&str>,
        body: &Value,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::EVENTS)?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /sync/outbox/events` — ingest durable edge sync outbox events.
    pub async fn post_sync_outbox_event_json(
        &self,
        bearer_override: Option<&str>,
        body: &Value,
    ) -> Result<SyncOutboxAck, ThinClientError> {
        let url = self.url(paths::SYNC_OUTBOX_EVENTS)?;
        let mut headers = self.auth_headers_for(bearer_override);
        if let Some(token) = self.resolved_bearer_token(bearer_override) {
            let signature = sync_outbox_request_signature(token, body);
            let header = HeaderValue::from_str(&signature).map_err(|error| {
                ThinClientError::InvalidInput(format!(
                    "failed to encode sync-outbox signature header: {error}"
                ))
            })?;
            headers.insert(SYNC_OUTBOX_SIGNATURE_HEADER, header);
        }
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        Self::typed_json_or_error(resp).await
    }

    /// Read back one authoritative event for delivery reconciliation.
    pub async fn get_event_json(
        &self,
        bearer_override: Option<&str>,
        event_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::event(event_id))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn post_tool_result(
        &self,
        bearer_override: Option<&str>,
        edge_executor_id: Option<&str>,
        body: &ToolResultRequest,
    ) -> Result<Value, ThinClientError> {
        self.post_tool_result_with_policy(
            bearer_override,
            edge_executor_id,
            body,
            Duration::from_secs(CONTROL_CALLBACK_TIMEOUT_SECS),
            CONTROL_CALLBACK_ATTEMPTS,
        )
        .await
    }

    async fn post_tool_result_with_policy(
        &self,
        bearer_override: Option<&str>,
        edge_executor_id: Option<&str>,
        body: &ToolResultRequest,
        timeout: Duration,
        attempts: usize,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::TOOLS_RESULT)?;
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            let mut req = self
                .http
                .post(url.clone())
                .headers(self.auth_headers_for(bearer_override))
                .timeout(timeout)
                .json(body);
            if let Some(id) = edge_executor_id
                && let Ok(v) = HeaderValue::from_str(id)
            {
                req = req.header(ASTRA_EDGE_ID_HEADER, v);
            }
            match req.send().await {
                Ok(resp) => return Self::callback_ack_or_error(resp).await,
                Err(error)
                    if attempt + 1 < attempts
                        && (error.is_connect() || error.is_timeout() || error.is_request()) => {}
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("at least one callback attempt is required")
    }

    pub async fn post_approval(
        &self,
        bearer_override: Option<&str>,
        body: &ApprovalRespondRequest,
    ) -> Result<Value, ThinClientError> {
        self.post_approval_with_policy(
            bearer_override,
            body,
            Duration::from_secs(CONTROL_CALLBACK_TIMEOUT_SECS),
            CONTROL_CALLBACK_ATTEMPTS,
        )
        .await
    }

    async fn post_approval_with_policy(
        &self,
        bearer_override: Option<&str>,
        body: &ApprovalRespondRequest,
        timeout: Duration,
        attempts: usize,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::APPROVAL_RESPOND)?;
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match self
                .http
                .post(url.clone())
                .headers(self.auth_headers_for(bearer_override))
                .timeout(timeout)
                .json(body)
                .send()
                .await
            {
                Ok(resp) => return Self::callback_ack_or_error(resp).await,
                Err(error)
                    if attempt + 1 < attempts
                        && (error.is_connect() || error.is_timeout() || error.is_request()) => {}
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("at least one callback attempt is required")
    }

    /// Submit a response to a durable `ask_user` interaction.
    pub async fn post_user_prompt_response(
        &self,
        bearer_override: Option<&str>,
        body: &UserPromptRespondRequest,
    ) -> Result<Value, ThinClientError> {
        self.post_user_prompt_response_with_policy(
            bearer_override,
            body,
            Duration::from_secs(CONTROL_CALLBACK_TIMEOUT_SECS),
            CONTROL_CALLBACK_ATTEMPTS,
        )
        .await
    }

    async fn post_user_prompt_response_with_policy(
        &self,
        bearer_override: Option<&str>,
        body: &UserPromptRespondRequest,
        timeout: Duration,
        attempts: usize,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::USER_PROMPT_RESPOND)?;
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match self
                .http
                .post(url.clone())
                .headers(self.auth_headers_for(bearer_override))
                .timeout(timeout)
                .json(body)
                .send()
                .await
            {
                Ok(resp) => return Self::callback_ack_or_error(resp).await,
                Err(error)
                    if attempt + 1 < attempts
                        && (error.is_connect() || error.is_timeout() || error.is_request()) => {}
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("at least one callback attempt is required")
    }

    /// Submit a response to a durable provider interaction.
    pub async fn post_provider_interaction_response(
        &self,
        bearer_override: Option<&str>,
        body: &ProviderInteractionRespondRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::PROVIDER_INTERACTION_RESPOND)?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body)
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /agents/edge` — persist edge registry row (JWT). `edge_transport_id` → [`ASTRA_EDGE_ID_HEADER`]
    /// (transport instance); `body.edge_agent_id` is the logical agent id (often the same string).
    pub async fn post_agents_edge_register(
        &self,
        bearer_override: Option<&str>,
        edge_transport_id: Option<&str>,
        body: &EdgeRegisterRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::AGENTS_EDGE)?;
        let mut req = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body);
        if let Some(id) = edge_transport_id
            && let Ok(v) = HeaderValue::from_str(id)
        {
            req = req.header(ASTRA_EDGE_ID_HEADER, v);
        }
        let resp = req.send().await?;
        Self::json_or_error(resp).await
    }

    /// `POST /agents/edge/heartbeat` — liveness ping (must register first).
    pub async fn post_agents_edge_heartbeat(
        &self,
        bearer_override: Option<&str>,
        edge_transport_id: Option<&str>,
        body: &EdgeHeartbeatRequest,
    ) -> Result<EdgeHeartbeatResponse, ThinClientError> {
        let url = self.url(paths::AGENTS_EDGE_HEARTBEAT)?;
        let mut req = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body);
        if let Some(id) = edge_transport_id
            && let Ok(v) = HeaderValue::from_str(id)
        {
            req = req.header(ASTRA_EDGE_ID_HEADER, v);
        }
        let resp = req.send().await?;
        Self::typed_json_or_error(resp).await
    }
}

fn attachment_filename(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_DISPOSITION)?.to_str().ok()?;
    let filename = value
        .split(';')
        .map(str::trim)
        .find_map(|segment| segment.strip_prefix("filename="))?
        .trim_matches('"')
        .trim();
    if filename.is_empty() {
        return None;
    }
    // Strip control characters and cap length for safety.
    let sanitized: String = filename
        .chars()
        .filter(|c| !c.is_control())
        .take(255)
        .collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Override `sleep_between_attempts` to `ms` for the duration of a test,
    /// clearing the probe counter as it goes. Returns a guard that resets
    /// both on drop.
    fn set_test_retry_sleep_ms(ms: u64) -> impl Drop {
        TEST_RETRY_SLEEP_OVERRIDE_MS.with(|c| *c.borrow_mut() = Some(ms));
        TEST_LAST_RETRY_SLEEP_SECS.with(|c| *c.borrow_mut() = None);
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                TEST_RETRY_SLEEP_OVERRIDE_MS.with(|c| *c.borrow_mut() = None);
                TEST_LAST_RETRY_SLEEP_SECS.with(|c| *c.borrow_mut() = None);
            }
        }
        Guard
    }

    fn last_retry_sleep_secs() -> Option<u64> {
        TEST_LAST_RETRY_SLEEP_SECS.with(|c| *c.borrow())
    }

    #[tokio::test]
    async fn work_requests_share_the_canonical_wire_contract_and_version_header() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/works"))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .and(body_json(serde_json::json!({
                "request_id": "start-1",
                "goal": "Ship the Work client boundary.",
                "criteria": [{
                    "kind": "test_check",
                    "criterion_id": "tests",
                    "statement": "The boundary tests pass.",
                    "command": "cargo test -p astra-thin-client"
                }]
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "overview": {"work_id": "work-1"}
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client
            .post_work(
                "work-token",
                &WorkCreateRequestV1 {
                    request_id: "start-1".into(),
                    goal: "Ship the Work client boundary.".into(),
                    criteria: vec![astra_server_types::WorkCreateCriterionV1::TestCheck {
                        criterion_id: "tests".into(),
                        statement: "The boundary tests pass.".into(),
                        command: "cargo test -p astra-thin-client".into(),
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(
            response.pointer("/overview/work_id"),
            Some(&serde_json::json!("work-1"))
        );
    }

    #[tokio::test]
    async fn work_attachment_and_turn_keep_session_authority_server_side() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/works/work-1/branches/branch-1/attachments"))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .and(body_json(serde_json::json!({"request_id": "attach-1"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "work_id": "work-1",
                "branch_id": "branch-1",
                "attachment_id": "attachment-1"
            })))
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/works/work-1/branches/branch-1/turns"))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .and(header("accept", "text/event-stream"))
            .and(body_json(serde_json::json!({
                "request_id": "turn-1",
                "attachment_id": "attachment-1",
                "message": "Continue from canonical Work state."
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "data: {\"type\":\"work_turn_started\",\"work_id\":\"work-1\"}\n\n",
            ))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let attachment = client
            .post_work_branch_attachment(
                "work-token",
                "work-1",
                "branch-1",
                &WorkBranchAttachRequestV1 {
                    request_id: "attach-1".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            attachment.get("attachment_id"),
            Some(&serde_json::json!("attachment-1"))
        );

        let response = client
            .post_work_branch_turn(
                "work-token",
                "work-1",
                "branch-1",
                &WorkTurnRequestV1 {
                    request_id: "turn-1".into(),
                    attachment_id: "attachment-1".into(),
                    message: "Continue from canonical Work state.".into(),
                },
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert!(response.text().await.unwrap().contains("work_turn_started"));
    }

    #[tokio::test]
    async fn work_task_graph_read_is_bounded_and_revision_pinned() {
        let srv = MockServer::start().await;
        let first: Value = serde_json::from_str(include_str!(
            "../../../fixtures/contracts/work_task_graph_v2.json"
        ))
        .expect("shared Task Graph fixture");
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .respond_with(ResponseTemplate::new(200).set_body_json(first.clone()))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let graph = client
            .get_work_branch_task_graph_page("work-token", "work-1", "branch-1", None, 0, 0)
            .await
            .unwrap();
        assert_eq!(graph.basis.work_id, "work-1");

        let mut continuation = first;
        continuation["cursor"] = serde_json::json!({
            "graph_revision": 1,
            "item_offset": 1,
            "dependency_offset": 1
        });
        continuation["next_cursor"] = Value::Null;
        continuation["items"]["offset"] = serde_json::json!(1);
        continuation["items"]["entries"] = serde_json::json!([{
            "item_id": "task-b",
            "revision": 1,
            "kind": "task",
            "objective": "Implement task-b",
            "expected_result": "Verify task-b",
            "declaration_state": "active",
            "execution": {"status": "not_started", "terminal": false, "run": null},
            "delivery": {"status": "unreported", "summary": null, "blocker_kind": null, "unavailable_capabilities": []},
            "verification": {"status": "unknown", "latest_check": null}
        }]);
        continuation["dependencies"]["offset"] = serde_json::json!(1);
        continuation["dependencies"]["entries"] = serde_json::json!([]);
        Mock::given(method("GET"))
            .and(path("/v1/works/work-1/branches/branch-1/task-graph"))
            .and(query_param("graph_revision", "1"))
            .and(query_param("item_offset", "1"))
            .and(query_param("dependency_offset", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(continuation))
            .mount(&srv)
            .await;
        client
            .get_work_branch_task_graph_page("work-token", "work-1", "branch-1", Some(1), 1, 1)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn work_session_binding_is_typed_owner_transport_and_path_safe() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/works/session-bindings/session-1"))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "work_id": "work-1",
                "branch_id": "branch-1",
                "graph_revision": 7
            })))
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/works/session-bindings/session-1"))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .and(body_json(serde_json::json!({
                "request_id": "promote-1",
                "goal": "Track this conversation as Work.",
                "criteria": []
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "overview": {"work_id": "work-1"}
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let binding = client
            .get_work_session_binding("work-token", "session-1")
            .await
            .unwrap();
        assert_eq!(binding.work_id, "work-1");
        assert_eq!(binding.branch_id, "branch-1");
        assert_eq!(binding.graph_revision, 7);
        let promoted = client
            .post_work_session_binding(
                "work-token",
                "session-1",
                &WorkCreateRequestV1 {
                    request_id: "promote-1".into(),
                    goal: "Track this conversation as Work.".into(),
                    criteria: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            promoted.pointer("/overview/work_id"),
            Some(&serde_json::json!("work-1"))
        );

        let unsafe_error = client
            .get_work_session_binding("work-token", "../session")
            .await
            .expect_err("path fragments must fail before transport");
        assert!(matches!(unsafe_error, ThinClientError::InvalidInput(_)));
        let unsafe_error = client
            .post_work_session_binding(
                "work-token",
                "../session",
                &WorkCreateRequestV1 {
                    request_id: "promote-unsafe".into(),
                    goal: "Never send this request.".into(),
                    criteria: Vec::new(),
                },
            )
            .await
            .expect_err("path fragments must fail before mutation transport");
        assert!(matches!(unsafe_error, ThinClientError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn work_controller_release_and_attachment_delete_are_distinct_typed_calls() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/works/work-1/branches/branch-1/control-operations",
            ))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .and(body_json(serde_json::json!({
                "request_id": "release-1",
                "expected_branch_revision": 2,
                "expected_writer_epoch": 7,
                "expected_canonical_root_hash": "sha256:root",
                "command": {
                    "kind": "release_branch_control",
                    "attachment_id": "attachment-1"
                }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "state": "succeeded",
                "outcome": "released"
            })))
            .mount(&srv)
            .await;
        Mock::given(method("DELETE"))
            .and(path(
                "/v1/works/work-1/branches/branch-1/attachments/attachment-1",
            ))
            .and(header("authorization", "Bearer work-token"))
            .and(header(WORK_API_MAJOR_HEADER, WORK_API_MAJOR))
            .respond_with(ResponseTemplate::new(204))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let operation = client
            .post_work_branch_control_operation(
                "work-token",
                "work-1",
                "branch-1",
                &WorkBranchControlOperationRequestV1 {
                    request_id: "release-1".into(),
                    expected_branch_revision: 2,
                    expected_writer_epoch: 7,
                    expected_canonical_root_hash: Some("sha256:root".into()),
                    command: astra_server_types::WorkBranchControlCommandV1::ReleaseBranchControl {
                        attachment_id: "attachment-1".into(),
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(operation["outcome"], "released");
        client
            .delete_work_branch_attachment("work-token", "work-1", "branch-1", "attachment-1")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn work_resource_ids_fail_before_transport_when_they_are_path_fragments() {
        let client = ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let error = client
            .get_work("token", "work/other")
            .await
            .expect_err("path fragments must not reach transport");
        assert!(matches!(error, ThinClientError::InvalidInput(_)));

        let error = client
            .delete_work_branch_attachment("token", "work-1", "branch-1", "../attachment")
            .await
            .expect_err("unsafe attachment identity must not reach transport");
        assert!(matches!(error, ThinClientError::InvalidInput(_)));

        let error = client
            .get_work_branch_task_graph_page("token", "work-1", "../branch", None, 0, 0)
            .await
            .expect_err("unsafe Task Graph identity must not reach transport");
        assert!(matches!(error, ThinClientError::InvalidInput(_)));

        let error = client
            .get_work_branch_task_graph_page("token", "work-1", "branch-1", None, 1, 0)
            .await
            .expect_err("unpinned continuation must fail before transport");
        assert!(matches!(error, ThinClientError::InvalidInput(_)));

        let error = client
            .post_work_branch_turn(
                "token",
                "work-1",
                "../branch",
                &WorkTurnRequestV1 {
                    request_id: "turn-1".into(),
                    attachment_id: "attachment-1".into(),
                    message: "Continue.".into(),
                },
            )
            .await
            .expect_err("unsafe branch identity must not reach transport");
        assert!(matches!(error, ThinClientError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn wiremock_chat_stream_parses_events() {
        let srv = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"session_info\",\"session_id\":\"s-x\",\"run_id\":\"r-y\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"hello\"}\n\n",
            "data: {\"type\":\"run_finished\",\"run_id\":\"r-y\",\"status\":\"completed\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer tkn"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let req = ChatStreamRequest::new("ping", "test-model");
        let evs = client.chat_stream_collect(&req, Some("tkn")).await.unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(
            evs[0],
            StreamEvent::SessionInfo {
                ref session_id,
                ref run_id,
            } if session_id == "s-x" && run_id.as_deref() == Some("r-y")
        ));
        assert!(matches!(
            evs[2],
            StreamEvent::RunFinished {
                ref run_id,
                ref status,
                ref error,
            } if run_id.as_deref() == Some("r-y")
                && status.as_deref() == Some("completed")
                && error.is_none()
        ));
    }

    #[tokio::test]
    async fn wiremock_chat_stream_allows_session_info_without_run_id() {
        let srv = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"session_info\",\"session_id\":\"s-x\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"hello\"}\n\n",
            "data: {\"type\":\"turn_complete\",\"assistant_text\":\"hello\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer tkn"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let req = ChatStreamRequest::new("ping", "test-model");
        let evs = client.chat_stream_collect(&req, Some("tkn")).await.unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(
            evs[0],
            StreamEvent::SessionInfo {
                ref session_id,
                ref run_id,
            } if session_id == "s-x" && run_id.is_none()
        ));
    }

    #[tokio::test]
    async fn wiremock_post_session() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "new",
                "status": "active"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("tok".into())).unwrap();
        let v = client
            .create_session(
                None,
                &SessionCreateRequest {
                    title: Some("t".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(v["session_id"], "new");
    }

    #[tokio::test]
    async fn wiremock_session_cancel_uses_the_run_converging_endpoint() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions/session-1/cancel"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "session-1",
                "status": "cancelled"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client
            .post_session_cancel_text("tok", "session-1")
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["status"],
            "cancelled"
        );
    }

    #[tokio::test]
    async fn wiremock_completion_uses_typed_scope_and_decodes_typed_response() {
        let srv = MockServer::start().await;
        let mut request = astra_server_types::CompletionRequest::new(
            astra_server_types::CompletionOperation::MemoryExtraction,
            "session-1",
            3,
            1,
            0,
            vec![serde_json::json!({"role": "user", "content": "summarize"})],
        )
        .with_offering_id("offer-memory");
        request.max_tokens = 128;
        request.temperature = 0.0;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer token"))
            .and(body_json(serde_json::to_value(&request).unwrap()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "response-1",
                "object": "chat.completion",
                "offering_id": "offer-memory",
                "model": "memory-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "summary"},
                    "finish_reason": "stop",
                    "future_optional_fact": true
                }],
                "usage": {
                    "prompt_tokens": 8,
                    "completion_tokens": 2,
                    "total_tokens": 10
                },
                "future_optional_projection": {"revision": 2}
            })))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client.post_completions("token", &request).await.unwrap();

        assert_eq!(response.offering_id, "offer-memory");
        assert_eq!(response.first_text(), Some("summary"));
        assert_eq!(response.usage.unwrap().total_tokens, 10);
    }

    #[tokio::test]
    async fn wiremock_get_session_artifact_latest_text() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/s-1/artifacts/latest/llm_capture"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "artifact_id": "art-1",
                "artifact_kind": "llm_capture"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let body = client
            .get_session_artifact_latest_text("tok", "s-1", "llm_capture")
            .await
            .unwrap();
        assert!(body.contains("\"artifact_id\":\"art-1\""));
    }

    #[tokio::test]
    async fn wiremock_download_session_artifact_reads_filename() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/s-1/artifacts/art-1/download"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "content-disposition",
                        "attachment; filename=\"llm_capture_art-1.json\"",
                    )
                    .set_body_string("{\"artifact_id\":\"art-1\"}"),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let (bytes, filename) = client
            .download_session_artifact("tok", "s-1", "art-1")
            .await
            .unwrap();
        assert_eq!(filename.as_deref(), Some("llm_capture_art-1.json"));
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"artifact_id\":\"art-1\"}"
        );
    }

    #[tokio::test]
    async fn wiremock_get_run_status() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chat/runs/run-1"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-1",
                "status": "running"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client.get_run(Some("tok"), "run-1").await.unwrap();
        assert_eq!(v["run_id"], "run-1");
        assert_eq!(v["status"], "running");
    }

    #[tokio::test]
    async fn wiremock_get_session_run_tree_decodes_typed_snapshot() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/s-1/runs"))
            .and(query_param("limit", "50"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 2,
                "session_id": "s-1",
                "snapshot_revision": "sha256:abc",
                "observed_at": "2026-07-11T00:00:00Z",
                "node_limit": 50,
                "truncated": false,
                "runs": [{
                    "run_id": "child-1",
                    "parent_run_id": "root-1",
                    "root_run_id": "root-1",
                    "depth": 1,
                    "agent_id": "reviewer",
                    "agent_name": "Reviewer",
                    "status": "waiting",
                    "waiting_for": "tool_result",
                    "run_event_high_watermark": 3,
                    "total_tool_calls": 1,
                    "runtime": {
                        "runtime_profile": "edge",
                        "model_name": "gpt-5",
                        "model_gateway": "primary",
                        "agent_binding_id": "reviewer-v2"
                    },
                    "available_actions": ["cancel"],
                    "created_at": "2026-07-11T00:00:00Z",
                    "updated_at": "2026-07-11T00:00:01Z"
                }]
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let snapshot = client
            .get_session_run_tree(Some("tok"), "s-1", 50)
            .await
            .unwrap();
        assert_eq!(snapshot.session_id, "s-1");
        assert_eq!(
            snapshot.runs[0].status,
            astra_server_types::SessionRunLifecycleStatus::Waiting
        );
        assert_eq!(
            snapshot.runs[0].available_actions,
            vec![astra_server_types::SessionRunAction::Cancel]
        );
        let runtime = &snapshot.runs[0].runtime;
        assert_eq!(runtime.runtime_profile.as_deref(), Some("edge"));
        assert_eq!(runtime.model_name.as_deref(), Some("gpt-5"));
        assert_eq!(runtime.agent_binding_id.as_deref(), Some("reviewer-v2"));
    }

    #[tokio::test]
    async fn wiremock_session_transcript_preserves_run_filter_and_identity() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/s-1/transcript"))
            .and(header("authorization", "Bearer tok"))
            .and(query_param("run_id", "child-run-1"))
            .and(query_param("before_seq", "42"))
            .and(query_param("limit", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "s-1",
                "items": [{
                    "session_id": "s-1",
                    "item_seq": 41,
                    "run_id": "child-run-1",
                    "role": "assistant",
                    "content": "Found the race.",
                    "reasoning": "Inspecting the state transition",
                    "reasoning_status": "done",
                    "created_at": "2026-07-11T00:00:00"
                }],
                "page_refs": [],
                "next_before_seq": 41,
                "has_more": false
            })))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let page = client
            .get_session_transcript(
                Some("tok"),
                "s-1",
                SessionTranscriptReadScope::Run("child-run-1"),
                Some(42),
                200,
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].run_id.as_deref(), Some("child-run-1"));
        assert_eq!(page.items[0].reasoning_status.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn wiremock_root_transcript_uses_typed_root_scope_without_run_filter() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/s-1/transcript"))
            .and(header("authorization", "Bearer tok"))
            .and(query_param("scope", "root_conversation"))
            .and(query_param("before_seq", "42"))
            .and(query_param("limit", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "s-1",
                "items": [{
                    "session_id": "s-1",
                    "item_seq": 41,
                    "run_id": "root-run-1",
                    "role": "assistant",
                    "content": "Root answer only.",
                    "created_at": "2026-07-11T00:00:00"
                }],
                "page_refs": [],
                "next_before_seq": 41,
                "has_more": false
            })))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let page = client
            .get_session_transcript(
                Some("tok"),
                "s-1",
                SessionTranscriptReadScope::RootConversation,
                Some(42),
                200,
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].run_id.as_deref(), Some("root-run-1"));
    }

    #[tokio::test]
    async fn wiremock_cancel_run_uses_durable_run_endpoint() {
        let srv = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/chat/runs/run-1"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-1",
                "status": "cancelled"
            })))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client.cancel_run(Some("tok"), "run-1").await.unwrap();
        assert_eq!(response["status"], "cancelled");
    }

    #[tokio::test]
    async fn wiremock_submit_run_user_intent() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/intents"))
            .and(header("authorization", "Bearer tok"))
            .and(body_json(serde_json::json!({
                "intent_id": "intent-1",
                "delivery": "guide_current_run",
                "input": {
                    "content": "stop after next tool call"
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-1",
                "intent_id": "intent-1",
                "status": "accepted_remote",
                "duplicate": false,
                "event_index": 19
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client
            .submit_run_user_intent(
                Some("tok"),
                "run-1",
                &RunUserIntentRequest {
                    intent_id: "intent-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    input: serde_json::json!({
                        "content": "stop after next tool call"
                    }),
                },
            )
            .await
            .unwrap();
        assert_eq!(response.run_id, "run-1");
        assert_eq!(response.intent_id, "intent-1");
        assert_eq!(
            response.status,
            astra_turn_types::UserIntentStatus::AcceptedRemote
        );
        assert!(!response.duplicate);
        assert_eq!(response.event_index, 19);
    }

    #[tokio::test]
    async fn wiremock_pause_run() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/pause"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-1",
                "status": "paused",
                "previous_status": "running",
                "disposition": "applied"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client.pause_run(Some("tok"), "run-1").await.unwrap();
        assert_eq!(v["status"], "paused");
        assert_eq!(v["previous_status"], "running");
    }

    #[tokio::test]
    async fn wiremock_resume_run() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/resume"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-1",
                "status": "running",
                "previous_status": "paused",
                "disposition": "applied"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let value = client.resume_run(Some("tok"), "run-1").await.unwrap();
        assert_eq!(value["status"], "running");
        assert_eq!(value["previous_status"], "paused");
    }

    #[tokio::test]
    async fn wiremock_list_runs() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/runs"))
            .and(query_param("limit", "25"))
            .and(query_param(
                "after_updated_at",
                "2024-01-02T00:00:00.000000",
            ))
            .and(query_param("after_run_id", "run-0"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "runs": [{"run_id": "run-1", "status": "running"}],
                "total": null,
                "limit": 25,
                "next_cursor": {"updated_at": "2024-01-01T00:00:00.000000", "run_id": "run-1"}
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .list_runs(
                Some("tok"),
                25,
                Some("2024-01-02T00:00:00.000000"),
                Some("run-0"),
            )
            .await
            .unwrap();
        assert!(v["total"].is_null());
        assert_eq!(v["runs"][0]["run_id"], "run-1");
    }

    #[tokio::test]
    async fn wiremock_stream_run_parses_events() {
        let srv = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"run_started\",\"run_id\":\"run-1\",\"session_id\":\"sess-1\"}\n\n",
            "data: {\"type\":\"run_paused\",\"run_id\":\"run-1\"}\n\n",
            "data: {\"type\":\"run_finished\",\"run_id\":\"run-1\",\"status\":\"completed\"}\n\n",
        );
        Mock::given(method("GET"))
            .and(path("/chat/runs/run-1/stream"))
            .and(query_param("last_index", "0"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let evs = client
            .stream_run_collect("run-1", 0, Some("tok"))
            .await
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(
            evs[0],
            StreamEvent::RunStarted {
                ref run_id,
                ref session_id,
            } if run_id.as_deref() == Some("run-1") && session_id.as_deref() == Some("sess-1")
        ));
        assert!(matches!(
            evs[1],
            StreamEvent::RunPaused {
                ref run_id,
            } if run_id.as_deref() == Some("run-1")
        ));
        assert!(matches!(
            evs[2],
            StreamEvent::RunFinished {
                ref run_id,
                ref status,
                ref error,
            } if run_id.as_deref() == Some("run-1")
                && status.as_deref() == Some("completed")
                && error.is_none()
        ));
    }

    #[tokio::test]
    async fn wiremock_delegate_run() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/delegate"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "delegation_id": "deleg-1",
                "status": "running"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .delegate_run(
                Some("tok"),
                "run-1",
                &serde_json::json!({"pattern": "fan_out", "agent_ids": ["a1"]}),
            )
            .await
            .unwrap();
        assert_eq!(v["delegation_id"], "deleg-1");
        assert_eq!(v["status"], "running");
    }

    #[tokio::test]
    async fn wiremock_list_run_delegations() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chat/runs/run-1/delegations"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parent_run_id": "run-1",
                "sub_run_ids": ["child-1", "child-2"]
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .list_run_delegations(Some("tok"), "run-1")
            .await
            .unwrap();
        assert_eq!(v["parent_run_id"], "run-1");
        assert_eq!(v["sub_run_ids"][0], "child-1");
    }

    #[tokio::test]
    async fn wiremock_pause_run_delegations() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/delegations/pause"))
            .and(header("authorization", "Bearer tok"))
            .and(body_json(serde_json::json!({
                "expected_session_id": "session-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parent_run_id": "run-1",
                "affected": 2
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .pause_run_delegations(Some("tok"), "run-1", "session-1")
            .await
            .unwrap();
        assert_eq!(v["parent_run_id"], "run-1");
        assert_eq!(v["affected"], 2);
    }

    #[tokio::test]
    async fn wiremock_resume_run_delegations() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/delegations/resume"))
            .and(header("authorization", "Bearer tok"))
            .and(body_json(serde_json::json!({
                "expected_session_id": "session-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parent_run_id": "run-1",
                "affected": 2
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .resume_run_delegations(Some("tok"), "run-1", "session-1")
            .await
            .unwrap();
        assert_eq!(v["parent_run_id"], "run-1");
        assert_eq!(v["affected"], 2);
    }

    #[tokio::test]
    async fn wiremock_post_event_json() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/events"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "event_id": "event_1",
                "metadata": {
                    "source": "ordinary"
                }
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .post_event_json(
                Some("tok"),
                &serde_json::json!({
                    "event_id": "event_1",
                    "session_id": "session-1",
                    "event_type": "ordinary_marker",
                    "content": "{}",
                    "metadata": {
                        "source": "ordinary"
                    }
                }),
            )
            .await
            .unwrap();
        assert_eq!(v["event_id"], "event_1");
        assert_eq!(v["metadata"]["source"], "ordinary");
    }

    #[tokio::test]
    async fn wiremock_post_sync_outbox_event_json() {
        let srv = MockServer::start().await;
        let body = serde_json::json!({
            "event_id": "sync_evt_1",
            "session_id": "session-1",
            "event_type": "sync_marker",
            "content": "{}",
            "metadata": {
                "sync_outbox": {
                    "payload_hash": "sha256:abc"
                }
            }
        });
        Mock::given(method("POST"))
            .and(path("/sync/outbox/events"))
            .and(header("authorization", "Bearer tok"))
            .and(header(
                "x-astra-sync-outbox-signature",
                sync_outbox_request_signature("tok", &body),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "schema_version": 1,
                "record_id": "sync_evt_1",
                "payload_hash": "sha256:abc",
                "ingestion_status": "created"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .post_sync_outbox_event_json(Some("tok"), &body)
            .await
            .unwrap();
        assert!(v.confirms("sync_evt_1", "sha256:abc"));
    }

    #[tokio::test]
    async fn wiremock_post_tool_result_sends_edge_header() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .and(header("x-astra-edge-id", "edge-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let body = ToolResultRequest::new_with_hash(crate::protocol::ToolResultRequestParts {
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            turn_chain_id: "chain-1".into(),
            request_id: "tr-1".into(),
            edge_agent_id: "edge-abc".into(),
            status: "completed".into(),
            output: "out".into(),
            duration_ms: 12,
            tool_result_fields: None,
        });
        let v = client
            .post_tool_result(Some("tok"), Some("edge-abc"), &body)
            .await
            .unwrap();
        assert!(v.is_null(), "callbacks acknowledge by successful status");
    }

    #[tokio::test]
    async fn approval_callback_timeout_is_bounded_and_retries_identical_request() {
        use tokio::io::AsyncReadExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request_tx = request_tx.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..read]);
                        let Some(headers_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let headers_end = headers_end + 4;
                        let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= headers_end + content_length {
                            let _ = request_tx
                                .send(bytes[headers_end..headers_end + content_length].to_vec());
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            return;
                        }
                    }
                });
            }
        });
        let client = ThinClient::new(&format!("http://{address}"), None).unwrap();
        let body = ApprovalRespondRequest {
            request_id: "approval-1".into(),
            decision: crate::protocol::ApprovalDecision::Allow,
            reason: None,
            session_id: "session-1".into(),
            run_id: "run-1".into(),
            tool_name: Some("bash".into()),
            approval_kind: Some(crate::protocol::ApprovalKind::Standard),
        };
        let started = std::time::Instant::now();

        let error = client
            .post_approval_with_policy(Some("tok"), &body, Duration::from_millis(250), 2)
            .await
            .expect_err("both response-header waits should time out");

        assert!(error.is_transport(), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let first = tokio::time::timeout(Duration::from_secs(1), request_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), request_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, second, "one bounded identical retry is required");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn approval_callback_success_does_not_wait_for_optional_response_body() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..read]);
                let Some(headers_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers_end = headers_end + 4;
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= headers_end + content_length {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    return;
                }
            }
        });
        let client = ThinClient::new(&format!("http://{address}"), None).unwrap();
        let body = ApprovalRespondRequest {
            request_id: "approval-2".into(),
            decision: crate::protocol::ApprovalDecision::Allow,
            reason: None,
            session_id: "session-1".into(),
            run_id: "run-1".into(),
            tool_name: Some("bash".into()),
            approval_kind: Some(crate::protocol::ApprovalKind::Standard),
        };

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            client.post_approval_with_policy(Some("tok"), &body, Duration::from_secs(2), 2),
        )
        .await
        .expect("successful callback status must not wait for a response body")
        .unwrap();

        assert!(result.is_null());
        server.abort();
    }

    #[tokio::test]
    async fn wiremock_get_url_absolute() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/mem/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let url = format!("{}/mem/health", srv.uri().as_str().trim_end_matches('/'));
        let r = client.get_url(&url).await.unwrap();
        assert!(r.status().is_success());
    }

    #[tokio::test]
    async fn wiremock_model_catalog_cursor_is_encoded_as_seek_query() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param("after_provider", "bedrock/openai"))
            .and(query_param("after_name", "model two"))
            .and(query_param("after_offering_id", "offer/2"))
            .and(header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [],
                "next_cursor": null,
                "limit": 1,
                "total": 1,
                "catalog_revision": "sha256:test"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client
            .get_models_page_response_timeout(
                "t",
                Duration::from_secs(1),
                Some(("bedrock/openai", "model two", "offer/2")),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
    }

    #[tokio::test]
    async fn chat_stream_non_ok_status_yields_error() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let err = client
            .chat_stream_collect(&ChatStreamRequest::new("x", "test-model"), None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "{msg}");
    }

    #[tokio::test]
    async fn wiremock_agents_edge_register_typed_body() {
        let srv = MockServer::start().await;
        let capabilities =
            crate::edge::edge_runtime_environment_capabilities("agent-logical", "/workspace/app");
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .and(header("authorization", "Bearer t"))
            .and(header("x-astra-edge-id", "transport-1"))
            .and(body_json(serde_json::json!({
                "edge_agent_id": "agent-logical",
                "hostname": "host-a",
                "worktree_path": "/workspace/app",
                "capabilities": capabilities,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let body = EdgeRegisterRequest {
            edge_agent_id: "agent-logical".into(),
            hostname: Some("host-a".into()),
            worktree_path: Some("/workspace/app".into()),
            capabilities: Some(crate::edge::edge_runtime_environment_capabilities(
                "agent-logical",
                "/workspace/app",
            )),
        };
        let v = client
            .post_agents_edge_register(Some("t"), Some("transport-1"), &body)
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn wiremock_agents_edge_heartbeat_round_trips_typed_reconciliation_contract() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge/heartbeat"))
            .and(header("authorization", "Bearer t"))
            .and(header("x-astra-edge-id", "transport-1"))
            .and(body_json(serde_json::json!({
                "edge_agent_id": "edge-1",
                "pending_request_count": 2,
                "last_seen_request_ids": ["invocation-2"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "user_id": "user-1",
                "edge_id": "transport-1",
                "edge_agent_id": "edge-1",
                "unresolved_request_ids": ["invocation-1"],
                "replay_policy": "durable_result_reconciliation_required",
                "ack_request_ids": ["invocation-2"]
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let response = client
            .post_agents_edge_heartbeat(
                Some("t"),
                Some("transport-1"),
                &EdgeHeartbeatRequest {
                    edge_agent_id: "edge-1".to_string(),
                    pending_request_count: 2,
                    last_seen_request_ids: vec!["invocation-2".to_string()],
                },
            )
            .await
            .unwrap();

        assert_eq!(
            response.replay_policy,
            crate::protocol::EdgeHeartbeatReplayPolicy::DurableResultReconciliationRequired
        );
        assert_eq!(
            response.unresolved_request_ids,
            vec!["invocation-1".to_string()]
        );
        assert_eq!(response.ack_request_ids, vec!["invocation-2".to_string()]);
    }

    // ── Constructor validation ──────────────────────────────────────────

    #[test]
    fn new_with_valid_url_succeeds() {
        let c = ThinClient::new("https://api.example.com", None);
        assert!(c.is_ok());
    }

    #[test]
    fn new_with_trailing_slash_succeeds() {
        let c = ThinClient::new("https://api.example.com/", None);
        assert!(c.is_ok());
    }

    #[test]
    fn new_with_invalid_url_returns_error() {
        let err = ThinClient::new("not a url", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid base URL"), "{msg}");
    }

    #[test]
    fn new_stores_bearer_token() {
        let c = ThinClient::new("https://x.io", Some("secret".into())).unwrap();
        assert_eq!(c.bearer_token.as_deref(), Some("secret"));
    }

    #[test]
    fn new_without_bearer_token() {
        let c = ThinClient::new("https://x.io", None).unwrap();
        assert!(c.bearer_token.is_none());
    }

    // ── api_origin() trailing-slash handling ─────────────────────────────

    #[test]
    fn api_origin_strips_trailing_slash() {
        let c = ThinClient::new("https://api.example.com/", None).unwrap();
        assert_eq!(c.api_origin(), "https://api.example.com");
    }

    #[test]
    fn api_origin_without_trailing_slash() {
        let c = ThinClient::new("https://api.example.com", None).unwrap();
        // Url::parse adds a trailing slash for scheme://host, so api_origin strips it
        assert!(!c.api_origin().ends_with('/'));
    }

    // ── bearer_headers() ────────────────────────────────────────────────

    #[test]
    fn bearer_headers_format() {
        let h = ThinClient::bearer_headers("my-tok").unwrap();
        assert_eq!(
            h.get(header::AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer my-tok"
        );
    }

    #[test]
    fn bearer_headers_rejects_non_ascii_token() {
        // Header values must be visible ASCII; newlines are rejected.
        let res = ThinClient::bearer_headers("bad\ntoken");
        assert!(res.is_err());
    }

    // ── Error paths via wiremock ─────────────────────────────────────────

    #[tokio::test]
    async fn wiremock_401_returns_api_error() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let err = client
            .get_bearer_path_query_text("tok", "/health", &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("unauthorized"), "{msg}");
    }

    #[tokio::test]
    async fn wiremock_500_returns_api_error() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("tok".into())).unwrap();
        let err = client
            .create_session(
                None,
                &SessionCreateRequest {
                    title: Some("t".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("500"), "{msg}");
    }

    #[tokio::test]
    async fn wiremock_empty_body_returns_null() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("tok".into())).unwrap();
        let v = client
            .create_session(
                None,
                &SessionCreateRequest {
                    title: Some("t".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(v.is_null(), "expected Null for empty body, got {v}");
    }

    // ── Bearer override precedence ──────────────────────────────────────

    #[tokio::test]
    async fn bearer_override_takes_precedence_over_default() {
        let srv = MockServer::start().await;
        // Only match when the override token is used
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer override-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(concat!(
                "data: {\"type\":\"text_delta\",\"content\":\"ok\"}\n\n",
                "data: {\"type\":\"turn_complete\"}\n\n"
            )))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("default-tok".into())).unwrap();
        let req = ChatStreamRequest::new("hi", "test-model");
        let evs = client
            .chat_stream_collect(&req, Some("override-tok"))
            .await
            .unwrap();
        assert_eq!(evs.len(), 2);
    }

    #[tokio::test]
    async fn default_bearer_used_when_no_override() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer default-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(concat!(
                "data: {\"type\":\"text_delta\",\"content\":\"ok\"}\n\n",
                "data: {\"type\":\"turn_complete\"}\n\n"
            )))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("default-tok".into())).unwrap();
        let req = ChatStreamRequest::new("hi", "test-model");
        let evs = client.chat_stream_collect(&req, None).await.unwrap();
        assert_eq!(evs.len(), 2);
    }

    #[tokio::test]
    async fn chat_stream_eof_without_terminal_is_an_error() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: {\"type\":\"text_delta\",\"content\":\"partial\"}\n\n"),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let error = client
            .chat_stream_collect(&ChatStreamRequest::new("hi", "test-model"), None)
            .await
            .expect_err("a partial stream must not look like a completed turn");
        assert!(error.to_string().contains("terminal event"));
    }

    // ── 429 retry logic ─────────────────────────────────────────────────

    #[tokio::test]
    async fn retry_429_succeeds_on_second_attempt() {
        let _guard = set_test_retry_sleep_ms(0);
        let srv = MockServer::start().await;
        // First call → 429, second call → 200
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(1)
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer t"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
                        astra_server_types::AGENT_INTERACTION_API_MAJOR,
                    )
                    .set_body_string("ok"),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        let resp = client
            .post_developer_loop_retry_429("t", &payload, 3, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // No Retry-After header → fell back to the default exponential backoff
        // for attempt 0: `2u64 << 0` == 2s.
        assert_eq!(last_retry_sleep_secs(), Some(2));
    }

    #[tokio::test]
    async fn retry_429_exhausts_all_attempts() {
        let srv = MockServer::start().await;
        // Always return 429
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        // With max_attempts=1, there is no retry — the 429 response is returned as-is.
        let resp = client
            .post_developer_loop_retry_429("t", &payload, 1, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 429);
    }

    #[tokio::test]
    async fn retry_429_returns_ok_on_non_429_immediately() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
                        astra_server_types::AGENT_INTERACTION_API_MAJOR,
                    )
                    .set_body_string("ok"),
            )
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        let resp = client
            .post_developer_loop_retry_429("t", &payload, 5, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn developer_loop_rejects_a_server_without_the_interaction_contract() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(ResponseTemplate::new(200).set_body_string("stale server"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let error = client
            .post_developer_loop("t", &serde_json::json!({"msg": "hello"}))
            .await
            .expect_err("missing protocol header must fail before SSE consumption");

        assert!(matches!(
            error,
            ThinClientError::IncompatibleRuntime { ref actual, .. } if actual == "missing"
        ));
        assert!(error.to_string().contains("restart or upgrade"));
    }

    #[tokio::test]
    async fn developer_loop_rejects_a_stale_interaction_contract_before_sse() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER, "1")
                    .set_body_string("stale server"),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let error = client
            .post_developer_loop("t", &serde_json::json!({"msg": "hello"}))
            .await
            .expect_err("stale protocol must fail before SSE consumption");
        assert!(matches!(
            error,
            ThinClientError::IncompatibleRuntime { ref expected, ref actual }
                if expected == "3" && actual == "1"
        ));
    }

    // ── parse_retry_after tests ─────────────────────────────────────────

    #[test]
    fn parse_retry_after_valid_integer() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("5"));
        assert_eq!(parse_retry_after(&headers), Some(5));
    }

    #[test]
    fn parse_retry_after_clamps_high() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("999"));
        assert_eq!(parse_retry_after(&headers), Some(120));
    }

    #[test]
    fn parse_retry_after_clamps_zero_to_one() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("0"));
        assert_eq!(parse_retry_after(&headers), Some(1));
    }

    #[test]
    fn parse_retry_after_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_non_numeric() {
        let mut headers = HeaderMap::new();
        // HTTP-date format not supported — returns None
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 09 Apr 2026 12:00:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_whitespace_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("  10  "));
        assert_eq!(parse_retry_after(&headers), Some(10));
    }

    #[tokio::test]
    async fn retry_429_honours_retry_after_header() {
        let _guard = set_test_retry_sleep_ms(0);
        let srv = MockServer::start().await;
        // First call → 429 with Retry-After: 1, second → 200
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "1")
                    .set_body_string("rate limited"),
            )
            .up_to_n_times(1)
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
                        astra_server_types::AGENT_INTERACTION_API_MAJOR,
                    )
                    .set_body_string("ok"),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        let resp = client
            .post_developer_loop_retry_429("t", &payload, 3, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // The real invariant: the client picked the Retry-After value (1s),
        // not the default exponential backoff for the first 429 (2s).
        assert_eq!(
            last_retry_sleep_secs(),
            Some(1),
            "Retry-After: 1 should be honoured over default exponential backoff"
        );
    }

    #[test]
    fn thin_client_http_stream_connect_timeout_policy_is_bounded() {
        assert_eq!(http_stream_connect_timeout(), Duration::from_secs(60));
        streaming_http_client(&Url::parse("https://astra.example.com").unwrap())
            .expect("streaming HTTP client builder");
    }

    #[test]
    fn authed_text_request_timeout_policy_is_bounded() {
        let timeout = authed_text_request_timeout();
        assert!(
            timeout <= Duration::from_secs(30),
            "authed text requests must stay bounded so callers cannot hang indefinitely"
        );
        assert!(
            timeout >= Duration::from_secs(1),
            "authed text request timeout should not fail healthy calls immediately"
        );
    }

    #[test]
    fn health_request_timeout_policy_is_bounded() {
        let timeout = Duration::from_secs(HEALTH_REQUEST_TIMEOUT_SECS);
        assert!(
            timeout <= Duration::from_secs(10),
            "health probes must fail fast when the control plane is stuck"
        );
        assert!(
            timeout >= Duration::from_secs(1),
            "health probes should leave healthy local servers time to respond"
        );
    }

    #[test]
    fn attachment_filename_parses_normal_filename() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_DISPOSITION,
            "attachment; filename=\"capture_2024.json\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            attachment_filename(&headers),
            Some("capture_2024.json".to_string())
        );
    }

    #[test]
    fn attachment_filename_caps_length() {
        let long_name = "a".repeat(300);
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{long_name}\"")
                .parse()
                .unwrap(),
        );
        let result = attachment_filename(&headers).unwrap();
        assert_eq!(result.len(), 255);
    }

    #[test]
    fn attachment_filename_returns_none_for_empty() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_DISPOSITION,
            "attachment; filename=\"\"".parse().unwrap(),
        );
        assert_eq!(attachment_filename(&headers), None);
    }

    // ── HTTP client proxy policy ──────────────────────────────────────────────
    //
    // The SSE chat-turn stream (`post_chat_turn`) and the model-list pre-flight
    // (`get_models_response_timeout`) both use `http_stream`, which is built by
    // `streaming_http_client()`.  Remote endpoints retain proxy-aware routing so
    // that sandbox environments with HTTP_PROXY as the only egress path continue
    // to work.  Loopback endpoints bypass the proxy so local Astra servers and
    // test harnesses cannot be redirected to an infrastructure proxy.

    #[test]
    fn proxy_policy_bypasses_only_process_local_base_urls() {
        for base in [
            "http://localhost:17001",
            "http://api.localhost:17001",
            "http://127.0.0.1:17001",
            "http://127.42.7.9:17001",
            "http://[::1]:17001",
        ] {
            assert!(
                base_is_loopback(&Url::parse(base).unwrap()),
                "{base} must bypass inherited outbound proxies"
            );
        }
        for base in [
            "https://astra.example.com",
            "http://10.0.0.8:17001",
            "http://host.docker.internal:17001",
            "http://notlocalhost:17001",
        ] {
            assert!(
                !base_is_loopback(&Url::parse(base).unwrap()),
                "{base} must retain environment-aware proxy routing"
            );
        }
    }
}
