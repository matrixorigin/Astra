//! After a headless tool round: intent-drift nudge, TurnGuard verdict, checkpoint, retry/abort decisions.

use std::collections::HashSet;

use serde_json::Value;

use crate::chat_history_openai::append_openai_user_content_messages;
use crate::guardrails::turn_guard::{TurnGuard, VerdictSeverity};
use crate::guardrails::verdict_audit::AgenticVerdictAuditEvent;
use crate::interaction_types::TurnInteractionMode;
use crate::stall::{
    CLI_AGENTIC_VERDICT_REMAINING_PENALTY_CRITICAL, CLI_AGENTIC_VERDICT_REMAINING_PENALTY_WARNING,
};
use crate::tool::args::shape::{tool_call_arguments_value, tool_call_name};
use astra_pipeline::step_checkpoint;
use astra_pipeline::step_protocol::StepCheckpoint;
use astra_pipeline::step_recorder::StepRecorder;

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
    pub current_user_id: Option<&'a str>,
    pub current_session_id: Option<&'a String>,
    pub max_turns: usize,
    pub loop_turn: usize,
    pub recent_tools: &'a [String],
    pub last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
    pub interaction_mode: TurnInteractionMode,
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
        message: _,
        tool_calls_for_guard,
        intent_tool_turns,
        messages,
        stall_events: _,
        turn_guard,
        verdict_events,
        restricted_tools,
        remaining_turns,
        step_recorder,
        current_user_id,
        current_session_id,
        max_turns,
        loop_turn,
        recent_tools,
        last_heavy_checkpoint,
        interaction_mode,
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
            let health_avoidance_tools = turn_guard
                .health
                .health_avoidance_tools()
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
                health_avoidance_tools,
                force_stop: verdict.force_stop,
                nudge_count: turn_guard.nudge_count,
                interaction_mode: interaction_mode.label().to_string(),
                suppressed_loop_nudges: interaction_mode.suppresses_loop_nudges(),
                recent_error_pressure: turn_guard.errors.recent_error_pressure(),
                recent_timeout_pressure: turn_guard
                    .errors
                    .recent_error_count(crate::error_recovery::ErrorCategory::ToolTimeout),
                total_errors: turn_guard.errors.total_errors,
                health_avoidance_count: health_summary.health_avoidance_count,
                total_timeouts: health_summary.total_timeouts,
                timeout_dominant_tools,
                total_cache_hits: health_summary.total_cache_hits,
                flaky_count: health_summary.flaky_count,
            });
        }

        if verdict.severity >= VerdictSeverity::Warning {
            append_openai_user_content_messages(messages, &verdict.injections);
        }

        // `avoid_tools` is advisory stall-recovery guidance. Removing those
        // tools from the visible schema changes the model contract mid-turn and
        // can force it onto a worse surface. Hard restrictions are owned by
        // permission, interaction-mode, runtime allowlist, and resource-limit
        // enforcement, not by soft health diagnostics.

        match verdict.severity {
            VerdictSeverity::Critical => {
                *remaining_turns =
                    remaining_turns.saturating_sub(CLI_AGENTIC_VERDICT_REMAINING_PENALTY_CRITICAL);
            }
            VerdictSeverity::Warning => {
                // Progressive (linear) penalty: each consecutive warning adds
                // one base penalty on top of the previous one — 1st warning
                // costs -2, 2nd -4, 3rd -6, etc. Capped at MAX_MULT to keep
                // the penalty bounded and prevent u32 overflow on pathological
                // sessions; we also use saturating_mul defensively.
                const MAX_MULT: usize = 16;
                let multiplier = turn_guard.consecutive_warnings.clamp(1, MAX_MULT);
                let penalty =
                    CLI_AGENTIC_VERDICT_REMAINING_PENALTY_WARNING.saturating_mul(multiplier);
                *remaining_turns = remaining_turns.saturating_sub(penalty);
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

        let checkpoint_blocked_tools = checkpoint_blocked_tools(restricted_tools);
        if let (Some(user_id), Some(sid)) = (current_user_id, current_session_id)
            && let Some(heavy) = step_recorder.build_heavy_checkpoint(
                messages,
                0,
                max_turns.saturating_sub(loop_turn) as u32,
                &checkpoint_blocked_tools,
                recent_tools,
            )
        {
            let cp = StepCheckpoint::Heavy(Box::new(heavy));
            let _ = step_checkpoint::write_step_checkpoint(
                user_id,
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

fn checkpoint_blocked_tools(restricted_tools: &HashSet<String>) -> Vec<String> {
    let mut blocked_tools: Vec<String> = restricted_tools.iter().cloned().collect();
    blocked_tools.sort();
    blocked_tools.dedup();
    blocked_tools
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "test-user";
    use serde_json::json;

    #[test]
    fn healthy_guard_proceeds_end_turn() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence(TEST_USER_ID, "sid", "tid");
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
            current_user_id: None,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
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
    fn reward_hacking_warning_retries_without_schema_restriction() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence(TEST_USER_ID, "sid", "tid");
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
            current_user_id: None,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::RetryLlmClearToolResults);
        assert_eq!(remaining_turns, 8);
        // Reward-hacking guidance is advisory and must not hide the schema.
        assert!(
            !restricted_tools.contains("read_file"),
            "read-only tools must not be added to restricted_tools"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.to_string().contains("Reward-hacking guard"))
        );
        assert_eq!(verdict_events.len(), 1);
        assert_eq!(verdict_events[0].severity, "warning");
    }

    #[test]
    fn cache_waste_info_records_without_retry_or_restriction() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence(TEST_USER_ID, "sid", "tid");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}})];
        turn_guard.record_cache_hit("read_file");
        turn_guard.record_cache_hit("read_file");
        turn_guard.record_cache_hit("read_file");

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
            current_user_id: None,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::ProceedEndTurn);
        assert!(
            messages.is_empty(),
            "info-level cache guidance must not pollute model messages"
        );
        // read_file is read-only — the filter prevents it from entering restricted_tools.
        assert!(
            !restricted_tools.contains("read_file"),
            "read-only tools must not be added to restricted_tools"
        );
        assert_eq!(remaining_turns, 10);
        assert_eq!(verdict_events.len(), 1);
        assert_eq!(verdict_events[0].severity, "info");
    }

    #[test]
    fn advisory_avoid_tools_do_not_remove_visible_tool_schema() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence(TEST_USER_ID, "sid", "tid");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![
            json!({"name": "agent_fanout", "arguments": {"action": "get_results", "group_id": "review"}}),
            json!({"name": "agent_fanout", "arguments": {"action": "get_results", "group_id": "review"}}),
            json!({"name": "agent_fanout", "arguments": {"action": "get_results", "group_id": "review"}}),
        ];
        turn_guard.record_tool_calls(&tool_calls);

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            message: "collect review results",
            tool_calls_for_guard: &tool_calls,
            intent_tool_turns: &mut intent_tool_turns,
            messages: &mut messages,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::RetryLlmClearToolResults);
        assert_eq!(verdict_events.len(), 1);
        assert!(
            verdict_events[0]
                .avoid_tools
                .contains(&"agent_fanout".to_string())
        );
        assert!(
            !turn_guard.health.is_avoidance_advised("agent_fanout"),
            "stall advice alone must not mark the tool unhealthy"
        );
        assert!(
            !restricted_tools.contains("agent_fanout"),
            "advisory avoid_tools must not remove the tool schema"
        );
    }

    #[test]
    fn health_avoidance_tools_remain_same_turn_advisory() {
        let mut intent_tool_turns = Vec::new();
        let mut messages = Vec::new();
        let mut stall_events = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder = StepRecorder::with_persistence(TEST_USER_ID, "sid", "tid");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        for _ in 0..3 {
            turn_guard.health.record_failure("write_file");
        }

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            message: "run command",
            tool_calls_for_guard: &[],
            intent_tool_turns: &mut intent_tool_turns,
            messages: &mut messages,
            stall_events: &mut stall_events,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::RetryLlmClearToolResults);
        assert!(turn_guard.health.is_avoidance_advised("write_file"));
        assert!(
            !restricted_tools.contains("write_file"),
            "soft health-avoidance tools must not be hidden"
        );
    }

    #[test]
    fn checkpoint_blocked_tools_uses_only_hard_restrictions() {
        let mut restricted_tools = HashSet::new();
        restricted_tools.insert("bash".to_string());
        restricted_tools.insert("write_file".to_string());

        let mut turn_guard = TurnGuard::new();
        for _ in 0..3 {
            turn_guard.health.record_failure("flaky_soft_tool");
        }
        assert!(turn_guard.health.is_avoidance_advised("flaky_soft_tool"));

        let blocked = super::checkpoint_blocked_tools(&restricted_tools);
        assert_eq!(blocked, vec!["bash".to_string(), "write_file".to_string()]);
        assert!(
            !blocked.contains(&"flaky_soft_tool".to_string()),
            "soft tool-health avoidance must not persist as hard checkpoint blocked_tools"
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
        let mut step_recorder = StepRecorder::with_persistence(TEST_USER_ID, "sid", "tid");
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
            current_user_id: None,
            current_session_id: None,
            max_turns: 8,
            loop_turn: 0,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert_eq!(out, AgenticPostToolPolicyOutcome::ProceedEndTurn);
        assert_eq!(intent_tool_turns.len(), 1);
        assert_eq!(intent_tool_turns[0].0, vec!["bash".to_string()]);
        assert!(intent_tool_turns[0].1.contains("\"command\":\"ls\""));
    }
}
