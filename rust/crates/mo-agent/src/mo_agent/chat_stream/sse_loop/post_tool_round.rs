//! After headless tool assembly: intent-drift nudge and TurnGuard verdict (injections, budget, checkpoints).

use std::collections::HashSet;

use mo_agent_runtime::{
    pipeline::step_checkpoint,
    pipeline::step_protocol::StepCheckpoint,
    pipeline::step_recorder::StepRecorder,
    turn::chat_history_openai::{append_openai_user_content_messages, openai_user_content_message},
    turn::stall::{IntentDrift, detect_intent_drift},
    turn::turn_guard::{TurnGuard, VerdictSeverity},
};

use crate::VerdictEvent;

pub(crate) struct PostToolTurnRequest<'a> {
    pub turn_index: u32,
    pub message: &'a str,
    pub tool_calls_for_guard: &'a [serde_json::Value],
    pub intent_tool_turns: &'a mut Vec<(Vec<String>, String)>,
    pub messages: &'a mut Vec<serde_json::Value>,
    pub stall_events: &'a mut Vec<(String, u32)>,
    pub turn_guard: &'a mut TurnGuard,
    pub verdict_events: &'a mut Vec<VerdictEvent>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub remaining_turns: &'a mut usize,
    pub step_recorder: &'a mut StepRecorder,
    pub current_session_id: Option<&'a String>,
    pub max_turns: usize,
    pub loop_turn: usize,
    pub recent_tools: &'a [String],
    pub last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
}

/// What the SSE loop should do after post-tool-turn policy runs.
pub(crate) enum PostToolTurnOutcome {
    /// Call `step_recorder.end_turn(false)` and proceed.
    ProceedEndTurn,
    /// Verdict already called `end_turn(false)`; clear tool results and `continue` the outer loop.
    RetryLlmClearToolResults,
    /// Fatal escalation; caller returns `Err`.
    Abort(String),
}

pub(crate) fn apply_post_tool_turn_policy(ctx: PostToolTurnRequest<'_>) -> PostToolTurnOutcome {
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

    // ── Intent drift detection ──
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

    // ── TurnGuard: unified non-happy-path evaluation ──
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
