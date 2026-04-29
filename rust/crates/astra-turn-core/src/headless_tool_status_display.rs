//! One-line tool argument previews and post-execution summaries for headless stderr (CLI styles strings).

use serde_json::{Map, Value};

use astra_text_utils::str_preview::{github_repo_display, shorten_path, truncate_str};

use crate::tool_categories::{ToolDisplayCategory, registry};

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

    match name {
        "github_create_issue" => {
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
        "github_get_pr" | "github_get_issue" => match (repo_display, number) {
            (Some(repo), Some(number)) => Some(format!("{repo}#{number}")),
            (Some(repo), None) => Some(repo),
            (None, Some(_)) => None,
            (None, None) => obj
                .get("query")
                .and_then(|v| v.as_str())
                .map(|q| truncate_str(q, 60)),
        },
        "github_list_prs" | "github_list_issues" | "github_repo_stats" | "github_ci_status" => {
            match repo_display {
                Some(repo) => Some(repo),
                None => obj
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(|q| truncate_str(q, 60)),
            }
        }
        _ => match repo_display {
            Some(repo) => Some(repo),
            None => obj
                .get("query")
                .and_then(|v| v.as_str())
                .map(|q| truncate_str(q, 60)),
        },
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
        "write_file" | "create_file" | "edit_file" | "multi_edit" | "delete_file" => obj
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
        "git_file_history" => obj
            .get("file")
            .and_then(|v| v.as_str())
            .map(|path| shorten_path(path, 60)),
        "git_contributors" => {
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
        "git_commit" => obj
            .get("message")
            .and_then(|v| v.as_str())
            .map(|message| truncate_str(message, 60)),
        "git_revert_commit" => obj
            .get("commit_sha")
            .and_then(|v| v.as_str())
            .map(|sha| truncate_str(sha, 16)),
        "git_stash" => {
            let action = obj.get("action").and_then(|v| v.as_str());
            let stash_ref = obj.get("stash_ref").and_then(|v| v.as_str());
            let index = obj.get("index").and_then(|v| v.as_i64());
            match (action, stash_ref, index) {
                (Some(action), Some(stash_ref), _) => {
                    Some(format!("{action} {}", truncate_str(stash_ref, 32)))
                }
                (Some(action), None, Some(index)) => Some(format!("{action} stash@{{{index}}}")),
                (Some(action), None, None) => Some(action.to_string()),
                _ => None,
            }
        }
        "git_checkout_file" => {
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
        "git_worktree" => {
            let action = obj.get("action").and_then(|v| v.as_str());
            let branch = obj.get("branch").and_then(|v| v.as_str());
            let path = obj.get("path").and_then(|v| v.as_str());
            match (action, branch, path) {
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
        "mo_snapshot" => {
            let action = obj.get("action").and_then(|v| v.as_str());
            let name = obj.get("name").and_then(|v| v.as_str());
            let database = obj.get("database").and_then(|v| v.as_str());
            match (action, name, database) {
                (Some(action), Some(name), Some(database)) => Some(format!(
                    "{} {} @ {}",
                    truncate_str(action, 16),
                    truncate_str(name, 24),
                    truncate_str(database, 20)
                )),
                (Some(action), Some(name), None) => Some(format!(
                    "{} {}",
                    truncate_str(action, 16),
                    truncate_str(name, 30)
                )),
                (Some(action), None, Some(database)) => Some(format!(
                    "{} @ {}",
                    truncate_str(action, 16),
                    truncate_str(database, 24)
                )),
                (Some(action), None, None) => Some(action.to_string()),
                _ => None,
            }
        }
        "mo_branch" => {
            let action = obj.get("action").and_then(|v| v.as_str());
            let name = obj.get("name").and_then(|v| v.as_str());
            match (action, name) {
                (Some(action), Some(name)) => Some(format!(
                    "{} {}",
                    truncate_str(action, 16),
                    truncate_str(name, 30)
                )),
                (Some(action), None) => Some(action.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn fmt_memory_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "memory_retrieve" | "memory_search" => obj
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| truncate_str(q, 50)),
        "memory_store" => obj
            .get("content")
            .and_then(|v| v.as_str())
            .map(|content| truncate_str(content, 50)),
        "memory_purge" => obj
            .get("topic")
            .and_then(|v| v.as_str())
            .map(|topic| truncate_str(topic, 40)),
        "memory_correct" => obj
            .get("memory_id")
            .and_then(|v| v.as_str())
            .map(|memory_id| truncate_str(memory_id, 40)),
        "memory_profile" => None,
        _ => None,
    }
}

fn fmt_utility_tool(name: &str, obj: &Map<String, Value>) -> Option<String> {
    match name {
        "ask_user" => obj
            .get("question")
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
        "spawn_agent" => {
            let description = obj.get("description").and_then(|v| v.as_str());
            let agent_type = obj.get("agent_type").and_then(|v| v.as_str());
            match (description, agent_type) {
                (Some(description), Some(agent_type)) => Some(format!(
                    "{} ({})",
                    truncate_str(description, 32),
                    truncate_str(agent_type, 12)
                )),
                (Some(description), None) => Some(truncate_str(description, 50)),
                (None, Some(agent_type)) => Some(truncate_str(agent_type, 24)),
                _ => None,
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
        "task_create" => obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(|title| truncate_str(title, 60)),
        "task_list" => obj
            .get("status")
            .and_then(|v| v.as_str())
            .map(|status| truncate_str(status, 30)),
        "task_get" | "task_stop" => obj
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|task_id| truncate_str(task_id, 50)),
        "task_update" => {
            let task_id = obj.get("task_id").and_then(|v| v.as_str());
            let status = obj.get("status").and_then(|v| v.as_str());
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
        "rollback_turn_actions" => {
            let scope = obj.get("scope").and_then(|v| v.as_str());
            let turn_index = obj.get("turn_index").and_then(|v| v.as_i64());
            match (scope, turn_index) {
                (Some("turn"), Some(turn_index)) => Some(format!("turn {turn_index}")),
                (Some(scope), _) => Some(scope.to_string()),
                _ => None,
            }
        }
        "adjust_config" => obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|path| truncate_str(path, 40)),
        "prioritize_tool" | "deprioritize_tool" => obj
            .get("tool")
            .and_then(|v| v.as_str())
            .map(|tool| truncate_str(tool, 30)),
        "set_goal" => obj
            .get("goal")
            .and_then(|v| v.as_str())
            .map(|goal| truncate_str(goal, 50)),
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
    fn tool_call_detail_github_repo_arg_shows_repo_and_number() {
        let detail = tool_call_detail(
            "github_get_issue",
            &json!({"repo": "matrixorigin/astra", "issue_number": 147}),
        );
        assert_eq!(detail.as_deref(), Some("matrixorigin/astra#147"));
    }

    #[test]
    fn tool_call_detail_github_create_issue_shows_repo_and_title() {
        let detail = tool_call_detail(
            "github_create_issue",
            &json!({"repo": "matrixorigin/astra", "title": "Fix renderer drift"}),
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
    fn tool_call_detail_git_commit_shows_message() {
        let detail = tool_call_detail("git_commit", &json!({"message": "ship the fix"}));
        assert_eq!(detail.as_deref(), Some("ship the fix"));
    }

    #[test]
    fn tool_call_detail_git_stash_shows_action() {
        let detail = tool_call_detail("git_stash", &json!({"action": "push"}));
        assert_eq!(detail.as_deref(), Some("push"));
    }

    #[test]
    fn tool_call_detail_git_file_history_shows_file() {
        let detail = tool_call_detail("git_file_history", &json!({"file": "src/main.rs"}));
        assert_eq!(detail.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_detail_git_checkout_file_shortens_long_path() {
        let detail = tool_call_detail(
            "git_checkout_file",
            &json!({
                "path": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                "ref": "HEAD~1"
            }),
        )
        .expect("detail");
        assert!(detail.starts_with("HEAD~1 -- .../"));
        assert!(detail.ends_with("src/lib.rs"));
    }

    #[test]
    fn tool_call_detail_git_contributors_shows_filters() {
        let detail = tool_call_detail(
            "git_contributors",
            &json!({"path": "src/", "since": "30 days ago"}),
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
        let detail = tool_call_detail("ask_user", &json!({"question": "Continue?"}));
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
    fn tool_call_detail_spawn_agent_shows_description_and_type() {
        let detail = tool_call_detail(
            "spawn_agent",
            &json!({"description": "review auth", "agent_type": "code-review"}),
        );
        assert_eq!(detail.as_deref(), Some("review auth (code-review)"));
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
        let detail = tool_call_detail("task_create", &json!({"title": "Fix renderer drift"}));
        assert_eq!(detail.as_deref(), Some("Fix renderer drift"));
    }

    #[test]
    fn tool_call_detail_task_update_shows_status() {
        let detail = tool_call_detail(
            "task_update",
            &json!({"task_id": "render-pass", "status": "in_progress"}),
        );
        assert_eq!(detail.as_deref(), Some("render-pass -> in_progress"));
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
    fn tool_call_detail_rollback_turn_actions_shows_turn_scope() {
        let detail = tool_call_detail(
            "rollback_turn_actions",
            &json!({"scope": "turn", "turn_index": 7}),
        );
        assert_eq!(detail.as_deref(), Some("turn 7"));
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
    fn tool_call_detail_mo_snapshot_shows_action_and_name() {
        let detail = tool_call_detail(
            "mo_snapshot",
            &json!({"action": "create", "name": "pre-migration"}),
        );
        assert_eq!(detail.as_deref(), Some("create pre-migration"));
    }

    #[test]
    fn tool_call_detail_mo_branch_shows_action_and_name() {
        let detail = tool_call_detail("mo_branch", &json!({"action": "create", "name": "exp-a"}));
        assert_eq!(detail.as_deref(), Some("create exp-a"));
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
    fn tool_call_detail_git_checkout_file_shows_ref_and_path() {
        let detail = tool_call_detail(
            "git_checkout_file",
            &json!({"path": "src/lib.rs", "ref": "HEAD~1"}),
        );
        assert_eq!(detail.as_deref(), Some("HEAD~1 -- src/lib.rs"));
    }

    #[test]
    fn tool_call_detail_git_worktree_shows_action_and_branch() {
        let detail = tool_call_detail(
            "git_worktree",
            &json!({"action": "add", "branch": "feature/ui"}),
        );
        assert_eq!(detail.as_deref(), Some("add feature/ui"));
    }

    #[test]
    fn tool_call_detail_memory_shows_query() {
        let detail = tool_call_detail("memory_search", &json!({"query": "memoria repo"}));
        assert_eq!(detail.as_deref(), Some("memoria repo"));
    }

    #[test]
    fn tool_call_detail_memory_purge_shows_topic() {
        let detail = tool_call_detail("memory_purge", &json!({"topic": "renderer drift"}));
        assert_eq!(detail.as_deref(), Some("renderer drift"));
    }

    #[test]
    fn tool_call_detail_memory_correct_shows_id() {
        let detail = tool_call_detail("memory_correct", &json!({"memory_id": "mem-123"}));
        assert_eq!(detail.as_deref(), Some("mem-123"));
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
