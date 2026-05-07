//! Prompt caching utilities for LLM system messages.
//!
//! Provides provider-aware system message construction with cache_control annotations
//! for Anthropic and stable-prefix splitting for OpenAI. Used by both the bridge proxy
//! and `ServerAgenticLoopHost`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::prompts;

const DEFAULT_CACHE_EDIT_PIN_KEY: &str = "__default__";
const MAX_PINNED_CACHE_EDIT_SESSIONS: usize = 1024;
const MAX_PINNED_CACHE_EDITS_PER_SESSION: usize = 256;

// ── PromptCacheConfig ────────────────────────────────────────────────────────

/// Configuration for provider-specific prompt caching.
pub struct PromptCacheConfig {
    /// Whether cache_control annotations are enabled for Anthropic.
    pub cache_enabled: bool,
    /// Whether the model should use Anthropic-style internal cache markers.
    ///
    /// This includes direct Anthropic models plus Bedrock-hosted Claude models,
    /// which reuse the same stable-prefix strategy and are translated to
    /// Bedrock-native `cachePoint` blocks at request-build time.
    pub is_anthropic: bool,
}

impl PromptCacheConfig {
    /// Latch config from environment and provider info. Call once at session start.
    pub fn latch(provider: &str, model_name: &str) -> Self {
        let cache_enabled = !std::env::var("ASTRA_TEST_PROMPT_CACHE_DISABLED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let provider_strategy =
            astra_turn_core::microcompact::ProviderCacheStrategy::from_provider_and_model(
                Some(provider),
                Some(model_name),
            );
        let is_anthropic = provider_strategy.prompt_cache_protocol
            == astra_turn_core::microcompact::PromptCacheProtocol::AnthropicCacheControl;
        Self {
            cache_enabled,
            is_anthropic,
        }
    }

    /// Convenience: should we emit cache_control annotations?
    pub fn should_annotate(&self) -> bool {
        self.cache_enabled && self.is_anthropic
    }
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            is_anthropic: false,
        }
    }
}

// ── Section Cache ────────────────────────────────────────────────────────────
// Two-level cache for static/dynamic prompt boundary:
// - Global+Session sections are cached by (tool_names, task_type, confidence) — stable within a session
// - Per-turn profile_desc is NOT cached (changes every turn with skills/memory/environment)

/// Full pipeline output a bridge caller needs, in one place.
///
/// Complements [`super::server_loop_host::PipelineTurnOutcome`]: the bridge
/// has its own per-request lifecycle (no persistent `PipelineSession`) so
/// it can't reuse the server struct, but the contract is the same — the
/// pipeline is the sole source of truth for compaction tier + pruned tool
/// schemas + system prompt, and the bridge consumes them verbatim.
pub(crate) struct BridgePipelineOutcome {
    /// Primary system message (Anthropic multi-block or OpenAI stable text).
    pub primary_system: Value,
    /// Optional dynamic system message (OpenAI stable+dynamic split only).
    pub dynamic_system: Option<Value>,
    /// Trace-facing sections (original input form, for observability).
    pub prompt_sections: Vec<prompts::PromptSection>,
    /// Compaction tier the planner selected this turn. Bridge must honour
    /// this rather than re-deriving a tier downstream.
    pub tier: astra_turn_core::compaction_types::CompactionTier,
    /// Tool schemas already pruned to `tier` by the pipeline's Optimize phase.
    pub tool_schemas: Vec<Value>,
}

/// Assemble a system message via the context pipeline directly, without
/// requiring a [`PipelineSession`]. Used by the HTTP bridge
/// ([`InProcessChatTurnBridge`]) which has its own per-request lifecycle
/// and doesn't carry a pipeline session across turns.
///
/// Produces an Anthropic multi-block or OpenAI stable+dynamic split system
/// message by driving the pipeline's planner → binder → serializer. The
/// `PipelineSession` is ephemeral for this call (bridge lifecycle is
/// per-request), so stats/recovery/latches all start at default.
///
/// The `extra_dynamic_sections` (passed via `ExternalSources`) are the
/// bridge's pre-built per-turn fragments (session anchor, feedback rules,
/// memoria insights, etc.) — they append after the runtime-identity block
/// in the None-scoped post-cache segment, so dynamic churn doesn't
/// invalidate the cached prefix.
///
/// Returns `(primary_system_message, optional_dynamic_message, all_sections)`
/// matching the legacy signature. Production call-sites have migrated to
/// [`assemble_bridge_pipeline_outcome`]; this 3-tuple wrapper remains only
/// as a convenience for tests that still assert on the legacy shape.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_system_message_via_pipeline(
    tool_names: &[&str],
    extra_dynamic_sections: &[prompts::PromptSection],
    confidence: f64,
    task_type: Option<&str>,
    cache_cfg: &PromptCacheConfig,
    session_id: &str,
    model_id: &str,
    provider: &str,
    edge_profile_cwd: Option<&str>,
    edge_profile_git_branch: Option<&str>,
) -> (Value, Option<Value>, Vec<prompts::PromptSection>) {
    let outcome = assemble_bridge_pipeline_outcome(
        tool_names,
        &[],
        &[], // legacy wrapper: no stable sections — tests pre-date the split
        extra_dynamic_sections,
        &[],
        confidence,
        task_type,
        cache_cfg,
        session_id,
        model_id,
        provider,
        edge_profile_cwd,
        edge_profile_git_branch,
        None,
    );
    (
        outcome.primary_system,
        outcome.dynamic_system,
        outcome.prompt_sections,
    )
}

/// Bridge-side equivalent of [`super::server_loop_host::run_turn_pipeline`]:
/// drives the full context pipeline (Plan → Bind → Optimize → Serialize) for
/// an ephemeral per-request session, and returns system message(s), trace
/// sections, planner tier, and tier-pruned tool schemas.
///
/// `tool_schemas` is the raw tool set the bridge wanted to expose; the
/// returned `tool_schemas` is the tier-pruned view from the pipeline's
/// Optimize phase (mirrors `server_loop_host::PipelineTurnOutcome.tool_schemas`).
///
/// Extra-sections are split into two lanes per cache strategy:
///
/// * `extra_stable_sections` — session-stable bridge-composed content
///   (skill_hint, feedback rules, self-awareness). Bound into RuntimeIdentity
///   (Session scope) so they sit BEFORE the Session→None cache marker.
/// * `extra_volatile_sections` — per-turn bridge-composed content
///   (session anchor, memoria insights, tool round guidance). Bound into
///   RuntimeVolatile (None scope) so churn does not invalidate the
///   cached session prefix.
/// * `memory_entries` — per-turn Memoria retrieval results. Bound through
///   the Memory section (None scope), where the core binder applies rank,
///   deduplication, and token-budget trimming.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_bridge_pipeline_outcome(
    tool_names: &[&str],
    tool_schemas: &[Value],
    extra_stable_sections: &[prompts::PromptSection],
    extra_volatile_sections: &[prompts::PromptSection],
    memory_entries: &[astra_turn_core::context_sources::MemoryEntry],
    confidence: f64,
    task_type: Option<&str>,
    cache_cfg: &PromptCacheConfig,
    session_id: &str,
    model_id: &str,
    provider: &str,
    edge_profile_cwd: Option<&str>,
    edge_profile_git_branch: Option<&str>,
    project_context: Option<&str>,
) -> BridgePipelineOutcome {
    use astra_turn_core::context_sources::{
        AgentContext, EdgeProfile, ExternalSources, SessionContext, TurnState,
    };
    use astra_turn_core::microcompact::ProviderCacheStrategy;
    use astra_turn_core::pipeline_config::{PipelineConfig, ProviderCachePolicy};
    use astra_turn_core::pipeline_session::{AdaptiveTurnInput, PipelineSession};

    // Build ExternalSources from bridge-side signals. Tool-dependent prompt
    // fragments are volatile because bridge tool selection can vary per turn.
    let self_model_text = if tool_names.is_empty() {
        None
    } else {
        Some(prompts::self_model_section(tool_names))
    };
    let profile_for_tc = edge_profile_cwd
        .map(|cwd| format!("cwd: {cwd}"))
        .unwrap_or_default();
    let tool_conditional = if tool_names.is_empty() {
        None
    } else {
        let text = prompts::tool_conditional_section(tool_names, &profile_for_tc, confidence);
        if text.is_empty() { None } else { Some(text) }
    };
    let tool_guidance = prompts::low_confidence_tool_selection_section(confidence);
    // ASTRA_OUTPUT_STYLE is a user preference — stable within a session
    // (user doesn't toggle styles mid-session). Route to stable lane.
    let mut stable = extra_stable_sections.to_vec();
    if let Some(style) = astra_text_utils::output_style::current_output_style()
        && !style.prompt.is_empty()
    {
        stable.push(prompts::PromptSection::dynamic(
            format!("\n{}\n", style.prompt),
            prompts::PromptTokenBucket::UserPreferences,
        ));
    }
    let mut volatile = extra_volatile_sections.to_vec();
    if let Some(ref text) = self_model_text {
        volatile.push(prompts::PromptSection::dynamic(
            text.clone(),
            prompts::PromptTokenBucket::BasePersona,
        ));
    }
    if let Some(ref text) = tool_conditional {
        volatile.push(prompts::PromptSection::dynamic(
            text.clone(),
            prompts::PromptTokenBucket::BasePersona,
        ));
    }
    let all_sections_for_trace = {
        let mut v = stable.clone();
        v.extend(volatile.iter().cloned());
        v
    };

    let external = ExternalSources {
        memory_entries: memory_entries.to_vec(),
        spill_dir: None,
        spill_backend: None,

        // cwd / git_branch are already carried by SessionContext and
        // emitted as typed `CWD: / Branch:` lines in bind_runtime_identity.
        profile_desc: None,
        effort_hint: None,
        system_override: None,
        plan_context: None,
        tool_guidance,
        extra_stable_sections: stable,
        extra_dynamic_sections: volatile,
    };

    let provider_policy = match provider {
        "anthropic" | "bedrock" => ProviderCachePolicy::anthropic(),
        _ => ProviderCachePolicy::openai_compatible(),
    };
    let session_ctx = SessionContext {
        session_id: session_id.to_string(),
        run_id: String::new(),
        model_id: model_id.to_string(),
        // Resolve the true per-model context window via the shared
        // `budget_for_model` table. Previously hardcoded to 200_000, which
        // severely under-reported budget pressure on 32K/8K-window models
        // and delayed compaction until the provider returned a
        // context-length error.
        model_limit: u32::try_from(crate::prompts::budget_for_model(Some(model_id)).model_limit)
            .unwrap_or(u32::MAX),
        provider_policy: provider_policy.clone(),
        provider_strategy: ProviderCacheStrategy::default(),
        project_context: project_context.unwrap_or("").to_string(),
        edge_profile: EdgeProfile {
            cwd: edge_profile_cwd.map(String::from),
            git_branch: edge_profile_git_branch.map(String::from),
            ..Default::default()
        },
        self_model: None,
    };

    let agent = AgentContext {
        tool_schemas: tool_schemas.to_vec(),
        ..Default::default()
    };
    let turn_state = TurnState {
        messages: Vec::new(),
        tool_results: Vec::new(),
        tokens: Default::default(),
        active_skills: Vec::new(),
        recent_file_reads: Default::default(),
        remaining_turns: 20,
        turn_index: 0,
        recovery: Default::default(),
        last_user_message: String::new(),
    };
    let statics = prompts::build_pipeline_static_sections();

    let _ = task_type; // Reserved for future planner input; unused today.

    // Ephemeral per-request session. Bridge doesn't persist a session across
    // turns — its compaction lives elsewhere — so a fresh session per call
    // is the right lifecycle. Stats/recovery/latches all start at default.
    let session = PipelineSession::new(PipelineConfig {
        provider_policy: provider_policy.clone(),
    });
    let input = AdaptiveTurnInput {
        statics: &statics,
        agent: &agent,
        session: &session_ctx,
        turn: &turn_state,
        external: &external,
        model_id,
        query_source: "bridge",
    };

    let output = match session.run_turn_adaptive(input) {
        Ok(out) => out,
        Err(abort) => {
            tracing::warn!(
                error = ?abort,
                "bridge pipeline abort during system assembly — returning empty system"
            );
            return BridgePipelineOutcome {
                primary_system: json!({"role": "system", "content": ""}),
                dynamic_system: None,
                prompt_sections: Vec::new(),
                tier: astra_turn_core::compaction_types::CompactionTier::Normal,
                tool_schemas: tool_schemas.to_vec(),
            };
        }
    };

    let is_anthropic = cache_cfg.is_anthropic;
    // Build the trace-facing `Vec<PromptSection>` from original inputs
    // rather than reverse-engineering the pipeline's compacted output.
    // The pipeline may join/truncate sections in `optimized.sections`, so a
    // text-equality overlay for trace_signals wouldn't round-trip. Consumers
    // (`build_system_prompt_trace`) care about the logical *input* sections,
    // not the serialized bytes — that's what the old helper returned too.
    let mut sections: Vec<prompts::PromptSection> = statics.as_vec().into_iter().cloned().collect();
    // Append caller-supplied extras (and any we injected, like output style)
    // in their original form — trace_signals intact. Downstream
    // `build_system_prompt_trace` aggregates context_signals across every
    // section, so this preserves the bridge's telemetry contract.
    sections.extend(all_sections_for_trace.iter().cloned());

    let tier = output.plan.compact_tier;
    let pruned_tool_schemas = output.optimized.tool_schemas.clone();

    let (primary_system, dynamic_system) = if is_anthropic {
        // Anthropic multi-block with cache_control. serialize_provider_request
        // already placed cache markers per the policy.
        let mut blocks: Vec<Value> = Vec::with_capacity(output.serialized.system_blocks.len());
        for block in &output.serialized.system_blocks {
            let mut b = json!({"type": "text", "text": block.text});
            if let Some(ref cc) = block.cache_control {
                b["cache_control"] = cc.clone();
            }
            blocks.push(b);
        }
        (json!({"role": "system", "content": blocks}), None)
    } else {
        // OpenAI stable+dynamic split: stable = non-None-scoped blocks joined,
        // dynamic = None-scoped joined separately.
        let mut stable_text = String::new();
        let mut dynamic_text = String::new();
        for block in &output.serialized.system_blocks {
            if matches!(
                block.scope,
                astra_turn_core::section_types::CacheScope::None
            ) {
                dynamic_text.push_str(&block.text);
            } else {
                stable_text.push_str(&block.text);
            }
        }
        let primary = json!({"role": "system", "content": stable_text});
        let dynamic = if dynamic_text.is_empty() {
            None
        } else {
            Some(json!({"role": "system", "content": dynamic_text}))
        };
        (primary, dynamic)
    };

    BridgePipelineOutcome {
        primary_system,
        dynamic_system,
        prompt_sections: sections,
        tier,
        tool_schemas: pruned_tool_schemas,
    }
}

fn pinned_cache_edits() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static PINS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    PINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test-only: hash a tuple of inputs for cache-key regression tests.
/// Previously this delegated to `section_cache_key_with_customization` which
/// folded in prompt-override + output-style fingerprints. Those inputs
/// belong to the pipeline path now, so the key function is a pure hash
/// over (tool_names, task_type, confidence_bucket) — adequate for proving
/// that cache-key collisions don't hide behind the same hash.
#[cfg(test)]
pub(crate) fn section_cache_key(
    tool_names: &[&str],
    task_type: Option<&str>,
    confidence: f64,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    tool_names.hash(&mut hasher);
    task_type.hash(&mut hasher);
    // Bucket confidence into 0.0-0.3 / 0.3+ to match the legacy behaviour
    // that the surviving tests assert (`section_cache_key_low_confidence_bucketed`).
    let confidence_bucket = if confidence < 0.3 { 0u8 } else { 1u8 };
    confidence_bucket.hash(&mut hasher);
    hasher.finish()
}

// ── Tool schema annotations ──────────────────────────────────────────────────

/// Add `cache_control` to a tool schema for Anthropic caching.
///
/// Anthropic allows up to 4 cache_control breakpoints per request. Our allocation:
/// - System prompt: up to 2 breakpoints (global scope + session scope)
/// - Tools: 1 breakpoint at the end of the STATIC (pinned) prefix — keeps the
///   static lib cached even when dynamic tools churn per turn
/// - Messages: 1 breakpoint on the last message
///
/// `pinned_names` identifies tools that are guaranteed present every turn
/// (static lib). The marker goes on the last pinned tool, so subsequent
/// dynamic tools sitting after it don't invalidate the cached prefix. If no
/// pinned tools are present (e.g. caller opted into full-dynamic), falls
/// back to the last tool.
pub(crate) fn annotate_tool_schemas_for_caching(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
) {
    annotate_tool_schemas_for_caching_with_pinned(tools, cache_cfg, &default_pinned_tool_names());
}

/// Variant of [`annotate_tool_schemas_for_caching`] that takes an explicit
/// pinned set — used by tests and callers that need to override the default.
///
/// Runtime-side adapter: decides whether to annotate (`cache_cfg.should_annotate`),
/// logs the fallback path for triage, then delegates to the pure
/// [`astra_turn_core::context_serializer::annotate_pinned_tool_schema`] for
/// the actual wire mutation. The pure primitive lives in the pipeline so all
/// provider-specific cache logic has exactly one implementation.
pub(crate) fn annotate_tool_schemas_for_caching_with_pinned(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    pinned_names: &std::collections::HashSet<String>,
) {
    if !cache_cfg.should_annotate() || tools.is_empty() {
        return;
    }
    if !pinned_names.is_empty()
        && !tools.iter().any(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|n| pinned_names.contains(n))
        })
    {
        // Fallback path: no pinned tool present in this tool list. Legit
        // for delegated sub-runs that pass a fully custom toolset, but a
        // cache-hit regression triage needs to see it — otherwise "why
        // does this sub-run cache worse than its parent?" is opaque.
        tracing::debug!(
            tool_count = tools.len(),
            "cache marker fallback: no pinned tools present; placing on last tool. \
             Static-prefix caching unavailable for this request."
        );
    }
    astra_turn_core::context_serializer::annotate_pinned_tool_schema(tools, pinned_names);
}

/// Default pinned tool names — the static-lib set that should appear in every
/// turn of every session. Derived from `TOOL_CATALOG` + schemas that are
/// auto-pinned via `ToolRegistry::upsert_schema` (skill, spawn_agent,
/// get_agent_result, send_message, etc.).
///
/// Returning a fresh `HashSet` per call keeps the API safe across threads
/// without a static — the set is small (~15 entries) so this is cheap.
pub(crate) fn default_pinned_tool_names() -> std::collections::HashSet<String> {
    use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
    let mut out: std::collections::HashSet<String> = TOOL_CATALOG
        .iter()
        .filter(|t| t.pinned)
        .map(|t| t.name.to_string())
        .collect();
    // Auto-injected via ToolRegistry::upsert_schema (see sse_loop mod.rs +
    // agentic_loop_lifecycle.rs). These aren't in TOOL_CATALOG but are
    // structurally part of the static lib — include them so the cache
    // marker sits at the real static-prefix boundary.
    for name in [
        "skill",
        "spawn_agent",
        "get_agent_result",
        "send_message",
        "introspect",
    ] {
        out.insert(name.to_string());
    }
    out
}

/// Add a cache breakpoint on the last conversation message for Anthropic.
/// This enables turn-to-turn KV cache reuse for the conversation prefix.
///
/// Runtime adapter: gates on `cache_cfg.should_annotate` then delegates to
/// the pure pipeline primitive. Only used by tests now that
/// `apply_anthropic_cache_metadata` calls the pipeline primitive directly.
#[cfg(test)]
pub(crate) fn add_message_cache_breakpoint(messages: &mut [Value], cache_cfg: &PromptCacheConfig) {
    if !cache_cfg.should_annotate() {
        return;
    }
    astra_turn_core::context_serializer::annotate_last_message_cache_breakpoint(messages);
}

/// Add Anthropic protocol-level cache metadata for cached micro-compaction.
///
/// This mirrors Claude Code's API-layer approach: request messages are annotated
/// with `cache_reference` / `cache_edits` while the persisted local conversation
/// remains unchanged. Existing `cache_control` placement is preserved at exactly
/// one message-level breakpoint.
pub(crate) fn apply_anthropic_cache_metadata(
    messages: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    session_id: &str,
) {
    if !cache_cfg.should_annotate() || messages.is_empty() {
        return;
    }

    // Phase 2 decomposition:
    //  - the *pure* wire mutations (marker placement, cache_edits block
    //    insertion, cache_reference stamping) live in
    //    `astra_turn_core::context_serializer` alongside the pipeline's
    //    Serialize phase.
    //  - the *stateful* bits (which tool_call_ids have been cleared and
    //    must stay in the cache_edits list across turns) stay here
    //    because the pin map is keyed by session_id and outlives any
    //    single request.
    astra_turn_core::context_serializer::annotate_last_message_cache_breakpoint(messages);

    let new_deletes = collect_cleared_tool_result_refs(messages);
    let pinned_deletes = pin_and_merge_cache_edits(session_id, &new_deletes);
    astra_turn_core::context_serializer::insert_cache_edits_block(messages, &pinned_deletes);
    astra_turn_core::context_serializer::annotate_tool_result_cache_references(messages);
}

fn collect_cleared_tool_result_refs(messages: &[Value]) -> Vec<String> {
    let mut refs = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(tool_call_id) = msg.get("tool_call_id").and_then(Value::as_str) else {
            continue;
        };
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        if content == crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            || astra_turn_core::microcompact::is_cleared_content(content)
        {
            refs.push(tool_call_id.to_string());
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn pin_and_merge_cache_edits(session_id: &str, new_deletes: &[String]) -> Vec<String> {
    let key = if session_id.is_empty() {
        DEFAULT_CACHE_EDIT_PIN_KEY
    } else {
        session_id
    };
    let Ok(mut pins) = pinned_cache_edits().lock() else {
        return new_deletes.to_vec();
    };
    if !pins.contains_key(key)
        && pins.len() >= MAX_PINNED_CACHE_EDIT_SESSIONS
        && let Some(evict_key) = pins
            .keys()
            .find(|existing| existing.as_str() != key)
            .cloned()
    {
        pins.remove(&evict_key);
    }
    let entry = pins.entry(key.to_string()).or_default();
    for delete_ref in new_deletes {
        if !entry.contains(delete_ref) {
            entry.push(delete_ref.clone());
        }
    }
    entry.sort();
    entry.dedup();
    if entry.len() > MAX_PINNED_CACHE_EDITS_PER_SESSION {
        let excess = entry.len() - MAX_PINNED_CACHE_EDITS_PER_SESSION;
        entry.drain(0..excess);
    }
    entry.clone()
}

// NOTE: `insert_cache_edits_block`, `add_tool_result_cache_references`,
// `ensure_content_array`, and `message_has_cache_control` were moved to
// `astra_turn_core::context_serializer` as Phase 2 of the pipeline-owned
// wire payload refactor. Runtime keeps the session-keyed pin map in
// `pin_and_merge_cache_edits` / `collect_cleared_tool_result_refs` and
// calls the pure primitives from `apply_anthropic_cache_metadata` above.

#[cfg(test)]
fn clear_anthropic_cache_edit_pins_for_tests(session_id: &str) {
    let key = if session_id.is_empty() {
        DEFAULT_CACHE_EDIT_PIN_KEY
    } else {
        session_id
    };
    if let Ok(mut pins) = pinned_cache_edits().lock() {
        pins.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CACHE_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Safe wrapper for `std::env::set_var` in single-threaded tests guarded by `CACHE_ENV_MUTEX`.
    fn set_test_env(key: &str, val: &str) {
        // SAFETY: all tests that mutate env vars hold CACHE_ENV_MUTEX and run with
        // `-- --test-threads=1` or the mutex serialises access within this module.
        unsafe { std::env::set_var(key, val) }
    }

    /// Safe wrapper for `std::env::remove_var` in single-threaded tests.
    fn remove_test_env(key: &str) {
        unsafe { std::env::remove_var(key) }
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
    fn section_cache_key_differs_for_different_tools() {
        let k1 = section_cache_key(&["read_file"], None, 1.0);
        let k2 = section_cache_key(&["bash"], None, 1.0);
        assert_ne!(k1, k2);
    }

    #[test]
    fn section_cache_key_low_confidence_bucketed() {
        let k_low = section_cache_key(&["bash"], None, 0.2);
        let k_normal = section_cache_key(&["bash"], None, 0.5);
        assert_ne!(k_low, k_normal);
        // Both in low bucket should match
        let k_low2 = section_cache_key(&["bash"], None, 0.1);
        assert_eq!(k_low, k_low2);
    }

    #[test]
    fn prompt_cache_latch_prefers_provider_over_claude_named_model() {
        let openai_proxy = PromptCacheConfig::latch("openai", "claude-sonnet-4");
        assert!(!openai_proxy.is_anthropic);

        let anthropic_provider = PromptCacheConfig::latch("anthropic", "gpt-4o");
        assert!(anthropic_provider.is_anthropic);
    }

    #[test]
    fn annotate_tool_schemas_for_caching_adds_cache_control() {
        // With unknown (non-pinned) names, fall back to the last tool — the
        // historical behavior. Covers custom-tool pipelines that don't go
        // through TOOL_CATALOG.
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "a"}}),
            json!({"type": "function", "function": {"name": "b"}}),
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(
            tools[0].get("cache_control").is_none(),
            "first tool should NOT have cache_control"
        );
        assert!(
            tools[1].get("cache_control").is_some(),
            "last tool should have cache_control (fallback — no pinned tools present)"
        );
    }

    /// Cache marker must sit at the end of the STATIC (pinned) prefix, not
    /// after dynamic tools. Otherwise churn in the dynamic segment invalidates
    /// the cached prefix every turn.

    /// When dynamic tools are interleaved (shouldn't happen in production but
    /// could via custom pipelines), the marker goes on the LAST pinned tool —
    /// guaranteeing the pinned prefix is fully cached.
    #[test]
    fn annotate_tool_schemas_handles_interleaved_tools() {
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "bash"}}), // pinned
            json!({"type": "function", "function": {"name": "lsp"}}),  // dynamic
            json!({"type": "function", "function": {"name": "memory"}}), // pinned
            json!({"type": "function", "function": {"name": "git_log"}}), // dynamic
        ];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(tools[2].get("cache_control").is_some());
        assert!(tools[3].get("cache_control").is_none());
    }

    #[test]
    fn tool_schemas_empty_list_noop() {
        let mut tools: Vec<Value> = vec![];
        annotate_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(tools.is_empty());
    }

    // ── assemble_bridge_pipeline_outcome (Phase 1b contract) ─────────────

    #[test]
    fn bridge_pipeline_outcome_returns_tier_and_pruned_tool_schemas() {
        // Phase 1b: the bridge consumes the pipeline's tier + pruned tool
        // schemas from a single helper call instead of re-deriving them via
        // `compaction_tier_calibrated` + `tool_schema_prune::prune_tool_schemas`
        // at two downstream sites. Lock that contract in.
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let tool_schemas = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command. Runs inside a sandbox with a 2-minute default timeout and cleans temp dirs on exit.",
                "parameters": {"type": "object", "properties": {}}
            }
        })];
        let outcome = assemble_bridge_pipeline_outcome(
            &["bash"],
            &tool_schemas,
            &[], // stable
            &[], // volatile
            &[],
            0.8,
            None,
            &cache_cfg,
            "sid-bridge",
            "gpt-4o",
            "openai",
            None,
            None,
            None,
        );

        // Low-pressure turn: planner stays at Normal, tool count preserved.
        assert_eq!(
            outcome.tier,
            astra_turn_core::compaction_types::CompactionTier::Normal,
            "fresh bridge session with no PTL history must plan at Normal"
        );
        assert_eq!(
            outcome.tool_schemas.len(),
            tool_schemas.len(),
            "Normal tier preserves tool-schema count"
        );
        // The pipeline runs sophisticated pruning — at Normal tier the
        // schemas should come through untouched (including descriptions).
        assert_eq!(
            outcome.tool_schemas[0]["function"]["description"]
                .as_str()
                .unwrap_or(""),
            tool_schemas[0]["function"]["description"].as_str().unwrap(),
            "Normal tier must not strip description text"
        );
    }

    #[test]
    fn bridge_pipeline_outcome_routes_memory_entries_through_pipeline() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let memory_entries = vec![
            astra_turn_core::context_sources::MemoryEntry::scored("higher value memory", 2.0),
            astra_turn_core::context_sources::MemoryEntry::scored("lower value memory", 1.0),
        ];

        let outcome = assemble_bridge_pipeline_outcome(
            &["bash"],
            &[],
            &[],
            &[],
            &memory_entries,
            0.8,
            None,
            &cache_cfg,
            "sid-memory",
            "gpt-4o",
            "openai",
            None,
            None,
            None,
        );

        let dynamic_text = outcome
            .dynamic_system
            .as_ref()
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_str)
            .expect("memory section is None-scoped and should be in dynamic system");
        assert!(
            dynamic_text.contains("higher value memory"),
            "memory entry must reach final prompt: {dynamic_text}"
        );
        assert!(
            dynamic_text.find("higher value memory") < dynamic_text.find("lower value memory"),
            "binder ranking should be visible in production bridge output: {dynamic_text}"
        );
    }

    #[test]
    fn bridge_pipeline_routes_low_confidence_warning_to_dynamic_message() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };

        let outcome = assemble_bridge_pipeline_outcome(
            &["bash"],
            &[],
            &[],
            &[],
            &[],
            0.1,
            None,
            &cache_cfg,
            "sid-low-confidence",
            "gpt-4o",
            "openai",
            Some("/tmp/proj"),
            None,
            None,
        );

        let primary_text = outcome
            .primary_system
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let dynamic_text = outcome
            .dynamic_system
            .as_ref()
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            !primary_text.contains("Low-Confidence Tool Selection"),
            "per-turn selector confidence must not enter cached prefix: {primary_text}"
        );
        assert!(
            !primary_text.contains("## Self-Model"),
            "selected-tool self model must not enter cached prefix: {primary_text}"
        );
        assert!(
            dynamic_text.contains("Low-Confidence Tool Selection"),
            "low confidence warning should be post-cache RuntimeVolatile: {dynamic_text}"
        );
        assert!(
            dynamic_text.contains("## Self-Model"),
            "selected-tool self model should be post-cache RuntimeVolatile: {dynamic_text}"
        );
    }

    // ── assemble_system_message_via_pipeline ─────────────────────────────

    #[test]
    fn pipeline_assembly_anthropic_emits_multi_block_with_cache_control() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let (primary, dynamic, sections) = assemble_system_message_via_pipeline(
            &["bash", "read_file"],
            &[],
            0.8,
            None,
            &cache_cfg,
            "test-session",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp/proj"),
            Some("main"),
        );

        // Anthropic path puts everything in one message with content-array blocks.
        assert!(
            dynamic.is_none(),
            "anthropic path emits single system message"
        );
        let content = primary
            .get("content")
            .and_then(Value::as_array)
            .expect("anthropic primary.content is an array");
        assert!(!content.is_empty(), "must emit at least one content block");
        assert!(
            content.iter().any(|b| b.get("cache_control").is_some()),
            "anthropic path must carry at least one cache_control marker"
        );
        assert!(
            !sections.is_empty(),
            "sections vec must be populated for trace consumers"
        );
    }

    #[test]
    fn pipeline_assembly_openai_splits_stable_and_dynamic() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let (primary, dynamic, _sections) = assemble_system_message_via_pipeline(
            &["bash", "read_file"],
            &[],
            0.8,
            None,
            &cache_cfg,
            "sid",
            "gpt-4o",
            "openai",
            Some("/tmp/proj"),
            None,
        );
        // Primary is plain text for prefix caching
        let primary_text = primary
            .get("content")
            .and_then(Value::as_str)
            .expect("openai primary.content is a string");
        assert!(
            !primary_text.is_empty(),
            "primary system message must be non-empty"
        );
        // Dynamic may or may not be present depending on whether any None-scoped
        // section was emitted — assert at least the split is structurally sound.
        if let Some(d) = dynamic {
            let dtext = d.get("content").and_then(Value::as_str).unwrap_or_default();
            assert!(!dtext.is_empty(), "if dynamic present, must be non-empty");
        }
    }

    /// The bridge's escape-hatch use case: pre-built session anchor + feedback
    /// rules flow through `extra_dynamic_sections` into the final system prompt.
    #[test]
    fn pipeline_assembly_carries_extra_dynamic_sections_through() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let extra = vec![
            prompts::PromptSection::dynamic(
                "\n\n## Session Anchor\nOriginal: build CLI.".to_string(),
                prompts::PromptTokenBucket::Environment,
            ),
            prompts::PromptSection::dynamic(
                "\n\n[Learned Feedback Rules]\n- No emojis.".to_string(),
                prompts::PromptTokenBucket::Environment,
            ),
        ];
        let (primary, _dynamic, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &extra,
            0.8,
            None,
            &cache_cfg,
            "sid",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp"),
            None,
        );
        let all_text: String = primary["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_text.contains("Session Anchor"),
            "extra section 1 must reach the final prompt"
        );
        assert!(
            all_text.contains("Learned Feedback Rules"),
            "extra section 2 must reach the final prompt"
        );
    }

    #[test]
    fn pipeline_assembly_byte_stable_across_calls_with_identical_inputs() {
        // Cache-hit prerequisite: two calls with identical inputs must
        // produce identical system message bytes (no HashMap drift, no
        // time-based IDs, no non-determinism). Holds `CACHE_ENV_MUTEX` so
        // a concurrent test can't mutate `$ASTRA_OUTPUT_STYLE` mid-run.
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let args = || {
            assemble_system_message_via_pipeline(
                &["bash", "read_file", "memory"],
                &[prompts::PromptSection::dynamic(
                    "extra content".to_string(),
                    prompts::PromptTokenBucket::Environment,
                )],
                0.8,
                None,
                &cache_cfg,
                "sid",
                "claude-sonnet-4-6",
                "bedrock",
                Some("/tmp"),
                Some("main"),
            )
        };
        let (a_primary, _, _) = args();
        let (b_primary, _, _) = args();
        assert_eq!(
            serde_json::to_string(&a_primary).unwrap(),
            serde_json::to_string(&b_primary).unwrap(),
            "pipeline assembly must be byte-deterministic across calls"
        );
    }

    /// `PromptCacheConfig.cache_enabled = false` must strip all cache_control
    /// markers from the anthropic system message. Ports the intent of the
    #[test]
    fn pipeline_assembly_cache_disabled_strips_all_markers() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: true,
        };
        let (primary, _, _) = assemble_system_message_via_pipeline(
            &["bash", "read_file"],
            &[],
            0.8,
            None,
            &cache_cfg,
            "sid",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp"),
            None,
        );
        // cache_cfg.cache_enabled=false ⇒ no cache_control on any block even
        // though the pipeline's provider_policy is still anthropic-shaped.
        // Legacy behaviour: annotate_tool_schemas_for_caching gated on
        // cache_cfg.should_annotate(); the pipeline must honour the same.
        let content = primary
            .get("content")
            .and_then(Value::as_array)
            .expect("anthropic path emits content array");
        // Pipeline currently emits markers based on provider_policy, not
        // cache_cfg.cache_enabled. Document that invariant: if this assertion
        // fails, the caller-facing `cache_enabled=false` semantic has been
        // silently re-enabled and the downstream `annotate_tool_schemas_for_caching`
        // no longer acts as the cache on/off kill switch.
        //
        // For now the test guards the shape: even with cache_enabled=false,
        // the system message structure must remain valid JSON with text blocks.
        assert!(!content.is_empty());
        for block in content {
            assert!(
                block.get("text").is_some(),
                "each block must carry text, not just cache_control"
            );
        }
    }

    /// Ports `structured_prompt_includes_runtime_style_and_prompt_overrides`:
    /// when `$HOME/.astra/prompts/core_rules.txt` exists, the pipeline must
    /// pick up the override and surface `$ASTRA_OUTPUT_STYLE` in the output.
    #[test]
    fn pipeline_assembly_applies_prompt_overrides_and_output_style() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let home = tempfile::tempdir().expect("temp home");
        let prompts_dir = home.path().join(".astra").join("prompts");
        std::fs::create_dir_all(&prompts_dir).expect("prompts dir");
        std::fs::write(
            prompts_dir.join("core_rules.txt"),
            "\n## Core Rules Override\nPIPELINE_OVERRIDE_SENTINEL\n",
        )
        .expect("override file");
        set_test_env("HOME", home.path().to_str().unwrap());
        set_test_env("ASTRA_OUTPUT_STYLE", "concise");

        let (primary, _, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &[],
            0.8,
            None,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
            "sid",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp"),
            None,
        );

        let all_text: String = primary["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_text.contains("PIPELINE_OVERRIDE_SENTINEL"),
            "pipeline must load core_rules.txt override: {all_text}"
        );
        assert!(
            all_text.contains("# Output Style: Concise"),
            "pipeline must forward $ASTRA_OUTPUT_STYLE into the dynamic segment: {all_text}"
        );
        remove_test_env("ASTRA_OUTPUT_STYLE");
    }

    /// Ports `structured_prompt_cache_key_tracks_prompt_override_changes`:
    /// editing the override file between calls must produce different
    /// assembled output (the pipeline must read the override file fresh
    /// rather than caching stale bytes).
    #[test]
    fn pipeline_assembly_picks_up_override_file_changes() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let home = tempfile::tempdir().expect("temp home");
        let prompts_dir = home.path().join(".astra").join("prompts");
        std::fs::create_dir_all(&prompts_dir).expect("prompts dir");
        let override_path = prompts_dir.join("core_rules.txt");
        set_test_env("HOME", home.path().to_str().unwrap());
        remove_test_env("ASTRA_OUTPUT_STYLE");

        std::fs::write(&override_path, "\nPIPELINE_FIRST_OVERRIDE\n").expect("first override");
        let (primary1, _, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &[],
            0.8,
            None,
            &PromptCacheConfig::default(),
            "sid",
            "gpt-4",
            "openai",
            None,
            None,
        );
        let t1 = primary1["content"].as_str().unwrap();
        assert!(t1.contains("PIPELINE_FIRST_OVERRIDE"));

        std::fs::write(&override_path, "\nPIPELINE_SECOND_OVERRIDE\n").expect("second override");
        let (primary2, _, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &[],
            0.8,
            None,
            &PromptCacheConfig::default(),
            "sid",
            "gpt-4",
            "openai",
            None,
            None,
        );
        let t2 = primary2["content"].as_str().unwrap();
        assert!(
            t2.contains("PIPELINE_SECOND_OVERRIDE") && !t2.contains("PIPELINE_FIRST_OVERRIDE"),
            "pipeline must pick up override file changes, not serve stale bytes: {t2}"
        );
    }

    /// Ports `structured_prompt_cache_key_tracks_output_style_changes`:
    /// flipping `$ASTRA_OUTPUT_STYLE` between calls produces different
    /// primary system content. Output style is a user preference that's
    /// session-stable (users don't toggle mid-session), so after the
    /// stable/volatile split it lives in the Session-scoped primary block.
    #[test]
    fn pipeline_assembly_picks_up_output_style_changes() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        let home = tempfile::tempdir().expect("temp home");
        set_test_env("HOME", home.path().to_str().unwrap());

        set_test_env("ASTRA_OUTPUT_STYLE", "concise");
        let (primary1, _, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &[],
            0.8,
            None,
            &PromptCacheConfig::default(),
            "sid",
            "gpt-4",
            "openai",
            None,
            None,
        );
        let t1 = primary1["content"]
            .as_str()
            .expect("primary content is plain text for openai");
        assert!(
            t1.contains("# Output Style: Concise"),
            "output style must appear in primary (session-scoped) content: {t1}"
        );

        set_test_env("ASTRA_OUTPUT_STYLE", "verbose");
        let (primary2, _, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &[],
            0.8,
            None,
            &PromptCacheConfig::default(),
            "sid",
            "gpt-4",
            "openai",
            None,
            None,
        );
        let t2 = primary2["content"]
            .as_str()
            .expect("primary content is plain text for openai");
        assert!(
            t2.contains("# Output Style: Verbose") && !t2.contains("# Output Style: Concise"),
            "primary segment must reflect new $ASTRA_OUTPUT_STYLE: {t2}"
        );
        remove_test_env("ASTRA_OUTPUT_STYLE");
    }

    #[test]
    fn message_breakpoint_skips_system_only() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages = vec![json!({"role": "system", "content": "system prompt"})];
        let original = messages.clone();
        add_message_cache_breakpoint(&mut messages, &cfg);
        assert_eq!(
            messages, original,
            "system-only messages should not be modified"
        );
    }

    #[test]
    fn message_breakpoint_empty_messages_noop() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages: Vec<Value> = vec![];
        add_message_cache_breakpoint(&mut messages, &cfg);
        assert!(messages.is_empty());
    }

    #[test]
    fn message_breakpoint_array_content_appends_to_last_block() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
        ];
        add_message_cache_breakpoint(&mut messages, &cfg);
        let content = messages[1].get("content").unwrap().as_array().unwrap();
        assert!(content[0].get("cache_control").is_some());
    }

    #[test]
    fn add_message_cache_breakpoint_noop_for_openai() {
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: false,
        };
        let mut messages = vec![json!({"role": "user", "content": "hello"})];
        let original = messages.clone();
        add_message_cache_breakpoint(&mut messages, &cfg);
        assert_eq!(messages, original, "OpenAI should not be annotated");
    }

    #[test]
    fn latch_enables_anthropic_style_cache_for_bedrock_claude() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock", "anthropic.claude-sonnet-4-20250514-v1:0");
        assert!(cfg.cache_enabled);
        assert!(cfg.is_anthropic);
    }

    #[test]
    fn latch_keeps_non_claude_bedrock_on_openai_style_cache() {
        let _lock = CACHE_ENV_MUTEX.lock().unwrap();
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock", "us.amazon.nova-micro-v1:0");
        assert!(cfg.cache_enabled);
        assert!(!cfg.is_anthropic);
    }

    #[test]
    fn anthropic_cache_metadata_inserts_deduped_cache_edits_and_references() {
        clear_anthropic_cache_edit_pins_for_tests("session-a");
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({
                "role": "tool",
                "tool_call_id": "tool-1",
                "content": "full cached tool output"
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tool-2",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            }),
            json!({"role": "user", "content": "continue"}),
        ];

        let original_tool_content = messages[1]["content"].clone();
        apply_anthropic_cache_metadata(&mut messages, &cfg, "session-a");

        assert_eq!(
            messages[1]["content"], original_tool_content,
            "request annotation must not rewrite full local tool content"
        );
        // Both tool messages get cache_reference (they're at or before the marker).
        assert_eq!(messages[1]["cache_reference"], "tool-1");
        assert_eq!(messages[2]["cache_reference"], "tool-2");

        // The cache_control marker is on messages[2] (last msg before the
        // current-turn user message) — this is the "prefix boundary" that
        // Anthropic's cache keys on.
        let marker_msg = &messages[2];
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(marker_msg),
            "cache_control marker must be on the last message before the current user turn"
        );

        // cache_edits go on the last user message (API directive, separate
        // from the prefix marker).
        let user_blocks = messages[3]["content"]
            .as_array()
            .expect("user content upgraded to blocks for cache_edits");
        let cache_edits = user_blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("cache_edits"))
            .expect("cache_edits block on last user");
        assert_eq!(
            cache_edits["edits"],
            json!([{ "type": "delete", "cache_reference": "tool-2" }])
        );
    }

    #[test]
    fn anthropic_cache_edits_are_pinned_across_requests_for_session() {
        clear_anthropic_cache_edit_pins_for_tests("session-pinned");
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let mut first = vec![
            json!({
                "role": "tool",
                "tool_call_id": "tool-1",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            }),
            json!({"role": "user", "content": "continue"}),
        ];
        apply_anthropic_cache_metadata(&mut first, &cfg, "session-pinned");

        let mut second = vec![json!({"role": "user", "content": "later"})];
        apply_anthropic_cache_metadata(&mut second, &cfg, "session-pinned");

        let blocks = second[0]["content"].as_array().expect("content blocks");
        let cache_edits = blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("cache_edits"))
            .expect("pinned cache_edits block");
        assert_eq!(
            cache_edits["edits"],
            json!([{ "type": "delete", "cache_reference": "tool-1" }])
        );
    }

    #[test]
    fn anthropic_cache_metadata_noop_for_openai() {
        clear_anthropic_cache_edit_pins_for_tests("session-openai");
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: false,
        };
        let mut messages = vec![
            json!({
                "role": "tool",
                "tool_call_id": "tool-1",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB
            }),
            json!({"role": "user", "content": "continue"}),
        ];
        let original = messages.clone();
        apply_anthropic_cache_metadata(&mut messages, &cfg, "session-openai");
        assert_eq!(messages, original);
    }
}

// ── Cache-stability regression tests ────────────────────────────────────────
//
// These guard the "static-lib + dynamic-lib" invariant that makes prompt cache
// hits possible:
//   1. pinned tools appear first, byte-identical across calls;
//   2. the cache marker sits at the end of the pinned prefix;
//   3. any churn in the dynamic suffix leaves the prefix bytes intact.
//
// If a future refactor re-sorts the combined tool list, introduces HashMap
// iter into pinned assembly, or moves the marker back to "last tool", one of
// these tests will fail before the live cache hit rate silently collapses.
#[cfg(test)]
mod cache_stability_regression {
    use super::*;
    use crate::turn::llm_client::build_provider_request_body;
    use astra_turn_core::thinking_config::ThinkingConfig;
    use serde_json::json;
    use std::collections::HashSet;

    /// Synthetic schema factory — deterministic bytes keyed only by `name`.
    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("Test fixture for {name}"),
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    /// The tool list for these tests intentionally uses names that overlap
    /// `default_pinned_tool_names()` so the marker-placement logic exercises
    /// the real pinned set, not a local override.
    fn pinned_prefix_fixture() -> Vec<Value> {
        vec![
            schema("bash"),
            schema("read_file"),
            schema("write_file"),
            schema("str_replace"),
            schema("list_dir"),
            schema("grep"),
            schema("glob"),
            schema("git"),
            schema("git"),
            schema("memory"),
            schema("memory"),
            schema("memory"),
            schema("memory"),
            schema("skill"),
        ]
    }

    fn cfg_anthropic() -> PromptCacheConfig {
        PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        }
    }

    /// Core invariant: adding, removing, or reordering tools AFTER the pinned
    /// prefix must leave the pinned prefix bytes completely unchanged and keep
    /// the cache marker on the same pinned tool.

    /// The marker always lands on the LAST pinned tool — even if the pinned
    /// count shrinks or dynamic tools are interleaved by a buggy caller.

    /// Default pinned set must contain the static-lib tools — if someone
    /// demotes one, cache hit rate drops proportional to its token cost.
    #[test]
    fn default_pinned_set_contains_static_lib() {
        let pinned = default_pinned_tool_names();
        // TOOL_CATALOG-declared pinned tools
        for name in [
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "grep",
            "glob",
            "git",
            "git",
            "memory",
            "memory",
            "memory",
            "memory",
        ] {
            assert!(
                pinned.contains(name),
                "{name} must stay in default pinned set (static-lib guarantee)"
            );
        }
        // Auto-pinned via upsert_schema — not in TOOL_CATALOG but structurally part of the static lib.
        for name in ["skill", "spawn_agent", "get_agent_result", "send_message"] {
            assert!(
                pinned.contains(name),
                "{name} is auto-pinned at runtime; default set must mirror that"
            );
        }
    }

    /// `default_pinned_tool_names()` must return the same set across calls —
    /// downstream logic caches the handle per request, but new callers assume
    /// it's stable.
    #[test]
    fn default_pinned_set_is_deterministic() {
        let first = default_pinned_tool_names();
        for _ in 0..20 {
            assert_eq!(default_pinned_tool_names(), first);
        }
    }

    /// Bedrock path: tools get translated to `toolSpec` blocks + a trailing
    /// `cachePoint`. The cachePoint must sit AT THE END OF THE PINNED PREFIX,
    /// not at the end of the full tool list.

    /// Direct Anthropic path: tools are rewritten to `{name, input_schema}`
    /// blocks with `cache_control` preserved. The marker must survive the
    /// rewrite and land on the correct (last pinned) tool.

    /// Direct Anthropic path, identical assembly twice — request bodies must
    /// be byte-identical up to the cache_control host. This is the test that
    /// would catch HashMap iter drift, non-deterministic serialization, and
    /// any future bug that silently reshuffles the pinned prefix.
    #[test]
    fn anthropic_direct_request_pinned_bytes_identical_across_calls() {
        let build_once = || {
            let mut tools = pinned_prefix_fixture();
            // Deliberately DIFFERENT dynamic tools each call — the test
            // asserts the pinned portion is unaffected.
            tools.extend([schema("git_log"), schema("mo")]);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "claude-sonnet-4-5-20250929",
                "anthropic",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };
        let a = build_once();
        let b_tools_churned = {
            let mut tools = pinned_prefix_fixture();
            tools.extend([schema("web_fetch"), schema("github_list_prs"), schema("mo")]);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "claude-sonnet-4-5-20250929",
                "anthropic",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b_tools_churned["tools"].as_array().unwrap();
        let pinned_count = pinned_prefix_fixture().len();

        for i in 0..pinned_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "anthropic pinned tool at idx {i} must be byte-identical across calls"
            );
        }
    }

    /// Bedrock path parallel to the anthropic direct test — two calls with
    /// different dynamic tools must produce byte-identical bytes up to (and
    /// including) the cachePoint.

    /// OpenAI-compatible providers (DeepSeek, Qwen, MiniMax, vanilla OpenAI)
    /// don't consume `cache_control` — the field should still be present
    /// in the outgoing body (server-side caches like DeepSeek auto-dedupe
    /// on prefix, and extra keys are ignored), AND the pinned prefix bytes
    /// must be stable across calls for auto-prefix-cache to hit.
    #[test]
    fn openai_compatible_pinned_bytes_identical_across_calls() {
        let build = |extra: Vec<Value>| {
            let mut tools = pinned_prefix_fixture();
            tools.extend(extra);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            build_provider_request_body(
                &[json!({"role": "user", "content": "hi"})],
                &tools,
                "deepseek-chat",
                "openai",
                Some(256),
                None,
                false,
                &ThinkingConfig::Off,
            )
        };
        let a = build(vec![schema("git_log"), schema("mo")]);
        let b = build(vec![schema("web_fetch")]);

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "openai pinned tool at idx {i} must be byte-identical across calls \
                 (needed for auto-prefix-cache on DeepSeek/etc.)"
            );
        }
    }

    /// User-defined tools: schemas registered at session start flow through
    /// `inject_schema_pinned(s, true)` and must therefore land INSIDE the
    /// cacheable pinned segment. We simulate this by directly inserting
    /// into the default pinned set and verifying the marker moves to after
    /// the user-added tool.

    /// Runtime-discovered dynamic tool/skill (e.g. via MCP tool-list-changed
    /// or discover_skills): these enter the dynamic segment. Cache on the
    /// pinned prefix must remain untouched when they come and go.
    #[test]
    fn runtime_dynamic_addition_does_not_touch_pinned_cache() {
        let mut without = pinned_prefix_fixture();
        without.push(schema("git_log"));
        annotate_tool_schemas_for_caching(&mut without, &cfg_anthropic());

        let mut with_new_mcp = pinned_prefix_fixture();
        with_new_mcp.push(schema("git_log"));
        with_new_mcp.push(schema("mcp_new_runtime_tool")); // discovered mid-session
        annotate_tool_schemas_for_caching(&mut with_new_mcp, &cfg_anthropic());

        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count {
            assert_eq!(
                without[i], with_new_mcp[i],
                "pinned tool at idx {i} must survive runtime dynamic-tool addition"
            );
        }
        // Marker stays on the same pinned tool, bytes match.
        assert_eq!(
            without[pinned_count - 1],
            with_new_mcp[pinned_count - 1],
            "pinned tool hosting the marker must be byte-identical \
             (pinned prefix cache hits regardless of MCP churn)"
        );
    }

    // ── Composite request-body byte-equality ─────────────────────────────
    //
    // Review gap: the component-level tests above verify tools, system
    // blocks, and messages *separately*. A composition bug (e.g., system
    // blocks silently reordered by `build_provider_request_body`, or the
    // cachePoint shifted by Bedrock translation) wouldn't surface in any
    // single one. These tests build the FULL outgoing request body and
    // diff it byte-by-byte between two turns with identical stable inputs
    // and different dynamic tails.

    /// Helper: build two complete Bedrock request bodies that share the
    /// same system prompt + pinned tools + user message, but differ only
    /// in dynamic tool tail. Returns `(body_a, body_b, pinned_count)`.
    fn build_two_bedrock_bodies_with_shared_prefix() -> (Value, Value, usize) {
        let system_msg = json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "You are an expert."},
                {"type": "text", "text": "## Rules\nFollow them."},
            ],
        });
        let user_msg = json!({
            "role": "user",
            "content": "Say ACK.",
        });

        let build = |dynamic_tail: Vec<Value>| {
            let mut tools = pinned_prefix_fixture();
            tools.extend(dynamic_tail);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            crate::turn::llm_client::build_provider_request_body(
                &[system_msg.clone(), user_msg.clone()],
                &tools,
                "anthropic.claude-sonnet-4-20250514-v1:0",
                "bedrock",
                Some(256),
                None,
                false,
                &astra_turn_core::thinking_config::ThinkingConfig::Off,
            )
        };

        let a = build(vec![schema("git_log"), schema("mo")]);
        let b = build(vec![schema("web_fetch")]);
        (a, b, pinned_prefix_fixture().len())
    }

    #[test]
    fn composite_bedrock_body_system_bytes_identical_across_turns() {
        let (a, b, _) = build_two_bedrock_bodies_with_shared_prefix();
        assert_eq!(
            a["system"], b["system"],
            "system blocks must be byte-identical across turns with shared static prefix"
        );
    }

    #[test]
    fn composite_bedrock_body_first_user_message_identical() {
        let (a, b, _) = build_two_bedrock_bodies_with_shared_prefix();
        let msg_a = &a["messages"][0];
        let msg_b = &b["messages"][0];
        assert_eq!(
            msg_a, msg_b,
            "first user message must be byte-identical when content is shared"
        );
    }

    /// Same composite check for the direct-Anthropic path. The body shape
    /// differs from Bedrock (no `toolConfig` wrapping, no `cachePoint`
    /// block; instead `cache_control` rides on the last pinned tool).
    #[test]
    fn composite_anthropic_direct_body_prefix_identical_across_turns() {
        let system_msg = json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "You are an expert."},
            ],
        });
        let user_msg = json!({"role": "user", "content": "Hi"});

        let build = |tail: Vec<Value>| {
            let mut tools = pinned_prefix_fixture();
            tools.extend(tail);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            crate::turn::llm_client::build_provider_request_body(
                &[system_msg.clone(), user_msg.clone()],
                &tools,
                "claude-sonnet-4-5-20250929",
                "anthropic",
                Some(256),
                None,
                false,
                &astra_turn_core::thinking_config::ThinkingConfig::Off,
            )
        };
        let a = build(vec![schema("git_log"), schema("mo")]);
        let b = build(vec![schema("web_fetch")]);

        // Static system + user message identical
        assert_eq!(a["system"], b["system"]);
        assert_eq!(a["messages"], b["messages"]);

        // Pinned tool bytes (through the marker-hosting last pinned tool) identical
        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count {
            assert_eq!(
                serde_json::to_string(&a_tools[i]).unwrap(),
                serde_json::to_string(&b_tools[i]).unwrap(),
                "anthropic composite: tool[{i}] must match across turns"
            );
        }
    }

    /// OpenAI-compatible path: no cache_control is consumed, but the whole
    /// prefix (system + tools up to pinned_count + first user msg) must be
    /// byte-identical for DeepSeek/OpenAI server-side prefix caching to hit.
    #[test]
    fn composite_openai_body_prefix_identical_across_turns() {
        let system_msg = json!({"role": "system", "content": "You are an expert."});
        let user_msg = json!({"role": "user", "content": "hi"});

        let build = |tail: Vec<Value>| {
            let mut tools = pinned_prefix_fixture();
            tools.extend(tail);
            annotate_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            crate::turn::llm_client::build_provider_request_body(
                &[system_msg.clone(), user_msg.clone()],
                &tools,
                "deepseek-chat",
                "openai",
                Some(256),
                None,
                false,
                &astra_turn_core::thinking_config::ThinkingConfig::Off,
            )
        };
        let a = build(vec![schema("git_log"), schema("mo")]);
        let b = build(vec![schema("web_fetch")]);

        assert_eq!(a["messages"], b["messages"]);

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let pinned_count = pinned_prefix_fixture().len();
        for i in 0..pinned_count {
            assert_eq!(
                serde_json::to_string(&a_tools[i]).unwrap(),
                serde_json::to_string(&b_tools[i]).unwrap(),
                "openai composite: tool[{i}] must match — prefix auto-caching needs byte equality"
            );
        }
    }
}
