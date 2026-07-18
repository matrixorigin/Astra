use serde_json::Value;

use crate::cli::tool_result_status::tool_result_status_is_canonical_success;
use crate::tui::agent_control_status::{
    AgentControlWireOutcomeKind, agent_control_interrupted_message, agent_control_running_preview,
    project_agent_control_wire,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentControlOutcome {
    Completed,
    Failed(AgentControlFailureKind),
    Cancelled,
    Running,
    NoChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentControlFailureKind {
    TimedOut,
    AgentFailed,
    Interrupted,
    ToolFailed,
}

pub(crate) struct AgentControlSurface<'a> {
    parsed: Option<&'a Value>,
    finish_reason: Option<&'a str>,
    agent_id: Option<&'a str>,
    display_name_hint: Option<&'a str>,
    cancelled_reason: Option<&'a str>,
    is_result_wait: bool,
    outcome: AgentControlOutcome,
}

impl<'a> AgentControlSurface<'a> {
    pub(crate) fn from_wire(action: &str, outer_status: &str, parsed: Option<&'a Value>) -> Self {
        let is_result_wait = action == "get_result";
        let outer_success = tool_result_status_is_canonical_success(outer_status);
        let projection = project_agent_control_wire(action, outer_success, parsed);

        let outcome = match projection.outcome {
            AgentControlWireOutcomeKind::Completed => AgentControlOutcome::Completed,
            AgentControlWireOutcomeKind::Failed if outer_success => {
                AgentControlOutcome::Failed(AgentControlFailureKind::AgentFailed)
            }
            AgentControlWireOutcomeKind::Failed => {
                AgentControlOutcome::Failed(AgentControlFailureKind::ToolFailed)
            }
            AgentControlWireOutcomeKind::TimedOut => {
                AgentControlOutcome::Failed(AgentControlFailureKind::TimedOut)
            }
            AgentControlWireOutcomeKind::Cancelled => AgentControlOutcome::Cancelled,
            AgentControlWireOutcomeKind::Interrupted => {
                AgentControlOutcome::Failed(AgentControlFailureKind::Interrupted)
            }
            AgentControlWireOutcomeKind::Running => AgentControlOutcome::Running,
            AgentControlWireOutcomeKind::NoChange => AgentControlOutcome::NoChange,
        };

        Self {
            parsed,
            finish_reason: projection.finish_reason,
            agent_id: projection.agent_id,
            display_name_hint: projection.display_name_hint,
            cancelled_reason: projection.cancelled_reason,
            is_result_wait,
            outcome,
        }
    }

    pub(crate) fn outcome(&self) -> AgentControlOutcome {
        self.outcome
    }

    pub(crate) fn agent_id(&self) -> Option<&'a str> {
        self.agent_id
    }

    pub(crate) fn display_name_hint(&self) -> Option<&'a str> {
        self.display_name_hint
    }

    pub(crate) fn cancelled_reason(&self) -> Option<&'a str> {
        self.cancelled_reason
    }

    pub(crate) fn failure_message(&self) -> Option<String> {
        match self.outcome {
            AgentControlOutcome::Failed(AgentControlFailureKind::TimedOut) => {
                Some("agent result timed out".to_string())
            }
            AgentControlOutcome::Failed(AgentControlFailureKind::AgentFailed) => {
                Some("agent failed".to_string())
            }
            AgentControlOutcome::Failed(AgentControlFailureKind::Interrupted) => Some(
                agent_control_interrupted_message(self.is_result_wait, self.finish_reason),
            ),
            AgentControlOutcome::Failed(AgentControlFailureKind::ToolFailed) => {
                Some("agent control tool failed".to_string())
            }
            _ => None,
        }
    }

    pub(crate) fn running_preview(&self) -> Option<String> {
        self.parsed.and_then(agent_control_running_preview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn result_without_status_is_completed_when_outer_status_is_completed() {
        let parsed = parse(r#"{"agent_id":"reviewer@abc","result":"done"}"#);
        let surface = AgentControlSurface::from_wire("get_result", "completed", Some(&parsed));
        assert_eq!(surface.outcome(), AgentControlOutcome::Completed);
    }

    #[test]
    fn result_without_status_fails_closed_when_outer_status_is_alias() {
        let parsed = parse(r#"{"agent_id":"reviewer@abc","result":"done"}"#);
        let surface = AgentControlSurface::from_wire("get_result", "ok", Some(&parsed));
        assert_eq!(
            surface.outcome(),
            AgentControlOutcome::Failed(AgentControlFailureKind::ToolFailed)
        );
    }

    #[test]
    fn tool_failure_without_status_is_control_failure() {
        let surface = AgentControlSurface::from_wire("spawn", "failed", None);
        assert_eq!(
            surface.outcome(),
            AgentControlOutcome::Failed(AgentControlFailureKind::ToolFailed)
        );
        assert_eq!(
            surface.failure_message().as_deref(),
            Some("agent control tool failed")
        );
    }

    #[test]
    fn empty_completed_get_result_does_not_complete_agent() {
        let parsed = parse(r#"{"agent_id":"reviewer@abc"}"#);
        let surface = AgentControlSurface::from_wire("get_result", "completed", Some(&parsed));
        assert_eq!(surface.outcome(), AgentControlOutcome::NoChange);
    }

    #[test]
    fn unknown_wire_status_fails_closed() {
        let parsed = parse(r#"{"status":"mystery","agent_id":"reviewer@abc"}"#);
        let surface = AgentControlSurface::from_wire("get_result", "completed", Some(&parsed));
        assert_eq!(
            surface.outcome(),
            AgentControlOutcome::Failed(AgentControlFailureKind::AgentFailed)
        );
    }

    #[test]
    fn cancelled_status_is_typed_cancelled_outcome() {
        let parsed = parse(r#"{"status":"cancelled","agent_id":"reviewer@abc"}"#);
        let surface = AgentControlSurface::from_wire("spawn", "completed", Some(&parsed));
        assert_eq!(surface.outcome(), AgentControlOutcome::Cancelled);
    }

    #[test]
    fn still_running_preview_uses_wait_fields() {
        let parsed = parse(
            r#"{"status":"still_running","agent_id":"reviewer@abc","current_status":"running","waited_secs":120,"hint":"call again"}"#,
        );
        let surface = AgentControlSurface::from_wire("get_result", "completed", Some(&parsed));
        assert_eq!(surface.outcome(), AgentControlOutcome::Running);
        assert_eq!(
            surface.running_preview().as_deref(),
            Some("Agent is running after 120s. call again")
        );
    }

    #[test]
    fn interrupted_get_result_is_failed_and_uses_shared_interrupted_copy() {
        let parsed = parse(
            r#"{"status":"interrupted","agent_id":"reviewer@abc","finish_reason":"budget_exhausted"}"#,
        );
        let surface = AgentControlSurface::from_wire("get_result", "completed", Some(&parsed));
        assert_eq!(
            surface.outcome(),
            AgentControlOutcome::Failed(AgentControlFailureKind::Interrupted)
        );
        assert_eq!(
            surface.failure_message().as_deref(),
            Some("Needs continuation: The run reached its turn budget.")
        );
    }

    #[test]
    fn interrupted_spawn_is_failed_and_preserves_finish_reason_copy() {
        let parsed = parse(
            r#"{"status":"interrupted","agent_id":"reviewer@abc","finish_reason":"budget_exhausted","result":"partial findings"}"#,
        );
        let surface = AgentControlSurface::from_wire("spawn", "completed", Some(&parsed));
        assert_eq!(
            surface.outcome(),
            AgentControlOutcome::Failed(AgentControlFailureKind::Interrupted)
        );
        assert_eq!(
            surface.failure_message().as_deref(),
            Some(
                crate::tui::agent_control_status::agent_control_interrupted_message(
                    false,
                    Some("budget_exhausted"),
                )
            )
            .as_deref()
        );
    }
}
