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
    PerModelCooldown, RateLimitAction, is_overload_status, is_rate_limit_status,
    parse_retry_after_ms,
};
use crate::output_style::current_output_style;
use crate::prompts;

/// Maximum retries for transient LLM errors (429, 5xx, network).
pub(crate) const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
pub(crate) const LLM_RETRY_BASE_MS: u64 = 1000;
/// Extended delay for TPM (tokens per minute) exhaustion (60 seconds).
/// TPM limits typically reset after 60 seconds, so we wait longer.
const TPM_EXHAUST_DELAY_MS: u64 = 60_000;
/// Maximum retries for TPM exhaustion (longer recovery period).
const TPM_MAX_RETRIES: u32 = 5;
/// TCP connect timeout for LLM API requests (seconds). Override: `MO_LLM_CONNECT_TIMEOUT_S`.
const LLM_CONNECT_TIMEOUT_S: u64 = 30;
/// Non-stream fallback hard timeout (seconds). Override: `MO_LLM_FALLBACK_TIMEOUT_S`.
const LLM_FALLBACK_TIMEOUT_S: u64 = 120;
/// Total budget across all retries + fallback for a single LLM call (seconds).
/// Override: `MO_LLM_TOTAL_BUDGET_S`.
const LLM_TOTAL_BUDGET_S: u64 = 300;

// ── Rate-Limit Cooldown ──────────────────────────────────────────────────────
use std::sync::OnceLock;

/// Per-model rate-limit cooldown tracker.
fn rate_limit_cooldown() -> &'static PerModelCooldown {
    static COOLDOWN: OnceLock<PerModelCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(PerModelCooldown::new)
}

// ── Global HTTP Client ───────────────────────────────────────────────────────

/// Global HTTP client for LLM requests (connection pooling, reuse).
fn global_llm_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(llm_connect_timeout())
            // Use a generous timeout; per-request timeout handled via tokio::time::timeout
            .timeout(std::time::Duration::from_secs(LLM_TOTAL_BUDGET_S + 60))
            .pool_max_idle_per_host(4)
            .build()
            .expect("failed to build global LLM HTTP client")
    })
}

#[cfg(test)]
fn reset_rate_limit_cooldown_for_tests() {
    rate_limit_cooldown().reset_for_tests();
}

/// Returns `true` if `name` looks like a valid tool function name.
///
/// LLM providers sometimes return malformed tool calls when the model leaks XML-style
/// thinking tags (e.g., `<reflect>`) into tool call blocks. We reject names that:
/// - are empty
/// - contain `<` or `>` (XML artifact)
/// - contain whitespace
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('<')
        && !name.contains('>')
        && !name.chars().any(char::is_whitespace)
}

// ── System Prompt ─────────────────────────────────────────────────────────

/// Build a system prompt for the given tool+profile context.
/// The underlying section builders are cached in bridge_inprocess::section_cache.
pub(crate) fn cached_system_prompt(
    tool_names: &[&str],
    profile_desc: &str,
    confidence: f64,
    task_type: Option<&str>,
) -> String {
    prompts::build_main_system_prompt_with_style(
        tool_names,
        profile_desc,
        confidence,
        task_type,
        current_output_style(),
    )
}

/// Classify an LLM error message into an [`ErrorKind`].
///
/// Used only for legacy callers that still have string errors. New code should
/// construct [`ClassifiedError`] at the source.
pub(crate) fn classify_llm_error(msg: &str) -> astra_core::ErrorKind {
    let lower = msg.to_lowercase();
    if is_context_window_error(&lower) {
        astra_core::ErrorKind::ContextWindow
    } else if lower.contains("rate") || lower.contains("429") {
        astra_core::ErrorKind::RateLimit
    } else if lower.contains("timeout") || lower.contains("timed out") {
        astra_core::ErrorKind::StreamIdle
    } else if lower.contains("connect") || lower.contains("transport") || lower.contains("network")
    {
        astra_core::ErrorKind::StreamTransport
    } else if lower.contains("401") || lower.contains("unauthorized") || lower.contains("api key") {
        astra_core::ErrorKind::Auth
    } else if lower.contains("cancelled") || lower.contains("canceled") {
        astra_core::ErrorKind::Cancelled
    } else {
        astra_core::ErrorKind::Unknown
    }
}

/// Detect context-window / prompt-too-long errors in API responses.
pub(crate) fn is_context_window_error(lower: &str) -> bool {
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("too many tokens")
        || lower.contains("input is too long")
        || lower.contains("context window")
        || lower.contains("max_tokens") && (lower.contains("exceed") || lower.contains("limit"))
}

/// Detect TPM (tokens per minute) exhaustion errors.
///
/// TPM errors require longer wait times because they indicate the account-level
/// token quota has been exhausted. These typically reset after 60 seconds.
fn is_tpm_exhaustion(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    (lower.contains("tpm") && (lower.contains("exceed") || lower.contains("limit")))
        || lower.contains("tokens per minute")
        || lower.contains("rate limit exceeded") && lower.contains("token")
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
    /// The finish_reason from the last SSE choice (e.g. "stop", "length", "tool_calls").
    /// `None` when the stream ended without an explicit finish_reason.
    pub finish_reason: Option<String>,
}

#[allow(dead_code)] // May be used for per-request timeout in the future
fn turn_timeout_s() -> f64 {
    astra_core::RuntimeLimits::global().turn_timeout_s
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
            LlmCancel::FlagAndToken(f, t) => f.load(Ordering::Relaxed) || t.is_cancelled(),
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
) -> Result<(), astra_core::ClassifiedError> {
    tokio::select! {
        biased;
        _ = wait_llm_cancel(cancel) => Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "LLM call cancelled",
        )),
        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => Ok(()),
    }
}

/// Per-chunk idle watchdog (pre-progress): no SSE JSON for this long → treat as stalled.
/// Delegate to the canonical public function in sse_stream_host.
pub(crate) fn stream_idle_timeout() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_STREAM_IDLE_TIMEOUT.with(|c| *c.borrow()) {
        return d;
    }
    crate::turn::sse_stream_host::stream_idle_timeout()
}

/// Per-chunk idle watchdog (post-progress): once at least one SSE chunk has been
/// received, allow a longer idle window to accommodate thinking/reasoning pauses.
pub(crate) fn stream_idle_timeout_after_progress() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| *c.borrow()) {
        return d;
    }
    crate::turn::sse_stream_host::stream_idle_timeout_after_progress()
}

#[cfg(test)]
thread_local! {
    static TEST_STREAM_IDLE_TIMEOUT: std::cell::RefCell<Option<std::time::Duration>> =
        const { std::cell::RefCell::new(None) };
    static TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS: std::cell::RefCell<Option<std::time::Duration>> =
        const { std::cell::RefCell::new(None) };
}

/// TCP connect timeout for LLM API requests.
pub(crate) fn llm_connect_timeout() -> std::time::Duration {
    let s = std::env::var("MO_LLM_CONNECT_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(LLM_CONNECT_TIMEOUT_S);
    std::time::Duration::from_secs(s)
}

/// Hard timeout for the non-stream fallback request.
pub(crate) fn llm_fallback_timeout() -> std::time::Duration {
    let s = std::env::var("MO_LLM_FALLBACK_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(LLM_FALLBACK_TIMEOUT_S);
    std::time::Duration::from_secs(s)
}

/// Total budget across all retries + fallback for a single LLM call.
fn llm_total_budget() -> std::time::Duration {
    let s = std::env::var("MO_LLM_TOTAL_BUDGET_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(LLM_TOTAL_BUDGET_S);
    std::time::Duration::from_secs(s)
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
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let cooldown = rate_limit_cooldown();
    let model_key = model_name;

    let started = Instant::now();
    let total_budget = llm_total_budget();
    let client = global_llm_client();

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
    let mut last_kind = astra_core::ErrorKind::Unknown;
    let mut tpm_exhaustion_detected = false;
    // Track idle timeouts for the retry-before-fallback logic.
    // This counter is NOT reset inside the retry loop: the first idle timeout across
    // all retries (rate-limit, network, etc.) only gets a streaming retry if the
    // stream never produced any useful output. Once a stream has emitted partial
    // content/tool calls and then stalls, fall back immediately instead of redoing
    // the same partial generation in a second streaming attempt.
    let mut idle_timeout_count = 0u32;
    let max_retries = LLM_MAX_RETRIES;
    // Read idle timeouts once before the retry loop to avoid env-var races between
    // parallel tests (and to ensure consistent timeouts across retries).
    let idle_pre = stream_idle_timeout();
    let idle_post = stream_idle_timeout_after_progress();

    for attempt in 0..=max_retries {
        // Extend retries if TPM exhaustion was detected (account-level limit)
        let effective_max = if tpm_exhaustion_detected {
            TPM_MAX_RETRIES
        } else {
            max_retries
        };
        if attempt > effective_max {
            break;
        }

        if cancel.is_triggered() {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "LLM call cancelled",
            ));
        }
        // Total budget guard: abort if we've already spent too long across retries.
        if started.elapsed() > total_budget {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::BudgetExhausted,
                format!(
                    "LLM total budget exhausted ({:.0}s): {last_err}",
                    total_budget.as_secs_f64()
                ),
            ));
        }
        if attempt > 0 {
            // Use longer delay for TPM exhaustion (60s) vs standard exponential (1s, 2s, 4s)
            let delay = if tpm_exhaustion_detected {
                TPM_EXHAUST_DELAY_MS
            } else {
                LLM_RETRY_BASE_MS * (1 << (attempt - 1))
            };
            if tpm_exhaustion_detected {
                astra_core::agent_warn!(
                    "llm",
                    "TPM exhaustion detected, waiting {}s before retry {}/{}",
                    delay / 1000,
                    attempt,
                    TPM_MAX_RETRIES
                );
            }
            tokio::select! {
                biased;
                _ = wait_llm_cancel(cancel) => return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "LLM call cancelled",
                )),
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
                last_kind = astra_core::ErrorKind::Network;
                continue;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Success — record to cooldown tracker
            cooldown.with(model_key, |c| c.record_success());
            let byte_stream = response.bytes_stream();
            match collect_llm_stream(
                byte_stream,
                model_name,
                started,
                cancel,
                idle_pre,
                idle_post,
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(StreamCollectError::Cancelled) => {
                    return Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::Cancelled,
                        "LLM call cancelled",
                    ));
                }
                Err(StreamCollectError::Transport(e)) => {
                    last_err = format!("LLM stream transport error: {e}");
                    last_kind = astra_core::ErrorKind::StreamTransport;
                    continue;
                }
                Err(StreamCollectError::IdleTimeout {
                    elapsed_ms,
                    made_progress,
                }) => {
                    if cancel.is_triggered() {
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "LLM call cancelled",
                        ));
                    }
                    // Check total budget before attempting retry/fallback.
                    let elapsed = started.elapsed();
                    if elapsed > total_budget {
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::BudgetExhausted,
                            format!(
                                "LLM total budget exhausted ({:.0}s) after stream idle timeout",
                                total_budget.as_secs_f64()
                            ),
                        ));
                    }

                    idle_timeout_count += 1;

                    // Only retry streaming if the connection stalled before any
                    // meaningful output arrived. Once partial content/tool calls
                    // have streamed, a second streaming attempt tends to replay
                    // the same partial work and wastes another idle window.
                    if idle_timeout_count == 1 && !made_progress {
                        astra_core::agent_warn!(
                            "llm",
                            "stream idle timeout after {}ms — retrying streaming once before fallback",
                            elapsed_ms
                        );
                        last_err = format!("stream idle timeout after {elapsed_ms}ms");
                        last_kind = astra_core::ErrorKind::StreamIdle;
                        continue; // retry streaming
                    }

                    // Mid-stream stall, or second idle timeout — fall back to a non-stream request.
                    // Cap the fallback timeout at min(fallback_timeout, remaining budget).
                    let remaining = total_budget.saturating_sub(elapsed);
                    let fb_timeout = llm_fallback_timeout().min(remaining);
                    astra_core::agent_warn!(
                        "llm",
                        "stream idle timeout #{} after {}ms (made_progress={}) — attempting non-stream fallback (timeout {}s)",
                        idle_timeout_count,
                        elapsed_ms,
                        made_progress,
                        fb_timeout.as_secs()
                    );
                    return call_llm_nonstream_fallback(
                        client,
                        messages,
                        tools,
                        model_name,
                        api_key,
                        base_url,
                        provider,
                        max_output_tokens,
                        fb_timeout,
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
            last_kind = astra_core::ErrorKind::RateLimit;

            // Detect TPM exhaustion for extended retry behavior
            if is_tpm_exhaustion(&text) && !tpm_exhaustion_detected {
                tpm_exhaustion_detected = true;
                astra_core::agent_warn!(
                    "llm",
                    "TPM exhaustion detected on {} — extending retry with {}s cooldown",
                    model_key,
                    TPM_EXHAUST_DELAY_MS / 1000
                );
            }

            let action = cooldown.with(model_key, |c| c.record_429(retry_after_ms, has_fallback));
            astra_core::agent_warn!(
                "llm",
                "rate limit (429) on {}: action={:?}",
                model_key,
                action,
            );
            if let RateLimitAction::WaitAndRetry { delay_ms } = action {
                // For TPM exhaustion, use longer delay
                let actual_delay = if tpm_exhaustion_detected {
                    delay_ms.max(TPM_EXHAUST_DELAY_MS)
                } else {
                    delay_ms
                };
                sleep_ms_or_llm_cancel(actual_delay, cancel).await?;
            }
            continue;
        }

        if is_overload_status(status) {
            last_kind = astra_core::ErrorKind::ServerError;
            let action = cooldown.with(model_key, |c| c.record_529(retry_after_ms, has_fallback));
            astra_core::agent_warn!(
                "llm",
                "server overload ({status}) on {}: action={:?}",
                model_key,
                action,
            );
            if let RateLimitAction::WaitAndRetry { delay_ms } = action {
                sleep_ms_or_llm_cancel(delay_ms, cancel).await?;
            }
            continue;
        }

        // Other 5xx errors are retryable
        if status >= 500 {
            last_kind = astra_core::ErrorKind::ServerError;
            continue;
        }

        // Context-window errors — classified at source, no string prefix needed.
        if status == 400 && is_context_window_error(&text.to_lowercase()) {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContextWindow,
                last_err,
            ));
        }

        // Auth errors
        if status == 401 || status == 403 {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Auth,
                last_err,
            ));
        }

        // Other 400 errors
        if status == 400 {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::InvalidRequest,
                last_err,
            ));
        }

        return Err(astra_core::ClassifiedError::new(last_kind, last_err));
    }

    let retries_used = if tpm_exhaustion_detected {
        TPM_MAX_RETRIES
    } else {
        LLM_MAX_RETRIES
    };
    Err(astra_core::ClassifiedError::new(
        last_kind,
        format!("{last_err} (after {} retries)", retries_used),
    ))
}

/// Maximum accumulated response size (text + reasoning + args) before aborting stream (16 MB).
const MAX_STREAM_ACCUMULATION_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of tool calls per LLM stream response.
const MAX_STREAM_TOOL_CALLS: usize = 128;

/// Parse an OpenAI-compatible SSE stream and collect into `LlmCallResult`.
async fn collect_llm_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_pre: std::time::Duration,
    idle_post: std::time::Duration,
) -> Result<LlmCallResult, StreamCollectError> {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls_map: HashMap<usize, Map<String, Value>> = HashMap::new();
    let mut usage = Map::new();
    let mut finish_reason: Option<String> = None;
    let mut accumulated_bytes: usize = 0;
    let mut made_progress = false;

    let sse = parse_openai_sse_json_stream(stream);
    tokio::pin!(sse);
    loop {
        let idle = if made_progress { idle_post } else { idle_pre };
        let item = tokio::select! {
            biased;
            _ = wait_llm_cancel(cancel) => return Err(StreamCollectError::Cancelled),
            r = tokio::time::timeout(idle, sse.next()) => match r {
                Ok(v) => v,
                Err(_elapsed) => {
                    return Err(StreamCollectError::IdleTimeout {
                        elapsed_ms: idle.as_millis() as u64,
                        made_progress,
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
                made_progress = true;
            }
        }

        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            continue;
        };

        // Extract finish_reason from the last chunk that carries one.
        if let Some(fr) = choices
            .first()
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
        {
            finish_reason = Some(fr.to_string());
            made_progress = true;
        }

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
            accumulated_bytes += content.len();
            if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                return Err(StreamCollectError::Transport(format!(
                    "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                )));
            }
            full_text.push_str(content);
            made_progress = true;
        }

        // Reasoning
        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str)
            && !r.is_empty()
        {
            accumulated_bytes += r.len();
            if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                return Err(StreamCollectError::Transport(format!(
                    "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                )));
            }
            reasoning.push_str(r);
            made_progress = true;
        }

        // Tool calls (streaming accumulation)
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if tool_calls_map.len() >= MAX_STREAM_TOOL_CALLS
                    && !tool_calls_map.contains_key(&idx)
                {
                    astra_core::agent_warn!(
                        "llm",
                        "stream tool_calls exceeded {MAX_STREAM_TOOL_CALLS} — ignoring extra"
                    );
                    continue;
                }
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
                    made_progress = true;
                }
                if let Some(func) = tc.get("function").and_then(Value::as_object) {
                    let f = entry
                        .entry("function".to_string())
                        .or_insert_with(|| json!({}));
                    let Some(f) = f.as_object_mut() else {
                        continue;
                    };
                    if let Some(name) = func.get("name").and_then(Value::as_str)
                        && is_valid_tool_name(name)
                    {
                        f.insert("name".to_string(), Value::String(name.to_string()));
                        made_progress = true;
                    } else if let Some(bad_name) = func.get("name").and_then(Value::as_str) {
                        astra_core::agent_warn!(
                            "llm",
                            "dropped malformed tool_call with invalid name: {bad_name:?}"
                        );
                    }
                    if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                        accumulated_bytes += args.len();
                        if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                            return Err(StreamCollectError::Transport(format!(
                                "stream tool-call arguments exceeded {MAX_STREAM_ACCUMULATION_BYTES} byte limit"
                            )));
                        }
                        let existing = f
                            .entry("arguments".to_string())
                            .or_insert_with(|| Value::String(String::new()));
                        if let Value::String(s) = existing {
                            s.push_str(args);
                            made_progress = true;
                        }
                    }
                }
            }
        }
    }

    let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
    sorted_tcs.sort_by_key(|(idx, _)| *idx);
    let mut tool_calls: Vec<Value> = sorted_tcs
        .into_iter()
        .map(|(_, v)| Value::Object(v))
        .collect();

    // Degraded tool-call fallback: some models emit <invoke> XML or <tool_call>
    // tags in content instead of structured tool_calls. Recover them.
    if tool_calls.is_empty() {
        if let Some(parsed) = super::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text) {
            astra_core::agent_warn!(
                "llm",
                "recovered {} tool call(s) from degraded text in content (stream)",
                parsed.len()
            );
            full_text = super::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
            tool_calls = parsed;
        }
    }

    Ok(LlmCallResult {
        full_text,
        reasoning,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason,
    })
}

#[derive(Debug)]
#[allow(dead_code)] // Transport variant reserved for future network error handling
enum StreamCollectError {
    IdleTimeout {
        elapsed_ms: u64,
        made_progress: bool,
    },
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
    timeout: std::time::Duration,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
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

    // Apply per-request timeout (overrides the client-level default).
    let resp = req.timeout(timeout).json(&body).send().await.map_err(|e| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Network,
            format!(
                "LLM fallback request failed (timeout {}s): {e}",
                timeout.as_secs()
            ),
        )
    })?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let kind = if status == 401 || status == 403 {
            astra_core::ErrorKind::Auth
        } else if is_rate_limit_status(status) {
            astra_core::ErrorKind::RateLimit
        } else if status >= 500 {
            astra_core::ErrorKind::ServerError
        } else if status == 400 && is_context_window_error(&text.to_lowercase()) {
            astra_core::ErrorKind::ContextWindow
        } else if status == 400 {
            astra_core::ErrorKind::InvalidRequest
        } else {
            astra_core::ErrorKind::Unknown
        };
        return Err(astra_core::ClassifiedError::new(
            kind,
            format!("LLM fallback error {status}: {text}"),
        ));
    }
    let v: Value = resp.json().await.map_err(|e| {
        astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, e.to_string())
    })?;
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

    let finish_reason = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map(String::from);

    // Degraded tool-call fallback: same recovery for non-stream responses.
    if tool_calls.is_empty() {
        if let Some(parsed) = super::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text) {
            astra_core::agent_warn!(
                "llm",
                "recovered {} tool call(s) from degraded text in content (non-stream)",
                parsed.len()
            );
            full_text = super::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
            tool_calls = parsed;
        }
    }

    LlmCallResult {
        full_text,
        reasoning,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason,
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
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::post;
    use futures_util::StreamExt;
    use futures_util::stream;
    use serde_json::json;
    use serial_test::serial;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Set thread-local stream idle timeouts for the duration of a test.
    /// Returns a guard that resets them on drop.
    fn set_test_stream_timeouts(pre_ms: u64, post_ms: Option<u64>) -> impl Drop {
        TEST_STREAM_IDLE_TIMEOUT.with(|c| {
            *c.borrow_mut() = Some(std::time::Duration::from_millis(pre_ms));
        });
        if let Some(post) = post_ms {
            TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| {
                *c.borrow_mut() = Some(std::time::Duration::from_millis(post));
            });
        }
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                TEST_STREAM_IDLE_TIMEOUT.with(|c| *c.borrow_mut() = None);
                TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| *c.borrow_mut() = None);
            }
        }
        Guard
    }

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
        assert_eq!(
            r.expect_err("cancelled").kind,
            astra_core::ErrorKind::Cancelled
        );
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
        assert_eq!(
            r.expect_err("cancelled").kind,
            astra_core::ErrorKind::Cancelled
        );
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
        assert_eq!(
            r.expect_err("cancelled").kind,
            astra_core::ErrorKind::Cancelled
        );
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

    // ── Timeout configuration tests ─────────────────────────────────────────

    #[test]
    fn connect_timeout_default_is_30s() {
        // Ensure no env override interferes.
        let dur = llm_connect_timeout();
        // Default is LLM_CONNECT_TIMEOUT_S = 30.
        assert_eq!(dur, std::time::Duration::from_secs(LLM_CONNECT_TIMEOUT_S));
    }

    #[test]
    fn fallback_timeout_default_is_120s() {
        let dur = llm_fallback_timeout();
        assert_eq!(dur, std::time::Duration::from_secs(LLM_FALLBACK_TIMEOUT_S));
    }

    #[test]
    fn total_budget_default_is_300s() {
        let dur = llm_total_budget();
        assert_eq!(dur, std::time::Duration::from_secs(LLM_TOTAL_BUDGET_S));
    }

    #[tokio::test]
    async fn total_budget_exhausted_returns_error() {
        // Simulate a scenario where started time is already past budget.
        // We test the logic inline since call_llm_and_collect needs a server.
        let budget = std::time::Duration::from_millis(1);
        let started = Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(
            started.elapsed() > budget,
            "elapsed should exceed tiny budget"
        );
    }

    #[tokio::test]
    async fn nonstream_fallback_respects_timeout() {
        // Create a mock server that delays longer than the fallback timeout.
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Response::builder()
                    .status(200)
                    .body(Body::from(
                        r#"{"choices":[{"message":{"content":"late"}}]}"#,
                    ))
                    .unwrap()
            }),
        );
        let base = spawn_local_http_server(app).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build client");
        // Use a very short timeout — should fail before the 5s delay completes.
        let timeout = std::time::Duration::from_millis(100);
        let result = call_llm_nonstream_fallback(
            &client,
            &[json!({"role":"user","content":"x"})],
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            timeout,
        )
        .await;
        assert!(result.is_err(), "should timeout: {result:?}");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("timeout") || err.message.contains("Timeout"),
            "error should mention timeout: {err}"
        );
    }

    #[test]
    fn classify_llm_error_categories() {
        use astra_core::ErrorKind;
        assert_eq!(
            classify_llm_error("rate limit exceeded"),
            ErrorKind::RateLimit
        );
        assert_eq!(
            classify_llm_error("error 429: too many requests"),
            ErrorKind::RateLimit
        );
        assert_eq!(
            classify_llm_error("request timed out"),
            ErrorKind::StreamIdle
        );
        assert_eq!(
            classify_llm_error("connection refused"),
            ErrorKind::StreamTransport
        );
        assert_eq!(classify_llm_error("401 unauthorized"), ErrorKind::Auth);
        assert_eq!(
            classify_llm_error("something went wrong"),
            ErrorKind::Unknown
        );
        assert_eq!(
            classify_llm_error("LLM stream transport error: connection reset"),
            ErrorKind::StreamTransport
        );
        assert_eq!(
            classify_llm_error("LLM call cancelled"),
            ErrorKind::Cancelled
        );
    }

    #[test]
    fn is_context_window_error_detects_all_patterns() {
        // These are the actual API response patterns from various providers
        assert!(is_context_window_error("context_length_exceeded"));
        assert!(is_context_window_error("maximum context length is 128000"));
        assert!(is_context_window_error("prompt is too long"));
        assert!(is_context_window_error("too many tokens in the input"));
        assert!(is_context_window_error("input is too long for this model"));
        assert!(is_context_window_error("context window exceeded"));
        assert!(is_context_window_error("max_tokens limit exceeded"));
        // Negative cases
        assert!(!is_context_window_error("rate limit exceeded"));
        assert!(!is_context_window_error("internal server error"));
        assert!(!is_context_window_error(""));
    }

    #[test]
    fn is_tpm_exhaustion_detects_patterns() {
        // TPM (tokens per minute) exhaustion patterns
        assert!(is_tpm_exhaustion("endpoint TPM exceeded"));
        assert!(is_tpm_exhaustion("TPM limit exceeded for this endpoint"));
        assert!(is_tpm_exhaustion("tokens per minute limit reached"));
        assert!(is_tpm_exhaustion(
            "Rate limit exceeded: token quota exhausted"
        ));
        // Negative cases - regular rate limits (not TPM)
        assert!(!is_tpm_exhaustion("rate limit exceeded"));
        assert!(!is_tpm_exhaustion("too many requests"));
        assert!(!is_tpm_exhaustion("429 quota exceeded"));
        assert!(!is_tpm_exhaustion(""));
    }

    #[test]
    fn context_window_error_detected_in_llm_error_format() {
        // Verify that is_context_window_error works on the format produced by
        // call_llm_stream: "LLM error 400: {api_response_body}"
        let api_body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens","type":"invalid_request_error"}}"#;
        let err = format!("LLM error 400: {api_body}");
        assert!(is_context_window_error(&err.to_lowercase()));
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

    async fn sample_reqwest_stream_error() -> reqwest::Error {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connection to closed port should fail")
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_surfaces_byte_stream_error() {
        let err = sample_reqwest_stream_error().await;
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![Err(err)];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let r = st.next().await.expect("one item");
        let msg = r.expect_err("transport");
        assert!(!msg.is_empty());
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_event_then_transport_error() {
        let err = sample_reqwest_stream_error().await;
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from("data: {\"x\":1}\n\n")), Err(err)];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        assert_eq!(st.next().await.unwrap().unwrap(), json!({"x": 1}));
        assert!(st.next().await.unwrap().is_err());
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_tail_flush_without_final_blank_line() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![Ok(Bytes::from("data: {\"z\":9}"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let ev = st.next().await.unwrap().unwrap();
        assert_eq!(ev, json!({"z": 9}));
        assert!(st.next().await.is_none());
    }

    // ── serial(stream_idle_env): all tests below mutate MO_STREAM_IDLE_TIMEOUT_MS
    // which is read at startup and cached globally. Parallel execution causes
    // race conditions where one test's timeout value bleeds into another test's
    // LlmClient construction. Any new test that sets this env var MUST be tagged.

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_surfaces_transport_error() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let err = sample_reqwest_stream_error().await;
        let byte_stream = stream::iter(vec![Err(err)]);
        let started = Instant::now();
        let res = collect_llm_stream(
            byte_stream,
            "test-model",
            started,
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await;
        assert!(
            matches!(res, Err(StreamCollectError::Transport(_))),
            "expected transport error, got: {res:?}"
        );
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_aggregates_delta_text_reasoning_usage() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let d1 = json!({"choices":[{"delta":{"content":"Hi ","reasoning_content":"R"}}]});
        let d2 = json!({"choices":[{"delta":{"content":"there"}}]});
        let u = json!({"usage":{"prompt_tokens":3,"completion_tokens":4}});
        let body = format!("data: {d1}\n\ndata: {d2}\n\ndata: {u}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "gpt-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect");
        assert_eq!(res.full_text, "Hi there");
        assert_eq!(res.reasoning, "R");
        assert_eq!(res.usage.get("prompt").and_then(Value::as_i64), Some(3));
        assert_eq!(res.usage.get("completion").and_then(Value::as_i64), Some(4));
        assert_eq!(res.usage.get("total").and_then(Value::as_i64), Some(7));
        assert_eq!(res.model_used, "gpt-test");
        assert!(res.tool_calls.is_empty());
        // No finish_reason chunk was sent, so it should be None
        assert_eq!(res.finish_reason, None);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_extracts_finish_reason_stop() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let d1 = json!({"choices":[{"delta":{"content":"Hello"}}]});
        let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        let body = format!("data: {d1}\n\ndata: {done}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect");
        assert_eq!(res.full_text, "Hello");
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_extracts_finish_reason_length() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let d1 = json!({"choices":[{"delta":{"content":"truncated"}}]});
        let done = json!({"choices":[{"delta":{},"finish_reason":"length"}]});
        let body = format!("data: {d1}\n\ndata: {done}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect");
        assert_eq!(res.finish_reason.as_deref(), Some("length"));
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_merges_tool_call_argument_chunks() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let c1 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"foo"}}]}}]});
        let c2 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":\"bar\"}"}}]}}]});
        let body = format!("data: {c1}\n\ndata: {c2}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect");
        assert_eq!(res.tool_calls.len(), 1);
        let args = res.tool_calls[0]["function"]["arguments"]
            .as_str()
            .expect("arguments string");
        let parsed: Value = serde_json::from_str(args).expect("valid merged JSON args");
        assert_eq!(parsed, json!({"foo":"bar"}));
        assert_eq!(res.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn stream_idle_timeout_triggers() {
        // Keep this test fast: override idle timeout to 1ms.
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "1") };
        // Stream that never yields any bytes (simulates a hung connection).
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let res = collect_llm_stream(
            pending_stream,
            "test-model",
            started,
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await;
        assert!(
            matches!(
                res,
                Err(StreamCollectError::IdleTimeout {
                    made_progress: false,
                    ..
                })
            ),
            "expected idle timeout, got: {res:?}"
        );
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn stream_idle_timeout_after_partial_output_marks_progress() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "1") };
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS", "1") };
        let d1 = json!({"choices":[{"delta":{"content":"partial"}}]});
        let stream = stream::iter(vec![Ok(Bytes::from(format!("data: {d1}\n\n")))])
            .chain(stream::pending::<Result<Bytes, reqwest::Error>>());
        let res = collect_llm_stream(
            stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await;
        assert!(
            matches!(
                res,
                Err(StreamCollectError::IdleTimeout {
                    made_progress: true,
                    ..
                })
            ),
            "expected idle timeout after partial output, got: {res:?}"
        );
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
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
                stream_idle_timeout(),
                stream_idle_timeout_after_progress(),
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
    #[serial(stream_idle_env)]
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
                stream_idle_timeout(),
                stream_idle_timeout_after_progress(),
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
    #[serial(stream_idle_env)]
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
                stream_idle_timeout(),
                stream_idle_timeout_after_progress(),
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

    #[derive(Clone)]
    struct Hit(Arc<AtomicU32>);

    #[derive(Clone)]
    struct StreamIdleHit {
        stream_hits: Arc<AtomicU32>,
        fallback_hits: Arc<AtomicU32>,
    }

    async fn spawn_local_http_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock llm listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        format!("http://{addr}")
    }

    async fn mock_429_retry_zero_then_sse(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Response::builder()
                .status(429)
                .header("retry-after", "0")
                .body(Body::from("rate limited"))
                .unwrap()
        } else {
            let payload = json!({"choices":[{"delta":{"content":"after-429"}}]});
            let body = format!("data: {}\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    async fn mock_429_retry_two_seconds(State(Hit(c)): State<Hit>) -> Response {
        c.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .status(429)
            .header("retry-after", "2")
            .body(Body::from("slow"))
            .unwrap()
    }

    async fn mock_529_retry_zero_then_sse(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Response::builder()
                .status(529)
                .header("retry-after", "0")
                .body(Body::from("overload"))
                .unwrap()
        } else {
            let payload = json!({"choices":[{"delta":{"content":"after-529"}}]});
            let body = format!("data: {}\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    async fn mock_503_retry_zero_then_sse(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Response::builder()
                .status(503)
                .header("retry-after", "0")
                .body(Body::from("unavailable"))
                .unwrap()
        } else {
            let payload = json!({"choices":[{"delta":{"content":"after-503"}}]});
            let body = format!("data: {}\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    async fn mock_500_once(State(Hit(c)): State<Hit>) -> Response {
        c.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .status(500)
            .body(Body::from("server error"))
            .unwrap()
    }

    async fn mock_stream_idle_after_partial_then_fallback(
        State(state): State<StreamIdleHit>,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        if is_stream {
            state.stream_hits.fetch_add(1, Ordering::SeqCst);
            let partial = json!({"choices":[{"delta":{"content":"partial"}}]});
            let body_stream = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(
                format!("data: {partial}\n\n"),
            ))])
            .chain(stream::pending::<Result<Bytes, std::io::Error>>());
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(body_stream))
                .unwrap()
        } else {
            state.fallback_hits.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(200)
                .body(Body::from(
                    r#"{"choices":[{"message":{"content":"from-fallback"}}]}"#,
                ))
                .unwrap()
        }
    }

    async fn mock_stream_idle_before_any_output_then_retry(
        State(Hit(c)): State<Hit>,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        if is_stream && n == 0 {
            let body_stream = stream::pending::<Result<Bytes, std::io::Error>>();
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(body_stream))
                .unwrap()
        } else if is_stream {
            let payload = json!({"choices":[{"delta":{"content":"after-retry"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {payload}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        } else {
            Response::builder()
                .status(500)
                .body(Body::from("unexpected non-stream fallback"))
                .unwrap()
        }
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_decodes_lossy_utf8_inside_json_string() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(br#"data: {"choices":[{"delta":{"content":"a"#);
        v.push(0xff);
        v.extend_from_slice(br#""}}]}"#);
        v.extend_from_slice(b"\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(v))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect");
        assert_eq!(res.full_text, "a\u{FFFD}");
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_retries_after_429_retry_after_zero() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_429_retry_zero_then_sse))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-429");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    async fn call_llm_and_collect_cancel_during_429_backoff_sleep() {
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_429_retry_two_seconds))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let token = CancellationToken::new();
        let token_for_call = token.clone();
        let messages = vec![json!({"role":"user","content":"x"})];
        let base_clone = base.clone();
        let handle = tokio::spawn(async move {
            call_llm_and_collect(
                &messages,
                &[],
                "m",
                "k",
                &base_clone,
                "openai",
                None,
                false,
                LlmCancel::Token(&token_for_call),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        token.cancel();
        let err = handle.await.expect("join").expect_err("cancelled");
        assert_eq!(err.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_retries_after_529_retry_after_zero() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_529_retry_zero_then_sse))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-529");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_retries_after_503_retry_after_zero() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_503_retry_zero_then_sse))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-503");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    async fn call_llm_and_collect_cancel_during_exponential_backoff_after_500() {
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_500_once))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let token = CancellationToken::new();
        let token_for_call = token.clone();
        let messages = vec![json!({"role":"user","content":"x"})];
        let base_clone = base.clone();
        let handle = tokio::spawn(async move {
            call_llm_and_collect(
                &messages,
                &[],
                "m",
                "k",
                &base_clone,
                "openai",
                None,
                false,
                LlmCancel::Token(&token_for_call),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
        let err = handle.await.expect("join").expect_err("cancelled");
        assert_eq!(err.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    // ── Output escalation E2E tests ─────────────────────────────────────────

    /// Mock server that returns finish_reason=length on first call,
    /// then finish_reason=stop on second call (simulating successful escalation).
    async fn mock_length_then_stop(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let d1 = json!({"choices":[{"delta":{"content":"truncat"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"length"}]});
            let body = format!("data: {d1}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        } else {
            let d1 = json!({"choices":[{"delta":{"content":"complete response"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {d1}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn output_escalation_e2e_length_then_stop() {
        // Verifies: first call returns finish_reason=length, second returns stop.
        // This is the data path used by server_loop_host's escalation loop.
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_length_then_stop))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];

        // First call: finish_reason=length
        let res1 = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            Some(1000),
            false,
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res1.full_text, "truncat");
        assert_eq!(res1.finish_reason.as_deref(), Some("length"));

        // Second call (escalated): finish_reason=stop
        let res2 = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            Some(4000),
            false,
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res2.full_text, "complete response");
        assert_eq!(res2.finish_reason.as_deref(), Some("stop"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn finish_reason_stop_no_retry() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        async fn mock_stop(State(Hit(c)): State<Hit>) -> Response {
            c.fetch_add(1, Ordering::SeqCst);
            let d = json!({"choices":[{"delta":{"content":"ok"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {d}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
        let app = Router::new()
            .route("/chat/completions", post(mock_stop))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            Some(1000),
            false,
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no retry when stop");
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn finish_reason_tool_calls_extracted() {
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let d = json!({"choices":[{
            "delta":{"tool_calls":[{"index":0,"id":"tc1","type":"function","function":{"name":"bash","arguments":"{}"}}]},
            "finish_reason":"tool_calls"
        }]});
        let body = format!("data: {d}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect");
        assert_eq!(res.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(res.tool_calls.len(), 1);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_falls_back_immediately_after_partial_stream_idle() {
        let _guard = set_test_stream_timeouts(10, Some(10));
        let state = StreamIdleHit {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route(
                "/chat/completions",
                post(mock_stream_idle_after_partial_then_fallback),
            )
            .with_state(state.clone());
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect("fallback succeeds");
        assert_eq!(res.full_text, "from-fallback");
        assert_eq!(
            state.stream_hits.load(Ordering::SeqCst),
            1,
            "partial stream idle should skip the extra streaming retry"
        );
        assert_eq!(state.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_retries_stream_once_when_idle_before_output() {
        let _guard = set_test_stream_timeouts(10, None);
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route(
                "/chat/completions",
                post(mock_stream_idle_before_any_output_then_retry),
            )
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect("stream retry succeeds");
        assert_eq!(res.full_text, "after-retry");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// Mock server that returns 400 with context_length_exceeded.
    async fn mock_400_context_window() -> Response {
        let body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 200000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        Response::builder()
            .status(400)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_returns_context_window_error_kind() {
        reset_rate_limit_cooldown_for_tests();
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let app = Router::new().route("/chat/completions", post(mock_400_context_window));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let err = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect_err("should fail with context window");
        assert_eq!(err.kind, astra_core::ErrorKind::ContextWindow);
        assert!(err.message.contains("context_length_exceeded"));
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }

    /// Mock server that returns 401 Unauthorized.
    async fn mock_401() -> Response {
        Response::builder()
            .status(401)
            .body(Body::from("Unauthorized"))
            .unwrap()
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_returns_auth_error_kind() {
        reset_rate_limit_cooldown_for_tests();
        unsafe { std::env::set_var("MO_STREAM_IDLE_TIMEOUT_MS", "60000") };
        let app = Router::new().route("/chat/completions", post(mock_401));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let err = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
        )
        .await
        .expect_err("should fail with auth");
        assert_eq!(err.kind, astra_core::ErrorKind::Auth);
        unsafe { std::env::remove_var("MO_STREAM_IDLE_TIMEOUT_MS") };
    }
}
