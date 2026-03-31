//! HTTP client: dispatch to cloud API, consume SSE streams.

use std::pin::Pin;
use std::time::Duration;

use async_stream::stream;
use futures_util::{Stream, StreamExt};
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    Client, Response, Url,
};
use serde_json::Value;

use crate::error::ThinClientError;
use crate::paths;
use crate::protocol::{
    ApprovalRespondRequest, ChatStreamRequest, SessionCreateRequest, SessionUpdateRequest,
    StreamEvent, ToolResultRequest,
};
use crate::sse::SseParser;

/// Stateless façade over the mo-agent HTTP API (thin client).
#[derive(Debug, Clone)]
pub struct ThinClient {
    http: Client,
    base: Url,
    /// Default bearer when call sites omit per-request token (optional).
    bearer_token: Option<String>,
}

impl ThinClient {
    /// `base` is the server origin, e.g. `https://api.example.com` (trailing slash optional).
    pub fn new(base: &str, bearer_token: Option<String>) -> Result<Self, ThinClientError> {
        let base = Url::parse(base).map_err(|_| ThinClientError::InvalidBaseUrl(base.to_string()))?;
        let http = Client::builder()
            .no_proxy()
            .build()?;
        Ok(Self {
            http,
            base,
            bearer_token,
        })
    }

    /// Shared `reqwest::Client` (TLS / proxy policy aligned with thin API). For `LlmToolSelector` and ad-hoc calls to other origins (e.g. Memoria health).
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
        if let Some(t) = token {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {t}")) {
                h.insert(header::AUTHORIZATION, v);
            }
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

    pub async fn get_model_text(&self, token: &str, model_name: &str) -> Result<String, ThinClientError> {
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

    pub async fn get_session_text(&self, token: &str, session_id: &str) -> Result<String, ThinClientError> {
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

    pub async fn delete_session_text(&self, token: &str, session_id: &str) -> Result<String, ThinClientError> {
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

    pub async fn get_task_text(&self, token: &str, task_id: &str) -> Result<String, ThinClientError> {
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
            .http
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
            .http
            .post(url)
            .timeout(timeout)
            .headers(headers)
            .json(payload)
            .send()
            .await?)
    }

    /// Retry on 429 up to `max_attempts` (same policy as mo-agent CLI).
    pub async fn post_chat_turn_retry_429(
        &self,
        token: &str,
        payload: &Value,
        max_attempts: u32,
        quiet: bool,
    ) -> Result<Response, ThinClientError> {
        for attempt in 0..max_attempts {
            let resp = self.post_chat_turn(token, payload).await?;
            if resp.status().as_u16() == 429 && attempt + 1 < max_attempts {
                let delay_secs = 2u64 << attempt;
                if !quiet {
                    eprintln!("  ⏳ Rate limited (429), retrying in {delay_secs}s…");
                }
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                continue;
            }
            return Ok(resp);
        }
        Err(ThinClientError::SseParse("retry exhausted".into()))
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
                return Err(ThinClientError::SseParse(format!(
                    "HTTP {status}: {text}"
                )));
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
            req = req.header("X-Mo-Edge-Id", v);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn wiremock_chat_stream_parses_events() {
        let srv = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"session_info\",\"session_id\":\"s-x\",\"run_id\":\"r-y\"}\n\n",
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
            } if session_id == "s-x" && run_id == "r-y"
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
            .create_session(None, &SessionCreateRequest {
                title: Some("t".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(v["session_id"], "new");
    }

    #[tokio::test]
    async fn wiremock_post_tool_result_sends_edge_header() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .and(header("x-mo-edge-id", "edge-abc"))
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"tasks": []})))
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
}
