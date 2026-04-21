/// In-process chat turn bridge — calls LLM directly without an external bridge service.
///
/// # Key behaviors
///
/// | Behavior | Implementation |
/// |------------------------|------|
/// | Long-lived stream "stall" / no chunks | [`super::llm_client::stream_idle_timeout`] on SSE `next()` (5 min default, `MO_STREAM_IDLE_TIMEOUT_MS`) |
/// | Recover via one-shot completion | [`super::llm_client::call_llm_nonstream_fallback`] after idle in both `call_llm_and_collect` and [`call_llm_stream`] below |
/// | User cancel clears in-flight work | HTTP `/chat/turn` passes `CancellationToken`; dropping the SSE body (client disconnect) cancels in-flight LLM byte/SSE consumption in-process |
/// | Cooldown / 429 wait cannot ignore disconnect | [`super::llm_client::sleep_ms_or_llm_cancel`] on retry backoff + rate-limit waits in [`call_llm_stream`]; initial cooldown wait `select!`s [`wait_until_cancelled_or_pending`](super::llm_client::wait_until_cancelled_or_pending) in the bridge stream |
/// | Tool permission queue + single resolve | CLI: `astra-cli` `permission_manager`; cloud: edge approval ledger / `POST /tools/result`. "resolve once" matches ledger single-shot semantics |
///
/// # Legacy Status
///
/// This module implements the **old-style cloud tool loop** (its own `for round_ix..`
/// loop inside `stream!`). It does NOT use [`run_agentic_loop_with_host`], so semantic
/// dedup and full step recording are still absent here. Legacy `/chat/turn` and
/// `/chat/stream` now thinly reuse the shared TurnGuard / post-tool-policy shell so
/// they no longer bypass runtime stall and tool-restriction controls entirely.
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
///   Rust API (`forward()` on [`InProcessChatTurnBridge`]) injects context into headers:
///     x-mo-user-id, x-mo-session-id, x-mo-turn-chain-id, x-mo-user-query-event-id, ...
///   This bridge reads those headers, calls the LLM, streams SSE back, persists events, and
///   for each tool round blocks on [`super::edge_ledger`] until `POST /tools/result` (or timeout).
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use astra_core::SharedPool;
use astra_services::session_journal::{
    JournalWriter, LlmRoundRecord, ToolCallRecord, TurnEventBuffer,
};
use async_stream::stream;
use axum::body::Body;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    FernetTokenEncryptor, MatrixOneSettings, SessionActivityUpdatePlan, TurnAuxiliaryEventWriter,
    TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan, TurnHookDbWriter,
    TurnObserverWorker, TurnReflectionLessonWriter, TurnReflectionStateStore,
    TurnSessionActivityWriter, TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
    build_explain_event, build_stream_error_event, prompts,
    turn::edge_ledger::ensure_tool_call_ids,
    turn::persist::{build_tool_call_event_payload, build_tool_result_event_payload},
    turn::sse_blocks::SseBlankLineUtf8Buf,
    turn::tool_call_shape::tool_call_name,
    turn::tool_schema_prune::prune_tool_schemas,
};

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;

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

// ── SSE helpers — delegated to turn::bridge_sse_helpers ───────────────────────
use super::bridge_sse_helpers::{
    extend_forward_from_validated_sse_block, flush_tail_buf_into_llm_forward,
    reasoning_done_sse_bytes_if_needed, render_sse, render_sse_map,
};

fn preview_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn build_bridge_tool_call_records(
    tool_calls: &[Value],
    tool_results: &[Value],
    round_info: &HashMap<String, (u32, usize)>, // request_id → (round, tools_in_round)
) -> Vec<ToolCallRecord> {
    let mut call_metadata: HashMap<String, (String, Option<String>, Option<u32>)> = HashMap::new();
    for tool_call in tool_calls {
        let Some(tool_call) = tool_call.as_object() else {
            continue;
        };
        let request_id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let function = tool_call.get("function").and_then(Value::as_object);
        let tool_name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let arguments = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let input_bytes = arguments
            .as_ref()
            .map(|arguments| arguments.len().min(u32::MAX as usize) as u32);
        if !request_id.is_empty() {
            call_metadata.insert(request_id, (tool_name, arguments, input_bytes));
        }
    }

    let mut seen_request_ids = HashSet::new();
    let mut records = Vec::new();
    for tool_result in tool_results {
        let Some(tool_result) = tool_result.as_object() else {
            continue;
        };
        let request_id = tool_result
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !request_id.is_empty() {
            seen_request_ids.insert(request_id.clone());
        }
        let fallback_name = tool_result
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (tool_name, arguments, input_bytes) = call_metadata
            .get(&request_id)
            .cloned()
            .unwrap_or((fallback_name, None, None));
        let status = tool_result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("ok");
        let ok = status.eq_ignore_ascii_case("ok");
        let output = tool_result.get("output").map(|output| match output {
            Value::String(output) => output.clone(),
            other => other.to_string(),
        });
        let error = tool_result
            .get("error")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| (!ok).then(|| output.clone().unwrap_or_else(|| status.to_string())));
        let output_bytes = output
            .as_ref()
            .map(|output| output.len().min(u32::MAX as usize) as u32);
        // Extract file_path from full arguments before truncation
        let file_path = arguments.as_deref().and_then(|args_str| {
            serde_json::from_str::<serde_json::Value>(args_str)
                .ok()
                .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
        });
        // Observability: assign round/batch_id/parallel from round_info.
        let (round, batch_id, parallel) = match round_info.get(&request_id) {
            Some(&(r, count)) if count > 1 => (Some(r), Some(format!("bridge-r{r}")), Some(true)),
            Some(&(r, _)) => (Some(r), None, None),
            None => (None, None, None),
        };
        records.push(ToolCallRecord {
            name: tool_name,
            ok,
            ms: tool_result
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            error,
            input_bytes,
            output_bytes,
            args_preview: arguments
                .as_deref()
                .map(|arguments| preview_chars(arguments, 80)),
            result_preview: output.as_deref().map(|output| preview_chars(output, 500)),
            file_path,
            surgically_removed: None,
            original_tool_name: None,
            args_full: arguments.clone(),
            result_full: output.clone(),
            round,
            batch_id,
            parallel,
            ..Default::default()
        });
    }

    for (request_id, (tool_name, arguments, input_bytes)) in call_metadata {
        if seen_request_ids.contains(&request_id) {
            continue;
        }
        let (round, batch_id, parallel) = match round_info.get(&request_id) {
            Some(&(r, count)) if count > 1 => (Some(r), Some(format!("bridge-r{r}")), Some(true)),
            Some(&(r, _)) => (Some(r), None, None),
            None => (None, None, None),
        };
        records.push(ToolCallRecord {
            name: tool_name,
            ok: false,
            ms: 0,
            error: Some("missing tool result".to_string()),
            input_bytes,
            output_bytes: None,
            args_preview: arguments
                .as_deref()
                .map(|arguments| preview_chars(arguments, 80)),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            args_full: arguments,
            result_full: None,
            round,
            batch_id,
            parallel,
            ..Default::default()
        });
    }

    records
}

// ── Bridge observability — delegated to turn::bridge_observability ────────────
use super::bridge_observability::{
    build_legacy_context_trace_signal, persist_legacy_bridge_trace_and_quality,
};

// ── LLM streaming — delegated to turn::bridge_llm_stream ─────────────────────
use super::bridge_llm_stream::call_llm_stream;
use super::bridge_llm_stream::rate_limit_cooldown;
use crate::bridge::rate_limit_cooldown::RateLimitAction;

#[cfg(test)]
async fn await_with_client_disconnect<T, F>(
    cancel: Option<&CancellationToken>,
    future: F,
) -> Result<T, Map<String, Value>>
where
    F: std::future::Future<Output = T>,
{
    tokio::select! {
        biased;
        _ = crate::turn::llm_client::wait_until_cancelled_or_pending(cancel) => Err(
            build_stream_error_event(
                "Request cancelled (client disconnected)",
                "CLIENT_DISCONNECT",
                false,
            ),
        ),
        out = future => Ok(out),
    }
}

fn latest_user_message_text(messages: &[Value]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content").and_then(Value::as_str))
}

fn latest_assistant_message_text(messages: &[Value]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

fn turn_count_from_messages(messages: &[Value]) -> i64 {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .count() as i64
}

fn tool_names_from_tool_calls(tool_calls: &[Value]) -> Vec<String> {
    tool_calls
        .iter()
        .filter_map(|tool_call| tool_call.get("function").and_then(Value::as_object))
        .filter_map(|function| function.get("name").and_then(Value::as_str))
        .map(std::string::ToString::to_string)
        .collect()
}

fn filter_round_edge_tools(edge_tools: &[Value], restricted_tools: &HashSet<String>) -> Vec<Value> {
    if restricted_tools.is_empty() {
        return edge_tools.to_vec();
    }

    edge_tools
        .iter()
        .filter(|tool| {
            tool_call_name(tool)
                .map(|name| !restricted_tools.contains(name))
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
fn record_turn_guard_tool_results(
    turn_guard: &mut crate::turn::turn_guard::TurnGuard,
    persist_tool_results: &[Value],
) {
    for tool_result in persist_tool_results {
        let Some(tool_result) = tool_result.as_object() else {
            continue;
        };
        let Some(tool_name) = tool_result.get("name").and_then(Value::as_str) else {
            continue;
        };
        let result = tool_result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("");
        turn_guard.record_tool_result(tool_name, result);
    }
}

fn turn_complete_event(messages: &[Value], assistant_text: &str, tool_calls: &[Value]) -> Value {
    let mut event = crate::turn::complete::build_turn_complete_event(
        !tool_calls.is_empty(),
        false,
        &crate::turn::stall::DivergenceStatus::Healthy,
        None,
    );
    if let Some(user_message) = latest_user_message_text(messages)
        && let Some(suggestion) = crate::turn::followup_suggestion::suggest_followup(
            user_message,
            assistant_text,
            &tool_names_from_tool_calls(tool_calls),
        )
    {
        event.insert(
            "followup_suggestion".to_string(),
            Value::String(suggestion.text),
        );
    }
    Value::Object(event)
}

// ── Prompt caching — delegated to turn::prompt_cache ─────────────────────────
pub use super::prompt_cache::PromptCacheConfig;
#[cfg(test)]
pub(crate) use super::prompt_cache::build_system_message;
pub(crate) use super::prompt_cache::{
    add_message_cache_breakpoint, annotate_tool_schemas_for_caching,
    build_system_message_with_dynamic_sections,
};

#[derive(Clone)]
pub struct InProcessChatTurnBridge {
    pub matrixone: MatrixOneSettings,
    pub encryptor: Arc<FernetTokenEncryptor>,
    /// Shared DB pool — avoids creating a new connection per turn.
    /// When `None`, falls back to ephemeral single-connection pool.
    pub shared_pool: Option<SharedPool>,
    /// Pipeline learning writer — auto-updates EntityGraph/PatternLibrary/Calibrator.
    pub turn_learning_writer: Option<Arc<dyn crate::TurnLearningWriter>>,
    /// Same `Arc` as [`crate::AppState::edge_callback_ledger`] — bridge takes tool callbacks here.
    pub edge_callback_ledger: Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    /// Session-scoped structured feedback store — accumulates correction rules
    /// and injects them into subsequent turn system prompts.
    pub feedback_store: Arc<crate::pipeline::feedback_store::FeedbackStore>,
    /// Cached Memoria client — created once, reused across turns.
    pub memoria_client: Option<crate::turn::cloud::memoria_compact::HttpMemoriaClient>,
    /// Shared session facts for facts-first compaction. Updated by the agentic loop
    /// at each turn end; read by the bridge during compaction.
    pub session_facts: Arc<std::sync::Mutex<crate::turn::cloud::session_facts::SessionFacts>>,
}

impl InProcessChatTurnBridge {
    pub fn new(matrixone: MatrixOneSettings, encryptor: Arc<FernetTokenEncryptor>) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            turn_learning_writer: None,
            edge_callback_ledger: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            feedback_store: Arc::new(crate::pipeline::feedback_store::FeedbackStore::new()),
            memoria_client: crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env(),
            session_facts: Arc::new(std::sync::Mutex::new(Default::default())),
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
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

fn inprocess_session_info_event(session_id: &str, run_id: &str) -> Value {
    json!({
        "type": "session_info",
        "session_id": session_id,
        "run_id": run_id,
    })
}

impl InProcessChatTurnBridge {
    #[allow(clippy::too_many_arguments)]
    pub async fn forward(
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
        let agent_id = payload
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
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
        let round_index = payload
            .get("round_index")
            .and_then(Value::as_i64)
            .unwrap_or(0) as u32;
        let _agent_id = payload
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        let matrixone = self.matrixone.clone();
        let encryptor = self.encryptor.clone();
        let shared_pool = self.shared_pool.clone();
        let turn_learning_writer = self.turn_learning_writer.clone();
        let _edge_callback_ledger = self.edge_callback_ledger.clone();

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
        let feedback_store_capture = self.feedback_store.clone();
        let memoria_client_owned = self.memoria_client.clone();
        let session_facts_shared = self.session_facts.clone();

        let stream = stream! {
            let cc = client_cancel_capture.clone();
            let _client_disconnect_guard = cc
                .as_ref()
                .map(|t| crate::turn::llm_client::CancelOnClientDisconnect::new(t.clone()));
            let turn_started = Instant::now();
            let run_id = uuid::Uuid::new_v4().to_string();
            let trace_turn = turn_count_from_messages(&messages).max(1) as u32;
            let mut turn_event_buffer = TurnEventBuffer::begin_turn(
                (!session_id.is_empty()).then_some(session_id.as_str()),
                trace_turn,
            );
            // Emit session_info first
            yield render_sse(&inprocess_session_info_event(&session_id, &run_id));

            let bridge_e2e = bridge_e2e_capture;
            let use_e2e_llm = bridge_e2e.as_ref().map(|r| !r.is_empty()).unwrap_or(false);

            // Resolve LLM model (skipped when `test_llm_rounds` drives the turn — feature `bridge-e2e-hooks`).
            // Also capture fallback_model name for rate-limit-triggered fallback.
            let pool_ref = shared_pool.as_ref().map(SharedPool::get);
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

            // Latch cache config at session init — prevents mid-session env var
            // changes from busting the KV cache.
            let cache_cfg = PromptCacheConfig::latch(&provider, &model_name);

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
                    let top_k = edge_profile
                        .get("retrieval_top_k")
                        .and_then(Value::as_u64)
                        .unwrap_or(5) as u32;
                    let result = prefetch_memories(mem_url, mem_key, user_msg, &user_id, top_k).await;
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
            let active_skill_names: Vec<&str> = edge_profile
                .get("active_skills")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let skill_hint = if active_skill_names.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\n## Active Output Skills\n\
                     The user has enabled these output constraints: {}. \
                     Follow their formatting rules strictly.",
                    active_skill_names.join(", ")
                )
            };
            // ── Extract user query for signal detection ──
            let user_content_for_signal = messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .unwrap_or("");

            let learned_context_text = edge_profile
                .get("learned_context_hint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            let learned_context_hint = if learned_context_text.is_empty() {
                String::new()
            } else {
                format!("\n\n## Learned Runtime Context\n{learned_context_text}")
            };
            let task_type = edge_profile
                .get("selection_task_type")
                .and_then(Value::as_str)
                .or_else(|| prompts::detect_task_type(user_content_for_signal));
            // ── Self-awareness section (injected by CLI via edge_profile) ──
            let self_awareness_hint = edge_profile
                .get("self_awareness_text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|text| format!("\n\n{text}"))
                .unwrap_or_default();

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

            // ── Implicit feedback detection: inject correction/frustration context ──
            // When user expresses dissatisfaction (correction, frustration, rephrasing),
            // inject a directive so the model adjusts its approach immediately.
            let feedback_store = feedback_store_capture.clone();

            // ── Learned feedback rules: inject accumulated correction rules ──
            // Build injection BEFORE storing the new rule so the current turn's
            // correction isn't redundantly injected (it's already in implicit_feedback_hint).
            let feedback_rules_hint = {
                if session_id.is_empty() {
                    String::new()
                } else {
                    let injection = feedback_store.build_injection_filtered(&session_id, Some(user_content_for_signal));
                    if injection.is_empty() {
                        String::new()
                    } else {
                        format!("\n\n{injection}")
                    }
                }
            };

            let (implicit_feedback_hint, is_correction_turn) = {
                let signal = crate::turn::implicit_feedback::detect_implicit_feedback_signal(
                    user_content_for_signal,
                    latest_assistant_message_text(&messages),
                );
                let is_correction = matches!(signal.signal_type.as_str(), "correction" | "frustration");
                // Store heuristic-extracted feedback only on correction/frustration signals
                // and only when we have a valid session_id (avoid cross-session leakage)
                if !session_id.is_empty() && is_correction {
                    if let Some(fb) = crate::pipeline::feedback_extraction::heuristic_extract(
                        user_content_for_signal,
                        &signal.signal_type,
                        signal.confidence,
                    ) {
                        feedback_store.add(&session_id, fb);
                    }
                }
                let hint = crate::turn::implicit_feedback::implicit_feedback_context_injection(&signal)
                    .map(|s| format!("\n\n{s}"))
                    .unwrap_or_default();
                (hint, is_correction)
            };

            // ── Memoria client (shared across P1 anchor + compaction + P3 write) ──
            let memoria_client_shared = memoria_client_owned.clone();

            // ── P1: L0 session anchor — inject original task into dynamic system prompt ──
            // Derive anchor from current conversation state. On turn 1, falls back to
            // first user message. On subsequent turns, builds a lightweight L1 from
            // messages to show current state + progress — zero network calls.
            let session_anchor = {
                use crate::turn::cloud::session_memory_protocol::{
                    extract_anchor, extract_anchor_from_facts, extract_message_text,
                    build_l1_from_messages, SessionMemory,
                };
                let first_user_text = messages
                    .iter()
                    .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                    .and_then(|m| extract_message_text(m))
                    .unwrap_or_default();

                if first_user_text.is_empty() {
                    String::new()
                } else {
                    // Prefer facts-based anchor (ground truth) when available
                    let facts_opt = session_facts_shared.lock().ok();
                    let has_facts = facts_opt.as_ref().map(|f| f.turn > 0).unwrap_or(false);

                    let anchor = if has_facts {
                        let facts = facts_opt.unwrap();
                        // Try to get narrative for task spec (optional enrichment)
                        let turn_count = messages.iter()
                            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
                            .count();
                        let l1 = if turn_count > 0 {
                            let l1_text = build_l1_from_messages(&messages, turn_count, 0);
                            SessionMemory::parse(&l1_text).filter(|l| l.validate().is_ok())
                        } else {
                            None
                        };
                        extract_anchor_from_facts(&first_user_text, &facts, l1.as_ref())
                    } else {
                        // First turn or lock failed — use legacy anchor
                        let turn_count = messages.iter()
                            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
                            .count();
                        let l1 = if turn_count > 0 {
                            let l1_text = build_l1_from_messages(&messages, turn_count, 0);
                            SessionMemory::parse(&l1_text).filter(|l| l.validate().is_ok())
                        } else {
                            None
                        };
                        extract_anchor(&first_user_text, l1.as_ref())
                    };
                    format!("\n\n{anchor}")
                }
            };

            // ── Round budget directive: encourage synthesis after several rounds ──
            let tool_cfg = crate::runtime_config::RuntimeConfig::load().tool_selection;
            let (tool_round_guidance, guidance_signals) = prompts::tool_round_guidance_trace_with(
                &messages,
                round_index,
                tool_cfg.effective_round_budget_warning(),
                tool_cfg.effective_round_budget_limit(),
            );

            let mut dynamic_sections = Vec::new();
            if !profile_desc.is_empty() {
                dynamic_sections.push(prompts::PromptSection::dynamic(
                    profile_desc.clone(),
                    prompts::PromptTokenBucket::Environment,
                ));
            }
            if !skill_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        skill_hint.clone(),
                        prompts::PromptTokenBucket::UserPreferences,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            active_output_skills: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !learned_context_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        learned_context_hint.clone(),
                        prompts::PromptTokenBucket::UserPreferences,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            learned_runtime_context: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !memory_signal_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        memory_signal_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            memory_signal_detected: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !implicit_feedback_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        implicit_feedback_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            implicit_feedback: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !feedback_rules_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        feedback_rules_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            learned_feedback_rules: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !self_awareness_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        self_awareness_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            self_awareness: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !session_anchor.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        session_anchor.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                            session_anchor: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !tool_round_guidance.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        tool_round_guidance.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(crate::turn::context_assembly_trace::PromptTraceSignals {
                        guidance_signals,
                        ..Default::default()
                    }),
                );
            }
            // Build provider-aware system message with static/dynamic boundary.
            // Anthropic gets multi-block content with cache_control on stable sections;
            // OpenAI/others get two messages: stable prefix (cacheable) + dynamic per-turn.
            let (system_msg, dynamic_msg, prompt_sections) = build_system_message_with_dynamic_sections(
                &tool_names,
                &dynamic_sections,
                selection_confidence,
                task_type,
                &cache_cfg,
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
                    session_facts: session_facts_shared.lock().ok().map(|f| f.clone()),
                };

                // Reuse shared Memoria client for compaction
                let memoria_client = memoria_client_shared.clone();

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

                // ── P2: Continuation prompt after compaction ──
                // When compaction removed messages, append a user-role nudge so the
                // LLM resumes the task instead of asking "how can I help?"
                // Skip if the last assistant message signals task completion.
                let mut msgs = compact_result.messages;
                if compact_result.boundary.is_some() && msgs.len() >= 2 {
                    let last_is_user = msgs.last()
                        .and_then(|m| m.get("role").and_then(Value::as_str))
                        == Some("user");
                    let last_signals_done = msgs.last()
                        .and_then(|m| m.get("content").and_then(Value::as_str))
                        .map(|c| {
                            // Check only the last ~200 chars (the conclusion) to avoid
                            // false positives from negations in earlier context.
                            let tail = if c.len() > 200 { &c[c.floor_char_boundary(c.len() - 200)..] } else { c };
                            let lower = tail.to_ascii_lowercase();
                            let has_completion = lower.contains("task complete") || lower.contains("all done")
                                || lower.contains("finished") || lower.contains("completed successfully")
                                || lower.contains("任务完成") || lower.contains("已完成");
                            if !has_completion { return false; }
                            // Only check negation near the completion phrase (same tail)
                            let has_negation = lower.contains("not yet") || lower.contains("not complete")
                                || lower.contains("not finished") || lower.contains("haven't finished")
                                || lower.contains("hasn't finished") || lower.contains("won't be finished")
                                || lower.contains("don't think") || lower.contains("not sure")
                                || lower.contains("没有完成") || lower.contains("尚未完成")
                                || lower.contains("except") || lower.contains("but ");
                            has_completion && !has_negation
                        })
                        .unwrap_or(false);
                    if !last_is_user && !last_signals_done {
                        // Detect if conversation is primarily CJK (Chinese/Japanese/Korean)
                        let is_cjk = msgs.iter().rev().take(4)
                            .filter_map(|m| m.get("content").and_then(Value::as_str))
                            .any(|c| c.chars().take(200).filter(|ch| ('\u{4e00}'..='\u{9fff}').contains(ch)).count() > 10);
                        let prompt = if is_cjk {
                            "从上次中断的地方继续。不要向用户提问，直接继续当前任务。"
                        } else {
                            "Continue the conversation from where it left off. \
                             Do not ask the user any further questions — \
                             pick up the current task and keep going."
                        };
                        msgs.push(serde_json::json!({
                            "role": "user",
                            "content": prompt
                        }));
                    }
                }

                (msgs, tier) // tier only feeds memoria_compact params
            };

            llm_messages.extend(merged_messages);

            // Strip old reasoning_content from history messages to reduce token
            // usage. Keeps the field (as empty string) for thinking-model API
            // compat; only the most recent assistant reasoning is preserved.
            // Heavy checkpoints and persisted events retain full reasoning.
            super::edge_ledger::strip_stale_reasoning(&mut llm_messages, &provider, &model_name);

            // Cloud loop: every tool round waits on §5.5 ledger (`POST /tools/result`) then continues LLM.
            let merged_tool_results: Vec<Value> = tool_results.clone();

            let mut full_text = String::new();
            let mut all_round_tool_calls: Vec<Value> = Vec::new();
            // Track (start_index, count) per round for post-hoc round assignment.
            let mut round_boundaries: Vec<(usize, usize)> = Vec::new();
            let mut reasoning = String::new();
            let mut usage = Map::new();
            let mut resolved_model = model_name.clone();
            let mut cloud_loop_turns: i64 = 0;
            let mut llm_steps: Vec<Value> = Vec::new();

            let llm_started = Instant::now();
            let budget = crate::prompts::budget_for_model(Some(&model_name));
            let max_output_tokens = crate::prompts::capped_output_tokens(&budget);
            let max_rounds = crate::turn::routing::max_tool_rounds();
            let _round_limit: i64 = if use_e2e_llm {
                bridge_e2e
                    .as_ref()
                    .map(|r| (r.len() as i64).clamp(1, max_rounds))
                    .unwrap_or(1)
            } else {
                max_rounds
            };

            let mut last_measured_prompt: Option<u64> = None;
            let mut cache_detector = crate::turn::cloud::cache_diagnostics::CacheBreakDetector::new();


            // Single LLM call per HTTP request (no multi-round tool loop).
            let round_ix = 0i64;
            {
                cloud_loop_turns += 1;

                // Budget check removed: single LLM call per HTTP request.

                let round_edge_tools =
                    filter_round_edge_tools(&edge_tools, &HashSet::new());
                let round_tools_fingerprint_str =
                    serde_json::to_string(&round_edge_tools).unwrap_or_default();

                let tool_schema_tokens_round: usize = round_edge_tools
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
                    0, // single-call proxy: no consecutive context-window errors to track
                );
                let mut pruned_tools = prune_tool_schemas(&round_edge_tools, round_tier);
                annotate_tool_schemas_for_caching(&mut pruned_tools, &cache_cfg);

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
                    add_message_cache_breakpoint(&mut llm_messages, &cache_cfg);

                    // Emit system prompt breakdown so CLI can record precise per-component trace.
                    let skill_injections: Vec<crate::turn::context_assembly_trace::SkillInjection> =
                        edge_profile
                            .get("active_skills")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                let names: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                                if names.is_empty() {
                                    vec![]
                                } else {
                                    // Total tokens for the skill hint section, split evenly
                                    let hint_tokens = prompts::estimate_str_tokens(&skill_hint) as u32;
                                    let per = hint_tokens / names.len().max(1) as u32;
                                    names.iter().map(|name| crate::turn::context_assembly_trace::SkillInjection {
                                        skill_name: name.to_string(),
                                        skill_version: None,
                                        tokens: per,
                                        selection_reason: "active_output_skill".into(),
                                    }).collect()
                                }
                            })
                            .unwrap_or_default();
                    let memory_injections: Vec<crate::turn::context_assembly_trace::MemoryInjection> =
                        memory_preview.iter().enumerate().map(|(i, line)| {
                            crate::turn::context_assembly_trace::MemoryInjection {
                                memory_id: format!("prefetch-{i}"),
                                memory_type: "hybrid_retrieval".into(),
                                tokens: prompts::estimate_str_tokens(line) as u32,
                                relevance_score: 0.0,
                                content_preview: line.chars().take(100).collect(),
                            }
                        }).collect();
                    let breakdown = prompts::build_system_prompt_trace(
                        &prompt_sections,
                        skill_injections,
                        memory_injections,
                    );
                    yield render_sse(&json!({
                        "type": "context_meta",
                        "system_prompt_tokens": breakdown.total_tokens,
                        "system_prompt_breakdown": {
                            "base_persona_tokens": breakdown.base_persona_tokens,
                            "environment_tokens": breakdown.environment_tokens,
                            "user_preferences_tokens": breakdown.user_preferences_tokens,
                            "context_signals": breakdown.context_signals,
                            "guidance_signals": breakdown.guidance_signals,
                            "skills_injected": breakdown.skills_injected,
                            "repository_memories": breakdown.repository_memories,
                            "total_tokens": breakdown.total_tokens,
                        },
                    }));
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
                        Err(e) if crate::turn::llm_client::is_context_window_error(&e.to_lowercase()) => {
                            // Context-window error: force aggressive compaction and retry once
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
                    session_facts: session_facts_shared.lock().ok().map(|f| f.clone()),
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
                            let memoria_client = memoria_client_owned.clone();
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
                            pruned_tools = prune_tool_schemas(
                                &round_edge_tools,
                                crate::prompts::CompactionTier::AggressivePrune,
                            );
                            annotate_tool_schemas_for_caching(&mut pruned_tools, &cache_cfg);

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
                                        &session_id, agent_id.as_deref(), &model_name, &provider,
                                        &e2, &llm_messages, &pruned_tools,
                                        round_ix, Some(max_output_tokens / 2),
                                    );
                                    if let Some(path) = dump.write_local() {
                                        eprintln!("[llm_error_dump] {path}");
                                    }
                                    dump.persist_cloud(&user_id, &turn_chain_id, turn_auxiliary_event_writer.clone());
                                    yield render_sse_map(&build_stream_error_event(
                                        &format!("Context window exceeded even after aggressive compaction: {e2}"),
                                        kind.as_str(),
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
                                &session_id, agent_id.as_deref(), &model_name, &provider,
                                &e, &llm_messages, &pruned_tools,
                                round_ix, Some(max_output_tokens),
                            );
                            if let Some(path) = dump.write_local() {
                                eprintln!("[llm_error_dump] {path}");
                            }
                            dump.persist_cloud(&user_id, &turn_chain_id, turn_auxiliary_event_writer.clone());
                            yield render_sse_map(&build_stream_error_event(&e, kind.as_str(), kind.is_retryable()));
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
                    if let Some(done) = reasoning_done_sse_bytes_if_needed(&loop_reasoning) {
                        yield done;
                    }
                }
                let round_ms = loop_started.elapsed().as_millis();
                let tok_in = usage.get("prompt").and_then(Value::as_i64).unwrap_or(0);
                let tok_out = usage.get("completion").and_then(Value::as_i64).unwrap_or(0);
                astra_core::agent_info!(
                    "llm",
                    "⏱ LLM round done: total={}ms tok_in={} tok_out={} tools={} model={} sid={} r={}",
                    round_ms,
                    tok_in,
                    tok_out,
                    loop_tool_calls.len(),
                    if resolved_model.is_empty() { &model_name } else { &resolved_model },
                    session_id,
                    round_ix,
                );
                llm_steps.push(json!({
                    "step": "llm",
                    "duration_ms": round_ms as i64,
                    "in": usage.get("prompt").and_then(Value::as_i64),
                    "out": usage.get("completion").and_then(Value::as_i64),
                    "tool_calls": loop_tool_calls.len(),
                }));

                let prompt_from_usage = usage
                    .get("prompt")
                    .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)));
                if let Some(p) = prompt_from_usage.filter(|&p| p > 0) {
                    last_measured_prompt = Some(p);
                }
                turn_event_buffer.record_llm_round(LlmRoundRecord {
                    ttft_ms: None,
                    duration_ms: round_ms as u64,
                    prompt_tokens: usage
                        .get("prompt")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                        .unwrap_or(0),
                    completion_tokens: usage
                        .get("completion")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                        .unwrap_or(0),
                    cache_read_tokens: usage
                        .get("cache_read")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                        .unwrap_or(0),
                    tool_calls_returned: loop_tool_calls.len().min(u32::MAX as usize) as u32,
                    tool_call_names: loop_tool_calls
                        .iter()
                        .filter_map(tool_call_name)
                        .map(ToString::to_string)
                        .collect(),
                    finish_reason: Some(if loop_tool_calls.is_empty() {
                        "stop".to_string()
                    } else {
                        "tool_calls".to_string()
                    }),
                });

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
                        &sys_prompt_str,
                        &round_tools_fingerprint_str,
                        &model_name,
                        &provider,
                    );
                    let cache_read = usage.get("cache_read")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
                        .unwrap_or(0);
                    if let Some(event) = cache_detector.detect_break(&fp, cache_read) {
                        let causes: Vec<String> = event.causes.iter().map(|c| c.to_string()).collect();
                        eprintln!("[cache_diagnostics] cache break: {} | {}",
                            causes.join(", "), cache_detector.stats_summary());
                    }
                }

                if !loop_tool_calls.is_empty() {
                    // Accumulate tool calls for turn_complete event.
                    // Tool execution and continuation happen on the CLI side.
                    ensure_tool_call_ids(&mut loop_tool_calls);

                    for tc in &loop_tool_calls {
                        if let Some(tc_map) = tc.as_object() {
                            // Emit tool_call with FULL accumulated arguments so the
                            // CLI's accum.tool_calls gets updated (replacing the
                            // empty-args entry from tool_call_start). Without this,
                            // signature matching in headless_tool_assembly fails
                            // because accum has empty args while edge_tool_round
                            // has the real parsed args.
                            let tc_event = astra_turn_core::stream_events::build_edge_tool_call_event(tc_map);
                            yield render_sse_map(&tc_event);

                            // Emit tool_request so the CLI executes the tool
                            // locally and populates edge_tool_round.
                            let req_event = astra_turn_core::stream_events::build_tool_request_event(tc_map);
                            yield render_sse_map(&req_event);
                        }
                    }

                    let round_start = all_round_tool_calls.len();
                    all_round_tool_calls.extend(loop_tool_calls.iter().cloned());
                    round_boundaries.push((round_start, loop_tool_calls.len()));
                }
            }


            let llm_duration_ms = llm_started.elapsed().as_millis() as i64;

            // Persist events (fire-and-forget)
            let user_content = latest_user_message_text(&messages).map(ToString::to_string);

            let has_tool_calls = !all_round_tool_calls.is_empty();
            let llm_content = full_text.trim().to_string();
            let should_persist_llm = !llm_content.is_empty() || has_tool_calls;

            // P2 fix: on continuation calls (CLI sent tool_results), the user query
            // was already persisted on the first bridge call. Skip to avoid duplicate event_id.
            let is_continuation = !tool_results.is_empty();
            let user_query_event = if is_continuation {
                None
            } else {
                user_content.as_ref().map(|content| TurnCoreEventRecord {
                    event_id: user_query_event_id.clone(),
                    user_id: user_id.clone(),
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    event_type: "user_query".to_string(),
                    content: content.clone(),
                    parent_event_id: None,
                    parent_event_ids: Vec::new(),
                    causal_chain_id: turn_chain_id.clone(),
                    llm_model_used: None,
                    token_usage: None,
                    llm_params: None,
                    reasoning_content: None,
                })
            };

            let llm_response_event = should_persist_llm.then(|| TurnCoreEventRecord {
                event_id: Uuid::now_v7().to_string(),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                event_type: "llm_response".to_string(),
                content: llm_content.clone(),
                parent_event_id: Some(user_query_event_id.clone()),
                parent_event_ids: vec![user_query_event_id.clone()],
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
                            agent_id: agent_id.clone(),
                            event_type: "tool_call".to_string(),
                            content: match payload.content {
                                Value::String(s) => s,
                                v => serde_json::to_string(&v).unwrap_or_default(),
                            },
                            parent_event_id: Some(user_query_event_id.clone()),
                            parent_event_ids: vec![user_query_event_id.clone()],
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
                            agent_id: agent_id.clone(),
                            event_type: "tool_result".to_string(),
                            content: match payload.content {
                                Value::String(s) => s,
                                v => serde_json::to_string(&v).unwrap_or_default(),
                            },
                            parent_event_id: Some(user_query_event_id.clone()),
                            parent_event_ids: vec![user_query_event_id.clone()],
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

            if !turn_event_buffer.is_empty() && !session_id.is_empty() {
                let journal_sid = session_id.clone();
                tokio::task::spawn_blocking(move || {
                    let writer = match JournalWriter::new(&journal_sid) {
                        Ok(writer) => writer,
                        Err(error) => {
                            astra_core::agent_warn!(
                                "bridge",
                                "failed to create journal writer for llm_round flush: session={} error={}",
                                journal_sid,
                                error
                            );
                            return;
                        }
                    };
                    if let Err(error) = turn_event_buffer.flush(&writer) {
                        astra_core::agent_warn!(
                            "bridge",
                            "failed to flush llm_round events: session={} error={}",
                            journal_sid,
                            error
                        );
                    }
                });
            }

            // Hook side effects: decision audit, skill selection, implicit feedback, reflection
            {
                let mut hook_payload = crate::turn::tail_persist::build_turn_hook_args(
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
                    turn_count_from_messages(&messages),
                    None, // session_start
                    false, // run_hook_db_writes = false → triggers persist
                    false, // run_observer = false → triggers observer
                    false, // run_implicit_feedback = false → triggers feedback
                    false, // run_reflection_learning = false → triggers reflection
                );
                // Propagate correction signal and routing metadata so pipeline
                // learning can update ProgressiveCalibrator with actual data.
                if is_correction_turn {
                    hook_payload.insert(
                        "is_correction".to_string(),
                        serde_json::Value::Bool(true),
                    );
                }
                if let Some(tt) = task_type {
                    if let Some(m) = hook_payload
                        .entry("routing_meta".to_string())
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                    {
                        m.insert("task_type".to_string(), json!(tt));
                    }
                }
                crate::bridge::side_effects::run_bridge_hook_side_effects(
                    Some(Value::Object(hook_payload)),
                    turn_hook_db_writer.clone(),
                    turn_reflection_state_store.clone(),
                    turn_reflection_lesson_writer.clone(),
                    turn_observer_worker.clone(),
                    turn_learning_writer.clone(),
                );
            }

            // Build request_id → (round, tools_in_round) for observability.
            let round_info: HashMap<String, (u32, usize)> = {
                let mut m = HashMap::new();
                for (round_idx, (start, count)) in round_boundaries.iter().enumerate() {
                    for tc in &all_round_tool_calls[*start..*start + *count] {
                        if let Some(id) = tc.get("id").and_then(Value::as_str) {
                            m.insert(id.to_string(), (round_idx as u32, *count));
                        }
                    }
                }
                m
            };
            let tool_call_records =
                build_bridge_tool_call_records(&all_round_tool_calls, &merged_tool_results, &round_info);
            let verdict_warning = false; // No multi-round verdicts in single-call mode.
            let recent_tools_for_quality = tool_names_from_tool_calls(&all_round_tool_calls);
            let budget_pressure = last_measured_prompt.map_or(0.0, |measured| {
                if budget.model_limit > 0 {
                    measured as f64 / budget.model_limit as f64
                } else {
                    0.0
                }
            });
            let user_message_for_eval = latest_user_message_text(&messages)
                .unwrap_or("")
                .to_string();
            let evaluation = (!tool_call_records.is_empty()).then(|| {
                crate::pipeline::evaluation::evaluate_tool_call_records(
                    &user_message_for_eval,
                    &recent_tools_for_quality,
                    &tool_call_records,
                    0, // No stall events in single-call mode.
                    verdict_warning,
                    budget_pressure,
                    false, // No prefetch in bridge single-call mode.
                )
            });
            let tool_execution_ms: u64 = merged_tool_results
                .iter()
                .filter_map(|tool_result| {
                    tool_result
                        .get("duration_ms")
                        .and_then(Value::as_u64)
                })
                .sum();
            let trace_signal = build_legacy_context_trace_signal(
                trace_turn,
                format!("turn-{trace_turn}"),
                edge_tools.len(),
                recent_tools_for_quality.clone(),
                selection_confidence,
                last_measured_prompt,
                budget.model_limit,
                tool_execution_ms,
                turn_started.elapsed().as_millis() as u64,
            );

            // Auxiliary events: routing decisions, quality assessments, snapshots
            {
                let aux_writer = turn_auxiliary_event_writer.clone();
                let aux_uid = user_id.clone();
                let aux_sid = session_id.clone();
                let aux_aid = agent_id.clone();
                let aux_chain = turn_chain_id.clone();
                let aux_parent = user_query_event_id.clone();
                let aux_matrixone = matrixone.clone();
                let aux_pool = shared_pool.clone();
                let aux_trace_signal = trace_signal.clone();
                let aux_evaluation = evaluation.clone();
                let aux_step_count = tool_call_records.len();
                tokio::spawn(async move {
                    // Routing decision event (inprocess uses default router)
                    let routing_event = crate::TurnAuxiliaryEventRecord {
                        event_id: Uuid::now_v7().to_string(),
                        user_id: aux_uid.clone(),
                        session_id: aux_sid.clone(),
                        agent_id: aux_aid.clone(),
                        event_type: "routing_decision".to_string(),
                        content: json!({"router": "inprocess-default", "intent": "default"}).to_string(),
                        parent_event_id: Some(aux_parent.clone()),
                        parent_event_ids: vec![aux_parent],
                        causal_chain_id: aux_chain.clone(),
                        metadata: None,
                        reasoning_content: None,
                    };
                    if let Err(e) = aux_writer.persist_events(vec![routing_event]).await {
                        eprintln!(
                            "PERSIST_FAIL session={} stage=auxiliary error={}",
                            aux_sid, e
                        );
                    }
                    persist_legacy_bridge_trace_and_quality(
                        &aux_matrixone,
                        aux_pool,
                        aux_uid,
                        aux_sid,
                        aux_aid,
                        aux_chain,
                        aux_trace_signal,
                        aux_evaluation,
                        aux_step_count,
                    )
                    .await;
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

            // ── P3: Async L1 session memory write to Memoria ──
            // Writes L1 at turn end. Deletes previous L1 for this session first to
            // avoid accumulating stale working memories. Retries once on failure.
            if cloud_loop_turns > 1 || !all_round_tool_calls.is_empty() {
                // Update shared SessionFacts from this turn's tool_call_records
                // so the next turn's compaction/anchor has fresh ground truth.
                if let Ok(mut facts) = session_facts_shared.lock() {
                    let mut event = astra_services::session_journal::JournalEvent::base_public(
                        astra_services::session_journal::JournalEventType::Turn, None,
                    );
                    event.turn = Some(cloud_loop_turns as u32);
                    event.tokens_in = usage.get("prompt_tokens").and_then(Value::as_i64).map(|t| t as u64);
                    event.tool_calls = Some(tool_call_records.clone());
                    facts.update_from_journal_event(&event);
                }

                let l1_content = crate::turn::cloud::session_memory_protocol::build_l1_from_messages(
                    &messages, cloud_loop_turns as usize,
                    usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0) as usize,
                );
                let l1_sid = session_id.clone();
                let l1_client = memoria_client_shared.clone();
                tokio::spawn(async move {
                    let Some(client) = l1_client else { return; };
                    match crate::turn::cloud::session_memory_protocol::persist_l1(
                        &client, &l1_content, &l1_sid,
                    ).await {
                        Ok(id) => tracing::debug!(session_id = %l1_sid, memory_id = %id, "L1 session memory persisted"),
                        Err(e) => tracing::warn!(session_id = %l1_sid, error = %e, "L1 session memory persist failed"),
                    }
                });
            }

            // turn_complete
            yield render_sse(&turn_complete_event(&messages, &llm_content, &all_round_tool_calls));
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

fn classify_llm_error(msg: &str) -> astra_core::ErrorKind {
    // Delegate to the canonical classifier in llm_client.
    crate::turn::llm_client::classify_llm_error(msg)
}

// ── Memory prefetch — delegated to turn::memory_prefetch ─────────────────────
pub use super::memory_prefetch::{MemoryPrefetchResult, prefetch_memories};

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
    use crate::turn::bridge_sse_helpers::apply_forward_llm_sse_event;
    use crate::turn::turn_guard::TurnGuard;

    #[test]
    fn count_inprocess_persisted_events_skips_failed_tool_events() {
        assert_eq!(count_inprocess_persisted_events(2, 3, false), 2);
        assert_eq!(count_inprocess_persisted_events(2, 3, true), 5);
    }

    #[test]
    fn build_legacy_context_trace_signal_keeps_only_known_timing_values() {
        let signal = build_legacy_context_trace_signal(
            3,
            "turn-3".to_string(),
            5,
            vec!["read_file".to_string(), "grep".to_string()],
            0.82,
            Some(1200),
            8000,
            450,
            1500,
        );

        let tool_selection = signal.tool_selection.as_ref().expect("tool selection");
        assert_eq!(tool_selection.strategy, "inprocess_bridge");
        assert_eq!(tool_selection.confidence, 0.82);

        let timing = signal.timing.as_ref().expect("timing");
        assert_eq!(timing.turn, 3);
        assert_eq!(timing.context_assembly_ms, 0);
        assert_eq!(timing.llm_total_ms, 1050);
        assert_eq!(timing.tool_execution_ms, 450);
        assert_eq!(timing.total_ms, 1500);
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

        let (msg, _, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            Some("implementation"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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

        let (msg, _, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            Some("implementation"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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

        let (msg, _, _) = build_system_message(
            &["bash"],
            "cwd: /test",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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
        let (msg, dynamic, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
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
    fn build_system_message_feedback_rules_in_dynamic_no_cache_control() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        // Simulate dynamic_desc with accumulated feedback rules
        let dynamic_with_rules = "cwd: /test\n\n[Learned Feedback Rules]\n- Rule: don't use mocks | Why: prod divergence | When: integration tests\n- Rule: never force push on main";

        let (msg, _, _) = build_system_message(
            &["bash", "read_file"],
            dynamic_with_rules,
            0.8,
            Some("implementation"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );

        let blocks = msg["content"].as_array().expect("should be array");

        // Last block should contain the feedback rules but have NO cache_control
        let last = blocks.last().unwrap();
        let text = last["text"].as_str().unwrap();
        assert!(
            text.contains("[Learned Feedback Rules]"),
            "dynamic block should contain rules"
        );
        assert!(
            text.contains("don't use mocks"),
            "dynamic block should contain rule text"
        );
        assert!(
            last.get("cache_control").is_none() || last["cache_control"].is_null(),
            "dynamic block with feedback rules must NOT have cache_control"
        );

        // Stable blocks with cache_control should NOT contain feedback rules
        for block in blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some() && !b["cache_control"].is_null())
        {
            let block_text = block["text"].as_str().unwrap_or("");
            assert!(
                !block_text.contains("[Learned Feedback Rules]"),
                "cached block must not contain feedback rules"
            );
        }
    }

    #[test]
    fn build_system_message_openai_feedback_rules_in_dynamic_message() {
        // For OpenAI: feedback rules should be in the second (dynamic) system message,
        // not in the first (stable/cacheable) message
        let dynamic_with_rules = "cwd: /test\n\n[Learned Feedback Rules]\n- Rule: use moerr";

        let (primary, dynamic, _) = build_system_message(
            &["bash"],
            dynamic_with_rules,
            0.8,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );

        // Primary (stable) must NOT contain feedback rules
        let primary_text = primary["content"].as_str().unwrap();
        assert!(
            !primary_text.contains("[Learned Feedback Rules]"),
            "stable prefix must not contain feedback rules"
        );

        // Dynamic message must contain them
        let dyn_msg = dynamic.expect("should have dynamic message");
        let dyn_text = dyn_msg["content"].as_str().unwrap();
        assert!(
            dyn_text.contains("[Learned Feedback Rules]"),
            "dynamic message should contain feedback rules"
        );
    }

    #[test]
    fn build_system_message_openai_keeps_late_round_guidance_in_dynamic_message() {
        let messages = vec![
            json!({"role": "user", "content": "inspect the project"}),
            json!({"role": "tool", "content": "Cargo.toml"}),
            json!({"role": "tool", "content": "README.md"}),
        ];
        let guidance = prompts::tool_round_guidance(&messages, prompts::ROUND_BUDGET_THRESHOLD);

        let (primary, dynamic, _) = build_system_message(
            &["read_file", "list_dir"],
            &guidance,
            0.8,
            Some("implementation"),
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );

        let primary_text = primary
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let dynamic_text = dynamic
            .as_ref()
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        assert!(
            !primary_text.contains("Synthesize Or Batch Now"),
            "stable primary message must not contain late-round dynamic guidance"
        );
        assert!(
            dynamic_text.contains("Synthesize Or Batch Now"),
            "dynamic message should include the late-round synthesis nudge"
        );
        assert!(
            dynamic_text.contains("2 tools executed in parallel"),
            "dynamic message should keep batching feedback"
        );
    }

    #[test]
    fn build_system_message_claude_model_triggers_anthropic_format() {
        // Even if provider is not "anthropic", claude model name should trigger it
        let (msg, _, _) = build_system_message(
            &["bash"],
            "",
            0.8,
            None,
            &PromptCacheConfig::latch("openrouter", "claude-sonnet-4-20250514"),
        );

        let content = msg.get("content").expect("should have content");
        assert!(
            content.is_array(),
            "claude model should use array content even through non-anthropic provider"
        );
    }

    #[test]
    fn build_system_message_returns_non_empty_sections() {
        let (_, _, sections) = build_system_message(
            &["bash", "grep"],
            "cwd: /test\ngit_branch: main",
            0.8,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );
        assert!(!sections.is_empty(), "should return prompt sections");
        // Should have Global + Session scoped sections
        let has_global = sections
            .iter()
            .any(|s| s.scope == crate::prompts::CacheScope::Global);
        let has_session = sections
            .iter()
            .any(|s| s.scope == crate::prompts::CacheScope::Session);
        assert!(has_global, "should have Global sections");
        assert!(has_session, "should have Session sections");
    }

    #[test]
    fn build_system_message_records_bridge_context_signals() {
        let active_skill_names = vec!["concise"];
        let learned_context_text = "matrixorigin => github";
        let memory_signal_hint =
            "\n\n⚡ MEMORY SIGNAL DETECTED: category=\"preference\", namespace=\"prefs\".";
        let implicit_feedback_hint =
            "\n\n## Implicit Feedback\nThe user is correcting the previous attempt.";
        let feedback_rules_hint = "\n\n[Learned Feedback Rules]\n- Rule: do not use mocks";
        let self_awareness_hint =
            "\n\n## Self-Awareness\nCurrent task: review runtime prompt assembly.";
        let session_anchor = "\n\n## Session Anchor\nOriginal task: optimize prompt tracing.";
        let dynamic_sections = vec![
            prompts::PromptSection::dynamic(
                "\n\n# Project Profile\ncwd: /test".to_string(),
                prompts::PromptTokenBucket::Environment,
            ),
            prompts::PromptSection::dynamic(
                "skill payload".to_string(),
                prompts::PromptTokenBucket::UserPreferences,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        active_output_skills: !active_skill_names.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "learned context payload".to_string(),
                prompts::PromptTokenBucket::UserPreferences,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        learned_runtime_context: !learned_context_text.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "memory signal payload".to_string(),
                prompts::PromptTokenBucket::Environment,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        memory_signal_detected: !memory_signal_hint.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "implicit feedback payload".to_string(),
                prompts::PromptTokenBucket::Environment,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        implicit_feedback: !implicit_feedback_hint.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "feedback rules payload".to_string(),
                prompts::PromptTokenBucket::Environment,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        learned_feedback_rules: !feedback_rules_hint.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "self awareness payload".to_string(),
                prompts::PromptTokenBucket::Environment,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        self_awareness: !self_awareness_hint.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "session anchor payload".to_string(),
                prompts::PromptTokenBucket::Environment,
            )
            .with_trace_signals(
                crate::turn::context_assembly_trace::PromptTraceSignals {
                    context_signals: crate::turn::context_assembly_trace::PromptContextSignals {
                        session_anchor: !session_anchor.is_empty(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ];
        let (_, _, prompt_sections) = build_system_message_with_dynamic_sections(
            &["bash", "read_file"],
            &dynamic_sections,
            0.8,
            Some("implementation"),
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );
        let breakdown = prompts::build_system_prompt_trace(&prompt_sections, vec![], vec![]);

        assert!(breakdown.context_signals.active_output_skills);
        assert!(breakdown.context_signals.learned_runtime_context);
        assert!(breakdown.context_signals.memory_signal_detected);
        assert!(breakdown.context_signals.self_awareness);
        assert!(breakdown.context_signals.implicit_feedback);
        assert!(breakdown.context_signals.learned_feedback_rules);
        assert!(breakdown.context_signals.session_anchor);
        assert!(!breakdown.context_signals.system_prompt_override);
        assert!(!breakdown.context_signals.effort_hint);
        assert!(!breakdown.context_signals.agent_type_hint);
        assert!(breakdown.environment_tokens > 0);
        assert!(breakdown.user_preferences_tokens > 0);
        // guidance_signals default to false — guard against accidental default changes
        assert!(!breakdown.guidance_signals.round_budget_warning);
        assert!(!breakdown.guidance_signals.synthesize_or_batch);
        assert!(!breakdown.guidance_signals.parallel_feedback);
    }

    #[test]
    fn build_system_prompt_trace_guidance_signals_only() {
        use crate::prompts::{CacheScope, PromptSection, PromptTokenBucket};
        use crate::turn::context_assembly_trace::{PromptGuidanceSignals, PromptTraceSignals};

        let section = PromptSection {
            text: "round budget warning".to_string(),
            scope: CacheScope::None,
            token_bucket: PromptTokenBucket::Environment,
            trace_signals: PromptTraceSignals {
                guidance_signals: PromptGuidanceSignals {
                    round_budget_warning: true,
                    synthesize_or_batch: true,
                    parallel_feedback: false,
                },
                ..Default::default()
            },
        };
        let breakdown = prompts::build_system_prompt_trace(&[section], vec![], vec![]);
        assert!(!breakdown.context_signals.active_output_skills);
        assert!(breakdown.guidance_signals.round_budget_warning);
        assert!(breakdown.guidance_signals.synthesize_or_batch);
        assert!(!breakdown.guidance_signals.parallel_feedback);
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
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );

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
        annotate_tool_schemas_for_caching(&mut tools, &PromptCacheConfig::latch("openai", "gpt-4"));
        assert!(
            tools[0].get("cache_control").is_none(),
            "OpenAI tools should not get cache_control"
        );
    }

    #[test]
    fn annotate_tool_schemas_only_last_tool() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        // bash and read_file are pinned; github_list_prs is dynamic
        let mut tools = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "read_file"}}),
            json!({"function": {"name": "github_list_prs"}}),
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );

        // Only the LAST tool should have cache_control (simplified strategy)
        assert!(
            tools[0].get("cache_control").is_none(),
            "first tool should not have cache_control"
        );
        assert!(
            tools[1].get("cache_control").is_none(),
            "middle tool should not have cache_control"
        );
        assert!(
            tools[2].get("cache_control").is_some(),
            "last tool should have cache_control"
        );
        assert_eq!(
            tools[2]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
        assert_eq!(tools[2]["cache_control"]["ttl"].as_str(), Some("1h"));
    }

    #[test]
    fn add_message_cache_breakpoint_targets_last_non_system() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let mut messages = vec![
            json!({"role": "system", "content": "sys prompt"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
        ];
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );

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
        add_message_cache_breakpoint(&mut messages, &PromptCacheConfig::latch("openai", "gpt-4"));

        // Should remain unchanged
        assert!(
            messages[1]["content"].is_string(),
            "OpenAI msgs should not be modified"
        );
    }

    // ── PromptCacheConfig unhappy-path / edge-case tests ────────────────────

    #[test]
    fn prompt_cache_config_latch_anthropic() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let cfg = PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514");
        assert!(cfg.cache_enabled, "cache should be enabled by default");
        assert!(
            cfg.is_anthropic,
            "anthropic provider should set is_anthropic"
        );
        assert!(
            cfg.should_annotate(),
            "anthropic with cache enabled should annotate"
        );
    }

    #[test]
    fn prompt_cache_config_latch_openai() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let cfg = PromptCacheConfig::latch("openai", "gpt-4");
        assert!(!cfg.is_anthropic, "openai should not be anthropic");
        assert!(
            !cfg.should_annotate(),
            "non-anthropic should not annotate even if cache enabled"
        );
    }

    #[test]
    fn prompt_cache_config_latch_unknown_provider() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let cfg = PromptCacheConfig::latch("my-custom-provider", "my-model");
        assert!(
            !cfg.is_anthropic,
            "unknown provider should not be anthropic"
        );
        assert!(
            !cfg.should_annotate(),
            "unknown provider should not annotate"
        );
    }

    #[test]
    fn prompt_cache_config_env_disabled() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("MO_PROMPT_CACHE_DISABLED", "1");
        }

        let cfg = PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514");
        assert!(
            !cfg.cache_enabled,
            "cache should be disabled when env var is set"
        );
        assert!(
            !cfg.should_annotate(),
            "should not annotate when cache disabled"
        );

        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }
    }

    #[test]
    fn prompt_cache_config_latch_idempotent() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let a = PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514");
        let b = PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(a.cache_enabled, b.cache_enabled, "idempotent cache_enabled");
        assert_eq!(a.is_anthropic, b.is_anthropic, "idempotent is_anthropic");
    }

    #[test]
    fn annotate_tool_schemas_noop_when_cache_disabled() {
        let cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: true,
        };
        let mut tools = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "read_file"}}),
        ];
        annotate_tool_schemas_for_caching(&mut tools, &cfg);

        for (i, tool) in tools.iter().enumerate() {
            assert!(
                tool.get("cache_control").is_none(),
                "tool {i} should not have cache_control when cache is disabled"
            );
        }
    }

    #[test]
    fn add_message_breakpoint_noop_when_not_anthropic() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: false,
        };
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hello"}),
        ];
        add_message_cache_breakpoint(&mut messages, &cfg);

        assert!(
            messages[1]["content"].is_string(),
            "messages should not be modified when not anthropic"
        );
    }

    #[test]
    fn intermediate_text_is_suppressed_when_tool_calls_exist() {
        let loop_text = "draft review text";
        let loop_tool_calls = [json!({
            "id": "call_1", "type": "function",
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
        let (sys, _, _) = build_system_message(
            &["bash", "read_file"],
            "cwd: /test",
            0.8,
            Some("implementation"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        assert!(
            tools.last().unwrap().get("cache_control").is_some(),
            "Layer 2: last tool should have cache_control"
        );

        // Layer 3: Message breakpoint
        let mut messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
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
        let (sys, _, _) = build_system_message(
            &["bash"],
            "cwd: /test",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        assert!(
            tools[0].get("cache_control").is_none(),
            "tools should not have cache_control when disabled"
        );

        // Layer 3: message breakpoint
        let mut messages = vec![json!({"role": "user", "content": "hello"})];
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        assert!(
            messages[0]["content"].is_string(),
            "messages should not be modified when cache disabled"
        );

        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }
    }

    // ── Message breakpoint edge cases ──────────────────────────────────

    #[test]
    fn message_breakpoint_skips_system_only() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("MO_PROMPT_CACHE_DISABLED");
        }

        let mut messages = vec![json!({"role": "system", "content": "sys prompt"})];
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
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
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
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
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );

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
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
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
        let (msg, _, _) = build_system_message(
            &tools,
            "profile",
            0.8,
            Some("code_review"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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

        let (msg, _, _) = build_system_message(
            &["bash", "read_file", "memory_store"],
            "profile",
            0.8,
            Some("debugging"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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

        let (msg1, _, _) = build_system_message(
            &["bash", "read_file"],
            "p1",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        let (msg2, _, _) = build_system_message(
            &["bash", "git_diff", "memory_store"],
            "p2",
            0.5,
            Some("debugging"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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
        let (msg_turn1, _, _) = build_system_message(
            &["bash", "read_file", "git_diff"],
            "turn1 profile",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        let (msg_turn2, _, _) = build_system_message(
            &["bash", "read_file", "git_diff"],
            "turn2 profile",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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

        let (msg1, dyn1, _) = build_system_message(
            &["bash", "read_file"],
            "turn1 profile",
            0.8,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );
        let (msg2, dyn2, _) = build_system_message(
            &["bash", "read_file"],
            "turn2 profile",
            0.8,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4o"),
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

        let (msg1, _, _) = build_system_message(
            &["bash"],
            "",
            0.8,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );
        let (msg2, _, _) = build_system_message(
            &["bash", "git_diff", "memory_store", "find_definition"],
            "",
            0.8,
            Some("code_review"),
            &PromptCacheConfig::latch("openai", "gpt-4o"),
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
        let (msg, _, _) = build_system_message(
            &tools,
            "",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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
        let (msg_none, _, _) = build_system_message(
            &tools,
            "",
            0.8,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        let (msg_review, _, _) = build_system_message(
            &tools,
            "",
            0.8,
            Some("code_review"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        let (msg_debug, _, _) = build_system_message(
            &tools,
            "",
            0.8,
            Some("debugging"),
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
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
        use astra_core::ErrorKind;
        assert_eq!(
            classify_llm_error("rate limit exceeded"),
            ErrorKind::RateLimit
        );
        assert_eq!(
            classify_llm_error("HTTP 429 Too Many Requests"),
            ErrorKind::RateLimit
        );
        assert_eq!(
            classify_llm_error("Rate limiting active"),
            ErrorKind::RateLimit
        );
    }

    #[test]
    fn classify_llm_error_timeout_variants() {
        use astra_core::ErrorKind;
        assert_eq!(classify_llm_error("request timeout"), ErrorKind::StreamIdle);
        assert_eq!(
            classify_llm_error("connection timed out"),
            ErrorKind::StreamIdle
        );
    }

    #[test]
    fn classify_llm_error_transport_variants() {
        use astra_core::ErrorKind;
        assert_eq!(
            classify_llm_error("connection refused"),
            ErrorKind::StreamTransport
        );
        assert_eq!(
            classify_llm_error("transport error"),
            ErrorKind::StreamTransport
        );
        assert_eq!(
            classify_llm_error("network unreachable"),
            ErrorKind::StreamTransport
        );
    }

    #[test]
    fn classify_llm_error_permission_variants() {
        use astra_core::ErrorKind;
        assert_eq!(classify_llm_error("HTTP 401"), ErrorKind::Auth);
        assert_eq!(classify_llm_error("unauthorized access"), ErrorKind::Auth);
        assert_eq!(classify_llm_error("invalid api key"), ErrorKind::Auth);
    }

    #[test]
    fn classify_llm_error_unknown_defaults_to_unknown() {
        use astra_core::ErrorKind;
        assert_eq!(
            classify_llm_error("something went wrong"),
            ErrorKind::Unknown
        );
        assert_eq!(classify_llm_error(""), ErrorKind::Unknown);
    }

    #[test]
    fn classify_llm_error_case_insensitive() {
        use astra_core::ErrorKind;
        assert_eq!(classify_llm_error("RATE LIMIT"), ErrorKind::RateLimit);
        assert_eq!(classify_llm_error("Timeout"), ErrorKind::StreamIdle);
        assert_eq!(classify_llm_error("UNAUTHORIZED"), ErrorKind::Auth);
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
    fn inprocess_session_info_event_includes_run_id() {
        let event = inprocess_session_info_event("sess-1", "run-1");
        assert_eq!(event["type"], "session_info");
        assert_eq!(event["session_id"], "sess-1");
        assert_eq!(event["run_id"], "run-1");
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        let s = std::str::from_utf8(&result[0]).unwrap();
        assert!(s.contains("text_delta"));
    }

    #[test]
    fn forward_event_reasoning_done_forwarded_as_sse() {
        let event = json!({"type": "reasoning_done"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        let s = std::str::from_utf8(&result[0]).unwrap();
        assert!(s.contains("reasoning_done"));
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
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
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tc,
            &mut usage,
            &mut model,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn await_with_client_disconnect_returns_future_output() {
        let token = CancellationToken::new();
        let result = await_with_client_disconnect(Some(&token), async { 42_u8 })
            .await
            .expect("future output");
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn await_with_client_disconnect_returns_disconnect_error() {
        let token = CancellationToken::new();
        token.cancel();
        let result = await_with_client_disconnect(Some(&token), async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            42_u8
        })
        .await
        .expect_err("disconnect error");
        assert_eq!(
            result.get("code").and_then(Value::as_str),
            Some("CLIENT_DISCONNECT")
        );
        assert_eq!(
            result.get("message").and_then(Value::as_str),
            Some("Request cancelled (client disconnected)")
        );
    }

    #[test]
    fn latest_user_message_text_prefers_last_user() {
        let messages = vec![
            json!({"role": "user", "content": "first prompt"}),
            json!({"role": "assistant", "content": "intermediate"}),
            json!({"role": "user", "content": "继续处理"}),
        ];
        assert_eq!(latest_user_message_text(&messages), Some("继续处理"));
    }

    #[test]
    fn turn_count_from_messages_counts_user_turns() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first prompt"}),
            json!({"role": "assistant", "content": "intermediate"}),
            json!({"role": "tool", "content": "tool output"}),
            json!({"role": "user", "content": "继续处理"}),
        ];
        assert_eq!(turn_count_from_messages(&messages), 2);
    }

    #[test]
    fn turn_count_from_messages_zero_without_user_messages() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "assistant", "content": "intermediate"}),
        ];
        assert_eq!(turn_count_from_messages(&messages), 0);
    }

    #[test]
    fn filter_round_edge_tools_excludes_restricted_tools() {
        let edge_tools = vec![
            json!({"type": "function", "function": {"name": "bash", "arguments": {}}}),
            json!({"type": "function", "function": {"name": "view", "arguments": {}}}),
        ];
        let restricted = std::collections::HashSet::from(["bash".to_string()]);

        let filtered = filter_round_edge_tools(&edge_tools, &restricted);

        assert_eq!(filtered.len(), 1);
        assert_eq!(tool_call_name(&filtered[0]), Some("view"));
    }

    #[test]
    fn record_turn_guard_tool_results_updates_error_summary() {
        let mut turn_guard = TurnGuard::new();

        record_turn_guard_tool_results(
            &mut turn_guard,
            &[json!({
                "name": "bash",
                "result": "{\"error\":\"permission denied\"}"
            })],
        );

        assert_eq!(turn_guard.errors.total_errors, 1);
    }

    #[test]
    fn turn_complete_event_includes_followup_suggestion_from_last_user() {
        let messages = vec![
            json!({"role": "user", "content": "first prompt"}),
            json!({"role": "assistant", "content": "intermediate"}),
            json!({"role": "user", "content": "继续处理"}),
        ];
        let event = turn_complete_event(&messages, "Should I continue?", &[]);
        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["has_tool_calls"], false);
        assert_eq!(event["followup_suggestion"], "继续");
    }

    // ── P1: L0 anchor appears in system prompt ──────────────────────────

    #[test]
    fn p1_anchor_injected_into_openai_dynamic_message() {
        use crate::turn::cloud::session_memory_protocol::extract_anchor;

        let anchor = extract_anchor("Build a distributed rate limiter using Redis", None);
        let profile_desc = format!("cwd: /home/user/project\n\n{anchor}");

        let cache_cfg = PromptCacheConfig {
            is_anthropic: false,
            cache_enabled: false,
        };
        let (_primary, dynamic, _sections) =
            build_system_message(&["read_file", "grep"], &profile_desc, 0.9, None, &cache_cfg);

        let dyn_content = dynamic
            .expect("OpenAI should have dynamic message")
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        assert!(
            dyn_content.contains("[session-anchor]"),
            "Dynamic message should contain anchor: {dyn_content}"
        );
        assert!(dyn_content.contains("rate limiter"));
    }

    #[test]
    fn p1_anchor_injected_into_anthropic_blocks() {
        use crate::turn::cloud::session_memory_protocol::extract_anchor;

        let anchor = extract_anchor("Refactor auth module to use JWT", None);
        let profile_desc = format!("cwd: /project\n\n{anchor}");

        let cache_cfg = PromptCacheConfig {
            is_anthropic: true,
            cache_enabled: true,
        };
        let (msg, _, _) =
            build_system_message(&["read_file"], &profile_desc, 0.9, None, &cache_cfg);

        // Anthropic: single message with content blocks array
        let blocks = msg.get("content").and_then(Value::as_array).unwrap();
        let all_text: String = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_text.contains("[session-anchor]"),
            "Anthropic blocks should contain anchor"
        );
        assert!(all_text.contains("JWT"));
    }

    // ── P2: Continuation prompt after compaction ────────────────────────

    #[test]
    fn p2_continuation_prompt_appended_after_compaction() {
        use crate::turn::cloud::memoria_compact::{
            MemoriaCompactConfig, MemoriaCompactParams, compact_with_memoria,
        };

        let mut messages: Vec<Value> = vec![json!({"role": "system", "content": "sys"})];
        messages.push(json!({"role": "user", "content": "Build X"}));
        for i in 0..20 {
            messages.push(
                json!({"role": "assistant", "content": format!("Step {i} {}", "x".repeat(400))}),
            );
            messages.push(json!({"role": "user", "content": format!("Next {}", i + 1)}));
        }

        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 3000,
            keep_chars: 1500,
            tier: crate::prompts::CompactionTier::AggressivePrune,
            keep_recent_turns: 2,
            current_tokens: 80000,
            session_memory_file: None,
            session_memory_combine:
                crate::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
            session_facts: None,
        };

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(compact_with_memoria(
                &messages, None, &config, &params, None, None, None,
            ));

        // Compaction happened (boundary present), so we simulate what the turn loop does
        assert!(
            result.boundary.is_some(),
            "compaction should have triggered"
        );

        let mut msgs = result.messages;
        if result.boundary.is_some() && msgs.len() >= 2 {
            msgs.push(json!({
                "role": "user",
                "content": "Continue the conversation from where it left off. \
                            Do not ask the user any further questions — \
                            pick up the current task and keep going."
            }));
        }

        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().contains("Continue"));
        assert!(last["content"].as_str().unwrap().contains("keep going"));
    }

    #[test]
    fn p2_no_continuation_when_no_compaction() {
        use crate::turn::cloud::memoria_compact::{
            MemoriaCompactConfig, MemoriaCompactParams, compact_with_memoria,
        };

        // Small conversation — no compaction needed
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];

        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 100_000,
            keep_chars: 50_000,
            tier: crate::prompts::CompactionTier::CompactHistory,
            keep_recent_turns: 2,
            current_tokens: 500,
            session_memory_file: None,
            session_memory_combine:
                crate::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
            session_facts: None,
        };

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(compact_with_memoria(
                &messages, None, &config, &params, None, None, None,
            ));

        assert!(result.boundary.is_none(), "no compaction should happen");
        // No continuation prompt should be added
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages.last().unwrap()["role"], "assistant");
    }

    #[test]
    fn p2_no_continuation_when_last_message_is_user() {
        use crate::turn::cloud::memoria_compact::{
            MemoriaCompactConfig, MemoriaCompactParams, compact_with_memoria,
        };

        // Build conversation where last message after compaction will be user
        let mut messages: Vec<Value> = vec![json!({"role": "system", "content": "sys"})];
        messages.push(json!({"role": "user", "content": "Build X"}));
        for i in 0..20 {
            messages.push(
                json!({"role": "assistant", "content": format!("Step {i} {}", "x".repeat(400))}),
            );
            messages.push(json!({"role": "user", "content": format!("Next {}", i + 1)}));
        }

        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 3000,
            keep_chars: 1500,
            tier: crate::prompts::CompactionTier::AggressivePrune,
            keep_recent_turns: 2,
            current_tokens: 80000,
            session_memory_file: None,
            session_memory_combine:
                crate::turn::cloud::memoria_compact::SessionMemoryFileCombine::None,
            session_facts: None,
        };

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(compact_with_memoria(
                &messages, None, &config, &params, None, None, None,
            ));

        assert!(result.boundary.is_some(), "compaction should trigger");

        // Simulate the turn loop's P2 logic
        let mut msgs = result.messages;
        if result.boundary.is_some() && msgs.len() >= 2 {
            let last_is_user = msgs
                .last()
                .and_then(|m| m.get("role").and_then(Value::as_str))
                == Some("user");
            if !last_is_user {
                msgs.push(json!({
                    "role": "user",
                    "content": "Continue..."
                }));
            }
        }

        // Verify no consecutive user messages
        for window in msgs.windows(2) {
            let r0 = window[0].get("role").and_then(Value::as_str).unwrap_or("");
            let r1 = window[1].get("role").and_then(Value::as_str).unwrap_or("");
            assert!(
                !(r0 == "user" && r1 == "user"),
                "Consecutive user messages found: [{r0}] then [{r1}]"
            );
        }
    }

    #[test]
    fn p1_anchor_handles_anthropic_content_blocks_in_user_message() {
        use crate::turn::cloud::session_memory_protocol::extract_anchor;

        // Simulate Anthropic-style content blocks in user message
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "Build a distributed cache with LRU eviction"}
            ]}),
        ];

        let first_user_text = messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|m| {
                m.get("content").and_then(|c| {
                    c.as_str().map(String::from).or_else(|| {
                        c.as_array().and_then(|blocks| {
                            blocks.iter().find_map(|b| {
                                b.get("text").and_then(Value::as_str).map(String::from)
                            })
                        })
                    })
                })
            });

        assert!(
            first_user_text.is_some(),
            "Should extract text from content blocks"
        );
        let anchor = extract_anchor(&first_user_text.unwrap(), None);
        assert!(anchor.contains("distributed cache"));
    }

    #[test]
    fn p1_anchor_does_not_break_cached_prefix() {
        use crate::turn::cloud::session_memory_protocol::extract_anchor;

        let cache_cfg = PromptCacheConfig {
            is_anthropic: false,
            cache_enabled: true,
        };
        let tools = &["read_file", "grep"];

        // Turn 1: no anchor
        let (primary1, _, _) = build_system_message(tools, "cwd: /project", 0.9, None, &cache_cfg);

        // Turn 2: with anchor
        let anchor = extract_anchor("Build rate limiter", None);
        let profile_with_anchor = format!("cwd: /project\n\n{anchor}");
        let (primary2, _, _) =
            build_system_message(tools, &profile_with_anchor, 0.9, None, &cache_cfg);

        // Stable cached primary must be identical — anchor only in dynamic
        assert_eq!(
            primary1.get("content").and_then(Value::as_str),
            primary2.get("content").and_then(Value::as_str),
            "Anchor must not change the cached primary system message"
        );
    }

    #[test]
    fn p3_usage_key_is_prompt_tokens_not_prompt() {
        // Regression: the P3 code previously used usage.get("prompt") which always
        // returned None. The correct key from LLM providers is "prompt_tokens".
        let mut usage = serde_json::Map::new();
        usage.insert("prompt_tokens".to_string(), json!(45000));
        usage.insert("completion_tokens".to_string(), json!(2000));

        // This is the exact expression from the P3 code path
        let estimated_tokens = usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0) as usize;
        assert_eq!(estimated_tokens, 45000);

        // The old buggy key must NOT work
        let wrong = usage.get("prompt").and_then(Value::as_i64).unwrap_or(0) as usize;
        assert_eq!(
            wrong, 0,
            "usage.get(\"prompt\") should return None — the key is prompt_tokens"
        );
    }

    // ── Fix #1: anchor evolves with L1 ──────────────────────────────────

    #[test]
    fn p1_anchor_evolves_when_l1_available() {
        use crate::turn::cloud::session_memory_protocol::{
            SESSION_MEMORY_PREFIX, SessionMemory, extract_anchor,
        };

        // Without L1 — shows "starting"
        let anchor_no_l1 = extract_anchor("Build rate limiter", None);
        assert!(anchor_no_l1.contains("starting"));
        assert!(anchor_no_l1.contains("0/0"));

        // With L1 — shows current state and progress
        let l1_text = format!(
            "{SESSION_MEMORY_PREFIX}\n\
             # Session Title\nRate Limiter\n\
             # Task Specification\nBuild a distributed rate limiter.\n\
             # Current State\nRedis integration complete, testing.\n\
             # Key Files\nsrc/main.rs\n\
             # Progress\n✅ Setup\n✅ Redis\n🔄 Testing\n⏳ Deploy\n\
             # Errors & Corrections\nNone\n\
             # Decisions\n- Use Redis\n\
             # User Messages\nBuild rate limiter\n\
             # Worklog\nT1\n\
             # Context\nT5"
        );
        let l1 = SessionMemory::parse(&l1_text).unwrap();
        let anchor_with_l1 = extract_anchor("Build rate limiter", Some(&l1));

        assert!(
            !anchor_with_l1.contains("starting"),
            "should not say 'starting' when L1 available"
        );
        assert!(
            anchor_with_l1.contains("Redis integration"),
            "should show current state from L1"
        );
        assert!(
            anchor_with_l1.contains("2/4"),
            "should show progress from L1"
        );
    }

    // ── Fix #4: P2 skips continuation when task is done ─────────────────

    /// Helper matching the actual P2 completion detection logic in the turn loop.
    fn signals_done(content: &str) -> bool {
        let tail = if content.len() > 200 {
            &content[content.floor_char_boundary(content.len() - 200)..]
        } else {
            content
        };
        let lower = tail.to_ascii_lowercase();
        let has_completion = lower.contains("task complete")
            || lower.contains("all done")
            || lower.contains("finished")
            || lower.contains("completed successfully")
            || lower.contains("任务完成")
            || lower.contains("已完成");
        if !has_completion {
            return false;
        }
        let has_negation = lower.contains("not yet")
            || lower.contains("not complete")
            || lower.contains("not finished")
            || lower.contains("haven't finished")
            || lower.contains("hasn't finished")
            || lower.contains("won't be finished")
            || lower.contains("don't think")
            || lower.contains("not sure")
            || lower.contains("没有完成")
            || lower.contains("尚未完成")
            || lower.contains("except")
            || lower.contains("but ");
        has_completion && !has_negation
    }

    #[test]
    fn p2_no_continuation_when_task_complete() {
        assert!(signals_done(
            "All tasks completed successfully. The rate limiter is deployed."
        ));
    }

    #[test]
    fn p2_continuation_when_task_in_progress() {
        assert!(!signals_done(
            "I've implemented step 3. Working on step 4 next."
        ));
    }

    #[test]
    fn p2_no_continuation_chinese_completion() {
        assert!(signals_done("所有步骤已完成，任务完成！"));
    }

    #[test]
    fn p2_no_false_positive_negated_finished() {
        assert!(!signals_done(
            "I haven't finished yet, still working on it."
        ));
    }

    #[test]
    fn p2_no_false_positive_not_complete() {
        assert!(!signals_done(
            "The task is not yet complete, need more work."
        ));
    }

    #[test]
    fn p2_no_false_positive_all_done_except() {
        assert!(!signals_done("All done except the deployment step."));
    }

    #[test]
    fn p2_no_false_positive_wont_be_finished() {
        assert!(!signals_done(
            "The task won't be finished until deployment is done."
        ));
    }

    #[test]
    fn p2_no_false_positive_cannot_be_completed() {
        assert!(!signals_done("This cannot be completed today."));
    }

    #[test]
    fn p2_no_false_positive_dont_think_finished() {
        assert!(!signals_done("I don't think we're finished yet."));
    }

    #[test]
    fn p2_true_positive_cant_believe_completed() {
        // "can't" in a non-negating context should NOT suppress completion detection
        assert!(signals_done("I can't believe we completed successfully!"));
    }

    // ── P1 latency fix: anchor from local messages, no network ──────────

    #[test]
    fn p1_anchor_evolves_from_local_messages_no_network() {
        use crate::turn::cloud::session_memory_protocol::{
            SessionMemory, build_l1_from_messages, extract_anchor,
        };

        // Multi-turn conversation — anchor should show progress, not "starting"
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "Build a rate limiter using Redis"}),
            json!({"role": "assistant", "content": "Starting implementation.", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}
            ]}),
            json!({"role": "tool", "content": "fn main() {}", "tool_call_id": "c1"}),
            json!({"role": "assistant", "content": "Done with step 1."}),
            json!({"role": "user", "content": "Now add Redis connection"}),
            json!({"role": "assistant", "content": "Added Redis."}),
        ];

        let turn_count = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .count();
        assert_eq!(turn_count, 3);

        let l1_text = build_l1_from_messages(&messages, turn_count, 0);
        let l1 = SessionMemory::parse(&l1_text).unwrap();
        let anchor = extract_anchor("Build a rate limiter using Redis", Some(&l1));

        assert!(
            !anchor.contains("starting"),
            "multi-turn anchor should not say 'starting'"
        );
        assert!(anchor.contains("Turn 3"), "should reflect current turn");
    }

    // ── Fix #11: CJK detection for bilingual continuation prompt ────────

    #[test]
    fn p2_cjk_detection_chinese_content() {
        let msgs = vec![
            json!({"role": "assistant", "content": "我已经完成了第一步的实现，接下来处理数据库连接。"}),
            json!({"role": "user", "content": "继续"}),
        ];
        let is_cjk = msgs
            .iter()
            .rev()
            .take(4)
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .any(|c| {
                c.chars()
                    .take(200)
                    .filter(|ch| ('\u{4e00}'..='\u{9fff}').contains(ch))
                    .count()
                    > 10
            });
        assert!(is_cjk, "should detect Chinese content");
    }

    #[test]
    fn p2_cjk_detection_english_content() {
        let msgs = vec![
            json!({"role": "assistant", "content": "I've implemented step 1. Working on the database connection next."}),
            json!({"role": "user", "content": "continue"}),
        ];
        let is_cjk = msgs
            .iter()
            .rev()
            .take(4)
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .any(|c| {
                c.chars()
                    .take(200)
                    .filter(|ch| ('\u{4e00}'..='\u{9fff}').contains(ch))
                    .count()
                    > 10
            });
        assert!(!is_cjk, "should not detect CJK in English content");
    }
}
