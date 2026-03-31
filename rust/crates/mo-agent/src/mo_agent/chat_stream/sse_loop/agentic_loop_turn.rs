//! One iteration of the multi-turn agentic SSE loop (fetch → ingest → stall preflight → tools → post-tool policy).

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use mo_agent_runtime::{
    pipeline::step_protocol::{InMemoryIdempotencyCache, StepCheckpoint},
    semantic_dedup::SemanticDedup,
    tool_registry::ToolRegistry,
    tool_selector::ToolSelector,
    turn::headless_tool_assembly::tool_calls_for_stall_guard,
    turn::turn_guard::TurnGuard,
};
use mo_agent_services::session_journal::ToolCallRecord;
use serde_json::Value;

use crate::{
    ExplainMode, VerdictEvent, edge_tools::ToolExecutor, permission_manager::PermissionManager,
    skill_instructions::SharedSkillRegistry,
};

use super::fetch_chat_turn_sse::{ChatTurnSseFetchRequest, fetch_chat_turn_sse};
use super::post_tool_round::{
    PostToolTurnOutcome, PostToolTurnRequest, apply_post_tool_turn_policy,
};
use super::prepare_turn_request::PrepareTurnTelemetry;
use super::stall_preflight::{StallPreflightRequest, apply_stall_preflight};
use super::tool_round::{HeadlessToolRoundRequest, run_headless_tool_round};
use super::turn_result_ingest::{
    TurnIngestOutcome, TurnResultIngestRequest, ingest_turn_sse_result,
};

/// Whether the outer `for` loop should keep iterating or stop.
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
    pub step_recorder: &'a mut mo_agent_runtime::pipeline::step_recorder::StepRecorder,
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

    let assembly_start = std::time::Instant::now();
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
    explain_turns.extend(turn_result.explain_turns);

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
