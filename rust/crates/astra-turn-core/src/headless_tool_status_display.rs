//! One-line tool argument previews and post-execution summaries for headless stderr (CLI styles strings).

use serde_json::{Map, Value};

use astra_text_utils::str_preview::truncate_str;

#[derive(Debug, Clone, Copy)]
enum ToolCat {
    Github,
    File,
    Shell,
    Search,
    Git,
    Mo,
    Memory,
    Other,
}

fn categorize(name: &str) -> ToolCat {
    match name {
        n if n.starts_with("github_") => ToolCat::Github,
        "read_file" | "view_file" | "write_file" | "edit_file" | "str_replace" => ToolCat::File,
        "run_command" | "shell" | "exec" | "bash" => ToolCat::Shell,
        "search" | "grep" | "find" | "glob" | "list_dir" => ToolCat::Search,
        "git_diff" | "git_log" | "git_show" | "git_blame" | "git_log_search" | "git_status" => {
            ToolCat::Git
        }
        "mo_query" => ToolCat::Mo,
        n if n.starts_with("memoria_") || n.starts_with("memory_") => ToolCat::Memory,
        _ => ToolCat::Other,
    }
}

fn fmt_github_tool(_name: &str, obj: &Map<String, Value>) -> Option<String> {
    let owner = obj.get("owner").and_then(|v| v.as_str());
    let repo = obj.get("repo").and_then(|v| v.as_str());
    match (owner, repo) {
        (Some(o), Some(r)) => Some(format!("{o}/{r}")),
        _ => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| truncate_str(q, 60)),
    }
}

fn fmt_file_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "read_file" | "view_file" => {
            let path = obj.get("path").and_then(|v| v.as_str())?;
            let start = obj.get("start_line").and_then(|v| v.as_u64());
            let end = obj.get("end_line").and_then(|v| v.as_u64());
            let outline = obj
                .get("outline")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if outline {
                Some(format!("{path} (outline)"))
            } else {
                match (start, end) {
                    (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                    (Some(s), None) => Some(format!("{path}:{s}-")),
                    _ => Some(path.to_string()),
                }
            }
        }
        "write_file" | "edit_file" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        "str_replace" => {
            let path = obj.get("path").and_then(|v| v.as_str())?;
            let old = obj.get("old_str").and_then(|v| v.as_str());
            match old {
                Some(s) => {
                    let first_line = s.lines().next().unwrap_or("");
                    let preview = truncate_str(first_line, 40);
                    let line_count = s.lines().count();
                    if line_count > 1 {
                        Some(format!("{path} ({line_count} lines)"))
                    } else {
                        Some(format!("{path}: \"{preview}\""))
                    }
                }
                None => Some(path.to_string()),
            }
        }
        _ => None,
    }
}

fn fmt_shell_tool(_name: &str, obj: &Map<String, Value>) -> Option<String> {
    obj.get("command")
        .and_then(|v| v.as_str())
        .map(|c| truncate_str(c, 60))
}

fn fmt_search_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "search" | "grep" | "find" => {
            let pattern = obj
                .get("query")
                .or_else(|| obj.get("pattern"))
                .and_then(|v| v.as_str());
            let path = obj.get("path").and_then(|v| v.as_str());
            match (pattern, path) {
                (Some(p), Some(dir)) => Some(format!("\"{}\" in {dir}", truncate_str(p, 40))),
                (Some(p), None) => Some(format!("\"{}\"", truncate_str(p, 50))),
                _ => None,
            }
        }
        "glob" => obj
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| truncate_str(p, 60)),
        "list_dir" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        _ => None,
    }
}

fn fmt_git_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "git_diff" => {
            let path = obj.get("path").and_then(|v| v.as_str());
            let staged = obj.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
            let base_ref = obj.get("base_ref").and_then(|v| v.as_str());
            let git_ref = obj.get("ref").and_then(|v| v.as_str());
            if let Some(base) = base_ref {
                let tip = git_ref.unwrap_or("HEAD");
                let range = format!("{base}..{tip}");
                return match path {
                    Some(p) => Some(format!("{range} -- {p}")),
                    None => Some(range),
                };
            }
            let suffix = if staged { " (staged)" } else { "" };
            match path {
                Some(p) => Some(format!("{p}{suffix}")),
                None => Some(format!("working tree{suffix}")),
            }
        }
        "git_log" => {
            let n = obj.get("max_count").and_then(|v| v.as_u64());
            let path = obj.get("path").and_then(|v| v.as_str());
            match (path, n) {
                (Some(p), Some(n)) => Some(format!("{p} (last {n})")),
                (Some(p), None) => Some(p.to_string()),
                (None, Some(n)) => Some(format!("last {n} commits")),
                _ => None,
            }
        }
        "git_show" => obj
            .get("revision")
            .and_then(|v| v.as_str())
            .map(|r| truncate_str(r, 40)),
        "git_blame" => {
            let path = obj.get("path").and_then(|v| v.as_str())?;
            let start = obj.get("start_line").and_then(|v| v.as_u64());
            let end = obj.get("end_line").and_then(|v| v.as_u64());
            match (start, end) {
                (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                _ => Some(path.to_string()),
            }
        }
        "git_log_search" => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| format!("\"{}\"", truncate_str(q, 50))),
        _ => None,
    }
}

fn fmt_mo_tool(_name: &str, obj: &Map<String, Value>) -> Option<String> {
    obj.get("sql")
        .and_then(|v| v.as_str())
        .map(|s| truncate_str(s, 60))
}

fn fmt_memory_tool(_name: &str, obj: &Map<String, Value>) -> Option<String> {
    obj.get("query")
        .or_else(|| obj.get("content"))
        .and_then(|v| v.as_str())
        .map(|q| truncate_str(q, 50))
}

fn fmt_default(obj: &Map<String, Value>) -> Option<String> {
    obj.values()
        .find_map(|v| v.as_str())
        .map(|s| truncate_str(s, 60))
}

/// Extract a brief detail string from tool call arguments for the └ line.
#[must_use]
pub fn tool_call_detail(name: &str, args: &Value) -> Option<String> {
    let obj = args.as_object()?;
    match categorize(name) {
        ToolCat::Github => fmt_github_tool(name, obj),
        ToolCat::File => fmt_file_tool(name, obj),
        ToolCat::Shell => fmt_shell_tool(name, obj),
        ToolCat::Search => fmt_search_tool(name, obj),
        ToolCat::Git => fmt_git_tool(name, obj),
        ToolCat::Mo => fmt_mo_tool(name, obj),
        ToolCat::Memory => fmt_memory_tool(name, obj),
        ToolCat::Other => fmt_default(obj),
    }
}

/// Build a brief summary of a tool result for the status line (after execution).
#[must_use]
pub fn tool_result_summary(name: &str, result: &str) -> Option<String> {
    match name {
        "read_file" | "view_file" => {
            let lines = result.lines().count();
            let truncated = result.contains("[truncated");
            if truncated {
                Some(format!("{lines} lines [truncated]"))
            } else if lines > 0 {
                Some(format!("{lines} lines"))
            } else {
                None
            }
        }
        "write_file" => {
            if let Ok(json) = serde_json::from_str::<Value>(result) {
                if json
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    if let Some(n) = json.get("bytes_written").and_then(|v| v.as_u64()) {
                        if n >= 1024 {
                            Some(format!("{:.1}KB written", n as f64 / 1024.0))
                        } else {
                            Some(format!("{n} bytes written"))
                        }
                    } else {
                        Some("written".to_string())
                    }
                } else {
                    json.get("error")
                        .and_then(|v| v.as_str())
                        .map(|err| format!("Error: {err}"))
                }
            } else {
                None
            }
        }
        "str_replace" => {
            if result.starts_with("Replaced successfully") {
                let line_count = result.lines().skip(1).count();
                if line_count > 0 {
                    Some(format!("{line_count} lines changed"))
                } else {
                    Some("replaced".to_string())
                }
            } else {
                None
            }
        }
        "bash" | "run_command" | "shell" | "exec" => {
            let lines = result.lines().count();
            let truncated = result.contains("[truncated]");
            if truncated {
                Some(format!("{lines} lines [truncated]"))
            } else if lines > 3 {
                Some(format!("{lines} lines"))
            } else {
                None
            }
        }
        "grep" | "search" | "find" => {
            if result == "No matches found" {
                Some("0 matches".to_string())
            } else {
                let lines = result.lines().count();
                let truncated = result.contains("[truncated]");
                if truncated {
                    Some(format!("{lines}+ matches [truncated]"))
                } else {
                    Some(format!("{lines} matches"))
                }
            }
        }
        "glob" => {
            if result == "No files found" {
                Some("0 files".to_string())
            } else {
                let count = result.lines().count();
                Some(format!("{count} files"))
            }
        }
        "list_dir" => {
            let count = result.lines().count();
            Some(format!("{count} entries"))
        }
        "git_diff" => {
            if result.trim().is_empty() {
                Some("no changes".to_string())
            } else {
                let adds = result.lines().filter(|l| l.starts_with('+')).count();
                let dels = result.lines().filter(|l| l.starts_with('-')).count();
                if adds > 0 || dels > 0 {
                    Some(format!("+{adds} -{dels}"))
                } else {
                    let lines = result.lines().count();
                    Some(format!("{lines} lines"))
                }
            }
        }
        "git_log" => {
            let commits = result
                .lines()
                .filter(|l| l.starts_with("commit ") || l.starts_with("* "))
                .count();
            if commits > 0 {
                Some(format!("{commits} commits"))
            } else {
                None
            }
        }
        "git_status" => {
            let changed = result.lines().filter(|l| !l.trim().is_empty()).count();
            if changed == 0 {
                Some("clean".to_string())
            } else {
                Some(format!("{changed} files"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_call_detail_github_shows_owner_repo() {
        let detail = tool_call_detail(
            "github_ci_status",
            &json!({"owner": "matrixorigin", "repo": "matrixone"}),
        );
        assert_eq!(detail.as_deref(), Some("matrixorigin/matrixone"));
    }

    #[test]
    fn tool_call_detail_bash_shows_command() {
        let detail = tool_call_detail("bash", &json!({"command": "ls -la"}));
        assert_eq!(detail.as_deref(), Some("ls -la"));
    }

    #[test]
    fn tool_call_detail_read_file_shows_path() {
        let detail = tool_call_detail("read_file", &json!({"path": "src/main.rs"}));
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_memory_shows_query() {
        let detail = tool_call_detail("memory_search", &json!({"query": "memoria repo"}));
        assert_eq!(detail.as_deref(), Some("memoria repo"));
    }

    #[test]
    fn tool_call_detail_grep_shows_pattern() {
        let detail = tool_call_detail("grep", &json!({"pattern": "TODO"}));
        assert_eq!(detail.as_deref(), Some("\"TODO\""));
    }

    #[test]
    fn tool_call_detail_grep_with_path() {
        let detail = tool_call_detail("grep", &json!({"pattern": "TODO", "path": "src/"}));
        assert_eq!(detail.as_deref(), Some("\"TODO\" in src/"));
    }

    #[test]
    fn tool_call_detail_read_file_with_line_range() {
        let detail = tool_call_detail(
            "read_file",
            &json!({"path": "src/main.rs", "start_line": 10, "end_line": 50}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs:10-50"));
    }

    #[test]
    fn tool_call_detail_read_file_outline() {
        let detail = tool_call_detail(
            "read_file",
            &json!({"path": "src/main.rs", "outline": true}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs (outline)"));
    }

    #[test]
    fn tool_call_detail_str_replace_shows_path_and_lines() {
        let detail = tool_call_detail(
            "str_replace",
            &json!({"path": "src/lib.rs", "old_str": "line1\nline2\nline3"}),
        );
        assert_eq!(detail.as_deref(), Some("src/lib.rs (3 lines)"));
    }

    #[test]
    fn tool_call_detail_git_diff_staged() {
        let detail = tool_call_detail("git_diff", &json!({"staged": true}));
        assert_eq!(detail.as_deref(), Some("working tree (staged)"));
    }

    #[test]
    fn tool_call_detail_git_diff_range() {
        let detail = tool_call_detail("git_diff", &json!({"base_ref": "HEAD~5", "ref": "HEAD"}));
        assert_eq!(detail.as_deref(), Some("HEAD~5..HEAD"));
    }

    #[test]
    fn tool_call_detail_git_diff_range_with_path() {
        let detail = tool_call_detail(
            "git_diff",
            &json!({"base_ref": "HEAD~3", "ref": "HEAD", "path": "src/main.rs"}),
        );
        assert_eq!(detail.as_deref(), Some("HEAD~3..HEAD -- src/main.rs"));
    }

    #[test]
    fn tool_call_detail_git_log_with_count() {
        let detail = tool_call_detail("git_log", &json!({"max_count": 5}));
        assert_eq!(detail.as_deref(), Some("last 5 commits"));
    }

    #[test]
    fn result_summary_read_file_line_count() {
        let result = "fn main() {\n    println!(\"hi\");\n}\n";
        let summary = tool_result_summary("read_file", result);
        assert_eq!(summary.as_deref(), Some("3 lines"));
    }

    #[test]
    fn result_summary_read_file_truncated() {
        let result = "some content\n[truncated]";
        let summary = tool_result_summary("read_file", result);
        assert_eq!(summary.as_deref(), Some("2 lines [truncated]"));
    }

    #[test]
    fn result_summary_write_file_bytes() {
        let result = r#"{"success": true, "bytes_written": 2048, "path": "/tmp/foo.rs"}"#;
        let summary = tool_result_summary("write_file", result);
        assert_eq!(summary.as_deref(), Some("2.0KB written"));
    }

    #[test]
    fn result_summary_write_file_small() {
        let result = r#"{"success": true, "bytes_written": 128, "path": "/tmp/foo.rs"}"#;
        let summary = tool_result_summary("write_file", result);
        assert_eq!(summary.as_deref(), Some("128 bytes written"));
    }

    #[test]
    fn result_summary_str_replace() {
        let summary = tool_result_summary("str_replace", "Replaced successfully\n- old\n+ new");
        assert_eq!(summary.as_deref(), Some("2 lines changed"));
    }

    #[test]
    fn result_summary_grep_matches() {
        let result = "src/a.rs:10:match1\nsrc/b.rs:20:match2\nsrc/c.rs:30:match3";
        let summary = tool_result_summary("grep", result);
        assert_eq!(summary.as_deref(), Some("3 matches"));
    }

    #[test]
    fn result_summary_grep_no_matches() {
        let summary = tool_result_summary("grep", "No matches found");
        assert_eq!(summary.as_deref(), Some("0 matches"));
    }

    #[test]
    fn result_summary_glob_files() {
        let result = "src/a.rs\nsrc/b.rs";
        let summary = tool_result_summary("glob", result);
        assert_eq!(summary.as_deref(), Some("2 files"));
    }

    #[test]
    fn result_summary_git_diff_empty() {
        let summary = tool_result_summary("git_diff", "");
        assert_eq!(summary.as_deref(), Some("no changes"));
    }

    #[test]
    fn result_summary_git_status_clean() {
        let summary = tool_result_summary("git_status", "");
        assert_eq!(summary.as_deref(), Some("clean"));
    }

    #[test]
    fn result_summary_bash_short_no_summary() {
        let summary = tool_result_summary("bash", "ok\ndone");
        assert!(summary.is_none());
    }

    #[test]
    fn result_summary_bash_long_shows_lines() {
        let result = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = tool_result_summary("bash", &result);
        assert_eq!(summary.as_deref(), Some("10 lines"));
    }
}
