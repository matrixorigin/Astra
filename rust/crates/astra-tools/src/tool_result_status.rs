use serde::{Deserialize, Serialize};

/// Tool result outcome — intentionally coarse. Detail lives in structured
/// metadata or the tool result body.
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
    /// Parse a tool-result status string into a `ToolResultStatusKind`.
    ///
    /// Producers should emit canonical values (`completed`, `failed`, `skipped`),
    /// but consumers must tolerate common success aliases seen in tool and agent
    /// outputs. Unknown values remain failed so malformed status channels fail
    /// closed instead of looking successful.
    pub fn from_status_str(status: &str) -> Self {
        let normalized = status.trim().to_lowercase();
        match normalized.as_str() {
            "completed" | "complete" | "ok" | "success" | "succeeded" | "passed" | "done"
            | "launched" | "pending" | "queued" | "in_progress" | "running" | "still_running"
            | "processing" | "starting" | "waiting" | "waiting_for_input" | "interrupted" => {
                Self::Completed
            }
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

#[cfg(test)]
mod tests {
    use super::ToolResultStatusKind;

    #[test]
    fn from_status_str_accepts_canonical_statuses_and_success_aliases() {
        assert_eq!(
            ToolResultStatusKind::from_status_str("completed"),
            ToolResultStatusKind::Completed
        );
        assert!(ToolResultStatusKind::from_status_str("completed").is_success());
        assert!(!ToolResultStatusKind::from_status_str("completed").is_failure());

        for alias in ["ok", "success", "succeeded", "complete", "passed", "done"] {
            let kind = ToolResultStatusKind::from_status_str(alias);
            assert_eq!(
                kind,
                ToolResultStatusKind::Completed,
                "success alias '{alias}' should not render successful tool output as failed"
            );
        }

        assert_eq!(
            ToolResultStatusKind::from_status_str("skipped"),
            ToolResultStatusKind::Skipped
        );
    }

    #[test]
    fn from_status_str_accepts_agent_runtime_domain_statuses() {
        for status in [
            "launched",
            "still_running",
            "waiting",
            "running",
            "interrupted",
        ] {
            let kind = ToolResultStatusKind::from_status_str(status);
            assert_eq!(
                kind,
                ToolResultStatusKind::Completed,
                "agent domain status '{status}' means the tool call returned state, not that execution failed"
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
        assert_eq!(
            ToolResultStatusKind::from_status_str("OK"),
            ToolResultStatusKind::Completed
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
        assert_eq!(
            ToolResultStatusKind::from_status_str("  ok  "),
            ToolResultStatusKind::Completed
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

    #[test]
    fn serde_roundtrip() {
        let json = serde_json::to_string(&ToolResultStatusKind::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
        let kind: ToolResultStatusKind = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(kind, ToolResultStatusKind::Failed);
    }
}
