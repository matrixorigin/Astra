use astra_services::session_journal::ToolCallRecord;
use serde_json::Value;

#[cfg(test)]
use astra_turn_core::orchestration_types::AgentStatus;
#[cfg(test)]
use serde_json::json;

use astra_turn_core::orchestration::agent_result_wire::{
    AgentToolResultStatusKind, AgentToolWireOutcomeKind, AgentToolWireProjection,
    agent_tool_completed_result_text, agent_tool_error_message, agent_tool_incomplete_reason,
    agent_tool_interrupted_message, agent_tool_result_output_summary, agent_tool_running_preview,
    agent_tool_status_summary, project_agent_tool_wire, render_agent_tool_error,
    render_completed_agent_result, render_unknown_agent_result, render_wait_for_agent_status,
    render_wait_timeout_outcome,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentToolRecordActionKind {
    Spawn,
    GetResult,
    Other,
}

#[derive(Debug, Clone)]
pub struct AgentToolRecordProjection {
    pub action: AgentToolRecordActionKind,
    pub agent_id: Option<String>,
    pub display_name_hint: Option<String>,
    pub parsed_result: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct AgentToolBudgetRecordProjection {
    pub action: AgentToolRecordActionKind,
    pub agent_id: Option<String>,
    pub display_name_hint: Option<String>,
    pub completed_result: Option<String>,
    pub incomplete_reason: Option<String>,
    pub control_error_summary: Option<String>,
}

pub fn project_agent_tool_record(record: &ToolCallRecord) -> AgentToolRecordProjection {
    let parsed_args = record
        .args_full
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let parsed_result = record
        .result_full
        .as_deref()
        .or(record.result_preview.as_deref())
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let action = parsed_args
        .as_ref()
        .and_then(|value| value.get("action"))
        .and_then(Value::as_str)
        .or(record.args_preview.as_deref())
        .map(|action| match action {
            "spawn" => AgentToolRecordActionKind::Spawn,
            "get_result" => AgentToolRecordActionKind::GetResult,
            _ => AgentToolRecordActionKind::Other,
        })
        .unwrap_or(AgentToolRecordActionKind::Other);
    let agent_id = parsed_args
        .as_ref()
        .and_then(|value| value.get("agent_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            parsed_result
                .as_ref()
                .and_then(|value| value.get("agent_id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    let display_name_hint = parsed_result
        .as_ref()
        .and_then(agent_tool_display_name_hint)
        .or_else(|| parsed_args.as_ref().and_then(agent_tool_display_name_hint));

    AgentToolRecordProjection {
        action,
        agent_id,
        display_name_hint,
        parsed_result,
    }
}

pub fn project_agent_tool_budget_record(
    record: &ToolCallRecord,
) -> AgentToolBudgetRecordProjection {
    let projection = project_agent_tool_record(record);
    let mut completed_result = None;
    let mut incomplete_reason = None;
    if let Some(parsed) = projection.parsed_result.as_ref() {
        if let Some(result) = agent_tool_completed_result_text(parsed) {
            completed_result = Some(result);
        } else {
            incomplete_reason = agent_tool_incomplete_reason(parsed);
        }
    }

    AgentToolBudgetRecordProjection {
        action: projection.action,
        agent_id: projection.agent_id,
        display_name_hint: projection.display_name_hint,
        completed_result,
        incomplete_reason,
        control_error_summary: record
            .error
            .as_deref()
            .map(summarize_agent_tool_control_error),
    }
}

pub fn summarize_agent_tool_budget_result(text: &str) -> String {
    const MAX_CHARS: usize = 320;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        trimmed.to_string()
    } else {
        let mut clipped: String = trimmed.chars().take(MAX_CHARS.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    }
}

pub fn render_agent_tool_budget_unfinished_detail(
    incomplete_reason: Option<&str>,
    control_errors: &[String],
    cancelled_by_parent_budget: bool,
) -> String {
    let mut detail = incomplete_reason
        .map(str::to_string)
        .unwrap_or_else(|| "did not finish before the turn budget was exhausted".to_string());
    if cancelled_by_parent_budget {
        detail.push_str(
            "; the parent turn budget was exhausted and the parent cancelled this sub-agent",
        );
    }
    if !control_errors.is_empty() {
        detail.push_str("; ");
        detail.push_str(&control_errors.join("; "));
    }
    detail
}

fn agent_tool_display_name_hint(value: &Value) -> Option<String> {
    value
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| value.get("name").and_then(Value::as_str))
        .map(str::to_string)
}

fn summarize_agent_tool_control_error(error: &str) -> String {
    let first_line = error.lines().next().unwrap_or(error).trim();
    match agent_tool_control_error_tag(first_line) {
        Some("duplicate_within_turn") => "same-turn retries hit duplicate_within_turn".to_string(),
        Some("blocked_tool") => {
            "later retries were blocked after the tool was restricted".to_string()
        }
        _ => first_line.to_string(),
    }
}

fn agent_tool_control_error_tag(first_line: &str) -> Option<&str> {
    let tag = first_line
        .split_once(':')
        .map(|(tag, _)| tag.trim())
        .unwrap_or(first_line);
    match tag {
        "duplicate_within_turn" | "blocked_tool" => Some(tag),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::time::Duration;

    #[test]
    fn shared_agent_tool_status_kind_roundtrips_via_from_str() {
        assert_eq!(
            AgentToolResultStatusKind::from_str(AgentToolResultStatusKind::Interrupted.as_str())
                .unwrap(),
            AgentToolResultStatusKind::Interrupted
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str(AgentToolResultStatusKind::StillRunning.as_str())
                .unwrap(),
            AgentToolResultStatusKind::StillRunning
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str(AgentToolResultStatusKind::Launched.as_str())
                .unwrap(),
            AgentToolResultStatusKind::Launched
        );
        // Strict typed parse: unknown is Err. Wire-tolerant parse: Other.
        assert!(AgentToolResultStatusKind::from_str("weird").is_err());
        assert_eq!(
            AgentToolResultStatusKind::parse_wire("weird"),
            AgentToolResultStatusKind::Other
        );
    }

    #[test]
    fn interrupted_message_uses_shared_wait_copy_for_get_result() {
        assert_eq!(
            agent_tool_interrupted_message(true, Some("budget_exhausted")),
            "Needs continuation: The run reached its turn budget."
        );
        assert_eq!(
            agent_tool_interrupted_message(false, Some("context_overflow")),
            "Needs compaction: The conversation exceeded the model context window."
        );
        assert_eq!(
            agent_tool_interrupted_message(false, None),
            "Agent stopped before completing its result."
        );
    }

    #[test]
    fn completed_result_reports_budget_exhaustion_as_interrupted() {
        let out =
            render_completed_agent_result("reviewer-tests", "partial", Some("budget_exhausted"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], AgentToolResultStatusKind::Interrupted.as_str());
        assert_eq!(v["incomplete"], true);
    }

    #[test]
    fn completed_result_keeps_normal_completion_completed() {
        let out = render_completed_agent_result("reviewer-tests", "done", Some("normal"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], AgentToolResultStatusKind::Completed.as_str());
        assert_eq!(v["incomplete"], false);
    }

    #[test]
    fn wait_timeout_still_running_when_live_state_non_terminal() {
        let status = AgentStatus::Running {
            activity: "review".into(),
        };
        let out =
            render_wait_timeout_outcome("reviewer-tests", Some(&status), Duration::from_secs(120));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["status"],
            AgentToolResultStatusKind::StillRunning.as_str()
        );
        assert_eq!(v["delivery"], "asynchronous_parent_mailbox");
        assert!(
            v["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("Do not busy-poll")),
            "{v}"
        );
    }

    #[test]
    fn wait_timeout_reports_terminal_states_as_timeout_not_still_running() {
        let terminals = [
            AgentStatus::Completed {
                result: "done".into(),
                finish_reason: Some("normal".into()),
            },
            AgentStatus::Failed {
                error: "boom".into(),
                finish_reason: Some("failed".into()),
            },
            AgentStatus::cancelled_anonymous(),
        ];
        for terminal in terminals {
            let out = render_wait_timeout_outcome("ag", Some(&terminal), Duration::from_secs(30));
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["status"], AgentToolResultStatusKind::TimedOut.as_str());
        }
    }

    #[test]
    fn wait_for_agent_status_renders_failed_live_and_cancelled_paths() {
        let failed = AgentStatus::Failed {
            error: "boom".into(),
            finish_reason: Some("failed".into()),
        };
        let failed_out = render_wait_for_agent_status("ag", &failed);
        let failed_json: serde_json::Value = serde_json::from_str(&failed_out).unwrap();
        assert_eq!(
            failed_json["status"],
            AgentToolResultStatusKind::Failed.as_str()
        );
        assert_eq!(failed_json["error"], "boom");

        let idle = AgentStatus::Idle;
        let idle_out = render_wait_for_agent_status("ag", &idle);
        let idle_json: serde_json::Value = serde_json::from_str(&idle_out).unwrap();
        assert_eq!(
            idle_json["status"],
            AgentToolResultStatusKind::StillRunning.as_str()
        );
        assert_eq!(idle_json["current_status"], "idle");

        let initializing = AgentStatus::Initializing;
        let init_out = render_wait_for_agent_status("ag", &initializing);
        let init_json: serde_json::Value = serde_json::from_str(&init_out).unwrap();
        assert_eq!(
            init_json["status"],
            AgentToolResultStatusKind::Launched.as_str()
        );

        let cancelled = AgentStatus::cancelled_anonymous();
        let cancelled_out = render_wait_for_agent_status("ag", &cancelled);
        let cancelled_json: serde_json::Value = serde_json::from_str(&cancelled_out).unwrap();
        assert_eq!(
            cancelled_json["status"],
            AgentToolResultStatusKind::Cancelled.as_str()
        );
        // Anonymous cancel must NOT carry the user-instruction —
        // that's reserved for user-driven Ctrl+G x / `/agent cancel`.
        assert_eq!(cancelled_json["cancelled_by_user"], false);
        assert!(cancelled_json.get("instruction").is_none());

        let user_cancelled = AgentStatus::cancelled_by_user("user-requested via Ctrl+G x");
        let user_out = render_wait_for_agent_status("ag", &user_cancelled);
        let user_json: serde_json::Value = serde_json::from_str(&user_out).unwrap();
        assert_eq!(user_json["cancelled_by_user"], true);
        assert!(
            user_json["instruction"]
                .as_str()
                .is_some_and(|s| s.contains("Do NOT respawn")),
            "user-cancelled wire output must include explicit instruction so the LLM doesn't respawn: {user_json}"
        );
        assert_eq!(user_json["reason"], "user-requested via Ctrl+G x");
    }

    #[test]
    fn completed_text_extracts_interrupted_partial_results() {
        let parsed = json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "agent_id": "reviewer@abc",
            "result": "partial findings",
            "finish_reason": "budget_exhausted"
        });
        assert_eq!(
            agent_tool_completed_result_text(&parsed).as_deref(),
            Some("partial findings")
        );
    }

    #[test]
    fn incomplete_reason_covers_live_cancelled_and_unknown_states() {
        let launched =
            json!({"status": AgentToolResultStatusKind::Launched.as_str(), "agent_id": "a"});
        assert_eq!(
            agent_tool_incomplete_reason(&launched).as_deref(),
            Some("launched and has not produced a child result yet")
        );

        let cancelled = json!({"status": AgentToolResultStatusKind::Cancelled.as_str(), "reason": "parent cancelled this sub-agent"});
        assert_eq!(
            agent_tool_incomplete_reason(&cancelled).as_deref(),
            Some("parent cancelled this sub-agent")
        );

        let unknown = json!({"status": "mystery", "detail": "legacy edge status"});
        assert_eq!(
            agent_tool_incomplete_reason(&unknown).as_deref(),
            Some("legacy edge status")
        );
    }

    #[test]
    fn running_preview_covers_still_running_and_launched() {
        let still_running = json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "current_status": "running",
            "waited_secs": 120,
            "hint": "call again"
        });
        assert_eq!(
            agent_tool_running_preview(&still_running).as_deref(),
            Some("Agent is running after 120s. call again")
        );

        let launched = json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "reviewer@abc"
        });
        assert_eq!(
            agent_tool_running_preview(&launched).as_deref(),
            Some("Agent launched; waiting for get_result output.")
        );
    }

    #[test]
    fn result_output_summary_prefers_parsed_result_and_falls_back_to_raw_output() {
        let parsed = json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "result": "partial findings"
        });
        assert_eq!(
            agent_tool_result_output_summary(Some(&parsed), Some("raw fallback")).as_deref(),
            Some("partial findings")
        );

        assert_eq!(
            agent_tool_result_output_summary(None, Some("raw fallback")).as_deref(),
            Some("raw fallback")
        );
    }

    #[test]
    fn error_message_prefers_payload_error_and_falls_back() {
        let parsed = json!({
            "status": AgentToolResultStatusKind::Failed.as_str(),
            "error": "child exploded"
        });
        assert_eq!(
            agent_tool_error_message(Some(&parsed), "fallback"),
            "child exploded"
        );
        assert_eq!(agent_tool_error_message(None, "fallback"), "fallback");
    }

    #[test]
    fn wire_projection_covers_interrupted_running_legacy_and_tool_failure_paths() {
        let interrupted = json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "agent_id": "reviewer@abc",
            "finish_reason": "budget_exhausted"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&interrupted));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Interrupted);
        assert_eq!(projection.agent_id, Some("reviewer@abc"));
        assert_eq!(projection.finish_reason, Some("budget_exhausted"));

        let launched = json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "reviewer@abc",
            "description": "Architecture review"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&launched));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Running);
        assert_eq!(projection.display_name_hint, Some("Architecture review"));

        let legacy = json!({
            "agent_id": "reviewer@abc",
            "result": "done"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&legacy));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Completed);
        assert!(projection.has_result);

        let empty_success = json!({
            "agent_id": "reviewer@abc"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&empty_success));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::NoChange);

        let unknown_status = json!({
            "status": "mystery",
            "agent_id": "reviewer@abc"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&unknown_status));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Failed);

        let tool_failed = project_agent_tool_wire("spawn", false, None);
        assert_eq!(tool_failed.outcome, AgentToolWireOutcomeKind::Failed);
    }

    #[test]
    fn record_projection_prefers_args_full_action_and_agent_identity() {
        let record = ToolCallRecord {
            name: "agent".into(),
            ok: true,
            ms: 0,
            args_full: Some(
                json!({
                    "action": "spawn",
                    "agent_id": "reviewer@abc",
                    "description": "Architecture review"
                })
                .to_string(),
            ),
            result_full: Some(
                json!({
                    "status": AgentToolResultStatusKind::Launched.as_str(),
                    "description": "Launched child"
                })
                .to_string(),
            ),
            ..Default::default()
        };

        let projection = project_agent_tool_record(&record);
        assert_eq!(projection.action, AgentToolRecordActionKind::Spawn);
        assert_eq!(projection.agent_id.as_deref(), Some("reviewer@abc"));
        assert_eq!(
            projection.display_name_hint.as_deref(),
            Some("Launched child")
        );
    }

    #[test]
    fn render_agent_tool_error_optionally_includes_agent_id() {
        let without_id: Value =
            serde_json::from_str(&render_agent_tool_error(None, "boom")).unwrap();
        assert_eq!(
            without_id["status"],
            AgentToolResultStatusKind::Failed.as_str()
        );
        assert_eq!(without_id["error"], "boom");
        assert!(without_id.get("agent_id").is_none());

        let with_id: Value =
            serde_json::from_str(&render_agent_tool_error(Some("agent-1"), "boom")).unwrap();
        assert_eq!(with_id["agent_id"], "agent-1");
    }

    #[test]
    fn budget_record_projection_summarizes_control_error_and_incomplete_state() {
        let record = ToolCallRecord {
            name: "agent".into(),
            ok: false,
            ms: 0,
            args_full: Some(
                json!({
                    "action": "get_result",
                    "agent_id": "reviewer@abc",
                    "description": "Architecture review"
                })
                .to_string(),
            ),
            result_full: Some(
                json!({
                    "status": AgentToolResultStatusKind::Launched.as_str(),
                    "agent_id": "reviewer@abc"
                })
                .to_string(),
            ),
            error: Some("duplicate_within_turn: blocked".into()),
            ..Default::default()
        };

        let projection = project_agent_tool_budget_record(&record);
        assert_eq!(projection.action, AgentToolRecordActionKind::GetResult);
        assert_eq!(projection.agent_id.as_deref(), Some("reviewer@abc"));
        assert_eq!(
            projection.display_name_hint.as_deref(),
            Some("Architecture review")
        );
        assert!(projection.completed_result.is_none());
        assert_eq!(
            projection.incomplete_reason.as_deref(),
            Some("launched and has not produced a child result yet")
        );
        assert_eq!(
            projection.control_error_summary.as_deref(),
            Some("same-turn retries hit duplicate_within_turn")
        );
    }

    #[test]
    fn budget_result_summary_clips_long_text() {
        let long = "a".repeat(400);
        let summarized = summarize_agent_tool_budget_result(&long);
        assert_eq!(summarized.chars().count(), 320);
        assert!(summarized.ends_with('…'));
    }

    #[test]
    fn unfinished_budget_detail_combines_reason_cancellation_and_control_errors() {
        let detail = render_agent_tool_budget_unfinished_detail(
            Some("launched and has not produced a child result yet"),
            &[
                "same-turn retries hit duplicate_within_turn".to_string(),
                "later retries were blocked after the tool was restricted".to_string(),
            ],
            true,
        );
        assert_eq!(
            detail,
            "launched and has not produced a child result yet; the parent turn budget was exhausted and the parent cancelled this sub-agent; same-turn retries hit duplicate_within_turn; later retries were blocked after the tool was restricted"
        );
    }
}
