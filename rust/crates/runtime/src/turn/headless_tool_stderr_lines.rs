//! Plain stderr line text for the headless tool round (CLI applies crossterm styles).

#[must_use]
pub fn headless_stderr_cache_hit_line(tool_name: &str) -> String {
    format!("  ↻ {tool_name} (cached)")
}

#[must_use]
pub fn headless_stderr_unknown_tool_header(tool_name: &str) -> String {
    format!("  ✗ {tool_name}")
}

#[must_use]
pub fn headless_stderr_unknown_tool_detail(err_msg: &str) -> String {
    format!("  └ {err_msg}")
}

#[must_use]
pub fn headless_stderr_resource_limit_blocked(tool: &str) -> String {
    format!("  ⚠ {tool} blocked: system resource limit reached")
}

#[must_use]
pub fn headless_stderr_resource_limit_in_output(tool: &str) -> String {
    format!("  ⚠ {tool}: resource limit detected in output — tool blocked")
}

#[must_use]
pub fn headless_stderr_tool_error_header(tool_name: &str, duration_str: &str) -> String {
    format!("  ✗ {tool_name} ({duration_str})")
}

#[must_use]
pub fn headless_stderr_tool_ok_header(tool_name: &str, duration_str: &str) -> String {
    format!("  ✓ {tool_name} ({duration_str})")
}

/// Truncate at UTF-8 char boundary for a one-line preview.
#[must_use]
pub fn headless_stderr_error_preview_line(first_line: &str, max_chars: usize) -> String {
    if first_line.chars().count() <= max_chars {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

#[must_use]
pub fn headless_stderr_tool_error_detail_line(preview: &str) -> String {
    format!("  └ Error: {preview}")
}

/// Footer after a successful tool (`detail` / `summary` from CLI helpers).
#[must_use]
pub fn headless_stderr_tool_ok_footer_line(
    detail: Option<&str>,
    summary: Option<&str>,
) -> Option<String> {
    match (detail, summary) {
        (Some(d), Some(s)) => Some(format!("  └ {d}  →  {s}")),
        (Some(d), None) => Some(format!("  └ {d}")),
        (None, Some(s)) => Some(format!("  └ {s}")),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_line_shape() {
        assert_eq!(
            headless_stderr_cache_hit_line("read_file"),
            "  ↻ read_file (cached)"
        );
    }

    #[test]
    fn ok_footer_four_cases() {
        assert_eq!(
            headless_stderr_tool_ok_footer_line(Some("a"), Some("b")).as_deref(),
            Some("  └ a  →  b")
        );
        assert_eq!(
            headless_stderr_tool_ok_footer_line(Some("a"), None).as_deref(),
            Some("  └ a")
        );
        assert_eq!(
            headless_stderr_tool_ok_footer_line(None, Some("b")).as_deref(),
            Some("  └ b")
        );
        assert!(headless_stderr_tool_ok_footer_line(None, None).is_none());
    }

    #[test]
    fn preview_truncates_utf8_safe() {
        let s = "αβγδε"; // 5 chars
        let p = headless_stderr_error_preview_line(s, 3);
        assert_eq!(p, "αβγ…");
    }
}
