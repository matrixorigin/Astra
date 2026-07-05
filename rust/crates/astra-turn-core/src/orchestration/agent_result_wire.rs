use super::types::{AgentStatus, agent_completion_is_interrupted, agent_finish_reason_text};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::str::FromStr;
use std::time::Duration;

/// Wire-level status of an agent tool result.
///
/// Serde round-trips to the lowercase wire strings (e.g. `"completed"`,
/// `"timeout"`, `"still_running"`) that LLMs see in JSON output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolResultStatusKind {
    Completed,
    Failed,
    #[serde(rename = "timeout")]
    TimedOut,
    Cancelled,
    Interrupted,
    Waiting,
    StillRunning,
    Launched,
    /// Catch-all for unknown wire statuses.
    #[serde(other)]
    Other,
}

impl AgentToolResultStatusKind {
    /// Return the serde wire name (the lowercase string used in JSON output).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timeout",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Waiting => "waiting",
            Self::StillRunning => "still_running",
            Self::Launched => "launched",
            Self::Other => "other",
        }
    }

    /// Wire-tolerant parser: any unrecognized status maps to `Other`.
    ///
    /// Use this from JSON-deserialization paths where dropping an unknown
    /// status would silently lose information from a peer running a different
    /// version. For typed call sites where you want to learn about a typo,
    /// use `FromStr::from_str` instead — it returns `Err` for unknowns.
    pub fn parse_wire(s: &str) -> Self {
        Self::from_str(s).unwrap_or(Self::Other)
    }
}

impl FromStr for AgentToolResultStatusKind {
    type Err = String;

    /// Parse a wire status string. Trims and lower-cases the input for
    /// resilience to upstream casing/whitespace variants. Truly unknown
    /// statuses produce an `Err` rather than silently mapping to `Other`,
    /// so callers must explicitly opt into the catch-all.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "timeout" => Ok(Self::TimedOut),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            "waiting" => Ok(Self::Waiting),
            "still_running" => Ok(Self::StillRunning),
            "launched" => Ok(Self::Launched),
            "other" => Ok(Self::Other),
            _ => Err(format!("unknown agent tool status: '{s}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentToolWireOutcomeKind {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
    Running,
    NoChange,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentToolWireProjection<'a> {
    pub outcome: AgentToolWireOutcomeKind,
    pub finish_reason: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub display_name_hint: Option<&'a str>,
    pub cancelled_reason: Option<&'a str>,
    pub has_result: bool,
}

pub const AGENT_RESULT_INTERRUPTED_ERROR: &str =
    "Agent did not return a final result before it was interrupted.";

pub fn project_agent_tool_wire<'a>(
    _action: &str,
    outer_tool_success: bool,
    parsed: Option<&'a Value>,
) -> AgentToolWireProjection<'a> {
    let status_kind = parsed
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .map(AgentToolResultStatusKind::parse_wire);
    let finish_reason = parsed
        .and_then(|value| value.get("finish_reason"))
        .and_then(Value::as_str);
    let has_result = parsed
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .is_some();
    let outcome = match status_kind {
        Some(AgentToolResultStatusKind::Completed) => AgentToolWireOutcomeKind::Completed,
        Some(AgentToolResultStatusKind::Failed) => AgentToolWireOutcomeKind::Failed,
        Some(AgentToolResultStatusKind::TimedOut) => AgentToolWireOutcomeKind::TimedOut,
        Some(AgentToolResultStatusKind::Cancelled) => AgentToolWireOutcomeKind::Cancelled,
        Some(AgentToolResultStatusKind::Interrupted) => AgentToolWireOutcomeKind::Interrupted,
        Some(
            AgentToolResultStatusKind::Waiting
            | AgentToolResultStatusKind::StillRunning
            | AgentToolResultStatusKind::Launched,
        ) => AgentToolWireOutcomeKind::Running,
        Some(AgentToolResultStatusKind::Other) => AgentToolWireOutcomeKind::Failed,
        _ if outer_tool_success && has_result => AgentToolWireOutcomeKind::Completed,
        _ if !outer_tool_success => AgentToolWireOutcomeKind::Failed,
        _ => AgentToolWireOutcomeKind::NoChange,
    };

    AgentToolWireProjection {
        outcome,
        finish_reason,
        agent_id: parsed
            .and_then(|value| value.get("agent_id"))
            .and_then(Value::as_str),
        display_name_hint: parsed.and_then(|value| {
            value
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| value.get("description").and_then(Value::as_str))
        }),
        cancelled_reason: parsed
            .and_then(|value| value.get("reason"))
            .and_then(Value::as_str),
        has_result,
    }
}

pub fn agent_tool_interrupted_message(is_result_wait: bool, finish_reason: Option<&str>) -> String {
    if is_result_wait {
        AGENT_RESULT_INTERRUPTED_ERROR.to_string()
    } else if let Some(reason) = finish_reason.filter(|value| !value.trim().is_empty()) {
        format!("agent interrupted: {reason}")
    } else {
        "agent interrupted".to_string()
    }
}

pub fn agent_tool_completed_result_text(parsed: &Value) -> Option<String> {
    match parsed
        .get("status")
        .and_then(Value::as_str)
        .map(AgentToolResultStatusKind::parse_wire)
    {
        Some(AgentToolResultStatusKind::Completed | AgentToolResultStatusKind::Interrupted) => {
            parsed
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        _ => None,
    }
}

pub fn agent_tool_result_output_summary(
    parsed: Option<&Value>,
    raw_output: Option<&str>,
) -> Option<String> {
    parsed
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or(raw_output)
        .map(agent_tool_result_preview)
}

pub fn agent_tool_error_message(parsed: Option<&Value>, fallback: &str) -> String {
    parsed
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

pub fn agent_tool_incomplete_reason(parsed: &Value) -> Option<String> {
    match parsed
        .get("status")
        .and_then(Value::as_str)
        .map(AgentToolResultStatusKind::parse_wire)
    {
        Some(AgentToolResultStatusKind::StillRunning) => {
            let detail = parsed
                .get("current_status")
                .and_then(Value::as_str)
                .unwrap_or("still running");
            Some(format!(
                "still running when the wait window expired ({detail})"
            ))
        }
        Some(AgentToolResultStatusKind::Launched) => {
            Some("launched and has not produced a child result yet".to_string())
        }
        Some(AgentToolResultStatusKind::TimedOut) => Some(
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("timed out while waiting for the child result")
                .to_string(),
        ),
        Some(AgentToolResultStatusKind::Failed) => Some(
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("child result retrieval failed")
                .to_string(),
        ),
        Some(AgentToolResultStatusKind::Waiting) => Some(format!(
            "child agent is waiting ({})",
            parsed
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("waiting")
        )),
        Some(AgentToolResultStatusKind::Cancelled) => Some(
            parsed
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("child agent was cancelled")
                .to_string(),
        ),
        Some(AgentToolResultStatusKind::Other) => Some(
            parsed
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("child agent returned an unknown status")
                .to_string(),
        ),
        _ => None,
    }
}

pub fn agent_tool_running_preview(parsed: &Value) -> Option<String> {
    match parsed
        .get("status")
        .and_then(Value::as_str)
        .map(AgentToolResultStatusKind::parse_wire)
    {
        Some(AgentToolResultStatusKind::StillRunning) => {
            let current_status = parsed
                .get("current_status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            let waited = parsed
                .get("waited_secs")
                .and_then(Value::as_u64)
                .map(|secs| format!(" after {secs}s"))
                .unwrap_or_default();
            let hint = parsed
                .get("hint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            Some(match hint {
                Some(hint) => format!("Agent is {current_status}{waited}. {hint}"),
                None => format!("Agent is {current_status}{waited}."),
            })
        }
        Some(AgentToolResultStatusKind::Launched) => {
            Some("Agent launched; waiting for get_result output.".to_string())
        }
        _ => None,
    }
}

pub fn agent_tool_status_summary(parsed: &Value) -> Option<String> {
    if let Some(result) = agent_tool_result_output_summary(Some(parsed), None) {
        return Some(one_line_preview(&result, 72));
    }
    if let Some(preview) = agent_tool_running_preview(parsed) {
        return Some(one_line_preview(&preview, 72));
    }
    if let Some(reason) = agent_tool_incomplete_reason(parsed) {
        return Some(one_line_preview(&reason, 72));
    }
    Some(one_line_preview(
        &agent_tool_error_message(Some(parsed), "agent result unavailable"),
        72,
    ))
}

pub fn render_completed_agent_result(
    agent_id: &str,
    result: &str,
    finish_reason: Option<&str>,
) -> String {
    let reason = agent_finish_reason_text(finish_reason);
    let interrupted = agent_completion_is_interrupted(Some(reason));
    let mut body = json!({
        "status": if interrupted {
            AgentToolResultStatusKind::Interrupted.as_str()
        } else {
            AgentToolResultStatusKind::Completed.as_str()
        },
        "agent_id": agent_id,
        "result": result,
        "finish_reason": reason,
        "incomplete": interrupted,
    });
    if interrupted {
        body["hint"] = json!(
            "The child agent stopped before fully finishing. Treat this as incomplete and either continue it or report the interruption explicitly."
        );
    }
    body.to_string()
}

pub fn render_wait_timeout_outcome(
    agent_id: &str,
    live_status: Option<&AgentStatus>,
    timeout: Duration,
) -> String {
    match live_status {
        Some(status) if !status.is_terminal() => json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "agent_id": agent_id,
            "current_status": format!("{status:?}"),
            "waited_secs": timeout.as_secs(),
            "hint": "The child agent is still working. Call `agent(action='get_result', agent_id=...)` again \
                    to continue waiting. Do NOT treat this as failure or fabricate \
                    what the child would have returned.",
        })
        .to_string(),
        _ => json!({
            "status": AgentToolResultStatusKind::TimedOut.as_str(),
            "agent_id": agent_id,
            "error": format!(
                "Agent '{agent_id}' did not complete within {}s and has no live state",
                timeout.as_secs()
            ),
        })
        .to_string(),
    }
}

pub fn render_wait_for_agent_status(agent_id: &str, status: &AgentStatus) -> String {
    match status {
        AgentStatus::Completed {
            result,
            finish_reason,
        } => render_completed_agent_result(agent_id, result, finish_reason.as_deref()),
        AgentStatus::Interrupted {
            partial_result,
            finish_reason,
        } => render_completed_agent_result(agent_id, partial_result, Some(finish_reason)),
        AgentStatus::Failed {
            error,
            finish_reason,
        } => {
            let reason = finish_reason.as_deref().unwrap_or(AgentToolResultStatusKind::Failed.as_str());
            json!({
                "status": AgentToolResultStatusKind::Failed.as_str(),
                "agent_id": agent_id,
                "error": error,
                "finish_reason": reason,
            })
            .to_string()
        }
        AgentStatus::Waiting { reason } => json!({
            "status": AgentToolResultStatusKind::Waiting.as_str(),
            "agent_id": agent_id,
            "reason": if reason.trim().is_empty() {
                "waiting".to_string()
            } else {
                reason.clone()
            },
            "hint": "The child agent is waiting for external input or executor recovery. Do not fabricate its result.",
        })
        .to_string(),
        AgentStatus::Cancelled { by_user, reason } => {
            let mut payload = json!({
                "status": AgentToolResultStatusKind::Cancelled.as_str(),
                "agent_id": agent_id,
                "reason": if reason.is_empty() {
                    "cancelled".to_string()
                } else {
                    reason.clone()
                },
                "cancelled_by_user": *by_user,
            });
            if *by_user {
                // Make it explicit so the LLM doesn't dutifully respawn
                // the work the user just killed. Without this, the LLM
                // observes "cancelled" and most models treat it as a
                // transient failure → immediately re-spawns, defeating
                // the user's intent.
                payload["instruction"] = json!(
                    "The user explicitly cancelled this sub-agent. \
                     Do NOT respawn it or retry the same work; treat \
                     this turn as the user's signal to change direction. \
                     If the original objective still needs attention, \
                     ask the user what to do next."
                );
            }
            payload.to_string()
        }
        AgentStatus::Initializing => json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": agent_id,
        })
        .to_string(),
        AgentStatus::Running { activity } => json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "agent_id": agent_id,
            "current_status": "running",
            "activity": activity,
            "hint": "The child agent is still working. Call `agent(action='get_result', agent_id=...)` again to continue waiting.",
        })
        .to_string(),
        AgentStatus::Idle => json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "agent_id": agent_id,
            "current_status": "idle",
            "hint": "The child agent is still running. Call `agent(action='get_result', agent_id=...)` again to continue waiting.",
        })
        .to_string(),
    }
}

pub fn render_unknown_agent_result(agent_id: &str, message: &str) -> String {
    render_agent_tool_error(Some(agent_id), message)
}

pub fn render_agent_tool_error(agent_id: Option<&str>, message: &str) -> String {
    render_agent_tool_error_with_kind(agent_id, message, None)
}

pub fn render_agent_tool_error_with_kind(
    agent_id: Option<&str>,
    message: &str,
    error_kind: Option<astra_core::ErrorKind>,
) -> String {
    let mut body = json!({
        "status": AgentToolResultStatusKind::Failed.as_str(),
        "error": message,
    });
    if let Some(error_kind) = error_kind {
        body["error_kind"] = json!(error_kind.as_str());
    }
    if let Some(agent_id) = agent_id {
        body["agent_id"] = json!(agent_id);
    }
    body.to_string()
}

fn agent_tool_result_preview(result: &str) -> String {
    const MAX_LINES: usize = 80;
    const MAX_CHARS: usize = 8_000;
    let mut out = String::new();
    for (idx, line) in result.lines().enumerate() {
        if idx >= MAX_LINES || out.len() > MAX_CHARS {
            out.push_str("\n…");
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if out.is_empty() {
        result.chars().take(MAX_CHARS).collect()
    } else {
        out
    }
}

fn one_line_preview(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    let mut out: String = first_line.chars().take(max_chars).collect();
    if first_line.chars().count() > max_chars || text.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_projection_covers_interrupted_running_legacy_and_tool_failure_paths() {
        let interrupted = json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "agent_id": "a1",
            "finish_reason": "budget_exhausted",
            "result": "partial"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&interrupted));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Interrupted);
        assert_eq!(projection.agent_id, Some("a1"));
        assert_eq!(projection.finish_reason, Some("budget_exhausted"));
        assert!(projection.has_result);

        let launched = json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "a1"
        });
        let projection = project_agent_tool_wire("get_result", true, Some(&launched));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Running);

        let legacy = json!({"agent_id": "a1", "result": "done"});
        let projection = project_agent_tool_wire("get_result", true, Some(&legacy));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Completed);

        let empty_success = json!({"agent_id": "a1"});
        let projection = project_agent_tool_wire("get_result", true, Some(&empty_success));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::NoChange);

        let unknown_status = json!({"status": "mystery", "agent_id": "a1"});
        let projection = project_agent_tool_wire("get_result", true, Some(&unknown_status));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Failed);

        let tool_failed = project_agent_tool_wire("spawn", false, None);
        assert_eq!(tool_failed.outcome, AgentToolWireOutcomeKind::Failed);
    }

    #[test]
    fn waiting_status_projects_as_incomplete_running_wire() {
        let waiting = json!({
            "status": AgentToolResultStatusKind::Waiting.as_str(),
            "agent_id": "a1",
            "reason": "executor_offline"
        });

        let projection = project_agent_tool_wire("get_result", true, Some(&waiting));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Running);
        assert_eq!(projection.agent_id, Some("a1"));
        assert_eq!(
            agent_tool_incomplete_reason(&waiting).as_deref(),
            Some("child agent is waiting (executor_offline)")
        );
    }

    #[test]
    fn render_wait_for_agent_status_preserves_waiting_reason_and_hint() {
        let rendered = render_wait_for_agent_status(
            "a1",
            &AgentStatus::Waiting {
                reason: "executor_offline".to_string(),
            },
        );
        let parsed: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(
            parsed["status"],
            AgentToolResultStatusKind::Waiting.as_str()
        );
        assert_eq!(parsed["agent_id"], "a1");
        assert_eq!(parsed["reason"], "executor_offline");
        assert!(
            parsed["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("Do not fabricate"))
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
            "agent_id": "a"
        });
        assert_eq!(
            agent_tool_running_preview(&launched).as_deref(),
            Some("Agent launched; waiting for get_result output.")
        );
    }

    #[test]
    fn unknown_status_via_from_str_is_error_but_via_serde_is_other() {
        // First-principles split: `from_str` is the typed call path — caller
        // is supposed to know what statuses exist, so unknown is a bug and
        // surfaces as `Err`. Serde wire deserialization (e.g. JSON arriving
        // from an out-of-version peer) routes unknowns to `Other` to keep
        // the wire backwards-tolerant rather than dropping the message.
        assert!(AgentToolResultStatusKind::from_str("mystery").is_err());
        let kind: AgentToolResultStatusKind =
            serde_json::from_value(serde_json::Value::String("mystery".into())).unwrap();
        assert_eq!(kind, AgentToolResultStatusKind::Other);
    }

    #[test]
    fn from_str_normalizes_case_and_whitespace() {
        assert_eq!(
            AgentToolResultStatusKind::from_str("Completed").unwrap(),
            AgentToolResultStatusKind::Completed
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("  STILL_RUNNING  ").unwrap(),
            AgentToolResultStatusKind::StillRunning
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("TIMEOUT").unwrap(),
            AgentToolResultStatusKind::TimedOut
        );
    }

    #[test]
    fn interrupted_message_uses_shared_wait_copy_for_get_result() {
        assert_eq!(
            agent_tool_interrupted_message(true, Some("budget_exhausted")),
            AGENT_RESULT_INTERRUPTED_ERROR
        );
        assert_eq!(
            agent_tool_interrupted_message(false, Some("context_overflow")),
            "agent interrupted: context_overflow"
        );
        assert_eq!(
            agent_tool_interrupted_message(false, None),
            "agent interrupted"
        );
    }

    #[test]
    fn status_summary_prefers_result_then_running_then_reason() {
        let completed = json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "result": "partial draft\nmore",
            "finish_reason": "budget_exhausted"
        });
        assert_eq!(
            agent_tool_status_summary(&completed).as_deref(),
            Some("partial draft…")
        );

        let launched = json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "a"
        });
        assert_eq!(
            agent_tool_status_summary(&launched).as_deref(),
            Some("Agent launched; waiting for get_result output.")
        );

        let failed = json!({
            "status": AgentToolResultStatusKind::Failed.as_str(),
            "error": "child result retrieval failed"
        });
        assert_eq!(
            agent_tool_status_summary(&failed).as_deref(),
            Some("child result retrieval failed")
        );
    }
}
