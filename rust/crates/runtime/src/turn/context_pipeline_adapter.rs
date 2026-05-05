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
    AgentContext, EdgeProfile, ExternalSources, SessionContext, TurnState,
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
    resolved_model: Option<&str>,
) -> ExternalSources {
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
    let task_type = edge_profile
        .get("selection_task_type")
        .and_then(Value::as_str)
        .or_else(|| crate::prompts::detect_task_type(user_content));
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

    ExternalSources {
        memory_snippets: vec![], // TODO: wire from Memoria retrieval
        spill_dir: None,
        self_model_text,
        tool_conditional,
        profile_desc,
        effort_hint,
        learned_context,
        system_override,
        plan_context,
        tool_guidance,
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
        remaining_turns: 20,
        turn_index: state.llm_rounds_completed,
        recovery: RecoveryState::default(),
        last_user_message: user_content.to_string(),
    }
}

/// Build SessionContext from Host state.
pub(crate) fn build_session_context(
    session_id: &str,
    run_id: Option<&str>,
    model_name: &str,
    max_input_tokens: u64,
    edge_profile: &serde_json::Map<String, Value>,
) -> SessionContext {
    SessionContext {
        session_id: session_id.to_string(),
        run_id: run_id.unwrap_or_default().to_string(),
        model_id: model_name.to_string(),
        model_limit: max_input_tokens as u32,
        provider_policy: ProviderCachePolicy::anthropic(),
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
