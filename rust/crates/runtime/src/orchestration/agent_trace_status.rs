#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTraceLifecycleStatusKind {
    Spawned,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    Other,
}

pub const AGENT_TRACE_EVENT_SPAWNED: &str = "agent_spawned";
pub const AGENT_TRACE_EVENT_COMPLETED: &str = "agent_completed";
pub const AGENT_TRACE_EVENT_FAILED: &str = "agent_failed";
pub const AGENT_TRACE_EVENT_CANCELLED: &str = "agent_cancelled";
pub const AGENT_TRACE_EVENT_INTERRUPTED: &str = "agent_interrupted";
pub const AGENT_TRACE_EVENT_WAITING: &str = "agent_waiting";

pub const AGENT_TRACE_STATUS_SPAWNED: &str = "spawned";
pub const AGENT_TRACE_STATUS_RUNNING: &str = "running";
pub const AGENT_TRACE_STATUS_WAITING: &str = "waiting";
pub const AGENT_TRACE_STATUS_COMPLETED: &str = "completed";
pub const AGENT_TRACE_STATUS_FAILED: &str = "failed";
pub const AGENT_TRACE_STATUS_CANCELLED: &str = "cancelled";
pub const AGENT_TRACE_STATUS_INTERRUPTED: &str = "interrupted";

pub fn agent_trace_lifecycle_kind(status: &str) -> AgentTraceLifecycleStatusKind {
    match status {
        AGENT_TRACE_STATUS_SPAWNED => AgentTraceLifecycleStatusKind::Spawned,
        AGENT_TRACE_STATUS_RUNNING => AgentTraceLifecycleStatusKind::Running,
        AGENT_TRACE_STATUS_WAITING => AgentTraceLifecycleStatusKind::Waiting,
        AGENT_TRACE_STATUS_COMPLETED => AgentTraceLifecycleStatusKind::Completed,
        AGENT_TRACE_STATUS_FAILED => AgentTraceLifecycleStatusKind::Failed,
        AGENT_TRACE_STATUS_CANCELLED => AgentTraceLifecycleStatusKind::Cancelled,
        AGENT_TRACE_STATUS_INTERRUPTED => AgentTraceLifecycleStatusKind::Interrupted,
        _ => AgentTraceLifecycleStatusKind::Other,
    }
}

pub fn agent_trace_terminal_event_type(status: &str) -> &'static str {
    match agent_trace_lifecycle_kind(status) {
        AgentTraceLifecycleStatusKind::Completed => AGENT_TRACE_EVENT_COMPLETED,
        AgentTraceLifecycleStatusKind::Failed => AGENT_TRACE_EVENT_FAILED,
        AgentTraceLifecycleStatusKind::Cancelled => AGENT_TRACE_EVENT_CANCELLED,
        AgentTraceLifecycleStatusKind::Interrupted => AGENT_TRACE_EVENT_INTERRUPTED,
        AgentTraceLifecycleStatusKind::Waiting => AGENT_TRACE_EVENT_WAITING,
        AgentTraceLifecycleStatusKind::Spawned
        | AgentTraceLifecycleStatusKind::Running
        | AgentTraceLifecycleStatusKind::Other => AGENT_TRACE_EVENT_COMPLETED,
    }
}

pub fn agent_trace_status_from_event(
    event_type: &str,
    metadata_status: Option<&str>,
) -> Option<&'static str> {
    match event_type {
        AGENT_TRACE_EVENT_SPAWNED => Some(AGENT_TRACE_STATUS_SPAWNED),
        AGENT_TRACE_EVENT_COMPLETED => Some(AGENT_TRACE_STATUS_COMPLETED),
        AGENT_TRACE_EVENT_FAILED => Some(AGENT_TRACE_STATUS_FAILED),
        AGENT_TRACE_EVENT_CANCELLED => Some(AGENT_TRACE_STATUS_CANCELLED),
        AGENT_TRACE_EVENT_INTERRUPTED => Some(AGENT_TRACE_STATUS_INTERRUPTED),
        AGENT_TRACE_EVENT_WAITING => Some(AGENT_TRACE_STATUS_WAITING),
        _ => metadata_status.and_then(|status| match agent_trace_lifecycle_kind(status) {
            AgentTraceLifecycleStatusKind::Spawned => Some(AGENT_TRACE_STATUS_SPAWNED),
            AgentTraceLifecycleStatusKind::Running => Some(AGENT_TRACE_STATUS_RUNNING),
            AgentTraceLifecycleStatusKind::Waiting => Some(AGENT_TRACE_STATUS_WAITING),
            AgentTraceLifecycleStatusKind::Completed => Some(AGENT_TRACE_STATUS_COMPLETED),
            AgentTraceLifecycleStatusKind::Failed => Some(AGENT_TRACE_STATUS_FAILED),
            AgentTraceLifecycleStatusKind::Cancelled => Some(AGENT_TRACE_STATUS_CANCELLED),
            AgentTraceLifecycleStatusKind::Interrupted => Some(AGENT_TRACE_STATUS_INTERRUPTED),
            AgentTraceLifecycleStatusKind::Other => None,
        }),
    }
}

pub fn is_agent_trace_settled_event(event_type: &str) -> bool {
    matches!(
        event_type,
        AGENT_TRACE_EVENT_COMPLETED
            | AGENT_TRACE_EVENT_FAILED
            | AGENT_TRACE_EVENT_CANCELLED
            | AGENT_TRACE_EVENT_INTERRUPTED
            | AGENT_TRACE_EVENT_WAITING
    )
}

pub fn agent_trace_requires_result_collection(
    event_type: &str,
    metadata_status: Option<&str>,
) -> bool {
    matches!(
        agent_trace_status_from_event(event_type, metadata_status),
        Some(
            AGENT_TRACE_STATUS_COMPLETED
                | AGENT_TRACE_STATUS_FAILED
                | AGENT_TRACE_STATUS_CANCELLED
                | AGENT_TRACE_STATUS_INTERRUPTED
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_status_owner_distinguishes_settled_and_collectable_states() {
        assert_eq!(
            agent_trace_status_from_event(AGENT_TRACE_EVENT_WAITING, None),
            Some(AGENT_TRACE_STATUS_WAITING)
        );
        assert!(is_agent_trace_settled_event(AGENT_TRACE_EVENT_WAITING));
        assert!(!agent_trace_requires_result_collection(
            AGENT_TRACE_EVENT_WAITING,
            Some(AGENT_TRACE_STATUS_WAITING)
        ));
        assert!(agent_trace_requires_result_collection(
            AGENT_TRACE_EVENT_INTERRUPTED,
            Some(AGENT_TRACE_STATUS_INTERRUPTED)
        ));
    }

    #[test]
    fn trace_terminal_event_type_includes_interrupted_and_waiting() {
        assert_eq!(
            agent_trace_terminal_event_type(AGENT_TRACE_STATUS_INTERRUPTED),
            AGENT_TRACE_EVENT_INTERRUPTED
        );
        assert_eq!(
            agent_trace_terminal_event_type(AGENT_TRACE_STATUS_WAITING),
            AGENT_TRACE_EVENT_WAITING
        );
    }
}
