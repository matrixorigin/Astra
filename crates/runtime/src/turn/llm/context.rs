//! Shared LLM context assembly entry points.
//!
//! This module is the runtime boundary for cache-sensitive prompt assembly.
//! Callers may collect context differently (CLI edge payloads, web-agent
//! server state, bridge requests), but once they have normalized inputs they
//! should route through this module instead of hand-assembling system prompts,
//! volatile preambles, provider cache placement, or tier-pruned tool schemas.

use std::collections::{BTreeMap, HashSet};

use astra_services::SessionArtifactStore;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::super::agentic_loop::host::AgenticLoopState;
use super::super::prompt_cache::PromptCacheConfig;

pub(crate) fn cache_capability_from_model_metadata(
    value: Option<astra_services::PromptCacheCapabilityData>,
) -> Option<astra_turn_core::cache_placement::CacheCapability> {
    let value = value?;
    let protocol = match value.protocol {
        astra_services::PromptCacheProtocolData::MarkerExplicit => {
            astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit
        }
        astra_services::PromptCacheProtocolData::BedrockCachePoint => {
            astra_turn_core::cache_placement::CacheProtocol::BedrockCachePoint
        }
        astra_services::PromptCacheProtocolData::OpenAiAutoPrefix => {
            astra_turn_core::cache_placement::CacheProtocol::OpenAiAutoPrefix
        }
        astra_services::PromptCacheProtocolData::StrictHistoryMatch => {
            astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch
        }
        astra_services::PromptCacheProtocolData::None => {
            astra_turn_core::cache_placement::CacheProtocol::None
        }
    };
    let volatile_placement = match value.volatile_placement {
        astra_services::PromptCacheVolatilePlacementData::MarkerIsolated => {
            astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated
        }
        astra_services::PromptCacheVolatilePlacementData::TailSuffix => {
            astra_turn_core::cache_placement::VolatilePlacement::TailSuffix
        }
        astra_services::PromptCacheVolatilePlacementData::AppendOnlyUserTail => {
            astra_turn_core::cache_placement::VolatilePlacement::AppendOnlyUserTail
        }
        astra_services::PromptCacheVolatilePlacementData::CurrentUserOnly => {
            astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly
        }
        astra_services::PromptCacheVolatilePlacementData::Free => {
            astra_turn_core::cache_placement::VolatilePlacement::Free
        }
    };
    let volatile_delivery = match value.volatile_delivery {
        astra_services::PromptCacheVolatileDeliveryData::All => {
            astra_turn_core::cache_placement::VolatileDeliveryPolicy::All
        }
        astra_services::PromptCacheVolatileDeliveryData::RequiredOnly => {
            astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly
        }
    };
    let reuse_scope = value.reuse_scope.map(|scope| match scope {
        astra_services::PromptCacheReuseScopeData::ConversationTurns => {
            astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns
        }
        astra_services::PromptCacheReuseScopeData::IntraTurnRounds => {
            astra_turn_core::cache_placement::CacheReuseScope::IntraTurnRounds
        }
    });
    Some(astra_turn_core::cache_placement::CacheCapability {
        protocol,
        volatile_placement,
        volatile_delivery,
        reuse_scope,
    })
}

pub(crate) fn compact_strategy_from_model_metadata(
    value: Option<astra_services::PromptCacheCapabilityData>,
    provider: &str,
) -> astra_turn_core::microcompact::CompactStrategy {
    let explicit = cache_capability_from_model_metadata(value);
    astra_turn_core::microcompact::ProviderCacheStrategy::from_explicit_or_provider(
        explicit,
        Some(provider),
    )
    .compact_strategy
}

fn estimate_json_tokens(value: &Value) -> u32 {
    estimate_json_tokens_u64(value).min(u32::MAX as u64) as u32
}

fn estimate_json_tokens_u64(value: &Value) -> u64 {
    match value {
        Value::Null => 1,
        Value::Bool(_) => 1,
        Value::Number(number) => u64::from(
            astra_turn_core::section_types::estimate_text_tokens(&number.to_string()).max(1),
        ),
        Value::String(text) => {
            u64::from(astra_turn_core::section_types::estimate_text_tokens(text).saturating_add(1))
        }
        Value::Array(items) => {
            let structural_tokens = 1_u64;
            items
                .iter()
                .map(estimate_json_tokens_u64)
                .fold(structural_tokens, u64::saturating_add)
        }
        Value::Object(map) => {
            let structural_tokens = 1_u64;
            map.iter()
                .map(|(key, value)| {
                    u64::from(
                        astra_turn_core::section_types::estimate_text_tokens(key).saturating_add(1),
                    )
                    .saturating_add(estimate_json_tokens_u64(value))
                })
                .fold(structural_tokens, u64::saturating_add)
        }
    }
}

fn session_memory_injection(
    entry: Option<&astra_turn_core::context_sources::MemoryEntry>,
) -> Option<astra_turn_core::context_assembly_trace::MemoryInjection> {
    let entry = entry?;
    if entry.content.trim().is_empty() {
        return None;
    }
    let source = entry
        .source
        .clone()
        .unwrap_or_else(|| "session_memory".into());
    Some(astra_turn_core::context_assembly_trace::MemoryInjection {
        memory_id: "session-memory".into(),
        memory_type: source.clone(),
        tokens: astra_turn_core::section_types::estimate_text_tokens(&entry.content).max(1),
        relevance_score: session_memory_relevance_score(&source),
        content_preview: astra_turn_core::context_assembly_trace::preview_snippet(
            &entry.content,
            100,
        ),
    })
}

pub(crate) fn prompt_memory_injections(
    entries: &[astra_turn_core::context_sources::MemoryEntry],
) -> Vec<astra_turn_core::context_assembly_trace::MemoryInjection> {
    entries
        .iter()
        .map(
            |entry| astra_turn_core::context_assembly_trace::MemoryInjection {
                memory_id: entry
                    .memory_id
                    .clone()
                    .unwrap_or_else(|| format!("content-{:016x}", entry.content_hash)),
                memory_type: entry
                    .memory_type
                    .clone()
                    .or_else(|| entry.source.clone())
                    .unwrap_or_else(|| "unknown".into()),
                tokens: entry.token_estimate,
                relevance_score: entry.relevance_score,
                content_preview: astra_turn_core::context_assembly_trace::preview_snippet(
                    &entry.content,
                    100,
                ),
            },
        )
        .collect()
}

fn session_memory_relevance_score(source: &str) -> f64 {
    if source == "session_memory.reanchor" {
        1.0
    } else {
        0.35
    }
}

fn resolve_pipeline_session_current_date(
    pipeline_session: Option<&astra_turn_core::pipeline_session::PipelineSession>,
    user_id: Option<&str>,
    session_id: &str,
) -> String {
    pipeline_session
        .map(|session| session.current_date().to_string())
        .unwrap_or_else(|| match user_id {
            Some(user_id) => {
                crate::turn::session_current_date::resolve_session_current_date_for_user(
                    user_id, session_id,
                )
            }
            None => crate::turn::session_current_date::resolve_session_current_date(session_id),
        })
}

/// Input for the shared context-pipeline assembly phase.
///
/// This is intentionally still close to the current server/web host state so
/// the first migration can preserve behavior. Follow-up adapters can build this
/// from CLI and web-agent source catalogs without changing the pipeline path.
pub(crate) struct LlmContextAssemblyInput<'a> {
    pub state: &'a mut AgenticLoopState,
    pub session_id: &'a str,
    pub tool_surface: ToolSurfacePlan<'a>,
    pub runtime_signals: RuntimeSignals<'a>,
    pub cache_cfg: &'a PromptCacheConfig,
    pub provider: &'a str,
    pub model_name: &'a str,
    pub context_window: Option<u32>,
    pub max_completion_tokens: Option<u32>,
    pub cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    pub user_content: &'a str,
    pub query_source: &'a str,
}

/// Normalized tool visibility for one LLM call.
///
/// CLI and web/server can populate this from different catalogs, selectors, or
/// executors. The shared assembler only sees the selected visible schemas.
pub(crate) struct ToolSurfacePlan<'a> {
    pub visible_tools: &'a [Value],
    pub always_load_tools: &'a [Value],
    pub dynamic_tools: &'a [Value],
    pub required_tools: &'a [Value],
    pub deferred_tools_block: &'a str,
    pub restricted_tools: &'a HashSet<String>,
}

impl<'a> ToolSurfacePlan<'a> {
    pub(crate) fn from_visible_tools(
        visible_tools: &'a [Value],
        restricted_tools: &'a HashSet<String>,
    ) -> Self {
        Self {
            visible_tools,
            always_load_tools: visible_tools,
            dynamic_tools: &[],
            required_tools: &[],
            deferred_tools_block: "",
            restricted_tools,
        }
    }

    fn effective_tools(&self) -> Vec<Value> {
        let tools = effective_tool_schemas(
            self.visible_tools,
            self.always_load_tools,
            self.dynamic_tools,
            self.required_tools,
        );
        filter_restricted_tool_schemas(tools, self.restricted_tools)
    }

    pub(crate) fn with_deferred_tools_block(mut self, block: &'a str) -> Self {
        self.deferred_tools_block = block;
        self
    }

    pub(crate) fn with_required_tools(mut self, tools: &'a [Value]) -> Self {
        self.required_tools = tools;
        self
    }
}

/// Normalized runtime prompt signals for one LLM call.
///
/// This keeps host-specific source collection outside the shared assembler.
pub(crate) struct RuntimeSignals<'a> {
    pub edge_profile: &'a Map<String, Value>,
    pub plan_resume_hint: Option<String>,
    pub extra_stable_sections: &'a [crate::prompts::PromptSection],
    pub extra_volatile_sections: &'a [crate::prompts::PromptSection],
    pub memory_entries: &'a [astra_turn_core::context_sources::MemoryEntry],
    pub memory_provider_source: Option<&'a str>,
    pub session_memory_entry: Option<astra_turn_core::context_sources::MemoryEntry>,
}

impl<'a> RuntimeSignals<'a> {
    pub(crate) fn new(
        edge_profile: &'a Map<String, Value>,
        plan_resume_hint: Option<String>,
    ) -> Self {
        Self {
            edge_profile,
            plan_resume_hint,
            extra_stable_sections: &[],
            extra_volatile_sections: &[],
            memory_entries: &[],
            memory_provider_source: None,
            session_memory_entry: None,
        }
    }

    pub(crate) fn with_extra_sections(
        mut self,
        stable: &'a [crate::prompts::PromptSection],
        volatile: &'a [crate::prompts::PromptSection],
    ) -> Self {
        self.extra_stable_sections = stable;
        self.extra_volatile_sections = volatile;
        self
    }

    pub(crate) fn with_session_memory_entry(
        mut self,
        session_memory_entry: Option<astra_turn_core::context_sources::MemoryEntry>,
    ) -> Self {
        self.session_memory_entry = session_memory_entry;
        self
    }

    pub(crate) fn with_memory_entries(
        mut self,
        memory_entries: &'a [astra_turn_core::context_sources::MemoryEntry],
    ) -> Self {
        self.memory_entries = memory_entries;
        self
    }

    pub(crate) fn with_memory_provider_source(mut self, source: Option<&'a str>) -> Self {
        self.memory_provider_source = source;
        self
    }
}

/// Output of the shared context-pipeline assembly phase.
///
/// The LLM caller should consume these fields directly. In particular,
/// `tool_schemas` is already pruned by the pipeline tier; downstream code may
/// add provider wire annotations, but must not re-derive the pruning decision.
pub(crate) struct LlmContextAssemblyOutput {
    pub system_messages: Vec<Value>,
    pub volatile_preamble: Vec<Value>,
    /// Conversation history after the pipeline's tier-aware optimization.
    ///
    /// Callers must feed this view into any later semantic compactor instead
    /// of restarting from the unoptimized session history.
    pub messages: Vec<Value>,
    pub system_plain: String,
    pub breakdown: astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
    pub tier: astra_turn_core::compaction_types::CompactionTier,
    pub tool_schemas: Vec<Value>,
    pub manifest_trace: LlmContextManifestTrace,
}

#[derive(Clone, Debug)]
pub(crate) struct LlmContextManifestTrace {
    pub source: &'static str,
    pub provider: String,
    pub model_name: String,
    pub model_context_window_tokens: u32,
    pub context_window_policy: crate::prompts::ContextWindowPolicy,
    pub compaction_tier: String,
    pub system_prompt_tokens: u32,
    pub stable_system_message_count: usize,
    pub volatile_preamble_count: usize,
    pub tool_schema_count: usize,
    pub runtime_manifest: Option<Value>,
}

impl LlmContextManifestTrace {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "provider": self.provider.clone(),
            "model_name": self.model_name.clone(),
            "model_context_window_tokens": self.model_context_window_tokens,
            "context_window_policy": self.context_window_policy,
            "compaction_tier": self.compaction_tier.clone(),
            "system_prompt_tokens": self.system_prompt_tokens,
            "stable_system_message_count": self.stable_system_message_count,
            "volatile_preamble_count": self.volatile_preamble_count,
            "tool_schema_count": self.tool_schema_count,
            "runtime_manifest": self.runtime_manifest.clone(),
        })
    }
}

fn manifest_u64(trace: &Value, pointer: &str) -> Option<u64> {
    trace.pointer(pointer).and_then(Value::as_u64)
}

fn manifest_string(trace: &Value, pointer: &str) -> Option<String> {
    trace
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn model_request_rollout_stage() -> astra_services::ModelRequestRolloutStage {
    use std::sync::OnceLock;
    static STAGE: OnceLock<astra_services::ModelRequestRolloutStage> = OnceLock::new();
    *STAGE.get_or_init(|| {
        match std::env::var("ASTRA_MODEL_REQUEST_CONTEXT_ROLLOUT")
            .ok()
            .as_deref()
        {
            Some("opt_in") => astra_services::ModelRequestRolloutStage::OptIn,
            Some("topology_canary") => astra_services::ModelRequestRolloutStage::TopologyCanary,
            Some("default") => astra_services::ModelRequestRolloutStage::Default,
            // Missing and invalid configuration both fail closed to shadow.
            Some("shadow") | None | Some(_) => astra_services::ModelRequestRolloutStage::Shadow,
        }
    })
}

/// Convert the shared assembly manifest into the content-free seed persisted
/// at the physical provider boundary.
///
/// Every lookup is an exact schema path. Unknown facts stay absent; provider
/// or model names are never interpreted to manufacture lineage or budgets.
pub(crate) fn model_request_context_seed_from_manifest(
    topology: astra_services::ModelRequestTopology,
    trace: Option<&Value>,
) -> astra_services::ModelRequestContextSeed {
    let (interaction_owner, loop_owner, execution_binding) = match topology {
        astra_services::ModelRequestTopology::CliServer => ("cli", "server", "server"),
        astra_services::ModelRequestTopology::EdgeServer => ("edge", "server", "edge"),
        astra_services::ModelRequestTopology::ServerOnly => ("server", "server", "server"),
    };
    let mut seed = astra_services::ModelRequestContextSeed::server_default();
    seed.topology = topology;
    seed.rollout_stage = model_request_rollout_stage();
    seed.interaction_owner = interaction_owner.to_string();
    seed.loop_owner = loop_owner.to_string();
    seed.execution_binding = execution_binding.to_string();
    let Some(trace) = trace else {
        return seed;
    };

    seed.budget.raw_context_window_tokens =
        manifest_u64(trace, "/context_window_policy/raw_context_window_tokens")
            .or_else(|| manifest_u64(trace, "/model_context_window_tokens"));
    seed.budget.usable_input_limit_tokens =
        manifest_u64(trace, "/context_window_policy/usable_input_limit_tokens")
            .or_else(|| manifest_u64(trace, "/wire/budget/effective_input_limit"));
    seed.budget.reserved_output_tokens =
        manifest_u64(trace, "/context_window_policy/reserved_output_tokens")
            .or_else(|| manifest_u64(trace, "/wire/budget/requested_output_tokens"));
    seed.budget.reserved_summary_tokens =
        manifest_u64(trace, "/context_window_policy/reserved_summary_tokens");
    seed.budget.reserved_protocol_tokens =
        manifest_u64(trace, "/context_window_policy/reserved_protocol_tokens")
            .or_else(|| manifest_u64(trace, "/wire/budget/reserved_protocol_tokens"));
    seed.budget.compact_trigger_tokens =
        manifest_u64(trace, "/context_window_policy/auto_compact_trigger_tokens");
    seed.budget.hard_limit_tokens =
        manifest_u64(trace, "/context_window_policy/hard_input_limit_tokens")
            .or_else(|| manifest_u64(trace, "/wire/budget/model_limit"));
    seed.budget.estimated_input_tokens = manifest_u64(trace, "/wire/budget/estimated_input_tokens");
    seed.budget.usage_source = Some("pre_provider_estimate".to_string());

    seed.composition.stable_system_tokens = manifest_u64(trace, "/system_prompt_tokens");
    seed.cache.layout = manifest_string(
        trace,
        "/wire/fingerprint/prompt_cache_identity/cache_layout",
    );
    seed.cache.current_identity =
        manifest_string(trace, "/wire/fingerprint/prompt_cache_identity/content_id");
    seed.compaction.tier = manifest_string(trace, "/compaction_tier");
    seed
}

/// Attach every durably admitted physical request, its exact serialized
/// provider-body composition, and its complete durable terminal when present.
///
/// Each durable attempt is admitted from the same immutable bytes used by its
/// HTTP body. The complete array is authoritative; `request_identity` is only
/// a latest-attempt projection for the existing turn-trace summary.
pub(crate) fn augment_manifest_trace_with_provider_attempts(
    trace: &mut Value,
    attempts: &[crate::turn::llm::durable::DurableProviderAttemptFact],
    round: u32,
) {
    let Some(latest) = attempts.last() else {
        return;
    };
    let Some(trace) = trace.as_object_mut() else {
        return;
    };
    let latest_response_id = latest
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.provider_response_id.as_deref());
    trace.insert(
        "request_identity".to_string(),
        json!({
            "request_id": latest.request.request_id,
            "request_hash": latest.request.request_hash,
            "round": round,
            "attempt": latest.request.attempt,
            "provider_response_id": latest_response_id,
        }),
    );
    trace.insert(
        "provider_request_attempts".to_string(),
        Value::Array(
            attempts
                .iter()
                .map(|attempt| {
                    let request = &attempt.request;
                    let terminal = attempt.terminal.as_ref();
                    json!({
                        "authority": "exact_serialized_provider_body_v1",
                        "transport_stage": if attempt.dispatch_started {
                            "dispatch_started"
                        } else {
                            "prepared_and_admitted"
                        },
                        "request_id": request.request_id,
                        "request_hash": request.request_hash,
                        "round": round,
                        "attempt": request.attempt,
                        "protocol": request.protocol.as_str(),
                        "provider_response_id": terminal
                            .and_then(|terminal| terminal.provider_response_id.as_deref()),
                        "terminal_status": terminal.map(|terminal| terminal.status.as_str()),
                        "usage_status": terminal.map(|terminal| terminal.usage_status.as_str()),
                        "usage": terminal.map(|terminal| json!({
                            "input": terminal.usage.input,
                            "output_tokens": terminal.usage.output_tokens,
                        })),
                        "error_kind": terminal
                            .and_then(|terminal| terminal.error_kind.as_deref()),
                        "error_message": terminal
                            .and_then(|terminal| terminal.error_message.as_deref()),
                        "serialized_bytes": request.provider_wire_bytes,
                        "composition_bytes": {
                            "system": request.composition.system_bytes,
                            "conversation": request.composition.conversation_bytes,
                            "tool_schema": request.composition.tool_schema_bytes,
                            "provider_envelope": request.composition.provider_envelope_bytes,
                            "total": request.composition.total_bytes(),
                        },
                        "composition_items": {
                            "system": request.composition.system_items,
                            "conversation": request.composition.conversation_items,
                            "tool_schema": request.composition.tool_schema_items,
                        },
                        "provider_final_fingerprints": {
                            "message_sequence_sha256": request.fingerprints.message_sequence_sha256,
                            "system_sequence_sha256": request.fingerprints.system_sequence_sha256,
                            "cache_key_system_sha256": request.fingerprints.cache_key_system_sha256,
                            "conversation_sequence_sha256": request.fingerprints.conversation_sequence_sha256,
                            "tool_schema_sequence_sha256": request.fingerprints.tool_schema_sequence_sha256,
                            "cache_key_tool_schema_sequence_sha256": request.fingerprints.cache_key_tool_schema_sequence_sha256,
                            "cache_capability": request.fingerprints.cache_capability,
                        },
                    })
                })
                .collect(),
        ),
    );
}

pub(crate) fn clear_manifest_provider_request(trace: &mut Value) {
    let Some(trace) = trace.as_object_mut() else {
        return;
    };
    trace.remove("request_identity");
    trace.remove("provider_request_attempts");
}

fn runtime_manifest_with_memory_context(
    mut manifest: Option<Value>,
    provider_source: Option<&str>,
    prompt_entry_count: usize,
    session_snapshot_injected: bool,
) -> Option<Value> {
    if provider_source.is_none() && prompt_entry_count == 0 && !session_snapshot_injected {
        return manifest;
    }
    let root = manifest.get_or_insert_with(|| json!({}));
    if !root.is_object() {
        *root = json!({});
    }
    root["memory_context"] = json!({
        "provider_source": provider_source,
        "prompt_entry_count": prompt_entry_count,
        "session_snapshot_injected": session_snapshot_injected,
        "delivery": "typed_runtime_dynamic",
    });
    manifest
}

#[cfg(test)]
pub(crate) fn context_meta_event(
    breakdown: &astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
    context_manifest_trace: Option<&Value>,
) -> Value {
    context_meta_event_with_compactions(breakdown, context_manifest_trace, &[])
}

pub(crate) fn context_meta_event_with_compactions(
    breakdown: &astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
    context_manifest_trace: Option<&Value>,
    compactions: &[astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation],
) -> Value {
    let mut event = json!({
        "type": "context_meta",
        "system_prompt_tokens": breakdown.total_tokens,
        "system_prompt_breakdown": breakdown,
    });
    if let Some(trace) = context_manifest_trace {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
            trace,
        );
        event["context_manifest_trace"] = trace.clone();
    }
    if !compactions.is_empty() {
        event["compactions"] = json!(compactions);
    }
    event
}

pub(crate) fn context_meta_event_with_tool_surface(
    breakdown: &astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
    context_manifest_trace: Option<&Value>,
    compactions: &[astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation],
    visible_tools: &[Value],
) -> Value {
    let mut event =
        context_meta_event_with_compactions(breakdown, context_manifest_trace, compactions);
    let mut names = visible_tools
        .iter()
        .filter_map(tool_name)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    event["visible_tools"] = json!(names);
    event["visible_tool_actions"] = json!(visible_tool_action_surface(visible_tools));
    event
}

/// Return the action union from the exact schemas sent to the provider.
/// Names alone are insufficient for audit: a tool can remain visible while a
/// hard authority projection removes its `start`/`write` branch. Keep this
/// projection deterministic and bounded so it can be persisted alongside the
/// round without retaining the full (potentially sensitive) schemas.
pub(crate) fn visible_tool_action_surface(
    visible_tools: &[Value],
) -> BTreeMap<String, Vec<String>> {
    visible_tools
        .iter()
        .filter_map(|schema| {
            let name = tool_name(schema)?;
            let mut actions = schema
                .pointer("/function/parameters/properties/action/enum")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>();
            for branch in schema
                .pointer("/function/parameters/oneOf")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(action) = branch
                    .pointer("/properties/action/const")
                    .and_then(Value::as_str)
                {
                    actions.push(action.to_string());
                }
                actions.extend(
                    branch
                        .pointer("/properties/action/enum")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string),
                );
            }
            actions.sort();
            actions.dedup();
            (!actions.is_empty()).then(|| (name.to_string(), actions))
        })
        .collect::<BTreeMap<_, _>>()
}

fn cache_control_count(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(cache_control_count).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(cache_control_count).sum(),
        _ => 0,
    }
}

fn canonical_wire_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_wire_value).collect()),
        Value::Object(map) => {
            let sorted = map
                .iter()
                .map(|(key, value)| (key.clone(), canonical_wire_value(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        other => other.clone(),
    }
}

fn canonical_wire_sha256(value: &Value) -> String {
    let canonical = canonical_wire_value(value);
    let bytes =
        serde_json::to_vec(&canonical).expect("serde_json::Value serialization is infallible");
    if astra_core::history_work::instrumentation_enabled() {
        let bytes_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        astra_core::history_work::record_bytes(
            astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
            bytes_len,
        );
        astra_core::history_work::record_bytes(
            astra_core::history_work::HistoryWorkSite::LlmWireTraceHash,
            bytes_len,
        );
    }
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn message_role(message: &Value) -> String {
    message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn tool_name(schema: &Value) -> Option<&str> {
    schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
}

fn push_unique_tool(out: &mut Vec<Value>, seen: &mut HashSet<String>, schema: &Value) {
    let Some(name) = tool_name(schema) else {
        out.push(schema.clone());
        return;
    };
    if seen.insert(name.to_string()) {
        out.push(schema.clone());
    }
}

fn effective_tool_schemas(
    visible_tools: &[Value],
    always_load_tools: &[Value],
    dynamic_tools: &[Value],
    required_tools: &[Value],
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for schema in always_load_tools {
        push_unique_tool(&mut out, &mut seen, schema);
    }
    for schema in required_tools {
        push_unique_tool(&mut out, &mut seen, schema);
    }
    for schema in dynamic_tools {
        push_unique_tool(&mut out, &mut seen, schema);
    }
    for schema in visible_tools {
        push_unique_tool(&mut out, &mut seen, schema);
    }
    out
}

fn filter_restricted_tool_schemas(
    schemas: Vec<Value>,
    restricted_tools: &HashSet<String>,
) -> Vec<Value> {
    if restricted_tools.is_empty() {
        return schemas;
    }
    schemas
        .into_iter()
        .filter(|schema| {
            tool_name(schema)
                .map(|name| !restricted_tools.contains(name))
                .unwrap_or(true)
        })
        .collect()
}

/// Input for the final wire-message stitching phase.
pub(crate) struct LlmWireAssemblyInput<'a> {
    pub system_messages: Vec<Value>,
    pub volatile_preamble: Vec<Value>,
    pub compacted_messages: Vec<Value>,
    pub state: &'a mut AgenticLoopState,
    /// A real compaction boundary rewrote the history for this stateless
    /// request. Re-emit the active-turn frame. Requests without a boundary
    /// retain the ordinary stable prompt path.
    pub compaction_boundary_hit: bool,
    pub thinking: &'a astra_turn_core::thinking_config::ThinkingConfig,
    pub session_id: &'a str,
    pub provider: &'a str,
    pub model_name: &'a str,
    pub cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    pub cache_cfg: &'a PromptCacheConfig,
}

pub(crate) struct ContextManifestProjectionInput<'a> {
    pub owner_id: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub turn_index: usize,
    pub llm_attempt_index: u32,
    pub pre_llm_messages: &'a [Value],
    pub tool_results: &'a [Value],
    pub schema_tokens: u32,
    pub result_prompt_tokens: Option<u32>,
    pub observed_fresh_input_tokens: Option<u64>,
    pub observed_cache_read_tokens: Option<u64>,
    pub observed_cache_creation_tokens: Option<u64>,
    pub observed_output_tokens: Option<u64>,
    pub assembly_trace: Option<Value>,
    pub turn_intent: &'a str,
    pub reason: &'a str,
    pub context_window_tokens: u32,
}

struct LlmArtifactEvidenceCollection {
    manifest: Option<astra_turn_types::LlmArtifactEvidenceManifestV1>,
    status: &'static str,
}

fn llm_artifact_evidence_from_tool_results(
    input: &ContextManifestProjectionInput<'_>,
) -> LlmArtifactEvidenceCollection {
    let mut entries = Vec::new();
    let mut seen_artifacts = std::collections::HashSet::new();
    let mut observed_reference_count = 0_usize;
    let mut invalid_reference_count = 0_usize;
    for tool_result in input.tool_results {
        let Some(reference) =
            tool_result.get(astra_turn_types::TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY)
        else {
            continue;
        };
        observed_reference_count = observed_reference_count.saturating_add(1);
        let Some(reference) = reference.as_object() else {
            invalid_reference_count = invalid_reference_count.saturating_add(1);
            continue;
        };
        let Some(artifact_id) = reference
            .get("artifactId")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            invalid_reference_count = invalid_reference_count.saturating_add(1);
            continue;
        };
        let Some(content_hash) =
            reference
                .get("contentHash")
                .and_then(Value::as_str)
                .filter(|value| {
                    value.len() == 71
                        && value.starts_with("sha256:")
                        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        else {
            invalid_reference_count = invalid_reference_count.saturating_add(1);
            continue;
        };
        let tool_name = tool_result
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool_result");
        let Ok(mut entry) =
            astra_turn_types::LlmArtifactEvidenceEntryV1::new(artifact_id, tool_name, content_hash)
        else {
            invalid_reference_count = invalid_reference_count.saturating_add(1);
            continue;
        };
        if !seen_artifacts.insert(artifact_id.to_string())
            || entries.len() >= astra_turn_types::LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES
        {
            continue;
        }
        entry.media_type = Some("application/json".to_string());
        entry.encoded_bytes = reference.get("encodedBytes").and_then(Value::as_u64);
        entries.push(entry);
    }
    if observed_reference_count == 0 {
        return LlmArtifactEvidenceCollection {
            manifest: None,
            status: "absent",
        };
    }
    let manifest = astra_turn_types::LlmArtifactEvidenceManifestV1::new(
        input.owner_id,
        input.session_id,
        format!(
            "run:{}:turn:{}:attempt:{}",
            input.run_id, input.turn_index, input.llm_attempt_index
        ),
        entries,
        observed_reference_count,
        invalid_reference_count,
    );
    match manifest {
        Ok(manifest) => {
            let status = if manifest.invalid_reference_count == manifest.observed_reference_count {
                "all_invalid"
            } else if manifest.invalid_reference_count > 0 {
                "partial_invalid"
            } else if manifest.omitted_reference_count > 0 {
                "partial_bounded"
            } else {
                "ready"
            };
            LlmArtifactEvidenceCollection {
                manifest: Some(manifest),
                status,
            }
        }
        Err(_) => LlmArtifactEvidenceCollection {
            manifest: None,
            status: "identity_invalid",
        },
    }
}

pub(crate) struct ContextManifestProjection {
    pub total_estimated_tokens: u32,
    pub max_output_tokens: u32,
    pub manifest_json: Value,
    pub items: Vec<astra_services::ContextManifestItemWrite>,
}

fn is_tool_result_wire_message(message: &Value) -> bool {
    if matches!(
        message.get("role").and_then(Value::as_str),
        Some("tool" | "tool_result")
    ) {
        return true;
    }
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// Build the persisted context-manifest projection for a single LLM call.
///
/// This is deliberately inside `llm_context` so manifest rows are derived by
/// the same module that owns prompt/cache assembly. The current projection
/// preserves the existing DB schema and coarse zones; richer section-level
/// traces can be added here without changing execution-phase orchestration.
pub(crate) fn build_context_manifest_projection(
    input: ContextManifestProjectionInput<'_>,
) -> ContextManifestProjection {
    let budget_allocation = astra_services::budget_for_turn_intent(Some(input.turn_intent));
    let budget = budget_allocation.budget.clone();
    let (message_tokens, tool_result_tokens) =
        input
            .pre_llm_messages
            .iter()
            .fold((0_u32, 0_u32), |(messages, tool_results), message| {
                let tokens = estimate_json_tokens(message);
                if is_tool_result_wire_message(message) {
                    (messages, tool_results.saturating_add(tokens))
                } else {
                    (messages.saturating_add(tokens), tool_results)
                }
            });
    let total_estimated_tokens = input.result_prompt_tokens.unwrap_or_else(|| {
        message_tokens
            .saturating_add(tool_result_tokens)
            .saturating_add(input.schema_tokens)
    });
    let artifact_evidence = llm_artifact_evidence_from_tool_results(&input);

    let mut items = vec![
        astra_services::ContextManifestItemWrite {
            session_id: input.session_id.to_string(),
            item_order: 0,
            zone: "session_anchor".to_string(),
            source_table: "agent_runs".to_string(),
            source_id: input.run_id.to_string(),
            source_hash: None,
            included: true,
            token_estimate: 0,
            budget_tokens: budget.anchor,
            reason: input.reason.to_string(),
            render_mode: "reference_only".to_string(),
            raw_ref: None,
        },
        astra_services::ContextManifestItemWrite {
            session_id: input.session_id.to_string(),
            item_order: 1,
            zone: "recent_tail".to_string(),
            source_table: "runtime_messages".to_string(),
            source_id: format!("{}:turn:{}:messages", input.run_id, input.turn_index),
            source_hash: None,
            included: true,
            token_estimate: message_tokens.min(budget.recent_tail),
            budget_tokens: budget.recent_tail,
            reason: input.reason.to_string(),
            render_mode: "markdown".to_string(),
            raw_ref: Some(format!(
                "conversation_log://{}/{}@runtime",
                input.session_id, input.turn_index
            )),
        },
        astra_services::ContextManifestItemWrite {
            session_id: input.session_id.to_string(),
            item_order: 2,
            zone: "system_tool_schemas".to_string(),
            source_table: "tool_registry".to_string(),
            source_id: "visible_tools".to_string(),
            source_hash: None,
            included: true,
            token_estimate: input.schema_tokens.min(budget.system_tool_schemas),
            budget_tokens: budget.system_tool_schemas,
            reason: input.reason.to_string(),
            render_mode: "reference_only".to_string(),
            raw_ref: None,
        },
    ];

    if tool_result_tokens > 0 {
        items.push(astra_services::ContextManifestItemWrite {
            session_id: input.session_id.to_string(),
            item_order: 3,
            zone: "tool_previews".to_string(),
            source_table: "tool_results".to_string(),
            source_id: format!("{}:turn:{}:tool_results", input.run_id, input.turn_index),
            source_hash: None,
            included: true,
            token_estimate: tool_result_tokens.min(budget.tool_previews),
            budget_tokens: budget.tool_previews,
            reason: "large_tool_output_gated".to_string(),
            render_mode: "tool_preview".to_string(),
            raw_ref: None,
        });
    }
    if let Some(manifest) = artifact_evidence.manifest.as_ref() {
        items.push(astra_services::ContextManifestItemWrite {
            session_id: input.session_id.to_string(),
            item_order: 4,
            zone: "llm_artifact_evidence".to_string(),
            source_table: "session_artifacts".to_string(),
            source_id: manifest.content_id.clone(),
            source_hash: Some(manifest.content_id.clone()),
            included: true,
            token_estimate: 0,
            budget_tokens: 0,
            reason: "content_addressed_attempt_artifact_evidence".to_string(),
            render_mode: "inline_manifest_evidence".to_string(),
            raw_ref: None,
        });
    }

    let estimated_input = message_tokens
        .saturating_add(tool_result_tokens)
        .saturating_add(input.schema_tokens);
    if estimated_input > budget.input_context_cap() {
        items.push(astra_services::ContextManifestItemWrite {
            session_id: input.session_id.to_string(),
            item_order: 90,
            zone: "recent_tail".to_string(),
            source_table: "runtime_messages".to_string(),
            source_id: format!("{}:turn:{}:overflow", input.run_id, input.turn_index),
            source_hash: None,
            included: false,
            token_estimate: estimated_input.saturating_sub(budget.input_context_cap()),
            budget_tokens: 0,
            reason: "progressive_loading".to_string(),
            render_mode: "summary".to_string(),
            raw_ref: Some(format!(
                "conversation_log://{}/{}@overflow",
                input.session_id, input.turn_index
            )),
        });
    }

    let normalized_prompt_cache_usage = if input.observed_fresh_input_tokens.is_some()
        || input.observed_cache_read_tokens.is_some()
        || input.observed_cache_creation_tokens.is_some()
    {
        Some(astra_turn_types::NormalizedPromptCacheUsage::new(
            input.observed_fresh_input_tokens.unwrap_or_default(),
            input.observed_cache_read_tokens.unwrap_or_default(),
            input.observed_cache_creation_tokens.unwrap_or_default(),
        ))
    } else {
        None
    };
    let manifest_json = json!({
        "source": "llm_context",
        "llm_attempt_index": input.llm_attempt_index,
        "turn_index": input.turn_index,
        "turn_intent": input.turn_intent,
        "model_context_window_tokens": input.context_window_tokens,
        "observed_usage": {
            "fresh_input_tokens": input.observed_fresh_input_tokens,
            "cached_input_tokens": input.observed_cache_read_tokens,
            "cache_creation_tokens": input.observed_cache_creation_tokens,
            "output_tokens": input.observed_output_tokens
        },
        "normalized_prompt_cache_usage": normalized_prompt_cache_usage,
        "llm_artifact_evidence_status": artifact_evidence.status,
        "llm_artifact_evidence": artifact_evidence.manifest,
        "assembly_trace": input.assembly_trace,
        "budget_template_id": "budget_v1_8k",
        "budget_flex": {
            "flex_applied": budget_allocation.flex_applied,
            "borrowed_from_recent_tail": budget_allocation.borrowed_from_recent_tail
        },
        "zones": {
            "session_anchor": {"budget_tokens": budget.anchor, "used_tokens": 0},
            "recent_tail": {"budget_tokens": budget.recent_tail, "used_tokens": message_tokens.min(budget.recent_tail)},
            "tool_previews": {"budget_tokens": budget.tool_previews, "used_tokens": tool_result_tokens.min(budget.tool_previews)},
            "system_tool_schemas": {"budget_tokens": budget.system_tool_schemas, "used_tokens": input.schema_tokens.min(budget.system_tool_schemas)}
        }
    });

    ContextManifestProjection {
        total_estimated_tokens,
        max_output_tokens: budget.reserved_output,
        manifest_json,
        items,
    }
}

/// Run the shared context pipeline and split stable/volatile system content
/// according to the provider's cache behavior.
pub(crate) fn assemble_context_pipeline(
    input: LlmContextAssemblyInput<'_>,
) -> Result<LlmContextAssemblyOutput, astra_core::ClassifiedError> {
    use crate::turn::context_pipeline_adapter::{
        build_external_sources, build_session_context, build_turn_state,
    };
    use astra_turn_core::cache_placement::{CacheCapability, VolatilePlacement};
    use astra_turn_core::context_sources::AgentContext;
    use astra_turn_core::pipeline_session::AdaptiveTurnInput;
    use astra_turn_core::section_types::CacheScope;

    let state = input.state;
    let effective_tools = input.tool_surface.effective_tools();
    let _tool_surface_metadata = (
        input.tool_surface.always_load_tools.len(),
        input.tool_surface.dynamic_tools.len(),
        input.tool_surface.required_tools.len(),
        input.tool_surface.restricted_tools.len(),
    );
    let tool_names_owned: Vec<String> = effective_tools
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let tool_names: Vec<&str> = tool_names_owned.iter().map(String::as_str).collect();
    let cache_cap =
        CacheCapability::from_explicit_or_provider(input.cache_capability, input.provider);
    if state.pipeline_session.is_none() {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::InvalidRequest,
            format!(
                "pipeline_session missing during context assembly for session {}",
                input.session_id
            ),
        ));
    }

    let mut external = build_external_sources(
        input.runtime_signals.edge_profile,
        state,
        &tool_names,
        input.runtime_signals.plan_resume_hint.as_deref(),
        Some(cache_cap),
    );
    external
        .memory_entries
        .extend(input.runtime_signals.memory_entries.iter().cloned());
    external
        .extra_stable_sections
        .extend(input.runtime_signals.extra_stable_sections.iter().cloned());
    external.extra_dynamic_sections.extend(
        input
            .runtime_signals
            .extra_volatile_sections
            .iter()
            .cloned(),
    );
    if tool_names.contains(&"memory")
        && let Some(selection) =
            astra_tools::memoria::MemoriaToolGateway::latest_selection_context(input.session_id)
    {
        external
            .extra_dynamic_sections
            .push(crate::prompts::PromptSection::dynamic(
                selection,
                crate::prompts::PromptTokenBucket::Environment,
            ));
    }
    external
        .extra_dynamic_sections
        .push(crate::turn::prompt_cache::model_identity_prompt_section(
            input.model_name,
        ));
    external.session_memory_entry = input.runtime_signals.session_memory_entry.clone();
    let required_runtime_texts = astra_turn_core::chat_turn_edge_profile::edge_profile_texts(
        input.runtime_signals.edge_profile,
        astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS,
    );
    let runtime_volatile_injections =
        astra_turn_core::chat_turn_edge_profile::edge_profile_runtime_volatile_injections(
            input.runtime_signals.edge_profile,
        );
    let mut turn_state = build_turn_state(state, input.user_content);
    let rehomed_append_only_authority = if matches!(
        cache_cap.volatile_placement,
        VolatilePlacement::AppendOnlyUserTail
    ) {
        Vec::new()
    } else {
        crate::turn::wire_assembly::rehome_append_only_runtime_authority(&mut turn_state.messages)
            .map_err(|error| {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                error.to_string(),
            )
        })?
    };
    // `AgenticLoopState::max_turn_input_tokens` is an input-budget/wind-down
    // cap, and `0` is its legacy "unlimited" sentinel. The pipeline's
    // `SessionContext::model_limit` is different: it must be the concrete
    // model context window used for section budgeting and pressure planning.
    let window_policy = crate::prompts::budget_for_model_with_metadata(
        Some(input.model_name),
        input.context_window,
        input.max_completion_tokens,
    )
    .window_policy();
    let model_context_limit =
        u64::try_from(window_policy.usable_input_limit_tokens).unwrap_or(u64::MAX);
    let session_current_date = resolve_pipeline_session_current_date(
        state.pipeline_session.as_ref(),
        state.context_manifest_user_id.as_deref(),
        input.session_id,
    );
    let mut session_ctx = build_session_context(
        input.session_id,
        state.current_run_id.as_deref(),
        input.model_name,
        model_context_limit,
        input.runtime_signals.edge_profile,
        input.provider,
        state.project_context.as_deref(),
        Some(cache_cap),
        &session_current_date,
        state.context_manifest_user_id.as_deref(),
    );
    session_ctx.pre_reserved_output_tokens =
        u32::try_from(window_policy.reserved_output_tokens).unwrap_or(u32::MAX);
    if !input.tool_surface.deferred_tools_block.is_empty() {
        session_ctx.deferred_tools_block = input.tool_surface.deferred_tools_block.to_string();
    }
    // Prompt overrides are stable within one pipeline session, but a newly
    // created/restored session must observe the current override files. Cache
    // the compiled sections on PipelineSession rather than for the process.
    let statics = state
        .pipeline_session
        .as_mut()
        .expect("pipeline_session checked before context assembly")
        .static_sections_or_init(crate::prompts::build_pipeline_static_sections);
    let agent = AgentContext {
        tool_schemas: effective_tools,
        ..Default::default()
    };

    let adaptive = AdaptiveTurnInput {
        statics: &statics,
        agent: &agent,
        session: &session_ctx,
        turn: &turn_state,
        external: &external,
        model_id: input.model_name,
        query_source: input.query_source,
    };

    let pipeline_result = {
        let pipeline_sess = state
            .pipeline_session
            .as_mut()
            .expect("pipeline_session checked before context assembly");
        if let Some(session_id) = state.current_session_id.as_deref()
            && let Ok(session_dir) = match state.context_manifest_user_id.as_deref() {
                Some(user_id) => astra_services::OwnerScope::user(user_id).and_then(|owner| {
                    astra_services::local_session_artifact_store()
                        .session_dir_for_owner(&owner, session_id)
                }),
                None => astra_services::local_session_artifact_store().session_dir(session_id),
            }
        {
            pipeline_sess.set_prompt_cache_diff_dir(session_dir.join("prompt-cache-diffs"));
        }
        pipeline_sess.run_turn_adaptive_with_history_owner(
            adaptive,
            astra_turn_core::context_pipeline::HistoryOptimizationOwner::
                DownstreamSemanticCompactor,
        )
    };

    let mut pipeline_output = match pipeline_result {
        Ok(out) => out,
        Err(abort) => {
            record_pipeline_abort(state, &abort);
            return Err(classify_pipeline_abort(abort));
        }
    };
    // Adaptive optimization may remove optional schemas, but an explicitly
    // required request/delegation capability is part of the executable wire
    // contract. Restore only those already admitted and restriction-filtered
    // schemas after optimization; this cannot widen authority and keeps a
    // narrow child from losing its sole useful tool.
    let mut optimized_tool_names = pipeline_output
        .optimized
        .tool_schemas
        .iter()
        .filter_map(tool_name)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    for schema in input.tool_surface.required_tools {
        let Some(name) = tool_name(schema) else {
            continue;
        };
        if optimized_tool_names.insert(name.to_string()) {
            pipeline_output.optimized.tool_schemas.push(schema.clone());
        }
    }

    let plain = astra_turn_core::context_serializer::flatten_serialized_system_blocks(
        &pipeline_output.serialized,
    );
    let system_prompt_override = input
        .runtime_signals
        .edge_profile
        .get("system_prompt_override")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    let mut breakdown = astra_turn_core::context_assembly_trace::SystemPromptBreakdown {
        total_tokens: 0,
        repository_memories: prompt_memory_injections(&external.memory_entries),
        session_memory_injected: session_memory_injection(
            input.runtime_signals.session_memory_entry.as_ref(),
        ),
        context_signals: astra_turn_core::context_assembly_trace::PromptContextSignals {
            system_prompt_override,
            ..Default::default()
        },
        ..Default::default()
    };

    let round_within_turn = state.current_round_index;
    let inject_volatile = cache_cap.should_inject_volatile_on_round(round_within_turn);

    let (mut system_messages, mut volatile_preamble) = match cache_cap.volatile_placement {
        VolatilePlacement::MarkerIsolated => {
            let stable_content: Vec<Value> = pipeline_output
                .serialized
                .system_blocks
                .iter()
                .filter(|block| block.scope != CacheScope::None)
                .map(|block| {
                    let mut v = json!({
                        "type": "text",
                        "text": block.text,
                    });
                    if input.cache_cfg.should_annotate()
                        && let Some(ref cc) = block.cache_control
                    {
                        v["cache_control"] = cc.clone();
                    }
                    v
                })
                .collect();
            let volatile_text: String = pipeline_output
                .serialized
                .system_blocks
                .iter()
                .filter(|block| block.scope == CacheScope::None)
                .map(|block| block.text.as_str())
                .collect();
            let system = vec![json!({"role": "system", "content": stable_content})];
            let preamble = runtime_system_messages_from_text(volatile_text, inject_volatile);
            (system, preamble)
        }
        VolatilePlacement::CurrentUserOnly => {
            let mut stable_text = String::new();
            let mut volatile_text = String::new();
            for block in &pipeline_output.serialized.system_blocks {
                // Strict-history providers suppress ordinary volatile text to
                // keep prior messages byte-stable. Deferred-tool discovery is
                // different: it is a control-plane contract the model must
                // see in order to activate a tool. Keep that typed block in
                // the stable lane; changing it intentionally starts a new
                // cache epoch instead of silently hiding a capability.
                if block.scope != CacheScope::None
                    || block.kind == astra_turn_core::section_types::SectionKind::DeferredTools
                {
                    stable_text.push_str(&block.text);
                } else {
                    volatile_text.push_str(&block.text);
                }
            }
            let system = vec![json!({"role": "system", "content": stable_text})];
            let preamble = volatile_preamble_from_text(volatile_text, inject_volatile);
            (system, preamble)
        }
        VolatilePlacement::TailSuffix
        | VolatilePlacement::AppendOnlyUserTail
        | VolatilePlacement::Free => {
            let mut stable_text = String::new();
            let mut volatile_text = String::new();
            for block in &pipeline_output.serialized.system_blocks {
                if block.scope == CacheScope::None {
                    volatile_text.push_str(&block.text);
                } else {
                    stable_text.push_str(&block.text);
                }
            }
            let system = vec![json!({"role": "system", "content": stable_text})];
            let preamble = runtime_system_messages_from_text(volatile_text, inject_volatile);
            (system, preamble)
        }
    };
    if matches!(
        cache_cap.volatile_placement,
        VolatilePlacement::AppendOnlyUserTail
    ) {
        crate::turn::wire_assembly::ensure_append_only_runtime_authority_policy(
            &mut system_messages,
        );
    }
    volatile_preamble.splice(0..0, rehomed_append_only_authority);
    if let Some(required_text) = crate::turn::wire_assembly::required_runtime_preamble_message(
        &required_runtime_texts.join("\n\n"),
        crate::turn::wire_assembly::RuntimeAuthorityKind::EdgeRequiredContext,
        astra_turn_types::RuntimeAuthorityLifetime::CurrentUserTurn,
    ) {
        volatile_preamble.push(required_text);
    }
    volatile_preamble.extend(
        runtime_volatile_injections.into_iter()
        .filter(|injection| {
            matches!(
                injection.delivery_class,
                astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext
                    | astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::DecisionFeedback
            )
        })
        .filter_map(|injection| {
            crate::turn::wire_assembly::runtime_volatile_preamble_message(&injection)
        }),
    );
    let stable_system_message_count = system_messages.len();
    let volatile_preamble_count = volatile_preamble.len();
    let system_prompt_tokens = system_messages
        .iter()
        .map(estimate_json_tokens)
        .fold(0_u32, u32::saturating_add);
    breakdown.total_tokens = system_prompt_tokens;
    let tool_schema_count = pipeline_output.optimized.tool_schemas.len();
    let tier = pipeline_output.plan.compact_tier;
    let compaction_tier = format!("{:?}", tier);
    let model_context_window_tokens =
        u32::try_from(window_policy.raw_context_window_tokens).unwrap_or(u32::MAX);

    Ok(LlmContextAssemblyOutput {
        system_messages,
        volatile_preamble,
        messages: pipeline_output.optimized.messages,
        system_plain: plain,
        breakdown,
        tier,
        tool_schemas: pipeline_output.optimized.tool_schemas,
        manifest_trace: LlmContextManifestTrace {
            source: "llm_context",
            provider: input.provider.to_string(),
            model_name: input.model_name.to_string(),
            model_context_window_tokens,
            context_window_policy: window_policy,
            compaction_tier,
            system_prompt_tokens,
            stable_system_message_count,
            volatile_preamble_count,
            tool_schema_count,
            runtime_manifest: runtime_manifest_with_memory_context(
                state.runtime_manifest.clone(),
                input.runtime_signals.memory_provider_source,
                external.memory_entries.len(),
                input.runtime_signals.session_memory_entry.is_some(),
            ),
        },
    })
}

fn volatile_preamble_from_text(text: String, inject: bool) -> Vec<Value> {
    if !inject || text.is_empty() {
        return Vec::new();
    }
    vec![json!({
        "role": "user",
        "content": crate::turn::wire_assembly::system_reminder_wrapped_text(&text),
    })]
}

fn runtime_system_messages_from_text(text: String, inject: bool) -> Vec<Value> {
    if !inject || text.is_empty() {
        return Vec::new();
    }
    crate::turn::wire_assembly::runtime_system_context_message(&text, false)
        .into_iter()
        .collect()
}

fn record_pipeline_abort(
    state: &mut AgenticLoopState,
    abort: &astra_turn_core::context_pipeline::PipelineAbort,
) {
    let turn = state.current_session_turn_number();
    let alert = astra_turn_core::trace_alert::TraceAlert {
        severity: astra_turn_core::trace_alert::AlertSeverity::Error,
        rule: "system_prompt_abort".into(),
        message: abort.to_string(),
        turn,
    };
    if let Some(ref mut buf) = state.turn_event_buffer {
        let alert_evt = astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(&alert);
        if let Ok(payload) = serde_json::to_value(&alert_evt) {
            buf.record(
                astra_services::session_journal::JournalEvent::pipeline_alert(
                    state.current_session_id.as_deref(),
                    turn,
                    payload,
                )
                .with_producer_scope(state.current_run_id.as_deref()),
            );
        }
    }
    astra_core::agent_warn!(
        "pipeline",
        "pipeline aborted during system-message build: {abort}"
    );
}

fn classify_pipeline_abort(
    abort: astra_turn_core::context_pipeline::PipelineAbort,
) -> astra_core::ClassifiedError {
    let message = abort.to_string();
    let kind = match abort {
        astra_turn_core::context_pipeline::PipelineAbort::ConsecutivePtlExhausted { .. } => {
            astra_core::ErrorKind::ContextWindow
        }
        astra_turn_core::context_pipeline::PipelineAbort::InvalidModelLimit { .. } => {
            astra_core::ErrorKind::InvalidRequest
        }
    };
    astra_core::ClassifiedError::new(kind, message)
}

/// Assemble final wire-ready LLM messages from the shared pipeline output and
/// runtime state.
///
/// This centralizes state-derived post-compaction attachments so CLI and web
/// paths can share the same message ordering and cache-sensitive volatile
/// placement instead of each host rebuilding this logic.
pub(crate) fn assemble_wire_messages(
    input: LlmWireAssemblyInput<'_>,
) -> Result<Vec<Value>, astra_core::ClassifiedError> {
    // The real user message is already in prompt history after the first
    // boundary. Repeating the full goal plus a changing round id on every
    // tool round creates an avoidable uncached suffix. Re-emit it for a
    // genuine compaction boundary so a rewritten history still has an
    // explicit current-turn anchor.
    if input.state.current_round_index == 0 || input.compaction_boundary_hit {
        queue_active_turn_frame(input.state);
    }
    let drained = input.state.lease_volatile_pending()?;

    let mut skills: Vec<_> = input.state.skills.invoked.values().collect();
    skills.sort_by_key(|skill| std::cmp::Reverse(skill.invoked_at_turn));
    let invoked_skills: Vec<crate::turn::wire_assembly::InvokedSkillRef<'_>> = skills
        .iter()
        .map(|skill| crate::turn::wire_assembly::InvokedSkillRef {
            name: skill.name.as_str(),
            content: skill.content.as_str(),
        })
        .collect();
    let attachments = crate::turn::wire_assembly::PostCompactAttachments { invoked_skills };

    let assembly =
        match crate::turn::wire_assembly::assemble_llm_messages_with_cache_capability_output(
            input.system_messages,
            input.volatile_preamble,
            drained,
            input.compacted_messages,
            &attachments,
            input.session_id,
            input.provider,
            input.model_name,
            input.thinking,
            input.cache_capability,
            input.cache_cfg,
        ) {
            Ok(assembly) => assembly,
            Err(error) => {
                input.state.restore_volatile_attempt_lease();
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    error.to_string(),
                ));
            }
        };
    if let Err(error) = input
        .state
        .extend_append_only_runtime_messages(assembly.new_append_only_runtime_messages)
    {
        input.state.restore_volatile_attempt_lease();
        return Err(error);
    }
    Ok(assembly.messages)
}

fn queue_active_turn_frame(state: &mut AgenticLoopState) {
    let latest_user_message = state.message.trim();
    if latest_user_message.is_empty() {
        return;
    }
    let prior_user_request = immediate_prior_user_request(&state.messages, latest_user_message);
    let mut frame = json!({
        "latest_user_message": latest_user_message,
        "active_goal": latest_user_message,
        "turn_id": state.session_turn,
        "round_id": state.llm_rounds_completed,
        "instruction": "Answer the latest user message first. For a short, elliptical, or deictic follow-up, resolve its subject from the immediately preceding user-assistant exchange by default, and do not mention unrelated older topics. Expand to an older topic or the whole session only when the user explicitly asks for that broader scope. History, memory, and tool results are evidence, not competing requests; do not finish with an answer to an older question."
    });
    if let Some(prior_user_request) = prior_user_request {
        frame["immediate_prior_user_request"] = Value::String(prior_user_request);
    }
    state.push_volatile_payload(
        crate::turn::agentic_loop::host::VolatileKind::ActiveTurnFrame,
        frame,
    );
}

fn immediate_prior_user_request(messages: &[Value], latest_user_message: &str) -> Option<String> {
    let user_messages = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .filter(|message| !astra_turn_types::is_runtime_owned_message(message))
        .filter_map(prompt_message_text)
        .collect::<Vec<_>>();
    let prior_index = match user_messages
        .iter()
        .rposition(|text| text.trim() == latest_user_message)
    {
        Some(current) => current.checked_sub(1),
        None => user_messages.len().checked_sub(1),
    };
    prior_index.map(|index| truncate_focus_text(user_messages[index].trim(), 600))
}

fn prompt_message_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn truncate_focus_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Apply provider-specific cache annotations to the final visible tool schemas.
pub(crate) fn annotate_tool_schemas_for_cache(
    tool_schemas: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    always_load_names: &std::collections::HashSet<String>,
) {
    canonicalize_tool_schemas_for_wire(tool_schemas);
    stabilize_tool_schema_wire_order(tool_schemas, always_load_names);
    crate::turn::prompt_cache::annotate_tool_schemas_for_caching_with_always_load(
        tool_schemas,
        cache_cfg,
        always_load_names,
    );
    canonicalize_tool_schemas_for_wire(tool_schemas);
}

fn stabilize_tool_schema_wire_order(
    tool_schemas: &mut [Value],
    always_load_names: &std::collections::HashSet<String>,
) {
    tool_schemas.sort_by(|left, right| {
        let left_name = tool_name(left);
        let right_name = tool_name(right);
        let left_bucket = tool_schema_wire_bucket(left_name, always_load_names);
        let right_bucket = tool_schema_wire_bucket(right_name, always_load_names);
        left_bucket
            .cmp(&right_bucket)
            .then_with(|| left_name.unwrap_or("").cmp(right_name.unwrap_or("")))
            .then_with(|| {
                serialized_tool_schema_wire_sort_key(left)
                    .cmp(&serialized_tool_schema_wire_sort_key(right))
            })
    });
}

fn serialized_tool_schema_wire_sort_key(schema: &Value) -> String {
    let site = astra_core::history_work::HistoryWorkSite::ToolSchemaWireSortSerialization;
    match serde_json::to_string(schema) {
        Ok(serialized) => {
            if astra_core::history_work::instrumentation_enabled() {
                astra_core::history_work::record_bytes(
                    site,
                    serialized.len().try_into().unwrap_or(u64::MAX),
                );
            }
            serialized
        }
        Err(error) => {
            astra_core::history_work::record_serialization_failure(site, &error);
            String::new()
        }
    }
}

fn tool_schema_wire_bucket(
    name: Option<&str>,
    always_load_names: &std::collections::HashSet<String>,
) -> u8 {
    match name {
        Some(name) if always_load_names.contains(name) => 0,
        Some(_) => 1,
        None => 2,
    }
}

fn canonicalize_tool_schemas_for_wire(tool_schemas: &mut [Value]) {
    for schema in tool_schemas {
        canonicalize_json_for_wire(schema);
    }
}

fn canonicalize_json_for_wire(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                canonicalize_json_for_wire(item);
            }
        }
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|(left_key, _), (right_key, _)| left_key.cmp(right_key));

            let mut canonical = Map::new();
            for (key, mut value) in entries {
                canonicalize_json_for_wire(&mut value);
                canonical.insert(key, value);
            }
            *map = canonical;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

pub(crate) fn stabilize_tool_schemas_for_cache(
    current_tool_schemas: &[Value],
    previous_tool_schemas: &[Value],
    visible_tool_schemas: &[Value],
    cache_capability: astra_turn_core::cache_placement::CacheCapability,
    round_in_turn: u32,
) -> Vec<Value> {
    if round_in_turn == 0
        || previous_tool_schemas.is_empty()
        || matches!(
            cache_capability.protocol,
            astra_turn_core::cache_placement::CacheProtocol::None
        )
    {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::ToolSchemaCacheStabilizationClone,
            current_tool_schemas,
        );
        return current_tool_schemas.to_vec();
    }

    let visible_names: HashSet<&str> = visible_tool_schemas.iter().filter_map(tool_name).collect();
    let current_by_name = current_tool_schemas
        .iter()
        .filter_map(|schema| tool_name(schema).map(|name| (name, schema)))
        .collect::<BTreeMap<_, _>>();
    let mut stabilized = Vec::new();
    let mut seen = HashSet::new();

    // Previous declarations contribute ordering only. Their JSON schema is
    // not authority: a same-named tool may have lost an action or parameter
    // branch since the preceding request. Reusing the old value would put a
    // forbidden action back on the provider wire merely to preserve cache
    // bytes, while the terminal admission gate correctly rejects it. Keep the
    // stable name order but always publish the current authoritative schema.
    for previous_schema in previous_tool_schemas {
        let Some(name) = tool_name(previous_schema) else {
            continue;
        };
        if visible_names.contains(name)
            && let Some(current_schema) = current_by_name.get(name)
        {
            push_unique_tool(&mut stabilized, &mut seen, current_schema);
        }
    }
    for schema in current_tool_schemas {
        push_unique_tool(&mut stabilized, &mut seen, schema);
    }

    if stabilized.is_empty() {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::ToolSchemaCacheStabilizationClone,
            current_tool_schemas,
        );
        current_tool_schemas.to_vec()
    } else {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::ToolSchemaCacheStabilizationClone,
            &stabilized,
        );
        stabilized
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireTraceDetail {
    MetricsOnly,
    Debug,
}

#[cfg(test)]
fn augment_manifest_trace_with_wire(trace: &mut Value, messages: &[Value], tool_schemas: &[Value]) {
    augment_manifest_trace_with_wire_detail(trace, messages, tool_schemas, WireTraceDetail::Debug);
}

/// Add final pre-provider request facts without making production
/// observability another history-sized payload.
///
/// The metrics path walks the already-materialized message slice once for
/// low-cardinality counts and hashes only the stable system/tool prefix.
/// Per-message hashes and whole-request canonical clones are debug capture:
/// they are useful diagnostically, but scale with the complete session and
/// must not be paid on every request in a long-running production session.
pub(crate) fn augment_manifest_trace_with_wire_detail(
    trace: &mut Value,
    messages: &[Value],
    tool_schemas: &[Value],
    detail: WireTraceDetail,
) {
    let message_cache_control_count = messages.iter().map(cache_control_count).sum::<usize>();
    let tool_cache_control_count = tool_schemas.iter().map(cache_control_count).sum::<usize>();
    let mut message_role_counts = BTreeMap::<String, usize>::new();
    let mut conversation_role_counts = BTreeMap::<String, usize>::new();
    let mut conversation_message_count = 0_usize;
    let mut system_messages = Vec::new();
    let mut leading_system_messages = Vec::new();
    let mut leading_system_prefix_open = true;
    for message in messages {
        let role = message_role(message);
        *message_role_counts.entry(role.clone()).or_default() += 1;
        if role == "system" {
            system_messages.push(message.clone());
            if leading_system_prefix_open {
                leading_system_messages.push(message.clone());
            }
        } else {
            leading_system_prefix_open = false;
            conversation_message_count = conversation_message_count.saturating_add(1);
            *conversation_role_counts.entry(role).or_default() += 1;
        }
    }
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
        &system_messages,
    );
    let stable_system_prefix = stable_cache_prefix(&leading_system_messages);
    let stable_tool_prefix = stable_cache_prefix(tool_schemas);
    let cache_layout = if message_cache_control_count + tool_cache_control_count > 0 {
        "explicit-breakpoints-v1"
    } else {
        "provider-prefix-v1"
    };
    let prompt_cache_identity = astra_turn_types::PromptCacheIdentityV1::from_prefixes(
        &stable_system_prefix,
        &stable_tool_prefix,
        cache_layout,
    )
    .expect("wire prompt cache identity inputs are bounded constants and JSON");

    if let Some(trace_obj) = trace.as_object_mut() {
        let mut wire = serde_json::json!({
            "projection_authority": "planned_pre_client_projection_v1",
            "trace_detail": match detail {
                WireTraceDetail::MetricsOnly => "metrics_only",
                WireTraceDetail::Debug => "debug",
            },
            "message_count": messages.len(),
            "leading_system_message_count": leading_system_messages.len(),
            "tool_schema_count": tool_schemas.len(),
            "message_role_counts": message_role_counts,
            "message_cache_control_count": message_cache_control_count,
            "tool_cache_control_count": tool_cache_control_count,
            "total_cache_control_count": message_cache_control_count + tool_cache_control_count,
            "conversation_projection": {
                "message_count": conversation_message_count,
                "role_counts": conversation_role_counts,
            },
            "fingerprint": {
                "prompt_cache_identity": prompt_cache_identity,
            },
        });
        if detail == WireTraceDetail::Debug {
            let message_roles: Vec<String> = messages.iter().map(message_role).collect();
            let conversation_messages: Vec<Value> = messages
                .iter()
                .filter(|message| message_role(message) != "system")
                .cloned()
                .collect();
            let conversation_roles: Vec<String> =
                conversation_messages.iter().map(message_role).collect();
            let message_hashes: Vec<Value> = messages
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    json!({
                        "index": index,
                        "role": message_role(message),
                        "sha256": canonical_wire_sha256(message),
                    })
                })
                .collect();
            let estimated_message_tokens: u64 = messages
                .iter()
                .map(estimate_json_tokens)
                .map(u64::from)
                .sum();
            let estimated_conversation_tokens: u64 = conversation_messages
                .iter()
                .map(estimate_json_tokens)
                .map(u64::from)
                .sum();
            astra_core::history_work::record_serialized_value(
                astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
                messages,
            );
            astra_core::history_work::record_serialized_value(
                astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
                &conversation_messages,
            );
            astra_core::history_work::record_serialized_value(
                astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
                tool_schemas,
            );
            wire["message_roles"] = json!(message_roles);
            wire["estimated_message_tokens"] = json!(estimated_message_tokens);
            wire["conversation_projection"]["message_roles"] = json!(conversation_roles);
            wire["conversation_projection"]["estimated_tokens"] =
                json!(estimated_conversation_tokens);
            wire["fingerprint"]["message_sequence_sha256"] =
                json!(canonical_wire_sha256(&Value::Array(messages.to_vec())));
            wire["fingerprint"]["system_message_sequence_sha256"] = json!(canonical_wire_sha256(
                &Value::Array(system_messages.clone())
            ));
            wire["fingerprint"]["conversation_message_sequence_sha256"] =
                json!(canonical_wire_sha256(&Value::Array(conversation_messages)));
            wire["fingerprint"]["tool_schema_sequence_sha256"] =
                json!(canonical_wire_sha256(&Value::Array(tool_schemas.to_vec())));
            wire["fingerprint"]["system_and_tools_sha256"] = json!(canonical_wire_sha256(&json!({
                "system_messages": system_messages,
                "tool_schemas": tool_schemas,
            })));
            wire["fingerprint"]["message_hashes"] = json!(message_hashes);
        }
        trace_obj.insert("wire".to_string(), wire);
    }
}

fn stable_cache_prefix(values: &[Value]) -> Vec<Value> {
    let prefix = match values
        .iter()
        .rposition(|value| cache_control_count(value) > 0)
    {
        Some(last_marker) => &values[..=last_marker],
        None => values,
    };
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::LlmWireTraceClone,
        prefix,
    );
    prefix.to_vec()
}

#[cfg(test)]
mod context_cache_contract_tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn tool(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("tool {name}"),
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    fn tool_names(tools: &[Value]) -> Vec<String> {
        tools
            .iter()
            .filter_map(|tool| tool_name(tool).map(str::to_string))
            .collect()
    }

    #[test]
    fn estimate_json_tokens_does_not_discount_dense_unicode_text() {
        let message = json!({
            "role": "user",
            "content": "你好世界🚀🔥💻"
        });

        let estimate = estimate_json_tokens(&message);
        let content_floor = astra_turn_core::section_types::estimate_text_tokens("你好世界🚀🔥💻");

        assert!(
            estimate >= content_floor,
            "json estimate must preserve the shared text-token floor: estimate={estimate}, floor={content_floor}"
        );
    }

    #[test]
    fn session_memory_injection_uses_shared_unicode_token_estimate() {
        let entry = astra_turn_core::context_sources::MemoryEntry::new("你好世界🚀🔥💻")
            .with_source("session_memory.reanchor");

        let injection =
            session_memory_injection(Some(&entry)).expect("non-empty memory entry should inject");

        assert_eq!(
            injection.tokens,
            astra_turn_core::section_types::estimate_text_tokens(&entry.content).max(1)
        );
        assert_eq!(injection.relevance_score, 1.0);
    }

    #[test]
    fn prompt_memory_trace_preserves_backend_identity_and_type() {
        let entry = astra_turn_core::context_sources::MemoryEntry::scored(
            "typed compact recall evidence",
            0.87,
        )
        .with_memory_identity("memory-42", "semantic")
        .with_source("memoria.prefetch");
        let trace = prompt_memory_injections(&[entry]);
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].memory_id, "memory-42");
        assert_eq!(trace[0].memory_type, "semantic");
        assert_eq!(trace[0].relevance_score, 0.87);
    }

    #[test]
    fn pipeline_abort_journal_event_uses_session_turn() {
        let mut state = crate::turn::agentic_loop::host::tests::make_state();
        state.current_session_id = Some("session-1".to_string());
        state.current_run_id = Some("run-1".to_string());
        state.session_turn = 9;
        state.llm_rounds_completed = 1;
        state.turn_event_buffer = Some(
            astra_services::session_journal::TurnEventBuffer::begin_turn(
                state.current_session_id.as_deref(),
                state.session_turn,
            ),
        );

        record_pipeline_abort(
            &mut state,
            &astra_turn_core::context_pipeline::PipelineAbort::InvalidModelLimit { model_limit: 0 },
        );

        let mut buffer = state
            .turn_event_buffer
            .take()
            .expect("pipeline abort should be buffered");
        let events = buffer.drain();
        let alert_event = events
            .iter()
            .find(|event| {
                event.event_type == astra_services::session_journal::JournalEventType::PipelineAlert
            })
            .expect("pipeline alert event");
        assert_eq!(alert_event.turn, Some(9));
        assert_eq!(
            alert_event
                .producer_scope
                .as_ref()
                .map(|scope| scope.run_id.as_str()),
            Some("run-1")
        );
        assert_eq!(
            alert_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("turn"))
                .and_then(|turn| turn.as_u64()),
            Some(9)
        );
    }

    fn message_text(message: &Value) -> String {
        match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    #[test]
    fn normalized_volatile_delivery_maps_without_behavior_guessing() {
        let capability =
            cache_capability_from_model_metadata(Some(astra_services::PromptCacheCapabilityData {
                protocol: astra_services::PromptCacheProtocolData::StrictHistoryMatch,
                volatile_placement:
                    astra_services::PromptCacheVolatilePlacementData::CurrentUserOnly,
                volatile_delivery: astra_services::PromptCacheVolatileDeliveryData::All,
                reuse_scope: None,
            }))
            .expect("declared capability");

        assert_eq!(
            capability.volatile_delivery,
            astra_turn_core::cache_placement::VolatileDeliveryPolicy::All,
            "runtime must map the total metadata value without inferring a behavior policy"
        );
    }

    #[test]
    fn declared_volatile_delivery_survives_model_metadata_mapping() {
        let capability =
            cache_capability_from_model_metadata(Some(astra_services::PromptCacheCapabilityData {
                protocol: astra_services::PromptCacheProtocolData::OpenAiAutoPrefix,
                volatile_placement: astra_services::PromptCacheVolatilePlacementData::TailSuffix,
                volatile_delivery: astra_services::PromptCacheVolatileDeliveryData::RequiredOnly,
                reuse_scope: Some(astra_services::PromptCacheReuseScopeData::ConversationTurns),
            }))
            .expect("declared capability");

        assert_eq!(
            capability.protocol,
            astra_turn_core::cache_placement::CacheProtocol::OpenAiAutoPrefix
        );
        assert_eq!(
            capability.volatile_placement,
            astra_turn_core::cache_placement::VolatilePlacement::TailSuffix
        );
        assert_eq!(
            capability.volatile_delivery,
            astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly
        );
        assert_eq!(
            capability.reuse_scope,
            Some(astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns)
        );
    }

    fn tool_with_parameter_insert_order(name: &str, parameter_names: &[&str]) -> Value {
        let mut properties = Map::new();
        for parameter_name in parameter_names {
            properties.insert(
                (*parameter_name).to_string(),
                json!({"type": "string", "description": format!("{name} {parameter_name}")}),
            );
        }

        let mut parameters = Map::new();
        parameters.insert("properties".to_string(), Value::Object(properties));
        parameters.insert("type".to_string(), Value::String("object".to_string()));

        let mut function = Map::new();
        function.insert("parameters".to_string(), Value::Object(parameters));
        function.insert(
            "description".to_string(),
            Value::String(format!("tool {name}")),
        );
        function.insert("name".to_string(), Value::String(name.to_string()));

        let mut schema = Map::new();
        schema.insert("function".to_string(), Value::Object(function));
        schema.insert("type".to_string(), Value::String("function".to_string()));
        Value::Object(schema)
    }

    fn strict_history_cache_capability() -> astra_turn_core::cache_placement::CacheCapability {
        astra_turn_core::cache_placement::CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement:
                astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
            volatile_delivery:
                astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: Some(astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns),
        }
    }

    #[test]
    fn final_tool_wire_list_is_byte_stable_across_input_order() {
        let mut left = vec![tool("aaa_dynamic"), tool("read_file"), tool("bash")];
        let mut right = vec![tool("bash"), tool("aaa_dynamic"), tool("read_file")];
        let always_load_names = HashSet::from(["bash".to_string(), "read_file".to_string()]);
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };

        annotate_tool_schemas_for_cache(&mut left, &cache_cfg, &always_load_names);
        annotate_tool_schemas_for_cache(&mut right, &cache_cfg, &always_load_names);

        assert_eq!(tool_names(&left), vec!["bash", "read_file", "aaa_dynamic"]);
        assert_eq!(tool_names(&right), tool_names(&left));
        assert_eq!(
            serde_json::to_vec(&left).expect("tool list serializes"),
            serde_json::to_vec(&right).expect("tool list serializes"),
            "final tool list must be byte-stable for the same CLI/provider surface"
        );
        assert!(left[0].get("cache_control").is_none());
        assert_eq!(
            left[1]["cache_control"]["type"].as_str(),
            Some("ephemeral"),
            "cache marker stays on the last always-load tool in the stable prefix"
        );
        assert!(left[2].get("cache_control").is_none());
    }

    #[test]
    fn final_tool_wire_list_is_byte_stable_across_multi_round_cli_surface() {
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let always_load_names = HashSet::from(["bash".to_string(), "read_file".to_string()]);
        let round_zero_current = vec![
            tool_with_parameter_insert_order("read_file", &["path", "start_line"]),
            tool_with_parameter_insert_order("aaa_dynamic", &["query", "limit"]),
            tool_with_parameter_insert_order("bash", &["cmd", "timeout"]),
        ];
        let round_zero_visible = round_zero_current.clone();
        let round_zero_sticky = stabilize_tool_schemas_for_cache(
            &round_zero_current,
            &[],
            &round_zero_visible,
            strict_history_cache_capability(),
            0,
        );
        let mut round_zero_wire = round_zero_sticky.clone();
        annotate_tool_schemas_for_cache(&mut round_zero_wire, &cache_cfg, &always_load_names);

        let round_one_current = vec![
            tool_with_parameter_insert_order("bash", &["timeout", "cmd"]),
            tool_with_parameter_insert_order("read_file", &["start_line", "path"]),
            tool_with_parameter_insert_order("aaa_dynamic", &["limit", "query"]),
        ];
        let round_one_visible = round_one_current.clone();
        let mut round_one_wire = stabilize_tool_schemas_for_cache(
            &round_one_current,
            &round_zero_sticky,
            &round_one_visible,
            strict_history_cache_capability(),
            1,
        );
        annotate_tool_schemas_for_cache(&mut round_one_wire, &cache_cfg, &always_load_names);

        assert_eq!(
            tool_names(&round_zero_wire),
            vec!["bash", "read_file", "aaa_dynamic"]
        );
        assert_eq!(tool_names(&round_one_wire), tool_names(&round_zero_wire));
        assert_eq!(
            serde_json::to_vec(&round_zero_wire).expect("round zero tools serialize"),
            serde_json::to_vec(&round_one_wire).expect("round one tools serialize"),
            "a stable CLI/provider surface must produce byte-identical final tool lists across rounds"
        );
        assert!(round_one_wire[0].get("cache_control").is_none());
        assert_eq!(
            round_one_wire[1]["cache_control"]["type"].as_str(),
            Some("ephemeral"),
            "the cache marker remains on the deterministic always-load prefix boundary"
        );
        assert!(round_one_wire[2].get("cache_control").is_none());
    }

    #[test]
    fn effective_tool_schemas_merges_priority_dedupes_and_filters_restricted() {
        let visible = vec![tool("visible_a"), tool("always_load_a"), tool("dynamic_a")];
        let always_load = vec![tool("always_load_a"), tool("always_load_b")];
        let dynamic = vec![tool("dynamic_a"), tool("dynamic_b")];
        let required = vec![tool("required_a"), tool("always_load_b")];
        let restricted = HashSet::from(["dynamic_b".to_string()]);

        let merged = filter_restricted_tool_schemas(
            effective_tool_schemas(&visible, &always_load, &dynamic, &required),
            &restricted,
        );

        assert_eq!(
            tool_names(&merged),
            vec![
                "always_load_a",
                "always_load_b",
                "required_a",
                "dynamic_a",
                "visible_a"
            ]
        );
    }

    #[test]
    fn assemble_context_pipeline_keeps_required_runtime_system_context_for_strict_history() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state
            .messages
            .push(json!({"role": "user", "content": "which model are you?"}));
        let history_before = state.messages.clone();
        let runtime_policy =
            "Terminal Control Policy: the delegated action must be first and terminal.";
        let mut edge_profile = serde_json::Map::new();
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS
                .to_string(),
            json!([runtime_policy]),
        );
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS
                .to_string(),
            json!(["## Volatile\nmust be suppressed"]),
        );
        let visible_tools = vec![tool("bash")];
        let restricted_tools = HashSet::new();
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let strict_history = astra_turn_core::cache_placement::CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement:
                astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
            volatile_delivery:
                astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: None,
        };

        let output = assemble_context_pipeline(LlmContextAssemblyInput {
            state: &mut state,
            session_id: "sid-deepseek",
            tool_surface: ToolSurfacePlan::from_visible_tools(&visible_tools, &restricted_tools),
            runtime_signals: RuntimeSignals::new(&edge_profile, None),
            cache_cfg: &cache_cfg,
            provider: "openai",
            model_name: "deepseek-v4-pro-official(thinking:high)",
            context_window: Some(1_000_000),
            max_completion_tokens: Some(64_000),
            cache_capability: Some(strict_history),
            user_content: "which model are you?",
            query_source: "test",
        })
        .expect("context pipeline should assemble");

        let primary_text = output
            .system_messages
            .first()
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            !primary_text.contains("Model: deepseek-v4-pro-official(thinking:high)"),
            "runtime model identity must stay out of the strict-history prefix: {primary_text}"
        );
        assert!(!primary_text.contains("via openai"));
        assert!(
            !primary_text.contains("must be suppressed"),
            "ordinary volatile content must stay out of strict-history stable prompt: {primary_text}"
        );
        assert!(
            primary_text.contains("## Tool Availability Protocol"),
            "strict-history providers must retain the generic typed availability contract: {primary_text}"
        );
        assert!(
            !primary_text.contains("<deferred-tools>")
                && !primary_text.contains("## Durable Work")
                && !primary_text.contains("`start_work` is the first tool call"),
            "the prompt must not advertise deferred or Work capabilities absent from the admitted tool surface: {primary_text}"
        );
        assert_eq!(
            output.volatile_preamble.len(),
            1,
            "strict-history placement suppresses all ordinary dynamic context"
        );
        assert!(output.volatile_preamble.iter().all(|message| {
            !message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .contains("Model:")
        }));
        let required_context = output
            .volatile_preamble
            .iter()
            .find(|message| crate::turn::wire_assembly::is_required_runtime_preamble(message))
            .expect("required runtime context must remain independently typed");
        assert_eq!(
            required_context["role"].as_str(),
            Some("system"),
            "required control policy must remain runtime-owned before provider wire adaptation"
        );
        assert_eq!(
            required_context["content"].as_str(),
            Some(runtime_policy),
            "required control policy must survive strict-history suppression"
        );
        assert_eq!(
            state.messages, history_before,
            "required runtime policy must not enter persistent conversation history"
        );
        assert_eq!(
            output.messages, history_before,
            "normal-pressure pipeline output must preserve the conversation history"
        );
        assert_eq!(
            output.breakdown.total_tokens,
            output
                .system_messages
                .iter()
                .map(estimate_json_tokens)
                .fold(0_u32, u32::saturating_add),
            "stable system prompt telemetry must not absorb volatile runtime bytes"
        );
        assert_eq!(output.manifest_trace.volatile_preamble_count, 1);
        assert_eq!(
            output.manifest_trace.system_prompt_tokens,
            output.breakdown.total_tokens
        );
        assert!(state.messages.iter().all(|message| {
            !message
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains(runtime_policy))
        }));
        let manifest_context_window =
            output.manifest_trace.to_json()["model_context_window_tokens"].clone();

        let wire = crate::turn::wire_assembly::assemble_llm_messages_with_cache_capability(
            output.system_messages,
            output.volatile_preamble,
            Vec::new(),
            output.messages,
            &crate::turn::wire_assembly::PostCompactAttachments::default(),
            "sid-deepseek",
            "openai",
            "deepseek-v4-pro-official(thinking:high)",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            Some(strict_history),
            &cache_cfg,
        );
        assert!(
            wire.iter().any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains(runtime_policy))
            }),
            "required control policy must reach the strict-history provider wire: {wire:#?}"
        );
        assert_eq!(
            wire.last(),
            Some(&json!({"role": "user", "content": "which model are you?"}))
        );
        assert!(
            wire[..wire.len() - 1]
                .iter()
                .all(|message| { message.get("role").and_then(Value::as_str) == Some("system") })
        );
        assert_eq!(
            state.messages, history_before,
            "wire assembly must not persist required runtime policy as user history"
        );
        assert_eq!(
            manifest_context_window,
            json!(1_000_000),
            "context assembly must use registry context_window, not model-name heuristics or 200K default"
        );
    }

    #[test]
    fn assemble_context_pipeline_rejects_missing_pipeline_session() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.pipeline_session = None;
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        let edge_profile = serde_json::Map::new();
        let visible_tools = vec![tool("bash")];
        let restricted_tools = HashSet::new();
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };

        let err = match assemble_context_pipeline(LlmContextAssemblyInput {
            state: &mut state,
            session_id: "sid-missing-pipeline",
            tool_surface: ToolSurfacePlan::from_visible_tools(&visible_tools, &restricted_tools),
            runtime_signals: RuntimeSignals::new(&edge_profile, None),
            cache_cfg: &cache_cfg,
            provider: "openai",
            model_name: "gpt-4",
            context_window: Some(200_000),
            max_completion_tokens: Some(16_384),
            cache_capability: None,
            user_content: "hello",
            query_source: "test",
        }) {
            Ok(_) => panic!("missing pipeline session must be a lifecycle error"),
            Err(err) => err,
        };

        assert_eq!(err.kind, astra_core::ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("pipeline_session missing"),
            "error must identify the lifecycle invariant, got {err}"
        );
    }

    #[test]
    fn assemble_wire_messages_auto_injects_active_turn_frame() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.message = "相关的测试够硬核吗？".to_string();
        state.user_intent = state.message.clone();
        state.session_turn = 7;
        state.llm_rounds_completed = 3;
        state.messages = vec![
            json!({"role": "user", "content": "一共多少 changes？"}),
            json!({"role": "assistant", "content": "148 files"}),
            json!({"role": "user", "content": "相关的测试够硬核吗？"}),
        ];
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
        let cache_cfg = PromptCacheConfig::latch("openai");

        let messages = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: state.messages.clone(),
            state: &mut state,
            compaction_boundary_hit: false,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "gpt-4",
            cache_capability: None,
            cache_cfg: &cache_cfg,
        })
        .unwrap();

        let user_text = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(user_text.contains("相关的测试够硬核吗"));
        assert_eq!(
            messages[4],
            json!({"role": "user", "content": "相关的测试够硬核吗？"})
        );
        assert_eq!(messages[3]["role"], "system");
        let runtime_system_text = message_text(&messages[3]);
        assert!(runtime_system_text.contains("<runtime-required-context>"));
        assert!(runtime_system_text.contains("\"turn_id\":7"));
        assert!(runtime_system_text.contains("\"round_id\":3"));
        assert!(!message_text(&messages[0]).contains("<runtime-required-context>"));
        assert_eq!(state.volatile_pending.len(), 1);
        assert!(state.volatile_pending[0].attempt_leased);
        state.commit_volatile_attempt_lease();
        assert!(state.volatile_pending.is_empty());
    }

    #[test]
    fn failed_wire_assembly_restores_pending_authority_transactionally() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.current_round_index = 1;
        state.message = "finish".to_string();
        state.messages = vec![json!({"role": "user", "content": "finish"})];
        state.push_volatile_payload(
            crate::turn::agentic_loop::host::VolatileKind::FinalAnswerSettlement,
            json!({"reason": "post_mutation_observation_required"}),
        );
        let pending_before = state.volatile_pending.clone();
        let canonical_before = state.messages.clone();
        let capability = astra_turn_core::cache_placement::CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::OpenAiAutoPrefix,
            volatile_placement:
                astra_turn_core::cache_placement::VolatilePlacement::AppendOnlyUserTail,
            volatile_delivery:
                astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: None,
        };
        let cache_cfg = PromptCacheConfig::from_cache_capability(Some(capability), "openai");
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
        let malformed = crate::turn::wire_assembly::runtime_system_context_message(
            "missing typed authority kind",
            true,
        )
        .unwrap();

        let error = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: vec![malformed],
            compacted_messages: state.messages.clone(),
            state: &mut state,
            compaction_boundary_hit: false,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "alias",
            cache_capability: Some(capability),
            cache_cfg: &cache_cfg,
        })
        .unwrap_err();
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(state.messages, canonical_before);
        assert_eq!(state.volatile_pending.len(), pending_before.len());
        assert_eq!(state.volatile_pending[0].kind, pending_before[0].kind);
        assert_eq!(state.volatile_pending[0].payload, pending_before[0].payload);
        assert!(!state.volatile_pending[0].attempt_leased);

        let wire = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: state.messages.clone(),
            state: &mut state,
            compaction_boundary_hit: false,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "alias",
            cache_capability: Some(capability),
            cache_cfg: &cache_cfg,
        })
        .expect("retry must retain and deliver the original authority");
        assert_eq!(state.volatile_pending.len(), 1);
        assert!(state.volatile_pending[0].attempt_leased);
        assert!(state.messages.iter().any(|message| {
            astra_turn_types::runtime_authority_kind(message) == Some("final_answer_settlement")
        }));
        assert!(wire.iter().any(|message| {
            message_text(message).contains("post_mutation_observation_required")
        }));
        state.commit_volatile_attempt_lease();
        assert!(state.volatile_pending.is_empty());
    }

    #[test]
    fn active_turn_frame_anchors_elliptical_follow_up_to_immediate_exchange() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.message = "问题总结？".to_string();
        state.session_turn = 9;
        state.messages = vec![
            json!({"role": "user", "content": "分析整个 task 系统"}),
            json!({"role": "assistant", "content": "旧话题分析"}),
            json!({"role": "user", "content": "不要修改，只读 review uncommitted changes"}),
            json!({"role": "assistant", "content": "发现 cursor pagination 和测试覆盖问题"}),
            json!({"role": "user", "content": "问题总结？"}),
        ];
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
        let cache_capability = strict_history_cache_capability();
        let cache_cfg = PromptCacheConfig::from_cache_capability(Some(cache_capability), "openai");
        let messages = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: state.messages.clone(),
            state: &mut state,
            compaction_boundary_hit: false,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "deployment-alias",
            cache_capability: Some(cache_capability),
            cache_cfg: &cache_cfg,
        })
        .unwrap();

        assert_eq!(
            messages.last(),
            Some(&json!({"role": "user", "content": "问题总结？"})),
            "runtime focus context must not be appended to user speech"
        );
        let focus_policy = messages
            .iter()
            .rev()
            .find(|message| {
                message.get("role").and_then(Value::as_str) == Some("system")
                    && message_text(message).contains("<runtime-focus-policy>")
            })
            .map(message_text)
            .expect("stable strict-history focus policy");
        assert!(focus_policy.contains("immediately preceding user-assistant exchange"));
        assert!(focus_policy.contains("explicitly broadens the scope"));
        assert!(!focus_policy.contains("问题总结？"));
        assert!(!focus_policy.contains("不要修改，只读 review uncommitted changes"));
        assert!(messages.iter().any(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && message_text(message) == "不要修改，只读 review uncommitted changes"
        }));
    }

    #[test]
    fn edge_profile_active_turn_frame_cannot_churn_strict_provider_prefix() {
        fn provider_messages(frame_value: &str) -> Vec<Value> {
            let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
            state.messages = vec![
                json!({"role": "user", "content": "review the current change"}),
                json!({"role": "assistant", "content": "I found one issue"}),
                json!({"role": "user", "content": "summarize it"}),
            ];
            let mut edge_profile = serde_json::Map::new();
            edge_profile.insert(
                astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS
                    .to_string(),
                json!([astra_turn_core::chat_turn_edge_profile::RuntimeVolatileInjection {
                    kind: "active_turn_frame".to_string(),
                    delivery_class: astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext,
                    payload: json!({"latest_user_message": frame_value, "turn_id": frame_value}),
                    round_index: 1,
                }]),
            );
            let visible_tools = vec![tool("bash")];
            let restricted_tools = HashSet::new();
            let cache_cfg = PromptCacheConfig::latch("openai");
            let strict_history = strict_history_cache_capability();
            let output = assemble_context_pipeline(LlmContextAssemblyInput {
                state: &mut state,
                session_id: "sid-edge-frame",
                tool_surface: ToolSurfacePlan::from_visible_tools(
                    &visible_tools,
                    &restricted_tools,
                ),
                runtime_signals: RuntimeSignals::new(&edge_profile, None),
                cache_cfg: &cache_cfg,
                provider: "openai",
                model_name: "deepseek-v4-flash",
                context_window: Some(200_000),
                max_completion_tokens: Some(16_384),
                cache_capability: Some(strict_history),
                user_content: "summarize it",
                query_source: "test",
            })
            .expect("context pipeline should assemble");
            let wire = crate::turn::wire_assembly::assemble_llm_messages_with_cache_capability(
                output.system_messages,
                output.volatile_preamble,
                Vec::new(),
                output.messages,
                &crate::turn::wire_assembly::PostCompactAttachments::default(),
                "sid-edge-frame",
                "openai",
                "deepseek-v4-flash",
                &astra_turn_core::thinking_config::ThinkingConfig::Off,
                Some(strict_history),
                &cache_cfg,
            );
            crate::turn::llm::client::consolidate_system_messages_for_provider(
                &wire,
                "openai",
                Some(strict_history),
            )
        }

        let first = provider_messages("frame-alpha-dynamic-value");
        let second = provider_messages("frame-beta-dynamic-value");
        assert_eq!(first[0]["role"], "system");
        assert_eq!(
            first[0], second[0],
            "typed edge-profile frames must not alter the consolidated strict-provider prefix"
        );
        let system = message_text(&first[0]);
        assert!(system.contains("active_turn_focus_policy.v1"));
        assert!(!system.contains("frame-alpha-dynamic-value"));
        assert!(!message_text(&second[0]).contains("frame-beta-dynamic-value"));
        assert!(first.iter().any(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && message_text(message) == "summarize it"
        }));
    }

    #[test]
    fn immediate_prior_user_request_uses_position_when_text_repeats() {
        let messages = vec![
            json!({"role": "user", "content": "问题总结？"}),
            json!({"role": "assistant", "content": "first"}),
            json!({"role": "user", "content": "CURSOR-BETA review"}),
            json!({"role": "assistant", "content": "second"}),
            json!({"role": "user", "content": "问题总结？"}),
        ];

        assert_eq!(
            immediate_prior_user_request(&messages, "问题总结？").as_deref(),
            Some("CURSOR-BETA review")
        );
    }

    #[test]
    fn immediate_prior_user_request_excludes_append_only_runtime_authority() {
        let mut runtime = json!({"role": "user", "content": "runtime settlement"});
        astra_turn_types::mark_append_only_required_context(
            &mut runtime,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let messages = vec![
            json!({"role": "user", "content": "actual prior goal"}),
            runtime,
            json!({"role": "assistant", "content": "checked"}),
            json!({"role": "user", "content": "continue"}),
        ];

        assert_eq!(
            immediate_prior_user_request(&messages, "continue").as_deref(),
            Some("actual prior goal")
        );
    }

    #[test]
    fn later_tool_round_does_not_repeat_the_current_goal_frame() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.message = "continue the current investigation".to_string();
        state.session_turn = 7;
        state.current_round_index = 1;
        state.messages = vec![
            json!({"role": "user", "content": state.message.clone()}),
            json!({"role": "assistant", "content": "", "tool_calls": []}),
            json!({"role": "tool", "content": "evidence", "tool_call_id": "c1"}),
        ];
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
        let messages = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: state.messages.clone(),
            state: &mut state,
            compaction_boundary_hit: false,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "gpt-4",
            cache_capability: None,
            cache_cfg: &PromptCacheConfig::latch("openai"),
        })
        .unwrap();

        assert!(
            messages
                .iter()
                .all(|message| !message_text(message).contains("active_turn_frame")),
            "the current goal is already in history; later rounds must not append a duplicate volatile frame"
        );
        assert!(state.volatile_pending.is_empty());
    }

    #[test]
    fn compaction_boundary_reanchors_goal_without_workspace_io() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.message = "continue".to_string();
        state.current_round_index = 1;
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::Off;
        let cache_cfg = PromptCacheConfig::latch("openai");

        let without_boundary = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: vec![json!({"role": "user", "content": "continue"})],
            state: &mut state,
            compaction_boundary_hit: false,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "gpt-4",
            cache_capability: None,
            cache_cfg: &cache_cfg,
        })
        .unwrap();
        assert!(
            !without_boundary
                .iter()
                .any(|message| { message_text(message).contains("Post-compaction context: file") })
        );

        let with_boundary = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: vec![json!({"role": "user", "content": "continue"})],
            state: &mut state,
            compaction_boundary_hit: true,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "gpt-4",
            cache_capability: None,
            cache_cfg: &cache_cfg,
        })
        .unwrap();
        assert!(
            with_boundary.iter().any(|message| {
                message_text(message).contains("\"kind\":\"active_turn_frame\"")
            })
        );
        assert!(
            with_boundary
                .iter()
                .all(|message| !message_text(message).contains("Post-compaction context: file"))
        );

        // A second stateless request may cross another real boundary in the
        // same user turn. Its compacted history is independent of the first
        // request, so the current-turn frame must be present again. Compaction
        // itself stays workspace-I/O-free; fresh bytes require an ordinary
        // governed read_file call.
        let with_second_boundary = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: vec![json!({"role": "user", "content": "continue"})],
            state: &mut state,
            compaction_boundary_hit: true,
            thinking: &thinking,
            session_id: "sid",
            provider: "openai",
            model_name: "gpt-4",
            cache_capability: None,
            cache_cfg: &cache_cfg,
        })
        .unwrap();
        assert!(
            with_second_boundary.iter().any(|message| {
                message_text(message).contains("\"kind\":\"active_turn_frame\"")
            })
        );
        assert!(
            with_second_boundary
                .iter()
                .all(|message| !message_text(message).contains("Post-compaction context: file"))
        );
    }

    #[test]
    fn stabilize_tool_schemas_does_not_restore_currently_pruned_tools_mid_turn() {
        let visible = vec![tool("bash"), tool("read_file"), tool("git")];
        let previous = vec![tool("bash"), tool("read_file"), tool("git")];
        let current = vec![tool("bash"), tool("read_file")];

        let stabilized = stabilize_tool_schemas_for_cache(
            &current,
            &previous,
            &visible,
            astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
                volatile_delivery:
                    astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            },
            1,
        );

        assert_eq!(tool_names(&stabilized), vec!["bash", "read_file"]);
    }

    #[test]
    fn stabilize_tool_schemas_never_restores_a_pruned_action_branch() {
        let previous = vec![json!({
            "type": "function",
            "function": {
                "name": "agent_fanout",
                "description": "fanout",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["start", "get_results"]}
                    }
                }
            }
        })];
        let current = vec![json!({
            "type": "function",
            "function": {
                "name": "agent_fanout",
                "description": "fanout",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["get_results"]}
                    }
                }
            }
        })];

        let stabilized = stabilize_tool_schemas_for_cache(
            &current,
            &previous,
            &current,
            strict_history_cache_capability(),
            1,
        );

        assert_eq!(stabilized, current);
        assert_eq!(
            stabilized[0]["function"]["parameters"]["properties"]["action"]["enum"],
            json!(["get_results"]),
            "cache stabilization may preserve order, never stale execution authority"
        );
    }

    #[test]
    fn stabilize_tool_schemas_resets_on_first_round() {
        let visible = vec![tool("bash"), tool("read_file"), tool("git")];
        let previous = vec![tool("bash"), tool("read_file"), tool("git")];
        let current = vec![tool("bash"), tool("read_file")];

        let stabilized = stabilize_tool_schemas_for_cache(
            &current,
            &previous,
            &visible,
            astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
                volatile_delivery:
                    astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            },
            0,
        );

        assert_eq!(tool_names(&stabilized), vec!["bash", "read_file"]);
    }

    #[test]
    fn stabilize_tool_schemas_drops_no_longer_visible_tools() {
        let visible = vec![tool("bash"), tool("read_file")];
        let previous = vec![tool("bash"), tool("read_file"), tool("git")];
        let current = vec![tool("bash"), tool("read_file")];

        let stabilized = stabilize_tool_schemas_for_cache(
            &current,
            &previous,
            &visible,
            astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
                volatile_delivery:
                    astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            },
            1,
        );

        assert_eq!(tool_names(&stabilized), vec!["bash", "read_file"]);
    }

    #[test]
    fn context_meta_event_preserves_manifest_trace() {
        let breakdown = astra_turn_core::context_assembly_trace::SystemPromptBreakdown {
            total_tokens: 123,
            ..Default::default()
        };
        let trace = json!({
            "source": "llm_context",
            "wire": {"total_cache_control_count": 2}
        });

        let event = context_meta_event(&breakdown, Some(&trace));

        assert_eq!(event["type"], "context_meta");
        assert_eq!(event["system_prompt_tokens"], 123);
        assert_eq!(event["context_manifest_trace"], trace);
    }

    #[test]
    fn context_meta_event_reports_exact_provider_tool_surface() {
        let breakdown = astra_turn_core::context_assembly_trace::SystemPromptBreakdown::default();
        let action_tool = json!({
            "type": "function",
            "function": {
                "name": "agent_fanout",
                "parameters": {
                    "type": "object",
                    "oneOf": [
                        {"properties": {"action": {"const": "get_results"}}},
                        {"properties": {"action": {"enum": ["stop_slot", "stop_group"]}}}
                    ]
                }
            }
        });
        let event = context_meta_event_with_tool_surface(
            &breakdown,
            None,
            &[],
            &[
                tool("start_work"),
                tool("agent"),
                tool("agent"),
                action_tool,
            ],
        );

        assert_eq!(
            event["visible_tools"],
            json!(["start_work", "agent", "agent_fanout"])
        );
        assert_eq!(
            event["visible_tool_actions"]["agent_fanout"],
            json!(["get_results", "stop_group", "stop_slot"])
        );
    }

    #[test]
    fn context_meta_event_carries_compaction_observations_as_runtime_facts() {
        let breakdown = astra_turn_core::context_assembly_trace::SystemPromptBreakdown {
            total_tokens: 42,
            ..Default::default()
        };
        let compactions = vec![
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation {
                id: "initial".to_string(),
                kind: astra_turn_core::compaction_types::CompactionKind::WireAssembly,
                tier: astra_turn_core::compaction_types::CompactionTier::CompactHistory,
                messages_before: 18,
                messages_after: 10,
                tokens_before: 12_000,
                tokens_after: 7_000,
                tokens_saved: 5_000,
                post_compaction_target_tokens: Some(7_000),
                effectiveness: astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Sufficient,
            },
        ];

        let event = context_meta_event_with_compactions(&breakdown, None, &compactions);

        assert_eq!(event["compactions"][0]["id"], "initial");
        assert_eq!(event["compactions"][0]["tokens_saved"], 5_000);
        assert!(
            event.get("context_manifest_trace").is_none(),
            "runtime facts must not fabricate a context-manifest trace"
        );
    }

    #[test]
    fn context_manifest_persists_provider_neutral_prompt_cache_accounting() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let projection = build_context_manifest_projection(ContextManifestProjectionInput {
            owner_id: "user-a",
            session_id: "session-a",
            run_id: "run-a",
            turn_index: 3,
            llm_attempt_index: 2,
            pre_llm_messages: &messages,
            tool_results: &[],
            schema_tokens: 7,
            result_prompt_tokens: Some(1_100),
            observed_fresh_input_tokens: Some(200),
            observed_cache_read_tokens: Some(800),
            observed_cache_creation_tokens: Some(100),
            observed_output_tokens: Some(50),
            assembly_trace: None,
            turn_intent: "implementation",
            reason: "test",
            context_window_tokens: 64_000,
        });

        let normalized = serde_json::from_value::<astra_turn_types::NormalizedPromptCacheUsage>(
            projection.manifest_json["normalized_prompt_cache_usage"].clone(),
        )
        .expect("manifest usage must retain its typed provider-neutral shape");
        assert_eq!(
            normalized,
            astra_turn_types::NormalizedPromptCacheUsage::new(200, 800, 100)
        );
        assert_eq!(normalized.cache_creation_tokens, 100);
        assert_eq!(normalized.total_input_tokens(), 1_100);
        assert_eq!(projection.total_estimated_tokens, 1_100);
        assert_eq!(
            projection.manifest_json["observed_usage"]["output_tokens"],
            50
        );
    }

    #[test]
    fn context_manifest_estimates_each_final_wire_zone_once() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"src/lib.rs\"}"}
                }]
            }),
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call-1",
                    "content": "file body"
                }]
            }),
        ];
        let duplicate_runtime_projection = vec![json!({
            "name": "read_file",
            "tool_call_id": "call-1",
            "result": "file body"
        })];
        let schema_tokens = 7;
        let projection = build_context_manifest_projection(ContextManifestProjectionInput {
            owner_id: "user-a",
            session_id: "session-a",
            run_id: "run-a",
            turn_index: 3,
            llm_attempt_index: 2,
            pre_llm_messages: &messages,
            tool_results: &duplicate_runtime_projection,
            schema_tokens,
            result_prompt_tokens: None,
            observed_fresh_input_tokens: None,
            observed_cache_read_tokens: None,
            observed_cache_creation_tokens: None,
            observed_output_tokens: None,
            assembly_trace: None,
            turn_intent: "implementation",
            reason: "test",
            context_window_tokens: 64_000,
        });

        let expected_wire_tokens = messages
            .iter()
            .map(estimate_json_tokens)
            .fold(schema_tokens, u32::saturating_add);
        assert_eq!(projection.total_estimated_tokens, expected_wire_tokens);
        let zone_used_total = projection.manifest_json["zones"]
            .as_object()
            .expect("zones must be an object")
            .values()
            .filter_map(|zone| zone.get("used_tokens").and_then(Value::as_u64))
            .sum::<u64>();
        assert_eq!(
            zone_used_total,
            u64::from(expected_wire_tokens),
            "mutually exclusive manifest zones must reconcile with the final-wire estimate"
        );
    }

    #[test]
    fn context_manifest_persists_bounded_attempt_artifact_evidence_without_all_or_nothing_loss() {
        let artifact_result = json!({
            "name": "catalog_read",
            "result": "bounded preview",
            (astra_turn_types::TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY): {
                "artifactId": "artifact-7",
                "contentHash": format!("sha256:{:064x}", 7),
                "encodedBytes": 4096
            }
        });
        let tool_results = vec![artifact_result];
        let projection = build_context_manifest_projection(ContextManifestProjectionInput {
            owner_id: "user-a",
            session_id: "session-a",
            run_id: "run-a",
            turn_index: 3,
            llm_attempt_index: 2,
            pre_llm_messages: &[],
            tool_results: &tool_results,
            schema_tokens: 0,
            result_prompt_tokens: None,
            observed_fresh_input_tokens: None,
            observed_cache_read_tokens: None,
            observed_cache_creation_tokens: None,
            observed_output_tokens: None,
            assembly_trace: None,
            turn_intent: "implementation",
            reason: "test",
            context_window_tokens: 64_000,
        });
        assert_eq!(
            projection.manifest_json["llm_artifact_evidence_status"],
            "ready"
        );
        let manifest = serde_json::from_value::<astra_turn_types::LlmArtifactEvidenceManifestV1>(
            projection.manifest_json["llm_artifact_evidence"].clone(),
        )
        .expect("persisted attempt artifact evidence revalidates");
        assert_eq!(manifest.entries[0].artifact_id, "artifact-7");
        assert_eq!(manifest.entries[0].tool_name, "catalog_read");
        assert_eq!(manifest.entries[0].encoded_bytes, Some(4096));
        assert!(projection.items.iter().any(|item| {
            item.zone == "llm_artifact_evidence"
                && item.source_hash.as_deref() == Some(manifest.content_id.as_str())
                && item.raw_ref.is_none()
        }));

        let invalid_results = vec![json!({
            "name": "catalog_read",
            (astra_turn_types::TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY): {
                "artifactId": "artifact-7",
                "contentHash": "forged"
            }
        })];
        let invalid = build_context_manifest_projection(ContextManifestProjectionInput {
            owner_id: "user-a",
            session_id: "session-a",
            run_id: "run-a",
            turn_index: 3,
            llm_attempt_index: 2,
            pre_llm_messages: &[],
            tool_results: &invalid_results,
            schema_tokens: 0,
            result_prompt_tokens: None,
            observed_fresh_input_tokens: None,
            observed_cache_read_tokens: None,
            observed_cache_creation_tokens: None,
            observed_output_tokens: None,
            assembly_trace: None,
            turn_intent: "implementation",
            reason: "test",
            context_window_tokens: 64_000,
        });
        assert_eq!(
            invalid.manifest_json["llm_artifact_evidence_status"],
            "all_invalid"
        );
        let invalid_manifest = serde_json::from_value::<
            astra_turn_types::LlmArtifactEvidenceManifestV1,
        >(invalid.manifest_json["llm_artifact_evidence"].clone())
        .expect("invalid references remain explicit evidence instead of disappearing");
        assert_eq!(invalid_manifest.observed_reference_count, 1);
        assert_eq!(invalid_manifest.invalid_reference_count, 1);
        assert!(invalid_manifest.entries.is_empty());

        let bounded_results = (0..=astra_turn_types::LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES)
            .map(|index| {
                json!({
                    "name": "catalog_read",
                    (astra_turn_types::TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY): {
                        "artifactId": format!("artifact-{index:03}"),
                        "contentHash": format!("sha256:{index:064x}")
                    }
                })
            })
            .collect::<Vec<_>>();
        let bounded = build_context_manifest_projection(ContextManifestProjectionInput {
            owner_id: "user-a",
            session_id: "session-a",
            run_id: "run-a",
            turn_index: 3,
            llm_attempt_index: 2,
            pre_llm_messages: &[],
            tool_results: &bounded_results,
            schema_tokens: 0,
            result_prompt_tokens: None,
            observed_fresh_input_tokens: None,
            observed_cache_read_tokens: None,
            observed_cache_creation_tokens: None,
            observed_output_tokens: None,
            assembly_trace: None,
            turn_intent: "implementation",
            reason: "test",
            context_window_tokens: 64_000,
        });
        assert_eq!(
            bounded.manifest_json["llm_artifact_evidence_status"],
            "partial_bounded"
        );
        let bounded_manifest = serde_json::from_value::<
            astra_turn_types::LlmArtifactEvidenceManifestV1,
        >(bounded.manifest_json["llm_artifact_evidence"].clone())
        .expect("bounded evidence revalidates");
        assert_eq!(
            bounded_manifest.entries.len(),
            astra_turn_types::LLM_ARTIFACT_EVIDENCE_MAX_ENTRIES
        );
        assert_eq!(bounded_manifest.observed_reference_count, 65);
        assert_eq!(bounded_manifest.omitted_reference_count, 1);
    }

    #[test]
    fn every_physical_request_identity_is_derived_from_its_exact_serialized_bytes() {
        let messages = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "user", "content": "task"}),
        ];
        let tools = vec![tool("read_file")];
        let mut attempts = Vec::new();
        for (attempt, max_tokens, status, response_id, usage, error_kind, error_message) in [
            (
                0_u32,
                4_096_u32,
                astra_services::InferenceTerminalStatus::DeliveryUnknown,
                "provider-partial",
                astra_services::InferenceUsage {
                    input: astra_turn_types::NormalizedPromptCacheUsage::new(200, 800, 100),
                    output_tokens: 50,
                },
                Some("stream_transport"),
                Some("provider stream ended after partial delivery"),
            ),
            (
                1,
                2_048,
                astra_services::InferenceTerminalStatus::Succeeded,
                "provider-success",
                astra_services::InferenceUsage {
                    input: astra_turn_types::NormalizedPromptCacheUsage::new(120, 400, 20),
                    output_tokens: 30,
                },
                None,
                None,
            ),
        ] {
            let body = json!({
                "model": "provider-model",
                "messages": messages,
                "tools": tools,
                "max_tokens": max_tokens,
                "stream": true,
            });
            let prepared = crate::turn::llm::client::PreparedProviderRequest::from_json(
                &body,
                astra_inference_adapter::ProviderProtocol::OpenAiCompatible,
            )
            .expect("prepare provider body once");
            let wire = prepared.identity();
            attempts.push(crate::turn::llm::durable::DurableProviderAttemptFact {
                request: crate::turn::llm::durable::DurableProviderRequestIdentity {
                    request_id: format!("attempt-{attempt}"),
                    request_hash: wire.provider_wire_hash.clone(),
                    attempt,
                    protocol: wire.protocol,
                    provider_wire_bytes: wire.provider_wire_bytes,
                    composition: wire.composition.clone(),
                    fingerprints: wire.fingerprints.clone(),
                },
                dispatch_started: true,
                terminal: Some(astra_services::InferenceInvocationTerminal {
                    status,
                    usage,
                    usage_status: if status == astra_services::InferenceTerminalStatus::Succeeded {
                        astra_services::InferenceUsageStatus::ProviderExact
                    } else {
                        astra_services::InferenceUsageStatus::ProviderPartial
                    },
                    provider_response_id: Some(response_id.to_string()),
                    error_kind: error_kind.map(str::to_string),
                    error_message: error_message.map(str::to_string),
                }),
            });
        }
        let mut trace = json!({"source": "llm_context"});
        augment_manifest_trace_with_wire(&mut trace, &messages, &tools);
        augment_manifest_trace_with_provider_attempts(&mut trace, &attempts, 3);

        assert_eq!(
            trace["provider_request_attempts"].as_array().unwrap().len(),
            2
        );
        for (index, attempt) in attempts.iter().enumerate() {
            let projected = &trace["provider_request_attempts"][index];
            assert_eq!(
                projected["composition_bytes"]["total"],
                projected["serialized_bytes"]
            );
            assert_eq!(projected["request_id"], attempt.request.request_id);
            assert_eq!(projected["request_hash"], attempt.request.request_hash);
            assert_eq!(projected["transport_stage"], "dispatch_started");
            assert_eq!(
                projected["provider_final_fingerprints"]["message_sequence_sha256"],
                attempt.request.fingerprints.message_sequence_sha256
            );
        }
        assert_ne!(
            trace["provider_request_attempts"][0]["request_hash"],
            trace["provider_request_attempts"][1]["request_hash"]
        );
        assert_eq!(
            trace["provider_request_attempts"][0]["terminal_status"],
            "delivery_unknown"
        );
        assert_eq!(
            trace["provider_request_attempts"][0]["usage_status"],
            "provider_partial"
        );
        assert_eq!(
            trace["provider_request_attempts"][0]["provider_response_id"],
            "provider-partial"
        );
        assert_eq!(
            trace["provider_request_attempts"][0]["usage"],
            json!({
                "input": {
                    "fresh_input_tokens": 200,
                    "cache_read_tokens": 800,
                    "cache_creation_tokens": 100,
                },
                "output_tokens": 50,
            })
        );
        assert_eq!(
            trace["provider_request_attempts"][0]["error_kind"],
            "stream_transport"
        );
        assert_eq!(
            trace["provider_request_attempts"][0]["error_message"],
            "provider stream ended after partial delivery"
        );
        assert_eq!(
            trace["provider_request_attempts"][1]["terminal_status"],
            "succeeded"
        );
        assert_eq!(
            trace["provider_request_attempts"][1]["usage_status"],
            "provider_exact"
        );
        assert_eq!(
            trace["provider_request_attempts"][1]["usage"],
            json!({
                "input": {
                    "fresh_input_tokens": 120,
                    "cache_read_tokens": 400,
                    "cache_creation_tokens": 20,
                },
                "output_tokens": 30,
            })
        );
        assert!(trace["provider_request_attempts"][1]["error_kind"].is_null());
        assert!(trace["provider_request_attempts"][1]["error_message"].is_null());
        assert_eq!(
            trace["request_identity"]["request_id"],
            attempts[1].request.request_id
        );
        assert_eq!(
            trace["request_identity"]["provider_response_id"],
            "provider-success"
        );
        assert_eq!(
            trace["wire"]["projection_authority"], "planned_pre_client_projection_v1",
            "the planned projection remains explicitly distinct from exact body facts"
        );

        clear_manifest_provider_request(&mut trace);
        assert!(trace.get("request_identity").is_none());
        assert!(trace.get("provider_request_attempts").is_none());
        assert!(trace["wire"].get("projection_authority").is_some());
    }

    #[test]
    fn augment_manifest_trace_records_final_wire_cache_control_counts() {
        let mut trace = json!({"source": "llm_context"});
        let messages = vec![
            json!({
                "role": "system",
                "content": [{
                    "type": "text",
                    "text": "stable",
                    "cache_control": {"type": "ephemeral"}
                }]
            }),
            json!({"role": "user", "content": "hi"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read_file", "parameters": {"type": "object"}},
            "cache_control": {"type": "ephemeral"}
        })];

        augment_manifest_trace_with_wire(&mut trace, &messages, &tools);

        assert_eq!(trace["wire"]["message_count"], 2);
        assert_eq!(
            trace["wire"]["projection_authority"],
            "planned_pre_client_projection_v1"
        );
        assert_eq!(trace["wire"]["tool_schema_count"], 1);
        assert_eq!(trace["wire"]["message_cache_control_count"], 1);
        assert_eq!(trace["wire"]["tool_cache_control_count"], 1);
        assert_eq!(trace["wire"]["total_cache_control_count"], 2);
        assert_eq!(trace["wire"]["message_roles"], json!(["system", "user"]));
        assert_eq!(
            trace["wire"]["message_role_counts"],
            json!({"system": 1, "user": 1})
        );
        assert_eq!(trace["wire"]["conversation_projection"]["message_count"], 1);
        assert_eq!(
            trace["wire"]["conversation_projection"]["role_counts"],
            json!({"user": 1})
        );
        assert!(
            trace["wire"]["fingerprint"]["message_sequence_sha256"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(
            trace["wire"]["fingerprint"]["system_and_tools_sha256"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert_eq!(
            trace["wire"]["fingerprint"]["message_hashes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let prompt_identity = &trace["wire"]["fingerprint"]["prompt_cache_identity"];
        assert_eq!(
            prompt_identity["contract_version"],
            astra_turn_types::PROMPT_CACHE_IDENTITY_CONTRACT_VERSION
        );
        assert!(
            prompt_identity["content_id"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
    }

    #[test]
    fn metrics_wire_trace_is_bounded_and_keeps_only_low_cardinality_facts() {
        let secret = "message-content-must-not-enter-metrics-trace";
        let messages = (0..2_048)
            .map(|index| {
                json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("{secret}-{index}"),
                })
            })
            .collect::<Vec<_>>();
        let mut trace = json!({"source": "llm_context"});

        augment_manifest_trace_with_wire_detail(
            &mut trace,
            &messages,
            &[],
            WireTraceDetail::MetricsOnly,
        );

        assert_eq!(trace["wire"]["trace_detail"], "metrics_only");
        assert_eq!(trace["wire"]["message_count"], 2_048);
        assert_eq!(
            trace["wire"]["message_role_counts"],
            json!({"assistant": 1024, "user": 1024})
        );
        assert!(trace["wire"].get("message_roles").is_none());
        assert!(trace["wire"].get("estimated_message_tokens").is_none());
        assert!(
            trace["wire"]["fingerprint"]
                .get("message_sequence_sha256")
                .is_none()
        );
        assert!(trace["wire"]["fingerprint"].get("message_hashes").is_none());
        assert!(!trace.to_string().contains(secret));
        assert!(
            trace.to_string().len() < 2_048,
            "metrics trace size must not grow with message count"
        );
    }

    #[test]
    fn prompt_cache_identity_hashes_only_the_contiguous_leading_system_prefix() {
        let mut baseline = json!({});
        let mut with_later_runtime_system = json!({});
        augment_manifest_trace_with_wire(
            &mut baseline,
            &[
                json!({"role": "system", "content": "stable"}),
                json!({"role": "user", "content": "work"}),
            ],
            &[],
        );
        augment_manifest_trace_with_wire(
            &mut with_later_runtime_system,
            &[
                json!({"role": "system", "content": "stable"}),
                json!({"role": "user", "content": "work"}),
                json!({"role": "system", "content": "volatile settlement"}),
            ],
            &[],
        );

        assert_eq!(
            baseline["wire"]["fingerprint"]["prompt_cache_identity"]["stable_system_prefix_hash"],
            with_later_runtime_system["wire"]["fingerprint"]["prompt_cache_identity"]["stable_system_prefix_hash"]
        );
        assert_ne!(
            baseline["wire"]["fingerprint"]["system_message_sequence_sha256"],
            with_later_runtime_system["wire"]["fingerprint"]["system_message_sequence_sha256"]
        );
    }

    #[test]
    fn model_request_seed_reads_only_exact_manifest_contract_fields() {
        let trace = json!({
            "model_context_window_tokens": 1_000_000,
            "context_window_policy": {
                "raw_context_window_tokens": 1_000_000,
                "usable_input_limit_tokens": 910_000,
                "reserved_output_tokens": 64_000,
                "reserved_summary_tokens": 24_000,
                "reserved_protocol_tokens": 2_000,
                "auto_compact_trigger_tokens": 728_000,
                "hard_input_limit_tokens": 934_000
            },
            "system_prompt_tokens": 12_000,
            "compaction_tier": "normal",
            "wire": {
                "budget": {
                    "estimated_input_tokens": 700_000
                },
                "fingerprint": {
                    "prompt_cache_identity": {
                        "cache_layout": "provider-prefix-v1",
                        "content_id": "sha256:stable"
                    }
                }
            }
        });

        let seed = model_request_context_seed_from_manifest(
            astra_services::ModelRequestTopology::CliServer,
            Some(&trace),
        );
        assert_eq!(
            seed.topology,
            astra_services::ModelRequestTopology::CliServer
        );
        assert_eq!(seed.interaction_owner, "cli");
        assert_eq!(seed.loop_owner, "server");
        assert_eq!(seed.budget.raw_context_window_tokens, Some(1_000_000));
        assert_eq!(seed.budget.usable_input_limit_tokens, Some(910_000));
        assert_eq!(seed.budget.estimated_input_tokens, Some(700_000));
        assert_eq!(seed.composition.stable_system_tokens, Some(12_000));
        assert_eq!(
            seed.cache.current_identity.as_deref(),
            Some("sha256:stable")
        );
    }

    #[test]
    fn topology_changes_surface_attribution_not_assembled_request_facts() {
        let trace = json!({
            "context_window_policy": {
                "raw_context_window_tokens": 1_000_000,
                "usable_input_limit_tokens": 910_000,
                "reserved_output_tokens": 64_000,
                "reserved_summary_tokens": 24_000,
                "reserved_protocol_tokens": 2_000,
                "auto_compact_trigger_tokens": 728_000,
                "hard_input_limit_tokens": 934_000
            },
            "system_prompt_tokens": 12_000,
            "compaction_tier": "normal",
            "wire": {
                "budget": {"estimated_input_tokens": 700_000},
                "fingerprint": {
                    "prompt_cache_identity": {
                        "cache_layout": "provider-prefix-v1",
                        "content_id": "sha256:stable"
                    }
                }
            }
        });

        let cli = model_request_context_seed_from_manifest(
            astra_services::ModelRequestTopology::CliServer,
            Some(&trace),
        );
        let server = model_request_context_seed_from_manifest(
            astra_services::ModelRequestTopology::ServerOnly,
            Some(&trace),
        );
        let edge = model_request_context_seed_from_manifest(
            astra_services::ModelRequestTopology::EdgeServer,
            Some(&trace),
        );

        for candidate in [&server, &edge] {
            assert_eq!(candidate.rollout_stage, cli.rollout_stage);
            assert_eq!(candidate.model_family, cli.model_family);
            assert_eq!(candidate.lineage, cli.lineage);
            assert_eq!(candidate.budget, cli.budget);
            assert_eq!(candidate.composition, cli.composition);
            assert_eq!(candidate.cache, cli.cache);
            assert_eq!(candidate.compaction, cli.compaction);
        }
        assert_eq!(cli.interaction_owner, "cli");
        assert_eq!(server.interaction_owner, "server");
        assert_eq!(edge.interaction_owner, "edge");
        assert_eq!(cli.loop_owner, "server");
        assert_eq!(server.loop_owner, "server");
        assert_eq!(edge.loop_owner, "server");
    }

    #[test]
    fn prompt_cache_identity_excludes_volatile_tail_and_invalidates_schema_changes() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [{
                    "type": "text",
                    "text": "stable policy",
                    "cache_control": {"type": "ephemeral"}
                }]
            }),
            json!({"role": "system", "content": "volatile task state A"}),
            json!({"role": "user", "content": "request A"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read_file", "parameters": {"type": "object"}},
            "cache_control": {"type": "ephemeral"}
        })];
        let mut baseline = json!({"source": "llm_context"});
        augment_manifest_trace_with_wire(&mut baseline, &messages, &tools);

        let mut changed_tail_messages = messages.clone();
        changed_tail_messages[1]["content"] = json!("volatile task state B");
        changed_tail_messages[2]["content"] = json!("request B");
        let mut changed_tail = json!({"source": "llm_context"});
        augment_manifest_trace_with_wire(&mut changed_tail, &changed_tail_messages, &tools);
        assert_eq!(
            baseline["wire"]["fingerprint"]["prompt_cache_identity"],
            changed_tail["wire"]["fingerprint"]["prompt_cache_identity"],
            "task/resource/user volatility after the marker must not churn the stable prefix"
        );

        let mut changed_tools = tools.clone();
        changed_tools[0]["function"]["parameters"] = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}}
        });
        let mut changed_schema = json!({"source": "llm_context"});
        augment_manifest_trace_with_wire(&mut changed_schema, &messages, &changed_tools);
        assert_ne!(
            baseline["wire"]["fingerprint"]["prompt_cache_identity"],
            changed_schema["wire"]["fingerprint"]["prompt_cache_identity"],
            "a stable tool-schema change must invalidate the prompt identity"
        );
    }

    #[test]
    fn augment_manifest_trace_records_wire_fingerprints_without_prompt_content() {
        let prompt_secret = "PROMPT_SECRET_DO_NOT_LOG";
        let tool_secret = "TOOL_SECRET_DO_NOT_LOG";
        let messages = vec![
            json!({"role": "system", "content": "stable policy"}),
            json!({"role": "user", "content": prompt_secret}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "secret_tool",
                "description": tool_secret,
                "parameters": {"type": "object"}
            }
        })];
        let mut trace = json!({"source": "llm_context"});

        augment_manifest_trace_with_wire(&mut trace, &messages, &tools);

        let rendered = trace.to_string();
        assert!(!rendered.contains(prompt_secret), "{rendered}");
        assert!(!rendered.contains(tool_secret), "{rendered}");
        let baseline_message_hash = trace["wire"]["fingerprint"]["message_sequence_sha256"]
            .as_str()
            .unwrap()
            .to_string();
        let baseline_tool_hash = trace["wire"]["fingerprint"]["tool_schema_sequence_sha256"]
            .as_str()
            .unwrap()
            .to_string();

        let mut changed_messages = messages.clone();
        changed_messages[1]["content"] = json!("changed user content");
        let mut changed_message_trace = json!({"source": "llm_context"});
        augment_manifest_trace_with_wire(&mut changed_message_trace, &changed_messages, &tools);

        assert_ne!(
            changed_message_trace["wire"]["fingerprint"]["message_sequence_sha256"]
                .as_str()
                .unwrap(),
            baseline_message_hash
        );
        assert_eq!(
            changed_message_trace["wire"]["fingerprint"]["tool_schema_sequence_sha256"]
                .as_str()
                .unwrap(),
            baseline_tool_hash
        );

        let mut changed_tools = tools.clone();
        changed_tools[0]["function"]["parameters"] = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}}
        });
        let mut changed_tool_trace = json!({"source": "llm_context"});
        augment_manifest_trace_with_wire(&mut changed_tool_trace, &messages, &changed_tools);

        assert_eq!(
            changed_tool_trace["wire"]["fingerprint"]["message_sequence_sha256"]
                .as_str()
                .unwrap(),
            baseline_message_hash
        );
        assert_ne!(
            changed_tool_trace["wire"]["fingerprint"]["tool_schema_sequence_sha256"]
                .as_str()
                .unwrap(),
            baseline_tool_hash
        );
    }

    #[test]
    fn resolve_pipeline_session_current_date_uses_journal_anchor_without_pipeline_session() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000291";
        let writer = astra_services::session_journal::JournalWriter::new(session_id)
            .expect("journal writer");

        let mut start =
            astra_services::session_journal::JournalEvent::session_start(Some(session_id), None);
        start.ts = "2026-05-24T23:59:50Z".to_string();
        writer.append(&start).unwrap();

        let mut later = astra_services::session_journal::JournalEvent::llm_request_full(
            Some(session_id),
            1,
            0,
            serde_json::json!({"provider": "openai", "request": {"messages": []}}),
        );
        later.ts = "2026-05-25T00:10:00Z".to_string();
        writer.append(&later).unwrap();

        assert_eq!(
            resolve_pipeline_session_current_date(None, None, session_id),
            "2026-05-24",
            "context assembly must use the journal-anchored session date when no pipeline session exists yet"
        );
    }

    #[test]
    fn resolve_pipeline_session_current_date_prefers_pipeline_session_date() {
        let session = astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
            "2026-05-21",
        );

        assert_eq!(
            resolve_pipeline_session_current_date(Some(&session), None, "ignored-session-id"),
            "2026-05-21"
        );
    }
}
