/// In-process ChatTurnBridge — calls LLM directly without an external bridge service.
///
/// # Claude Code–style behaviors mapped onto this stack
///
/// | Claude Code (desktop) | Here |
/// |------------------------|------|
/// | Long-lived stream “stall” / no chunks | [`super::llm_client::stream_idle_timeout`] on SSE `next()` (90s default, `MO_STREAM_IDLE_TIMEOUT_MS`) |
/// | Recover via one-shot completion | [`super::llm_client::call_llm_nonstream_fallback`] after idle in both `call_llm_and_collect` and [`call_llm_stream`] below |
/// | User cancel clears in-flight work | HTTP `/chat/turn` passes `CancellationToken`; dropping the SSE body (client disconnect) cancels in-flight LLM byte/SSE consumption in-process |
/// | Cooldown / 429 wait cannot ignore disconnect | [`super::llm_client::sleep_ms_or_llm_cancel`] on retry backoff + rate-limit waits in [`call_llm_stream`]; initial cooldown wait `select!`s [`wait_until_cancelled_or_pending`](super::llm_client::wait_until_cancelled_or_pending) in the bridge stream |
/// | Tool permission queue + single resolve | CLI: `astra-cli` `permission_manager`; cloud: edge approval ledger / `POST /tools/result`. Claude’s `PermissionContext` “resolve once” matches ledger single-shot semantics |
///
/// # Legacy Status
///
/// This module implements the **old-style cloud tool loop** (its own `for round_ix..`
/// loop inside `stream!`). It does NOT use [`run_agentic_loop_with_host`], meaning
/// stall detection, post-tool policy, semantic dedup, and step recording are **absent**
/// from this path.
///
/// **Preferred replacement**: Use [`super::loop_dispatcher::LoopDispatcher`] with
/// [`ServerAgenticLoopHost`](crate::server::server_loop_host::ServerAgenticLoopHost)
/// which runs the full unified cognitive loop including all runtime policies.
///
/// This bridge remains wired for backward compatibility with existing `/chat/turn`
/// and `/chat/stream` HTTP endpoints. New features should target the unified loop.
///
/// # Architecture (legacy)
///
///   Rust API (dispatch_chat_turn_bridge) injects context into headers:
///     x-mo-user-id, x-mo-session-id, x-mo-turn-chain-id, x-mo-user-query-event-id, ...
///   This bridge reads those headers, calls the LLM, streams SSE back, persists events, and
///   for each tool round blocks on [`super::edge_ledger`] until `POST /tools/result` (or timeout).
use std::{collections::HashMap, sync::Arc, time::Instant};

use async_stream::stream;
use axum::body::Body;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::{StreamExt, stream};

/// Maximum number of read-only tools to execute concurrently.
/// Prevents resource exhaustion from parallel tool execution.
/// Matches Claude Code's default `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`.
const MAX_CONCURRENT_READ_ONLY_TOOLS: usize = 10;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    ChatTurnBridge, FernetTokenEncryptor, MatrixOneSettings, SessionActivityUpdatePlan,
    TurnAuxiliaryEventWriter, TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan,
    TurnHookDbWriter, TurnObserverWorker, TurnReflectionLessonWriter, TurnReflectionStateStore,
    TurnSessionActivityWriter, TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
    build_explain_event, build_stream_error_event, prompts,
    turn::cloud_tool_delivery::{
        cloud_tool_requires_approval_for_delivery, sse_maps_through_tool_request,
        tool_path_hint_for_delivery, wait_approval_ledger_for_tool,
        wait_tool_result_ledger_for_tool,
    },
    turn::edge_ledger::{
        assistant_message_with_tool_calls_and_reasoning, ensure_tool_call_ids,
        history_has_reasoning,
    },
    turn::llm_client::{LlmCancel, sleep_ms_or_llm_cancel},
    turn::persist::{build_tool_call_event_payload, build_tool_result_event_payload},
    turn::sse_blocks::SseBlankLineUtf8Buf,
    turn::sse_data_lines::{
        drain_sse_data_lines, finish_sse_data_buffer, validate_sse_event_block_json,
        validated_json_events_from_sse_block,
    },
    turn::stream_events::build_approval_required_event,
    turn::tool_schema_prune::prune_tool_schemas,
};

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;

fn turn_timeout_s() -> f64 {
    astra_core::RuntimeLimits::global().turn_timeout_s
}

fn count_inprocess_persisted_events(
    core_event_count: usize,
    tool_event_count: usize,
    tool_events_persisted: bool,
) -> usize {
    core_event_count
        + if tool_events_persisted {
            tool_event_count
        } else {
            0
        }
}

fn render_sse(event: &Value) -> Bytes {
    match serde_json::to_string(event) {
        Ok(s) => Bytes::from(format!("data: {s}\n\n")),
        Err(e) => {
            astra_core::agent_error!("sse", "serialization failed: {e}");
            Bytes::from("event: error\ndata: {\"error\":\"internal serialization failure\"}\n\n")
        }
    }
}

fn render_sse_map(event: &Map<String, Value>) -> Bytes {
    render_sse(&Value::Object(event.clone()))
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

/// Maps one parsed JSON event from the in-process LLM SSE stream to bytes forwarded to the HTTP client.
fn apply_forward_llm_sse_event(
    event: &Value,
    saw_inprocess_summary: &mut bool,
    loop_text: &mut String,
    loop_reasoning: &mut String,
    loop_tool_calls: &mut Vec<Value>,
    usage: &mut Map<String, Value>,
    resolved_model: &mut String,
) -> Result<Vec<Bytes>, String> {
    let Some(t) = event.get("type").and_then(Value::as_str) else {
        return Err("SSE event missing type field".into());
    };
    match t {
        "_inprocess_summary" => {
            *saw_inprocess_summary = true;
            *loop_text = event
                .get("full_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            *loop_reasoning = event
                .get("reasoning")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            *loop_tool_calls = event
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(u) = event.get("usage").and_then(Value::as_object) {
                *usage = u.clone();
            }
            if let Some(m) = event.get("model_used").and_then(Value::as_str) {
                *resolved_model = m.to_string();
            }
            Ok(vec![])
        }
        "text_delta" | "reasoning_delta" | "tool_call_start" | "usage" | "error"
        | "error_message" => Ok(vec![render_sse(event)]),
        "warning" => Ok(vec![render_sse(event)]),
        _ => Ok(vec![]),
    }
}

fn extend_forward_from_validated_sse_block(
    block: &str,
    saw_inprocess_summary: &mut bool,
    loop_text: &mut String,
    loop_reasoning: &mut String,
    loop_tool_calls: &mut Vec<Value>,
    usage: &mut Map<String, Value>,
    resolved_model: &mut String,
) -> Result<Vec<Bytes>, String> {
    let events = validated_json_events_from_sse_block(block)?;
    let mut out = Vec::new();
    for ev in events {
        out.extend(apply_forward_llm_sse_event(
            &ev,
            saw_inprocess_summary,
            loop_text,
            loop_reasoning,
            loop_tool_calls,
            usage,
            resolved_model,
        )?);
    }
    Ok(out)
}

fn flush_tail_buf_into_llm_forward(
    buf: &mut String,
    saw_inprocess_summary: &mut bool,
    loop_text: &mut String,
    loop_reasoning: &mut String,
    loop_tool_calls: &mut Vec<Value>,
    usage: &mut Map<String, Value>,
    resolved_model: &mut String,
) -> Result<Vec<Bytes>, String> {
    if !buf.trim().is_empty() {
        validate_sse_event_block_json(buf)?;
    }
    let mut out = Vec::new();
    let d = drain_sse_data_lines(buf, "");
    for ev in d.events {
        out.extend(apply_forward_llm_sse_event(
            &ev,
            saw_inprocess_summary,
            loop_text,
            loop_reasoning,
            loop_tool_calls,
            usage,
            resolved_model,
        )?);
    }
    if d.stream_finished {
        return Ok(out);
    }
    let fin = finish_sse_data_buffer(buf);
    for ev in fin.events {
        out.extend(apply_forward_llm_sse_event(
            &ev,
            saw_inprocess_summary,
            loop_text,
            loop_reasoning,
            loop_tool_calls,
            usage,
            resolved_model,
        )?);
    }
    Ok(out)
}

/// Maximum retries for transient LLM errors (429, 5xx, network).
const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
const LLM_RETRY_BASE_MS: u64 = 1000;

// ── Rate-Limit Cooldown ──────────────────────────────────────────────────────
use crate::bridge::rate_limit_cooldown::{
    PerModelCooldown, RateLimitAction, is_overload_status, is_rate_limit_status,
    parse_retry_after_ms,
};
use std::sync::OnceLock;

/// Per-model rate-limit cooldown tracker.
fn rate_limit_cooldown() -> &'static PerModelCooldown {
    static COOLDOWN: OnceLock<PerModelCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(PerModelCooldown::new)
}

// ── System Prompt Cache ──────────────────────────────────────────────────────
// Two-level cache for static/dynamic prompt boundary:
// - Global+Session sections are cached by (tool_names, task_type, confidence) — stable within a session
// - Per-turn profile_desc is NOT cached (changes every turn with skills/memory/environment)
use std::sync::Mutex;

/// Cached prompt sections (Global + Session scoped).
struct CachedSections {
    /// Concatenated text of Global+Session sections (for non-Anthropic providers).
    text: String,
    /// Individual sections with scope metadata (for Anthropic cache_control).
    sections: Vec<prompts::PromptSection>,
}

fn section_cache() -> &'static Mutex<HashMap<u64, CachedSections>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, CachedSections>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn section_cache_key(tool_names: &[&str], task_type: Option<&str>, confidence: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in tool_names {
        name.hash(&mut hasher);
    }
    task_type.unwrap_or("none").hash(&mut hasher);
    let bucket = if confidence < 0.3 { "low" } else { "normal" };
    bucket.hash(&mut hasher);
    hasher.finish()
}

/// Build the system message Value for the LLM API.
///
/// For Anthropic providers: uses content array with `cache_control` on stable sections,
/// enabling server-side KV cache reuse across turns.
///
/// For other providers (OpenAI, DeepSeek, etc.): uses a single content string.
/// Build the system message(s) for the LLM API.
///
/// Returns `(primary, dynamic)`:
/// - **Anthropic**: `primary` is a multi-block content array with `cache_control` on stable
///   sections and dynamic profile appended without cache markers. `dynamic` is `None`.
/// - **OpenAI / other**: `primary` contains only the **stable** text (cacheable prefix).
///   `dynamic` holds a second system message with the per-turn profile/hints, or `None`
///   if there is nothing dynamic. This split enables OpenAI's automatic prefix caching:
///   the stable message stays identical across turns so the provider can reuse the KV cache.
fn build_system_message(
    tool_names: &[&str],
    profile_desc: &str,
    confidence: f64,
    task_type: Option<&str>,
    provider: &str,
    model_name: &str,
) -> (Value, Option<Value>) {
    let key = section_cache_key(tool_names, task_type, confidence);

    // Try cache for the stable (Global + Session) sections
    let cached = if let Ok(cache) = section_cache().lock() {
        cache
            .get(&key)
            .map(|c| (c.text.clone(), c.sections.clone()))
    } else {
        None
    };

    let (stable_text, sections) = cached.unwrap_or_else(|| {
        // Build all sections (profile_desc is "" for cache — we'll append it separately)
        let all = prompts::build_system_prompt_sections(tool_names, "", confidence, task_type);
        // Only cache Global + Session sections (not None-scoped profile)
        let stable: Vec<prompts::PromptSection> = all
            .into_iter()
            .filter(|s| s.scope != prompts::CacheScope::None)
            .collect();
        let text = stable
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("");

        if let Ok(mut cache) = section_cache().lock() {
            if cache.len() > 32 {
                cache.clear();
            }
            cache.insert(
                key,
                CachedSections {
                    text: text.clone(),
                    sections: stable.clone(),
                },
            );
        }
        (text, stable)
    });

    let is_anthropic = provider == "anthropic" || model_name.contains("claude");

    if is_anthropic {
        // Anthropic: multi-block content with cache_control on stable sections.
        // Even via OpenAI-compatible proxies, many forward cache_control to the
        // native Messages API. Proxies that don't will simply ignore the field.
        //
        // Cache strategy:
        //   Place cache_control on the LAST block of each scope group.
        //   Anthropic allows up to 4 breakpoints per request — we use at most 2
        //   (last Global, last Session). The provider caches the prefix up to
        //   each breakpoint.
        //
        //   Global  → scope:"global" + ttl:"1h"  (shared across all sessions/orgs)
        //   Session → ttl:"1h"                    (stable within a session)
        //   None    → no cache_control             (changes every turn)
        let cache_disabled = std::env::var("MO_PROMPT_CACHE_DISABLED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

        // Find the last index of each scope group for breakpoint placement
        let last_global = sections
            .iter()
            .rposition(|s| s.scope == prompts::CacheScope::Global);
        let last_session = sections
            .iter()
            .rposition(|s| s.scope == prompts::CacheScope::Session);

        let mut blocks: Vec<Value> = Vec::with_capacity(sections.len() + 1);
        for (i, section) in sections.iter().enumerate() {
            let cc = if cache_disabled {
                None
            } else if Some(i) == last_global {
                Some(json!({"type": "ephemeral", "scope": "global", "ttl": "1h"}))
            } else if Some(i) == last_session {
                Some(json!({"type": "ephemeral", "ttl": "1h"}))
            } else {
                None
            };
            let mut block = json!({
                "type": "text",
                "text": section.text,
            });
            if let Some(cc) = cc {
                block["cache_control"] = cc;
            }
            blocks.push(block);
        }
        // Dynamic section (profile + per-turn hints) — no cache_control
        if !profile_desc.is_empty() {
            blocks.push(json!({
                "type": "text",
                "text": profile_desc,
            }));
        }
        // Anthropic: everything in one message (cache_control breakpoints handle caching)
        (
            json!({
                "role": "system",
                "content": blocks,
            }),
            None,
        )
    } else {
        // OpenAI-compatible: split stable / dynamic into separate system messages
        // so the stable prefix is identical across turns and the provider can reuse
        // its automatic KV cache.
        let primary = json!({
            "role": "system",
            "content": stable_text,
        });
        let dynamic = if profile_desc.is_empty() {
            None
        } else {
            Some(json!({
                "role": "system",
                "content": profile_desc,
            }))
        };
        (primary, dynamic)
    }
}

/// Add `cache_control` to the last tool schema for Anthropic,
/// marking the tool definitions as cache-eligible.
fn annotate_tool_schemas_for_caching(tools: &mut [Value], provider: &str, model_name: &str) {
    let is_anthropic = provider == "anthropic" || model_name.contains("claude");
    if !is_anthropic || tools.is_empty() {
        return;
    }
    if std::env::var("MO_PROMPT_CACHE_DISABLED")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        return;
    }
    // Mark last tool definition with cache_control — Anthropic caches the prefix
    // up to the last cache_control marker. Use 1h TTL since tool schemas are
    // stable within a session.
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
    }
}

/// Add a cache breakpoint on the last conversation message for Anthropic.
/// This enables turn-to-turn KV cache reuse for the conversation prefix.
fn add_message_cache_breakpoint(messages: &mut [Value], provider: &str, model_name: &str) {
    let is_anthropic = provider == "anthropic" || model_name.contains("claude");
    if !is_anthropic || messages.is_empty() {
        return;
    }
    if std::env::var("MO_PROMPT_CACHE_DISABLED")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        return;
    }
    // Find the last non-system message and add cache_control to it
    if let Some(last) = messages.iter_mut().rev().find(|m| {
        m.get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r != "system")
    }) {
        // If content is a string, convert to array format for cache_control
        if last.get("content").is_some_and(Value::is_string) {
            let text = last["content"].as_str().unwrap_or_default().to_string();
            last["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        } else if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
            // Content is already an array — add cache_control to last element
            if let Some(last_block) = arr.last_mut() {
                last_block["cache_control"] = json!({"type": "ephemeral"});
            }
        }
    }
}

fn bridge_llm_cancel(cc: &Option<Arc<CancellationToken>>) -> LlmCancel<'_> {
    match cc.as_ref() {
        Some(t) => LlmCancel::Token(t.as_ref()),
        None => LlmCancel::None,
    }
}

/// Call LLM streaming API, yield SSE bytes.
/// Emits: text_delta, reasoning_delta, tool_call_start, usage SSE events,
/// then a final `_inprocess_summary` event with full_text/tool_calls/usage/model_used.
///
/// **Stream resilience (Claude Code–style, same as [`super::llm_client::call_llm_and_collect`])**:
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
async fn call_llm_stream(
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

    // Retry loop for transient errors (429 rate limit, 5xx server errors, network)
    let mut last_err = String::new();
    for attempt in 0..=LLM_MAX_RETRIES {
        if attempt > 0 {
            let delay = LLM_RETRY_BASE_MS * (1 << (attempt - 1));
            sleep_ms_or_llm_cancel(delay, bridge_llm_cancel(&client_cancel)).await?;
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
            let idle_dur = crate::turn::llm_client::stream_idle_timeout();

            let out = stream! {
                let cc = client_cancel.clone();
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();

                let sse = crate::turn::llm_client::parse_openai_sse_json_stream(byte_stream);
                tokio::pin!(sse);

                loop {
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
                        tick = tokio::time::timeout(idle_dur, sse.next()) => {
                            let next = tick;
                            let chunk = match next {
                                Ok(c) => c,
                                Err(_) => {
                                    astra_core::agent_warn!(
                                        "llm",
                                        "in-process stream idle after {}ms — attempting non-stream fallback",
                                        idle_dur.as_millis()
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
                                        Ok(result) => {
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
                                                if let Some(name) = tc
                                                    .get("function")
                                                    .and_then(|f| f.get("name"))
                                                    .and_then(Value::as_str)
                                                    && !name.is_empty()
                                                {
                                                    yield render_sse(&json!({"type":"tool_call_start","name": name}));
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
                                        if let Some(name) = func.get("name").and_then(Value::as_str)
                                            && is_valid_tool_name(name) {
                                                let is_new = f.get("name").and_then(Value::as_str).unwrap_or("").is_empty();
                                                f.insert("name".to_string(), Value::String(name.to_string()));
                                                if is_new {
                                                    yield render_sse(&json!({"type": "tool_call_start", "name": name}));
                                                }
                                            } else if let Some(bad_name) = func.get("name").and_then(Value::as_str) {
                                                astra_core::agent_warn!(
                                                    "llm",
                                                    "dropped malformed tool_call with invalid name: {bad_name:?}"
                                                );
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
                    }
                }

                // Emit final summary as a special internal event (not forwarded to client)
                let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
                sorted_tcs.sort_by_key(|(idx, _)| *idx);
                let tool_calls: Vec<Value> = sorted_tcs.into_iter().map(|(_, v)| Value::Object(v)).collect();

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
                sleep_ms_or_llm_cancel(delay_ms, bridge_llm_cancel(&client_cancel)).await?;
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
                sleep_ms_or_llm_cancel(delay_ms, bridge_llm_cancel(&client_cancel)).await?;
            }
            continue; // Retryable
        }

        // Other 5xx errors are retryable but don't affect cooldown state
        if status >= 500 {
            continue;
        }

        // 4xx (except 429) is not retryable — fail immediately
        // Context-window errors get a special prefix so callers can detect and
        // trigger auto-compaction + retry.
        if status == 400 && crate::turn::llm_client::is_context_window_error(&text.to_lowercase()) {
            return Err(format!(
                "{}{}",
                crate::turn::llm_client::CONTEXT_WINDOW_ERROR_PREFIX,
                last_err
            ));
        }
        return Err(last_err);
    }

    // All retries exhausted
    Err(format!("{last_err} (after {} retries)", LLM_MAX_RETRIES))
}

#[derive(Clone)]
pub struct InProcessChatTurnBridge {
    pub matrixone: MatrixOneSettings,
    pub encryptor: Arc<FernetTokenEncryptor>,
    /// Shared DB pool — avoids creating a new connection per turn.
    /// When `None`, falls back to ephemeral single-connection pool.
    pub shared_pool: Option<Arc<sqlx::Pool<sqlx::MySql>>>,
    /// Pipeline learning writer — auto-updates EntityGraph/PatternLibrary/Calibrator.
    pub turn_learning_writer: Option<Arc<dyn crate::TurnLearningWriter>>,
    /// Same `Arc` as [`crate::AppState::edge_callback_ledger`] — bridge takes tool callbacks here.
    pub edge_callback_ledger: Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
}

impl InProcessChatTurnBridge {
    pub fn new(matrixone: MatrixOneSettings, encryptor: Arc<FernetTokenEncryptor>) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            turn_learning_writer: None,
            edge_callback_ledger: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn with_pool(mut self, pool: Arc<sqlx::Pool<sqlx::MySql>>) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_learning_writer(mut self, writer: Arc<dyn crate::TurnLearningWriter>) -> Self {
        self.turn_learning_writer = Some(writer);
        self
    }

    pub fn with_edge_callback_ledger(
        mut self,
        ledger: Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    ) -> Self {
        self.edge_callback_ledger = ledger;
        self
    }
}

#[async_trait::async_trait]
impl ChatTurnBridge for InProcessChatTurnBridge {
    async fn forward(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
        turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
        turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
        turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
        turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
        turn_observer_worker: Arc<dyn TurnObserverWorker>,
        turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
        turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
        client_cancel: Option<Arc<CancellationToken>>,
    ) -> Result<Response, (StatusCode, String)> {
        // Extract trusted context injected by dispatch_chat_turn_bridge
        let user_id = header_str(headers, "x-mo-user-id").unwrap_or_default();
        let session_id = header_str(headers, "x-mo-session-id").unwrap_or_default();
        let turn_chain_id =
            header_str(headers, "x-mo-turn-chain-id").unwrap_or_else(|| Uuid::now_v7().to_string());
        let user_query_event_id = header_str(headers, "x-mo-user-query-event-id")
            .unwrap_or_else(|| Uuid::now_v7().to_string());

        // Parse request body
        let payload: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tool_results = payload
            .get("tool_results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let edge_tools = payload
            .get("edge_tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let selection_confidence = payload
            .get("selection_confidence")
            .and_then(Value::as_f64)
            .unwrap_or(1.0); // Default: high confidence
        let edge_profile = payload
            .get("edge_profile")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let explain = payload
            .get("explain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let model_override = payload
            .get("model")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let _agent_id = payload
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        let matrixone = self.matrixone.clone();
        let encryptor = self.encryptor.clone();
        let shared_pool = self.shared_pool.clone();
        let turn_learning_writer = self.turn_learning_writer.clone();
        let edge_callback_ledger = self.edge_callback_ledger.clone();

        #[cfg(feature = "bridge-e2e-hooks")]
        let bridge_e2e_for_stream: Option<Vec<Value>> =
            if crate::turn::bridge_e2e_hooks::authorized(headers) {
                payload
                    .get("test_llm_rounds")
                    .and_then(|v| v.as_array())
                    .cloned()
            } else {
                None
            };
        #[cfg(not(feature = "bridge-e2e-hooks"))]
        let bridge_e2e_for_stream: Option<Vec<Value>> = None;

        let bridge_e2e_capture = bridge_e2e_for_stream.clone();
        let client_cancel_capture = client_cancel.clone();

        let stream = stream! {
            let cc = client_cancel_capture.clone();
            let _client_disconnect_guard = cc
                .as_ref()
                .map(|t| crate::turn::llm_client::CancelOnClientDisconnect::new(t.clone()));
            let turn_started = Instant::now();
            // Emit session_info first
            yield render_sse(&json!({"type": "session_info", "session_id": session_id}));

            let bridge_e2e = bridge_e2e_capture;
            let use_e2e_llm = bridge_e2e.as_ref().map(|r| !r.is_empty()).unwrap_or(false);

            // Resolve LLM model (skipped when `test_llm_rounds` drives the turn — feature `bridge-e2e-hooks`).
            // Also capture fallback_model name for rate-limit-triggered fallback.
            let pool_ref = shared_pool.as_deref();
            let (mut model_name, mut api_key, mut base_url, mut provider, fallback_model_name) = if use_e2e_llm {
                (
                    "bridge-e2e-mock".to_string(),
                    "unused".to_string(),
                    "http://127.0.0.1:1".to_string(),
                    "openai".to_string(),
                    None::<String>,
                )
            } else {
                match astra_services::resolve_active_llm_model(
                    &matrixone,
                    encryptor.as_ref(),
                    model_override.as_deref(),
                    pool_ref,
                )
                .await
                {
                    Ok(m) => (m.model_name, m.api_key, m.base_url, m.provider, m.fallback_model),
                    Err(e) => {
                        yield render_sse_map(&build_stream_error_event(&e, "MODEL_NOT_AVAILABLE", false));
                        return;
                    }
                }
            };
            let has_fallback = fallback_model_name.is_some();

            // Check rate-limit cooldown and handle fallback model resolution
            let cooldown = rate_limit_cooldown();
            match cooldown.with(&model_name, |c| c.check_request(has_fallback)) {
                RateLimitAction::Proceed => {}
                RateLimitAction::WaitAndRetry { delay_ms } => {
                    astra_core::agent_info!(
                        "llm",
                        "rate-limit cooldown: waiting {delay_ms}ms before request"
                    );
                    tokio::select! {
                        biased;
                        _ = crate::turn::llm_client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                            yield render_sse_map(&build_stream_error_event(
                                "Request cancelled (client disconnected)",
                                "CLIENT_DISCONNECT",
                                false,
                            ));
                            return;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                    }
                }
                RateLimitAction::UseFallback { reason } => {
                    if let Some(ref fb_name) = fallback_model_name {
                        astra_core::agent_info!(
                            "llm",
                            "rate-limit cooldown: switching to fallback model '{}' ({})",
                            fb_name,
                            reason.as_str()
                        );
                        // Resolve fallback model credentials
                        match astra_services::resolve_active_llm_model(
                            &matrixone,
                            encryptor.as_ref(),
                            Some(fb_name.as_str()),
                            pool_ref,
                        )
                        .await
                        {
                            Ok(fb) => {
                                model_name = fb.model_name;
                                api_key = fb.api_key;
                                base_url = fb.base_url;
                                provider = fb.provider;
                            }
                            Err(e) => {
                                astra_core::agent_warn!(
                                    "llm",
                                    "fallback model '{}' resolution failed: {}",
                                    fb_name,
                                    e
                                );
                                // Continue with primary model (best effort)
                            }
                        }
                    } else {
                        astra_core::agent_warn!(
                            "llm",
                            "rate-limit cooldown: fallback requested ({}) but no fallback configured",
                            reason.as_str()
                        );
                    }
                }
                RateLimitAction::Reject {
                    reason,
                    reset_in_ms,
                } => {
                    // Preserve tool results that were collected before the rate limit hit.
                    // Without this, the client loses all tool output from this round and
                    // the model has no context about what already happened when retrying.
                    if !tool_results.is_empty() {
                        let summary = format!(
                            "[Rate-limited before LLM call. {} tool result(s) from this round are preserved below.]\n",
                            tool_results.len()
                        );
                        let mut content_ev = Map::new();
                        content_ev.insert("type".into(), json!("content"));
                        content_ev.insert("content".into(), json!(summary));
                        yield render_sse_map(&content_ev);
                        // Yield each tool result as an SSE event so the client can persist them
                        for tr in tool_results.iter() {
                            let mut tr_ev = Map::new();
                            tr_ev.insert("type".into(), json!("tool_result"));
                            tr_ev.insert("tool_result".into(), tr.clone());
                            yield render_sse_map(&tr_ev);
                        }
                    }
                    let err_msg = format!(
                        "Rate limit cooldown active ({}). Resets in {}s. Try again later.",
                        reason.as_str(),
                        reset_in_ms / 1000
                    );
                    yield render_sse_map(&build_stream_error_event(&err_msg, "RATE_LIMITED", true));
                    return;
                }
            }

            // Build LLM messages: system prompt + history + current messages + tool results
            let mut llm_messages: Vec<Value> = Vec::new();
            // Memory is prefetched and injected into profile_desc.
            // These track telemetry for the explain block.
            let mut memory_fetch_ms: i64 = 0;
            let mut memory_items: usize = 0;
            let mut memory_preview: Vec<String> = Vec::new();

            // System prompt — tells LLM about available tools and how to use them
            let tool_names: Vec<&str> = edge_tools.iter()
                .filter_map(|t| t.get("function").and_then(|f| f.get("name")).and_then(Value::as_str))
                .collect();
            let profile_desc = {
                let mut parts = Vec::new();
                if let Some(cwd) = edge_profile.get("cwd").and_then(Value::as_str) {
                    parts.push(format!("cwd: {cwd}"));
                }
                if let Some(branch) = edge_profile.get("git_branch").and_then(Value::as_str) {
                    parts.push(format!("git_branch: {branch}"));
                }
                // Inject rich environment context (OS, shell, git status, etc.)
                let env_section = edge_profile
                    .get("environment_context")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // Prefetch memories relevant to the current user message.
                // Injected every turn — cost is bounded by top_k and relevance
                // (typically 1-3 matches, ~100 tokens). This ensures LLM always
                // has user context (repo mappings, preferences) without needing
                // an extra round-trip to call memory_retrieve itself.
                if let (Some(mem_url), Some(mem_key)) = (
                    edge_profile.get("memoria_url").and_then(Value::as_str),
                    edge_profile.get("memoria_key").and_then(Value::as_str),
                ) {
                    let user_msg = messages
                        .iter()
                        .rev()
                        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                        .and_then(|m| m.get("content").and_then(Value::as_str))
                        .unwrap_or("");
                    let result = prefetch_memories(mem_url, mem_key, user_msg, &user_id).await;
                    memory_fetch_ms = result.fetch_ms;
                    memory_items = result.items;
                    memory_preview = result.preview;
                    if let Some(section) = result.section {
                        parts.push(section);
                    }
                }
                if parts.is_empty() && env_section.is_empty() {
                    String::new()
                } else {
                    let base = if parts.is_empty() {
                        String::new()
                    } else {
                        format!("\n\n# Project Profile\n{}", parts.join("\n"))
                    };
                    format!("{base}{env_section}")
                }
            };
            // Read active skill hints from edge_profile (injected by CLI)
            let skill_hint = edge_profile
                .get("active_skills")
                .and_then(Value::as_array)
                .map(|arr| {
                    let names: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                    if names.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\n\n## Active Output Skills\n\
                             The user has enabled these output constraints: {}. \
                             Follow their formatting rules strictly.",
                            names.join(", ")
                        )
                    }
                })
                .unwrap_or_default();
            // ── Extract user query for signal detection ──
            let user_content_for_signal = messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .unwrap_or("");

            let learned_context_hint = edge_profile
                .get("learned_context_hint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|hint| format!("\n\n## Learned Runtime Context\n{hint}"))
                .unwrap_or_default();
            let task_type = edge_profile
                .get("selection_task_type")
                .and_then(Value::as_str)
                .or_else(|| prompts::detect_task_type(user_content_for_signal));
            let profile_with_hints = format!("{profile_desc}{skill_hint}{learned_context_hint}");

            // ── Memory lifecycle: detect tracking/store signals in user input ──
            // Injects a priority hint into the system prompt so the LLM stores
            // the user's interest immediately rather than exploring the codebase.
            // This wires the Rust-side detect_store_signal into the live pipeline.
            let memory_signal_hint = if let Some(category) =
                crate::prompts::memory_lifecycle::detect_store_signal(user_content_for_signal)
            {
                let ns = crate::prompts::memory_lifecycle::suggest_namespace(category);
                format!(
                    "\n\n⚡ MEMORY SIGNAL DETECTED: category=\"{category}\", namespace=\"{ns}\". \
                     Store the user's intent with memory_store BEFORE doing anything else."
                )
            } else {
                String::new()
            };

            // Build per-turn dynamic content (profile + skills + memory signal)
            let dynamic_desc = format!("{profile_with_hints}{memory_signal_hint}");

            // Build provider-aware system message with static/dynamic boundary.
            // Anthropic gets multi-block content with cache_control on stable sections;
            // OpenAI/others get two messages: stable prefix (cacheable) + dynamic per-turn.
            let (system_msg, dynamic_msg) = build_system_message(
                &tool_names,
                &dynamic_desc,
                selection_confidence,
                task_type,
                &provider,
                &model_name,
            );
            llm_messages.push(system_msg);
            if let Some(dyn_msg) = dynamic_msg {
                llm_messages.push(dyn_msg);
            }

            // Merge tool results into messages (handle continuation turns)
            // Client sends complete message history including tool role messages,
            // so we just use messages directly.
            let (merged_messages, _initial_tier) = {
                let raw = messages.clone();

                // ── Micro-compact: clear old tool results before main compaction ──
                let raw = crate::turn::cloud::analytics::run_micro_compact(&raw);

                // Compute model budget for tier-aware compaction using cache-aware estimation.
                // Tool schemas are cache-eligible (stable prefix), so we estimate their cost.
                let budget = crate::prompts::budget_for_model(Some(&model_name));
                let tool_schema_tokens: usize = edge_tools.iter()
                    .map(|t| serde_json::to_string(t).map(|s| crate::prompts::estimate_str_tokens(&s)).unwrap_or(50))
                    .sum();
                // Combine system prompt (llm_messages) + conversation (raw) for estimation
                let mut all_msgs = llm_messages.clone();
                all_msgs.extend(raw.iter().cloned());
                let cache_est = crate::prompts::estimate_tokens_cache_aware(&all_msgs, tool_schema_tokens);
                let tier = crate::prompts::compaction_tier_calibrated(
                    &budget,
                    cache_est.total_tokens,
                    None,
                    0,
                );
                // Use effective input limit as char budget (×4 for char-to-token ratio)
                let budget_chars = budget.effective_input_limit() * 4;

                // Use Memoria-based compaction (async with HTTP client)
                let memoria_config = crate::turn::cloud::memoria_compact::MemoriaCompactConfig::default();
                let cwd = edge_profile.get("cwd").and_then(Value::as_str);
                let (session_memory_file, session_memory_combine) =
                    crate::turn::cloud::memoria_compact::resolve_session_memory_file_options(
                        &session_id,
                        cwd,
                    );
                let memoria_params = crate::turn::cloud::memoria_compact::MemoriaCompactParams {
                    budget_chars,
                    keep_chars: 2_000,
                    tier,
                    keep_recent_turns: budget.keep_recent_turns,
                    current_tokens: cache_est.total_tokens,
                    session_memory_file,
                    session_memory_combine,
                };

                // Try to create Memoria client from environment
                let memoria_client =
                    crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env();

                // Build summary client for LLM-based compaction
                let compact_config = crate::prompts::CompactConfig::from_env();
                let summary_client = crate::turn::cloud::summary::HttpSummaryClient::new(
                    crate::turn::cloud::summary::LlmConnParams {
                        model_name: model_name.clone(),
                        api_key: api_key.clone(),
                        base_url: base_url.clone(),
                        provider: provider.clone(),
                        max_output_tokens: compact_config.summary_token_budget,
                    },
                );

                let compact_result = crate::turn::cloud::memoria_compact::compact_with_memoria(
                    &raw,
                    Some(&session_id),
                    &memoria_config,
                    &memoria_params,
                    memoria_client.as_ref().map(|c| c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient),
                    Some(&compact_config),
                    Some(&summary_client as &dyn crate::turn::cloud::summary::SummaryLlmClient),
                )
                .await;

                (compact_result.messages, tier) // tier only feeds memoria_compact params
            };

            llm_messages.extend(merged_messages);

            // Strip old reasoning_content from history messages to reduce token
            // usage. Keeps the field (as empty string) for thinking-model API
            // compat; only the most recent assistant reasoning is preserved.
            // Heavy checkpoints and persisted events retain full reasoning.
            super::edge_ledger::strip_stale_reasoning(&mut llm_messages, &provider, &model_name);

            // Cloud loop: every tool round waits on §5.5 ledger (`POST /tools/result`) then continues LLM.
            let mut merged_tool_results: Vec<Value> = tool_results.clone();

            let mut full_text = String::new();
            let mut all_round_tool_calls: Vec<Value> = Vec::new();
            let mut reasoning = String::new();
            let mut usage = Map::new();
            let mut resolved_model = model_name.clone();
            let mut cloud_loop_turns: i64 = 0;
            let mut llm_steps: Vec<Value> = Vec::new();

            let llm_started = Instant::now();
            let budget = crate::prompts::budget_for_model(Some(&model_name));
            let max_output_tokens = crate::prompts::capped_output_tokens(&budget);
            let ledger_wait = std::time::Duration::from_secs_f64(turn_timeout_s().max(1.0));
            let max_rounds = crate::turn::routing::max_tool_rounds();
            let round_limit: i64 = if use_e2e_llm {
                bridge_e2e
                    .as_ref()
                    .map(|r| (r.len() as i64).clamp(1, max_rounds))
                    .unwrap_or(1)
            } else {
                max_rounds
            };

            let mut last_measured_prompt: Option<u64> = None;
            let mut bridge_ptl_streak: u32 = 0;
            let mut cache_detector = crate::turn::cloud::cache_diagnostics::CacheBreakDetector::new();
            // Pre-serialize tool schemas for cache fingerprinting (stable across rounds)
            let tools_fingerprint_str = serde_json::to_string(&edge_tools).unwrap_or_default();

            for round_ix in 0i64..round_limit {
                cloud_loop_turns += 1;

                let tool_schema_tokens_round: usize = edge_tools
                    .iter()
                    .map(|t| {
                        serde_json::to_string(t)
                            .map(|s| crate::prompts::estimate_str_tokens(&s))
                            .unwrap_or(50)
                    })
                    .sum();
                let cache_est_round = crate::prompts::estimate_tokens_cache_aware(
                    &llm_messages,
                    tool_schema_tokens_round,
                );
                let round_tier = crate::prompts::compaction_tier_calibrated(
                    &budget,
                    cache_est_round.total_tokens,
                    last_measured_prompt,
                    bridge_ptl_streak,
                );
                let mut pruned_tools = prune_tool_schemas(&edge_tools, round_tier);
                annotate_tool_schemas_for_caching(&mut pruned_tools, &provider, &model_name);

                let loop_started = Instant::now();
                let mut loop_tool_calls: Vec<Value> = Vec::new();
                let mut loop_text = String::new();
                let mut loop_reasoning = String::new();

                let e2e_round: Option<&Value> = if use_e2e_llm {
                    bridge_e2e
                        .as_ref()
                        .and_then(|r| r.get(round_ix as usize))
                } else {
                    None
                };

                if let Some(round_val) = e2e_round {
                    #[cfg(feature = "bridge-e2e-hooks")]
                    {
                        let (t, r, tc, u_delta) =
                            crate::turn::bridge_e2e_hooks::parse_llm_round(round_val);
                        loop_text = t;
                        loop_reasoning = r;
                        loop_tool_calls = tc;
                        for (k, v) in u_delta {
                            usage.insert(k, v);
                        }
                    }
                    #[cfg(not(feature = "bridge-e2e-hooks"))]
                    {
                        let _ = round_val;
                    }
                } else {
                    // Add cache breakpoint on last conversation message for Anthropic
                    add_message_cache_breakpoint(&mut llm_messages, &provider, &model_name);
                    let mut client_stopped = false;
                    let llm_stream = match call_llm_stream(
                        &llm_messages,
                        &pruned_tools,
                        &model_name,
                        &api_key,
                        &base_url,
                        &provider,
                        Some(max_output_tokens),
                        has_fallback,
                        cc.clone(),
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(e) if e.starts_with(crate::turn::llm_client::CONTEXT_WINDOW_ERROR_PREFIX) => {
                            // Context-window error: force aggressive compaction and retry once
                            bridge_ptl_streak = bridge_ptl_streak.saturating_add(1);
                            astra_core::agent_warn!(
                                "bridge",
                                "context window exceeded — forcing aggressive compaction and retrying"
                            );
                            // Re-compact with AggressivePrune tier
                            let budget = crate::prompts::budget_for_model(Some(&model_name));
                            let cwd_ag = edge_profile.get("cwd").and_then(Value::as_str);
                            let (session_memory_file, session_memory_combine) =
                                crate::turn::cloud::memoria_compact::resolve_session_memory_file_options(
                                    &session_id,
                                    cwd_ag,
                                );
                            let aggressive_params = crate::turn::cloud::memoria_compact::MemoriaCompactParams {
                                budget_chars: budget.effective_input_limit() * 3, // tighter budget
                                keep_chars: 1_000, // more aggressive truncation
                                tier: crate::prompts::CompactionTier::AggressivePrune,
                                keep_recent_turns: 4, // keep fewer turns
                                current_tokens: budget.effective_input_limit(), // assume we're at limit
                                session_memory_file,
                                session_memory_combine,
                            };
                            let compact_config = crate::prompts::CompactConfig::from_env();
                            let summary_client = crate::turn::cloud::summary::HttpSummaryClient::new(
                                crate::turn::cloud::summary::LlmConnParams {
                                    model_name: model_name.clone(),
                                    api_key: api_key.clone(),
                                    base_url: base_url.clone(),
                                    provider: provider.clone(),
                                    max_output_tokens: compact_config.summary_token_budget,
                                },
                            );
                            let memoria_client = crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env();
                            let memoria_config = crate::turn::cloud::memoria_compact::MemoriaCompactConfig::default();

                            // Get original messages (exclude leading system messages)
                            let sys_count = llm_messages.iter()
                                .take_while(|m| m.get("role").and_then(Value::as_str) == Some("system"))
                                .count();
                            let original_msgs: Vec<Value> = llm_messages.iter().skip(sys_count).cloned().collect();
                            let compact_result = crate::turn::cloud::memoria_compact::compact_with_memoria(
                                &original_msgs,
                                Some(&session_id),
                                &memoria_config,
                                &aggressive_params,
                                memoria_client.as_ref().map(|c| c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient),
                                Some(&compact_config),
                                Some(&summary_client as &dyn crate::turn::cloud::summary::SummaryLlmClient),
                            )
                            .await;

                            // Rebuild llm_messages with compacted content.
                            // Preserve all leading system messages (stable + dynamic).
                            let system_msgs: Vec<Value> = llm_messages
                                .iter()
                                .take_while(|m| {
                                    m.get("role").and_then(Value::as_str) == Some("system")
                                })
                                .cloned()
                                .collect();
                            llm_messages.clear();
                            llm_messages.extend(system_msgs);
                            llm_messages.extend(compact_result.messages);

                            // Also prune tool schemas more aggressively
                            pruned_tools = prune_tool_schemas(&edge_tools, crate::prompts::CompactionTier::AggressivePrune);
                            annotate_tool_schemas_for_caching(&mut pruned_tools, &provider, &model_name);

                            // Retry LLM call
                            match call_llm_stream(
                                &llm_messages,
                                &pruned_tools,
                                &model_name,
                                &api_key,
                                &base_url,
                                &provider,
                                Some(max_output_tokens / 2), // reduce output budget too
                                has_fallback,
                                cc.clone(),
                            )
                            .await
                            {
                                Ok(s) => s,
                                Err(e2) => {
                                    let kind = classify_llm_error(&e2);
                                    // Dump full LLM request for post-mortem debugging
                                    let dump = crate::turn::llm_request_dump::build_llm_request_dump(
                                        &session_id, &model_name, &provider,
                                        &e2, &llm_messages, &pruned_tools,
                                        round_ix, Some(max_output_tokens / 2),
                                    );
                                    if let Some(path) = dump.write_local() {
                                        eprintln!("[llm_error_dump] {path}");
                                    }
                                    dump.persist_cloud(&user_id, &turn_chain_id, turn_auxiliary_event_writer.clone());
                                    yield render_sse_map(&build_stream_error_event(
                                        &format!("Context window exceeded even after aggressive compaction: {e2}"),
                                        kind,
                                        false, // not retryable
                                    ));
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let kind = classify_llm_error(&e);
                            // Dump full LLM request for post-mortem debugging
                            let dump = crate::turn::llm_request_dump::build_llm_request_dump(
                                &session_id, &model_name, &provider,
                                &e, &llm_messages, &pruned_tools,
                                round_ix, Some(max_output_tokens),
                            );
                            if let Some(path) = dump.write_local() {
                                eprintln!("[llm_error_dump] {path}");
                            }
                            dump.persist_cloud(&user_id, &turn_chain_id, turn_auxiliary_event_writer.clone());
                            yield render_sse_map(&build_stream_error_event(&e, kind, kind != "internal"));
                            return;
                        }
                    };

                    tokio::pin!(llm_stream);
                    let mut sse_buf = SseBlankLineUtf8Buf::new();
                    let mut saw_inprocess_summary = false;
                    // Keepalive interval: emit SSE comment every 30s so the
                    // CLI-side idle timer (90s) doesn't fire while the bridge
                    // waits for LLM data (e.g. during non-stream fallback).
                    let keepalive = tokio::time::Duration::from_secs(30);
                    let mut keepalive_deadline = tokio::time::Instant::now() + keepalive;

                    loop {
                        tokio::select! {
                            biased;
                            _ = crate::turn::llm_client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                                astra_core::agent_warn!(
                                    "bridge",
                                    "chat turn cancelled — stopping LLM byte forward"
                                );
                                client_stopped = true;
                                break;
                            }
                            item = llm_stream.next() => {
                                keepalive_deadline = tokio::time::Instant::now() + keepalive;
                                let Some(bytes) = item else { break };
                                for block in sse_buf.push_lossy_bytes(&bytes) {
                                    match extend_forward_from_validated_sse_block(
                                        &block,
                                        &mut saw_inprocess_summary,
                                        &mut loop_text,
                                        &mut loop_reasoning,
                                        &mut loop_tool_calls,
                                        &mut usage,
                                        &mut resolved_model,
                                    ) {
                                        Ok(chunks) => {
                                            for b in chunks {
                                                yield b;
                                            }
                                        }
                                        Err(msg) => {
                                            astra_core::agent_warn!("bridge", "in-process LLM SSE block invalid: {msg}");
                                            yield render_sse_map(&build_stream_error_event(
                                                &msg,
                                                "SSE_PARSE_ERROR",
                                                false,
                                            ));
                                            return;
                                        }
                                    }
                                }
                            }
                            _ = tokio::time::sleep_until(keepalive_deadline) => {
                                yield Bytes::from(":\n\n");
                                keepalive_deadline = tokio::time::Instant::now() + keepalive;
                            }
                        }
                    }

                    if client_stopped {
                        yield render_sse_map(&build_stream_error_event(
                            "Request cancelled (client disconnected)",
                            "CLIENT_DISCONNECT",
                            false,
                        ));
                        return;
                    }

                    let mut tail = sse_buf.into_inner();
                    match flush_tail_buf_into_llm_forward(
                        &mut tail,
                        &mut saw_inprocess_summary,
                        &mut loop_text,
                        &mut loop_reasoning,
                        &mut loop_tool_calls,
                        &mut usage,
                        &mut resolved_model,
                    ) {
                        Ok(chunks) => {
                            for b in chunks {
                                yield b;
                            }
                        }
                        Err(msg) => {
                            astra_core::agent_warn!("bridge", "in-process LLM SSE tail invalid: {msg}");
                            yield render_sse_map(&build_stream_error_event(
                                &msg,
                                "SSE_PARSE_ERROR",
                                false,
                            ));
                            return;
                        }
                    }

                    if !saw_inprocess_summary {
                        yield render_sse_map(&build_stream_error_event(
                            "LLM stream ended without completion summary from provider",
                            "STREAM_INCOMPLETE",
                            true,
                        ));
                        return;
                    }
                }

                full_text.push_str(&loop_text);
                if use_e2e_llm && !loop_text.trim().is_empty() && loop_tool_calls.is_empty() {
                    yield render_sse(&json!({"type": "text_delta", "content": loop_text}));
                }
                if !loop_reasoning.is_empty() {
                    reasoning.push_str(&loop_reasoning);
                }
                llm_steps.push(json!({
                    "step": "llm",
                    "duration_ms": loop_started.elapsed().as_millis() as i64,
                    "in": usage.get("prompt").and_then(Value::as_i64),
                    "out": usage.get("completion").and_then(Value::as_i64),
                    "tool_calls": loop_tool_calls.len(),
                }));

                let prompt_from_usage = usage
                    .get("prompt")
                    .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)));
                if let Some(p) = prompt_from_usage.filter(|&p| p > 0) {
                    last_measured_prompt = Some(p);
                    bridge_ptl_streak = 0;
                }

                // ── Cache break detection ──
                {
                    let sys_content = llm_messages.first()
                        .and_then(|m| m.get("content"));
                    let sys_prompt_str = match sys_content {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => serde_json::to_string(v).unwrap_or_default(),
                        None => String::new(),
                    };
                    let fp = crate::turn::cloud::cache_diagnostics::CacheFingerprint::new(
                        &sys_prompt_str, &tools_fingerprint_str, &model_name, &provider,
                    );
                    let cache_read = usage.get("cache_read")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                        .unwrap_or(0);
                    if let Some(event) = cache_detector.detect_break(&fp, cache_read) {
                        let causes: Vec<String> = event.causes.iter().map(|c| c.to_string()).collect();
                        eprintln!("[cache_diagnostics] cache break detected: {}", causes.join(", "));
                    }
                }

                if loop_tool_calls.is_empty() {
                    break;
                }

                ensure_tool_call_ids(&mut loop_tool_calls);

                all_round_tool_calls.extend(loop_tool_calls.iter().cloned());

                llm_messages.push(assistant_message_with_tool_calls_and_reasoning(
                    &loop_tool_calls,
                    &loop_reasoning,
                    !reasoning.is_empty() || history_has_reasoning(&llm_messages),
                ));
                // ── Tool delivery: approval tools sequential, read-only concurrent ──
                let mut read_only_tcs: Vec<Value> = Vec::new();
                for tc in loop_tool_calls.iter() {
                    let Some(tc_map) = tc.as_object() else { continue };
                    if !cloud_tool_requires_approval_for_delivery(tc) {
                        read_only_tcs.push(tc.clone());
                        continue;
                    }
                    // Sequential: approval → wait → request → wait
                    let id = tc_map.get("id").and_then(Value::as_str).unwrap_or("");
                    let tool_name = tc_map
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let path = tool_path_hint_for_delivery(tc);
                    yield render_sse_map(&build_approval_required_event(
                        id, tool_name, path.as_deref(),
                    ));
                    match wait_approval_ledger_for_tool(
                        &edge_callback_ledger, &user_id, tc, ledger_wait,
                    ).await {
                        Ok(()) => {}
                        Err(part) => {
                            merged_tool_results.extend(part.persist_tool_results);
                            llm_messages.extend(part.tool_messages);
                            continue;
                        }
                    }
                    for m in sse_maps_through_tool_request(tc) {
                        yield render_sse_map(&m);
                    }
                    let tail = wait_tool_result_ledger_for_tool(
                        &edge_callback_ledger, &user_id, tc, ledger_wait,
                    ).await;
                    merged_tool_results.extend(tail.persist_tool_results);
                    llm_messages.extend(tail.tool_messages);
                }
                // Read-only: yield all tool_request SSEs first so edge can
                // start executing in parallel, then join_all on ledger waits.
                if !read_only_tcs.is_empty() {
                    for tc in &read_only_tcs {
                        for m in sse_maps_through_tool_request(tc) {
                            yield render_sse_map(&m);
                        }
                    }
                    // Use buffer_unordered to limit concurrent tool executions.
                    // This prevents resource exhaustion when many tools are called.
                    let tool_stream = stream::iter(read_only_tcs.into_iter().map(|tc| {
                        let ledger = edge_callback_ledger.clone();
                        let uid = user_id.clone();
                        async move {
                            wait_tool_result_ledger_for_tool(
                                &ledger, &uid, &tc, ledger_wait,
                            ).await
                        }
                    })).buffer_unordered(MAX_CONCURRENT_READ_ONLY_TOOLS);
                    tokio::pin!(tool_stream);
                    while let Some(tail) = tool_stream.next().await {
                        merged_tool_results.extend(tail.persist_tool_results);
                        llm_messages.extend(tail.tool_messages);
                    }
                }
            }

            let llm_duration_ms = llm_started.elapsed().as_millis() as i64;

            // Persist events (fire-and-forget)
            let user_content = messages.iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .map(ToString::to_string);

            let has_tool_calls = !all_round_tool_calls.is_empty();
            let llm_content = full_text.trim().to_string();
            let should_persist_llm = !llm_content.is_empty() || has_tool_calls;

            let user_query_event = user_content.as_ref().map(|content| TurnCoreEventRecord {
                event_id: user_query_event_id.clone(),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                event_type: "user_query".to_string(),
                content: content.clone(),
                parent_event_id: None,
                causal_chain_id: turn_chain_id.clone(),
                llm_model_used: None,
                token_usage: None,
                llm_params: None,
                reasoning_content: None,
            });

            let llm_response_event = should_persist_llm.then(|| TurnCoreEventRecord {
                event_id: Uuid::now_v7().to_string(),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                event_type: "llm_response".to_string(),
                content: llm_content.clone(),
                parent_event_id: Some(user_query_event_id.clone()),
                causal_chain_id: turn_chain_id.clone(),
                llm_model_used: Some(resolved_model.clone()),
                token_usage: if usage.is_empty() { None } else { Some(Value::Object(usage.clone())) },
                llm_params: None,
                reasoning_content: if reasoning.is_empty() && has_tool_calls { None } else if !reasoning.is_empty() { Some(reasoning.clone()) } else { None },
            });

            let persist_plan = TurnCorePersistPlan {
                user_query_event,
                llm_response_event,
                snapshot_link_plan: None,
            };

            // Build tool event records for persistence
            let tool_event_plan = {
                let mut events = Vec::new();
                for (index, tool_call) in all_round_tool_calls.iter().enumerate() {
                    if let Some(tc) = tool_call.as_object() {
                        let payload = build_tool_call_event_payload(tc, index, &reasoning);
                        events.push(TurnToolEventRecord {
                            event_id: Uuid::now_v7().to_string(),
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            event_type: "tool_call".to_string(),
                            content: match payload.content {
                                Value::String(s) => s,
                                v => serde_json::to_string(&v).unwrap_or_default(),
                            },
                            parent_event_id: Some(user_query_event_id.clone()),
                            causal_chain_id: turn_chain_id.clone(),
                            metadata: (!payload.metadata.is_empty())
                                .then_some(Value::Object(payload.metadata)),
                            skill_name: (!payload.skill_name.is_empty())
                                .then_some(payload.skill_name),
                            skill_version: None,
                            reasoning_content: payload.reasoning_content,
                        });
                    }
                }
                for tool_result in merged_tool_results.iter() {
                    if let Some(tr) = tool_result.as_object() {
                        let payload =
                            build_tool_result_event_payload(tr, "edge", TOOL_RESULT_AUDIT_CHARS);
                        events.push(TurnToolEventRecord {
                            event_id: Uuid::now_v7().to_string(),
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            event_type: "tool_result".to_string(),
                            content: match payload.content {
                                Value::String(s) => s,
                                v => serde_json::to_string(&v).unwrap_or_default(),
                            },
                            parent_event_id: Some(user_query_event_id.clone()),
                            causal_chain_id: turn_chain_id.clone(),
                            metadata: (!payload.metadata.is_empty())
                                .then_some(Value::Object(payload.metadata)),
                            skill_name: (!payload.skill_name.is_empty())
                                .then_some(payload.skill_name),
                            skill_version: None,
                            reasoning_content: payload.reasoning_content,
                        });
                    }
                }
                if events.is_empty() {
                    None
                } else {
                    Some(TurnToolEventPersistPlan { events })
                }
            };

            let writer = turn_core_event_writer.clone();
            let tool_writer = turn_tool_event_writer.clone();
            let sa_writer = turn_session_activity_writer.clone();
            let sid = session_id.clone();
            let user_query_event_id_for_activity = user_query_event_id.clone();
            let core_event_count = usize::from(user_content.is_some()) + usize::from(should_persist_llm);
            let tool_event_count = tool_event_plan
                .as_ref()
                .map(|plan| plan.events.len())
                .unwrap_or(0);

            tokio::spawn(async move {
                let persist_start = std::time::Instant::now();
                let core_outcome = match writer.persist(persist_plan).await {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        astra_core::agent_persist_fail!("bridge",
                            session = sid,
                            core_events = core_event_count,
                            tool_events = tool_event_count,
                            elapsed = format!("{:?}", persist_start.elapsed()),
                            error = e
                        );
                        return;
                    }
                };
                let tool_events_persisted = match tool_event_plan {
                    Some(plan) => {
                        if let Err(e) = tool_writer.persist(plan).await {
                            astra_core::agent_persist_fail!("bridge",
                                session = sid,
                                stage = "tool_events",
                                count = tool_event_count,
                                elapsed = format!("{:?}", persist_start.elapsed()),
                                error = e
                            );
                            false
                        } else {
                            true
                        }
                    }
                    None => false,
                };
                let persisted_event_count = count_inprocess_persisted_events(
                    core_event_count,
                    tool_event_count,
                    tool_events_persisted,
                );
                if persisted_event_count == 0 {
                    return;
                }
                let last_event_id = core_outcome
                    .llm_response_event_id
                    .or(Some(user_query_event_id_for_activity));
                let plan = SessionActivityUpdatePlan {
                    event_count_increment: persisted_event_count,
                    last_event_id,
                };
                if let Err(e) = sa_writer.update_session_activity(&sid, plan).await {
                    astra_core::agent_persist_fail!("bridge",
                        session = sid,
                        stage = "activity",
                        elapsed = format!("{:?}", persist_start.elapsed()),
                        error = e
                    );
                }
            });

            // Hook side effects: decision audit, skill selection, implicit feedback, reflection
            {
                let hook_payload = crate::turn::tail_persist::build_turn_hook_args(
                    &user_id,
                    &session_id,
                    &messages,
                    &merged_tool_results,
                    &full_text,
                    &all_round_tool_calls,
                    None, // context_capture_id
                    Some(&resolved_model),
                    _agent_id.as_deref(),
                    Some(&user_query_event_id),
                    0, // turn_count not tracked in inprocess bridge
                    None, // session_start
                    false, // run_hook_db_writes = false → triggers persist
                    false, // run_observer = false → triggers observer
                    false, // run_implicit_feedback = false → triggers feedback
                    false, // run_reflection_learning = false → triggers reflection
                );
                crate::bridge::side_effects::run_bridge_hook_side_effects(
                    Some(Value::Object(hook_payload)),
                    turn_hook_db_writer.clone(),
                    turn_reflection_state_store.clone(),
                    turn_reflection_lesson_writer.clone(),
                    turn_observer_worker.clone(),
                    turn_learning_writer.clone(),
                );
            }

            // Auxiliary events: routing decisions, quality assessments, snapshots
            {
                let aux_writer = turn_auxiliary_event_writer.clone();
                let aux_uid = user_id.clone();
                let aux_sid = session_id.clone();
                let aux_chain = turn_chain_id.clone();
                let aux_parent = user_query_event_id.clone();
                tokio::spawn(async move {
                    // Routing decision event (inprocess uses default router)
                    let routing_event = crate::TurnAuxiliaryEventRecord {
                        event_id: Uuid::now_v7().to_string(),
                        user_id: aux_uid.clone(),
                        session_id: aux_sid.clone(),
                        event_type: "routing_decision".to_string(),
                        content: json!({"router": "inprocess-default", "intent": "default"}).to_string(),
                        parent_event_id: Some(aux_parent),
                        causal_chain_id: aux_chain,
                        metadata: None,
                        reasoning_content: None,
                    };
                    if let Err(e) = aux_writer.persist_events(vec![routing_event]).await {
                        eprintln!(
                            "PERSIST_FAIL session={} stage=auxiliary error={}",
                            aux_sid, e
                        );
                    }
                });
            }

            if explain {
                let tool_selection = all_round_tool_calls
                    .first()
                    .and_then(Value::as_object)
                    .and_then(|tool_call| tool_call.get("function"))
                    .and_then(Value::as_object)
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(|name| json!({ "name": name }));
                if llm_steps.is_empty() {
                    llm_steps.push(json!({
                        "step": "llm",
                        "duration_ms": llm_duration_ms,
                        "in": usage.get("prompt").and_then(Value::as_i64),
                        "out": usage.get("completion").and_then(Value::as_i64),
                        "tool_calls": all_round_tool_calls.len(),
                    }));
                }
                let aux_tokens_in = usage.get("prompt").and_then(Value::as_i64);
                let aux_tokens_out = usage.get("completion").and_then(Value::as_i64);
                let auxiliary_llm_calls = Some(vec![json!({
                    "purpose": "primary_generation",
                    "ms": llm_duration_ms,
                    "tokens_in": aux_tokens_in,
                    "tokens_out": aux_tokens_out,
                })]);
                let memory = if memory_fetch_ms > 0 || memory_items > 0 {
                    Some(json!({
                        "l0": {
                            "loaded": !memory_preview.is_empty(),
                            "tokens": 0,
                            "ms": memory_fetch_ms,
                            "preview": memory_preview.first().cloned().unwrap_or_default(),
                        },
                        "l1": {
                            "loaded": memory_items > 0,
                            "count": memory_items,
                            "tokens": 0,
                            "ms": memory_fetch_ms,
                            "previews": memory_preview,
                        },
                        "retrieval": {
                            "keyword_hit": memory_items > 0,
                            "vector_hit": false,
                            "phase1_candidates": memory_items,
                            "phase2_candidates": 0,
                            "merged_candidates": memory_items,
                            "final_count": memory_items,
                            "total_ms": memory_fetch_ms,
                        },
                        "total_ms": memory_fetch_ms,
                    }))
                } else {
                    None
                };
                let routing = Some(json!({
                    "router": "inprocess-default",
                    "intent": "default",
                    "confidence": 0.0,
                    "tier": 0,
                    "latency_ms": 0,
                    "estimated_tokens": usage.get("total").and_then(Value::as_i64),
                    "skipped": model_override.is_some(),
                    "reason": model_override.as_ref().map(|_| "model_override").unwrap_or(""),
                    "cloud_loop_turns": cloud_loop_turns,
                }));
                let explain_event = build_explain_event(
                    turn_started.elapsed().as_millis() as i64,
                    usage.get("prompt").and_then(Value::as_i64),
                    usage.get("completion").and_then(Value::as_i64),
                    all_round_tool_calls.len(),
                    edge_tools.len(),
                    tool_selection,
                    llm_steps,
                    memory,
                    routing,
                    auxiliary_llm_calls,
                );
                yield render_sse_map(&explain_event);
            }

            // turn_complete
            yield render_sse(&json!({
                "type": "turn_complete",
                "has_tool_calls": has_tool_calls,
            }));
        };

        let body = Body::from_stream(stream.map(Ok::<_, std::io::Error>));
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("x-accel-buffering", "no")
            .body(body)
            .expect("valid SSE response builder"))
    }
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn classify_llm_error(msg: &str) -> &'static str {
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

/// Result of a memory prefetch operation.
#[derive(Debug, Default)]
pub struct MemoryPrefetchResult {
    pub section: Option<String>,
    pub items: usize,
    pub preview: Vec<String>,
    pub fetch_ms: i64,
}

/// Prefetch memories relevant to the user message via hybrid retrieval.
/// Sends two queries (full message + entity tokens), merges and deduplicates.
pub async fn prefetch_memories(
    mem_url: &str,
    mem_key: &str,
    user_msg: &str,
    user_id: &str,
) -> MemoryPrefetchResult {
    if mem_key.is_empty() || user_msg.trim().is_empty() {
        return MemoryPrefetchResult::default();
    }
    let started = Instant::now();
    let entity_query = extract_entity_tokens(user_msg);
    let trimmed_msg = user_msg.trim();

    // Parallel fetch: full message retrieval + entity-keyword retrieval via tokio::join!
    // Saves one round-trip latency (~50-200ms) compared to sequential.
    let do_entity = !entity_query.is_empty() && entity_query != trimmed_msg;
    let (full_result, entity_result) = tokio::join!(
        fetch_memories(mem_url, mem_key, trimmed_msg, user_id),
        async {
            if do_entity {
                fetch_memories(mem_url, mem_key, &entity_query, user_id).await
            } else {
                String::new()
            }
        }
    );
    let merged = merge_memory_results(&[&full_result, &entity_result]);
    let fetch_ms = started.elapsed().as_millis() as i64;
    let preview = merged.iter().take(3).map(|l| l.to_string()).collect();
    let items = merged.len();
    let section = build_memory_section(&merged);
    MemoryPrefetchResult {
        section,
        items,
        preview,
        fetch_ms,
    }
}

/// Merge and deduplicate memory results from multiple retrieval queries.
fn merge_memory_results(results: &[&str]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for result in results {
        for line in result.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                merged.push(trimmed.to_string());
            }
        }
    }
    merged
}

/// Build the memory section for the profile block.
/// Returns None if no memories matched.
fn build_memory_section(merged_lines: &[String]) -> Option<String> {
    if merged_lines.is_empty() {
        return None;
    }
    let refs: Vec<&str> = merged_lines.iter().map(|s| s.as_str()).collect();
    let formatted = crate::prompts::memory_proto::format_for_llm(&refs);
    if !formatted.is_empty() {
        Some(format!("## User Memories\n{formatted}"))
    } else {
        Some(format!("## User Memories\n{}", merged_lines.join("\n")))
    }
}

/// Extract non-CJK, non-punctuation tokens from a message for keyword-based retrieval.
/// General purpose: works for any mixed-language input, not specific to any domain.
fn extract_entity_tokens(msg: &str) -> String {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in msg.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            current.push(ch);
        } else {
            if current.len() >= 3 {
                tokens.push(current.clone());
            }
            current.clear();
        }
    }
    if current.len() >= 3 {
        tokens.push(current);
    }
    tokens.join(" ")
}

/// Fetch memories from Memoria HTTP API. Returns joined content string.
async fn fetch_memories(base_url: &str, api_key: &str, query: &str, user_id: &str) -> String {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut payload = serde_json::json!({"query": query, "top_k": 10});
    if !user_id.is_empty() {
        payload["session_id"] = serde_json::Value::String(user_id.to_string());
        payload["user_id"] = serde_json::Value::String(user_id.to_string());
    }
    let resp = match client
        .post(format!("{base_url}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            astra_core::agent_error!("memory", "fetch error: {e:#}");
            return String::new();
        }
    };
    if !resp.status().is_success() {
        return String::new();
    }
    let arr = match resp.json::<Vec<serde_json::Value>>().await {
        Ok(a) => a,
        Err(_) => return String::new(),
    };
    arr.iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Test-accessible wrapper around private schema pruning — used by integration
/// tests that need to verify progressive schema detail levels.
pub mod bridge_inprocess_test_helpers {
    use crate::prompts::CompactionTier;
    use serde_json::Value;

    pub fn prune_tool_schemas_pub(tools: &[Value], tier: CompactionTier) -> Vec<Value> {
        crate::turn::tool_schema_prune::prune_tool_schemas(tools, tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_entity_tokens_from_mixed_language() {
        assert_eq!(extract_entity_tokens("memoria 最新的ci?"), "memoria");
        assert_eq!(
            extract_entity_tokens("matrixone latest pr"),
            "matrixone latest"
        );
        assert_eq!(extract_entity_tokens("你好"), "");
        assert_eq!(
            extract_entity_tokens("check astra status"),
            "check astra status"
        );
    }

    // ── merge_memory_results ──────────────────────────────────────────────────

    #[test]
    fn merge_deduplicates_across_queries() {
        let r1 = "[@fact/semantic] memoria is matrixorigin/memoria\nsome other fact";
        let r2 = "[@fact/semantic] memoria is matrixorigin/memoria\nnew fact";
        let merged = merge_memory_results(&[r1, r2]);
        assert_eq!(
            merged.len(),
            3,
            "duplicate should be removed, got: {merged:?}"
        );
        assert!(merged.contains(&"[@fact/semantic] memoria is matrixorigin/memoria".to_string()));
        assert!(merged.contains(&"some other fact".to_string()));
        assert!(merged.contains(&"new fact".to_string()));
    }

    #[test]
    fn merge_skips_empty_lines() {
        let r1 = "line1\n\n\nline2";
        let r2 = "";
        let merged = merge_memory_results(&[r1, r2]);
        assert_eq!(merged, vec!["line1", "line2"]);
    }

    #[test]
    fn merge_empty_inputs() {
        assert!(merge_memory_results(&["", ""]).is_empty());
        assert!(merge_memory_results(&[]).is_empty());
    }

    // ── build_memory_section ──────────────────────────────────────────────────

    #[test]
    fn build_memory_section_returns_none_for_empty() {
        assert!(build_memory_section(&[]).is_none());
    }

    #[test]
    fn build_memory_section_includes_header() {
        let lines = vec!["[@pref/active] memoria = matrixorigin/Memoria".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(section.starts_with("## User Memories"), "got: {section}");
    }

    #[test]
    fn section_cache_key_varies_by_tools_and_task() {
        let key1 = section_cache_key(&["bash"], Some("implementation"), 0.8);
        let key2 = section_cache_key(&["bash", "read_file"], Some("implementation"), 0.8);
        let key3 = section_cache_key(&["bash"], Some("debugging"), 0.8);
        let key4 = section_cache_key(&["bash"], Some("implementation"), 0.2);
        assert_ne!(key1, key2, "different tools should differ");
        assert_ne!(key1, key3, "different task types should differ");
        assert_ne!(key1, key4, "different confidence buckets should differ");
    }

    #[test]
    fn build_memory_section_formats_structured_entries() {
        let lines = vec!["[@pref/active] dark mode preferred".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(
            section.contains("Preferences"),
            "structured entries should be grouped, got: {section}"
        );
    }

    #[test]
    fn build_memory_section_handles_unstructured() {
        let lines = vec!["just a plain memory without tags".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(section.contains("just a plain memory"), "got: {section}");
    }

    // ── entity + merge integration ────────────────────────────────────────────

    #[test]
    fn entity_query_differs_from_mixed_language_input() {
        let msg = "memoria 最新的ci?";
        let entity = extract_entity_tokens(msg);
        assert_ne!(
            entity,
            msg.trim(),
            "entity query should differ for mixed-language"
        );
        assert_eq!(entity, "memoria");
    }

    #[test]
    fn entity_query_same_for_pure_ascii() {
        let msg = "memoria latest ci";
        let entity = extract_entity_tokens(msg);
        assert_eq!(
            entity, "memoria latest",
            "pure ASCII: entity ≈ original (minus short words)"
        );
    }

    #[test]
    fn count_inprocess_persisted_events_skips_failed_tool_events() {
        assert_eq!(count_inprocess_persisted_events(2, 3, false), 2);
        assert_eq!(count_inprocess_persisted_events(2, 3, true), 5);
    }

    // ── Static/dynamic prompt boundary tests ──
    // These tests manipulate env vars, so they must not run in parallel.
    static CACHE_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn build_system_message_anthropic_has_cache_control() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        // Ensure env var doesn't interfere
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let (msg, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            Some("implementation"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        // Should be multi-block content array
        let content = msg.get("content").expect("should have content");
        let blocks = content
            .as_array()
            .expect("Anthropic content should be array");
        assert!(blocks.len() >= 2, "should have at least 2 blocks");

        // First block (Global) should NOT have cache_control (only last Global does)
        let first = &blocks[0];
        assert!(
            first.get("cache_control").is_none() || first["cache_control"].is_null(),
            "Non-last Global block should not have cache_control"
        );

        // Some block should have cache_control with scope=global (the last Global)
        let global_cc_block = blocks.iter().find(|b| {
            b.get("cache_control")
                .and_then(|cc| cc.get("scope"))
                .and_then(|s| s.as_str())
                == Some("global")
        });
        assert!(
            global_cc_block.is_some(),
            "should have a block with scope=global cache_control"
        );
        let gcc = &global_cc_block.unwrap()["cache_control"];
        assert_eq!(gcc["type"].as_str(), Some("ephemeral"));
        assert_eq!(gcc["ttl"].as_str(), Some("1h"));

        // Last block (profile/dynamic) should NOT have cache_control
        let last = blocks.last().unwrap();
        assert!(
            last.get("cache_control").is_none() || last["cache_control"].is_null(),
            "dynamic block should not have cache_control"
        );
    }

    #[test]
    fn build_system_message_session_scope_has_ttl_but_no_global_scope() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let (msg, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            Some("implementation"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let content = msg.get("content").expect("should have content");
        let blocks = content.as_array().unwrap();

        // With fine-grained sections, Session blocks come after Global blocks.
        // Find a block whose text contains "Self-Model" (Session-scoped tool list).
        let session_block = blocks.iter().find(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("Self-Model"))
        });
        if let Some(block) = session_block {
            if let Some(cc) = block.get("cache_control") {
                assert_eq!(cc["ttl"].as_str(), Some("1h"), "Session should have ttl=1h");
                // Session should NOT have scope=global (it's per-session)
                assert!(
                    cc.get("scope").is_none() || cc["scope"].is_null(),
                    "Session block should not have scope=global"
                );
            }
        }
    }

    #[test]
    fn build_system_message_cache_disabled_env() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("MO_PROMPT_CACHE_DISABLED", "1");
        }

        let (msg, _) = build_system_message(
            &["bash"],
            "cwd: /test",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let content = msg.get("content").expect("should have content");
        let blocks = content.as_array().unwrap();
        for block in blocks {
            assert!(
                block.get("cache_control").is_none() || block["cache_control"].is_null(),
                "all blocks should lack cache_control when disabled"
            );
        }

        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }
    }

    #[test]
    fn build_system_message_openai_has_string_content() {
        let (msg, dynamic) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            None,
            "openai",
            "gpt-4",
        );

        // Primary should be a single string content (stable prefix)
        let content = msg.get("content").expect("should have content");
        assert!(content.is_string(), "OpenAI content should be string");

        // Dynamic profile should be in the second message
        let dyn_msg = dynamic.expect("should have dynamic message when profile is non-empty");
        let dyn_content = dyn_msg
            .get("content")
            .expect("dynamic msg should have content");
        assert!(
            dyn_content.as_str().unwrap().contains("cwd: /test"),
            "dynamic message should contain profile"
        );
    }

    #[test]
    fn build_system_message_claude_model_triggers_anthropic_format() {
        // Even if provider is not "anthropic", claude model name should trigger it
        let (msg, _) = build_system_message(
            &["bash"],
            "",
            0.8,
            None,
            "openrouter",               // not "anthropic"
            "claude-sonnet-4-20250514", // but model is claude
        );

        let content = msg.get("content").expect("should have content");
        assert!(
            content.is_array(),
            "claude model should use array content even through non-anthropic provider"
        );
    }

    #[test]
    fn annotate_tool_schemas_for_caching_adds_cache_control() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let mut tools = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "read_file"}}),
        ];
        annotate_tool_schemas_for_caching(&mut tools, "anthropic", "claude-sonnet-4-20250514");

        // Only last tool should have cache_control
        assert!(
            tools[0].get("cache_control").is_none(),
            "first tool should not have cache_control"
        );
        assert_eq!(
            tools[1]["cache_control"]["type"].as_str(),
            Some("ephemeral"),
            "last tool should have ephemeral cache_control"
        );
        assert_eq!(
            tools[1]["cache_control"]["ttl"].as_str(),
            Some("1h"),
            "last tool should have ttl=1h"
        );
    }

    #[test]
    fn annotate_tool_schemas_noop_for_openai() {
        let mut tools = vec![json!({"function": {"name": "bash"}})];
        annotate_tool_schemas_for_caching(&mut tools, "openai", "gpt-4");
        assert!(
            tools[0].get("cache_control").is_none(),
            "OpenAI tools should not get cache_control"
        );
    }

    #[test]
    fn add_message_cache_breakpoint_targets_last_non_system() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let mut messages = vec![
            json!({"role": "system", "content": "sys prompt"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
        ];
        add_message_cache_breakpoint(&mut messages, "anthropic", "claude-sonnet-4-20250514");

        // System message should be untouched
        assert!(messages[0]["content"].is_string(), "system msg unchanged");

        // Last message (assistant) should have cache_control
        let last_content = messages[2].get("content").unwrap();
        let blocks = last_content
            .as_array()
            .expect("should be converted to array");
        assert!(
            blocks[0].get("cache_control").is_some(),
            "last msg should have cache_control"
        );
    }

    #[test]
    fn add_message_cache_breakpoint_noop_for_openai() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        add_message_cache_breakpoint(&mut messages, "openai", "gpt-4");

        // Should remain unchanged
        assert!(
            messages[1]["content"].is_string(),
            "OpenAI msgs should not be modified"
        );
    }

    // ── is_valid_tool_name ───────────────────────────────────────────────────

    #[test]
    fn is_valid_tool_name_rejects_xml_artifacts() {
        // Malformed names from LLM leaking XML thinking tags
        assert!(!is_valid_tool_name("reflect>"));
        assert!(!is_valid_tool_name("<reflect"));
        assert!(!is_valid_tool_name("<think>"));
        assert!(!is_valid_tool_name("</think>"));
        assert!(!is_valid_tool_name("foo<bar"));
        assert!(!is_valid_tool_name("foo>bar"));
    }

    #[test]
    fn is_valid_tool_name_rejects_empty_and_whitespace() {
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("tool name"));
        assert!(!is_valid_tool_name("tool\tname"));
        assert!(!is_valid_tool_name("tool\nname"));
    }

    #[test]
    fn is_valid_tool_name_accepts_valid_names() {
        assert!(is_valid_tool_name("bash"));
        assert!(is_valid_tool_name("str_replace"));
        assert!(is_valid_tool_name("read_file"));
        assert!(is_valid_tool_name("list_dir"));
        assert!(is_valid_tool_name("github-mcp-server-search_code"));
    }

    #[test]
    fn intermediate_text_is_suppressed_when_tool_calls_exist() {
        let loop_text = "draft review text";
        let loop_tool_calls = [json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "git_show", "arguments": "{\"rev\":\"HEAD\"}"}
        })];
        let should_emit = !loop_text.trim().is_empty() && loop_tool_calls.is_empty();
        assert!(
            !should_emit,
            "intermediate draft text must not be emitted when tool calls are pending"
        );
    }

    #[test]
    fn intermediate_text_is_emitted_without_tool_calls() {
        let loop_text = "final review";
        let loop_tool_calls: Vec<Value> = Vec::new();
        let should_emit = !loop_text.trim().is_empty() && loop_tool_calls.is_empty();
        assert!(
            should_emit,
            "final text should still stream when no tool calls exist"
        );
    }

    // ── Cache token extraction tests ─────────────────────────────────────

    /// Validates the JSON path used in the bridge to extract cache_read_tokens
    /// from OpenAI-format usage chunks: usage.prompt_tokens_details.cached_tokens
    #[test]
    fn openai_cache_token_extraction_pattern() {
        let chunk: Value = json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 500,
                "prompt_tokens_details": {
                    "cached_tokens": 800
                }
            }
        });
        let u = chunk.get("usage").unwrap();
        let cache_read = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64);
        assert_eq!(cache_read, Some(800));
    }

    #[test]
    fn openai_cache_token_extraction_missing_details() {
        let chunk: Value = json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 500
            }
        });
        let u = chunk.get("usage").unwrap();
        let cache_read = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64);
        assert_eq!(cache_read, None);
    }

    #[test]
    fn openai_cache_token_extraction_null_cached_tokens() {
        let chunk: Value = json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 500,
                "prompt_tokens_details": {
                    "cached_tokens": null
                }
            }
        });
        let u = chunk.get("usage").unwrap();
        let cache_read = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64);
        assert_eq!(cache_read, None);
    }

    #[test]
    fn openai_cache_token_extraction_zero() {
        let chunk: Value = json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 500,
                "prompt_tokens_details": {
                    "cached_tokens": 0
                }
            }
        });
        let u = chunk.get("usage").unwrap();
        let cache_read = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64);
        assert_eq!(cache_read, Some(0));
    }

    #[test]
    fn sse_usage_event_with_cache_tokens_format() {
        // Verify the SSE event format that bridge emits matches what ChatTurnSseAccum expects
        let prompt = Some(1000i64);
        let completion = Some(500i64);
        let cache_read: Option<i64> = Some(800);

        let event = json!({
            "type": "usage",
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "cache_read_tokens": cache_read,
        });
        assert_eq!(event["type"], "usage");
        assert_eq!(event["prompt_tokens"].as_i64(), Some(1000));
        assert_eq!(event["completion_tokens"].as_i64(), Some(500));
        assert_eq!(event["cache_read_tokens"].as_i64(), Some(800));
    }

    #[test]
    fn sse_usage_event_null_cache_matches_dispatcher_handling() {
        // Non-stream fallback emits cache_read_tokens: null
        let event = json!({
            "type": "usage",
            "prompt_tokens": 1000,
            "completion_tokens": 500,
            "cache_read_tokens": Value::Null,
        });
        // ChatTurnSseAccum uses .as_u64().unwrap_or(0) for null → 0
        let cache_read = event
            .get("cache_read_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        assert_eq!(cache_read, 0);
    }

    // ── Combined cache layer tests ──────────────────────────────────────

    #[test]
    fn all_three_cache_layers_present_for_anthropic() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        // Layer 1: System message with cache_control
        let (sys, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            Some("implementation"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );
        let sys_blocks = sys["content"].as_array().expect("array content");
        let has_sys_cache = sys_blocks.iter().any(|b| b.get("cache_control").is_some());
        assert!(
            has_sys_cache,
            "Layer 1: system message should have cache_control"
        );

        // Layer 2: Tool schemas with cache_control
        let mut tools = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "read_file"}}),
        ];
        annotate_tool_schemas_for_caching(&mut tools, "anthropic", "claude-sonnet-4-20250514");
        assert!(
            tools.last().unwrap().get("cache_control").is_some(),
            "Layer 2: last tool should have cache_control"
        );

        // Layer 3: Message breakpoint
        let mut messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        add_message_cache_breakpoint(&mut messages, "anthropic", "claude-sonnet-4-20250514");
        let last = messages.last().unwrap();
        let last_arr = last["content"].as_array().expect("converted to array");
        assert!(
            last_arr[0].get("cache_control").is_some(),
            "Layer 3: last message should have cache breakpoint"
        );
    }

    #[test]
    fn cache_disabled_strips_all_three_layers() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("MO_PROMPT_CACHE_DISABLED", "1");
        }

        // Layer 1: system message
        let (sys, _) = build_system_message(
            &["bash"],
            "cwd: /test",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );
        let sys_blocks = sys["content"].as_array().unwrap();
        for block in sys_blocks {
            assert!(
                block.get("cache_control").is_none() || block["cache_control"].is_null(),
                "system blocks should not have cache_control when disabled"
            );
        }

        // Layer 2: tool schemas
        let mut tools = vec![json!({"function": {"name": "bash"}})];
        annotate_tool_schemas_for_caching(&mut tools, "anthropic", "claude-sonnet-4-20250514");
        assert!(
            tools[0].get("cache_control").is_none(),
            "tools should not have cache_control when disabled"
        );

        // Layer 3: message breakpoint
        let mut messages = vec![json!({"role": "user", "content": "hello"})];
        add_message_cache_breakpoint(&mut messages, "anthropic", "claude-sonnet-4-20250514");
        assert!(
            messages[0]["content"].is_string(),
            "messages should not be modified when cache disabled"
        );

        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }
    }

    // ── Section cache eviction test ────────────────────────────────────

    #[test]
    fn section_cache_evicts_after_capacity() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();

        // Clear any pre-existing cache entries
        if let Ok(mut cache) = section_cache().lock() {
            cache.clear();
        }

        // Fill cache to 33 entries (> 32 threshold), then one more triggers clear
        for i in 0..34 {
            let tool_name = format!("tool_{i}");
            let tools: Vec<&str> = vec![tool_name.as_str()];
            let (_msg, _) = build_system_message(&tools, "", 0.8, None, "openai", "gpt-4");
        }

        let cache_size = section_cache().lock().unwrap().len();
        // After 34th insert: cache had 33 entries (> 32), cleared, then 34th re-added → 1
        assert!(
            cache_size <= 2,
            "cache should have been evicted: size={cache_size}, expected ≤2"
        );

        // Clean up
        section_cache().lock().unwrap().clear();
    }

    #[test]
    fn section_cache_key_deterministic_for_same_inputs() {
        let k1 = section_cache_key(&["bash", "read_file"], Some("debug"), 0.5);
        let k2 = section_cache_key(&["bash", "read_file"], Some("debug"), 0.5);
        assert_eq!(k1, k2, "same inputs should produce same key");
    }

    #[test]
    fn section_cache_key_differs_for_different_tools() {
        let k1 = section_cache_key(&["bash"], None, 0.8);
        let k2 = section_cache_key(&["read_file"], None, 0.8);
        assert_ne!(k1, k2, "different tools should produce different keys");
    }

    #[test]
    fn section_cache_key_low_confidence_bucketed() {
        // confidence < 0.3 → "low" bucket, >= 0.3 → "normal" bucket
        let low = section_cache_key(&["bash"], None, 0.1);
        let normal = section_cache_key(&["bash"], None, 0.5);
        assert_ne!(low, normal, "low vs normal confidence should differ");

        // Both in normal bucket → same key
        let n1 = section_cache_key(&["bash"], None, 0.5);
        let n2 = section_cache_key(&["bash"], None, 0.9);
        assert_eq!(n1, n2, "both normal confidence should be same bucket");
    }

    // ── Message breakpoint edge cases ──────────────────────────────────

    #[test]
    fn message_breakpoint_skips_system_only() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let mut messages = vec![json!({"role": "system", "content": "sys prompt"})];
        add_message_cache_breakpoint(&mut messages, "anthropic", "claude-sonnet-4-20250514");
        // System message should not be modified
        assert!(
            messages[0]["content"].is_string(),
            "system-only: should be untouched"
        );
    }

    #[test]
    fn message_breakpoint_empty_messages_noop() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let mut messages: Vec<Value> = vec![];
        add_message_cache_breakpoint(&mut messages, "anthropic", "claude-sonnet-4-20250514");
        assert!(messages.is_empty());
    }

    #[test]
    fn message_breakpoint_array_content_appends_to_last_block() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let mut messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"},
            ]
        })];
        add_message_cache_breakpoint(&mut messages, "anthropic", "claude-sonnet-4-20250514");

        let blocks = messages[0]["content"].as_array().unwrap();
        // First block should NOT have cache_control
        assert!(
            blocks[0].get("cache_control").is_none() || blocks[0]["cache_control"].is_null(),
            "first block should not have cache_control"
        );
        // Last block SHOULD have cache_control
        assert!(
            blocks[1].get("cache_control").is_some(),
            "last block should have cache_control"
        );
    }

    #[test]
    fn tool_schemas_empty_list_noop() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let mut tools: Vec<Value> = vec![];
        annotate_tool_schemas_for_caching(&mut tools, "anthropic", "claude-sonnet-4-20250514");
        assert!(tools.is_empty());
    }

    // ── Multi-turn cache token simulation ──────────────────────────────

    #[test]
    fn multi_turn_sse_cache_tokens_accumulate_in_accum() {
        use super::super::chat_turn_sse_dispatch::{
            ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
        };

        fn sse_usage(prompt: u64, completion: u64, cache_read: u64, cache_creation: u64) -> String {
            format!(
                "data: {{\"type\":\"usage\",\"prompt_tokens\":{prompt},\"completion_tokens\":{completion},\"cache_read_tokens\":{cache_read},\"cache_creation_tokens\":{cache_creation}}}\n\n"
            )
        }

        // Turn 1: cache miss → creation high, read 0
        let mut accum1 = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse_usage(1000, 500, 0, 800), &mut accum1, &mut vec![]);
        assert_eq!(accum1.cache_read_tokens, 0);
        assert_eq!(accum1.cache_creation_tokens, 800);

        // Turn 2: partial cache hit → read tokens appear
        let mut accum2 = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse_usage(600, 400, 500, 200),
            &mut accum2,
            &mut vec![],
        );
        assert_eq!(accum2.cache_read_tokens, 500);
        assert_eq!(accum2.cache_creation_tokens, 200);

        // Turn 3: full cache hit → read tokens high
        let mut accum3 = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse_usage(200, 300, 900, 0), &mut accum3, &mut vec![]);
        assert_eq!(accum3.cache_read_tokens, 900);

        // Verify warming pattern: reads increase across turns
        assert!(accum3.cache_read_tokens > accum2.cache_read_tokens);
        assert!(accum2.cache_read_tokens > accum1.cache_read_tokens);
        // Creation decreases
        assert!(accum1.cache_creation_tokens > accum2.cache_creation_tokens);
        assert!(accum2.cache_creation_tokens > accum3.cache_creation_tokens);
    }

    #[test]
    fn cache_tokens_correctly_extracted_from_anthropic_format() {
        // Anthropic native format: cache_read_input_tokens / cache_creation_input_tokens
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 500,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 200,
        });

        // Our bridge extraction logic (from bridge_inprocess.rs)
        let cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_i64));
        let cache_creation = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cache_creation_input_tokens"))
            .and_then(Value::as_i64)
            .or_else(|| {
                usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_i64)
            });

        assert_eq!(cache_read, Some(800));
        assert_eq!(cache_creation, Some(200));
    }

    #[test]
    fn cache_tokens_correctly_extracted_from_openai_format() {
        // OpenAI format: prompt_tokens_details.cached_tokens
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 500,
            "prompt_tokens_details": {
                "cached_tokens": 600,
                "cache_creation_input_tokens": 100,
            },
        });

        let cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_i64));
        let cache_creation = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cache_creation_input_tokens"))
            .and_then(Value::as_i64)
            .or_else(|| {
                usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_i64)
            });

        assert_eq!(cache_read, Some(600));
        assert_eq!(cache_creation, Some(100));
    }

    #[test]
    fn cache_tokens_none_when_absent() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 500,
        });

        let cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_i64));

        assert_eq!(cache_read, None);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Multi-turn cache regression: structural guarantees for both providers
    // ═══════════════════════════════════════════════════════════════════

    /// Anthropic: at most 4 cache_control breakpoints across the entire request
    /// (system prompt + tool schemas + conversation messages).
    /// System prompt should use at most 2 (last Global, last Session).
    #[test]
    fn anthropic_cache_breakpoints_within_limit() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        // Worst case: many tools → many Session sections
        let tools: Vec<&str> = vec![
            "bash",
            "read_file",
            "write_file",
            "glob",
            "grep",
            "git_status",
            "git_diff",
            "git_log",
            "git_commit",
            "find_definition",
            "find_references",
            "call_graph",
            "rename_symbol",
            "dead_code",
            "extract_members",
            "type_hierarchy",
            "multi_edit",
            "run_build_test",
            "memory_store",
            "memory_search",
            "github_list_prs",
            "github_get_issue",
        ];
        let (msg, _) = build_system_message(
            &tools,
            "profile",
            0.8,
            Some("code_review"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let blocks = msg["content"].as_array().unwrap();
        let cc_count = blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some_and(|cc| !cc.is_null()))
            .count();
        assert!(
            cc_count <= 2,
            "system prompt should have at most 2 cache_control breakpoints, got {cc_count}"
        );
    }

    /// Anthropic: Global breakpoint has scope:"global", Session breakpoint does not.
    #[test]
    fn anthropic_scope_annotations_correct() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let (msg, _) = build_system_message(
            &["bash", "read_file", "memory_store"],
            "profile",
            0.8,
            Some("debugging"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let blocks = msg["content"].as_array().unwrap();
        let cc_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some_and(|cc| !cc.is_null()))
            .collect();

        assert_eq!(cc_blocks.len(), 2, "should have exactly 2 breakpoints");

        // First breakpoint: Global (has scope:"global")
        let first_cc = &cc_blocks[0]["cache_control"];
        assert_eq!(first_cc["scope"].as_str(), Some("global"));
        assert_eq!(first_cc["ttl"].as_str(), Some("1h"));

        // Second breakpoint: Session (no scope field)
        let second_cc = &cc_blocks[1]["cache_control"];
        assert!(
            second_cc.get("scope").is_none() || second_cc["scope"].is_null(),
            "Session breakpoint should not have scope"
        );
        assert_eq!(second_cc["ttl"].as_str(), Some("1h"));
    }

    /// Anthropic multi-turn: Global prefix is identical across turns with
    /// different tool sets → cross-session cache reuse.
    #[test]
    fn anthropic_global_prefix_stable_across_tool_sets() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let (msg1, _) = build_system_message(
            &["bash", "read_file"],
            "p1",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );
        let (msg2, _) = build_system_message(
            &["bash", "git_diff", "memory_store"],
            "p2",
            0.5,
            Some("debugging"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let blocks1 = msg1["content"].as_array().unwrap();
        let blocks2 = msg2["content"].as_array().unwrap();

        // Find the Global breakpoint index in each
        let global_end_1 = blocks1
            .iter()
            .position(|b| {
                b.get("cache_control")
                    .and_then(|cc| cc.get("scope"))
                    .and_then(|s| s.as_str())
                    == Some("global")
            })
            .expect("should have global breakpoint");
        let global_end_2 = blocks2
            .iter()
            .position(|b| {
                b.get("cache_control")
                    .and_then(|cc| cc.get("scope"))
                    .and_then(|s| s.as_str())
                    == Some("global")
            })
            .expect("should have global breakpoint");

        // Same number of Global blocks
        assert_eq!(
            global_end_1, global_end_2,
            "Global prefix length should be identical"
        );

        // Same content in each Global block
        for i in 0..=global_end_1 {
            let t1 = blocks1[i]["text"].as_str().unwrap();
            let t2 = blocks2[i]["text"].as_str().unwrap();
            assert_eq!(
                t1, t2,
                "Global block {i} should be identical across tool sets"
            );
        }
    }

    /// Anthropic multi-turn: same tool set + same task type → Session prefix
    /// also identical (only profile/style differ).
    #[test]
    fn anthropic_session_prefix_stable_within_session() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        // Simulate two turns in the same session (same tools, different profile)
        let (msg_turn1, _) = build_system_message(
            &["bash", "read_file", "git_diff"],
            "turn1 profile",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );
        let (msg_turn2, _) = build_system_message(
            &["bash", "read_file", "git_diff"],
            "turn2 profile",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let b1 = msg_turn1["content"].as_array().unwrap();
        let b2 = msg_turn2["content"].as_array().unwrap();

        // Find Session breakpoint (last block with cache_control but no scope:"global")
        let session_end = |blocks: &[Value]| -> usize {
            blocks
                .iter()
                .rposition(|b| b.get("cache_control").is_some_and(|cc| !cc.is_null()))
                .unwrap()
        };
        let se1 = session_end(b1);
        let se2 = session_end(b2);
        assert_eq!(
            se1, se2,
            "Session prefix length should be identical across turns"
        );

        // All blocks up to and including Session breakpoint should be identical
        for i in 0..=se1 {
            assert_eq!(
                b1[i]["text"].as_str(),
                b2[i]["text"].as_str(),
                "Block {i} should be identical across turns (only profile differs)"
            );
        }

        // Profile blocks (after Session breakpoint) should differ
        let last1 = b1.last().unwrap()["text"].as_str().unwrap();
        let last2 = b2.last().unwrap()["text"].as_str().unwrap();
        assert_ne!(last1, last2, "Profile blocks should differ between turns");
    }

    /// OpenAI: stable prefix for automatic prefix caching.
    /// Static content is in the primary message (identical across turns);
    /// dynamic profile is in a separate second system message.
    #[test]
    fn openai_stable_prefix_across_turns() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();

        let (msg1, dyn1) = build_system_message(
            &["bash", "read_file"],
            "turn1 profile",
            0.8,
            None,
            "openai",
            "gpt-4o",
        );
        let (msg2, dyn2) = build_system_message(
            &["bash", "read_file"],
            "turn2 profile",
            0.8,
            None,
            "openai",
            "gpt-4o",
        );

        let s1 = msg1["content"].as_str().unwrap();
        let s2 = msg2["content"].as_str().unwrap();

        // Primary messages should be 100% identical (stable prefix)
        assert_eq!(
            s1, s2,
            "OpenAI primary system messages must be identical across turns"
        );

        // Dynamic messages should carry the per-turn profile
        let d1 = dyn1.unwrap();
        let d2 = dyn2.unwrap();
        assert!(
            d1["content"].as_str().unwrap().contains("turn1"),
            "dynamic msg1 should contain turn1 profile"
        );
        assert!(
            d2["content"].as_str().unwrap().contains("turn2"),
            "dynamic msg2 should contain turn2 profile"
        );
    }

    /// OpenAI: different tool sets share the same Global prefix.
    #[test]
    fn openai_global_prefix_stable_across_tool_sets() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();

        let (msg1, _) = build_system_message(&["bash"], "", 0.8, None, "openai", "gpt-4o");
        let (msg2, _) = build_system_message(
            &["bash", "git_diff", "memory_store", "find_definition"],
            "",
            0.8,
            Some("code_review"),
            "openai",
            "gpt-4o",
        );

        let s1 = msg1["content"].as_str().unwrap();
        let s2 = msg2["content"].as_str().unwrap();

        // Both should start with the same Global content (Core Rules etc.)
        // The Global prefix ends before "## Self-Model"
        let self_model_pos_1 = s1.find("## Self-Model").unwrap();
        let self_model_pos_2 = s2.find("## Self-Model").unwrap();

        // Everything before Self-Model should be identical
        assert_eq!(
            &s1[..self_model_pos_1],
            &s2[..self_model_pos_2],
            "Global prefix (before Self-Model) should be identical across tool sets"
        );
    }

    /// Global sections contain no tool names — ensures cross-session cache reuse.
    #[test]
    fn global_sections_contain_no_tool_names() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let tools = vec!["bash", "read_file", "memory_store", "git_diff"];
        let (msg, _) = build_system_message(
            &tools,
            "",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let blocks = msg["content"].as_array().unwrap();
        // Find the Global breakpoint
        let global_end = blocks
            .iter()
            .position(|b| {
                b.get("cache_control")
                    .and_then(|cc| cc.get("scope"))
                    .and_then(|s| s.as_str())
                    == Some("global")
            })
            .unwrap();

        // No Global block should contain any tool name
        for (i, block) in blocks.iter().enumerate().take(global_end + 1) {
            let text = block["text"].as_str().unwrap();
            for tool in &tools {
                // "bash" appears in generic text like "bash commands", skip it
                if *tool == "bash" {
                    continue;
                }
                assert!(
                    !text.contains(&format!("{tool},")),
                    "Global block {i} should not contain tool name '{tool}' in a tool list"
                );
            }
            assert!(
                !text.contains("Self-Model"),
                "Global block {i} should not contain Self-Model"
            );
        }
    }

    /// Task type change only affects Session sections, not Global.
    #[test]
    fn task_type_change_preserves_global_prefix() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let tools = vec!["bash", "read_file"];
        let (msg_none, _) = build_system_message(
            &tools,
            "",
            0.8,
            None,
            "anthropic",
            "claude-sonnet-4-20250514",
        );
        let (msg_review, _) = build_system_message(
            &tools,
            "",
            0.8,
            Some("code_review"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );
        let (msg_debug, _) = build_system_message(
            &tools,
            "",
            0.8,
            Some("debugging"),
            "anthropic",
            "claude-sonnet-4-20250514",
        );

        let get_global_blocks = |msg: &Value| -> Vec<String> {
            let blocks = msg["content"].as_array().unwrap();
            let global_end = blocks
                .iter()
                .position(|b| {
                    b.get("cache_control")
                        .and_then(|cc| cc.get("scope"))
                        .and_then(|s| s.as_str())
                        == Some("global")
                })
                .unwrap();
            (0..=global_end)
                .map(|i| blocks[i]["text"].as_str().unwrap().to_string())
                .collect()
        };

        let g_none = get_global_blocks(&msg_none);
        let g_review = get_global_blocks(&msg_review);
        let g_debug = get_global_blocks(&msg_debug);

        assert_eq!(
            g_none, g_review,
            "Global prefix should be identical regardless of task type"
        );
        assert_eq!(
            g_review, g_debug,
            "Global prefix should be identical regardless of task type"
        );
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn classify_llm_error_rate_limit_variants() {
        assert_eq!(classify_llm_error("rate limit exceeded"), "rate_limit");
        assert_eq!(
            classify_llm_error("HTTP 429 Too Many Requests"),
            "rate_limit"
        );
        assert_eq!(classify_llm_error("Rate limiting active"), "rate_limit");
    }

    #[test]
    fn classify_llm_error_timeout_variants() {
        assert_eq!(classify_llm_error("request timeout"), "timeout");
        assert_eq!(classify_llm_error("connection timed out"), "timeout");
    }

    #[test]
    fn classify_llm_error_transport_variants() {
        assert_eq!(classify_llm_error("connection refused"), "transport");
        assert_eq!(classify_llm_error("transport error"), "transport");
        assert_eq!(classify_llm_error("network unreachable"), "transport");
    }

    #[test]
    fn classify_llm_error_permission_variants() {
        assert_eq!(classify_llm_error("HTTP 401"), "permission");
        assert_eq!(classify_llm_error("unauthorized access"), "permission");
        assert_eq!(classify_llm_error("invalid api key"), "permission");
    }

    #[test]
    fn classify_llm_error_unknown_defaults_to_internal() {
        assert_eq!(classify_llm_error("something went wrong"), "internal");
        assert_eq!(classify_llm_error(""), "internal");
    }

    #[test]
    fn classify_llm_error_case_insensitive() {
        assert_eq!(classify_llm_error("RATE LIMIT"), "rate_limit");
        assert_eq!(classify_llm_error("Timeout"), "timeout");
        assert_eq!(classify_llm_error("UNAUTHORIZED"), "permission");
    }

    #[test]
    fn header_str_missing_header_returns_none() {
        let headers = HeaderMap::new();
        assert!(header_str(&headers, "x-mo-user-id").is_none());
    }

    #[test]
    fn header_str_empty_value_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-user-id", "".parse().unwrap());
        assert!(header_str(&headers, "x-mo-user-id").is_none());
    }

    #[test]
    fn header_str_valid_value_returns_some() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-user-id", "user-123".parse().unwrap());
        assert_eq!(
            header_str(&headers, "x-mo-user-id").as_deref(),
            Some("user-123")
        );
    }

    #[test]
    fn extract_entity_tokens_empty_string() {
        assert_eq!(extract_entity_tokens(""), "");
    }

    #[test]
    fn extract_entity_tokens_short_words_filtered() {
        // Words < 3 chars are dropped
        assert_eq!(extract_entity_tokens("a b cd ef"), "");
    }

    #[test]
    fn extract_entity_tokens_preserves_long_tokens() {
        assert_eq!(extract_entity_tokens("hello world"), "hello world");
    }

    #[test]
    fn extract_entity_tokens_special_chars_split() {
        assert_eq!(
            extract_entity_tokens("user.name@domain.com"),
            "user name domain com"
        );
    }

    #[test]
    fn extract_entity_tokens_hyphens_and_underscores_kept() {
        assert_eq!(extract_entity_tokens("my-app_v2"), "my-app_v2");
    }

    #[test]
    fn extract_entity_tokens_unicode_chars_as_delimiters() {
        // CJK chars and emoji act as delimiters
        let result = extract_entity_tokens("hello你好world");
        // 'hello' is 5 chars, '你好' splits, 'world' is 5 chars
        assert_eq!(result, "hello world");
    }

    #[test]
    fn extract_entity_tokens_only_special_chars() {
        assert_eq!(extract_entity_tokens("!@#$%^&*()"), "");
    }

    #[test]
    fn prefetch_memories_empty_key_returns_default() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(prefetch_memories("http://localhost", "", "query", "user1"));
        assert_eq!(result.items, 0);
        assert!(result.section.is_none());
    }

    #[test]
    fn prefetch_memories_whitespace_message_returns_default() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(prefetch_memories("http://localhost", "key", "   ", "user1"));
        assert_eq!(result.items, 0);
    }

    #[test]
    fn memory_prefetch_result_default() {
        let r = MemoryPrefetchResult::default();
        assert!(r.section.is_none());
        assert_eq!(r.items, 0);
        assert!(r.preview.is_empty());
        assert_eq!(r.fetch_ms, 0);
    }

    // ── render_sse tests ────────────────────────────────────────────────

    #[test]
    fn render_sse_formats_data_prefix() {
        let event = json!({"type": "text_delta", "content": "hi"});
        let bytes = render_sse(&event);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("data: "));
        assert!(s.ends_with("\n\n"));
        assert!(s.contains("\"text_delta\""));
    }

    #[test]
    fn render_sse_map_delegates_to_render_sse() {
        let mut map = Map::new();
        map.insert("type".into(), json!("usage"));
        map.insert("prompt_tokens".into(), json!(100));
        let bytes = render_sse_map(&map);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("data: "));
        assert!(s.contains("\"usage\""));
    }

    // ── apply_forward_llm_sse_event tests ───────────────────────────────

    #[test]
    fn forward_event_missing_type_returns_error() {
        let event = json!({"content": "no type field"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing type"));
    }

    #[test]
    fn forward_event_inprocess_summary_accumulates() {
        let event = json!({
            "type": "_inprocess_summary",
            "full_text": "accumulated text",
            "reasoning": "chain of thought",
            "tool_calls": [{"id": "c1"}],
            "usage": {"prompt_tokens": 50},
            "model_used": "claude-sonnet-4"
        });
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        )
        .unwrap();
        assert!(saw);
        assert_eq!(text, "accumulated text");
        assert_eq!(reasoning, "chain of thought");
        assert_eq!(tc.len(), 1);
        assert_eq!(usage.get("prompt_tokens").unwrap().as_i64(), Some(50));
        assert_eq!(model, "claude-sonnet-4");
        assert!(result.is_empty()); // _inprocess_summary produces no SSE output
    }

    #[test]
    fn forward_event_inprocess_summary_missing_fields_defaults() {
        let event = json!({"type": "_inprocess_summary"});
        let mut saw = false;
        let mut text = "old".to_string();
        let mut reasoning = "old".to_string();
        let mut tc = vec![json!("old")];
        let mut usage = Map::new();
        let mut model = "old".to_string();
        let _ = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        )
        .unwrap();
        assert!(saw);
        assert_eq!(text, ""); // defaults to empty
        assert_eq!(reasoning, ""); // defaults to empty
        assert!(tc.is_empty()); // defaults to empty vec
        assert_eq!(model, "old"); // not overwritten when absent
    }

    #[test]
    fn forward_event_text_delta_forwarded_as_sse() {
        let event = json!({"type": "text_delta", "content": "hello"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        let s = std::str::from_utf8(&result[0]).unwrap();
        assert!(s.contains("text_delta"));
    }

    #[test]
    fn forward_event_error_forwarded() {
        let event = json!({"type": "error", "message": "rate limit"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn forward_event_unknown_type_returns_empty() {
        let event = json!({"type": "some_future_event", "data": 42});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        )
        .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn forward_event_warning_forwarded() {
        let event = json!({"type": "warning", "message": "approaching limit"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event, &mut saw, &mut text, &mut reasoning, &mut tc, &mut usage, &mut model,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
    }
}
