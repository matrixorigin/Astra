//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Entry [`stream_chat_sse`] builds a [`CliAgenticLoopHost`] + [`AgenticLoopState`],
//! runs the runtime's [`run_agentic_loop_with_host`], then finalizes to [`StreamResult`].
//! One iteration is driven by the runtime; the host handles payload prep + HTTP + SSE.

mod agentic_loop_turn;
mod agentic_sse_loop;
mod cli_loop_host;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use mo_agent_core::RuntimeLimits;
use mo_agent_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    plan_decompose::CHAT_PLAN_ONLY_SYSTEM,
    semantic_dedup::SemanticDedup,
    tool_registry::ToolRegistry,
    turn::agentic_loop_host::{AgenticLoopState, run_agentic_loop_with_host},
    turn::agentic_turn_telemetry::step_recorder_chat_ephemeral_run_id,
    turn::chat_history_openai::openai_messages_from_repl_history,
    turn::chat_turn_heuristics::{TaskExecutionProfile, infer_task_execution_profile},
    turn::edge_prompt_context::detect_project_languages,
    turn::tool_health::ToolHealthTracker,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};

use crate::{StreamResult, cli_utils::terminal_width_usize, edge_tools};

use super::ChatTurnParams;
use agentic_sse_loop::{
    StreamLoopSidecarEprint, StreamResultBuild, build_stream_result, eprint_stream_loop_sidecars,
};
use cli_loop_host::CliAgenticLoopHost;
use serde_json::json;

/// Auto-detect stop hooks from project root based on build system markers.
fn detect_stop_hooks(
    project_root: &std::path::Path,
    task_profile: TaskExecutionProfile,
) -> Vec<mo_agent_runtime::turn::stop_hooks::StopHook> {
    use mo_agent_runtime::turn::stop_hooks::StopHook;
    if !task_profile.verification_required {
        return Vec::new();
    }
    let dir = project_root.to_string_lossy().to_string();
    let mut hooks = Vec::new();

    // Rust: prefer rust/Cargo.toml (nested workspace) over root Cargo.toml
    if project_root.join("rust/Cargo.toml").exists() {
        hooks.push(StopHook {
            label: "cargo-check".into(),
            command: "cargo check --manifest-path rust/Cargo.toml --quiet 2>&1 | head -30".into(),
            working_dir: Some(dir.clone()),
        });
    } else if project_root.join("Cargo.toml").exists() {
        hooks.push(StopHook {
            label: "cargo-check".into(),
            command: "cargo check --quiet 2>&1 | head -30".into(),
            working_dir: Some(dir.clone()),
        });
    }
    // Node: package.json with "build" script → npm run build
    if project_root.join("package.json").exists()
        && let Ok(content) = std::fs::read_to_string(project_root.join("package.json"))
        && content.contains("\"build\"")
    {
        hooks.push(StopHook {
            label: "npm-build".into(),
            command: "npm run build 2>&1 | tail -20".into(),
            working_dir: Some(dir.clone()),
        });
    }
    // Go: go.mod → go vet
    if project_root.join("go.mod").exists() {
        hooks.push(StopHook {
            label: "go-vet".into(),
            command: "go vet ./... 2>&1 | head -30".into(),
            working_dir: Some(dir),
        });
    }

    hooks
}

pub(crate) async fn stream_chat_sse(p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    let start = Instant::now();
    let term_width = terminal_width_usize();
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let executor =
        edge_tools::ToolExecutor::new(&project_root).with_cloud(p.api.api_origin(), p.token);
    let mut messages = openai_messages_from_repl_history(p.history, p.message);
    let all_schemas = if p.plan_only_chat {
        messages.insert(
            0,
            json!({
                "role": "system",
                "content": CHAT_PLAN_ONLY_SYSTEM,
            }),
        );
        Vec::new()
    } else {
        edge_tools::all_tool_schemas()
    };
    let registry = ToolRegistry::new(all_schemas.clone());
    let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

    let current_session_id = p.session_id.map(|s| s.to_string());
    let task_profile = infer_task_execution_profile(p.message);

    let turn_guard = if p.tool_health_entries.is_empty() {
        TurnGuard::with_profile(task_profile)
    } else {
        let health = ToolHealthTracker::from_entries(p.tool_health_entries);
        TurnGuard::with_health_and_profile(health, task_profile)
    };

    let max_turns = RuntimeLimits::global().max_turns;
    let step_recorder = StepRecorder::with_persistence(
        current_session_id.as_deref().unwrap_or("ephemeral"),
        step_recorder_chat_ephemeral_run_id(start.elapsed().as_millis()).as_str(),
    );

    // ─── Build host + state ──────────────────────────────────────────────
    let mut host = CliAgenticLoopHost {
        api: p.api,
        token: p.token,
        model: p.model,
        explain: p.explain,
        render_md: p.render_md,
        term_width,
        quiet: p.quiet,
        message: p.message,
        history: p.history,
        recent_tools: p.recent_tools,
        project_root: project_root.clone(),
        executor,
        selector: p.selector,
        registry,
        all_schemas,
        skill_registry: p.skill_registry,
        file_context,
        perm_manager: p.perm_manager,
        valid_tool_names,
        pending_clear_lines: 0,
    };

    let mut state = AgenticLoopState {
        messages,
        tool_results: Vec::new(),
        current_session_id,
        current_run_id: None,
        final_text: String::new(),
        total_prompt: 0,
        total_completion: 0,
        total_tool_calls: 0,
        has_any_usage: false,
        max_turns,
        remaining_turns: max_turns,
        turn_guard,
        restricted_tools: HashSet::new(),
        step_recorder,
        idempotency_cache: InMemoryIdempotencyCache::new(),
        semantic_dedup: SemanticDedup::new(
            mo_agent_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
        ),
        turn_sigs: Vec::new(),
        turn_tool_names: Vec::new(),
        stall_events: Vec::new(),
        intent_tool_turns: Vec::new(),
        verdict_events: Vec::new(),
        last_heavy_checkpoint: None,
        tool_call_records: Vec::new(),
        forced_factual_retry: false,
        explain_turns: Vec::new(),
        first_ttft_ms: None,
        all_tools_used: HashSet::new(),
        first_selection_report: None,
        first_budget_pressure: 0.0,
        first_context_assembly_ms: None,
        first_memoria_ms: None,
        first_selector_ms: None,
        first_selector_strategy: None,
        selector_tokens_in: 0,
        selector_tokens_out: 0,
        all_selected_skills: Vec::new(),
        message: p.message.to_string(),
        recent_tools: p.recent_tools.to_vec(),
        task_profile,
        api: p.api.clone(),
        api_token: p.token.to_string(),
        cancel_flag: None,
        cancel_token: None,
        delegation_engine: None,
        stop_hooks: detect_stop_hooks(&project_root, task_profile),
        stop_hook_runs: 0,
        consecutive_same_error: 0,
        last_error_category: None,
    };

    // ─── Run the runtime loop ────────────────────────────────────────────
    run_agentic_loop_with_host(&mut host, &mut state).await?;

    // ─── Finalize ────────────────────────────────────────────────────────
    eprint_stream_loop_sidecars(StreamLoopSidecarEprint {
        explain: p.explain,
        quiet: p.quiet,
        verbose_mode: p.verbose_mode,
        start,
        model: p.model,
        explain_turns: &state.explain_turns,
        verdict_events: &state.verdict_events,
        has_any_usage: state.has_any_usage,
        total_prompt: state.total_prompt,
        total_completion: state.total_completion,
        current_session_id: state.current_session_id.as_deref(),
    });

    Ok(build_stream_result(StreamResultBuild {
        tool_health_entries: p.tool_health_entries,
        session_id: state.current_session_id,
        run_id: state.current_run_id,
        full_text: state.final_text,
        prompt_tokens: state.total_prompt,
        completion_tokens: state.total_completion,
        tool_calls_count: state.total_tool_calls,
        first_selection_report: state.first_selection_report,
        selected_skills: state.all_selected_skills,
        tools_used: state.all_tools_used,
        tool_call_records: state.tool_call_records,
        budget_pressure: state.first_budget_pressure,
        stall_events: state.stall_events,
        verdict_events: state.verdict_events,
        step_recorder: &state.step_recorder,
        turn_guard: &state.turn_guard,
        last_heavy_checkpoint: state.last_heavy_checkpoint,
        ttft_ms: state.first_ttft_ms,
        context_ms: state.first_context_assembly_ms,
        selector_strategy: state.first_selector_strategy,
        selector_ms: state.first_selector_ms,
        selector_tokens_in: state.selector_tokens_in,
        selector_tokens_out: state.selector_tokens_out,
        memoria_ms: state.first_memoria_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::detect_stop_hooks;
    use mo_agent_runtime::turn::chat_turn_heuristics::infer_task_execution_profile;
    use std::path::Path;

    #[test]
    fn read_only_requests_skip_stop_hooks() {
        let hooks = detect_stop_hooks(
            Path::new("."),
            infer_task_execution_profile("review 最新的commit"),
        );
        assert!(hooks.is_empty());
        let hooks = detect_stop_hooks(
            Path::new("."),
            infer_task_execution_profile("explain this diff"),
        );
        assert!(hooks.is_empty());
    }

    #[test]
    fn mutating_requests_keep_stop_hooks() {
        let hooks = detect_stop_hooks(
            Path::new("."),
            infer_task_execution_profile("implement a fix for the failing test"),
        );
        assert!(!hooks.is_empty());
    }
}
