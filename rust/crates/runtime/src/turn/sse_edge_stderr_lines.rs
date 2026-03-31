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
}
