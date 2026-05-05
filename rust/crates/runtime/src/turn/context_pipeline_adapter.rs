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
    EdgeProfile, ExternalSources, SessionContext, TurnState,
};
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::pipeline_config::ProviderCachePolicy;
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::token_accounting::TokenAccounting;

use super::agentic_loop_host::AgenticLoopState;

/// Build ExternalSources from the Host's edge_profile + state.
///
/// This replaces the scattered inline logic in `build_system_messages_cached()`
/// with a single extraction point. Each field maps to one of the 15 dynamic
/// prompt fragments that the legacy path computed.
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
        Some(format!("\n\n# Project Profile\n{}", profile_parts.join("\n")))
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
    let plan_context = plan_resume_hint
        .filter(|s| !s.is_empty())
        .map(String::from);

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

    // Memory snippets come from `edge_profile.memory_section`, which the
    // CLI / legacy bridge populate via `prefetch_memories`. When this is
    // empty, the pipeline simply omits the `## Memoria Recall` block
    // from the runtime-identity section (binder.bind_memory joins with
    // `\n\n`, so an empty Vec is a no-op).
    //
    // Architectural note: the server loop host (`ServerAgenticLoopHost`)
    // does NOT currently prefetch memories directly — it relies on the CLI
    // to forward the pre-retrieved section through `edge_profile`.
    // Running the pipeline without a CLI in front (e.g., direct HTTP
    // `/chat/turn`) still goes through `InProcessChatTurnBridge`'s prefetch,
    // which flows back into `edge_profile`. A future refactor should pull
    // the prefetch into the pipeline host itself so memory is a first-class
    // pipeline input rather than plumbed through edge_profile.
    let memory_snippets = edge_profile
        .get("memory_section")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();

    ExternalSources {
        memory_snippets,
        spill_dir: None,
        self_model_text,
        tool_conditional,
        profile_desc,
        effort_hint,
        learned_context,
        system_override,
        plan_context,
        tool_guidance,
        // Adapter path doesn't use the escape hatch — all its signals have
        // typed fields. The bridge (task #30) feeds this via its own builder.
        extra_dynamic_sections: Vec::new(),
    }
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
pub(crate) fn build_session_context(
    session_id: &str,
    run_id: Option<&str>,
    model_name: &str,
    max_input_tokens: u64,
    edge_profile: &serde_json::Map<String, Value>,
    provider: &str,
) -> SessionContext {
    let provider_policy = provider_policy_for(provider);
    SessionContext {
        session_id: session_id.to_string(),
        run_id: run_id.unwrap_or_default().to_string(),
        model_id: model_name.to_string(),
        model_limit: max_input_tokens as u32,
        provider_policy,
        provider_strategy: ProviderCacheStrategy::default(),
        project_context: String::new(),
        edge_profile: EdgeProfile {
            cwd: edge_profile.get("cwd").and_then(Value::as_str).map(String::from),
            git_branch: edge_profile.get("git_branch").and_then(Value::as_str).map(String::from),
            ..Default::default()
        },
        self_model: None,
    }
}

/// Map a provider name to its cache policy.
///
/// Anthropic-family providers (direct Anthropic, Bedrock Claude models) use
/// `cache_control` markers; everyone else gets prefix-only caching. The
/// model name isn't used — Bedrock identifies itself via `provider=bedrock`
/// even when the model is Claude, and `build_provider_request_body` handles
/// the cache_control → cachePoint translation downstream.
fn provider_policy_for(provider: &str) -> ProviderCachePolicy {
    match provider {
        "anthropic" | "bedrock" => ProviderCachePolicy::anthropic(),
        _ => ProviderCachePolicy::openai_compatible(),
    }
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
        let ctx = build_session_context("sid", None, "claude-sonnet", 200_000, &ep, "anthropic");
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
    fn session_context_picks_openai_policy_for_openai_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", 128_000, &ep, "openai");
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
        let ctx = build_session_context("sid", None, "unknown-model-v9", 100_000, &ep, "unknown");
        assert_eq!(ctx.provider_policy.max_markers, 0);
    }

    #[test]
    fn external_sources_carries_prefetched_memory_from_edge_profile() {
        // Memory prefetch happens in the legacy bridge (InProcessChatTurnBridge)
        // before the request reaches the server loop host. When the CLI
        // injects the prefetch result into `edge_profile.memory_section`, the
        // adapter must forward it into `ExternalSources.memory_snippets` so
        // the pipeline's binder can render `## Memoria Recall` into the
        // runtime-identity block.
        let mut ep = serde_json::Map::new();
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\n- prefers Rust\n- hates emojis".into()),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);
        assert!(
            !sources.memory_snippets.is_empty(),
            "edge_profile.memory_section must flow into ExternalSources.memory_snippets"
        );
        assert!(sources.memory_snippets[0].contains("prefers Rust"));
    }

    #[test]
    fn external_sources_empty_memory_when_edge_profile_has_none() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);
        assert!(sources.memory_snippets.is_empty());
    }
}
