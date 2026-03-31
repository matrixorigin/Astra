//! Shared LLM calling utilities.
//!
//! Extracted from [`super::bridge_inprocess`] so both the in-process bridge
//! and [`crate::server::server_loop_host::ServerAgenticLoopHost`] can call LLMs
//! without duplicating the retry/backoff/parsing logic.

use std::{collections::HashMap, time::Instant};

use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};

use super::sse_blocks::SseBlankLineUtf8Buf;
use super::sse_data_lines::{drain_sse_data_lines, finish_sse_data_buffer, json_events_from_sse_event_block};
use crate::prompts;

/// Maximum retries for transient LLM errors (429, 5xx, network).
pub(crate) const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
pub(crate) const LLM_RETRY_BASE_MS: u64 = 1000;

// ── System Prompt Cache ──────────────────────────────────────────────────────
use std::sync::{Mutex, OnceLock};

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

/// Call the LLM streaming API, collect the full response, and return a structured result.
///
/// Unlike `call_llm_stream` (which returns raw SSE bytes), this function
/// parses the stream and returns the aggregated `LlmCallResult` directly.
/// Used by `ServerAgenticLoopHost` for server-side agentic loops.
pub(crate) async fn call_llm_and_collect(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
) -> Result<LlmCallResult, String> {
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
        if attempt > 0 {
            let delay = LLM_RETRY_BASE_MS * (1 << (attempt - 1));
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
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
            let byte_stream = response.bytes_stream();
            let result = collect_llm_stream(byte_stream, model_name, started).await;
            return Ok(result);
        }

        let text = response.text().await.unwrap_or_default();
        last_err = format!("LLM error {status}: {text}");
        if status == 429 || status >= 500 {
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
) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls_map: HashMap<usize, Map<String, Value>> = HashMap::new();
    let mut usage = Map::new();

    let sse = parse_sse_chunks(stream);
    tokio::pin!(sse);

    while let Some(chunk) = sse.next().await {
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
                        (
                            "function".to_string(),
                            json!({"name": "", "arguments": ""}),
                        ),
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

    LlmCallResult {
        full_text,
        reasoning,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// Parse OpenAI-style SSE chunks into JSON values.
fn parse_sse_chunks(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
) -> impl futures_util::Stream<Item = Value> + Send + 'static {
    async_stream::stream! {
        let mut sse_in = SseBlankLineUtf8Buf::new();
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            for block in sse_in.push_lossy_bytes(&bytes) {
                let d = json_events_from_sse_event_block(&block);
                for v in d.events {
                    yield v;
                }
                if d.stream_finished {
                    return;
                }
            }
        }
        let mut buf = sse_in.into_inner();
        let tail = drain_sse_data_lines(&mut buf, "");
        for v in tail.events {
            yield v;
        }
        if tail.stream_finished {
            return;
        }
        let fin = finish_sse_data_buffer(&mut buf);
        for v in fin.events {
            yield v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_llm_error_categories() {
        assert_eq!(classify_llm_error("rate limit exceeded"), "rate_limit");
        assert_eq!(classify_llm_error("error 429: too many requests"), "rate_limit");
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
}
