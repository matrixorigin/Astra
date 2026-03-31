/// In-process ChatTurnBridge — calls LLM directly without an external bridge service.
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
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
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
    turn::edge_ledger::{assistant_message_with_tool_calls, ensure_tool_call_ids},
    turn::persist::{build_tool_call_event_payload, build_tool_result_event_payload},
    turn::sse_blocks::SseBlankLineUtf8Buf,
    turn::sse_data_lines::{
        drain_sse_data_lines, finish_sse_data_buffer, json_events_from_sse_event_block,
    },
    turn::stream_events::build_approval_required_event,
    turn::tool_schema_prune::prune_tool_schemas,
};

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;

fn turn_timeout_s() -> f64 {
    mo_agent_core::RuntimeLimits::global().turn_timeout_s
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
            mo_agent_core::agent_error!("sse", "serialization failed: {e}");
            Bytes::from("event: error\ndata: {\"error\":\"internal serialization failure\"}\n\n")
        }
    }
}

fn render_sse_map(event: &Map<String, Value>) -> Bytes {
    render_sse(&Value::Object(event.clone()))
}

/// Parse OpenAI-style SSE from a streaming response body.
///
/// Complete events are split on blank lines (`sse_blocks::SseBlankLineUtf8Buf`, same framing as
/// `/chat/turn` and `chat_turn_sse_dispatch::ChatTurnSseFramer`). Each block is scanned
/// for `data:` JSON lines. After the byte stream ends, any remainder is flushed with line-oriented
/// `sse_data_lines` draining so single-`\n` or partial tails still work.
fn parse_sse_chunks(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
) -> impl futures_util::Stream<Item = Value> + Send + 'static {
    stream! {
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

/// Maximum retries for transient LLM errors (429, 5xx, network).
const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
const LLM_RETRY_BASE_MS: u64 = 1000;

// ── System Prompt Cache ──────────────────────────────────────────────────────
// The system prompt is ~1.2K tokens and identical for most turns within a session
// (same tool set, same task type, same profile/learned hints). Cache by tool/task/confidence/profile.
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
    // Bucket confidence: 0.0-0.3 = "low", 0.3-1.0 = "normal"
    let bucket = if confidence < 0.3 { "low" } else { "normal" };
    bucket.hash(&mut hasher);
    profile_desc.hash(&mut hasher);
    hasher.finish()
}

fn cached_system_prompt(
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
        // Cap cache size to avoid unbounded growth
        if cache.len() > 32 {
            cache.clear();
        }
        cache.insert(key, prompt.clone());
    }
    prompt
}

/// Call LLM streaming API, yield SSE bytes.
/// Emits: text_delta, reasoning_delta, tool_call_start, usage SSE events,
/// then a final `_inprocess_summary` event with full_text/tool_calls/usage/model_used.
///
/// Retries up to LLM_MAX_RETRIES times on transient errors (429/5xx/network)
/// with exponential backoff.
async fn call_llm_stream(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    api_key: &str,
    base_url: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
) -> Result<impl futures_util::Stream<Item = Bytes> + Send + 'static, String> {
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
                // Network errors are always retryable
                continue;
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Success — return the stream
            let byte_stream = response.bytes_stream();
            let model_name = model_name.to_string();

            let out = stream! {
                let mut full_text = String::new();
                let mut reasoning = String::new();
                let mut tool_calls_map: std::collections::HashMap<usize, Map<String, Value>> =
                    std::collections::HashMap::new();
                let mut usage = Map::new();

                let sse = parse_sse_chunks(byte_stream);
                tokio::pin!(sse);

                while let Some(chunk) = sse.next().await {
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
                            yield render_sse(&json!({
                                "type": "usage",
                                "prompt_tokens": prompt,
                                "completion_tokens": completion,
                                "cache_read_tokens": u.get("prompt_tokens_details")
                                    .and_then(|d| d.get("cached_tokens"))
                                    .and_then(Value::as_i64),
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
                                    && !name.is_empty() {
                                        let is_new = f.get("name").and_then(Value::as_str).unwrap_or("").is_empty();
                                        f.insert("name".to_string(), Value::String(name.to_string()));
                                        if is_new {
                                            yield render_sse(&json!({"type": "tool_call_start", "name": name}));
                                        }
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
        let text = response.text().await.unwrap_or_default();
        last_err = format!("LLM error {status}: {text}");
        if status == 429 || status >= 500 {
            continue; // Retryable
        }
        // 4xx (except 429) is not retryable — fail immediately
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

        let stream = stream! {
            let turn_started = Instant::now();
            // Emit session_info first
            yield render_sse(&json!({"type": "session_info", "session_id": session_id}));

            let bridge_e2e = bridge_e2e_capture;
            let use_e2e_llm = bridge_e2e.as_ref().map(|r| !r.is_empty()).unwrap_or(false);

            // Resolve LLM model (skipped when `test_llm_rounds` drives the turn — feature `bridge-e2e-hooks`).
            let pool_ref = shared_pool.as_deref();
            let (model_name, api_key, base_url, provider) = if use_e2e_llm {
                (
                    "bridge-e2e-mock".to_string(),
                    "unused".to_string(),
                    "http://127.0.0.1:1".to_string(),
                    "openai".to_string(),
                )
            } else {
                match mo_agent_services::resolve_active_llm_model(
                    &matrixone,
                    encryptor.as_ref(),
                    model_override.as_deref(),
                    pool_ref,
                )
                .await
                {
                    Ok(m) => (m.model_name, m.api_key, m.base_url, m.provider),
                    Err(e) => {
                        yield render_sse_map(&build_stream_error_event(&e, "MODEL_NOT_AVAILABLE", false));
                        return;
                    }
                }
            };

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
                if parts.is_empty() { String::new() } else { format!("\n\n# Project Profile\n{}", parts.join("\n")) }
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

            let system_prompt_content = cached_system_prompt(
                &tool_names,
                &profile_with_hints,
                selection_confidence,
                task_type,
            );

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

            llm_messages.push(json!({
                "role": "system",
                "content": format!("{system_prompt_content}{memory_signal_hint}")
            }));

            // Merge tool results into messages (handle continuation turns)
            // Client sends complete message history including tool role messages,
            // so we just use messages directly.
            let (merged_messages, tier) = {
                let raw = messages.clone();
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
                let tier = budget.compaction_tier(cache_est.total_tokens);
                // Use effective input limit as char budget (×4 for char-to-token ratio)
                let budget_chars = budget.effective_input_limit() * 4;

                // Use Memoria-based compaction (sync fallback to pure truncation)
                let memoria_config = crate::turn::cloud::memoria_compact::MemoriaCompactConfig::default();
                let memoria_params = crate::turn::cloud::memoria_compact::MemoriaCompactParams {
                    budget_chars,
                    keep_chars: 2_000,
                    tier,
                    keep_recent_turns: budget.keep_recent_turns,
                    current_tokens: cache_est.total_tokens,
                };

                let compact_result = crate::turn::cloud::memoria_compact::compact_with_memoria_sync(
                    &raw,
                    Some(&session_id),
                    &memoria_config,
                    &memoria_params,
                );

                (compact_result.messages, tier)
            };

            llm_messages.extend(merged_messages);

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
            let max_output_tokens =
                (budget.model_limit as f64 * budget.output_reserve_ratio) as usize;
            let pruned_tools = prune_tool_schemas(&edge_tools, tier);
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

            for round_ix in 0i64..round_limit {
                cloud_loop_turns += 1;

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
                    let llm_stream = match call_llm_stream(
                        &llm_messages,
                        &pruned_tools,
                        &model_name,
                        &api_key,
                        &base_url,
                        &provider,
                        Some(max_output_tokens),
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            let kind = classify_llm_error(&e);
                            yield render_sse_map(&build_stream_error_event(&e, kind, kind != "internal"));
                            return;
                        }
                    };

                    tokio::pin!(llm_stream);

                    while let Some(bytes) = llm_stream.next().await {
                        let text = String::from_utf8_lossy(&bytes);
                        if let Some(data) = text.strip_prefix("data: ")
                            && let Ok(event) = serde_json::from_str::<Value>(data.trim()) {
                                let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                                match event_type {
                                    "_inprocess_summary" => {
                                        loop_text = event.get("full_text").and_then(Value::as_str).unwrap_or("").to_string();
                                        loop_reasoning = event.get("reasoning").and_then(Value::as_str).unwrap_or("").to_string();
                                        loop_tool_calls = event.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
                                        if let Some(u) = event.get("usage").and_then(Value::as_object) {
                                            usage = u.clone();
                                        }
                                        if let Some(m) = event.get("model_used").and_then(Value::as_str) {
                                            resolved_model = m.to_string();
                                        }
                                    }
                                    "text_delta" | "reasoning_delta" | "tool_call_start" | "usage" => {
                                        yield bytes;
                                    }
                                    _ => {}
                                }
                            }
                    }
                }

                full_text.push_str(&loop_text);
                if use_e2e_llm && !loop_text.trim().is_empty() {
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

                if loop_tool_calls.is_empty() {
                    break;
                }

                ensure_tool_call_ids(&mut loop_tool_calls);

                all_round_tool_calls.extend(loop_tool_calls.iter().cloned());

                llm_messages.push(assistant_message_with_tool_calls(&loop_tool_calls));
                for tc in loop_tool_calls.iter() {
                    let Some(tc_map) = tc.as_object() else {
                        continue;
                    };
                    let id = tc_map.get("id").and_then(Value::as_str).unwrap_or("");
                    let tool_name = tc_map
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");

                    if cloud_tool_requires_approval_for_delivery(tc) {
                        let path = tool_path_hint_for_delivery(tc);
                        yield render_sse_map(&build_approval_required_event(
                            id,
                            tool_name,
                            path.as_deref(),
                        ));
                        match wait_approval_ledger_for_tool(
                            &edge_callback_ledger,
                            &user_id,
                            tc,
                            ledger_wait,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(part) => {
                                merged_tool_results.extend(part.persist_tool_results);
                                llm_messages.extend(part.tool_messages);
                                continue;
                            }
                        }
                    }

                    for m in sse_maps_through_tool_request(tc) {
                        yield render_sse_map(&m);
                    }
                    let tail = wait_tool_result_ledger_for_tool(
                        &edge_callback_ledger,
                        &user_id,
                        tc,
                        ledger_wait,
                    )
                    .await;
                    merged_tool_results.extend(tail.persist_tool_results);
                    llm_messages.extend(tail.tool_messages);
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
                        mo_agent_core::agent_persist_fail!("bridge",
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
                            mo_agent_core::agent_persist_fail!("bridge",
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
                    mo_agent_core::agent_persist_fail!("bridge",
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
            mo_agent_core::agent_error!("memory", "fetch error: {e:#}");
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
            extract_entity_tokens("check mo-agent status"),
            "check mo-agent status"
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
    fn prompt_cache_key_includes_profile_context() {
        let key_plain = prompt_cache_key(&["bash"], Some("implementation"), 0.8, "");
        let key_learned = prompt_cache_key(
            &["bash"],
            Some("implementation"),
            0.8,
            "\n\n## Learned Runtime Context\nmatrixorigin => github",
        );
        assert_ne!(key_plain, key_learned);
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
}
