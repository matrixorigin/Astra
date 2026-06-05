#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultStatusKind {
    Success,
    NonSuccess,
}

pub fn tool_result_status_kind(status: &str) -> ToolResultStatusKind {
    // Normalize: uppercase variants like "OK" / "SUCCESS" occasionally arrive
    // from upstream edge handlers and API responses.
    let normalized = status.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "ok" | "success" | "succeeded" | "completed" | "complete" | "passed"
    ) {
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
        for s in [
            "ok",
            "success",
            "succeeded",
            "completed",
            "complete",
            "passed",
        ] {
            assert_eq!(
                tool_result_status_kind(s),
                ToolResultStatusKind::Success,
                "expected {s} to be Success"
            );
            assert!(tool_result_status_is_success(s));
            assert!(!tool_result_status_is_failure(s));
        }
    }

    #[test]
    fn success_statuses_are_case_insensitive() {
        for s in ["OK", "Success", "SUCCEEDED", "Completed", "PASSED"] {
            assert_eq!(
                tool_result_status_kind(s),
                ToolResultStatusKind::Success,
                "expected {s} to be Success (case-insensitive)"
            );
            assert!(tool_result_status_is_success(s));
        }
    }

    #[test]
    fn leading_trailing_whitespace_is_ignored() {
        assert_eq!(
            tool_result_status_kind("  ok  "),
            ToolResultStatusKind::Success
        );
        assert_eq!(
            tool_result_status_kind("\tOK\n"),
            ToolResultStatusKind::Success
        );
    }

    #[test]
    fn non_success_status_is_recognized_as_failure() {
        assert_eq!(
            tool_result_status_kind("permission_denied"),
            ToolResultStatusKind::NonSuccess
        );
        assert!(!tool_result_status_is_success("permission_denied"));
        assert!(tool_result_status_is_failure("permission_denied"));
        // Even with mixed case, non-success is still NonSuccess
        assert_eq!(
            tool_result_status_kind("PERMISSION_DENIED"),
            ToolResultStatusKind::NonSuccess
        );
    }
}
