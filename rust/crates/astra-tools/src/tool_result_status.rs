use serde::{Deserialize, Serialize};

/// Tool result outcome — intentionally coarse. Detail lives in output text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatusKind {
    Success,
    NonSuccess,
    /// Protective deduplication or skipped execution (not an error).
    Skipped,
}

impl ToolResultStatusKind {
    /// Parse a status string with normalization: case-insensitive, leading/trailing whitespace stripped.
    pub fn from_status_str(status: &str) -> Self {
        let normalized = status.trim().to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "ok" | "success" | "succeeded" | "completed" | "complete" | "passed"
        ) {
            Self::Success
        } else if normalized.as_str() == "skipped" {
            Self::Skipped
        } else {
            Self::NonSuccess
        }
    }

    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }

    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, Self::NonSuccess)
    }

    #[must_use]
    pub fn is_skipped(self) -> bool {
        matches!(self, Self::Skipped)
    }
}

// ── Deprecated free functions (preserved for existing callers) ─────────────────

pub fn tool_result_status_kind(status: &str) -> ToolResultStatusKind {
    ToolResultStatusKind::from_status_str(status)
}

pub fn tool_result_status_is_success(status: &str) -> bool {
    ToolResultStatusKind::from_status_str(status).is_success()
}

pub fn tool_result_status_is_failure(status: &str) -> bool {
    ToolResultStatusKind::from_status_str(status).is_failure()
}

#[cfg(test)]
mod tests {
    use super::{
        ToolResultStatusKind, tool_result_status_is_failure, tool_result_status_is_success,
        tool_result_status_kind,
    };

    #[test]
    fn from_status_str_success_variants() {
        for s in [
            "ok",
            "success",
            "succeeded",
            "completed",
            "complete",
            "passed",
        ] {
            let kind = ToolResultStatusKind::from_status_str(s);
            assert_eq!(
                kind,
                ToolResultStatusKind::Success,
                "expected {s} to be Success"
            );
            assert!(kind.is_success());
            assert!(!kind.is_failure());
        }
    }

    #[test]
    fn from_status_str_case_insensitive() {
        for s in ["OK", "Success", "SUCCEEDED", "Completed", "PASSED"] {
            let kind = ToolResultStatusKind::from_status_str(s);
            assert_eq!(
                kind,
                ToolResultStatusKind::Success,
                "expected {s} to be Success (case-insensitive)"
            );
            assert!(kind.is_success());
        }
    }

    #[test]
    fn from_status_str_trims_whitespace() {
        assert_eq!(
            ToolResultStatusKind::from_status_str("  ok  "),
            ToolResultStatusKind::Success
        );
        assert_eq!(
            ToolResultStatusKind::from_status_str("\tOK\n"),
            ToolResultStatusKind::Success
        );
    }

    #[test]
    fn non_success_is_failure() {
        let kind = ToolResultStatusKind::from_status_str("permission_denied");
        assert_eq!(kind, ToolResultStatusKind::NonSuccess);
        assert!(!kind.is_success());
        assert!(kind.is_failure());
        // Even with mixed case, non-success is still NonSuccess
        assert_eq!(
            ToolResultStatusKind::from_status_str("PERMISSION_DENIED"),
            ToolResultStatusKind::NonSuccess
        );
    }

    // Regression: free functions still work
    #[test]
    fn free_functions_match_methods() {
        assert!(tool_result_status_is_success("ok"));
        assert!(!tool_result_status_is_success("err"));
        assert!(tool_result_status_is_failure("err"));
        assert!(!tool_result_status_is_failure("ok"));
        assert_eq!(tool_result_status_kind("ok"), ToolResultStatusKind::Success);
    }

    #[test]
    fn serde_roundtrip() {
        let json = serde_json::to_string(&ToolResultStatusKind::Success).unwrap();
        assert_eq!(json, "\"success\"");
        let kind: ToolResultStatusKind = serde_json::from_str("\"non_success\"").unwrap();
        assert_eq!(kind, ToolResultStatusKind::NonSuccess);
    }
}
