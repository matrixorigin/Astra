pub(crate) use astra_turn_core::orchestration::agent_result_wire::AgentToolWireOutcomeKind as AgentControlWireOutcomeKind;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_error_message as agent_control_error_message;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_interrupted_message as agent_control_interrupted_message;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_result_output_summary as agent_control_result_output_summary;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_running_preview as agent_control_running_preview;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::project_agent_tool_wire as project_agent_control_wire;

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::agent_control_interrupted_message;
    use astra_turn_core::orchestration::agent_result_wire::AgentToolResultStatusKind;

    #[test]
    fn shared_agent_control_status_kind_roundtrips_via_from_str() {
        assert_eq!(
            AgentToolResultStatusKind::from_str("interrupted").unwrap(),
            AgentToolResultStatusKind::Interrupted
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("still_running").unwrap(),
            AgentToolResultStatusKind::StillRunning
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("launched").unwrap(),
            AgentToolResultStatusKind::Launched
        );
        assert!(AgentToolResultStatusKind::from_str("weird").is_err());
    }

    #[test]
    fn interrupted_message_uses_shared_wait_copy_for_get_result() {
        assert_eq!(
            agent_control_interrupted_message(true, Some("budget_exhausted")),
            "Needs continuation: The run reached its turn budget."
        );
        assert_eq!(
            agent_control_interrupted_message(false, Some("context_overflow")),
            "Needs compaction: The conversation exceeded the model context window."
        );
        assert_eq!(
            agent_control_interrupted_message(false, None),
            "Agent stopped before completing its result."
        );
    }
}
