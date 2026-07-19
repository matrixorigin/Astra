//! LLM streaming call with retry logic, rate-limit cooldown, and idle-timeout fallback.
//!
//! This module encapsulates the HTTP streaming call to the LLM provider, including:
//! - SSE chunk parsing and forwarding (text_delta, reasoning_delta, tool_call_start, usage)
//! - Idle-timeout detection with automatic non-stream fallback
//! - Retry with exponential backoff for transient errors (429, 5xx, network)
//! - Per-model rate-limit cooldown tracking
//! - Degraded tool-call recovery from XML-like text content

use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};

use async_stream::stream;
use axum::body::Bytes;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::turn::bridge::sse_helpers::render_sse;
use crate::turn::llm::client::{
    LLM_MAX_RETRIES, LlmCall, LlmCancel, LlmExecutionRoute, apply_llm_header_overrides,
    apply_provider_auth, build_provider_request_body_with_overrides, llm_request_url,
    llm_retry_base_ms, parse_openai_sse_json_stream, provider_uses_anthropic_messages,
    provider_uses_bedrock_converse, sleep_ms_or_llm_cancel, split_think_chunks,
};
use astra_turn_core::bridge_rate_limit_cooldown::{
    PerModelCooldown, RateLimitAction, is_overload_status, is_rate_limit_status,
    parse_retry_after_ms,
};
use astra_turn_core::edge_ledger::ensure_tool_call_ids;
use futures_util::StreamExt;
use std::sync::OnceLock;

#[cfg(test)]
thread_local! {
    static TEST_BRIDGE_RETRY_BACKOFF_MS: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
}

fn bridge_retry_backoff_ms(attempt: u32) -> u64 {
    #[cfg(test)]
    if let Some(ms) = TEST_BRIDGE_RETRY_BACKOFF_MS.with(|c| *c.borrow()) {
        return ms;
    }
    llm_retry_base_ms() * (1 << (attempt - 1))
}

/// Returns `true` if `name` looks like a valid tool function name.
///
/// Rejects names that are:
/// - empty
/// - contain `<` or `>` (XML artifact)
/// - contain whitespace
pub(crate) fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('<')
        && !name.contains('>')
        && !name.chars().any(char::is_whitespace)
}

/// Build a `tool_call_start` SSE event from a streaming tool call accumulator entry.
pub(crate) fn tool_call_start_event(tool_call: &mut Map<String, Value>) -> Option<Value> {
    let function = tool_call.get("function").and_then(Value::as_object)?;
    let tool = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| is_valid_tool_name(name))?
        .to_string();
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .filter(|args| !args.is_empty())
        .map(std::string::ToString::to_string);
    let call_id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            let id = Uuid::now_v7().to_string();
            tool_call.insert("id".to_string(), Value::String(id.clone()));
            id
        });

    let mut event = json!({
        "type": "tool_call_start",
        "tool": tool,
        "call_id": call_id,
    });
    if let Some(arguments) = arguments
        && let Some(obj) = event.as_object_mut()
    {
        obj.insert("arguments".to_string(), Value::String(arguments));
    }
    Some(event)
}

fn streamed_suffix(already_streamed: &str, recovered_full: &str) -> Option<String> {
    if recovered_full.is_empty() {
        None
    } else if already_streamed.is_empty() {
        Some(recovered_full.to_string())
    } else {
        recovered_full
            .strip_prefix(already_streamed)
            .filter(|suffix| !suffix.is_empty())
            .map(ToString::to_string)
    }
}

fn tool_call_start_already_emitted(
    existing_tool_calls: &std::collections::HashMap<usize, Map<String, Value>>,
    index: usize,
) -> bool {
    existing_tool_calls
        .get(&index)
        .and_then(|tc| tc.get("function"))
        .and_then(Value::as_object)
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
}

/// Per-model rate-limit cooldown tracker (global singleton).
pub(crate) fn rate_limit_cooldown() -> &'static PerModelCooldown {
    static COOLDOWN: OnceLock<PerModelCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(PerModelCooldown::new)
}

fn bridge_llm_cancel(cc: &Option<Arc<CancellationToken>>) -> LlmCancel<'_> {
    match cc.as_ref() {
        Some(t) => LlmCancel::Token(t.as_ref()),
        None => LlmCancel::None,
    }
}

/// Build the canonical `usage` SSE event JSON from an `LlmCallResult::usage`
/// map (which uses our canonical keys). Returns `None` when the map is empty.
fn usage_sse_event_from_result_map(m: &Map<String, Value>) -> Option<Value> {
    if m.is_empty() {
        return None;
    }
    let u = crate::turn::token_usage::TokenUsage::from_partial_json_map(m);
    Some(json!({
        "type": "usage",
        "input_tokens": u.input_tokens,
        "cached_input_tokens": u.cached_input_tokens,
        "cache_creation_tokens": u.cache_creation_tokens,
        "output_tokens": u.output_tokens,
        "total_tokens": u.total_tokens(),
    }))
}

fn turn_timeout_s() -> f64 {
    astra_core::RuntimeLimits::global().turn_timeout_s
}

/// What the retry-loop should do with the current non-2xx response.
///
/// Returned by [`classify_non_success_and_record_cooldown`] so both the
/// Bedrock and OpenAI streaming paths share exactly one piece of
/// error-classification + cooldown-bookkeeping code.
enum RetryDecision {
    /// Retryable transient error. If `Some(delay_ms)`, the caller must
    /// sleep that long before attempting again. Applies to 429, 529/503
    /// (overload), and generic 5xx.
    Retry { delay_ms: Option<u64> },
    /// Terminal error — caller must return `Err(last_err)`.
    Terminal,
}

/// Classify a non-2xx response and update the rate-limit cooldown tracker.
///
/// Maps:
/// - `429`           → `record_429` + Retry (respecting cooldown's advised delay)
/// - `503`/`529`     → `record_529` + Retry (overload path)
/// - other `5xx`     → Retry without cooldown bookkeeping
/// - other `4xx`     → Terminal
///
/// `model_key` is the cooldown scope (normally `model_name`). `log_tag` is
/// a short prefix for warn messages so logs reveal which HTTP path triggered.
fn classify_non_success_and_record_cooldown(
    status: u16,
    retry_after_ms: Option<u64>,
    cooldown: &PerModelCooldown,
    model_key: &str,
    has_fallback: bool,
    log_tag: &str,
) -> RetryDecision {
    if is_rate_limit_status(status) {
        let action = cooldown.with(model_key, |c| c.record_429(retry_after_ms, has_fallback));
        astra_core::agent_warn!(
            "llm",
            "{log_tag} rate limit (429) on {model_key}: action={action:?}"
        );
        let delay_ms = match action {
            RateLimitAction::WaitAndRetry { delay_ms } => Some(delay_ms),
            _ => None,
        };
        return RetryDecision::Retry { delay_ms };
    }
    if is_overload_status(status) {
        let action = cooldown.with(model_key, |c| c.record_529(retry_after_ms, has_fallback));
        astra_core::agent_warn!(
            "llm",
            "{log_tag} overload ({status}) on {model_key}: action={action:?}"
        );
        let delay_ms = match action {
            RateLimitAction::WaitAndRetry { delay_ms } => Some(delay_ms),
            _ => None,
        };
        return RetryDecision::Retry { delay_ms };
    }
    if status >= 500 {
        return RetryDecision::Retry { delay_ms: None };
    }
    RetryDecision::Terminal
}

/// Bedrock Converse streaming POST with the same retry + cooldown discipline
/// the OpenAI branch of [`call_llm_stream`] uses:
///
/// - HTTP 429 → `record_429` on the cooldown tracker + retry with backoff.
/// - HTTP 5xx → record via `record_529` for overload (529 / 503) or plain
///   retry otherwise, bounded by `LLM_MAX_RETRIES`.
/// - Network errors → retry with exponential backoff.
/// - On the first 2xx response, hand the body to
///   [`bedrock_transport::bedrock_stream_response_bytes`] and return the
///   canonical internal SSE stream.
async fn bedrock_stream_with_retry(
    client: &reqwest::Client,
    call: LlmCall<'_>,
    client_cancel: Option<Arc<CancellationToken>>,
) -> Result<Pin<Box<dyn futures_util::Stream<Item = Bytes> + Send + 'static>>, String> {
    let LlmCall {
        purpose,
        messages,
        tools,
        route,
        max_output_tokens,
        temperature,
        has_fallback,
        thinking,
    } = call;
    let LlmExecutionRoute {
        model_name,
        wire_model_name,
        api_key,
        base_url,
        provider,
        header_overrides,
        request_body_overrides,
        completions_url_override,
        request_timeout,
    } = route;
    let cooldown = rate_limit_cooldown();
    let model_key = model_name;
    let upstream_name = wire_model_name.unwrap_or(model_name);

    let body = build_provider_request_body_with_overrides(
        messages,
        tools,
        upstream_name,
        provider,
        max_output_tokens,
        temperature,
        true,
        thinking,
        request_body_overrides,
    );
    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        upstream_name,
        true,
    );

    let total_budget = crate::turn::llm::client::llm_total_budget();
    let started = std::time::Instant::now();
    let mut last_err = String::new();

    for attempt in 0..=LLM_MAX_RETRIES {
        if attempt > 0 && started.elapsed() > total_budget {
            return Err(format!(
                "bedrock stream total budget exhausted ({:.0}s): {last_err}",
                total_budget.as_secs_f64()
            ));
        }
        if attempt > 0 {
            let delay = bridge_retry_backoff_ms(attempt);
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel))
                .await
                .map_err(|e| e.to_string())?;
        }

        let mut req = client.post(&url).header("content-type", "application/json");
        req = apply_provider_auth(req, provider, api_key, header_overrides);
        req = apply_llm_header_overrides(req, header_overrides);
        if let Some(timeout) = request_timeout {
            req = req.timeout(timeout);
        }

        if std::env::var("ASTRA_PIPELINE_DUMP_SYSTEM_PROMPT").is_ok() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let dump_path = std::env::temp_dir().join(format!("astra-bedrock-body-{ts}.json"));
            let dump_content =
                serde_json::to_string_pretty(&body).unwrap_or_else(|_| "serialize error".into());
            let _ = std::fs::write(&dump_path, &dump_content);
        }

        let request_started = std::time::Instant::now();
        tracing::debug!(
            target: "astra_runtime::bridge_llm_stream",
            purpose = purpose.as_str(),
            provider,
            model_name,
            attempt,
            "sending Bedrock inference request"
        );
        let response = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("bedrock converse-stream send failed: {e}");
                astra_core::agent_warn!(
                    "llm",
                    "bedrock network retry: attempt={attempt} model={model_name} err={e}"
                );
                continue;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            cooldown.with(model_key, |c| c.record_success());
            let idle = crate::turn::llm::client::stream_idle_timeout();
            return Ok(Box::pin(
                crate::turn::bedrock::transport::bedrock_stream_response_bytes(
                    response,
                    model_name.to_string(),
                    request_started,
                    client_cancel,
                    idle,
                ),
            ));
        }

        let retry_after_ms = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);
        let text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));
        last_err = format!("bedrock converse-stream HTTP {status}: {text}");

        match classify_non_success_and_record_cooldown(
            status,
            retry_after_ms,
            cooldown,
            model_key,
            has_fallback,
            "bedrock",
        ) {
            RetryDecision::Retry { delay_ms } => {
                if let Some(d) = delay_ms {
                    sleep_ms_or_llm_cancel(d, bridge_llm_cancel(&client_cancel))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                continue;
            }
            RetryDecision::Terminal => return Err(last_err),
        }
    }

    Err(format!(
        "bedrock stream exhausted {LLM_MAX_RETRIES} retries: {last_err}"
    ))
}

/// Anthropic Messages streaming parser (native Anthropic API + anthropic-
/// compatible endpoints like DeepSeek's `/anthropic`).
///
/// The Anthropic streaming protocol is distinct from OpenAI's
/// `choices[0].delta.content` shape. Events arrive as:
///   - `message_start`         — initial usage (input_tokens, cached_input_tokens, ...)
///   - `content_block_start`   — per-block metadata (text / tool_use / thinking)
///   - `content_block_delta`   — incremental `text_delta` / `thinking_delta` / `input_json_delta`
///   - `content_block_stop`
///   - `message_delta`         — final usage patch (output_tokens)
///   - `message_stop`
///
/// Pre-fix, `call_llm_stream` handed anthropic bytes to the OpenAI parser,
/// which expected `choices` + `delta.content` and silently dropped every
/// anthropic event. Result: `turn_complete` with empty `full_text` and
/// empty `usage` — captured in `llm_capture_*.json` as
/// `response: {finish_reason:"stop", full_text:"", usage:{}}`.
///
/// This function translates anthropic events into the SAME canonical SSE
/// types the OpenAI branch emits (`text_delta`, `reasoning_delta`,
/// `tool_call_start`, `usage`), so downstream consumers
/// (`apply_forward_llm_sse_event` in `bridge_sse_helpers.rs`) work
/// unchanged.
async fn anthropic_stream_with_retry(
    client: &reqwest::Client,
    call: LlmCall<'_>,
    client_cancel: Option<Arc<CancellationToken>>,
) -> Result<Pin<Box<dyn futures_util::Stream<Item = Bytes> + Send + 'static>>, String> {
    let LlmCall {
        purpose,
        messages,
        tools,
        route,
        max_output_tokens,
        temperature,
        has_fallback,
        thinking,
    } = call;
    let LlmExecutionRoute {
        model_name,
        wire_model_name,
        api_key,
        base_url,
        provider,
        header_overrides,
        request_body_overrides,
        completions_url_override,
        request_timeout,
    } = route;
    let cooldown = rate_limit_cooldown();
    let model_key = model_name;
    let upstream_name = wire_model_name.unwrap_or(model_name);

    let body = build_provider_request_body_with_overrides(
        messages,
        tools,
        upstream_name,
        provider,
        max_output_tokens,
        temperature,
        true,
        thinking,
        request_body_overrides,
    );
    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        upstream_name,
        true,
    );

    let total_budget = crate::turn::llm::client::llm_total_budget();
    let started = std::time::Instant::now();
    let mut last_err = String::new();

    for attempt in 0..=LLM_MAX_RETRIES {
        if attempt > 0 && started.elapsed() > total_budget {
            return Err(format!(
                "anthropic stream total budget exhausted ({:.0}s): {last_err}",
                total_budget.as_secs_f64()
            ));
        }
        if attempt > 0 {
            let delay = bridge_retry_backoff_ms(attempt);
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel))
                .await
                .map_err(|e| e.to_string())?;
        }

        let mut req = client.post(&url).header("content-type", "application/json");
        req = apply_provider_auth(req, provider, api_key, header_overrides);
        req = apply_llm_header_overrides(req, header_overrides);
        if let Some(timeout) = request_timeout {
            req = req.timeout(timeout);
        }

        tracing::debug!(
            target: "astra_runtime::bridge_llm_stream",
            purpose = purpose.as_str(),
            provider,
            model_name,
            attempt,
            "sending Anthropic inference request"
        );
        let response = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("anthropic stream send failed: {e}");
                continue;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            cooldown.with(model_key, |c| c.record_success());
            let byte_stream = response.bytes_stream();
            let idle_pre = crate::turn::llm::client::stream_idle_timeout();
            let idle_post = crate::turn::llm::client::stream_idle_timeout_after_progress();
            let cc = client_cancel.clone();
            let client_for_fallback = client.clone();
            let messages_for_fallback: Arc<[Value]> = Arc::from(messages);
            let tools_for_fallback: Arc<[Value]> = Arc::from(tools);
            let model_for_fallback = model_name.to_string();
            let wire_model_for_fallback = wire_model_name.map(str::to_string);
            let api_key_for_fallback = api_key.to_string();
            let base_url_for_fallback = base_url.to_string();
            let provider_for_fallback = provider.to_string();
            let header_overrides_for_fallback = header_overrides.cloned();
            let max_out_for_fallback = max_output_tokens;
            let request_body_overrides_for_fallback = request_body_overrides.cloned();
            let completions_url_override_for_fallback =
                completions_url_override.map(str::to_string);
            let request_timeout_for_fallback = request_timeout;
            let thinking_for_fallback = thinking.clone();
            let out = stream! {
                let sse = parse_openai_sse_json_stream(byte_stream);
                tokio::pin!(sse);
                // Accumulate state for the final `_inprocess_summary` event.
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut reasoning_signature = String::new();
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();
                let mut made_progress = false;
                let mut had_terminal_error = false;

                loop {
                    let idle = if made_progress { idle_post } else { idle_pre };
                    tokio::select! {
                        biased;
                        _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                            astra_core::agent_warn!(
                                "llm",
                                "anthropic SSE cancelled (client disconnect)"
                            );
                            break;
                        }
                        tick = tokio::time::timeout(idle, sse.next()) => {
                            let Ok(next) = tick else {
                                astra_core::agent_warn!(
                                    "llm",
                                    "anthropic SSE idle after {}ms (made_progress={})",
                                    idle.as_millis(),
                                    made_progress,
                                );
                                if made_progress {
                                    let streamed_text = full_text.clone();
                                    let streamed_reasoning = reasoning.clone();
                                    let existing_tool_calls = tool_calls_map.clone();
                                    let fb_timeout = crate::turn::llm::client::llm_nonstream_timeout();
                                    match crate::turn::llm::client::call_llm_nonstream(
                                        &client_for_fallback,
                                        LlmCall {
                                            purpose,
                                            messages: &messages_for_fallback,
                                            tools: &tools_for_fallback,
                                            route: LlmExecutionRoute {
                                                model_name: &model_for_fallback,
                                                wire_model_name: wire_model_for_fallback.as_deref(),
                                                api_key: &api_key_for_fallback,
                                                base_url: &base_url_for_fallback,
                                                provider: &provider_for_fallback,
                                                header_overrides: header_overrides_for_fallback.as_ref(),
                                                request_body_overrides: request_body_overrides_for_fallback.as_ref(),
                                                completions_url_override: completions_url_override_for_fallback.as_deref(),
                                                request_timeout: request_timeout_for_fallback,
                                            },
                                            max_output_tokens: max_out_for_fallback,
                                            temperature,
                                            has_fallback,
                                            thinking: &thinking_for_fallback,
                                        },
                                        fb_timeout,
                                    )
                                    .await
                                    {
                                        Ok(mut result) => {
                                            ensure_tool_call_ids(&mut result.tool_calls);
                                            full_text = result.full_text.clone();
                                            reasoning = result.reasoning.clone();
                                            if !result.reasoning_signature.is_empty() {
                                                reasoning_signature = result.reasoning_signature.clone();
                                            }
                                            usage = result.usage.clone();
                                            tool_calls_map.clear();
                                            for (i, tc) in result.tool_calls.iter().enumerate() {
                                                if let Value::Object(m) = tc {
                                                    tool_calls_map.insert(i, m.clone());
                                                }
                                            }
                                            if result.tool_calls.is_empty()
                                                && let Some(suffix) =
                                                    streamed_suffix(&streamed_text, &result.full_text)
                                            {
                                                yield render_sse(&json!({"type":"text_delta","content": suffix}));
                                            }
                                            if let Some(suffix) =
                                                streamed_suffix(&streamed_reasoning, &result.reasoning)
                                            {
                                                yield render_sse(&json!({"type":"reasoning_delta","content": suffix}));
                                            }
                                            for (i, tc) in result.tool_calls.iter().enumerate() {
                                                if tool_call_start_already_emitted(&existing_tool_calls, i) {
                                                    continue;
                                                }
                                                if let Some(obj) = tc.as_object() {
                                                    let mut tc = obj.clone();
                                                    if let Some(event) = tool_call_start_event(&mut tc) {
                                                        yield render_sse(&event);
                                                    }
                                                }
                                            }
                                            if let Some(event) = usage_sse_event_from_result_map(&result.usage) {
                                                yield render_sse(&event);
                                            }
                                        }
                                        Err(e) => {
                                            had_terminal_error = true;
                                            yield render_sse(&Value::Object(
                                                crate::build_stream_error_event(
                                                    &format!("anthropic stream stalled; non-stream recovery failed: {e}"),
                                                    "stream_transport",
                                                    true,
                                                ),
                                            ));
                                            tool_calls_map.clear();
                                            full_text.clear();
                                            reasoning.clear();
                                            reasoning_signature.clear();
                                        }
                                    }
                                } else {
                                    had_terminal_error = true;
                                    yield render_sse(&Value::Object(
                                        crate::build_stream_error_event(
                                            "anthropic SSE idle before any response data",
                                            "stream_idle",
                                            true,
                                        ),
                                    ));
                                }
                                break;
                            };
                            let Some(chunk) = next else { break };
                            let chunk = match chunk {
                                Ok(v) => v,
                                Err(e) => {
                                    if made_progress {
                                        astra_core::agent_warn!(
                                            "llm",
                                            "anthropic SSE transport error after progress: {e} — attempting non-stream fallback"
                                        );
                                        let streamed_text = full_text.clone();
                                        let streamed_reasoning = reasoning.clone();
                                        let existing_tool_calls = tool_calls_map.clone();
                                        let fb_timeout = crate::turn::llm::client::llm_nonstream_timeout();
                                        match crate::turn::llm::client::call_llm_nonstream(
                                            &client_for_fallback,
                                            LlmCall {
                                                purpose,
                                                messages: &messages_for_fallback,
                                                tools: &tools_for_fallback,
                                                route: LlmExecutionRoute {
                                                    model_name: &model_for_fallback,
                                                    wire_model_name: wire_model_for_fallback.as_deref(),
                                                    api_key: &api_key_for_fallback,
                                                    base_url: &base_url_for_fallback,
                                                    provider: &provider_for_fallback,
                                                    header_overrides: header_overrides_for_fallback.as_ref(),
                                                    request_body_overrides: request_body_overrides_for_fallback.as_ref(),
                                                    completions_url_override: completions_url_override_for_fallback.as_deref(),
                                                    request_timeout: request_timeout_for_fallback,
                                                },
                                                max_output_tokens: max_out_for_fallback,
                                                temperature,
                                                has_fallback,
                                                thinking: &thinking_for_fallback,
                                            },
                                            fb_timeout,
                                        )
                                        .await
                                        {
                                            Ok(mut result) => {
                                                ensure_tool_call_ids(&mut result.tool_calls);
                                                full_text = result.full_text.clone();
                                                reasoning = result.reasoning.clone();
                                                if !result.reasoning_signature.is_empty() {
                                                    reasoning_signature = result.reasoning_signature.clone();
                                                }
                                                usage = result.usage.clone();
                                                tool_calls_map.clear();
                                                for (i, tc) in result.tool_calls.iter().enumerate() {
                                                    if let Value::Object(m) = tc {
                                                        tool_calls_map.insert(i, m.clone());
                                                    }
                                                }
                                                if result.tool_calls.is_empty()
                                                    && let Some(suffix) =
                                                        streamed_suffix(&streamed_text, &result.full_text)
                                                {
                                                    yield render_sse(&json!({"type":"text_delta","content": suffix}));
                                                }
                                                if let Some(suffix) =
                                                    streamed_suffix(&streamed_reasoning, &result.reasoning)
                                                {
                                                    yield render_sse(&json!({"type":"reasoning_delta","content": suffix}));
                                                }
                                                for (i, tc) in result.tool_calls.iter().enumerate() {
                                                    if tool_call_start_already_emitted(&existing_tool_calls, i) {
                                                        continue;
                                                    }
                                                    if let Some(obj) = tc.as_object() {
                                                        let mut tc = obj.clone();
                                                        if let Some(event) = tool_call_start_event(&mut tc) {
                                                            yield render_sse(&event);
                                                        }
                                                    }
                                                }
                                                if let Some(event) = usage_sse_event_from_result_map(&result.usage) {
                                                    yield render_sse(&event);
                                                }
                                            }
                                            Err(error) => {
                                                had_terminal_error = true;
                                                yield render_sse(&Value::Object(
                                                    crate::build_stream_error_event(
                                                        &format!("anthropic stream transport failed; non-stream recovery failed: {error}"),
                                                        "stream_transport",
                                                        true,
                                                    ),
                                                ));
                                                tool_calls_map.clear();
                                                full_text.clear();
                                                reasoning.clear();
                                                reasoning_signature.clear();
                                            }
                                        }
                                    } else {
                                        had_terminal_error = true;
                                        yield render_sse(&Value::Object(
                                            crate::build_stream_error_event(
                                                &format!("anthropic SSE transport error: {e}"),
                                                "stream_transport",
                                                true,
                                            ),
                                        ));
                                        tool_calls_map.clear();
                                        full_text.clear();
                                        reasoning.clear();
                                        reasoning_signature.clear();
                                    }
                                    break;
                                }
                            };
                            for emitted in apply_anthropic_event(
                                &chunk,
                                &mut full_text,
                                &mut reasoning,
                                &mut reasoning_signature,
                                &mut tool_calls_map,
                                &mut usage,
                                &mut made_progress,
                            ) {
                                yield emitted;
                            }
                        }
                    }
                }

                if had_terminal_error {
                    return;
                }

                let tool_calls: Vec<Value> = {
                    let mut entries: Vec<_> = tool_calls_map.drain().collect();
                    entries.sort_by_key(|(i, _)| *i);
                    entries.into_iter().map(|(_, m)| Value::Object(m)).collect()
                };
                yield render_sse(&json!({
                    "type": "_inprocess_summary",
                    "full_text": full_text,
                    "reasoning": reasoning,
                    "reasoning_signature": reasoning_signature,
                    "tool_calls": tool_calls,
                    "usage": Value::Object(usage),
                }));
            };
            return Ok(Box::pin(out));
        }

        let retry_after_ms = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);
        let text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));
        last_err = format!("anthropic stream HTTP {status}: {text}");

        match classify_non_success_and_record_cooldown(
            status,
            retry_after_ms,
            cooldown,
            model_key,
            has_fallback,
            "anthropic",
        ) {
            RetryDecision::Retry { delay_ms } => {
                if let Some(d) = delay_ms {
                    sleep_ms_or_llm_cancel(d, bridge_llm_cancel(&client_cancel))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                continue;
            }
            RetryDecision::Terminal => return Err(last_err),
        }
    }
    Err(format!(
        "anthropic stream exhausted {LLM_MAX_RETRIES} retries: {last_err}"
    ))
}

/// Translate one parsed Anthropic SSE event into zero or more canonical
/// SSE bytes the bridge's forwarder knows how to handle.
///
/// Updates the caller's accumulator for `full_text` / `reasoning` /
/// `reasoning_signature` / `tool_calls_map` / `usage` so the final
/// `_inprocess_summary` event has complete state.
///
/// `reasoning_signature` is the HMAC-like token Anthropic emits at the
/// end of every `thinking` content block via a `signature_delta`. When
/// the caller re-submits the assistant message on the next turn, the
/// upstream requires the signature to be echoed back — DeepSeek's
/// anthropic-compatible endpoint surfaces its absence as
/// `content[].thinking must be passed back` (HTTP 400).
fn apply_anthropic_event(
    chunk: &Value,
    full_text: &mut String,
    reasoning: &mut String,
    reasoning_signature: &mut String,
    tool_calls_map: &mut std::collections::HashMap<usize, Map<String, Value>>,
    usage: &mut Map<String, Value>,
    made_progress: &mut bool,
) -> Vec<Bytes> {
    let mut out = Vec::new();
    let Some(ty) = chunk.get("type").and_then(Value::as_str) else {
        return out;
    };
    match ty {
        "message_start" => {
            if let Some(u) = chunk
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(Value::as_object)
                && let Some(extracted) = crate::turn::token_usage::extract_usage(
                    crate::turn::token_usage::UsageDialect::AnthropicMessages,
                    u,
                )
            {
                let map = extracted.to_json_map();
                *usage = map.clone();
                out.push(render_sse(&json!({
                    "type": "usage",
                    "input_tokens": extracted.input_tokens,
                    "cached_input_tokens": extracted.cached_input_tokens,
                    "cache_creation_tokens": extracted.cache_creation_tokens,
                    "output_tokens": extracted.output_tokens,
                    "total_tokens": extracted.total_tokens(),
                })));
                *made_progress = true;
            }
        }
        "content_block_start" => {
            // Record a tool_use block start so later input_json_delta can
            // be accumulated. Text / thinking blocks need no per-start
            // record.
            if let Some(block) = chunk.get("content_block").and_then(Value::as_object)
                && block.get("type").and_then(Value::as_str) == Some("tool_use")
            {
                let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("_unknown");
                tool_calls_map.insert(
                    index,
                    Map::from_iter([
                        ("id".to_string(), Value::String(id.to_string())),
                        ("type".to_string(), Value::String("function".to_string())),
                        (
                            "function".to_string(),
                            json!({"name": name, "arguments": ""}),
                        ),
                    ]),
                );
                out.push(render_sse(&json!({
                    "type": "tool_call_start",
                    "call_id": id,
                    "tool": name,
                })));
                *made_progress = true;
            }
        }
        "content_block_delta" => {
            let Some(delta) = chunk.get("delta").and_then(Value::as_object) else {
                return out;
            };
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        full_text.push_str(text);
                        out.push(render_sse(&json!({
                            "type": "text_delta",
                            "content": text,
                        })));
                        *made_progress = true;
                    }
                }
                Some("thinking_delta") => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        reasoning.push_str(text);
                        out.push(render_sse(&json!({
                            "type": "reasoning_delta",
                            "content": text,
                        })));
                        *made_progress = true;
                    }
                }
                Some("signature_delta") => {
                    // Anthropic emits the HMAC-style signature for the
                    // preceding thinking block as its own delta. Append
                    // to accumulator so the final `_inprocess_summary`
                    // event carries it to the bridge's forwarder, which
                    // persists it on the assistant message so the NEXT
                    // turn can echo it back unchanged. DeepSeek's
                    // anthropic endpoint rejects replays that lose the
                    // signature.
                    if let Some(sig) = delta.get("signature").and_then(Value::as_str)
                        && !sig.is_empty()
                    {
                        reasoning_signature.push_str(sig);
                        *made_progress = true;
                    }
                }
                Some("input_json_delta") => {
                    if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                        && !partial.is_empty()
                    {
                        let index =
                            chunk.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        if let Some(entry) = tool_calls_map.get_mut(&index)
                            && let Some(f) =
                                entry.get_mut("function").and_then(Value::as_object_mut)
                            && let Some(args) = f.get_mut("arguments")
                        {
                            if let Value::String(s) = args {
                                s.push_str(partial);
                            }
                        }
                        *made_progress = true;
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            // Final usage patch: `usage.output_tokens`.
            if let Some(u) = chunk.get("usage").and_then(Value::as_object)
                && let Some(out_toks) = u.get("output_tokens").and_then(Value::as_u64)
            {
                usage.insert("output_tokens".into(), Value::from(out_toks));
                let input = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cached = usage
                    .get("cached_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cache_creation = usage
                    .get("cache_creation_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push(render_sse(&json!({
                    "type": "usage",
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "cache_creation_tokens": cache_creation,
                    "output_tokens": out_toks,
                    "total_tokens": input + cached + cache_creation + out_toks,
                })));
            }
        }
        "message_stop" | "content_block_stop" | "ping" => {}
        _ => {}
    }
    out
}

/// Call LLM streaming API, yield SSE bytes.
/// Emits: text_delta, reasoning_delta, reasoning_done, tool_call_start, usage SSE events,
/// then a final `_inprocess_summary` event with full_text/tool_calls/usage/model_used.
///
/// **Stream resilience (same as [`super::llm::client::call_llm_and_collect`])**:
/// per-chunk idle watchdog on parsed SSE; if the provider stops sending, partial state is
/// discarded and a **single non-stream** `/chat/completions` request attempts recovery.
///
/// Retries up to LLM_MAX_RETRIES times on transient errors (429/5xx/network)
/// with exponential backoff.
///
/// **Note**: Caller must check rate-limit cooldown state and handle fallback model
/// resolution BEFORE calling this function. This function only handles retries for
/// transient errors within a single model.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn call_llm_stream(
    call: LlmCall<'_>,
    client_cancel: Option<Arc<CancellationToken>>,
) -> Result<Pin<Box<dyn futures_util::Stream<Item = Bytes> + Send + 'static>>, String> {
    let LlmCall {
        purpose,
        messages,
        tools,
        route,
        max_output_tokens,
        temperature,
        has_fallback,
        thinking,
    } = call;
    let LlmExecutionRoute {
        model_name,
        wire_model_name,
        api_key,
        base_url,
        provider,
        header_overrides,
        request_body_overrides,
        completions_url_override,
        request_timeout,
    } = route;
    let cooldown = rate_limit_cooldown();
    // `model_key` addresses the per-local-row rate-limit state. Two local
    // rows that share an upstream wire name (via `wire_model_name` alias)
    // still cooldown independently.
    let model_key = model_name;
    // `upstream_name` is what the provider actually sees in the request
    // body and URL. When no alias is configured, falls back to the local
    // name so callers that don't care about aliases pass `None`.
    let upstream_name = wire_model_name.unwrap_or(model_name);

    let mut client_builder = reqwest::Client::builder()
        .connect_timeout(crate::turn::llm::client::llm_connect_timeout())
        .timeout(std::time::Duration::from_secs(turn_timeout_s() as u64 + 10));
    // Honour HTTPS_PROXY / https_proxy / ALL_PROXY env vars (same as global_llm_client).
    client_builder = crate::turn::llm::client::apply_env_proxy(client_builder);
    let client = client_builder.build().map_err(|e| e.to_string())?;

    let messages =
        crate::turn::llm::client::consolidate_system_messages_for_provider(messages, provider);
    if provider_uses_bedrock_converse(provider) {
        return bedrock_stream_with_retry(
            &client,
            LlmCall {
                purpose,
                messages: &messages,
                tools,
                route: LlmExecutionRoute {
                    model_name,
                    wire_model_name,
                    api_key,
                    base_url,
                    provider,
                    header_overrides,
                    request_body_overrides,
                    completions_url_override,
                    request_timeout,
                },
                max_output_tokens,
                temperature,
                has_fallback,
                thinking,
            },
            client_cancel,
        )
        .await;
    }
    if provider_uses_anthropic_messages(provider) {
        return anthropic_stream_with_retry(
            &client,
            LlmCall {
                purpose,
                messages: &messages,
                tools,
                route: LlmExecutionRoute {
                    model_name,
                    wire_model_name,
                    api_key,
                    base_url,
                    provider,
                    header_overrides,
                    request_body_overrides,
                    completions_url_override,
                    request_timeout,
                },
                max_output_tokens,
                temperature,
                has_fallback,
                thinking,
            },
            client_cancel,
        )
        .await;
    }

    let body = build_provider_request_body_with_overrides(
        &messages,
        tools,
        upstream_name,
        provider,
        max_output_tokens,
        temperature,
        true,
        thinking,
        request_body_overrides,
    );

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        upstream_name,
        true,
    );
    let req_bytes = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);

    // Total budget guard: abort if retries + cooldown delays exceed the budget.
    let total_budget = crate::turn::llm::client::llm_total_budget();
    let started = std::time::Instant::now();

    // Retry loop for transient errors (429 rate limit, 5xx server errors, network)
    let mut last_err = String::new();
    for attempt in 0..=LLM_MAX_RETRIES {
        // Check total budget before each attempt
        if attempt > 0 && started.elapsed() > total_budget {
            return Err(format!(
                "LLM total budget exhausted ({:.0}s): {last_err}",
                total_budget.as_secs_f64()
            ));
        }

        if attempt > 0 {
            let delay = bridge_retry_backoff_ms(attempt);
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel))
                .await
                .map_err(|e| e.to_string())?;
        }

        let mut req = client.post(&url).header("content-type", "application/json");
        req = apply_provider_auth(req, provider, api_key, header_overrides);
        req = apply_llm_header_overrides(req, header_overrides);
        if let Some(timeout) = request_timeout {
            req = req.timeout(timeout);
        }

        let request_start = std::time::Instant::now();
        tracing::debug!(
            target: "astra_runtime::bridge_llm_stream",
            purpose = purpose.as_str(),
            provider,
            model_name,
            attempt,
            "sending inference request"
        );
        let response = match req.json(&body).send().await {
            Ok(r) => {
                astra_core::agent_info!(
                    "llm",
                    "⏱ LLM HTTP ok: status={} connect={}ms req={}B model={} attempt={}",
                    r.status().as_u16(),
                    request_start.elapsed().as_millis(),
                    req_bytes,
                    model_name,
                    attempt,
                );
                r
            }
            Err(e) => {
                astra_core::agent_warn!(
                    "llm",
                    "⏱ LLM send failed: {}ms model={} attempt={}",
                    request_start.elapsed().as_millis(),
                    model_name,
                    attempt,
                );
                last_err = format!("LLM request failed: {e}");
                // Network errors are always retryable
                continue;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Success — record to cooldown tracker and return the stream
            cooldown.with(model_key, |c| c.record_success());
            let byte_stream = response.bytes_stream();
            let model_name = model_name.to_string();
            let wire_model_for_fallback = wire_model_name.map(str::to_string);

            let client_for_fallback = client.clone();
            let messages_for_fallback: Arc<[Value]> = Arc::from(messages.as_slice());
            let tools_for_fallback: Arc<[Value]> = Arc::from(tools);
            let api_key_for_fallback = api_key.to_string();
            let base_url_for_fallback = base_url.to_string();
            let provider_for_fallback = provider.to_string();
            let header_overrides_for_fallback = header_overrides.cloned();
            let request_body_overrides_for_fallback = request_body_overrides.cloned();
            let completions_url_override_for_fallback =
                completions_url_override.map(str::to_string);
            let request_timeout_for_fallback = request_timeout;
            let max_out_for_fallback = max_output_tokens;
            let thinking_for_fallback = thinking.clone();
            let idle_pre = crate::turn::llm::client::stream_idle_timeout();
            let idle_post = crate::turn::llm::client::stream_idle_timeout_after_progress();

            let out = stream! {
                let cc = client_cancel.clone();
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut in_think = false;
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();
                let mut made_progress = false;
                let mut had_terminal_error = false;

                let sse = crate::turn::llm::client::parse_openai_sse_json_stream(byte_stream);
                tokio::pin!(sse);

                loop {
                    let idle = if made_progress { idle_post } else { idle_pre };
                    tokio::select! {
                        biased;
                        _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                            astra_core::agent_warn!(
                                "llm",
                                "in-process LLM SSE cancelled (client disconnect)"
                            );
                            tool_calls_map.clear();
                            full_text.clear();
                            reasoning.clear();
                            break;
                        }
                        tick = tokio::time::timeout(idle, sse.next()) => {
                            let next = tick;
                            let chunk = match next {
                                Ok(c) => c,
                                Err(_) => {
                                    astra_core::agent_warn!(
                                        "llm",
                                        "in-process stream idle after {}ms (made_progress={}) — attempting non-stream fallback",
                                        idle.as_millis(),
                                        made_progress
                                    );
                                    let streamed_text = full_text.clone();
                                    let streamed_reasoning = reasoning.clone();
                                    let existing_tool_calls = tool_calls_map.clone();
                                    let fb_timeout = crate::turn::llm::client::llm_nonstream_timeout();
                                    match crate::turn::llm::client::call_llm_nonstream(
                                        &client_for_fallback,
                                        LlmCall {
                                            purpose,
                                            messages: &messages_for_fallback,
                                            tools: &tools_for_fallback,
                                            route: LlmExecutionRoute {
                                                model_name: &model_name,
                                                wire_model_name: wire_model_for_fallback.as_deref(),
                                                api_key: &api_key_for_fallback,
                                                base_url: &base_url_for_fallback,
                                                provider: &provider_for_fallback,
                                                header_overrides: header_overrides_for_fallback.as_ref(),
                                                request_body_overrides: request_body_overrides_for_fallback.as_ref(),
                                                completions_url_override: completions_url_override_for_fallback.as_deref(),
                                                request_timeout: request_timeout_for_fallback,
                                            },
                                            max_output_tokens: max_out_for_fallback,
                                            temperature,
                                            has_fallback,
                                            thinking: &thinking_for_fallback,
                                        },
                                        fb_timeout,
                                    )
                                    .await
                                    {
                                        Ok(mut result) => {
                                            ensure_tool_call_ids(&mut result.tool_calls);
                                            full_text = result.full_text.clone();
                                            reasoning = result.reasoning.clone();
                                            usage = result.usage.clone();
                                            tool_calls_map.clear();
                                            for (i, tc) in result.tool_calls.iter().enumerate() {
                                                if let Value::Object(m) = tc {
                                                    tool_calls_map.insert(i, m.clone());
                                                }
                                            }
                                            if result.tool_calls.is_empty()
                                                && let Some(suffix) =
                                                    streamed_suffix(&streamed_text, &result.full_text)
                                            {
                                                yield render_sse(
                                                    &json!({"type":"text_delta","content": suffix}),
                                                );
                                            }
                                            if let Some(suffix) =
                                                streamed_suffix(&streamed_reasoning, &result.reasoning)
                                            {
                                                yield render_sse(
                                                    &json!({"type":"reasoning_delta","content": suffix}),
                                                );
                                            }
                                            for (i, tc) in result.tool_calls.iter().enumerate() {
                                                if tool_call_start_already_emitted(
                                                    &existing_tool_calls,
                                                    i,
                                                ) {
                                                    continue;
                                                }
                                                if let Some(obj) = tc.as_object() {
                                                    let mut tc = obj.clone();
                                                    if let Some(event) = tool_call_start_event(&mut tc)
                                                    {
                                                        yield render_sse(&event);
                                                    }
                                                }
                                            }
                                            if let Some(event) = usage_sse_event_from_result_map(&result.usage) {
                                                yield render_sse(&event);
                                            }
                                        }
                                        Err(e) => {
                                            had_terminal_error = true;
                                            yield render_sse(&Value::Object(
                                                crate::build_stream_error_event(
                                                    &format!(
                                                        "stream stalled; non-stream recovery failed: {e}"
                                                    ),
                                                    "stream_idle",
                                                    true,
                                                ),
                                            ));
                                            tool_calls_map.clear();
                                            full_text.clear();
                                            reasoning.clear();
                                        }
                                    }
                                    break;
                                }
                            };
                            let Some(item) = chunk else { break };
                            let chunk = match item {
                                Ok(v) => v,
                                Err(e) => {
                                    if made_progress {
                                        astra_core::agent_warn!(
                                            "llm",
                                            "in-process stream transport error after progress: {e} — attempting non-stream fallback"
                                        );
                                        let streamed_text = full_text.clone();
                                        let streamed_reasoning = reasoning.clone();
                                        let existing_tool_calls = tool_calls_map.clone();
                                        let fb_timeout = crate::turn::llm::client::llm_nonstream_timeout();
                                        match crate::turn::llm::client::call_llm_nonstream(
                                            &client_for_fallback,
                                            LlmCall {
                                                purpose,
                                                messages: &messages_for_fallback,
                                                tools: &tools_for_fallback,
                                                route: LlmExecutionRoute {
                                                    model_name: &model_name,
                                                    wire_model_name: wire_model_for_fallback.as_deref(),
                                                    api_key: &api_key_for_fallback,
                                                    base_url: &base_url_for_fallback,
                                                    provider: &provider_for_fallback,
                                                    header_overrides: header_overrides_for_fallback.as_ref(),
                                                    request_body_overrides: request_body_overrides_for_fallback.as_ref(),
                                                    completions_url_override: completions_url_override_for_fallback.as_deref(),
                                                    request_timeout: request_timeout_for_fallback,
                                                },
                                                max_output_tokens: max_out_for_fallback,
                                                temperature,
                                                has_fallback,
                                                thinking: &thinking_for_fallback,
                                            },
                                            fb_timeout,
                                        )
                                        .await
                                        {
                                            Ok(mut result) => {
                                                ensure_tool_call_ids(&mut result.tool_calls);
                                                full_text = result.full_text.clone();
                                                reasoning = result.reasoning.clone();
                                                usage = result.usage.clone();
                                                tool_calls_map.clear();
                                                for (i, tc) in result.tool_calls.iter().enumerate() {
                                                    if let Value::Object(m) = tc {
                                                        tool_calls_map.insert(i, m.clone());
                                                    }
                                                }
                                                if result.tool_calls.is_empty()
                                                    && let Some(suffix) = streamed_suffix(&streamed_text, &result.full_text)
                                                {
                                                    yield render_sse(&json!({"type":"text_delta","content": suffix}));
                                                }
                                                if let Some(suffix) = streamed_suffix(&streamed_reasoning, &result.reasoning) {
                                                    yield render_sse(&json!({"type":"reasoning_delta","content": suffix}));
                                                }
                                                for (i, tc) in result.tool_calls.iter().enumerate() {
                                                    if tool_call_start_already_emitted(&existing_tool_calls, i) {
                                                        continue;
                                                    }
                                                    if let Some(obj) = tc.as_object() {
                                                        let mut tc = obj.clone();
                                                        if let Some(event) = tool_call_start_event(&mut tc) {
                                                            yield render_sse(&event);
                                                        }
                                                    }
                                                }
                                                if let Some(event) = usage_sse_event_from_result_map(&result.usage) {
                                                    yield render_sse(&event);
                                                }
                                            }
                                            Err(error) => {
                                                had_terminal_error = true;
                                                yield render_sse(&Value::Object(
                                                    crate::build_stream_error_event(
                                                        &format!(
                                                            "stream transport failed; non-stream recovery failed: {error}"
                                                        ),
                                                        "stream_transport",
                                                        true,
                                                    ),
                                                ));
                                                tool_calls_map.clear();
                                                full_text.clear();
                                                reasoning.clear();
                                            }
                                        }
                                    } else {
                                        astra_core::agent_warn!(
                                            "llm",
                                            "in-process stream transport error: {e}"
                                        );
                                        had_terminal_error = true;
                                        yield render_sse(&Value::Object(
                                            crate::build_stream_error_event(
                                                &format!("LLM stream transport error: {e}"),
                                                "stream_transport",
                                                true,
                                            ),
                                        ));
                                        tool_calls_map.clear();
                                        full_text.clear();
                                        reasoning.clear();
                                    }
                                    break;
                                }
                            };
                            // Some providers attach usage to a chunk that also contains choices,
                            // so parse usage first on every chunk. Streaming endpoints are
                            // OpenAI-compatible; Bedrock is intercepted earlier.
                            made_progress = true;
                            if let Some(u) = chunk.get("usage").and_then(Value::as_object)
                                && let Some(extracted) = crate::turn::token_usage::extract_usage(
                                    crate::turn::token_usage::UsageDialect::OpenAi,
                                    u,
                                )
                            {
                                usage = extracted.to_json_map();
                                yield render_sse(&json!({
                                    "type": "usage",
                                    "input_tokens": extracted.input_tokens,
                                    "cached_input_tokens": extracted.cached_input_tokens,
                                    "cache_creation_tokens": extracted.cache_creation_tokens,
                                    "output_tokens": extracted.output_tokens,
                                    "total_tokens": extracted.total_tokens(),
                                }));
                            }

                            let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
                                continue;
                            };

                            let Some(delta) = choices.first()
                                .and_then(|c| c.get("delta"))
                                .and_then(Value::as_object)
                            else { continue };

                            // Text content
                            if let Some(content) = delta.get("content").and_then(Value::as_str)
                                && !content.is_empty() {
                                    for (chunk, is_reasoning) in split_think_chunks(content, &mut in_think) {
                                        if chunk.is_empty() {
                                            continue;
                                        }
                                        if is_reasoning {
                                            reasoning.push_str(&chunk);
                                            yield render_sse(&json!({"type": "reasoning_delta", "content": chunk}));
                                        } else {
                                            full_text.push_str(&chunk);
                                            yield render_sse(&json!({"type": "text_delta", "content": chunk}));
                                        }
                                    }
                                }

                            // Reasoning (DeepSeek / o1 style)
                            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str)
                                && !r.is_empty() {
                                    reasoning.push_str(r);
                                    yield render_sse(&json!({"type": "reasoning_delta", "content": r}));
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
                                        && !id.is_empty() {
                                            entry.insert("id".to_string(), Value::String(id.to_string()));
                                        }
                                        if let Some(func) = tc.get("function").and_then(Value::as_object) {
                                            let f = entry
                                                .entry("function".to_string())
                                                .or_insert_with(|| json!({}));
                                            let Some(f) = f.as_object_mut() else { continue; };
                                            if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                                                let existing = f
                                                    .entry("arguments".to_string())
                                                    .or_insert_with(|| Value::String(String::new()));
                                                if let Value::String(s) = existing {
                                                    s.push_str(args);
                                                }
                                            }
                                            if let Some(name) = func.get("name").and_then(Value::as_str)
                                            && is_valid_tool_name(name) {
                                                let is_new = f.get("name").and_then(Value::as_str).unwrap_or("").is_empty();
                                                f.insert("name".to_string(), Value::String(name.to_string()));
                                                if is_new {
                                                    if let Some(event) = tool_call_start_event(entry) {
                                                        yield render_sse(&event);
                                                    }
                                                }
                                            } else if let Some(bad_name) = func.get("name").and_then(Value::as_str) {
                                                astra_core::agent_warn!(
                                                    "llm",
                                                    "dropped malformed tool_call with invalid name: {bad_name:?}"
                                                );
                                            }
                                        }
                                    }
                                }
                        }
                    }
                }

                if had_terminal_error {
                    return;
                }

                // Emit final summary as a special internal event (not forwarded to client)
                let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
                sorted_tcs.sort_by_key(|(idx, _)| *idx);
                let mut tool_calls: Vec<Value> = sorted_tcs.into_iter().map(|(_, v)| Value::Object(v)).collect();

                // Degraded tool-call fallback: recover <invoke> or <tool_call> blocks.
                if tool_calls.is_empty() {
                    if let Some(parsed) = astra_turn_core::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text) {
                        astra_core::agent_warn!(
                            "llm",
                            "recovered {} tool call(s) from degraded text in content (inprocess)",
                            parsed.len()
                        );
                        full_text = astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
                        tool_calls = parsed;
                    }
                }

                yield render_sse(&json!({
                    "type": "_inprocess_summary",
                    "full_text": full_text,
                    "reasoning": reasoning,
                    "tool_calls": tool_calls,
                    "usage": usage,
                    "model_used": model_name,
                }));
            };

            return Ok(Box::pin(out));
        }

        // Non-success: check if retryable (429 rate limit, 5xx server error)
        let headers = response.headers();
        let retry_after_ms = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);

        let text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));
        last_err = format!("LLM error {status}: {text}");

        match classify_non_success_and_record_cooldown(
            status,
            retry_after_ms,
            cooldown,
            model_key,
            has_fallback,
            "openai-stream",
        ) {
            RetryDecision::Retry { delay_ms } => {
                if let Some(d) = delay_ms {
                    sleep_ms_or_llm_cancel(d, bridge_llm_cancel(&client_cancel))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                continue;
            }
            // 4xx (except 429) is not retryable — fail immediately.
            // Context-window errors are detected by content at the call site
            // (bridge_inprocess forward()), not here.
            RetryDecision::Terminal => return Err(last_err),
        }
    }

    // All retries exhausted
    Err(format!("{last_err} (after {} retries)", LLM_MAX_RETRIES))
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
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn is_valid_tool_name_rejects_xml_artifacts() {
        assert!(!is_valid_tool_name("<invoke>"));
        assert!(!is_valid_tool_name("</invoke>"));
        assert!(!is_valid_tool_name("<tool_call>"));
        assert!(!is_valid_tool_name("name<"));
        assert!(!is_valid_tool_name(">name"));
    }

    #[test]
    fn is_valid_tool_name_rejects_empty_and_whitespace() {
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("has space"));
        assert!(!is_valid_tool_name("has\ttab"));
        assert!(!is_valid_tool_name("has\nnewline"));
    }

    #[test]
    fn is_valid_tool_name_accepts_valid_names() {
        assert!(is_valid_tool_name("read_file"));
        assert!(is_valid_tool_name("bash"));
        assert!(is_valid_tool_name("list_directory"));
        assert!(is_valid_tool_name("SearchDefinition"));
    }

    #[test]
    fn tool_call_start_event_builds_correctly() {
        let mut tc = Map::new();
        tc.insert("id".to_string(), json!("call_1"));
        tc.insert("type".to_string(), json!("function"));
        tc.insert(
            "function".to_string(),
            json!({"name": "read_file", "arguments": "{\"path\":\"/tmp\"}"}),
        );
        let event = tool_call_start_event(&mut tc).unwrap();
        assert_eq!(event["type"], "tool_call_start");
        assert_eq!(event["tool"], "read_file");
        assert_eq!(event["call_id"], "call_1");
        assert_eq!(event["arguments"], "{\"path\":\"/tmp\"}");
    }

    #[test]
    fn tool_call_start_event_assigns_id_when_missing() {
        let mut tc = Map::new();
        tc.insert("type".to_string(), json!("function"));
        tc.insert(
            "function".to_string(),
            json!({"name": "bash", "arguments": ""}),
        );
        let event = tool_call_start_event(&mut tc).unwrap();
        assert_eq!(event["type"], "tool_call_start");
        assert_eq!(event["tool"], "bash");
        assert!(!event["call_id"].as_str().unwrap().is_empty());
        // Should also have inserted the id back into tc
        assert!(!tc["id"].as_str().unwrap().is_empty());
    }

    #[test]
    fn tool_call_start_event_rejects_invalid_name() {
        let mut tc = Map::new();
        tc.insert("type".to_string(), json!("function"));
        tc.insert(
            "function".to_string(),
            json!({"name": "<invoke>", "arguments": ""}),
        );
        assert!(tool_call_start_event(&mut tc).is_none());
    }

    /// P1-F: call_llm_stream must have a total budget guard to prevent
    /// unbounded blocking when the provider returns repeated 429s with
    /// long retry-after headers.

    // ── wire_model_name alias routing ────────────────────────────────────
    //
    // When a local row carries `wire_model_name = Some("deepseek-v4-pro")`
    // but its stored `model_name` is `deepseek-v4-pro-anthropic`, the
    // outbound request's `model` field MUST be the wire name. This is the
    // fix that lets us host two local rows backed by the same upstream
    // model id but differing on provider / base_url / credentials.
    #[tokio::test]
    async fn call_llm_stream_sends_wire_model_name_in_body_when_alias_set() {
        use std::sync::Mutex;

        use axum::{Router, body::Body, extract::State, response::Response, routing::post};

        #[derive(Clone, Default, Debug)]
        struct ModelCapture {
            model: Option<String>,
        }

        async fn handler(
            State(capture): State<Arc<Mutex<ModelCapture>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            capture.lock().expect("capture lock").model = body
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let payload = json!({"choices":[{"delta":{"content":"ok"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {payload}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("response")
        }

        let capture = Arc::new(Mutex::new(ModelCapture::default()));
        let app = Router::new()
            .route("/chat/completions", post(handler))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let base = format!("http://{addr}");

        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role": "user", "content": "hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "deepseek-v4-pro-anthropic",
                    wire_model_name: Some("deepseek-v4-pro"),
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream");
        let _: Vec<_> = stream.collect().await;

        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(
            seen.model.as_deref(),
            Some("deepseek-v4-pro"),
            "wire_model_name must land in the request body's `model` field, \
             not the local row name. seen={seen:?}",
        );
    }

    #[tokio::test]
    async fn call_llm_stream_sends_local_model_name_when_no_alias() {
        use std::sync::Mutex;

        use axum::{Router, body::Body, extract::State, response::Response, routing::post};

        #[derive(Clone, Default, Debug)]
        struct ModelCapture {
            model: Option<String>,
        }

        async fn handler(
            State(capture): State<Arc<Mutex<ModelCapture>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            capture.lock().expect("capture lock").model = body
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let payload = json!({"choices":[{"delta":{"content":"ok"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {payload}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("response")
        }

        let capture = Arc::new(Mutex::new(ModelCapture::default()));
        let app = Router::new()
            .route("/chat/completions", post(handler))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let base = format!("http://{addr}");

        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role": "user", "content": "hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream");
        let _: Vec<_> = stream.collect().await;

        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(
            seen.model.as_deref(),
            Some("gpt-5-mini"),
            "with no alias, request body.model must equal the local name. seen={seen:?}",
        );
    }

    // ── anthropic-sse stream parser ─────────────────────────────────────
    //
    // Pre-fix: `call_llm_stream` branched only on `BedrockConverse` and fell
    // through to the OpenAI SSE parser for everything else, including
    // `provider=anthropic`. Anthropic's stream uses distinct event names
    // (`message_start`, `content_block_delta`, `message_delta`,
    // `message_stop`) whose payloads do NOT match the OpenAI
    // `choices[0].delta.{content,tool_calls}` schema. So every event was
    // silently dropped → empty text + empty usage even though the upstream
    // responded correctly. This regression surfaced when we added
    // `deepseek-v4-pro-anthropic` via the anthropic-compatible endpoint.
    //
    // The test below emits a minimal anthropic stream and asserts both:
    //   - the accumulated text reaches the client via `text_delta` forward
    //   - usage tokens from `message_start` + `message_delta` reach the
    //     client as a canonical `type: usage` SSE event
    #[tokio::test]
    async fn call_llm_stream_parses_anthropic_sse_text_and_usage() {
        use axum::{Router, body::Body, extract::State, response::Response, routing::post};
        use std::sync::Mutex;

        #[derive(Clone, Default, Debug)]
        struct Hit {
            got: bool,
        }

        async fn handler(State(hit): State<Arc<Mutex<Hit>>>) -> Response {
            hit.lock().expect("hit").got = true;
            // A minimal but realistic Anthropic SSE stream: message_start
            // with initial usage, two text_deltas, a message_delta with
            // final usage, then message_stop.
            let sse = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-test\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":10,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            );
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(sse))
                .expect("response")
        }

        let hit = Arc::new(Mutex::new(Hit::default()));
        let app = Router::new()
            .route("/v1/messages", post(handler))
            .with_state(hit.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let base = format!("http://{addr}");

        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role": "user", "content": "hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "claude-test",
                    wire_model_name: None,
                    api_key: "test-key",
                    base_url: &base,
                    provider: "anthropic",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(50),
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream started");

        // Collect every forwarded SSE event into (type, payload) pairs so
        // we can assert on the canonical types the bridge emits downstream.
        let bytes: Vec<Bytes> = stream.collect().await;
        let raw = bytes
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<String>();

        let mut text_deltas = Vec::<String>::new();
        let mut usage_events = Vec::<Value>::new();
        for chunk in raw.split("\n\n") {
            let chunk = chunk.trim();
            let Some(rest) = chunk.strip_prefix("data: ") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(rest) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("text_delta") => text_deltas.push(
                    v.get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                Some("usage") => usage_events.push(v),
                _ => {}
            }
        }

        assert!(hit.lock().expect("hit").got, "upstream was called");
        assert_eq!(
            text_deltas.join(""),
            "Hello world",
            "forwarded text must aggregate the anthropic text_deltas — got {text_deltas:?}",
        );
        let last_usage = usage_events.last().cloned().unwrap_or_default();
        assert_eq!(
            last_usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            42,
            "input_tokens must reach the forwarded usage event — got {last_usage}",
        );
        assert_eq!(
            last_usage
                .get("cached_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            10,
            "cached_input_tokens must propagate — got {last_usage}",
        );
        assert_eq!(
            last_usage
                .get("cache_creation_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            2,
            "cache_creation_tokens must propagate from message_start to final usage — got {last_usage}",
        );
        assert!(
            last_usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= 7,
            "output_tokens must reflect message_delta update — got {last_usage}",
        );
    }

    // ── anthropic-sse thinking + signature ──────────────────────────────
    //
    // Anthropic's extended-thinking protocol sends reasoning as a
    // `thinking` content block. During streaming the block emits:
    //   1) `content_block_start` with `content_block.type = "thinking"`
    //   2) one or more `content_block_delta` with `delta.type = "thinking_delta"`
    //   3) one `content_block_delta` with `delta.type = "signature_delta"`
    //      carrying `delta.signature: "<signed_hmac>"`
    //   4) `content_block_stop`
    //
    // The upstream refuses to accept a subsequent assistant message that
    // echoes thinking content WITHOUT the original signature — DeepSeek's
    // anthropic-compatible endpoint surfaces this as:
    //   `content[].thinking in the thinking mode must be passed back to the API`
    //
    // Without capturing signature_delta, astra records only the reasoning
    // text; the next turn's request loses the signature and the upstream
    // rejects. This test pins the contract that the final
    // `_inprocess_summary` event carries `reasoning_signature`.
    #[tokio::test]
    async fn call_llm_stream_captures_anthropic_thinking_signature() {
        use axum::{Router, body::Body, extract::State, response::Response, routing::post};
        use std::sync::Mutex;

        #[derive(Clone, Default, Debug)]
        struct Hit {
            got: bool,
        }

        async fn handler(State(hit): State<Arc<Mutex<Hit>>>) -> Response {
            hit.lock().expect("hit").got = true;
            let sse = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-test\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":100,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":0}}}\n\n",
                // Thinking block: start, two thinking deltas, signature delta, stop.
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" about this.\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_abc123\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                // Text block to satisfy Anthropic's rule about a non-thinking
                // final block.
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            );
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(sse))
                .expect("response")
        }

        let hit = Arc::new(Mutex::new(Hit::default()));
        let app = Router::new()
            .route("/v1/messages", post(handler))
            .with_state(hit.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let base = format!("http://{addr}");

        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role": "user", "content": "hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "claude-test",
                    wire_model_name: None,
                    api_key: "test-key",
                    base_url: &base,
                    provider: "anthropic",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(50),
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream started");

        let bytes: Vec<Bytes> = stream.collect().await;
        let raw = bytes
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<String>();

        // Walk SSE events and find the _inprocess_summary — it carries the
        // final `reasoning_signature` the forwarder propagates downstream.
        let mut summary: Option<Value> = None;
        for chunk in raw.split("\n\n") {
            let chunk = chunk.trim();
            let Some(rest) = chunk.strip_prefix("data: ") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(rest) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) == Some("_inprocess_summary") {
                summary = Some(v);
                break;
            }
        }
        assert!(hit.lock().expect("hit").got, "upstream was called");
        let summary = summary.expect("stream must emit a final _inprocess_summary event");
        assert_eq!(
            summary
                .get("reasoning")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "Let me think about this.",
            "thinking deltas must accumulate into reasoning — got {summary}",
        );
        assert_eq!(
            summary
                .get("reasoning_signature")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "sig_abc123",
            "signature_delta must be captured and forwarded so the next \
             turn can echo it back — got {summary}",
        );
    }

    #[tokio::test]
    async fn call_llm_stream_omits_empty_assistant_tool_calls_in_request_body() {
        #[derive(Clone, Default, Debug)]
        struct Capture {
            messages: Vec<Value>,
        }

        async fn handler(
            State(capture): State<Arc<Mutex<Capture>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            capture.lock().expect("capture lock").messages = body
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let payload = json!({"choices":[{"delta":{"content":"ok"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {payload}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("response")
        }

        async fn spawn_test_server(app: Router) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("server");
            });
            format!("http://{addr}")
        }

        let capture = Arc::new(Mutex::new(Capture::default()));
        let app = Router::new()
            .route("/chat/completions", post(handler))
            .with_state(capture.clone());
        let base = spawn_test_server(app).await;
        let messages = vec![
            json!({"role":"assistant","content":"Done.","tool_calls":[]}),
            json!({"role":"user","content":"hi"}),
        ];

        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream");
        let _: Vec<_> = stream.collect().await;

        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(seen.messages.len(), 2);
        assert!(seen.messages[0].get("tool_calls").is_none(), "{seen:?}");
    }

    #[derive(Clone)]
    struct TransportFallbackHits {
        stream_hits: Arc<AtomicU32>,
        fallback_hits: Arc<AtomicU32>,
    }

    async fn spawn_raw_partial_transport_server(
        hits: TransportFallbackHits,
        fallback_status: u16,
        fallback_body: &'static str,
    ) -> String {
        spawn_raw_partial_transport_server_with_content(
            hits,
            "partial",
            fallback_status,
            fallback_body,
        )
        .await
    }

    async fn spawn_raw_partial_transport_server_with_content(
        hits: TransportFallbackHits,
        stream_content: &'static str,
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
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..read]);
                    let is_stream = req.contains("\"stream\":true");
                    if is_stream {
                        hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                        let partial = format!(
                            "data: {}\n\n",
                            json!({"choices":[{"delta":{"content":stream_content}}]})
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
                        hits.fallback_hits.fetch_add(1, Ordering::SeqCst);
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

    async fn spawn_raw_anthropic_partial_transport_server(
        hits: TransportFallbackHits,
        fallback_status: u16,
        fallback_body: &'static str,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw mock anthropic listener");
        let addr = listener.local_addr().expect("raw anthropic local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..read]);
                    let is_stream = req.contains("\"stream\":true");
                    if is_stream {
                        hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                        let partial = format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {"type": "text_delta", "text": "partial"},
                            })
                        );
                        let chunk = format!("{:X}\r\n{}\r\n", partial.len(), partial);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write partial anthropic stream response");
                        let _ = socket.shutdown().await;
                    } else {
                        hits.fallback_hits.fetch_add(1, Ordering::SeqCst);
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
                            .expect("write anthropic fallback response");
                        let _ = socket.shutdown().await;
                    }
                });
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn call_llm_stream_falls_back_after_partial_stream_transport_error() {
        let hits = TransportFallbackHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(
            hits.clone(),
            200,
            r#"{"choices":[{"message":{"content":"from-transport-fallback"}}]}"#,
        )
        .await;
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("bridge stream");
        let body = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<String>();
        assert!(
            !body.contains("\"type\":\"error\""),
            "transport-after-progress should recover via fallback instead of emitting an error: {body}"
        );
        assert!(
            body.contains("from-transport-fallback"),
            "fallback content should reach the client: {body}"
        );
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn anthropic_stream_transport_falls_back_after_partial_stream_transport_error() {
        let hits = TransportFallbackHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_anthropic_partial_transport_server(
            hits.clone(),
            200,
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"partial recovered"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}"#,
        )
        .await;
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "claude-test",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "anthropic",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("anthropic bridge stream");
        let body = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<String>();
        assert!(
            !body.contains("\"type\":\"error\""),
            "anthropic transport-after-progress should recover via fallback instead of emitting an error: {body}"
        );
        let stitched_text = body
            .split("\n\n")
            .filter_map(|frame| frame.trim().strip_prefix("data: "))
            .filter_map(|json_line| serde_json::from_str::<Value>(json_line).ok())
            .filter(|event| event.get("type").and_then(Value::as_str) == Some("text_delta"))
            .filter_map(|event| {
                event
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<String>();
        assert_eq!(
            stitched_text, "partial recovered",
            "fallback should only emit the missing anthropic suffix: {body}"
        );
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn anthropic_stream_transport_fallback_failure_emits_structured_error_code() {
        let hits = TransportFallbackHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_anthropic_partial_transport_server(
            hits.clone(),
            500,
            r#"{"error":{"message":"fallback transport recovery failed"}}"#,
        )
        .await;
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "claude-test",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "anthropic",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("anthropic bridge stream");
        let body = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<String>();
        assert!(
            body.contains("\"content\":\"partial\""),
            "partial anthropic text should reach the client before fallback failure: {body}"
        );
        assert!(
            body.contains("\"code\":\"stream_transport\""),
            "anthropic transport fallback failure should emit structured stream_transport code: {body}"
        );
        assert!(
            body.contains("\"retryable\":true"),
            "anthropic transport fallback failure should stay retryable: {body}"
        );
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_stream_transport_fallback_failure_emits_structured_error_code() {
        let hits = TransportFallbackHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(
            hits.clone(),
            500,
            r#"{"error":{"message":"fallback transport recovery failed"}}"#,
        )
        .await;
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("bridge stream");
        let body = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<String>();
        assert!(
            body.contains("\"content\":\"partial\""),
            "partial streamed text should still reach the client before the failure: {body}"
        );
        assert!(
            body.contains("\"code\":\"stream_transport\""),
            "transport fallback failure should emit a structured stream_transport code: {body}"
        );
        assert!(
            body.contains("\"retryable\":true"),
            "transport fallback failure should stay retryable: {body}"
        );
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_stream_routes_inline_think_to_reasoning_before_fallback_failure() {
        let hits = TransportFallbackHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server_with_content(
            hits.clone(),
            "<think>hidden reasoning",
            500,
            r#"{"error":{"message":"fallback transport recovery failed"}}"#,
        )
        .await;
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("bridge stream");
        let body = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<String>();
        assert!(
            !body.contains("<think>"),
            "streamed body must not expose inline think tags: {body}"
        );
        assert!(
            body.contains("\"type\":\"reasoning_delta\"")
                && body.contains("\"content\":\"hidden reasoning\""),
            "inline think content should be routed to reasoning_delta before fallback failure: {body}"
        );
        assert!(
            body.contains("\"code\":\"stream_transport\""),
            "transport fallback failure should still surface structured error: {body}"
        );
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.fallback_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_stream_transport_fallback_emits_only_missing_text_suffix() {
        let hits = TransportFallbackHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            fallback_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(
            hits,
            200,
            r#"{"choices":[{"message":{"content":"partial done"}}]}"#,
        )
        .await;
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("bridge stream");
        let body = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect::<String>();
        let stitched_text = body
            .split("\n\n")
            .filter_map(|frame| frame.trim().strip_prefix("data: "))
            .filter_map(|json_line| serde_json::from_str::<Value>(json_line).ok())
            .filter(|event| event.get("type").and_then(Value::as_str) == Some("text_delta"))
            .filter_map(|event| {
                event
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<String>();
        assert_eq!(
            stitched_text, "partial done",
            "fallback should only emit the missing suffix, not duplicate already-streamed text: {body}"
        );
    }

    // ── Bedrock streaming retry contract ────────────────────────────────

    /// Hand-roll a minimal AWS EventStream frame (two string headers +
    /// JSON payload) matching Bedrock's Converse event envelope.
    fn build_bedrock_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        fn enc_str(out: &mut Vec<u8>, name: &str, value: &str) {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
            out.push(7); // string type
            out.extend_from_slice(&(value.len() as u16).to_be_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        let mut headers = Vec::new();
        enc_str(&mut headers, ":message-type", "event");
        enc_str(&mut headers, ":event-type", event_type);
        let headers_len = headers.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;
        let mut out = Vec::with_capacity(total_len as usize);
        out.extend_from_slice(&total_len.to_be_bytes());
        out.extend_from_slice(&headers_len.to_be_bytes());
        let prelude_crc = crc32fast::hash(&out[0..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        let msg_crc = crc32fast::hash(&out);
        out.extend_from_slice(&msg_crc.to_be_bytes());
        out
    }

    /// Raw TCP server that returns HTTP 429 on the first POST and a valid
    /// Bedrock EventStream body on the second. Counts hits so the test
    /// can assert both attempts happened and the cooldown tracker saw the
    /// 429 → retry transition.
    async fn spawn_bedrock_retry_server(hits: Arc<AtomicU32>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind bedrock retry listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let _ = socket.read(&mut buf).await.unwrap_or(0);
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // First attempt → 429 Too Many Requests with Retry-After: 0
                        // so the cooldown's "wait before retry" collapses to 0ms.
                        // Without this, the production default of 5s kicks in and
                        // the test spends ~5s sleeping between attempts.
                        let body = "{\"message\":\"rate limited\"}";
                        let resp = format!(
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    } else {
                        // Second attempt → 200 + valid EventStream body.
                        let start = build_bedrock_frame("messageStart", br#"{"role":"assistant"}"#);
                        let delta = build_bedrock_frame(
                            "contentBlockDelta",
                            br#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
                        );
                        let stop =
                            build_bedrock_frame("messageStop", br#"{"stopReason":"end_turn"}"#);
                        let meta = build_bedrock_frame(
                            "metadata",
                            br#"{"usage":{"inputTokens":3,"outputTokens":1,"totalTokens":4}}"#,
                        );
                        let mut body = Vec::new();
                        body.extend_from_slice(&start);
                        body.extend_from_slice(&delta);
                        body.extend_from_slice(&stop);
                        body.extend_from_slice(&meta);
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                        let _ = socket.shutdown().await;
                    }
                });
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        format!("http://{addr}")
    }

    // ── classify_non_success_and_record_cooldown (shared retry helper) ──

    #[test]
    fn classify_429_records_cooldown_and_returns_retry_with_delay() {
        let cd = PerModelCooldown::new();
        let d = classify_non_success_and_record_cooldown(
            429,
            Some(2500),
            &cd,
            "test-model",
            false,
            "unit",
        );
        match d {
            RetryDecision::Retry { delay_ms } => {
                // Cooldown's own delay can override our hint; just assert SOME delay came back.
                assert!(delay_ms.is_some(), "429 should yield a wait delay");
            }
            RetryDecision::Terminal => panic!("429 must be retryable"),
        }
    }

    #[test]
    fn classify_529_records_overload_and_returns_retry() {
        let cd = PerModelCooldown::new();
        let d =
            classify_non_success_and_record_cooldown(529, None, &cd, "test-model", false, "unit");
        assert!(matches!(d, RetryDecision::Retry { .. }));
    }

    #[test]
    fn classify_generic_5xx_retries_without_cooldown() {
        let cd = PerModelCooldown::new();
        let d =
            classify_non_success_and_record_cooldown(502, None, &cd, "test-model", false, "unit");
        match d {
            RetryDecision::Retry { delay_ms } => assert!(
                delay_ms.is_none(),
                "plain 5xx should not request a cooldown-imposed delay"
            ),
            RetryDecision::Terminal => panic!("5xx must be retryable"),
        }
    }

    #[test]
    fn classify_4xx_except_429_is_terminal() {
        let cd = PerModelCooldown::new();
        for status in [400u16, 401, 403, 404, 422] {
            let d = classify_non_success_and_record_cooldown(
                status,
                None,
                &cd,
                "test-model",
                false,
                "unit",
            );
            assert!(
                matches!(d, RetryDecision::Terminal),
                "{status} must be terminal, got {d:?}"
            );
        }
    }

    // Trivial Debug impl used by the above assertion messages.
    impl std::fmt::Debug for RetryDecision {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                RetryDecision::Retry { delay_ms } => {
                    write!(f, "Retry {{ delay_ms: {delay_ms:?} }}")
                }
                RetryDecision::Terminal => write!(f, "Terminal"),
            }
        }
    }

    #[tokio::test]
    async fn bedrock_streaming_retries_on_429_and_delivers_body() {
        // Collapse the real 1s retry backoff to 0ms for this test; the
        // invariant being checked is "after a 429 the client retries and the
        // retry succeeds", not the absolute wait duration.
        TEST_BRIDGE_RETRY_BACKOFF_MS.with(|c| *c.borrow_mut() = Some(0));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_BRIDGE_RETRY_BACKOFF_MS.with(|c| *c.borrow_mut() = None);
            }
        }
        let _reset = Reset;

        let hits = Arc::new(AtomicU32::new(0));
        let base_url = spawn_bedrock_retry_server(hits.clone()).await;

        let messages = vec![json!({"role":"user","content":"say hi"})];
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "anthropic.claude-sonnet-4-test",
                    wire_model_name: None,
                    api_key: "dummy-key",
                    base_url: &base_url,
                    provider: "bedrock",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(32),
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream should succeed after retry");

        // Drive the stream to completion.
        let mut all = Vec::new();
        let mut s = stream;
        while let Some(chunk) = s.next().await {
            all.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&all);
        assert!(
            body.contains("\"text_delta\""),
            "stream must deliver canonical text_delta after retry; got: {body}"
        );
        assert!(
            body.contains("_inprocess_summary"),
            "stream must end with _inprocess_summary: {body}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "server must have seen 2 POSTs (the 429 + the successful retry)"
        );
    }

    /// Bedrock delivers `metadata` (carrying usage) in a SEPARATE TCP chunk
    /// AFTER `messageStop`. The streaming transport must keep draining the
    /// byte stream until EOS — it cannot exit early when `messageStop`
    /// arrives, or usage accounting silently becomes zero.
    ///
    /// Regression guard: the previous transport broke out on
    /// `accum.is_finished()` (true after `messageStop`), losing the usage
    /// frame. Symptom in practice: CLI status line shows `tokens:0 (↑0 ↓0)`
    /// even for successful Claude/Bedrock turns.
    async fn spawn_bedrock_split_meta_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind bedrock split listener");
        let addr = listener.local_addr().expect("local_addr");
        // Signal the caller as soon as we have a bound address — no sleep needed.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            // Notify the test that the server is ready before entering the accept loop.
            let _ = ready_tx.send(format!("http://{addr}"));
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    // TCP_NODELAY disables Nagle's algorithm so that each flush()
                    // produces an independent TCP segment. This guarantees the two
                    // chunked-transfer chunks reach the client in separate reads
                    // without relying on any timing / sleep.
                    let _ = socket.set_nodelay(true);
                    let mut buf = vec![0_u8; 8192];
                    let _ = socket.read(&mut buf).await.unwrap_or(0);
                    let start = build_bedrock_frame("messageStart", br#"{"role":"assistant"}"#);
                    let delta = build_bedrock_frame(
                        "contentBlockDelta",
                        br#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
                    );
                    let stop = build_bedrock_frame("messageStop", br#"{"stopReason":"end_turn"}"#);
                    let meta = build_bedrock_frame(
                        "metadata",
                        br#"{"usage":{"inputTokens":42,"outputTokens":7,"totalTokens":49}}"#,
                    );
                    // Part 1: start + delta + stop
                    let mut part1 = Vec::new();
                    part1.extend_from_slice(&start);
                    part1.extend_from_slice(&delta);
                    part1.extend_from_slice(&stop);
                    // Use chunked transfer to deliver metadata in a distinct
                    // read unit — emulates real Bedrock where metadata
                    // arrives after messageStop on the wire.
                    let header = "HTTP/1.1 200 OK\r\n\
                                  Content-Type: application/vnd.amazon.eventstream\r\n\
                                  Transfer-Encoding: chunked\r\n\
                                  Connection: close\r\n\r\n";
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket
                        .write_all(format!("{:x}\r\n", part1.len()).as_bytes())
                        .await;
                    let _ = socket.write_all(&part1).await;
                    let _ = socket.write_all(b"\r\n").await;
                    // flush() with TCP_NODELAY forces chunk1 out as a separate
                    // TCP segment before chunk2 is written — no sleep required.
                    let _ = socket.flush().await;
                    let _ = socket
                        .write_all(format!("{:x}\r\n", meta.len()).as_bytes())
                        .await;
                    let _ = socket.write_all(&meta).await;
                    let _ = socket.write_all(b"\r\n").await;
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        // Block until the server has bound and is ready to accept — deterministic,
        // no arbitrary sleep.
        ready_rx.await.expect("bedrock split meta server ready")
    }

    #[tokio::test]
    async fn bedrock_stream_drains_metadata_after_message_stop() {
        let base_url = spawn_bedrock_split_meta_server().await;
        let messages = vec![json!({"role":"user","content":"hi"})];
        let stream = call_llm_stream(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: "anthropic.claude-sonnet-4-test",
                    wire_model_name: None,
                    api_key: "dummy-key",
                    base_url: &base_url,
                    provider: "bedrock",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(32),
                temperature: None,
                has_fallback: false,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            None,
        )
        .await
        .expect("stream should succeed");

        let mut all = Vec::new();
        let mut s = stream;
        while let Some(chunk) = s.next().await {
            all.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&all);
        assert!(
            body.contains("\"type\":\"usage\""),
            "canonical usage SSE event MUST be delivered from the metadata frame \
             that arrives after messageStop; body was:\n{body}"
        );
        assert!(
            body.contains("\"input_tokens\":42"),
            "usage must carry the accounted input_tokens=42 from metadata frame; body:\n{body}"
        );
        assert!(
            body.contains("\"output_tokens\":7"),
            "usage must carry the accounted output_tokens=7 from metadata frame; body:\n{body}"
        );
    }
}
