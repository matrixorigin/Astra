//! One-line tool argument previews and post-execution summaries for headless stderr (CLI styles strings).

use serde_json::{Map, Value};

use astra_text_utils::str_preview::{github_repo_display, shorten_path, truncate_str};

use crate::orchestration::agent_result_wire::agent_tool_status_summary;
use crate::tool::categories::{ToolDisplayCategory, registry};

fn format_path_location(
    path: &str,
    line: Option<u64>,
    column: Option<u64>,
    max_chars: usize,
) -> String {
    match (line, column) {
        (Some(line), Some(column)) => {
            let suffix_len = format!(":{line}:{column}").chars().count();
            let short_path = shorten_path(path, max_chars.saturating_sub(suffix_len));
            format!("{short_path}:{line}:{column}")
        }
        (Some(line), None) => {
            let suffix_len = format!(":{line}").chars().count();
            let short_path = shorten_path(path, max_chars.saturating_sub(suffix_len));
            format!("{short_path}:{line}")
        }
        (None, _) => shorten_path(path, max_chars),
    }
}

fn fmt_github_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    let owner = obj.get("owner").and_then(|v| v.as_str());
    let repo = obj.get("repo").and_then(|v| v.as_str());
    let repo_display = github_repo_display(owner, repo);
    let number = obj
        .get("number")
        .or_else(|| obj.get("pr_number"))
        .or_else(|| obj.get("issue_number"))
        .and_then(|v| v.as_u64());

    match (name, obj.get("action").and_then(Value::as_str)) {
        ("github", Some("create_issue")) => {
            let title = obj.get("title").and_then(|v| v.as_str());
            match (repo_display, title) {
                (Some(repo), Some(title)) => {
                    Some(format!(r#"{repo}: "{}""#, truncate_str(title, 40)))
                }
                (Some(repo), None) => Some(repo),
                (None, Some(title)) => Some(truncate_str(title, 50)),
                (None, None) => None,
            }
        }
        ("github", Some("get_pr" | "get_issue")) => match (repo_display, number) {
            (Some(repo), Some(number)) => Some(format!("{repo}#{number}")),
            (Some(repo), None) => Some(repo),
            (None, Some(_)) => None,
            (None, None) => obj
                .get("query")
                .and_then(|v| v.as_str())
                .map(|q| truncate_str(q, 60)),
        },
        ("github", Some("list_prs" | "list_issues" | "repo_stats" | "ci_status")) => {
            match repo_display {
                Some(repo) => Some(repo),
                None => obj
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(|q| truncate_str(q, 60)),
            }
        }
        ("github", _) => match repo_display {
            Some(repo) => Some(repo),
            None => obj
                .get("query")
                .and_then(|v| v.as_str())
                .map(|q| truncate_str(q, 60)),
        },
        _ => None,
    }
}

fn fmt_file_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "read_file" => {
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
        "write_file" | "create_file" | "edit_file" | "multi_edit" | "delete_file" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        "str_replace" => {
            let path = obj.get("path").and_then(|v| v.as_str()).or_else(|| {
                obj.get("edits")
                    .and_then(|v| v.as_array())
                    .and_then(|edits| edits.first())
                    .and_then(|edit| edit.get("path"))
                    .and_then(|v| v.as_str())
            })?;
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
                None => {
                    let file_count = obj
                        .get("edits")
                        .and_then(|v| v.as_array())
                        .map(|edits| {
                            let mut paths = std::collections::BTreeSet::new();
                            for edit in edits {
                                if let Some(path) = edit.get("path").and_then(|v| v.as_str()) {
                                    paths.insert(path);
                                }
                            }
                            paths.len()
                        })
                        .unwrap_or(0);
                    if file_count > 1 {
                        Some(format!("{path} (+{} files)", file_count - 1))
                    } else {
                        Some(path.to_string())
                    }
                }
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
        "search" | "grep" | "find" | "tool_search" | "web_search" => {
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
        "web_fetch" => obj
            .get("url")
            .and_then(|v| v.as_str())
            .map(|url| truncate_str(url, 60)),
        "list_dir" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        _ => None,
    }
}

fn fmt_git_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "git" => match obj.get("action").and_then(Value::as_str) {
            Some("diff") => {
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
            Some("log") => {
                let n = obj
                    .get("n")
                    .or_else(|| obj.get("max_count"))
                    .and_then(|v| v.as_u64());
                let path = obj.get("path").and_then(|v| v.as_str());
                match (path, n) {
                    (Some(p), Some(n)) => Some(format!("{p} (last {n})")),
                    (Some(p), None) => Some(p.to_string()),
                    (None, Some(n)) => Some(format!("last {n} commits")),
                    _ => None,
                }
            }
            Some("show") => obj
                .get("revision")
                .or_else(|| obj.get("ref"))
                .and_then(|v| v.as_str())
                .map(|r| truncate_str(r, 40)),
            Some("blame") => {
                let path = obj.get("path").and_then(|v| v.as_str())?;
                let start = obj.get("start_line").and_then(|v| v.as_u64());
                let end = obj.get("end_line").and_then(|v| v.as_u64());
                match (start, end) {
                    (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                    _ => Some(path.to_string()),
                }
            }
            Some("log_search") => obj
                .get("query")
                .and_then(|v| v.as_str())
                .map(|q| format!("\"{}\"", truncate_str(q, 50))),
            Some("file_history") => obj
                .get("file")
                .and_then(|v| v.as_str())
                .map(|path| shorten_path(path, 60)),
            Some("contributors") => {
                let path = obj.get("path").and_then(|v| v.as_str());
                let since = obj.get("since").and_then(|v| v.as_str());
                match (path, since) {
                    (Some(path), Some(since)) => Some(format!(
                        "{} since {}",
                        shorten_path(path, 36),
                        truncate_str(since, 20)
                    )),
                    (Some(path), None) => Some(shorten_path(path, 60)),
                    (None, Some(since)) => Some(format!("since {}", truncate_str(since, 24))),
                    (None, None) => None,
                }
            }
            Some("commit") => obj
                .get("message")
                .and_then(|v| v.as_str())
                .map(|message| truncate_str(message, 60)),
            Some("revert_commit") => obj
                .get("commit_sha")
                .and_then(|v| v.as_str())
                .map(|sha| truncate_str(sha, 16)),
            Some("stash") => {
                let sub_action = obj
                    .get("sub_action")
                    .or_else(|| obj.get("stash_action"))
                    .and_then(|v| v.as_str());
                let stash_ref = obj.get("stash_ref").and_then(|v| v.as_str());
                let index = obj.get("index").and_then(|v| v.as_i64());
                match (sub_action, stash_ref, index) {
                    (Some(action), Some(stash_ref), _) => {
                        Some(format!("{action} {}", truncate_str(stash_ref, 32)))
                    }
                    (Some(action), None, Some(index)) => {
                        Some(format!("{action} stash@{{{index}}}"))
                    }
                    (Some(action), None, None) => Some(action.to_string()),
                    _ => None,
                }
            }
            Some("checkout_file") => {
                let path = obj.get("path").and_then(|v| v.as_str());
                let git_ref = obj.get("ref").and_then(|v| v.as_str());
                match (path, git_ref) {
                    (Some(path), Some(git_ref)) => {
                        Some(format!("{git_ref} -- {}", shorten_path(path, 40)))
                    }
                    (Some(path), None) => Some(shorten_path(path, 60)),
                    _ => None,
                }
            }
            Some("worktree") => {
                let sub_action = obj
                    .get("sub_action")
                    .or_else(|| obj.get("worktree_action"))
                    .and_then(|v| v.as_str());
                let branch = obj.get("branch").and_then(|v| v.as_str());
                let path = obj.get("path").and_then(|v| v.as_str());
                match (sub_action, branch, path) {
                    (Some(action), Some(branch), _) => Some(format!(
                        "{} {}",
                        truncate_str(action, 16),
                        truncate_str(branch, 30)
                    )),
                    (Some(action), None, Some(path)) => Some(format!(
                        "{} {}",
                        truncate_str(action, 16),
                        truncate_str(path, 30)
                    )),
                    (Some(action), None, None) => Some(action.to_string()),
                    _ => None,
                }
            }
            Some("status") => None,
            _ => None,
        },
        _ => None,
    }
}

fn fmt_code_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "find_definition" | "find_references" => obj
            .get("symbol")
            .and_then(|v| v.as_str())
            .map(|symbol| truncate_str(symbol, 50)),
        "symbol_search" => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|query| truncate_str(query, 50)),
        "symbols" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|path| shorten_path(path, 60)),
        "call_graph" => {
            let symbol = obj.get("symbol").and_then(|v| v.as_str());
            let path = obj.get("path").and_then(|v| v.as_str());
            let start = obj.get("start_line").and_then(|v| v.as_u64());
            let end = obj.get("end_line").and_then(|v| v.as_u64());
            match (symbol, path, start, end) {
                (Some(symbol), _, _, _) => Some(truncate_str(symbol, 50)),
                (None, Some(path), Some(start), Some(end)) => Some(format!(
                    "{}:{start}-{end}",
                    shorten_path(
                        path,
                        40usize.saturating_sub(format!(":{start}-{end}").chars().count())
                    )
                )),
                (None, Some(path), Some(start), None) => Some(format!(
                    "{}:{start}-",
                    shorten_path(
                        path,
                        40usize.saturating_sub(format!(":{start}-").chars().count())
                    )
                )),
                (None, Some(path), None, None) => Some(shorten_path(path, 60)),
                _ => None,
            }
        }
        "hover_info" => {
            let file = obj.get("file").and_then(|v| v.as_str())?;
            let line = obj.get("line").and_then(|v| v.as_u64());
            let column = obj.get("column").and_then(|v| v.as_u64());
            Some(format_path_location(file, line, column, 40))
        }
        "type_hierarchy" => {
            let name = obj.get("name").and_then(|v| v.as_str());
            let direction = obj.get("direction").and_then(|v| v.as_str());
            match (name, direction) {
                (Some(name), Some(direction)) => Some(format!(
                    "{} ({})",
                    truncate_str(name, 36),
                    truncate_str(direction, 16)
                )),
                (Some(name), None) => Some(truncate_str(name, 50)),
                _ => None,
            }
        }
        "rename_symbol" => {
            let symbol = obj.get("symbol").and_then(|v| v.as_str());
            let new_name = obj.get("new_name").and_then(|v| v.as_str());
            match (symbol, new_name) {
                (Some(symbol), Some(new_name)) => Some(format!(
                    "{} -> {}",
                    truncate_str(symbol, 24),
                    truncate_str(new_name, 24)
                )),
                (Some(symbol), None) => Some(truncate_str(symbol, 50)),
                _ => None,
            }
        }
        "dead_code" => {
            let path = obj.get("path").and_then(|v| v.as_str());
            let kind = obj.get("kind").and_then(|v| v.as_str());
            match (path, kind) {
                (Some(path), Some(kind)) => Some(format!(
                    "{} ({})",
                    truncate_str(path, 36),
                    truncate_str(kind, 16)
                )),
                (Some(path), None) => Some(truncate_str(path, 50)),
                (None, Some(kind)) => Some(truncate_str(kind, 24)),
                _ => None,
            }
        }
        "extract_members" => {
            let file = obj.get("file").and_then(|v| v.as_str());
            let line = obj.get("line").and_then(|v| v.as_u64());
            match (file, line) {
                (Some(file), Some(line)) => Some(format_path_location(file, Some(line), None, 40)),
                (Some(file), None) => Some(shorten_path(file, 60)),
                _ => None,
            }
        }
        "lsp" => {
            let operation = obj.get("operation").and_then(|v| v.as_str());
            let file = obj.get("file").and_then(|v| v.as_str());
            let line = obj.get("line").and_then(|v| v.as_u64());
            let column = obj.get("column").and_then(|v| v.as_u64());
            let symbol = obj.get("symbol").and_then(|v| v.as_str());
            let query = obj.get("query").and_then(|v| v.as_str());
            match (operation, file, line, column, symbol, query) {
                (Some(operation), Some(file), Some(line), Some(column), _, _) => Some(format!(
                    "{operation} {}",
                    format_path_location(file, Some(line), Some(column), 32)
                )),
                (Some(operation), Some(file), _, _, _, _) => {
                    Some(format!("{operation} {}", shorten_path(file, 32)))
                }
                (Some(operation), _, _, _, Some(symbol), _) => Some(format!(
                    "{} {}",
                    truncate_str(operation, 20),
                    truncate_str(symbol, 26)
                )),
                (Some(operation), _, _, _, _, Some(query)) => Some(format!(
                    "{} {}",
                    truncate_str(operation, 20),
                    truncate_str(query, 26)
                )),
                (Some(operation), _, _, _, _, _) => Some(truncate_str(operation, 40)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn fmt_mo_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "mo_query" => obj
            .get("sql")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 60)),
        _ => None,
    }
}

fn fmt_memory_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    // `memory` is the single consolidated tool; the `action` field picks
    // a v2 cognitive verb (remember / recall / expand / forget / update /
    // focus / reflect / profile / feedback).
    if name != "memory" {
        return None;
    }
    let action = obj.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match action {
        "recall" => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| truncate_str(q, 50)),
        "remember" => obj
            .get("content")
            .and_then(|v| v.as_str())
            .map(|content| truncate_str(content, 50)),
        "forget" => obj
            .get("memory_id")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("topic").and_then(|v| v.as_str()))
            .map(|t| truncate_str(t, 40)),
        "update" | "expand" | "feedback" => obj
            .get("memory_id")
            .and_then(|v| v.as_str())
            .map(|memory_id| truncate_str(memory_id, 40)),
        "focus" => obj
            .get("focus_value")
            .or_else(|| obj.get("value"))
            .and_then(|v| v.as_str())
            .map(|v| truncate_str(v, 40)),
        _ => None,
    }
}

fn fmt_utility_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "ask_user" => obj
            .get("questions")
            .and_then(|v| v.as_array())
            .and_then(|questions| questions.first())
            .and_then(|question| question.get("question"))
            .and_then(|v| v.as_str())
            .map(|q| truncate_str(q, 60)),
        "sleep" => {
            let duration_ms = obj.get("duration_ms").and_then(|v| v.as_u64());
            let reason = obj.get("reason").and_then(|v| v.as_str());
            match (duration_ms, reason) {
                (Some(duration_ms), Some(reason)) => {
                    Some(format!("{duration_ms}ms ({})", truncate_str(reason, 36)))
                }
                (Some(duration_ms), None) => Some(format!("{duration_ms}ms")),
                (None, Some(reason)) => Some(truncate_str(reason, 50)),
                (None, None) => None,
            }
        }
        "send_message" => {
            let to = obj.get("to").and_then(|v| v.as_str());
            let summary = obj.get("summary").and_then(|v| v.as_str());
            let message = obj.get("message").and_then(|v| v.as_str());
            match (to, summary, message) {
                (Some(to), Some(summary), _) => Some(format!(
                    "{}: {}",
                    truncate_str(to, 18),
                    truncate_str(summary, 28)
                )),
                (Some(to), None, Some(message)) => Some(format!(
                    "{}: {}",
                    truncate_str(to, 18),
                    truncate_str(message, 28)
                )),
                (Some(to), None, None) => Some(truncate_str(to, 40)),
                _ => None,
            }
        }
        // Consolidated agent tool
        "agent" => {
            let action = obj.get("action").and_then(|v| v.as_str()).unwrap_or("");
            match action {
                "spawn" => {
                    let description = obj.get("description").and_then(|v| v.as_str());
                    let agent_type = obj.get("agent_type").and_then(|v| v.as_str());
                    match (description, agent_type) {
                        (Some(desc), Some(at)) => Some(format!(
                            "spawn {} ({})",
                            truncate_str(desc, 28),
                            truncate_str(at, 12)
                        )),
                        (Some(desc), None) => Some(format!("spawn {}", truncate_str(desc, 40))),
                        (None, Some(at)) => Some(format!("spawn ({})", truncate_str(at, 20))),
                        _ => Some("spawn".to_string()),
                    }
                }
                "get_result" => obj
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .map(|id| format!("get_result {}", truncate_str(id, 36))),
                "send_message" => {
                    let to = obj.get("to").and_then(|v| v.as_str());
                    let summary = obj.get("summary").and_then(|v| v.as_str());
                    let message = obj.get("message").and_then(|v| v.as_str());
                    match (to, summary, message) {
                        (Some(to), Some(summary), _) => Some(format!(
                            "send {}: {}",
                            truncate_str(to, 14),
                            truncate_str(summary, 24)
                        )),
                        (Some(to), None, Some(message)) => Some(format!(
                            "send {}: {}",
                            truncate_str(to, 14),
                            truncate_str(message, 24)
                        )),
                        (Some(to), None, None) => Some(format!("send {}", truncate_str(to, 36))),
                        _ => None,
                    }
                }
                "delegate" => obj
                    .get("task")
                    .and_then(|v| v.as_str())
                    .map(|t| format!("delegate: {}", truncate_str(t, 36))),
                "run_chain" => obj
                    .get("name")
                    .or_else(|| obj.get("description"))
                    .and_then(|v| v.as_str())
                    .map(|n| format!("chain: {}", truncate_str(n, 36))),
                _ => Some(action.to_string()),
            }
        }
        "diagnose" => {
            let category = obj.get("category").and_then(|v| v.as_str());
            let verbose = obj.get("verbose").and_then(|v| v.as_bool());
            match (category, verbose) {
                (Some(category), Some(true)) => Some(format!("{category} verbose")),
                (Some(category), _) => Some(category.to_string()),
                (None, Some(true)) => Some("verbose".to_string()),
                _ => None,
            }
        }
        "env" => {
            let operation = obj.get("operation").and_then(|v| v.as_str());
            let name = obj.get("name").and_then(|v| v.as_str());
            let pattern = obj.get("pattern").and_then(|v| v.as_str());
            match (operation, name, pattern) {
                (Some(operation), Some(name), _) => {
                    Some(format!("{operation} {}", truncate_str(name, 30)))
                }
                (Some("search"), _, Some(pattern)) => {
                    Some(format!("search {}", truncate_str(pattern, 24)))
                }
                (Some(operation), _, _) => Some(operation.to_string()),
                _ => None,
            }
        }
        "notebook_edit" => {
            let notebook_path = obj.get("notebook_path").and_then(|v| v.as_str());
            let edit_mode = obj.get("edit_mode").and_then(|v| v.as_str());
            match (edit_mode, notebook_path) {
                (Some(edit_mode), Some(notebook_path)) => Some(format!(
                    "{} {}",
                    truncate_str(edit_mode, 12),
                    truncate_str(notebook_path, 32)
                )),
                (_, Some(notebook_path)) => Some(truncate_str(notebook_path, 50)),
                _ => None,
            }
        }
        "config" => {
            let setting = obj.get("setting").and_then(|v| v.as_str());
            let value = obj.get("value").and_then(|v| v.as_str());
            match (setting, value) {
                (Some(setting), Some(value)) => Some(format!(
                    "{}={}",
                    truncate_str(setting, 18),
                    truncate_str(value, 24)
                )),
                (Some(setting), None) => Some(truncate_str(setting, 40)),
                _ => None,
            }
        }
        "brief" => obj
            .get("focus")
            .and_then(|v| v.as_str())
            .map(|focus| truncate_str(focus, 24)),
        "share_context" => obj
            .get("key")
            .and_then(|v| v.as_str())
            .map(|key| truncate_str(key, 50)),
        "query_context" => {
            let key = obj.get("key").and_then(|v| v.as_str());
            let prefix = obj.get("prefix").and_then(|v| v.as_str());
            let list_keys = obj.get("list_keys").and_then(|v| v.as_bool());
            match (key, prefix, list_keys) {
                (Some(key), _, _) => Some(truncate_str(key, 50)),
                (None, Some(prefix), _) => Some(truncate_str(prefix, 50)),
                (None, None, Some(true)) => Some("keys".to_string()),
                _ => None,
            }
        }
        "task" => task_tool_detail(obj),
        "get_agent_info" => obj
            .get("dimension")
            .and_then(|v| v.as_str())
            .map(|dimension| truncate_str(dimension, 30)),
        "reflect" => obj
            .get("question")
            .or_else(|| obj.get("focus"))
            .and_then(|v| v.as_str())
            .map(|value| truncate_str(value, 50)),
        "context_analysis" => {
            let mode = obj.get("mode").and_then(|v| v.as_str());
            let turn = obj.get("turn").and_then(|v| v.as_i64());
            let turn_a = obj.get("turn_a").and_then(|v| v.as_i64());
            let turn_b = obj.get("turn_b").and_then(|v| v.as_i64());
            match (mode, turn, turn_a, turn_b) {
                (Some("turn"), Some(turn), _, _) => Some(format!("turn {turn}")),
                (Some("compare"), _, Some(turn_a), Some(turn_b)) => {
                    Some(format!("compare {turn_a} vs {turn_b}"))
                }
                (Some(mode), _, _, _) => Some(mode.to_string()),
                _ => None,
            }
        }
        "run_chain" => obj
            .get("name")
            .or_else(|| obj.get("description"))
            .and_then(|v| v.as_str())
            .map(|value| truncate_str(value, 50)),
        "rollback_file_edits" => {
            let scope = obj.get("scope").and_then(|v| v.as_str());
            let turn_index = obj.get("turn_index").and_then(|v| v.as_i64());
            let path = obj.get("path").and_then(|v| v.as_str());
            match (scope, turn_index, path) {
                (Some("turn"), Some(turn_index), _) => Some(format!("turn {turn_index}")),
                (Some("file"), _, Some(path)) => Some(truncate_str(path, 40)),
                (Some(scope), _, _) => Some(scope.to_string()),
                _ => None,
            }
        }
        "rollback_database_snapshots" => {
            let scope = obj.get("scope").and_then(|v| v.as_str());
            let turn_index = obj.get("turn_index").and_then(|v| v.as_i64());
            let snapshot_id = obj.get("snapshot_id").and_then(|v| v.as_str());
            match (scope, turn_index, snapshot_id) {
                (Some("turn"), Some(turn_index), _) => Some(format!("turn {turn_index}")),
                (Some("snapshot"), _, Some(snapshot_id)) => Some(truncate_str(snapshot_id, 40)),
                (Some(scope), _, _) => Some(scope.to_string()),
                _ => None,
            }
        }
        "adjust_config" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|path| truncate_str(path, 40)),
        "compress_context" => obj
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|reason| truncate_str(reason, 50)),
        "rollback_session_state" => {
            let scope = obj.get("scope").and_then(|v| v.as_str());
            let turn_index = obj.get("turn_index").and_then(|v| v.as_i64());
            match (scope, turn_index) {
                (Some("turn"), Some(turn_index)) => Some(format!("turn {turn_index}")),
                (Some(scope), _) => Some(scope.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn task_tool_detail(obj: &Map<String, Value>) -> Option<String> {
    match obj.get("action").and_then(|v| v.as_str()).unwrap_or("list") {
        "create" => obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(|title| truncate_str(title, 60)),
        "list" => obj
            .get("status_filter")
            .and_then(|v| v.as_str())
            .map(|status| truncate_str(status, 30)),
        "get" | "stop" | "archive" | "adopt" => obj
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|task_id| truncate_str(task_id, 50)),
        "update" => {
            let task_id = obj.get("task_id").and_then(|v| v.as_str());
            let status = obj.get("new_status").and_then(|v| v.as_str());
            let subtask_id = obj.get("subtask_id").and_then(|v| v.as_str());
            match (task_id, subtask_id, status) {
                (Some(task_id), Some(subtask_id), Some(status)) => Some(format!(
                    "{}:{} -> {}",
                    truncate_str(task_id, 24),
                    truncate_str(subtask_id, 16),
                    truncate_str(status, 16)
                )),
                (Some(task_id), None, Some(status)) => Some(format!(
                    "{} -> {}",
                    truncate_str(task_id, 36),
                    truncate_str(status, 16)
                )),
                (Some(task_id), _, None) => Some(truncate_str(task_id, 50)),
                _ => None,
            }
        }
        _ => None,
    }
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
    match registry().display_category(name) {
        ToolDisplayCategory::Github => fmt_github_tool(name, obj),
        ToolDisplayCategory::File => fmt_file_tool(name, obj),
        ToolDisplayCategory::Shell => fmt_shell_tool(name, obj),
        ToolDisplayCategory::Search => fmt_search_tool(name, obj),
        ToolDisplayCategory::Git => fmt_git_tool(name, obj),
        ToolDisplayCategory::Code => fmt_code_tool(name, obj),
        ToolDisplayCategory::Mo => fmt_mo_tool(name, obj),
        ToolDisplayCategory::Memory => fmt_memory_tool(name, obj),
        ToolDisplayCategory::Utility => fmt_utility_tool(name, obj),
        ToolDisplayCategory::Other => fmt_default(obj),
    }
}

const TOOL_ERROR_SUMMARY_MAX_CHARS: usize = 100;

fn structured_field<'a>(result: &'a str, field: &str) -> Option<&'a str> {
    result
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(field).map(str::trim))
        .filter(|value| !value.is_empty())
}

fn json_error_summary(result: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    let obj = value.as_object()?;
    let explicitly_failed = obj.get("success").and_then(Value::as_bool) == Some(false)
        || obj.get("status").and_then(Value::as_str) == Some("failed")
        || obj.get("status").and_then(Value::as_str) == Some("error");
    if !explicitly_failed && !obj.contains_key("error") {
        return None;
    }
    for key in ["error", "message", "detail"] {
        if let Some(text) = obj.get(key).and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            return Some(text.trim().to_string());
        }
    }
    None
}

fn informative_error_line(result: &str) -> Option<&str> {
    const NEEDLES: &[&str] = &[
        "error:",
        "failed",
        "not found",
        "permission denied",
        "no such file",
        "missing",
        "invalid",
        "denied",
    ];
    result.lines().find(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        let lower = trimmed.to_lowercase();
        NEEDLES.iter().any(|needle| lower.contains(needle))
    })
}

/// Extract a compact, actionable error summary from arbitrary tool output.
///
/// Preference order is protocol-shaped rather than tool-shaped: structured JSON
/// error, `WHAT:` failure records, informative error lines, then the first
/// non-empty line. This keeps UI surfaces from collapsing rich failures like
/// `WHAT/WHY/NEXT` into a generic banner.
#[must_use]
pub fn tool_error_summary(tool_name: &str, result: &str) -> String {
    let result = astra_core::error_kind::strip_tool_binding_sentinel(result);
    let result = result.as_ref();
    let trimmed = result.trim();
    if trimmed.is_empty() {
        return format!("{tool_name} failed before returning output");
    }

    if let Some(summary) = json_error_summary(trimmed) {
        return truncate_str(&summary, TOOL_ERROR_SUMMARY_MAX_CHARS);
    }

    if let Some(what) = structured_field(result, "WHAT:") {
        return truncate_str(what, TOOL_ERROR_SUMMARY_MAX_CHARS);
    }

    if let Some(line) = informative_error_line(result) {
        return truncate_str(line.trim(), TOOL_ERROR_SUMMARY_MAX_CHARS);
    }

    let first = result
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(trimmed)
        .trim();
    truncate_str(first, TOOL_ERROR_SUMMARY_MAX_CHARS)
}

fn summarize_git_result(result: &str) -> Option<String> {
    if result.trim().is_empty() {
        return Some("clean/no changes".to_string());
    }

    if result.contains("--- ") && (result.contains("+++ ") || result.contains("diff --git ")) {
        let adds = result
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .count();
        let dels = result
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .count();
        if adds > 0 || dels > 0 {
            return Some(format!("+{adds} -{dels}"));
        }
    }

    let commits = result
        .lines()
        .filter(|line| line.starts_with("commit ") || line.starts_with("* "))
        .count();
    if commits > 0 {
        return Some(format!("{commits} commits"));
    }

    let lines = result
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    (lines > 0).then(|| format!("{lines} lines"))
}

/// Build a brief summary of a tool result for the status line (after execution).
#[must_use]
pub fn tool_result_summary(name: &str, result: &str) -> Option<String> {
    let result = astra_core::error_kind::strip_tool_binding_sentinel(result);
    let result = result.as_ref();
    match name {
        "read_file" => {
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
            if result.starts_with("Successfully applied edits to")
                || result.starts_with("Successfully applied ")
            {
                Some(result.lines().next().unwrap_or("edits applied").to_string())
            } else if result.starts_with("Replaced successfully") {
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
        "git" => summarize_git_result(result),
        "agent" => serde_json::from_str::<Value>(result)
            .ok()
            .as_ref()
            .and_then(agent_tool_status_summary),
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
            "github",
            &json!({"action": "ci_status", "owner": "matrixorigin", "repo": "matrixone"}),
        );
        assert_eq!(detail.as_deref(), Some("matrixorigin/matrixone"));
    }

    #[test]
    fn tool_call_detail_github_repo_arg_shows_repo_and_number() {
        let detail = tool_call_detail(
            "github",
            &json!({"action": "get_issue", "repo": "matrixorigin/astra", "issue_number": 147}),
        );
        assert_eq!(detail.as_deref(), Some("matrixorigin/astra#147"));
    }

    #[test]
    fn tool_call_detail_github_action_create_issue_shows_repo_and_title() {
        let detail = tool_call_detail(
            "github",
            &json!({
                "action": "create_issue",
                "repo": "matrixorigin/astra",
                "title": "Fix renderer drift"
            }),
        );
        assert_eq!(
            detail.as_deref(),
            Some(r#"matrixorigin/astra: "Fix renderer drift""#)
        );
    }

    #[test]
    fn tool_call_detail_bash_shows_command() {
        let detail = tool_call_detail("bash", &json!({"command": "ls -la"}));
        assert_eq!(detail.as_deref(), Some("ls -la"));
    }

    #[test]
    fn tool_call_detail_run_build_test_shows_command() {
        let detail = tool_call_detail("run_build_test", &json!({"command": "cargo test"}));
        assert_eq!(detail.as_deref(), Some("cargo test"));
    }

    #[test]
    fn tool_call_detail_powershell_shows_command() {
        let detail = tool_call_detail("powershell", &json!({"command": "Get-ChildItem"}));
        assert_eq!(detail.as_deref(), Some("Get-ChildItem"));
    }

    #[test]
    fn tool_call_detail_read_file_shows_path() {
        let detail = tool_call_detail("read_file", &json!({"path": "src/main.rs"}));
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_multi_edit_shows_path() {
        let detail = tool_call_detail(
            "multi_edit",
            &json!({"path": "src/main.rs", "edits": [{"old_str": "a", "new_str": "b"}]}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_delete_file_shows_path() {
        let detail = tool_call_detail("delete_file", &json!({"path": "src/old.rs"}));
        assert_eq!(detail.as_deref(), Some("src/old.rs"));
    }

    #[test]
    fn tool_call_detail_git_action_commit_shows_message() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "commit", "message": "ship the fix"}),
        );
        assert_eq!(detail.as_deref(), Some("ship the fix"));
    }

    #[test]
    fn tool_call_detail_git_action_stash_shows_action() {
        let detail = tool_call_detail("git", &json!({"action": "stash", "sub_action": "push"}));
        assert_eq!(detail.as_deref(), Some("push"));
    }

    #[test]
    fn tool_call_detail_git_action_file_history_shows_file() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "file_history", "file": "src/main.rs"}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_git_action_checkout_file_shortens_long_path() {
        let detail = tool_call_detail(
            "git",
            &json!({
                "action": "checkout_file",
                "path": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                "ref": "HEAD~1"
            }),
        )
        .expect("detail");
        assert!(detail.starts_with("HEAD~1 -- .../"));
        assert!(detail.ends_with("src/lib.rs"));
    }

    #[test]
    fn tool_call_detail_git_action_contributors_shows_filters() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "contributors", "path": "src/", "since": "30 days ago"}),
        );
        assert_eq!(detail.as_deref(), Some("src/ since 30 days ago"));
    }

    #[test]
    fn tool_call_detail_tool_search_shows_query() {
        let detail = tool_call_detail("tool_search", &json!({"query": "git"}));
        assert_eq!(detail.as_deref(), Some("\"git\""));
    }

    #[test]
    fn tool_call_detail_web_search_shows_query() {
        let detail = tool_call_detail("web_search", &json!({"query": "matrixone latest"}));
        assert_eq!(detail.as_deref(), Some("\"matrixone latest\""));
    }

    #[test]
    fn tool_call_detail_web_fetch_shows_url() {
        let detail = tool_call_detail("web_fetch", &json!({"url": "https://example.com/docs"}));
        assert_eq!(detail.as_deref(), Some("https://example.com/docs"));
    }

    #[test]
    fn tool_call_detail_ask_user_shows_question() {
        let detail = tool_call_detail(
            "ask_user",
            &json!({"questions": [{"header": "Confirm", "question": "Continue?", "options": ["Yes", "No"]}]}),
        );
        assert_eq!(detail.as_deref(), Some("Continue?"));
    }

    #[test]
    fn tool_call_detail_sleep_shows_duration() {
        let detail = tool_call_detail(
            "sleep",
            &json!({"duration_ms": 1500, "reason": "waiting for CI"}),
        );
        assert_eq!(detail.as_deref(), Some("1500ms (waiting for CI)"));
    }

    #[test]
    fn tool_call_detail_send_message_shows_summary() {
        let detail = tool_call_detail(
            "send_message",
            &json!({"to": "agent-2", "summary": "Need review on auth flow"}),
        );
        assert_eq!(detail.as_deref(), Some("agent-2: Need review on auth flow"));
    }

    #[test]
    fn tool_call_detail_env_shows_operation_and_name() {
        let detail = tool_call_detail("env", &json!({"operation": "get", "name": "PATH"}));
        assert_eq!(detail.as_deref(), Some("get PATH"));
    }

    #[test]
    fn tool_call_detail_notebook_edit_shows_mode_and_path() {
        let detail = tool_call_detail(
            "notebook_edit",
            &json!({"edit_mode": "replace", "notebook_path": "analysis.ipynb"}),
        );
        assert_eq!(detail.as_deref(), Some("replace analysis.ipynb"));
    }

    #[test]
    fn tool_call_detail_query_context_shows_prefix() {
        let detail = tool_call_detail("query_context", &json!({"prefix": "auth/"}));
        assert_eq!(detail.as_deref(), Some("auth/"));
    }

    #[test]
    fn tool_call_detail_lsp_shows_operation_and_position() {
        let detail = tool_call_detail(
            "lsp",
            &json!({"operation": "hover", "file": "src/lib.rs", "line": 12, "column": 3}),
        );
        assert_eq!(detail.as_deref(), Some("hover src/lib.rs:12:3"));
    }

    #[test]
    fn tool_call_detail_position_paths_are_shortened() {
        let detail = tool_call_detail(
            "hover_info",
            &json!({
                "file": "/very/long/path/to/deeply/nested/module/src/lib.rs",
                "line": 42,
                "column": 3
            }),
        )
        .expect("detail");
        assert!(detail.starts_with(".../"));
        assert!(detail.ends_with(":42:3"));
        assert!(detail.chars().count() <= 40);
    }

    #[test]
    fn tool_call_detail_call_graph_path_budget_respects_limit() {
        let detail = tool_call_detail(
            "call_graph",
            &json!({
                "path": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                "start_line": 10,
                "end_line": 24
            }),
        )
        .expect("detail");
        assert!(detail.starts_with(".../"));
        assert!(detail.ends_with(":10-24"));
        assert!(detail.chars().count() <= 40);
    }

    #[test]
    fn tool_call_detail_task_create_shows_title() {
        let detail = tool_call_detail(
            "task",
            &json!({"action": "create", "title": "Fix renderer drift"}),
        );
        assert_eq!(detail.as_deref(), Some("Fix renderer drift"));
    }

    #[test]
    fn tool_call_detail_task_update_shows_status() {
        let detail = tool_call_detail(
            "task",
            &json!({"action": "update", "task_id": "render-pass", "new_status": "in_progress"}),
        );
        assert_eq!(detail.as_deref(), Some("render-pass -> in_progress"));
    }

    #[test]
    fn tool_result_summary_agent_uses_shared_child_result_projection() {
        let launched = tool_result_summary(
            "agent",
            r#"{"status":"launched","agent_id":"reviewer@abc"}"#,
        );
        assert_eq!(
            launched.as_deref(),
            Some("Agent launched; waiting for get_result output.")
        );

        let interrupted = tool_result_summary(
            "agent",
            r#"{"status":"interrupted","agent_id":"reviewer@abc","result":"partial draft","finish_reason":"budget_exhausted"}"#,
        );
        assert_eq!(interrupted.as_deref(), Some("partial draft"));
    }

    #[test]
    fn tool_call_detail_reflect_shows_question() {
        let detail = tool_call_detail("reflect", &json!({"question": "why did the tool fail?"}));
        assert_eq!(detail.as_deref(), Some("why did the tool fail?"));
    }

    #[test]
    fn tool_call_detail_context_analysis_shows_compare_turns() {
        let detail = tool_call_detail(
            "context_analysis",
            &json!({"mode": "compare", "turn_a": 3, "turn_b": 7}),
        );
        assert_eq!(detail.as_deref(), Some("compare 3 vs 7"));
    }

    #[test]
    fn tool_call_detail_run_chain_shows_name() {
        let detail = tool_call_detail("run_chain", &json!({"name": "search-and-read"}));
        assert_eq!(detail.as_deref(), Some("search-and-read"));
    }

    #[test]
    fn tool_call_detail_rollback_file_edits_shows_file_scope() {
        let detail = tool_call_detail(
            "rollback_file_edits",
            &json!({"scope": "file", "path": "src/main.rs"}),
        );
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_rollback_database_snapshots_shows_snapshot_scope() {
        let detail = tool_call_detail(
            "rollback_database_snapshots",
            &json!({"scope": "snapshot", "snapshot_id": "snap_123"}),
        );
        assert_eq!(detail.as_deref(), Some("snap_123"));
    }

    #[test]
    fn tool_call_detail_adjust_config_shows_path() {
        let detail = tool_call_detail(
            "adjust_config",
            &json!({"path": "display.max_output_lines"}),
        );
        assert_eq!(detail.as_deref(), Some("display.max_output_lines"));
    }

    #[test]
    fn tool_call_detail_rollback_session_state_shows_turn_scope() {
        let detail = tool_call_detail(
            "rollback_session_state",
            &json!({"scope": "turn", "turn_index": 5}),
        );
        assert_eq!(detail.as_deref(), Some("turn 5"));
    }

    #[test]
    fn tool_call_detail_symbol_search_shows_query() {
        let detail = tool_call_detail("symbol_search", &json!({"query": "SessionFacts"}));
        assert_eq!(detail.as_deref(), Some("SessionFacts"));
    }

    #[test]
    fn tool_call_detail_hover_info_shows_location() {
        let detail = tool_call_detail(
            "hover_info",
            &json!({"file": "src/lib.rs", "line": 42, "column": 3}),
        );
        assert_eq!(detail.as_deref(), Some("src/lib.rs:42:3"));
    }

    #[test]
    fn tool_call_detail_type_hierarchy_shows_direction() {
        let detail = tool_call_detail(
            "type_hierarchy",
            &json!({"name": "SessionStore", "direction": "implementations"}),
        );
        assert_eq!(detail.as_deref(), Some("SessionStore (implementations)"));
    }

    #[test]
    fn tool_call_detail_rename_symbol_shows_transition() {
        let detail = tool_call_detail(
            "rename_symbol",
            &json!({"symbol": "SessionStore", "new_name": "StoreSession"}),
        );
        assert_eq!(detail.as_deref(), Some("SessionStore -> StoreSession"));
    }

    #[test]
    fn tool_call_detail_extract_members_shows_location() {
        let detail = tool_call_detail(
            "extract_members",
            &json!({"file": "src/lib.rs", "line": 88}),
        );
        assert_eq!(detail.as_deref(), Some("src/lib.rs:88"));
    }

    #[test]
    fn tool_call_detail_git_action_checkout_file_shows_ref_and_path() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "checkout_file", "path": "src/lib.rs", "ref": "HEAD~1"}),
        );
        assert_eq!(detail.as_deref(), Some("HEAD~1 -- src/lib.rs"));
    }

    #[test]
    fn tool_call_detail_git_action_worktree_shows_sub_action_and_branch() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "worktree", "sub_action": "add", "branch": "feature/ui"}),
        );
        assert_eq!(detail.as_deref(), Some("add feature/ui"));
    }

    #[test]
    fn tool_call_detail_memory_recall_shows_query() {
        let detail = tool_call_detail(
            "memory",
            &json!({"action": "recall", "query": "memoria repo"}),
        );
        assert_eq!(detail.as_deref(), Some("memoria repo"));
    }

    #[test]
    fn tool_call_detail_memory_forget_shows_target() {
        let detail = tool_call_detail(
            "memory",
            &json!({"action": "forget", "topic": "renderer drift"}),
        );
        assert_eq!(detail.as_deref(), Some("renderer drift"));
    }

    #[test]
    fn tool_call_detail_memory_update_shows_id() {
        let detail = tool_call_detail(
            "memory",
            &json!({"action": "update", "memory_id": "mem-123"}),
        );
        assert_eq!(detail.as_deref(), Some("mem-123"));
    }

    #[test]
    fn tool_call_detail_memory_focus_shows_value() {
        let detail = tool_call_detail(
            "memory",
            &json!({"action": "focus", "focus_value": "oauth"}),
        );
        assert_eq!(detail.as_deref(), Some("oauth"));
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
    fn tool_call_detail_str_replace_uses_per_edit_path() {
        let detail = tool_call_detail(
            "str_replace",
            &json!({
                "edits": [
                    {"path": "src/a.rs", "old_str": "a", "new_str": "b"},
                    {"path": "src/b.rs", "old_str": "c", "new_str": "d"}
                ]
            }),
        );
        assert_eq!(detail.as_deref(), Some("src/a.rs (+1 files)"));
    }

    #[test]
    fn tool_call_detail_git_action_diff_staged() {
        let detail = tool_call_detail("git", &json!({"action": "diff", "staged": true}));
        assert_eq!(detail.as_deref(), Some("working tree (staged)"));
    }

    #[test]
    fn tool_call_detail_git_action_diff_range() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "diff", "base_ref": "HEAD~5", "ref": "HEAD"}),
        );
        assert_eq!(detail.as_deref(), Some("HEAD~5..HEAD"));
    }

    #[test]
    fn tool_call_detail_git_action_diff_range_with_path() {
        let detail = tool_call_detail(
            "git",
            &json!({"action": "diff", "base_ref": "HEAD~3", "ref": "HEAD", "path": "src/main.rs"}),
        );
        assert_eq!(detail.as_deref(), Some("HEAD~3..HEAD -- src/main.rs"));
    }

    #[test]
    fn tool_call_detail_git_action_log_with_count() {
        let detail = tool_call_detail("git", &json!({"action": "log", "max_count": 5}));
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
    fn tool_error_summary_prefers_structured_what() {
        let output = "❌ STR_REPLACE FAILED — FILE NOT MODIFIED\n\nWHAT: old_str not found in file.\nWHY:  The exact byte sequence does not appear.\nNEXT: Re-read the target region.";
        let summary = tool_error_summary("str_replace", output);
        assert_eq!(summary, "old_str not found in file.");
    }

    #[test]
    fn tool_error_summary_uses_json_error() {
        let output = r#"{"success":false,"error":"missing 'path' for scope=file"}"#;
        let summary = tool_error_summary("rollback_file_edits", output);
        assert_eq!(summary, "missing 'path' for scope=file");
    }

    #[test]
    fn tool_error_summary_skips_banner_for_informative_line() {
        let output = "banner\n\nError: Missing 'path' parameter";
        let summary = tool_error_summary("str_replace", output);
        assert_eq!(summary, "Error: Missing 'path' parameter");
    }

    #[test]
    fn tool_error_summary_strips_tool_binding_sentinel() {
        let output = format!(
            "Error: tool `agent` runtime binding is unavailable. {}",
            astra_core::error_kind::TOOL_BINDING_SENTINEL
        );
        let summary = tool_error_summary("agent", &output);
        assert!(!summary.contains(astra_core::error_kind::TOOL_BINDING_SENTINEL));
        assert!(summary.contains("runtime binding is unavailable"));
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
    fn result_summary_git_empty() {
        let summary = tool_result_summary("git", "");
        assert_eq!(summary.as_deref(), Some("clean/no changes"));
    }

    #[test]
    fn result_summary_git_counts_diff_changes() {
        let summary = tool_result_summary("git", "--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+new");
        assert_eq!(summary.as_deref(), Some("+1 -1"));
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
