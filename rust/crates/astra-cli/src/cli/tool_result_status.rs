use crate::cli::theme;
pub(crate) fn tool_result_status_is_skipped(status: &str) -> bool {
    let normalized = status.trim().to_lowercase();
    normalized.as_str() == "skipped"
}

pub(crate) fn tool_result_status_is_failure(status: &str) -> bool {
    // Skipped is not a failure — it's protective deduplication
    if tool_result_status_is_skipped(status) {
        return false;
    }
    let kind = astra_tools::tool_result_status::tool_result_status_kind(status);
    kind.is_failure()
}

pub(crate) fn tool_result_status_is_success(status: &str) -> bool {
    let kind = astra_tools::tool_result_status::tool_result_status_kind(status);
    kind.is_success()
}

pub(crate) fn tool_result_status_is_canonical_success(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("completed")
}

pub(crate) fn tool_result_status_icon(status: &str) -> String {
    if tool_result_status_is_failure(status) {
        theme::icon_err()
    } else if tool_result_status_is_skipped(status) {
        // Skipped is protective deduplication, not a success or failure — use
        // a distinct warn icon so users can scan and distinguish it from real
        // successes (ok) and from warnings surfaced in the output text.
        theme::icon_warn()
    } else {
        theme::icon_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{tool_result_status_icon, tool_result_status_is_canonical_success};
    use crate::cli::theme;

    #[test]
    fn icon_uses_failure_semantics_for_non_success_status() {
        assert_eq!(tool_result_status_icon("completed"), theme::icon_ok());
        assert_eq!(tool_result_status_icon("ok"), theme::icon_ok());
        assert_eq!(tool_result_status_icon("success"), theme::icon_ok());
        assert_eq!(tool_result_status_icon("done"), theme::icon_ok());
        assert_eq!(tool_result_status_icon("skipped"), theme::icon_warn());
        assert_eq!(tool_result_status_icon("failed"), theme::icon_err());
    }

    #[test]
    fn canonical_success_is_strict_for_control_flow() {
        assert!(tool_result_status_is_canonical_success("completed"));
        assert!(tool_result_status_is_canonical_success("  COMPLETED  "));
        for alias in ["ok", "success", "done", "complete", "passed"] {
            assert!(
                !tool_result_status_is_canonical_success(alias),
                "non-canonical alias '{alias}' must not advance control-flow state"
            );
        }
    }
}
