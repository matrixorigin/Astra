/// In-process ChatTurnBridge — calls LLM directly without an external bridge service.
///
/// Architecture:
///   Rust API (dispatch_chat_turn_bridge) injects context into headers:
///     x-mo-user-id, x-mo-session-id, x-mo-turn-chain-id, x-mo-user-query-event-id, ...
///   This bridge reads those headers, calls the LLM, streams SSE back, and persists events.
use std::{sync::Arc, time::Instant};

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
    build_edge_tool_call_event, build_explain_event, build_stream_error_event, prompts,
    turn::persist::{build_tool_call_event_payload, build_tool_result_event_payload},
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

/// Resolve the first active model from DB, returning (model_name, api_key, base_url, provider).
///
/// Accepts an optional shared pool. When `None`, creates an ephemeral single-connection
/// pool (fallback for contexts where AppState is unavailable).
async fn resolve_active_model(
    matrixone: &MatrixOneSettings,
    encryptor: &FernetTokenEncryptor,
    preferred: Option<&str>,
    pool: Option<&sqlx::Pool<sqlx::MySql>>,
) -> Result<(String, String, String, String), String> {
    // Use shared pool if available; otherwise create ephemeral one
    let ephemeral;
    let pool: &sqlx::Pool<sqlx::MySql> = match pool {
        Some(p) => p,
        None => {
            ephemeral = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .connect(&matrixone.database_url())
                .await
                .map_err(|e| format!("DB connect: {e}"))?;
            &ephemeral
        }
    };

    let row = if let Some(name) = preferred {
        sqlx::query(
            "SELECT model_name, api_key_encrypted, base_url, provider \
             FROM infra_llm_models WHERE model_name = ? AND is_active = 1 LIMIT 1",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("DB query: {e}"))?
    } else {
        None
    };

    let row = if row.is_none() {
        sqlx::query(
            "SELECT model_name, api_key_encrypted, base_url, provider \
             FROM infra_llm_models WHERE is_active = 1 ORDER BY model_name LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("DB query fallback: {e}"))?
    } else {
        row
    };

    let row =
        row.ok_or_else(|| "No active LLM model configured. Run: mo-admin model add".to_string())?;

    use sqlx::Row;
    let model_name: String = row.try_get("model_name").map_err(|e| e.to_string())?;
    let encrypted: String = row
        .try_get("api_key_encrypted")
        .map_err(|e| e.to_string())?;
    let base_url: String = row
        .try_get("base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let provider: String = row
        .try_get("provider")
        .unwrap_or_else(|_| "openai".to_string());
    let api_key = encryptor
        .decrypt(&encrypted)
        .map_err(|e| format!("Decrypt: {e}"))?;

    Ok((model_name, api_key, base_url, provider))
}

/// Parse SSE data lines from a streaming response body.
/// Yields parsed JSON objects for each `data: {...}` line.
fn parse_sse_chunks(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
) -> impl futures_util::Stream<Item = Value> + Send + 'static {
    stream! {
        let mut buf = String::new();
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { break };
            let Ok(text) = std::str::from_utf8(&bytes) else { continue };
            buf.push_str(text);
            while let Some(newline) = buf.find('\n') {
                let line = buf.drain(..=newline).collect::<String>();
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" { return; }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        yield v;
                    }
                }
            }
        }
    }
}

/// Maximum retries for transient LLM errors (429, 5xx, network).
const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
const LLM_RETRY_BASE_MS: u64 = 1000;

// ── System Prompt Cache ──────────────────────────────────────────────────────
// The system prompt is ~1.2K tokens and identical for most turns within a session
// (same tool set, same task type). Cache by (tool_names_hash, task_type, confidence_bucket).
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn prompt_cache() -> &'static Mutex<HashMap<u64, String>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prompt_cache_key(tool_names: &[&str], task_type: Option<&str>, confidence: f64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in tool_names {
        name.hash(&mut hasher);
    }
    task_type.unwrap_or("none").hash(&mut hasher);
    // Bucket confidence: 0.0-0.3 = "low", 0.3-1.0 = "normal"
    let bucket = if confidence < 0.3 { "low" } else { "normal" };
    bucket.hash(&mut hasher);
    hasher.finish()
}

fn cached_system_prompt(
    tool_names: &[&str],
    profile_desc: &str,
    confidence: f64,
    task_type: Option<&str>,
) -> String {
    let key = prompt_cache_key(tool_names, task_type, confidence);
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

/// Prune tool schemas under token pressure to reduce context size.
/// - TrimSchemas tier: truncate descriptions to first sentence
/// - CompactHistory tier: truncate descriptions to first sentence
/// - AggressivePrune tier: truncate + remove optional parameters
fn prune_tool_schemas(tools: &[Value], tier: crate::prompts::CompactionTier) -> Vec<Value> {
    use crate::prompts::CompactionTier;
    match tier {
        CompactionTier::Normal => tools.to_vec(),
        CompactionTier::TrimSchemas => {
            // Compact: first-sentence descriptions, all params preserved
            tools
                .iter()
                .map(|tool| {
                    let mut t = tool.clone();
                    if let Some(func) = t.get_mut("function") {
                        if let Some(desc) = func.get("description").and_then(Value::as_str) {
                            let truncated = truncate_to_first_sentence(desc).to_string();
                            if let Some(obj) = func.as_object_mut() {
                                obj.insert("description".to_string(), json!(truncated));
                            }
                        }
                    }
                    t
                })
                .collect()
        }
        CompactionTier::CompactHistory => {
            // Compact descriptions + strip property descriptions (keep names + types)
            tools
                .iter()
                .map(|tool| {
                    let mut t = tool.clone();
                    if let Some(func) = t.get_mut("function") {
                        if let Some(desc) = func.get("description").and_then(Value::as_str) {
                            let truncated = truncate_to_first_sentence(desc).to_string();
                            if let Some(obj) = func.as_object_mut() {
                                obj.insert("description".to_string(), json!(truncated));
                            }
                        }
                        strip_property_descriptions(func);
                    }
                    t
                })
                .collect()
        }
        CompactionTier::AggressivePrune => {
            // Minimal: no function descriptions, required params only, no param descriptions
            tools
                .iter()
                .map(|tool| {
                    let mut t = tool.clone();
                    if let Some(func) = t.get_mut("function") {
                        if let Some(obj) = func.as_object_mut() {
                            obj.remove("description");
                        }
                        strip_optional_params(func);
                        strip_property_descriptions(func);
                    }
                    t
                })
                .collect()
        }
    }
}

/// Truncate a description to the first sentence (period/newline boundary).
fn truncate_to_first_sentence(desc: &str) -> &str {
    // Find first sentence end: period followed by space or end of string
    if let Some(pos) = desc.find(". ") {
        &desc[..pos + 1]
    } else if let Some(pos) = desc.find(".\n") {
        &desc[..pos + 1]
    } else if desc.len() > 200 {
        // Long description with no clear sentence break — hard-truncate
        let boundary = desc
            .char_indices()
            .take_while(|&(i, _)| i < 200)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(200);
        &desc[..boundary]
    } else {
        desc
    }
}

/// Remove optional parameters from a JSON Schema function definition.
fn strip_optional_params(func: &mut Value) {
    if let Some(params) = func.get_mut("parameters").and_then(Value::as_object_mut) {
        let required: Vec<String> = params
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        if let Some(props) = params.get_mut("properties").and_then(Value::as_object_mut) {
            let keys_to_remove: Vec<String> = props
                .keys()
                .filter(|k| !required.contains(k))
                .cloned()
                .collect();
            for key in keys_to_remove {
                props.remove(&key);
            }
        }
    }
}

/// Strip "description" fields from all properties in a JSON Schema.
/// Keeps property names, types, and enum values — removes the verbose
/// human-readable descriptions that consume tokens but are redundant
/// when the LLM already knows the tool from its function name.
fn strip_property_descriptions(func: &mut Value) {
    if let Some(props) = func
        .get_mut("parameters")
        .and_then(|p| p.get_mut("properties"))
        .and_then(Value::as_object_mut)
    {
        for (_key, prop) in props.iter_mut() {
            if let Some(obj) = prop.as_object_mut() {
                obj.remove("description");
            }
        }
    }
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
}

impl InProcessChatTurnBridge {
    pub fn new(matrixone: MatrixOneSettings, encryptor: Arc<FernetTokenEncryptor>) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            turn_learning_writer: None,
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
            .unwrap_or(1.0); // Default: high confidence (backward compat)
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

        let stream = stream! {
            let turn_started = Instant::now();
            // Emit session_info first
            yield render_sse(&json!({"type": "session_info", "session_id": session_id}));

            // Resolve LLM model
            let pool_ref = shared_pool.as_deref();
            let (model_name, api_key, base_url, provider) =
                match resolve_active_model(&matrixone, &encryptor, model_override.as_deref(), pool_ref).await {
                    Ok(m) => m,
                    Err(e) => {
                        yield render_sse_map(&build_stream_error_event(&e, "MODEL_NOT_AVAILABLE", false));
                        return;
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

            let task_type = prompts::detect_task_type(user_content_for_signal);

            let system_prompt_content = cached_system_prompt(
                &tool_names,
                &format!("{profile_desc}{skill_hint}"),
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
                let merged = crate::compact_tiered(&raw, budget_chars, 2_000, tier, budget.keep_recent_turns);
                (merged, tier)
            };

            llm_messages.extend(merged_messages);

            // Cloud loop: call LLM, handle tool calls
            let mut full_text = String::new();
            let mut final_tool_calls: Vec<Value> = Vec::new();
            let mut reasoning = String::new();
            let mut usage = Map::new();
            let mut resolved_model = model_name.clone();
            let cloud_loop_turns: i64 = 1;
            let mut llm_steps: Vec<Value> = Vec::new();

            let llm_started = Instant::now();
            // Compute max output tokens from model budget
            let budget = crate::prompts::budget_for_model(Some(&model_name));
            let max_output_tokens = (budget.model_limit as f64 * budget.output_reserve_ratio) as usize;
            // Prune tool schemas under token pressure
            let pruned_tools = prune_tool_schemas(&edge_tools, tier);
            {
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

                let loop_started = Instant::now();
                let mut loop_tool_calls: Vec<Value> = Vec::new();
                let mut loop_text = String::new();
                let mut loop_reasoning = String::new();

                while let Some(bytes) = llm_stream.next().await {
                    // Parse the SSE event
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(data) = text.strip_prefix("data: ")
                        && let Ok(event) = serde_json::from_str::<Value>(data.trim()) {
                            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                            match event_type {
                                "_inprocess_summary" => {
                                    // Internal summary — extract data, don't forward
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
                                    // Forward to client
                                    yield bytes;
                                }
                                _ => {}
                            }
                        }
                }

                full_text.push_str(&loop_text);
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
                    // Final text answer — done
                } else {
                    // All tool calls are edge tools — emit to client
                    final_tool_calls = loop_tool_calls;
                }
            }
            let llm_duration_ms = llm_started.elapsed().as_millis() as i64;

            // Emit edge tool calls to client
            for tc in &final_tool_calls {
                if let Some(tc_map) = tc.as_object() {
                    yield render_sse_map(&build_edge_tool_call_event(tc_map));
                }
            }

            // Persist events (fire-and-forget)
            let user_content = messages.iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .map(ToString::to_string);

            let has_tool_calls = !final_tool_calls.is_empty();
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
                for (index, tool_call) in final_tool_calls.iter().enumerate() {
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
                for tool_result in tool_results.iter() {
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
                    &tool_results,
                    &full_text,
                    &final_tool_calls,
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
                let tool_selection = final_tool_calls
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
                        "tool_calls": final_tool_calls.len(),
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
                    final_tool_calls.len(),
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
    use super::prune_tool_schemas;
    use crate::prompts::CompactionTier;
    use serde_json::Value;

    pub fn prune_tool_schemas_pub(tools: &[Value], tier: CompactionTier) -> Vec<Value> {
        prune_tool_schemas(tools, tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::CompactionTier;

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

    // ── Schema pruning tests ──

    fn make_tool_schema(name: &str, desc: &str, optional_param: bool) -> Value {
        let mut props = serde_json::Map::new();
        props.insert("command".to_string(), json!({"type": "string"}));
        if optional_param {
            props.insert("timeout".to_string(), json!({"type": "number"}));
        }
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": ["command"]
                }
            }
        })
    }

    #[test]
    fn prune_normal_tier_no_changes() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = super::prune_tool_schemas(&tools, CompactionTier::Normal);
        assert_eq!(result, tools, "Normal tier should not modify schemas");
    }

    #[test]
    fn prune_trim_schemas_truncates_descriptions() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools and build systems.",
            true,
        )];
        let result = super::prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert_eq!(
            desc, "Execute shell commands.",
            "TrimSchemas should truncate to first sentence"
        );
        // Optional params should still be present (only stripped in AggressivePrune)
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_some()
        );
    }

    #[test]
    fn prune_compact_history_truncates_descriptions() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = super::prune_tool_schemas(&tools, CompactionTier::CompactHistory);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert_eq!(desc, "Execute shell commands.");
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_some(),
            "CompactHistory should NOT strip optional params"
        );
    }

    #[test]
    fn prune_aggressive_strips_optional_params() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = super::prune_tool_schemas(&tools, CompactionTier::AggressivePrune);
        // AggressivePrune removes function description entirely
        assert!(
            result[0]["function"].get("description").is_none()
                || result[0]["function"]["description"].is_null(),
            "AggressivePrune should remove function description"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_none(),
            "AggressivePrune should strip optional params"
        );
        // Required params should remain
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("command")
                .is_some()
        );
    }

    #[test]
    fn prune_trim_schemas_saves_tokens_vs_normal() {
        let tools: Vec<Value> = (0..5)
            .map(|i| {
                make_tool_schema(
                    &format!("tool_{i}"),
                    "A very long description that explains everything the tool does. \
                 It handles multiple scenarios and edge cases.",
                    true,
                )
            })
            .collect();
        let normal = super::prune_tool_schemas(&tools, CompactionTier::Normal);
        let trimmed = super::prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
        let normal_bytes: usize = normal.iter().map(|t| t.to_string().len()).sum();
        let trimmed_bytes: usize = trimmed.iter().map(|t| t.to_string().len()).sum();
        assert!(
            trimmed_bytes < normal_bytes,
            "TrimSchemas should reduce total bytes: {} < {}",
            trimmed_bytes,
            normal_bytes
        );
    }
}
