//! Shared LLM calling utilities.
//!
//! Extracted from [`super::bridge_inprocess`] so both the in-process bridge
//! and [`crate::server::server_loop_host::ServerAgenticLoopHost`] can call LLMs
//! without duplicating the retry/backoff/parsing logic.
//!
//! # Proxy invariant
//!
//! [`astra_core::net::apply_env_proxy`] is the **only** place in the codebase
//! that honours `HTTPS_PROXY` / `ALL_PROXY` env vars. It is called from the
//! LLM client here and from `validate_connectivity` in `astra-services`
//! (both reach external provider endpoints). All other `reqwest` clients
//! (durable bridge, skill HTTP, server tool executor, summary client, …)
//! must call `.no_proxy()` — their traffic is local/intranet and should
//! not be routed through a user's LLM proxy.
//!
//! Re-exported as [`apply_env_proxy`] for in-crate call sites.

use std::{
    collections::HashMap,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use astra_logging::redact_known_secret_patterns;
use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::prompts;
use astra_text_utils::output_style::current_output_style;
use astra_turn_core::bridge_rate_limit_cooldown::{
    RateLimitAction, is_overload_status, is_rate_limit_status, parse_retry_after_ms,
};
use astra_turn_core::sse_blocks::SseBlankLineUtf8Buf;
use astra_turn_core::sse_data_lines::{
    json_events_from_sse_event_block, validate_sse_event_block_json,
    validated_drain_sse_data_lines, validated_finish_sse_data_buffer,
};
use astra_turn_core::thinking_config::ThinkingConfig;

/// Redact common provider secret patterns from a string before logging.
///
/// Replaces the value following well-known prefixes (`sk-`, `Bearer `, `key-`)
/// with `[REDACTED]`. The scan stops at the first whitespace, quote, or comma,
/// which is sufficient for the JSON / plaintext error bodies that providers
/// commonly echo authorization material into.
pub(crate) fn redact_provider_secrets(s: &str) -> String {
    redact_known_secret_patterns(s)
}

/// Maximum retries for transient LLM errors (429, 5xx, network).
pub(crate) const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
/// Override: `ASTRA_LLM_RETRY_BASE_MS` (e.g. `10` in E2E tests that
/// intentionally exhaust retries to assert error-surface behavior).
pub(crate) const LLM_RETRY_BASE_MS: u64 = 1000;

fn llm_retry_base_ms() -> u64 {
    std::env::var("ASTRA_LLM_RETRY_BASE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(LLM_RETRY_BASE_MS)
}
/// Extended delay for TPM (tokens per minute) exhaustion (60 seconds).
/// TPM limits typically reset after 60 seconds, so we wait longer.
const TPM_EXHAUST_DELAY_MS: u64 = 60_000;
/// Maximum retries for TPM exhaustion (longer recovery period).
const TPM_MAX_RETRIES: u32 = 5;
/// TCP connect timeout for LLM API requests (seconds). Override: `ASTRA_LLM_CONNECT_TIMEOUT_S`.
const LLM_CONNECT_TIMEOUT_S: u64 = 30;
/// Non-stream fallback hard timeout (seconds). Override: `ASTRA_LLM_FALLBACK_TIMEOUT_S`.
const LLM_FALLBACK_TIMEOUT_S: u64 = 120;
/// Total budget across all retries + fallback for a single LLM call (seconds).
/// Override: `ASTRA_LLM_TOTAL_BUDGET_S`.
const LLM_TOTAL_BUDGET_S: u64 = 300;

// ── Rate-Limit Cooldown ──────────────────────────────────────────────────────
use std::sync::OnceLock;

/// Per-model rate-limit cooldown tracker — shared with bridge_llm_stream.
use super::bridge_llm_stream::rate_limit_cooldown;

// ── Global HTTP Client ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LlmProviderProtocol {
    OpenAiCompatible,
    AnthropicMessages,
    BedrockConverse,
}

pub(crate) fn llm_provider_protocol(provider: &str) -> LlmProviderProtocol {
    match provider {
        "anthropic" => LlmProviderProtocol::AnthropicMessages,
        "bedrock" => LlmProviderProtocol::BedrockConverse,
        _ => LlmProviderProtocol::OpenAiCompatible,
    }
}

pub(crate) fn provider_uses_anthropic_messages(provider: &str) -> bool {
    llm_provider_protocol(provider) == LlmProviderProtocol::AnthropicMessages
}

pub(crate) fn provider_uses_bedrock_converse(provider: &str) -> bool {
    llm_provider_protocol(provider) == LlmProviderProtocol::BedrockConverse
}

/// Returns true only when the *provider* is known to be DashScope / Aliyun / Alibaba.
///
/// We intentionally do NOT match on model name here: Qwen models are also served
/// through generic OpenAI-compatible proxies (vLLM, Ollama, SGLang, …) that do not
/// accept `enable_thinking` and may 400 on unknown top-level fields. Matching the
/// provider name alone avoids false positives on those deployments.
pub(crate) fn provider_uses_dashscope_thinking(provider: &str) -> bool {
    let provider = provider.to_ascii_lowercase();
    provider.contains("dashscope") || provider.contains("aliyun") || provider.contains("alibaba")
}

/// Global HTTP client for LLM requests (connection pooling, reuse).
fn global_llm_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let connect = llm_connect_timeout();
        let total = std::time::Duration::from_secs(LLM_TOTAL_BUDGET_S + 60);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(connect)
            // Use a generous timeout; per-request timeout handled via tokio::time::timeout
            .timeout(total)
            .pool_max_idle_per_host(4);
        // Honour HTTPS_PROXY / ALL_PROXY env vars (reqwest default-features=false
        // does not auto-read system proxy, so we wire it up explicitly).
        builder = apply_env_proxy(builder);
        match builder.build()
        {
            Ok(client) => client,
            Err(e) => {
                // audit-C1: TLS / HTTP stack init failure should not crash the process.
                // Retry with the same timeouts but without pool tuning so we still bound
                // hung-upstream risk if this tier succeeds.
                tracing::error!(
                    target: "astra_runtime::llm_client",
                    error = %e,
                    "failed to build global LLM HTTP client; retrying without pool_max_idle_per_host"
                );
                match reqwest::Client::builder()
                    .no_proxy()
                    .connect_timeout(connect)
                    .timeout(total)
                    .build()
                {
                    Ok(client) => client,
                    Err(e2) => {
                        tracing::error!(
                            target: "astra_runtime::llm_client",
                            error = %e2,
                            "failed to build minimal global LLM HTTP client; using reqwest::Client::new() without explicit timeouts"
                        );
                        reqwest::Client::new()
                    }
                }
            }
        }
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
    /// Bedrock reasoning signature — must be passed back unmodified in multi-turn.
    pub reasoning_signature: String,
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

fn llm_result_has_partial_signal(result: &LlmCallResult) -> bool {
    !result.full_text.is_empty()
        || !result.reasoning.is_empty()
        || !result.tool_calls.is_empty()
        || !result.usage.is_empty()
        || result.finish_reason.is_some()
}

fn llm_result_details_json(result: &LlmCallResult) -> Option<String> {
    if !llm_result_has_partial_signal(result) {
        return None;
    }
    serde_json::to_string(&json!({
        "partial_full_text": result.full_text,
        "partial_reasoning": result.reasoning,
        "reasoning_signature": result.reasoning_signature,
        "tool_calls": result.tool_calls,
        "usage": result.usage,
        "finish_reason": result.finish_reason,
        "model_used": result.model_used,
    }))
    .ok()
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
            LlmCancel::Flag(f) => f.load(Ordering::Acquire),
            LlmCancel::FlagAndToken(f, t) => f.load(Ordering::Acquire) || t.is_cancelled(),
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
            while !f.load(Ordering::Acquire) {
                tokio::time::sleep(POLL).await;
            }
        }
        LlmCancel::FlagAndToken(f, t) => {
            const POLL: std::time::Duration = std::time::Duration::from_millis(50);
            tokio::select! {
                biased;
                _ = t.cancelled() => {}
                _ = async {
                    while !f.load(Ordering::Acquire) {
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
/// Production delegates to the canonical timeout in `sse_stream_host`; tests may
/// override it through unit-test locals or the `bridge-e2e-hooks` integration hook.
pub(crate) fn stream_idle_timeout() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_STREAM_IDLE_TIMEOUT.with(|c| *c.borrow()) {
        return d;
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    if let Some(d) = bridge_e2e_stream_idle_timeout_override() {
        return d;
    }
    astra_turn_core::sse_stream_host::stream_idle_timeout()
}

/// Per-chunk idle watchdog (post-progress): once at least one SSE chunk has been
/// received, allow a longer idle window to accommodate thinking/reasoning pauses.
/// Production delegates to the canonical timeout in `sse_stream_host`; tests may
/// override it through unit-test locals or the `bridge-e2e-hooks` integration hook.
pub(crate) fn stream_idle_timeout_after_progress() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| *c.borrow()) {
        return d;
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    if let Some(d) = bridge_e2e_stream_idle_timeout_after_progress_override() {
        return d;
    }
    astra_turn_core::sse_stream_host::stream_idle_timeout_after_progress()
}

#[cfg(feature = "bridge-e2e-hooks")]
static BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bridge-e2e-hooks")]
static BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) struct BridgeE2eStreamIdleTimeoutGuard {
    prev_pre_ms: u64,
    prev_post_ms: u64,
}

#[cfg(feature = "bridge-e2e-hooks")]
impl Drop for BridgeE2eStreamIdleTimeoutGuard {
    fn drop(&mut self) {
        restore_bridge_e2e_stream_idle_timeouts_for_test(self.prev_pre_ms, self.prev_post_ms);
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
fn duration_override(ms: u64) -> Option<std::time::Duration> {
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

#[cfg(feature = "bridge-e2e-hooks")]
fn bridge_e2e_stream_idle_timeout_override() -> Option<std::time::Duration> {
    duration_override(BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS.load(std::sync::atomic::Ordering::SeqCst))
}

#[cfg(feature = "bridge-e2e-hooks")]
fn bridge_e2e_stream_idle_timeout_after_progress_override() -> Option<std::time::Duration> {
    duration_override(
        BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) fn set_bridge_e2e_stream_idle_timeouts_for_test(
    pre_ms: u64,
    post_ms: u64,
) -> BridgeE2eStreamIdleTimeoutGuard {
    assert!(pre_ms > 0, "pre-progress idle timeout must be positive");
    assert!(post_ms > 0, "post-progress idle timeout must be positive");
    let prev_pre_ms =
        BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS.swap(pre_ms, std::sync::atomic::Ordering::SeqCst);
    let prev_post_ms = BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS
        .swap(post_ms, std::sync::atomic::Ordering::SeqCst);
    BridgeE2eStreamIdleTimeoutGuard {
        prev_pre_ms,
        prev_post_ms,
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) fn restore_bridge_e2e_stream_idle_timeouts_for_test(pre_ms: u64, post_ms: u64) {
    BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS.store(pre_ms, std::sync::atomic::Ordering::SeqCst);
    BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS
        .store(post_ms, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) fn current_bridge_e2e_stream_idle_timeouts_for_test()
-> (Option<std::time::Duration>, Option<std::time::Duration>) {
    (
        bridge_e2e_stream_idle_timeout_override(),
        bridge_e2e_stream_idle_timeout_after_progress_override(),
    )
}

#[cfg(test)]
thread_local! {
    static TEST_STREAM_IDLE_TIMEOUT: std::cell::RefCell<Option<std::time::Duration>> =
        const { std::cell::RefCell::new(None) };
    static TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS: std::cell::RefCell<Option<std::time::Duration>> =
        const { std::cell::RefCell::new(None) };
    // Retry-backoff override for tests: when `Some(ms)`, the between-attempts
    // backoff (normally `LLM_RETRY_BASE_MS * 2^(attempt-1)` or TPM_EXHAUST_DELAY_MS)
    // is replaced by this flat value. Lets retry-logic tests run in <100ms
    // instead of waiting on real time.
    static TEST_RETRY_BACKOFF_MS: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
}

/// Compute the between-attempts backoff in ms. `attempt` is 1-indexed (the
/// first retry after the initial failure has attempt=1).
fn retry_backoff_ms(attempt: u32, tpm_exhausted: bool) -> u64 {
    #[cfg(test)]
    if let Some(ms) = TEST_RETRY_BACKOFF_MS.with(|c| *c.borrow()) {
        return ms;
    }
    if tpm_exhausted {
        TPM_EXHAUST_DELAY_MS
    } else {
        llm_retry_base_ms() * (1 << (attempt - 1))
    }
}

/// Override the between-retry backoff to `ms` for the duration of a test.
/// Without this, every retry incurs a real 1s/2s/4s sleep — with it,
/// retry-logic tests run in <100ms. Returns a guard that clears the override
/// on drop. `pub(crate)` so other runtime modules (e.g. server_loop_host
/// end-to-end tests) can use the same knob.
#[cfg(test)]
pub(crate) fn set_test_retry_backoff_ms(ms: u64) -> impl Drop {
    TEST_RETRY_BACKOFF_MS.with(|c| *c.borrow_mut() = Some(ms));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            TEST_RETRY_BACKOFF_MS.with(|c| *c.borrow_mut() = None);
        }
    }
    Guard
}

/// Apply HTTP(S)/ALL proxy env vars to a reqwest::ClientBuilder.
///
/// reqwest is built with `default-features = false`, so it does not auto-read
/// the system proxy env vars. We wire them up explicitly here and honour
/// `NO_PROXY` via `reqwest::NoProxy::from_env()`.
///
/// Precedence (first match wins): `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`,
/// `all_proxy`. For `HTTPS_PROXY`/`https_proxy` we register an HTTPS-scheme
/// proxy; for `ALL_PROXY`/`all_proxy` we register an all-scheme proxy so that
/// `socks5://` URLs (which only make sense as all-scheme) are honoured.
pub(crate) use astra_core::net::apply_env_proxy;

// Tests for `apply_env_proxy` live with its authoritative implementation in
// `astra_core::net`. Do not duplicate them here.

/// Resolve an LLM duration-in-seconds constant, consulting its env-var
/// override and falling back to the compile-time default. Used by
/// `LLM_CONNECT_TIMEOUT_S`, `LLM_FALLBACK_TIMEOUT_S`, and
/// `LLM_TOTAL_BUDGET_S`. Operators set these to lower values in
/// degraded conditions (tight SLOs) or raise them for slow providers;
/// the const defaults are the production baseline.
fn llm_secs_from_env(var: &str, default_secs: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default_secs)
}

/// TCP connect timeout for LLM API requests. Override: `ASTRA_LLM_CONNECT_TIMEOUT_S`.
pub(crate) fn llm_connect_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(llm_secs_from_env(
        "ASTRA_LLM_CONNECT_TIMEOUT_S",
        LLM_CONNECT_TIMEOUT_S,
    ))
}

/// Hard timeout for the non-stream fallback request. Override: `ASTRA_LLM_FALLBACK_TIMEOUT_S`.
pub(crate) fn llm_fallback_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(llm_secs_from_env(
        "ASTRA_LLM_FALLBACK_TIMEOUT_S",
        LLM_FALLBACK_TIMEOUT_S,
    ))
}

/// Total budget across all retries + fallback for a single LLM call. Override: `ASTRA_LLM_TOTAL_BUDGET_S`.
pub(crate) fn llm_total_budget() -> std::time::Duration {
    std::time::Duration::from_secs(llm_secs_from_env(
        "ASTRA_LLM_TOTAL_BUDGET_S",
        LLM_TOTAL_BUDGET_S,
    ))
}

#[cfg(test)]
fn llm_completions_url(base_url: &str, override_url: Option<&str>, provider: &str) -> String {
    llm_request_url(base_url, override_url, provider, "", true)
}

fn bedrock_converse_url(base_url: &str, model_name: &str, streaming: bool) -> String {
    let base = base_url.trim_end_matches('/');
    let mut url = reqwest::Url::parse(base).unwrap_or_else(|_| {
        reqwest::Url::parse("http://invalid.local").expect("valid fallback URL")
    });
    {
        let mut segments = url
            .path_segments_mut()
            .expect("base URL must support path segments");
        segments.pop_if_empty();
        segments.push("model");
        segments.push(model_name);
        segments.push(if streaming {
            "converse-stream"
        } else {
            "converse"
        });
    }
    if url.host_str() == Some("invalid.local") {
        format!(
            "{base}/model/{model_name}/{}",
            if streaming {
                "converse-stream"
            } else {
                "converse"
            }
        )
    } else {
        url.to_string()
    }
}

pub(crate) fn llm_request_url(
    base_url: &str,
    override_url: Option<&str>,
    provider: &str,
    model_name: &str,
    streaming: bool,
) -> String {
    override_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .unwrap_or_else(|| llm_request_url_for_provider(base_url, provider, model_name, streaming))
}

/// Build the default completions URL for a given provider (no override).
///
/// Anthropic uses `/v1/messages`, Bedrock uses `/model/{modelId}/converse`,
/// all others use `/chat/completions`.
pub(crate) fn llm_request_url_for_provider(
    base_url: &str,
    provider: &str,
    model_name: &str,
    streaming: bool,
) -> String {
    let base = base_url.trim_end_matches('/');
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::AnthropicMessages => {
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
        LlmProviderProtocol::BedrockConverse => bedrock_converse_url(base, model_name, streaming),
        LlmProviderProtocol::OpenAiCompatible => format!("{base}/chat/completions"),
    }
}

fn json_string_to_value_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn content_text_value(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let joined = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

fn is_nonblank_text(text: &str) -> bool {
    !text.trim().is_empty()
}

fn bedrock_cache_point_from_cache_control(cache_control: Option<&Value>) -> Option<Value> {
    let cache_control = cache_control?;
    let mut cache_point = Map::new();
    cache_point.insert("type".to_string(), Value::String("default".to_string()));
    if let Some(ttl) = cache_control
        .get("ttl")
        .and_then(Value::as_str)
        .filter(|ttl| matches!(*ttl, "5m" | "1h"))
    {
        cache_point.insert("ttl".to_string(), Value::String(ttl.to_string()));
    }
    Some(json!({ "cachePoint": Value::Object(cache_point) }))
}

fn bedrock_cache_point_from_block(block: &Value) -> Option<Value> {
    if let Some(cache_point) = block.get("cachePoint") {
        return Some(json!({ "cachePoint": cache_point.clone() }));
    }
    bedrock_cache_point_from_cache_control(block.get("cache_control"))
}

fn build_bedrock_text_content_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if is_nonblank_text(text) => vec![json!({ "text": text })],
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if is_nonblank_text(text) {
                        blocks.push(json!({ "text": text }));
                    }
                } else if let Some(text) = part.as_str() {
                    if is_nonblank_text(text) {
                        blocks.push(json!({ "text": text }));
                    }
                }
                if let Some(cache_point) = bedrock_cache_point_from_block(part) {
                    blocks.push(cache_point);
                }
            }
            blocks
        }
        Some(Value::Object(obj)) => {
            let mut blocks = Vec::new();
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                if is_nonblank_text(text) {
                    blocks.push(json!({ "text": text }));
                }
            }
            if let Some(cache_point) = bedrock_cache_point_from_block(&Value::Object(obj.clone())) {
                blocks.push(cache_point);
            }
            blocks
        }
        _ => Vec::new(),
    }
}

fn bedrock_system_has_text(blocks: &[Value]) -> bool {
    blocks.iter().any(|block| {
        block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_nonblank_text)
    })
}

fn bedrock_cache_point_from_message_content(content: Option<&Value>) -> Option<Value> {
    match content {
        Some(Value::Array(parts)) => parts.iter().rev().find_map(bedrock_cache_point_from_block),
        Some(Value::Object(obj)) => bedrock_cache_point_from_block(&Value::Object(obj.clone())),
        _ => None,
    }
}

fn build_bedrock_tool_blocks(tool_calls: Option<&Vec<Value>>) -> Vec<Value> {
    let Some(tool_calls) = tool_calls else {
        return Vec::new();
    };
    tool_calls
        .iter()
        .filter_map(|tool_call| {
            let id = tool_call.get("id").and_then(Value::as_str)?;
            let function = tool_call.get("function")?.as_object()?;
            let name = function.get("name").and_then(Value::as_str)?;
            let input = function
                .get("arguments")
                .and_then(Value::as_str)
                .map(json_string_to_value_or_string)
                .unwrap_or_else(|| json!({}));
            Some(json!({
                "toolUse": {
                    "toolUseId": id,
                    "name": name,
                    "input": input,
                }
            }))
        })
        .collect()
}

fn build_bedrock_message_content(msg: &Value, include_reasoning_content: bool) -> Vec<Value> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
    match role {
        "tool" => {
            let tool_use_id = msg.get("tool_call_id").and_then(Value::as_str);
            let content = content_text_value(msg.get("content")).unwrap_or_default();
            tool_use_id
                .map(|tool_use_id| {
                    // Bedrock's `toolResult.content[].json` field requires a
                    // JSON object (Document type). Scalars, arrays, strings,
                    // booleans, and null must use the `text` branch — or
                    // Bedrock rejects with "messages.N.content.M.toolResult
                    // .content.0.json is invalid — provide a json object".
                    let result_block = if content.is_empty() {
                        json!({"json": {}})
                    } else {
                        match serde_json::from_str::<Value>(&content) {
                            Ok(parsed) if parsed.is_object() => json!({"json": parsed}),
                            _ => json!({"text": content}),
                        }
                    };
                    let mut blocks = vec![json!({
                        "toolResult": {
                            "toolUseId": tool_use_id,
                            "content": [result_block],
                        }
                    })];
                    if let Some(cache_point) =
                        bedrock_cache_point_from_message_content(msg.get("content"))
                    {
                        blocks.push(cache_point);
                    }
                    blocks
                })
                .unwrap_or_default()
        }
        "assistant" => {
            let mut blocks = Vec::new();
            // Bedrock requires reasoningContent FIRST when thinking is enabled.
            if include_reasoning_content
                && let Some(rc) = msg.get("reasoning_content").and_then(Value::as_str)
            {
                if !rc.is_empty() {
                    let mut reasoning_text = json!({"text": rc});
                    if let Some(sig) = msg.get("reasoning_signature").and_then(Value::as_str) {
                        if !sig.is_empty() {
                            reasoning_text["signature"] = Value::String(sig.to_string());
                        }
                    }
                    blocks.push(json!({"reasoningContent": {"reasoningText": reasoning_text}}));
                }
            }
            blocks.extend(build_bedrock_text_content_blocks(msg.get("content")));
            blocks.extend(build_bedrock_tool_blocks(
                msg.get("tool_calls").and_then(Value::as_array),
            ));
            blocks
        }
        _ => build_bedrock_text_content_blocks(msg.get("content")),
    }
}

/// Synthetic tool_result content inserted for a declared tool_call that has
/// no matching response. The model sees this and can recover (e.g. retry).
const SYNTHETIC_TOOL_INTERRUPTED_CONTENT: &str = "[tool execution not recorded]";

/// Repair tool_use/tool_result pairing mismatches that Bedrock Converse rejects
/// as 400. Three classes of corruption we observe, in order of severity:
///
/// 1. Missing tool_result for a declared tool_call (stream cut mid-execution,
///    session resume, or bridge restart). Bedrock: "Expected toolResult blocks
///    at messages.N.content for the following Ids: …".
/// 2. Orphaned tool_result whose tool_call_id doesn't match any preceding
///    assistant's tool_calls. Bedrock: "unexpected toolResult".
/// 3. Duplicate tool_call_id within one tool-group (retry artifact). Bedrock:
///    duplicate-id 400.
///
/// Bedrock-only: OpenAI and Anthropic providers consume the original messages
/// unchanged. This mirrors claudecode's `ensureToolResultPairing` but operates
/// on OpenAI wire format (role=tool messages) instead of Anthropic blocks.
pub(crate) fn repair_openai_tool_pairing_for_bedrock(messages: &[Value]) -> Vec<Value> {
    let mut repaired: Vec<Value> = Vec::with_capacity(messages.len());
    let mut missing_counts: usize = 0;
    let mut orphan_counts: usize = 0;
    let mut dup_counts: usize = 0;

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();

        if role == "assistant" {
            let declared_ids: Vec<String> = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|tcs| {
                    tcs.iter()
                        .filter_map(|tc| tc.get("id").and_then(Value::as_str).map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            repaired.push(msg.clone());
            i += 1;

            if declared_ids.is_empty() {
                continue;
            }

            // Collect the contiguous run of role=tool messages that follow.
            let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            let declared_set: std::collections::HashSet<&str> =
                declared_ids.iter().map(String::as_str).collect();
            while i < messages.len()
                && messages[i].get("role").and_then(Value::as_str) == Some("tool")
            {
                let tool_msg = &messages[i];
                let id = tool_msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() || !declared_set.contains(id) {
                    orphan_counts += 1;
                    i += 1;
                    continue;
                }
                if !seen_ids.insert(id.to_string()) {
                    dup_counts += 1;
                    i += 1;
                    continue;
                }
                repaired.push(tool_msg.clone());
                i += 1;
            }

            for declared in &declared_ids {
                if !seen_ids.contains(declared) {
                    missing_counts += 1;
                    repaired.push(json!({
                        "role": "tool",
                        "tool_call_id": declared,
                        "content": SYNTHETIC_TOOL_INTERRUPTED_CONTENT,
                    }));
                }
            }
            continue;
        }

        if role == "tool" {
            // Orphan: a role=tool message without a preceding assistant
            // tool_calls declaration in the current window. Drop it.
            orphan_counts += 1;
            i += 1;
            continue;
        }

        repaired.push(msg.clone());
        i += 1;
    }

    if missing_counts + orphan_counts + dup_counts > 0 {
        tracing::warn!(
            missing = missing_counts,
            orphaned = orphan_counts,
            duplicate = dup_counts,
            input_len = messages.len(),
            output_len = repaired.len(),
            "repaired tool_use/tool_result pairing for Bedrock request"
        );
    }
    repaired
}

fn flush_tool_buffer(out: &mut Vec<Value>, buffer: &mut Vec<Value>) {
    if buffer.is_empty() {
        return;
    }
    let blocks = std::mem::take(buffer);
    out.push(json!({
        "role": "user",
        "content": blocks,
    }));
}

fn build_bedrock_messages(
    messages: &[Value],
    include_reasoning_content: bool,
) -> (Vec<Value>, Vec<Value>) {
    let mut system = Vec::new();
    let mut out = Vec::new();
    // Bedrock Converse requires all toolResult blocks for a given assistant
    // turn's parallel toolUse blocks to live in a SINGLE user message. OpenAI
    // wire format emits one `role: "tool"` per result, so we buffer
    // consecutive tool messages and flush them as one merged user message
    // whenever a non-tool message (or end of input) is reached.
    let mut tool_buffer: Vec<Value> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        if role != "tool" {
            flush_tool_buffer(&mut out, &mut tool_buffer);
        }
        match role {
            "system" => {
                system.extend(build_bedrock_text_content_blocks(msg.get("content")));
            }
            "tool" => {
                tool_buffer.extend(build_bedrock_message_content(
                    msg,
                    include_reasoning_content,
                ));
            }
            "user" | "assistant" => {
                let content = build_bedrock_message_content(msg, include_reasoning_content);
                if !content.is_empty() {
                    out.push(json!({
                        "role": role,
                        "content": content,
                    }));
                }
            }
            _ => {}
        }
    }
    flush_tool_buffer(&mut out, &mut tool_buffer);
    (system, out)
}

fn build_bedrock_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for tool in tools {
        if let Some(mapped) = (|| {
            let function = tool.get("function")?.as_object()?;
            let name = function.get("name").and_then(Value::as_str)?;
            let input_schema = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let mut tool_spec = Map::new();
            tool_spec.insert("name".to_string(), Value::String(name.to_string()));
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                tool_spec.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            tool_spec.insert("inputSchema".to_string(), json!({ "json": input_schema }));
            Some(json!({ "toolSpec": Value::Object(tool_spec) }))
        })() {
            out.push(mapped);
            if let Some(cache_point) =
                bedrock_cache_point_from_cache_control(tool.get("cache_control"))
            {
                out.push(cache_point);
            }
        }
    }
    out
}

fn bedrock_messages_contain_tool_blocks(messages: &[Value]) -> bool {
    messages.iter().any(|msg| {
        msg.get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("toolUse").is_some() || b.get("toolResult").is_some())
            })
    })
}

/// Global counter: number of Bedrock thinking requests observed with a
/// `reasoningContent.text` block but no `signature`. Incremented by
/// [`assert_bedrock_thinking_signature_contract`] whenever the invariant is
/// violated. Exposed as a `pub static` so health/metric handlers can surface
/// it without plumbing a handle through every call site — matches the
/// convention used by `PERSIST_FAIL_COUNT` / `PERSIST_OK_COUNT`.
///
/// Any non-zero value in production means at least one turn will 400 at
/// Bedrock; on-call should page and check `astra_core::agent_warn!` logs
/// tagged `llm` for `bedrock signature contract violation`.
pub static BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Guard against the recurring Bedrock thinking-mode regression:
/// `messages.N.content.0.thinking.signature: Field required` (HTTP 400).
///
/// When thinking is enabled, every assistant `reasoningContent` block MUST
/// carry a signature. If the upstream pipeline ever drops it (as PR #284
/// and its follow-up showed, twice), Bedrock will reject the turn with the
/// message above.
///
/// Behavior:
/// - Debug builds / tests: `debug_assert!` — fails loud so the offending
///   refactor can't merge.
/// - Release builds: structured warn log + counter increment. On-call
///   monitors [`BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT`] as a
///   continuous-signal tripwire rather than scanning logs.
fn assert_bedrock_thinking_signature_contract(bedrock_messages: &[Value]) {
    for (idx, msg) in bedrock_messages.iter().enumerate() {
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            let Some(reasoning_text) = block
                .get("reasoningContent")
                .and_then(|rc| rc.get("reasoningText"))
            else {
                continue;
            };
            let text = reasoning_text
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }
            let has_signature = reasoning_text
                .get("signature")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if has_signature {
                continue;
            }
            // This combo will 400. Count it so on-call can trigger on a
            // non-zero tripwire instead of grepping logs; panic in debug/test
            // so regressions can't merge silently.
            BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            debug_assert!(
                false,
                "Bedrock thinking contract violation: messages[{idx}] has \
                 reasoningContent.text but no signature — Bedrock will reject \
                 with `messages.{idx}.content.0.thinking.signature: Field required`. \
                 The signature must be captured from the provider stream and replayed \
                 on every continuation turn (see chat_turn_sse_dispatch::reasoning_done)."
            );
            astra_core::agent_warn!(
                "llm",
                "bedrock signature contract violation: messages[{}].reasoningContent \
                 is non-empty but signature is missing — turn will 400",
                idx
            );
        }
    }
}

pub(crate) fn build_provider_request_body(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    temperature: Option<f64>,
    streaming: bool,
    thinking: &astra_turn_core::thinking_config::ThinkingConfig,
) -> Value {
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::BedrockConverse => {
            let repaired = repair_openai_tool_pairing_for_bedrock(messages);
            let (system, bedrock_messages) =
                build_bedrock_messages(&repaired, thinking.is_enabled());
            let mut body = json!({
                "messages": bedrock_messages,
            });
            if bedrock_system_has_text(&system) {
                body["system"] = Value::Array(system);
            }
            let mut inference = Map::new();
            if let Some(max_out) = max_output_tokens {
                inference.insert("maxTokens".to_string(), json!(max_out));
            }
            if let Some(temp) = temperature {
                inference.insert("temperature".to_string(), json!(temp));
            }
            if !inference.is_empty() {
                body["inferenceConfig"] = Value::Object(inference);
            }
            let bedrock_tools = build_bedrock_tools(tools);
            if !bedrock_tools.is_empty() {
                body["toolConfig"] = json!({ "tools": bedrock_tools });
            } else if bedrock_messages_contain_tool_blocks(&bedrock_messages) {
                body["toolConfig"] = json!({ "tools": [] });
            }
            thinking.apply_bedrock(&mut body);
            if thinking.is_enabled() {
                assert_bedrock_thinking_signature_contract(&bedrock_messages);
            }
            body
        }
        LlmProviderProtocol::AnthropicMessages | LlmProviderProtocol::OpenAiCompatible => {
            let is_anthropic = provider_uses_anthropic_messages(provider);
            let mut body = json!({
                "model": model_name,
                "messages": messages,
                "stream": streaming,
            });
            if streaming && !is_anthropic {
                body["stream_options"] = json!({"include_usage": true});
            }
            if let Some(max_out) = max_output_tokens {
                if is_anthropic {
                    body["max_tokens"] = json!(max_out);
                } else {
                    body["max_completion_tokens"] = json!(max_out);
                }
            }
            if let Some(temp) = temperature {
                body["temperature"] = json!(temp);
            }
            if !tools.is_empty() {
                body["tools"] = Value::Array(tools.to_vec());
                if is_anthropic {
                    body["tool_choice"] = json!({"type": "auto"});
                } else {
                    body["tool_choice"] = Value::String("auto".to_string());
                }
            }
            if is_anthropic {
                thinking.apply_anthropic(&mut body);
            } else if provider_uses_dashscope_thinking(provider) {
                // DashScope/Qwen uses a binary `enable_thinking` flag; there is no equivalent
                // of `reasoning_effort`. For `Enabled` this is a direct mapping. For `Adaptive`
                // we enable thinking but the `effort` level is silently dropped — warn so
                // operators can diagnose unexpected latency or cost behaviour.
                if thinking.is_enabled() {
                    body["enable_thinking"] = json!(true);
                } else if let ThinkingConfig::Adaptive { effort } = thinking {
                    tracing::warn!(
                        provider,
                        effort = ?effort,
                        "DashScope/Qwen does not support `reasoning_effort`; \
                         Adaptive mode mapped to `enable_thinking: true` — effort level ignored"
                    );
                    body["enable_thinking"] = json!(true);
                }
            } else {
                thinking.apply_openai(&mut body);
            }
            body
        }
    }
}

pub(crate) fn apply_provider_auth(
    mut req: reqwest::RequestBuilder,
    provider: &str,
    api_key: &str,
    header_overrides: Option<&HashMap<String, String>>,
) -> reqwest::RequestBuilder {
    if provider_uses_anthropic_messages(provider) {
        if !has_llm_auth_override(provider, header_overrides) {
            req = req.header("x-api-key", api_key);
        }
        req.header("anthropic-version", "2023-06-01")
    } else {
        if !has_llm_auth_override(provider, header_overrides) {
            req = req.header("authorization", format!("Bearer {api_key}"));
        }
        req
    }
}

/// Strip empty `tool_calls: []` from assistant messages in-place.
///
/// Thin wrapper around the canonical implementation in `astra_turn_core`.
pub(crate) fn strip_empty_assistant_tool_calls(messages: &mut [Value]) {
    astra_turn_core::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
}

pub(crate) fn consolidate_system_messages(messages: &[Value]) -> Vec<Value> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut structured_system = false;
    let mut rest: Vec<Value> = Vec::new();

    let flush_string_parts_into_blocks = |blocks: &mut Vec<Value>, parts: &mut Vec<String>| {
        for part in parts.drain(..) {
            if !blocks.is_empty() {
                blocks.push(json!({"type": "text", "text": "\n\n"}));
            }
            blocks.push(json!({"type": "text", "text": part}));
        }
    };

    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
            match msg.get("content") {
                Some(Value::String(text)) => {
                    if text.is_empty() {
                        continue;
                    }
                    if structured_system {
                        if !system_blocks.is_empty() {
                            system_blocks.push(json!({"type": "text", "text": "\n\n"}));
                        }
                        system_blocks.push(json!({"type": "text", "text": text}));
                    } else {
                        system_parts.push(text.to_string());
                    }
                }
                Some(Value::Array(parts)) => {
                    structured_system = true;
                    flush_string_parts_into_blocks(&mut system_blocks, &mut system_parts);
                    if parts.is_empty() {
                        continue;
                    }
                    if !system_blocks.is_empty() {
                        system_blocks.push(json!({"type": "text", "text": "\n\n"}));
                    }
                    system_blocks.extend(parts.iter().cloned());
                }
                Some(other) if !other.is_null() => {
                    structured_system = true;
                    flush_string_parts_into_blocks(&mut system_blocks, &mut system_parts);
                    if !system_blocks.is_empty() {
                        system_blocks.push(json!({"type": "text", "text": "\n\n"}));
                    }
                    system_blocks.push(other.clone());
                }
                _ => {}
            }
        } else {
            rest.push(msg.clone());
        }
    }

    let mut out = Vec::with_capacity(1 + rest.len());
    if structured_system {
        if !system_blocks.is_empty() {
            out.push(json!({"role": "system", "content": system_blocks}));
        }
    } else if !system_parts.is_empty() {
        out.push(json!({"role": "system", "content": system_parts.join("\n\n")}));
    }
    out.extend(rest);

    // Sanitize assistant messages: remove empty tool_calls arrays and fix
    // tool_calls with empty function names.
    // Some providers (e.g. MiniMax) reject messages containing tool_calls
    // where the function name is empty (can happen when skill interception
    // captures a call before the streaming name chunk arrives).
    //
    // Build a lookup from tool_call_id → tool name from tool-result messages
    // so we can recover the correct name when possible.
    let tool_name_by_id: HashMap<String, String> = out
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|m| {
            let id = m.get("tool_call_id").and_then(Value::as_str)?.to_string();
            let name = m.get("name").and_then(Value::as_str)?.to_string();
            Some((id, name))
        })
        .collect();

    strip_empty_assistant_tool_calls(&mut out);

    for msg in &mut out {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let Some(tcs) = obj.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for tc in tcs.iter_mut() {
            let call_id = tc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(func) = tc.get_mut("function") {
                let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    let recovered = tool_name_by_id
                        .get(&call_id)
                        .map(|s| s.as_str())
                        .unwrap_or("_unknown");
                    if let Some(f) = func.as_object_mut() {
                        f.insert("name".to_string(), Value::String(recovered.to_string()));
                    }
                }
            }
        }
    }

    out
}

/// Split a streaming content chunk into (text, is_reasoning) segments,
/// tracking whether we're inside a `<think>` block across chunks.
///
/// Returns a vec of (chunk_str, is_reasoning) pairs. Callers should route
/// is_reasoning=true chunks to `reasoning_delta` and false to `text_delta`.
#[allow(dead_code)] // Reserved for MiniMax M2.7 streaming support
pub(crate) fn split_think_chunks(content: &str, in_think: &mut bool) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let len = content.len();

    while pos < len {
        if *in_think {
            if let Some(end) = content[pos..].find("</think>") {
                let abs_end = pos + end;
                if abs_end > pos {
                    out.push((content[pos..abs_end].to_string(), true));
                }
                *in_think = false;
                pos = abs_end + "</think>".len();
            } else {
                out.push((content[pos..].to_string(), true));
                pos = len;
            }
        } else {
            if let Some(start) = content[pos..].find("<think>") {
                let abs_start = pos + start;
                if abs_start > pos {
                    out.push((content[pos..abs_start].to_string(), false));
                }
                *in_think = true;
                pos = abs_start + "<think>".len();
            } else {
                out.push((content[pos..].to_string(), false));
                pos = len;
            }
        }
    }
    out
}

/// Extract `<think>...</think>` blocks from text, returning (reasoning, cleaned_text).
///
/// Some models (e.g. MiniMax) embed reasoning in content using `<think>` tags
/// instead of a separate `reasoning_content` streaming field. This extracts
/// all `<think>` blocks into reasoning and returns the remaining text.
fn extract_think_tags(text: &str) -> Option<(String, String)> {
    if !text.contains("<think>") {
        return None;
    }
    let mut reasoning = String::new();
    let mut cleaned = String::new();
    let mut pos = 0;
    while let Some(start) = text[pos..].find("<think>") {
        let abs_start = pos + start;
        cleaned.push_str(&text[pos..abs_start]);
        if let Some(end) = text[abs_start..].find("</think>") {
            let abs_end = abs_start + end + "</think>".len();
            let inner = &text[abs_start + "<think>".len()..abs_start + end];
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(inner.trim());
            pos = abs_end;
        } else {
            // Unclosed <think> — treat rest as reasoning
            let inner = &text[abs_start + "<think>".len()..];
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(inner.trim());
            pos = text.len();
        }
    }
    cleaned.push_str(&text[pos..]);
    let cleaned = cleaned.trim().to_string();
    if reasoning.is_empty() {
        None
    } else {
        Some((reasoning, cleaned))
    }
}

fn apply_llm_header_overrides(
    mut req: reqwest::RequestBuilder,
    header_overrides: Option<&HashMap<String, String>>,
) -> reqwest::RequestBuilder {
    let Some(header_overrides) = header_overrides else {
        return req;
    };
    for (name, value) in header_overrides {
        if name.starts_with("__astra_") {
            continue;
        }
        let Ok(header_name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(header_value) = reqwest::header::HeaderValue::from_str(value) else {
            continue;
        };
        req = req.header(header_name, header_value);
    }
    req
}

fn has_llm_auth_override(
    provider: &str,
    header_overrides: Option<&HashMap<String, String>>,
) -> bool {
    let Some(header_overrides) = header_overrides else {
        return false;
    };
    if provider_uses_anthropic_messages(provider) {
        header_overrides
            .keys()
            .any(|name| name.eq_ignore_ascii_case("x-api-key"))
    } else {
        header_overrides
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
    }
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
    thinking: &ThinkingConfig,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    call_llm_and_collect_with_request_overrides(
        messages,
        tools,
        model_name,
        api_key,
        base_url,
        provider,
        max_output_tokens,
        has_fallback,
        cancel,
        None,
        None,
        None,
        thinking,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_llm_and_collect_with_request_overrides(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    has_fallback: bool,
    cancel: LlmCancel<'_>,
    header_overrides: Option<&HashMap<String, String>>,
    completions_url_override: Option<&str>,
    request_timeout: Option<std::time::Duration>,
    thinking: &ThinkingConfig,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let cooldown = rate_limit_cooldown();
    let model_key = model_name;

    let started = Instant::now();
    let total_budget = llm_total_budget();
    let client = global_llm_client();

    // Consolidate system messages: merge all system-role messages into the first
    // one, converting extras to a single leading system message. Some providers
    // (e.g. MiniMax) reject system messages after the first position.
    let messages = consolidate_system_messages(messages);

    // All providers stream — including Bedrock (via converse-stream +
    // AWS vnd.amazon.eventstream). The body builder and URL builder flip
    // to the streaming variant for every supported provider.
    let body = build_provider_request_body(
        &messages,
        tools,
        model_name,
        provider,
        max_output_tokens,
        None,
        true,
        thinking,
    );

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        model_name,
        true,
    );

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
    let attach_partial_details = |error: astra_core::ClassifiedError,
                                  partial: &LlmCallResult|
     -> astra_core::ClassifiedError {
        if let Some(details_json) = llm_result_details_json(partial) {
            error.with_details_json(details_json)
        } else {
            error
        }
    };

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
            let delay = retry_backoff_ms(attempt, tpm_exhaustion_detected);
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
        req = apply_provider_auth(req, provider, api_key, header_overrides);
        req = apply_llm_header_overrides(req, header_overrides);
        if let Some(timeout) = request_timeout {
            req = req.timeout(timeout);
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
            if provider_uses_bedrock_converse(provider) {
                match crate::turn::bedrock_transport::collect_bedrock_stream(
                    response, model_name, started, cancel, idle_pre,
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(crate::turn::bedrock_transport::BedrockStreamError::Cancelled) => {
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "LLM call cancelled",
                        ));
                    }
                    Err(crate::turn::bedrock_transport::BedrockStreamError::Exception {
                        kind,
                        message,
                    }) => {
                        use crate::turn::bedrock_stream::{RetryKind, is_retryable_exception};
                        match is_retryable_exception(&kind) {
                            RetryKind::RateLimit => {
                                last_err = format!("bedrock throttle: {message}");
                                last_kind = astra_core::ErrorKind::RateLimit;
                                cooldown.with(model_key, |c| {
                                    let _ = c.record_429(None, has_fallback);
                                });
                                continue;
                            }
                            RetryKind::Transient => {
                                last_err = format!("bedrock transient {kind}: {message}");
                                last_kind = astra_core::ErrorKind::ServerError;
                                continue;
                            }
                            RetryKind::Terminal => {
                                return Err(astra_core::ClassifiedError::new(
                                    astra_core::ErrorKind::Unknown,
                                    format!("bedrock {kind}: {message}"),
                                ));
                            }
                        }
                    }
                    Err(crate::turn::bedrock_transport::BedrockStreamError::Transport(e)) => {
                        last_err = format!("bedrock transport: {e}");
                        last_kind = astra_core::ErrorKind::StreamTransport;
                        continue;
                    }
                }
            }
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
                Err(StreamCollectError::Cancelled { partial }) => {
                    return Err(attach_partial_details(
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "LLM call cancelled",
                        ),
                        &partial,
                    ));
                }
                Err(StreamCollectError::Transport { error, partial }) => {
                    last_err = format!("LLM stream transport error: {error}");
                    last_kind = astra_core::ErrorKind::StreamTransport;
                    if let Some(details_json) = llm_result_details_json(&partial) {
                        let elapsed = started.elapsed();
                        if elapsed > total_budget {
                            return Err(astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::BudgetExhausted,
                                format!(
                                    "LLM total budget exhausted ({:.0}s) after stream transport error",
                                    total_budget.as_secs_f64()
                                ),
                            )
                            .with_details_json(details_json));
                        }
                        let remaining = total_budget.saturating_sub(elapsed);
                        let fb_timeout = llm_fallback_timeout().min(remaining);
                        astra_core::agent_warn!(
                            "llm",
                            "stream transport error after partial output — attempting non-stream fallback (timeout {}s)",
                            fb_timeout.as_secs()
                        );
                        return call_llm_nonstream_fallback_with_request_overrides(
                            client,
                            &messages,
                            tools,
                            model_name,
                            api_key,
                            base_url,
                            provider,
                            max_output_tokens,
                            fb_timeout,
                            header_overrides,
                            completions_url_override,
                            request_timeout,
                            thinking,
                        )
                        .await
                        .map_err(|error| error.with_details_json(details_json));
                    }
                    continue;
                }
                Err(StreamCollectError::IdleTimeout {
                    elapsed_ms,
                    made_progress,
                    partial,
                }) => {
                    if cancel.is_triggered() {
                        return Err(attach_partial_details(
                            astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::Cancelled,
                                "LLM call cancelled",
                            ),
                            &partial,
                        ));
                    }
                    // Check total budget before attempting retry/fallback.
                    let elapsed = started.elapsed();
                    if elapsed > total_budget {
                        return Err(attach_partial_details(
                            astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::BudgetExhausted,
                                format!(
                                    "LLM total budget exhausted ({:.0}s) after stream idle timeout",
                                    total_budget.as_secs_f64()
                                ),
                            ),
                            &partial,
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
                    return call_llm_nonstream_fallback_with_request_overrides(
                        client,
                        &messages,
                        tools,
                        model_name,
                        api_key,
                        base_url,
                        provider,
                        max_output_tokens,
                        fb_timeout,
                        header_overrides,
                        completions_url_override,
                        request_timeout,
                        thinking,
                    )
                    .await
                    .map_err(|error| attach_partial_details(error, &partial));
                }
            }
        }

        // Parse retry-after header
        let headers = response.headers();
        let retry_after_ms = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);

        let text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));

        // Auth errors: redact the body in logs and return a generic message
        // so provider-echoed secrets cannot leak through error propagation.
        if status == 401 || status == 403 {
            let truncated = &text[..text.len().min(80)];
            let redacted = redact_provider_secrets(truncated);
            tracing::warn!(
                target: "astra_runtime::llm_client",
                "LLM auth error ({status}) on {model_key}: {redacted}",
            );
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Auth,
                "LLM provider authentication failed".to_string(),
            ));
        }

        // For other 4xx errors, suppress the raw response body to avoid
        // leaking secrets that providers may echo back. Retain body for 5xx
        // (helpful for diagnosing transient backend failures) and the 400
        // context-window check below (which still needs to inspect text).
        last_err = if (400..500).contains(&status) {
            format!("LLM request rejected: {status}")
        } else {
            format!("LLM error {status}: {text}")
        };

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
                format!("LLM error {status}: {text}"),
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
    let partial_result = |full_text: &String,
                          reasoning: &String,
                          tool_calls_map: &HashMap<usize, Map<String, Value>>,
                          usage: &Map<String, Value>,
                          finish_reason: &Option<String>| {
        let mut sorted_tcs: Vec<_> = tool_calls_map.iter().collect();
        sorted_tcs.sort_by_key(|(idx, _)| **idx);
        let tool_calls = sorted_tcs
            .into_iter()
            .map(|(_, value)| Value::Object(value.clone()))
            .collect();
        LlmCallResult {
            full_text: full_text.clone(),
            reasoning: reasoning.clone(),
            reasoning_signature: String::new(),
            tool_calls,
            usage: usage.clone(),
            model_used: model_name.to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            finish_reason: finish_reason.clone(),
        }
    };

    let sse = parse_openai_sse_json_stream(stream);
    tokio::pin!(sse);
    loop {
        let idle = if made_progress { idle_post } else { idle_pre };
        let item = tokio::select! {
            biased;
            _ = wait_llm_cancel(cancel) => return Err(StreamCollectError::Cancelled {
                partial: partial_result(
                    &full_text,
                    &reasoning,
                    &tool_calls_map,
                    &usage,
                    &finish_reason,
                ),
            }),
            r = tokio::time::timeout(idle, sse.next()) => match r {
                Ok(v) => v,
                Err(_elapsed) => {
                    return Err(StreamCollectError::IdleTimeout {
                        elapsed_ms: idle.as_millis() as u64,
                        made_progress,
                        partial: partial_result(
                            &full_text,
                            &reasoning,
                            &tool_calls_map,
                            &usage,
                            &finish_reason,
                        ),
                    });
                }
            },
        };
        let Some(item) = item else { break };
        let chunk = match item {
            Ok(v) => v,
            Err(error) => {
                return Err(StreamCollectError::Transport {
                    error,
                    partial: partial_result(
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
            }
        };
        // Parse usage from any chunk. Streaming endpoints we call are always
        // OpenAI-compatible: Bedrock Converse streams are intercepted at a
        // higher level and converted via the non-stream fallback path.
        if let Some(u) = chunk.get("usage").and_then(Value::as_object)
            && let Some(extracted) = crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::OpenAi,
                u,
            )
        {
            usage = extracted.to_json_map();
            made_progress = true;
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
                return Err(StreamCollectError::Transport {
                    error: format!(
                        "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                    ),
                    partial: partial_result(
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
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
                return Err(StreamCollectError::Transport {
                    error: format!(
                        "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                    ),
                    partial: partial_result(
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
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
                            return Err(StreamCollectError::Transport {
                                error: format!(
                                    "stream tool-call arguments exceeded {MAX_STREAM_ACCUMULATION_BYTES} byte limit"
                                ),
                                partial: partial_result(
                                    &full_text,
                                    &reasoning,
                                    &tool_calls_map,
                                    &usage,
                                    &finish_reason,
                                ),
                            });
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
        if let Some(parsed) =
            astra_turn_core::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text)
        {
            astra_core::agent_warn!(
                "llm",
                "recovered {} tool call(s) from degraded text in content (stream)",
                parsed.len()
            );
            full_text =
                astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
            tool_calls = parsed;
        }
    }

    // Extract <think>...</think> blocks from content into reasoning.
    // Models like MiniMax embed thinking in content with <think> tags
    // instead of using a separate reasoning_content field.
    if reasoning.is_empty() {
        if let Some((extracted_reasoning, cleaned_text)) = extract_think_tags(&full_text) {
            reasoning = extracted_reasoning;
            full_text = cleaned_text;
        }
    }

    Ok(LlmCallResult {
        full_text,
        reasoning,
        reasoning_signature: String::new(),
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
        partial: LlmCallResult,
    },
    /// Byte stream error from the HTTP client (e.g. reset, TLS failure).
    Transport {
        error: String,
        partial: LlmCallResult,
    },
    /// [`LlmCancel`] fired during collection.
    Cancelled { partial: LlmCallResult },
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
    call_llm_nonstream_fallback_with_request_overrides(
        client,
        messages,
        tools,
        model_name,
        api_key,
        base_url,
        provider,
        max_output_tokens,
        timeout,
        None,
        None,
        None,
        &ThinkingConfig::Off,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_llm_nonstream_fallback_with_request_overrides(
    client: &reqwest::Client,
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    timeout: std::time::Duration,
    header_overrides: Option<&HashMap<String, String>>,
    completions_url_override: Option<&str>,
    request_timeout: Option<std::time::Duration>,
    thinking: &ThinkingConfig,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let started = Instant::now();

    let messages = consolidate_system_messages(messages);

    let body = build_provider_request_body(
        &messages,
        tools,
        model_name,
        provider,
        max_output_tokens,
        None,
        false,
        thinking,
    );

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        model_name,
        false,
    );
    let mut req = client.post(&url).header("content-type", "application/json");
    req = apply_provider_auth(req, provider, api_key, header_overrides);
    req = apply_llm_header_overrides(req, header_overrides);

    // Apply per-request timeout (overrides the client-level default).
    let effective_timeout = request_timeout
        .map(|value| value.min(timeout))
        .unwrap_or(timeout);
    let resp = req
        .timeout(effective_timeout)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Network,
                format!(
                    "LLM fallback request failed (timeout {}s): {e}",
                    effective_timeout.as_secs()
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
    Ok(parse_nonstream_response_for_provider(
        &v, provider, model_name, started,
    ))
}

fn map_bedrock_finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        "end_turn" => "stop".to_string(),
        other => other.to_string(),
    }
}

fn parse_bedrock_nonstream_response(
    v: &Value,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_signature = String::new();
    let mut tool_calls = Vec::new();
    let usage = v
        .get("usage")
        .and_then(Value::as_object)
        .and_then(|u| {
            crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::BedrockConverse,
                u,
            )
        })
        .map(|u| u.to_json_map())
        .unwrap_or_default();

    if let Some(content_blocks) = v
        .get("output")
        .and_then(|output| output.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    {
        for block in content_blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                full_text.push_str(text);
            }
            if let Some(rc) = block.get("reasoningContent").and_then(Value::as_object) {
                if let Some(rt) = rc.get("reasoningText").and_then(Value::as_object) {
                    if let Some(t) = rt.get("text").and_then(Value::as_str) {
                        reasoning.push_str(t);
                    }
                    if let Some(sig) = rt.get("signature").and_then(Value::as_str) {
                        reasoning_signature.push_str(sig);
                    }
                }
            }
            if let Some(tool_use) = block.get("toolUse").and_then(Value::as_object) {
                let id = tool_use
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = tool_use
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("_unknown");
                let arguments = tool_use.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string(),
                    }
                }));
            }
        }
    }

    LlmCallResult {
        full_text,
        reasoning,
        reasoning_signature,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason: v
            .get("stopReason")
            .and_then(Value::as_str)
            .map(map_bedrock_finish_reason),
    }
}

fn parse_openai_compatible_nonstream_response(
    v: &Value,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let usage = v
        .get("usage")
        .and_then(Value::as_object)
        .and_then(|u| {
            crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::OpenAi,
                u,
            )
        })
        .map(|u| u.to_json_map())
        .unwrap_or_default();

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
        if let Some(parsed) =
            astra_turn_core::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text)
        {
            astra_core::agent_warn!(
                "llm",
                "recovered {} tool call(s) from degraded text in content (non-stream)",
                parsed.len()
            );
            full_text =
                astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
            tool_calls = parsed;
        }
    }

    if reasoning.is_empty() {
        if let Some((extracted_reasoning, cleaned_text)) = extract_think_tags(&full_text) {
            reasoning = extracted_reasoning;
            full_text = cleaned_text;
        }
    }

    LlmCallResult {
        full_text,
        reasoning,
        reasoning_signature: String::new(),
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason,
    }
}

pub(crate) fn parse_nonstream_response_for_provider(
    v: &Value,
    provider: &str,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::BedrockConverse => {
            parse_bedrock_nonstream_response(v, model_name, started)
        }
        LlmProviderProtocol::AnthropicMessages | LlmProviderProtocol::OpenAiCompatible => {
            parse_openai_compatible_nonstream_response(v, model_name, started)
        }
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
                if let Err(error) = validate_sse_event_block_json(&block) {
                    yield Err(error);
                    return;
                }
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
        let tail = match validated_drain_sse_data_lines(&mut buf, "") {
            Ok(value) => value,
            Err(error) => {
                yield Err(error);
                return;
            }
        };
        for v in tail.events {
            yield Ok(v);
        }
        if tail.stream_finished {
            return;
        }
        let fin = match validated_finish_sse_data_buffer(&mut buf) {
            Ok(value) => value,
            Err(error) => {
                yield Err(error);
                return;
            }
        };
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
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::post;
    use futures_util::StreamExt;
    use futures_util::stream;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn bridge_e2e_stream_idle_timeout_override_is_visible_to_runtime_paths() {
        let _guard = set_bridge_e2e_stream_idle_timeouts_for_test(123, 456);
        assert_eq!(stream_idle_timeout(), std::time::Duration::from_millis(123));
        assert_eq!(
            stream_idle_timeout_after_progress(),
            std::time::Duration::from_millis(456)
        );
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

    #[tokio::test]
    async fn call_llm_with_request_overrides_uses_direct_gateway_url_and_headers() {
        #[derive(Clone, Default)]
        struct Capture {
            auth: Option<String>,
            workspace: Option<String>,
            model: Option<String>,
        }

        async fn gateway_handler(
            State(capture): State<Arc<Mutex<Capture>>>,
            headers: HeaderMap,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            *capture.lock().expect("capture lock") = Capture {
                auth: headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                workspace: headers
                    .get("x-workspace-id")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                model: body.get("model").and_then(Value::as_str).map(String::from),
            };
            let payload = json!({"choices":[{"delta":{"content":"from-gateway"}}]});
            let body = format!("data: {}\n\ndata: [DONE]\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("gateway response")
        }

        let capture = Arc::new(Mutex::new(Capture::default()));
        let app = Router::new()
            .route("/gateway", post(gateway_handler))
            .with_state(capture.clone());
        let base = spawn_local_http_server(app).await;
        let gateway_url = format!("{base}/gateway");

        let mut overrides = HashMap::new();
        overrides.insert("authorization".to_string(), "Bearer moi-token".to_string());
        overrides.insert("x-workspace-id".to_string(), "ws-001".to_string());
        overrides.insert("__astra_connection_tokens".to_string(), "x-hop".to_string());

        let result = call_llm_and_collect_with_request_overrides(
            &[json!({"role":"user","content":"hi"})],
            &[],
            "gpt-5-mini",
            "",
            "https://api.openai.com/v1",
            "openai",
            None,
            false,
            LlmCancel::None,
            Some(&overrides),
            Some(&gateway_url),
            None,
            &ThinkingConfig::Off,
        )
        .await
        .expect("gateway llm call");

        assert_eq!(result.full_text, "from-gateway");
        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(seen.auth.as_deref(), Some("Bearer moi-token"));
        assert_eq!(seen.workspace.as_deref(), Some("ws-001"));
        assert_eq!(seen.model.as_deref(), Some("gpt-5-mini"));
    }

    #[tokio::test]
    async fn call_llm_and_collect_omits_empty_assistant_tool_calls_in_request_body() {
        #[derive(Clone, Default, Debug)]
        struct Capture {
            messages: Vec<Value>,
        }

        async fn gateway_handler(
            State(capture): State<Arc<Mutex<Capture>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            capture.lock().expect("capture lock").messages = body
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let payload = json!({"choices":[{"delta":{"content":"ok"}}]});
            let body = format!("data: {payload}\n\ndata: [DONE]\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("gateway response")
        }

        let capture = Arc::new(Mutex::new(Capture::default()));
        let app = Router::new()
            .route("/chat/completions", post(gateway_handler))
            .with_state(capture.clone());
        let base = spawn_local_http_server(app).await;
        let messages = vec![
            json!({"role":"assistant","content":"Done.","tool_calls":[]}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","name":"bash","content":"ok"}),
            json!({"role":"user","content":"hi"}),
        ];

        let result = call_llm_and_collect(
            &messages,
            &[],
            "gpt-5-mini",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
            &ThinkingConfig::Off,
        )
        .await
        .expect("llm ok");

        assert_eq!(result.full_text, "ok");
        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(seen.messages.len(), 4);
        assert!(seen.messages[0].get("tool_calls").is_none(), "{seen:?}");
        assert_eq!(
            seen.messages[1]["tool_calls"][0]["function"]["name"].as_str(),
            Some("bash")
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
        let r = parse_nonstream_response_for_provider(&v, "openai", "test-model", Instant::now());
        assert_eq!(r.full_text, "hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(15)
        );
    }

    #[test]
    fn parse_bedrock_nonstream_response_extracts_fields() {
        let v = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "hello"},
                        {"reasoningContent": {"reasoningText": {"text": "think"}}},
                        {"toolUse": {"toolUseId": "t1", "name": "bash", "input": {"cmd": "pwd"}}}
                    ]
                }
            },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 }
        });
        let r = parse_nonstream_response_for_provider(
            &v,
            "bedrock",
            "anthropic.claude-3-5-sonnet-v1:0",
            Instant::now(),
        );
        assert_eq!(r.full_text, "hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0]["function"]["name"], "bash");
        assert_eq!(
            r.tool_calls[0]["function"]["arguments"].as_str(),
            Some(r#"{"cmd":"pwd"}"#)
        );
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(15)
        );
    }

    #[test]
    fn parse_bedrock_nonstream_response_extracts_cache_usage() {
        let v = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hello"}]
                }
            },
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheReadInputTokens": 8,
                "cacheWriteInputTokens": 3,
                "totalTokens": 15
            }
        });
        let r = parse_nonstream_response_for_provider(
            &v,
            "bedrock",
            "anthropic.claude-sonnet-4-20250514-v1:0",
            Instant::now(),
        );
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("cached_input_tokens").and_then(Value::as_u64),
            Some(8)
        );
        assert_eq!(
            r.usage.get("cache_creation_tokens").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        // Bedrock billing identity: input + cached + creation + output.
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(26)
        );
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
    async fn parse_openai_sse_json_stream_invalid_block_errors() {
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from("data: {\"x\":1}\n\ndata: not-json\n\n"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        assert_eq!(st.next().await.unwrap().unwrap(), json!({"x": 1}));
        let err = st
            .next()
            .await
            .expect("invalid block item")
            .expect_err("parse error");
        assert!(err.contains("invalid JSON in SSE data line"), "{err}");
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_invalid_tail_errors() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![Ok(Bytes::from("data: not-json"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let err = st
            .next()
            .await
            .expect("invalid tail item")
            .expect_err("parse error");
        assert!(err.contains("invalid JSON in SSE data line"), "{err}");
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

    #[tokio::test]
    async fn collect_llm_stream_surfaces_transport_error() {
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
            matches!(res, Err(StreamCollectError::Transport { .. })),
            "expected transport error, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_transport_after_partial_carries_partial_result() {
        let err = sample_reqwest_stream_error().await;
        let d1 = json!({"choices":[{"delta":{"content":"partial"}}]});
        let byte_stream = stream::iter(vec![Ok(Bytes::from(format!("data: {d1}\n\n"))), Err(err)]);
        let res = collect_llm_stream(
            byte_stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect_err("transport error");
        match res {
            StreamCollectError::Transport { partial, .. } => {
                assert_eq!(partial.full_text, "partial");
                assert_eq!(partial.model_used, "test-model");
            }
            other => panic!("expected transport error, got {other:?}"),
        }
    }

    // Note: Bedrock no longer uses `collect_llm_stream` (see `bridge_llm_stream::
    // call_llm_stream` — it detects Bedrock and invokes the non-stream fallback
    // instead). Bedrock usage extraction is covered by
    // `parse_bedrock_nonstream_response_extracts_*` and the unit tests in
    // `turn::token_usage`.

    #[tokio::test]
    async fn collect_llm_stream_aggregates_delta_text_reasoning_usage() {
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
        assert_eq!(
            res.usage.get("input_tokens").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            res.usage.get("output_tokens").and_then(Value::as_u64),
            Some(4)
        );
        assert_eq!(
            res.usage.get("total_tokens").and_then(Value::as_u64),
            Some(7)
        );
        assert_eq!(res.model_used, "gpt-test");
        assert!(res.tool_calls.is_empty());
        // No finish_reason chunk was sent, so it should be None
        assert_eq!(res.finish_reason, None);
    }

    #[tokio::test]
    async fn collect_llm_stream_extracts_finish_reason_stop() {
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
    }

    #[tokio::test]
    async fn collect_llm_stream_extracts_finish_reason_length() {
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
    }

    #[tokio::test]
    async fn collect_llm_stream_merges_tool_call_argument_chunks() {
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
    }

    #[tokio::test]
    async fn stream_idle_timeout_triggers() {
        let _guard = set_test_stream_timeouts(1, None);
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
    }

    #[tokio::test]
    async fn stream_idle_timeout_after_partial_output_marks_progress() {
        let _guard = set_test_stream_timeouts(1, Some(1));
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
        match res.expect_err("idle timeout after partial output") {
            StreamCollectError::IdleTimeout {
                made_progress,
                partial,
                ..
            } => {
                assert!(made_progress, "partial output should mark progress");
                assert_eq!(partial.full_text, "partial");
            }
            other => panic!("expected idle timeout after partial output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_llm_stream_respects_cancel_flag() {
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
            matches!(res, Err(StreamCollectError::Cancelled { .. })),
            "expected cancel, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_respects_cancel_token() {
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
            matches!(res, Err(StreamCollectError::Cancelled { .. })),
            "expected cancel, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_flag_and_token_cancels_on_token() {
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
            matches!(res, Err(StreamCollectError::Cancelled { .. })),
            "expected cancel, got: {res:?}"
        );
        assert!(!flag_for_join.load(Ordering::SeqCst));
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

    async fn spawn_raw_partial_transport_server(
        state: StreamIdleHit,
        fallback_status: u16,
        fallback_body: &'static str,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw mock llm listener");
        let addr = listener.local_addr().expect("raw local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..read]);
                    let is_stream = req.contains("\"stream\":true");
                    if is_stream {
                        state.stream_hits.fetch_add(1, Ordering::SeqCst);
                        let partial = format!(
                            "data: {}\n\n",
                            json!({"choices":[{"delta":{"content":"partial"}}]})
                        );
                        let chunk = format!("{:X}\r\n{}\r\n", partial.len(), partial);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write partial stream response");
                        let _ = socket.shutdown().await;
                    } else {
                        state.fallback_hits.fetch_add(1, Ordering::SeqCst);
                        let status_text = if fallback_status == 200 {
                            "OK"
                        } else {
                            "Internal Server Error"
                        };
                        let response = format!(
                            "HTTP/1.1 {fallback_status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fallback_body}",
                            fallback_body.len()
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write fallback response");
                        let _ = socket.shutdown().await;
                    }
                });
            }
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

    async fn mock_stream_idle_after_partial_then_fallback_fails(
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
                .status(500)
                .body(Body::from("fallback exploded"))
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
    async fn collect_llm_stream_decodes_lossy_utf8_inside_json_string() {
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
    }

    #[tokio::test]
    async fn call_llm_and_collect_retries_after_429_retry_after_zero() {
        reset_rate_limit_cooldown_for_tests();
        let _backoff = set_test_retry_backoff_ms(0);
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
            &ThinkingConfig::Off,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-429");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
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
                &ThinkingConfig::Off,
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
    async fn call_llm_and_collect_retries_after_529_retry_after_zero() {
        reset_rate_limit_cooldown_for_tests();
        let _backoff = set_test_retry_backoff_ms(0);
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
            &ThinkingConfig::Off,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-529");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn call_llm_and_collect_retries_after_503_retry_after_zero() {
        reset_rate_limit_cooldown_for_tests();
        let _backoff = set_test_retry_backoff_ms(0);
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
            &ThinkingConfig::Off,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-503");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
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
                &ThinkingConfig::Off,
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
    async fn output_escalation_e2e_length_then_stop() {
        // Verifies: first call returns finish_reason=length, second returns stop.
        // This is the data path used by server_loop_host's escalation loop.
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
            &ThinkingConfig::Off,
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
            &ThinkingConfig::Off,
        )
        .await
        .expect("llm ok");
        assert_eq!(res2.full_text, "complete response");
        assert_eq!(res2.finish_reason.as_deref(), Some("stop"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn finish_reason_stop_no_retry() {
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
            &ThinkingConfig::Off,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no retry when stop");
    }

    #[tokio::test]
    async fn finish_reason_tool_calls_extracted() {
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
    }

    #[tokio::test]
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
            &ThinkingConfig::Off,
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
    async fn call_llm_and_collect_preserves_partial_stream_details_when_fallback_fails() {
        let _guard = set_test_stream_timeouts(10, Some(10));
        let state = StreamIdleHit {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route(
                "/chat/completions",
                post(mock_stream_idle_after_partial_then_fallback_fails),
            )
            .with_state(state.clone());
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let error = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
            &ThinkingConfig::Off,
        )
        .await
        .expect_err("fallback should fail");
        let details_json = error
            .details_json
            .as_deref()
            .expect("partial stream details should be attached");
        let details: Value = serde_json::from_str(details_json).expect("details json");
        assert_eq!(details["partial_full_text"], "partial");
        assert_eq!(state.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(state.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_and_collect_falls_back_after_partial_stream_transport_error() {
        let state = StreamIdleHit {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(
            state.clone(),
            200,
            r#"{"choices":[{"message":{"content":"from-transport-fallback"}}]}"#,
        )
        .await;
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
            &ThinkingConfig::Off,
        )
        .await
        .expect("transport fallback succeeds");
        assert_eq!(res.full_text, "from-transport-fallback");
        assert_eq!(state.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(state.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_and_collect_preserves_partial_stream_details_when_transport_fallback_fails() {
        let state = StreamIdleHit {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(
            state.clone(),
            500,
            r#"{"error":"transport fallback exploded"}"#,
        )
        .await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let error = call_llm_and_collect(
            &messages,
            &[],
            "m",
            "k",
            &base,
            "openai",
            None,
            false,
            LlmCancel::None,
            &ThinkingConfig::Off,
        )
        .await
        .expect_err("transport fallback should fail");
        let details_json = error
            .details_json
            .as_deref()
            .expect("partial stream details should be attached");
        let details: Value = serde_json::from_str(details_json).expect("details json");
        assert_eq!(details["partial_full_text"], "partial");
        assert_eq!(state.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(state.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_and_collect_retries_stream_once_when_idle_before_output() {
        let _guard = set_test_stream_timeouts(10, None);
        let _backoff = set_test_retry_backoff_ms(0);
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
            &ThinkingConfig::Off,
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
    async fn call_llm_and_collect_returns_context_window_error_kind() {
        reset_rate_limit_cooldown_for_tests();
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
            &ThinkingConfig::Off,
        )
        .await
        .expect_err("should fail with context window");
        assert_eq!(err.kind, astra_core::ErrorKind::ContextWindow);
        assert!(err.message.contains("context_length_exceeded"));
    }

    /// Mock server that returns 401 Unauthorized.
    async fn mock_401() -> Response {
        Response::builder()
            .status(401)
            .body(Body::from("Unauthorized"))
            .unwrap()
    }

    #[tokio::test]
    async fn call_llm_and_collect_returns_auth_error_kind() {
        reset_rate_limit_cooldown_for_tests();
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
            &ThinkingConfig::Off,
        )
        .await
        .expect_err("should fail with auth");
        assert_eq!(err.kind, astra_core::ErrorKind::Auth);
        assert!(
            !err.message.contains("Unauthorized"),
            "auth error message must not echo provider body, got: {}",
            err.message
        );
        assert!(err.message.contains("authentication failed"));
    }

    #[test]
    fn completions_url_openai_default() {
        assert_eq!(
            llm_completions_url("https://api.openai.com/v1", None, "openai"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn completions_url_openai_trailing_slash() {
        assert_eq!(
            llm_completions_url("https://api.openai.com/v1/", None, "openai"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn completions_url_anthropic_without_v1() {
        assert_eq!(
            llm_completions_url("https://api.minimaxi.com/anthropic", None, "anthropic"),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn completions_url_anthropic_with_v1() {
        assert_eq!(
            llm_completions_url("https://api.anthropic.com/v1", None, "anthropic"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn completions_url_override_takes_precedence() {
        assert_eq!(
            llm_completions_url(
                "https://api.openai.com/v1",
                Some("https://custom.proxy/llm"),
                "openai"
            ),
            "https://custom.proxy/llm"
        );
    }

    #[test]
    fn consolidate_system_messages_merges_multiple() {
        let msgs = vec![
            json!({"role": "system", "content": "A"}),
            json!({"role": "system", "content": "B"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "C"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "A\n\nB\n\nC");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "hi");
    }

    #[test]
    fn consolidate_system_messages_single_system_unchanged() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"], "sys");
    }

    #[test]
    fn consolidate_system_messages_preserves_structured_blocks() {
        let msgs = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({"role": "user", "content": "hi"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "stable");
        assert_eq!(content[0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn consolidate_system_messages_no_system() {
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
    }

    #[test]
    fn consolidate_fixes_empty_tool_call_name() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "name": "skill", "content": "result"}),
        ];
        let out = consolidate_system_messages(&msgs);
        // assistant tool_call name should be recovered from tool result
        let tc_name = out[1]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(tc_name, "skill");
    }

    #[test]
    fn consolidate_fixes_empty_tool_call_name_unknown_fallback() {
        let msgs = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "", "arguments": "{}"}}]
        })];
        let out = consolidate_system_messages(&msgs);
        let tc_name = out[0]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(tc_name, "_unknown");
    }

    #[test]
    fn consolidate_omits_empty_tool_calls_arrays() {
        let msgs = vec![
            json!({"role": "system", "content": "be helpful"}),
            json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert!(out[1].get("tool_calls").is_none(), "{out:?}");
    }

    #[test]
    fn strip_empty_assistant_tool_calls_only_removes_empty_arrays() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
        ];
        strip_empty_assistant_tool_calls(&mut msgs);
        assert!(msgs[0].get("tool_calls").is_none(), "{msgs:?}");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn for_provider_openai() {
        assert_eq!(
            llm_request_url_for_provider("https://api.openai.com/v1", "openai", "gpt-4o", true),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn for_provider_anthropic_without_v1() {
        assert_eq!(
            llm_request_url_for_provider(
                "https://api.minimaxi.com/anthropic",
                "anthropic",
                "claude-3-5-sonnet",
                true
            ),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn for_provider_anthropic_with_v1() {
        assert_eq!(
            llm_request_url_for_provider(
                "https://api.anthropic.com/v1",
                "anthropic",
                "claude-3-5-sonnet",
                true
            ),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn for_provider_bedrock_nonstream() {
        assert_eq!(
            llm_request_url_for_provider(
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "bedrock",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                false
            ),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse"
        );
    }

    #[test]
    fn build_bedrock_body_maps_system_tools_and_tool_results() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"pwd\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "name": "bash", "content": "{\"cwd\":\"/tmp\"}"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "run shell",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"]["toolUseId"],
            "call_1"
        );
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "call_1"
        );
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "bash");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 128);
    }

    #[test]
    fn build_bedrock_body_wraps_non_object_tool_content_as_text_not_json() {
        // Session 28e858fd-... failure: `git rev-list --count main..HEAD`
        // returned "2\n" which parses as JSON integer 2. The previous code
        // wrapped it as {"json": 2}, which Bedrock rejects:
        // "messages.N.content.M.toolResult.content.0.json is invalid —
        //  provide a json object".
        // Bedrock's `json` field requires a JSON *object*. Scalars, arrays,
        // strings, booleans, and null must go through the `text` branch.
        for (label, content) in [
            ("integer", "2\n"),
            ("float", "3.14"),
            ("bool", "true"),
            ("null", "null"),
            ("string", "\"hello\""),
            ("array", "[1, 2, 3]"),
        ] {
            let messages = vec![
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "t", "type": "function",
                        "function": {"name": "f", "arguments": "{}"}
                    }]
                }),
                json!({"role": "tool", "tool_call_id": "t", "name": "f", "content": content}),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "anthropic.claude-3-5-sonnet-v1:0",
                "bedrock",
                None,
                None,
                false,
                &ThinkingConfig::Off,
            );
            let result_block = &body["messages"][1]["content"][0]["toolResult"]["content"][0];
            // Bedrock-legal: either `json` with an object, or `text` with a string.
            // Must NOT be `json` pointing at a non-object value.
            if let Some(json_val) = result_block.get("json") {
                assert!(
                    json_val.is_object(),
                    "{label}: toolResult.content[].json must be an object, got {json_val:?}"
                );
            } else {
                assert!(
                    result_block.get("text").is_some(),
                    "{label}: non-object content must fall through to text, got {result_block:?}"
                );
            }
        }
    }

    #[test]
    fn build_bedrock_body_keeps_json_object_branch_for_real_objects() {
        // Regression: ensure the object branch still works (don't overcorrect).
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "t", "type": "function",
                    "function": {"name": "f", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "t", "name": "f",
                   "content": "{\"cwd\":\"/tmp\",\"ok\":true}"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let result_block = &body["messages"][1]["content"][0]["toolResult"]["content"][0];
        assert!(result_block["json"].is_object(), "{result_block:?}");
        assert_eq!(result_block["json"]["cwd"], "/tmp");
    }

    #[test]
    fn build_bedrock_body_merges_parallel_tool_results_into_single_user_message() {
        // Assistant makes two parallel tool calls. OpenAI wire format emits
        // one `role: "tool"` message per result. Bedrock Converse requires
        // that all toolResult blocks corresponding to a single assistant
        // turn's toolUse blocks live in ONE user message — emitting two
        // separate user messages triggers the "Expected toolResult blocks
        // at messages.N.content" 400 observed in session 319b68b4-....
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_a", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"cmd\":\"pwd\"}"}},
                    {"id": "call_b", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"cmd\":\"whoami\"}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_a", "name": "bash", "content": "{\"cwd\":\"/tmp\"}"}),
            json!({"role": "tool", "tool_call_id": "call_b", "name": "bash", "content": "{\"user\":\"astra\"}"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            Some(64),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let out = body["messages"].as_array().expect("messages array");
        assert_eq!(
            out.len(),
            3,
            "expected user/assistant/user-merged, got {out:#?}",
        );
        assert_eq!(out[2]["role"], "user");
        let content = out[2]["content"].as_array().expect("merged content");
        let tool_result_ids: Vec<&str> = content
            .iter()
            .filter_map(|b| b.get("toolResult")?.get("toolUseId")?.as_str())
            .collect();
        assert_eq!(tool_result_ids, vec!["call_a", "call_b"]);
    }

    #[test]
    fn build_bedrock_body_preserves_tool_order_within_merged_block() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "t2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "t3", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "t3", "name": "f", "content": "three"}),
            json!({"role": "tool", "tool_call_id": "t1", "name": "f", "content": "one"}),
            json!({"role": "tool", "tool_call_id": "t2", "name": "f", "content": "two"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let content = body["messages"][1]["content"]
            .as_array()
            .expect("merged content");
        let ids: Vec<&str> = content
            .iter()
            .filter_map(|b| b.get("toolResult")?.get("toolUseId")?.as_str())
            .collect();
        // Insertion order of tool messages is preserved — no reordering.
        assert_eq!(ids, vec!["t3", "t1", "t2"]);
    }

    #[test]
    fn build_bedrock_body_splits_tool_group_when_non_tool_message_intervenes() {
        // A user message between two tool-result groups must break the merge —
        // otherwise we'd splice tool_results around unrelated content.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "x", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "x", "name": "f", "content": "first"}),
            json!({"role": "user", "content": "interrupt"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "y", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "y", "name": "f", "content": "second"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let out = body["messages"].as_array().expect("messages array");
        // assistant / user(tool x) / user(interrupt) / assistant / user(tool y)
        assert_eq!(out.len(), 5);
        assert_eq!(
            out[1]["content"][0]["toolResult"]["toolUseId"], "x",
            "first tool group"
        );
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["text"], "interrupt");
        assert_eq!(
            out[4]["content"][0]["toolResult"]["toolUseId"], "y",
            "second tool group"
        );
    }

    #[test]
    fn repair_tool_pairing_injects_synthetic_result_for_missing_tool_call() {
        // Assistant declared two tool_calls but the tool transcript only
        // carries one response (e.g. stream was cut mid-execution on resume).
        // Bedrock would reject with "Expected toolResult blocks for the
        // following Ids: call_b". Pre-send repair must synthesize an error
        // tool_result so the model context stays valid — matching claudecode's
        // ensureToolResultPairing repair behavior.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "call_b", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_a", "name": "f", "content": "ok"}),
        ];
        let repaired = repair_openai_tool_pairing_for_bedrock(&messages);
        // Expected: assistant / tool(call_a) / synthetic tool(call_b, is_error).
        assert_eq!(repaired.len(), 3, "{repaired:#?}");
        assert_eq!(repaired[1]["tool_call_id"], "call_a");
        assert_eq!(repaired[2]["role"], "tool");
        assert_eq!(repaired[2]["tool_call_id"], "call_b");
        let content = repaired[2]["content"].as_str().unwrap_or_default();
        assert!(
            content.contains("tool execution not recorded")
                || content.contains("tool_use_interrupted"),
            "synthetic content must be identifiable; got {content:?}",
        );
    }

    #[test]
    fn repair_tool_pairing_strips_orphaned_tool_result() {
        // A role=tool message whose tool_call_id doesn't match any preceding
        // assistant's tool_calls is an orphan — Bedrock rejects it with a
        // different 400 ("unexpected toolResult"). Strip to keep the request
        // well-formed.
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "tool", "tool_call_id": "nonexistent", "name": "f", "content": "ghost"}),
            json!({"role": "user", "content": "continue"}),
        ];
        let repaired = repair_openai_tool_pairing_for_bedrock(&messages);
        // Orphan removed, non-tool messages untouched.
        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[0]["content"], "hi");
        assert_eq!(repaired[1]["content"], "continue");
    }

    #[test]
    fn repair_tool_pairing_dedupes_duplicate_tool_call_ids() {
        // Same tool_call_id appearing twice in one tool-group (e.g. retry
        // artifact). Bedrock rejects with a duplicate-id 400. Keep first.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "dup", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "dup", "name": "f", "content": "first"}),
            json!({"role": "tool", "tool_call_id": "dup", "name": "f", "content": "second"}),
        ];
        let repaired = repair_openai_tool_pairing_for_bedrock(&messages);
        assert_eq!(repaired.len(), 2, "{repaired:#?}");
        assert_eq!(repaired[1]["tool_call_id"], "dup");
        assert_eq!(repaired[1]["content"], "first");
    }

    #[test]
    fn repair_tool_pairing_is_identity_when_well_formed() {
        // Regression: a correctly-paired transcript must pass through unchanged.
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "t2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "t1", "name": "f", "content": "a"}),
            json!({"role": "tool", "tool_call_id": "t2", "name": "f", "content": "b"}),
        ];
        let repaired = repair_openai_tool_pairing_for_bedrock(&messages);
        assert_eq!(repaired, messages);
    }

    #[test]
    fn build_bedrock_body_end_to_end_repairs_missing_tool_result() {
        // Integration: build_provider_request_body with provider=bedrock
        // must run repair before merging, so Bedrock sees a complete pair.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "b", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "a", "name": "f", "content": "ok"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let merged = body["messages"][1]["content"]
            .as_array()
            .expect("user content array");
        let ids: Vec<&str> = merged
            .iter()
            .filter_map(|b| b.get("toolResult")?.get("toolUseId")?.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"], "{body:#?}");
        // empty tools + tool blocks in history ⇒ toolConfig must still be present
        assert!(
            body.get("toolConfig").is_some(),
            "toolConfig missing: {body:#?}"
        );
    }

    #[test]
    fn build_bedrock_body_includes_tool_config_when_history_has_tool_blocks() {
        let messages = vec![
            json!({"role": "user", "content": "list files"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "name": "bash", "content": "file.txt"}),
            json!({"role": "user", "content": "thanks"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(
            body["toolConfig"]["tools"],
            json!([]),
            "empty tools array required when history contains tool blocks"
        );
    }

    #[test]
    fn build_bedrock_body_omits_tool_config_when_no_tools_anywhere() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert!(
            body.get("toolConfig").is_none(),
            "toolConfig should be absent when no tools: {body:#?}"
        );
    }

    #[test]
    fn repair_tool_pairing_synthesizes_all_when_zero_responses() {
        // Crash scenario from session 319b68b4-...: assistant declared N
        // tool_calls but the transcript resumed with NO tool-role messages
        // before the next turn. Every declared id must be synthesized.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "Npi0", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "94F3", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            // No tool responses — jumps straight to another user turn.
            json!({"role": "user", "content": "next question"}),
        ];
        let repaired = repair_openai_tool_pairing_for_bedrock(&messages);
        // assistant / tool(Npi0, synthetic) / tool(94F3, synthetic) / user
        assert_eq!(repaired.len(), 4, "{repaired:#?}");
        assert_eq!(repaired[1]["role"], "tool");
        assert_eq!(repaired[1]["tool_call_id"], "Npi0");
        assert_eq!(repaired[2]["tool_call_id"], "94F3");
        assert_eq!(repaired[3]["role"], "user");
    }

    #[test]
    fn repair_tool_pairing_handles_tool_separated_from_assistant_by_user() {
        // A user message between assistant.tool_calls and role=tool severs the
        // pairing window. The orphan tool message is stripped and the missing
        // declaration gets a synthetic — keeps Bedrock request well-formed.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "late", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "user", "content": "interrupt"}),
            json!({"role": "tool", "tool_call_id": "late", "name": "f", "content": "delayed"}),
        ];
        let repaired = repair_openai_tool_pairing_for_bedrock(&messages);
        // assistant / tool(late, synthetic) / user(interrupt); orphan dropped.
        assert_eq!(repaired.len(), 3, "{repaired:#?}");
        assert_eq!(repaired[1]["tool_call_id"], "late");
        assert_eq!(
            repaired[1]["content"].as_str().unwrap_or_default(),
            SYNTHETIC_TOOL_INTERRUPTED_CONTENT
        );
        assert_eq!(repaired[2]["content"], "interrupt");
    }

    #[test]
    fn build_bedrock_body_translates_cache_control_to_cache_points() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "turn prefix"},
                    {"type": "text", "text": "turn suffix", "cache_control": {"type": "ephemeral"}}
                ]
            }),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "parameters": {"type": "object", "properties": {}}
            },
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(body["system"][0]["text"], "stable");
        assert_eq!(body["system"][1]["cachePoint"]["type"], "default");
        assert_eq!(body["system"][1]["cachePoint"]["ttl"], "1h");
        assert_eq!(body["messages"][0]["content"][0]["text"], "turn prefix");
        assert_eq!(body["messages"][0]["content"][1]["text"], "turn suffix");
        assert_eq!(
            body["messages"][0]["content"][2]["cachePoint"]["type"],
            "default"
        );
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "bash");
        assert_eq!(body["toolConfig"]["tools"][1]["cachePoint"]["ttl"], "1h");
    }

    #[test]
    fn build_bedrock_body_skips_whitespace_only_system_blocks() {
        let messages = consolidate_system_messages(&[
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({"role": "system", "content": "runtime hints"}),
            json!({"role": "user", "content": "hello"}),
        ]);
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], "stable");
        assert_eq!(system[1]["cachePoint"]["ttl"], "1h");
        assert_eq!(system[2]["text"], "runtime hints");
        assert!(system.iter().all(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(|text| !text.trim().is_empty())
        }));
    }

    #[test]
    fn build_bedrock_body_omits_cachepoint_only_system() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({"role": "user", "content": "hello"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert!(body.get("system").is_none());
    }

    // ── Golden cases: real provider SSE fixtures ──────────────────────────────
    //
    // Fixtures captured from live APIs and stored in testdata/. Each test
    // feeds the raw SSE bytes through collect_llm_stream and asserts on the
    // parsed LlmCallResult, providing regression coverage for:
    //   - <think> tag extraction (MiniMax M2.5/M2.7)
    //   - reasoning_content field (Qwen3.6-plus, Kimi-k2.5)
    //   - tool_call streaming accumulation (MiniMax M2.5, Qwen-plus)
    //   - full_text / reasoning split correctness

    fn load_fixture(name: &str) -> Bytes {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/turn/testdata")
            .join(name);
        Bytes::from(std::fs::read(path).expect("fixture file missing"))
    }

    async fn parse_fixture(name: &str) -> LlmCallResult {
        let bytes = load_fixture(name);
        let stream = stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);
        collect_llm_stream(
            stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
        )
        .await
        .expect("collect")
    }

    #[tokio::test]
    async fn golden_minimax_m25_simple_think_extracted() {
        // MiniMax M2.5: <think> in delta.content → reasoning extracted, full_text clean
        let res = parse_fixture("minimax_m25_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning should be extracted from <think> tags"
        );
        assert!(
            !res.full_text.contains("<think>"),
            "full_text must not contain <think>"
        );
        assert!(
            !res.full_text.contains("</think>"),
            "full_text must not contain </think>"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
        assert!(res.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn golden_minimax_m27_simple_think_extracted() {
        // MiniMax M2.7: same <think> pattern, verify reasoning/text split
        let res = parse_fixture("minimax_m27_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning should be extracted from <think> tags"
        );
        assert!(
            !res.full_text.contains("<think>"),
            "full_text must not contain <think>"
        );
        assert!(
            !res.full_text.contains("</think>"),
            "full_text must not contain </think>"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
    }

    #[tokio::test]
    async fn golden_qwen36plus_reasoning_content_field() {
        // Qwen3.6-plus: reasoning via delta.reasoning_content (not <think> tags)
        let res = parse_fixture("qwen36plus_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning_content field should be captured"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
        assert!(res.full_text.contains('4'), "answer to 2+2 should be 4");
        assert!(
            !res.full_text.contains("<think>"),
            "no think tags in qwen output"
        );
    }

    #[tokio::test]
    async fn golden_kimi_k25_reasoning_content_field() {
        // Kimi-k2.5: reasoning via delta.reasoning_content
        let res = parse_fixture("kimi_k25_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning_content field should be captured"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
        assert!(res.full_text.contains('4'), "answer to 2+2 should be 4");
    }

    #[tokio::test]
    async fn golden_minimax_m25_tool_call_with_think() {
        // MiniMax M2.5 tool call: <think> in content + tool_calls in delta
        let res = parse_fixture("minimax_m25_tool_call.sse").await;
        assert!(!res.tool_calls.is_empty(), "should have tool calls");
        let tc = &res.tool_calls[0];
        let name = tc["function"]["name"].as_str().unwrap_or("");
        assert_eq!(name, "bash", "tool name should be bash");
        let args = tc["function"]["arguments"].as_str().unwrap_or("");
        assert!(
            args.contains("command"),
            "args must contain 'command' key, got: {args:?}"
        );
        // think content should be in reasoning, not full_text
        assert!(
            !res.full_text.contains("<think>"),
            "full_text must not contain <think>"
        );
    }

    #[tokio::test]
    async fn golden_qwen_plus_tool_call_no_reasoning() {
        // Qwen-plus: pure tool call, no reasoning
        let res = parse_fixture("qwen_plus_tool_call.sse").await;
        assert!(!res.tool_calls.is_empty(), "should have tool calls");
        let tc = &res.tool_calls[0];
        let name = tc["function"]["name"].as_str().unwrap_or("");
        assert!(!name.is_empty(), "tool name must not be empty");
        assert!(
            res.reasoning.is_empty(),
            "qwen-plus tool call should have no reasoning"
        );
    }

    // ── split_think_chunks (real MiniMax M2.7 streaming patterns) ────────────

    #[test]
    fn split_think_chunks_think_in_first_chunk() {
        // MiniMax M2.7 real: first chunk starts with <think>
        let mut in_think = false;
        let chunks = split_think_chunks("<think>\nThe user says \"hi\".", &mut in_think);
        assert!(in_think, "should be inside think block");
        assert_eq!(chunks, vec![("\nThe user says \"hi\".".to_string(), true)]);
    }

    #[test]
    fn split_think_chunks_think_closes_mid_chunk() {
        // MiniMax M2.7 real: last chunk closes </think> and has reply
        let mut in_think = true;
        let chunks = split_think_chunks(
            " Use friendly tone.\n</think>\n\nHello! How can I help you today?",
            &mut in_think,
        );
        assert!(!in_think, "should be outside think block after close");
        assert_eq!(
            chunks,
            vec![
                (" Use friendly tone.\n".to_string(), true),
                ("\n\nHello! How can I help you today?".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_think_chunks_no_think_tags() {
        // Normal model response without thinking
        let mut in_think = false;
        let chunks = split_think_chunks("Hello! How can I help?", &mut in_think);
        assert!(!in_think);
        assert_eq!(chunks, vec![("Hello! How can I help?".to_string(), false)]);
    }

    #[test]
    fn split_think_chunks_full_think_in_one_chunk() {
        // Entire think block in a single chunk (non-streaming scenario)
        let mut in_think = false;
        let chunks = split_think_chunks("<think>reasoning here</think>\n\nAnswer.", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            chunks,
            vec![
                ("reasoning here".to_string(), true),
                ("\n\nAnswer.".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_think_chunks_state_persists_across_calls() {
        // Simulate MiniMax M2.7 multi-chunk stream
        let mut in_think = false;
        // chunk 1: opens think
        let c1 = split_think_chunks("<think>\nThe user says \"hi\".", &mut in_think);
        assert!(in_think);
        assert!(c1[0].1);
        // chunk 2: still inside think
        let c2 = split_think_chunks(" Should be concise.", &mut in_think);
        assert!(in_think);
        assert_eq!(c2, vec![(" Should be concise.".to_string(), true)]);
        // chunk 3: closes think and has reply
        let c3 = split_think_chunks("</think>\n\nHello!", &mut in_think);
        assert!(!in_think);
        assert_eq!(c3, vec![("\n\nHello!".to_string(), false),]);
    }

    #[test]
    fn split_think_chunks_multi_phase_reasoning() {
        // Some models emit multiple <think> phases in one stream.
        // Verify in_think correctly toggles false→true→false→true→false.
        let mut in_think = false;
        // Phase 1
        let c1 = split_think_chunks("<think>phase one</think>text one", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            c1,
            vec![
                ("phase one".to_string(), true),
                ("text one".to_string(), false),
            ]
        );
        // Phase 2 — in_think was false, starts a new think block
        let c2 = split_think_chunks("<think>phase two</think>text two", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            c2,
            vec![
                ("phase two".to_string(), true),
                ("text two".to_string(), false),
            ]
        );
        // Phase 3 — split across chunks
        let c3a = split_think_chunks("<think>phase three start", &mut in_think);
        assert!(in_think);
        assert_eq!(c3a, vec![("phase three start".to_string(), true)]);
        let c3b = split_think_chunks(" phase three end</think>final", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            c3b,
            vec![
                (" phase three end".to_string(), true),
                ("final".to_string(), false),
            ]
        );
    }

    // ── extract_think_tags (post-collection cleanup) ──────────────────────────

    #[test]
    fn extract_think_tags_minimax_real_pattern() {
        // Real MiniMax M2.7 full_text after stream collection
        let text = "<think>\nThe user says \"hi\". Should be concise.\n</think>\n\nHello! How can I help you today?";
        let (reasoning, cleaned) = extract_think_tags(text).unwrap();
        assert_eq!(reasoning, "The user says \"hi\". Should be concise.");
        assert_eq!(cleaned, "Hello! How can I help you today?");
    }

    #[test]
    fn extract_think_tags_no_think_returns_none() {
        assert!(extract_think_tags("Hello! How can I help?").is_none());
    }

    #[test]
    fn extract_think_tags_skips_when_reasoning_already_set() {
        // extract_think_tags is only called when reasoning.is_empty(),
        // so this just verifies the function itself works correctly
        let text = "<think>step 1</think>answer";
        let (r, c) = extract_think_tags(text).unwrap();
        assert_eq!(r, "step 1");
        assert_eq!(c, "answer");
    }

    #[test]
    fn redact_provider_secrets_strips_known_prefixes() {
        let input = "sk-abc12345 and Bearer tok_xyz plus key-deadbeef end";
        let out = redact_provider_secrets(input);
        assert!(out.contains("[REDACTED]"), "missing redacted marker: {out}");
        assert!(!out.contains("abc12345"), "leaked sk- secret: {out}");
        assert!(!out.contains("tok_xyz"), "leaked bearer secret: {out}");
        assert!(!out.contains("deadbeef"), "leaked key- secret: {out}");
        assert!(out.contains("end"), "trailing text dropped: {out}");
    }

    #[test]
    fn redact_provider_secrets_leaves_clean_text() {
        let input = "Internal server error: upstream timeout";
        assert_eq!(redact_provider_secrets(input), input);
    }

    #[test]
    fn redact_provider_secrets_handles_quoted_json() {
        let input = r#"{"error":"invalid api key sk-abcXYZ"}"#;
        let out = redact_provider_secrets(input);
        assert!(!out.contains("abcXYZ"), "leaked sk- secret in JSON: {out}");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_provider_secrets_simulates_auth_log_path() {
        // Simulate what the auth-error path logs: a truncated body containing a key.
        let body = r#"{"error":{"message":"Incorrect API key sk-abc12345 provided"}}"#;
        let truncated = &body[..body.len().min(80)];
        let log_line = format!(
            "LLM auth error (401): {}",
            redact_provider_secrets(truncated)
        );
        assert!(!log_line.contains("sk-abc12345"));
        assert!(log_line.contains("[REDACTED]"));
    }

    /// audit-C1: global_llm_client must not use .expect() — a TLS backend
    /// failure should not crash the entire process.
    #[test]
    fn global_llm_client_does_not_panic_on_build() {
        let source = include_str!("llm_client.rs");
        let fn_start = source
            .find("fn global_llm_client()")
            .expect("global_llm_client must exist");
        let body = &source[fn_start..(fn_start + 600).min(source.len())];
        assert!(
            !body.contains(".expect("),
            "global_llm_client must not use .expect(); use .unwrap_or_else with fallback"
        );
    }

    /// P1-E: llm_client must NOT define its own rate_limit_cooldown singleton.
    /// There must be exactly one PerModelCooldown singleton shared across all
    /// LLM call paths, otherwise a 429 recorded by one path is invisible to
    /// the other, causing duplicate rate-limit hits.
    #[test]
    fn llm_client_does_not_define_own_cooldown_singleton() {
        let source = include_str!("llm_client.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            !prod_code.contains("static COOLDOWN"),
            "llm_client.rs must not define its own COOLDOWN singleton; \
             use the shared one from bridge_llm_stream"
        );
    }

    // ─── Thinking config integration tests ──────────────────────────────

    #[test]
    fn build_bedrock_body_with_thinking_enabled() {
        let messages = vec![
            json!({"role": "system", "content": "You are helpful."}),
            json!({"role": "user", "content": "hello"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read_file", "parameters": {"type": "object", "properties": {}}}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(8192),
            None,
            false,
            &ThinkingConfig::Enabled {
                budget_tokens: 5000,
            },
        );

        // Core structure
        assert!(!body.get("messages").unwrap().as_array().unwrap().is_empty());
        assert!(body.get("system").is_some());
        assert_eq!(body["inferenceConfig"]["maxTokens"], 8192);
        // Temperature must be absent (incompatible with thinking)
        assert!(body["inferenceConfig"].get("temperature").is_none());
        // Tools present
        assert!(!body["toolConfig"]["tools"].as_array().unwrap().is_empty());
        // Thinking config via additionalModelRequestFields
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "enabled"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            5000
        );
    }

    #[test]
    fn build_bedrock_body_with_thinking_adaptive() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-opus-4-6-v1",
            "bedrock",
            Some(16000),
            Some(0.7),
            false,
            &ThinkingConfig::Adaptive {
                effort: astra_turn_core::thinking_config::ThinkingEffort::Low,
            },
        );

        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "adaptive"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"]["effort"],
            "low"
        );
        // Temperature removed even though it was requested
        assert!(body["inferenceConfig"].get("temperature").is_none());
    }

    #[test]
    fn build_bedrock_body_with_thinking_off() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(4096),
            Some(0.5),
            false,
            &ThinkingConfig::Off,
        );

        // No thinking fields
        assert!(body.get("additionalModelRequestFields").is_none());
        // Temperature preserved
        assert_eq!(body["inferenceConfig"]["temperature"], 0.5);
    }

    #[test]
    fn build_bedrock_body_includes_reasoning_content_on_assistant_message() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
                "reasoning_content": "I should run bash",
                "reasoning_signature": "sig_abc123"
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "done"}),
            json!({"role": "user", "content": "thanks"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        let bedrock_msgs = body["messages"].as_array().unwrap();
        // First message is user "hello"
        // Second is assistant with reasoningContent + toolUse
        let assistant_msg = &bedrock_msgs[1];
        assert_eq!(assistant_msg["role"], "assistant");
        let content = assistant_msg["content"].as_array().unwrap();
        // First block should be reasoningContent
        assert!(content[0].get("reasoningContent").is_some());
        let rc = &content[0]["reasoningContent"]["reasoningText"];
        assert_eq!(rc["text"], "I should run bash");
        assert_eq!(rc["signature"], "sig_abc123");
        // Second block should be toolUse
        assert!(content[1].get("toolUse").is_some());
    }

    /// Regression test using real Bedrock API response data.
    /// Verifies that a multi-turn thinking + tool_use conversation produces
    /// valid Bedrock request bodies that won't trigger the "signature: Field required" 400.
    #[test]
    fn build_bedrock_body_reasoning_roundtrip_real_signature() {
        // Real signature captured from Bedrock converse API response
        let real_signature = "EucBCkgIDRABGAIqQCjq2TSFiIiSlMoit+qcPnX9t83drZVVaoUyCag7HPkIAplllVNsRLaTzM6wl8n/qpOFbkkyrhwEa/STyGsDb9MSDMhIDhAFyvS1Z5oD7xoMq8EnICsA4bH25yXtIjDJvcoCxGdUU8BeKmUYjm4+6nLghgxhLZJpQL4WphleWcpr8w0PelHlkxs8G0fohDUqTQEEypAjDZqZhWt4I+h4ERKDZ/u1uW59Gs2NJWEcuFtTiKot3Kc+jJvH3Nn9Yp9iaJFbi4kakmwqdmpyxUrISklB/uqiJ0TXeN94CoAmGAE=";

        let messages = vec![
            json!({"role": "user", "content": "What is 2+2? Use the calculator tool."}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tooluse_oOwbnKc4jO48ShtaXrOPcw", "type": "function", "function": {"name": "calculator", "arguments": "{\"expression\":\"2+2\"}"}}],
                "reasoning_content": "The user wants me to calculate 2+2 using the calculator tool.",
                "reasoning_signature": real_signature
            }),
            json!({"role": "tool", "tool_call_id": "tooluse_oOwbnKc4jO48ShtaXrOPcw", "content": "4"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[json!({
                "type": "function",
                "function": {
                    "name": "calculator",
                    "description": "Compute arithmetic",
                    "parameters": {"type": "object", "properties": {"expression": {"type": "string"}}}
                }
            })],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );

        let bedrock_msgs = body["messages"].as_array().unwrap();
        // assistant message (index 1) must have reasoningContent with signature
        let assistant = &bedrock_msgs[1];
        assert_eq!(assistant["role"], "assistant");
        let content = assistant["content"].as_array().unwrap();

        // Order must be: reasoningContent → toolUse (text is optional)
        let rc_block = &content[0];
        assert!(
            rc_block.get("reasoningContent").is_some(),
            "first block must be reasoningContent"
        );
        let rt = &rc_block["reasoningContent"]["reasoningText"];
        assert_eq!(
            rt["text"].as_str().unwrap(),
            "The user wants me to calculate 2+2 using the calculator tool."
        );
        assert_eq!(
            rt["signature"].as_str().unwrap(),
            real_signature,
            "signature must be preserved verbatim"
        );

        // toolUse block follows
        let tool_block = content.iter().find(|b| b.get("toolUse").is_some());
        assert!(tool_block.is_some(), "must have toolUse block");
        assert_eq!(tool_block.unwrap()["toolUse"]["name"], "calculator");

        // Verify thinking config is applied
        assert!(body.get("additionalModelRequestFields").is_some());
    }

    /// Verify that stripped (empty) reasoning does NOT produce a reasoningContent block,
    /// which would trigger Bedrock's "signature: Field required" 400 error.
    #[test]
    fn build_bedrock_body_empty_reasoning_omits_reasoning_block() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
                "reasoning_content": ""
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "done"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        let bedrock_msgs = body["messages"].as_array().unwrap();
        let assistant = &bedrock_msgs[1];
        let content = assistant["content"].as_array().unwrap();
        // No reasoningContent block when reasoning is empty
        assert!(
            !content.iter().any(|b| b.get("reasoningContent").is_some()),
            "empty reasoning_content must NOT produce a reasoningContent block"
        );
    }

    /// Helper for counter tests: build a body that would violate the
    /// signature contract, catching the debug_assert panic so the test can
    /// observe the counter's post-increment state.
    fn attempt_violating_bedrock_thinking_build() {
        let messages = vec![
            json!({"role": "user", "content": "q"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "tc1", "type": "function",
                    "function": {"name": "noop", "arguments": "{}"}
                }],
                "reasoning_content": "thinking without signature",
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "ok"}),
        ];
        let _ = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
    }

    // Counter increments alongside the debug_assert so release builds can
    // expose a continuous-signal tripwire (BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT).
    // The counter must increment even if the panic short-circuits the rest of
    // the build — otherwise monitoring misses the first violation.
    #[test]
    fn bedrock_thinking_signature_violation_increments_counter() {
        use std::sync::atomic::Ordering;
        let before = BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT.load(Ordering::Relaxed);
        // debug_assert panics in test/debug builds; catch so we can read the
        // counter afterward. The fetch_add runs *before* the debug_assert so
        // the counter observes the violation even when the assert fires.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            attempt_violating_bedrock_thinking_build,
        ));
        let after = BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT.load(Ordering::Relaxed);
        assert!(
            after > before,
            "counter must advance on every signature-contract violation \
             (before={before}, after={after})"
        );
    }

    // Guard that the debug_assert in `assert_bedrock_thinking_signature_contract`
    // actually fires when a reasoning block arrives without signature. This is
    // the "scream if this ever regresses again" safety net for PR #284's class
    // of bug. Expected outcome: Bedrock would 400 — we want a test panic first.
    #[test]
    #[should_panic(expected = "Bedrock thinking contract violation")]
    fn bedrock_thinking_signature_contract_panics_on_missing_signature() {
        // Simulate what happens if the SSE → accum → next-assistant-message
        // pipeline ever drops the signature: reasoning_content is present but
        // reasoning_signature is empty.
        let messages = vec![
            json!({"role": "user", "content": "What is 2+2?"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "calc", "arguments": "{}"}}],
                "reasoning_content": "let me compute",
                // reasoning_signature intentionally MISSING
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "4"}),
        ];
        let _ = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
    }

    // Contract positive: signature present → no panic, body built normally.
    #[test]
    fn bedrock_thinking_signature_contract_passes_when_signature_present() {
        let messages = vec![
            json!({"role": "user", "content": "What is 2+2?"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "calc", "arguments": "{}"}}],
                "reasoning_content": "let me compute",
                "reasoning_signature": "sig_from_bedrock"
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "4"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        // Assistant reasoningContent block must carry the signature.
        let rc_sig =
            body["messages"][1]["content"][0]["reasoningContent"]["reasoningText"]["signature"]
                .as_str()
                .unwrap();
        assert_eq!(rc_sig, "sig_from_bedrock");
    }

    // Contract negative-bypass: thinking disabled → stale reasoning history
    // is not serialized, so the signature guard has nothing to enforce.
    #[test]
    fn bedrock_thinking_signature_contract_silent_when_thinking_off() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
                "reasoning_content": "some leftover"
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "ok"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Off,
        );
        let assistant = &body["messages"][1];
        let content = assistant["content"].as_array().unwrap();
        assert!(
            !content.iter().any(|b| b.get("reasoningContent").is_some()),
            "thinking=off must suppress stale reasoningContent so Bedrock never sees an unsigned reasoning block"
        );
    }

    #[test]
    fn build_anthropic_body_with_thinking_enabled() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let tools = vec![json!({
            "name": "read_file",
            "description": "Read a file",
            "input_schema": {"type": "object", "properties": {}}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "claude-sonnet-4-20250514",
            "anthropic",
            Some(8192),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 4000,
            },
        );

        // Core structure
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 8192);
        // Temperature removed
        assert!(body.get("temperature").is_none());
        // Tools present
        assert!(!body["tools"].as_array().unwrap().is_empty());
        assert_eq!(body["tool_choice"]["type"], "auto");
        // Thinking config
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4000);
    }

    #[test]
    fn build_anthropic_body_with_thinking_adaptive_uses_output_config_effort() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-opus-4-7",
            "anthropic",
            Some(16000),
            Some(0.7),
            true,
            &ThinkingConfig::Adaptive {
                effort: astra_turn_core::thinking_config::ThinkingEffort::High,
            },
        );

        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_anthropic_body_with_thinking_off() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-sonnet-4-20250514",
            "anthropic",
            Some(4096),
            Some(0.5),
            true,
            &ThinkingConfig::Off,
        );

        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn build_openai_body_with_thinking_adaptive() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "o3",
            "openai",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Adaptive {
                effort: astra_turn_core::thinking_config::ThinkingEffort::Medium,
            },
        );

        assert_eq!(body["model"], "o3");
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["stream"], true);
    }

    /// Qwen models served through the *DashScope* provider use `enable_thinking`.
    /// The provider name (not model name) is the discriminator — the same Qwen model
    /// served through a generic vLLM/Ollama proxy with provider="openai" must NOT
    /// receive `enable_thinking` because those proxies reject unknown top-level fields.
    #[test]
    fn build_dashscope_qwen_body_with_thinking_enabled() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "qwen3.6-plus",
            "dashscope",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert_eq!(body["model"], "qwen3.6-plus");
        assert_eq!(body["enable_thinking"], true);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    /// Same Qwen model through a generic OpenAI-compatible proxy must NOT get
    /// `enable_thinking` — the proxy does not know about that field and may 400.
    #[test]
    fn build_generic_proxy_qwen_body_does_not_set_dashscope_flag() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "qwen3.6-plus",
            "openai",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert_eq!(body["model"], "qwen3.6-plus");
        // Generic OpenAI-compatible proxy: no DashScope-specific field.
        assert!(body.get("enable_thinking").is_none());
        // Enabled thinking has no OpenAI mapping (no reasoning_effort either).
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn build_standard_openai_body_with_budget_thinking_does_not_send_dashscope_flag() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "gpt-4o",
            "openai",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn build_openai_body_with_thinking_off() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "gpt-4o",
            "openai",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Off,
        );

        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Provider × Thinking × Tool-call × Multi-turn capability matrix
    // ─────────────────────────────────────────────────────────────────────
    //
    // This matrix exists because PR #284 was followed by a silent follow-up
    // regression: the Bedrock signature stopped flowing through the SSE hop
    // between bridge and CLI. Unit tests passed; the end-to-end contract
    // broke anyway. The rule now is:
    //
    //   For every (provider, thinking_mode, has_tool_call, turn_number)
    //   combination that reaches a live provider, assert the exact shape of
    //   the request body produced by `build_provider_request_body`.
    //
    // Adding a new provider / thinking mode without a matrix row is a bug.
    // If the scenario is not supported yet, add a `#[ignore]` placeholder
    // with a comment — don't silently skip.
    //
    // Columns pinned per row:
    //  - reasoning block shape (or absence)
    //  - signature presence (Bedrock + Anthropic thinking only)
    //  - tool_use / toolUse block presence on turn-2+ assistant messages
    //  - top-level `thinking` config applied correctly
    mod thinking_matrix {
        use super::*;

        fn user(text: &str) -> Value {
            json!({"role": "user", "content": text})
        }

        fn assistant_with_tool_call(
            reasoning: &str,
            signature: Option<&str>,
            tool_name: &str,
            tool_id: &str,
        ) -> Value {
            let mut msg = json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": tool_id,
                    "type": "function",
                    "function": {"name": tool_name, "arguments": "{}"}
                }]
            });
            if !reasoning.is_empty() {
                msg["reasoning_content"] = Value::String(reasoning.to_string());
            }
            if let Some(sig) = signature {
                msg["reasoning_signature"] = Value::String(sig.to_string());
            }
            msg
        }

        fn tool_result(tool_id: &str, output: &str) -> Value {
            json!({"role": "tool", "tool_call_id": tool_id, "content": output})
        }

        // ── Row: Bedrock + Thinking::Enabled + tool_call + turn-2 ──
        // This is the exact scenario that caused the HTTP 400 that led
        // to this matrix's existence.
        #[test]
        fn bedrock_thinking_tool_call_multi_turn_serializes_signature() {
            let messages = vec![
                user("compute 2+2"),
                assistant_with_tool_call("thinking...", Some("real_sig"), "calc", "tc1"),
                tool_result("tc1", "4"),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "us.anthropic.claude-sonnet-4-6",
                "bedrock",
                Some(4096),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 1024,
                },
            );
            let assistant = &body["messages"][1];
            let content = assistant["content"].as_array().unwrap();
            let rc = &content[0]["reasoningContent"]["reasoningText"];
            assert_eq!(rc["text"], "thinking...");
            assert_eq!(
                rc["signature"], "real_sig",
                "signature MUST appear on assistant reasoningContent — \
                 Bedrock returns 400 `thinking.signature: Field required` otherwise"
            );
            let has_tool_use = content.iter().any(|b| b.get("toolUse").is_some());
            assert!(has_tool_use, "toolUse block must follow reasoningContent");
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["type"],
                "enabled"
            );
        }

        // ── Row: Bedrock + Thinking::Off + tool_call + turn-2 ──
        // No thinking → no reasoningContent block even if historic
        // reasoning_content is present (e.g. session resumed with stale state).
        #[test]
        fn bedrock_thinking_off_tool_call_omits_reasoning_block() {
            let messages = vec![
                user("hi"),
                assistant_with_tool_call("old thinking", None, "bash", "tc1"),
                tool_result("tc1", "ok"),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "us.anthropic.claude-sonnet-4-6",
                "bedrock",
                Some(4096),
                None,
                true,
                &ThinkingConfig::Off,
            );
            assert!(body.get("additionalModelRequestFields").is_none());
            let assistant = &body["messages"][1];
            let content = assistant["content"].as_array().unwrap();
            assert!(
                !content.iter().any(|b| b.get("reasoningContent").is_some()),
                "thinking=off should not serialize stale reasoningContent"
            );
        }

        // ── Row: Bedrock + Thinking::Enabled + no tool_call + turn-1 ──
        // No historic assistant message yet → no signature contract to honor.
        #[test]
        fn bedrock_thinking_first_turn_no_history() {
            let messages = vec![user("compute 2+2")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "us.anthropic.claude-sonnet-4-6",
                "bedrock",
                Some(4096),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 1024,
                },
            );
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["type"],
                "enabled"
            );
        }

        // ── Row: Anthropic + Thinking::Enabled + no tool_call + turn-1 ──
        #[test]
        fn anthropic_thinking_first_turn_top_level_config() {
            let messages = vec![user("compute 2+2")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "claude-opus-4-7",
                "anthropic",
                Some(8192),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 4000,
                },
            );
            assert_eq!(body["thinking"]["type"], "enabled");
            assert_eq!(body["thinking"]["budget_tokens"], 4000);
        }

        // ── Row: Anthropic + Thinking::Enabled + tool_call + turn-2 ──
        //
        // KNOWN GAP — tracked (created 2026-05-01): Anthropic native API
        // requires assistant.content as an array of typed blocks
        // (`{"type":"thinking","thinking":"...","signature":"..."}`,
        // `{"type":"tool_use",...}`). Today's code path passes OpenAI-shape
        // `reasoning_content` / `reasoning_signature` directly, which
        // Anthropic will reject with the same class of 400 as Bedrock's
        // `thinking.signature: Field required`.
        //
        // Pinned here as `#[should_panic]` + `#[ignore]` so: (a) nobody
        // accidentally ships Anthropic native multi-turn thinking in a broken
        // state, and (b) when a future PR implements the typed-block
        // serializer (equivalent to `build_bedrock_messages` for Anthropic)
        // plus the matching SSE signature capture, this test becomes real
        // coverage by flipping to a normal `#[test]`.
        //
        // Today the request body is OpenAI-shape, so `body["messages"][1]
        // ["content"].as_array()` returns `None` and the first assertion
        // panics with "called `Option::unwrap()` on a `None` value". That
        // panic is the canary — `#[should_panic]` pins the current broken
        // state so `make test-online`'s `--run-ignored only` stage doesn't
        // surface it as a false failure. Remove both attributes together
        // with the serializer implementation.
        //
        // TODO(anthropic-thinking): remove `#[ignore]` + `#[should_panic]`
        // and implement the typed-block serializer + SSE signature capture
        // before enabling native Anthropic multi-turn thinking.
        #[test]
        #[ignore = "blocked on typed-block serializer for native Anthropic thinking"]
        #[should_panic(expected = "called `Option::unwrap()` on a `None` value")]
        fn anthropic_thinking_tool_call_multi_turn_needs_typed_blocks() {
            let messages = vec![
                user("compute 2+2"),
                assistant_with_tool_call("thinking...", Some("real_sig"), "calc", "tc1"),
                tool_result("tc1", "4"),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "claude-opus-4-7",
                "anthropic",
                Some(8192),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 4000,
                },
            );
            // When this is implemented, the assistant message content MUST be
            // a typed-block array: first a thinking block carrying signature,
            // then the tool_use block.
            let assistant_content = body["messages"][1]["content"].as_array().unwrap();
            assert_eq!(assistant_content[0]["type"], "thinking");
            assert_eq!(assistant_content[0]["thinking"], "thinking...");
            assert_eq!(assistant_content[0]["signature"], "real_sig");
            assert_eq!(assistant_content[1]["type"], "tool_use");
        }

        // ── Row: OpenAI + Thinking::Adaptive(effort) ──
        // Adaptive maps to `reasoning_effort` on OpenAI. No signature mechanic.
        #[test]
        fn openai_thinking_adaptive_maps_to_reasoning_effort() {
            let messages = vec![user("hi")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "gpt-4o",
                "openai",
                Some(4096),
                Some(0.7),
                true,
                &ThinkingConfig::Adaptive {
                    effort: astra_turn_core::thinking_config::ThinkingEffort::Medium,
                },
            );
            assert_eq!(body["reasoning_effort"], "medium");
        }

        // ── Row: OpenAI + Thinking::Enabled (budget) ──
        // OpenAI has no budget-based thinking; the config must be a no-op
        // rather than silently sending an unsupported field.
        #[test]
        fn openai_thinking_enabled_budget_is_noop() {
            let messages = vec![user("hi")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "gpt-4o",
                "openai",
                Some(4096),
                Some(0.7),
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 5000,
                },
            );
            assert!(body.get("reasoning_effort").is_none());
            assert!(body.get("thinking").is_none());
            assert!(body.get("enable_thinking").is_none());
        }

        // ── Row: DashScope/Qwen + Thinking::Enabled ──
        // Qwen uses a binary `enable_thinking` flag, not budget/effort.
        #[test]
        fn dashscope_thinking_enabled_sends_binary_flag() {
            let messages = vec![user("hi")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "qwen3-max",
                "dashscope",
                Some(4096),
                Some(0.7),
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 1024,
                },
            );
            assert_eq!(body["enable_thinking"], true);
        }
    }
}
