//! Shared LLM context assembly entry points.
//!
//! This module is the runtime boundary for cache-sensitive prompt assembly.
//! Callers may collect context differently (CLI edge payloads, web-agent
//! server state, bridge requests), but once they have normalized inputs they
//! should route through this module instead of hand-assembling system prompts,
//! volatile preambles, provider cache placement, or tier-pruned tool schemas.

use std::collections::HashSet;

use astra_services::SessionArtifactStore;
use serde_json::{Map, Value, json};

use super::agentic_loop_host::AgenticLoopState;
use super::prompt_cache::PromptCacheConfig;

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
    (value.to_string().len() as u32 / 4).saturating_add(1)
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
    pub pinned_tools: &'a [Value],
    pub dynamic_tools: &'a [Value],
    pub required_tools: &'a [Value],
    pub deferred_tools_block: &'a str,
    pub restricted_tools: &'a HashSet<String>,
    pub selection_trace: Option<Value>,
}

impl<'a> ToolSurfacePlan<'a> {
    pub(crate) fn from_visible_tools(
        visible_tools: &'a [Value],
        restricted_tools: &'a HashSet<String>,
    ) -> Self {
        Self {
            visible_tools,
            pinned_tools: visible_tools,
            dynamic_tools: &[],
            required_tools: &[],
            deferred_tools_block: "",
            restricted_tools,
            selection_trace: None,
        }
    }

    fn effective_tools(&self) -> Vec<Value> {
        let tools = effective_tool_schemas(
            self.visible_tools,
            self.pinned_tools,
            self.dynamic_tools,
            self.required_tools,
        );
        filter_restricted_tool_schemas(tools, self.restricted_tools)
    }

    pub(crate) fn with_selection_trace(mut self, trace: Option<Value>) -> Self {
        self.selection_trace = trace;
        self
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
    pub selection_confidence: f64,
    pub extra_stable_sections: &'a [crate::prompts::PromptSection],
    pub extra_volatile_sections: &'a [crate::prompts::PromptSection],
}

impl<'a> RuntimeSignals<'a> {
    pub(crate) fn new(
        edge_profile: &'a Map<String, Value>,
        plan_resume_hint: Option<String>,
        selection_confidence: f64,
    ) -> Self {
        Self {
            edge_profile,
            plan_resume_hint,
            selection_confidence,
            extra_stable_sections: &[],
            extra_volatile_sections: &[],
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
}

/// Output of the shared context-pipeline assembly phase.
///
/// The LLM caller should consume these fields directly. In particular,
/// `tool_schemas` is already pruned by the pipeline tier; downstream code may
/// add provider wire annotations, but must not re-derive the pruning decision.
pub(crate) struct LlmContextAssemblyOutput {
    pub system_messages: Vec<Value>,
    pub volatile_preamble: Vec<Value>,
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
    pub compaction_tier: String,
    pub system_prompt_tokens: u32,
    pub stable_system_message_count: usize,
    pub volatile_preamble_count: usize,
    pub tool_schema_count: usize,
    pub selection_trace: Option<Value>,
}

impl LlmContextManifestTrace {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "provider": self.provider.clone(),
            "model_name": self.model_name.clone(),
            "compaction_tier": self.compaction_tier.clone(),
            "system_prompt_tokens": self.system_prompt_tokens,
            "stable_system_message_count": self.stable_system_message_count,
            "volatile_preamble_count": self.volatile_preamble_count,
            "tool_schema_count": self.tool_schema_count,
            "selection_trace": self.selection_trace.clone(),
        })
    }
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

/// Bridge-facing context assembly input.
///
/// The bridge still has a per-request lifecycle and a few bridge-specific
/// source lanes, so this adapter preserves current behavior while routing the
/// call through the shared context boundary. The implementation delegates to
/// the existing bridge pipeline helper until the bridge source collection is
/// fully normalized into [`LlmContextAssemblyInput`].
pub(crate) struct BridgeContextAssemblyInput<'a> {
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
    pinned_tools: &[Value],
    dynamic_tools: &[Value],
    required_tools: &[Value],
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for schema in pinned_tools {
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
    pub selection_confidence: f64,
    pub task_type: Option<&'a str>,
}

impl<'a> BridgeRuntimeSignals<'a> {
    pub(crate) fn new(
        extra_stable_sections: &'a [crate::prompts::PromptSection],
        extra_volatile_sections: &'a [crate::prompts::PromptSection],
        memory_entries: &'a [astra_turn_core::context_sources::MemoryEntry],
        selection_confidence: f64,
        task_type: Option<&'a str>,
    ) -> Self {
        Self {
            extra_stable_sections,
            extra_volatile_sections,
            memory_entries,
            selection_confidence,
            task_type,
        }
    }
}

pub(crate) struct BridgeSessionContextInput<'a> {
    pub cache_cfg: &'a PromptCacheConfig,
    pub session_id: &'a str,
    pub model_id: &'a str,
    pub provider: &'a str,
    pub edge_profile_cwd: Option<&'a str>,
    pub edge_profile_git_branch: Option<&'a str>,
    pub project_context: Option<&'a str>,
    pub skill_listing_block: &'a str,
}

impl<'a> BridgeSessionContextInput<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cache_cfg: &'a PromptCacheConfig,
        session_id: &'a str,
        model_id: &'a str,
        provider: &'a str,
        edge_profile_cwd: Option<&'a str>,
        edge_profile_git_branch: Option<&'a str>,
        project_context: Option<&'a str>,
    ) -> Self {
        Self {
            cache_cfg,
            session_id,
            model_id,
            provider,
            edge_profile_cwd,
            edge_profile_git_branch,
            project_context,
            skill_listing_block: "",
        }
    }

    pub(crate) fn with_skill_listing_block(mut self, skill_listing_block: &'a str) -> Self {
        self.skill_listing_block = skill_listing_block;
        self
    }
}

pub(crate) struct BridgeContextAssemblyOutput {
    pub primary_system: Value,
    pub dynamic_system: Option<Value>,
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
    pub edge_profile: &'a Map<String, Value>,
    pub session_id: &'a str,
    pub provider: &'a str,
    pub model_name: &'a str,
    pub cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    pub cache_cfg: &'a PromptCacheConfig,
}

pub(crate) struct ContextManifestProjectionInput<'a> {
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
            source_table: "tool_runner_registry".to_string(),
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
    let _tool_surface_metadata = (
        input.tool_surface.visible_tools.len(),
        input.tool_surface.pinned_tools.len(),
        input.tool_surface.dynamic_tools.len(),
        input.tool_surface.required_tools.len(),
        input.tool_surface.restricted_tools.len(),
        input.tool_surface.selection_trace.as_ref(),
    );
    let outcome = crate::turn::prompt_cache::assemble_bridge_pipeline_outcome(
        &effective_tool_names,
        &effective_tool_schemas,
        input.runtime_signals.extra_stable_sections,
        input.runtime_signals.extra_volatile_sections,
        input.runtime_signals.memory_entries,
        input.runtime_signals.selection_confidence,
        input.runtime_signals.task_type,
        input.session.cache_cfg,
        input.session.session_id,
        input.session.model_id,
        input.session.provider,
        input.session.edge_profile_cwd,
        input.session.edge_profile_git_branch,
        input.session.project_context,
        input.tool_surface.deferred_tools_block,
        input.session.skill_listing_block,
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
    BridgeContextAssemblyOutput {
        primary_system: outcome.primary_system,
        dynamic_system: outcome.dynamic_system,
        prompt_sections: outcome.prompt_sections,
        tier: outcome.tier,
        tool_schemas: outcome.tool_schemas,
        manifest_trace: LlmContextManifestTrace {
            source: "llm_context_bridge",
            provider: input.session.provider.to_string(),
            model_name: input.session.model_id.to_string(),
            compaction_tier,
            system_prompt_tokens,
            stable_system_message_count,
            volatile_preamble_count,
            tool_schema_count,
            selection_trace: input.tool_surface.selection_trace.clone(),
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
        input.tool_surface.pinned_tools.len(),
        input.tool_surface.dynamic_tools.len(),
        input.tool_surface.required_tools.len(),
        input.tool_surface.restricted_tools.len(),
        input.tool_surface.selection_trace.as_ref(),
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

    let mut external = build_external_sources(
        input.runtime_signals.edge_profile,
        state,
        input.user_content,
        &tool_names,
        input.runtime_signals.selection_confidence,
        input.runtime_signals.plan_resume_hint.as_deref(),
        Some(cache_cap),
    );
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
    let turn_state = build_turn_state(state, input.user_content);
    // `AgenticLoopState::max_turn_input_tokens` is an input-budget/wind-down
    // cap, and `0` is its legacy "unlimited" sentinel. The pipeline's
    // `SessionContext::model_limit` is different: it must be the concrete
    // model context window used for section budgeting and pressure planning.
    let model_context_limit =
        u64::try_from(crate::prompts::budget_for_model(Some(input.model_name)).model_limit)
            .unwrap_or(u64::MAX);
    let mut session_ctx = build_session_context(
        input.session_id,
        state.current_run_id.as_deref(),
        input.model_name,
        model_context_limit,
        input.runtime_signals.edge_profile,
        input.provider,
        state.project_context.as_deref(),
        Some(cache_cap),
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
            .expect("pipeline_session must be initialized for all production paths");
        if let Some(session_id) = state.current_session_id.as_deref()
            && let Ok(session_dir) =
                astra_services::local_session_artifact_store().session_dir(session_id)
        {
            pipeline_sess.set_prompt_cache_diff_dir(session_dir.join("prompt-cache-diffs"));
        }
        pipeline_sess.run_turn_adaptive(adaptive)
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
    let breakdown = astra_turn_core::context_assembly_trace::SystemPromptBreakdown {
        total_tokens: pipeline_output.metrics.sections,
        ..Default::default()
    };

    let round_within_turn = state.current_round_index;
    let inject_volatile = cache_cap.should_inject_volatile_on_round(round_within_turn);

    let (system_messages, volatile_preamble) = match cache_cap.volatile_placement {
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
    let stable_system_message_count = system_messages.len();
    let volatile_preamble_count = volatile_preamble.len();
    let tool_schema_count = pipeline_output.optimized.tool_schemas.len();
    let tier = pipeline_output.plan.compact_tier;
    let compaction_tier = format!("{:?}", tier);

    Ok(LlmContextAssemblyOutput {
        system_messages,
        volatile_preamble,
        system_plain: plain,
        breakdown,
        tier,
        tool_schemas: pipeline_output.optimized.tool_schemas,
        manifest_trace: LlmContextManifestTrace {
            source: "llm_context",
            provider: input.provider.to_string(),
            model_name: input.model_name.to_string(),
            compaction_tier,
            system_prompt_tokens: pipeline_output.metrics.sections,
            stable_system_message_count,
            volatile_preamble_count,
            tool_schema_count,
            selection_trace: input.tool_surface.selection_trace.clone(),
        },
    })
}

fn volatile_preamble_from_text(text: String, inject: bool) -> Vec<Value> {
    if !inject || text.is_empty() {
        return Vec::new();
    }
    vec![
        json!({
            "role": "user",
            "content": format!("<system-reminder>\n{text}</system-reminder>"),
        }),
        json!({
            "role": "assistant",
            "content": "Understood.",
        }),
    ]
}

fn record_pipeline_abort(
    state: &mut AgenticLoopState,
    abort: &astra_turn_core::context_pipeline::PipelineAbort,
) {
    let turn = state.llm_rounds_completed;
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
        input.cache_capability,
        input.cache_cfg,
    )
}

/// Apply provider-specific cache annotations to the final visible tool schemas.
pub(crate) fn annotate_tool_schemas_for_cache(
    tool_schemas: &mut [Value],
    cache_cfg: &PromptCacheConfig,
) {
    crate::turn::prompt_cache::annotate_tool_schemas_for_caching(tool_schemas, cache_cfg);
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
    let mut stabilized = Vec::new();
    let mut seen = HashSet::new();

    for schema in previous_tool_schemas {
        let Some(name) = tool_name(schema) else {
            continue;
        };
        if visible_names.contains(name) {
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

/// Apply provider-specific message cache metadata.
pub(crate) fn apply_message_cache_metadata(
    messages: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    session_id: &str,
) {
    crate::turn::prompt_cache::apply_anthropic_cache_metadata(messages, cache_cfg, session_id);
}

/// Finalize bridge wire messages after bridge-specific compaction and context
/// release have run.
///
/// The bridge currently compacts and mutates its message vector inline. This
/// helper centralizes the cache-sensitive tail rule shared with
/// [`assemble_wire_messages`]: volatile runtime text must live on the true
/// tail, not be spliced into historical user turns.
pub(crate) fn finalize_bridge_wire_messages(
    llm_messages: &mut Vec<Value>,
    volatile_text: Option<String>,
    provider: &str,
    model_name: &str,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
) -> bool {
    astra_turn_core::edge_ledger::strip_stale_reasoning(llm_messages, provider, model_name);
    let cache_cap =
        astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider_model(
            cache_capability,
            provider,
            model_name,
        );
    let mut appended_synthetic_tail = false;
    if let Some(text) = volatile_text
        && !text.is_empty()
    {
        if matches!(
            cache_cap.volatile_placement,
            astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly
        ) {
            return false;
        }
        let wrapped = format!("<system-reminder>\n{text}</system-reminder>");
        let tail_role = llm_messages
            .last()
            .and_then(|m| m.get("role").and_then(Value::as_str));
        if tail_role == Some("user") {
            let last_user = llm_messages
                .last_mut()
                .expect("tail_role=user implies a last message exists");
            let existing = last_user
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            last_user["content"] = Value::String(format!("{wrapped}\n\n{existing}"));
        } else if tail_role == Some("tool") {
            llm_messages.push(serde_json::json!({
                "role": "assistant",
                "content": "Understood.",
            }));
            llm_messages.push(serde_json::json!({"role": "user", "content": wrapped}));
            appended_synthetic_tail = true;
        } else {
            llm_messages.push(serde_json::json!({"role": "user", "content": wrapped}));
            appended_synthetic_tail = true;
        }
    }
    appended_synthetic_tail
}

pub(crate) fn augment_manifest_trace_with_wire(
    trace: &mut Value,
    messages: &[Value],
    tool_schemas: &[Value],
) {
    let message_cache_control_count = messages.iter().map(cache_control_count).sum::<usize>();
    let tool_cache_control_count = tool_schemas.iter().map(cache_control_count).sum::<usize>();

    if let Some(trace_obj) = trace.as_object_mut() {
        trace_obj.insert(
            "wire".to_string(),
            serde_json::json!({
                "message_count": messages.len(),
                "tool_schema_count": tool_schemas.len(),
                "message_cache_control_count": message_cache_control_count,
                "tool_cache_control_count": tool_cache_control_count,
                "total_cache_control_count": message_cache_control_count + tool_cache_control_count,
            }),
        );
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
    fn effective_tool_schemas_merges_priority_dedupes_and_filters_restricted() {
        let visible = vec![tool("visible_a"), tool("pinned_a"), tool("dynamic_a")];
        let pinned = vec![tool("pinned_a"), tool("pinned_b")];
        let dynamic = vec![tool("dynamic_a"), tool("dynamic_b")];
        let required = vec![tool("required_a"), tool("pinned_b")];
        let restricted = HashSet::from(["dynamic_b".to_string()]);

        let merged = filter_restricted_tool_schemas(
            effective_tool_schemas(&visible, &pinned, &dynamic, &required),
            &restricted,
        );

        assert_eq!(
            tool_names(&merged),
            vec![
                "pinned_a",
                "pinned_b",
                "required_a",
                "dynamic_a",
                "visible_a"
            ]
        );
    }

    #[test]
    fn stabilize_tool_schemas_keeps_prior_tools_visible_mid_turn() {
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

        assert_eq!(tool_names(&stabilized), vec!["bash", "read_file", "git"]);
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
            "openai",
            "gpt-4",
            None,
        );

        assert_eq!(messages[0]["content"], "original user");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["content"], "Understood.");
        assert_eq!(
            messages[4]["content"],
            "<system-reminder>\nvolatile</system-reminder>"
        );
    }

    #[test]
    fn finalize_bridge_wire_messages_prepends_to_tail_user_when_available() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];

        finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            "openai",
            "gpt-4",
            None,
        );

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["content"],
            "<system-reminder>\nvolatile</system-reminder>\n\noriginal user"
        );
    }

    #[test]
    fn finalize_bridge_wire_messages_skips_current_user_only_models() {
        let mut messages = vec![json!({"role": "user", "content": "original user"})];

        let appended = finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            "openai",
            "deepseek-v4-flash",
            None,
        );

        assert!(!appended);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "original user");
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

        let appended = finalize_bridge_wire_messages(
            &mut messages,
            Some("volatile".to_string()),
            "openai",
            "gpt-4o",
            Some(explicit),
        );

        assert!(!appended);
        assert_eq!(messages[0]["content"], "original user");
    }
}
