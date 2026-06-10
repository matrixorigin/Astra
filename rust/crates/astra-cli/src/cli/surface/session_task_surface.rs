use astra_tools::task_mgmt::SessionTaskStatusKind;

pub(crate) fn session_task_status_marker(status: SessionTaskStatusKind) -> &'static str {
    match status {
        SessionTaskStatusKind::InProgress => "▸",
        SessionTaskStatusKind::Paused => "⏸",
        SessionTaskStatusKind::Pending
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Migrated
        | SessionTaskStatusKind::Other => "·",
        SessionTaskStatusKind::Completed => "✓",
        SessionTaskStatusKind::Failed => "✗",
        SessionTaskStatusKind::Cancelled => "⏹",
    }
}

pub(crate) fn session_task_active_priority(status: SessionTaskStatusKind) -> u8 {
    match status {
        SessionTaskStatusKind::InProgress => 0,
        SessionTaskStatusKind::Pending => 1,
        SessionTaskStatusKind::Paused => 2,
        SessionTaskStatusKind::Completed
        | SessionTaskStatusKind::Failed
        | SessionTaskStatusKind::Cancelled
        | SessionTaskStatusKind::Archived
        | SessionTaskStatusKind::Deleted
        | SessionTaskStatusKind::Migrated
        | SessionTaskStatusKind::Other => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::session_task_active_priority;
    use astra_tools::task_mgmt::SessionTaskStatusKind;

    #[test]
    fn session_task_status_helpers_keep_active_vs_terminal_split() {
        assert!(SessionTaskStatusKind::InProgress.is_active());
        assert!(SessionTaskStatusKind::Pending.is_active());
        assert!(!SessionTaskStatusKind::Paused.is_active());
        assert!(SessionTaskStatusKind::Paused.is_open_work());
        assert!(!SessionTaskStatusKind::Completed.is_active());
        assert!(!SessionTaskStatusKind::Failed.is_active());
        assert!(!SessionTaskStatusKind::Cancelled.is_active());
        assert!(!SessionTaskStatusKind::Migrated.is_active());
        assert!(!SessionTaskStatusKind::Migrated.is_open_work());
        assert!(SessionTaskStatusKind::Failed.is_unsuccessful());
        assert!(SessionTaskStatusKind::Cancelled.is_unsuccessful());
        assert_eq!(SessionTaskStatusKind::InProgress.status_marker(), "▸");
        assert_eq!(SessionTaskStatusKind::Pending.status_marker(), "·");
        assert_eq!(SessionTaskStatusKind::Cancelled.status_marker(), "⏹");
        assert_eq!(SessionTaskStatusKind::Archived.status_marker(), "·");
        assert_eq!(SessionTaskStatusKind::Migrated.status_marker(), "·");
        assert_eq!(
            session_task_active_priority(SessionTaskStatusKind::InProgress),
            0
        );
        assert_eq!(
            session_task_active_priority(SessionTaskStatusKind::Pending),
            1
        );
        assert_eq!(
            session_task_active_priority(SessionTaskStatusKind::Completed),
            3
        );
        assert_eq!(
            session_task_active_priority(SessionTaskStatusKind::Migrated),
            3
        );
    }
}
