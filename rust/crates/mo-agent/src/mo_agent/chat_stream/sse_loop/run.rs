//! Multi-turn `/chat/stream` loop (`stream_chat_sse`), kept here so `sse_loop/` can split further without one monolithic file.

use std::{collections::HashSet, path::PathBuf, time::Instant};

use crossterm::terminal;
use mo_agent_core::RuntimeLimits;
use mo_agent_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    tool_registry::{self},
    turn::chat_history_openai::openai_messages_from_repl_history,
    turn::edge_prompt_context::detect_project_languages,
};

use crate::{StreamResult, VerdictEvent, edge_tools};

use super::super::ChatTurnParams;
use super::agentic_loop_turn::{
    AgenticLoopTurnExit, AgenticTurnRequest, run_agentic_loop_iteration,
};
use super::prepare_turn_request::PrepareTurnTelemetry;
use super::stream_result_finalize::{
    StreamLoopSidecarEprint, StreamResultBuild, build_stream_result, eprint_stream_loop_sidecars,
};

pub(crate) async fn stream_chat_sse(p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    // Destructure for readability within the function body
    let ChatTurnParams {
        api,
        token,
        message,
        session_id,
        model,
        explain,
        render_md,
        history,
        perm_manager,
        verbose_mode,
        quiet,
        selector,
        recent_tools,
        tool_health_entries,
        skill_registry,
    } = p;
    let start = Instant::now();
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let mut executor =
        edge_tools::ToolExecutor::new(&project_root).with_cloud(api.api_origin(), token);
    let all_schemas = edge_tools::all_tool_schemas();
    let registry = tool_registry::ToolRegistry::new(all_schemas.clone());
    let valid_tool_names: HashSet<String> = all_schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();

    let mut current_session_id: Option<String> = session_id.map(|s| s.to_string());
    // Build messages: history + current user message
    let mut messages: Vec<serde_json::Value> = openai_messages_from_repl_history(history, message);

    let mut tool_results: Vec<serde_json::Value> = Vec::new();
    let mut final_text = String::new();
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    let mut total_tool_calls = 0u32;
    let mut has_any_usage = false;
    let mut explain_turns: Vec<serde_json::Value> = Vec::new();
    // Track first-turn selection report and all unique tools actually used
    let mut first_selection_report: Option<tool_registry::SelectionReport> = None;
    let mut first_budget_pressure: f64 = 0.0;
    let mut all_tools_used: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut turn_sigs: Vec<std::collections::BTreeSet<String>> = Vec::new();
    let mut turn_tool_names: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut forced_factual_retry = false;
    let mut current_run_id: Option<String> = None;
    let mut stall_events: Vec<(String, u32)> = Vec::new();
    let mut verdict_events: Vec<VerdictEvent> = Vec::new();
    let mut last_heavy_checkpoint: Option<
        mo_agent_runtime::pipeline::step_protocol::StepCheckpoint,
    > = None;
    let mut tool_call_records: Vec<mo_agent_services::session_journal::ToolCallRecord> = Vec::new();
    // Capture first turn's TTFT for observability
    let mut first_ttft_ms: Option<u64> = None;
    // Cross-turn dedup: IdempotencyCache with content-hash keys (Step Protocol)
    let mut idempotency_cache = InMemoryIdempotencyCache::new();
    // Semantic near-duplicate tracker (Tier 2: param-aware, Tier 3: output similarity)
    let mut semantic_dedup = mo_agent_runtime::semantic_dedup::SemanticDedup::new(
        mo_agent_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
    );
    // Unified non-happy-path guard: stall + divergence + tool health + error recovery + escalation
    let mut turn_guard = if tool_health_entries.is_empty() {
        mo_agent_runtime::turn::turn_guard::TurnGuard::new()
    } else {
        let health = mo_agent_runtime::turn::tool_health::ToolHealthTracker::from_entries(
            tool_health_entries,
        );
        mo_agent_runtime::turn::turn_guard::TurnGuard::with_health(health)
    };
    // Stall enforcement: tools restricted from schema after nudge-ignore
    let mut restricted_tools: HashSet<String> = HashSet::new();
    // Dynamic turn budget: each stall/divergence costs turns to prevent runaway sessions
    let max_turns = RuntimeLimits::global().max_turns;
    let mut remaining_turns: usize = max_turns;
    // Intent drift tracker: per-turn tool names + args for drift detection
    let mut intent_tool_turns: Vec<(Vec<String>, String)> = Vec::new();
    // Step Protocol recorder: maps implicit chat_stream phases to explicit Step events
    let mut step_recorder =
        mo_agent_runtime::pipeline::step_recorder::StepRecorder::with_persistence(
            current_session_id.as_deref().unwrap_or("ephemeral"),
            &format!("chat-{}", start.elapsed().as_millis()),
        );

    // Track first turn's context assembly time for observability
    let mut first_context_assembly_ms: Option<u64> = None;
    let mut first_memoria_ms: Option<u64> = None;
    let mut first_selector_ms: Option<u64> = None;
    let mut first_selector_strategy: Option<String> = None;
    let mut selector_tokens_in: u64 = 0;
    let mut selector_tokens_out: u64 = 0;
    let mut all_selected_skills: Vec<String> = Vec::new();

    for _turn in 0..max_turns {
        if remaining_turns == 0 {
            return Err("Turn budget exhausted due to repeated stalls. Aborting.".to_string());
        }
        remaining_turns = remaining_turns.saturating_sub(1);
        step_recorder.begin_turn(_turn as u32);

        match run_agentic_loop_iteration(AgenticTurnRequest {
            turn_index: _turn,
            max_turns,
            api,
            token,
            model,
            explain,
            render_md,
            term_width,
            quiet,
            message,
            history,
            recent_tools,
            project_root: project_root.as_path(),
            executor: &mut executor,
            selector,
            registry: &registry,
            messages: &mut messages,
            current_session_id: &mut current_session_id,
            tool_results: &mut tool_results,
            all_schemas: &all_schemas,
            turn_guard: &mut turn_guard,
            restricted_tools: &mut restricted_tools,
            step_recorder: &mut step_recorder,
            skill_registry,
            file_context: &file_context,
            perm_manager,
            valid_tool_names: &valid_tool_names,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            turn_sigs: &mut turn_sigs,
            turn_tool_names: &mut turn_tool_names,
            stall_events: &mut stall_events,
            intent_tool_turns: &mut intent_tool_turns,
            verdict_events: &mut verdict_events,
            remaining_turns: &mut remaining_turns,
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            tool_call_records: &mut tool_call_records,
            first_ttft_ms: &mut first_ttft_ms,
            current_run_id: &mut current_run_id,
            final_text: &mut final_text,
            total_prompt: &mut total_prompt,
            total_completion: &mut total_completion,
            total_tool_calls: &mut total_tool_calls,
            all_tools_used: &mut all_tools_used,
            has_any_usage: &mut has_any_usage,
            forced_factual_retry: &mut forced_factual_retry,
            explain_turns: &mut explain_turns,
            telem: PrepareTurnTelemetry {
                first_memoria_ms: &mut first_memoria_ms,
                first_selector_ms: &mut first_selector_ms,
                first_selector_strategy: &mut first_selector_strategy,
                selector_tokens_in: &mut selector_tokens_in,
                selector_tokens_out: &mut selector_tokens_out,
                first_selection_report: &mut first_selection_report,
                first_budget_pressure: &mut first_budget_pressure,
                first_context_assembly_ms: &mut first_context_assembly_ms,
                all_selected_skills: &mut all_selected_skills,
            },
        })
        .await
        {
            Ok(AgenticLoopTurnExit::BreakLoop) => break,
            Ok(AgenticLoopTurnExit::ContinueIterating) => {}
            Err(e) => return Err(e),
        }
    }

    eprint_stream_loop_sidecars(StreamLoopSidecarEprint {
        explain,
        quiet,
        verbose_mode,
        start,
        model,
        explain_turns: &explain_turns,
        verdict_events: &verdict_events,
        has_any_usage,
        total_prompt,
        total_completion,
        current_session_id: current_session_id.as_deref(),
    });

    Ok(build_stream_result(StreamResultBuild {
        tool_health_entries,
        session_id: current_session_id,
        run_id: current_run_id,
        full_text: final_text,
        prompt_tokens: total_prompt,
        completion_tokens: total_completion,
        tool_calls_count: total_tool_calls,
        first_selection_report,
        selected_skills: all_selected_skills,
        tools_used: all_tools_used,
        tool_call_records,
        budget_pressure: first_budget_pressure,
        stall_events,
        verdict_events,
        step_recorder: &step_recorder,
        turn_guard: &turn_guard,
        last_heavy_checkpoint,
        ttft_ms: first_ttft_ms,
        context_ms: first_context_assembly_ms,
        selector_strategy: first_selector_strategy,
        selector_ms: first_selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms: first_memoria_ms,
    }))
}
