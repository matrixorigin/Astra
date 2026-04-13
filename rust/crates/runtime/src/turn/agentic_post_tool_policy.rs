//! After a headless tool round: intent-drift nudge, TurnGuard verdict, checkpoint, retry/abort decisions.

use std::collections::HashSet;

use serde_json::Value;

use super::agentic_verdict_audit::AgenticVerdictAuditEvent;
use super::chat_history_openai::{
    append_openai_user_content_messages, openai_user_content_message,
};
use super::stall::{
    CLI_AGENTIC_VERDICT_REMAINING_PENALTY_CRITICAL, CLI_AGENTIC_VERDICT_REMAINING_PENALTY_WARNING,
    IntentDrift, detect_intent_drift,
};
use super::tool_call_shape::{tool_call_arguments_value, tool_call_name};
use super::turn_guard::{TurnGuard, VerdictSeverity};
use crate::pipeline::step_checkpoint;
use crate::pipeline::step_protocol::StepCheckpoint;
use crate::pipeline::step_recorder::StepRecorder;

pub struct AgenticPostToolPolicyRequest<'a> {
    pub turn_index: u32,
    pub message: &'a str,
    pub tool_calls_for_guard: &'a [Value],
    pub intent_tool_turns: &'a mut Vec<(Vec<String>, String)>,
    pub messages: &'a mut Vec<Value>,
    pub stall_events: &'a mut Vec<(String, u32)>,
    pub turn_guard: &'a mut TurnGuard,
    pub verdict_events: &'a mut Vec<AgenticVerdictAuditEvent>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub remaining_turns: &'a mut usize,
    pub step_recorder: &'a mut StepRecorder,
    pub current_session_id: Option<&'a String>,
    pub max_turns: usize,
    pub loop_turn: usize,
    pub recent_tools: &'a [String],
    pub last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgenticPostToolPolicyOutcome {
    ProceedEndTurn,
    RetryLlmClearToolResults,
    Abort(String),
}

/// Maps [`AgenticPostToolPolicyOutcome`] for host loop control (CLI maps to `AgenticLoopTurnExit`).
#[derive(Debug, PartialEq, Eq)]
pub enum AgenticPostToolIterationControl {
    ProceedEndTurn,
    RetryLlmClearToolResults,
    Abort(String),
}

#[must_use]
pub fn map_post_tool_policy_outcome(
    outcome: AgenticPostToolPolicyOutcome,
) -> AgenticPostToolIterationControl {
    match outcome {
        AgenticPostToolPolicyOutcome::Abort(s) => AgenticPostToolIterationControl::Abort(s),
        AgenticPostToolPolicyOutcome::RetryLlmClearToolResults => {
            AgenticPostToolIterationControl::RetryLlmClearToolResults
        }
        AgenticPostToolPolicyOutcome::ProceedEndTurn => {
            AgenticPostToolIterationControl::ProceedEndTurn
        }
    }
}

pub fn apply_agentic_post_tool_policy(
    ctx: AgenticPostToolPolicyRequest<'_>,
) -> AgenticPostToolPolicyOutcome {
    let AgenticPostToolPolicyRequest {
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
            .filter_map(|tc| tool_call_name(tc).map(String::from))
            .collect();
        let turn_args_text: String = tool_calls_for_guard
            .iter()
            .map(|tc| serde_json::to_string(&tool_call_arguments_value(tc)).unwrap_or_default())
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
            let deprioritized_tools = turn_guard
                .health
                .deprioritized_tools()
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>();
            let timeout_dominant_tools = turn_guard
                .health
                .timeout_dominant_tools()
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>();
            verdict_events.push(AgenticVerdictAuditEvent {
                turn: turn_index,
                severity: severity_str.to_string(),
                injections: verdict.injections.clone(),
                avoid_tools: verdict.avoid_tools.clone(),
                deprioritized_tools,
                force_stop: verdict.force_stop,
                nudge_count: turn_guard.nudge_count,
                total_errors: turn_guard.errors.total_errors,
                deprioritized_count: health_summary.deprioritized_count,
                total_timeouts: health_summary.total_timeouts,
                timeout_dominant_tools,
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
                *remaining_turns =
                    remaining_turns.saturating_sub(CLI_AGENTIC_VERDICT_REMAINING_PENALTY_CRITICAL);
            }
            VerdictSeverity::Warning => {
                *remaining_turns =
                    remaining_turns.saturating_sub(CLI_AGENTIC_VERDICT_REMAINING_PENALTY_WARNING);
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
            return AgenticPostToolPolicyOutcome::Abort(
                "Agent escalated to critical — too many errors and stalls. Aborting.".to_string(),
            );
        }

        if !verdict.injections.is_empty() && verdict.severity >= VerdictSeverity::Warning {
            step_recorder.end_turn(false);
            return AgenticPostToolPolicyOutcome::RetryLlmClearToolResults;
        }
    }

    AgenticPostToolPolicyOutcome::ProceedEndTurn
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn healthy_guard_proceeds_end_turn() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence("sid", "tid");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls: Vec<Value> = Vec::new();

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            message: "just say hi",
            tool_calls_for_guard: &tool_calls,
            intent_tool_turns: &mut intent_tool_turns,
            messages: &mut messages,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::ProceedEndTurn);
        assert_eq!(intent_tool_turns.len(), 1);
    }

    #[test]
    fn map_post_tool_outcome_round_trip_variants() {
        assert_eq!(
            map_post_tool_policy_outcome(AgenticPostToolPolicyOutcome::ProceedEndTurn),
            AgenticPostToolIterationControl::ProceedEndTurn
        );
        assert_eq!(
            map_post_tool_policy_outcome(AgenticPostToolPolicyOutcome::RetryLlmClearToolResults),
            AgenticPostToolIterationControl::RetryLlmClearToolResults
        );
        assert_eq!(
            map_post_tool_policy_outcome(AgenticPostToolPolicyOutcome::Abort("x".into())),
            AgenticPostToolIterationControl::Abort("x".into())
        );
    }

    #[test]
    fn reward_hacking_warning_retries_and_restricts_tools() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence("sid", "tid");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![
            json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
            json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
        ];
        turn_guard.record_tool_calls(&tool_calls);
        turn_guard.record_tool_result("read_file", "fn main() {}");
        turn_guard.record_tool_result("read_file", "fn main() {}");

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            message: "inspect the code",
            tool_calls_for_guard: &tool_calls,
            intent_tool_turns: &mut intent_tool_turns,
            messages: &mut messages,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::RetryLlmClearToolResults);
        assert_eq!(remaining_turns, 8);
        assert!(restricted_tools.contains("read_file"));
        assert!(
            messages
                .iter()
                .any(|message| message.to_string().contains("Reward-hacking guard"))
        );
        assert_eq!(verdict_events.len(), 1);
        assert_eq!(verdict_events[0].severity, "warning");
        assert!(
            verdict_events[0]
                .avoid_tools
                .contains(&"read_file".to_string())
        );
    }

    #[test]
    fn canonical_tool_call_shape_feeds_intent_tracking() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence("sid", "tid");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"ls\"}"
            }
        })];

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            message: "list the files",
            tool_calls_for_guard: &tool_calls,
            intent_tool_turns: &mut intent_tool_turns,
            messages: &mut messages,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::ProceedEndTurn);
        assert_eq!(intent_tool_turns.len(), 1);
        assert_eq!(intent_tool_turns[0].0, vec!["bash".to_string()]);
        assert!(intent_tool_turns[0].1.contains("\"command\":\"ls\""));
    }
}
