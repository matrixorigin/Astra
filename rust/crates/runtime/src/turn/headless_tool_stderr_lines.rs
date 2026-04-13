//! Plain stderr line text for the headless tool round (CLI applies crossterm styles).

/// Human-friendly display name for a tool (matches the SSE stream rendering).
fn friendly_tool_name(tool_name: &str) -> &str {
    match tool_name {
        "read_file" | "view_file" => "Reading",
        "rollback_database_snapshots" | "rollback_file_edits" | "rollback_turn_actions" => {
            "Reverting"
        }
        "write_file" | "create_file" => "Writing",
        "str_replace" | "multi_edit" => "Editing",
        "delete_file" => "Deleting",
        "list_dir" => "Listing",
        _ => tool_name,
    }
}

#[must_use]
pub fn headless_stderr_cache_hit_line(tool_name: &str) -> String {
    format!("  ↻ {} (cached)", friendly_tool_name(tool_name))
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

/// Single-line tool success: `  ✓ Reading: path:1-20  46 lines (0ms)`
#[must_use]
pub fn headless_stderr_tool_ok_line(
    tool_name: &str,
    duration_str: &str,
    detail: Option<&str>,
    summary: Option<&str>,
) -> String {
    let name = friendly_tool_name(tool_name);
    match (detail, summary) {
        (Some(d), Some(s)) => format!("  ✓ {name}: {d}  {s} ({duration_str})"),
        (Some(d), None) => format!("  ✓ {name}: {d} ({duration_str})"),
        (None, Some(s)) => format!("  ✓ {name}  {s} ({duration_str})"),
        (None, None) => format!("  ✓ {name} ({duration_str})"),
    }
}

/// Single-line tool error: `  ✗ Reading: path:1-20 (0ms)`
#[must_use]
pub fn headless_stderr_tool_error_line(
    tool_name: &str,
    duration_str: &str,
    detail: Option<&str>,
) -> String {
    let name = friendly_tool_name(tool_name);
    match detail {
        Some(d) => format!("  ✗ {name}: {d} ({duration_str})"),
        None => format!("  ✗ {name} ({duration_str})"),
    }
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

/// Footer line for tool errors (second line with error preview).
#[must_use]
pub fn headless_stderr_tool_error_detail_line(preview: &str) -> String {
    format!("    {preview}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_line_shape() {
        assert_eq!(
            headless_stderr_cache_hit_line("read_file"),
            "  ↻ Reading (cached)"
        );
    }

    #[test]
    fn ok_line_with_detail_and_summary() {
        assert_eq!(
            headless_stderr_tool_ok_line(
                "read_file",
                "0ms",
                Some("src/main.rs:1-20"),
                Some("20 lines")
            ),
            "  ✓ Reading: src/main.rs:1-20  20 lines (0ms)"
        );
    }

    #[test]
    fn ok_line_detail_only() {
        assert_eq!(
            headless_stderr_tool_ok_line("grep", "100ms", Some("\"TODO\" in src"), None),
            "  ✓ grep: \"TODO\" in src (100ms)"
        );
    }

    #[test]
    fn ok_line_summary_only() {
        assert_eq!(
            headless_stderr_tool_ok_line("git_status", "5ms", None, Some("3 files")),
            "  ✓ git_status  3 files (5ms)"
        );
    }

    #[test]
    fn ok_line_no_detail_no_summary() {
        assert_eq!(
            headless_stderr_tool_ok_line("bash", "50ms", None, None),
            "  ✓ bash (50ms)"
        );
    }

    #[test]
    fn error_line_with_detail() {
        assert_eq!(
            headless_stderr_tool_error_line("read_file", "0ms", Some("missing.rs")),
            "  ✗ Reading: missing.rs (0ms)"
        );
    }

    #[test]
    fn error_line_no_detail() {
        assert_eq!(
            headless_stderr_tool_error_line("bash", "150ms", None),
            "  ✗ bash (150ms)"
        );
    }

    #[test]
    fn preview_truncates_utf8_safe() {
        let s = "αβγδε"; // 5 chars
        let p = headless_stderr_error_preview_line(s, 3);
        assert_eq!(p, "αβγ…");
    }

    #[test]
    fn unknown_tool_header() {
        assert_eq!(
            headless_stderr_unknown_tool_header("bad_tool"),
            "  ✗ bad_tool"
        );
    }

    #[test]
    fn unknown_tool_detail() {
        assert_eq!(
            headless_stderr_unknown_tool_detail("not found"),
            "  └ not found"
        );
    }

    #[test]
    fn resource_limit_blocked() {
        let s = headless_stderr_resource_limit_blocked("bash");
        assert!(s.contains("bash"));
        assert!(s.contains("blocked"));
    }

    #[test]
    fn resource_limit_in_output() {
        let s = headless_stderr_resource_limit_in_output("exec");
        assert!(s.contains("exec"));
        assert!(s.contains("resource limit"));
    }

    #[test]
    fn preview_within_limit_no_truncation() {
        assert_eq!(headless_stderr_error_preview_line("short", 100), "short");
    }

    #[test]
    fn preview_empty_string() {
        assert_eq!(headless_stderr_error_preview_line("", 10), "");
    }

    #[test]
    fn preview_exact_limit() {
        assert_eq!(headless_stderr_error_preview_line("abcde", 5), "abcde");
    }

    #[test]
    fn tool_error_detail_line() {
        assert_eq!(
            headless_stderr_tool_error_detail_line("permission denied"),
            "    permission denied"
        );
    }

    #[test]
    fn friendly_names_match_sse_rendering() {
        assert_eq!(friendly_tool_name("read_file"), "Reading");
        assert_eq!(friendly_tool_name("view_file"), "Reading");
        assert_eq!(
            friendly_tool_name("rollback_database_snapshots"),
            "Reverting"
        );
        assert_eq!(friendly_tool_name("rollback_file_edits"), "Reverting");
        assert_eq!(friendly_tool_name("rollback_turn_actions"), "Reverting");
        assert_eq!(friendly_tool_name("write_file"), "Writing");
        assert_eq!(friendly_tool_name("create_file"), "Writing");
        assert_eq!(friendly_tool_name("str_replace"), "Editing");
        assert_eq!(friendly_tool_name("multi_edit"), "Editing");
        assert_eq!(friendly_tool_name("delete_file"), "Deleting");
        assert_eq!(friendly_tool_name("list_dir"), "Listing");
        assert_eq!(friendly_tool_name("bash"), "bash");
        assert_eq!(friendly_tool_name("grep"), "grep");
    }
}
