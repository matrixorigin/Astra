#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStatusKind {
    Created,
    Running,
    Completed,
    Unfinished,
    Partial,
    CompletedWithConflicts,
    CompletedOverBudget,
    Failed,
    Cancelled,
    Interrupted,
    Timeout,
    Other,
}

pub(crate) fn run_status_kind(status: &str) -> RunStatusKind {
    match status {
        "created" => RunStatusKind::Created,
        "running" => RunStatusKind::Running,
        "completed" => RunStatusKind::Completed,
        "unfinished" => RunStatusKind::Unfinished,
        "partial" | "partial_failure" => RunStatusKind::Partial,
        "completed_with_conflicts" => RunStatusKind::CompletedWithConflicts,
        "completed_over_budget" => RunStatusKind::CompletedOverBudget,
        "failed" => RunStatusKind::Failed,
        "cancelled" => RunStatusKind::Cancelled,
        "interrupted" => RunStatusKind::Interrupted,
        "timeout" => RunStatusKind::Timeout,
        _ => RunStatusKind::Other,
    }
}

pub(crate) fn run_status_is_done(status: &str) -> bool {
    matches!(
        run_status_kind(status),
        RunStatusKind::Completed
            | RunStatusKind::Unfinished
            | RunStatusKind::Partial
            | RunStatusKind::CompletedWithConflicts
            | RunStatusKind::CompletedOverBudget
    )
}

pub(crate) fn run_status_is_failed(status: &str) -> bool {
    matches!(
        run_status_kind(status),
        RunStatusKind::Failed
            | RunStatusKind::Cancelled
            | RunStatusKind::Interrupted
            | RunStatusKind::Timeout
    )
}

pub(crate) fn run_status_is_active(status: &str) -> bool {
    matches!(
        run_status_kind(status),
        RunStatusKind::Created | RunStatusKind::Running
    )
}

pub(crate) fn run_status_is_completed(status: &str) -> bool {
    run_status_kind(status) == RunStatusKind::Completed
}

pub(crate) fn run_status_icon(status: &str) -> &'static str {
    match run_status_kind(status) {
        RunStatusKind::Created => "⏳",
        RunStatusKind::Running => "🔄",
        RunStatusKind::Completed => "✅",
        RunStatusKind::Unfinished => "⏳",
        RunStatusKind::Partial
        | RunStatusKind::CompletedWithConflicts
        | RunStatusKind::CompletedOverBudget => "⚠️ ",
        RunStatusKind::Failed | RunStatusKind::Timeout => "❌",
        RunStatusKind::Cancelled | RunStatusKind::Interrupted => "🛑",
        RunStatusKind::Other => "⑂",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_uses_warn_for_degraded_done_states_and_error_for_timeout() {
        assert_eq!(run_status_icon("unfinished"), "⏳");
        assert_eq!(run_status_icon("partial"), "⚠️ ");
        assert_eq!(run_status_icon("partial_failure"), "⚠️ ");
        assert_eq!(run_status_icon("completed_with_conflicts"), "⚠️ ");
        assert_eq!(run_status_icon("completed_over_budget"), "⚠️ ");
        assert_eq!(run_status_icon("timeout"), "❌");
        assert_eq!(run_status_icon("interrupted"), "🛑");
        assert_eq!(run_status_icon("cancelled"), "🛑");
    }
}
