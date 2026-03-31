//! Fold one `TurnResult` from `consume_turn_sse` into loop state (telemetry, response guard, usage, no-tool exit).

use std::collections::HashSet;

use crossterm::style::Stylize;
use mo_agent_core::agent_warn;
use mo_agent_runtime::{
    pipeline::step_recorder::StepRecorder,
    turn::chat_turn_heuristics::{
        openai_factual_tool_retry_user_message, should_force_factual_tool_retry,
    },
    turn::response_guard::apply_response_guards,
};

use crate::stream_render::TurnResult;

pub(crate) struct TurnResultIngestRequest<'a> {
    pub turn_result: &'a TurnResult,
    pub message: &'a str,
    pub recent_tools: &'a [String],
    pub quiet: bool,
    pub first_ttft_ms: &'a mut Option<u64>,
    pub current_session_id: &'a mut Option<String>,
    pub current_run_id: &'a mut Option<String>,
    pub final_text: &'a mut String,
    pub total_prompt: &'a mut u64,
    pub total_completion: &'a mut u64,
    pub total_tool_calls: &'a mut u32,
    pub step_recorder: &'a mut StepRecorder,
    pub all_tools_used: &'a mut HashSet<String>,
    pub has_any_usage: &'a mut bool,
    pub forced_factual_retry: &'a mut bool,
    pub messages: &'a mut Vec<serde_json::Value>,
}

/// How the outer SSE loop should proceed after ingesting one turn.
pub(crate) enum TurnIngestOutcome {
    /// Leave the multi-turn loop (`break`).
    Break,
    /// Start the next loop iteration (`continue`) — e.g. factual-tool retry.
    Continue,
    /// Propagate SSE terminal error (`return Err`).
    Fatal(String),
    /// This round has tool work; run stall preflight + headless tool assembly.
    HasToolCalls,
}

pub(crate) fn ingest_turn_sse_result(ctx: TurnResultIngestRequest<'_>) -> TurnIngestOutcome {
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
