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
//! The request builder allocates two stable system breakpoints, one tool-prefix
//! breakpoint, and one conversation breakpoint. Runtime facts stay outside the
//! stable system prefix; provider annotations are applied at the request boundary.
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
//! | `Session` | Stable within a session (version, cwd, date, user, branch) | Middle, before the breakpoint |
//! | `None` | Per-turn/non-cacheable runtime facts (memory, retrieval, runtime policy) | After the breakpoint |
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
//! ## Provider Strategy Resolution
//!
//! [`provider_cache_policy_for`] determines the caching strategy from three sources in
//! priority order:
//!
//! 1. **Explicit deployment metadata** (`CacheCapability`)
//! 2. **Provider transport baseline** when metadata is absent (never a model-name guess)
//! 3. **Environment enablement** (`ASTRA_TEST_PROMPT_CACHE_DISABLED`) controls whether
//!    admitted annotations are emitted; it does not reclassify the protocol
//!
//! ## Public Interface
//!
//! The primary entry points consumed by callers:
//!
//! | Function | Consumer | Purpose |
//! |---|---|---|
//! | [`super::llm::context::assemble_context_pipeline`] | Shared LLM caller | Assemble prompt, history, and tool schemas |
//! | [`annotate_tool_schemas_for_caching_with_always_load`] | Request build | Add `cache_control` to tool definitions |
//! | [`apply_anthropic_cache_metadata`] | Anthropic adapter | Insert the final conversation cache breakpoint |
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

use serde_json::{Value, json};

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

pub(crate) fn model_identity_prompt_text(model_id: &str) -> String {
    format!("Model: {model_id}")
}

pub(crate) fn model_identity_prompt_section(model_id: &str) -> prompts::PromptSection {
    prompts::PromptSection::dynamic(
        model_identity_prompt_text(model_id),
        prompts::PromptTokenBucket::Environment,
    )
}

impl PromptCacheConfig {
    /// Latch config from environment and provider transport. Call once at
    /// session start.
    pub fn latch(provider: &str) -> Self {
        Self::from_cache_capability(None, provider)
    }

    pub fn from_cache_capability(
        cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
        provider: &str,
    ) -> Self {
        let cache_enabled = !std::env::var("ASTRA_TEST_PROMPT_CACHE_DISABLED")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let capability =
            astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider(
                cache_capability,
                provider,
            );
        let provider_strategy =
            astra_turn_core::microcompact::ProviderCacheStrategy::from_cache_capability(capability);
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
// Static/dynamic prompt boundary:
// - Global+Session sections form the stable provider-cache prefix.
// - Per-turn volatile content (environment_volatile, memoria recall, …) is
//   bound into RuntimeVolatile post-cache-marker so it re-sends each turn
//   without invalidating the cached prefix.

/// Resolve the context-pipeline cache policy from the same provider+model
/// classification used by [`PromptCacheConfig::latch`].
///
/// This matters for multiplexed providers like Bedrock: Claude models support
/// Anthropic-style cache markers (translated to Bedrock cache points), while
/// Nova/Titan models must remain prefix-only.
pub(crate) fn provider_cache_policy_for(
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    provider: &str,
) -> ProviderCachePolicy {
    let capability = astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider(
        cache_capability,
        provider,
    );
    let strategy = ProviderCacheStrategy::from_cache_capability(capability);
    if strategy.prompt_cache_protocol == PromptCacheProtocol::AnthropicCacheControl {
        ProviderCachePolicy::anthropic()
    } else {
        ProviderCachePolicy::openai_compatible()
    }
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
/// clears stale top-level markers, then delegates to the pure
/// [`astra_turn_core::context_serializer::annotate_always_load_tool_schema`] for
/// the actual wire mutation. The core primitive owns fallback observability and
/// all provider-specific cache annotation logic has exactly one implementation.
pub(crate) fn annotate_tool_schemas_for_caching_with_always_load(
    tools: &mut [Value],
    cache_cfg: &PromptCacheConfig,
    always_load_names: &std::collections::HashSet<String>,
) {
    clear_tool_cache_controls(tools);
    if !cache_cfg.should_annotate() || tools.is_empty() {
        return;
    }
    astra_turn_core::context_serializer::annotate_always_load_tool_schema(tools, always_load_names);
}

fn clear_tool_cache_controls(tools: &mut [Value]) {
    for tool in tools {
        if let Some(object) = tool.as_object_mut() {
            object.remove("cache_control");
        }
    }
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
    let schemas = astra_tools::schemas::all_tool_schemas();
    crate::tool_registry::surface::ToolSurface::build(schemas, cfg, &[])
        .always_load_names()
        .into_iter()
        .collect()
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
/// prompt-cache pipeline, including `ASTRA_TEST_PROMPT_CACHE_DISABLED`.
/// Sibling test modules share the same lock — otherwise
/// two independent mutexes race to the same `std::env::set_var` and a
/// panic in one poisons the other's tests. Recover from poison on lock
/// acquire; test panics carry their own failure and should not cascade.
#[cfg(test)]
pub(crate) static CACHE_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn default_test_always_load_tool_names() -> std::collections::HashSet<String> {
    resolve_always_load_tool_names_for_config(&ToolSurfaceConfig::default())
}

#[cfg(test)]
fn annotate_test_tool_schemas_for_caching(tools: &mut [Value], cache_cfg: &PromptCacheConfig) {
    annotate_tool_schemas_for_caching_with_always_load(
        tools,
        cache_cfg,
        &default_test_always_load_tool_names(),
    );
}

#[cfg(test)]
mod tests {
    use super::CACHE_ENV_MUTEX;
    use super::*;

    /// Safe wrapper for `std::env::remove_var` in single-threaded tests.
    fn remove_test_env(key: &str) {
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    fn prompt_cache_latch_uses_provider_transport_only() {
        let openai_proxy = PromptCacheConfig::latch("openai");
        assert!(!openai_proxy.is_anthropic);

        let anthropic_provider = PromptCacheConfig::latch("anthropic");
        assert!(anthropic_provider.is_anthropic);
    }

    #[test]
    fn prompt_cache_config_prefers_explicit_marker_capability() {
        let cfg = PromptCacheConfig::from_cache_capability(
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                volatile_delivery: astra_turn_core::cache_placement::VolatileDeliveryPolicy::All,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "openai",
        );
        assert!(cfg.is_anthropic);
    }

    // ── always_load-tool audit ────────────────────────────────────────────────
    //
    // The tool-schema cache marker belongs at the end of the always_load/static
    // prefix, not at the end of the whole catalog. Deferred tool *schemas* may
    // become callable for a turn, but they must not silently enlarge that
    // repeated schema prefix. Their compact name manifest is separate
    // capability-epoch metadata and is tested below.
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
            "agent_fanout",
            "inspect_work_plan",
            "propose_work_plan",
            "inspect_work_criteria",
            "propose_work_criteria",
            "lsp",
            "web_search",
            "web_fetch",
            "web_search",
            "session",
            "mo_query",
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
            pinned_tools: vec!["web_search".into(), "not_a_real_tool".into()],
        };
        let always_load = resolve_always_load_tool_names_for_config(&cfg);

        assert!(
            always_load.contains("web_search"),
            "config-always_load web_search must be part of the cache static prefix"
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
            !always_load.contains("web_fetch"),
            "deferred web_fetch must not become cache always_load without an explicit TOML always_load entry"
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
            json!({"type": "function", "function": {"name": "introspect"}}), // always_load
            json!({"type": "function", "function": {"name": "agent"}}), // always_load name in dynamic tail
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
            json!({"type": "function", "function": {"name": "agent"}}),
        ];
        let prefix_len = tools.len();
        tools.push(json!({"type": "function", "function": {"name": "web_fetch"}}));
        tools.push(json!({"type": "function", "function": {"name": "worktree"}}));

        let always_load = default_test_always_load_tool_names();
        assert!(always_load.contains("bash"));
        assert!(always_load.contains("read_file"));
        assert!(always_load.contains("agent"));

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

    #[test]
    fn declared_capability_enables_anthropic_style_cache_for_bedrock() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::from_cache_capability(
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::BedrockCachePoint,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                volatile_delivery: astra_turn_core::cache_placement::VolatileDeliveryPolicy::All,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "bedrock",
        );
        assert!(cfg.cache_enabled);
        assert!(cfg.is_anthropic);
    }

    #[test]
    fn undeclared_bedrock_stays_on_unmarked_cache_path() {
        let _lock = astra_core::sync_poison::recover_mutex_lock(&CACHE_ENV_MUTEX);
        remove_test_env("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        let cfg = PromptCacheConfig::latch("bedrock");
        assert!(cfg.cache_enabled);
        assert!(!cfg.is_anthropic);
    }

    #[test]
    fn ephemeral_provider_policy_keeps_non_claude_bedrock_prefix_only() {
        let policy = provider_cache_policy_for(None, "bedrock");

        assert_eq!(
            policy.protocol,
            astra_turn_core::microcompact::PromptCacheProtocol::Prefix,
            "non-Claude Bedrock models must not receive Anthropic cache_control markers"
        );
        assert_eq!(policy.max_markers, 0);
        assert!(!policy.supports_global_scope);
    }

    #[test]
    fn ephemeral_provider_policy_honors_declared_bedrock_cachepoint() {
        let policy = provider_cache_policy_for(
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::BedrockCachePoint,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                volatile_delivery: astra_turn_core::cache_placement::VolatileDeliveryPolicy::All,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "bedrock",
        );

        assert_eq!(
            policy.protocol,
            astra_turn_core::microcompact::PromptCacheProtocol::AnthropicCacheControl
        );
        assert!(policy.max_markers > 0);
        assert!(policy.supports_global_scope);
    }

    #[test]
    fn provider_cache_policy_prefers_explicit_capability_over_provider_hint() {
        let policy = provider_cache_policy_for(
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                volatile_delivery: astra_turn_core::cache_placement::VolatileDeliveryPolicy::All,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "openai",
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
                "content": "[tool result cleared — re-run if needed]"
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
                "content": "[tool result cleared — re-run if needed]"
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
        crate::tool_registry::surface::default_always_load_names()
            .iter()
            .map(|name| schema(name))
            .collect()
    }

    fn cfg_anthropic() -> PromptCacheConfig {
        PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        }
    }

    /// Core invariant: adding, removing, or reordering tools AFTER the always_load
    /// prefix must leave the always_load prefix bytes completely unchanged and keep
    /// the cache marker on the same always_load tool.

    /// The marker always lands on the LAST always_load tool — even if the always_load
    /// count shrinks or dynamic tools are interleaved by a buggy caller.

    /// The default cached prefix keeps ordinary first-request primitives and
    /// hot Work transitions and compact live observation while
    /// excluding optional workflows whose full schemas load on demand.
    #[test]
    fn default_always_load_set_contains_primitives_not_optional_workflows() {
        let always_load = default_test_always_load_tool_names();
        for name in [
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "grep",
            "tool_search",
            "introspect",
            "agent",
            "start_work",
            "run_next_work_item",
            "settle_work_item",
        ] {
            assert!(
                always_load.contains(name),
                "{name} must stay in the default first-request prefix"
            );
        }
        assert!(
            !always_load.contains("glob"),
            "specialized glob navigation must remain deferred from the default prefix"
        );
        for name in [
            "skill",
            "agent_fanout",
            "worktree",
            "memory",
            "reflect",
            "notify",
        ] {
            assert!(
                !always_load.contains(name),
                "{name} must load on demand instead of extending the default cache prefix"
            );
        }
    }

    #[test]
    fn deferred_invocation_carrier_can_close_the_stable_tool_prefix() {
        let carrier =
            astra_turn_core::tool::deferred_activation::deferred_tool_invocation_carrier_schema();
        let carrier_name =
            astra_turn_core::tool::deferred_activation::DEFERRED_TOOL_INVOCATION_CARRIER;
        let mut tools = vec![
            schema("bash"),
            schema("tool_search"),
            carrier,
            schema("web_search"),
        ];
        let mut always_load = default_test_always_load_tool_names();
        always_load.insert(carrier_name.to_string());

        annotate_tool_schemas_for_caching_with_always_load(
            &mut tools,
            &cfg_anthropic(),
            &always_load,
        );

        assert_eq!(
            tools[2]["cache_control"],
            astra_turn_core::context_serializer::anthropic_ephemeral_cache_control(),
            "the stable carrier, not a dynamic deferred target, owns the breakpoint"
        );
        assert!(tools[3].get("cache_control").is_none());
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
            tools.extend([schema("mo_query"), schema("web_search")]);
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
            tools.extend([
                schema("web_fetch"),
                schema("web_search"),
                schema("mo_query"),
            ]);
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
        let a = build(vec![schema("mo_query"), schema("web_search")]);
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

        let a = build(vec![schema("mo_query"), schema("web_search")]);
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
        let a = build(vec![schema("mo_query"), schema("web_search")]);
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
        let a = build(vec![schema("mo_query"), schema("web_search")]);
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
