use serde::{Deserialize, Serialize};

/// Tool result outcome — intentionally coarse. Detail lives in output text.
///
/// Aligned with [`ToolResultStatus`](astra_turn_core::tool_result_semantics::ToolResultStatus):
/// - `Completed` ↔ `Completed`
/// - `Failed` ↔ `Failed`
/// - `Skipped` ↔ `Skipped`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatusKind {
    Completed,
    Failed,
    /// Protective deduplication or skipped execution (not an error).
    Skipped,
}

impl ToolResultStatusKind {
    /// Parse a canonical status string into a `ToolResultStatusKind`.
    ///
    /// Accepts only the exact canonical values: `"completed"`, `"skipped"`.
    /// Everything else (including aliases like `"ok"`, `"success"`, `"done"`)
    /// is treated as `Failed`, signaling that the producer should emit the
    /// canonical value rather than relying on ambiguous aliases.
    pub fn from_status_str(status: &str) -> Self {
        let normalized = status.trim().to_lowercase();
        match normalized.as_str() {
            "completed" => Self::Completed,
            "skipped" => Self::Skipped,
            _ => Self::Failed,
        }
    }

    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }

    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failed)
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
    fn from_status_str_exact_completed_and_skipped() {
        // Only exact canonical values are accepted as Completed
        assert_eq!(
            ToolResultStatusKind::from_status_str("completed"),
            ToolResultStatusKind::Completed
        );
        assert!(ToolResultStatusKind::from_status_str("completed").is_success());
        assert!(!ToolResultStatusKind::from_status_str("completed").is_failure());

        // Aliases like "ok", "success", "done" now return Failed
        for alias in ["ok", "success", "succeeded", "complete", "passed", "done"] {
            let kind = ToolResultStatusKind::from_status_str(alias);
            assert_eq!(
                kind,
                ToolResultStatusKind::Failed,
                "alias '{alias}' should NOT parse as Completed under strict first-principles parsing"
            );
        }
    }

    #[test]
    fn from_status_str_case_insensitive() {
        for s in ["Completed", "COMPLETED", "completed"] {
            let kind = ToolResultStatusKind::from_status_str(s);
            assert_eq!(
                kind,
                ToolResultStatusKind::Completed,
                "expected '{s}' to be Completed (case-insensitive)"
            );
            assert!(kind.is_success());
        }
        // Non-canonical even with correct case → Failed
        assert_eq!(
            ToolResultStatusKind::from_status_str("OK"),
            ToolResultStatusKind::Failed
        );
    }

    #[test]
    fn from_status_str_trims_whitespace() {
        assert_eq!(
            ToolResultStatusKind::from_status_str("  completed  "),
            ToolResultStatusKind::Completed
        );
        assert_eq!(
            ToolResultStatusKind::from_status_str("\tcompleted\n"),
            ToolResultStatusKind::Completed
        );
        // Trimming does NOT make aliases canonical
        assert_eq!(
            ToolResultStatusKind::from_status_str("  ok  "),
            ToolResultStatusKind::Failed
        );
    }

    #[test]
    fn non_success_is_failure() {
        let kind = ToolResultStatusKind::from_status_str("permission_denied");
        assert_eq!(kind, ToolResultStatusKind::Failed);
        assert!(!kind.is_success());
        assert!(kind.is_failure());
        // Even with mixed case, non-success is still Failed
        assert_eq!(
            ToolResultStatusKind::from_status_str("PERMISSION_DENIED"),
            ToolResultStatusKind::Failed
        );
    }

    // Regression: free functions still work (but with new semantics)
    #[test]
    fn free_functions_with_new_semantics() {
        // "completed" and "skipped" work as expected
        assert!(tool_result_status_is_success("completed"));
        assert!(!tool_result_status_is_success("err"));
        assert!(tool_result_status_is_failure("err"));
        assert!(!tool_result_status_is_failure("completed"));
        assert_eq!(
            tool_result_status_kind("completed"),
            ToolResultStatusKind::Completed
        );
        // "ok" is NO longer considered success
        assert!(!tool_result_status_is_success("ok"));
        assert!(tool_result_status_is_failure("ok"));
        // "skipped" is neither success nor failure
        assert!(!tool_result_status_is_success("skipped"));
        assert!(!tool_result_status_is_failure("skipped"));
    }

    #[test]
    fn serde_roundtrip() {
        let json = serde_json::to_string(&ToolResultStatusKind::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
        let kind: ToolResultStatusKind = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(kind, ToolResultStatusKind::Failed);
    }
}
