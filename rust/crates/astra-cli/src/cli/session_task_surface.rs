pub(crate) use astra_tools::task_mgmt::SessionTaskStatusKind;

pub(crate) fn session_task_status_kind(status: &str) -> SessionTaskStatusKind {
    astra_tools::task_mgmt::session_task_status_kind(status)
}

pub(crate) fn session_task_is_active(status: &str) -> bool {
    astra_tools::task_mgmt::session_task_is_active(status)
}

pub(crate) fn session_task_is_in_progress(status: &str) -> bool {
    astra_tools::task_mgmt::session_task_is_in_progress(status)
}

pub(crate) fn session_task_is_pending(status: &str) -> bool {
    astra_tools::task_mgmt::session_task_is_pending(status)
}

pub(crate) fn session_task_is_completed(status: &str) -> bool {
    astra_tools::task_mgmt::session_task_is_completed(status)
}

pub(crate) fn session_task_is_unsuccessful(status: &str) -> bool {
    astra_tools::task_mgmt::session_task_is_unsuccessful(status)
}

pub(crate) fn session_task_status_marker(status: &str) -> &'static str {
    match session_task_status_kind(status) {
        SessionTaskStatusKind::InProgress => "▸",
        SessionTaskStatusKind::Pending
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Other => "·",
        SessionTaskStatusKind::Completed => "✓",
        SessionTaskStatusKind::Failed => "✗",
        SessionTaskStatusKind::Cancelled => "⏹",
    }
}

pub(crate) fn session_task_active_priority(status: &str) -> u8 {
    match session_task_status_kind(status) {
        SessionTaskStatusKind::InProgress => 0,
        SessionTaskStatusKind::Pending => 1,
        SessionTaskStatusKind::Completed
        | SessionTaskStatusKind::Failed
        | SessionTaskStatusKind::Cancelled
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Other => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_task_status_helpers_classify_known_statuses() {
        assert_eq!(
            session_task_status_kind("in_progress"),
            SessionTaskStatusKind::InProgress
        );
        assert_eq!(
            session_task_status_kind("pending"),
            SessionTaskStatusKind::Pending
        );
        assert_eq!(
            session_task_status_kind("completed"),
            SessionTaskStatusKind::Completed
        );
        assert_eq!(
            session_task_status_kind("failed"),
            SessionTaskStatusKind::Failed
        );
        assert_eq!(
            session_task_status_kind("cancelled"),
            SessionTaskStatusKind::Cancelled
        );
        assert_eq!(
            session_task_status_kind("archived"),
            SessionTaskStatusKind::Archived
        );
        assert_eq!(
            session_task_status_kind("deleted"),
            SessionTaskStatusKind::Deleted
        );
        assert_eq!(
            session_task_status_kind("paused"),
            SessionTaskStatusKind::Other
        );
    }

    #[test]
    fn session_task_status_helpers_keep_active_vs_terminal_split() {
        assert!(session_task_is_active("in_progress"));
        assert!(session_task_is_active("pending"));
        assert!(!session_task_is_active("completed"));
        assert!(!session_task_is_active("failed"));
        assert!(!session_task_is_active("cancelled"));
        assert!(session_task_is_unsuccessful("failed"));
        assert!(session_task_is_unsuccessful("cancelled"));
        assert_eq!(session_task_status_marker("in_progress"), "▸");
        assert_eq!(session_task_status_marker("pending"), "·");
        assert_eq!(session_task_status_marker("cancelled"), "⏹");
        assert_eq!(session_task_status_marker("archived"), "·");
        assert_eq!(session_task_active_priority("in_progress"), 0);
        assert_eq!(session_task_active_priority("pending"), 1);
        assert_eq!(session_task_active_priority("completed"), 2);
    }
}
