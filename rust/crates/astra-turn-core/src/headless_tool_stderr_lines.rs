//! Plain stderr line text for the headless tool round (CLI applies crossterm styles).

/// Human-friendly display name for a tool (matches the SSE stream rendering).
fn friendly_tool_name(tool_name: &str) -> &str {
    match tool_name {
        "read_file" | "view_file" => "Reading",
        "run_build_test" => "Running build/test",
        "powershell" => "PowerShell",
        "rollback_database_snapshots" | "rollback_file_edits" | "rollback_turn_actions" => {
            "Reverting"
        }
        "rollback_session_state" => "Reverting session state",
        "git_status" => "Git status",
        "git_log" => "Git log",
        "git_show" => "Git show",
        "git_diff" => "Git diff",
        "git_blame" => "Git blame",
        "git_file_history" => "Git history",
        "git_log_search" => "Git log search",
        "git_contributors" => "Git contributors",
        "git_revert_commit" => "Git revert",
        "git_stash" => "Git stash",
        "git_commit" => "Git commit",
        "git_checkout_file" => "Git checkout file",
        "git_worktree" => "Git worktree",
        "github_get_pr" => "Getting PR",
        "github_list_prs" => "Listing PRs",
        "github_get_issue" => "Getting issue",
        "github_list_issues" => "Listing issues",
        "github_repo_stats" => "GitHub stats",
        "github_ci_status" => "GitHub CI",
        "github_create_issue" => "Creating issue",
        "tool_search" => "Searching tools",
        "lsp" => "LSP",
        "web_search" => "Searching web",
        "web_fetch" => "Fetching",
        "glob" => "Globbing",
        "ask_user" => "Asking user",
        "sleep" => "Sleeping",
        "send_message" => "Sending message",
        "spawn_agent" => "Spawning agent",
        "get_agent_result" => "Getting agent result",
        "diagnose" => "Diagnosing",
        "env" => "Environment",
        "notebook_edit" => "Editing notebook",
        "config" => "Config",
        "brief" => "Brief",
        "share_context" => "Sharing context",
        "query_context" => "Querying context",
        "task_create" => "Creating task",
        "task_list" => "Listing tasks",
        "task_get" => "Getting task",
        "task_update" => "Updating task",
        "task_stop" => "Stopping task",
        "get_agent_info" => "Getting agent info",
        "reflect" => "Reflecting",
        "context_analysis" => "Analyzing context",
        "run_chain" => "Running chain",
        "adjust_config" => "Adjusting config",
        "prioritize_tool" => "Prioritizing tool",
        "deprioritize_tool" => "Deprioritizing tool",
        "set_goal" => "Setting goal",
        "compress_context" => "Compressing context",
        "mo_query" => "MatrixOne query",
        "mo_snapshot" => "MatrixOne snapshot",
        "mo_branch" => "MatrixOne branch",
        "memory_retrieve" => "Recalling",
        "memory_store" => "Storing",
        "memory_search" => "Searching memory",
        "memory_purge" => "Purging memory",
        "memory_correct" => "Correcting memory",
        "memory_profile" => "Checking profile",
        "find_definition" => "Finding definition",
        "find_references" => "Finding references",
        "symbol_search" => "Searching symbols",
        "symbols" => "Getting symbols",
        "call_graph" => "Call graph",
        "hover_info" => "Hover info",
        "type_hierarchy" => "Type hierarchy",
        "rename_symbol" => "Renaming symbol",
        "dead_code" => "Finding dead code",
        "extract_members" => "Extracting members",
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
            "  ✓ Git status  3 files (5ms)"
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
        assert_eq!(friendly_tool_name("run_build_test"), "Running build/test");
        assert_eq!(friendly_tool_name("powershell"), "PowerShell");
        assert_eq!(
            friendly_tool_name("rollback_database_snapshots"),
            "Reverting"
        );
        assert_eq!(friendly_tool_name("rollback_file_edits"), "Reverting");
        assert_eq!(friendly_tool_name("rollback_turn_actions"), "Reverting");
        assert_eq!(
            friendly_tool_name("rollback_session_state"),
            "Reverting session state"
        );
        assert_eq!(friendly_tool_name("git_status"), "Git status");
        assert_eq!(friendly_tool_name("git_log"), "Git log");
        assert_eq!(friendly_tool_name("git_show"), "Git show");
        assert_eq!(friendly_tool_name("git_diff"), "Git diff");
        assert_eq!(friendly_tool_name("git_blame"), "Git blame");
        assert_eq!(friendly_tool_name("git_file_history"), "Git history");
        assert_eq!(friendly_tool_name("git_log_search"), "Git log search");
        assert_eq!(friendly_tool_name("git_contributors"), "Git contributors");
        assert_eq!(friendly_tool_name("git_revert_commit"), "Git revert");
        assert_eq!(friendly_tool_name("git_stash"), "Git stash");
        assert_eq!(friendly_tool_name("git_commit"), "Git commit");
        assert_eq!(friendly_tool_name("git_checkout_file"), "Git checkout file");
        assert_eq!(friendly_tool_name("git_worktree"), "Git worktree");
        assert_eq!(friendly_tool_name("github_get_pr"), "Getting PR");
        assert_eq!(friendly_tool_name("github_list_prs"), "Listing PRs");
        assert_eq!(friendly_tool_name("github_get_issue"), "Getting issue");
        assert_eq!(friendly_tool_name("github_list_issues"), "Listing issues");
        assert_eq!(friendly_tool_name("github_repo_stats"), "GitHub stats");
        assert_eq!(friendly_tool_name("github_ci_status"), "GitHub CI");
        assert_eq!(friendly_tool_name("github_create_issue"), "Creating issue");
        assert_eq!(friendly_tool_name("tool_search"), "Searching tools");
        assert_eq!(friendly_tool_name("lsp"), "LSP");
        assert_eq!(friendly_tool_name("web_search"), "Searching web");
        assert_eq!(friendly_tool_name("web_fetch"), "Fetching");
        assert_eq!(friendly_tool_name("ask_user"), "Asking user");
        assert_eq!(friendly_tool_name("sleep"), "Sleeping");
        assert_eq!(friendly_tool_name("send_message"), "Sending message");
        assert_eq!(friendly_tool_name("spawn_agent"), "Spawning agent");
        assert_eq!(
            friendly_tool_name("get_agent_result"),
            "Getting agent result"
        );
        assert_eq!(friendly_tool_name("diagnose"), "Diagnosing");
        assert_eq!(friendly_tool_name("env"), "Environment");
        assert_eq!(friendly_tool_name("notebook_edit"), "Editing notebook");
        assert_eq!(friendly_tool_name("config"), "Config");
        assert_eq!(friendly_tool_name("brief"), "Brief");
        assert_eq!(friendly_tool_name("share_context"), "Sharing context");
        assert_eq!(friendly_tool_name("query_context"), "Querying context");
        assert_eq!(friendly_tool_name("task_create"), "Creating task");
        assert_eq!(friendly_tool_name("task_list"), "Listing tasks");
        assert_eq!(friendly_tool_name("task_get"), "Getting task");
        assert_eq!(friendly_tool_name("task_update"), "Updating task");
        assert_eq!(friendly_tool_name("task_stop"), "Stopping task");
        assert_eq!(friendly_tool_name("get_agent_info"), "Getting agent info");
        assert_eq!(friendly_tool_name("reflect"), "Reflecting");
        assert_eq!(friendly_tool_name("context_analysis"), "Analyzing context");
        assert_eq!(friendly_tool_name("run_chain"), "Running chain");
        assert_eq!(friendly_tool_name("adjust_config"), "Adjusting config");
        assert_eq!(friendly_tool_name("prioritize_tool"), "Prioritizing tool");
        assert_eq!(
            friendly_tool_name("deprioritize_tool"),
            "Deprioritizing tool"
        );
        assert_eq!(friendly_tool_name("set_goal"), "Setting goal");
        assert_eq!(
            friendly_tool_name("compress_context"),
            "Compressing context"
        );
        assert_eq!(friendly_tool_name("mo_query"), "MatrixOne query");
        assert_eq!(friendly_tool_name("mo_snapshot"), "MatrixOne snapshot");
        assert_eq!(friendly_tool_name("mo_branch"), "MatrixOne branch");
        assert_eq!(friendly_tool_name("memory_retrieve"), "Recalling");
        assert_eq!(friendly_tool_name("memory_store"), "Storing");
        assert_eq!(friendly_tool_name("memory_search"), "Searching memory");
        assert_eq!(friendly_tool_name("memory_purge"), "Purging memory");
        assert_eq!(friendly_tool_name("memory_correct"), "Correcting memory");
        assert_eq!(friendly_tool_name("memory_profile"), "Checking profile");
        assert_eq!(friendly_tool_name("find_definition"), "Finding definition");
        assert_eq!(friendly_tool_name("find_references"), "Finding references");
        assert_eq!(friendly_tool_name("symbol_search"), "Searching symbols");
        assert_eq!(friendly_tool_name("symbols"), "Getting symbols");
        assert_eq!(friendly_tool_name("call_graph"), "Call graph");
        assert_eq!(friendly_tool_name("hover_info"), "Hover info");
        assert_eq!(friendly_tool_name("type_hierarchy"), "Type hierarchy");
        assert_eq!(friendly_tool_name("rename_symbol"), "Renaming symbol");
        assert_eq!(friendly_tool_name("dead_code"), "Finding dead code");
        assert_eq!(friendly_tool_name("extract_members"), "Extracting members");
        assert_eq!(friendly_tool_name("write_file"), "Writing");
        assert_eq!(friendly_tool_name("create_file"), "Writing");
        assert_eq!(friendly_tool_name("str_replace"), "Editing");
        assert_eq!(friendly_tool_name("multi_edit"), "Editing");
        assert_eq!(friendly_tool_name("delete_file"), "Deleting");
        assert_eq!(friendly_tool_name("list_dir"), "Listing");
        assert_eq!(friendly_tool_name("bash"), "bash");
        assert_eq!(friendly_tool_name("grep"), "grep");
        assert_eq!(friendly_tool_name("glob"), "Globbing");
    }
}
