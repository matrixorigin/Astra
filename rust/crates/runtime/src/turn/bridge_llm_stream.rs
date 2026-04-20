//! LLM streaming call with retry logic, rate-limit cooldown, and idle-timeout fallback.
//!
//! This module encapsulates the HTTP streaming call to the LLM provider, including:
//! - SSE chunk parsing and forwarding (text_delta, reasoning_delta, tool_call_start, usage)
//! - Idle-timeout detection with automatic non-stream fallback
//! - Retry with exponential backoff for transient errors (429, 5xx, network)
//! - Per-model rate-limit cooldown tracking
//! - Degraded tool-call recovery from XML-like text content

use std::sync::Arc;

use async_stream::stream;
use axum::body::Bytes;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::bridge::rate_limit_cooldown::{
    PerModelCooldown, RateLimitAction, is_overload_status, is_rate_limit_status,
    parse_retry_after_ms,
};
use crate::turn::bridge_sse_helpers::render_sse;
use crate::turn::edge_ledger::ensure_tool_call_ids;
use crate::turn::llm_client::{LlmCancel, sleep_ms_or_llm_cancel};
use futures_util::StreamExt;
use std::sync::OnceLock;

/// Maximum retries for transient LLM errors (429, 5xx, network).
const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
const LLM_RETRY_BASE_MS: u64 = 1000;

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

fn turn_timeout_s() -> f64 {
    astra_core::RuntimeLimits::global().turn_timeout_s
}

/// Call LLM streaming API, yield SSE bytes.
/// Emits: text_delta, reasoning_delta, reasoning_done, tool_call_start, usage SSE events,
/// then a final `_inprocess_summary` event with full_text/tool_calls/usage/model_used.
///
/// **Stream resilience (same as [`super::llm_client::call_llm_and_collect`])**:
/// per-chunk idle watchdog on parsed SSE; if the provider stops sending, partial state is
/// discarded and a **single non-stream** `/chat/completions` request attempts recovery.
///
/// Retries up to LLM_MAX_RETRIES times on transient errors (429/5xx/network)
/// with exponential backoff.
///
/// **Note**: Caller must check rate-limit cooldown state and handle fallback model
/// resolution BEFORE calling this function. This function only handles retries for
/// transient errors within a single model.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_llm_stream(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    has_fallback: bool,
    client_cancel: Option<Arc<CancellationToken>>,
) -> Result<impl futures_util::Stream<Item = Bytes> + Send + 'static, String> {
    let cooldown = rate_limit_cooldown();
    let model_key = model_name;

    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(crate::turn::llm_client::llm_connect_timeout())
        .timeout(std::time::Duration::from_secs(turn_timeout_s() as u64 + 10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut body = json!({
        "model": model_name,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });

    // Set max output tokens to prevent generation cutoff.
    // Use provider-appropriate field name.
    if let Some(max_out) = max_output_tokens {
        if provider == "anthropic" || model_name.contains("claude") {
            body["max_tokens"] = json!(max_out);
        } else {
            // OpenAI, DeepSeek, Qwen, etc. use max_completion_tokens (newer)
            // or max_tokens (legacy). Prefer max_completion_tokens.
            body["max_completion_tokens"] = json!(max_out);
        }
    }

    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = Value::String("auto".to_string());
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let req_bytes = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);

    // Retry loop for transient errors (429 rate limit, 5xx server errors, network)
    let mut last_err = String::new();
    for attempt in 0..=LLM_MAX_RETRIES {
        if attempt > 0 {
            let delay = LLM_RETRY_BASE_MS * (1 << (attempt - 1));
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel))
                .await
                .map_err(|e| e.to_string())?;
        }

        let mut req = client.post(&url).header("content-type", "application/json");

        if provider == "anthropic" {
            req = req
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01");
        } else {
            req = req.header("authorization", format!("Bearer {api_key}"));
        }

        let request_start = std::time::Instant::now();
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

            let client_for_fallback = client.clone();
            let messages_for_fallback: Vec<Value> = messages.to_vec();
            let tools_for_fallback: Vec<Value> = tools.to_vec();
            let api_key_for_fallback = api_key.to_string();
            let base_url_for_fallback = base_url.to_string();
            let provider_for_fallback = provider.to_string();
            let max_out_for_fallback = max_output_tokens;
            let idle_pre = crate::turn::llm_client::stream_idle_timeout();
            let idle_post = crate::turn::llm_client::stream_idle_timeout_after_progress();

            let out = stream! {
                let cc = client_cancel.clone();
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();
                let mut made_progress = false;

                let sse = crate::turn::llm_client::parse_openai_sse_json_stream(byte_stream);
                tokio::pin!(sse);

                loop {
                    let idle = if made_progress { idle_post } else { idle_pre };
                    tokio::select! {
                        biased;
                        _ = crate::turn::llm_client::wait_until_cancelled_or_pending(cc.as_deref()) => {
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
                                    tool_calls_map.clear();
                                    full_text.clear();
                                    reasoning.clear();
                                    let fb_timeout = crate::turn::llm_client::llm_fallback_timeout();
                                    match crate::turn::llm_client::call_llm_nonstream_fallback(
                                        &client_for_fallback,
                                        &messages_for_fallback,
                                        &tools_for_fallback,
                                        &model_name,
                                        &api_key_for_fallback,
                                        &base_url_for_fallback,
                                        &provider_for_fallback,
                                        max_out_for_fallback,
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
                                            if !result.full_text.is_empty()
                                                && result.tool_calls.is_empty()
                                            {
                                                yield render_sse(&json!({"type":"text_delta","content": result.full_text}));
                                            }
                                            if !result.reasoning.is_empty() {
                                                yield render_sse(&json!({"type":"reasoning_delta","content": result.reasoning}));
                                            }
                                            for tc in &result.tool_calls {
                                                if let Some(obj) = tc.as_object() {
                                                    let mut tc = obj.clone();
                                                    if let Some(event) = tool_call_start_event(&mut tc) {
                                                        yield render_sse(&event);
                                                    }
                                                }
                                            }
                                            let prompt = result.usage.get("prompt").and_then(Value::as_i64);
                                            let completion = result.usage.get("completion").and_then(Value::as_i64);
                                            if prompt.is_some() || completion.is_some() {
                                                let cache_read = result.usage.get("cache_read").and_then(Value::as_i64);
                                                let cache_creation = result.usage.get("cache_creation").and_then(Value::as_i64);
                                                yield render_sse(&json!({
                                                    "type": "usage",
                                                    "prompt_tokens": prompt,
                                                    "completion_tokens": completion,
                                                    "cache_read_tokens": cache_read,
                                                    "cache_creation_tokens": cache_creation,
                                                }));
                                            }
                                        }
                                        Err(e) => {
                                            yield render_sse(&json!({"type":"error","message": format!("stream stalled; non-stream recovery failed: {e}")}));
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
                                    astra_core::agent_warn!(
                                        "llm",
                                        "in-process stream transport error: {e}"
                                    );
                                    yield render_sse(&json!({"type":"error","message": format!("LLM stream transport error: {e}")}));
                                    tool_calls_map.clear();
                                    full_text.clear();
                                    reasoning.clear();
                                    break;
                                }
                            };
                            // Some providers attach usage to a chunk that also contains choices,
                            // so parse usage first on every chunk.
                            made_progress = true;
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
                                    // OpenAI: prompt_tokens_details.cached_tokens
                                    // Anthropic (via proxy): cache_read_input_tokens / cache_creation_input_tokens
                                    let cache_read = u.get("prompt_tokens_details")
                                        .and_then(|d| d.get("cached_tokens"))
                                        .and_then(Value::as_i64)
                                        .or_else(|| u.get("cache_read_input_tokens").and_then(Value::as_i64));
                                    let cache_creation = u.get("prompt_tokens_details")
                                        .and_then(|d| d.get("cache_creation_input_tokens"))
                                        .and_then(Value::as_i64)
                                        .or_else(|| u.get("cache_creation_input_tokens").and_then(Value::as_i64));
                                    yield render_sse(&json!({
                                        "type": "usage",
                                        "prompt_tokens": prompt,
                                        "completion_tokens": completion,
                                        "cache_read_tokens": cache_read,
                                        "cache_creation_tokens": cache_creation,
                                    }));
                                }
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
                                    full_text.push_str(content);
                                    yield render_sse(&json!({"type": "text_delta", "content": content}));
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

                // Emit final summary as a special internal event (not forwarded to client)
                let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
                sorted_tcs.sort_by_key(|(idx, _)| *idx);
                let mut tool_calls: Vec<Value> = sorted_tcs.into_iter().map(|(_, v)| Value::Object(v)).collect();

                // Degraded tool-call fallback: recover <invoke> or <tool_call> blocks.
                if tool_calls.is_empty() {
                    if let Some(parsed) = crate::turn::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text) {
                        astra_core::agent_warn!(
                            "llm",
                            "recovered {} tool call(s) from degraded text in content (inprocess)",
                            parsed.len()
                        );
                        full_text = crate::turn::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
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

            return Ok(out);
        }

        // Non-success: check if retryable (429 rate limit, 5xx server error)
        let headers = response.headers();
        let retry_after_ms = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);

        let text = response.text().await.unwrap_or_default();
        last_err = format!("LLM error {status}: {text}");

        // Record rate-limit errors to cooldown tracker
        if is_rate_limit_status(status) {
            let action = cooldown.with(model_key, |c| c.record_429(retry_after_ms, has_fallback));
            astra_core::agent_warn!(
                "llm",
                "rate limit (429) on {}: action={:?}",
                model_key,
                action,
            );

            // If cooldown says to wait, honor it
            if let RateLimitAction::WaitAndRetry { delay_ms } = action {
                sleep_ms_or_llm_cancel(delay_ms, bridge_llm_cancel(&client_cancel))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            continue; // Retryable
        }

        if is_overload_status(status) {
            let action = cooldown.with(model_key, |c| c.record_529(retry_after_ms, has_fallback));
            astra_core::agent_warn!(
                "llm",
                "server overload ({status}) on {}: action={:?}",
                model_key,
                action,
            );

            // If cooldown says to wait, honor it
            if let RateLimitAction::WaitAndRetry { delay_ms } = action {
                sleep_ms_or_llm_cancel(delay_ms, bridge_llm_cancel(&client_cancel))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            continue; // Retryable
        }

        // Other 5xx errors are retryable but don't affect cooldown state
        if status >= 500 {
            continue;
        }

        // 4xx (except 429) is not retryable — fail immediately.
        // Context-window errors are detected by content at the call site
        // (bridge_inprocess forward()), not here.
        return Err(last_err);
    }

    // All retries exhausted
    Err(format!("{last_err} (after {} retries)", LLM_MAX_RETRIES))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
