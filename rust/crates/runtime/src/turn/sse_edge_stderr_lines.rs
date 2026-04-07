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
        let s = edge_sse_tool_request_notice_line("bash", "r1");
        assert!(s.contains("bash") && s.contains("r1"));
    }

    #[test]
    fn thought_line_formats() {
        assert!(edge_sse_thought_duration_line(1.25).contains("1.2"));
    }

    // --- edge cases ---

    #[test]
    fn tool_request_unicode_tool_name() {
        let s = edge_sse_tool_request_notice_line("读取文件", "r1");
        assert!(s.contains("读取文件"));
    }

    #[test]
    fn tool_request_empty_strings() {
        let s = edge_sse_tool_request_notice_line("", "");
        assert!(s.contains("⚡"));
    }

    #[test]
    fn post_tool_result_fail_multiline_error() {
        let s = edge_sse_post_tool_result_fail_line("line1\nline2");
        assert!(s.contains("line1\nline2"));
    }

    #[test]
    fn post_approval_fail_unicode() {
        let s = edge_sse_post_approval_fail_line("审批失败 🚫");
        assert!(s.contains("审批失败"));
    }

    #[test]
    fn thought_duration_zero() {
        let s = edge_sse_thought_duration_line(0.0);
        assert!(s.contains("0.0s"));
    }

    #[test]
    fn thought_duration_large() {
        let s = edge_sse_thought_duration_line(123.456);
        assert!(s.contains("123.5"));
    }
}
