#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultStatusKind {
    Success,
    NonSuccess,
}

pub fn tool_result_status_kind(status: &str) -> ToolResultStatusKind {
    if matches!(status, "ok" | "success") {
        ToolResultStatusKind::Success
    } else {
        ToolResultStatusKind::NonSuccess
    }
}

pub fn tool_result_status_is_success(status: &str) -> bool {
    matches!(
        tool_result_status_kind(status),
        ToolResultStatusKind::Success
    )
}

pub fn tool_result_status_is_failure(status: &str) -> bool {
    !tool_result_status_is_success(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_statuses_are_recognized_as_success() {
        assert_eq!(tool_result_status_kind("ok"), ToolResultStatusKind::Success);
        assert_eq!(
            tool_result_status_kind("success"),
            ToolResultStatusKind::Success
        );
        assert!(tool_result_status_is_success("ok"));
        assert!(tool_result_status_is_success("success"));
        assert!(!tool_result_status_is_failure("ok"));
        assert!(!tool_result_status_is_failure("success"));
    }

    #[test]
    fn non_success_status_is_recognized_as_failure() {
        assert_eq!(
            tool_result_status_kind("permission_denied"),
            ToolResultStatusKind::NonSuccess
        );
        assert!(!tool_result_status_is_success("permission_denied"));
        assert!(tool_result_status_is_failure("permission_denied"));
    }
}
