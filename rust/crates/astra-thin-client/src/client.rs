//! HTTP client: dispatch to cloud API, consume SSE streams.

use std::pin::Pin;
use std::time::Duration;

use async_stream::stream;
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, Response, Url,
    header::{self, HeaderMap, HeaderValue},
};
use serde_json::Value;

use crate::edge::ASTRA_EDGE_ID_HEADER;
use crate::error::ThinClientError;
use crate::paths;
use crate::protocol::{
    ApprovalRespondRequest, ChatStreamRequest, EdgeHeartbeatRequest, EdgeRegisterRequest,
    SessionCreateRequest, SessionUpdateRequest, StreamEvent, TaskLeaseMutationRequest,
    ToolResultRequest,
};
use crate::sse::SseParser;

/// Parse the `Retry-After` header value into seconds.
/// Supports integer seconds format; ignores HTTP-date format.
/// Clamps to [1, 120] seconds. Returns `None` on missing/unparseable.
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(secs.clamp(1, 120))
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
        let http = Client::builder().no_proxy().build()?;
        let http_stream = Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .build()?;
        Ok(Self {
            http,
            http_stream,
            base,
            bearer_token,
        })
    }

    /// Shared `reqwest::Client` (TLS / proxy policy aligned with thin API). For optional in-library LLM tool selection and ad-hoc calls to other origins (e.g. Memoria health).
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
        let resp = self.http.get(url).send().await?;
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
        let url = self.url(paths::MODELS)?;
        Ok(self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .timeout(timeout)
            .send()
            .await?)
    }

    pub async fn get_models_text(&self, token: &str) -> Result<String, ThinClientError> {
        let url = self.url(paths::MODELS)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
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

    pub async fn post_skills_register_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::SKILLS)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_skills_test_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::SKILLS_TEST)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
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

    // ── Tasks (§5.5 state CRUD — `router_builder`) ───────────────────────────

    pub async fn get_tasks_query_text(
        &self,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::TASKS)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .query(query)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_tasks_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::TASKS)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_task_text(
        &self,
        token: &str,
        task_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::task(task_id))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_task_progress_text(
        &self,
        token: &str,
        task_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::task_progress(task_id))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn put_task_status_json(
        &self,
        token: &str,
        task_id: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::task_status(task_id))?;
        let resp = self
            .http
            .put(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Context snapshots ──────────────────────────────────────────────────

    pub async fn get_context_query_text(
        &self,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::CONTEXT)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .query(query)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn post_context_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::CONTEXT)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    pub async fn get_context_capture_text(
        &self,
        token: &str,
        context_capture_id: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(&paths::context_capture(context_capture_id))?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// `POST /chat/route` — non-streaming route/classify (optional for IDE/SDK).
    pub async fn post_chat_route_json(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<String, ThinClientError> {
        let url = self.url(paths::CHAT_ROUTE)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .json(body)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    /// `POST /v1/chat/completions` — lightweight LLM proxy for verification judge.
    ///
    /// Returns the raw JSON response from the server's completions proxy.
    /// The server resolves the active model, decrypts the API key, and forwards
    /// to the upstream LLM provider.
    pub async fn post_completions(
        &self,
        token: &str,
        body: &Value,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::COMPLETIONS)?;
        let resp = self
            .http
            .post(url)
            .headers(Self::bearer_headers(token)?)
            .timeout(std::time::Duration::from_secs(120))
            .json(body)
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    // ── Reflect / decision trace ─────────────────────────────────────────────

    /// `path_with_query` is relative to origin, e.g. `chat/session/sid/reflect?focus=auto`.
    pub async fn get_authed_path_text(
        &self,
        token: &str,
        path_with_query: &str,
    ) -> Result<String, ThinClientError> {
        let url = self.url(path_with_query)?;
        let resp = self
            .http
            .get(url)
            .headers(Self::bearer_headers(token)?)
            .send()
            .await?;
        Self::text_or_api(resp).await
    }

    // ── Chat turn (SSE) ─────────────────────────────────────────────────────

    /// Single POST `/chat/turn` with SSE accept header.
    pub async fn post_chat_turn(
        &self,
        token: &str,
        payload: &Value,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::CHAT_TURN)?;
        let mut headers = Self::bearer_headers(token)?;
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        Ok(self
            .http_stream
            .post(url)
            .headers(headers)
            .json(payload)
            .send()
            .await?)
    }

    /// Same as [`Self::post_chat_turn`] but with a per-request timeout (e.g. LLM tool-selection probe).
    pub async fn post_chat_turn_timeout(
        &self,
        token: &str,
        payload: &Value,
        timeout: Duration,
    ) -> Result<Response, ThinClientError> {
        let url = self.url(paths::CHAT_TURN)?;
        let mut headers = Self::bearer_headers(token)?;
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        Ok(self
            .http_stream
            .post(url)
            .timeout(timeout)
            .headers(headers)
            .json(payload)
            .send()
            .await?)
    }

    /// Retry on 429 and transport errors up to `max_attempts`, honouring `Retry-After` header.
    ///
    /// Transport errors (connection reset, timeout) are retried with exponential backoff.
    /// The total retry budget is capped at `max_attempts` across both 429s and transport errors.
    pub async fn post_chat_turn_retry_429(
        &self,
        token: &str,
        payload: &Value,
        max_attempts: u32,
        quiet: bool,
    ) -> Result<Response, ThinClientError> {
        let mut last_err: Option<ThinClientError> = None;
        for attempt in 0..max_attempts {
            match self.post_chat_turn(token, payload).await {
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
                                "rate limited, retrying"
                            );
                            eprintln!("  ⏳ Rate limited (429), retrying in {delay_secs}s…");
                        }
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    if attempt + 1 < max_attempts && e.is_transport() {
                        let delay_secs = 1u64 << attempt; // 1s, 2s, 4s…
                        if !quiet {
                            tracing::warn!(
                                target: "astra.thin_client",
                                error = %e,
                                delay_secs,
                                attempt = attempt + 1,
                                max_attempts,
                                "transport error, retrying"
                            );
                            eprintln!("  ⏳ Transport error, retrying in {delay_secs}s… ({e})");
                        }
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
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
        offset: u32,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::RUNS)?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .query(&[("limit", limit), ("offset", offset)])
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
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_delegations_pause(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /chat/runs/{run_id}/delegations/resume` — resume delegated child runs.
    pub async fn resume_run_delegations(
        &self,
        bearer_override: Option<&str>,
        run_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::chat_run_delegations_resume(run_id))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
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
        let url = self.url(paths::TOOLS_RESULT)?;
        let mut req = self
            .http
            .post(url)
            .headers(self.auth_headers_for(bearer_override))
            .json(body);
        if let Some(id) = edge_executor_id
            && let Ok(v) = HeaderValue::from_str(id)
        {
            req = req.header(ASTRA_EDGE_ID_HEADER, v);
        }
        let resp = req.send().await?;
        Self::json_or_error(resp).await
    }

    pub async fn post_approval(
        &self,
        bearer_override: Option<&str>,
        body: &ApprovalRespondRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(paths::APPROVAL_RESPOND)?;
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
    ) -> Result<Value, ThinClientError> {
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
        Self::json_or_error(resp).await
    }

    /// `GET /tasks/{task_id}/lease` — current lease metadata.
    pub async fn get_task_lease(
        &self,
        bearer_override: Option<&str>,
        task_id: &str,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::task_lease(task_id))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers_for(bearer_override))
            .send()
            .await?;
        Self::json_or_error(resp).await
    }

    /// `POST /tasks/{task_id}/lease/claim`
    pub async fn post_task_lease_claim(
        &self,
        bearer_override: Option<&str>,
        edge_transport_id: Option<&str>,
        task_id: &str,
        body: &TaskLeaseMutationRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::task_lease_claim(task_id))?;
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

    /// `POST /tasks/{task_id}/lease/release`
    pub async fn post_task_lease_release(
        &self,
        bearer_override: Option<&str>,
        edge_transport_id: Option<&str>,
        task_id: &str,
        body: &TaskLeaseMutationRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::task_lease_release(task_id))?;
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

    /// `POST /tasks/{task_id}/lease/renew`
    pub async fn post_task_lease_renew(
        &self,
        bearer_override: Option<&str>,
        edge_transport_id: Option<&str>,
        task_id: &str,
        body: &TaskLeaseMutationRequest,
    ) -> Result<Value, ThinClientError> {
        let url = self.url(&paths::task_lease_renew(task_id))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
        let req = ChatStreamRequest::new("ping");
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
        );
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer tkn"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let req = ChatStreamRequest::new("ping");
        let evs = client.chat_stream_collect(&req, Some("tkn")).await.unwrap();
        assert_eq!(evs.len(), 2);
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
    async fn wiremock_pause_run() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/runs/run-1/pause"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "run_id": "run-1",
                "status": "paused",
                "previous_status": "running"
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client.pause_run(Some("tok"), "run-1").await.unwrap();
        assert_eq!(v["status"], "paused");
        assert_eq!(v["previous_status"], "running");
    }

    #[tokio::test]
    async fn wiremock_list_runs() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/runs"))
            .and(query_param("limit", "25"))
            .and(query_param("offset", "5"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "runs": [{"run_id": "run-1", "status": "running"}],
                "total": 1,
                "limit": 25,
                "offset": 5
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client.list_runs(Some("tok"), 25, 5).await.unwrap();
        assert_eq!(v["total"], 1);
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parent_run_id": "run-1",
                "affected": 2
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .pause_run_delegations(Some("tok"), "run-1")
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "parent_run_id": "run-1",
                "affected": 2
            })))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let v = client
            .resume_run_delegations(Some("tok"), "run-1")
            .await
            .unwrap();
        assert_eq!(v["parent_run_id"], "run-1");
        assert_eq!(v["affected"], 2);
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
        let body = ToolResultRequest {
            request_id: "tr-1".into(),
            status: "success".into(),
            output: Some("out".into()),
            duration_ms: Some(12),
        };
        let v = client
            .post_tool_result(Some("tok"), Some("edge-abc"), &body)
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn wiremock_tasks_list() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tasks"))
            .and(header("authorization", "Bearer t"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tasks": []})),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let body = client.get_tasks_query_text("t", &[]).await.unwrap();
        assert!(body.contains("tasks"), "{body}");
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
    async fn chat_stream_non_ok_status_yields_error() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let err = client
            .chat_stream_collect(&ChatStreamRequest::new("x"), None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "{msg}");
    }

    #[tokio::test]
    async fn wiremock_agents_edge_register_typed_body() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .and(header("authorization", "Bearer t"))
            .and(header("x-astra-edge-id", "transport-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let body = EdgeRegisterRequest {
            edge_agent_id: "agent-logical".into(),
            hostname: Some("host-a".into()),
            worktree_path: None,
            capabilities: None,
        };
        let v = client
            .post_agents_edge_register(Some("t"), Some("transport-1"), &body)
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn wiremock_task_lease_claim_sends_edge_header() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tasks/t1/lease/claim"))
            .and(header("authorization", "Bearer t"))
            .and(header("x-astra-edge-id", "edge-xyz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"status":"granted"})),
            )
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let body = TaskLeaseMutationRequest {
            edge_agent_id: "edge-xyz".into(),
            ttl_sec: Some(120),
        };
        let v = client
            .post_task_lease_claim(Some("t"), Some("edge-xyz"), "t1", &body)
            .await
            .unwrap();
        assert_eq!(v["status"], "granted");
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
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: {\"type\":\"text_delta\",\"content\":\"ok\"}\n\n"),
            )
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("default-tok".into())).unwrap();
        let req = ChatStreamRequest::new("hi");
        let evs = client
            .chat_stream_collect(&req, Some("override-tok"))
            .await
            .unwrap();
        assert_eq!(evs.len(), 1);
    }

    #[tokio::test]
    async fn default_bearer_used_when_no_override() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/stream"))
            .and(header("authorization", "Bearer default-tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: {\"type\":\"text_delta\",\"content\":\"ok\"}\n\n"),
            )
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), Some("default-tok".into())).unwrap();
        let req = ChatStreamRequest::new("hi");
        let evs = client.chat_stream_collect(&req, None).await.unwrap();
        assert_eq!(evs.len(), 1);
    }

    // ── 429 retry logic ─────────────────────────────────────────────────

    #[tokio::test]
    async fn retry_429_succeeds_on_second_attempt() {
        let srv = MockServer::start().await;
        // First call → 429, second call → 200
        Mock::given(method("POST"))
            .and(path("/chat/turn"))
            .and(header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(1)
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/turn"))
            .and(header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        let resp = client
            .post_chat_turn_retry_429("t", &payload, 3, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn retry_429_exhausts_all_attempts() {
        let srv = MockServer::start().await;
        // Always return 429
        Mock::given(method("POST"))
            .and(path("/chat/turn"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        // With max_attempts=1, there is no retry — the 429 response is returned as-is.
        let resp = client
            .post_chat_turn_retry_429("t", &payload, 1, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 429);
    }

    #[tokio::test]
    async fn retry_429_returns_ok_on_non_429_immediately() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/turn"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        let resp = client
            .post_chat_turn_retry_429("t", &payload, 5, true)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
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
        let srv = MockServer::start().await;
        // First call → 429 with Retry-After: 1, second → 200
        Mock::given(method("POST"))
            .and(path("/chat/turn"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "1")
                    .set_body_string("rate limited"),
            )
            .up_to_n_times(1)
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/turn"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&srv)
            .await;

        let client = ThinClient::new(&srv.uri(), None).unwrap();
        let payload = serde_json::json!({"msg": "hello"});
        let start = std::time::Instant::now();
        let resp = client
            .post_chat_turn_retry_429("t", &payload, 3, true)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status().as_u16(), 200);
        // Should wait ~1s (from Retry-After: 1), not 2s (default)
        assert!(elapsed >= Duration::from_millis(900), "waited too little");
        assert!(
            elapsed < Duration::from_millis(1800),
            "waited too long — Retry-After not honoured"
        );
    }
}
