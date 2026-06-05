pub(crate) use astra_tools::task_mgmt::SessionTaskStatusKind;

/// By-value convenience wrappers delegating to `task_mgmt` reference-based functions.
/// `SessionTaskStatusKind` is Copy, so callers frequently pass `t.status` by value
/// rather than `&t.status`.

pub(crate) fn session_task_is_active(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_active(&status)
}

pub(crate) fn session_task_is_in_progress(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_in_progress(&status)
}

pub(crate) fn session_task_is_pending(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_pending(&status)
}

pub(crate) fn session_task_is_completed(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_completed(&status)
}

pub(crate) fn session_task_is_failed(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_failed(&status)
}

pub(crate) fn session_task_is_cancelled(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_cancelled(&status)
}

pub(crate) fn session_task_is_unsuccessful(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_unsuccessful(&status)
}

pub(crate) fn session_task_is_started(status: SessionTaskStatusKind) -> bool {
    astra_tools::task_mgmt::session_task_is_started(&status)
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
        assert!(SessionTaskStatusKind::InProgress.is_active());
        assert!(SessionTaskStatusKind::Pending.is_active());
        assert!(!SessionTaskStatusKind::Completed.is_active());
        assert!(!SessionTaskStatusKind::Failed.is_active());
        assert!(!SessionTaskStatusKind::Cancelled.is_active());
        assert!(SessionTaskStatusKind::Failed.is_unsuccessful());
        assert!(SessionTaskStatusKind::Cancelled.is_unsuccessful());
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
