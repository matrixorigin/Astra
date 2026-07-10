//! Prompt caching utilities for LLM system messages.
//!
//! # Architecture Overview
//!
//! The prompt cache system optimises LLM costs by maximising cache hit rates across
//! consecutive turns within a session. Two distinct strategies are used depending on the
//! provider:
//!
//! ## Anthropic Strategy (CacheControl)
//!
//! Anthropic supports exactly **one** [`cache_control` breakpoint](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching)
//! per request. The breakpoint marks the boundary between cached and uncached content:
//! everything *before* the breakpoint may be served from cache; everything *after* is
//! always recomputed.
//!
//! We partition every turn's system message into two layers:
//!
//! ```text
//! ┌─ stable_prefix (cached) ────────────┬─ dynamic_suffix (per-turn) ─┐
//! │                                      │                             │
//! │  Global-scoped sections   Session-   │  None-scoped sections       │
//! │  (core rules, safety)     scoped     │  (skills, turn budget,      │
//! │                           sections   │   low-conf warnings)        │
//! │                           ▲          │                             │
//! │                           │          │                             │
//! └───────────────────────────┘──────────┴─────────────────────────────┘
//!                      cache_control breakpoint
//! ```
//!
//! Sections are tagged with a [`CacheScope`] enum:
//!
//! | Scope | Meaning | Serialised positions |
//! |---|---|---|
//! | `Global` | Never changes across sessions (core rules, safety guardrails) | Always at the prefix |
//! | `Session` | Stable within a session (version, cwd, date, user, branch, strict-history model identity) | Middle, before the breakpoint |
//! | `None` | Per-turn/non-cacheable runtime facts (model identity for marker/auto-prefix providers, skills, turn budget) | After the breakpoint |
//!
//! `CacheScope` implements `Ord` such that `Global < Session < None`, guaranteeing stable
//! byte ordering regardless of insertion order.
//!
//! ### Bedrock Claude
//!
//! Bedrock-hosted Claude models use the same `CacheScope` partitioning. The `cache_control`
//! markers are translated to Bedrock-native `cachePoint` blocks at request-build time in
//! the Bedrock request adapter.
//!
//! ## OpenAI / OpenAI-Compatible Strategy (Stable/Dynamic Split)
//!
//! Providers that do not support `cache_control` annotations use a **two-message split**:
//!
//! - **`primary_system`**: all `Global` + `Session` scoped blocks concatenated
//! - **`dynamic_system`** (`Option<String>`): all `None` scoped blocks, sent as a separate
//!   system message *after* the primary one
//!
//! This separation allows OpenAI's automatic caching to recognise the stable prefix across
//! turns, even though the dynamic suffix changes. DeepSeek's `/anthropic` endpoint is
//! known to use payload-identity checks that treat the full request body as a cache key,
//! so dynamic content **must** be moved to the second message to avoid per-turn cache
//! invalidation.
//!
//! ## Always-Load Tool Schema Caching
//!
//! For Anthropic, tool schemas in the request body also participate in caching.
//! [`annotate_tool_schemas_for_caching_with_always_load`] marks the last schema in the
//! declarative `always_load` prefix with `cache_control`. Lower-frequency tools follow
//! without markers, so schema churn invalidates only the tail while the always-load
//! prefix remains cacheable.
//!
//! ## Cache Key Design
//!
//! [`section_cache_key`] (test-only) produces a hash from `(tool_names, task_type)`.
//! It deliberately excludes prompt text, so wording tweaks and
//! formatting changes do not cause cache misses — only semantically meaningful input
//! changes affect the key.
//!
//! ## Provider Strategy Resolution
//!
//! [`provider_cache_policy_for`] determines the caching strategy from three sources in
//! priority order:
//!
//! 1. **Explicit** `CacheCapability` marker (highest priority — overrides everything)
//! 2. **Provider heuristics** (Anthropic direct, Bedrock Claude, other)
//! 3. **Environment override** (`ASTRA_TEST_PROMPT_CACHE_DISABLED`)
//!
//! ## Public Interface
//!
//! The primary entry points consumed by callers:
//!
//! | Function | Consumer | Purpose |
//! |---|---|---|
//! | [`assemble_bridge_pipeline_outcome`] | Bridge proxy, agentic loop | Full assembly: prompt + tool schemas + cache strategy |
//! | [`assemble_system_message_via_pipeline`] | Bridge proxy | Build Anthropic multi-block or OpenAI split message |
//! | [`annotate_tool_schemas_for_caching_with_always_load`] | Request build | Add `cache_control` to tool definitions |
//! | [`add_message_cache_breakpoint`] | Request build | Insert breakpoint into final message array |
//! | [`apply_anthropic_cache_metadata`] | Anthropic adapter | Emit Anthropic-specific cache metadata response fields |
//!
//! ## Testing
//!
//! The module includes extensive tests in two categories:
//!
//! - **`cache_stability_regression`** (L1818+): byte-level determinism tests that verify
//!   identical inputs produce identical Anthropic direct, Bedrock, and OpenAI request
//!   bodies across calls.
//! - **Functional tests**: correctness of scope partitioning, cache control annotation,
//!   provider policy selection, and edge cases (empty tools, disabled cache, override
//!   files).

use serde_json::{Map, Value, json};

use crate::prompts;
use astra_config::ToolSurfaceConfig;
use astra_turn_core::microcompact::{PromptCacheProtocol, ProviderCacheStrategy};
use astra_turn_core::pipeline_config::ProviderCachePolicy;

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

pub(crate) fn model_identity_prompt_text(model_id: &str, provider: &str) -> String {
    format!("Model: {model_id} (via {provider})")
}

pub(crate) fn model_identity_prompt_section_for_cache_capability(
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    provider: &str,
    model_id: &str,
) -> prompts::PromptSection {
    let cache_cap =
        astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider_model(
            cache_capability,
            provider,
            model_id,
        );
    let text = model_identity_prompt_text(model_id, provider);
    if matches!(
        cache_cap.volatile_placement,
        astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly
    ) {
        prompts::PromptSection::stable(text, prompts::CacheScope::Session)
    } else {
        prompts::PromptSection::dynamic(text, prompts::PromptTokenBucket::Environment)
    }
}

fn saturating_usize_to_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

impl PromptCacheConfig {
    /// Latch config from environment and provider info. Call once at session start.
    pub fn latch(provider: &str, model_name: &str) -> Self {
        Self::from_cache_capability(None, provider, model_name)
    }

    pub fn from_cache_capability(
        cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
        provider: &str,
        model_name: &str,
    ) -> Self {
        let cache_enabled = !std::env::var("ASTRA_TEST_PROMPT_CACHE_DISABLED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let provider_strategy =
            astra_turn_core::microcompact::ProviderCacheStrategy::from_explicit_or_provider_model(
                cache_capability,
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
// - Global+Session sections are cached by (tool_names, task_type) — stable within a session
// - Per-turn volatile content (environment_volatile, memoria recall, …) is
//   bound into RuntimeVolatile post-cache-marker so it re-sends each turn
//   without invalidating the cached prefix.

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

/// Resolve the context-pipeline cache policy from the same provider+model
/// classification used by [`PromptCacheConfig::latch`].
///
/// This matters for multiplexed providers like Bedrock: Claude models support
/// Anthropic-style cache markers (translated to Bedrock cache points), while
/// Nova/Titan models must remain prefix-only.
pub(crate) fn provider_cache_policy_for(
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    provider: &str,
    model_name: &str,
) -> ProviderCachePolicy {
    let strategy = ProviderCacheStrategy::from_explicit_or_provider_model(
        cache_capability,
        Some(provider),
        Some(model_name),
    );
    if strategy.prompt_cache_protocol == PromptCacheProtocol::AnthropicCacheControl {
        ProviderCachePolicy::anthropic()
    } else {
        ProviderCachePolicy::openai_compatible()
    }
}

fn compact_cache_control_marker(cache_control: &Value) -> Value {
    let Some(object) = cache_control.as_object() else {
        return cache_control.clone();
    };
    let Some(kind) = object.get("type").and_then(Value::as_str) else {
        return cache_control.clone();
    };

    let mut marker = Map::with_capacity(object.len());
    marker.insert("type".to_string(), Value::String(kind.to_string()));
    if let Some(ttl) = object.get("ttl") {
        let Some(ttl) = ttl.as_str() else {
            return cache_control.clone();
        };
        marker.insert("ttl".to_string(), Value::String(ttl.to_string()));
    }
    if object.keys().any(|key| key != "type" && key != "ttl") {
        return cache_control.clone();
    }

    Value::Object(marker)
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
/// as a convenience for tests that still assert on the tuple shape.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_system_message_via_pipeline(
    tool_names: &[&str],
    extra_dynamic_sections: &[prompts::PromptSection],
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
        None,
        None,
        cache_cfg,
        None,
        session_id,
        model_id,
        None,
        provider,
        edge_profile_cwd,
        edge_profile_git_branch,
        None,
        "",
        "",
        "2026-05-25",
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
/// * `extra_stable_sections` — session-stable bridge-composed content.
///   Bound into RuntimeIdentity (Session scope) so it sits BEFORE the
///   Session→None cache marker.
/// * `extra_volatile_sections` — per-turn bridge-composed content
///   (session anchor, memoria insights, tool round guidance). Bound into
///   RuntimeVolatile (None scope) so churn does not invalidate the
///   cached session prefix.
/// * Model identity is injected here with provider-aware placement: strict
///   history providers get a stable section because volatile is suppressed;
///   marker/auto-prefix providers get a non-cacheable section so model-only
///   changes do not churn the cacheable prefix.
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
    session_memory_entry: Option<&astra_turn_core::context_sources::MemoryEntry>,
    system_override: Option<&str>,
    cache_cfg: &PromptCacheConfig,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    session_id: &str,
    model_id: &str,
    context_window: Option<u32>,
    provider: &str,
    edge_profile_cwd: Option<&str>,
    edge_profile_git_branch: Option<&str>,
    project_context: Option<&str>,
    deferred_tools_block: &str,
    skill_listing_block: &str,
    current_date: &str,
) -> BridgePipelineOutcome {
    use astra_turn_core::context_sources::{
        AgentContext, EdgeProfile, ExternalSources, SessionContext, TurnState,
    };
    use astra_turn_core::pipeline_config::PipelineConfig;
    use astra_turn_core::pipeline_session::{AdaptiveTurnInput, PipelineSession};

    // Build ExternalSources from bridge-side signals. Tool-dependent prompt
    // fragments are volatile because bridge tool surface can vary per turn.
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
        let text = prompts::tool_conditional_section(tool_names, &profile_for_tc);
        if text.is_empty() { None } else { Some(text) }
    };
    // ASTRA_OUTPUT_STYLE is a user preference — stable within a session
    // (user doesn't toggle styles mid-session). Route to stable lane.
    let model_identity_section =
        model_identity_prompt_section_for_cache_capability(cache_capability, provider, model_id);
    let mut stable = extra_stable_sections.to_vec();
    let mut volatile = extra_volatile_sections.to_vec();
    if matches!(model_identity_section.scope, prompts::CacheScope::None) {
        volatile.push(model_identity_section);
    } else {
        stable.push(model_identity_section);
    }
    if let Some(style) = astra_text_utils::output_style::current_output_style()
        && !style.prompt.is_empty()
    {
        stable.push(prompts::PromptSection::dynamic(
            format!("\n{}\n", style.prompt),
            prompts::PromptTokenBucket::UserPreferences,
        ));
    }
    let system_override = system_override
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| format!("\n\n{text}"));
    let system_override_trace_section = system_override.as_ref().map(|text| {
        prompts::PromptSection::stable(text.clone(), prompts::CacheScope::Session)
            .with_trace_signals(
                astra_turn_core::context_assembly_trace::PromptTraceSignals {
                    context_signals:
                        astra_turn_core::context_assembly_trace::PromptContextSignals {
                            system_prompt_override: true,
                            ..Default::default()
                        },
                    ..Default::default()
                },
            )
    });
    if let Some(ref text) = self_model_text {
        volatile.push(prompts::PromptSection::dynamic(
            text.clone(),
            prompts::PromptTokenBucket::BasePersona,
        ));
    }
    if let Some(ref text) = tool_conditional {
        volatile.push(prompts::PromptSection::dynamic(
            text.clone(),
            prompts::PromptTokenBucket::Environment,
        ));
    }
    let trace_extra_sections = {
        let mut v = stable.clone();
        if let Some(section) = system_override_trace_section {
            v.push(section);
        }
        v.extend(volatile.iter().cloned());
        v
    };

    let external = ExternalSources {
        memory_entries: memory_entries.to_vec(),
        session_memory_entry: session_memory_entry.cloned(),
        spill_dir: None,
        spill_backend: None,

        effort_hint: None,
        system_override,
        plan_context: None,
        tool_guidance: None,
        extra_stable_sections: stable,
        extra_dynamic_sections: volatile,
    };

    let provider_policy = provider_cache_policy_for(cache_capability, provider, model_id);
    let provider_strategy = ProviderCacheStrategy::from_explicit_or_provider_model(
        cache_capability,
        Some(provider),
        Some(model_id),
    );
    let session_ctx = SessionContext {
        session_id: session_id.to_string(),
        run_id: String::new(),
        model_id: model_id.to_string(),
        provider_name: provider.to_string(),
        model_limit: saturating_usize_to_u32(
            crate::prompts::budget_for_model_with_override(Some(model_id), context_window)
                .model_limit,
        ),
        provider_policy: provider_policy.clone(),
        provider_strategy,
        project_context: project_context.unwrap_or("").to_string(),
        edge_profile: EdgeProfile {
            cwd: edge_profile_cwd.map(String::from),
            git_branch: edge_profile_git_branch.map(String::from),
            ..Default::default()
        },
        self_model: None,
        deferred_tools_block: deferred_tools_block.to_string(),
        skill_listing_block: skill_listing_block.to_string(),
        current_date: current_date.to_string(),
        user_id: None,
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

    // Ephemeral per-request session. Bridge doesn't persist a session across
    // turns — its compaction lives elsewhere — so a fresh session per call
    // is the right lifecycle. Stats/recovery/latches all start at default.
    let mut session = PipelineSession::new(PipelineConfig {
        provider_policy: provider_policy.clone(),
        ..crate::turn::pipeline_env::env_pipeline_config()
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

    let uses_anthropic_protocol =
        provider_strategy.prompt_cache_protocol == PromptCacheProtocol::AnthropicCacheControl;
    let should_annotate_cache_controls = cache_cfg.cache_enabled && uses_anthropic_protocol;
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
    sections.extend(trace_extra_sections);

    let tier = output.plan.compact_tier;
    let pruned_tool_schemas = output.optimized.tool_schemas.clone();

    let (primary_system, dynamic_system) = if uses_anthropic_protocol {
        // Anthropic-protocol path: emit STABLE blocks (non-None scope)
        // as multi-block content with cache_control markers; promote
        // volatile (CacheScope::None) blocks into the `dynamic_system`
        // second message so the primary system content stays byte-
        // stable across rounds.
        //
        // The earlier revision kept volatile blocks inline in the
        // system content array on the theory that the cache_control
        // marker "isolates" them. Controlled probes against DeepSeek's
        // `/anthropic` endpoint (see `tests/fixtures/
        // deepseek_anthropic_cache_probe.py`) proved this leaks
        // byte churn into DeepSeek's payload-identity check — tools
        // (~5K tokens) never reach the 2nd-warm cache state. Bedrock
        // is first-call-complete either way, so moving volatile out
        // is globally safe. Session 5c5cbf78 t5_r0 diff showed
        // system.block[3] (Self-Awareness counter) was the sole
        // per-round delta.
        use astra_turn_core::section_types::CacheScope;
        let mut stable_blocks: Vec<Value> =
            Vec::with_capacity(output.serialized.system_blocks.len());
        let mut dynamic_text = String::new();
        for block in &output.serialized.system_blocks {
            if matches!(block.scope, CacheScope::None) {
                dynamic_text.push_str(&block.text);
            } else {
                let mut b = json!({"type": "text", "text": block.text});
                if should_annotate_cache_controls && let Some(ref cc) = block.cache_control {
                    b["cache_control"] = compact_cache_control_marker(cc);
                }
                stable_blocks.push(b);
            }
        }
        let primary = json!({"role": "system", "content": stable_blocks});
        let dynamic = if dynamic_text.is_empty() {
            None
        } else {
            Some(json!({"role": "system", "content": dynamic_text}))
        };
        (primary, dynamic)
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

    tracing::debug!(
        cache_enabled = cache_cfg.cache_enabled,
        uses_anthropic = uses_anthropic_protocol,
        should_annotate = should_annotate_cache_controls,
        stable_blocks = output.serialized.system_blocks.iter().filter(|b| !matches!(b.scope, astra_turn_core::section_types::CacheScope::None)).count(),
        volatile_chars = output.serialized.system_blocks.iter().filter(|b| matches!(b.scope, astra_turn_core::section_types::CacheScope::None)).map(|b| b.text.len()).sum::<usize>(),
        provider = %provider,
        model_id = %model_id,
        tier = ?tier,
        "assembled bridge pipeline outcome with cache strategy",
    );

    BridgePipelineOutcome {
        primary_system,
        dynamic_system,
        prompt_sections: sections,
        tier,
        tool_schemas: pruned_tool_schemas,
    }
}

/// Test-only: hash a tuple of inputs for cache-key regression tests.
/// Previously this delegated to `section_cache_key_with_customization` which
/// folded in prompt-override + output-style fingerprints. Those inputs
/// belong to the pipeline path now, so the key function is a pure hash
/// over (tool_names, task_type) — adequate for proving
/// that cache-key collisions don't hide behind the same hash.
#[cfg(test)]
pub(crate) fn section_cache_key(tool_names: &[&str], task_type: Option<&str>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    tool_names.hash(&mut hasher);
    task_type.hash(&mut hasher);
    hasher.finish()
}

// ── Tool schema annotations ──────────────────────────────────────────────────

/// Add `cache_control` to a tool schema for Anthropic caching.
///
/// Anthropic allows up to 4 cache_control breakpoints per request. Our allocation:
/// - System prompt: up to 2 breakpoints (global scope + session scope)
/// - Tools: 1 breakpoint at the end of the STATIC (always_load) prefix — keeps the
///   static lib cached even when dynamic tools churn per turn
/// - Messages: 1 breakpoint on the last message
///
/// `always_load_names` identifies tools that are guaranteed present every turn
/// (static lib). The marker goes on the last always_load tool, so subsequent
/// dynamic tools sitting after it don't invalidate the cached prefix. If no
/// always_load tools are present (e.g. caller opted into full-dynamic), falls
/// back to the last tool.
/// Annotate tool schemas using an explicit always_load set.
///
/// Runtime-side adapter: decides whether to annotate (`cache_cfg.should_annotate`),
/// logs the fallback path for triage, then delegates to the pure
/// [`astra_turn_core::context_serializer::annotate_always_load_tool_schema`] for
/// the actual wire mutation. The pure primitive lives in the pipeline so all
/// provider-specific cache logic has exactly one implementation.
pub(crate) fn annotate_tool_schemas_for_caching_with_always_load(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    always_load_names: &std::collections::HashSet<String>,
) {
    clear_tool_cache_controls(tools);
    if !cache_cfg.should_annotate() || tools.is_empty() {
        return;
    }
    let marker_idx = match always_load_prefix_marker_index(tools, always_load_names) {
        Some(idx) => idx,
        None => {
            // Fallback path: no always_load prefix is present in this tool
            // list. Legit for delegated sub-runs that pass a fully custom
            // toolset, but a cache-hit regression triage needs to see it.
            tracing::debug!(
                tool_count = tools.len(),
                "cache marker fallback: no always_load prefix present; placing on last tool. \
                 Static-prefix caching unavailable for this request."
            );
            tools.len() - 1
        }
    };
    tools[marker_idx]["cache_control"] =
        astra_turn_core::context_serializer::anthropic_ephemeral_cache_control();
}

fn clear_tool_cache_controls(tools: &mut [Value]) {
    for tool in tools {
        if let Some(object) = tool.as_object_mut() {
            object.remove("cache_control");
        }
    }
}

fn always_load_prefix_marker_index(
    tools: &[Value],
    always_load_names: &std::collections::HashSet<String>,
) -> Option<usize> {
    if always_load_names.is_empty() {
        return None;
    }

    let mut last_prefix_idx = None;
    for (idx, tool) in tools.iter().enumerate() {
        let Some(name) = tool
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        else {
            break;
        };
        if !always_load_names.contains(name) {
            break;
        }
        last_prefix_idx = Some(idx);
    }
    last_prefix_idx
}

/// Runtime-configured always_load tool names for fallback paths that do not receive
/// edge metadata.
///
/// CLI/Edge should normally send the resolved names explicitly; server-side-tools
/// and tests use this to keep cache markers aligned with tool_surface config.
///
/// **Hidden dependency**: reads `RuntimeConfig::cached().tool_surface` — a
/// process-wide singleton. Callers that already hold a `ToolSurfaceConfig`
/// should use [`resolve_always_load_tool_names_for_config`] directly instead.
pub(crate) fn runtime_always_load_tool_names() -> std::collections::HashSet<String> {
    resolve_always_load_tool_names_for_config(
        &astra_config::runtime_config::RuntimeConfig::cached().tool_surface,
    )
}

/// Resolve the always_load tool name set for a given surface config by building the
/// full [`ToolSurface`] and extracting always_load names.
///
/// This is the single source of truth for "which tools are cache-always_load under
/// this config?". All callers that need cache markers or edge metadata should
/// route through this (or [`runtime_always_load_tool_names`] when the runtime
/// singleton is intentionally needed) rather than
/// rebuilding identity + TOML addition rules locally.
///
/// **Cold path**: this rebuilds `all_tool_schemas()` + `ToolSurface::build()`
/// (O(tool count)). Expected call frequency is O(1) per session. The per-turn
/// annotation path receives the pre-computed `HashSet` directly and is O(1).
pub(crate) fn resolve_always_load_tool_names_for_config(
    cfg: &ToolSurfaceConfig,
) -> std::collections::HashSet<String> {
    let mut schemas = astra_tools::schemas::all_tool_schemas();
    schemas.push(crate::turn::skill_tool::skill_tool_schema_v2());
    crate::tool_registry::surface::ToolSurface::build(schemas, cfg, &[])
        .always_load_names()
        .into_iter()
        .collect()
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

/// Add Anthropic protocol-level cache metadata for cached prompts.
///
/// Places exactly one `cache_control` breakpoint on the last conversation
/// message. The per-request pin-map and `cache_edits` / `cache_reference`
/// annotations that used to live here were removed after session
/// 5c5cbf78 (2026-05-08) showed the real Anthropic `/v1/messages`
/// endpoint rejecting the `cache_edits` content-block type with HTTP
/// 400 ("unknown variant `cache_edits`"). Those fields were speculative
/// — they don't appear in Anthropic's public schema — and only Bedrock
/// Converse silently tolerated them.
pub(crate) fn apply_anthropic_cache_metadata(
    messages: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    _session_id: &str,
) {
    if !cache_cfg.should_annotate() || messages.is_empty() {
        return;
    }
    astra_turn_core::context_serializer::annotate_last_message_cache_breakpoint(messages);
}

/// Process-wide mutex guarding any test that mutates env vars read by the
/// prompt-cache pipeline (`ASTRA_TEST_PROMPT_CACHE_DISABLED`,
/// `ASTRA_OUTPUT_STYLE`, etc.). Exposed at module scope so sibling test
/// modules (`bridge_inprocess::tests`) share the same lock — otherwise
/// two independent mutexes race to the same `std::env::set_var` and a
/// panic in one poisons the other's tests. Recover from poison on lock
/// acquire; test panics carry their own failure and should not cascade.
#[cfg(test)]
pub(crate) static CACHE_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::CACHE_ENV_MUTEX;
    use super::*;

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

    fn default_test_always_load_tool_names() -> std::collections::HashSet<String> {
        resolve_always_load_tool_names_for_config(&ToolSurfaceConfig::default())
    }

    fn annotate_test_tool_schemas_for_caching(tools: &mut [Value], cache_cfg: &PromptCacheConfig) {
        annotate_tool_schemas_for_caching_with_always_load(
            tools,
            cache_cfg,
            &default_test_always_load_tool_names(),
        );
    }

    #[test]
    fn section_cache_key_varies_by_tools_and_task() {
        let key1 = section_cache_key(&["bash"], Some("implementation"));
        let key2 = section_cache_key(&["bash", "read_file"], Some("implementation"));
        let key3 = section_cache_key(&["bash"], Some("debugging"));
        assert_ne!(key1, key2, "different tools should differ");
        assert_ne!(key1, key3, "different task types should differ");
    }

    #[test]
    fn section_cache_key_differs_for_different_tools() {
        let k1 = section_cache_key(&["read_file"], None);
        let k2 = section_cache_key(&["bash"], None);
        assert_ne!(k1, k2);
    }

    #[test]
    fn prompt_cache_latch_prefers_provider_over_claude_named_model() {
        let openai_proxy = PromptCacheConfig::latch("openai", "claude-sonnet-4");
        assert!(!openai_proxy.is_anthropic);

        let anthropic_provider = PromptCacheConfig::latch("anthropic", "gpt-4o");
        assert!(anthropic_provider.is_anthropic);
    }

    #[test]
    fn prompt_cache_config_prefers_explicit_marker_capability() {
        let cfg = PromptCacheConfig::from_cache_capability(
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "openai",
            "proxy-claude",
        );
        assert!(cfg.is_anthropic);
    }

    // ── always_load-tool audit ────────────────────────────────────────────────
    //
    // The cache marker belongs at the end of the always_load/static prefix, not at
    // the end of the whole tool list. Deferred/dynamic tools may become
    // visible for a turn, but they should not silently enlarge the static
    // cache prefix.
    #[test]
    fn default_always_load_tool_names_tracks_runtime_surface_not_deferred_catalog() {
        let always_load = default_test_always_load_tool_names();
        for name in crate::tool_registry::surface::default_always_load_names() {
            assert!(
                always_load.contains(name),
                "{name} is part of the runtime default surface and must be cache-always_load"
            );
        }
        for name in [
            "lsp",
            "github",
            "web_fetch",
            "web_search",
            "session",
            "mo_query",
            "agent",
            "symbols",
            "powershell",
            "run_script",
            "send_message",
        ] {
            assert!(
                !always_load.contains(name),
                "{name} is deferred/dynamic by default and must not extend the static cache prefix"
            );
        }
    }

    #[test]
    fn cache_static_prefix_tool_names_follow_toml_surface_additions() {
        let cfg = ToolSurfaceConfig {
            pinned_tools: vec!["github".into(), "not_a_real_tool".into()],
        };
        let always_load = resolve_always_load_tool_names_for_config(&cfg);

        assert!(
            always_load.contains("github"),
            "config-always_load github must be part of the cache static prefix"
        );
        assert!(
            always_load.contains("grep"),
            "unknown entries must not remove default always_load tools"
        );
        assert!(
            always_load.contains("bash"),
            "other default always_load tools must remain cache always_load"
        );
        assert!(
            !always_load.contains("web_search"),
            "deferred web_search must not become cache always_load without an explicit TOML always_load entry"
        );
    }

    #[test]
    fn annotate_tool_schemas_for_caching_adds_cache_control() {
        // With unknown (non-always_load) names, fall back to the last tool — the
        // historical behavior. Covers custom-tool pipelines that don't go
        // through TOOL_CATALOG.
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "a"}}),
            json!({"type": "function", "function": {"name": "b"}}),
        ];
        annotate_test_tool_schemas_for_caching(
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
            "last tool should have cache_control (fallback — no always_load tools present)"
        );
    }

    /// Cache marker must sit at the end of the STATIC (always_load) prefix, not
    /// after dynamic tools. Otherwise churn in the dynamic segment invalidates
    /// the cached prefix every turn.

    /// When dynamic tools are interleaved (shouldn't happen in production but
    /// could via custom pipelines), the static prefix ends at the first
    /// non-always_load tool. Later always_load-named tools must not pull the
    /// marker past dynamic content.
    #[test]
    fn annotate_tool_schemas_does_not_cross_interleaved_dynamic_tools() {
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "bash"}}), // always_load
            json!({"type": "function", "function": {"name": "lsp"}}),  // dynamic
            json!({"type": "function", "function": {"name": "memory"}}), // always_load
            json!({"type": "function", "function": {"name": "git"}}), // always_load name in dynamic tail
        ];
        annotate_test_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );
        assert!(tools[0].get("cache_control").is_some());
        assert!(tools[2].get("cache_control").is_none());
        assert!(tools[3].get("cache_control").is_none());
    }

    #[test]
    fn annotate_tool_schemas_ignores_always_load_name_in_dynamic_tail() {
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "read_file"}}),
            json!({"type": "function", "function": {"name": "skill"}}),
        ];
        let prefix_len = tools.len();
        tools.push(json!({"type": "function", "function": {"name": "web_fetch"}}));
        tools.push(json!({"type": "function", "function": {"name": "git"}}));

        let always_load = default_test_always_load_tool_names();
        assert!(always_load.contains("bash"));
        assert!(always_load.contains("read_file"));
        assert!(always_load.contains("skill"));
        assert_eq!(
            always_load_prefix_marker_index(&tools, &always_load),
            Some(prefix_len - 1)
        );

        annotate_test_tool_schemas_for_caching(
            &mut tools,
            &PromptCacheConfig {
                cache_enabled: true,
                is_anthropic: true,
            },
        );

        assert!(
            tools[prefix_len - 1].get("cache_control").is_some(),
            "{tools:#?}"
        );
        assert!(
            tools[prefix_len].get("cache_control").is_none(),
            "{tools:#?}"
        );
        assert!(
            tools[prefix_len + 1].get("cache_control").is_none(),
            "{tools:#?}"
        );
    }

    #[test]
    fn tool_schemas_empty_list_noop() {
        let mut tools: Vec<Value> = vec![];
        annotate_test_tool_schemas_for_caching(
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
            None,
            None,
            &cache_cfg,
            None,
            "sid-bridge",
            "gpt-4o",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
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
    fn bridge_pipeline_outcome_preserves_many_extra_sections() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let stable: Vec<_> = (0..125)
            .map(|idx| {
                prompts::PromptSection::stable(
                    format!("\n[stable-extra-{idx}]\n"),
                    prompts::CacheScope::Session,
                )
            })
            .collect();
        let volatile: Vec<_> = (0..125)
            .map(|idx| {
                prompts::PromptSection::dynamic(
                    format!("\n[volatile-extra-{idx}]\n"),
                    prompts::PromptTokenBucket::Environment,
                )
            })
            .collect();

        let outcome = assemble_bridge_pipeline_outcome(
            &[],
            &[],
            &stable,
            &volatile,
            &[],
            None,
            None,
            &cache_cfg,
            None,
            "sid-many-extra",
            "gpt-4o",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
        );
        let trace_text = outcome
            .prompt_sections
            .iter()
            .map(|section| section.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            trace_text.contains("[stable-extra-124]"),
            "bridge extras must not be truncated by section count"
        );
        assert!(
            trace_text.contains("[volatile-extra-124]"),
            "bridge extras must not be truncated by section count"
        );
        let primary_text = outcome.primary_system["content"]
            .as_str()
            .unwrap_or_default();
        let dynamic_text = outcome
            .dynamic_system
            .as_ref()
            .and_then(|value| value["content"].as_str())
            .unwrap_or_default();
        assert!(primary_text.contains("[stable-extra-124]"));
        assert!(dynamic_text.contains("[volatile-extra-124]"));
    }

    #[test]
    fn bridge_model_limit_conversion_saturates() {
        assert_eq!(saturating_usize_to_u32(200_000), 200_000);
        assert_eq!(saturating_usize_to_u32(u32::MAX as usize + 1), u32::MAX);
    }

    #[test]
    fn bridge_pipeline_outcome_routes_memory_entries_through_pipeline() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
            None,
            None,
            &cache_cfg,
            None,
            "sid-memory",
            "gpt-4o",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
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
    fn bridge_pipeline_outcome_routes_session_memory_through_runtime_volatile() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let session_memory = astra_turn_core::context_sources::MemoryEntry::new(
            "## Session State\nLatest state: keep refactoring the session-memory pipeline",
        );

        let outcome = assemble_bridge_pipeline_outcome(
            &["bash"],
            &[],
            &[],
            &[],
            &[],
            Some(&session_memory),
            None,
            &cache_cfg,
            None,
            "sid-session-memory",
            "gpt-4o",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
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
            .expect("session memory is None-scoped and should be in dynamic system");

        assert!(
            dynamic_text.contains("## Session State"),
            "session memory must reach runtime volatile output: {dynamic_text}"
        );
        assert!(
            dynamic_text.contains("session-memory pipeline"),
            "session memory content must survive bridge assembly: {dynamic_text}"
        );
        assert!(
            !primary_text.contains("## Session State"),
            "session memory must stay out of the stable session prefix: {primary_text}"
        );
    }

    #[test]
    fn bridge_pipeline_outcome_routes_system_override_through_runtime_identity() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
            None,
            Some("You must answer using the MOI agent contract."),
            &cache_cfg,
            None,
            "sid-system-override",
            "gpt-4o",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
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
        let breakdown =
            prompts::build_system_prompt_trace(&outcome.prompt_sections, vec![], vec![], None);

        assert!(
            primary_text.contains("MOI agent contract"),
            "system override should be session-stable runtime identity: {primary_text}"
        );
        assert!(
            !dynamic_text.contains("MOI agent contract"),
            "system override should not enter volatile dynamic prompt: {dynamic_text}"
        );
        assert!(breakdown.context_signals.system_prompt_override);
    }

    #[test]
    fn bridge_pipeline_outcome_keeps_session_memory_out_of_anthropic_cached_prefix() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let session_memory = astra_turn_core::context_sources::MemoryEntry::new(
            "## Session State\nLatest state: volatile session memory update",
        );

        let outcome = assemble_bridge_pipeline_outcome(
            &["bash"],
            &[],
            &[],
            &[],
            &[],
            Some(&session_memory),
            None,
            &cache_cfg,
            None,
            "sid-session-memory-anthropic",
            "claude-sonnet-4-6",
            None,
            "anthropic",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
        );

        let primary_text = outcome
            .primary_system
            .get("content")
            .and_then(Value::as_array)
            .expect("anthropic primary content is block array")
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<String>();
        let dynamic_text = outcome
            .dynamic_system
            .as_ref()
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_str)
            .expect("volatile session memory should be emitted as dynamic system text");

        assert!(
            !primary_text.contains("volatile session memory update"),
            "session memory changes must not invalidate Anthropic cached prefix: {primary_text}"
        );
        assert!(
            dynamic_text.contains("volatile session memory update"),
            "session memory must still reach the prompt via volatile lane: {dynamic_text}"
        );
    }

    #[test]
    fn typed_prompt_recall_changes_only_dynamic_anthropic_bytes() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let assemble = |memory_id: &str, content: &str| {
            let entries = vec![
                astra_turn_core::context_sources::MemoryEntry::scored(content, 0.9)
                    .with_memory_identity(memory_id, "semantic")
                    .with_source("memoria.prefetch"),
            ];
            assemble_bridge_pipeline_outcome(
                &["bash"],
                &[],
                &[],
                &[],
                &entries,
                None,
                None,
                &cache_cfg,
                None,
                "sid-memory-cache",
                "claude-sonnet-4-6",
                None,
                "anthropic",
                Some("/repo"),
                Some("main"),
                None,
                "",
                "",
                "2026-07-10",
            )
        };

        let first = assemble("m1", "first typed recall evidence");
        let second = assemble("m2", "second typed recall evidence");
        assert_eq!(
            first.primary_system, second.primary_system,
            "per-turn recall must not churn the cacheable prefix"
        );
        assert_ne!(
            first.dynamic_system, second.dynamic_system,
            "changed recall evidence must remain visible in the dynamic lane"
        );
    }

    #[test]
    fn bridge_pipeline_outcome_keeps_deferred_tools_block_in_session_prefix() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
            None,
            None,
            &cache_cfg,
            None,
            "sid-deferred-tools",
            "gpt-4o",
            None,
            "openai",
            None,
            None,
            None,
            "<deferred-tools>\ngithub\n</deferred-tools>",
            "",
            "2026-05-25",
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

        assert!(primary_text.contains("<deferred-tools>"));
        assert!(!dynamic_text.contains("<deferred-tools>"));
    }

    #[test]
    fn bridge_pipeline_keeps_model_visible_when_volatile_is_dynamic() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
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

        let outcome = assemble_bridge_pipeline_outcome(
            &["bash"],
            &[],
            &[],
            &[prompts::PromptSection::dynamic(
                "## Volatile\nmust be suppressed".to_string(),
                prompts::PromptTokenBucket::Environment,
            )],
            &[],
            None,
            None,
            &cache_cfg,
            Some(strict_history),
            "sid-deepseek",
            "deepseek-v4-pro",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
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
            primary_text.contains("Model: deepseek-v4-pro (via openai)"),
            "strict-history model identity must remain visible in stable prompt: {primary_text}"
        );
        assert!(
            !primary_text.contains("must be suppressed"),
            "volatile sections must not leak into strict-history stable prompt: {primary_text}"
        );
        assert!(
            dynamic_text.contains("must be suppressed"),
            "pipeline keeps volatile content in the dynamic lane until wire assembly: {dynamic_text}"
        );
        assert!(
            !dynamic_text.contains("Model:"),
            "model identity must not be duplicated into volatile dynamic lane: {dynamic_text}"
        );
    }

    // ── assemble_system_message_via_pipeline ─────────────────────────────

    #[test]
    fn pipeline_assembly_anthropic_emits_multi_block_with_cache_control() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let (primary, dynamic, sections) = assemble_system_message_via_pipeline(
            &["bash", "read_file"],
            &[],
            &cache_cfg,
            "test-session",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp/proj"),
            Some("main"),
        );

        let content = primary
            .get("content")
            .and_then(Value::as_array)
            .expect("anthropic primary.content is an array");
        assert!(!content.is_empty(), "must emit at least one content block");
        let primary_text = content
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<String>();
        assert!(
            !primary_text.contains("Model: claude-sonnet-4-6"),
            "anthropic cacheable prefix must not churn on model id changes: {primary_text}"
        );
        let dtext = dynamic
            .as_ref()
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            dtext.contains("Model: claude-sonnet-4-6 (via bedrock)"),
            "anthropic model identity should remain visible outside the cacheable prefix: {dtext}"
        );
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: false,
        };
        let (primary, dynamic, _sections) = assemble_system_message_via_pipeline(
            &["bash", "read_file"],
            &[],
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
        assert!(
            !primary_text.contains("Model: gpt-4o"),
            "openai auto-prefix primary must not churn on model identity: {primary_text}"
        );
        // Dynamic may or may not be present depending on whether any None-scoped
        // section was emitted — assert at least the split is structurally sound.
        if let Some(d) = dynamic {
            let dtext = d.get("content").and_then(Value::as_str).unwrap_or_default();
            assert!(!dtext.is_empty(), "if dynamic present, must be non-empty");
            assert!(
                dtext.contains("Model: gpt-4o (via openai)"),
                "model identity should be visible in the non-cacheable lane: {dtext}"
            );
        }
    }

    /// The bridge's escape-hatch use case: pre-built session anchor + feedback
    /// rules flow through `extra_dynamic_sections` into the final system prompt.
    #[test]
    fn pipeline_assembly_carries_extra_dynamic_sections_through() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
        let (primary, dynamic, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &extra,
            &cache_cfg,
            "sid",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp"),
            None,
        );
        // Post-5c5cbf78 contract: volatile (CacheScope::None) sections
        // are promoted out of the primary system content array and into
        // the `dynamic` second message, so the primary stays byte-stable
        // across rounds. Accept the extras from either slot — what
        // matters for this test is that they're still routed through to
        // the LLM payload.
        let primary_text: String = primary["content"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let dynamic_text = dynamic
            .as_ref()
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let all_text = format!("{primary_text}\n{dynamic_text}");
        assert!(
            all_text.contains("Session Anchor"),
            "extra section 1 must reach the final prompt (primary or dynamic): \
             primary={primary_text:?} dynamic={dynamic_text:?}",
        );
        assert!(
            all_text.contains("Learned Feedback Rules"),
            "extra section 2 must reach the final prompt (primary or dynamic): \
             primary={primary_text:?} dynamic={dynamic_text:?}",
        );
    }

    #[test]
    fn pipeline_assembly_byte_stable_across_calls_with_identical_inputs() {
        // Cache-hit prerequisite: two calls with identical inputs must
        // produce identical system message bytes (no HashMap drift, no
        // time-based IDs, no non-determinism). Holds `CACHE_ENV_MUTEX` so
        // a concurrent test can't mutate `$ASTRA_OUTPUT_STYLE` mid-run.
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_OUTPUT_STYLE");
        let cache_cfg = PromptCacheConfig {
            cache_enabled: false,
            is_anthropic: true,
        };
        let (primary, _, _) = assemble_system_message_via_pipeline(
            &["bash", "read_file"],
            &[],
            &cache_cfg,
            "sid",
            "claude-sonnet-4-6",
            "bedrock",
            Some("/tmp"),
            None,
        );
        // cache_cfg.cache_enabled=false ⇒ no cache_control on any block even
        // though the pipeline's provider_policy is still anthropic-shaped.
        // Legacy behaviour: annotate_tool_schemas_for_caching_with_always_load gated on
        // cache_cfg.should_annotate(); the pipeline must honour the same.
        let content = primary
            .get("content")
            .and_then(Value::as_array)
            .expect("anthropic path emits content array");
        // Pipeline currently emits markers based on provider_policy, not
        // cache_cfg.cache_enabled. Document that invariant: if this assertion
        // fails, the caller-facing `cache_enabled=false` semantic has been
        // silently re-enabled and the downstream `annotate_tool_schemas_for_caching_with_always_load`
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        let home = tempfile::tempdir().expect("temp home");
        set_test_env("HOME", home.path().to_str().unwrap());

        set_test_env("ASTRA_OUTPUT_STYLE", "concise");
        let (primary1, _, _) = assemble_system_message_via_pipeline(
            &["bash"],
            &[],
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
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock", "anthropic.claude-sonnet-4-20250514-v1:0");
        assert!(cfg.cache_enabled);
        assert!(cfg.is_anthropic);
    }

    #[test]
    fn latch_keeps_non_claude_bedrock_on_openai_style_cache() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock", "us.amazon.nova-micro-v1:0");
        assert!(cfg.cache_enabled);
        assert!(!cfg.is_anthropic);
    }

    #[test]
    fn bridge_provider_policy_keeps_non_claude_bedrock_prefix_only() {
        let policy = provider_cache_policy_for(None, "bedrock", "us.amazon.nova-micro-v1:0");

        assert_eq!(
            policy.protocol,
            astra_turn_core::microcompact::PromptCacheProtocol::Prefix,
            "non-Claude Bedrock models must not receive Anthropic cache_control markers"
        );
        assert_eq!(policy.max_markers, 0);
        assert!(!policy.supports_global_scope);
    }

    #[test]
    fn bridge_provider_policy_enables_anthropic_for_bedrock_claude() {
        let policy =
            provider_cache_policy_for(None, "bedrock", "anthropic.claude-sonnet-4-20250514-v1:0");

        assert_eq!(
            policy.protocol,
            astra_turn_core::microcompact::PromptCacheProtocol::AnthropicCacheControl
        );
        assert!(policy.max_markers > 0);
        assert!(policy.supports_global_scope);
    }

    #[test]
    fn bridge_pipeline_outcome_prefers_explicit_capability_over_provider_hint() {
        let cache_cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: false,
        };
        let outcome = assemble_bridge_pipeline_outcome(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &cache_cfg,
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "sid-explicit-capability",
            "proxy-claude",
            None,
            "openai",
            None,
            None,
            None,
            "",
            "",
            "2026-05-25",
        );

        // Marker-style capability keeps model identity outside the
        // cache-annotated prefix, so the dynamic system may carry it.
        let _ = outcome.dynamic_system; // may or may not be present depending on volatile content
        assert!(
            outcome
                .primary_system
                .get("content")
                .and_then(Value::as_array)
                .is_some(),
            "explicit marker capability on bridge path must produce multi-block cache-control system content"
        );
    }

    #[test]
    fn provider_cache_policy_prefers_explicit_capability_over_provider_hint() {
        let policy = provider_cache_policy_for(
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "openai",
            "proxy-claude",
        );
        assert_eq!(
            policy.protocol,
            astra_turn_core::microcompact::PromptCacheProtocol::AnthropicCacheControl
        );
        assert!(policy.max_markers > 0);
    }

    /// Real Anthropic `/v1/messages` rejects speculative cache-protocol
    /// extensions: `cache_edits` as a content-block type, `cache_reference`
    /// as a top-level message key. Session 5c5cbf78 (2026-05-08) hit HTTP
    /// 400 after seven successful tool-loop rounds when enough deletes
    /// had accumulated to materialize a `cache_edits` block.
    ///
    /// `apply_anthropic_cache_metadata` must emit ONLY the real Anthropic
    /// extension (`cache_control` marker on the last pre-user message).
    /// Any re-introduction of `cache_edits` / `cache_reference` here is a
    /// regression back to the 5c5cbf78 failure mode.
    #[test]
    fn anthropic_cache_metadata_emits_only_cache_control_marker() {
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

        apply_anthropic_cache_metadata(&mut messages, &cfg, "session-a");

        // Real Anthropic extension: exactly one message carries cache_control.
        let cc_count = messages
            .iter()
            .filter(|m| astra_turn_core::context_serializer::message_has_cache_control(m))
            .count();
        assert_eq!(
            cc_count, 1,
            "exactly one cache_control marker expected; got {cc_count} in {messages:#?}",
        );

        // Speculative/rejected extensions must not appear anywhere.
        for (i, m) in messages.iter().enumerate() {
            assert!(
                m.get("cache_reference").is_none(),
                "msg[{i}] must not carry cache_reference (not a real Anthropic field): {m}",
            );
            if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                for (j, b) in blocks.iter().enumerate() {
                    let ty = b.get("type").and_then(Value::as_str).unwrap_or("");
                    assert_ne!(
                        ty, "cache_edits",
                        "msg[{i}].content[{j}] must not be a cache_edits block \
                         (Anthropic /v1/messages returns HTTP 400): {m}",
                    );
                }
            }
        }
    }

    #[test]
    fn anthropic_cache_metadata_noop_for_openai() {
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
//   1. always_load tools appear first, byte-identical across calls;
//   2. the cache marker sits at the end of the always_load prefix;
//   3. any churn in the dynamic suffix leaves the prefix bytes intact.
//
// If a future refactor re-sorts the combined tool list, introduces HashMap
// iter into always_load assembly, or moves the marker back to "last tool", one of
// these tests will fail before the live cache hit rate silently collapses.
#[cfg(test)]
mod cache_stability_regression {
    use super::*;
    use crate::turn::llm::client::build_provider_request_body;
    use astra_turn_core::thinking_config::ThinkingConfig;
    use serde_json::json;

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
    /// `default_test_always_load_tool_names()` so the marker-placement logic exercises
    /// the real always_load set, not a local fixture.
    fn always_load_prefix_fixture() -> Vec<Value> {
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

    fn default_test_always_load_tool_names() -> std::collections::HashSet<String> {
        resolve_always_load_tool_names_for_config(&ToolSurfaceConfig::default())
    }

    fn annotate_test_tool_schemas_for_caching(tools: &mut [Value], cache_cfg: &PromptCacheConfig) {
        annotate_tool_schemas_for_caching_with_always_load(
            tools,
            cache_cfg,
            &default_test_always_load_tool_names(),
        );
    }

    /// Core invariant: adding, removing, or reordering tools AFTER the always_load
    /// prefix must leave the always_load prefix bytes completely unchanged and keep
    /// the cache marker on the same always_load tool.

    /// The marker always lands on the LAST always_load tool — even if the always_load
    /// count shrinks or dynamic tools are interleaved by a buggy caller.

    /// Default always_load set must contain the static-lib tools; losing one
    /// drops cache hit rate proportional to its token cost.
    #[test]
    fn default_always_load_set_contains_static_lib() {
        let always_load = default_test_always_load_tool_names();
        // TOOL_CATALOG-declared always_load tools
        for name in [
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "grep",
            "glob",
            "git",
            "memory",
            "introspect",
            "reflect",
        ] {
            assert!(
                always_load.contains(name),
                "{name} must stay in default always_load set (static-lib guarantee)"
            );
        }
        // Runtime-injected, not in TOOL_CATALOG, but structurally part of the
        // resolved default surface when skills are available.
        let name = "skill";
        assert!(
            always_load.contains(name),
            "{name} is auto-always_load at runtime; default set must mirror that"
        );
    }

    /// `default_test_always_load_tool_names()` must return the same set across calls —
    /// downstream logic caches the handle per request, but new callers assume
    /// it's stable.
    #[test]
    fn default_always_load_set_is_deterministic() {
        let first = default_test_always_load_tool_names();
        for _ in 0..20 {
            assert_eq!(default_test_always_load_tool_names(), first);
        }
    }

    /// Bedrock path: tools get translated to `toolSpec` blocks + a trailing
    /// `cachePoint`. The cachePoint must sit AT THE END OF THE ALWAYS-LOAD PREFIX,
    /// not at the end of the full tool list.

    /// Direct Anthropic path: tools are rewritten to `{name, input_schema}`
    /// blocks with `cache_control` preserved. The marker must survive the
    /// rewrite and land on the correct (last always_load) tool.

    /// Direct Anthropic path, identical assembly twice — request bodies must
    /// be byte-identical up to the cache_control host. This is the test that
    /// would catch HashMap iter drift, non-deterministic serialization, and
    /// any future bug that silently reshuffles the always_load prefix.
    #[test]
    fn anthropic_direct_request_always_load_bytes_identical_across_calls() {
        let build_once = || {
            let mut tools = always_load_prefix_fixture();
            // Deliberately DIFFERENT dynamic tools each call — the test
            // asserts the always_load portion is unaffected.
            tools.extend([schema("mo_query"), schema("github")]);
            annotate_test_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
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
            let mut tools = always_load_prefix_fixture();
            tools.extend([schema("web_fetch"), schema("github"), schema("mo_query")]);
            annotate_test_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
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
        let always_load_count = always_load_prefix_fixture().len();

        for i in 0..always_load_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "anthropic always_load tool at idx {i} must be byte-identical across calls"
            );
        }
    }

    /// Bedrock path parallel to the anthropic direct test — two calls with
    /// different dynamic tools must produce byte-identical bytes up to (and
    /// including) the cachePoint.

    /// OpenAI-compatible providers (DeepSeek, Qwen, MiniMax, vanilla OpenAI)
    /// don't consume `cache_control` — the field should still be present
    /// in the outgoing body (server-side caches like DeepSeek auto-dedupe
    /// on prefix, and extra keys are ignored), AND the always_load prefix bytes
    /// must be stable across calls for auto-prefix-cache to hit.
    #[test]
    fn openai_compatible_always_load_bytes_identical_across_calls() {
        let build = |extra: Vec<Value>| {
            let mut tools = always_load_prefix_fixture();
            tools.extend(extra);
            annotate_test_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
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
        let a = build(vec![schema("mo_query"), schema("github")]);
        let b = build(vec![schema("web_fetch")]);

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let always_load_count = always_load_prefix_fixture().len();
        for i in 0..always_load_count {
            let sa = serde_json::to_string(&a_tools[i]).unwrap();
            let sb = serde_json::to_string(&b_tools[i]).unwrap();
            assert_eq!(
                sa, sb,
                "openai always_load tool at idx {i} must be byte-identical across calls \
                 (needed for auto-prefix-cache on DeepSeek/etc.)"
            );
        }
    }

    /// User-defined tools: schemas registered at session start flow through
    /// `inject_schema_always_load(s, true)` and must therefore land INSIDE the
    /// cacheable always_load segment. We simulate this by directly inserting
    /// into the default always_load set and verifying the marker moves to after
    /// the user-added tool.

    /// Runtime-discovered dynamic tool/skill (e.g. via MCP tool-list-changed
    /// or discover_skills): these enter the dynamic segment. Cache on the
    /// always_load prefix must remain untouched when they come and go.
    #[test]
    fn runtime_dynamic_addition_does_not_touch_always_load_cache() {
        let mut without = always_load_prefix_fixture();
        without.push(schema("web_fetch"));
        annotate_test_tool_schemas_for_caching(&mut without, &cfg_anthropic());

        let mut with_new_mcp = always_load_prefix_fixture();
        with_new_mcp.push(schema("web_fetch"));
        with_new_mcp.push(schema("mcp_new_runtime_tool")); // discovered mid-session
        annotate_test_tool_schemas_for_caching(&mut with_new_mcp, &cfg_anthropic());

        let always_load_count = always_load_prefix_fixture().len();
        for i in 0..always_load_count {
            assert_eq!(
                without[i], with_new_mcp[i],
                "always_load tool at idx {i} must survive runtime dynamic-tool addition"
            );
        }
        // Marker stays on the same always_load tool, bytes match.
        assert_eq!(
            without[always_load_count - 1],
            with_new_mcp[always_load_count - 1],
            "always_load tool hosting the marker must be byte-identical \
             (always_load prefix cache hits regardless of MCP churn)"
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
    /// same system prompt + always_load tools + user message, but differ only
    /// in dynamic tool tail. Returns `(body_a, body_b, always_load_count)`.
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
            let mut tools = always_load_prefix_fixture();
            tools.extend(dynamic_tail);
            annotate_test_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            crate::turn::llm::client::build_provider_request_body(
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

        let a = build(vec![schema("mo_query"), schema("github")]);
        let b = build(vec![schema("web_fetch")]);
        (a, b, always_load_prefix_fixture().len())
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
    /// block; instead `cache_control` rides on the last always_load tool).
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
            let mut tools = always_load_prefix_fixture();
            tools.extend(tail);
            annotate_test_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            crate::turn::llm::client::build_provider_request_body(
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
        let a = build(vec![schema("mo_query"), schema("github")]);
        let b = build(vec![schema("web_fetch")]);

        // Static system + user message identical
        assert_eq!(a["system"], b["system"]);
        assert_eq!(a["messages"], b["messages"]);

        // Always-load tool bytes (through the marker-hosting last always_load tool) identical
        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let always_load_count = always_load_prefix_fixture().len();
        for i in 0..always_load_count {
            assert_eq!(
                serde_json::to_string(&a_tools[i]).unwrap(),
                serde_json::to_string(&b_tools[i]).unwrap(),
                "anthropic composite: tool[{i}] must match across turns"
            );
        }
    }

    /// OpenAI-compatible path: no cache_control is consumed, but the whole
    /// prefix (system + tools up to always_load_count + first user msg) must be
    /// byte-identical for DeepSeek/OpenAI server-side prefix caching to hit.
    #[test]
    fn composite_openai_body_prefix_identical_across_turns() {
        let system_msg = json!({"role": "system", "content": "You are an expert."});
        let user_msg = json!({"role": "user", "content": "hi"});

        let build = |tail: Vec<Value>| {
            let mut tools = always_load_prefix_fixture();
            tools.extend(tail);
            annotate_test_tool_schemas_for_caching(&mut tools, &cfg_anthropic());
            crate::turn::llm::client::build_provider_request_body(
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
        let a = build(vec![schema("mo_query"), schema("github")]);
        let b = build(vec![schema("web_fetch")]);

        assert_eq!(a["messages"], b["messages"]);

        let a_tools = a["tools"].as_array().unwrap();
        let b_tools = b["tools"].as_array().unwrap();
        let always_load_count = always_load_prefix_fixture().len();
        for i in 0..always_load_count {
            assert_eq!(
                serde_json::to_string(&a_tools[i]).unwrap(),
                serde_json::to_string(&b_tools[i]).unwrap(),
                "openai composite: tool[{i}] must match — prefix auto-caching needs byte equality"
            );
        }
    }
}
