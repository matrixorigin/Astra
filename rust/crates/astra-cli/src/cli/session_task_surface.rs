pub(crate) use astra_tools::task_mgmt::SessionTaskStatusKind;

pub(crate) fn session_task_is_active(status: SessionTaskStatusKind) -> bool {
    matches!(
        status,
        SessionTaskStatusKind::InProgress | SessionTaskStatusKind::Pending
    )
}

pub(crate) fn session_task_is_in_progress(status: SessionTaskStatusKind) -> bool {
    matches!(status, SessionTaskStatusKind::InProgress)
}

pub(crate) fn session_task_is_pending(status: SessionTaskStatusKind) -> bool {
    matches!(status, SessionTaskStatusKind::Pending)
}

pub(crate) fn session_task_is_completed(status: SessionTaskStatusKind) -> bool {
    matches!(status, SessionTaskStatusKind::Completed)
}

pub(crate) fn session_task_is_unsuccessful(status: SessionTaskStatusKind) -> bool {
    matches!(
        status,
        SessionTaskStatusKind::Failed | SessionTaskStatusKind::Cancelled
    )
}

pub(crate) fn session_task_status_marker(status: SessionTaskStatusKind) -> &'static str {
    match status {
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

pub(crate) fn session_task_active_priority(status: SessionTaskStatusKind) -> u8 {
    match status {
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
    fn session_task_status_helpers_keep_active_vs_terminal_split() {
        assert!(session_task_is_active(SessionTaskStatusKind::InProgress));
        assert!(session_task_is_active(SessionTaskStatusKind::Pending));
        assert!(!session_task_is_active(SessionTaskStatusKind::Completed));
        assert!(!session_task_is_active(SessionTaskStatusKind::Failed));
        assert!(!session_task_is_active(SessionTaskStatusKind::Cancelled));
        assert!(session_task_is_unsuccessful(SessionTaskStatusKind::Failed));
        assert!(session_task_is_unsuccessful(
            SessionTaskStatusKind::Cancelled
        ));
        assert_eq!(
            session_task_status_marker(SessionTaskStatusKind::InProgress),
            "▸"
        );
        assert_eq!(
            session_task_status_marker(SessionTaskStatusKind::Pending),
            "·"
        );
        assert_eq!(
            session_task_status_marker(SessionTaskStatusKind::Cancelled),
            "⏹"
        );
        assert_eq!(
            session_task_status_marker(SessionTaskStatusKind::Archived),
            "·"
        );
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
            2
        );
    }
}
