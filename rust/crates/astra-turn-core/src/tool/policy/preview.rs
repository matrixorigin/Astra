//! Unified tool-call preview renderer.
//!
//! Single source of truth for the one-line human-readable description
//! of an in-flight or pending tool call. Used by:
//!
//! - TUI `ToolCell` / `stream_render` scrollback display
//! - CLI / TUI permission approval prompts (header + detail lines)
//! - Cloud `approval_required` payloads
//!
//! Before this module existed, the CLI display and the permission
//! prompt each had their own ad-hoc formatter. They drifted: the
//! approval prompt showed raw `command` / `path` strings while the
//! scrollback showed nicely-formatted `$ ls -la /tmp` / `Reading:
//! foo.rs:10-50`. Consolidating here eliminates the drift and makes
//! a future `--verbose` flag a one-change affair.
//!
//! ## Scope
//!
//! Only covers the **active** tool set shipped today (the names
//! advertised in `astra-tools::schemas`). Legacy separate names
//! (`task_create`, `git_show`, `memory_retrieve`, `hover_info`, …)
//! have been retired — the model now issues unified action-param
//! calls (`task(action="create")`, `git(action="show")`,
//! `memory(action="retrieve")`, `lsp(operation="hover")`) and we
//! don't maintain preview code for dead paths.
//!
//! ## Design
//!
//! - No I/O, no clock: pure function of `(tool_name, args, verbose,
//!   width_budget)` → `String`.
//! - Output is intentionally a single line; multi-line structure is
//!   the caller's job.
//! - Width budget is dynamic: callers pass the total chars available
//!   for the preview. Long prefixes (e.g. `"Searching memory: "`)
//!   subtract prefix length before truncation.

use serde_json::Value;

use astra_text_utils::str_preview::{github_repo_display, shorten_path, truncate_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewStyle {
    Concise,
    Verbose,
}

/// Render the canonical preview line for a tool call.
///
/// `desc_budget` is the total chars the caller has reserved for the
/// preview string. Passing 80 is safe for typical terminal widths;
/// the ToolCell renderer passes `term_width - 14` (room for prefix +
/// duration suffix). If your caller has no width constraint (cloud
/// JSON payload), pass a large value like `usize::MAX`.
pub fn render_preview(tool: &str, args: &Value, style: PreviewStyle, desc_budget: usize) -> String {
    let path_budget = |prefix_len: usize| desc_budget.saturating_sub(prefix_len).max(20);
    let verbose = style == PreviewStyle::Verbose;
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, b)
        }
    };
    let short = |p: &str, b: usize| -> String {
        if verbose {
            p.to_string()
        } else {
            shorten_path(p, b)
        }
    };

    match tool {
        // ── Shell ───────────────────────────────────────────────────
        "bash" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("$ {}", trunc(cmd, path_budget(2)))
        }
        "powershell" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("PS> {}", trunc(cmd, path_budget(4)))
        }

        // ── File I/O ────────────────────────────────────────────────
        "read_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            let start = args.get("start_line").and_then(Value::as_u64);
            let end = args.get("end_line").and_then(Value::as_u64);
            let short_path = short(path, path_budget(10));
            match (start, end) {
                (Some(s), Some(e)) => format!("Reading: {short_path}:{s}-{e}"),
                (Some(s), None) => format!("Reading: {short_path}:{s}-"),
                _ => format!("Reading: {short_path}"),
            }
        }
        "write_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            if args.get("delete").and_then(Value::as_bool).unwrap_or(false) {
                format!("Deleting: {}", short(path, path_budget(10)))
            } else {
                format!("Writing: {}", short(path, path_budget(9)))
            }
        }
        "str_replace" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            format!("Editing: {}", short(path, path_budget(9)))
        }
        "list_dir" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            format!("Listing: {}", short(path, path_budget(9)))
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
            let glob_filter = args.get("glob").and_then(Value::as_str);
            let path = args.get("path").and_then(Value::as_str);
            let pat_budget = desc_budget / 3;
            let short_pattern = trunc(pattern, pat_budget);
            match (glob_filter, path) {
                (Some(g), _) => format!("Grep: \"{short_pattern}\" in {g}"),
                (None, Some(p)) => {
                    let p_budget = desc_budget.saturating_sub(10 + pat_budget);
                    format!("Grep: \"{short_pattern}\" in {}", short(p, p_budget))
                }
                _ => format!("Grep: \"{short_pattern}\""),
            }
        }
        "glob" => {
            let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
            format!("Glob: {}", trunc(pattern, path_budget(6)))
        }

        // ── Code navigation ─────────────────────────────────────────
        "symbols" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            format!("Symbols in {}", short(path, path_budget(12)))
        }
        "lsp" => lsp_preview(args, path_budget, verbose),

        // ── Unified action-param tools ──────────────────────────────
        "git" => git_preview(args, path_budget, verbose),
        "github" => github_preview(args, path_budget, verbose),
        "memory" => memory_preview(args, path_budget, verbose),
        "session" => session_preview(args, path_budget, verbose),
        "mo" => mo_preview(args, path_budget, verbose),
        "agent" => agent_preview(args, path_budget, verbose),
        "skill" => skill_preview(args, path_budget, verbose),
        "task" => task_preview(args, path_budget, verbose),
        "task_output" => background_task_output_preview(args, path_budget, verbose),
        "task_stop" => background_task_stop_preview(args, path_budget, verbose),
        "task_list" => "List background tasks".to_string(),

        // ── Web ─────────────────────────────────────────────────────
        "web_fetch" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            format!("Fetching: {}", trunc(url, path_budget(10)))
        }
        "web_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            format!("Searching web: \"{}\"", trunc(query, path_budget(17)))
        }

        // ── Scripting ───────────────────────────────────────────────
        "run_script" => {
            // `script` is Python source — show the first line (typical
            // scripts start with a comment or import that hints at
            // intent). Full text is long and would overwhelm the cell.
            let script = args.get("script").and_then(Value::as_str).unwrap_or("");
            let first_line = script.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                "Running script".to_string()
            } else {
                format!("Running script: {}", trunc(first_line, path_budget(17)))
            }
        }

        // ── Misc ────────────────────────────────────────────────────
        "introspect" => "Introspecting…".to_string(),
        "notify" => {
            let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
            format!("Notify: \"{}\"", trunc(msg, path_budget(10)))
        }
        "ask_user" => {
            let question = args
                .get("questions")
                .and_then(Value::as_array)
                .and_then(|questions| questions.first())
                .and_then(|question| question.get("question"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("Asking user: \"{}\"", trunc(question, path_budget(15)))
        }

        // ── MCP passthrough ─────────────────────────────────────────
        other if other.starts_with("mcp_") => mcp_preview(other, path_budget, verbose),

        _ => tool.to_string(),
    }
}

fn git_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, b)
        }
    };
    let short = |p: &str, b: usize| -> String {
        if verbose {
            p.to_string()
        } else {
            shorten_path(p, b)
        }
    };
    match action {
        "status" => "Git status".to_string(),
        "log" => {
            // Schema canonical key is `ref`; accept `branch` as legacy
            // alias since some older replays still emit that.
            let n = args.get("n").and_then(Value::as_u64);
            let git_ref = args
                .get("ref")
                .or_else(|| args.get("branch"))
                .and_then(Value::as_str);
            match (n, git_ref) {
                (Some(n), Some(r)) => format!("Git log -{n} {r}"),
                (Some(n), None) => format!("Git log -{n}"),
                (None, Some(r)) => format!("Git log {r}"),
                _ => "Git log".to_string(),
            }
        }
        "show" => {
            // Schema canonical: `revision` (runtime `git_ops::show` reads
            // this). Accept legacy `commit`/`ref` for resilience.
            let rev = args
                .get("revision")
                .or_else(|| args.get("commit"))
                .or_else(|| args.get("ref"))
                .and_then(Value::as_str)
                .unwrap_or("HEAD");
            format!("Git show {}", trunc(rev, path_budget(9)))
        }
        "diff" => {
            let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
            let path = args.get("path").and_then(Value::as_str);
            match (staged, path) {
                (true, Some(p)) => format!("Git diff --staged {}", short(p, path_budget(18))),
                (true, None) => "Git diff --staged".to_string(),
                (false, Some(p)) => format!("Git diff {}", short(p, path_budget(10))),
                _ => "Git diff".to_string(),
            }
        }
        "blame" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            format!("Git blame {}", short(path, path_budget(10)))
        }
        "file_history" => {
            let file = args.get("file").and_then(Value::as_str).unwrap_or("");
            format!("Git history {}", short(file, path_budget(12)))
        }
        "log_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            format!("Git log search \"{}\"", trunc(query, path_budget(17)))
        }
        "contributors" => match args.get("path").and_then(Value::as_str) {
            Some(p) => format!("Git contributors {}", short(p, path_budget(17))),
            None => "Git contributors".to_string(),
        },
        "commit" => {
            let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
            format!("Git commit \"{}\"", trunc(msg, path_budget(13)))
        }
        "revert_commit" => {
            let sha = args.get("commit_sha").and_then(Value::as_str).unwrap_or("");
            format!("Git revert {}", trunc(sha, path_budget(11)))
        }
        "stash" => {
            // Schema canonical: `sub_action`. Accept `stash_action` alias.
            let sub = args
                .get("sub_action")
                .or_else(|| args.get("stash_action"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if sub.is_empty() {
                "Git stash".to_string()
            } else {
                format!("Git stash {sub}")
            }
        }
        "checkout_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            let git_ref = args.get("ref").and_then(Value::as_str);
            match git_ref {
                Some(r) => format!(
                    "Git checkout {} -- {}",
                    trunc(r, 16),
                    short(path, path_budget(20)),
                ),
                None => format!("Git checkout {}", short(path, path_budget(13))),
            }
        }
        "worktree" => {
            let sub = args.get("sub_action").and_then(Value::as_str).unwrap_or("");
            let path = args.get("path").and_then(Value::as_str);
            match (sub, path) {
                ("", _) => "Git worktree".to_string(),
                (s, Some(p)) => format!("Git worktree {s} {}", short(p, path_budget(14 + s.len()))),
                (s, None) => format!("Git worktree {s}"),
            }
        }
        _ => format!("Git {action}"),
    }
}

fn github_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let owner = args.get("owner").and_then(Value::as_str);
    let repo = args.get("repo").and_then(Value::as_str);
    let repo_display = github_repo_display(owner, repo).unwrap_or_default();
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, b)
        }
    };
    // Schema canonical: `number` (single field for both PR and issue
    // numbers). Runtime `github.rs` still reads `pr_number` /
    // `issue_number` — accept both so the preview works regardless
    // of which shape the model sends.
    let numeric = |primary: &str| -> Option<u64> {
        args.get("number")
            .or_else(|| args.get(primary))
            .and_then(Value::as_u64)
    };
    match action {
        "list_prs" => format!("GitHub: list PRs {repo_display}"),
        "get_pr" => match numeric("pr_number") {
            Some(n) => format!("GitHub: PR #{n} {repo_display}"),
            None => format!("GitHub: get PR {repo_display}"),
        },
        "ci_status" => format!("GitHub: CI status {repo_display}"),
        "list_issues" => format!("GitHub: list issues {repo_display}"),
        "get_issue" => match numeric("issue_number") {
            Some(n) => format!("GitHub: issue #{n} {repo_display}"),
            None => format!("GitHub: get issue {repo_display}"),
        },
        "repo_stats" => format!("GitHub: stats {repo_display}"),
        "create_issue" => {
            let title = args.get("title").and_then(Value::as_str).unwrap_or("");
            format!("GitHub: create issue \"{}\"", trunc(title, path_budget(22)))
        }
        _ => format!("GitHub: {action}"),
    }
}

fn memory_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, b)
        }
    };
    match action {
        "retrieve" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            format!("Recalling: \"{}\"", trunc(query, path_budget(13)))
        }
        "store" => {
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            format!("Storing: \"{}\"", trunc(content, path_budget(11)))
        }
        "search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            format!("Searching memory: \"{}\"", trunc(query, path_budget(20)))
        }
        "purge" => {
            let topic = args.get("topic").and_then(Value::as_str);
            match topic {
                Some(t) => format!("Purging memory: \"{}\"", trunc(t, path_budget(17))),
                None => "Purging memory".to_string(),
            }
        }
        "correct" => {
            let id = args.get("memory_id").and_then(Value::as_str);
            match id {
                Some(m) => format!("Correcting memory: {}", trunc(m, path_budget(20))),
                None => "Correcting memory".to_string(),
            }
        }
        "profile" => "Checking profile".to_string(),
        "feedback" => "Memory feedback".to_string(),
        _ => format!("Memory: {action}"),
    }
}

fn session_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, path_budget(b))
        }
    };
    match action {
        "config" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            format!("Adjust config: {}", trunc(path, 15))
        }
        "prioritize" => {
            let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
            format!("Prioritize: {}", trunc(tool, 12))
        }
        "deprioritize" => {
            let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
            format!("Deprioritize: {}", trunc(tool, 14))
        }
        "set_goal" => {
            let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
            format!("Set goal: \"{}\"", trunc(goal, 12))
        }
        "compact" => "Compress context".to_string(),
        "timeline" => "Session timeline".to_string(),
        "summary" => "Session summary".to_string(),
        "history" => "Session history".to_string(),
        "rollback_edits" => match args.get("scope").and_then(Value::as_str) {
            Some(s) => format!("Revert file edits: {}", trunc(s, 19)),
            None => "Revert file edits".to_string(),
        },
        "ask_user" => {
            let q = args.get("question").and_then(Value::as_str).unwrap_or("");
            format!("Asking user: \"{}\"", trunc(q, 15))
        }
        "sleep" => {
            let duration_ms = args.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
            let reason = args.get("reason").and_then(Value::as_str);
            match reason {
                Some(r) if !r.is_empty() => format!("Sleeping: {duration_ms}ms ({r})"),
                _ => format!("Sleeping: {duration_ms}ms"),
            }
        }
        "tool_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            format!("Searching tools: \"{}\"", trunc(query, 18))
        }
        _ => format!("Session: {action}"),
    }
}

fn mo_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, path_budget(b))
        }
    };
    match action {
        "query" => {
            let sql = args.get("sql").and_then(Value::as_str).unwrap_or("");
            format!("MO query: \"{}\"", trunc(sql, 11))
        }
        "snapshot" => {
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            format!("MO snapshot: {}", trunc(name, 13))
        }
        "branch" => {
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            format!("MO branch: {}", trunc(name, 11))
        }
        _ => format!("MO: {action}"),
    }
}

fn agent_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, path_budget(b))
        }
    };
    match action {
        "delegate" => {
            let task = args.get("task").and_then(Value::as_str).unwrap_or("");
            format!("Delegating: \"{}\"", trunc(task, 14))
        }
        "run_chain" => {
            let chain = args.get("chain_name").and_then(Value::as_str).unwrap_or("");
            format!("Running chain: {}", trunc(chain, 15))
        }
        "spawn" => {
            // Prefer the short `name` over the long `description`.
            // Names like "review_tui" / "review_fixes" are stable
            // display handles meant for the multi_agent strip and the
            // InFlightAgentsView rows; descriptions are verbose
            // human-readable summaries that visually clutter when N
            // parallel agents all share a similar prefix
            // ("Correctness & logic review of the latest 7 commits…").
            // Matches claudecode/Kiro: per-agent rows show the name,
            // not the full description.
            let name = args.get("name").and_then(Value::as_str);
            let label = name.or_else(|| args.get("description").and_then(Value::as_str));
            let agent_type = args.get("agent_type").and_then(Value::as_str);
            match (label, agent_type) {
                (Some(l), Some(at)) => format!("Spawn agent: {} ({})", trunc(l, 13), trunc(at, 8),),
                (Some(l), None) => format!("Spawn agent: {}", trunc(l, 13)),
                (None, Some(at)) => format!("Spawn agent: {}", trunc(at, 13)),
                _ => "Spawn agent".to_string(),
            }
        }
        "get_result" => {
            let agent_id = args.get("agent_id").and_then(Value::as_str).unwrap_or("");
            format!("Get agent result: {}", trunc(agent_id, 19))
        }
        "send_message" => {
            let to = args.get("to").and_then(Value::as_str).unwrap_or("");
            let summary = args.get("summary").and_then(Value::as_str);
            let message = args.get("message").and_then(Value::as_str);
            match (summary, message) {
                (Some(s), _) => format!("Send message: {}: {}", trunc(to, 12), trunc(s, 16),),
                (None, Some(m)) => format!("Send message: {}: {}", trunc(to, 12), trunc(m, 16),),
                (None, None) => format!("Send message: {}", trunc(to, 14)),
            }
        }
        _ => format!("Agent: {action}"),
    }
}

fn skill_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("run");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, path_budget(b))
        }
    };
    match action {
        "discover" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("skills");
            format!("Discovering skills: \"{}\"", trunc(query, 22))
        }
        _ => {
            let skill_name = args
                .get("skill_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("Running skill: {}", trunc(skill_name, 16))
        }
    }
}

fn task_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, path_budget(b))
        }
    };
    match action {
        "create" => {
            let title = args.get("title").and_then(Value::as_str).unwrap_or("");
            format!("Creating task: \"{}\"", trunc(title, 16))
        }
        "update" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let status = args.get("new_status").and_then(Value::as_str);
            let subtask = args.get("subtask_id").and_then(Value::as_str);
            match (subtask, status) {
                (Some(sub), Some(st)) => format!(
                    "Updating subtask {}/{} -> {}",
                    trunc(task_id, 10),
                    trunc(sub, 10),
                    trunc(st, 12),
                ),
                (None, Some(st)) => {
                    format!("Updating task: {} -> {}", trunc(task_id, 14), trunc(st, 14),)
                }
                _ => format!("Updating task: {}", trunc(task_id, 14)),
            }
        }
        "list" => {
            let status = args.get("status_filter").and_then(Value::as_str);
            match status {
                Some(s) => format!("Listing tasks: {}", trunc(s, 15)),
                None => "Listing tasks".to_string(),
            }
        }
        "list_user" => {
            let status = args
                .get("user_status")
                .and_then(Value::as_str)
                .unwrap_or("active");
            format!("Listing cross-session tasks: {}", trunc(status, 15))
        }
        "get" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            format!("Getting task: {}", trunc(task_id, 14))
        }
        "stop" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let reason = args.get("reason").and_then(Value::as_str);
            match reason {
                Some(r) => format!("Stopping task {}: {}", trunc(task_id, 10), trunc(r, 14),),
                None => format!("Stopping task: {}", trunc(task_id, 14)),
            }
        }
        _ => format!("Task: {action}"),
    }
}

fn background_task_id(args: &Value) -> Option<&str> {
    args.get("task_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn background_task_output_preview(
    args: &Value,
    path_budget: impl Fn(usize) -> usize,
    verbose: bool,
) -> String {
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, b)
        }
    };
    match background_task_id(args) {
        Some(id) => format!("Read background task output {}", trunc(id, path_budget(18))),
        None => "Read background task output".to_string(),
    }
}

fn background_task_stop_preview(
    args: &Value,
    path_budget: impl Fn(usize) -> usize,
    verbose: bool,
) -> String {
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, b)
        }
    };
    match background_task_id(args) {
        Some(id) => format!("Stop background task {}", trunc(id, path_budget(22))),
        None => "Stop background task".to_string(),
    }
}

/// LSP preview — the unified Language Server tool. The `operation`
/// enum covers what were once dozens of individual tool names
/// (find_references, hover, rename_symbol, …) so one preview branch
/// replaces them all.
fn lsp_preview(args: &Value, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let op = args.get("operation").and_then(Value::as_str).unwrap_or("");
    let file = args.get("file").and_then(Value::as_str).unwrap_or("");
    let short = |p: &str, b: usize| -> String {
        if verbose {
            p.to_string()
        } else {
            shorten_path(p, path_budget(b))
        }
    };
    let trunc = |s: &str, b: usize| -> String {
        if verbose {
            s.to_string()
        } else {
            truncate_line(s, path_budget(b))
        }
    };
    let line = args.get("line").and_then(Value::as_u64);
    let column = args.get("column").and_then(Value::as_u64);
    let position = match (line, column) {
        (Some(l), Some(c)) => Some(format!(":{l}:{c}")),
        (Some(l), None) => Some(format!(":{l}")),
        _ => None,
    };

    let label = match op {
        "goto_definition" | "declaration" => "Goto definition",
        "find_references" => "Find references",
        "hover" => "Hover",
        "document_symbols" => "Document symbols",
        "workspace_symbols" => {
            let q = args.get("query").and_then(Value::as_str).unwrap_or("");
            return format!("Workspace symbols: \"{}\"", trunc(q, 20));
        }
        "call_hierarchy" | "incoming_calls" | "outgoing_calls" => "Call hierarchy",
        "type_definition" => "Type definition",
        "implementation" => "Implementation",
        "supertypes" | "subtypes" => "Type hierarchy",
        "prepare_rename" | "rename" => {
            let new_name = args.get("new_name").and_then(Value::as_str);
            match (position.as_deref(), new_name) {
                (Some(pos), Some(n)) => {
                    return format!("Rename at {}{pos} -> {}", short(file, 18), trunc(n, 14));
                }
                (Some(pos), None) => return format!("Rename at {}{pos}", short(file, 12)),
                _ => return format!("Rename at {}", short(file, 12)),
            }
        }
        "code_actions" => "Code actions",
        "completions" => "Completions",
        "signature_help" => "Signature help",
        "document_highlight" => "Document highlight",
        "document_links" => "Document links",
        "inlay_hints" => "Inlay hints",
        "folding_ranges" => "Folding ranges",
        "document_colors" | "color_presentations" => "Document colors",
        "semantic_tokens" => "Semantic tokens",
        "code_lenses" => "Code lenses",
        "selection_ranges" => "Selection ranges",
        "linked_editing_range" => "Linked editing range",
        "format_document" | "format_range" | "format_on_type" => "Format",
        "diagnostics" => "Diagnostics",
        _ => {
            return format!("LSP {op} {}", short(file, path_budget(5 + op.len())));
        }
    };

    match position {
        Some(pos) => format!(
            "{label} at {}{pos}",
            short(file, path_budget(label.len() + 5))
        ),
        None if !file.is_empty() => {
            format!("{label} in {}", short(file, path_budget(label.len() + 5)))
        }
        _ => label.to_string(),
    }
}

fn mcp_preview(tool: &str, path_budget: impl Fn(usize) -> usize, verbose: bool) -> String {
    let rest = &tool[4..];
    if let Some(sep) = rest.find('_') {
        let server = &rest[..sep];
        let tool_name = &rest[sep + 1..];
        let t = if verbose {
            tool_name.to_string()
        } else {
            truncate_line(tool_name, path_budget(5 + server.len()))
        };
        format!("MCP {server} {t}")
    } else {
        format!("MCP {rest}")
    }
}

/// Compact summary of MCP tool arguments for permission prompts.
/// Unlike [`render_preview`]'s `mcp_` branch (which previews by tool
/// name), this summarises **up to three alphabetically-earliest**
/// argument key/value pairs. Used where the approval prompt wants to
/// show "what args will be sent to the MCP server" rather than
/// "which MCP tool will run".
///
/// Ordering note: serde_json's `Map` is backed by `BTreeMap` workspace-
/// wide (the `preserve_order` feature is deliberately NOT enabled —
/// `stall::canonical_tool_args` relies on alphabetic round-trip for
/// stall-detection de-duplication across re-ordered tool calls). This
/// means `mcp_args_summary({"query": "x", "limit": 10, "a": 1})`
/// shows `a=1, limit=10, query="x"` — alphabetic by key, not insertion
/// order. Acceptable for this cosmetic preview: the user sees all
/// three pairs, just not necessarily the ones the model considered
/// "most important" first. For a real MCP arg capture, the approval
/// dialog should use the full JSON.
pub fn mcp_args_summary(args: &Value) -> String {
    let obj = match args.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return "(no arguments)".into(),
    };
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj.iter().take(3) {
        let val_str = match v {
            Value::String(s) => {
                if s.chars().count() > 60 {
                    format!("\"{}\"", astra_text_utils::str_preview::truncate_str(s, 57))
                } else {
                    format!("\"{s}\"")
                }
            }
            other => {
                let s = other.to_string();
                if s.chars().count() > 60 {
                    astra_text_utils::str_preview::truncate_str(&s, 57)
                } else {
                    s
                }
            }
        };
        parts.push(format!("{k}={val_str}"));
    }
    if obj.len() > 3 {
        parts.push(format!("+{} more", obj.len() - 3));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(tool: &str, args: Value) -> String {
        render_preview(tool, &args, PreviewStyle::Concise, 80)
    }

    #[test]
    fn bash_shows_dollar_prefix() {
        assert_eq!(p("bash", json!({"command": "ls -la"})), "$ ls -la");
    }

    #[test]
    fn typed_background_task_tools_render_previews() {
        assert_eq!(
            p("task_output", json!({"task_id": "bg-shell-3"})),
            "Read background task output bg-shell-3"
        );
        assert_eq!(
            p("task_stop", json!({"task_id": "bg-shell-3"})),
            "Stop background task bg-shell-3"
        );
        assert_eq!(p("task_list", json!({})), "List background tasks");
    }

    #[test]
    fn powershell_shows_ps_prefix() {
        assert_eq!(
            p("powershell", json!({"command": "Get-Process"})),
            "PS> Get-Process"
        );
    }

    #[test]
    fn read_file_with_range() {
        assert_eq!(
            p(
                "read_file",
                json!({"path": "src/main.rs", "start_line": 10, "end_line": 50})
            ),
            "Reading: src/main.rs:10-50"
        );
    }

    #[test]
    fn read_file_without_range() {
        assert_eq!(
            p("read_file", json!({"path": "src/main.rs"})),
            "Reading: src/main.rs"
        );
    }

    #[test]
    fn write_file_vs_delete() {
        assert_eq!(
            p("write_file", json!({"path": "/tmp/x"})),
            "Writing: /tmp/x"
        );
        assert_eq!(
            p("write_file", json!({"path": "/tmp/x", "delete": true})),
            "Deleting: /tmp/x"
        );
    }

    #[test]
    fn grep_with_glob() {
        assert_eq!(
            p("grep", json!({"pattern": "fn main", "glob": "**/*.rs"})),
            r#"Grep: "fn main" in **/*.rs"#
        );
    }

    #[test]
    fn symbols_file_path() {
        assert_eq!(
            p("symbols", json!({"path": "src/lib.rs"})),
            "Symbols in src/lib.rs"
        );
    }

    #[test]
    fn lsp_hover_with_position() {
        assert_eq!(
            p(
                "lsp",
                json!({"operation": "hover", "file": "src/lib.rs", "line": 42, "column": 3})
            ),
            "Hover at src/lib.rs:42:3"
        );
    }

    #[test]
    fn lsp_rename_shows_target() {
        assert_eq!(
            p(
                "lsp",
                json!({
                    "operation": "rename",
                    "file": "src/lib.rs",
                    "line": 10,
                    "column": 5,
                    "new_name": "BetterName"
                })
            ),
            "Rename at src/lib.rs:10:5 -> BetterName"
        );
    }

    #[test]
    fn lsp_workspace_symbols_shows_query() {
        assert_eq!(
            p(
                "lsp",
                json!({"operation": "workspace_symbols", "query": "SessionStore"})
            ),
            r#"Workspace symbols: "SessionStore""#
        );
    }

    #[test]
    fn web_fetch_shows_url() {
        assert_eq!(
            p("web_fetch", json!({"url": "https://example.com/docs"})),
            "Fetching: https://example.com/docs"
        );
    }

    #[test]
    fn web_search_shows_query() {
        assert_eq!(
            p("web_search", json!({"query": "matrixone latest"})),
            r#"Searching web: "matrixone latest""#
        );
    }

    #[test]
    fn notify_shows_message() {
        assert_eq!(
            p("notify", json!({"message": "Task complete"})),
            r#"Notify: "Task complete""#
        );
    }

    #[test]
    fn ask_user_shows_question() {
        assert_eq!(
            p(
                "ask_user",
                json!({"questions": [{"header": "Confirm", "question": "Continue?", "options": ["Yes", "No"]}]})
            ),
            r#"Asking user: "Continue?""#
        );
    }

    #[test]
    fn git_unified_status() {
        assert_eq!(p("git", json!({"action": "status"})), "Git status");
    }

    #[test]
    fn git_unified_log_with_n() {
        assert_eq!(p("git", json!({"action": "log", "n": 5})), "Git log -5");
    }

    #[test]
    fn git_unified_show() {
        assert_eq!(
            p("git", json!({"action": "show", "commit": "abc123"})),
            "Git show abc123"
        );
    }

    #[test]
    fn github_get_pr() {
        assert_eq!(
            p(
                "github",
                json!({"action": "get_pr", "owner": "o", "repo": "r", "pr_number": 42})
            ),
            "GitHub: PR #42 o/r"
        );
    }

    #[test]
    fn memory_retrieve() {
        assert_eq!(
            p("memory", json!({"action": "retrieve", "query": "branches"})),
            r#"Recalling: "branches""#
        );
    }

    #[test]
    fn memory_purge_with_topic() {
        assert_eq!(
            p(
                "memory",
                json!({"action": "purge", "topic": "renderer drift"})
            ),
            r#"Purging memory: "renderer drift""#
        );
    }

    #[test]
    fn memory_correct_with_id() {
        assert_eq!(
            p(
                "memory",
                json!({"action": "correct", "memory_id": "mem-123"})
            ),
            "Correcting memory: mem-123"
        );
    }

    #[test]
    fn session_tool_search() {
        assert_eq!(
            p(
                "session",
                json!({"action": "tool_search", "query": "github"})
            ),
            r#"Searching tools: "github""#
        );
    }

    #[test]
    fn session_sleep_with_reason() {
        assert_eq!(
            p(
                "session",
                json!({"action": "sleep", "duration_ms": 1500, "reason": "waiting for CI"})
            ),
            "Sleeping: 1500ms (waiting for CI)"
        );
    }

    #[test]
    fn mo_query() {
        assert_eq!(
            p("mo", json!({"action": "query", "sql": "SELECT 1"})),
            r#"MO query: "SELECT 1""#
        );
    }

    #[test]
    fn agent_send_message_with_summary() {
        assert_eq!(
            p(
                "agent",
                json!({"action": "send_message", "to": "agent-2", "summary": "Need review"})
            ),
            "Send message: agent-2: Need review"
        );
    }

    /// REGRESSION: agent spawn preview should prefer the short `name`
    /// field over the long `description` field. The TUI's multi_agent
    /// strip and InFlightAgentsView render this preview as the per-
    /// agent display label; with description-only rendering, parallel
    /// agents all show "Spawn agent: Correctness & logic re…
    /// (code-revi…)" — visually indistinguishable. Names like
    /// "review_tui" / "review_fixes" make the strip readable
    /// (claudecode/Kiro both display names this way).
    #[test]
    fn agent_spawn_prefers_short_name_over_long_description() {
        assert_eq!(
            p(
                "agent",
                json!({
                    "action": "spawn",
                    "agent_type": "code-review",
                    "name": "review_tui",
                    "description": "Correctness & logic review of the latest 7 commits on improve_tui7",
                    "prompt": "ignored for preview"
                })
            ),
            "Spawn agent: review_tui (code-review)",
            "when `name` is set it must beat `description` — names are \
             stable display handles, descriptions are verbose prompts"
        );
    }

    /// Backwards-compat: when `name` is absent, fall back to
    /// `description` (the pre-fix behaviour). This keeps every
    /// historical session's render bytes identical.
    #[test]
    fn agent_spawn_falls_back_to_description_when_name_absent() {
        assert_eq!(
            p(
                "agent",
                json!({
                    "action": "spawn",
                    "agent_type": "code-review",
                    "description": "Correctness review",
                    "prompt": "x"
                })
            ),
            "Spawn agent: Correctness review (code-review)",
            "without `name`, fall back to description (legacy behaviour)"
        );
    }

    /// `name` only (no description, no agent_type) — covers the
    /// minimal-args path the model sometimes emits.
    #[test]
    fn agent_spawn_name_only_renders_just_name() {
        assert_eq!(
            p(
                "agent",
                json!({
                    "action": "spawn",
                    "name": "reviewer",
                    "prompt": "x"
                })
            ),
            "Spawn agent: reviewer"
        );
    }

    #[test]
    fn task_create() {
        assert_eq!(
            p(
                "task",
                json!({"action": "create", "title": "Fix renderer drift"})
            ),
            r#"Creating task: "Fix renderer drift""#
        );
    }

    #[test]
    fn task_update_status() {
        assert_eq!(
            p(
                "task",
                json!({"action": "update", "task_id": "render-pass", "new_status": "in_progress"})
            ),
            "Updating task: render-pass -> in_progress"
        );
    }

    #[test]
    fn task_list_with_filter() {
        assert_eq!(
            p("task", json!({"action": "list", "status_filter": "active"})),
            "Listing tasks: active"
        );
    }

    #[test]
    fn mcp_unknown_tool_formats_as_server_toolname() {
        assert_eq!(
            p("mcp_github_search_issues", json!({})),
            "MCP github search_issues"
        );
    }

    #[test]
    fn unknown_tool_echoes_name() {
        assert_eq!(p("some_exotic_tool", json!({})), "some_exotic_tool");
    }

    #[test]
    fn introspect_is_standalone() {
        assert_eq!(p("introspect", json!({})), "Introspecting…");
    }

    #[test]
    fn verbose_disables_truncation() {
        let long_cmd = "a".repeat(200);
        let out = render_preview(
            "bash",
            &json!({"command": long_cmd}),
            PreviewStyle::Verbose,
            80,
        );
        assert_eq!(out.len(), 202);
    }

    #[test]
    fn concise_truncates_long_bash() {
        let long_cmd = "a".repeat(200);
        let out = render_preview(
            "bash",
            &json!({"command": long_cmd}),
            PreviewStyle::Concise,
            80,
        );
        assert!(out.ends_with('…'), "expected ellipsis: {out}");
        assert!(
            out.chars().count() <= 82,
            "expected ≤80 char budget + 2 prefix: {out}"
        );
    }

    #[test]
    fn mcp_args_summary_empty() {
        assert_eq!(mcp_args_summary(&json!({})), "(no arguments)");
    }

    #[test]
    fn mcp_args_summary_three_keys() {
        assert_eq!(
            mcp_args_summary(&json!({"a": "x", "b": 1, "c": true})),
            r#"a="x", b=1, c=true"#
        );
    }

    #[test]
    fn mcp_args_summary_truncates_after_three() {
        let out = mcp_args_summary(&json!({"a": 1, "b": 2, "c": 3, "d": 4}));
        assert!(out.contains("+1 more"), "{out}");
    }

    /// Pins the documented behaviour: alphabetic key order (since
    /// serde_json's workspace Map is BTreeMap). If
    /// `stall::canonical_tool_args`'s round-trip-sort invariant is
    /// ever replaced by a different canonicalisation strategy, this
    /// test flags that `mcp_args_summary`'s doc comment needs
    /// updating too.
    #[test]
    fn mcp_args_summary_alphabetic_key_order() {
        let out = mcp_args_summary(&json!({"z": 1, "a": 2, "b": 3, "c": 4}));
        assert!(
            out.starts_with("a=2, b=3, c=4"),
            "expected alpha order: {out}"
        );
        assert!(out.contains("+1 more"), "{out}");
    }

    // ─── Schema-canonical field names ──────────────────────────────────
    // The older tests above use legacy field names (status / commit /
    // branch / stash_action / pr_number) to pin the backward-compat
    // fallback paths. These tests pin the schema-canonical keys so a
    // refactor that drops an alias won't silently regress the
    // canonical path.

    #[test]
    fn git_show_uses_canonical_revision_field() {
        assert_eq!(
            p("git", json!({"action": "show", "revision": "abc123"})),
            "Git show abc123"
        );
    }

    #[test]
    fn git_show_defaults_to_head_when_omitted() {
        assert_eq!(p("git", json!({"action": "show"})), "Git show HEAD");
    }

    #[test]
    fn git_log_uses_canonical_ref_field() {
        assert_eq!(
            p("git", json!({"action": "log", "ref": "main"})),
            "Git log main"
        );
    }

    #[test]
    fn git_stash_uses_canonical_sub_action_field() {
        assert_eq!(
            p("git", json!({"action": "stash", "sub_action": "push"})),
            "Git stash push"
        );
    }

    #[test]
    fn git_revert_commit_renders() {
        assert_eq!(
            p(
                "git",
                json!({"action": "revert_commit", "commit_sha": "deadbee"})
            ),
            "Git revert deadbee"
        );
    }

    #[test]
    fn git_checkout_file_with_ref() {
        assert_eq!(
            p(
                "git",
                json!({"action": "checkout_file", "path": "src/lib.rs", "ref": "HEAD~1"})
            ),
            "Git checkout HEAD~1 -- src/lib.rs"
        );
    }

    #[test]
    fn git_worktree_add() {
        assert_eq!(
            p(
                "git",
                json!({"action": "worktree", "sub_action": "add", "path": "../wt"})
            ),
            "Git worktree add ../wt"
        );
    }

    #[test]
    fn github_get_pr_uses_canonical_number_field() {
        assert_eq!(
            p(
                "github",
                json!({"action": "get_pr", "owner": "o", "repo": "r", "number": 7})
            ),
            "GitHub: PR #7 o/r"
        );
    }

    #[test]
    fn github_get_issue_uses_canonical_number_field() {
        assert_eq!(
            p(
                "github",
                json!({"action": "get_issue", "owner": "o", "repo": "r", "number": 99})
            ),
            "GitHub: issue #99 o/r"
        );
    }

    #[test]
    fn task_update_uses_canonical_new_status_field() {
        assert_eq!(
            p(
                "task",
                json!({"action": "update", "task_id": "t-1", "new_status": "completed"})
            ),
            "Updating task: t-1 -> completed"
        );
    }

    #[test]
    fn task_list_uses_canonical_status_filter_field() {
        assert_eq!(
            p(
                "task",
                json!({"action": "list", "status_filter": "pending"})
            ),
            "Listing tasks: pending"
        );
    }

    #[test]
    fn task_list_user_uses_canonical_user_status_field() {
        assert_eq!(
            p("task", json!({"action": "list_user"})),
            "Listing cross-session tasks: active"
        );
        assert_eq!(
            p(
                "task",
                json!({"action": "list_user", "user_status": "paused"})
            ),
            "Listing cross-session tasks: paused"
        );
    }

    #[test]
    fn session_timeline_summary_history_render() {
        assert_eq!(
            p("session", json!({"action": "timeline"})),
            "Session timeline"
        );
        assert_eq!(
            p("session", json!({"action": "summary"})),
            "Session summary"
        );
        assert_eq!(
            p("session", json!({"action": "history"})),
            "Session history"
        );
    }

    #[test]
    fn run_script_shows_first_line() {
        assert_eq!(
            p(
                "run_script",
                json!({"script": "# compute checksum\nimport hashlib\n..."})
            ),
            "Running script: # compute checksum"
        );
    }

    #[test]
    fn run_script_empty_falls_back_to_generic() {
        assert_eq!(p("run_script", json!({"script": ""})), "Running script");
    }
}
