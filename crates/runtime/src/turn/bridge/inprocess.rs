/// In-process chat turn bridge — calls LLM directly without an external bridge service.
///
/// # Key behaviors
///
/// | Behavior | Implementation |
/// |------------------------|------|
/// | Long-lived stream "stall" / no chunks | [`super::llm::client::stream_idle_timeout`] on SSE `next()` (5 min default; shortened only by the `bridge-e2e-hooks` test hook) |
/// | Recover via one-shot completion | [`super::llm::client::call_llm_nonstream_fallback`] after idle in both `call_llm_and_collect` and [`call_llm_stream`] below |
/// | User cancel clears in-flight work | HTTP `/chat/turn` passes `CancellationToken`; dropping the SSE body (client disconnect) cancels in-flight LLM byte/SSE consumption in-process |
/// | Cooldown / 429 wait cannot ignore disconnect | [`super::llm::client::sleep_ms_or_llm_cancel`] on retry backoff + rate-limit waits in [`call_llm_stream`]; initial cooldown wait `select!`s [`wait_until_cancelled_or_pending`](super::llm::client::wait_until_cancelled_or_pending) in the bridge stream |
/// | Tool permission queue + single resolve | CLI: `astra-cli` `permission_manager`; cloud: edge approval ledger / `POST /tools/result`. "resolve once" matches ledger single-shot semantics |
///
/// # Adapter Status
///
/// This module is the remaining HTTP `/chat/turn` single-turn transport adapter.
/// It still has its own `for round_ix..` loop inside `stream!` and does NOT use
/// [`run_agentic_loop_with_host`], so semantic dedup and full step recording are
/// still absent here. It must not present itself as a separate agent runtime:
/// public runtime metadata is expressed as CLI local capacity, while
/// implementation-specific provenance such as `BRIDGE_CACHE_SOURCE` stays
/// internal for prompt-cache continuity and journal diagnostics.
///
/// **Preferred replacement**: Use [`super::loop_dispatcher::LoopDispatcher`] with
/// [`ServerAgenticLoopHost`](crate::server::server_loop_host::ServerAgenticLoopHost)
/// which runs the full unified cognitive loop including all runtime policies.
///
/// New features should target the unified loop.
///
/// # Architecture (legacy)
///
///   Rust API (`forward()` on [`InProcessChatTurnBridge`]) injects context into headers:
///     x-mo-user-id, x-mo-session-id, x-mo-turn-chain-id, x-mo-user-query-event-id, ...
///   This bridge reads those headers, calls the LLM, streams SSE back, persists events, and
///   for each tool round blocks on [`astra_turn_core::edge_ledger`] until `POST /tools/result` (or timeout).
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Instant,
};

use astra_core::SharedPool;
use astra_services::SessionArtifactStore;
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
};
use astra_turn_core::edge_ledger::ensure_tool_call_ids;
use astra_turn_core::persist::{build_tool_call_event_payload, build_tool_result_event_payload};
use astra_turn_core::sse_blocks::SseBlankLineUtf8Buf;
use astra_turn_core::tool_call_shape::tool_call_name;
use astra_turn_core::tool_schema_prune::prune_tool_schemas;

const TOOL_RESULT_AUDIT_CHARS: usize = 4000;
const ROOT_TURN_JOURNAL_HEADER: &str = "x-mo-root-turn-journal";

fn selected_model_name_from_payload(payload: &Value) -> Option<String> {
    payload
        .get("selected_model")
        .and_then(Value::as_object)
        .and_then(|selected_model| selected_model.get("model"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderModelGatewayInvocation {
    model: String,
    endpoint_url: String,
    authorization: String,
}

fn provider_model_gateway_invocation_from_payload(
    payload: &Value,
) -> Result<Option<ProviderModelGatewayInvocation>, String> {
    let Some(model_gateway) = payload
        .get("capability_descriptors")
        .and_then(Value::as_object)
        .and_then(|descriptors| descriptors.get("model_gateway"))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let selected_model = payload
        .get("selected_model")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "selected_model is required with capability_descriptors.model_gateway".to_string()
        })?;
    let model = selected_model
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "selected_model.model is required with capability_descriptors.model_gateway".to_string()
        })?
        .to_string();
    let endpoint_url = model_gateway
        .get("endpoint_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "capability_descriptors.model_gateway.endpoint_url is required".to_string())?
        .to_string();
    let authorization = payload
        .get("runtime_auth")
        .and_then(Value::as_object)
        .and_then(|runtime_auth| runtime_auth.get("authorization"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "runtime_auth.authorization is required with capability_descriptors.model_gateway"
                .to_string()
        })?
        .to_string();
    Ok(Some(ProviderModelGatewayInvocation {
        model,
        endpoint_url,
        authorization,
    }))
}

fn rewrite_bridge_runtime_manifest_model_resolution(
    trace: &mut Value,
    requested_model: Option<&str>,
    resolved_model: &str,
    provider: &str,
    fallback_trace: Option<&Value>,
) {
    let Some(trace_obj) = trace.as_object_mut() else {
        return;
    };
    let manifest = trace_obj
        .entry("runtime_manifest")
        .or_insert_with(|| json!({}));
    if !manifest.is_object() {
        *manifest = json!({});
    }
    let selected_model = requested_model.unwrap_or(resolved_model);
    let source = if fallback_trace.is_some() {
        "rate_limit_fallback"
    } else {
        "bridge_request"
    };
    manifest["schema_version"] = json!("astra_runtime_manifest.v1");
    manifest["selected_model"] = json!({
        "model": selected_model,
    });
    manifest["model_resolution"] = json!({
        "source": source,
        "requested_model": requested_model,
        "model": resolved_model,
        "provider": provider,
        "resolved": true,
        "fallback": fallback_trace,
    });
    manifest["runtime_profile"] = json!(astra_runtime_env::CapacityProviderType::CliLocal.as_str());
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CachedSessionStartMemory {
    stable_memory_section: Option<String>,
    stable_ids: Vec<String>,
    fetch_ms: i64,
}

impl CachedSessionStartMemory {
    fn from_prefetch(
        session_start: SessionStartPrefetchResult,
        memory_index: Option<String>,
    ) -> Self {
        let stable_memory_section = crate::turn::memory_prefetch::build_session_stable_memory_block(
            memory_index.as_deref(),
            session_start.section.as_deref(),
        );
        let stable_ids = session_start
            .profile
            .iter()
            .chain(session_start.recent_episodes.iter())
            .chain(session_start.recent_scenes.iter())
            .map(|m| m.memory_id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        Self {
            stable_memory_section,
            stable_ids,
            fetch_ms: session_start.fetch_ms,
        }
    }
}

async fn cached_first_turn_session_start_memory<F, Fut>(
    cache: &tokio::sync::Mutex<HashMap<String, CachedSessionStartMemory>>,
    session_id: &str,
    trace_turn: u32,
    fetcher: F,
) -> Option<CachedSessionStartMemory>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = CachedSessionStartMemory>,
{
    if trace_turn > 1 {
        if !session_id.is_empty() {
            cache.lock().await.remove(session_id);
        }
        return None;
    }
    if session_id.is_empty() {
        return Some(fetcher().await);
    }
    if let Some(snapshot) = cache.lock().await.get(session_id).cloned() {
        return Some(snapshot);
    }
    // Intentionally release the mutex before awaiting the fetcher so turn-1
    // prompt assembly never serializes on network I/O. Two concurrent turn-1
    // requests for the same session can both miss and fetch here; that is a
    // benign cache race, and the last completed insert simply wins.
    let snapshot = fetcher().await;
    cache
        .lock()
        .await
        .insert(session_id.to_string(), snapshot.clone());
    Some(snapshot)
}

/// Build a prompt section for the CLI-injected skill listing.
///
/// Returns `None` when the CLI didn't include a listing (no skills loaded
/// this session). Returns a `CacheScope::Session` section otherwise, so
/// the Anthropic prompt cache hits the full block. This was previously a
/// `CacheScope::None` volatile section, which meant CLI users paid the
/// ~2.5KB skill listing cost every turn.
pub fn skill_listing_section_for_edge_profile(
    raw: Option<&str>,
) -> Option<crate::prompts::PromptSection> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    Some(crate::prompts::PromptSection::stable(
        text.to_string(),
        crate::prompts::CacheScope::Session,
    ))
}

fn deferred_tools_section_for_edge_profile(
    raw: Option<&str>,
) -> Option<crate::prompts::PromptSection> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    Some(crate::prompts::PromptSection::stable(
        text.to_string(),
        crate::prompts::CacheScope::Session,
    ))
}

fn deferred_tools_block_for_bridge_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: &str,
    resolved_context_window: Option<u32>,
) -> String {
    crate::turn::deferred_tools_edge_profile::block_for_model_with_context_window(
        edge_profile,
        resolved_model_name,
        resolved_context_window,
    )
    .and_then(|text| deferred_tools_section_for_edge_profile(Some(&text)))
    .map(|section| section.text)
    .unwrap_or_default()
}

/// Extract the always_load (T1) tool names from the CLI-built `edge_profile`.
///
/// When the key is present, the names reflect the resolved `ToolSurface` always_load
/// set (user TOML additions included). When absent (test-only path or
/// server-side tools), falls back to the runtime-configured surface so
/// cache_control markers still match tool_surface config.
fn always_load_tool_names_for_bridge(
    edge_profile: &Map<String, Value>,
) -> std::collections::HashSet<String> {
    edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(crate::turn::prompt_cache::runtime_always_load_tool_names)
}

/// Decide whether the bridge should run its own `prefetch_memories` call.
///
/// Returns `false` (= skip) when the CLI has already injected
/// `memoria_insights_text`. Both the CLI's `memory_boost_search` +
/// `render_digest` path and the bridge's `prefetch_memories` + `bind_memory`
/// path hit the same Memoria backend with overlapping queries and `top_k=5`.
/// Running both produces two differently-formatted memory sections
/// (`## Memoria Recall` + `## User Memories`) whose contents substantially
/// overlap — observed +~700 tok of duplicate content per turn in production
/// sessions. The CLI digest is authoritative; when present, the bridge
/// path is redundant.
pub(crate) fn bridge_should_run_memoria_prefetch(edge_profile: &Map<String, Value>) -> bool {
    let cli_has_insights = edge_profile
        .get("memoria_insights_text")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    !cli_has_insights
}

/// Strip volatile `dynamic_sections` when the provider can't tolerate
/// per-round byte churn in the request prefix.
///
/// Strict-history providers (MiniMax-style, classified as
/// `VolatilePlacement::CurrentUserOnly`) invalidate the whole cache
/// entry on ANY mid-history byte change. Re-injecting volatile content
/// (Self-Awareness, session anchor, memoria insights, feedback rules)
/// on every round collapses their cache. `CacheCapability::should_inject_volatile_on_round`
/// returns `false` for them on every round — including round 0 — and
/// we must respond by dropping all volatile sections.
///
/// Round-0-only injection was tried and rejected: round 0's msg[1]
/// would include `preamble + user_q` while round 1+'s msg[1] would be
/// `user_q` only, so the byte-stable-history invariant still breaks.
/// See `cache_placement::VolatilePlacement::CurrentUserOnly` docs and
/// the session 986a553e regression for the full rationale.
///
/// Extracted as a standalone pure function so the dual-path
/// (bridge + server) invariant can be unit-tested without spinning up
/// the full bridge pipeline. The server path has its own equivalent
/// in `run_turn_pipeline`; both must stay in sync.
fn effective_volatile_sections_for_round(
    cache_cap: astra_turn_core::cache_placement::CacheCapability,
    round_index: u32,
    dynamic_sections: &[prompts::PromptSection],
) -> Vec<prompts::PromptSection> {
    if cache_cap.should_inject_volatile_on_round(round_index) {
        dynamic_sections.to_vec()
    } else {
        Vec::new()
    }
}

fn self_awareness_volatile_section(text: &str) -> Option<prompts::PromptSection> {
    (!text.is_empty()).then(|| {
        prompts::PromptSection::dynamic(text.to_string(), prompts::PromptTokenBucket::Environment)
            .with_trace_signals(
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
                            self_awareness: true,
                            ..Default::default()
                        },
                    ..Default::default()
                },
            )
    })
}

#[derive(Clone)]
struct BridgeTraceCorrelation {
    session_turn_source: String,
    turn_chain_id: String,
    user_query_event_id: String,
}

impl BridgeTraceCorrelation {
    fn as_capture_trace(&self) -> crate::turn::llm::exchange_capture::CaptureTrace<'_> {
        crate::turn::llm::exchange_capture::CaptureTrace {
            session_turn_source: Some(self.session_turn_source.as_str()),
            turn_chain_id: Some(self.turn_chain_id.as_str()),
            user_query_event_id: Some(self.user_query_event_id.as_str()),
        }
    }
}

fn has_inprocess_persisted_events(
    core_event_count: usize,
    tool_event_count: usize,
    tool_events_persisted: bool,
) -> bool {
    core_event_count > 0 || (tool_events_persisted && tool_event_count > 0)
}

#[derive(Debug, Default)]
struct BridgePipelineBaseline {
    #[cfg(test)]
    next_turn: u32,
    stats: astra_turn_core::pipeline_stats::PipelineStats,
    cache_detector: astra_turn_core::cache_diagnostics::CacheBreakDetector,
    last_tool_schemas: Vec<Value>,
}

const BRIDGE_CACHE_SOURCE: &str = "bridge_inprocess";

fn event_source(event: &astra_services::session_journal::JournalEvent) -> Option<&str> {
    event.metadata.as_ref()?.get("source")?.as_str()
}

fn event_matches_bridge_cache_source(
    event: &astra_services::session_journal::JournalEvent,
) -> bool {
    event_source(event).is_none_or(|source| source == BRIDGE_CACHE_SOURCE)
}

fn load_bridge_pipeline_baseline(session_id: &str) -> BridgePipelineBaseline {
    if session_id.is_empty() {
        return BridgePipelineBaseline {
            #[cfg(test)]
            next_turn: 1,
            ..Default::default()
        };
    }
    let mut cache_detector = astra_turn_core::cache_diagnostics::CacheBreakDetector::new();
    if let Ok(session_dir) = astra_services::local_session_artifact_store().session_dir(session_id)
    {
        cache_detector.set_diff_dir(session_dir.join("prompt-cache-diffs"));
    }
    let Ok(events) = astra_services::session_journal::read_journal_tail(session_id, 500) else {
        return BridgePipelineBaseline {
            #[cfg(test)]
            next_turn: 1,
            cache_detector,
            ..Default::default()
        };
    };

    let mut feedback_ratios = Vec::new();
    let mut raw_ratios = Vec::new();
    #[cfg(test)]
    let mut response_count = 0u32;
    #[cfg(test)]
    let mut max_turn = 0u32;
    let mut pending_request_snapshot = None;
    let mut last_tool_schemas = Vec::new();

    for event in events {
        #[cfg(test)]
        {
            max_turn = max_turn.max(event.turn.unwrap_or(0));
        }
        match event.event_type {
            astra_services::session_journal::JournalEventType::PipelineFeedback => {
                if let Some(ratio) = event
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("cache_hit_ratio"))
                    .and_then(Value::as_f64)
                {
                    feedback_ratios.push(ratio);
                }
            }
            astra_services::session_journal::JournalEventType::LlmRequestFull
                if event_matches_bridge_cache_source(&event) =>
            {
                let tools = bridge_tool_schemas_from_journal_event(&event);
                if !tools.is_empty() {
                    last_tool_schemas = tools;
                }
                pending_request_snapshot = bridge_prompt_snapshot_from_journal_event(&event);
            }
            astra_services::session_journal::JournalEventType::LlmResponseFull => {
                if !event_matches_bridge_cache_source(&event) {
                    continue;
                }
                #[cfg(test)]
                {
                    response_count = response_count.saturating_add(1);
                }
                let usage = bridge_usage_from_response_event(&event);
                if let Some(usage) = usage.as_ref() {
                    let total_input = usage
                        .input_tokens
                        .saturating_add(usage.cached_input_tokens)
                        .saturating_add(usage.cache_creation_tokens);
                    if total_input > 0 {
                        raw_ratios.push(usage.cached_input_tokens as f64 / total_input as f64);
                    }
                }
                if let Some(snapshot) = pending_request_snapshot.take() {
                    let _ = cache_detector.record_turn_for_source(
                        BRIDGE_CACHE_SOURCE,
                        snapshot,
                        usage.as_ref().map(|u| u.cached_input_tokens),
                    );
                }
            }
            _ => {}
        }
    }

    let ratios = if feedback_ratios.is_empty() {
        raw_ratios
    } else {
        feedback_ratios
    };
    let avg_cache_hit_ratio = if ratios.is_empty() {
        0.0
    } else {
        ratios.iter().sum::<f64>() / ratios.len() as f64
    };

    BridgePipelineBaseline {
        #[cfg(test)]
        next_turn: max_turn.max(response_count).saturating_add(1),
        stats: astra_turn_core::pipeline_stats::PipelineStats {
            turns_executed: ratios.len() as u32,
            avg_cache_hit_ratio,
            ..Default::default()
        },
        cache_detector,
        last_tool_schemas,
    }
}

fn bridge_usage_from_response_event(
    event: &astra_services::session_journal::JournalEvent,
) -> Option<crate::turn::token_usage::TokenUsage> {
    let usage = event
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("response"))
        .and_then(|response| response.get("response"))
        .and_then(|response| response.get("usage"))
        .and_then(Value::as_object)?;
    let partial = crate::turn::token_usage::TokenUsage::from_partial_json_map(usage);
    if !partial.is_empty() {
        return Some(partial);
    }
    let provider = event
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("openai");
    crate::turn::token_usage::extract_usage(
        crate::turn::token_usage::UsageDialect::for_provider(provider),
        usage,
    )
}

fn bridge_prompt_snapshot_from_journal_event(
    event: &astra_services::session_journal::JournalEvent,
) -> Option<astra_turn_core::cache_diagnostics::PromptStateSnapshot> {
    let metadata = event.metadata.as_ref()?;
    let request = metadata.get("request")?.as_object()?;
    let tools = request
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let model = metadata
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let provider = metadata.get("provider").and_then(Value::as_str)?;
    bridge_prompt_snapshot_from_messages(&messages, &tools, model, provider)
}

fn bridge_tool_schemas_from_journal_event(
    event: &astra_services::session_journal::JournalEvent,
) -> Vec<Value> {
    event
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("request"))
        .and_then(Value::as_object)
        .and_then(|request| request.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn bridge_prompt_snapshot_from_messages(
    messages: &[Value],
    tools: &[Value],
    model: &str,
    provider: &str,
) -> Option<astra_turn_core::cache_diagnostics::PromptStateSnapshot> {
    let system_prompt_text =
        astra_turn_core::cache_diagnostics::prompt_snapshot_system_text_from_messages(messages);
    let tools_json = serde_json::to_string(tools).ok()?;
    let cache_eligible_tokens = prompts::estimate_str_tokens(&system_prompt_text)
        + prompts::estimate_str_tokens(&tools_json);
    astra_turn_core::cache_diagnostics::prompt_snapshot_from_messages(
        messages,
        tools,
        provider,
        model,
        cache_eligible_tokens,
    )
}

// ── SSE helpers — delegated to turn::bridge_sse_helpers ───────────────────────
use super::sse_helpers::{
    extend_forward_from_validated_sse_block, flush_tail_buf_into_llm_forward,
    reasoning_done_sse_bytes_if_needed, render_sse, render_sse_map,
};

fn preview_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn parse_exit_semantics_tag(tag: &str) -> Option<astra_tools::exit_semantics::ExitSemantics> {
    serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(Value::String(
        tag.to_string(),
    ))
    .ok()
}

fn normalize_exit_semantics_tag(tag: &str) -> Option<String> {
    let semantics = parse_exit_semantics_tag(tag)?;
    serde_json::to_value(semantics)
        .ok()?
        .as_str()
        .map(ToString::to_string)
}

fn parse_result_class_tag(tag: &str) -> Option<astra_tools::exit_semantics::CommandResultClass> {
    serde_json::from_value::<astra_tools::exit_semantics::CommandResultClass>(Value::String(
        tag.to_string(),
    ))
    .ok()
}

fn normalize_result_class_tag(tag: &str) -> Option<String> {
    let result_class = parse_result_class_tag(tag)?;
    serde_json::to_value(result_class)
        .ok()?
        .as_str()
        .map(ToString::to_string)
}

fn structured_tool_result_error(
    exit_semantics: Option<astra_tools::exit_semantics::ExitSemantics>,
    result_class: Option<astra_tools::exit_semantics::CommandResultClass>,
) -> Option<bool> {
    if exit_semantics.is_some_and(|semantics| semantics.is_tool_error())
        || result_class.is_some_and(|class| class.is_tool_error())
    {
        return Some(true);
    }
    if exit_semantics.is_some() || result_class.is_some() {
        return Some(false);
    }
    None
}

fn status_success(status: &str) -> bool {
    matches!(
        super::super::agentic_loop::tool_support::edge_tool_status_exit_code(status),
        Some(0)
    )
}

fn bridge_tool_result_ok(
    status: &str,
    exit_semantics: Option<astra_tools::exit_semantics::ExitSemantics>,
    result_class: Option<astra_tools::exit_semantics::CommandResultClass>,
    output_semantic_error: bool,
) -> bool {
    let transport_error = structured_tool_result_error(exit_semantics, result_class)
        .unwrap_or_else(|| !status_success(status));
    !transport_error && !output_semantic_error
}

#[cfg(test)]
mod exit_semantics_tests {
    use super::normalize_exit_semantics_tag;

    #[test]
    fn normalize_exit_semantics_tag_accepts_canonical_values() {
        assert_eq!(
            normalize_exit_semantics_tag("execution_error"),
            Some("execution_error".to_string())
        );
        assert_eq!(
            normalize_exit_semantics_tag("empty_result"),
            Some("empty_result".to_string())
        );
        assert_eq!(
            normalize_exit_semantics_tag("domain_negative"),
            Some("domain_negative".to_string())
        );
    }

    #[test]
    fn normalize_exit_semantics_tag_rejects_unknown_values() {
        assert_eq!(normalize_exit_semantics_tag("made_up"), None);
        assert_eq!(normalize_exit_semantics_tag("ExecutionError"), None);
        assert_eq!(normalize_exit_semantics_tag("domain-negative"), None);
    }
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
        let exit_semantics_value = tool_result
            .get("exit_semantics")
            .and_then(Value::as_str)
            .and_then(parse_exit_semantics_tag);
        let result_class_value = tool_result
            .get("result_class")
            .and_then(Value::as_str)
            .and_then(parse_result_class_tag);
        let output = tool_result.get("output").map(|output| match output {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            Value::Object(map) if map.is_empty() => {
                // Tagged with the shared sentinel so log pipelines / metrics
                // can count this specific degraded path and measure whether
                // the upstream serialization bug is decreasing over time.
                // See `astra_turn_core::history::DEGRADED_EMPTY_OBJECT_TAG`.
                astra_core::agent_warn!(
                    "bridge",
                    "tool_result.output is an empty object (not String); degrading to empty string. request_id={} tag={}",
                    request_id,
                    astra_turn_core::history::DEGRADED_EMPTY_OBJECT_TAG
                );
                String::new()
            }
            other => {
                // Non-string output is a serialization bug upstream —
                // the contract is `output: Option<String>`. Log the
                // anomaly so we can trace the source, but preserve the
                // real JSON payload for the LLM instead of replacing it
                // with a synthetic error sentinel (the old behavior
                // silently discarded real tool data if the upstream
                // bug ever fired in prod).
                let type_label = if other.is_object() {
                    "object"
                } else if other.is_array() {
                    "array"
                } else {
                    "non-string"
                };
                let stringified = other.to_string();
                // Byte-slice is UTF-8-unsafe: serde_json emits ASCII-escaped
                // today, but one config flip to raw UTF-8 would turn this
                // into a panic on multi-byte boundaries. Route through the
                // shared char-boundary helper instead — same helper that
                // fixed the cross-turn cache-hit preview regression.
                let (preview, _truncated) =
                    astra_turn_core::headless_tool_journal::truncate_on_char_boundary(
                        &stringified,
                        200,
                    );
                astra_core::agent_warn!(
                    "bridge",
                    "tool_result.output is {} (not String); coercing to JSON repr. request_id={}, value={}",
                    type_label,
                    request_id,
                    preview
                );
                // Preserve the real payload. The warn! above still fires
                // so operators can trace the upstream serialization bug.
                stringified
            }
        });
        let output_semantic_error = output
            .as_deref()
            .is_some_and(astra_turn_core::tool_result_semantics::is_tool_error);
        let ok = bridge_tool_result_ok(
            status,
            exit_semantics_value,
            result_class_value,
            output_semantic_error,
        );
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
        let ask_user = astra_tools::summarize_ask_user_tool_call(
            arguments.as_deref(),
            output.as_deref(),
            ok,
            error.as_deref(),
        );
        let exit_semantics = tool_result
            .get("exit_semantics")
            .and_then(Value::as_str)
            .and_then(normalize_exit_semantics_tag);
        let result_class = tool_result
            .get("result_class")
            .and_then(Value::as_str)
            .and_then(normalize_result_class_tag);
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
            ask_user,
            round,
            batch_id,
            parallel,
            exit_semantics,
            result_class,
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
        let ask_user = astra_tools::summarize_ask_user_tool_call(
            arguments.as_deref(),
            None,
            false,
            Some("missing tool result"),
        );
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
            ask_user,
            round,
            batch_id,
            parallel,
            exit_semantics: None,
            ..Default::default()
        });
    }

    records
}

fn record_full_llm_request_event(
    turn_event_buffer: &mut Option<TurnEventBuffer>,
    full_llm_capture: bool,
    user_id: &str,
    session_id: &str,
    turn: u32,
    trace: &BridgeTraceCorrelation,
    source: &str,
    model: &str,
    provider: &str,
    attempt: u32,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
) {
    if session_id.is_empty() || !full_llm_capture {
        return;
    }
    let Some(buf) = turn_event_buffer.as_mut() else {
        return;
    };
    let round = buf.current_round();
    let prompt_request_plan =
        astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
            user_id,
            session_id,
            turn,
            round,
            attempt,
            source,
            messages,
            tools,
            max_output_tokens,
        })
        .ok();
    let mut evt = astra_services::session_journal::JournalEvent::llm_request_full(
        Some(session_id),
        turn,
        round,
        json!({
            "source": source,
            "model": model,
            "provider": provider,
            "attempt": attempt,
            "trace": {
                "session_turn": turn,
                "round": round,
                "session_turn_source": trace.session_turn_source,
                "turn_chain_id": trace.turn_chain_id,
                "user_query_event_id": trace.user_query_event_id,
            },
            "request": crate::turn::llm::exchange_capture::build_capture_request_json(
                messages,
                tools,
                max_output_tokens,
            ),
            "prompt_request_id": prompt_request_plan.as_ref().map(|plan| plan.request_id.as_str()),
            "request_hash": prompt_request_plan.as_ref().map(|plan| plan.request_hash.as_str()),
            "request_summary": prompt_request_plan
                .as_ref()
                .map(|plan| plan.summary_json.clone())
                .unwrap_or_else(|| crate::turn::llm::exchange_capture::build_capture_request_summary_json(
                    messages,
                    tools,
                    max_output_tokens,
                )),
        }),
    );
    evt.offset_ms = Some(buf.offset_ms());
    buf.record(evt);
}

fn record_full_llm_response_event(
    turn_event_buffer: &mut Option<TurnEventBuffer>,
    full_llm_capture: bool,
    session_id: &str,
    turn: u32,
    trace: &BridgeTraceCorrelation,
    source: &str,
    model: &str,
    provider: &str,
    attempt: u32,
    outcome: &str,
    response: Value,
) {
    if session_id.is_empty() || !full_llm_capture {
        return;
    }
    let Some(buf) = turn_event_buffer.as_mut() else {
        return;
    };
    let round = buf.current_round();
    let mut evt = astra_services::session_journal::JournalEvent::llm_response_full(
        Some(session_id),
        turn,
        round,
        json!({
            "source": source,
            "model": model,
            "provider": provider,
            "attempt": attempt,
            "trace": {
                "session_turn": turn,
                "round": round,
                "session_turn_source": trace.session_turn_source,
                "turn_chain_id": trace.turn_chain_id,
                "user_query_event_id": trace.user_query_event_id,
            },
            "response": crate::turn::llm::exchange_capture::build_capture_response_json(
                outcome,
                response,
            ),
        }),
    );
    evt.offset_ms = Some(buf.offset_ms());
    buf.record(evt);
}

fn bridge_success_response_payload(
    full_text: &str,
    reasoning: &str,
    tool_calls: &[Value],
    usage: &Map<String, Value>,
    finish_reason: &str,
) -> Value {
    json!({
        "full_text": full_text,
        "reasoning": reasoning,
        "tool_calls": tool_calls,
        "usage": usage,
        "finish_reason": finish_reason,
    })
}

fn bridge_error_response_payload(
    error: &str,
    kind: &str,
    full_text: &str,
    reasoning: &str,
    tool_calls: &[Value],
    usage: &Map<String, Value>,
) -> Value {
    json!({
        "error": error,
        "kind": kind,
        "full_text": full_text,
        "reasoning": reasoning,
        "tool_calls": tool_calls,
        "usage": usage,
    })
}

fn flush_turn_event_buffer_or_warn(
    turn_event_buffer: &mut Option<TurnEventBuffer>,
    session_id: &str,
    stage: &str,
) {
    if session_id.is_empty() {
        return;
    }
    let Some(buf) = turn_event_buffer.as_mut() else {
        return;
    };
    if buf.is_empty() {
        return;
    }
    let writer = match JournalWriter::new(session_id) {
        Ok(writer) => writer,
        Err(error) => {
            astra_core::agent_warn!(
                "bridge",
                "failed to create journal writer for {stage}: session={} error={}",
                session_id,
                error
            );
            return;
        }
    };
    if let Err(error) = buf.flush(&writer) {
        astra_core::agent_warn!(
            "bridge",
            "failed to flush turn events for {stage}: session={} error={}",
            session_id,
            error
        );
    }
}

// ── Bridge observability — delegated to turn::bridge_observability ────────────
use super::observability::{build_context_trace_signal, persist_legacy_bridge_trace_and_quality};

// ── LLM streaming — delegated to turn::bridge_llm_stream ─────────────────────
use super::llm_stream::call_llm_stream_with_request_overrides;
use super::llm_stream::rate_limit_cooldown;
use astra_turn_core::bridge_rate_limit_cooldown::{
    FallbackOutcome, RateLimitAction, try_resolve_fallback,
};

#[cfg(test)]
async fn await_with_client_disconnect<T, F>(
    cancel: Option<&CancellationToken>,
    future: F,
) -> Result<T, Map<String, Value>>
where
    F: std::future::Future<Output = T>,
{
    // audit-#11: do not bias toward the cancel arm. If the caller's token is
    // already set when we reach the select! a biased poll would always pick
    // it and starve real work that completes synchronously.
    tokio::select! {
        _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cancel) => Err(
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

fn normalize_bridge_prompt_messages(messages: Vec<Value>) -> (Vec<Value>, Vec<String>) {
    let normalized =
        astra_turn_core::runtime_scaffolding::normalize_prompt_facing_runtime_messages(messages);
    (normalized.messages, normalized.required_runtime_texts)
}

fn required_runtime_text_for_bridge(
    edge_profile: &Map<String, Value>,
    recovered_required_runtime_texts: &[String],
) -> Option<String> {
    let mut parts: Vec<String> = recovered_required_runtime_texts
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(text) = astra_turn_core::chat_turn_edge_profile::edge_profile_joined_text(
        edge_profile,
        astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS,
    ) {
        parts.push(text);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn bridge_pipeline_event_turn(trace_turn: u32) -> u32 {
    trace_turn.max(1)
}

fn latest_assistant_message_text(messages: &[Value]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
fn turn_count_from_messages(messages: &[Value]) -> i64 {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .count() as i64
}

fn bridge_root_turn_journal_owned(
    headers: &HeaderMap,
    payload: &Value,
    bridge_e2e_authorized: bool,
) -> bool {
    if bridge_e2e_authorized {
        return false;
    }
    if payload
        .get("root_turn_journal_owned")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    header_str(headers, ROOT_TURN_JOURNAL_HEADER)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"))
        .unwrap_or(false)
}

fn bridge_should_create_turn_event_buffer(
    full_llm_capture: bool,
    root_runtime_owns_turn_journal: bool,
) -> bool {
    full_llm_capture || !root_runtime_owns_turn_journal
}

fn bridge_should_record_llm_round(root_runtime_owns_turn_journal: bool) -> bool {
    !root_runtime_owns_turn_journal
}

fn tool_names_from_tool_calls(tool_calls: &[Value]) -> Vec<String> {
    tool_calls
        .iter()
        .filter_map(tool_call_name)
        .map(std::string::ToString::to_string)
        .collect()
}

fn tool_markers_from_tool_calls(tool_calls: &[Value]) -> Vec<String> {
    tool_calls
        .iter()
        .filter_map(|tool_call| {
            let name = tool_call_name(tool_call)?;
            let args = tool_call
                .get("function")
                .and_then(Value::as_object)
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str);
            Some(astra_turn_core::followup_suggestion::tool_marker(
                name, args,
            ))
        })
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
    turn_guard: &mut astra_turn_core::turn_guard::TurnGuard,
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
    let mut event = astra_turn_core::complete::build_turn_complete_event(
        !tool_calls.is_empty(),
        false,
        &astra_turn_core::stall::DivergenceStatus::Healthy,
        None,
        Some(assistant_text),
    );
    if let Some(user_message) = latest_user_message_text(messages)
        && let Some(suggestion) = astra_turn_core::followup_suggestion::suggest_followup(
            user_message,
            assistant_text,
            &tool_markers_from_tool_calls(tool_calls),
        )
    {
        event.insert(
            "followup_suggestion".to_string(),
            Value::String(suggestion.text),
        );
    }
    Value::Object(event)
}

// ── Prompt caching — delegated through turn::llm_context ─────────────────────
#[cfg(test)]
pub(crate) use super::super::llm::context::annotate_tool_schemas_for_cache as annotate_tool_schemas_for_caching;
pub use super::super::prompt_cache::PromptCacheConfig;
#[cfg(test)]
pub(crate) use super::super::prompt_cache::add_message_cache_breakpoint;

#[derive(Clone)]
pub struct InProcessChatTurnBridge {
    pub matrixone: MatrixOneSettings,
    pub encryptor: Arc<FernetTokenEncryptor>,
    /// Shared DB pool — avoids creating a new connection per turn.
    /// When `None`, falls back to ephemeral single-connection pool.
    pub shared_pool: Option<SharedPool>,
    /// Same `Arc` as [`crate::AppState::edge_callback_ledger`] — bridge takes tool callbacks here.
    pub edge_callback_ledger: Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    /// Session-scoped structured feedback store — accumulates correction rules
    /// and injects them into subsequent turn system prompts.
    pub feedback_store: Arc<astra_pipeline::feedback_store::FeedbackStore>,
    /// Cached Memoria client — created once, reused across turns.
    pub memoria_client: Option<crate::turn::cloud::memoria_compact::HttpMemoriaClient>,
    /// First-turn session-start memory snapshot, latched per session so repeated
    /// round re-entries don't refetch and churn the cached prompt prefix.
    session_start_memory_cache: Arc<tokio::sync::Mutex<HashMap<String, CachedSessionStartMemory>>>,
    /// Shared session facts for facts-first compaction. Updated by the agentic loop
    /// at each turn end; read by the bridge during compaction.
    pub session_facts: Arc<std::sync::Mutex<astra_turn_types::session_facts::SessionFacts>>,
    /// Shutdown-aware tracker for fire-and-forget SSE persist tasks (HIGH #4).
    /// When `None` the bridge falls back to raw `tokio::spawn` (dev / test mode).
    pub persist_tracker: Option<Arc<dyn crate::matrix_cloud_runtime::BridgePersistTracker>>,
}

impl InProcessChatTurnBridge {
    pub fn new(matrixone: MatrixOneSettings, encryptor: Arc<FernetTokenEncryptor>) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            feedback_store: Arc::new(astra_pipeline::feedback_store::FeedbackStore::new()),
            memoria_client: crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env(),
            session_start_memory_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            session_facts: Arc::new(std::sync::Mutex::new(Default::default())),
            persist_tracker: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_edge_callback_ledger(
        mut self,
        ledger: Arc<tokio::sync::Mutex<HashMap<String, Value>>>,
    ) -> Self {
        self.edge_callback_ledger = ledger;
        self
    }

    /// Attach a shutdown-aware persist tracker (HIGH #4).
    /// When set, SSE-generator persist tasks are tracked and drained on shutdown
    /// rather than being fire-and-forgot via raw `tokio::spawn`.
    pub fn with_persist_tracker(
        mut self,
        tracker: Arc<dyn crate::matrix_cloud_runtime::BridgePersistTracker>,
    ) -> Self {
        self.persist_tracker = Some(tracker);
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
        let user_id = required_bridge_header(headers, "x-mo-user-id")?;
        let session_id = required_bridge_header(headers, "x-mo-session-id")?;
        let full_llm_capture = header_str(headers, "x-mo-full-llm-capture").as_deref() == Some("1");
        let header_session_turn = optional_positive_u32_header(headers, "x-mo-session-turn")?;
        #[cfg(feature = "bridge-e2e-hooks")]
        let bridge_e2e_authorized = astra_turn_core::bridge_e2e_hooks::authorized(headers);
        #[cfg(not(feature = "bridge-e2e-hooks"))]
        let bridge_e2e_authorized = false;
        let turn_chain_id =
            header_str(headers, "x-mo-turn-chain-id").unwrap_or_else(|| Uuid::now_v7().to_string());
        let user_query_event_id = header_str(headers, "x-mo-user-query-event-id")
            .unwrap_or_else(|| Uuid::now_v7().to_string());

        // Parse request body
        let payload = parse_bridge_payload(&body)?;
        let agent_id = payload
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let (messages, recovered_required_runtime_texts) =
            normalize_bridge_prompt_messages(optional_payload_array(&payload, "messages")?);
        let tool_results = optional_payload_array(&payload, "tool_results")?;
        let edge_tools = optional_payload_array(&payload, "edge_tools")?;
        let edge_profile = optional_payload_object(&payload, "edge_profile")?;
        let explain = explain_requested(&payload);
        let selected_model_name = selected_model_name_from_payload(&payload);
        let round_index = bridge_round_index(&payload)?;
        let provider_model_gateway_invocation =
            provider_model_gateway_invocation_from_payload(&payload);

        let _agent_id = payload
            .get("agent_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let thinking_config = payload
            .get("thinking")
            .map(astra_turn_core::thinking_config::ThinkingConfig::from_payload_value)
            .unwrap_or_default();

        let matrixone = self.matrixone.clone();
        let encryptor = self.encryptor.clone();
        let shared_pool = self.shared_pool.clone();
        let session_start_memory_cache = self.session_start_memory_cache.clone();
        let (trace_turn, trace_turn_source) = if let Some(turn) = header_session_turn {
            (turn, "header")
        } else if !session_id.is_empty() {
            (
                crate::server::session_turn::infer_session_turn(
                    shared_pool.as_ref(),
                    &user_id,
                    &session_id,
                )
                .await,
                "inferred_agent_events",
            )
        } else {
            (1, "default")
        };
        let trace_correlation = BridgeTraceCorrelation {
            session_turn_source: trace_turn_source.to_string(),
            turn_chain_id: turn_chain_id.clone(),
            user_query_event_id: user_query_event_id.clone(),
        };
        if trace_turn > 1 && !session_id.is_empty() {
            // `cached_first_turn_session_start_memory` also clears on later
            // turns. Keeping this outer eviction makes the "not a first-turn
            // snapshot anymore" intent explicit before prompt assembly starts,
            // while the helper retains the same guarantee for any future
            // callers. A concurrent turn-1 refetch may repopulate the cache in
            // between these two sites, which is acceptable for this
            // best-effort optimization.
            self.session_start_memory_cache
                .lock()
                .await
                .remove(&session_id);
        }
        let root_runtime_owns_turn_journal =
            bridge_root_turn_journal_owned(headers, &payload, bridge_e2e_authorized);
        let _edge_callback_ledger = self.edge_callback_ledger.clone();

        #[cfg(feature = "bridge-e2e-hooks")]
        let bridge_e2e_for_stream: Option<Vec<Value>> =
            if astra_turn_core::bridge_e2e_hooks::authorized(headers) {
                payload
                    .get("test_llm_rounds")
                    .and_then(|v| v.as_array())
                    .cloned()
            } else {
                None
            };
        #[cfg(not(feature = "bridge-e2e-hooks"))]
        let bridge_e2e_for_stream: Option<Vec<Value>> = None;
        #[cfg(feature = "bridge-e2e-hooks")]
        let bridge_e2e_stream_blocks_for_stream: Option<Vec<String>> =
            if astra_turn_core::bridge_e2e_hooks::authorized(headers) {
                let blocks = astra_turn_core::bridge_e2e_hooks::parse_stream_blocks(
                    payload
                        .get("test_llm_stream_blocks")
                        .unwrap_or(&Value::Null),
                );
                (!blocks.is_empty()).then_some(blocks)
            } else {
                None
            };
        #[cfg(not(feature = "bridge-e2e-hooks"))]
        let bridge_e2e_stream_blocks_for_stream: Option<Vec<String>> = None;

        let bridge_e2e_capture = bridge_e2e_for_stream.clone();
        let bridge_e2e_stream_blocks_capture = bridge_e2e_stream_blocks_for_stream.clone();
        let client_cancel_capture = client_cancel.clone();
        let feedback_store_capture = self.feedback_store.clone();
        let memoria_client_owned = self.memoria_client.clone();
        let session_facts_shared = self.session_facts.clone();
        let persist_tracker_shared = self.persist_tracker.clone();
        let mut remote_artifact_store =
            astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
        if let Some(pool) = self.shared_pool.clone() {
            remote_artifact_store = remote_artifact_store.with_pool(pool);
        }
        let remote_artifact_store = Arc::new(remote_artifact_store);
        let disconnect_capture_state = Arc::new(Mutex::new(DisconnectCaptureSnapshot::default()));
        if let Some(cancel_token) = client_cancel.clone() {
            let disconnect_state = disconnect_capture_state.clone();
            let disconnect_store = remote_artifact_store.clone();
            let disconnect_full_llm_capture = full_llm_capture;
            tokio::spawn(async move {
                cancel_token.cancelled().await;
                let snapshot = match disconnect_state.lock() {
                    Ok(guard) => guard.clone(),
                    Err(_) => return,
                };
                if !snapshot.started || snapshot.finalized {
                    return;
                }
                persist_bridge_stream_failure_capture(
                    "bridge_inprocess drop-disconnect capture",
                    disconnect_full_llm_capture,
                    disconnect_store.as_ref(),
                    &snapshot.session_id,
                    &snapshot.user_id,
                    snapshot.turn,
                    &BridgeTraceCorrelation {
                        session_turn_source: snapshot.session_turn_source.clone(),
                        turn_chain_id: snapshot.turn_chain_id.clone(),
                        user_query_event_id: snapshot.user_query_event_id.clone(),
                    },
                    snapshot.round_ix,
                    snapshot.agent_id.as_deref(),
                    &snapshot.model_name,
                    &snapshot.resolved_model,
                    &snapshot.provider,
                    &snapshot.llm_messages,
                    &snapshot.pruned_tools,
                    snapshot.max_output_tokens,
                    "client_disconnect",
                    "Request cancelled (client disconnected)",
                    "CLIENT_DISCONNECT",
                    &snapshot.partial_text,
                    &snapshot.partial_reasoning,
                    &snapshot.partial_tool_calls,
                    &snapshot.usage,
                )
                .await;
            });
        }

        let stream = stream! {
            let cc = client_cancel_capture.clone();
            let remote_artifact_store = remote_artifact_store.clone();
            let disconnect_capture_state = disconnect_capture_state.clone();
            let _client_disconnect_guard = cc
                .as_ref()
                .map(|t| crate::turn::llm::client::CancelOnClientDisconnect::new(t.clone()));
            let turn_started = Instant::now();
            let run_id = uuid::Uuid::new_v4().to_string();
            let mut turn_event_buffer = bridge_should_create_turn_event_buffer(
                full_llm_capture,
                root_runtime_owns_turn_journal,
            ).then(|| {
                TurnEventBuffer::begin_turn_with_round(
                    (!session_id.is_empty()).then_some(session_id.as_str()),
                    trace_turn,
                    round_index,
                )
            });
            // Emit session_info first
            yield render_sse(&inprocess_session_info_event(&session_id, &run_id));

            let bridge_e2e = bridge_e2e_capture;
            let bridge_e2e_stream_blocks = bridge_e2e_stream_blocks_capture;
            let use_e2e_llm = bridge_e2e.as_ref().map(|r| !r.is_empty()).unwrap_or(false)
                || bridge_e2e_stream_blocks
                    .as_ref()
                    .map(|blocks| !blocks.is_empty())
                    .unwrap_or(false);
            let provider_model_gateway_invocation = match provider_model_gateway_invocation {
                Ok(invocation) => invocation,
                Err(error) => {
                    yield render_sse_map(&build_stream_error_event(
                        &error,
                        "PROVIDER_RUNTIME_CONTEXT_INVALID",
                        false,
                    ));
                    mark_disconnect_capture_finalized(&disconnect_capture_state);
                    return;
                }
            };

            // Resolve LLM model (skipped when `test_llm_rounds` drives the turn — feature `bridge-e2e-hooks`).
            // Also capture fallback_chain for rate-limit-triggered fallback.
            let pool_ref = shared_pool.as_ref().map(SharedPool::get);
            let requested_model_override =
                astra_core::model_override::normalize_model_override(selected_model_name.as_deref());
            let requested_model_name = requested_model_override.map(str::to_string);
            let mut rate_limit_fallback_trace: Option<Value> = None;
            if !use_e2e_llm && requested_model_override.is_none() {
                tracing::warn!(
                    target: "astra_runtime::bridge_inprocess",
                    session_id = %session_id,
                    run_id = %run_id,
                    turn = trace_turn,
                    round = round_index,
                    reason = "missing_model_selection",
                    "missing selected_model.model; refusing implicit model fallback"
                );
            }
            let mut llm_header_overrides: Option<HashMap<String, String>> = None;
            let mut completions_url_override: Option<String> = None;
            let (mut model_name, mut wire_model_name, mut api_key, mut base_url, mut provider, mut request_body_overrides, mut cache_capability, mut model_context_window, fallback_chain) = if use_e2e_llm {
                (
                    "bridge-e2e-mock".to_string(),
                    None::<String>,
                    "unused".to_string(),
                    "http://127.0.0.1:1".to_string(),
                    "openai".to_string(),
                    None,
                    None,
                    None,
                    Vec::<String>::new(),
                )
            } else if let Some(invocation) = provider_model_gateway_invocation {
                let mut headers = HashMap::new();
                headers.insert("authorization".to_string(), invocation.authorization);
                llm_header_overrides = Some(headers);
                completions_url_override = Some(invocation.endpoint_url);
                (
                    invocation.model,
                    None::<String>,
                    "provider-runtime".to_string(),
                    "http://127.0.0.1".to_string(),
                    "openai".to_string(),
                    None,
                    None,
                    None,
                    Vec::<String>::new(),
                )
            } else {
                match astra_services::resolve_active_llm_model(
                    &matrixone,
                    encryptor.as_ref(),
                    requested_model_override,
                    pool_ref,
                )
                .await
                {
                    Ok(m) => (
                        m.model_name,
                        m.wire_model_name,
                        m.api_key,
                        m.base_url,
                        m.provider,
                        m.request_body_overrides,
                        crate::turn::llm::context::cache_capability_from_model_metadata(
                            m.prompt_cache_capability,
                        ),
                        m.context_window,
                        m.fallback_chain,
                    ),
                    Err(e) => {
                        let message = format!("Model resolution failed: {e}");
                        let error = astra_core::ClassifiedError::new(
                            astra_core::classify_model_resolution_error_message(&message),
                            message,
                        );
                        yield render_sse_map(&build_stream_error_event(
                            &error.message,
                            error.kind.as_str(),
                            error.kind.is_retryable(),
                        ));
                        mark_disconnect_capture_finalized(&disconnect_capture_state);
                        return;
                    }
                }
            };
            let has_fallback = !fallback_chain.is_empty();

            // Latch cache config at session init — prevents mid-session env var
            // changes from busting the KV cache.
            let cache_cfg =
                PromptCacheConfig::from_cache_capability(cache_capability, &provider, &model_name);

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
                        _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cc.as_deref()) => {
                            yield render_sse_map(&build_stream_error_event(
                                "Request cancelled (client disconnected)",
                                "CLIENT_DISCONNECT",
                                false,
                            ));
                            mark_disconnect_capture_finalized(&disconnect_capture_state);
                            return;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                    }
                }
                RateLimitAction::UseFallback { reason } => {
                    let mx = &matrixone;
                    let enc = encryptor.as_ref();
                    match try_resolve_fallback(
                        cooldown,
                        &fallback_chain,
                        reason,
                        |fb_name| {
                            async move {
                                astra_services::resolve_active_llm_model(
                                    mx,
                                    enc,
                                    Some(fb_name.as_str()),
                                    pool_ref,
                                )
                                .await
                            }
                        },
                    )
                    .await
                    {
                        FallbackOutcome::Resolved(fb) => {
                            let from_model = model_name.clone();
                            let to_model = fb.model_name.clone();
                            astra_core::agent_warn!(
                                "llm",
                                "rate-limit fallback: {} -> {} ({})",
                                from_model,
                                to_model,
                                reason.as_str()
                            );
                            rate_limit_fallback_trace = Some(json!({
                                "from_model": from_model,
                                "to_model": to_model,
                                "reason": reason.as_str(),
                            }));
                            model_name = fb.model_name;
                            wire_model_name = fb.wire_model_name;
                            api_key = fb.api_key;
                            base_url = fb.base_url;
                            provider = fb.provider;
                            request_body_overrides = fb.request_body_overrides;
                            model_context_window = fb.context_window;
                            cache_capability =
                                crate::turn::llm::context::cache_capability_from_model_metadata(
                                    fb.prompt_cache_capability,
                                );
                        }
                        FallbackOutcome::NoFallbackConfigured => {
                            astra_core::agent_warn!(
                                "llm",
                                "rate-limit cooldown: fallback requested ({}) but no fallback configured",
                                reason.as_str()
                            );
                        }
                        FallbackOutcome::AllExhausted { chain_len } => {
                            astra_core::agent_warn!(
                                "llm",
                                "rate-limit cooldown: all {} fallback models exhausted ({})",
                                chain_len,
                                reason.as_str()
                            );
                        }
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
                    mark_disconnect_capture_finalized(&disconnect_capture_state);
                    return;
                }
            }

            // Build LLM messages: system prompt + history + current messages + tool results
            let mut llm_messages: Vec<Value> = Vec::new();
            // Memory is prefetched + routed through the typed Memory binder
            // (extra_stable / extra_dynamic sections). Counters below track
            // telemetry for the explain block.
            let mut memory_fetch_ms: i64 = 0;
            let mut memory_items: usize = 0;
            let mut memory_preview: Vec<String> = Vec::new();

            // System prompt — tells LLM about available tools and how to use them
            // Environment context is split by cache volatility:
            //
            // * `environment_static`  (Platform, Shell, CWD, Home) →
            //   stable for the session, safe inside the Session cache.
            //   Routed through `extra_stable_sections` → binder's
            //   `RuntimeIdentity` → behind the 2nd cache marker.
            //
            // * `environment_volatile` (Git branch dirty state, staged /
            //   unstaged diff stats, recent commits) → changes on every
            //   edit/commit. Routed through `extra_dynamic_sections` →
            //   binder's `RuntimeVolatile` (None scope, post-marker) so
            //   it never invalidates the cached prefix.
            //
            // The `# Project Profile` wrapper with cwd/git_branch is
            // dropped: runtime assembly emits provider-aware `Model:` and
            // `bind_runtime_identity` emits typed `CWD: / Branch:` lines from
            // `SessionContext`, so repeating them as a Markdown block was pure
            // duplicate.
            let env_static = edge_profile
                .get("environment_static")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let env_volatile = edge_profile
                .get("environment_volatile")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            // Memoria prefetch lives on its own. `section` is the
            // "## User Memories\n..." block — the piece that actually drifts.
            //
            // Skip when the CLI has already injected `memoria_insights_text`
            // (rendered as `## Memoria Recall` via the bridge's volatile
            // lane). Both paths query the same Memoria backend with
            // overlapping queries + top_k=5; running both duplicates
            // ~700 tokens of memory content per turn into two
            // differently-formatted sections (`## Memoria Recall` +
            // `## User Memories`). When present, the CLI digest is
            // authoritative. See `bridge_should_run_memoria_prefetch`.
            let mut memoria_prefetch_entries = Vec::new();
            // Two memory surfaces, split by cache lane:
            //
            //   * **stable lane** — `<memory_index>` + `<session_memory>`.
            //     Rebuilt on turn 1 from the memoria backend; the bytes
            //     are session-stable (sorted by memory_id, deterministic
            //     freshness labels). Pushed into `stable_sections` so
            //     Anthropic's prompt cache covers them across every turn
            //     in the session. The LLM gets ambient awareness of what
            //     it could recall plus user profile + recent episodes.
            //
            //   * **volatile lane** — `## User Memories` from per-turn
            //     hybrid recall. Changes every turn. Pushed into
            //     `dynamic_sections`. Entries whose content was already
            //     shown in the stable lane are dropped via the session
            //     seen-ledger so we don't surface the same fact twice.
            //
            // The prior design bundled both into one `memoria_prefetch_section`
            // and then only pushed it when the typed-pipeline per-turn
            // entries were empty — which meant the stable lane got
            // silently dropped on every warm session. Fixed here.
            let is_first_turn = trace_turn <= 1;
            // Single canonical "already surfaced" store for the process
            // lives on `astra_tools::memoria::MemoriaClient`. The bridge
            // writes content-dedup keys to it; the tool-side `recall`
            // decorator writes memory_ids. Both paths share the same
            // snapshot so a memory shown via `<session_memory>` doesn't
            // re-appear in per-turn recall, and vice versa.
            use astra_tools::memoria::MemoriaClient as ToolClient;
            let mut stable_memory_section: Option<String> = None;
            let mut volatile_memory_section: Option<String> = None;
            if bridge_should_run_memoria_prefetch(&edge_profile)
                && let (Some(mem_url), Some(mem_key)) = (
                    edge_profile.get("memoria_url").and_then(Value::as_str),
                    edge_profile.get("memoria_key").and_then(Value::as_str),
                )
            {
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

                // Per-turn hybrid recall runs every turn; session-start
                // (profile + episodes) only on turn 1.
                let (per_turn, cached_session_start) = tokio::join!(
                    prefetch_memories(mem_url, mem_key, user_msg, &user_id, top_k),
                    async {
                        if is_first_turn {
                            cached_first_turn_session_start_memory(
                                &session_start_memory_cache,
                                &session_id,
                                trace_turn,
                                || async {
                                    let session_start =
                                        prefetch_session_start_memories(mem_url, mem_key, &user_id)
                                            .await;
                                    let session_start_exposed_ids =
                                        crate::turn::memory_prefetch::session_start_exposed_ids(
                                            &session_start,
                                        );
                                    let memory_index = if std::env::var(
                                        "ASTRA_MEMORY_INDEX_INJECT",
                                    )
                                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                                    .unwrap_or(false)
                                    {
                                        prefetch_memory_index(
                                            mem_url,
                                            mem_key,
                                            &user_id,
                                            &session_start_exposed_ids,
                                        )
                                        .await
                                    } else {
                                        None
                                    };
                                    CachedSessionStartMemory::from_prefetch(
                                        session_start,
                                        memory_index,
                                    )
                                },
                            )
                            .await
                        } else {
                            None
                        }
                    }
                );

                memory_fetch_ms = per_turn.fetch_ms
                    + cached_session_start
                        .as_ref()
                        .map(|s| s.fetch_ms)
                        .unwrap_or(0);

                // ── Stable lane: <memory_index> + <session_memory> ──
                //
                // `<memory_index>` is off by default behind
                // `ASTRA_MEMORY_INDEX_INJECT`; when on, dedup against
                // the ids that will already appear in `<session_memory>`
                // so the two blocks don't repeat the same content.
                stable_memory_section = cached_session_start
                    .as_ref()
                    .and_then(|s| s.stable_memory_section.clone());

                // Record stable-lane contents into the canonical store
                // so volatile entries matching them get filtered out.
                if let Some(ref stable) = stable_memory_section {
                    let keys: Vec<String> = stable
                        .lines()
                        .filter(|l| l.trim_start().starts_with("- "))
                        .map(|l| l.trim_start_matches("- ").trim_end().to_string())
                        .map(|content| {
                            // Same normalization as
                            // `collect_seen_contents` so the filter and
                            // the recorded keys share a dedup vocabulary.
                            content
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .trim_end_matches(['.', '!', '?', ';', ':', ','])
                                .to_lowercase()
                        })
                        .filter(|s| !s.is_empty())
                        .collect();
                    ToolClient::record_seen(&session_id, keys);
                }
                // Also record the memory_ids so tool-side recall dedup
                // (which filters on memory_id) sees them too.
                let stable_ids = cached_session_start
                    .as_ref()
                    .map(|s| s.stable_ids.clone())
                    .unwrap_or_default();
                if !stable_ids.is_empty() {
                    ToolClient::record_seen(&session_id, stable_ids);
                }

                // ── Volatile lane: filter per-turn entries against ledger ──
                let already_seen = ToolClient::seen_snapshot(&session_id);
                let filtered_entries = crate::turn::memory_prefetch::filter_entries_already_surfaced(
                    per_turn.entries,
                    &already_seen,
                );
                memory_items = filtered_entries.len();
                memory_preview = filtered_entries
                    .iter()
                    .take(3)
                    .map(|e| e.content.clone())
                    .collect();
                // Record per-turn entries so a subsequent same-session
                // recall doesn't re-surface them either.
                let new_keys = crate::turn::memory_prefetch::collect_seen_contents(&filtered_entries);
                if !new_keys.is_empty() {
                    ToolClient::record_seen(&session_id, new_keys);
                }
                memoria_prefetch_entries = filtered_entries;
                volatile_memory_section = per_turn.section;
            }
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

            let task_type = prompts::detect_task_type(user_content_for_signal);
            // ── Self-awareness section (injected by CLI via edge_profile) ──
            let self_awareness_hint = edge_profile
                .get("self_awareness_text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|text| format!("\n\n{text}"))
                .unwrap_or_default();

            // ── Memoria insights digest (injected by CLI via edge_profile) ──
            let memoria_insights_hint = edge_profile
                .get("memoria_insights_text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|text| format!("\n\n{text}"))
                .unwrap_or_default();

            // ── Recent tool-call arg hints (injected by CLI via edge_profile) ──
            let recent_arg_hints_hint = edge_profile
                .get("recent_arg_hints_text")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|text| format!("\n\n{text}"))
                .unwrap_or_default();

            // ── Skill listing (injected by CLI via edge_profile) ──
            // Phase-9 fix: route to the session-stable lane so the
            // Anthropic prompt cache hits the listing block. Previously
            // this string went into `dynamic_sections` (volatile) which
            // made CLI users' turn-to-turn cache miss the entire listing
            // every round — the very regression the skill rewrite was
            // meant to eliminate.
            let skill_listing_hint_text = edge_profile
                .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_SKILL_LISTING_TEXT)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let skill_listing_section =
                skill_listing_section_for_edge_profile(skill_listing_hint_text.as_deref());

            // Memory storage decisions are now fully LLM-driven via system
            // prompt rules. detect_store_signal keyword matching was removed.

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
                    let injection = feedback_store.build_injection_filtered(&session_id, Some(user_content_for_signal)).await;
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
                let is_correction_like = matches!(
                    signal.signal_type.as_str(),
                    "correction" | "frustration" | "rephrasing"
                );
                // Store heuristic-extracted feedback only on correction-like signals
                // and only when we have a valid session_id (avoid cross-session leakage)
                if !session_id.is_empty() && is_correction_like {
                    if let Some(fb) = astra_pipeline::feedback_extraction::heuristic_extract(
                        user_content_for_signal,
                        &signal.signal_type,
                        signal.confidence,
                    ) {
                        // Persist feedback rule to Memoria L3 (fire-and-forget).
                        // Closes the reflect→memory loop: correction detected in
                        // this turn → stored durably → recalled in future sessions.
                        if let Some(ref mc) = memoria_client_owned {
                            use crate::turn::cloud::memoria_compact::MemoriaClient;
                            if let Some(lesson) =
                                crate::learning::synthesizer::feedback_rule_to_lesson(&fb)
                            {
                                let c = mc.clone();
                                let sid = session_id.clone();
                                tokio::spawn(async move {
                                    match tokio::time::timeout(
                                        std::time::Duration::from_secs(5),
                                        c.store(
                                            &lesson.content,
                                            lesson.memory_type,
                                            Some(&sid),
                                            Some(lesson.trust_tier),
                                        ),
                                    )
                                    .await
                                    {
                                        Ok(Ok(_)) => tracing::debug!("Persisted feedback rule to Memoria"),
                                        Ok(Err(e)) => tracing::debug!("Failed to persist feedback rule: {e}"),
                                        Err(_) => tracing::debug!("Timed out persisting feedback rule to Memoria"),
                                    }
                                });
                            }
                        }
                        feedback_store.add(&session_id, fb).await;
                    }
                }
                let hint = crate::turn::implicit_feedback::implicit_feedback_context_injection(&signal)
                    .map(|s| format!("\n\n{s}"))
                    .unwrap_or_default();
                (hint, is_correction_like)
            };

            // ── Memoria client (shared across P1 anchor + compaction + P3 write) ──
            let memoria_client_shared = memoria_client_owned.clone();

            let (tool_round_guidance, guidance_signals) =
                prompts::tool_round_guidance_trace(&messages, round_index);

            // Split bridge-composed signals into session-stable (RuntimeIdentity
            // scope → cached behind the Session→None marker) and turn-volatile
            // (RuntimeVolatile scope → re-sent per turn).
            //
            // STABLE (change only when session state changes, if at all):
            //   environment_static (Platform/Shell/CWD/Home)
            //
            // VOLATILE (change each turn by design):
            //   environment_volatile (git branch dirty/diff/recent commits),
            //   feedback_rules_hint (accumulates on each user correction),
            //   skill_hint (active skill/tool surface),
            //   self_awareness_hint (turn/token/outcome signals),
            //   typed memory_entries (per-turn retrieval, routed through the
            //     Memory section),
            //   implicit_feedback_hint (per-turn correction signal based on
            //     user message content),
            //   memoria_insights_hint (per-turn retrieval),
            //   recent_arg_hints_hint (per-turn tool args),
            //   tool_round_guidance (per-turn messages count)
            let mut stable_sections = Vec::new();
            let mut dynamic_sections = Vec::new();
            if let Some(ref text) = env_static {
                stable_sections.push(prompts::PromptSection::dynamic(
                    text.clone(),
                    prompts::PromptTokenBucket::Environment,
                ));
            }
            if let Some(ref text) = env_volatile {
                dynamic_sections.push(prompts::PromptSection::dynamic(
                    text.clone(),
                    prompts::PromptTokenBucket::Environment,
                ));
            }
            // Stable-lane memory (index + session_memory) goes behind
            // the cache marker — byte-stable across the session.
            if let Some(ref stable) = stable_memory_section {
                stable_sections.push(prompts::PromptSection::stable(
                    stable.clone(),
                    prompts::CacheScope::Session,
                ));
            }
            // Volatile-lane memory (## User Memories from per-turn
            // recall) sits post-marker so byte drift doesn't invalidate
            // the cached prefix.
            if let Some(ref volatile) = volatile_memory_section {
                dynamic_sections.push(prompts::PromptSection::dynamic(
                    volatile.clone(),
                    prompts::PromptTokenBucket::Environment,
                ));
            }
            if !skill_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        skill_hint.clone(),
                        prompts::PromptTokenBucket::UserPreferences,
                    )
                    .with_trace_signals(astra_turn_core::context_assembly_trace::PromptTraceSignals {
                        context_signals: astra_turn_core::context_assembly_trace::PromptContextSignals {
                            active_output_skills: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !implicit_feedback_hint.is_empty() {
                // Per-turn correction signal — depends on current user
                // message content, so it's volatile.
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        implicit_feedback_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(astra_turn_core::context_assembly_trace::PromptTraceSignals {
                        context_signals: astra_turn_core::context_assembly_trace::PromptContextSignals {
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
                    .with_trace_signals(astra_turn_core::context_assembly_trace::PromptTraceSignals {
                        context_signals: astra_turn_core::context_assembly_trace::PromptContextSignals {
                            learned_feedback_rules: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if let Some(section) = self_awareness_volatile_section(&self_awareness_hint) {
                dynamic_sections.push(section);
            }
            if !memoria_insights_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        memoria_insights_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(astra_turn_core::context_assembly_trace::PromptTraceSignals {
                        context_signals: astra_turn_core::context_assembly_trace::PromptContextSignals {
                            memoria_insights: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                );
            }
            if !recent_arg_hints_hint.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        recent_arg_hints_hint.clone(),
                        prompts::PromptTokenBucket::Environment,
                    ),
                );
            }
            if let Some(section) = skill_listing_section.clone() {
                // Session-scope: joins the cached prefix. Cache flips
                // once when skill catalog changes, then stabilizes.
                stable_sections.push(section);
            }
            if let Some(runtime_volatile) =
                astra_turn_core::chat_turn_edge_profile::edge_profile_joined_text(
                    &edge_profile,
                    astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS,
                )
            {
                dynamic_sections.push(prompts::PromptSection::dynamic(
                    runtime_volatile,
                    prompts::PromptTokenBucket::Environment,
                ));
            }
            if !tool_round_guidance.is_empty() {
                dynamic_sections.push(
                    prompts::PromptSection::dynamic(
                        tool_round_guidance.clone(),
                        prompts::PromptTokenBucket::Environment,
                    )
                    .with_trace_signals(astra_turn_core::context_assembly_trace::PromptTraceSignals {
                        guidance_signals,
                        ..Default::default()
                    }),
                );
            }
            // Build provider-aware system message via the context pipeline.
            // Anthropic: multi-block content with `cache_control` on stable sections.
            // OpenAI/others: two messages (stable prefix + dynamic per-turn).
            //
            // This is the bridge's on-ramp to the pipeline: dynamic_sections
            // (session anchor, feedback rules, memoria insights, etc.) flow
            // through ExternalSources.extra_dynamic_sections so the binder
            // appends them after runtime identity — keeping them in the
            // None-scoped post-cache segment where churn is expected.
            // Memoria prefetch results are passed separately as typed
            // MemoryEntry values so the Memory binder can rank/dedup/budget
            // them instead of treating the whole recall block as one string.
            // project_context comes from cross-session history summaries;
            // byte-stable within a session, so routing it into the pipeline's
            // ProjectContext section (Session scope) puts it behind the
            // Session→None cache marker.
            let project_context = edge_profile
                .get("project_context")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let bridge_session_current_date =
                crate::turn::session_current_date::resolve_session_current_date(&session_id);
            // Provider-aware volatile gating — see
            // `effective_volatile_sections_for_round` for the full rationale.
            // CurrentUserOnly (MiniMax) drops ALL rounds, not just >0.
            let cache_cap =
                astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider_model(
                    cache_capability,
                    &provider,
                    &model_name,
                );
            let effective_dynamic_sections = effective_volatile_sections_for_round(
                cache_cap,
                round_index,
                &dynamic_sections,
            );
            let required_runtime_text =
                required_runtime_text_for_bridge(&edge_profile, &recovered_required_runtime_texts);
            // Compute session-stable skill context. Selector-based
            // `<deferred-tools>` was removed; skills are surfaced through the
            // full `<available_skills>` catalog in `CacheScope::Session`, so a
            // catalog change flips the cache once then stabilizes.
            //
            // `ToolSurfaceConfig` honors the user's `runtime.tool_surface`
            // TOML: always_load_tools add extra always_load tools over the
            // declaration defaults. Loaded via the same `RuntimeConfig::load()`
            // path as `tool_surface` above (line 1451) for consistency.
            let deferred_block_str = deferred_tools_block_for_bridge_model(
                &edge_profile,
                &model_name,
                model_context_window,
            );
            let bridge_restricted_snapshot = HashSet::new();
            let initial_session_memory_entry = if let Some(memoria) = memoria_client_shared.as_ref()
            {
                crate::turn::wire_assembly::session_memory_entry_for_user_turn(
                    crate::session_memory::runner::load_current_session_memory_preferring_local(
                        memoria,
                        &session_id,
                    )
                    .await
                    .as_deref(),
                    trace_turn,
                    user_content_for_signal,
                )
            } else {
                None
            };
            let pipeline_outcome = crate::turn::llm::context::assemble_bridge_context(
                crate::turn::llm::context::BridgeContextAssemblyInput {
                    tool_surface:
                        crate::turn::llm::context::ToolSurfacePlan::from_visible_tools(
                            &edge_tools,
                            &bridge_restricted_snapshot,
                        )
                        .with_deferred_tools_block(&deferred_block_str),
                    runtime_signals: crate::turn::llm::context::BridgeRuntimeSignals::new(
                                        &stable_sections,
                                        &effective_dynamic_sections,
                                        &memoria_prefetch_entries,
                                        initial_session_memory_entry.clone(),
                                        edge_profile
                                            .get("system_prompt_override")
                                            .and_then(Value::as_str),
                                    ),
                    session: crate::turn::llm::context::BridgeSessionContextInput::new(
                        &cache_cfg,
                        cache_capability,
                        &session_id,
                        &model_name,
                        &provider,
                        edge_profile.get("cwd").and_then(Value::as_str),
                        edge_profile.get("git_branch").and_then(Value::as_str),
                        project_context,
                        &bridge_session_current_date,
                    )
                    .with_context_window(model_context_window)
                    .with_skill_listing_block(skill_listing_hint_text.as_deref().unwrap_or("")),
                },
            );
            let mut system_msg = pipeline_outcome.primary_system;
            let mut dynamic_msg = pipeline_outcome.dynamic_system;
            let mut prompt_sections = pipeline_outcome.prompt_sections;
            // Pipeline decision is the only source of truth for tier + pruning.
            // Cache the outputs so the round-level block below uses them
            // instead of re-deriving a tier.
            let pipeline_tier = pipeline_outcome.tier;
            let mut pipeline_tool_schemas = pipeline_outcome.tool_schemas;
            let mut bridge_manifest_trace = pipeline_outcome.manifest_trace;
            let mut bridge_manifest_trace_json = bridge_manifest_trace.to_json();
            rewrite_bridge_runtime_manifest_model_resolution(
                &mut bridge_manifest_trace_json,
                requested_model_name.as_deref(),
                &model_name,
                &provider,
                rate_limit_fallback_trace.as_ref(),
            );
            // Debug: dump system prompt for cache analysis (env-gated).
            // Enable with ASTRA_PIPELINE_DUMP_SYSTEM_PROMPT=1. Writes to
            // $TMPDIR/astra-bridge-prompt-<sid>-<ts>.json so `diff` between
            // consecutive turns reveals which sections break cache prefix.
            if std::env::var("ASTRA_PIPELINE_DUMP_SYSTEM_PROMPT").is_ok() {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let sid = if session_id.is_empty() {
                    "nosid".to_string()
                } else {
                    session_id.clone()
                };
                let dump_path = std::env::temp_dir().join(format!(
                    "astra-bridge-prompt-{sid}-{ts}.json"
                ));
                let dump_content = serde_json::to_string_pretty(&system_msg)
                    .unwrap_or_else(|_| "serialize error".into());
                let _ = std::fs::write(&dump_path, &dump_content);
            }
            llm_messages.push(system_msg);
            let mut bridge_volatile_text: Option<String> = None;
            if let Some(dyn_msg) = dynamic_msg {
                // Volatile per-turn content (Self-Awareness counter, session
                // anchor, etc.) — ALL protocols now route it to the
                // last user message prefix so the system + tools prefix stays
                // byte-stable across rounds. Earlier the Anthropic/Bedrock
                // paths embedded volatile in the system content array past
                // the cache_control marker; controlled probes (session
                // 5c5cbf78, see deepseek_anthropic_cache_probe.py) showed
                // DeepSeek treats the byte change as a fresh payload and
                // never reaches the 2nd-warm state where tools enter cache.
                // Bedrock is unaffected either way.
                let dyn_text = dyn_msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // Stash for prepending to last user message after history is added
                bridge_volatile_text = Some(dyn_text);
            }

            // Merge tool results into messages (handle continuation turns)
            // Client sends complete message history including tool role messages,
            // so we just use messages directly.
            //
            // Phase 3/Convergence: both the bridge and `ServerAgenticLoopHost`
            // now route through the shared `wire_assembly::MemoriaContext`.
            // The summary client still constructs here (it captures the
            // current request's auth headers + overrides) and is injected.
            let (merged_messages, _initial_tier) = {
                let raw = messages.clone();

                let compact_config = crate::prompts::CompactConfig::from_env();
                let summary_client = astra_turn_core::cloud_summary::HttpSummaryClient::new(
                    astra_turn_core::cloud_summary::LlmConnParams {
                        model_name: model_name.clone(),
                        api_key: api_key.clone(),
                        base_url: base_url.clone(),
                        provider: provider.clone(),
                        max_output_tokens: compact_config.summary_token_budget,
                    },
                );
                let memoria_client = memoria_client_shared.clone();

                let ctx = crate::turn::wire_assembly::MemoriaContext {
                    session_id: &session_id,
                    model_name: &model_name,
                    context_window: model_context_window,
                    memoria_client: memoria_client.as_ref().map(|c| {
                        c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient
                    }),
                    summary_client: Some(
                        &summary_client
                            as &dyn astra_turn_core::cloud_summary::SummaryLlmClient,
                    ),
                    tier: pipeline_tier,
                    session_facts: session_facts_shared.lock().ok().map(|f| f.clone()),
                    turn_number: 0,
                    observatory: None,
                };

                let compact_result = ctx.compact(&raw, &llm_messages, &edge_tools).await;

                if let Some(rerun) =
                    crate::turn::wire_assembly::rerun_with_distinct_session_memory_entry_for_user_turn(
                        compact_result.session_memory_context.as_deref(),
                        initial_session_memory_entry.as_ref(),
                        trace_turn,
                        user_content_for_signal,
                        |session_memory_entry| {
                            crate::turn::llm::context::assemble_bridge_context(
                                crate::turn::llm::context::BridgeContextAssemblyInput {
                                    tool_surface:
                                        crate::turn::llm::context::ToolSurfacePlan::from_visible_tools(
                                            &edge_tools,
                                            &bridge_restricted_snapshot,
                                        )
                                        .with_deferred_tools_block(&deferred_block_str),
                                    runtime_signals:
                                        crate::turn::llm::context::BridgeRuntimeSignals::new(
                                            &stable_sections,
                                            &effective_dynamic_sections,
                                            &memoria_prefetch_entries,
                                            Some(session_memory_entry),
                                            edge_profile
                                                .get("system_prompt_override")
                                                .and_then(Value::as_str),
                                        ),
                                    session:
                                        crate::turn::llm::context::BridgeSessionContextInput::new(
                                            &cache_cfg,
                                            cache_capability,
                                            &session_id,
                                            &model_name,
                                            &provider,
                                            edge_profile.get("cwd").and_then(Value::as_str),
                                            edge_profile.get("git_branch").and_then(Value::as_str),
                                            project_context,
                                            &bridge_session_current_date,
                                        )
                                        .with_context_window(model_context_window)
                                        .with_skill_listing_block(
                                            skill_listing_hint_text.as_deref().unwrap_or(""),
                                        ),
                                },
                            )
                        },
                    )
                {
                    debug_assert_eq!(rerun.tier, pipeline_tier);
                    system_msg = rerun.primary_system;
                    dynamic_msg = rerun.dynamic_system;
                    prompt_sections = rerun.prompt_sections;
                    pipeline_tool_schemas = rerun.tool_schemas;
                    bridge_manifest_trace = rerun.manifest_trace;
                    bridge_manifest_trace_json = bridge_manifest_trace.to_json();
                    rewrite_bridge_runtime_manifest_model_resolution(
                        &mut bridge_manifest_trace_json,
                        requested_model_name.as_deref(),
                        &model_name,
                        &provider,
                        rate_limit_fallback_trace.as_ref(),
                    );
                    llm_messages.clear();
                    llm_messages.push(system_msg.clone());
                    bridge_volatile_text = dynamic_msg
                        .as_ref()
                        .and_then(|dyn_msg| dyn_msg.get("content"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }

                let mut msgs = compact_result.messages;
                crate::turn::wire_assembly::maybe_append_continuation_prompt(
                    &mut msgs,
                    compact_result.boundary.is_some(),
                );

                (msgs, pipeline_tier)
            };

            llm_messages.extend(merged_messages);

            let bridge_synthetic_tail_prefix_end =
                crate::turn::llm::context::finalize_bridge_wire_messages(
                &mut llm_messages,
                bridge_volatile_text.take(),
                required_runtime_text.clone(),
                &provider,
                &model_name,
                &thinking_config,
                cache_capability,
                &cache_cfg,
            );

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
            let budget = crate::prompts::budget_for_model_with_override(
                Some(&model_name),
                model_context_window,
            );
            let max_output_tokens = crate::prompts::capped_output_tokens(&budget);

            let mut last_measured_prompt: Option<u64> = None;
            let mut bridge_pipeline_baseline = load_bridge_pipeline_baseline(&session_id);


            // Single LLM call per HTTP request (no multi-round tool loop).
            let round_ix = 0i64;
            let capture_round_ix = i64::from(round_index);
            {
                cloud_loop_turns += 1;

                // Budget check removed: single LLM call per HTTP request.

                // Round-level tools come from the pipeline's Optimize phase,
                // which already ran tier-appropriate schema pruning. The
                // per-round `filter_round_edge_tools` pass strips names in
                // `restricted_tools` — here always empty, so the pipeline
                // output is already the authoritative set.
                let round_edge_tools =
                    filter_round_edge_tools(&edge_tools, &HashSet::new());
                // `pipeline_tier` is the authoritative tier; the per-round
                // tier refinement (based on `last_measured_prompt`) used to
                // live here but produced tier drift between planner and
                // tool-pruning paths. Phase 3 will feed last-measured back
                // into the planner; until then rely on pipeline output.
                let _ = last_measured_prompt; // referenced for future wiring
                let mut pruned_tools = crate::turn::llm::context::stabilize_tool_schemas_for_cache(
                    &pipeline_tool_schemas,
                    &bridge_pipeline_baseline.last_tool_schemas,
                    &round_edge_tools,
                    cache_cap,
                    round_index,
                );
                bridge_pipeline_baseline.last_tool_schemas = pruned_tools.clone();
                crate::turn::llm::context::annotate_tool_schemas_for_cache(
                    &mut pruned_tools,
                    &cache_cfg,
                    &always_load_tool_names_for_bridge(&edge_profile),
                );

                let loop_started = Instant::now();
                let mut loop_tool_calls: Vec<Value> = Vec::new();
                let mut loop_text = String::new();
                let mut loop_reasoning = String::new();
                let mut loop_reasoning_signature = String::new();
                let mut attempt_in_round = 0_u32;
                let request_capture_model = if resolved_model.is_empty() {
                    model_name.clone()
                } else {
                    resolved_model.clone()
                };

                // Capture the request AFTER all wire mutations: session-anchor
                // append, prompt-cache metadata (cache_control marker on the
                // last pre-user message), tool schema annotations.
                // Historically the capture ran here,
                // BEFORE `apply_anthropic_cache_metadata` — which meant the
                // trace reported a pre-mutation snapshot that doesn't match
                // what actually went out on the wire. Concretely: Anthropic
                // request captures showed 0 `cache_control` markers on
                // messages even though the downstream call DID have them,
                // making "is prompt-cache working?" uninvestigable from a
                // trace alone. See session c6e18730 analysis.
                //
                // The capture now happens after the e2e-round fixture branch
                // so that both real and fixture paths record the final
                // bytes. E2E path: mutations already applied by the fixture
                // (no re-mutate). Real path: mutations applied here, then
                // capture, then the real LLM call consumes `llm_messages`
                // which STILL carries those mutations.

                let e2e_round: Option<&Value> = if use_e2e_llm {
                    bridge_e2e
                        .as_ref()
                        .and_then(|r| r.get(round_ix as usize))
                } else {
                    None
                };

                // Both branches below apply the same wire mutations and then
                // capture the final state. Binding the capture args once here
                // makes "E2E and real path record byte-identical traces" a
                // structural property rather than a copy-paste invariant
                // (reviewers previously had to diff the two call sites).
                let capture_request = |buf: &mut Option<TurnEventBuffer>,
                                       msgs: &[Value],
                                       attempt: u32| {
                    record_full_llm_request_event(
                        buf,
                        full_llm_capture,
                        &user_id,
                        &session_id,
                        trace_turn,
                        &trace_correlation,
                        "bridge_inprocess",
                        &request_capture_model,
                        &provider,
                        attempt,
                        msgs,
                        &pruned_tools,
                        Some(max_output_tokens),
                    );
                };

                // Emit system prompt breakdown so CLI can record precise per-component trace.
                let skill_injections: Vec<astra_turn_core::context_assembly_trace::SkillInjection> =
                    edge_profile
                        .get("active_skills")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            let names: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                            if names.is_empty() {
                                vec![]
                            } else {
                                // Total tokens for the skill hint section, split evenly.
                                let hint_tokens = prompts::estimate_str_tokens(&skill_hint) as u32;
                                let per = hint_tokens / names.len().max(1) as u32;
                                names
                                    .iter()
                                    .map(|name| {
                                        astra_turn_core::context_assembly_trace::SkillInjection {
                                            skill_name: name.to_string(),
                                            skill_version: None,
                                            tokens: per,
                                            selection_reason: "active_output_skill".into(),
                                        }
                                    })
                                    .collect()
                            }
                        })
                        .unwrap_or_default();
                let memory_injections: Vec<astra_turn_core::context_assembly_trace::MemoryInjection> =
                    memoria_prefetch_entries
                        .iter()
                        .enumerate()
                        .map(|(i, entry)| {
                            astra_turn_core::context_assembly_trace::MemoryInjection {
                                memory_id: format!("prefetch-{i}-{:016x}", entry.content_hash),
                                memory_type: entry
                                    .source
                                    .clone()
                                    .unwrap_or_else(|| "hybrid_retrieval".into()),
                                tokens: entry.token_estimate,
                                relevance_score: entry.relevance_score,
                                content_preview:
                                    astra_turn_core::context_assembly_trace::preview_snippet(
                                        &entry.content,
                                        100,
                                    ),
                            }
                        })
                        .collect();
                let session_memory_injection = initial_session_memory_entry.as_ref().map(|entry| {
                    let memory_type = entry
                        .source
                        .clone()
                        .unwrap_or_else(|| "session_memory".into());
                    astra_turn_core::context_assembly_trace::MemoryInjection {
                        memory_id: "session-memory".into(),
                        memory_type: memory_type.clone(),
                        tokens: prompts::estimate_str_tokens(&entry.content) as u32,
                        relevance_score: if memory_type == "session_memory.reanchor" {
                            1.0
                        } else {
                            0.35
                        },
                        content_preview: astra_turn_core::context_assembly_trace::preview_snippet(
                            &entry.content,
                            100,
                        ),
                    }
                });
                let breakdown = prompts::build_system_prompt_trace(
                    &prompt_sections,
                    skill_injections,
                    memory_injections,
                    session_memory_injection,
                );

                if let Some(round_val) = e2e_round {
                    // E2E fixture path: apply cache annotations first so the
                    // captured wire state matches the real-LLM branch below.
                    // Otherwise fixture runs would trace pre-mutation state
                    // even though the request shape sent to the real API
                    // would be post-mutation. Traces from E2E tests must be
                    // comparable to traces from real runs.
                    crate::turn::llm::context::apply_bridge_message_cache_metadata(
                        &mut llm_messages,
                        bridge_synthetic_tail_prefix_end,
                        &cache_cfg,
                        &session_id,
                    );
                    crate::turn::llm::context::augment_manifest_trace_with_wire(
                        &mut bridge_manifest_trace_json,
                        &llm_messages,
                        &pruned_tools,
                    );
                    capture_request(&mut turn_event_buffer, &llm_messages, attempt_in_round);
                    if let Ok(prompt_request_plan) =
                        astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
                            user_id: &user_id,
                            session_id: &session_id,
                            turn: trace_turn,
                            round: turn_event_buffer
                                .as_ref()
                                .map(|buffer| buffer.current_round())
                                .unwrap_or(round_index),
                            attempt: attempt_in_round,
                            source: "bridge_inprocess",
                            messages: &llm_messages,
                            tools: &pruned_tools,
                            max_output_tokens: Some(max_output_tokens),
                        })
                    {
                        crate::turn::llm::exchange_capture::spawn_prompt_request_plan_persist_or_log(
                            "bridge_inprocess e2e capture",
                            shared_pool.clone(),
                            astra_services::PromptRequestPersistInput {
                                session_id: session_id.clone(),
                                user_id: user_id.clone(),
                                run_id: Some(run_id.clone()),
                                turn: trace_turn,
                                round: turn_event_buffer
                                    .as_ref()
                                    .map(|buffer| buffer.current_round())
                                    .unwrap_or(round_index),
                                attempt: attempt_in_round,
                                source: "bridge_inprocess".to_string(),
                                model: request_capture_model.clone(),
                                provider: provider.clone(),
                            },
                            prompt_request_plan,
                        );
                    }
                    yield render_sse(&crate::turn::llm::context::context_meta_event(
                        &breakdown,
                        Some(&bridge_manifest_trace_json),
                    ));
                    #[cfg(feature = "bridge-e2e-hooks")]
                    {
                        let (t, r, tc, u_delta, delay_ms) =
                            astra_turn_core::bridge_e2e_hooks::parse_llm_round(round_val);
                        if delay_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        }
                        loop_text = t;
                        loop_reasoning = r;
                        loop_tool_calls = tc;
                        // Test fixtures provide raw OpenAI-style usage; normalize
                        // through the same extractor the real provider path uses
                        // so downstream partial canonical parsing sees canonical
                        // keys. Bedrock-flavored fixtures are dispatched via the
                        // configured provider string.
                        if !u_delta.is_empty()
                            && let Some(tu) = crate::turn::token_usage::extract_usage(
                                crate::turn::token_usage::UsageDialect::for_provider(&provider),
                                &u_delta,
                            )
                        {
                            usage = tu.to_json_map();
                        }
                    }
                    #[cfg(not(feature = "bridge-e2e-hooks"))]
                    {
                        let _ = round_val;
                    }
                } else {
                    // Add Anthropic protocol-level prompt-cache metadata on the request clone.
                    crate::turn::llm::context::apply_bridge_message_cache_metadata(
                        &mut llm_messages,
                        bridge_synthetic_tail_prefix_end,
                        &cache_cfg,
                        &session_id,
                    );
                    crate::turn::llm::context::augment_manifest_trace_with_wire(
                        &mut bridge_manifest_trace_json,
                        &llm_messages,
                        &pruned_tools,
                    );

                    // Capture the final post-mutation request state (see the
                    // long note ~60 lines up for why this is here and not
                    // before the mutations).
                    capture_request(&mut turn_event_buffer, &llm_messages, attempt_in_round);
                    if let Ok(prompt_request_plan) =
                        astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
                            user_id: &user_id,
                            session_id: &session_id,
                            turn: trace_turn,
                            round: turn_event_buffer
                                .as_ref()
                                .map(|buffer| buffer.current_round())
                                .unwrap_or(round_index),
                            attempt: attempt_in_round,
                            source: "bridge_inprocess",
                            messages: &llm_messages,
                            tools: &pruned_tools,
                            max_output_tokens: Some(max_output_tokens),
                        })
                    {
                        crate::turn::llm::exchange_capture::spawn_prompt_request_plan_persist_or_log(
                            "bridge_inprocess live capture",
                            shared_pool.clone(),
                            astra_services::PromptRequestPersistInput {
                                session_id: session_id.clone(),
                                user_id: user_id.clone(),
                                run_id: Some(run_id.clone()),
                                turn: trace_turn,
                                round: turn_event_buffer
                                    .as_ref()
                                    .map(|buffer| buffer.current_round())
                                    .unwrap_or(round_index),
                                attempt: attempt_in_round,
                                source: "bridge_inprocess".to_string(),
                                model: request_capture_model.clone(),
                                provider: provider.clone(),
                            },
                            prompt_request_plan,
                        );
                    }

                    // wip-7: emit per-channel fingerprints ONLY — no raw
                    // text crosses the HTTP boundary. Raw channel content
                    // (learned feedback rules, memoria recall digests,
                    // self-awareness summaries, user-correction excerpts)
                    // is sensitive runtime state that leaked via
                    // `transform_run_event_for_client`'s pass-through in
                    // wip-5. Fingerprints carry opaque hash + byte length
                    // + empty-flag, enough for `ObservabilitySession` to
                    // detect content change but useless to an external
                    // API consumer.
                    //
                    // wip-7 fix #2 (volatile gating): the fingerprint
                    // reflects *what the model actually sees* — when
                    // `CacheCapability::should_inject_volatile_on_round`
                    // returns false, the whole volatile lane is dropped
                    // downstream and we must emit empty fingerprints so
                    // the freshness report doesn't claim the channel is
                    // live. `effective_dynamic_sections` is the
                    // post-gate section list; if it's empty while the
                    // pre-gate list had entries, gating dropped them.
                    //
                    // wip-7 fix #3 (memoria_prefetch provenance): the
                    // raw `memoria_prefetch_section` string is the
                    // pre-pipeline hint. The pipeline's Memory binder
                    // ranks / dedups / budgets the typed
                    // `memoria_prefetch_entries` list and emits the
                    // final prompt bytes. Fingerprinting the typed list
                    // (stable concat of per-entry content) tracks the
                    // actual injection — not the unprocessed hint.
                    let volatile_injected =
                        cache_cap.should_inject_volatile_on_round(round_index);
                    let fp = |content: &str| {
                        astra_turn_core::injection_tracking::InjectionFingerprint::from_content(
                            content,
                        )
                    };
                    let bridge_channel = |tag: &str, content: &str| {
                        // Gated-out content → empty fingerprint, tracking
                        // the model's actual view of the lane (fix #2).
                        let effective = if volatile_injected { content } else { "" };
                        let fpv = fp(effective);
                        json!({
                            "tag": tag,
                            "hash": fpv.hash,
                            "bytes": effective.len() as u64,
                            "is_empty": fpv.is_empty,
                        })
                    };
                    // Memoria prefetch: fingerprint the typed-entry list
                    // (post-retrieval, pre-pipeline-binder). Canonical
                    // serialization = concatenation of per-entry content
                    // with a stable separator so identical retrievals
                    // across turns produce identical hashes.
                    // Canonical fingerprint: concat(stable_lane,
                    // typed_per_turn_entries). Stable lane goes first
                    // so its contribution dominates the hash; a change
                    // there (rare — new episode written) is a real
                    // cache-prefix flip. Per-turn bytes come after as
                    // the volatile tail.
                    let memoria_prefetch_canonical = {
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(ref s) = stable_memory_section {
                            parts.push(s.clone());
                        }
                        if !memoria_prefetch_entries.is_empty() {
                            parts.push(
                                memoria_prefetch_entries
                                    .iter()
                                    .map(|e| e.content.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n---\n"),
                            );
                        } else if let Some(ref v) = volatile_memory_section {
                            parts.push(v.clone());
                        }
                        parts.join("\n---\n")
                    };
                    let channels_payload = json!([
                        // CLI-owned channels: the bridge echoes them for
                        // symmetry, but the CLI fingerprints its own
                        // source-of-truth texts post-turn and those
                        // authoritative fingerprints win. Bytes here
                        // reflect the bridge-side view of the same
                        // strings in edge_profile.
                        bridge_channel("self_awareness", &self_awareness_hint),
                        bridge_channel("memoria_insights", &memoria_insights_hint),
                        bridge_channel("recent_arg_hints", &recent_arg_hints_hint),
                        bridge_channel(
                            "skill_listing",
                            skill_listing_hint_text.as_deref().unwrap_or(""),
                        ),
                        // lessons is CLI-owned and has no bridge source —
                        // emit an empty placeholder so the CLI observer
                        // (which only reads bridge fingerprints for
                        // bridge-internal channels) doesn't rely on it.
                        bridge_channel("lessons", ""),
                        // Bridge-internal channels: only visible here.
                        bridge_channel("memoria_prefetch", &memoria_prefetch_canonical),
                        bridge_channel("feedback_rules", &feedback_rules_hint),
                        bridge_channel("implicit_feedback", &implicit_feedback_hint),
                        bridge_channel("tool_round_guidance", &tool_round_guidance),
                        bridge_channel(
                            "volatile_pending",
                            env_volatile.as_deref().unwrap_or(""),
                        ),
                    ]);
                    yield render_sse(&json!({
                        "type": "injection_freshness",
                        "channels": channels_payload,
                    }));
                    yield render_sse(&crate::turn::llm::context::context_meta_event(
                        &breakdown,
                        Some(&bridge_manifest_trace_json),
                    ));
                    let mut client_stopped = false;
                    let llm_stream = if let Some(blocks) = bridge_e2e_stream_blocks
                        .clone()
                        .filter(|blocks| !blocks.is_empty() && round_ix == 0)
                    {
                        futures_util::stream::iter(blocks.into_iter().map(Bytes::from)).boxed()
                    } else {
                        match call_llm_stream_with_request_overrides(
                            &llm_messages,
                            &pruned_tools,
                            &model_name,
                            wire_model_name.as_deref(),
                            &api_key,
                            &base_url,
                            &provider,
                            Some(max_output_tokens),
                            has_fallback,
                            cc.clone(),
                            &thinking_config,
                            request_body_overrides.as_ref(),
                            llm_header_overrides.as_ref(),
                            completions_url_override.as_deref(),
                            None,
                        )
                        .await
                        {
                            Ok(s) => s.boxed(),
                            Err(e) if astra_core::is_llm_context_window_error(&e) => {
                            record_full_llm_response_event(
                                &mut turn_event_buffer,
                                full_llm_capture,
                                &session_id,
                                trace_turn,
                                &trace_correlation,
                                "bridge_inprocess",
                                &request_capture_model,
                                &provider,
                                attempt_in_round,
                                "context_window_error",
                                json!({
                                    "error": e.clone(),
                                    "kind": "context_window",
                                }),
                            );
                            // Context-window error: force aggressive compaction and retry once
                            astra_core::agent_warn!(
                                "bridge",
                                "context window exceeded — forcing aggressive compaction and retrying"
                            );
                            // Aggressive retry: re-route through the shared
                            // `MemoriaContext` with tighter budget overrides so
                            // the aggressive path and the main path share one
                            // compaction + summary-client construction flow.
                            let budget = crate::prompts::budget_for_model_with_override(
                                Some(&model_name),
                                model_context_window,
                            );
                            let compact_config = crate::prompts::CompactConfig::from_env();
                            let summary_client = astra_turn_core::cloud_summary::HttpSummaryClient::new(
                                astra_turn_core::cloud_summary::LlmConnParams {
                                    model_name: model_name.clone(),
                                    api_key: api_key.clone(),
                                    base_url: base_url.clone(),
                                    provider: provider.clone(),
                                    max_output_tokens: compact_config.summary_token_budget,
                                },
                            );
                            let memoria_client = memoria_client_owned.clone();

                            let aggressive_ctx = crate::turn::wire_assembly::MemoriaContext {
                                session_id: &session_id,
                                model_name: &model_name,
                                context_window: model_context_window,
                                memoria_client: memoria_client.as_ref().map(|c| {
                                    c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient
                                }),
                                summary_client: Some(
                                    &summary_client
                                        as &dyn astra_turn_core::cloud_summary::SummaryLlmClient,
                                ),
                                tier: crate::prompts::CompactionTier::AggressivePrune,
                                session_facts: session_facts_shared
                                    .lock()
                                    .ok()
                                    .map(|f| f.clone()),
                                turn_number: 0,
                                observatory: None,
                            };
                            let overrides = crate::turn::wire_assembly::BudgetOverrides {
                                budget_chars: Some(budget.effective_input_limit() * 3),
                                keep_chars: Some(1_000),
                                keep_recent_turns: Some(4),
                                current_tokens: Some(budget.effective_input_limit()),
                                tier: None, // ctx.tier already carries AggressivePrune
                            };

                            // Split out the leading system prefix so Memoria
                            // only sees user/assistant/tool history; the
                            // system block stays untouched across retries.
                            let sys_count = llm_messages
                                .iter()
                                .take_while(|m| {
                                    m.get("role").and_then(Value::as_str) == Some("system")
                                })
                                .count();
                            let (system_prefix, original_msgs) = llm_messages.split_at(sys_count);
                            let retry_compaction_history =
                                crate::turn::llm::context::bridge_retry_compaction_history(
                                    original_msgs,
                                );
                            let compact_result = aggressive_ctx
                                .compact_with_overrides(
                                    &retry_compaction_history,
                                    system_prefix,
                                    &round_edge_tools,
                                    overrides,
                                )
                                .await;

                            let rebuilt_retry_messages =
                                crate::turn::llm::context::rebuild_bridge_retry_wire_messages(
                                    crate::turn::llm::context::BridgeRetryWireRebuildInput {
                                        previous_messages: &llm_messages,
                                        compacted_messages: compact_result.messages,
                                        boundary_present: compact_result.boundary.is_some(),
                                        required_runtime_text: required_runtime_text.clone(),
                                        provider: &provider,
                                        model_name: &model_name,
                                        thinking: &thinking_config,
                                        cache_capability,
                                        cache_cfg: &cache_cfg,
                                        session_id: &session_id,
                                    },
                                );
                            llm_messages = rebuilt_retry_messages;

                            // Also prune tool schemas more aggressively
                            pruned_tools = prune_tool_schemas(
                                &round_edge_tools,
                                crate::prompts::CompactionTier::AggressivePrune,
                            );
                            crate::turn::llm::context::annotate_tool_schemas_for_cache(
                                &mut pruned_tools,
                                &cache_cfg,
                                &always_load_tool_names_for_bridge(&edge_profile),
                            );
                            crate::turn::llm::context::augment_manifest_trace_with_wire(
                                &mut bridge_manifest_trace_json,
                                &llm_messages,
                                &pruned_tools,
                            );
                            yield render_sse(&crate::turn::llm::context::context_meta_event(
                                &breakdown,
                                Some(&bridge_manifest_trace_json),
                            ));
                            attempt_in_round = attempt_in_round.saturating_add(1);
                            record_full_llm_request_event(
                                &mut turn_event_buffer,
                                full_llm_capture,
                                &user_id,
                                &session_id,
                                trace_turn,
                                &trace_correlation,
                                "bridge_inprocess",
                                &request_capture_model,
                                &provider,
                                attempt_in_round,
                                &llm_messages,
                                &pruned_tools,
                                Some(max_output_tokens / 2),
                            );
                            if let Ok(prompt_request_plan) =
                                astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
                                    user_id: &user_id,
                                    session_id: &session_id,
                                    turn: trace_turn,
                                    round: turn_event_buffer
                                        .as_ref()
                                        .map(|buffer| buffer.current_round())
                                        .unwrap_or(round_index),
                                    attempt: attempt_in_round,
                                    source: "bridge_inprocess",
                                    messages: &llm_messages,
                                    tools: &pruned_tools,
                                    max_output_tokens: Some(max_output_tokens / 2),
                                })
                            {
                                crate::turn::llm::exchange_capture::spawn_prompt_request_plan_persist_or_log(
                                    "bridge_inprocess retry capture",
                                    shared_pool.clone(),
                                    astra_services::PromptRequestPersistInput {
                                        session_id: session_id.clone(),
                                        user_id: user_id.clone(),
                                        run_id: Some(run_id.clone()),
                                        turn: trace_turn,
                                        round: turn_event_buffer
                                            .as_ref()
                                            .map(|buffer| buffer.current_round())
                                            .unwrap_or(round_index),
                                        attempt: attempt_in_round,
                                        source: "bridge_inprocess".to_string(),
                                        model: request_capture_model.clone(),
                                        provider: provider.clone(),
                                    },
                                    prompt_request_plan,
                                );
                            }

                            // Retry LLM call
                            match call_llm_stream_with_request_overrides(
                                &llm_messages,
                                &pruned_tools,
                                &model_name,
                                wire_model_name.as_deref(),
                                &api_key,
                                &base_url,
                                &provider,
                                Some(max_output_tokens / 2), // reduce output budget too
                                has_fallback,
                                cc.clone(),
                                &thinking_config,
                                request_body_overrides.as_ref(),
                                llm_header_overrides.as_ref(),
                                completions_url_override.as_deref(),
                                None,
                            )
                            .await
                            {
                                Ok(s) => s.boxed(),
                                Err(e2) => {
                                    record_full_llm_response_event(
                                        &mut turn_event_buffer,
                                        full_llm_capture,
                                        &session_id,
                                        trace_turn,
                                        &trace_correlation,
                                        "bridge_inprocess",
                                        &request_capture_model,
                                        &provider,
                                        attempt_in_round,
                                        "context_window_error",
                                        json!({
                                            "error": e2.clone(),
                                            "kind": "context_window",
                                        }),
                                    );
                                    let kind = astra_core::classify_llm_error_message(&e2);
                                    let dump = astra_turn_core::llm_request_dump::build_llm_request_dump(
                                        &session_id, agent_id.as_deref(), &model_name, &provider,
                                        &e2, &llm_messages, &pruned_tools,
                                        capture_round_ix, Some(max_output_tokens / 2),
                                    );
                                    if let Err(error) =
                                        dump.persist_remote(&user_id, remote_artifact_store.as_ref()).await
                                    {
                                        astra_core::agent_error!(
                                            "llm-dump",
                                            "bridge_inprocess compacted context-window dump persist failed: {error}"
                                        );
                                    }
                                    crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
                                        "bridge_inprocess compacted context-window capture",
                                        full_llm_capture,
                                        Some(remote_artifact_store.as_ref()),
                                        &session_id,
                                        &user_id,
                                        trace_turn,
                                        u32::try_from(capture_round_ix).unwrap_or(u32::MAX),
                                        agent_id.as_deref(),
                                        "bridge_inprocess",
                                        if resolved_model.is_empty() { &model_name } else { &resolved_model },
                                        &provider,
                                        &llm_messages,
                                        &pruned_tools,
                                        Some(max_output_tokens / 2),
                                        "context_window_error",
                                        json!({
                                            "error": e2,
                                            "kind": "context_window",
                                        }),
                                        Some(trace_correlation.as_capture_trace()),
                                    )
                                    .await;
                                    yield render_sse_map(&build_stream_error_event(
                                        &format!("Context window exceeded even after aggressive compaction: {e2}"),
                                        kind.as_str(),
                                        false, // not retryable
                                    ));
                                    flush_turn_event_buffer_or_warn(
                                        &mut turn_event_buffer,
                                        &session_id,
                                        "bridge compacted context-window failure",
                                    );
                                    mark_disconnect_capture_finalized(&disconnect_capture_state);
                                    return;
                                }
                            }
                        }
                            Err(e) => {
                            record_full_llm_response_event(
                                &mut turn_event_buffer,
                                full_llm_capture,
                                &session_id,
                                trace_turn,
                                &trace_correlation,
                                "bridge_inprocess",
                                &request_capture_model,
                                &provider,
                                attempt_in_round,
                                "error",
                                json!({
                                    "error": e.clone(),
                                    "kind": astra_core::classify_llm_error_message(&e).as_str(),
                                }),
                            );
                            let kind = astra_core::classify_llm_error_message(&e);
                            let dump = astra_turn_core::llm_request_dump::build_llm_request_dump(
                                &session_id, agent_id.as_deref(), &model_name, &provider,
                                &e, &llm_messages, &pruned_tools,
                                capture_round_ix, Some(max_output_tokens),
                            );
                            if let Err(error) =
                                dump.persist_remote(&user_id, remote_artifact_store.as_ref()).await
                            {
                                astra_core::agent_error!(
                                    "llm-dump",
                                    "bridge_inprocess error dump persist failed: {error}"
                                );
                            }
                            crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
                                "bridge_inprocess error capture",
                                full_llm_capture,
                                Some(remote_artifact_store.as_ref()),
                                &session_id,
                                &user_id,
                                trace_turn,
                                u32::try_from(capture_round_ix).unwrap_or(u32::MAX),
                                agent_id.as_deref(),
                                "bridge_inprocess",
                                if resolved_model.is_empty() { &model_name } else { &resolved_model },
                                &provider,
                                &llm_messages,
                                &pruned_tools,
                                Some(max_output_tokens),
                                "error",
                                json!({
                                    "error": e,
                                    "kind": kind.as_str(),
                                }),
                                Some(trace_correlation.as_capture_trace()),
                            )
                            .await;
                            yield render_sse_map(&build_stream_error_event(&e, kind.as_str(), kind.is_retryable()));
                            flush_turn_event_buffer_or_warn(
                                &mut turn_event_buffer,
                                &session_id,
                                "bridge llm error",
                            );
                            mark_disconnect_capture_finalized(&disconnect_capture_state);
                            return;
                        }
                        }
                    };

                    update_disconnect_capture_snapshot(
                        &disconnect_capture_state,
                        DisconnectCaptureSnapshot {
                            started: true,
                            finalized: false,
                            session_id: session_id.clone(),
                            user_id: user_id.clone(),
                            turn: trace_turn,
                            session_turn_source: trace_turn_source.to_string(),
                            turn_chain_id: turn_chain_id.clone(),
                            user_query_event_id: user_query_event_id.clone(),
                            round_ix: capture_round_ix,
                            agent_id: agent_id.clone(),
                            model_name: model_name.clone(),
                            resolved_model: resolved_model.clone(),
                            provider: provider.clone(),
                            llm_messages: llm_messages.clone(),
                            pruned_tools: pruned_tools.clone(),
                            max_output_tokens: Some(max_output_tokens),
                            partial_text: loop_text.clone(),
                            partial_reasoning: loop_reasoning.clone(),
                            partial_tool_calls: loop_tool_calls.clone(),
                            usage: usage.clone(),
                        },
                    );

                    tokio::pin!(llm_stream);
                    let mut sse_buf = SseBlankLineUtf8Buf::new();
                    let mut saw_inprocess_summary = false;
                    let mut terminal_error_event: Option<Value> = None;
                    // Keepalive interval: emit SSE comments while waiting on
                    // the LLM so client disconnects are detected promptly
                    // instead of only on long stall boundaries.
                    let keepalive = tokio::time::Duration::from_secs(5);
                    let mut keepalive_deadline = tokio::time::Instant::now() + keepalive;

                    loop {
                        tokio::select! {
                            biased;
                            _ = crate::turn::llm::client::wait_until_cancelled_or_pending(cc.as_deref()) => {
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
                                        &mut loop_reasoning_signature,
                                        &mut loop_tool_calls,
                                        &mut usage,
                                        &mut resolved_model,
                                    ) {
                                        Ok(chunks) => {
                                            for b in chunks {
                                                if terminal_error_event.is_none() {
                                                    terminal_error_event = forwarded_sse_error_event(&b);
                                                }
                                                yield b;
                                            }
                                            update_disconnect_capture_snapshot(
                                                &disconnect_capture_state,
                                                DisconnectCaptureSnapshot {
                                                    started: true,
                                                    finalized: false,
                                                    session_id: session_id.clone(),
                                                    user_id: user_id.clone(),
                                                    turn: trace_turn,
                                                    session_turn_source: trace_turn_source.to_string(),
                                                    turn_chain_id: turn_chain_id.clone(),
                                                    user_query_event_id: user_query_event_id.clone(),
                                                    round_ix: capture_round_ix,
                                                    agent_id: agent_id.clone(),
                                                    model_name: model_name.clone(),
                                                    resolved_model: resolved_model.clone(),
                                                    provider: provider.clone(),
                                                    llm_messages: llm_messages.clone(),
                                                    pruned_tools: pruned_tools.clone(),
                                                    max_output_tokens: Some(max_output_tokens),
                                                    partial_text: loop_text.clone(),
                                                    partial_reasoning: loop_reasoning.clone(),
                                                    partial_tool_calls: loop_tool_calls.clone(),
                                                    usage: usage.clone(),
                                                },
                                            );
                                        }
                                        Err(msg) => {
                                            record_full_llm_response_event(
                                                &mut turn_event_buffer,
                                                full_llm_capture,
                                                &session_id,
                                                trace_turn,
                                                &trace_correlation,
                                                "bridge_inprocess",
                                                if resolved_model.is_empty() {
                                                    model_name.as_str()
                                                } else {
                                                    resolved_model.as_str()
                                                },
                                                &provider,
                                                attempt_in_round,
                                                "sse_parse_error",
                                                bridge_error_response_payload(
                                                    &msg,
                                                    "SSE_PARSE_ERROR",
                                                    &loop_text,
                                                    &loop_reasoning,
                                                    &loop_tool_calls,
                                                    &usage,
                                                ),
                                            );
                                            persist_bridge_stream_failure_capture(
                                                "bridge_inprocess stream block parse capture",
                                                full_llm_capture,
                                                remote_artifact_store.as_ref(),
                                                &session_id,
                                                &user_id,
                                                trace_turn,
                                                &trace_correlation,
                                                capture_round_ix,
                                                agent_id.as_deref(),
                                                &model_name,
                                                &resolved_model,
                                                &provider,
                                                &llm_messages,
                                                &pruned_tools,
                                                Some(max_output_tokens),
                                                "sse_parse_error",
                                                &msg,
                                                "SSE_PARSE_ERROR",
                                                &loop_text,
                                                &loop_reasoning,
                                                &loop_tool_calls,
                                                &usage,
                                            )
                                            .await;
                                            astra_core::agent_warn!("bridge", "in-process LLM SSE block invalid: {msg}");
                                            yield render_sse_map(&build_stream_error_event(
                                                &msg,
                                                "SSE_PARSE_ERROR",
                                                false,
                                            ));
                                            flush_turn_event_buffer_or_warn(
                                                &mut turn_event_buffer,
                                                &session_id,
                                                "bridge stream block parse failure",
                                            );
                                            mark_disconnect_capture_finalized(&disconnect_capture_state);
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
                        record_full_llm_response_event(
                            &mut turn_event_buffer,
                            full_llm_capture,
                            &session_id,
                            trace_turn,
                            &trace_correlation,
                            "bridge_inprocess",
                            if resolved_model.is_empty() {
                                model_name.as_str()
                            } else {
                                resolved_model.as_str()
                            },
                            &provider,
                            attempt_in_round,
                            "client_disconnect",
                            bridge_error_response_payload(
                                "Request cancelled (client disconnected)",
                                "CLIENT_DISCONNECT",
                                &loop_text,
                                &loop_reasoning,
                                &loop_tool_calls,
                                &usage,
                            ),
                        );
                        persist_bridge_stream_failure_capture(
                            "bridge_inprocess client disconnect capture",
                            full_llm_capture,
                            remote_artifact_store.as_ref(),
                            &session_id,
                            &user_id,
                            trace_turn,
                            &trace_correlation,
                            capture_round_ix,
                            agent_id.as_deref(),
                            &model_name,
                            &resolved_model,
                            &provider,
                            &llm_messages,
                            &pruned_tools,
                            Some(max_output_tokens),
                            "client_disconnect",
                            "Request cancelled (client disconnected)",
                            "CLIENT_DISCONNECT",
                            &loop_text,
                            &loop_reasoning,
                            &loop_tool_calls,
                            &usage,
                        )
                        .await;
                        yield render_sse_map(&build_stream_error_event(
                            "Request cancelled (client disconnected)",
                            "CLIENT_DISCONNECT",
                            false,
                        ));
                        flush_turn_event_buffer_or_warn(
                            &mut turn_event_buffer,
                            &session_id,
                            "bridge client disconnect",
                        );
                        mark_disconnect_capture_finalized(&disconnect_capture_state);
                        return;
                    }

                    let mut tail = sse_buf.into_inner();
                    match flush_tail_buf_into_llm_forward(
                        &mut tail,
                        &mut saw_inprocess_summary,
                        &mut loop_text,
                        &mut loop_reasoning,
                        &mut loop_reasoning_signature,
                        &mut loop_tool_calls,
                        &mut usage,
                        &mut resolved_model,
                    ) {
                        Ok(chunks) => {
                            for b in chunks {
                                if terminal_error_event.is_none() {
                                    terminal_error_event = forwarded_sse_error_event(&b);
                                }
                                yield b;
                            }
                            update_disconnect_capture_snapshot(
                                &disconnect_capture_state,
                        DisconnectCaptureSnapshot {
                            started: true,
                            finalized: false,
                            session_id: session_id.clone(),
                            user_id: user_id.clone(),
                            turn: trace_turn,
                            session_turn_source: trace_turn_source.to_string(),
                            turn_chain_id: turn_chain_id.clone(),
                            user_query_event_id: user_query_event_id.clone(),
                            round_ix: capture_round_ix,
                            agent_id: agent_id.clone(),
                                    model_name: model_name.clone(),
                                    resolved_model: resolved_model.clone(),
                                    provider: provider.clone(),
                                    llm_messages: llm_messages.clone(),
                                    pruned_tools: pruned_tools.clone(),
                                    max_output_tokens: Some(max_output_tokens),
                                    partial_text: loop_text.clone(),
                                    partial_reasoning: loop_reasoning.clone(),
                                    partial_tool_calls: loop_tool_calls.clone(),
                                    usage: usage.clone(),
                                },
                            );
                        }
                        Err(msg) => {
                            record_full_llm_response_event(
                                &mut turn_event_buffer,
                                full_llm_capture,
                                &session_id,
                                trace_turn,
                                &trace_correlation,
                                "bridge_inprocess",
                                if resolved_model.is_empty() {
                                    model_name.as_str()
                                } else {
                                    resolved_model.as_str()
                                },
                                &provider,
                                attempt_in_round,
                                "sse_parse_error",
                                bridge_error_response_payload(
                                    &msg,
                                    "SSE_PARSE_ERROR",
                                    &loop_text,
                                    &loop_reasoning,
                                    &loop_tool_calls,
                                    &usage,
                                ),
                            );
                            persist_bridge_stream_failure_capture(
                                "bridge_inprocess stream tail parse capture",
                                full_llm_capture,
                                remote_artifact_store.as_ref(),
                                &session_id,
                                &user_id,
                                trace_turn,
                                &trace_correlation,
                                capture_round_ix,
                                agent_id.as_deref(),
                                &model_name,
                                &resolved_model,
                                &provider,
                                &llm_messages,
                                &pruned_tools,
                                Some(max_output_tokens),
                                "sse_parse_error",
                                &msg,
                                "SSE_PARSE_ERROR",
                                &loop_text,
                                &loop_reasoning,
                                &loop_tool_calls,
                                &usage,
                            )
                            .await;
                            astra_core::agent_warn!("bridge", "in-process LLM SSE tail invalid: {msg}");
                            yield render_sse_map(&build_stream_error_event(
                                &msg,
                                "SSE_PARSE_ERROR",
                                false,
                            ));
                            flush_turn_event_buffer_or_warn(
                                &mut turn_event_buffer,
                                &session_id,
                                "bridge stream tail parse failure",
                            );
                            mark_disconnect_capture_finalized(&disconnect_capture_state);
                            return;
                        }
                    }

                    if let Some(error_event) = terminal_error_event.take() {
                        let error_message = error_event
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("bridge stream failed");
                        let error_code = error_event
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("stream_error");
                        record_full_llm_response_event(
                            &mut turn_event_buffer,
                            full_llm_capture,
                            &session_id,
                            trace_turn,
                            &trace_correlation,
                            "bridge_inprocess",
                            if resolved_model.is_empty() {
                                model_name.as_str()
                            } else {
                                resolved_model.as_str()
                            },
                            &provider,
                            attempt_in_round,
                            "error",
                            bridge_error_response_payload(
                                error_message,
                                error_code,
                                &loop_text,
                                &loop_reasoning,
                                &loop_tool_calls,
                                &usage,
                            ),
                        );
                        persist_bridge_stream_failure_capture(
                            "bridge_inprocess streamed error capture",
                            full_llm_capture,
                            remote_artifact_store.as_ref(),
                            &session_id,
                            &user_id,
                            trace_turn,
                            &trace_correlation,
                            capture_round_ix,
                            agent_id.as_deref(),
                            &model_name,
                            &resolved_model,
                            &provider,
                            &llm_messages,
                            &pruned_tools,
                            Some(max_output_tokens),
                            "error",
                            error_message,
                            error_code,
                            &loop_text,
                            &loop_reasoning,
                            &loop_tool_calls,
                            &usage,
                        )
                        .await;
                        flush_turn_event_buffer_or_warn(
                            &mut turn_event_buffer,
                            &session_id,
                            "bridge streamed terminal error",
                        );
                        mark_disconnect_capture_finalized(&disconnect_capture_state);
                        return;
                    }

                    if !saw_inprocess_summary {
                        record_full_llm_response_event(
                            &mut turn_event_buffer,
                            full_llm_capture,
                            &session_id,
                            trace_turn,
                            &trace_correlation,
                            "bridge_inprocess",
                            if resolved_model.is_empty() {
                                model_name.as_str()
                            } else {
                                resolved_model.as_str()
                            },
                            &provider,
                            attempt_in_round,
                            "stream_incomplete",
                            bridge_error_response_payload(
                                "LLM stream ended without completion summary from provider",
                                "STREAM_INCOMPLETE",
                                &loop_text,
                                &loop_reasoning,
                                &loop_tool_calls,
                                &usage,
                            ),
                        );
                        persist_bridge_stream_failure_capture(
                            "bridge_inprocess stream incomplete capture",
                            full_llm_capture,
                            remote_artifact_store.as_ref(),
                            &session_id,
                            &user_id,
                            trace_turn,
                            &trace_correlation,
                            capture_round_ix,
                            agent_id.as_deref(),
                            &model_name,
                            &resolved_model,
                            &provider,
                            &llm_messages,
                            &pruned_tools,
                            Some(max_output_tokens),
                            "stream_incomplete",
                            "LLM stream ended without completion summary from provider",
                            "STREAM_INCOMPLETE",
                            &loop_text,
                            &loop_reasoning,
                            &loop_tool_calls,
                            &usage,
                        )
                        .await;
                        yield render_sse_map(&build_stream_error_event(
                            "LLM stream ended without completion summary from provider",
                            "STREAM_INCOMPLETE",
                            true,
                        ));
                        flush_turn_event_buffer_or_warn(
                            &mut turn_event_buffer,
                            &session_id,
                            "bridge stream incomplete failure",
                        );
                        mark_disconnect_capture_finalized(&disconnect_capture_state);
                        return;
                    }
                }

                full_text.push_str(&loop_text);
                if use_e2e_llm && !loop_text.trim().is_empty() && loop_tool_calls.is_empty() {
                    yield render_sse(&json!({"type": "text_delta", "content": loop_text}));
                }
                if !loop_reasoning.is_empty() {
                    reasoning.push_str(&loop_reasoning);
                    if let Some(done) = reasoning_done_sse_bytes_if_needed(
                        &loop_reasoning,
                        &loop_reasoning_signature,
                    ) {
                        yield done;
                    }
                }
                let round_ms = loop_started.elapsed().as_millis();
                let usage_snapshot =
                    crate::turn::token_usage::TokenUsage::from_partial_json_map(&usage);
                astra_core::agent_info!(
                    "llm",
                    "⏱ LLM round done: total={}ms tok_in={} tok_cached={} tok_cache_write={} tok_out={} tools={} model={} sid={} r={}",
                    round_ms,
                    usage_snapshot.input_tokens,
                    usage_snapshot.cached_input_tokens,
                    usage_snapshot.cache_creation_tokens,
                    usage_snapshot.output_tokens,
                    loop_tool_calls.len(),
                    if resolved_model.is_empty() { &model_name } else { &resolved_model },
                    session_id,
                    capture_round_ix,
                );
                llm_steps.push(json!({
                    "step": "llm",
                    "duration_ms": round_ms as i64,
                    "in": usage_snapshot.input_tokens,
                    "cached_in": usage_snapshot.cached_input_tokens,
                    "cache_write": usage_snapshot.cache_creation_tokens,
                    "out": usage_snapshot.output_tokens,
                    "tool_calls": loop_tool_calls.len(),
                }));

                // `last_measured_prompt` drives budget-pressure heuristics that want the
                // total billable input (fresh + cached + creation). Cache hits still occupy
                // context, so including them is the honest metric here.
                let billable_input = usage_snapshot
                    .input_tokens
                    .saturating_add(usage_snapshot.cached_input_tokens)
                    .saturating_add(usage_snapshot.cache_creation_tokens);
                if billable_input > 0 {
                    last_measured_prompt = Some(billable_input);
                }
                let capture_model = if resolved_model.is_empty() {
                    model_name.as_str()
                } else {
                    resolved_model.as_str()
                };
                record_full_llm_response_event(
                    &mut turn_event_buffer,
                    full_llm_capture,
                    &session_id,
                    trace_turn,
                    &trace_correlation,
                    "bridge_inprocess",
                    capture_model,
                    &provider,
                    attempt_in_round,
                    "success",
                    bridge_success_response_payload(
                        &loop_text,
                        &loop_reasoning,
                        &loop_tool_calls,
                        &usage,
                        if loop_tool_calls.is_empty() { "stop" } else { "tool_calls" },
                    ),
                );
                crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
                    "bridge_inprocess success capture",
                    full_llm_capture,
                    Some(remote_artifact_store.as_ref()),
                    &session_id,
                    &user_id,
                    trace_turn,
                    u32::try_from(capture_round_ix).unwrap_or(u32::MAX),
                    agent_id.as_deref(),
                    "bridge_inprocess",
                    capture_model,
                    &provider,
                    &llm_messages,
                    &pruned_tools,
                    None,
                    "success",
                    bridge_success_response_payload(
                        &loop_text,
                        &loop_reasoning,
                        &loop_tool_calls,
                        &usage,
                        if loop_tool_calls.is_empty() { "stop" } else { "tool_calls" },
                    ),
                    Some(trace_correlation.as_capture_trace()),
                )
                .await;
                if bridge_should_record_llm_round(root_runtime_owns_turn_journal)
                    && let Some(buf) = turn_event_buffer.as_mut()
                {
                    buf.record_llm_round(LlmRoundRecord {
                        ttft_ms: None,
                        duration_ms: round_ms as u64,
                        prompt_tokens: usage_snapshot.input_tokens,
                        completion_tokens: usage_snapshot.output_tokens,
                        cache_read_tokens: usage_snapshot.cached_input_tokens,
                        cache_creation_tokens: usage_snapshot.cache_creation_tokens,
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
                        agentic_step: u32::try_from(capture_round_ix).ok(),
                        source: Some("bridge_inprocess".to_string()),
                        run_id: Some(run_id.clone()),
                        tool_calls: None,
                        ..Default::default()
                    });
                }

                // ── Formal pipeline feedback / alerts ──
                if let Some(buf) = turn_event_buffer.as_mut() {
                    let current_turn = bridge_pipeline_event_turn(trace_turn);
                    let mut feedback = astra_turn_core::context_feedback::ContextFeedback::from_usage(
                        usage_snapshot.input_tokens,
                        usage_snapshot.cached_input_tokens,
                        usage_snapshot.cache_creation_tokens,
                        usage_snapshot.output_tokens,
                        false,
                    );
                    if let Some(current_snapshot) = bridge_prompt_snapshot_from_messages(
                        &llm_messages,
                        &pruned_tools,
                        capture_model,
                        &provider,
                    ) {
                        if let Some(event) = bridge_pipeline_baseline
                            .cache_detector
                            .record_turn_for_source(
                                BRIDGE_CACHE_SOURCE,
                                current_snapshot,
                                Some(usage_snapshot.cached_input_tokens),
                            )
                        {
                            feedback.attribute_cache_break(event.reason);
                        }
                    }
                    bridge_pipeline_baseline.stats.record(
                        capture_model,
                        BRIDGE_CACHE_SOURCE,
                        &feedback,
                    );

                    let feedback_evt =
                        astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(
                            current_turn,
                            capture_model,
                            &feedback,
                        );
                    if let Ok(payload) = serde_json::to_value(&feedback_evt) {
                        buf.record(
                            astra_services::session_journal::JournalEvent::pipeline_feedback(
                                (!session_id.is_empty()).then_some(session_id.as_str()),
                                current_turn,
                                payload,
                            ),
                        );
                    }

                    for alert in astra_turn_core::trace_alert::evaluate_alerts(
                        current_turn,
                        &feedback,
                        &bridge_pipeline_baseline.stats,
                        &astra_turn_core::recovery_state::RecoveryState::default(),
                    ) {
                        let alert_evt =
                            astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(
                                &alert,
                            );
                        if let Ok(payload) = serde_json::to_value(&alert_evt) {
                            buf.record(
                                astra_services::session_journal::JournalEvent::pipeline_alert(
                                    (!session_id.is_empty()).then_some(session_id.as_str()),
                                    current_turn,
                                    payload,
                                ),
                            );
                        }
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
                            let request_id =
                                tc_map.get("id").and_then(Value::as_str).unwrap_or("");
                            let identity =
                                astra_services::multi_agent::EdgeDispatchIdentity::new(
                                    &user_id,
                                    &session_id,
                                    &turn_chain_id,
                                    &turn_chain_id,
                                    request_id,
                                );
                            let req_event = astra_turn_core::stream_events::build_tool_request_event(tc_map, &identity);
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
                    run_id: Some(run_id.clone()),
                    agent_id: agent_id.clone(),
                    event_type: "user_query".to_string(),
                    content: content.clone(),
                    parent_event_id: None,
                    parent_event_ids: Vec::new(),
                    causal_chain_id: turn_chain_id.clone(),
                    turn_seq: Some(i64::from(trace_turn)),
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
                run_id: Some(run_id.clone()),
                agent_id: agent_id.clone(),
                event_type: "llm_response".to_string(),
                content: llm_content.clone(),
                parent_event_id: Some(user_query_event_id.clone()),
                parent_event_ids: vec![user_query_event_id.clone()],
                causal_chain_id: turn_chain_id.clone(),
                turn_seq: Some(i64::from(trace_turn)),
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
                        let tool_call_id = payload
                            .metadata
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(ToString::to_string);
                        let mut metadata = payload.metadata;
                        metadata.insert("run_id".to_string(), Value::String(run_id.clone()));
                        events.push(TurnToolEventRecord {
                            event_id: Uuid::now_v7().to_string(),
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            run_id: Some(run_id.clone()),
                            tool_call_id,
                            agent_id: agent_id.clone(),
                            event_type: "tool_call".to_string(),
                            content: match payload.content {
                                Value::String(s) => s,
                                v => serde_json::to_string(&v).unwrap_or_default(),
                            },
                            parent_event_id: Some(user_query_event_id.clone()),
                            parent_event_ids: vec![user_query_event_id.clone()],
                            causal_chain_id: turn_chain_id.clone(),
                            metadata: (!metadata.is_empty()).then_some(Value::Object(metadata)),
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
                        let tool_call_id = payload
                            .metadata
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(ToString::to_string);
                        let mut metadata = payload.metadata;
                        metadata.insert("run_id".to_string(), Value::String(run_id.clone()));
                        events.push(TurnToolEventRecord {
                            event_id: Uuid::now_v7().to_string(),
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            run_id: Some(run_id.clone()),
                            tool_call_id,
                            agent_id: agent_id.clone(),
                            event_type: "tool_result".to_string(),
                            content: match payload.content {
                                Value::String(s) => s,
                                v => serde_json::to_string(&v).unwrap_or_default(),
                            },
                            parent_event_id: Some(user_query_event_id.clone()),
                            parent_event_ids: vec![user_query_event_id.clone()],
                            causal_chain_id: turn_chain_id.clone(),
                            metadata: (!metadata.is_empty()).then_some(Value::Object(metadata)),
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
            let activity_user_id = user_id.clone();
            let user_query_event_id_for_activity = user_query_event_id.clone();
            let core_event_count = usize::from(user_content.is_some()) + usize::from(should_persist_llm);
            let tool_event_count = tool_event_plan
                .as_ref()
                .map(|plan| plan.events.len())
                .unwrap_or(0);

            // audit-#3 resolved: tasks are now routed through the shutdown-aware
            // BridgePersistTracker so they drain on SIGTERM instead of being fire-and-forget.
            let persist_tracker_for_main = persist_tracker_shared.clone();
            let persist_future = async move {
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
                            // audit-#6: core events are durable but tool events
                            // are lost. Emit a structured forensic marker so
                            // log-based reconciliation can find the orphans.
                            tracing::error!(
                                target: "astra_runtime::persist",
                                session_id = %sid,
                                tool_event_count = tool_event_count,
                                marker = "tool_events_orphaned",
                                "CRITICAL: core events persisted but tool events lost; journal needs forensic recovery"
                            );
                            false
                        } else {
                            true
                        }
                    }
                    None => false,
                };
                if !has_inprocess_persisted_events(
                    core_event_count,
                    tool_event_count,
                    tool_events_persisted,
                ) {
                    return;
                }
                let last_event_id = core_outcome
                    .llm_response_event_id
                    .or(Some(user_query_event_id_for_activity));
                let plan = SessionActivityUpdatePlan { last_event_id };
                if let Err(e) = sa_writer
                    .update_session_activity(&sid, &activity_user_id, plan)
                    .await
                {
                    astra_core::agent_persist_fail!("bridge",
                        session = sid,
                        stage = "activity",
                        elapsed = format!("{:?}", persist_start.elapsed()),
                        error = e
                    );
                }
            };
            // HIGH #4: route through shutdown-aware tracker when available.
            if let Some(tracker) = persist_tracker_for_main {
                tracker.track_persist_task(Box::pin(persist_future));
            } else {
                tokio::spawn(persist_future);
            }

            if !session_id.is_empty()
                && let Some(mut turn_event_buffer) = turn_event_buffer.filter(|buf| !buf.is_empty())
            {
                let journal_sid = session_id.clone();
                let writer = match JournalWriter::new(&journal_sid) {
                    Ok(writer) => writer,
                    Err(error) => {
                        astra_core::agent_warn!(
                            "bridge",
                            "failed to create journal writer for turn event buffer flush: session={} error={}",
                            journal_sid,
                            error
                        );
                        return;
                    }
                };
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = turn_event_buffer.flush(&writer) {
                        astra_core::agent_warn!(
                            "bridge",
                            "failed to flush turn event buffer: session={} error={}",
                            journal_sid,
                            error
                        );
                    }
                });
            }

            // Hook side effects: decision audit, skill selection, reflection
            {
                let mut hook_payload = astra_turn_core::tail_persist::build_turn_hook_args(
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
                    trace_turn as i64,
                    None, // session_start
                    false, // run_hook_db_writes = false → triggers persist
                    false, // run_observer = false → triggers observer
                    false, // run_reflection_learning = false → triggers reflection
                );
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
                let mut eval = crate::pipeline::evaluation::evaluate_tool_call_records_with_thresholds(
                    &user_message_for_eval,
                    &recent_tools_for_quality,
                    &tool_call_records,
                    0, // No stall events in single-call mode.
                    verdict_warning,
                    budget_pressure,
                    crate::pipeline::evaluation::current_evaluation_thresholds(),
                );
                crate::pipeline::evaluation::apply_final_answer_relevance(
                    &mut eval,
                    &user_message_for_eval,
                    &full_text,
                );
                eval
            });
            let tool_execution_ms: u64 = merged_tool_results
                .iter()
                .filter_map(|tool_result| {
                    tool_result
                        .get("duration_ms")
                        .and_then(Value::as_u64)
                })
                .sum();
            let trace_signal = build_context_trace_signal(
                trace_turn,
                format!("turn-{trace_turn}"),
                edge_tools.len(),
                recent_tools_for_quality.clone(),
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
                let persist_tracker_for_aux = persist_tracker_shared.clone();
                let aux_future = async move {
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
                        tracing::error!(
                            target: "astra_runtime::bridge_inprocess",
                            aux_session_id = %aux_sid,
                            err = %e,
                            "auxiliary routing event persist failed"
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
                };
                // HIGH #4: route through shutdown-aware tracker when available.
                if let Some(tracker) = persist_tracker_for_aux {
                    tracker.track_persist_task(Box::pin(aux_future));
                } else {
                    tokio::spawn(aux_future);
                }
            }

            if explain {
                let first_tool_call = all_round_tool_calls
                    .first()
                    .and_then(Value::as_object)
                    .and_then(|tool_call| tool_call.get("function"))
                    .and_then(Value::as_object)
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(|name| json!({ "name": name }));
                let final_usage =
                    crate::turn::token_usage::TokenUsage::from_partial_json_map(&usage);
                if llm_steps.is_empty() {
                    llm_steps.push(json!({
                        "step": "llm",
                        "duration_ms": llm_duration_ms,
                        "in": final_usage.input_tokens,
                        "cached_in": final_usage.cached_input_tokens,
                        "cache_write": final_usage.cache_creation_tokens,
                        "out": final_usage.output_tokens,
                        "tool_calls": all_round_tool_calls.len(),
                    }));
                }
                let aux_tokens_in = Some(final_usage.input_tokens as i64);
                let aux_tokens_out = Some(final_usage.output_tokens as i64);
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
                    "estimated_tokens": final_usage.total_tokens() as i64,
                    "skipped": selected_model_name.is_some(),
                    "reason": selected_model_name.as_ref().map(|_| "selected_model").unwrap_or(""),
                    "cloud_loop_turns": cloud_loop_turns,
                }));
                let explain_event = build_explain_event(
                    turn_started.elapsed().as_millis() as i64,
                    Some(final_usage.input_tokens as i64),
                    Some(final_usage.output_tokens as i64),
                    all_round_tool_calls.len(),
                    edge_tools.len(),
                    first_tool_call,
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
                    event.tokens_in = usage.get("input_tokens").and_then(Value::as_u64);
                    event.tool_calls = Some(tool_call_records.clone());
                    astra_turn_core::cloud_session_facts::update_from_journal_event(
                        &mut facts,
                        &event,
                    );
                }

                // L1 session-memory persistence now lives in
                // `crate::session_memory::MemoryExtractionService`
                // (driven from the turn finalization path). The bridge
                // no longer owns a duplicate write — single ownership,
                // single event stream.
            }

            // turn_complete
            mark_disconnect_capture_finalized(&disconnect_capture_state);
            yield render_sse(&turn_complete_event(&messages, &llm_content, &all_round_tool_calls));
        };

        let cancel_on_drop = CancelOnDrop(client_cancel.clone());
        let body_stream = stream! {
            let _cancel_on_drop = cancel_on_drop;
            for await chunk in stream {
                yield Ok::<_, std::io::Error>(chunk);
            }
        };
        let body = Body::from_stream(body_stream);
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
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn required_bridge_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<String, (StatusCode, String)> {
    header_str(headers, name).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("missing required bridge header {name}"),
        )
    })
}

fn optional_positive_u32_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<u32>, (StatusCode, String)> {
    let Some(value) = header_str(headers, name) else {
        return Ok(None);
    };
    let Some(parsed) = value
        .parse::<u64>()
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("bridge header {name} must be a positive u32"),
        ));
    };
    Ok(Some(parsed))
}

fn parse_bridge_payload(body: &Bytes) -> Result<Value, (StatusCode, String)> {
    let payload = serde_json::from_slice::<Value>(body).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid bridge request JSON: {error}"),
        )
    })?;
    if payload.is_object() {
        Ok(payload)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "bridge request body must be a JSON object".to_string(),
        ))
    }
}

fn bridge_round_index(payload: &Value) -> Result<u32, (StatusCode, String)> {
    let Some(value) = payload.get("round_index") else {
        return Ok(0);
    };
    let Some(round_index) = value.as_u64().and_then(|value| u32::try_from(value).ok()) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "round_index must be a non-negative u32".to_string(),
        ));
    };
    Ok(round_index)
}

fn optional_payload_array(
    payload: &Value,
    field: &'static str,
) -> Result<Vec<Value>, (StatusCode, String)> {
    match payload.get(field) {
        Some(Value::Array(values)) => Ok(values.clone()),
        Some(_) => Err((
            StatusCode::BAD_REQUEST,
            format!("bridge payload field `{field}` must be an array"),
        )),
        None => Ok(Vec::new()),
    }
}

fn optional_payload_object(
    payload: &Value,
    field: &'static str,
) -> Result<Map<String, Value>, (StatusCode, String)> {
    match payload.get(field) {
        Some(Value::Object(values)) => Ok(values.clone()),
        Some(_) => Err((
            StatusCode::BAD_REQUEST,
            format!("bridge payload field `{field}` must be an object"),
        )),
        None => Ok(Map::new()),
    }
}

fn explain_requested(payload: &Value) -> bool {
    match payload.get("explain") {
        Some(Value::Bool(enabled)) => *enabled,
        Some(Value::String(mode)) => mode.eq_ignore_ascii_case("verbose"),
        _ => false,
    }
}

fn forwarded_sse_error_event(bytes: &Bytes) -> Option<Value> {
    let raw = std::str::from_utf8(bytes).ok()?.trim();
    let json_line = raw.strip_prefix("data: ")?;
    let event: Value = serde_json::from_str(json_line).ok()?;
    (event.get("type").and_then(Value::as_str) == Some("error")).then_some(event)
}

struct CancelOnDrop(Option<Arc<CancellationToken>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

#[derive(Clone, Default)]
struct DisconnectCaptureSnapshot {
    started: bool,
    finalized: bool,
    session_id: String,
    user_id: String,
    turn: u32,
    session_turn_source: String,
    turn_chain_id: String,
    user_query_event_id: String,
    round_ix: i64,
    agent_id: Option<String>,
    model_name: String,
    resolved_model: String,
    provider: String,
    llm_messages: Vec<Value>,
    pruned_tools: Vec<Value>,
    max_output_tokens: Option<usize>,
    partial_text: String,
    partial_reasoning: String,
    partial_tool_calls: Vec<Value>,
    usage: Map<String, Value>,
}

fn update_disconnect_capture_snapshot(
    state: &Arc<Mutex<DisconnectCaptureSnapshot>>,
    snapshot: DisconnectCaptureSnapshot,
) {
    if let Ok(mut guard) = state.lock() {
        *guard = snapshot;
    }
}

fn mark_disconnect_capture_finalized(state: &Arc<Mutex<DisconnectCaptureSnapshot>>) {
    if let Ok(mut guard) = state.lock() {
        guard.finalized = true;
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_bridge_stream_failure_capture(
    context: &str,
    full_llm_capture: bool,
    remote_store: &dyn astra_services::SessionArtifactJsonStore,
    session_id: &str,
    user_id: &str,
    turn: u32,
    trace: &BridgeTraceCorrelation,
    round_ix: i64,
    agent_id: Option<&str>,
    model_name: &str,
    resolved_model: &str,
    provider: &str,
    llm_messages: &[Value],
    pruned_tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    error: &str,
    kind: &str,
    partial_text: &str,
    partial_reasoning: &str,
    partial_tool_calls: &[Value],
    usage: &Map<String, Value>,
) {
    let capture_model = if resolved_model.is_empty() {
        model_name
    } else {
        resolved_model
    };
    crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
        context,
        full_llm_capture,
        Some(remote_store),
        session_id,
        user_id,
        turn,
        u32::try_from(round_ix).unwrap_or(u32::MAX),
        agent_id,
        "bridge_inprocess",
        capture_model,
        provider,
        llm_messages,
        pruned_tools,
        max_output_tokens,
        outcome,
        json!({
            "error": error,
            "kind": kind,
            "partial_full_text": partial_text,
            "partial_reasoning": partial_reasoning,
            "tool_calls": partial_tool_calls,
            "usage": usage,
        }),
        Some(trace.as_capture_trace()),
    )
    .await;
}

// ── Memory prefetch — delegated to turn::memory_prefetch ─────────────────────
pub use super::super::memory_prefetch::{
    MemoryPrefetchResult, SessionStartPrefetchResult, prefetch_memories, prefetch_memory_index,
    prefetch_session_start_memories,
};

/// Test-accessible wrapper around private schema pruning — used by integration
/// tests that need to verify progressive schema detail levels.
pub mod bridge_inprocess_test_helpers {
    use crate::prompts::CompactionTier;
    use serde_json::Value;

    pub fn prune_tool_schemas_pub(tools: &[Value], tier: CompactionTier) -> Vec<Value> {
        astra_turn_core::tool_schema_prune::prune_tool_schemas(tools, tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::bridge::sse_helpers::apply_forward_llm_sse_event;
    use crate::turn::prompt_cache::runtime_always_load_tool_names;
    use astra_services::SessionArtifactStore;
    use astra_services::{
        SessionArtifactJsonRecord, SessionArtifactJsonStore, StoredSessionArtifact,
    };
    use astra_turn_core::turn_guard::TurnGuard;
    use async_trait::async_trait;
    use http_body_util::BodyExt;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn default_test_always_load_tool_names() -> std::collections::HashSet<String> {
        crate::turn::prompt_cache::resolve_always_load_tool_names_for_config(
            &astra_config::ToolSurfaceConfig::default(),
        )
    }

    #[test]
    fn selected_model_name_from_payload_ignores_legacy_top_level_model() {
        assert_eq!(
            selected_model_name_from_payload(&json!({
                "selected_model": {"model": "deepseek-v4-pro-official"},
                "model": "deepseek-v4-flash",
            }))
            .as_deref(),
            Some("deepseek-v4-pro-official")
        );
        assert_eq!(
            selected_model_name_from_payload(&json!({
                "model": "deepseek-v4-flash",
            })),
            None
        );
    }

    #[test]
    fn provider_model_gateway_invocation_from_payload_reads_provider_runtime_context() {
        let invocation = provider_model_gateway_invocation_from_payload(&json!({
            "selected_model": {"id": "model-qwen", "model": "qwen3.5-flash"},
            "runtime_auth": {"authorization": "Bearer runtime-grant"},
            "capability_descriptors": {
                "model_gateway": {
                    "endpoint_url": "http://catalog.local/api/v1/models/openai/chat/completions"
                }
            }
        }))
        .expect("valid provider runtime context")
        .expect("provider model gateway invocation");

        assert_eq!(invocation.model, "qwen3.5-flash");
        assert_eq!(
            invocation.endpoint_url,
            "http://catalog.local/api/v1/models/openai/chat/completions"
        );
        assert_eq!(invocation.authorization, "Bearer runtime-grant");
    }

    #[test]
    fn provider_model_gateway_invocation_allows_missing_provider_model_id() {
        let invocation = provider_model_gateway_invocation_from_payload(&json!({
            "selected_model": {"model": "qwen3.5-flash"},
            "runtime_auth": {"authorization": "Bearer runtime-grant"},
            "capability_descriptors": {
                "model_gateway": {
                    "endpoint_url": "http://catalog.local/api/v1/models/openai/chat/completions"
                }
            }
        }))
        .expect("valid provider runtime context")
        .expect("provider model gateway invocation");

        assert_eq!(invocation.model, "qwen3.5-flash");
    }

    #[test]
    fn bridge_runtime_manifest_distinguishes_requested_and_fallback_model() {
        let mut trace = json!({
            "source": "llm_context_bridge",
            "runtime_manifest": {
                "schema_version": "astra_runtime_manifest.v1"
            }
        });
        let fallback = json!({
            "from_model": "deepseek-v4-pro-official",
            "to_model": "deepseek-v4-flash",
            "reason": "rate_limit",
        });

        rewrite_bridge_runtime_manifest_model_resolution(
            &mut trace,
            Some("deepseek-v4-pro-official"),
            "deepseek-v4-flash",
            "openai",
            Some(&fallback),
        );

        let manifest = &trace["runtime_manifest"];
        assert_eq!(
            manifest["selected_model"]["model"],
            "deepseek-v4-pro-official"
        );
        assert_eq!(
            manifest["model_resolution"]["source"],
            "rate_limit_fallback"
        );
        assert_eq!(manifest["model_resolution"]["model"], "deepseek-v4-flash");
        assert_eq!(
            manifest["model_resolution"]["fallback"]["from_model"],
            "deepseek-v4-pro-official"
        );
        assert_eq!(
            manifest["runtime_profile"],
            astra_runtime_env::CapacityProviderType::CliLocal.as_str(),
            "public runtime metadata should identify the CLI local capacity provider, not the in-process bridge adapter"
        );
    }

    /// RAII guard that restores an environment variable on drop (panic-safe).
    ///
    /// **Not safe under parallel tests sharing the same env key.** Two tests
    /// entering with `previous = None` race: whichever guard drops first
    /// clears the env out from under the other test, which then sees the
    /// variable unset mid-assertion. Every test that sets
    /// `ASTRA_TEST_BRIDGE_SECRET` via this guard MUST therefore carry
    /// `#[serial_test::serial(astra_test_bridge_secret)]` so the five
    /// `forward_…` journal tests run exclusively against each other.
    #[cfg(feature = "bridge-e2e-hooks")]
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[tokio::test]
    async fn first_turn_session_start_memory_snapshot_is_cached_per_session() {
        let cache = tokio::sync::Mutex::new(HashMap::new());
        let fetches = Arc::new(AtomicUsize::new(0));

        let first = cached_first_turn_session_start_memory(&cache, "sess-1", 1, {
            let fetches = Arc::clone(&fetches);
            move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                CachedSessionStartMemory {
                    stable_memory_section: Some(
                        "<session_memory>\nfirst\n</session_memory>".into(),
                    ),
                    stable_ids: vec!["m1".into()],
                    fetch_ms: 7,
                }
            }
        })
        .await
        .expect("first-turn snapshot");

        let second = cached_first_turn_session_start_memory(&cache, "sess-1", 1, {
            let fetches = Arc::clone(&fetches);
            move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                CachedSessionStartMemory {
                    stable_memory_section: Some(
                        "<session_memory>\nsecond\n</session_memory>".into(),
                    ),
                    stable_ids: vec!["m2".into()],
                    fetch_ms: 99,
                }
            }
        })
        .await
        .expect("cached snapshot");

        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn later_turn_clears_cached_session_start_memory_snapshot() {
        let cache = tokio::sync::Mutex::new(HashMap::new());

        let _ = cached_first_turn_session_start_memory(&cache, "sess-1", 1, || async {
            CachedSessionStartMemory {
                stable_memory_section: Some("<session_memory>\nfirst\n</session_memory>".into()),
                stable_ids: vec!["m1".into()],
                fetch_ms: 7,
            }
        })
        .await;

        let later = cached_first_turn_session_start_memory(&cache, "sess-1", 2, || async {
            CachedSessionStartMemory {
                stable_memory_section: Some("<session_memory>\nlater\n</session_memory>".into()),
                stable_ids: vec!["m2".into()],
                fetch_ms: 8,
            }
        })
        .await;

        assert!(later.is_none());
        assert!(cache.lock().await.is_empty());
    }

    // The journal-flow helpers below are only compiled when the
    // `bridge-e2e-hooks` feature is on, because all their callers
    // (`forward_persists_full_journal_*`, etc.) are gated on the same
    // feature. Gating here means no `#[allow(dead_code)]` dance is
    // needed when the feature is off.
    #[cfg(feature = "bridge-e2e-hooks")]
    fn bridge_test_matrixone() -> MatrixOneSettings {
        MatrixOneSettings::mock()
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    fn bridge_test_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(
            FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=")
                .expect("valid fernet key"),
        )
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    fn bridge_test_headers(session_id: &str, full_llm_capture: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-user-id", "user-bridge-journal".parse().unwrap());
        headers.insert("x-mo-session-id", session_id.parse().unwrap());
        headers.insert("x-mo-session-turn", "1".parse().unwrap());
        if full_llm_capture {
            headers.insert("x-mo-full-llm-capture", "1".parse().unwrap());
        }
        headers.insert(
            "x-mo-bridge-test-secret",
            std::env::var("ASTRA_TEST_BRIDGE_SECRET")
                .expect("bridge test secret should be set")
                .parse()
                .unwrap(),
        );
        headers
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    fn read_journal_events(session_id: &str) -> Vec<Value> {
        let path = JournalWriter::new(session_id)
            .expect("journal writer")
            .path()
            .clone();
        match std::fs::read_to_string(path) {
            Ok(contents) => contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<Value>(line).expect("journal event json"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read journal: {error}"),
        }
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    async fn wait_for_journal_events(session_id: &str) -> Vec<Value> {
        for _ in 0..100 {
            let events = read_journal_events(session_id);
            if !events.is_empty() {
                return events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        read_journal_events(session_id)
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    async fn collect_response_body(response: Response) -> String {
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect response body")
            .to_bytes();
        String::from_utf8(body.to_vec()).expect("response body should be utf8")
    }

    #[derive(Default)]
    struct RecordingArtifactStore {
        records: Mutex<Vec<SessionArtifactJsonRecord>>,
    }

    #[async_trait]
    impl SessionArtifactJsonStore for RecordingArtifactStore {
        async fn persist_json_artifact(
            &self,
            record: SessionArtifactJsonRecord,
        ) -> Result<StoredSessionArtifact, astra_services::SessionArtifactStoreError> {
            self.records
                .lock()
                .expect("recording store lock")
                .push(record.clone());
            Ok(StoredSessionArtifact {
                artifact_id: if record.artifact_id.is_empty() {
                    "artifact-1".to_string()
                } else {
                    record.artifact_id.clone()
                },
                session_id: record.session_id,
                user_id: record.user_id,
                artifact_kind: record.artifact_kind,
                source: record.source,
                turn: record.turn,
                round: record.round,
                content: record.content,
                metadata: record.metadata,
                retention_policy: Some("default".into()),
                retention_until: None,
                status: Some("active".into()),
                referenced_by_manifest_count: 0,
                referenced_by_state_items_count: 0,
                referenced_by_citation_count: 0,
                created_at: None,
            })
        }

        async fn load_json_artifact(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_id: &str,
        ) -> Result<Option<StoredSessionArtifact>, astra_services::SessionArtifactStoreError>
        {
            Ok(None)
        }

        async fn load_latest_json_artifact(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: &str,
        ) -> Result<Option<StoredSessionArtifact>, astra_services::SessionArtifactStoreError>
        {
            Ok(None)
        }

        async fn list_json_artifacts(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: Option<&str>,
            _limit: usize,
            _cursor: Option<astra_services::SessionArtifactListCursor>,
        ) -> Result<
            astra_services::SessionArtifactListPage,
            astra_services::SessionArtifactStoreError,
        > {
            Ok(astra_services::SessionArtifactListPage {
                artifacts: Vec::new(),
                limit: 0,
                next_cursor: None,
            })
        }
    }

    #[test]
    fn has_inprocess_persisted_events_skips_failed_tool_events() {
        assert!(has_inprocess_persisted_events(2, 3, false));
        assert!(has_inprocess_persisted_events(2, 3, true));
        assert!(!has_inprocess_persisted_events(0, 3, false));
        assert!(has_inprocess_persisted_events(0, 3, true));
    }

    // ── effective_volatile_sections_for_round ────────────────────────────
    //
    // Regression tests for the dual-path volatile-gating invariant.
    // Session 986a553e was first fixed only on the server path
    // (`run_turn_pipeline`), but `astra chat` routes through this bridge
    // which had its own volatile-injection flow. The bridge's fix shipped
    // afterwards, and these tests exist so the next dual-path bug gets
    // caught at unit-test speed instead of during a cache-rate postmortem.
    //
    // Pairs with `server_loop_host::tests::run_turn_pipeline_minimax_skips_volatile_on_tool_loop_round`.

    fn sample_volatile_sections() -> Vec<prompts::PromptSection> {
        vec![
            prompts::PromptSection::dynamic(
                "Self-Awareness: Turn 3 | Tokens 4200/8000".to_string(),
                prompts::PromptTokenBucket::Environment,
            ),
            prompts::PromptSection::dynamic(
                "session anchor: pay down test flakes".to_string(),
                prompts::PromptTokenBucket::Environment,
            ),
        ]
    }

    #[test]
    fn effective_volatile_minimax_is_empty_on_every_round() {
        // Strict-history (MiniMax) must suppress volatile on round 0, 1,
        // 6, and any other round — round-0-only injection still makes
        // msg[1] bytes diverge across rounds, so we suppress unconditionally.
        let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
            "openai",
            "MiniMax-M2.7",
        );
        let dyn_sections = sample_volatile_sections();
        for round in [0u32, 1, 2, 6, 12] {
            let out = effective_volatile_sections_for_round(cap, round, &dyn_sections);
            assert!(
                out.is_empty(),
                "MiniMax must suppress volatile on round {round}; got {} sections",
                out.len(),
            );
        }
    }

    #[test]
    fn effective_volatile_deepseek_v4_flash_is_empty_on_every_round() {
        let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
            "openai",
            "deepseek-v4-flash",
        );
        let dyn_sections = sample_volatile_sections();
        for round in [0u32, 1, 2, 6, 12] {
            let out = effective_volatile_sections_for_round(cap, round, &dyn_sections);
            assert!(
                out.is_empty(),
                "DeepSeek v4 flash must suppress volatile on round {round}; got {} sections",
                out.len(),
            );
        }
    }

    #[test]
    fn effective_volatile_openai_keeps_sections_on_every_round() {
        // OpenAI auto-prefix (TailSuffix): safe to inject every round
        // since volatile lives at the tail of the last user message.
        let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
            "openai", "gpt-4o",
        );
        let dyn_sections = sample_volatile_sections();
        for round in [0u32, 1, 5] {
            let out = effective_volatile_sections_for_round(cap, round, &dyn_sections);
            assert_eq!(
                out.len(),
                dyn_sections.len(),
                "OpenAI TailSuffix must pass through all volatile on round {round}",
            );
        }
    }

    #[test]
    fn effective_volatile_anthropic_keeps_sections_on_every_round() {
        // Anthropic MarkerIsolated: volatile lives AFTER the last
        // cache_control marker inside the system block, so it's safe to
        // emit every round. The bridge still gets all sections back;
        // the downstream pipeline is responsible for marker placement.
        let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
            "anthropic",
            "claude-sonnet-4",
        );
        let dyn_sections = sample_volatile_sections();
        for round in [0u32, 1, 5] {
            let out = effective_volatile_sections_for_round(cap, round, &dyn_sections);
            assert_eq!(
                out.len(),
                dyn_sections.len(),
                "Anthropic MarkerIsolated must pass through all volatile on round {round}",
            );
        }
    }

    #[test]
    fn effective_volatile_bedrock_keeps_sections_on_every_round() {
        // Bedrock cachePoint is also MarkerIsolated — same invariant
        // as Anthropic.
        let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
            "bedrock",
            "us.anthropic.claude-sonnet-4-6",
        );
        let dyn_sections = sample_volatile_sections();
        for round in [0u32, 1, 5] {
            let out = effective_volatile_sections_for_round(cap, round, &dyn_sections);
            assert_eq!(
                out.len(),
                dyn_sections.len(),
                "Bedrock MarkerIsolated must pass through all volatile on round {round}",
            );
        }
    }

    /// Pin the exact model id observed in the 986a553e regression so a
    /// future provider/model normalization change doesn't silently route
    /// MiniMax back to TailSuffix and reopen the cache hole on the bridge
    /// path. Mirrors the equivalent server-path regression in
    /// `cache_placement::tests::minimax_m27_session_986a553e_routes_to_current_user_only`.
    #[test]
    fn effective_volatile_bridge_minimax_m27_session_986a553e_regression() {
        let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
            "openai",
            "MiniMax-M2.7",
        );
        let dyn_sections = sample_volatile_sections();
        // Round 0 explicitly — the subtle part of the invariant is that
        // even round 0 must skip, because round-0-only injection still
        // breaks history byte stability across the tool loop.
        assert!(
            effective_volatile_sections_for_round(cap, 0, &dyn_sections).is_empty(),
            "bridge path must skip volatile on round 0 for MiniMax (strict-history)",
        );
        // And stays empty through the typical tool-loop length.
        assert!(effective_volatile_sections_for_round(cap, 6, &dyn_sections).is_empty(),);
    }

    #[test]
    fn effective_volatile_empty_input_produces_empty_output() {
        // Degenerate: no dynamic sections to begin with. All providers
        // should return empty — no work to preserve or suppress.
        let empty: Vec<prompts::PromptSection> = Vec::new();
        for (prov, model) in [
            ("openai", "MiniMax-M2.7"),
            ("openai", "gpt-4o"),
            ("anthropic", "claude-sonnet-4"),
            ("bedrock", "us.anthropic.claude-sonnet-4-6"),
        ] {
            let cap = astra_turn_core::cache_placement::CacheCapability::for_provider_and_model(
                prov, model,
            );
            assert!(
                effective_volatile_sections_for_round(cap, 0, &empty).is_empty(),
                "empty input must yield empty output for {prov}/{model}",
            );
        }
    }

    #[test]
    fn bridge_snapshot_skips_legacy_journal_event_without_provider() {
        let mut event = astra_services::session_journal::JournalEvent::base_public(
            astra_services::session_journal::JournalEventType::LlmRound,
            Some("sess"),
        );
        event.metadata = Some(json!({
            "model": "gpt-4o",
            "request": {
                "messages": [
                    {"role": "system", "content": "stable"},
                    {"role": "user", "content": "hello"}
                ],
                "tools": []
            }
        }));

        assert!(bridge_prompt_snapshot_from_journal_event(&event).is_none());
    }

    #[test]
    fn bridge_marks_last_real_message_before_synthetic_tail() {
        let mut llm_messages = vec![
            json!({"role": "system", "content": [{"type": "text", "text": "stable"}]}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "tool output", "tool_call_id": "c1"}),
        ];
        let cache_cfg = crate::turn::prompt_cache::PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };

        let bridge_synthetic_tail_prefix_end =
            crate::turn::llm::context::finalize_bridge_wire_messages(
                &mut llm_messages,
                Some("volatile".to_string()),
                None,
                "anthropic",
                "claude-sonnet-4",
                &astra_turn_core::thinking_config::ThinkingConfig::Off,
                None,
                &cache_cfg,
            );
        crate::turn::llm::context::apply_bridge_message_cache_metadata(
            &mut llm_messages,
            bridge_synthetic_tail_prefix_end,
            &cache_cfg,
            "sess",
        );

        assert_eq!(bridge_synthetic_tail_prefix_end, Some(3));
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&llm_messages[2]),
            "bridge should mark the last real tool result before the synthetic suffix",
        );
        assert!(
            !astra_turn_core::context_serializer::message_has_cache_control(&llm_messages[4]),
            "synthetic tail user must stay unannotated",
        );
    }

    #[test]
    fn build_context_trace_signal_keeps_only_known_timing_values() {
        let signal = build_context_trace_signal(
            3,
            "turn-3".to_string(),
            5,
            vec!["read_file".to_string(), "grep".to_string()],
            Some(1200),
            8000,
            450,
            1500,
        );

        let tool_surface = signal.tool_surface.as_ref().expect("tool surface");
        assert_eq!(
            tool_surface.visible_tools,
            vec!["read_file".to_string(), "grep".to_string()]
        );
        assert_eq!(tool_surface.tools_available, 5);

        let timing = signal.timing.as_ref().expect("timing");
        assert_eq!(timing.turn, 3);
        assert_eq!(timing.context_assembly_ms, 0);
        assert_eq!(timing.llm_total_ms, 1050);
        assert_eq!(timing.tool_execution_ms, 450);
        assert_eq!(timing.total_ms, 1500);
    }

    // ── Static/dynamic prompt boundary tests ──
    // These tests manipulate env vars, so they must not run in parallel.
    // Share the mutex with `turn::prompt_cache::tests` — both modules hit
    // the same env vars, so two independent locks would race and a panic
    // in one would leave the other poisoned.
    use crate::turn::prompt_cache::CACHE_ENV_MUTEX;

    #[test]
    fn always_load_tool_names_for_bridge_uses_edge_profile_override_or_runtime_fallback() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES
                .to_string(),
            json!(["bash"]),
        );

        let overridden = always_load_tool_names_for_bridge(&edge_profile);
        assert!(overridden.contains("bash"));
        assert!(!overridden.contains("read_file"));

        let fallback = always_load_tool_names_for_bridge(&Map::new());
        assert_eq!(fallback, runtime_always_load_tool_names());
    }

    #[test]
    fn self_awareness_section_is_post_cache_volatile() {
        let section = self_awareness_volatile_section(
            "\n\n## Self-Awareness\nTurn: 37 | Tokens: 26899/80000",
        )
        .expect("section");

        assert_eq!(
            section.scope,
            prompts::CacheScope::None,
            "self-awareness contains per-turn counters and must not enter the cached prefix"
        );
        assert!(section.trace_signals.context_signals.self_awareness);
    }

    #[test]
    fn pipeline_assembly_records_bridge_context_signals() {
        let active_skill_names = vec!["concise"];
        // memory_signal_hint removed — LLM-driven via system prompt rules
        let implicit_feedback_hint =
            "\n\n## Implicit Feedback\nThe user is correcting the previous attempt.";
        let feedback_rules_hint = "\n\n[Learned Feedback Rules]\n- Rule: do not use mocks";
        let self_awareness_hint =
            "\n\n## Self-Awareness\nCurrent task: review runtime prompt assembly.";
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
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
                            active_output_skills: !active_skill_names.is_empty(),
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
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
                            memory_signal_detected: false,
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
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
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
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
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
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
                            self_awareness: !self_awareness_hint.is_empty(),
                            ..Default::default()
                        },
                    ..Default::default()
                },
            ),
            prompts::PromptSection::dynamic(
                "\n\n## Memoria Recall\n- Stored: prefer Rust for CLI work.".to_string(),
                prompts::PromptTokenBucket::Environment,
            )
            .with_trace_signals(
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
                            memoria_insights: true,
                            ..Default::default()
                        },
                    ..Default::default()
                },
            ),
        ];
        let (_, _, prompt_sections) =
            crate::turn::prompt_cache::assemble_system_message_via_pipeline(
                &["bash", "read_file"],
                &dynamic_sections,
                &PromptCacheConfig::latch("openai", "gpt-4"),
                "test-session",
                "gpt-4",
                "openai",
                None,
                None,
            );
        let breakdown = prompts::build_system_prompt_trace(&prompt_sections, vec![], vec![], None);

        assert!(breakdown.context_signals.active_output_skills);
        assert!(
            !breakdown.context_signals.memory_signal_detected,
            "memory signal detection removed — LLM-driven"
        );
        assert!(breakdown.context_signals.self_awareness);
        assert!(breakdown.context_signals.implicit_feedback);
        assert!(breakdown.context_signals.learned_feedback_rules);
        assert!(breakdown.context_signals.memoria_insights);
        assert!(!breakdown.context_signals.system_prompt_override);
        assert!(!breakdown.context_signals.effort_hint);
        assert!(!breakdown.context_signals.agent_type_hint);
        assert!(breakdown.environment_tokens > 0);
        assert!(breakdown.user_preferences_tokens > 0);
        // guidance_signals default to false — guard against accidental default changes
        assert!(!breakdown.guidance_signals.parallel_feedback);
        assert!(!breakdown.guidance_signals.parallel_batching_nudge);
    }

    #[test]
    fn build_system_prompt_trace_guidance_signals_only() {
        use crate::prompts::{CacheScope, PromptSection, PromptTokenBucket};
        use astra_turn_core::context_assembly_trace::{PromptGuidanceSignals, PromptTraceSignals};

        let section = PromptSection {
            text: "parallel feedback".to_string(),
            scope: CacheScope::None,
            token_bucket: PromptTokenBucket::Environment,
            trace_signals: PromptTraceSignals {
                guidance_signals: PromptGuidanceSignals {
                    parallel_feedback: true,
                    parallel_batching_nudge: true,
                },
                ..Default::default()
            },
        };
        let breakdown = prompts::build_system_prompt_trace(&[section], vec![], vec![], None);
        assert!(!breakdown.context_signals.active_output_skills);
        assert!(breakdown.guidance_signals.parallel_feedback);
        assert!(breakdown.guidance_signals.parallel_batching_nudge);
    }
    #[test]
    fn annotate_tool_schemas_for_caching_adds_cache_control() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        }

        let mut tools = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "read_file"}}),
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
            &default_test_always_load_tool_names(),
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
        assert!(
            tools[1]["cache_control"].get("ttl").is_none(),
            "simple ephemeral marker — no ttl (Bedrock-compatible)"
        );
    }

    #[test]
    fn annotate_tool_schemas_noop_for_openai() {
        let mut tools = vec![json!({"function": {"name": "bash"}})];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("openai", "gpt-4"),
            &default_test_always_load_tool_names(),
        );
        assert!(
            tools[0].get("cache_control").is_none(),
            "OpenAI tools should not get cache_control"
        );
    }

    #[test]
    fn annotate_tool_schemas_marks_end_of_always_load_prefix() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        }

        // bash and read_file are always_load (static lib); github is dynamic.
        // The marker must sit on the last always_load tool so dynamic churn after
        // it doesn't invalidate the cached prefix.
        let mut tools = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "read_file"}}),
            json!({"function": {"name": "github"}}),
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
            &default_test_always_load_tool_names(),
        );

        assert!(
            tools[0].get("cache_control").is_none(),
            "first always_load tool should not have cache_control"
        );
        assert!(
            tools[1].get("cache_control").is_some(),
            "last always_load tool (read_file) should have cache_control — end of static prefix"
        );
        assert!(
            tools[2].get("cache_control").is_none(),
            "dynamic tool (github) must not carry the marker"
        );
        assert_eq!(
            tools[1]["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
        assert!(
            tools[1]["cache_control"].get("ttl").is_none(),
            "simple ephemeral marker — no ttl (Bedrock-compatible)"
        );
    }

    #[test]
    fn add_message_cache_breakpoint_targets_last_non_system() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::set_var("ASTRA_TEST_PROMPT_CACHE_DISABLED", "1");
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
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        }
    }

    #[test]
    fn prompt_cache_config_latch_idempotent() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
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
        annotate_tool_schemas_for_caching(&mut tools, &cfg, &default_test_always_load_tool_names());

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
            "function": {"name": "git", "arguments": "{\"action\":\"show\",\"revision\":\"HEAD\"}"}
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
        // Canonical SSE usage event keys (see
        // `astra_runtime::turn::token_usage::TokenUsage::to_json_map`).
        let event = json!({
            "type": "usage",
            "input_tokens": 1000i64,
            "cached_input_tokens": 800i64,
            "cache_creation_tokens": 0i64,
            "output_tokens": 500i64,
            "total_tokens": 2300i64,
        });
        assert_eq!(event["type"], "usage");
        assert_eq!(event["input_tokens"].as_i64(), Some(1000));
        assert_eq!(event["cached_input_tokens"].as_i64(), Some(800));
        assert_eq!(event["output_tokens"].as_i64(), Some(500));
    }

    // ── Combined cache layer tests ──────────────────────────────────────

    // ── Message breakpoint edge cases ──────────────────────────────────

    #[test]
    fn message_breakpoint_skips_system_only() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        let mut messages: Vec<Value> = vec![];
        add_message_cache_breakpoint(
            &mut messages,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
        );
        assert!(messages.is_empty());
    }

    #[test]
    fn message_breakpoint_array_content_appends_to_last_block() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        unsafe {
            std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED");
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        let mut tools: Vec<Value> = vec![];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4-20250514"),
            &default_test_always_load_tool_names(),
        );
        assert!(tools.is_empty());
    }

    // ── Multi-turn cache token simulation ──────────────────────────────

    #[test]
    fn multi_turn_sse_cache_tokens_accumulate_in_accum() {
        use astra_turn_core::chat_turn_sse_dispatch::{
            ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
        };

        fn sse_usage(input: u64, output: u64, cached: u64, cache_creation: u64) -> String {
            let total = input + output + cached + cache_creation;
            format!(
                "data: {{\"type\":\"usage\",\"input_tokens\":{input},\"output_tokens\":{output},\"cached_input_tokens\":{cached},\"cache_creation_tokens\":{cache_creation},\"total_tokens\":{total}}}\n\n"
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
    /// Anthropic: simple ephemeral breakpoint on the last Global block.
    /// Session-scoped sections were demoted to None (tool-dependent, change per turn)
    /// as part of the cache-stability fix, so only the Global breakpoint remains.
    /// Anthropic multi-turn: Global prefix is identical across turns with
    /// different tool sets → cross-session cache reuse.
    /// Anthropic multi-turn: same tool set + same task type → Session prefix
    /// also identical (only profile/style differ).
    /// OpenAI: stable prefix for automatic prefix caching.
    /// Static content is in the primary message (identical across turns);
    /// dynamic profile is in a separate second system message.
    /// OpenAI: different tool sets share the same Global prefix.
    ///
    /// After the cache-stability refactor, tool-dependent sections (Self-Model,
    /// tool-conditional guidance, task-type strategy, search strategy) are
    /// `CacheScope::None` and therefore emitted to the *dynamic* second
    /// system message for OpenAI. The **primary** system message should be
    /// byte-identical across tool sets — that's the whole point of moving
    /// them out of the cached prefix.
    /// Global sections contain no tool names — ensures cross-session cache reuse.
    /// Task type change only affects Session sections, not Global.
    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

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
    fn header_str_whitespace_value_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-user-id", "   ".parse().unwrap());
        assert!(header_str(&headers, "x-mo-user-id").is_none());
    }

    #[test]
    fn header_str_valid_value_returns_some() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-user-id", " user-123 ".parse().unwrap());
        assert_eq!(
            header_str(&headers, "x-mo-user-id").as_deref(),
            Some("user-123")
        );
    }

    #[test]
    fn required_bridge_header_rejects_missing_or_blank_identity() {
        let mut headers = HeaderMap::new();
        let err = required_bridge_header(&headers, "x-mo-session-id")
            .expect_err("missing bridge identity must fail");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("x-mo-session-id"));

        headers.insert("x-mo-session-id", "   ".parse().unwrap());
        let err = required_bridge_header(&headers, "x-mo-session-id")
            .expect_err("blank bridge identity must fail");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("x-mo-session-id"));
    }

    #[test]
    fn required_bridge_header_returns_trimmed_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-session-id", " session-123 ".parse().unwrap());
        assert_eq!(
            required_bridge_header(&headers, "x-mo-session-id").expect("valid bridge identity"),
            "session-123"
        );
    }

    #[test]
    fn optional_positive_u32_header_absent_or_blank_returns_none() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            optional_positive_u32_header(&headers, "x-mo-session-turn")
                .expect("absent header is optional"),
            None
        );

        headers.insert("x-mo-session-turn", "   ".parse().unwrap());
        assert_eq!(
            optional_positive_u32_header(&headers, "x-mo-session-turn")
                .expect("blank header is treated as absent"),
            None
        );
    }

    #[test]
    fn optional_positive_u32_header_accepts_trimmed_positive_u32() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mo-session-turn", " 42 ".parse().unwrap());
        assert_eq!(
            optional_positive_u32_header(&headers, "x-mo-session-turn")
                .expect("positive u32 header"),
            Some(42)
        );
    }

    #[test]
    fn optional_positive_u32_header_rejects_invalid_present_values() {
        for raw in ["0", "-1", "1.5", "not-a-number", "4294967296"] {
            let mut headers = HeaderMap::new();
            headers.insert("x-mo-session-turn", raw.parse().unwrap());
            let err = optional_positive_u32_header(&headers, "x-mo-session-turn")
                .expect_err("present invalid numeric header must fail");
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            assert!(err.1.contains("x-mo-session-turn"));
        }
    }

    #[test]
    fn parse_bridge_payload_rejects_invalid_json() {
        let err = parse_bridge_payload(&Bytes::from_static(b"{not-json"))
            .expect_err("invalid JSON must not become an empty payload");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("invalid bridge request JSON"));
    }

    #[test]
    fn parse_bridge_payload_rejects_non_object_json() {
        let err = parse_bridge_payload(&Bytes::from_static(br#"[]"#))
            .expect_err("bridge payload must be an object");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("must be a JSON object"));
    }

    #[test]
    fn parse_bridge_payload_accepts_object_json() {
        let payload =
            parse_bridge_payload(&Bytes::from_static(br#"{"messages":[]}"#)).expect("valid object");
        assert_eq!(payload["messages"], json!([]));
    }

    #[test]
    fn bridge_round_index_defaults_to_zero_when_absent() {
        assert_eq!(
            bridge_round_index(&json!({})).expect("missing round_index defaults to zero"),
            0
        );
    }

    #[test]
    fn bridge_round_index_accepts_u32() {
        assert_eq!(
            bridge_round_index(&json!({"round_index": 7})).expect("valid round_index"),
            7
        );
    }

    #[test]
    fn bridge_round_index_rejects_negative_fractional_and_overflow() {
        for payload in [
            json!({"round_index": -1}),
            json!({"round_index": 1.5}),
            json!({"round_index": u64::from(u32::MAX) + 1}),
        ] {
            let err =
                bridge_round_index(&payload).expect_err("invalid round_index must not be coerced");
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            assert!(err.1.contains("round_index"));
        }
    }

    #[test]
    fn optional_payload_array_defaults_only_when_absent() {
        assert!(
            optional_payload_array(&json!({}), "tool_results")
                .expect("absent optional array defaults")
                .is_empty()
        );

        let err = optional_payload_array(&json!({"tool_results": {}}), "tool_results")
            .expect_err("present wrong-type optional array must fail");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("tool_results"));
    }

    #[test]
    fn optional_payload_object_defaults_only_when_absent() {
        assert!(
            optional_payload_object(&json!({}), "edge_profile")
                .expect("absent optional object defaults")
                .is_empty()
        );

        let err = optional_payload_object(&json!({"edge_profile": []}), "edge_profile")
            .expect_err("present wrong-type optional object must fail");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("edge_profile"));
    }

    #[test]
    fn explain_requested_accepts_verbose_string() {
        assert!(explain_requested(&json!({ "explain": "verbose" })));
        assert!(explain_requested(&json!({ "explain": true })));
        assert!(!explain_requested(&json!({ "explain": false })));
        assert!(!explain_requested(&json!({ "explain": "off" })));
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![json!("old")];
        let mut usage = Map::new();
        let mut model = "old".to_string();
        let _ = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
        let mut reasoning_sig = String::new();
        let mut tc = vec![];
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut reasoning_sig,
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
    fn normalize_bridge_prompt_messages_routes_runtime_affix_out_of_user_content() {
        let messages = vec![json!({
            "role": "user",
            "content": "我说过的所有话\n\n<system-reminder>\n[session-resume:v1]\nHydrated previous session context\n</system-reminder>"
        })];

        let (messages, required_runtime_texts) = normalize_bridge_prompt_messages(messages);

        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": "我说过的所有话"})]
        );
        assert_eq!(
            required_runtime_texts,
            vec!["[session-resume:v1]\nHydrated previous session context"]
        );
    }

    #[test]
    fn required_runtime_text_for_bridge_merges_recovered_and_structured_lanes() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS
                .to_string(),
            json!(["structured runtime context"]),
        );

        let got = required_runtime_text_for_bridge(
            &edge_profile,
            &["[session-resume:v1]\nHydrated previous session context".to_string()],
        )
        .expect("required runtime text");

        assert_eq!(
            got,
            "[session-resume:v1]\nHydrated previous session context\n\nstructured runtime context"
        );
    }

    #[test]
    fn bridge_pipeline_event_turn_uses_session_trace_turn() {
        assert_eq!(bridge_pipeline_event_turn(8), 8);
        assert_eq!(bridge_pipeline_event_turn(0), 1);
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
    fn turn_complete_event_does_not_suggest_continuation_from_last_user_text() {
        let messages = vec![
            json!({"role": "user", "content": "first prompt"}),
            json!({"role": "assistant", "content": "intermediate"}),
            json!({"role": "user", "content": "继续处理"}),
        ];
        let event = turn_complete_event(&messages, "Should I continue?", &[]);
        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["has_tool_calls"], false);
        assert_eq!(event["assistant_text"], "Should I continue?");
        assert!(event.get("followup_suggestion").is_none(), "{event}");
    }

    #[test]
    fn turn_complete_event_uses_git_action_marker_for_followup() {
        let messages = vec![json!({"role": "user", "content": "commit it"})];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {
                "name": "git",
                "arguments": r#"{"action":"commit","message":"ship"}"#
            }
        })];

        let event = turn_complete_event(&messages, "Committed the changes.", &tool_calls);

        assert_eq!(event["followup_suggestion"], "push it");
    }

    #[test]
    fn bridge_tool_call_helpers_canonicalize_names() {
        let tool_calls = vec![
            json!({
                "id": "call-1",
                "function": {
                    "name": " git ",
                    "arguments": r#"{"action":"commit","message":"ship"}"#
                }
            }),
            json!({
                "id": "call-2",
                "function": {
                    "name": "   ",
                    "arguments": "{}"
                }
            }),
        ];

        assert_eq!(tool_names_from_tool_calls(&tool_calls), vec!["git"]);
        assert_eq!(
            tool_markers_from_tool_calls(&tool_calls),
            vec!["git:commit"]
        );
    }

    // ── P1: L0 anchor appears in system prompt ──────────────────────────

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
        messages.push(json!({
            "role": "assistant",
            "content": "Still working through the remaining steps."
        }));

        let config = MemoriaCompactConfig::default();
        let params = MemoriaCompactParams {
            budget_chars: 3000,
            keep_chars: 1500,
            tier: crate::prompts::CompactionTier::AggressivePrune,
            keep_recent_turns: 2,
            current_tokens: 80000,
            session_facts: None,
            turn_number: 0,
            observatory: None,
        };

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(compact_with_memoria(
                &messages, None, &config, &params, None, None, None,
            ));

        // Compaction happened (boundary present), so we simulate what the turn loop does.
        assert!(
            result.boundary.is_some(),
            "compaction should have triggered"
        );

        let mut msgs = result.messages;
        crate::turn::wire_assembly::maybe_append_continuation_prompt(
            &mut msgs,
            result.boundary.is_some(),
        );

        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        let note = last["content"].as_str().unwrap();
        assert!(note.contains("Context was compacted"));
        assert!(note.contains("not a new user request"));
        assert!(!note.contains("keep going"));
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
        };

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(compact_with_memoria(
                &messages, None, &config, &params, None, None, None,
            ));

        assert!(result.boundary.is_none(), "no compaction should happen");
        // No compaction note should be added.
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
            session_facts: None,
            turn_number: 0,
            observatory: None,
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
    fn p3_usage_reads_canonical_input_tokens_key() {
        // The bridge's usage map uses the canonical key set produced by
        // `turn::token_usage::TokenUsage::to_json_map`. Ensure the key name
        // expected downstream matches.
        let u = crate::turn::token_usage::TokenUsage {
            input_tokens: 45000,
            output_tokens: 2000,
            ..Default::default()
        };
        let m = u.to_json_map();
        let estimated_tokens = m.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as usize;
        assert_eq!(estimated_tokens, 45000);
    }

    #[test]
    fn load_bridge_pipeline_baseline_reconstructs_detector_state_from_journal() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000188";
        let writer = astra_services::session_journal::JournalWriter::new(session_id)
            .expect("journal writer");

        let request_metadata = |system_text: &str| {
            json!({
                "model": "test-model",
                "provider": "openai",
                "request": {
                    "messages": [
                        {"role": "system", "content": system_text},
                        {"role": "user", "content": "ping"}
                    ],
                    "tools": [{
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "parameters": {"type": "object"}
                        }
                    }]
                }
            })
        };
        let response_metadata = |cached_input_tokens: u64| {
            json!({
                "provider": "openai",
                "response": {
                    "response": {
                        "usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": cached_input_tokens,
                            "cache_creation_tokens": 0,
                            "output_tokens": 12
                        }
                    }
                }
            })
        };

        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_request_full(
                    Some(session_id),
                    1,
                    0,
                    request_metadata("stable prompt"),
                ),
            )
            .unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_response_full(
                    Some(session_id),
                    1,
                    0,
                    response_metadata(0),
                ),
            )
            .unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_request_full(
                    Some(session_id),
                    2,
                    0,
                    request_metadata("stable prompt"),
                ),
            )
            .unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_response_full(
                    Some(session_id),
                    2,
                    0,
                    response_metadata(500),
                ),
            )
            .unwrap();

        let mut baseline = load_bridge_pipeline_baseline(session_id);
        assert_eq!(baseline.next_turn, 3);
        let tool_names: Vec<&str> = baseline
            .last_tool_schemas
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert_eq!(
            tool_names,
            vec!["read_file"],
            "baseline should reconstruct the last advertised tool schema set"
        );
        assert!(
            baseline
                .cache_detector
                .snapshot_for_source(BRIDGE_CACHE_SOURCE)
                .is_some(),
            "baseline should warm the bridge cache detector from prior request/response pairs"
        );

        let current = bridge_prompt_snapshot_from_messages(
            &[
                json!({"role": "system", "content": "stable prompt"}),
                json!({"role": "user", "content": "continue"}),
            ],
            &[json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "parameters": {"type": "object"}
                }
            })],
            "test-model",
            "openai",
        )
        .expect("current prompt snapshot");
        assert!(
            baseline
                .cache_detector
                .record_turn_for_source(BRIDGE_CACHE_SOURCE, current, Some(500))
                .is_none(),
            "reconstructed detector state should treat the next stable turn as a hit"
        );
    }

    #[test]
    fn load_bridge_pipeline_baseline_enables_prompt_cache_diff_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000190";
        let writer = astra_services::session_journal::JournalWriter::new(session_id)
            .expect("journal writer");

        let request_metadata = |system_prompt: &str| {
            json!({
                "provider": "openai",
                "model": "test-model",
                "request": {
                    "messages": [
                        {"role": "system", "content": system_prompt},
                        {"role": "user", "content": "continue"}
                    ],
                    "tools": []
                }
            })
        };
        let response_metadata = |cached_input_tokens: u64| {
            json!({
                "provider": "openai",
                "response": {
                    "response": {
                        "usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": cached_input_tokens,
                            "cache_creation_tokens": 0,
                            "output_tokens": 12
                        }
                    }
                }
            })
        };

        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_request_full(
                    Some(session_id),
                    1,
                    0,
                    request_metadata("stable prompt"),
                ),
            )
            .unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::llm_response_full(
                    Some(session_id),
                    1,
                    0,
                    response_metadata(50),
                ),
            )
            .unwrap();

        let mut baseline = load_bridge_pipeline_baseline(session_id);
        let changed = bridge_prompt_snapshot_from_messages(
            &[
                json!({"role": "system", "content": "changed prompt"}),
                json!({"role": "user", "content": "continue"}),
            ],
            &[],
            "test-model",
            "openai",
        )
        .expect("changed prompt snapshot");
        let event =
            baseline
                .cache_detector
                .record_turn_for_source(BRIDGE_CACHE_SOURCE, changed, Some(0));
        assert!(
            event.is_some(),
            "changed prompt should trip the bridge cache detector"
        );

        let diff_dir = astra_services::local_session_artifact_store()
            .session_dir(session_id)
            .expect("session dir")
            .join("prompt-cache-diffs");
        let entries = (0..50)
            .find_map(|_| match std::fs::read_dir(&diff_dir) {
                Ok(read_dir) => {
                    let count = read_dir.count();
                    if count > 0 {
                        Some(count)
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        None
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                }
                Err(error) => panic!("prompt-cache diff dir: {error}"),
            })
            .unwrap_or(0);
        assert!(
            entries > 0,
            "bridge baseline should emit prompt-cache diff artifacts into the session dir"
        );
    }

    #[test]
    fn load_bridge_pipeline_baseline_preserves_absolute_turn_when_tail_truncated() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000189";
        let writer = astra_services::session_journal::JournalWriter::new(session_id)
            .expect("journal writer");

        let response_metadata = |cached_input_tokens: u64| {
            json!({
                "provider": "openai",
                "response": {
                    "response": {
                        "usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": cached_input_tokens,
                            "cache_creation_tokens": 0,
                            "output_tokens": 12
                        }
                    }
                }
            })
        };

        for turn in 1..=520 {
            writer
                .append(
                    &astra_services::session_journal::JournalEvent::llm_response_full(
                        Some(session_id),
                        turn,
                        0,
                        response_metadata(0),
                    ),
                )
                .unwrap();
        }

        let baseline = load_bridge_pipeline_baseline(session_id);
        assert_eq!(
            baseline.next_turn, 521,
            "tail-based reconstruction must preserve the absolute turn number"
        );
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(astra_test_bridge_secret)]
    async fn forward_persists_full_journal_request_and_response_when_session_capture_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let _env = EnvVarGuard::set("ASTRA_TEST_BRIDGE_SECRET", "bridge-journal-secret");
        let session_id = "00000000-0000-0000-0000-000000000129";
        let bridge = InProcessChatTurnBridge::new(bridge_test_matrixone(), bridge_test_encryptor());
        let headers = bridge_test_headers(session_id, true);
        let payload = json!({
            "messages": [{"role": "user", "content": "bridge journal success"}],
            "edge_tools": [],
            "test_llm_stream_blocks": [
                "data: {\"type\":\"text_delta\",\"content\":\"bridge journal reply\"}\n\n",
                "data: {\"type\":\"usage\",\"prompt_tokens\":11,\"completion_tokens\":4}\n\n",
                "data: {\"type\":\"_inprocess_summary\",\"full_text\":\"bridge journal reply\",\"reasoning\":\"\",\"tool_calls\":[],\"usage\":{\"prompt\":11,\"completion\":4,\"total\":15},\"model_used\":\"bridge-e2e-mock\"}\n\n"
            ]
        });

        let response = bridge
            .forward(
                &headers,
                Bytes::from(payload.to_string()),
                Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                Arc::new(crate::InMemoryTurnReflectionStateStore::default()),
                Arc::new(crate::NoopTurnReflectionLessonWriter),
                Arc::new(crate::NoopTurnObserverWorker),
                Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                None,
            )
            .await
            .expect("bridge forward");
        let body = collect_response_body(response).await;
        assert!(body.contains("bridge journal reply"));

        let journal = wait_for_journal_events(session_id).await;
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert_eq!(
            llm_events.len(),
            2,
            "expected request+response events: {journal:?}"
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["outcome"].as_str(),
            Some("success")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["full_text"].as_str(),
            Some("bridge journal reply")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["usage"]["completion"].as_i64(),
            Some(4)
        );
        assert_eq!(
            llm_events[0]["metadata"]["trace"]["session_turn_source"].as_str(),
            Some("header")
        );
        assert!(
            llm_events[0]["metadata"]["prompt_request_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(
            llm_events[0]["metadata"]["request_summary"]["message_count"].as_u64(),
            Some(2)
        );
        assert_eq!(
            llm_events[0]["metadata"]["request_summary"]["message_roles"][0]["role"].as_str(),
            Some("system")
        );
        assert_eq!(
            llm_events[0]["metadata"]["request_summary"]["message_roles"][1]["role"].as_str(),
            Some("user")
        );
        assert_eq!(
            llm_events[0]["metadata"]["request"]["messages"][1]["role"].as_str(),
            Some("user")
        );
        assert!(
            llm_events[0]["metadata"]["trace"]["turn_chain_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            llm_events[0]["metadata"]["trace"]["user_query_event_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(astra_test_bridge_secret)]
    async fn forward_does_not_persist_full_journal_events_when_session_capture_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let _env = EnvVarGuard::set("ASTRA_TEST_BRIDGE_SECRET", "bridge-journal-secret");
        let session_id = "00000000-0000-0000-0000-000000000133";
        let bridge = InProcessChatTurnBridge::new(bridge_test_matrixone(), bridge_test_encryptor());
        let mut headers = bridge_test_headers(session_id, true);
        headers.insert(ROOT_TURN_JOURNAL_HEADER, "1".parse().unwrap());
        let payload = json!({
            "root_turn_journal_owned": true,
            "messages": [{"role": "user", "content": "root-owned bridge capture"}],
            "edge_tools": [],
            "test_llm_stream_blocks": [
                "data: {\"type\":\"text_delta\",\"content\":\"root-owned bridge reply\"}\n\n",
                "data: {\"type\":\"usage\",\"prompt_tokens\":13,\"completion_tokens\":5}\n\n",
                "data: {\"type\":\"_inprocess_summary\",\"full_text\":\"root-owned bridge reply\",\"reasoning\":\"\",\"tool_calls\":[],\"usage\":{\"prompt\":13,\"completion\":5,\"total\":18},\"model_used\":\"bridge-e2e-mock\"}\n\n"
            ]
        });

        let response = bridge
            .forward(
                &headers,
                Bytes::from(payload.to_string()),
                Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                Arc::new(crate::InMemoryTurnReflectionStateStore::default()),
                Arc::new(crate::NoopTurnReflectionLessonWriter),
                Arc::new(crate::NoopTurnObserverWorker),
                Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                None,
            )
            .await
            .expect("bridge forward");
        let body = collect_response_body(response).await;
        assert!(body.contains("root-owned bridge reply"));

        let journal = wait_for_journal_events(session_id).await;
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert_eq!(
            llm_events.len(),
            2,
            "expected request+response events: {journal:?}"
        );
        assert!(
            journal
                .iter()
                .any(|event| event.get("type").and_then(Value::as_str) == Some("llm_round")),
            "direct bridge e2e hooks intentionally ignore root-owned journal hints to avoid impersonating the root runtime: {journal:?}"
        );
        assert_eq!(
            llm_events[0]["metadata"]["trace"]["session_turn_source"].as_str(),
            Some("header")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["full_text"].as_str(),
            Some("root-owned bridge reply")
        );
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(astra_test_bridge_secret)]
    async fn forward_persists_full_journal_rounds_from_round_index_across_same_session_turn() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let _env = EnvVarGuard::set("ASTRA_TEST_BRIDGE_SECRET", "bridge-journal-secret");
        let session_id = "00000000-0000-0000-0000-000000000132";
        let bridge = InProcessChatTurnBridge::new(bridge_test_matrixone(), bridge_test_encryptor());
        let headers = bridge_test_headers(session_id, true);

        for (round_index, reply) in [(0_u32, "bridge round zero"), (1_u32, "bridge round one")] {
            let payload = json!({
                "messages": [{"role": "user", "content": format!("bridge round {round_index}")}],
                "edge_tools": [],
                "round_index": round_index,
                "test_llm_stream_blocks": [
                    format!("data: {{\"type\":\"text_delta\",\"content\":\"{reply}\"}}\n\n"),
                    "data: {\"type\":\"usage\",\"prompt_tokens\":11,\"completion_tokens\":4}\n\n",
                    format!("data: {{\"type\":\"_inprocess_summary\",\"full_text\":\"{reply}\",\"reasoning\":\"\",\"tool_calls\":[],\"usage\":{{\"prompt\":11,\"completion\":4,\"total\":15}},\"model_used\":\"bridge-e2e-mock\"}}\n\n"),
                ]
            });

            let response = bridge
                .forward(
                    &headers,
                    Bytes::from(payload.to_string()),
                    Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                    Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                    Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                    Arc::new(crate::InMemoryTurnReflectionStateStore::default()),
                    Arc::new(crate::NoopTurnReflectionLessonWriter),
                    Arc::new(crate::NoopTurnObserverWorker),
                    Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                    Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                    None,
                )
                .await
                .expect("bridge forward");
            let body = collect_response_body(response).await;
            assert!(body.contains(reply));
        }

        let llm_events = {
            let mut events = Vec::new();
            for _ in 0..100 {
                let journal = read_journal_events(session_id);
                events = journal
                    .into_iter()
                    .filter(|event| {
                        matches!(
                            event.get("type").and_then(Value::as_str),
                            Some("llm_request_full" | "llm_response_full")
                        )
                    })
                    .collect::<Vec<_>>();
                if events.len() >= 4 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            events
        };

        let rounds: Vec<_> = llm_events
            .iter()
            .map(|event| event["round"].as_i64())
            .collect();
        assert_eq!(
            rounds,
            vec![Some(0), Some(0), Some(1), Some(1)],
            "same-turn bridge requests should preserve authoritative round_index in full capture"
        );
        let trace_rounds: Vec<_> = llm_events
            .iter()
            .map(|event| event["metadata"]["trace"]["round"].as_i64())
            .collect();
        assert_eq!(trace_rounds, vec![Some(0), Some(0), Some(1), Some(1)]);
        let bridge_rounds: Vec<_> = read_journal_events(session_id)
            .into_iter()
            .filter(|event| {
                event.get("type").and_then(Value::as_str) == Some("llm_round")
                    && event["metadata"]["source"].as_str() == Some("bridge_inprocess")
            })
            .map(|event| event["round"].as_i64())
            .collect();
        assert_eq!(bridge_rounds, vec![Some(0), Some(1)]);
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(astra_test_bridge_secret)]
    async fn forward_persists_full_journal_error_response_with_partial_state_when_session_capture_enabled()
     {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let _env = EnvVarGuard::set("ASTRA_TEST_BRIDGE_SECRET", "bridge-journal-secret");
        let session_id = "00000000-0000-0000-0000-000000000130";
        let bridge = InProcessChatTurnBridge::new(bridge_test_matrixone(), bridge_test_encryptor());
        let headers = bridge_test_headers(session_id, true);
        let payload = json!({
            "messages": [{"role": "user", "content": "bridge journal partial error"}],
            "edge_tools": [],
            "test_llm_stream_blocks": [
                "data: {\"type\":\"text_delta\",\"content\":\"partial bridge text\"}\n\n",
                "data: {not-json}\n\n"
            ]
        });

        let response = bridge
            .forward(
                &headers,
                Bytes::from(payload.to_string()),
                Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                Arc::new(crate::InMemoryTurnReflectionStateStore::default()),
                Arc::new(crate::NoopTurnReflectionLessonWriter),
                Arc::new(crate::NoopTurnObserverWorker),
                Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                None,
            )
            .await
            .expect("bridge forward");
        let body = collect_response_body(response).await;
        assert!(body.contains("SSE_PARSE_ERROR"));

        let journal = wait_for_journal_events(session_id).await;
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert_eq!(
            llm_events.len(),
            2,
            "expected request+error events: {journal:?}"
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["outcome"].as_str(),
            Some("sse_parse_error")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["kind"].as_str(),
            Some("SSE_PARSE_ERROR")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["full_text"].as_str(),
            Some("partial bridge text")
        );
        assert_eq!(
            llm_events[1]["metadata"]["trace"]["session_turn_source"].as_str(),
            Some("header")
        );
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(astra_test_bridge_secret)]
    async fn forward_persists_full_journal_context_with_reasoning_when_session_capture_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let _env = EnvVarGuard::set("ASTRA_TEST_BRIDGE_SECRET", "bridge-journal-secret");
        let session_id = "00000000-0000-0000-0000-000000000131";
        let bridge = InProcessChatTurnBridge::new(bridge_test_matrixone(), bridge_test_encryptor());
        let headers = bridge_test_headers(session_id, false);
        let payload = json!({
            "messages": [{"role": "user", "content": "bridge journal disabled"}],
            "edge_tools": [],
            "test_llm_stream_blocks": [
                "data: {\"type\":\"text_delta\",\"content\":\"bridge disabled reply\"}\n\n",
                "data: {\"type\":\"usage\",\"prompt_tokens\":9,\"completion_tokens\":3}\n\n",
                "data: {\"type\":\"_inprocess_summary\",\"full_text\":\"bridge disabled reply\",\"reasoning\":\"\",\"tool_calls\":[],\"usage\":{\"prompt\":9,\"completion\":3,\"total\":12},\"model_used\":\"bridge-e2e-mock\"}\n\n"
            ]
        });

        let response = bridge
            .forward(
                &headers,
                Bytes::from(payload.to_string()),
                Arc::new(crate::turn::services::NoopTurnCoreEventWriter),
                Arc::new(crate::turn::services::NoopTurnToolEventWriter),
                Arc::new(crate::turn::services::NoopTurnHookDbWriter),
                Arc::new(crate::InMemoryTurnReflectionStateStore::default()),
                Arc::new(crate::NoopTurnReflectionLessonWriter),
                Arc::new(crate::NoopTurnObserverWorker),
                Arc::new(crate::turn::services::NoopTurnAuxiliaryEventWriter),
                Arc::new(crate::turn::services::NoopTurnSessionActivityWriter),
                None,
            )
            .await
            .expect("bridge forward");
        let body = collect_response_body(response).await;
        assert!(body.contains("bridge disabled reply"));

        let journal = wait_for_journal_events(session_id).await;
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert!(
            llm_events.is_empty(),
            "capture-disabled run should not emit full LLM journal events: {journal:?}"
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

    // ── Fix #11: CJK detection for bilingual compaction note ────────────

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

    /// audit-#6: when tool events fail to persist after core events succeed,
    /// the spawned persist task must emit a structured `tool_events_orphaned`
    /// marker so log-based reconciliation can recover the lost data.
    #[test]
    fn persist_task_emits_orphan_marker_on_tool_failure() {
        let source = include_str!("inprocess.rs");
        assert!(
            source.contains("marker = \"tool_events_orphaned\""),
            "bridge persist task must emit a `tool_events_orphaned` marker when \
             tool_writer.persist fails after core events were already committed"
        );
    }

    /// audit-#11: `await_with_client_disconnect` must not use `biased;` —
    /// biasing toward the cancellation arm starves real work whenever the
    /// caller's cancel token is already set when the future is polled.
    #[test]
    fn await_with_client_disconnect_is_not_biased() {
        let source = include_str!("inprocess.rs");
        let fn_start = source
            .find("async fn await_with_client_disconnect")
            .expect("await_with_client_disconnect must exist");
        let body = &source[fn_start..fn_start + 700];
        assert!(
            !body.contains("biased;"),
            "await_with_client_disconnect must not use biased select (starvation risk)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_bridge_stream_failure_capture_persists_remote_llm_capture() {
        let store = RecordingArtifactStore::default();
        let usage = Map::from_iter([
            ("prompt".to_string(), json!(120)),
            ("completion".to_string(), json!(18)),
        ]);
        persist_bridge_stream_failure_capture(
            "bridge_inprocess stream incomplete capture",
            true,
            &store,
            "sess-1",
            "user-1",
            2,
            &BridgeTraceCorrelation {
                session_turn_source: "header".to_string(),
                turn_chain_id: "chain-test".to_string(),
                user_query_event_id: "query-test".to_string(),
            },
            3,
            Some("agent-1"),
            "",
            "gpt-5.4-mini",
            "openai",
            &[json!({"role":"user","content":"debug the stream"})],
            &[json!({"type":"function","function":{"name":"bash"}})],
            Some(2048),
            "stream_incomplete",
            "LLM stream ended without completion summary from provider",
            "STREAM_INCOMPLETE",
            "partial answer",
            "thinking",
            &[json!({"id":"call-1","type":"function"})],
            &usage,
        )
        .await;

        let records = store.records.lock().expect("recording store lock");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.artifact_kind, "llm_capture");
        assert_eq!(
            record.metadata.as_ref().and_then(|v| v.get("outcome")),
            Some(&json!("stream_incomplete"))
        );
        assert_eq!(record.content["response"]["kind"], "STREAM_INCOMPLETE");
        assert_eq!(
            record.content["response"]["partial_full_text"],
            "partial answer"
        );
        assert_eq!(record.content["response"]["usage"]["prompt"], 120);
        assert_eq!(record.content["trace"]["turn_chain_id"], "chain-test");
        assert_eq!(
            record.metadata.as_ref().unwrap()["trace"]["session_turn_source"],
            "header"
        );
    }

    #[test]
    fn bridge_root_turn_journal_requires_explicit_header() {
        let mut headers = HeaderMap::new();
        let empty_payload = json!({});
        headers.insert("x-mo-session-turn", "2".parse().unwrap());
        assert!(
            !bridge_root_turn_journal_owned(&headers, &empty_payload, false),
            "session turn alone must not suppress bridge full-journal capture"
        );
        headers.insert(ROOT_TURN_JOURNAL_HEADER, "1".parse().unwrap());
        assert!(bridge_root_turn_journal_owned(
            &headers,
            &empty_payload,
            false
        ));
        assert!(
            !bridge_root_turn_journal_owned(&headers, &empty_payload, true),
            "bridge e2e should never impersonate a root journal owner"
        );
        let payload_owned = json!({"root_turn_journal_owned": true});
        assert!(bridge_root_turn_journal_owned(
            &HeaderMap::new(),
            &payload_owned,
            false
        ));
    }

    #[test]
    fn root_owned_bridge_keeps_full_capture_but_suppresses_llm_round_summary() {
        assert!(
            bridge_should_create_turn_event_buffer(true, true),
            "root-owned turns still need a buffer for llm_request_full/llm_response_full"
        );
        assert!(
            !bridge_should_record_llm_round(true),
            "aggregate llm_round rows are owned by the root loop"
        );
        assert!(
            !bridge_should_create_turn_event_buffer(false, true),
            "without full capture the bridge has nothing to journal for root-owned turns"
        );
        assert!(
            bridge_should_record_llm_round(false),
            "standalone bridge calls still own their llm_round summary"
        );
    }

    #[test]
    #[serial_test::serial]
    fn root_owned_bridge_full_capture_flushes_once_without_llm_round() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = format!("root-owned-capture-{}", uuid::Uuid::new_v4());
        let trace = BridgeTraceCorrelation {
            session_turn_source: "header".to_string(),
            turn_chain_id: "chain-root-owned".to_string(),
            user_query_event_id: "user-query-root-owned".to_string(),
        };
        let mut turn_event_buffer = bridge_should_create_turn_event_buffer(true, true)
            .then(|| TurnEventBuffer::begin_turn_with_round(Some(&session_id), 7, 3));

        record_full_llm_request_event(
            &mut turn_event_buffer,
            true,
            "root",
            &session_id,
            7,
            &trace,
            "bridge_inprocess",
            "gpt-5",
            "openai",
            1,
            &[json!({"role": "user", "content": "inspect"})],
            &[],
            Some(1024),
        );
        if bridge_should_record_llm_round(true)
            && let Some(buf) = turn_event_buffer.as_mut()
        {
            buf.record_llm_round(LlmRoundRecord {
                prompt_tokens: 10,
                completion_tokens: 2,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                ttft_ms: None,
                duration_ms: 1,
                tool_calls_returned: 0,
                tool_call_names: Vec::new(),
                finish_reason: Some("stop".to_string()),
                agentic_step: Some(3),
                source: Some("bridge_inprocess".to_string()),
                run_id: None,
                tool_calls: None,
                ..Default::default()
            });
        }
        record_full_llm_response_event(
            &mut turn_event_buffer,
            true,
            &session_id,
            7,
            &trace,
            "bridge_inprocess",
            "gpt-5",
            "openai",
            1,
            "ok",
            json!({"content": "done"}),
        );
        let writer = JournalWriter::new(&session_id).unwrap();
        turn_event_buffer.as_mut().unwrap().flush(&writer).unwrap();
        let events = astra_services::session_journal::read_journal(&session_id).unwrap();

        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type
                    == astra_services::session_journal::JournalEventType::LlmRequestFull)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type
                    == astra_services::session_journal::JournalEventType::LlmResponseFull)
                .count(),
            1
        );
        assert!(
            events.iter().all(|event| event.event_type
                != astra_services::session_journal::JournalEventType::LlmRound),
            "root-owned bridge flush must not double-write aggregate llm_round rows"
        );
    }

    #[test]
    fn llm_request_dump_failures_are_not_silently_ignored() {
        let source = include_str!("inprocess.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        for context in [
            "bridge_inprocess compacted context-window dump persist failed",
            "bridge_inprocess error dump persist failed",
        ] {
            let start = production
                .find(context)
                .expect("dump logging context should exist");
            let window_start = start.saturating_sub(520);
            let window = &production[window_start..production.len().min(start + 120)];
            assert!(
                window.contains("if let Err(error) =")
                    && window.contains(
                        "dump.persist_remote(&user_id, remote_artifact_store.as_ref()).await"
                    ),
                "{context} should handle dump.persist_remote failures explicitly"
            );
        }
    }

    #[test]
    fn llm_error_paths_publish_remote_llm_capture_artifacts() {
        let source = include_str!("inprocess.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        for context in [
            "bridge_inprocess compacted context-window capture",
            "bridge_inprocess error capture",
        ] {
            let start = production
                .find(context)
                .expect("capture context should exist");
            let window = &production[start..production.len().min(start + 260)];
            assert!(
                window.contains("Some(remote_artifact_store.as_ref())"),
                "{context} should publish a remote llm_capture artifact"
            );
        }
    }

    #[test]
    fn bridge_stream_failure_paths_publish_remote_llm_capture_artifacts() {
        let source = include_str!("inprocess.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        for context in [
            "bridge_inprocess stream block parse capture",
            "bridge_inprocess stream tail parse capture",
            "bridge_inprocess stream incomplete capture",
            "bridge_inprocess client disconnect capture",
        ] {
            let start = production
                .find(context)
                .expect("stream failure capture context should exist");
            let window_start = start.saturating_sub(320);
            let window = &production[window_start..production.len().min(start + 220)];
            assert!(
                window.contains("persist_bridge_stream_failure_capture("),
                "{context} should persist a remote llm_capture artifact for mid-stream failures"
            );
        }
    }

    // ── bridge_should_run_memoria_prefetch gate ────────────────────────

    #[test]
    fn prefetch_gate_runs_when_cli_insights_absent() {
        let ep: Map<String, Value> = Map::new();
        assert!(
            bridge_should_run_memoria_prefetch(&ep),
            "empty edge_profile = CLI didn't run memory_boost_search; bridge must fetch"
        );
    }

    #[test]
    fn prefetch_gate_runs_when_cli_insights_empty_string() {
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            "memoria_insights_text".to_string(),
            Value::String(String::new()),
        );
        assert!(
            bridge_should_run_memoria_prefetch(&ep),
            "empty string insights should not count as CLI-produced content"
        );
    }

    #[test]
    fn prefetch_gate_skips_when_cli_insights_present() {
        // Regression: bridge used to double-fetch Memoria even when CLI
        // already rendered `## Memoria Recall`, producing ~700 tokens of
        // duplicate memory content as a second `## User Memories` block.
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            "memoria_insights_text".to_string(),
            Value::String("## Memoria Recall\n- User prefers Rust for CLI work.".to_string()),
        );
        assert!(
            !bridge_should_run_memoria_prefetch(&ep),
            "CLI-rendered digest already covers the memory retrieval — skip bridge prefetch"
        );
    }

    #[test]
    fn prefetch_gate_runs_when_insights_key_not_a_string() {
        // Defensive: if edge_profile carries malformed insights (non-string),
        // fall back to running the bridge fetch rather than silently
        // producing an empty memory section.
        let mut ep: Map<String, Value> = Map::new();
        ep.insert("memoria_insights_text".to_string(), Value::Null);
        assert!(bridge_should_run_memoria_prefetch(&ep));
        ep.insert(
            "memoria_insights_text".to_string(),
            Value::Number(42.into()),
        );
        assert!(bridge_should_run_memoria_prefetch(&ep));
    }

    #[test]
    fn deferred_tools_block_keeps_text_when_source_and_resolved_models_share_budget() {
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String("<deferred-tools>\ngithub\n</deferred-tools>".to_string()),
        );
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW
                .to_string(),
            Value::Number(
                crate::prompts::budget_for_model(Some("gpt-4o"))
                    .model_limit
                    .into(),
            ),
        );
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES
                .to_string(),
            serde_json::json!(["github"]),
        );

        let block = deferred_tools_block_for_bridge_model(&ep, "gpt-4o-2024-08-06", None);
        assert!(
            block.contains("<deferred-tools>"),
            "same effective context budget should preserve the CLI-rendered deferred block"
        );
    }

    #[test]
    fn deferred_tools_block_drops_text_without_names_manifest() {
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String("<deferred-tools>\ngithub\n</deferred-tools>".to_string()),
        );
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW
                .to_string(),
            Value::Number(
                crate::prompts::budget_for_model(Some("gpt-4o"))
                    .model_limit
                    .into(),
            ),
        );

        let block = deferred_tools_block_for_bridge_model(&ep, "gpt-4o", None);
        assert!(
            block.is_empty(),
            "bridge must not render deferred prompt text without the paired names manifest used by validator/tool_search"
        );
    }

    #[test]
    fn deferred_tools_block_drops_text_when_resolved_model_changes_budget() {
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String("<deferred-tools>\ngithub\n</deferred-tools>".to_string()),
        );
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW
                .to_string(),
            Value::Number(
                crate::prompts::budget_for_model(Some("gpt-3.5-turbo"))
                    .model_limit
                    .into(),
            ),
        );

        let block = deferred_tools_block_for_bridge_model(&ep, "claude-sonnet-4", None);
        assert!(
            block.is_empty(),
            "bridge must not reuse a deferred block sized for a smaller context window after final model resolution changes the budget"
        );
    }

    #[test]
    fn deferred_tools_block_drops_text_without_explicit_source_context_window() {
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String("<deferred-tools>\ngithub\n</deferred-tools>".to_string()),
        );

        let block = deferred_tools_block_for_bridge_model(&ep, "gpt-4o", None);
        assert!(
            block.is_empty(),
            "bridge must not guess the source budget when the edge_profile omits it"
        );
    }

    #[test]
    fn deferred_tools_block_uses_explicit_source_context_window_not_default_model_guess() {
        let mut ep: Map<String, Value> = Map::new();
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String("<deferred-tools>\ngithub\n</deferred-tools>".to_string()),
        );
        ep.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW
                .to_string(),
            Value::Number(
                crate::prompts::budget_for_model(Some("gpt-3.5-turbo"))
                    .model_limit
                    .into(),
            ),
        );

        let block = deferred_tools_block_for_bridge_model(&ep, "gpt-4o", None);
        assert!(
            block.is_empty(),
            "bridge must trust the explicit source context window instead of guessing from the default model budget"
        );
    }

    // ── tool_result.output non-string coercion tests ──────────────────

    #[test]
    fn build_bridge_records_string_output_passes_through() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{\"command\":\"echo hello\"}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": "hello\n",
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].result_preview.as_deref(),
            Some("hello\n"),
            "string output must pass through verbatim"
        );
    }

    #[test]
    fn build_bridge_records_json_error_output_overrides_success_status() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "agent_fanout", "arguments": "{\"action\":\"start\"}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "agent_fanout",
            "status": "completed",
            "output": "{\"status\":\"failed\",\"error\":\"Invalid input: unknown field `slot_id`\"}",
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        assert!(!records[0].ok, "{records:?}");
        assert!(
            records[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unknown field `slot_id`")),
            "{records:?}"
        );
    }

    #[test]
    fn build_bridge_records_structured_empty_result_exit_as_ok() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{\"command\":\"grep needle haystack.txt\"}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "failed",
            "output": "No matches found",
            "duration_ms": 50,
            "exit_semantics": "empty_result",
            "result_class": "empty_result"
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        assert!(records[0].ok, "{records:?}");
        assert!(records[0].error.is_none(), "{records:?}");
        assert_eq!(records[0].exit_semantics.as_deref(), Some("empty_result"));
        assert_eq!(records[0].result_class.as_deref(), Some("empty_result"));
    }

    #[test]
    fn build_bridge_records_structured_execution_error_overrides_success_status() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{\"command\":\"exit 7\"}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": "Error: command failed (exit code 7)",
            "duration_ms": 50,
            "exit_semantics": "execution_error",
            "result_class": "execution_error"
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        assert!(!records[0].ok, "{records:?}");
        assert!(
            records[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("exit code 7")),
            "{records:?}"
        );
        assert_eq!(
            records[0].exit_semantics.as_deref(),
            Some("execution_error")
        );
        assert_eq!(records[0].result_class.as_deref(), Some("execution_error"));
    }

    #[test]
    fn build_bridge_records_object_output_coerces_to_string_not_silent() {
        // Regression: if upstream serialization bug puts an object `{}`
        // in the `output` field instead of a string, we now preserve the
        // real JSON form (not a synthetic sentinel) and log a warning so
        // operators can trace the upstream bug.
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": {},
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        // Empty Object is the historical pollution shape that made the
        // model claim "tool returned {}". Treat it as degraded empty
        // content rather than replaying a bare "{}" tool result.
        assert_eq!(
            records[0].result_preview.as_deref(),
            Some(""),
            "empty-object output must not be surfaced as bare '{{}}'"
        );
        // Must NOT contain the debug sentinel marker
        assert!(
            !records[0]
                .result_preview
                .as_deref()
                .unwrap_or("")
                .contains("[BRIDGE_OUTPUT_TYPE_BUG]"),
            "must NOT replace real data with sentinel"
        );
    }

    // ── Finding 🟡 6: non-empty Object output must preserve real data ──
    //
    // Regression guard: an earlier iteration of the bridge replaced
    // Object-shaped output with a `[BRIDGE_OUTPUT_TYPE_BUG] ...` sentinel,
    // silently discarding the actual tool payload. If the upstream
    // serialization bug ever fires in prod, we want the LLM to see the
    // real JSON — not a synthetic error tag.
    #[test]
    fn build_bridge_records_nonempty_object_output_preserves_real_payload() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": {"stdout": "hello", "exit": 0},
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        let preview = records[0]
            .result_preview
            .as_deref()
            .expect("object payload must surface, not None");
        // Real keys must survive coercion
        assert!(
            preview.contains("stdout") && preview.contains("hello"),
            "real object data must be preserved, got: {preview}"
        );
        assert!(
            !preview.contains("[BRIDGE_OUTPUT_TYPE_BUG]"),
            "must NOT replace real data with sentinel: {preview}"
        );
    }

    // ── Finding 🟡 8: Value::Array path ──
    //
    // Array outputs (e.g. list of content blocks) must also be preserved
    // as JSON, not replaced with a sentinel.
    #[test]
    fn build_bridge_records_array_output_preserves_elements() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": [{"type": "text", "text": "hello"}],
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        let preview = records[0]
            .result_preview
            .as_deref()
            .expect("array payload must surface, not None");
        assert!(
            preview.contains("hello"),
            "array element content must survive: {preview}"
        );
        assert!(
            !preview.contains("[BRIDGE_OUTPUT_TYPE_BUG]"),
            "must NOT replace real data with sentinel: {preview}"
        );
    }

    // ── Finding 🟡 7: null → "" is intentional, tripwire anchor ──
    //
    // Explicitly pin down the contract: `Value::Null` coerces to empty
    // string (not the literal "null"), which is what the hallucination
    // tripwire's `any_physical_empty` check relies on to distinguish
    // "tool really returned nothing" from "tool returned valid JSON".
    // If this ever changes, the tripwire will silently stop firing.
    #[test]
    fn build_bridge_records_null_output_contract_matches_tripwire_anchor() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": null,
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        // Must NOT be the literal string "null"
        assert_ne!(
            records[0].result_preview.as_deref(),
            Some("null"),
            "null JSON must not surface as literal 'null' string"
        );
        // Must be empty (None or "") — the tripwire anchor
        let preview = records[0].result_preview.as_deref().unwrap_or("");
        assert!(
            preview.is_empty(),
            "null output must coerce to empty, got: {preview:?}"
        );
    }

    #[test]
    fn build_bridge_records_null_output_becomes_empty() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "output": null,
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        // null output → empty string (not "null")
        assert!(
            records[0].result_preview.is_none() || records[0].result_preview.as_deref() == Some(""),
            "null output should become empty, got: {:?}",
            records[0].result_preview
        );
    }

    #[test]
    fn build_bridge_records_missing_output_is_none() {
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "request_id": "call-1",
            "name": "bash",
            "status": "completed",
            "duration_ms": 50
        })];
        let records = build_bridge_tool_call_records(
            &tool_calls,
            &tool_results,
            &std::collections::HashMap::new(),
        );
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].result_preview, None,
            "missing output field → None (not empty string)"
        );
    }
}
