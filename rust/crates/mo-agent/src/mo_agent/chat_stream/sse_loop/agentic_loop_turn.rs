//! Single agentic iteration: `/chat/turn` fetch + SSE consume, turn ingest, stall preflight, headless tool round, post-tool policy.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::time::Instant;

use crossterm::style::Stylize;
use mo_agent_core::agent_warn;
use mo_agent_runtime::{
    pipeline::step_checkpoint,
    pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint},
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    tool_registry::ToolRegistry,
    tool_selector::ToolSelector,
    turn::chat_history_openai::{append_openai_user_content_messages, openai_user_content_message},
    turn::chat_turn_heuristics::{
        openai_factual_tool_retry_user_message, should_force_factual_tool_retry,
    },
    turn::headless_tool_assembly::tool_calls_for_stall_guard,
    turn::response_guard::apply_response_guards,
    turn::stall::{IntentDrift, detect_intent_drift},
    turn::turn_guard::{TurnGuard, VerdictSeverity},
};
use mo_agent_services::session_journal::ToolCallRecord;
use serde_json::Value;

use crate::{
    ExplainMode, VerdictEvent,
    cli_utils::compact_or_raw,
    edge_tools::ToolExecutor,
    permission_manager::PermissionManager,
    skill_instructions::SharedSkillRegistry,
    stream_render::{EdgeSseContext, TurnResult, consume_turn_sse},
};

use super::super::edge_executor::edge_executor_instance_id;
use super::prepare_turn_request::{
    PrepareChatTurnRequest, PrepareTurnTelemetry, prepare_chat_turn_payload,
};
use super::tool_round::{HeadlessToolRoundRequest, run_headless_tool_round};

// ─── Fetch: payload → POST → consume_turn_sse ─────────────────────────────────

pub(crate) struct ChatTurnSseFetchRequest<'a> {
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub quiet: bool,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: &'a Path,
    pub executor: &'a mut ToolExecutor,
    pub selector: &'a dyn ToolSelector,
    pub registry: &'a ToolRegistry,
    pub messages: &'a [Value],
    pub current_session_id: Option<&'a str>,
    pub tool_results: &'a [Value],
    pub all_schemas: &'a [Value],
    pub turn_guard: &'a mo_agent_runtime::turn::turn_guard::TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub skill_registry: &'a SharedSkillRegistry,
    pub file_context: &'a [String],
    pub assembly_start: Instant,
    pub telem: PrepareTurnTelemetry<'a>,
    pub perm_manager: &'a mut PermissionManager,
}

async fn fetch_chat_turn_sse(ctx: ChatTurnSseFetchRequest<'_>) -> Result<TurnResult, String> {
    let ChatTurnSseFetchRequest {
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
        project_root,
        executor,
        selector,
        registry,
        messages,
        current_session_id,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        file_context,
        assembly_start,
        telem,
        perm_manager,
    } = ctx;

    let explain_stderr = explain != ExplainMode::Off;
    let payload = prepare_chat_turn_payload(PrepareChatTurnRequest {
        messages,
        current_session_id,
        model,
        explain_verbose: matches!(explain, ExplainMode::Verbose),
        explain_on: matches!(explain, ExplainMode::On),
        explain_stderr,
        project_root,
        message,
        history,
        recent_tools,
        executor,
        selector,
        registry,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        quiet,
        file_context,
        assembly_start,
        telem,
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

    let edge_ctx = EdgeSseContext {
        api,
        token,
        executor_id: edge_executor_instance_id(),
        executor,
        quiet,
        perm_manager: Some(std::ptr::NonNull::from(&mut *perm_manager)),
        _pm: std::marker::PhantomData,
    };

    Ok(consume_turn_sse(resp, render_md, term_width, quiet, Some(edge_ctx)).await)
}

// ─── Ingest TurnResult (guards, usage, no-tool exit) ──────────────────────────

struct TurnResultIngestRequest<'a> {
    turn_result: &'a TurnResult,
    message: &'a str,
    recent_tools: &'a [String],
    quiet: bool,
    first_ttft_ms: &'a mut Option<u64>,
    current_session_id: &'a mut Option<String>,
    current_run_id: &'a mut Option<String>,
    final_text: &'a mut String,
    total_prompt: &'a mut u64,
    total_completion: &'a mut u64,
    total_tool_calls: &'a mut u32,
    step_recorder: &'a mut StepRecorder,
    all_tools_used: &'a mut HashSet<String>,
    has_any_usage: &'a mut bool,
    forced_factual_retry: &'a mut bool,
    messages: &'a mut Vec<Value>,
}

enum TurnIngestOutcome {
    Break,
    Continue,
    Fatal(String),
    HasToolCalls,
}

fn ingest_turn_sse_result(ctx: TurnResultIngestRequest<'_>) -> TurnIngestOutcome {
    let TurnResultIngestRequest {
        turn_result,
        message,
        recent_tools,
        quiet,
        first_ttft_ms,
        current_session_id,
        current_run_id,
        final_text,
        total_prompt,
        total_completion,
        total_tool_calls,
        step_recorder,
        all_tools_used,
        has_any_usage,
        forced_factual_retry,
        messages,
    } = ctx;

    if first_ttft_ms.is_none() {
        *first_ttft_ms = turn_result.ttft_ms;
    }

    if let Some(sid) = &turn_result.session_id {
        *current_session_id = Some(sid.clone());
    }
    if turn_result.run_id.is_some() {
        *current_run_id = turn_result.run_id.clone();
    }
    if !turn_result.full_text.is_empty() {
        *final_text = turn_result.full_text.clone();

        let guard =
            apply_response_guards(final_text.as_str(), &turn_result.tool_calls, &[], message);
        if let Some(replacement) = guard.replacement {
            agent_warn!("response_guard", "Guard triggered, replacing LLM output");
            *final_text = replacement;
            return TurnIngestOutcome::Break;
        }
        if guard.quality.has_fabrication_markers {
            agent_warn!(
                "response_guard",
                "Fabrication markers detected: placeholder paths in response"
            );
        }
        if guard.quality.is_echo {
            agent_warn!(
                "response_guard",
                "Echo detected: LLM repeated user query instead of answering"
            );
        }
    }

    *total_prompt += turn_result.prompt_tokens;
    *total_completion += turn_result.completion_tokens;
    *total_tool_calls += if !turn_result.tool_calls.is_empty() {
        turn_result.tool_calls.len()
    } else {
        turn_result.edge_tool_round.len()
    } as u32;

    step_recorder.record_tokens(turn_result.prompt_tokens, turn_result.completion_tokens);

    for tc in &turn_result.tool_calls {
        if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
            all_tools_used.insert(name.to_string());
        }
    }
    for e in &turn_result.edge_tool_round {
        all_tools_used.insert(e.tool.clone());
    }
    *has_any_usage = *has_any_usage || turn_result.has_usage;

    if let Some(ref err) = turn_result.error_message {
        return TurnIngestOutcome::Fatal(err.clone());
    }

    let round_has_edge_work =
        !turn_result.tool_calls.is_empty() || !turn_result.edge_tool_round.is_empty();
    if !round_has_edge_work {
        if should_force_factual_tool_retry(
            message,
            recent_tools,
            *total_tool_calls,
            *forced_factual_retry,
        ) {
            *forced_factual_retry = true;
            if !quiet {
                eprintln!(
                    "{}",
                    "  ↻ No tool call on a live-data query; forcing one corrective retry…".yellow()
                );
            }
            messages.push(openai_factual_tool_retry_user_message(message));
            final_text.clear();
            return TurnIngestOutcome::Continue;
        }
        return TurnIngestOutcome::Break;
    }

    TurnIngestOutcome::HasToolCalls
}

// ─── Stall preflight (signatures + name-stall) ────────────────────────────────

const TOOL_NAME_STALL_WINDOW: usize = 3;

struct StallPreflightRequest<'a> {
    turn_index: u32,
    tool_calls_for_guard: &'a [Value],
    turn_sigs: &'a mut Vec<BTreeSet<String>>,
    turn_tool_names: &'a mut Vec<HashSet<String>>,
    stall_events: &'a mut Vec<(String, u32)>,
    turn_guard: &'a mut TurnGuard,
}

fn apply_stall_preflight(ctx: StallPreflightRequest<'_>) {
    let StallPreflightRequest {
        turn_index,
        tool_calls_for_guard,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    } = ctx;

    let sig_set: BTreeSet<String> = tool_calls_for_guard
        .iter()
        .map(|tc| {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = tc.get("arguments").cloned().unwrap_or_default();
            format!(
                "{}:{}",
                name,
                serde_json::to_string(&args).unwrap_or_default()
            )
        })
        .collect();
    let name_set: HashSet<String> = tool_calls_for_guard
        .iter()
        .map(|tc| {
            tc.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    turn_sigs.push(sig_set);
    turn_tool_names.push(name_set.clone());

    turn_guard.record_tool_calls(tool_calls_for_guard);

    let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
        && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
            .windows(2)
            .all(|w| w[0] == w[1]);

    if name_stall {
        stall_events.push(("name_stall".to_string(), turn_index));
    }
}

// ─── Post-tool: intent drift + TurnGuard verdict ─────────────────────────────

struct PostToolTurnRequest<'a> {
    turn_index: u32,
    message: &'a str,
    tool_calls_for_guard: &'a [Value],
    intent_tool_turns: &'a mut Vec<(Vec<String>, String)>,
    messages: &'a mut Vec<Value>,
    stall_events: &'a mut Vec<(String, u32)>,
    turn_guard: &'a mut TurnGuard,
    verdict_events: &'a mut Vec<VerdictEvent>,
    restricted_tools: &'a mut HashSet<String>,
    remaining_turns: &'a mut usize,
    step_recorder: &'a mut StepRecorder,
    current_session_id: Option<&'a String>,
    max_turns: usize,
    loop_turn: usize,
    recent_tools: &'a [String],
    last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
}

enum PostToolTurnOutcome {
    ProceedEndTurn,
    RetryLlmClearToolResults,
    Abort(String),
}

fn apply_post_tool_turn_policy(ctx: PostToolTurnRequest<'_>) -> PostToolTurnOutcome {
    let PostToolTurnRequest {
        turn_index,
        message,
        tool_calls_for_guard,
        intent_tool_turns,
        messages,
        stall_events,
        turn_guard,
        verdict_events,
        restricted_tools,
        remaining_turns,
        step_recorder,
        current_session_id,
        max_turns,
        loop_turn,
        recent_tools,
        last_heavy_checkpoint,
    } = ctx;

    {
        let turn_names: Vec<String> = tool_calls_for_guard
            .iter()
            .filter_map(|tc| tc.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let turn_args_text: String = tool_calls_for_guard
            .iter()
            .filter_map(|tc| {
                tc.get("arguments")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join(" ");
        intent_tool_turns.push((turn_names, turn_args_text));

        if let IntentDrift::Drifting { correction, .. } =
            detect_intent_drift(message, intent_tool_turns)
        {
            messages.push(openai_user_content_message(&correction));
            stall_events.push(("intent_drift".to_string(), turn_index));
        }
    }

    {
        let verdict = turn_guard.evaluate();

        if verdict.severity > VerdictSeverity::Healthy {
            let severity_str = match verdict.severity {
                VerdictSeverity::Critical => "critical",
                VerdictSeverity::Warning => "warning",
                VerdictSeverity::Info => "info",
                VerdictSeverity::Healthy => unreachable!(),
            };
            let health_summary = turn_guard.health.summary();
            verdict_events.push(VerdictEvent {
                turn: turn_index,
                severity: severity_str.to_string(),
                injections: verdict.injections.clone(),
                avoid_tools: verdict.avoid_tools.clone(),
                force_stop: verdict.force_stop,
                nudge_count: turn_guard.nudge_count,
                total_errors: turn_guard.errors.total_errors,
                deprioritized_count: health_summary.deprioritized_count,
                total_timeouts: health_summary.total_timeouts,
                total_cache_hits: health_summary.total_cache_hits,
                flaky_count: health_summary.flaky_count,
            });
        }

        append_openai_user_content_messages(messages, &verdict.injections);

        for tool in &verdict.avoid_tools {
            restricted_tools.insert(tool.clone());
        }

        match verdict.severity {
            VerdictSeverity::Critical => {
                *remaining_turns = remaining_turns.saturating_sub(5);
            }
            VerdictSeverity::Warning => {
                *remaining_turns = remaining_turns.saturating_sub(2);
            }
            _ => {}
        }

        let severity_label = match verdict.severity {
            VerdictSeverity::Critical => "critical",
            VerdictSeverity::Warning => "warning",
            VerdictSeverity::Info => "info",
            VerdictSeverity::Healthy => "healthy",
        };
        step_recorder.record_verdict(
            severity_label,
            verdict.stall_detected,
            verdict.is_diverging,
            verdict.force_stop,
            verdict.injections.len(),
        );

        if let Some(sid) = current_session_id
            && let Some(heavy) = step_recorder.build_heavy_checkpoint(
                messages,
                0,
                max_turns.saturating_sub(loop_turn) as u32,
                &turn_guard
                    .health
                    .deprioritized_tools()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
                recent_tools,
            )
        {
            let cp = StepCheckpoint::Heavy(Box::new(heavy));
            let _ = step_checkpoint::write_step_checkpoint(
                sid,
                step_recorder.summary().checkpoints,
                &cp,
            );
            *last_heavy_checkpoint = Some(cp);
        }

        if verdict.force_stop {
            step_recorder.end_turn(true);
            return PostToolTurnOutcome::Abort(
                "Agent escalated to critical — too many errors and stalls. Aborting.".to_string(),
            );
        }

        if !verdict.injections.is_empty() && verdict.severity >= VerdictSeverity::Warning {
            step_recorder.end_turn(false);
            return PostToolTurnOutcome::RetryLlmClearToolResults;
        }
    }

    PostToolTurnOutcome::ProceedEndTurn
}

// ─── Orchestrator: one full iteration ────────────────────────────────────────

pub(crate) enum AgenticLoopTurnExit {
    ContinueIterating,
    BreakLoop,
}

pub(crate) struct AgenticTurnRequest<'a> {
    pub turn_index: usize,
    pub max_turns: usize,
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub model: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub term_width: usize,
    pub quiet: bool,
    pub message: &'a str,
    pub history: &'a [(String, String)],
    pub recent_tools: &'a [String],
    pub project_root: &'a Path,
    pub executor: &'a mut ToolExecutor,
    pub selector: &'a dyn ToolSelector,
    pub registry: &'a ToolRegistry,
    pub messages: &'a mut Vec<Value>,
    pub current_session_id: &'a mut Option<String>,
    pub tool_results: &'a mut Vec<Value>,
    pub all_schemas: &'a [Value],
    pub turn_guard: &'a mut TurnGuard,
    pub restricted_tools: &'a mut HashSet<String>,
    pub step_recorder: &'a mut StepRecorder,
    pub skill_registry: &'a SharedSkillRegistry,
    pub file_context: &'a [String],
    pub perm_manager: &'a mut PermissionManager,
    pub valid_tool_names: &'a HashSet<String>,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub turn_sigs: &'a mut Vec<BTreeSet<String>>,
    pub turn_tool_names: &'a mut Vec<HashSet<String>>,
    pub stall_events: &'a mut Vec<(String, u32)>,
    pub intent_tool_turns: &'a mut Vec<(Vec<String>, String)>,
    pub verdict_events: &'a mut Vec<VerdictEvent>,
    pub remaining_turns: &'a mut usize,
    pub last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
    pub tool_call_records: &'a mut Vec<ToolCallRecord>,
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub forced_factual_retry: &'a mut bool,
    pub explain_turns: &'a mut Vec<Value>,
    pub telem: PrepareTurnTelemetry<'a>,
}

pub(crate) async fn run_agentic_loop_iteration(
    ctx: AgenticTurnRequest<'_>,
) -> Result<AgenticLoopTurnExit, String> {
    let AgenticTurnRequest {
        turn_index,
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
        project_root,
        executor,
        selector,
        registry,
        messages,
        current_session_id,
        tool_results,
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        file_context,
        perm_manager,
        valid_tool_names,
        idempotency_cache,
        semantic_dedup,
        turn_sigs,
        turn_tool_names,
        stall_events,
        intent_tool_turns,
        verdict_events,
        remaining_turns,
        last_heavy_checkpoint,
        tool_call_records,
        first_ttft_ms,
        current_run_id,
        final_text,
        total_prompt,
        total_completion,
        total_tool_calls,
        all_tools_used,
        has_any_usage,
        forced_factual_retry,
        explain_turns,
        telem,
    } = ctx;

    let assembly_start = Instant::now();
    let turn_result = fetch_chat_turn_sse(ChatTurnSseFetchRequest {
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
        project_root,
        executor,
        selector,
        registry,
        messages: messages.as_slice(),
        current_session_id: current_session_id.as_deref(),
        tool_results: tool_results.as_slice(),
        all_schemas,
        turn_guard,
        restricted_tools,
        step_recorder,
        skill_registry,
        file_context,
        assembly_start,
        telem,
        perm_manager,
    })
    .await?;

    match ingest_turn_sse_result(TurnResultIngestRequest {
        turn_result: &turn_result,
        message,
        recent_tools,
        quiet,
        first_ttft_ms,
        current_session_id,
        current_run_id,
        final_text,
        total_prompt,
        total_completion,
        total_tool_calls,
        step_recorder,
        all_tools_used,
        has_any_usage,
        forced_factual_retry,
        messages,
    }) {
        TurnIngestOutcome::Fatal(e) => return Err(e),
        TurnIngestOutcome::Break => return Ok(AgenticLoopTurnExit::BreakLoop),
        TurnIngestOutcome::Continue => return Ok(AgenticLoopTurnExit::ContinueIterating),
        TurnIngestOutcome::HasToolCalls => {}
    }

    let tool_calls_for_guard =
        tool_calls_for_stall_guard(&turn_result.tool_calls, &turn_result.edge_tool_round);

    apply_stall_preflight(StallPreflightRequest {
        turn_index: turn_index as u32,
        tool_calls_for_guard: &tool_calls_for_guard,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    });

    run_headless_tool_round(HeadlessToolRoundRequest {
        turn_index,
        quiet,
        api,
        token,
        current_session_id: current_session_id.as_ref(),
        turn_result: &turn_result,
        messages,
        tool_results,
        valid_tool_names,
        restricted_tools,
        turn_guard,
        step_recorder,
        idempotency_cache,
        semantic_dedup,
        tool_call_records,
    })
    .await;
    explain_turns.extend(turn_result.explain_turns.iter().cloned());

    match apply_post_tool_turn_policy(PostToolTurnRequest {
        turn_index: turn_index as u32,
        message,
        tool_calls_for_guard: &tool_calls_for_guard,
        intent_tool_turns,
        messages,
        stall_events,
        turn_guard,
        verdict_events,
        restricted_tools,
        remaining_turns,
        step_recorder,
        current_session_id: current_session_id.as_ref(),
        max_turns,
        loop_turn: turn_index,
        recent_tools,
        last_heavy_checkpoint,
    }) {
        PostToolTurnOutcome::Abort(e) => Err(e),
        PostToolTurnOutcome::RetryLlmClearToolResults => {
            tool_results.clear();
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
        PostToolTurnOutcome::ProceedEndTurn => {
            step_recorder.end_turn(false);
            Ok(AgenticLoopTurnExit::ContinueIterating)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_stall_fires_when_last_three_turns_repeat_same_tool_names() {
        let tc = serde_json::json!({"name":"bash","arguments":{}});
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..3u32 {
            apply_stall_preflight(StallPreflightRequest {
                turn_index: i,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        assert_eq!(stall_events, vec![("name_stall".to_string(), 2)]);
    }
}
