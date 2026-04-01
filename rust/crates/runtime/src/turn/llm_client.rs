//! Shared LLM calling utilities.
//!
//! Extracted from [`super::bridge_inprocess`] so both the in-process bridge
//! and [`crate::server::server_loop_host::ServerAgenticLoopHost`] can call LLMs
//! without duplicating the retry/backoff/parsing logic.

use std::{
    collections::HashMap,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::sse_blocks::SseBlankLineUtf8Buf;
use super::sse_data_lines::{
    drain_sse_data_lines, finish_sse_data_buffer, json_events_from_sse_event_block,
};
use crate::bridge::rate_limit_cooldown::{
    RateLimitAction, RateLimitCooldown, is_overload_status, is_rate_limit_status,
    parse_retry_after_ms,
};
use crate::prompts;

/// Maximum retries for transient LLM errors (429, 5xx, network).
pub(crate) const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
pub(crate) const LLM_RETRY_BASE_MS: u64 = 1000;
/// Stream idle watchdog: abort streaming if no chunk arrives within this time.
pub(crate) const STREAM_IDLE_TIMEOUT_MS: u64 = 90_000;

// ── Rate-Limit Cooldown ──────────────────────────────────────────────────────
use std::sync::OnceLock;

/// Global rate-limit cooldown tracker (shared across all LLM calls in this process).
fn rate_limit_cooldown() -> &'static RateLimitCooldown {
    static COOLDOWN: OnceLock<RateLimitCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(RateLimitCooldown::new)
}

// ── System Prompt Cache ──────────────────────────────────────────────────────
use std::sync::Mutex;

fn prompt_cache() -> &'static Mutex<HashMap<u64, String>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prompt_cache_key(
    tool_names: &[&str],
    task_type: Option<&str>,
    confidence: f64,
    profile_desc: &str,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in tool_names {
        name.hash(&mut hasher);
    }
    task_type.unwrap_or("none").hash(&mut hasher);
    let bucket = if confidence < 0.3 { "low" } else { "normal" };
    bucket.hash(&mut hasher);
    profile_desc.hash(&mut hasher);
    hasher.finish()
}

/// Build or retrieve a cached system prompt for the given tool+profile context.
pub(crate) fn cached_system_prompt(
    tool_names: &[&str],
    profile_desc: &str,
    confidence: f64,
    task_type: Option<&str>,
) -> String {
    let key = prompt_cache_key(tool_names, task_type, confidence, profile_desc);
    if let Ok(cache) = prompt_cache().lock()
        && let Some(cached) = cache.get(&key)
    {
        return cached.clone();
    }
    let prompt = prompts::build_main_system_prompt(tool_names, profile_desc, confidence, task_type);
    if let Ok(mut cache) = prompt_cache().lock() {
        if cache.len() > 32 {
            cache.clear();
        }
        cache.insert(key, prompt.clone());
    }
    prompt
}

/// Classify an LLM error message into a category for SSE error events.
pub(crate) fn classify_llm_error(msg: &str) -> &'static str {
    let lower = msg.to_lowercase();
    if lower.contains("rate") || lower.contains("429") {
        "rate_limit"
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout"
    } else if lower.contains("connect") || lower.contains("transport") || lower.contains("network")
    {
        "transport"
    } else if lower.contains("401") || lower.contains("unauthorized") || lower.contains("api key") {
        "permission"
    } else {
        "internal"
    }
}

/// Collected result from a single LLM streaming call.
#[derive(Debug, Clone, Default)]
pub(crate) struct LlmCallResult {
    pub full_text: String,
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub usage: Map<String, Value>,
    #[allow(dead_code)] // consumed by bridge_inprocess and explain telemetry
    pub model_used: String,
    #[allow(dead_code)]
    pub duration_ms: u64,
}

fn turn_timeout_s() -> f64 {
    mo_agent_core::RuntimeLimits::global().turn_timeout_s
}

/// Cooperative cancellation for [`call_llm_and_collect`] / [`collect_llm_stream`].
#[derive(Clone, Copy)]
pub(crate) enum LlmCancel<'a> {
    None,
    /// Cooperative cancel when the caller already owns a [`CancellationToken`].
    Token(&'a CancellationToken),
    Flag(&'a AtomicBool),
    /// User cancel (`AtomicBool`) plus a [`CancellationToken`] for immediate wake during LLM I/O.
    FlagAndToken(&'a AtomicBool, &'a CancellationToken),
}

impl LlmCancel<'_> {
    pub(crate) fn is_triggered(self) -> bool {
        match self {
            LlmCancel::None => false,
            LlmCancel::Token(t) => t.is_cancelled(),
            LlmCancel::Flag(f) => f.load(Ordering::Relaxed),
            LlmCancel::FlagAndToken(f, t) => {
                f.load(Ordering::Relaxed) || t.is_cancelled()
            }
        }
    }
}

/// Completes when cancellation is requested; otherwise never completes if [`LlmCancel::None`].
pub(crate) async fn wait_llm_cancel(cancel: LlmCancel<'_>) {
    match cancel {
        LlmCancel::None => std::future::pending().await,
        LlmCancel::Token(t) => t.cancelled().await,
        LlmCancel::Flag(f) => {
            const POLL: std::time::Duration = std::time::Duration::from_millis(50);
            while !f.load(Ordering::Relaxed) {
                tokio::time::sleep(POLL).await;
            }
        }
        LlmCancel::FlagAndToken(f, t) => {
            const POLL: std::time::Duration = std::time::Duration::from_millis(50);
            tokio::select! {
                biased;
                _ = t.cancelled() => {}
                _ = async {
                    while !f.load(Ordering::Relaxed) {
                        tokio::time::sleep(POLL).await;
                    }
                } => {}
            }
        }
    }
}

/// Sleep for rate-limit / cooldown delays unless [`LlmCancel`] fires first (cooperative abort).
pub(crate) async fn sleep_ms_or_llm_cancel(
    delay_ms: u64,
    cancel: LlmCancel<'_>,
) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = wait_llm_cancel(cancel) => Err("LLM call cancelled".to_string()),
        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => Ok(()),
    }
}

/// Per-chunk idle watchdog (Claude Code–style): no SSE JSON for this long → treat as stalled.
pub(crate) fn stream_idle_timeout() -> std::time::Duration {
    // Allow tests and deployments to override the idle watchdog.
    let ms = std::env::var("MO_STREAM_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(STREAM_IDLE_TIMEOUT_MS);
    std::time::Duration::from_millis(ms)
}

/// Call the LLM streaming API, collect the full response, and return a structured result.
///
/// Unlike `call_llm_stream` (which returns raw SSE bytes), this function
/// parses the stream and returns the aggregated `LlmCallResult` directly.
/// Used by `ServerAgenticLoopHost` for server-side agentic loops.
///
/// Records 429/529 errors for rate-limit cooldown tracking.
///
/// **Note**: Caller must check rate-limit cooldown state and handle fallback model
/// resolution BEFORE calling this function. This function only records errors
/// for cooldown tracking, not pre-checks.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_llm_and_collect(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    has_fallback: bool,
    cancel: LlmCancel<'_>,
) -> Result<LlmCallResult, String> {
    let cooldown = rate_limit_cooldown();

    let started = Instant::now();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(turn_timeout_s() as u64 + 10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut body = json!({
        "model": model_name,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });

    if let Some(max_out) = max_output_tokens {
        if provider == "anthropic" || model_name.contains("claude") {
            body["max_tokens"] = json!(max_out);
        } else {
            body["max_completion_tokens"] = json!(max_out);
        }
    }

    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = Value::String("auto".to_string());
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let mut last_err = String::new();
    for attempt in 0..=LLM_MAX_RETRIES {
        if cancel.is_triggered() {
            return Err("LLM call cancelled".to_string());
        }
        if attempt > 0 {
            let delay = LLM_RETRY_BASE_MS * (1 << (attempt - 1));
            tokio::select! {
                biased;
                _ = wait_llm_cancel(cancel) => return Err("LLM call cancelled".to_string()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
            }
        }

        let mut req = client.post(&url).header("content-type", "application/json");
        if provider == "anthropic" {
            req = req
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01");
        } else {
            req = req.header("authorization", format!("Bearer {api_key}"));
        }

        let response = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("LLM request failed: {e}");
                continue;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Success — record to cooldown tracker
            cooldown.record_success();
            let byte_stream = response.bytes_stream();
            match collect_llm_stream(byte_stream, model_name, started, cancel).await {
                Ok(result) => return Ok(result),
                Err(StreamCollectError::Cancelled) => {
                    return Err("LLM call cancelled".to_string());
                }
                Err(StreamCollectError::Transport(e)) => {
                    last_err = format!("LLM stream transport error: {e}");
                    continue;
                }
                Err(StreamCollectError::IdleTimeout { elapsed_ms }) => {
                    if cancel.is_triggered() {
                        return Err("LLM call cancelled".to_string());
                    }
                    // Abort streaming and fall back to non-stream request (single response).
                    mo_agent_core::agent_warn!(
                        "llm",
                        "stream idle timeout after {}ms — attempting non-stream fallback",
                        elapsed_ms
                    );
                    return call_llm_nonstream_fallback(
                        &client,
                        messages,
                        tools,
                        model_name,
                        api_key,
                        base_url,
                        provider,
                        max_output_tokens,
                    )
                    .await;
                }
            }
        }

        // Parse retry-after header
        let headers = response.headers();
        let retry_after_ms = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);

        let text = response.text().await.unwrap_or_default();
        last_err = format!("LLM error {status}: {text}");

        // Record rate-limit errors to cooldown tracker
        if is_rate_limit_status(status) {
            let action = cooldown.record_429(retry_after_ms, has_fallback);
            mo_agent_core::agent_warn!(
                "llm",
                "rate limit (429): action={:?}, metrics={:?}",
                action,
                cooldown.metrics()
            );
            if let RateLimitAction::WaitAndRetry { delay_ms } = action {
                sleep_ms_or_llm_cancel(delay_ms, cancel).await?;
            }
            continue;
        }

        if is_overload_status(status) {
            let action = cooldown.record_529(retry_after_ms, has_fallback);
            mo_agent_core::agent_warn!(
                "llm",
                "server overload ({status}): action={:?}, metrics={:?}",
                action,
                cooldown.metrics()
            );
            if let RateLimitAction::WaitAndRetry { delay_ms } = action {
                sleep_ms_or_llm_cancel(delay_ms, cancel).await?;
            }
            continue;
        }

        // Other 5xx errors are retryable
        if status >= 500 {
            continue;
        }

        return Err(last_err);
    }

    Err(format!("{last_err} (after {} retries)", LLM_MAX_RETRIES))
}

/// Parse an OpenAI-compatible SSE stream and collect into `LlmCallResult`.
async fn collect_llm_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
) -> Result<LlmCallResult, StreamCollectError> {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls_map: HashMap<usize, Map<String, Value>> = HashMap::new();
    let mut usage = Map::new();

    let sse = parse_openai_sse_json_stream(stream);
    tokio::pin!(sse);

    let idle = stream_idle_timeout();
    loop {
        let item = tokio::select! {
            biased;
            _ = wait_llm_cancel(cancel) => return Err(StreamCollectError::Cancelled),
            r = tokio::time::timeout(idle, sse.next()) => match r {
                Ok(v) => v,
                Err(_elapsed) => {
                    return Err(StreamCollectError::IdleTimeout {
                        elapsed_ms: idle.as_millis() as u64,
                    });
                }
            },
        };
        let Some(item) = item else { break };
        let chunk = match item {
            Ok(v) => v,
            Err(e) => return Err(StreamCollectError::Transport(e)),
        };
        // Parse usage from any chunk
        if let Some(u) = chunk.get("usage").and_then(Value::as_object) {
            let prompt = u.get("prompt_tokens").and_then(Value::as_i64);
            let completion = u.get("completion_tokens").and_then(Value::as_i64);
            if prompt.is_some() || completion.is_some() {
                let mut usage_map = Map::new();
                if let Some(value) = prompt {
                    usage_map.insert("prompt".to_string(), Value::from(value));
                }
                if let Some(value) = completion {
                    usage_map.insert("completion".to_string(), Value::from(value));
                }
                if let (Some(p), Some(c)) = (prompt, completion) {
                    usage_map.insert("total".to_string(), Value::from(p + c));
                }
                usage = usage_map;
            }
        }

        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            continue;
        };
        let Some(delta) = choices
            .first()
            .and_then(|c| c.get("delta"))
            .and_then(Value::as_object)
        else {
            continue;
        };

        // Text content
        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            full_text.push_str(content);
        }

        // Reasoning
        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str)
            && !r.is_empty()
        {
            reasoning.push_str(r);
        }

        // Tool calls (streaming accumulation)
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let entry = tool_calls_map.entry(idx).or_insert_with(|| {
                    Map::from_iter([
                        ("id".to_string(), Value::String(String::new())),
                        ("type".to_string(), Value::String("function".to_string())),
                        ("function".to_string(), json!({"name": "", "arguments": ""})),
                    ])
                });
                if let Some(id) = tc.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    entry.insert("id".to_string(), Value::String(id.to_string()));
                }
                if let Some(func) = tc.get("function").and_then(Value::as_object) {
                    let f = entry
                        .entry("function".to_string())
                        .or_insert_with(|| json!({}));
                    let Some(f) = f.as_object_mut() else {
                        continue;
                    };
                    if let Some(name) = func.get("name").and_then(Value::as_str)
                        && !name.is_empty()
                    {
                        f.insert("name".to_string(), Value::String(name.to_string()));
                    }
                    if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                        let existing = f
                            .entry("arguments".to_string())
                            .or_insert_with(|| Value::String(String::new()));
                        if let Value::String(s) = existing {
                            s.push_str(args);
                        }
                    }
                }
            }
        }
    }

    let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
    sorted_tcs.sort_by_key(|(idx, _)| *idx);
    let tool_calls: Vec<Value> = sorted_tcs
        .into_iter()
        .map(|(_, v)| Value::Object(v))
        .collect();

    Ok(LlmCallResult {
        full_text,
        reasoning,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[derive(Debug)]
#[allow(dead_code)] // Transport variant reserved for future network error handling
enum StreamCollectError {
    IdleTimeout { elapsed_ms: u64 },
    /// Byte stream error from the HTTP client (e.g. reset, TLS failure).
    Transport(String),
    /// [`LlmCancel`] fired during collection.
    Cancelled,
}

/// For `tokio::select!`: completes when `cancel` fires, or never if `cancel` is `None`.
pub(crate) async fn wait_until_cancelled_or_pending(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Optional: cancel in-flight collection when this drops (e.g. SSE response body dropped).
pub(crate) struct CancelOnClientDisconnect {
    token: Arc<CancellationToken>,
}

impl CancelOnClientDisconnect {
    pub(crate) fn new(token: Arc<CancellationToken>) -> Self {
        Self { token }
    }
}

impl Drop for CancelOnClientDisconnect {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_llm_nonstream_fallback(
    client: &reqwest::Client,
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
) -> Result<LlmCallResult, String> {
    let started = Instant::now();
    let mut body = json!({
        "model": model_name,
        "messages": messages,
        "stream": false,
    });
    if let Some(max_out) = max_output_tokens {
        if provider == "anthropic" || model_name.contains("claude") {
            body["max_tokens"] = json!(max_out);
        } else {
            body["max_completion_tokens"] = json!(max_out);
        }
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = Value::String("auto".to_string());
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut req = client.post(&url).header("content-type", "application/json");
    if provider == "anthropic" {
        req = req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("authorization", format!("Bearer {api_key}"));
    }

    let resp = req.json(&body).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("LLM fallback error {status}: {text}"));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(parse_nonstream_response(&v, model_name, started))
}

fn parse_nonstream_response(v: &Value, model_name: &str, started: Instant) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = Map::new();

    if let Some(u) = v.get("usage").and_then(Value::as_object) {
        if let Some(p) = u.get("prompt_tokens").and_then(Value::as_i64) {
            usage.insert("prompt".to_string(), Value::from(p));
        }
        if let Some(c) = u.get("completion_tokens").and_then(Value::as_i64) {
            usage.insert("completion".to_string(), Value::from(c));
        }
        if let (Some(p), Some(c)) = (
            u.get("prompt_tokens").and_then(Value::as_i64),
            u.get("completion_tokens").and_then(Value::as_i64),
        ) {
            usage.insert("total".to_string(), Value::from(p + c));
        }
    }

    if let Some(choice) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        && let Some(msg) = choice.get("message").and_then(Value::as_object)
    {
        if let Some(content) = msg.get("content").and_then(Value::as_str) {
            full_text = content.to_string();
        }
        if let Some(r) = msg.get("reasoning_content").and_then(Value::as_str) {
            reasoning = r.to_string();
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
            tool_calls = tcs.clone();
        }
    }

    LlmCallResult {
        full_text,
        reasoning,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// Parse OpenAI-style SSE bytes into JSON event values. Transport errors surface as `Err`.
pub(crate) fn parse_openai_sse_json_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
) -> impl futures_util::Stream<Item = Result<Value, String>> + Send + 'static {
    async_stream::stream! {
        let mut sse_in = SseBlankLineUtf8Buf::new();
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    yield Err(e.to_string());
                    return;
                }
            };
            for block in sse_in.push_lossy_bytes(&bytes) {
                let d = json_events_from_sse_event_block(&block);
                for v in d.events {
                    yield Ok(v);
                }
                if d.stream_finished {
                    return;
                }
            }
        }
        let mut buf = sse_in.into_inner();
        let tail = drain_sse_data_lines(&mut buf, "");
        for v in tail.events {
            yield Ok(v);
        }
        if tail.stream_finished {
            return;
        }
        let fin = finish_sse_data_buffer(&mut buf);
        for v in fin.events {
            yield Ok(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_sleeps_when_no_cancel_source() {
        let r = sleep_ms_or_llm_cancel(8, LlmCancel::None).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_aborts_on_token() {
        let token = CancellationToken::new();
        let token_for_wait = token.clone();
        let h = tokio::spawn(async move {
            sleep_ms_or_llm_cancel(60_000, LlmCancel::Token(&token_for_wait)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let r = h.await.expect("join");
        assert_eq!(r.expect_err("cancelled"), "LLM call cancelled");
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_aborts_on_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_signal = flag.clone();
        let flag_for_wait = flag.clone();
        let h = tokio::spawn(async move {
            sleep_ms_or_llm_cancel(60_000, LlmCancel::Flag(flag_for_wait.as_ref())).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flag_signal.store(true, Ordering::SeqCst);
        let r = h.await.expect("join");
        assert_eq!(r.expect_err("cancelled"), "LLM call cancelled");
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_aborts_flag_and_token_via_token() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_join = flag.clone();
        let token = CancellationToken::new();
        let token_for_wait = token.clone();
        let h = tokio::spawn(async move {
            sleep_ms_or_llm_cancel(
                60_000,
                LlmCancel::FlagAndToken(flag.as_ref(), &token_for_wait),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let r = h.await.expect("join");
        assert_eq!(r.expect_err("cancelled"), "LLM call cancelled");
        assert!(!flag_for_join.load(Ordering::SeqCst));
    }

    #[test]
    fn llm_cancel_is_triggered_matrix() {
        assert!(!LlmCancel::None.is_triggered());

        let flag_off = Arc::new(AtomicBool::new(false));
        assert!(!LlmCancel::Flag(flag_off.as_ref()).is_triggered());
        flag_off.store(true, Ordering::SeqCst);
        assert!(LlmCancel::Flag(flag_off.as_ref()).is_triggered());

        let token = CancellationToken::new();
        assert!(!LlmCancel::Token(&token).is_triggered());
        token.cancel();
        assert!(LlmCancel::Token(&token).is_triggered());

        let flag2 = Arc::new(AtomicBool::new(false));
        let token2 = CancellationToken::new();
        assert!(!LlmCancel::FlagAndToken(flag2.as_ref(), &token2).is_triggered());
        token2.cancel();
        assert!(LlmCancel::FlagAndToken(flag2.as_ref(), &token2).is_triggered());

        let flag3 = Arc::new(AtomicBool::new(true));
        let token3 = CancellationToken::new();
        assert!(LlmCancel::FlagAndToken(flag3.as_ref(), &token3).is_triggered());
    }

    #[test]
    fn classify_llm_error_categories() {
        assert_eq!(classify_llm_error("rate limit exceeded"), "rate_limit");
        assert_eq!(
            classify_llm_error("error 429: too many requests"),
            "rate_limit"
        );
        assert_eq!(classify_llm_error("request timed out"), "timeout");
        assert_eq!(classify_llm_error("connection refused"), "transport");
        assert_eq!(classify_llm_error("401 unauthorized"), "permission");
        assert_eq!(classify_llm_error("something went wrong"), "internal");
    }

    #[test]
    fn cached_system_prompt_is_deterministic() {
        let p1 = cached_system_prompt(&["bash"], "", 0.8, Some("code"));
        let p2 = cached_system_prompt(&["bash"], "", 0.8, Some("code"));
        assert_eq!(p1, p2);
    }

    #[test]
    fn cached_system_prompt_varies_by_profile() {
        let p1 = cached_system_prompt(&["bash"], "", 0.8, Some("code"));
        let p2 = cached_system_prompt(&["bash"], "cwd: /tmp", 0.8, Some("code"));
        assert_ne!(p1, p2);
    }

    #[test]
    fn cached_system_prompt_varies_by_confidence_bucket() {
        let low = cached_system_prompt(&["bash"], "", 0.1, None);
        let normal = cached_system_prompt(&["bash"], "", 0.5, None);
        assert_ne!(low, normal);
    }

    #[test]
    fn llm_call_result_default() {
        let r = LlmCallResult::default();
        assert!(r.full_text.is_empty());
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.duration_ms, 0);
    }

    #[test]
    fn parse_nonstream_response_extracts_fields() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "hello",
                    "reasoning_content": "think",
                    "tool_calls": [{"id":"t1","type":"function","function":{"name":"bash","arguments":"{}"}}]
                }
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        });
        let r = parse_nonstream_response(&v, "test-model", Instant::now());
        assert_eq!(r.full_text, "hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.usage.get("total").and_then(Value::as_i64), Some(15));
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_split_chunks() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from("data: ")),
            Ok(Bytes::from(r#"{"t":1}"#)),
            Ok(Bytes::from("\n\n")),
        ];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let ev = st.next().await.unwrap().unwrap();
        assert_eq!(ev, json!({"t": 1}));
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_done_terminates() {
        let body = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::copy_from_slice(body.as_bytes()))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let e1 = st.next().await.unwrap().unwrap();
        assert_eq!(e1, json!({"a": 1}));
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_idle_timeout_triggers() {
        // Keep this test fast: override idle timeout to 1ms.
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "1") };
        // Stream that never yields any bytes (simulates a hung connection).
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let res = collect_llm_stream(pending_stream, "test-model", started, LlmCancel::None).await;
        assert!(
            matches!(res, Err(StreamCollectError::IdleTimeout { .. })),
            "expected idle timeout, got: {res:?}"
        );
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    async fn collect_llm_stream_respects_cancel_flag() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let flag = Arc::new(AtomicBool::new(false));
        let flag_signal = flag.clone();
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            collect_llm_stream(
                pending_stream,
                "test-model",
                started,
                LlmCancel::Flag(flag.as_ref()),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flag_signal.store(true, Ordering::SeqCst);
        let res = handle.await.expect("join");
        assert!(
            matches!(res, Err(StreamCollectError::Cancelled)),
            "expected cancel, got: {res:?}"
        );
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    async fn collect_llm_stream_respects_cancel_token() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let token = CancellationToken::new();
        let token_for_stream = token.clone();
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            collect_llm_stream(
                pending_stream,
                "test-model",
                started,
                LlmCancel::Token(&token_for_stream),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let res = handle.await.expect("join");
        assert!(
            matches!(res, Err(StreamCollectError::Cancelled)),
            "expected cancel, got: {res:?}"
        );
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    async fn collect_llm_stream_flag_and_token_cancels_on_token() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_join = flag.clone();
        let token = CancellationToken::new();
        let token_signal = token.clone();
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            collect_llm_stream(
                pending_stream,
                "test-model",
                started,
                LlmCancel::FlagAndToken(flag.as_ref(), &token_signal),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let res = handle.await.expect("join");
        assert!(
            matches!(res, Err(StreamCollectError::Cancelled)),
            "expected cancel, got: {res:?}"
        );
        assert!(!flag_for_join.load(Ordering::SeqCst));
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }
}
