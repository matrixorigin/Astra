//! LLM streaming call with delivery-aware retry logic and rate-limit cooldown.
//!
//! This module encapsulates the HTTP streaming call to the LLM provider, including:
//! - SSE chunk parsing and forwarding (text_delta, reasoning_delta, tool_call_start, usage)
//! - Typed idle/transport terminals that never hide a second inference request
//! - Retry with exponential backoff only for known failures (429, 5xx, connect)
//! - Per-model rate-limit cooldown tracking
//! - Degraded tool-call recovery from XML-like text content

use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};

use async_stream::stream;
use axum::body::Bytes;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::turn::bridge::sse_helpers::{render_classified_error_sse, render_sse};
use crate::turn::llm::client::{
    LLM_MAX_RETRIES, LlmCall, LlmCancel, LlmExecutionRoute, PreparedProviderRequest,
    ProviderAttemptObserver, ProviderWireRequestIdentity, apply_llm_header_overrides,
    apply_provider_auth, build_provider_request_body_with_overrides, classify_provider_send_error,
    finish_observed_provider_attempt, finish_observed_provider_error,
    finish_observed_provider_error_with_partial, llm_provider_protocol, llm_request_url,
    llm_retry_base_ms, parse_openai_sse_json_stream, provider_attempt_terminal_from_result,
    provider_uses_anthropic_messages, provider_uses_bedrock_converse, sleep_ms_or_llm_cancel,
    split_think_chunks,
};
use astra_turn_core::bridge_rate_limit_cooldown::{
    CooldownReason, PerModelCooldown, RateLimitAction, is_overload_status, is_rate_limit_status,
    parse_retry_after_ms,
};
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

fn take_bridge_retry_delay_ms(attempt: u32, provider_delay: &mut Option<u64>) -> u64 {
    provider_delay
        .take()
        .unwrap_or_else(|| bridge_retry_backoff_ms(attempt))
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

type SharedAttemptObserver = Arc<dyn ProviderAttemptObserver>;

fn bridge_stream_error(
    kind: astra_core::ErrorKind,
    message: impl Into<String>,
) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(kind, message)
}

fn bridge_cancelled_error() -> astra_core::ClassifiedError {
    bridge_stream_error(
        astra_core::ErrorKind::Cancelled,
        "LLM stream cancelled by client disconnect",
    )
}

fn bridge_delivery_unknown_error(message: impl Into<String>) -> astra_core::ClassifiedError {
    bridge_stream_error(astra_core::ErrorKind::StreamTransport, message)
}

async fn begin_observed_attempt(
    observer: Option<&SharedAttemptObserver>,
    wire: &ProviderWireRequestIdentity,
) -> Result<Option<u32>, astra_core::ClassifiedError> {
    match observer {
        Some(observer) => observer.begin_attempt(wire).await.map(Some),
        None => Ok(None),
    }
}

async fn finish_stream_success(
    observer: Option<&SharedAttemptObserver>,
    attempt_index: Option<u32>,
    result: &crate::turn::llm::client::LlmCallResult,
) -> Result<(), astra_core::ClassifiedError> {
    finish_observed_provider_attempt(
        observer.map(AsRef::as_ref),
        attempt_index,
        &provider_attempt_terminal_from_result(result),
    )
    .await
}

async fn finish_stream_error(
    observer: Option<&SharedAttemptObserver>,
    attempt_index: Option<u32>,
    error: &astra_core::ClassifiedError,
) -> Result<(), astra_core::ClassifiedError> {
    finish_observed_provider_error(observer.map(AsRef::as_ref), attempt_index, error).await
}

async fn finish_stream_error_with_partial(
    observer: Option<&SharedAttemptObserver>,
    attempt_index: Option<u32>,
    error: &astra_core::ClassifiedError,
    partial: &crate::turn::llm::client::LlmCallResult,
) -> Result<(), astra_core::ClassifiedError> {
    finish_observed_provider_error_with_partial(
        observer.map(AsRef::as_ref),
        attempt_index,
        error,
        partial,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
fn bridge_partial_result(
    provider_response_id: &Option<String>,
    full_text: &str,
    reasoning: &str,
    reasoning_signature: &str,
    tool_calls_map: &std::collections::HashMap<usize, Map<String, Value>>,
    usage: &Map<String, Value>,
    model_name: &str,
    started: std::time::Instant,
    finish_reason: Option<&str>,
) -> crate::turn::llm::client::LlmCallResult {
    let mut tool_calls = tool_calls_map
        .iter()
        .map(|(index, tool_call)| (*index, Value::Object(tool_call.clone())))
        .collect::<Vec<_>>();
    tool_calls.sort_by_key(|(index, _)| *index);
    crate::turn::llm::client::LlmCallResult {
        response_id: provider_response_id.clone(),
        full_text: full_text.to_string(),
        reasoning: reasoning.to_string(),
        reasoning_signature: reasoning_signature.to_string(),
        tool_calls: tool_calls
            .into_iter()
            .map(|(_, tool_call)| tool_call)
            .collect(),
        usage: usage.clone(),
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason: finish_reason.map(ToString::to_string),
    }
}

fn classified_stream_error_event(error: &astra_core::ClassifiedError, code: &str) -> Bytes {
    // Once a streaming response exists, delivery may already have happened.
    // The current invocation must never invite an automatic reissue. Explicit
    // HTTP/provider rejections are handled before this streaming boundary.
    render_classified_error_sse(error, code, false)
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
    /// The current physical route must stop. The caller may resolve a different
    /// route only at its model-admission boundary.
    UseFallback { reason: CooldownReason },
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
        return match action {
            RateLimitAction::WaitAndRetry { delay_ms } => RetryDecision::Retry {
                delay_ms: Some(delay_ms),
            },
            RateLimitAction::UseFallback { reason } => RetryDecision::UseFallback { reason },
            RateLimitAction::Proceed | RateLimitAction::Reject { .. } => RetryDecision::Terminal,
        };
    }
    if is_overload_status(status) {
        let action = cooldown.with(model_key, |c| c.record_529(retry_after_ms, has_fallback));
        astra_core::agent_warn!(
            "llm",
            "{log_tag} overload ({status}) on {model_key}: action={action:?}"
        );
        return match action {
            RateLimitAction::WaitAndRetry { delay_ms } => RetryDecision::Retry {
                delay_ms: Some(delay_ms),
            },
            RateLimitAction::UseFallback { reason } => RetryDecision::UseFallback { reason },
            RateLimitAction::Proceed | RateLimitAction::Reject { .. } => RetryDecision::Terminal,
        };
    }
    if status >= 500 {
        return RetryDecision::Retry { delay_ms: None };
    }
    RetryDecision::Terminal
}

const FALLBACK_REQUIRED_SOURCE: &str = "llm_fallback_required";

pub(crate) fn fallback_required_error(
    cause: astra_core::ClassifiedError,
    reason: CooldownReason,
) -> astra_core::ClassifiedError {
    cause.with_details_json(
        json!({
            "source": FALLBACK_REQUIRED_SOURCE,
            "reason": reason.as_str(),
        })
        .to_string(),
    )
}

pub(crate) fn fallback_required_reason(
    error: &astra_core::ClassifiedError,
) -> Option<CooldownReason> {
    let details = serde_json::from_str::<Value>(error.details_json.as_deref()?).ok()?;
    if details.get("source").and_then(Value::as_str) != Some(FALLBACK_REQUIRED_SOURCE) {
        return None;
    }
    match details.get("reason").and_then(Value::as_str) {
        Some("rate_limit") => Some(CooldownReason::RateLimit),
        Some("overloaded") => Some(CooldownReason::Overloaded),
        _ => None,
    }
}

/// Bedrock Converse streaming POST with the same retry + cooldown discipline
/// the OpenAI branch of [`call_llm_stream`] uses:
///
/// - HTTP 429 → `record_429` on the cooldown tracker + retry with backoff.
/// - HTTP 5xx → record via `record_529` for overload (529 / 503) or plain
///   retry otherwise, bounded by `LLM_MAX_RETRIES`.
/// - Connect failures known to precede delivery → retry with exponential backoff.
/// - Other send/stream failures → terminalize as delivery-unknown without reissue.
/// - On the first 2xx response, hand the body to
///   [`bedrock_transport::bedrock_stream_response_bytes`] and return the
///   canonical internal SSE stream.
async fn bedrock_stream_with_retry(
    client: &reqwest::Client,
    call: LlmCall<'_>,
    client_cancel: Option<Arc<CancellationToken>>,
    attempt_observer: Option<SharedAttemptObserver>,
) -> Result<
    Pin<Box<dyn futures_util::Stream<Item = Bytes> + Send + 'static>>,
    astra_core::ClassifiedError,
> {
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
    let prepared_request =
        PreparedProviderRequest::from_json(&body, llm_provider_protocol(provider))?;
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
    let mut last_kind = astra_core::ErrorKind::Unknown;
    let mut retry_delay_override = None;

    for attempt in 0..=LLM_MAX_RETRIES {
        if attempt > 0 && started.elapsed() > total_budget {
            return Err(bridge_stream_error(
                astra_core::ErrorKind::BudgetExhausted,
                format!(
                    "bedrock stream total budget exhausted ({:.0}s): {last_err}",
                    total_budget.as_secs_f64()
                ),
            ));
        }
        if attempt > 0 {
            let delay = take_bridge_retry_delay_ms(attempt, &mut retry_delay_override);
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel)).await?;
        }
        if client_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(bridge_cancelled_error());
        }

        let observed_attempt =
            begin_observed_attempt(attempt_observer.as_ref(), prepared_request.identity()).await?;
        if client_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            let error = bridge_cancelled_error();
            finish_stream_error(attempt_observer.as_ref(), observed_attempt, &error).await?;
            return Err(error);
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
        let response = match req.body(prepared_request.body()).send().await {
            Ok(r) => r,
            Err(e) => {
                let (error, retry_safe) =
                    classify_provider_send_error("bedrock converse-stream send failed", &e);
                finish_stream_error(attempt_observer.as_ref(), observed_attempt, &error).await?;
                last_err = error.message.clone();
                last_kind = error.kind;
                if retry_safe {
                    astra_core::agent_warn!(
                        "llm",
                        "bedrock connect retry: attempt={attempt} model={model_name} err={e}"
                    );
                    continue;
                }
                return Err(error);
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
                    attempt_observer,
                    observed_attempt,
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
        last_kind = if is_rate_limit_status(status) {
            astra_core::ErrorKind::RateLimit
        } else if is_overload_status(status) || status >= 500 {
            astra_core::ErrorKind::ServerError
        } else if status == 401 || status == 403 {
            astra_core::ErrorKind::Auth
        } else if status == 400 && astra_core::is_llm_context_window_error(&text) {
            astra_core::ErrorKind::ContextWindow
        } else if status == 400 {
            astra_core::ErrorKind::InvalidRequest
        } else {
            astra_core::ErrorKind::Unknown
        };
        let observed_error = bridge_stream_error(last_kind, last_err.clone());
        finish_stream_error(attempt_observer.as_ref(), observed_attempt, &observed_error).await?;

        match classify_non_success_and_record_cooldown(
            status,
            retry_after_ms,
            cooldown,
            model_key,
            has_fallback,
            "bedrock",
        ) {
            RetryDecision::Retry { delay_ms } => {
                retry_delay_override = delay_ms;
                continue;
            }
            RetryDecision::UseFallback { reason } => {
                return Err(fallback_required_error(observed_error, reason));
            }
            RetryDecision::Terminal => return Err(observed_error),
        }
    }

    Err(bridge_stream_error(
        last_kind,
        format!("bedrock stream exhausted {LLM_MAX_RETRIES} retries: {last_err}"),
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
    attempt_observer: Option<SharedAttemptObserver>,
) -> Result<
    Pin<Box<dyn futures_util::Stream<Item = Bytes> + Send + 'static>>,
    astra_core::ClassifiedError,
> {
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
    let prepared_request =
        PreparedProviderRequest::from_json(&body, llm_provider_protocol(provider))?;
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
    let mut last_kind = astra_core::ErrorKind::Unknown;
    let mut retry_delay_override = None;

    for attempt in 0..=LLM_MAX_RETRIES {
        if attempt > 0 && started.elapsed() > total_budget {
            return Err(bridge_stream_error(
                astra_core::ErrorKind::BudgetExhausted,
                format!(
                    "anthropic stream total budget exhausted ({:.0}s): {last_err}",
                    total_budget.as_secs_f64()
                ),
            ));
        }
        if attempt > 0 {
            let delay = take_bridge_retry_delay_ms(attempt, &mut retry_delay_override);
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel)).await?;
        }
        if client_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(bridge_cancelled_error());
        }

        let observed_attempt =
            begin_observed_attempt(attempt_observer.as_ref(), prepared_request.identity()).await?;
        if client_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            let error = bridge_cancelled_error();
            finish_stream_error(attempt_observer.as_ref(), observed_attempt, &error).await?;
            return Err(error);
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
        let response = match req.body(prepared_request.body()).send().await {
            Ok(r) => r,
            Err(e) => {
                let (error, retry_safe) =
                    classify_provider_send_error("anthropic stream send failed", &e);
                finish_stream_error(attempt_observer.as_ref(), observed_attempt, &error).await?;
                last_err = error.message.clone();
                last_kind = error.kind;
                if retry_safe {
                    continue;
                }
                return Err(error);
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            cooldown.with(model_key, |c| c.record_success());
            let byte_stream = response.bytes_stream();
            let idle_pre = crate::turn::llm::client::stream_idle_timeout();
            let idle_post = crate::turn::llm::client::stream_idle_timeout_after_progress();
            let cc = client_cancel.clone();
            let model_name_for_summary = model_name.to_string();
            let attempt_observer_for_stream = attempt_observer.clone();
            let out = stream! {
                let mut streaming_attempt = observed_attempt;
                let sse = parse_openai_sse_json_stream(byte_stream);
                tokio::pin!(sse);
                // Accumulate state for the final `_inprocess_summary` event.
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut reasoning_signature = String::new();
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();
                let mut provider_response_id: Option<String> = None;
                let mut made_progress = false;
                let mut had_terminal_error = false;
                let mut saw_terminal = false;

                loop {
                    let idle = if made_progress { idle_post } else { idle_pre };
                    tokio::select! {
                        biased;
                        _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                            astra_core::agent_warn!(
                                "llm",
                                "anthropic SSE cancelled (client disconnect)"
                            );
                            let error = bridge_delivery_unknown_error(
                                "Anthropic stream delivery became unknown after client disconnect",
                            );
                            let partial = bridge_partial_result(
                                &provider_response_id,
                                &full_text,
                                &reasoning,
                                &reasoning_signature,
                                &tool_calls_map,
                                &usage,
                                &model_name_for_summary,
                                started,
                                None,
                            );
                            if let Err(ledger_error) = finish_stream_error_with_partial(
                                attempt_observer_for_stream.as_ref(),
                                streaming_attempt.take(),
                                &error,
                                &partial,
                            ).await {
                                yield classified_stream_error_event(
                                    &ledger_error,
                                    "inference_ledger",
                                );
                                return;
                            }
                            yield classified_stream_error_event(
                                &error,
                                "client_disconnect",
                            );
                            return;
                        }
                        tick = tokio::time::timeout(idle, sse.next()) => {
                            let Ok(next) = tick else {
                                astra_core::agent_warn!(
                                    "llm",
                                    "anthropic SSE idle after {}ms (made_progress={})",
                                    idle.as_millis(),
                                    made_progress,
                                );
                                let stream_error = bridge_stream_error(
                                    astra_core::ErrorKind::StreamIdle,
                                    format!("anthropic SSE idle after {}ms", idle.as_millis()),
                                );
                                let partial = bridge_partial_result(
                                    &provider_response_id,
                                    &full_text,
                                    &reasoning,
                                    &reasoning_signature,
                                    &tool_calls_map,
                                    &usage,
                                    &model_name_for_summary,
                                    started,
                                    None,
                                );
                                if let Err(ledger_error) = finish_stream_error_with_partial(
                                    attempt_observer_for_stream.as_ref(),
                                    streaming_attempt.take(),
                                    &stream_error,
                                    &partial,
                                ).await {
                                    yield classified_stream_error_event(
                                        &ledger_error,
                                        "inference_ledger",
                                    );
                                    return;
                                }
                                had_terminal_error = true;
                                yield classified_stream_error_event(
                                    &stream_error,
                                    "stream_idle",
                                );
                                break;
                            };
                            let Some(chunk) = next else { break };
                            let chunk = match chunk {
                                Ok(crate::turn::llm::client::ParsedSseEvent::Done) => {
                                    saw_terminal = true;
                                    break;
                                }
                                Ok(crate::turn::llm::client::ParsedSseEvent::Data(v)) => v,
                                Err(e) => {
                                    let stream_error = bridge_delivery_unknown_error(format!(
                                        "anthropic SSE transport error: {e}"
                                    ));
                                    let partial = bridge_partial_result(
                                        &provider_response_id,
                                        &full_text,
                                        &reasoning,
                                        &reasoning_signature,
                                        &tool_calls_map,
                                        &usage,
                                        &model_name_for_summary,
                                        started,
                                        None,
                                    );
                                    if let Err(ledger_error) = finish_stream_error_with_partial(
                                        attempt_observer_for_stream.as_ref(),
                                        streaming_attempt.take(),
                                        &stream_error,
                                        &partial,
                                    ).await {
                                        yield classified_stream_error_event(
                                            &ledger_error,
                                            "inference_ledger",
                                        );
                                        return;
                                    }
                                    had_terminal_error = true;
                                    yield classified_stream_error_event(
                                        &stream_error,
                                        "stream_transport",
                                    );
                                    break;
                                }
                            };
                            if provider_response_id.is_none() {
                                if let Some(response_id) = chunk
                                    .pointer("/message/id")
                                    .or_else(|| chunk.get("id"))
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                                {
                                    provider_response_id = Some(response_id.clone());
                                    yield render_sse(&json!({
                                        "type": "_provider_response_identity",
                                        "provider_response_id": response_id,
                                    }));
                                }
                            }
                            let message_stopped =
                                chunk.get("type").and_then(Value::as_str) == Some("message_stop");
                            if message_stopped {
                                saw_terminal = true;
                            }
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
                            if message_stopped {
                                break;
                            }
                        }
                    }
                }

                if had_terminal_error {
                    return;
                }
                if !saw_terminal {
                    let error = bridge_delivery_unknown_error(
                        "Anthropic SSE ended without message_stop",
                    );
                    let partial = bridge_partial_result(
                        &provider_response_id,
                        &full_text,
                        &reasoning,
                        &reasoning_signature,
                        &tool_calls_map,
                        &usage,
                        &model_name_for_summary,
                        started,
                        None,
                    );
                    if let Err(ledger_error) = finish_stream_error_with_partial(
                        attempt_observer_for_stream.as_ref(),
                        streaming_attempt.take(),
                        &error,
                        &partial,
                    ).await {
                        yield classified_stream_error_event(
                            &ledger_error,
                            "inference_ledger",
                        );
                        return;
                    }
                    yield classified_stream_error_event(
                        &error,
                        "stream_transport",
                    );
                    return;
                }

                let tool_calls: Vec<Value> = {
                    let mut entries: Vec<_> = tool_calls_map.drain().collect();
                    entries.sort_by_key(|(i, _)| *i);
                    entries.into_iter().map(|(_, m)| Value::Object(m)).collect()
                };
                let result = crate::turn::llm::client::LlmCallResult {
                    response_id: provider_response_id,
                    full_text,
                    reasoning,
                    reasoning_signature,
                    tool_calls,
                    usage,
                    model_used: model_name_for_summary,
                    duration_ms: started.elapsed().as_millis() as u64,
                    finish_reason: None,
                };
                if let Err(ledger_error) = finish_stream_success(
                    attempt_observer_for_stream.as_ref(),
                    streaming_attempt.take(),
                    &result,
                ).await {
                    yield classified_stream_error_event(
                        &ledger_error,
                        "inference_ledger",
                    );
                    return;
                }
                yield render_sse(&json!({
                    "type": "_inprocess_summary",
                    "full_text": result.full_text,
                    "reasoning": result.reasoning,
                    "reasoning_signature": result.reasoning_signature,
                    "tool_calls": result.tool_calls,
                    "usage": Value::Object(result.usage),
                    "provider_response_id": result.response_id,
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
        last_kind = if is_rate_limit_status(status) {
            astra_core::ErrorKind::RateLimit
        } else if is_overload_status(status) || status >= 500 {
            astra_core::ErrorKind::ServerError
        } else if status == 401 || status == 403 {
            astra_core::ErrorKind::Auth
        } else if status == 400 && astra_core::is_llm_context_window_error(&text) {
            astra_core::ErrorKind::ContextWindow
        } else if status == 400 {
            astra_core::ErrorKind::InvalidRequest
        } else {
            astra_core::ErrorKind::Unknown
        };
        let observed_error = bridge_stream_error(last_kind, last_err.clone());
        finish_stream_error(attempt_observer.as_ref(), observed_attempt, &observed_error).await?;

        match classify_non_success_and_record_cooldown(
            status,
            retry_after_ms,
            cooldown,
            model_key,
            has_fallback,
            "anthropic",
        ) {
            RetryDecision::Retry { delay_ms } => {
                retry_delay_override = delay_ms;
                continue;
            }
            RetryDecision::UseFallback { reason } => {
                return Err(fallback_required_error(observed_error, reason));
            }
            RetryDecision::Terminal => return Err(observed_error),
        }
    }
    Err(bridge_stream_error(
        last_kind,
        format!("anthropic stream exhausted {LLM_MAX_RETRIES} retries: {last_err}"),
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
                let final_usage = crate::turn::token_usage::TokenUsage {
                    output_tokens: out_toks,
                    ..crate::turn::token_usage::TokenUsage::from_partial_json_map(usage)
                };
                *usage = final_usage.to_json_map();
                out.push(render_sse(&json!({
                    "type": "usage",
                    "input_tokens": final_usage.input_tokens,
                    "cached_input_tokens": final_usage.cached_input_tokens,
                    "cache_creation_tokens": final_usage.cache_creation_tokens,
                    "output_tokens": final_usage.output_tokens,
                    "total_tokens": final_usage.total_tokens(),
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
/// Per-chunk idle and transport failures preserve partial evidence, end the
/// physical attempt as `delivery_unknown`, and never issue a hidden recovery
/// request.
///
/// Retries up to LLM_MAX_RETRIES times on transient errors (429/5xx/network)
/// with exponential backoff.
///
/// **Note**: Caller must check rate-limit cooldown state and handle fallback model
/// resolution BEFORE calling this function. This function only handles retries for
/// transient errors within a single model.
pub(crate) async fn call_llm_stream_with_attempt_observer(
    call: LlmCall<'_>,
    client_cancel: Option<Arc<CancellationToken>>,
    attempt_observer: Option<SharedAttemptObserver>,
) -> Result<
    Pin<Box<dyn futures_util::Stream<Item = Bytes> + Send + 'static>>,
    astra_core::ClassifiedError,
> {
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
    let client = client_builder.build().map_err(|error| {
        bridge_stream_error(
            astra_core::ErrorKind::Network,
            format!("failed to build LLM streaming client: {error}"),
        )
    })?;

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
            attempt_observer,
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
            attempt_observer,
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
    let prepared_request =
        PreparedProviderRequest::from_json(&body, llm_provider_protocol(provider))?;

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        upstream_name,
        true,
    );
    let req_bytes = prepared_request.identity().provider_wire_bytes;

    // Total budget guard: abort if retries + cooldown delays exceed the budget.
    let total_budget = crate::turn::llm::client::llm_total_budget();
    let started = std::time::Instant::now();

    // Retry loop for known failures (429, 5xx, and connect-before-delivery).
    let mut last_err = String::new();
    let mut last_kind = astra_core::ErrorKind::Unknown;
    let mut retry_delay_override = None;
    for attempt in 0..=LLM_MAX_RETRIES {
        // Check total budget before each attempt
        if attempt > 0 && started.elapsed() > total_budget {
            return Err(bridge_stream_error(
                astra_core::ErrorKind::BudgetExhausted,
                format!(
                    "LLM total budget exhausted ({:.0}s): {last_err}",
                    total_budget.as_secs_f64()
                ),
            ));
        }

        if attempt > 0 {
            let delay = take_bridge_retry_delay_ms(attempt, &mut retry_delay_override);
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel)).await?;
        }
        if client_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(bridge_cancelled_error());
        }

        let observed_attempt =
            begin_observed_attempt(attempt_observer.as_ref(), prepared_request.identity()).await?;
        if client_cancel
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            let error = bridge_cancelled_error();
            finish_stream_error(attempt_observer.as_ref(), observed_attempt, &error).await?;
            return Err(error);
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
        let response = match req.body(prepared_request.body()).send().await {
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
                let (error, retry_safe) = classify_provider_send_error("LLM request failed", &e);
                finish_stream_error(attempt_observer.as_ref(), observed_attempt, &error).await?;
                last_err = error.message.clone();
                last_kind = error.kind;
                if retry_safe {
                    continue;
                }
                return Err(error);
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Success — record to cooldown tracker and return the stream
            cooldown.with(model_key, |c| c.record_success());
            let byte_stream = response.bytes_stream();
            let model_name = model_name.to_string();
            let idle_pre = crate::turn::llm::client::stream_idle_timeout();
            let idle_post = crate::turn::llm::client::stream_idle_timeout_after_progress();
            let attempt_observer_for_stream = attempt_observer.clone();

            let out = stream! {
                let mut streaming_attempt = observed_attempt;
                let cc = client_cancel.clone();
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut in_think = false;
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();
                let mut provider_response_id: Option<String> = None;
                let mut made_progress = false;
                let mut had_terminal_error = false;
                let mut saw_terminal = false;
                let mut finish_reason: Option<String> = None;

                let sse = crate::turn::llm::client::parse_openai_sse_json_stream(byte_stream);
                tokio::pin!(sse);

                loop {
                    let ordinary_idle = if made_progress { idle_post } else { idle_pre };
                    let idle = if saw_terminal {
                        crate::turn::llm::client::stream_terminal_drain_timeout(ordinary_idle)
                    } else {
                        ordinary_idle
                    };
                    tokio::select! {
                        biased;
                        _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                            if saw_terminal {
                                break;
                            }
                            astra_core::agent_warn!(
                                "llm",
                                "in-process LLM SSE cancelled (client disconnect)"
                            );
                            let error = bridge_delivery_unknown_error(
                                "LLM stream delivery became unknown after client disconnect",
                            );
                            let partial = bridge_partial_result(
                                &provider_response_id,
                                &full_text,
                                &reasoning,
                                "",
                                &tool_calls_map,
                                &usage,
                                &model_name,
                                started,
                                finish_reason.as_deref(),
                            );
                            if let Err(ledger_error) = finish_stream_error_with_partial(
                                attempt_observer_for_stream.as_ref(),
                                streaming_attempt.take(),
                                &error,
                                &partial,
                            ).await {
                                yield classified_stream_error_event(
                                    &ledger_error,
                                    "inference_ledger",
                                );
                                return;
                            }
                            yield classified_stream_error_event(
                                &error,
                                "client_disconnect",
                            );
                            return;
                        }
                        tick = tokio::time::timeout(idle, sse.next()) => {
                            let next = tick;
                            let chunk = match next {
                                Ok(c) => c,
                                Err(_) => {
                                    if saw_terminal {
                                        break;
                                    }
                                    astra_core::agent_warn!(
                                        "llm",
                                        "in-process stream idle after {}ms (made_progress={})",
                                        idle.as_millis(),
                                        made_progress
                                    );
                                    let stream_error = bridge_stream_error(
                                        astra_core::ErrorKind::StreamIdle,
                                        format!("LLM stream idle after {}ms", idle.as_millis()),
                                    );
                                    let partial = bridge_partial_result(
                                        &provider_response_id,
                                        &full_text,
                                        &reasoning,
                                        "",
                                        &tool_calls_map,
                                        &usage,
                                        &model_name,
                                        started,
                                        finish_reason.as_deref(),
                                    );
                                    if let Err(ledger_error) = finish_stream_error_with_partial(
                                        attempt_observer_for_stream.as_ref(),
                                        streaming_attempt.take(),
                                        &stream_error,
                                        &partial,
                                    ).await {
                                        yield classified_stream_error_event(
                                            &ledger_error,
                                            "inference_ledger",
                                        );
                                        return;
                                    }
                                    had_terminal_error = true;
                                    yield classified_stream_error_event(
                                        &stream_error,
                                        "stream_idle",
                                    );
                                    break;
                                }
                            };
                            let Some(item) = chunk else { break };
                            let chunk = match item {
                                Ok(crate::turn::llm::client::ParsedSseEvent::Done) => {
                                    saw_terminal = true;
                                    break;
                                }
                                Ok(crate::turn::llm::client::ParsedSseEvent::Data(v)) => v,
                                Err(e) => {
                                    if saw_terminal {
                                        break;
                                    }
                                    let stream_error = bridge_delivery_unknown_error(format!(
                                        "LLM stream transport error: {e}"
                                    ));
                                    let partial = bridge_partial_result(
                                        &provider_response_id,
                                        &full_text,
                                        &reasoning,
                                        "",
                                        &tool_calls_map,
                                        &usage,
                                        &model_name,
                                        started,
                                        finish_reason.as_deref(),
                                    );
                                    if let Err(ledger_error) = finish_stream_error_with_partial(
                                        attempt_observer_for_stream.as_ref(),
                                        streaming_attempt.take(),
                                        &stream_error,
                                        &partial,
                                    ).await {
                                        yield classified_stream_error_event(
                                            &ledger_error,
                                            "inference_ledger",
                                        );
                                        return;
                                    }
                                    astra_core::agent_warn!(
                                        "llm",
                                        "in-process stream transport error: {e}"
                                    );
                                    had_terminal_error = true;
                                    yield classified_stream_error_event(
                                        &stream_error,
                                        "stream_transport",
                                    );
                                    break;
                                }
                            };
                            // Some providers attach usage to a chunk that also contains choices,
                            // so parse usage first on every chunk. Streaming endpoints are
                            // OpenAI-compatible; Bedrock is intercepted earlier.
                            made_progress = true;
                            if provider_response_id.is_none() {
                                if let Some(response_id) = chunk
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                                {
                                    provider_response_id = Some(response_id.clone());
                                    yield render_sse(&json!({
                                        "type": "_provider_response_identity",
                                        "provider_response_id": response_id,
                                    }));
                                }
                            }
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
                            if saw_terminal && !usage.is_empty() {
                                break;
                            }

                            let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
                                continue;
                            };

                            let Some(choice) = choices.first() else {
                                continue;
                            };
                            if let Some(reason) = choice
                                .get("finish_reason")
                                .and_then(Value::as_str)
                            {
                                finish_reason = Some(reason.to_string());
                                saw_terminal = true;
                            }
                            let Some(delta) = choice.get("delta")
                                .and_then(Value::as_object)
                            else {
                                if saw_terminal && !usage.is_empty() {
                                    break;
                                }
                                continue;
                            };

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
                                            } else if let Some(bad_name) = func
                                                .get("name")
                                                .and_then(Value::as_str)
                                                .filter(|name| !name.is_empty())
                                            {
                                                astra_core::agent_warn!(
                                                    "llm",
                                                    "dropped malformed tool_call with invalid name: {bad_name:?}"
                                                );
                                            }
                                        }
                                    }
                                }
                            if saw_terminal && !usage.is_empty() {
                                break;
                            }
                        }
                    }
                }

                if had_terminal_error {
                    return;
                }
                if !saw_terminal {
                    let error = bridge_delivery_unknown_error(
                        "LLM SSE ended without a terminal marker",
                    );
                    let partial = bridge_partial_result(
                        &provider_response_id,
                        &full_text,
                        &reasoning,
                        "",
                        &tool_calls_map,
                        &usage,
                        &model_name,
                        started,
                        finish_reason.as_deref(),
                    );
                    if let Err(ledger_error) = finish_stream_error_with_partial(
                        attempt_observer_for_stream.as_ref(),
                        streaming_attempt.take(),
                        &error,
                        &partial,
                    ).await {
                        yield classified_stream_error_event(
                            &ledger_error,
                            "inference_ledger",
                        );
                        return;
                    }
                    yield classified_stream_error_event(
                        &error,
                        "stream_transport",
                    );
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

                let result = crate::turn::llm::client::LlmCallResult {
                    response_id: provider_response_id,
                    full_text,
                    reasoning,
                    reasoning_signature: String::new(),
                    tool_calls,
                    usage,
                    model_used: model_name,
                    duration_ms: started.elapsed().as_millis() as u64,
                    finish_reason,
                };
                if let Err(ledger_error) = finish_stream_success(
                    attempt_observer_for_stream.as_ref(),
                    streaming_attempt.take(),
                    &result,
                ).await {
                    yield classified_stream_error_event(
                        &ledger_error,
                        "inference_ledger",
                    );
                    return;
                }
                yield render_sse(&json!({
                    "type": "_inprocess_summary",
                    "full_text": result.full_text,
                    "reasoning": result.reasoning,
                    "tool_calls": result.tool_calls,
                    "usage": result.usage,
                    "model_used": result.model_used,
                    "provider_response_id": result.response_id,
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
        last_kind = if is_rate_limit_status(status) {
            astra_core::ErrorKind::RateLimit
        } else if is_overload_status(status) || status >= 500 {
            astra_core::ErrorKind::ServerError
        } else if status == 401 || status == 403 {
            astra_core::ErrorKind::Auth
        } else if status == 400 && astra_core::is_llm_context_window_error(&text) {
            astra_core::ErrorKind::ContextWindow
        } else if status == 400 {
            astra_core::ErrorKind::InvalidRequest
        } else {
            astra_core::ErrorKind::Unknown
        };
        let observed_error = bridge_stream_error(last_kind, last_err.clone());
        finish_stream_error(attempt_observer.as_ref(), observed_attempt, &observed_error).await?;

        match classify_non_success_and_record_cooldown(
            status,
            retry_after_ms,
            cooldown,
            model_key,
            has_fallback,
            "openai-stream",
        ) {
            RetryDecision::Retry { delay_ms } => {
                retry_delay_override = delay_ms;
                continue;
            }
            RetryDecision::UseFallback { reason } => {
                return Err(fallback_required_error(observed_error, reason));
            }
            // 4xx (except 429) is not retryable — fail immediately.
            // Context-window errors are detected by content at the call site
            // (bridge_inprocess forward()), not here.
            RetryDecision::Terminal => return Err(observed_error),
        }
    }

    // All retries exhausted
    Err(bridge_stream_error(
        last_kind,
        format!("{last_err} (after {} retries)", LLM_MAX_RETRIES),
    ))
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

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RecordedAttemptEvent {
        Began(u32),
        Finished(u32, astra_services::InferenceInvocationTerminal),
    }

    #[derive(Default)]
    struct RecordingAttemptObserver {
        next_attempt: AtomicU32,
        events: Mutex<Vec<RecordedAttemptEvent>>,
    }

    impl RecordingAttemptObserver {
        fn events(&self) -> Vec<RecordedAttemptEvent> {
            self.events.lock().expect("attempt events").clone()
        }
    }

    #[async_trait::async_trait]
    impl ProviderAttemptObserver for RecordingAttemptObserver {
        async fn begin_attempt(
            &self,
            _wire: &ProviderWireRequestIdentity,
        ) -> Result<u32, astra_core::ClassifiedError> {
            let attempt = self.next_attempt.fetch_add(1, Ordering::AcqRel);
            self.events
                .lock()
                .expect("attempt events")
                .push(RecordedAttemptEvent::Began(attempt));
            Ok(attempt)
        }

        async fn finish_attempt(
            &self,
            attempt_index: u32,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            self.events
                .lock()
                .expect("attempt events")
                .push(RecordedAttemptEvent::Finished(
                    attempt_index,
                    terminal.clone(),
                ));
            Ok(())
        }
    }

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

        let stream = call_llm_stream_with_attempt_observer(
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

        let stream = call_llm_stream_with_attempt_observer(
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

    #[tokio::test]
    async fn cancelled_stream_terminalizes_the_admitted_physical_attempt() {
        async fn handler() -> Response {
            let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(pending))
                .expect("response")
        }

        let app = Router::new().route("/chat/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });

        let base = format!("http://{addr}");
        let cancel = Arc::new(CancellationToken::new());
        let observer = Arc::new(RecordingAttemptObserver::default());
        let stream = call_llm_stream_with_attempt_observer(
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
            Some(cancel.clone()),
            Some(observer.clone()),
        )
        .await
        .expect("stream admitted");

        assert_eq!(observer.events(), vec![RecordedAttemptEvent::Began(0)]);
        cancel.cancel();
        let body = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            stream
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .collect::<String>()
        })
        .await
        .expect("cancelled stream must settle");

        assert!(body.contains("\"code\":\"client_disconnect\""), "{body}");
        assert!(!body.contains("_inprocess_summary"), "{body}");
        let events = observer.events();
        assert_eq!(
            events.len(),
            2,
            "cancel must close the open attempt: {events:?}"
        );
        assert!(matches!(
            &events[1],
            RecordedAttemptEvent::Finished(
                0,
                astra_services::InferenceInvocationTerminal {
                    status: astra_services::InferenceTerminalStatus::DeliveryUnknown,
                    error_kind: Some(kind),
                    ..
                }
            ) if kind == "stream_transport"
        ));
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

        let observer = Arc::new(RecordingAttemptObserver::default());
        let stream = call_llm_stream_with_attempt_observer(
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
            Some(observer.clone()),
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
        let mut summary_events = Vec::<Value>::new();
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
                Some("_inprocess_summary") => summary_events.push(v),
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
        let summary_usage = summary_events
            .last()
            .and_then(|summary| summary.get("usage"))
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            summary_usage.get("total_tokens").and_then(Value::as_u64),
            Some(61),
            "the persisted summary must retain the canonical invariant \
             total = fresh + cached + cache_creation + output — got {summary_usage}",
        );
        assert!(raw.contains("\"provider_response_id\":\"msg_1\""), "{raw}");
        let events = observer.events();
        assert_eq!(
            events.len(),
            2,
            "one physical request must have one terminal: {events:?}"
        );
        assert_eq!(events[0], RecordedAttemptEvent::Began(0));
        assert!(matches!(
            &events[1],
            RecordedAttemptEvent::Finished(
                0,
                astra_services::InferenceInvocationTerminal {
                    status: astra_services::InferenceTerminalStatus::Succeeded,
                    usage: astra_services::InferenceUsage {
                        input_tokens: 42,
                        output_tokens: 7,
                        cache_read_tokens: 10,
                        cache_creation_tokens: 2,
                    },
                    provider_response_id: Some(response_id),
                    ..
                }
            ) if response_id == "msg_1"
        ));
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

        let stream = call_llm_stream_with_attempt_observer(
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

        let stream = call_llm_stream_with_attempt_observer(
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
    struct TransportRequestHits {
        stream_hits: Arc<AtomicU32>,
        nonstream_hits: Arc<AtomicU32>,
    }

    async fn spawn_raw_partial_transport_server(
        hits: TransportRequestHits,
        stream_content: &'static str,
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
                            json!({
                                "id": "chatcmpl-partial-7",
                                "choices":[{"delta":{"content":stream_content}}],
                                "usage": {
                                    "prompt_tokens": 1100,
                                    "completion_tokens": 50,
                                    "prompt_tokens_details": {
                                        "cached_tokens": 800,
                                        "cache_creation_input_tokens": 100
                                    }
                                }
                            })
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
                        hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                        let fallback_body =
                            r#"{"choices":[{"message":{"content":"unexpected second request"}}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fallback_body}",
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

    async fn spawn_raw_anthropic_partial_transport_server(hits: TransportRequestHits) -> String {
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
                            "event: message_start\ndata: {}\n\n\
                             event: content_block_delta\ndata: {}\n\n\
                             event: message_delta\ndata: {}\n\n",
                            json!({
                                "type": "message_start",
                                "message": {
                                    "id": "msg_partial_7",
                                    "usage": {
                                        "input_tokens": 200,
                                        "cache_read_input_tokens": 800,
                                        "cache_creation_input_tokens": 100
                                    }
                                }
                            }),
                            json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {"type": "text_delta", "text": "partial"},
                            }),
                            json!({
                                "type": "message_delta",
                                "delta": {},
                                "usage": {"output_tokens": 50}
                            }),
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
                        hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                        let fallback_body =
                            r#"{"content":[{"type":"text","text":"unexpected second request"}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fallback_body}",
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
    async fn openai_stream_transport_error_preserves_partial_without_reissuing() {
        let hits = TransportRequestHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            nonstream_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(hits.clone(), "partial").await;
        let observer = Arc::new(RecordingAttemptObserver::default());
        let stream = call_llm_stream_with_attempt_observer(
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
            Some(observer.clone()),
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
            "partial provider output must remain visible: {body}"
        );
        assert!(
            body.contains("\"code\":\"stream_transport\"")
                && body.contains("\"error_kind\":\"stream_transport\"")
                && body.contains("\"retryable\":false"),
            "uncertain delivery must end in a structured error: {body}"
        );
        assert!(!body.contains("_inprocess_summary"), "{body}");
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.nonstream_hits.load(Ordering::SeqCst), 0);
        let events = observer.events();
        assert_eq!(
            events.len(),
            2,
            "uncertain delivery must close exactly one physical attempt: {events:?}"
        );
        assert_eq!(events[0], RecordedAttemptEvent::Began(0));
        let RecordedAttemptEvent::Finished(0, terminal) = &events[1] else {
            panic!("expected one physical delivery-unknown terminal: {events:?}");
        };
        assert_eq!(
            terminal.status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            Some("chatcmpl-partial-7")
        );
        assert_eq!(
            terminal.usage,
            astra_services::InferenceUsage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
            }
        );
    }

    #[tokio::test]
    async fn anthropic_stream_transport_error_preserves_partial_without_reissuing() {
        let hits = TransportRequestHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            nonstream_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_anthropic_partial_transport_server(hits.clone()).await;
        let observer = Arc::new(RecordingAttemptObserver::default());
        let stream = call_llm_stream_with_attempt_observer(
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
            Some(observer.clone()),
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
            "partial anthropic output must remain visible: {body}"
        );
        assert!(
            body.contains("\"code\":\"stream_transport\"")
                && body.contains("\"error_kind\":\"stream_transport\"")
                && body.contains("\"retryable\":false"),
            "uncertain anthropic delivery must end in a structured error: {body}"
        );
        assert!(!body.contains("_inprocess_summary"), "{body}");
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.nonstream_hits.load(Ordering::SeqCst), 0);
        let events = observer.events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0], RecordedAttemptEvent::Began(0));
        let RecordedAttemptEvent::Finished(0, terminal) = &events[1] else {
            panic!("expected one physical delivery-unknown terminal: {events:?}");
        };
        assert_eq!(
            terminal.status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            Some("msg_partial_7")
        );
        assert_eq!(
            terminal.usage,
            astra_services::InferenceUsage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
            }
        );
    }

    #[tokio::test]
    async fn stream_transport_preserves_preceding_inline_reasoning_without_reissuing() {
        let hits = TransportRequestHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            nonstream_hits: Arc::new(AtomicU32::new(0)),
        };
        let base =
            spawn_raw_partial_transport_server(hits.clone(), "<think>hidden reasoning").await;
        let stream = call_llm_stream_with_attempt_observer(
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
            "inline think content should be routed before the terminal error: {body}"
        );
        assert!(
            body.contains("\"code\":\"stream_transport\""),
            "uncertain delivery should surface a structured error: {body}"
        );
        assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(hits.nonstream_hits.load(Ordering::SeqCst), 0);
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
            RetryDecision::UseFallback { .. } => {
                panic!("fallback is disabled for this classification")
            }
            RetryDecision::Terminal => panic!("429 must be retryable"),
        }
    }

    #[test]
    fn provider_retry_delay_replaces_generic_backoff_instead_of_accumulating() {
        TEST_BRIDGE_RETRY_BACKOFF_MS.with(|cell| *cell.borrow_mut() = Some(1_000));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_BRIDGE_RETRY_BACKOFF_MS.with(|cell| *cell.borrow_mut() = None);
            }
        }
        let _reset = Reset;

        let mut provider_delay = Some(2_500);
        assert_eq!(
            take_bridge_retry_delay_ms(1, &mut provider_delay),
            2_500,
            "one retry must consume the provider delay, not provider plus generic backoff"
        );
        assert_eq!(provider_delay, None);
        assert_eq!(
            take_bridge_retry_delay_ms(2, &mut provider_delay),
            1_000,
            "generic backoff applies only when the provider supplied no delay"
        );
    }

    #[test]
    fn classify_529_records_overload_and_returns_retry() {
        let cd = PerModelCooldown::new();
        let d =
            classify_non_success_and_record_cooldown(529, None, &cd, "test-model", false, "unit");
        assert!(matches!(d, RetryDecision::Retry { .. }));
    }

    #[test]
    fn configured_fallback_is_a_route_switch_signal_not_a_same_model_retry() {
        let cooldown = PerModelCooldown::new();
        let mut decision = RetryDecision::Terminal;
        for _ in 0..3 {
            decision = classify_non_success_and_record_cooldown(
                429,
                Some(0),
                &cooldown,
                "primary-model",
                true,
                "unit",
            );
        }
        assert!(matches!(
            decision,
            RetryDecision::UseFallback {
                reason: CooldownReason::RateLimit
            }
        ));
        let error = fallback_required_error(
            bridge_stream_error(
                astra_core::ErrorKind::RateLimit,
                "provider rejected request",
            ),
            CooldownReason::RateLimit,
        );
        assert_eq!(
            fallback_required_reason(&error),
            Some(CooldownReason::RateLimit)
        );
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
            RetryDecision::UseFallback { .. } => {
                panic!("fallback is disabled for this classification")
            }
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
                RetryDecision::UseFallback { reason } => {
                    write!(f, "UseFallback {{ reason: {reason:?} }}")
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
        let observer = Arc::new(RecordingAttemptObserver::default());
        let stream = call_llm_stream_with_attempt_observer(
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
            Some(observer.clone()),
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
        let events = observer.events();
        assert_eq!(
            events.len(),
            4,
            "every physical request must terminate: {events:?}"
        );
        assert_eq!(events[0], RecordedAttemptEvent::Began(0));
        assert!(matches!(
            &events[1],
            RecordedAttemptEvent::Finished(
                0,
                astra_services::InferenceInvocationTerminal {
                    status: astra_services::InferenceTerminalStatus::Failed,
                    error_kind: Some(kind),
                    ..
                }
            ) if kind == "rate_limit"
        ));
        assert_eq!(events[2], RecordedAttemptEvent::Began(1));
        assert!(matches!(
            &events[3],
            RecordedAttemptEvent::Finished(
                1,
                astra_services::InferenceInvocationTerminal {
                    status: astra_services::InferenceTerminalStatus::Succeeded,
                    ..
                }
            )
        ));
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

    async fn spawn_bedrock_stop_without_metadata_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind bedrock truncated-tail listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await;
            let start = build_bedrock_frame("messageStart", br#"{"role":"assistant"}"#);
            let delta = build_bedrock_frame(
                "contentBlockDelta",
                br#"{"contentBlockIndex":0,"delta":{"text":"partial"}}"#,
            );
            let stop = build_bedrock_frame("messageStop", br#"{"stopReason":"end_turn"}"#);
            let mut body = Vec::new();
            body.extend_from_slice(&start);
            body.extend_from_slice(&delta);
            body.extend_from_slice(&stop);
            let header = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/vnd.amazon.eventstream\r\n\
                 x-amzn-requestid: bedrock-tail-7\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.expect("header");
            socket.write_all(&body).await.expect("body");
            socket.shutdown().await.expect("shutdown");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn bedrock_stream_drains_metadata_after_message_stop() {
        let base_url = spawn_bedrock_split_meta_server().await;
        let messages = vec![json!({"role":"user","content":"hi"})];
        let stream = call_llm_stream_with_attempt_observer(
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

    #[tokio::test]
    async fn bedrock_message_stop_without_metadata_is_partial_delivery_not_success() {
        let base_url = spawn_bedrock_stop_without_metadata_server().await;
        let messages = vec![json!({"role":"user","content":"hi"})];
        let observer = Arc::new(RecordingAttemptObserver::default());
        let stream = call_llm_stream_with_attempt_observer(
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
            Some(observer.clone()),
        )
        .await
        .expect("HTTP stream should open");

        let body = stream
            .fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                bytes
            })
            .await;
        let body = String::from_utf8(body).expect("canonical SSE");
        assert!(body.contains("\"content\":\"partial\""), "{body}");
        assert!(body.contains("\"code\":\"stream_transport\""), "{body}");
        assert!(!body.contains("\"type\":\"_inprocess_summary\""), "{body}");

        let terminal = observer
            .events()
            .into_iter()
            .find_map(|event| match event {
                RecordedAttemptEvent::Finished(_, terminal) => Some(terminal),
                RecordedAttemptEvent::Began(_) => None,
            })
            .expect("physical attempt terminal");
        assert_eq!(
            terminal.status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            Some("bedrock-tail-7")
        );
        assert_eq!(terminal.usage, astra_services::InferenceUsage::default());
    }
}
