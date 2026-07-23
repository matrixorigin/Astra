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
        astra_services::PromptCacheVolatilePlacementData::CurrentUserOnly => {
            astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly
        }
        astra_services::PromptCacheVolatilePlacementData::Free => {
            astra_turn_core::cache_placement::VolatilePlacement::Free
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
        reuse_scope,
    })
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
            "compaction_tier": self.compaction_tier.clone(),
            "system_prompt_tokens": self.system_prompt_tokens,
            "stable_system_message_count": self.stable_system_message_count,
            "volatile_preamble_count": self.volatile_preamble_count,
            "tool_schema_count": self.tool_schema_count,
            "runtime_manifest": self.runtime_manifest.clone(),
        })
    }
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

pub(crate) fn context_meta_event(
    breakdown: &astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
    context_manifest_trace: Option<&Value>,
) -> Value {
    let mut event = json!({
        "type": "context_meta",
        "system_prompt_tokens": breakdown.total_tokens,
        "system_prompt_breakdown": breakdown,
    });
    if let Some(trace) = context_manifest_trace {
        event["context_manifest_trace"] = trace.clone();
    }
    event
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

fn role_counts_json(roles: &[String]) -> Value {
    let mut counts = BTreeMap::<String, usize>::new();
    for role in roles {
        *counts.entry(role.clone()).or_default() += 1;
    }
    json!(counts)
}

/// Bridge-facing context assembly input.
///
/// The bridge still has a per-request lifecycle and a few bridge-specific
/// source lanes, so this adapter preserves current behavior while routing the
/// call through the shared context boundary. The implementation delegates to
/// the existing bridge pipeline helper until the bridge source collection is
/// fully normalized into [`LlmContextAssemblyInput`].
pub(crate) struct BridgeContextAssemblyInput<'a> {
    /// Current conversation working set. It is optimized by the same pipeline
    /// that selects the system sections and tool surface.
    pub conversation_messages: &'a [Value],
    pub tool_surface: ToolSurfacePlan<'a>,
    pub runtime_signals: BridgeRuntimeSignals<'a>,
    pub session: BridgeSessionContextInput<'a>,
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

pub(crate) struct BridgeRuntimeSignals<'a> {
    pub extra_stable_sections: &'a [crate::prompts::PromptSection],
    pub extra_volatile_sections: &'a [crate::prompts::PromptSection],
    pub memory_entries: &'a [astra_turn_core::context_sources::MemoryEntry],
    pub memory_provider_source: Option<&'a str>,
    pub session_memory_entry: Option<astra_turn_core::context_sources::MemoryEntry>,
    pub system_override: Option<&'a str>,
}

impl<'a> BridgeRuntimeSignals<'a> {
    pub(crate) fn new(
        extra_stable_sections: &'a [crate::prompts::PromptSection],
        extra_volatile_sections: &'a [crate::prompts::PromptSection],
        memory_entries: &'a [astra_turn_core::context_sources::MemoryEntry],
        session_memory_entry: Option<astra_turn_core::context_sources::MemoryEntry>,
        system_override: Option<&'a str>,
    ) -> Self {
        Self {
            extra_stable_sections,
            extra_volatile_sections,
            memory_entries,
            memory_provider_source: None,
            session_memory_entry,
            system_override,
        }
    }

    pub(crate) fn with_memory_provider_source(mut self, source: Option<&'a str>) -> Self {
        self.memory_provider_source = source;
        self
    }
}

pub(crate) struct BridgeSessionContextInput<'a> {
    pub cache_cfg: &'a PromptCacheConfig,
    pub cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    pub session_id: &'a str,
    pub model_id: &'a str,
    pub context_window: Option<u32>,
    pub provider: &'a str,
    pub edge_profile_cwd: Option<&'a str>,
    pub edge_profile_git_branch: Option<&'a str>,
    pub project_context: Option<&'a str>,
    pub current_date: &'a str,
    pub skill_listing_block: &'a str,
}

impl<'a> BridgeSessionContextInput<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cache_cfg: &'a PromptCacheConfig,
        cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
        session_id: &'a str,
        model_id: &'a str,
        provider: &'a str,
        edge_profile_cwd: Option<&'a str>,
        edge_profile_git_branch: Option<&'a str>,
        project_context: Option<&'a str>,
        current_date: &'a str,
    ) -> Self {
        Self {
            cache_cfg,
            cache_capability,
            session_id,
            model_id,
            context_window: None,
            provider,
            edge_profile_cwd,
            edge_profile_git_branch,
            project_context,
            current_date,
            skill_listing_block: "",
        }
    }

    pub(crate) fn with_skill_listing_block(mut self, skill_listing_block: &'a str) -> Self {
        self.skill_listing_block = skill_listing_block;
        self
    }

    pub(crate) fn with_context_window(mut self, context_window: Option<u32>) -> Self {
        self.context_window = context_window;
        self
    }
}

pub(crate) struct BridgeContextAssemblyOutput {
    pub primary_system: Value,
    pub dynamic_system: Option<Value>,
    pub messages: Vec<Value>,
    pub prompt_sections: Vec<crate::prompts::PromptSection>,
    pub tier: astra_turn_core::compaction_types::CompactionTier,
    pub tool_schemas: Vec<Value>,
    pub manifest_trace: LlmContextManifestTrace,
}

/// Input for the final wire-message stitching phase.
pub(crate) struct LlmWireAssemblyInput<'a> {
    pub system_messages: Vec<Value>,
    pub volatile_preamble: Vec<Value>,
    pub compacted_messages: Vec<Value>,
    pub state: &'a mut AgenticLoopState,
    pub thinking: &'a astra_turn_core::thinking_config::ThinkingConfig,
    pub edge_profile: &'a Map<String, Value>,
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
    let message_tokens = input
        .pre_llm_messages
        .iter()
        .map(estimate_json_tokens)
        .fold(0_u32, u32::saturating_add);
    let tool_result_tokens = input
        .tool_results
        .iter()
        .map(estimate_json_tokens)
        .fold(0_u32, u32::saturating_add);
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
        Some(astra_turn_types::NormalizedPromptCacheUsage {
            fresh_input_tokens: input.observed_fresh_input_tokens.unwrap_or_default(),
            cache_read_tokens: input.observed_cache_read_tokens.unwrap_or_default(),
            cache_creation_tokens: input.observed_cache_creation_tokens.unwrap_or_default(),
        })
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

/// Assemble bridge context through the shared context boundary.
///
/// This is an intentional compatibility adapter. It lets the CLI/HTTP bridge
/// stop calling prompt-cache internals directly before we collapse bridge
/// source collection into the same normalized input used by server/web agent
/// sessions.
pub(crate) fn assemble_bridge_context(
    input: BridgeContextAssemblyInput<'_>,
) -> BridgeContextAssemblyOutput {
    let effective_tool_schemas = input.tool_surface.effective_tools();
    let effective_tool_names: Vec<&str> = effective_tool_schemas
        .iter()
        .filter_map(tool_name)
        .collect();
    let mut extra_volatile_sections = input.runtime_signals.extra_volatile_sections.to_vec();
    if effective_tool_names.contains(&"memory")
        && let Some(selection) = astra_tools::memoria::MemoriaToolGateway::latest_selection_context(
            input.session.session_id,
        )
    {
        extra_volatile_sections.push(crate::prompts::PromptSection::dynamic(
            selection,
            crate::prompts::PromptTokenBucket::Environment,
        ));
    }
    let _tool_surface_metadata = (
        input.tool_surface.visible_tools.len(),
        input.tool_surface.always_load_tools.len(),
        input.tool_surface.dynamic_tools.len(),
        input.tool_surface.required_tools.len(),
        input.tool_surface.restricted_tools.len(),
    );
    let outcome = crate::turn::prompt_cache::assemble_bridge_pipeline_outcome_with_messages(
        &effective_tool_names,
        &effective_tool_schemas,
        input.runtime_signals.extra_stable_sections,
        &extra_volatile_sections,
        input.runtime_signals.memory_entries,
        input.runtime_signals.session_memory_entry.as_ref(),
        input.runtime_signals.system_override,
        input.session.cache_cfg,
        input.session.cache_capability,
        input.session.session_id,
        input.session.model_id,
        input.session.context_window,
        input.session.provider,
        input.session.edge_profile_cwd,
        input.session.edge_profile_git_branch,
        input.session.project_context,
        input.tool_surface.deferred_tools_block,
        input.session.skill_listing_block,
        input.session.current_date,
        input.conversation_messages,
    );
    let system_prompt_tokens = estimate_json_tokens(&outcome.primary_system).saturating_add(
        outcome
            .dynamic_system
            .as_ref()
            .map(estimate_json_tokens)
            .unwrap_or(0),
    );
    let stable_system_message_count = 1;
    let volatile_preamble_count = usize::from(outcome.dynamic_system.is_some());
    let tool_schema_count = outcome.tool_schemas.len();
    let compaction_tier = format!("{:?}", outcome.tier);
    let model_context_window_tokens = u32::try_from(
        crate::prompts::budget_for_model_with_override(
            Some(input.session.model_id),
            input.session.context_window,
        )
        .model_limit,
    )
    .unwrap_or(u32::MAX);
    BridgeContextAssemblyOutput {
        primary_system: outcome.primary_system,
        dynamic_system: outcome.dynamic_system,
        messages: outcome.messages,
        prompt_sections: outcome.prompt_sections,
        tier: outcome.tier,
        tool_schemas: outcome.tool_schemas,
        manifest_trace: LlmContextManifestTrace {
            source: "llm_context_bridge",
            provider: input.session.provider.to_string(),
            model_name: input.session.model_id.to_string(),
            model_context_window_tokens,
            compaction_tier,
            system_prompt_tokens,
            stable_system_message_count,
            volatile_preamble_count,
            tool_schema_count,
            runtime_manifest: runtime_manifest_with_memory_context(
                Some(json!({
                    "schema_version": "astra_runtime_manifest.v1",
                    "selected_model": {
                        "model": input.session.model_id,
                    },
                    "model_resolution": {
                        "source": "bridge_request",
                        "model": input.session.model_id,
                        "provider": input.session.provider,
                        "resolved": true,
                    },
                    "runtime_profile": astra_runtime_env::CapacityProviderType::CliLocal.as_str(),
                })),
                input.runtime_signals.memory_provider_source,
                input.runtime_signals.memory_entries.len(),
                input.runtime_signals.session_memory_entry.is_some(),
            ),
        },
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
    let cache_cap = CacheCapability::from_explicit_or_provider_model(
        input.cache_capability,
        input.provider,
        input.model_name,
    );
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
    let model_identity_section =
        crate::turn::prompt_cache::model_identity_prompt_section_for_cache_capability(
            input.cache_capability,
            input.provider,
            input.model_name,
        );
    if matches!(
        model_identity_section.scope,
        astra_turn_core::section_types::CacheScope::None
    ) {
        external.extra_dynamic_sections.push(model_identity_section);
    } else {
        external.extra_stable_sections.push(model_identity_section);
    }
    external.session_memory_entry = input.runtime_signals.session_memory_entry.clone();
    let turn_state = build_turn_state(state, input.user_content);
    // `AgenticLoopState::max_turn_input_tokens` is an input-budget/wind-down
    // cap, and `0` is its legacy "unlimited" sentinel. The pipeline's
    // `SessionContext::model_limit` is different: it must be the concrete
    // model context window used for section budgeting and pressure planning.
    let model_context_limit = u64::try_from(
        crate::prompts::budget_for_model_with_override(
            Some(input.model_name),
            input.context_window,
        )
        .model_limit,
    )
    .unwrap_or(u64::MAX);
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
    if !input.tool_surface.deferred_tools_block.is_empty() {
        session_ctx.deferred_tools_block = input.tool_surface.deferred_tools_block.to_string();
    }
    let statics = crate::prompts::build_pipeline_static_sections();
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

    let pipeline_output = match pipeline_result {
        Ok(out) => out,
        Err(abort) => {
            record_pipeline_abort(state, &abort);
            return Err(classify_pipeline_abort(abort));
        }
    };

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

    let (system_messages, mut volatile_preamble) = match cache_cap.volatile_placement {
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
            let preamble = volatile_preamble_from_text(volatile_text, inject_volatile);
            (system, preamble)
        }
        VolatilePlacement::TailSuffix
        | VolatilePlacement::CurrentUserOnly
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
            let preamble = volatile_preamble_from_text(volatile_text, inject_volatile);
            (system, preamble)
        }
    };
    let mut required_runtime_texts = astra_turn_core::chat_turn_edge_profile::edge_profile_texts(
        input.runtime_signals.edge_profile,
        astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS,
    );
    required_runtime_texts.extend(
        astra_turn_core::chat_turn_edge_profile::edge_profile_runtime_volatile_injections(
            input.runtime_signals.edge_profile,
        )
        .into_iter()
        .filter(|injection| {
            injection.delivery_class
                == astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext
        })
        .filter_map(|injection| injection.render_for_prompt()),
    );
    if let Some(required_text) = crate::turn::wire_assembly::required_runtime_preamble_message(
        &required_runtime_texts.join("\n\n"),
    ) {
        volatile_preamble.push(required_text);
    }
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
    let model_context_window_tokens = u32::try_from(model_context_limit).unwrap_or(u32::MAX);

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
        "content": ensure_system_reminder_wrapper(&text),
    })]
}

fn ensure_system_reminder_wrapper(text: &str) -> String {
    crate::turn::wire_assembly::system_reminder_wrapped_text(text)
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
                ),
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
pub(crate) fn assemble_wire_messages(input: LlmWireAssemblyInput<'_>) -> Vec<Value> {
    queue_active_turn_frame(input.state);
    let drained = input.state.take_volatile_pending();

    let mut skills: Vec<_> = input.state.skills.invoked.values().collect();
    skills.sort_by_key(|skill| std::cmp::Reverse(skill.invoked_at_turn));
    let invoked_skills: Vec<crate::turn::wire_assembly::InvokedSkillRef<'_>> = skills
        .iter()
        .map(|skill| crate::turn::wire_assembly::InvokedSkillRef {
            name: skill.name.as_str(),
            content: skill.content.as_str(),
        })
        .collect();
    let attachments = crate::turn::wire_assembly::PostCompactAttachments {
        invoked_skills,
        recent_file_reads: &input.state.recent_file_reads,
        cwd: input.edge_profile.get("cwd").and_then(Value::as_str),
    };

    crate::turn::wire_assembly::assemble_llm_messages_with_cache_capability(
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
    )
}

fn queue_active_turn_frame(state: &mut AgenticLoopState) {
    let latest_user_message = state.message.trim();
    if latest_user_message.is_empty() {
        return;
    }
    let frame = json!({
        "latest_user_message": latest_user_message,
        "active_goal": latest_user_message,
        "turn_id": state.session_turn,
        "round_id": state.llm_rounds_completed,
        "instruction": "Answer the latest user message first. History, memory, and tool results are evidence for this goal; do not finish with an answer to an older question."
    });
    state.push_volatile_payload(
        crate::turn::agentic_loop::host::VolatileKind::ActiveTurnFrame,
        frame,
    );
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
                serde_json::to_string(left)
                    .unwrap_or_default()
                    .cmp(&serde_json::to_string(right).unwrap_or_default())
            })
    });
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
        return current_tool_schemas.to_vec();
    }

    let visible_names: HashSet<&str> = visible_tool_schemas.iter().filter_map(tool_name).collect();
    let current_names: HashSet<&str> = current_tool_schemas.iter().filter_map(tool_name).collect();
    let mut stabilized = Vec::new();
    let mut seen = HashSet::new();

    for schema in previous_tool_schemas {
        let Some(name) = tool_name(schema) else {
            continue;
        };
        if visible_names.contains(name) && current_names.contains(name) {
            push_unique_tool(&mut stabilized, &mut seen, schema);
        }
    }
    for schema in current_tool_schemas {
        push_unique_tool(&mut stabilized, &mut seen, schema);
    }

    if stabilized.is_empty() {
        current_tool_schemas.to_vec()
    } else {
        stabilized
    }
}

pub(crate) fn apply_bridge_message_cache_metadata(
    messages: &mut [Value],
    synthetic_tail_prefix_end: Option<usize>,
    cache_cfg: &PromptCacheConfig,
    session_id: &str,
) {
    if let Some(prefix_end) = synthetic_tail_prefix_end {
        crate::turn::prompt_cache::apply_anthropic_cache_metadata(
            &mut messages[..prefix_end],
            cache_cfg,
            session_id,
        );
    } else {
        crate::turn::prompt_cache::apply_anthropic_cache_metadata(messages, cache_cfg, session_id);
    }
}

pub(crate) struct BridgeRetryWireRebuildInput<'a> {
    pub previous_messages: &'a [Value],
    pub compacted_messages: Vec<Value>,
    pub boundary_present: bool,
    pub required_runtime_text: Option<String>,
    pub provider: &'a str,
    pub model_name: &'a str,
    pub thinking: &'a astra_turn_core::thinking_config::ThinkingConfig,
    pub cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    pub cache_cfg: &'a PromptCacheConfig,
    pub session_id: &'a str,
}

pub(crate) fn bridge_retry_compaction_history(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| !crate::turn::wire_assembly::is_required_runtime_preamble(message))
        .cloned()
        .map(|mut message| {
            crate::turn::wire_assembly::strip_runtime_context_from_tool_message(&mut message);
            message
        })
        .collect()
}

pub(crate) fn rebuild_bridge_retry_wire_messages(
    input: BridgeRetryWireRebuildInput<'_>,
) -> Vec<Value> {
    let system_prefix_count = input
        .previous_messages
        .iter()
        .take_while(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .count();
    let mut messages = input.previous_messages[..system_prefix_count].to_vec();
    let mut compacted_messages = input.compacted_messages;
    crate::turn::wire_assembly::maybe_append_continuation_prompt(
        &mut compacted_messages,
        input.boundary_present,
    );
    messages.extend(compacted_messages);
    let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
        &mut messages,
        None,
        input.required_runtime_text,
        input.provider,
        input.model_name,
        input.thinking,
        input.cache_capability,
        input.cache_cfg,
    );
    apply_bridge_message_cache_metadata(
        &mut messages,
        synthetic_tail_prefix_end,
        input.cache_cfg,
        input.session_id,
    );
    messages
}

/// Finalize bridge wire messages after bridge-specific compaction and context
/// release have run.
///
/// The bridge currently compacts and mutates its message vector inline. This
/// helper centralizes the cache-sensitive tail rule shared with
/// [`assemble_wire_messages`]: runtime control text must stay out of
/// post-prefix system messages and ride the provider-valid tail suffix.
pub(crate) fn finalize_bridge_wire_messages(
    llm_messages: &mut Vec<Value>,
    volatile_text: Option<String>,
    required_runtime_text: Option<String>,
    provider: &str,
    model_name: &str,
    thinking: &astra_turn_core::thinking_config::ThinkingConfig,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    cache_cfg: &PromptCacheConfig,
) -> Option<usize> {
    let reasoning_policy = astra_turn_core::edge_ledger::ReasoningReplayPolicy::infer(
        llm_messages,
        thinking,
        provider,
        model_name,
    );
    astra_turn_core::edge_ledger::strip_stale_reasoning_with_policy(
        llm_messages,
        &reasoning_policy,
    );
    let cache_cap =
        astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider_model(
            cache_capability,
            provider,
            model_name,
        );
    let mut synthetic_tail_prefix_end = None;
    let suppress_volatile = matches!(
        cache_cap.volatile_placement,
        astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly
    );
    let mut runtime_parts = Vec::new();
    if let Some(text) = required_runtime_text
        && !text.trim().is_empty()
    {
        runtime_parts.push(text);
    }
    if !suppress_volatile
        && let Some(text) = volatile_text
        && !text.trim().is_empty()
    {
        runtime_parts.push(text);
    }
    let text = runtime_parts.join("\n\n");
    if !text.is_empty() {
        let tail_role = llm_messages
            .last()
            .and_then(|m| m.get("role").and_then(Value::as_str));
        if tail_role == Some("user") {
            let tail_index = llm_messages.len().saturating_sub(1);
            if let Some(tail) = llm_messages.last_mut() {
                crate::turn::wire_assembly::append_volatile_to_tail_user_message(
                    tail,
                    &text,
                    cache_cfg.should_annotate(),
                );
            }
            synthetic_tail_prefix_end = Some(tail_index);
        } else if tail_role == Some("tool") {
            let tail_index = llm_messages.len().saturating_sub(1);
            if let Some(tail) = llm_messages.last_mut() {
                crate::turn::wire_assembly::append_runtime_context_to_tail_tool_message(
                    tail, &text,
                );
            }
            synthetic_tail_prefix_end = Some(tail_index);
        } else {
            synthetic_tail_prefix_end = Some(llm_messages.len());
            llm_messages.push(serde_json::json!({
                "role": "user",
                "content": text,
            }));
        }
    }
    synthetic_tail_prefix_end
}

pub(crate) fn augment_manifest_trace_with_wire(
    trace: &mut Value,
    messages: &[Value],
    tool_schemas: &[Value],
) {
    let message_cache_control_count = messages.iter().map(cache_control_count).sum::<usize>();
    let tool_cache_control_count = tool_schemas.iter().map(cache_control_count).sum::<usize>();
    let message_roles: Vec<String> = messages.iter().map(message_role).collect();
    let conversation_messages: Vec<Value> = messages
        .iter()
        .filter(|message| message_role(message) != "system")
        .cloned()
        .collect();
    let conversation_roles: Vec<String> = conversation_messages.iter().map(message_role).collect();
    let system_messages: Vec<Value> = messages
        .iter()
        .filter(|message| message_role(message) == "system")
        .cloned()
        .collect();
    let stable_system_prefix = stable_cache_prefix(&system_messages);
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

    if let Some(trace_obj) = trace.as_object_mut() {
        trace_obj.insert(
            "wire".to_string(),
            serde_json::json!({
                "message_count": messages.len(),
                "tool_schema_count": tool_schemas.len(),
                "message_roles": message_roles.clone(),
                "message_role_counts": role_counts_json(&message_roles),
                "estimated_message_tokens": estimated_message_tokens,
                "message_cache_control_count": message_cache_control_count,
                "tool_cache_control_count": tool_cache_control_count,
                "total_cache_control_count": message_cache_control_count + tool_cache_control_count,
                "conversation_projection": {
                    "message_count": conversation_messages.len(),
                    "message_roles": conversation_roles.clone(),
                    "role_counts": role_counts_json(&conversation_roles),
                    "estimated_tokens": estimated_conversation_tokens,
                },
                "fingerprint": {
                    "message_sequence_sha256": canonical_wire_sha256(&Value::Array(messages.to_vec())),
                    "system_message_sequence_sha256": canonical_wire_sha256(&Value::Array(system_messages.clone())),
                    "conversation_message_sequence_sha256": canonical_wire_sha256(&Value::Array(conversation_messages)),
                    "tool_schema_sequence_sha256": canonical_wire_sha256(&Value::Array(tool_schemas.to_vec())),
                    "system_and_tools_sha256": canonical_wire_sha256(&json!({
                        "system_messages": system_messages,
                        "tool_schemas": tool_schemas,
                    })),
                    "prompt_cache_identity": prompt_cache_identity,
                    "message_hashes": message_hashes,
                },
            }),
        );
    }
}

fn stable_cache_prefix(values: &[Value]) -> Vec<Value> {
    match values
        .iter()
        .rposition(|value| cache_control_count(value) > 0)
    {
        Some(last_marker) => values[..=last_marker].to_vec(),
        None => values.to_vec(),
    }
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
    fn assemble_context_pipeline_keeps_strict_history_model_visible_without_volatile_preamble() {
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
        let visible_tools = vec![tool("bash")];
        let restricted_tools = HashSet::new();
        let volatile = vec![crate::prompts::PromptSection::dynamic(
            "## Volatile\nmust be suppressed".to_string(),
            crate::prompts::PromptTokenBucket::Environment,
        )];
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let strict_history = astra_turn_core::cache_placement::CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement:
                astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
            reuse_scope: None,
        };

        let output = assemble_context_pipeline(LlmContextAssemblyInput {
            state: &mut state,
            session_id: "sid-deepseek",
            tool_surface: ToolSurfacePlan::from_visible_tools(&visible_tools, &restricted_tools),
            runtime_signals: RuntimeSignals::new(&edge_profile, None)
                .with_extra_sections(&[], &volatile),
            cache_cfg: &cache_cfg,
            provider: "openai",
            model_name: "deepseek-v4-pro-official(thinking:high)",
            context_window: Some(1_000_000),
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
            primary_text.contains("Model: deepseek-v4-pro-official(thinking:high) (via openai)"),
            "strict-history model identity must survive volatile suppression: {primary_text}"
        );
        assert!(
            !primary_text.contains("must be suppressed"),
            "ordinary volatile content must stay out of strict-history stable prompt: {primary_text}"
        );
        assert_eq!(output.volatile_preamble.len(), 1);
        assert!(crate::turn::wire_assembly::is_required_runtime_preamble(
            &output.volatile_preamble[0]
        ));
        assert_eq!(
            output.volatile_preamble[0]["role"].as_str(),
            Some("system"),
            "required control policy must remain runtime-owned before provider wire adaptation"
        );
        assert_eq!(
            output.volatile_preamble[0]["content"].as_str(),
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
            "system prompt telemetry must report estimated tokens, not section count"
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
        let edge_profile = Map::new();
        let cache_cfg = PromptCacheConfig::latch("openai", "gpt-4");

        let messages = assemble_wire_messages(LlmWireAssemblyInput {
            system_messages: vec![json!({"role": "system", "content": "sys"})],
            volatile_preamble: Vec::new(),
            compacted_messages: state.messages.clone(),
            state: &mut state,
            thinking: &thinking,
            edge_profile: &edge_profile,
            session_id: "sid",
            provider: "openai",
            model_name: "gpt-4",
            cache_capability: None,
            cache_cfg: &cache_cfg,
        });

        let user_text = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(user_text.contains("相关的测试够硬核吗"));
        assert_eq!(messages[3]["role"], "user");
        let tail_user = message_text(&messages[3]);
        assert!(
            tail_user.starts_with("相关的测试够硬核吗？"),
            "real user content must remain first: {tail_user}"
        );
        assert!(
            tail_user.contains("<runtime-required-context>"),
            "runtime goal frame must be visible in the protocol-valid tail suffix"
        );
        assert!(tail_user.contains("\"turn_id\":7"));
        assert!(tail_user.contains("\"round_id\":3"));
        assert!(
            messages
                .iter()
                .skip(1)
                .all(|message| message.get("role").and_then(Value::as_str) != Some("system")),
            "wire messages must not introduce post-prefix system messages: {messages:#?}"
        );
        let system_text = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !system_text.contains("<runtime-required-context>"),
            "active-turn runtime context must not mutate the stable system lane"
        );
        assert!(
            state.volatile_pending.is_empty(),
            "active frame must be one-shot per LLM request"
        );
    }

    #[test]
    fn assemble_bridge_context_reports_configured_context_window() {
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let visible_tools: Vec<Value> = Vec::new();
        let restricted_tools = HashSet::new();
        let memory_entries = vec![
            astra_turn_core::context_sources::MemoryEntry::scored(
                "typed bridge memory evidence",
                0.9,
            )
            .with_memory_identity("bridge-memory-1", "semantic"),
        ];

        let output = assemble_bridge_context(BridgeContextAssemblyInput {
            conversation_messages: &[],
            tool_surface: ToolSurfacePlan::from_visible_tools(&visible_tools, &restricted_tools),
            runtime_signals: BridgeRuntimeSignals::new(&[], &[], &memory_entries, None, None)
                .with_memory_provider_source(Some("request_binding")),
            session: BridgeSessionContextInput::new(
                &cache_cfg,
                None,
                "sid-bridge-context-window",
                "deepseek-v4-pro-official",
                "openai",
                None,
                None,
                None,
                "2026-07-01",
            )
            .with_context_window(Some(1_000_000)),
        });

        assert_eq!(
            output.manifest_trace.to_json()["model_context_window_tokens"],
            json!(1_000_000),
            "bridge context assembly must preserve the resolved model context_window"
        );
        assert_eq!(
            output.manifest_trace.to_json()["runtime_manifest"]["runtime_profile"],
            astra_runtime_env::CapacityProviderType::CliLocal.as_str(),
            "the /chat/turn adapter must surface CLI local capacity, not an implementation class name"
        );
        assert_eq!(
            output.manifest_trace.to_json()["runtime_manifest"]["memory_context"],
            json!({
                "provider_source": "request_binding",
                "prompt_entry_count": 1,
                "session_snapshot_injected": false,
                "delivery": "typed_runtime_dynamic",
            })
        );
    }

    #[test]
    fn bridge_context_carries_the_latest_resource_selection_only_with_its_tool() {
        let session_id = "bridge-resource-selection";
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(session_id);
        astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
            session_id,
            "bridge-turn",
            3,
            vec!["memory-1".to_string(), "memory-2".to_string()],
        );
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let memory_tool = vec![tool("memory")];
        let unrestricted = HashSet::new();
        let assemble = |restricted_tools: &HashSet<String>| {
            assemble_bridge_context(BridgeContextAssemblyInput {
                conversation_messages: &[],
                tool_surface: ToolSurfacePlan::from_visible_tools(&memory_tool, restricted_tools),
                runtime_signals: BridgeRuntimeSignals::new(&[], &[], &[], None, None),
                session: BridgeSessionContextInput::new(
                    &cache_cfg,
                    None,
                    session_id,
                    "test-model",
                    "openai",
                    None,
                    None,
                    None,
                    "2026-07-20",
                ),
            })
        };

        let visible = assemble(&unrestricted);
        let selection = visible
            .prompt_sections
            .iter()
            .filter_map(|section| serde_json::from_str::<serde_json::Value>(&section.text).ok())
            .find(|section| section["schema"] == "astra.resource_selection.v1")
            .expect("typed selection section");
        assert_eq!(selection["resource_kind"], "memory");
        assert_eq!(
            selection["identities"],
            serde_json::json!(["memory-1", "memory-2"])
        );

        let restricted = HashSet::from(["memory".to_string()]);
        let hidden = assemble(&restricted);
        assert!(hidden.prompt_sections.iter().all(|section| {
            serde_json::from_str::<serde_json::Value>(&section.text)
                .ok()
                .is_none_or(|value| value["schema"] != "astra.resource_selection.v1")
        }));
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(session_id);
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
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            },
            1,
        );

        assert_eq!(tool_names(&stabilized), vec!["bash", "read_file"]);
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
            result_prompt_tokens: Some(500),
            observed_fresh_input_tokens: Some(100),
            observed_cache_read_tokens: Some(300),
            observed_cache_creation_tokens: Some(100),
            observed_output_tokens: Some(20),
            assembly_trace: None,
            turn_intent: "implementation",
            reason: "test",
            context_window_tokens: 64_000,
        });

        assert_eq!(
            projection.manifest_json["normalized_prompt_cache_usage"],
            json!({
                "fresh_input_tokens": 100,
                "cache_read_tokens": 300,
                "cache_creation_tokens": 100
            })
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
    fn finalize_bridge_wire_messages_keeps_historical_user_stable_when_tail_is_tool() {
        let mut messages = vec![
            json!({"role": "user", "content": "original user"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "tool output", "tool_call_id": "c1"}),
        ];

        finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            None,
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );

        assert_eq!(messages[0]["content"], "original user");
        assert_eq!(messages[2]["role"], "tool");
        assert!(message_text(&messages[2]).starts_with("tool output"));
        assert!(message_text(&messages[2]).contains("volatile"));
        assert_eq!(
            messages.len(),
            3,
            "runtime framing must not invent any conversation turn"
        );
    }

    #[test]
    fn bridge_cache_annotation_handles_live_tool_loop_shape() {
        let mut messages = vec![
            json!({"role": "system", "content": [{"type": "text", "text": "stable"}]}),
            json!({"role": "user", "content": "Analyze the journal"}),
            json!({"role": "assistant", "content": Value::Null}),
            json!({
                "role": "tool",
                "content": "tool output",
                "tool_call_id": "tooluse_QN9FlElvUeRXS3lbPNtp08",
                "_tool_name": "read_file",
                "_round_index": 0
            }),
        ];
        let cache_cfg = crate::turn::prompt_cache::PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            None,
            "bedrock",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg,
        );
        apply_bridge_message_cache_metadata(
            &mut messages,
            synthetic_tail_prefix_end,
            &cache_cfg,
            "sess",
        );

        assert_eq!(synthetic_tail_prefix_end, Some(3));
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&messages[1]),
            "live tool-loop shape must walk past an empty assistant and mark the stable user message",
        );
        assert!(
            !astra_turn_core::context_serializer::message_has_cache_control(&messages[3]),
            "dynamic tool/runtime tail must remain unannotated",
        );
        assert!(message_text(&messages[3]).contains("volatile"));
    }

    #[test]
    fn bridge_retry_compaction_history_excludes_required_runtime_tail() {
        let required_tail =
            crate::turn::wire_assembly::required_runtime_preamble_message("required runtime")
                .expect("required runtime tail");
        let messages = vec![
            json!({"role": "user", "content": "keep"}),
            required_tail,
            json!({"role": "assistant", "content": "keep too"}),
            json!({
                "role": "tool",
                "tool_call_id": "call-1",
                "content": "tool evidence\n\n<runtime-context-after-tool>\nold runtime\n</runtime-context-after-tool>"
            }),
        ];

        let history = bridge_retry_compaction_history(&messages);

        assert_eq!(history.len(), 3);
        assert_eq!(history[0]["content"], "keep");
        assert_eq!(history[1]["content"], "keep too");
        assert_eq!(history[2]["content"], "tool evidence");
        assert!(
            history
                .iter()
                .all(|message| !crate::turn::wire_assembly::is_required_runtime_preamble(message)),
            "retry compaction input must not preserve the prior synthetic runtime tail"
        );
    }

    #[test]
    fn rebuild_bridge_retry_wire_messages_reapplies_runtime_tail_and_cache_metadata() {
        let previous_messages = vec![
            json!({"role": "system", "content": [{"type": "text", "text": "stable"}]}),
            json!({"role": "user", "content": "oversized history"}),
            crate::turn::wire_assembly::required_runtime_preamble_message("old runtime")
                .expect("old runtime tail"),
        ];
        let cache_cfg = crate::turn::prompt_cache::PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };

        let messages = rebuild_bridge_retry_wire_messages(BridgeRetryWireRebuildInput {
            previous_messages: &previous_messages,
            compacted_messages: vec![json!({"role": "user", "content": "compacted retry"})],
            boundary_present: false,
            required_runtime_text: Some("required retry runtime".to_string()),
            provider: "bedrock",
            model_name: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            cache_capability: None,
            cache_cfg: &cache_cfg,
            session_id: "sess",
        });

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        let content_blocks = messages[1]["content"].as_array().expect("content blocks");
        assert_eq!(content_blocks[0]["text"], "compacted retry");
        assert_eq!(content_blocks[1]["text"], "required retry runtime");
        assert!(
            content_blocks[0].get("cache_control").is_some(),
            "retry wire rebuild must mark the real user text before the runtime suffix"
        );
        assert!(
            content_blocks[1].get("cache_control").is_none(),
            "runtime suffix must stay outside the cache-marked block"
        );
    }

    #[test]
    fn finalize_bridge_wire_messages_appends_runtime_suffix_when_user_available() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];

        let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            None,
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );

        assert_eq!(synthetic_tail_prefix_end, Some(0));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let tail_text = message_text(&messages[0]);
        assert!(tail_text.starts_with("original user"));
        assert!(tail_text.contains("volatile"));
    }

    #[test]
    fn finalize_bridge_wire_messages_keeps_runtime_wrapper_after_user_content() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];

        finalize_bridge_wire_messages(
            &mut messages,
            Some("<system-reminder>\nvolatile</system-reminder>".to_string()),
            None,
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );

        assert_eq!(messages.len(), 1);
        let tail_text = message_text(&messages[0]);
        assert!(tail_text.starts_with("original user"));
        assert!(tail_text.contains("<system-reminder>\nvolatile</system-reminder>"));
    }

    #[test]
    fn finalize_bridge_wire_messages_noops_when_volatile_text_is_absent() {
        let mut messages = vec![json!({"role": "assistant", "content": "stable"})];

        let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
            &mut messages,
            None,
            None,
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );

        assert!(synthetic_tail_prefix_end.is_none());
        assert_eq!(
            messages,
            vec![json!({"role": "assistant", "content": "stable"})]
        );
    }

    #[test]
    fn finalize_bridge_wire_messages_skips_current_user_only_models() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];

        let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            None,
            "openai",
            "deepseek-v4-flash",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "deepseek-v4-flash"),
        );

        assert!(synthetic_tail_prefix_end.is_none());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "original user");
    }

    #[test]
    fn finalize_bridge_wire_messages_keeps_required_context_for_current_user_only_models() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];

        let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
            &mut messages,
            Some("best effort volatile".to_string()),
            Some("required resume context".to_string()),
            "openai",
            "deepseek-v4-flash",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "deepseek-v4-flash"),
        );

        assert_eq!(synthetic_tail_prefix_end, Some(0));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let tail_text = message_text(&messages[0]);
        assert!(tail_text.starts_with("original user"));
        assert!(tail_text.contains("required resume context"));
        assert!(!tail_text.contains("best effort volatile"));
    }

    #[test]
    fn finalize_bridge_wire_messages_uses_explicit_cache_capability() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];
        let explicit = astra_turn_core::cache_placement::CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement:
                astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly,
            reuse_scope: Some(astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns),
        };

        let synthetic_tail_prefix_end = finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            None,
            "openai",
            "gpt-4o",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            Some(explicit),
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );

        assert!(synthetic_tail_prefix_end.is_none());
        assert_eq!(messages[0]["content"], "original user");
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
