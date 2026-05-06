//! Context pipeline adapter — bridges AgenticLoopState to ContextSources.
//!
//! This module is the SOLE place where runtime-specific state is translated
//! into the pipeline's typed inputs. The pipeline never sees AgenticLoopState
//! directly; it only sees the clean ContextSources view.
//!
//! The adapter extracts dynamic prompt fragments from the Host + State,
//! packages them into ExternalSources, and returns a TurnInput ready for
//! `PipelineSession::run_turn_adaptive()`.

use serde_json::Value;

use astra_turn_core::context_sources::{
    EdgeProfile, ExternalSources, MemoryEntry, SessionContext, TurnState,
};
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::pipeline_config::ProviderCachePolicy;
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::token_accounting::TokenAccounting;

use super::agentic_loop_host::AgenticLoopState;

/// Build ExternalSources from the Host's edge_profile + state.
///
/// Single extraction point for all dynamic prompt fragments.
pub(crate) fn build_external_sources(
    edge_profile: &serde_json::Map<String, Value>,
    state: &AgenticLoopState,
    user_content: &str,
    tool_names: &[&str],
    selection_confidence: f64,
    plan_resume_hint: Option<&str>,
) -> ExternalSources {
    // silence: the binder consumes `user_content` via TurnState.last_user_message;
    // here it's only used for future task-type detection if needed.
    let _ = user_content;
    // 1. Self-model (tool-dependent capabilities)
    let self_model_text = if tool_names.is_empty() {
        None
    } else {
        Some(crate::prompts::self_model_section(tool_names))
    };

    // 2. Tool-conditional guidance
    let profile_for_tc = edge_profile
        .get("cwd")
        .and_then(Value::as_str)
        .map(|cwd| format!("cwd: {cwd}"))
        .unwrap_or_default();
    let tool_conditional = if tool_names.is_empty() {
        None
    } else {
        Some(crate::prompts::tool_conditional_section(
            tool_names,
            &profile_for_tc,
            selection_confidence,
        ))
    };

    // 3. Profile description
    let mut profile_parts = Vec::new();
    if let Some(cwd) = edge_profile.get("cwd").and_then(Value::as_str) {
        profile_parts.push(format!("cwd: {cwd}"));
    }
    if let Some(branch) = edge_profile.get("git_branch").and_then(Value::as_str) {
        profile_parts.push(format!("git_branch: {branch}"));
    }
    let profile_desc = if profile_parts.is_empty() {
        None
    } else {
        Some(format!(
            "\n\n# Project Profile\n{}",
            profile_parts.join("\n")
        ))
    };

    // 4. Effort/agent_type hint
    let effort_hint = {
        let mut hint = String::new();
        if let Some(ref effort) = state.skills.effort {
            hint.push_str(&format!(
                "\n\n## Effort Level\nThe active skill requests effort level: **{effort}**. Adjust thoroughness accordingly.",
            ));
        }
        if let Some(ref agent_type) = state.skills.agent_type {
            hint.push_str(&format!(
                "\n\n## Agent Type\nYou are acting as a **{agent_type}** agent for this skill.",
            ));
        }
        if hint.is_empty() { None } else { Some(hint) }
    };

    // 5. Learned context
    let learned_context = edge_profile
        .get("learned_context_hint")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\n## Learned Runtime Context\n{s}"));

    // 6. System override (delegation)
    let system_override = edge_profile
        .get("system_prompt_override")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\n{s}"));

    // 7. Plan context
    let plan_context = plan_resume_hint.filter(|s| !s.is_empty()).map(String::from);

    // 8. Tool round guidance
    let tool_cfg = astra_config::runtime_config::RuntimeConfig::load().tool_selection;
    let (tool_guidance_text, _signals) = crate::prompts::tool_round_guidance_trace_with(
        &state.messages,
        state.llm_rounds_completed,
        tool_cfg.effective_round_budget_warning(),
        tool_cfg.effective_round_budget_limit(),
    );
    let tool_guidance = if tool_guidance_text.is_empty() {
        None
    } else {
        Some(tool_guidance_text)
    };

    // 9. Active skill names as hint
    let _active_skill_names: Vec<&str> = edge_profile
        .get("active_skills")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let memory_entries = build_memory_entries_from_edge_profile(edge_profile);

    let spill_backend: Option<std::sync::Arc<dyn astra_turn_core::spill_backend::SpillBackend>> =
        edge_profile
            .get("spill_dir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                std::sync::Arc::new(astra_turn_core::spill_backend::FileSystemSpillBackend::new(
                    s,
                ))
                    as std::sync::Arc<dyn astra_turn_core::spill_backend::SpillBackend>
            });

    // Skill listing — per-turn ranked shortlist of available skills, based
    // on the current user message. Previously injected post-pipeline as a
    // `role: system` message and folded into `body.system[]` via
    // `consolidate_system_messages`. Routing it through the pipeline's
    // volatile lane keeps the same cache behaviour (it lands in
    // RuntimeVolatile / None scope) but makes the pipeline the single
    // owner of system-block content — simpler to reason about where each
    // byte of the system prompt comes from.
    let skill_listing_extra = state.skills.listing_message.as_ref().and_then(|listing| {
        listing
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                crate::prompts::PromptSection::dynamic(
                    s.to_string(),
                    crate::prompts::PromptTokenBucket::Environment,
                )
            })
    });

    ExternalSources {
        memory_entries,
        spill_dir: None,
        spill_backend,
        self_model_text,
        tool_conditional,
        profile_desc,
        effort_hint,
        learned_context,
        system_override,
        plan_context,
        tool_guidance,
        // Adapter path (ServerAgenticLoopHost) doesn't use the bridge's
        // stable-lane escape hatch — session-stable signals have typed
        // fields above.
        extra_stable_sections: Vec::new(),
        // Volatile lane: the per-turn skill-listing shortlist. Turn-varying
        // by design (content depends on current user message). Kept in
        // None scope via RuntimeVolatile so it doesn't invalidate the
        // cached prefix.
        extra_dynamic_sections: skill_listing_extra.into_iter().collect(),
    }
}

fn build_memory_entries_from_edge_profile(
    edge_profile: &serde_json::Map<String, Value>,
) -> Vec<MemoryEntry> {
    if let Some(entries) = edge_profile.get("memory_entries").and_then(Value::as_array) {
        let total = entries.len();
        return entries
            .iter()
            .enumerate()
            .filter_map(|(idx, value)| memory_entry_from_value(value, total, idx))
            .collect();
    }

    edge_profile
        .get("memory_section")
        .and_then(Value::as_str)
        .map(memory_entries_from_section)
        .unwrap_or_default()
}

fn memory_entry_from_value(value: &Value, total: usize, idx: usize) -> Option<MemoryEntry> {
    match value {
        Value::String(content) => {
            let content = content.trim();
            if content.is_empty() {
                return None;
            }
            Some(
                MemoryEntry::scored(content, default_memory_relevance_from_total(total, idx))
                    .with_source("edge_profile.memory_entries"),
            )
        }
        Value::Object(obj) => {
            let content = obj.get("content").and_then(Value::as_str)?.trim();
            if content.is_empty() {
                return None;
            }
            let mut entry = MemoryEntry::scored(
                content,
                obj.get("relevance_score")
                    .and_then(Value::as_f64)
                    .unwrap_or_else(|| default_memory_relevance_from_total(total, idx)),
            )
            .with_source(
                obj.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("edge_profile.memory_entries"),
            );
            if let Some(tokens) = obj.get("token_estimate").and_then(Value::as_u64) {
                entry = entry.with_token_estimate(tokens.min(u64::from(u32::MAX)) as u32);
            }
            if let Some(turn) = obj.get("freshness_turn").and_then(Value::as_u64) {
                entry = entry.with_freshness_turn(turn.min(u64::from(u32::MAX)) as u32);
            }
            Some(entry)
        }
        _ => None,
    }
}

fn memory_entries_from_section(section: &str) -> Vec<MemoryEntry> {
    let lines: Vec<&str> = section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "## User Memories")
        .collect();
    let total = lines.len();
    lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            MemoryEntry::scored(line, default_memory_relevance_from_total(total, idx))
                .with_source("edge_profile.memory_section")
        })
        .collect()
}

fn default_memory_relevance_from_total(total: usize, idx: usize) -> f64 {
    total.saturating_sub(idx) as f64
}

/// Build TurnState from AgenticLoopState.
pub(crate) fn build_turn_state(state: &AgenticLoopState, user_content: &str) -> TurnState {
    TurnState {
        messages: state.messages.clone(),
        tool_results: vec![],
        tokens: TokenAccounting::from_fields(
            state.total_prompt,
            state.total_cache_read,
            state.total_cache_creation,
            state.total_completion,
        ),
        active_skills: vec![],
        recent_file_reads: std::collections::HashMap::new(),
        // Pull the real per-turn budget from the host state instead of a 20
        // hardcode. Planner uses this to decide when to escalate compaction.
        remaining_turns: state.remaining_turns as u32,
        turn_index: state.llm_rounds_completed,
        // RecoveryState lives on the pipeline session; feeding it freshly
        // here each turn is correct — `run_turn_adaptive` merges it with
        // the session's persisted counters before planning.
        recovery: RecoveryState::default(),
        last_user_message: user_content.to_string(),
    }
}

/// Build SessionContext from Host state.
///
/// The `provider` argument selects the cache policy: Anthropic-family (direct
/// Anthropic, Bedrock Claude) gets `cache_control` semantics; everyone else
/// falls back to prefix-only caching. Pulling this from the actual provider
/// (vs. a hardcoded `anthropic()`) is what prevents the pipeline from emitting
/// unsupported markers to OpenAI-compatible endpoints.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_session_context(
    session_id: &str,
    run_id: Option<&str>,
    model_name: &str,
    max_input_tokens: u64,
    edge_profile: &serde_json::Map<String, Value>,
    provider: &str,
    project_context: Option<&str>,
) -> SessionContext {
    let provider_policy = provider_policy_for(provider, model_name);
    SessionContext {
        session_id: session_id.to_string(),
        run_id: run_id.unwrap_or_default().to_string(),
        model_id: model_name.to_string(),
        model_limit: u32::try_from(max_input_tokens).unwrap_or(u32::MAX),
        provider_policy,
        provider_strategy: ProviderCacheStrategy::default(),
        // Cross-session project context (summaries of prior sessions on
        // this repo) is session-stable — feeding it through the pipeline's
        // `ProjectContext` section puts it in CacheScope::Session behind the
        // 2nd marker instead of runtime-injecting it into `state.messages`
        // AFTER the marker.
        project_context: project_context.unwrap_or("").to_string(),
        edge_profile: EdgeProfile {
            cwd: edge_profile
                .get("cwd")
                .and_then(Value::as_str)
                .map(String::from),
            git_branch: edge_profile
                .get("git_branch")
                .and_then(Value::as_str)
                .map(String::from),
            ..Default::default()
        },
        self_model: None,
    }
}

/// Map a provider name to its cache policy.
///
/// Anthropic-family providers use `cache_control` markers; everyone else gets
/// prefix-only caching. Bedrock is provider-multiplexed, so it must opt in only
/// for Claude model IDs rather than all `provider=bedrock` traffic.
fn provider_policy_for(provider: &str, model_name: &str) -> ProviderCachePolicy {
    match provider {
        "anthropic" => ProviderCachePolicy::anthropic(),
        "bedrock" if is_bedrock_claude_model(model_name) => ProviderCachePolicy::anthropic(),
        _ => ProviderCachePolicy::openai_compatible(),
    }
}

fn is_bedrock_claude_model(model_name: &str) -> bool {
    let model = model_name.to_ascii_lowercase();
    model.contains("anthropic.claude")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop_host::tests::make_state;

    #[test]
    fn turn_state_uses_real_remaining_turns_not_hardcoded_20() {
        // The adapter used to hardcode remaining_turns=20 regardless of the
        // actual host budget. That broke the planner's "about to exhaust
        // budget" heuristic and fed wrong signals into compaction escalation.
        let mut state = make_state();
        state.remaining_turns = 7;
        let ts = build_turn_state(&state, "hi");
        assert_eq!(
            ts.remaining_turns, 7,
            "adapter must pass host's remaining_turns through verbatim, \
             not hardcode a default"
        );

        state.remaining_turns = 0;
        let ts = build_turn_state(&state, "hi");
        assert_eq!(ts.remaining_turns, 0, "exhausted budget must surface as 0");
    }

    #[test]
    fn turn_state_tokens_reflect_host_state() {
        let mut state = make_state();
        state.total_prompt = 1000;
        state.total_cache_read = 800;
        state.total_cache_creation = 200;
        state.total_completion = 50;
        let ts = build_turn_state(&state, "user message");
        // TokenAccounting splits prompt_tokens across cache buckets — what
        // matters is the adapter passed the right inputs. Verify via total.
        assert!(
            ts.tokens.total_input() > 0,
            "token accounting must be populated from host fields"
        );
    }

    #[test]
    fn session_context_picks_anthropic_policy_for_anthropic_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "claude-sonnet",
            200_000,
            &ep,
            "anthropic",
            None,
        );
        // anthropic policy supports cache_control markers (max_markers > 0).
        assert!(
            ctx.provider_policy.max_markers > 0,
            "anthropic provider must get a policy that supports cache markers"
        );
        assert!(ctx.provider_policy.supports_global_scope);
    }

    #[test]
    fn session_context_picks_anthropic_policy_for_bedrock_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "anthropic.claude-sonnet-4-20250514-v1:0",
            200_000,
            &ep,
            "bedrock",
            None,
        );
        // Bedrock Claude translates cache_control → cachePoint downstream,
        // so the pipeline still emits Anthropic-style markers.
        assert!(
            ctx.provider_policy.max_markers > 0,
            "bedrock must use anthropic policy — Bedrock Converse translates cache_control \
             to cachePoint transparently"
        );
    }

    #[test]
    fn session_context_uses_prefix_policy_for_non_claude_bedrock_model() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "amazon.titan-text-express-v1",
            200_000,
            &ep,
            "bedrock",
            None,
        );
        assert_eq!(
            ctx.provider_policy.max_markers, 0,
            "non-Claude Bedrock models must not receive Anthropic cache_control markers"
        );
        assert!(!ctx.provider_policy.supports_global_scope);
    }

    #[test]
    fn session_context_saturates_oversized_model_limit() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", u64::MAX, &ep, "openai", None);
        assert_eq!(ctx.model_limit, u32::MAX);
    }

    #[test]
    fn session_context_picks_openai_policy_for_openai_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", 128_000, &ep, "openai", None);
        // OpenAI uses prefix-only caching — emitting cache_control is a no-op
        // at best and (for some proxies) a 400 Bad Request at worst.
        assert_eq!(
            ctx.provider_policy.max_markers, 0,
            "openai provider must get prefix-only policy — no cache_control markers"
        );
        assert!(!ctx.provider_policy.supports_global_scope);
    }

    #[test]
    fn session_context_defaults_unknown_provider_to_openai_policy() {
        // New providers (DeepSeek, Qwen, MiniMax) come through "openai"
        // protocol; fresh names we don't yet recognize must err on the side
        // of "no markers" — emitting cache_control to an unknown backend
        // risks protocol errors, whereas omitting it just loses caching.
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "unknown-model-v9",
            100_000,
            &ep,
            "unknown",
            None,
        );
        assert_eq!(ctx.provider_policy.max_markers, 0);
    }

    #[test]
    fn session_context_passes_project_context_through() {
        // The Cross-Session Project Context (summaries of prior sessions on
        // this repo) must land in `SessionContext.project_context` so the
        // pipeline's `ProjectContext` section picks it up. Previously this
        // content was runtime-injected into `state.messages` AFTER the cache
        // marker — every turn re-sent it as cache_creation. Now it sits in
        // `CacheScope::Session` behind the 2nd marker and becomes cacheable.
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "claude-sonnet",
            200_000,
            &ep,
            "anthropic",
            Some(
                "1. [active] (2026-05-06, 22 turns, branch: main)\n2. [active] (2026-05-05, 4 turns)",
            ),
        );
        assert!(
            ctx.project_context.contains("22 turns"),
            "project_context must flow through to SessionContext: {}",
            ctx.project_context
        );
    }

    #[test]
    fn session_context_defaults_project_context_to_empty_when_absent() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", 128_000, &ep, "openai", None);
        assert!(ctx.project_context.is_empty());
    }

    #[test]
    fn external_sources_carries_prefetched_memory_from_edge_profile() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\n- prefers Rust\n- hates emojis".into()),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);
        assert!(
            !sources.memory_entries.is_empty(),
            "edge_profile.memory_section must flow into ExternalSources.memory_entries"
        );
        assert_eq!(sources.memory_entries.len(), 2);
        assert!(sources.memory_entries[0].content.contains("prefers Rust"));
        assert!(
            sources.memory_entries[0].relevance_score > sources.memory_entries[1].relevance_score,
            "memory_section line order should become production relevance"
        );
    }

    #[test]
    fn external_sources_prefers_structured_memory_entries() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\nfallback should be ignored".into()),
        );
        ep.insert(
            "memory_entries".into(),
            Value::Array(vec![
                serde_json::json!({
                    "content": "fresh structured memory",
                    "relevance_score": 0.9,
                    "source": "test",
                    "token_estimate": 7,
                    "freshness_turn": 3
                }),
                Value::String("ranked fallback string".into()),
            ]),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);

        assert_eq!(sources.memory_entries.len(), 2);
        assert_eq!(sources.memory_entries[0].content, "fresh structured memory");
        assert_eq!(sources.memory_entries[0].source.as_deref(), Some("test"));
        assert_eq!(sources.memory_entries[0].token_estimate, 7);
        assert_eq!(sources.memory_entries[0].freshness_turn, Some(3));
        assert!(
            sources.memory_entries[1]
                .content
                .contains("ranked fallback string")
        );
    }

    #[test]
    fn external_sources_empty_memory_when_edge_profile_has_none() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);
        assert!(sources.memory_entries.is_empty());
    }
}
