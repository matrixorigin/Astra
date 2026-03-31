//! Multi-turn `/chat/stream` loop (`stream_chat_sse`), kept here so `sse_loop/` can split further without one monolithic file.

use std::{collections::HashSet, path::PathBuf, time::Instant};

use crossterm::style::Stylize;
use crossterm::terminal;
use mo_agent_core::RuntimeLimits;
use mo_agent_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    tool_registry::{self},
    turn::chat_history_openai::openai_messages_from_repl_history,
    turn::edge_prompt_context::detect_project_languages,
    turn::headless_tool_assembly::tool_calls_for_stall_guard,
};

use crate::{
    ExplainMode, StreamResult, VerdictEvent, cli_utils::compact_or_raw, edge_tools,
    stream_render::consume_turn_sse,
};

use super::super::{
    ChatTurnParams,
    edge_executor::edge_executor_instance_id,
    explain_reports::{print_explain_report, print_verdict_report},
};
use super::post_tool_round::{
    PostToolTurnOutcome, PostToolTurnRequest, apply_post_tool_turn_policy,
};
use super::prepare_turn_request::{
    PrepareChatTurnRequest, PrepareTurnTelemetry, prepare_chat_turn_payload,
};
use super::stall_preflight::{StallPreflightRequest, apply_stall_preflight};
use super::tool_round::{HeadlessToolRoundRequest, run_headless_tool_round};
use super::turn_result_ingest::{
    TurnIngestOutcome, TurnResultIngestRequest, ingest_turn_sse_result,
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

        let assembly_start = Instant::now();
        let explain_stderr = explain != ExplainMode::Off;
        let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
            messages: &messages,
            current_session_id: current_session_id.as_deref(),
            model,
            explain_verbose: matches!(explain, ExplainMode::Verbose),
            explain_on: matches!(explain, ExplainMode::On),
            explain_stderr,
            project_root: &project_root,
            message,
            history,
            recent_tools,
            executor: &mut executor,
            selector,
            registry: &registry,
            tool_results: &tool_results,
            all_schemas: &all_schemas,
            turn_guard: &turn_guard,
            restricted_tools: &mut restricted_tools,
            step_recorder: &mut step_recorder,
            skill_registry,
            quiet,
            file_context: &file_context,
            assembly_start,
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
        .await;

        let resp = api
            .post_chat_turn_retry_429(token, &payload, 3, quiet)
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("API Error ({}): {}", status, compact_or_raw(&body)));
        }

        let edge_ctx = crate::stream_render::EdgeSseContext {
            api,
            token,
            executor_id: edge_executor_instance_id(),
            executor: &executor,
            quiet,
            perm_manager: Some(std::ptr::NonNull::from(&mut *perm_manager)),
            _pm: std::marker::PhantomData,
        };
        let turn_result =
            consume_turn_sse(resp, render_md, term_width, quiet, Some(edge_ctx)).await;

        match ingest_turn_sse_result(TurnResultIngestRequest {
            turn_result: &turn_result,
            message,
            recent_tools,
            quiet,
            first_ttft_ms: &mut first_ttft_ms,
            current_session_id: &mut current_session_id,
            current_run_id: &mut current_run_id,
            final_text: &mut final_text,
            total_prompt: &mut total_prompt,
            total_completion: &mut total_completion,
            total_tool_calls: &mut total_tool_calls,
            step_recorder: &mut step_recorder,
            all_tools_used: &mut all_tools_used,
            has_any_usage: &mut has_any_usage,
            forced_factual_retry: &mut forced_factual_retry,
            messages: &mut messages,
        }) {
            TurnIngestOutcome::Fatal(e) => return Err(e),
            TurnIngestOutcome::Break => break,
            TurnIngestOutcome::Continue => continue,
            TurnIngestOutcome::HasToolCalls => {}
        }

        let tool_calls_for_guard =
            tool_calls_for_stall_guard(&turn_result.tool_calls, &turn_result.edge_tool_round);

        apply_stall_preflight(StallPreflightRequest {
            turn_index: _turn as u32,
            tool_calls_for_guard: &tool_calls_for_guard,
            turn_sigs: &mut turn_sigs,
            turn_tool_names: &mut turn_tool_names,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
        });

        // Assemble tool results from SSE `tool_request` only — legacy inline execution removed.
        run_headless_tool_round(HeadlessToolRoundRequest {
            turn_index: _turn,
            quiet,
            api,
            token,
            current_session_id: current_session_id.as_ref(),
            turn_result: &turn_result,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            tool_call_records: &mut tool_call_records,
        })
        .await;
        explain_turns.extend(turn_result.explain_turns);

        match apply_post_tool_turn_policy(PostToolTurnRequest {
            turn_index: _turn as u32,
            message,
            tool_calls_for_guard: &tool_calls_for_guard,
            intent_tool_turns: &mut intent_tool_turns,
            messages: &mut messages,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_session_id: current_session_id.as_ref(),
            max_turns,
            loop_turn: _turn,
            recent_tools,
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
        }) {
            PostToolTurnOutcome::Abort(e) => return Err(e),
            PostToolTurnOutcome::RetryLlmClearToolResults => {
                tool_results = Vec::new();
                continue;
            }
            PostToolTurnOutcome::ProceedEndTurn => step_recorder.end_turn(false),
        }
    }

    if explain != ExplainMode::Off && !explain_turns.is_empty() && !quiet {
        print_explain_report(&explain_turns, explain == ExplainMode::Verbose);
    }
    if explain != ExplainMode::Off && !verdict_events.is_empty() && !quiet {
        print_verdict_report(&verdict_events, explain == ExplainMode::Verbose);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let format_footer_tokens = |tokens: u64| -> String {
        if tokens < 1000 {
            format!("{}tok", tokens)
        } else {
            format!("{:.1}k", tokens as f64 / 1000.0)
        }
    };
    let model_tag = model.unwrap_or("auto");
    let session_tag = current_session_id
        .as_deref()
        .map(|s| if s.len() > 8 { &s[..8] } else { s })
        .unwrap_or("?");
    if verbose_mode && !quiet {
        eprintln!(
            "{}",
            format!(
                "  ⏱ {:.1}s  ↓ {}  ↑ {}  model: {}  session: {}",
                elapsed,
                if has_any_usage {
                    format_footer_tokens(total_completion)
                } else {
                    "?".to_string()
                },
                if has_any_usage {
                    format_footer_tokens(total_prompt)
                } else {
                    "?".to_string()
                },
                model_tag,
                session_tag,
            )
            .dim()
        );
    }

    let report = first_selection_report.unwrap_or_else(|| tool_registry::SelectionReport {
        tools_selected: Vec::new(),
        selected_count: 0,
        budget_used: 0,
        budget_total: 0,
    });

    // Deduplicate stall events by type (keep only one of each type per user turn).
    // The internal _turn numbers were used for in-loop deduplication; for journal
    // output, we normalize all turn numbers to 0 (repl_turn.rs will use state.turn).
    let deduped_stall_events: Vec<(String, u32)> = {
        let mut seen = std::collections::HashSet::new();
        stall_events
            .into_iter()
            .filter(|(stall_type, _)| seen.insert(stall_type.clone()))
            .map(|(stall_type, _)| (stall_type, 0)) // turn will be filled by repl_turn
            .collect()
    };

    // Deduplicate verdict events by severity (keep only the first of each severity).
    // Same rationale: internal turn numbers are loop-internal, not user turns.
    let deduped_verdict_events: Vec<VerdictEvent> = {
        let mut seen = std::collections::HashSet::new();
        verdict_events
            .into_iter()
            .filter(|ve| seen.insert(ve.severity.clone()))
            .map(|mut ve| {
                ve.turn = 0; // turn will be filled by repl_turn
                ve
            })
            .collect()
    };

    Ok(StreamResult {
        session_id: current_session_id,
        run_id: current_run_id,
        full_text: final_text,
        prompt_tokens: total_prompt,
        completion_tokens: total_completion,
        tool_calls_count: total_tool_calls,
        tools_selected: report.tools_selected,
        selected_skills: all_selected_skills,
        tools_used: all_tools_used.into_iter().collect(),
        tool_call_records,
        budget_used: report.budget_used,
        budget_pressure: first_budget_pressure,
        stall_events: deduped_stall_events,
        verdict_events: deduped_verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        // Export tool health with merged historical entries to preserve unused tools
        tool_health_export: turn_guard.health.export_merged(tool_health_entries),
        last_heavy_checkpoint,
        ttft_ms: first_ttft_ms,
        context_ms: first_context_assembly_ms,
        selector_strategy: first_selector_strategy,
        selector_ms: first_selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms: first_memoria_ms,
    })
}
