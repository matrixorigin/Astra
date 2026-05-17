//! Plain stderr lines for live SSE `tool_request` handling and thin-client post failures (CLI applies styling).

use std::fmt::Display;

#[must_use]
pub fn edge_sse_tool_request_notice_line(tool: &str, request_id: &str) -> String {
    format!("  ⚡ tool_request: {tool} ({request_id})")
}

#[must_use]
pub fn edge_sse_post_tool_result_fail_line(err: impl Display) -> String {
    format!("  ! post_tool_result: {err}")
}

#[must_use]
pub fn edge_sse_post_approval_fail_line(err: impl Display) -> String {
    format!("  ! post_approval: {err}")
}

#[must_use]
pub fn edge_sse_thought_duration_line(elapsed_secs: f64) -> String {
    format!("  ● Thought for {elapsed_secs:.1}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_request_line_shape() {
        assert_eq!(
            edge_sse_tool_request_notice_line("bash", "r1"),
            "  ⚡ tool_request: bash (r1)"
        );
    }

    #[test]
    fn thought_line_formats() {
        assert_eq!(edge_sse_thought_duration_line(1.25), "  ● Thought for 1.2s");
    }

    // --- edge cases ---

    #[test]
    fn tool_request_unicode_tool_name() {
        assert_eq!(
            edge_sse_tool_request_notice_line("读取文件", "r1"),
            "  ⚡ tool_request: 读取文件 (r1)"
        );
    }

    #[test]
    fn tool_request_empty_strings() {
        assert_eq!(
            edge_sse_tool_request_notice_line("", ""),
            "  ⚡ tool_request:  ()"
        );
    }

    #[test]
    fn post_tool_result_fail_multiline_error() {
        assert_eq!(
            edge_sse_post_tool_result_fail_line("line1\nline2"),
            "  ! post_tool_result: line1\nline2"
        );
    }

    #[test]
    fn post_approval_fail_unicode() {
        assert_eq!(
            edge_sse_post_approval_fail_line("审批失败 🚫"),
            "  ! post_approval: 审批失败 🚫"
        );
    }

    #[test]
    fn thought_duration_zero() {
        assert_eq!(edge_sse_thought_duration_line(0.0), "  ● Thought for 0.0s");
    }

    #[test]
    fn thought_duration_large() {
        assert_eq!(
            edge_sse_thought_duration_line(123.456),
            "  ● Thought for 123.5s"
        );
    }
}
