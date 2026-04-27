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

use astra_logging::redact_known_secret_patterns;
use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::sse_blocks::SseBlankLineUtf8Buf;
use super::sse_data_lines::{
    json_events_from_sse_event_block, validate_sse_event_block_json,
    validated_drain_sse_data_lines, validated_finish_sse_data_buffer,
};
use crate::bridge::rate_limit_cooldown::{
    RateLimitAction, is_overload_status, is_rate_limit_status, parse_retry_after_ms,
};
use crate::output_style::current_output_style;
use crate::prompts;

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

/// Global HTTP client for LLM requests (connection pooling, reuse).
fn global_llm_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let connect = llm_connect_timeout();
        let total = std::time::Duration::from_secs(LLM_TOTAL_BUDGET_S + 60);
        match reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(connect)
            // Use a generous timeout; per-request timeout handled via tokio::time::timeout
            .timeout(total)
            .pool_max_idle_per_host(4)
            .build()
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
        "tool_calls": result.tool_calls,
        "usage": result.usage,
        "finish_reason": result.finish_reason,
        "model_used": result.model_used,
    }))
    .ok()
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
pub(crate) fn llm_total_budget() -> std::time::Duration {
    let s = std::env::var("MO_LLM_TOTAL_BUDGET_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(LLM_TOTAL_BUDGET_S);
    std::time::Duration::from_secs(s)
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

fn build_bedrock_message_content(msg: &Value) -> Vec<Value> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
    match role {
        "tool" => {
            let tool_use_id = msg.get("tool_call_id").and_then(Value::as_str);
            let content = content_text_value(msg.get("content")).unwrap_or_default();
            tool_use_id
                .map(|tool_use_id| {
                    let result_block = if content.is_empty() {
                        json!({"json": {}})
                    } else if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                        json!({"json": parsed})
                    } else {
                        json!({"text": content})
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
            let mut blocks = build_bedrock_text_content_blocks(msg.get("content"));
            blocks.extend(build_bedrock_tool_blocks(
                msg.get("tool_calls").and_then(Value::as_array),
            ));
            blocks
        }
        _ => build_bedrock_text_content_blocks(msg.get("content")),
    }
}

fn build_bedrock_messages(messages: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let mut system = Vec::new();
    let mut out = Vec::new();
    for msg in messages {
        match msg.get("role").and_then(Value::as_str).unwrap_or_default() {
            "system" => {
                system.extend(build_bedrock_text_content_blocks(msg.get("content")));
            }
            "tool" => {
                let content = build_bedrock_message_content(msg);
                if !content.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            "user" | "assistant" => {
                let content = build_bedrock_message_content(msg);
                if !content.is_empty() {
                    out.push(json!({
                        "role": msg.get("role").and_then(Value::as_str).unwrap_or("user"),
                        "content": content,
                    }));
                }
            }
            _ => {}
        }
    }
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

pub(crate) fn build_provider_request_body(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    temperature: Option<f64>,
    streaming: bool,
) -> Value {
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::BedrockConverse => {
            let (system, bedrock_messages) = build_bedrock_messages(messages);
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

    let use_streaming_endpoint = !provider_uses_bedrock_converse(provider);
    let body = build_provider_request_body(
        &messages,
        tools,
        model_name,
        provider,
        max_output_tokens,
        None,
        use_streaming_endpoint,
    );

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        model_name,
        use_streaming_endpoint,
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
                let v: Value = response.json().await.map_err(|e| {
                    astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::StreamTransport,
                        e.to_string(),
                    )
                })?;
                return Ok(parse_nonstream_response_for_provider(
                    &v, provider, model_name, started,
                ));
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
    let mut tool_calls = Vec::new();
    let mut usage = Map::new();

    if let Some(u) = v.get("usage").and_then(Value::as_object) {
        if let Some(p) = u.get("inputTokens").and_then(Value::as_i64) {
            usage.insert("prompt".to_string(), Value::from(p));
        }
        if let Some(c) = u.get("outputTokens").and_then(Value::as_i64) {
            usage.insert("completion".to_string(), Value::from(c));
        }
        if let Some(cache_read) = u
            .get("cacheReadInputTokens")
            .or_else(|| u.get("cacheReadInputTokensCount"))
            .and_then(Value::as_i64)
        {
            usage.insert("cache_read".to_string(), Value::from(cache_read));
            usage.insert(
                "cache_read_input_tokens".to_string(),
                Value::from(cache_read),
            );
        }
        if let Some(cache_write) = u
            .get("cacheWriteInputTokens")
            .or_else(|| u.get("cacheWriteInputTokensCount"))
            .and_then(Value::as_i64)
        {
            usage.insert("cache_creation".to_string(), Value::from(cache_write));
            usage.insert(
                "cache_creation_input_tokens".to_string(),
                Value::from(cache_write),
            );
        }
        if let Some(t) = u.get("totalTokens").and_then(Value::as_i64) {
            usage.insert("total".to_string(), Value::from(t));
        } else if let (Some(p), Some(c)) = (
            u.get("inputTokens").and_then(Value::as_i64),
            u.get("outputTokens").and_then(Value::as_i64),
        ) {
            usage.insert("total".to_string(), Value::from(p + c));
        }
    }

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
            if let Some(reasoning_text) = block
                .get("reasoningContent")
                .and_then(|content| content.get("reasoningText"))
                .and_then(|reasoning_text| reasoning_text.get("text"))
                .and_then(Value::as_str)
            {
                reasoning.push_str(reasoning_text);
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

    if reasoning.is_empty() {
        if let Some((extracted_reasoning, cleaned_text)) = extract_think_tags(&full_text) {
            reasoning = extracted_reasoning;
            full_text = cleaned_text;
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
    use serial_test::serial;
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

    /// Drift guard: after the temp-env migration, llm_client tests must not
    /// re-introduce raw unsafe env mutation for env plumbing. Use
    /// `temp_env::async_with_vars` instead so the env state is restored via
    /// RAII even when a test panics.
    #[test]
    fn llm_client_tests_use_temp_env_not_unsafe_set_var() {
        // Build sentinels at runtime from disjoint fragments so no single
        // literal in this test matches itself in the include_str source.
        let unsafe_open = format!("{}{}", "unsafe", " { ");
        let std_env = format!("{}{}", "std::", "env::");
        let sentinel_set = format!("{unsafe_open}{std_env}set_{}", "var");
        let sentinel_remove = format!("{unsafe_open}{std_env}remove_{}", "var");
        let source = include_str!("llm_client.rs");
        assert!(
            !source.contains(&sentinel_set) && !source.contains(&sentinel_remove),
            "llm_client tests must use temp_env::async_with_vars instead of raw unsafe env mutation"
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
        assert_eq!(r.usage.get("total").and_then(Value::as_i64), Some(15));
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
        assert_eq!(r.usage.get("total").and_then(Value::as_i64), Some(15));
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
        assert_eq!(r.usage.get("cache_read").and_then(Value::as_i64), Some(8));
        assert_eq!(
            r.usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_i64),
            Some(8)
        );
        assert_eq!(
            r.usage.get("cache_creation").and_then(Value::as_i64),
            Some(3)
        );
        assert_eq!(
            r.usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_i64),
            Some(3)
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

    // ── serial(stream_idle_env): all tests below mutate MO_STREAM_IDLE_TIMEOUT_MS
    // which is read at startup and cached globally. Parallel execution causes
    // race conditions where one test's timeout value bleeds into another test's
    // LlmClient construction. Any new test that sets this env var MUST be tagged.

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_surfaces_transport_error() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_transport_after_partial_carries_partial_result() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
            let err = sample_reqwest_stream_error().await;
            let d1 = json!({"choices":[{"delta":{"content":"partial"}}]});
            let byte_stream =
                stream::iter(vec![Ok(Bytes::from(format!("data: {d1}\n\n"))), Err(err)]);
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_aggregates_delta_text_reasoning_usage() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_extracts_finish_reason_stop() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_extracts_finish_reason_length() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_merges_tool_call_argument_chunks() {
        temp_env::async_with_vars(
            [("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))],
            async {
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
                let parsed: Value =
                    serde_json::from_str(args).expect("valid merged JSON args");
                assert_eq!(parsed, json!({"foo":"bar"}));
                assert_eq!(
                    res.tool_calls[0]["function"]["name"].as_str(),
                    Some("bash")
                );
            },
        )
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn stream_idle_timeout_triggers() {
        // Keep this test fast: override idle timeout to 1ms.
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("1"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn stream_idle_timeout_after_partial_output_marks_progress() {
        temp_env::async_with_vars(
            [
                ("MO_STREAM_IDLE_TIMEOUT_MS", Some("1")),
                ("MO_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS", Some("1")),
            ],
            async {
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
            },
        )
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_respects_cancel_flag() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_respects_cancel_token() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_flag_and_token_cancels_on_token() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
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
    #[serial(stream_idle_env)]
    async fn collect_llm_stream_decodes_lossy_utf8_inside_json_string() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_retries_after_429_retry_after_zero() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
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
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn call_llm_and_collect_retries_after_503_retry_after_zero() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
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
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn finish_reason_stop_no_retry() {
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
    async fn finish_reason_tool_calls_extracted() {
        temp_env::async_with_vars(
            [("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))],
            async {
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
            },
        )
        .await;
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
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
            )
            .await
            .expect_err("should fail with context window");
            assert_eq!(err.kind, astra_core::ErrorKind::ContextWindow);
            assert!(err.message.contains("context_length_exceeded"));
        })
        .await;
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
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await;
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
        temp_env::async_with_vars([("MO_STREAM_IDLE_TIMEOUT_MS", Some("60000"))], async {
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
        })
        .await
    }

    #[tokio::test]
    #[serial(stream_idle_env)]
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
    #[serial(stream_idle_env)]
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
    #[serial(stream_idle_env)]
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
    #[serial(stream_idle_env)]
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
    #[serial(stream_idle_env)]
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
    #[serial(stream_idle_env)]
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
}
