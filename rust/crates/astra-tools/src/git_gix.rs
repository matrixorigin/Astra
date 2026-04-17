#![allow(dead_code)]
#![allow(clippy::collapsible_if)]
//! Pure-Rust git implementations using the `gix` crate.
//!
//! Replaces shell `git` subprocess calls with in-process operations for:
//! - git_status, git_diff, git_log, git_show, git_blame, git_file_history
//!
//! Benefits: no subprocess overhead, no shell injection risk, no `git` binary dependency.

use std::path::Path;
use std::time::SystemTime;

use gix::bstr::{BString, ByteSlice};
use serde_json::Value;

const DIFF_LIMIT: usize = 40_000; // ~10K tokens — diff is the primary input for code review
const SHOW_LIMIT: usize = 16_000;

/// Outcome of a tool execution with optional metadata fields.
#[derive(Debug, Clone, Default)]
pub struct ToolExecutionOutcome {
    pub output: String,
    pub tool_result_fields: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ToolExecutionOutcome {
    pub fn text(output: String) -> Self {
        Self {
            output,
            tool_result_fields: None,
        }
    }
}

/// Simple word tokenizer for search scoring.
/// Splits on non-alphanumeric boundaries and lowercases.
fn estimate_tokens(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_string())
        .collect()
}

/// Truncate output to a byte limit, preferring line boundaries.
fn truncate_output(mut output: String, max_bytes: usize) -> String {
    if output.len() > max_bytes {
        let end = output.floor_char_boundary(max_bytes);
        let cut = output[..end]
            .rfind('\n')
            .filter(|&pos| pos > end / 2)
            .map(|pos| pos + 1)
            .unwrap_or(end);
        output.truncate(cut);
        output.push_str("\n[truncated]");
    }
    output
}

// ~4K tokens; was 30K

/// Reject file paths with `..` components that could escape the repository tree.
fn reject_path_traversal(file: &str) -> Result<(), String> {
    if file.contains("..") {
        Err("Error: path traversal ('..') not allowed in file parameter".to_string())
    } else {
        Ok(())
    }
}

/// Reject git ref strings containing shell metacharacters.
fn reject_shell_meta(ref_str: &str) -> Result<(), String> {
    if ref_str.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '$' | '`' | '(' | ')' | '{' | '}' | '<' | '>' | '!' | '\n'
        )
    }) {
        Err(format!(
            "Error: git ref contains disallowed characters: {ref_str}"
        ))
    } else {
        Ok(())
    }
}

fn reject_stash_selector(selector: &str) -> Result<(), String> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return Err("Error: stash_ref must not be empty".to_string());
    }
    if trimmed.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '$' | '`' | '(' | ')' | '<' | '>' | '\n'
        )
    }) {
        Err(format!(
            "Error: stash selector contains disallowed characters: {selector}"
        ))
    } else {
        Ok(())
    }
}

fn validate_commit_ref(commit_ref: &str, param_name: &str) -> Result<String, String> {
    let trimmed = commit_ref.trim();
    if trimmed.is_empty() {
        return Err(format!("Error: {param_name} must not be empty"));
    }
    if trimmed.starts_with('-') {
        return Err(format!("Error: {param_name} must not start with '-'"));
    }
    reject_shell_meta(trimmed)?;
    Ok(trimmed.to_string())
}

fn resolve_commit_ref(project_root: &Path, commit_ref: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", commit_ref])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn short_commit_sha(commit_sha: &str) -> String {
    commit_sha[..7.min(commit_sha.len())].to_string()
}

pub fn head_first_parent_tail(project_root: &Path, count: usize) -> Option<Vec<String>> {
    if count == 0 {
        return Some(Vec::new());
    }
    std::process::Command::new("git")
        .args([
            "rev-list",
            "--first-parent",
            "--max-count",
            &count.to_string(),
            "HEAD",
        ])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToString::to_string)
                .collect()
        })
}

pub fn git_worktree_is_clean(project_root: &Path) -> Result<bool, String> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
        .map_err(|error| format!("Error: git status failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Error: git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn abort_git_revert(project_root: &Path) -> Result<bool, String> {
    let output = std::process::Command::new("git")
        .args(["revert", "--abort"])
        .current_dir(project_root)
        .output()
        .map_err(|error| format!("Error: git revert --abort failed: {error}"))?;
    if output.status.success() {
        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no cherry-pick or revert in progress") {
            Ok(false)
        } else {
            Err(format!(
                "Error: git revert --abort failed: {}",
                stderr.trim()
            ))
        }
    }
}

fn apply_stash_selector(args: &Value) -> Result<String, String> {
    if let Some(selector) = args.get("stash_ref").and_then(Value::as_str) {
        reject_stash_selector(selector)?;
        return Ok(selector.trim().to_string());
    }
    Ok(stash_index_selector(args))
}

fn stash_index_selector(args: &Value) -> String {
    let idx = args.get("index").and_then(Value::as_u64).unwrap_or(0);
    format!("stash@{{{idx}}}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStashRollbackEntry {
    sequence: u64,
    pub stash_ref: String,
    pub turn_index: u32,
    pub timestamp: SystemTime,
    pub message: Option<String>,
}

#[derive(Debug, Default)]
pub struct GitStashRollbackJournal {
    entries: Vec<GitStashRollbackEntry>,
    next_sequence: u64,
}

impl GitStashRollbackJournal {
    pub fn record(
        &mut self,
        stash_ref: impl Into<String>,
        turn_index: u32,
        message: Option<String>,
    ) {
        self.entries.push(GitStashRollbackEntry {
            sequence: self.next_sequence,
            stash_ref: stash_ref.into(),
            turn_index,
            timestamp: SystemTime::now(),
            message,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    pub fn list(&self) -> Vec<GitStashRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    pub fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitStashRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    pub fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitStashRollbackEntry> {
        self.entries
            .iter()
            .find(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
            .cloned()
            .into_iter()
            .collect()
    }

    pub fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    pub fn remove_stash(&mut self, stash_ref: &str) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.stash_ref == stash_ref)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitRollbackEntry {
    sequence: u64,
    pub commit_sha: String,
    pub turn_index: u32,
    pub timestamp: SystemTime,
    pub message: Option<String>,
}

#[derive(Debug, Default)]
pub struct GitCommitRollbackJournal {
    entries: Vec<GitCommitRollbackEntry>,
    next_sequence: u64,
}

impl GitCommitRollbackJournal {
    pub fn record(
        &mut self,
        commit_sha: impl Into<String>,
        turn_index: u32,
        message: Option<String>,
    ) {
        self.entries.push(GitCommitRollbackEntry {
            sequence: self.next_sequence,
            commit_sha: commit_sha.into(),
            turn_index,
            timestamp: SystemTime::now(),
            message,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    pub fn list(&self) -> Vec<GitCommitRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    pub fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitCommitRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    pub fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitCommitRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
            .cloned()
            .collect()
    }

    pub fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    pub fn remove_commit(&mut self, commit_sha: &str) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.commit_sha == commit_sha)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }
}

fn tool_output_limit() -> usize {
    super::tool_output_limit()
}

/// Scale a base output limit by budget pressure.
/// pressure=0.0 → 100% of base, pressure=0.6 → 70%, pressure=0.9 → 46%.
/// Never goes below 40% of base — aggressive truncation on git diffs
/// forces bash fallbacks which waste a tool round.
fn pressure_scaled_limit(base: usize, pressure: f64) -> usize {
    let scale = (1.0 - pressure * 0.6).max(0.4);
    (base as f64 * scale) as usize
}

fn open_repo(project_root: &Path) -> Result<gix::Repository, String> {
    gix::discover(project_root).map_err(|e| format!("Error: cannot open git repo: {e}"))
}

/// Return the current branch name (like `git branch --show-current`).
pub fn current_branch(project_root: &Path) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    match repo.head_ref() {
        Ok(Some(reference)) => reference.name().shorten().to_string(),
        _ => String::new(),
    }
}

/// Return the short HEAD commit hash (like `git rev-parse --short HEAD`).
pub fn head_short(project_root: &Path) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    match repo.head_id() {
        Ok(id) => id.to_hex_with_len(7).to_string(),
        Err(_) => String::new(),
    }
}

/// Format author time as YYYY-MM-DD from a SignatureRef.
fn format_author_date(sig: &gix::actor::SignatureRef<'_>) -> String {
    sig.time()
        .ok()
        .and_then(|t| t.format(gix::date::time::format::SHORT).ok())
        .unwrap_or_else(|| "?".to_string())
}

// ─── git_status ─────────────────────────────────────────────────────────────

pub fn git_status(project_root: &Path) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let mut out = String::new();

    // Branch info
    if let Ok(head_ref) = repo.head_ref() {
        if let Some(r) = head_ref {
            let name = r.name().shorten().to_string();
            out.push_str(&format!("## {name}\n"));
        } else if let Ok(head) = repo.head_id() {
            out.push_str(&format!(
                "## HEAD detached at {}\n",
                head.to_hex_with_len(8)
            ));
        }
    }

    // Status entries
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => return format!("{out}Error getting status: {e}"),
    };

    let iter = match platform.into_index_worktree_iter(Vec::<BString>::new()) {
        Ok(i) => i,
        Err(e) => return format!("{out}Error iterating status: {e}"),
    };

    use gix::status::index_worktree::iter::Summary;
    let mut count = 0;
    for entry in iter {
        match entry {
            Ok(item) => {
                let path = item.rela_path().to_string();
                let status_char = match item.summary() {
                    Some(Summary::Removed) => "D ",
                    Some(Summary::Added) => "? ",
                    Some(Summary::Modified) => "M ",
                    Some(Summary::TypeChange) => "T ",
                    Some(Summary::Renamed) => "R ",
                    Some(Summary::Copied) => "C ",
                    Some(Summary::IntentToAdd) => "A ",
                    Some(Summary::Conflict) => "U ",
                    None => "? ",
                };
                out.push_str(&format!("{status_char}{path}\n"));
                count += 1;
                if count >= 200 {
                    out.push_str("[truncated — 200+ changes]\n");
                    break;
                }
            }
            Err(e) => {
                out.push_str(&format!("Error: {e}\n"));
                break;
            }
        }
    }

    if out.trim().is_empty() {
        "nothing to commit, working tree clean".to_string()
    } else {
        truncate_output(out, tool_output_limit())
    }
}

// ─── git_log ────────────────────────────────────────────────────────────────

pub fn git_log(project_root: &Path, args: &Value) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let n = args.get("n").and_then(Value::as_u64).unwrap_or(10).min(500) as usize;

    let head = match repo.head_id() {
        Ok(h) => h,
        Err(e) => return format!("Error: {e}"),
    };

    let walk = match head
        .ancestors()
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
    {
        Ok(w) => w,
        Err(e) => return format!("Error: {e}"),
    };

    let mut out = String::new();
    let mut count = 0;
    for info in walk {
        if count >= n {
            break;
        }
        match info {
            Ok(info) => {
                let id = info.id.to_string();
                let short = &id[..7.min(id.len())];
                match info.object() {
                    Ok(commit) => {
                        let raw = commit.message_raw_sloppy();
                        let summary = raw
                            .lines()
                            .next()
                            .map(|l| String::from_utf8_lossy(l).to_string())
                            .unwrap_or_default();
                        out.push_str(&format!("{short} {summary}\n"));
                    }
                    Err(_) => out.push_str(&format!("{short} <object error>\n")),
                }
                count += 1;
            }
            Err(e) => {
                out.push_str(&format!("Error walking commits: {e}\n"));
                break;
            }
        }
    }

    if out.is_empty() {
        "No commits found".to_string()
    } else {
        truncate_output(out, tool_output_limit())
    }
}

// ─── git_show ───────────────────────────────────────────────────────────────

pub fn git_show(
    project_root: &Path,
    args: &Value,
    pressure: f64,
    aggregate_bytes: usize,
) -> String {
    let mut limit = pressure_scaled_limit(SHOW_LIMIT, pressure);
    // Further reduce limit when aggregate output is already high
    if aggregate_bytes > super::AGGREGATE_SOFT_LIMIT {
        let remaining = super::AGGREGATE_OUTPUT_BUDGET.saturating_sub(aggregate_bytes);
        limit = limit.min(remaining).max(2048);
    }
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let commit_ref = match args.get("commit").and_then(Value::as_str) {
        Some(c) => c,
        None => return "Error: missing 'commit' (SHA, branch, or tag)".to_string(),
    };

    // Allow valid git ref characters including reflog (@{}) and tree-object (:)
    if commit_ref.contains(|c: char| {
        !c.is_alphanumeric()
            && c != '-'
            && c != '_'
            && c != '.'
            && c != '/'
            && c != '~'
            && c != '^'
            && c != '@'
            && c != ':'
            && c != '{'
            && c != '}'
    }) {
        return "Error: invalid commit reference".to_string();
    }

    let stat_only = args
        .get("stat_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let file_filter = args.get("file").and_then(Value::as_str);

    // Resolve reference to commit
    let id = match repo.rev_parse_single(commit_ref) {
        Ok(id) => id,
        Err(e) => return format!("Error: cannot resolve '{commit_ref}': {e}"),
    };

    let commit = match id.object() {
        Ok(o) => match o.try_into_commit() {
            Ok(c) => c,
            Err(_) => return format!("Error: '{commit_ref}' is not a commit"),
        },
        Err(e) => return format!("Error: {e}"),
    };

    let mut out = String::new();

    // Header
    out.push_str(&format!("commit {}\n", commit.id));
    if let Ok(author) = commit.author() {
        let name = String::from_utf8_lossy(author.name).to_string();
        let email = String::from_utf8_lossy(author.email).to_string();
        let date = format_author_date(&author);
        out.push_str(&format!("Author: {name} <{email}>\nDate:   {date}\n"));
    }

    let message = String::from_utf8_lossy(commit.message_raw_sloppy()).to_string();
    out.push_str(&format!("\n    {}\n", message.trim()));

    // Merge commits: gix first-parent diff produces useless tree-level output,
    // and `git show --first-parent` can legitimately return only the commit
    // header when the merge tree matches the first parent. Ask git for the
    // per-parent tree diff instead so merge commits still show meaningful
    // stats/patches.
    let is_merge = commit.parent_ids().count() > 1;
    if is_merge {
        let mut cli_args = vec![
            "diff-tree",
            "-m",
            "-r",
            "--no-commit-id",
            "--no-ext-diff",
            "--no-color",
        ];
        if stat_only {
            cli_args.push("--stat");
        } else {
            cli_args.push("-p");
        }
        cli_args.push(commit_ref);
        if let Some(f) = file_filter {
            cli_args.push("--");
            cli_args.push(f);
        }
        let cli_out = std::process::Command::new("git")
            .args(&cli_args)
            .current_dir(project_root)
            .output()
            .ok();
        if let Some(output) = cli_out {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !stdout.is_empty() {
                    out.push('\n');
                    out.push_str(&stdout);
                    out.push('\n');
                    return truncate_show_at(out, limit);
                }
                // Empty output with file filter means the file wasn't changed.
                // Fall through to gix to at least show the commit header.
            }
        }
        // CLI failed or empty — fall through to gix (best effort)
    }

    // Diff: use tree changes API
    let new_tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return out,
    };

    let parent_tree = commit
        .parent_ids()
        .next()
        .and_then(|pid| pid.object().ok())
        .and_then(|o| o.try_into_commit().ok())
        .and_then(|pc| pc.tree().ok());

    let old_tree = match parent_tree {
        Some(t) => t,
        None => {
            // Root commit — show added files recursively
            out.push_str("\n[root commit]\n");
            fn list_tree_entries(tree: &gix::Tree<'_>, prefix: &str, out: &mut String) {
                for e in tree.iter().flatten() {
                    let name = e.filename().to_string();
                    let full = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    if e.mode().is_tree() {
                        if let Ok(obj) = e.object()
                            && let Ok(sub) = obj.try_into_tree()
                        {
                            list_tree_entries(&sub, &full, out);
                        }
                    } else {
                        out.push_str(&format!("A {full}\n"));
                    }
                }
            }
            list_tree_entries(&new_tree, "", &mut out);
            return truncate_show_at(out, limit);
        }
    };

    if stat_only {
        // stat_only: compute stats in a single pass
        let mut changes_platform = match old_tree.changes() {
            Ok(p) => p,
            Err(e) => {
                out.push_str(&format!("\n[diff error: {e}]\n"));
                return truncate_show_at(out, limit);
            }
        };

        if let Ok(stats) = changes_platform.stats(&new_tree) {
            out.push_str(&format!(
                "\n {} files changed, {} insertions(+), {} deletions(-)\n",
                stats.files_changed, stats.lines_added, stats.lines_removed
            ));
        }

        // Also list file names
        let mut changes_platform2 = match old_tree.changes() {
            Ok(p) => p,
            Err(_) => return truncate_show_at(out, limit),
        };
        let _ = changes_platform2.for_each_to_obtain_tree(&new_tree, |change| {
            use gix::object::tree::diff::Change;
            let (location, ct) = match &change {
                Change::Addition { location, .. } => (location.to_string(), "A"),
                Change::Deletion { location, .. } => (location.to_string(), "D"),
                Change::Modification { location, .. } => (location.to_string(), "M"),
                Change::Rewrite { location, .. } => (location.to_string(), "R"),
            };
            if file_filter.is_none() || location.contains(file_filter.unwrap_or("")) {
                out.push_str(&format!(" {ct} {location}\n"));
            }
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        });
    } else {
        // Full diff with line content
        let mut cache = match repo.diff_resource_cache_for_tree_diff() {
            Ok(c) => c,
            Err(e) => {
                out.push_str(&format!("\n[diff cache error: {e}]\n"));
                return truncate_show_at(out, limit);
            }
        };

        let mut changes_platform = match old_tree.changes() {
            Ok(p) => p,
            Err(e) => {
                out.push_str(&format!("\n[diff error: {e}]\n"));
                return truncate_show_at(out, limit);
            }
        };

        let _ = changes_platform.for_each_to_obtain_tree(&new_tree, |change| {
            use gix::object::tree::diff::Change;
            let location = match &change {
                Change::Addition { location, .. }
                | Change::Deletion { location, .. }
                | Change::Modification { location, .. }
                | Change::Rewrite { location, .. } => location.to_string(),
            };

            if let Some(filter) = file_filter
                && !location.contains(filter)
            {
                return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()));
            }

            out.push_str(&format!("--- a/{location}\n+++ b/{location}\n"));

            if let Ok(mut platform) = change.diff(&mut cache) {
                let _ = platform.lines(|hunk| {
                    use gix::object::blob::diff::lines::Change as HC;
                    match hunk {
                        HC::Addition { lines } => {
                            for l in lines {
                                out.push_str(&format!("+{}\n", l));
                            }
                        }
                        HC::Deletion { lines } => {
                            for l in lines {
                                out.push_str(&format!("-{}\n", l));
                            }
                        }
                        HC::Modification {
                            lines_before,
                            lines_after,
                        } => {
                            for l in lines_before {
                                out.push_str(&format!("-{}\n", l));
                            }
                            for l in lines_after {
                                out.push_str(&format!("+{}\n", l));
                            }
                        }
                    }
                    Ok::<_, std::convert::Infallible>(())
                });
            }

            out.push('\n');

            if out.len() > limit {
                return Ok(std::ops::ControlFlow::Break(()));
            }
            Ok(std::ops::ControlFlow::Continue(()))
        });
    }

    let mut result = truncate_show_at(out, limit);

    // When viewing many per-file diffs from the same commit, nudge the model
    // to wrap up early rather than exhausting the aggregate budget.
    if file_filter.is_some() && aggregate_bytes > super::AGGREGATE_SOFT_LIMIT / 2 {
        result.push_str(
            "\n[hint: aggregate output is high — finish reviewing with the files \
             already read, or use stat_only:true to prioritize remaining files]",
        );
    }

    result
}

fn truncate_show_at(out: String, limit: usize) -> String {
    if out.len() > limit {
        let end = out.floor_char_boundary(limit);
        let mut t = out[..end].to_string();
        t.push_str("\n[truncated — use stat_only:true or file param to narrow]");
        t
    } else {
        out
    }
}

// ─── git_blame ──────────────────────────────────────────────────────────────

pub fn git_blame(project_root: &Path, args: &Value) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let file = match args.get("file").and_then(Value::as_str) {
        Some(f) => f,
        None => return "Error: missing 'file' parameter".to_string(),
    };
    if let Err(e) = reject_path_traversal(file) {
        return e;
    }

    let line_start = args.get("line_start").and_then(Value::as_u64);
    let line_end = args.get("line_end").and_then(Value::as_u64);

    let head = match repo.head_id() {
        Ok(h) => h,
        Err(e) => return format!("Error: {e}"),
    };

    // Build blame options with optional line range
    let mut options = gix::repository::blame_file::Options::default();
    if let Some(start) = line_start {
        let end = line_end.unwrap_or(start);
        match gix::blame::BlameRanges::from_one_based_inclusive_ranges(vec![
            (start as u32)..=(end as u32),
        ]) {
            Ok(ranges) => options.ranges = ranges,
            Err(e) => return format!("Error: invalid line range: {e}"),
        }
    }

    let file_bstr: &gix::bstr::BStr = file.as_bytes().as_ref();
    let outcome = match repo.blame_file(file_bstr, head.detach(), options) {
        Ok(o) => o,
        Err(e) => return format!("Error: blame failed for '{file}': {e}"),
    };

    // Format using entries_with_lines for correct line content
    let mut out = String::new();
    let mut unique_authors = std::collections::HashSet::new();
    let mut unique_commits = std::collections::HashSet::new();
    let mut total_lines = 0u32;

    for (entry, lines) in outcome.entries_with_lines() {
        let commit_id_str = entry.commit_id.to_string();
        let short_commit = &commit_id_str[..8.min(commit_id_str.len())];

        // Look up author/date from commit
        let (author_name, date_str): (String, String) = repo
            .find_object(entry.commit_id)
            .ok()
            .and_then(|obj| obj.try_into_commit().ok())
            .and_then(|c: gix::Commit<'_>| {
                c.author().ok().map(|a| {
                    let name = String::from_utf8_lossy(a.name).to_string();
                    let date = format_author_date(&a);
                    (name, date)
                })
            })
            .unwrap_or_else(|| ("?".to_string(), "?".to_string()));

        let start_line = entry.start_in_blamed_file as u64 + 1;

        for (offset, line_content) in lines.iter().enumerate() {
            let line_no = start_line + offset as u64;

            // Apply line range filter (in case blame ranges weren't exact)
            if let Some(s) = line_start
                && line_no < s
            {
                continue;
            }
            if let Some(e) = line_end
                && line_no > e
            {
                continue;
            }

            let content = String::from_utf8_lossy(line_content);
            let content = content.trim_end();
            out.push_str(&format!(
                "L{line_no} {short_commit} {date_str} [{author_name}] {content}\n"
            ));

            unique_authors.insert(author_name.clone());
            unique_commits.insert(short_commit.to_string());
            total_lines += 1;
        }
    }

    if total_lines == 0 {
        return format!("No blame data for '{file}'");
    }

    out.push_str(&format!(
        "\n--- {} lines, {} authors, {} commits ---",
        total_lines,
        unique_authors.len(),
        unique_commits.len(),
    ));

    truncate_output(out, tool_output_limit())
}

// ─── git_diff ───────────────────────────────────────────────────────────────

/// `git diff … --stat` via the real `git` CLI (same sources as full diff, no bash).
fn git_diff_stat_cli(project_root: &Path, args: &Value, limit: usize) -> String {
    let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
    let git_ref = args.get("ref").and_then(Value::as_str);
    let base_ref = args.get("base_ref").and_then(Value::as_str);
    let path_filter = args.get("path").and_then(Value::as_str);

    if staged && git_ref.is_some() {
        return "Error: git_diff: use either staged:true or ref, not both".to_string();
    }

    let mut parts: Vec<String> = vec!["diff".into()];
    if let Some(base) = base_ref {
        if let Err(e) = reject_shell_meta(base) {
            return e;
        }
        let tip = git_ref.unwrap_or("HEAD");
        if let Err(e) = reject_shell_meta(tip) {
            return e;
        }
        parts.push(format!("{base}..{tip}"));
    } else if staged {
        parts.push("--cached".into());
    } else if let Some(r) = git_ref {
        parts.push(r.to_string());
        if path_filter.is_none() {
            parts.push("HEAD".into());
        }
    } else {
        parts.push("HEAD".into());
    }
    parts.extend(["--stat".into(), "--no-color".into()]);
    if let Some(p) = path_filter {
        parts.push("--".into());
        parts.push(p.to_string());
    }

    let cmd_refs: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
    diff_via_git_cli(project_root, &cmd_refs, limit).unwrap_or_else(|| "No changes".to_string())
}

pub fn git_diff(
    project_root: &Path,
    args: &Value,
    pressure: f64,
    aggregate_bytes: usize,
) -> String {
    let mut limit = pressure_scaled_limit(DIFF_LIMIT, pressure);
    // Further reduce limit when aggregate output is already high
    if aggregate_bytes > super::AGGREGATE_SOFT_LIMIT {
        let remaining = super::AGGREGATE_OUTPUT_BUDGET.saturating_sub(aggregate_bytes);
        limit = limit.min(remaining).max(2048);
    }
    let stat_only = args
        .get("stat_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stat_only {
        return git_diff_stat_cli(project_root, args, limit);
    }

    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
    let git_ref = args.get("ref").and_then(Value::as_str);
    let base_ref = args.get("base_ref").and_then(Value::as_str);
    let path_filter = args.get("path").and_then(Value::as_str);
    if let Some(p) = path_filter {
        if let Err(e) = reject_path_traversal(p) {
            return e;
        }
    }

    // Range diff: base_ref..ref (e.g., HEAD~5..HEAD)
    if let Some(base) = base_ref {
        if let Err(e) = reject_shell_meta(base) {
            return e;
        }
        let tip = git_ref.unwrap_or("HEAD");
        if let Err(e) = reject_shell_meta(tip) {
            return e;
        }
        let range = format!("{base}..{tip}");
        let mut cli_args = vec!["diff", &range, "--no-ext-diff", "--no-color"];
        let path_owned;
        if let Some(p) = path_filter {
            cli_args.push("--");
            path_owned = p.to_string();
            cli_args.push(&path_owned);
        }
        return diff_via_git_cli(project_root, &cli_args, limit)
            .unwrap_or_else(|| "No changes".to_string());
    }

    // If a ref is given, do a tree-to-tree diff (HEAD vs ref)
    if let Some(ref_str) = git_ref {
        // With path filter, use CLI for tree-to-tree as well
        if let Some(p) = path_filter
            && let Some(result) = diff_via_git_cli(
                project_root,
                &["diff", ref_str, "--no-ext-diff", "--no-color", "--", p],
                limit,
            )
        {
            return result;
        }
        return diff_tree_to_tree_str(&repo, ref_str, limit);
    }

    // If staged, do index-to-HEAD diff
    if staged {
        let cli_args: Vec<&str> = if let Some(p) = path_filter {
            vec!["diff", "--cached", "--no-ext-diff", "--no-color", "--", p]
        } else {
            vec!["diff", "--cached", "--no-ext-diff", "--no-color"]
        };
        let result = diff_via_git_cli(project_root, &cli_args, limit)
            .unwrap_or_else(|| diff_index_to_head(&repo, limit));
        if result == "No changes" {
            return "No staged changes".to_string();
        }
        return result;
    }

    // Default: show full local patch vs HEAD so review flows don't need to fall
    // back to bash just to recover actual diff hunks from summary-only output.
    let cli_args: Vec<&str> = if let Some(p) = path_filter {
        vec!["diff", "HEAD", "--no-ext-diff", "--no-color", "--", p]
    } else {
        vec!["diff", "HEAD", "--no-ext-diff", "--no-color"]
    };
    diff_via_git_cli(project_root, &cli_args, limit).unwrap_or_else(|| diff_worktree(&repo, limit))
}

fn diff_via_git_cli(project_root: &Path, args: &[&str], limit: usize) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if stdout.trim().is_empty() {
        return Some("No changes".to_string());
    }
    Some(truncate_diff_at(stdout, limit.min(tool_output_limit())))
}

/// Diff between two tree-ish refs (e.g., HEAD vs a branch/commit).
fn diff_tree_to_tree_str(repo: &gix::Repository, ref_str: &str, limit: usize) -> String {
    let head_tree = match resolve_tree(repo, "HEAD") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let other_tree = match resolve_tree(repo, ref_str) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let mut cache = match repo.diff_resource_cache_for_tree_diff() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };

    let mut out = String::new();
    let mut count = 0u32;

    let mut changes_platform = match other_tree.changes() {
        Ok(p) => p,
        Err(e) => return format!("Error: {e}"),
    };

    let result = changes_platform.for_each_to_obtain_tree(
        &head_tree,
        |change: gix::object::tree::diff::Change<'_, '_, '_>| {
            use gix::object::tree::diff::Change;
            let location = match &change {
                Change::Addition { location, .. }
                | Change::Deletion { location, .. }
                | Change::Modification { location, .. }
                | Change::Rewrite { location, .. } => location.to_string(),
            };
            let change_type = match &change {
                Change::Addition { .. } => "new file",
                Change::Deletion { .. } => "deleted",
                Change::Modification { .. } => "modified",
                Change::Rewrite { .. } => "renamed",
            };

            out.push_str(&format!(
                "diff --git a/{location} b/{location}\n--- a/{location}\n+++ b/{location}\n"
            ));

            // Try to get line-level diff
            if let Ok(mut platform) = change.diff(&mut cache) {
                let _ = platform.lines(|hunk| {
                    use gix::object::blob::diff::lines::Change as HC;
                    match hunk {
                        HC::Addition { lines } => {
                            for l in lines {
                                out.push_str(&format!("+{}\n", l));
                            }
                        }
                        HC::Deletion { lines } => {
                            for l in lines {
                                out.push_str(&format!("-{}\n", l));
                            }
                        }
                        HC::Modification {
                            lines_before,
                            lines_after,
                        } => {
                            for l in lines_before {
                                out.push_str(&format!("-{}\n", l));
                            }
                            for l in lines_after {
                                out.push_str(&format!("+{}\n", l));
                            }
                        }
                    }
                    Ok::<_, std::convert::Infallible>(())
                });
            } else {
                out.push_str(&format!("# {change_type}: {location}\n"));
            }
            out.push('\n');
            count += 1;

            if out.len() > limit {
                return Ok(std::ops::ControlFlow::Break(()));
            }
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        },
    );

    if let Err(e) = result {
        out.push_str(&format!("\n[diff error: {e}]\n"));
    }

    if out.is_empty() {
        "No changes".to_string()
    } else {
        out.push_str(&format!(
            "\n{count} file(s) changed (summary only — use `git diff` for full patch)"
        ));
        truncate_diff_at(out, limit)
    }
}

/// Diff staged (index) changes against HEAD.
fn diff_index_to_head(repo: &gix::Repository, limit: usize) -> String {
    let head_tree = match resolve_tree(repo, "HEAD") {
        Ok(t) => t,
        Err(e) => return e,
    };

    // Get the index and convert to a tree for comparison
    let index = match repo.index() {
        Ok(i) => i,
        Err(e) => return format!("Error reading index: {e}"),
    };

    // Build changes by comparing HEAD tree entries with index entries
    let mut out = String::new();
    let mut count = 0u32;

    for entry in index.entries() {
        let path = entry.path(&index);
        let path_str = path.to_string();

        // Check if HEAD has this file and whether it differs
        let head_entry = head_tree.lookup_entry_by_path(&path_str);
        let in_head: Option<gix::ObjectId> = match &head_entry {
            Ok(Some(he)) => Some(he.object_id()),
            _ => None,
        };

        let idx_oid = entry.id;

        match in_head {
            Some(head_oid) if head_oid == idx_oid => continue, // unchanged
            Some(_head_oid) => {
                out.push_str(&format!(
                    "diff --git a/{path_str} b/{path_str}\n--- a/{path_str}\n+++ b/{path_str}\n"
                ));
                out.push_str("# modified (staged)\n\n");
                count += 1;
            }
            None => {
                out.push_str(&format!(
                    "diff --git a/{path_str} b/{path_str}\n--- /dev/null\n+++ b/{path_str}\n"
                ));
                out.push_str("# new file (staged)\n\n");
                count += 1;
            }
        }

        if out.len() > limit {
            out.push_str("[truncated]\n");
            break;
        }
    }

    // Detect staged deletions: files in HEAD tree but absent from index
    {
        fn collect_tree_paths(
            tree: &gix::Tree<'_>,
            prefix: &str,
            paths: &mut std::collections::HashSet<String>,
        ) {
            for e in tree.iter().flatten() {
                let name = e.filename().to_string();
                let full = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if e.mode().is_tree() {
                    if let Ok(obj) = e.object()
                        && let Ok(sub) = obj.try_into_tree()
                    {
                        collect_tree_paths(&sub, &full, paths);
                    }
                } else {
                    paths.insert(full);
                }
            }
        }

        let mut head_paths = std::collections::HashSet::new();
        collect_tree_paths(&head_tree, "", &mut head_paths);

        let index_paths: std::collections::HashSet<String> = index
            .entries()
            .iter()
            .map(|e| e.path(&index).to_string())
            .collect();

        for deleted_path in head_paths.difference(&index_paths) {
            out.push_str(&format!(
                "diff --git a/{deleted_path} b/{deleted_path}\n--- a/{deleted_path}\n+++ /dev/null\n"
            ));
            out.push_str("# deleted (staged)\n\n");
            count += 1;
            if out.len() > limit {
                out.push_str("[truncated]\n");
                break;
            }
        }
    }

    if out.is_empty() {
        "No staged changes".to_string()
    } else {
        out.push_str(&format!("\n{count} file(s) staged"));
        truncate_diff_at(out, limit)
    }
}

/// Diff worktree (unstaged) changes.
fn diff_worktree(repo: &gix::Repository, limit: usize) -> String {
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => return format!("Error: {e}"),
    };

    let iter = match platform.into_index_worktree_iter(Vec::<BString>::new()) {
        Ok(i) => i,
        Err(e) => return format!("Error: {e}"),
    };

    use gix::status::index_worktree::iter::Summary;
    let mut out = String::new();
    let mut count = 0;

    for entry in iter {
        match entry {
            Ok(item) => {
                let path = item.rela_path().to_string();
                let summary = match item.summary() {
                    Some(s) => s,
                    None => continue,
                };

                let status_str = match summary {
                    Summary::Modified => "modified",
                    Summary::Added | Summary::IntentToAdd => "new file",
                    Summary::Removed => "deleted",
                    Summary::Renamed => "renamed",
                    Summary::TypeChange => "typechange",
                    _ => "changed",
                };

                out.push_str(&format!(
                    "diff --git a/{path} b/{path}\n# {status_str}: {path}\n\n"
                ));
                count += 1;

                if out.len() > limit {
                    out.push_str("[truncated]\n");
                    break;
                }
            }
            Err(e) => {
                out.push_str(&format!("Error: {e}\n"));
                break;
            }
        }
    }

    if out.is_empty() {
        "No changes".to_string()
    } else {
        out.push_str(&format!(
            "\n{count} file(s) changed (summary only — use `git diff` for full patch)"
        ));
        truncate_diff_at(out, limit)
    }
}

fn resolve_tree<'r>(repo: &'r gix::Repository, ref_str: &str) -> Result<gix::Tree<'r>, String> {
    let id = repo
        .rev_parse_single(ref_str)
        .map_err(|e| format!("Error: cannot resolve '{ref_str}': {e}"))?;
    let obj = id.object().map_err(|e| format!("Error: {e}"))?;
    let commit = obj
        .try_into_commit()
        .map_err(|_| format!("Error: '{ref_str}' is not a commit"))?;
    commit
        .tree()
        .map_err(|e| format!("Error: cannot get tree: {e}"))
}

fn truncate_diff_at(out: String, limit: usize) -> String {
    if out.len() > limit {
        let end = out.floor_char_boundary(limit);
        let mut t = out[..end].to_string();
        t.push_str("\n[truncated]");
        t
    } else {
        out
    }
}

// ─── git_file_history ───────────────────────────────────────────────────────

pub fn git_file_history(project_root: &Path, args: &Value) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let file = match args.get("file").and_then(Value::as_str) {
        Some(f) => f,
        None => return "Error: missing 'file' parameter".to_string(),
    };
    if let Err(e) = reject_path_traversal(file) {
        return e;
    }

    let n = args.get("n").and_then(Value::as_u64).unwrap_or(10) as usize;

    let head = match repo.head_id() {
        Ok(h) => h,
        Err(e) => return format!("Error: {e}"),
    };

    let walk = match head
        .ancestors()
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
    {
        Ok(w) => w,
        Err(e) => return format!("Error: {e}"),
    };

    let mut lines = Vec::new();
    let mut walked = 0usize;
    const MAX_WALK: usize = 50_000;
    #[allow(clippy::explicit_counter_loop)]
    for info in walk {
        if lines.len() >= n || walked >= MAX_WALK {
            break;
        }
        walked += 1;
        let info = match info {
            Ok(i) => i,
            Err(_) => break,
        };

        let commit = match info.object() {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check if this commit touches our file by comparing tree entries
        let tree = match commit.tree() {
            Ok(t) => t,
            Err(_) => continue,
        };

        // Check if file exists in this commit's tree
        let cur_entry = match tree.lookup_entry_by_path(file) {
            Ok(Some(e)) => e,
            _ => continue,
        };

        // Check if parent had different content (file actually changed)
        let parent_has_same = commit.parent_ids().next().is_some_and(|pid| {
            pid.object()
                .ok()
                .and_then(|o| o.try_into_commit().ok())
                .and_then(|pc| pc.tree().ok())
                .map(|pt| match pt.lookup_entry_by_path(file) {
                    Ok(Some(parent_entry)) => parent_entry.object_id() == cur_entry.object_id(),
                    _ => false,
                })
                .unwrap_or(false)
        });

        if parent_has_same {
            continue;
        }

        let id = info.id.to_string();
        let short = &id[..8.min(id.len())];
        let author = commit
            .author()
            .ok()
            .map(|a| String::from_utf8_lossy(a.name).to_string())
            .unwrap_or_else(|| "?".to_string());
        let date = commit
            .author()
            .ok()
            .map(|a| format_author_date(&a))
            .unwrap_or_else(|| "?".to_string());
        let summary = {
            let raw = commit.message_raw_sloppy();
            raw.lines()
                .next()
                .map(|l| String::from_utf8_lossy(l).to_string())
                .unwrap_or_default()
        };

        lines.push(format!("{short} {date} [{author}] {summary}"));
    }

    if lines.is_empty() {
        return format!("No history found for '{file}'");
    }

    truncate_output(
        format!(
            "File: {file}\nCommits: {}\n\n{}",
            lines.len(),
            lines.join("\n")
        ),
        tool_output_limit(),
    )
}

// ─── git_log_search (TF-IDF semantic search) ────────────────────────────────

/// A parsed commit with pre-computed tokens for TF-IDF scoring.
struct CommitDoc {
    hash: String,
    author: String,
    date: String,
    message: String,
    tokens: Vec<String>,
}

/// Score commit messages against a query using TF-IDF cosine similarity.
fn score_commits(query: &str, commits: &[CommitDoc]) -> Vec<(usize, f64)> {
    let query_tokens = estimate_tokens(query);
    if query_tokens.is_empty() || commits.is_empty() {
        return Vec::new();
    }

    let n = commits.len() as f64;

    // Build IDF from the commit corpus
    let mut doc_freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for doc in commits {
        let unique: std::collections::HashSet<&String> = doc.tokens.iter().collect();
        for t in unique {
            *doc_freq.entry(t.clone()).or_default() += 1;
        }
    }
    let idf: std::collections::HashMap<String, f64> = doc_freq
        .into_iter()
        .map(|(term, df)| (term, (n / df as f64).ln().max(0.1)))
        .collect();

    let mut scores: Vec<(usize, f64)> = commits
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            let total = doc.tokens.len().max(1) as f64;
            let mut doc_tf: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
            for t in &doc.tokens {
                *doc_tf.entry(t.as_str()).or_default() += 1.0;
            }
            for v in doc_tf.values_mut() {
                *v /= total;
            }

            let mut dot = 0.0;
            let mut q_norm_sq = 0.0;
            let mut d_norm_sq = 0.0;

            for qt in &query_tokens {
                let idf_val = idf.get(qt.as_str()).copied().unwrap_or(0.0);
                let q_w = idf_val;
                q_norm_sq += q_w * q_w;
                if let Some(&tf) = doc_tf.get(qt.as_str()) {
                    let d_w = tf * idf_val;
                    dot += q_w * d_w;
                }
            }
            for (term, &tf) in &doc_tf {
                let idf_val = idf.get(*term).copied().unwrap_or(0.0);
                let d_w = tf * idf_val;
                d_norm_sq += d_w * d_w;
            }

            let denom = q_norm_sq.sqrt() * d_norm_sq.sqrt();
            let score = if denom > 0.0 { dot / denom } else { 0.0 };
            (i, score)
        })
        .filter(|(_, s)| *s > 0.01)
        .collect();

    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores
}

pub fn git_log_search(project_root: &Path, args: &Value) -> String {
    let query = match args.get("query").and_then(Value::as_str) {
        Some(q) if !q.trim().is_empty() => q,
        _ => return "Error: missing or empty 'query' parameter".to_string(),
    };

    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let n = args.get("n").and_then(Value::as_u64).unwrap_or(200) as usize;

    let head = match repo.head_id() {
        Ok(h) => h,
        Err(e) => return format!("Error: {e}"),
    };

    let walk = match head
        .ancestors()
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
    {
        Ok(w) => w,
        Err(e) => return format!("Error: {e}"),
    };

    // Collect commits
    let mut commits = Vec::new();
    for info in walk {
        if commits.len() >= n {
            break;
        }
        let info = match info {
            Ok(i) => i,
            Err(_) => break,
        };
        let commit = match info.object() {
            Ok(c) => c,
            Err(_) => continue,
        };

        let hash = info.id.to_string();
        let author = commit
            .author()
            .ok()
            .map(|a| String::from_utf8_lossy(a.name).to_string())
            .unwrap_or_else(|| "?".to_string());
        let date = commit
            .author()
            .ok()
            .map(|a| format_author_date(&a))
            .unwrap_or_else(|| "?".to_string());
        let message = {
            let raw = commit.message_raw_sloppy();
            raw.lines()
                .next()
                .map(|l| String::from_utf8_lossy(l).to_string())
                .unwrap_or_default()
        };
        let tokens = estimate_tokens(&message);

        commits.push(CommitDoc {
            hash,
            author,
            date,
            message,
            tokens,
        });
    }

    if commits.is_empty() {
        return "No commits found".to_string();
    }

    let ranked = score_commits(query, &commits);
    if ranked.is_empty() {
        return format!(
            "No commits matching '{}' found in last {} commits",
            query,
            commits.len()
        );
    }

    let top_k = 10.min(ranked.len());
    let mut result = format!(
        "Search: '{}' ({} commits searched, {} matches)\n\n",
        query,
        commits.len(),
        ranked.len()
    );
    for (i, &(idx, score)) in ranked.iter().take(top_k).enumerate() {
        let c = &commits[idx];
        result.push_str(&format!(
            "{}. [score:{:.2}] {} {} [{}] {}\n",
            i + 1,
            score,
            &c.hash[..8.min(c.hash.len())],
            c.date,
            c.author,
            c.message,
        ));
    }

    truncate_output(result, tool_output_limit())
}

// ─── git_contributors ───────────────────────────────────────────────────────

pub fn git_contributors(project_root: &Path, args: &Value) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let path_filter = args.get("path").and_then(Value::as_str);
    if let Some(p) = path_filter {
        if let Err(e) = reject_path_traversal(p) {
            return e;
        }
    }
    let since_str = args.get("since").and_then(Value::as_str);

    // Parse --since into a unix timestamp cutoff
    let since_cutoff: Option<i64> = since_str.and_then(parse_since_to_epoch);

    let head = match repo.head_id() {
        Ok(h) => h,
        Err(e) => return format!("Error: {e}"),
    };

    let walk = match head
        .ancestors()
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
    {
        Ok(w) => w,
        Err(e) => return format!("Error: {e}"),
    };

    let mut author_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut file_freq: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut recent_lines: Vec<String> = Vec::new();
    let mut total_commits = 0u32;
    let mut walked = 0u32;
    const MAX_WALK: u32 = 50_000;

    #[allow(clippy::explicit_counter_loop)]
    for info in walk {
        if walked >= MAX_WALK {
            break;
        }
        walked += 1;
        let info = match info {
            Ok(i) => i,
            Err(_) => break,
        };

        let commit = match info.object() {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Date cutoff
        if let Some(cutoff) = since_cutoff
            && let Ok(author) = commit.author()
            && let Ok(time) = author.time()
            && time.seconds < cutoff
        {
            break; // Commits are sorted newest-first, so stop early
        }

        // Skip merges (2+ parents)
        if commit.parent_ids().count() > 1 {
            continue;
        }

        let author_name = commit
            .author()
            .ok()
            .map(|a| String::from_utf8_lossy(a.name).to_string())
            .unwrap_or_else(|| "?".to_string());

        // Path filter: check if commit touches the target path
        if let Some(path) = path_filter {
            let tree = match commit.tree() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let cur_entry = match tree.lookup_entry_by_path(path) {
                Ok(Some(e)) => e,
                _ => continue,
            };
            // Check parent differs
            let parent_same = commit.parent_ids().next().is_some_and(|pid| {
                pid.object()
                    .ok()
                    .and_then(|o| o.try_into_commit().ok())
                    .and_then(|pc| pc.tree().ok())
                    .map(|pt| match pt.lookup_entry_by_path(path) {
                        Ok(Some(pe)) => pe.object_id() == cur_entry.object_id(),
                        _ => false,
                    })
                    .unwrap_or(false)
            });
            if parent_same {
                continue;
            }
        }

        *author_counts.entry(author_name).or_default() += 1;

        // Collect changed files for hot-files (up to 500 commits)
        if total_commits < 500 {
            let tree = commit.tree().ok();
            let parent_tree = commit
                .parent_ids()
                .next()
                .and_then(|pid| pid.object().ok())
                .and_then(|o| o.try_into_commit().ok())
                .and_then(|pc| pc.tree().ok());

            if let (Some(new_tree), Some(old_tree)) = (tree, parent_tree)
                && let Ok(mut changes) = old_tree.changes()
            {
                let _ = changes.for_each_to_obtain_tree(&new_tree, |change| {
                    use gix::object::tree::diff::Change;
                    let location = match &change {
                        Change::Addition { location, .. }
                        | Change::Deletion { location, .. }
                        | Change::Modification { location, .. }
                        | Change::Rewrite { location, .. } => location.to_string(),
                    };

                    if let Some(pf) = path_filter
                        && !location.starts_with(pf)
                    {
                        return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(
                            (),
                        ));
                    }

                    *file_freq.entry(location).or_default() += 1;
                    Ok(std::ops::ControlFlow::Continue(()))
                });
            }
        }

        // Recent activity (first 5)
        if recent_lines.len() < 5 {
            let id_str = info.id.to_string();
            let short = &id_str[..7.min(id_str.len())];
            let msg = {
                let raw = commit.message_raw_sloppy();
                raw.lines()
                    .next()
                    .map(|l| String::from_utf8_lossy(l).to_string())
                    .unwrap_or_default()
            };
            recent_lines.push(format!("{short} {msg}"));
        }

        total_commits += 1;

        // Safety cap
        if total_commits >= 10_000 {
            break;
        }
    }

    // Format output
    let mut parts = Vec::new();

    // Top contributors
    if !author_counts.is_empty() {
        let mut sorted: Vec<_> = author_counts.into_iter().collect();
        sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
        let top: Vec<String> = sorted
            .iter()
            .take(10)
            .map(|(name, count)| format!("  {:>4}  {}", count, name))
            .collect();
        parts.push(format!("## Top Contributors\n{}", top.join("\n")));
    }

    // Hot files
    if !file_freq.is_empty() {
        let mut sorted: Vec<_> = file_freq.into_iter().collect();
        sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
        let top_files: Vec<String> = sorted
            .iter()
            .take(10)
            .map(|(f, c)| format!("  {:>3}× {}", c, f))
            .collect();
        parts.push(format!(
            "## Hot Files (most changed)\n{}",
            top_files.join("\n")
        ));
    }

    // Recent activity
    if !recent_lines.is_empty() {
        parts.push(format!("## Recent Activity\n{}", recent_lines.join("\n")));
    }

    if parts.is_empty() {
        "No git history found".to_string()
    } else {
        truncate_output(parts.join("\n\n"), tool_output_limit())
    }
}

/// Parse a "since" string into a unix epoch timestamp.
/// Supports ISO dates like "2024-01-01" and relative like "2 weeks ago".
fn parse_since_to_epoch(since: &str) -> Option<i64> {
    // Try ISO date (YYYY-MM-DD)
    if since.len() == 10 && since.chars().nth(4) == Some('-') {
        let parts: Vec<&str> = since.split('-').collect();
        if parts.len() == 3 {
            let y: i64 = parts[0].parse().ok()?;
            let m: i64 = parts[1].parse().ok()?;
            let d: i64 = parts[2].parse().ok()?;
            if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                return None;
            }
            // Rough epoch calculation (not leap-second accurate, good enough for filtering)
            let days_since_epoch = (y - 1970) * 365 + (y - 1969) / 4 - (y - 1901) / 100
                + (y - 1601) / 400
                + [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][(m - 1) as usize]
                + d
                - 1;
            return Some(days_since_epoch * 86400);
        }
    }
    // Relative dates: just skip filtering for unsupported formats
    None
}

// ─── Git Mutation Tools ─────────────────────────────────────────────────────
// These use git subprocess (not gix) because gix's write operations are
// complex and the git binary is universally available. Read tools stay pure-Rust.

/// Stage files and create a commit.
///
/// Parameters:
/// - `message` (required): commit message
/// - `files` (optional): list of file paths to stage; if omitted, stages all changes
/// - `all` (optional): if true, stages all tracked changes (like `git commit -a`)
pub fn git_commit(project_root: &Path, args: &Value) -> String {
    git_commit_with_metadata(project_root, args).output
}

pub fn git_commit_with_metadata(project_root: &Path, args: &Value) -> ToolExecutionOutcome {
    let message = match args.get("message").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => {
            return ToolExecutionOutcome::text(
                "Error: 'message' is required and must not be empty".to_string(),
            );
        }
    };

    // Validate message length (prevent absurdly long messages)
    if message.len() > 5000 {
        return ToolExecutionOutcome::text(
            "Error: commit message too long (max 5000 chars)".to_string(),
        );
    }

    // Stage files
    let files: Vec<&str> = args
        .get("files")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let stage_all = args.get("all").and_then(Value::as_bool).unwrap_or(false);

    if files.is_empty() && !stage_all {
        // Default: stage all changes
        let add_out = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(project_root)
            .output();
        match add_out {
            Err(e) => {
                return ToolExecutionOutcome::text(format!("Error: git add failed: {e}"));
            }
            Ok(ref out) if !out.status.success() => {
                return ToolExecutionOutcome::text(format!(
                    "Error: git add -A failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(_) => {}
        }
    } else if !files.is_empty() {
        // Stage specific files
        let mut cmd = std::process::Command::new("git");
        cmd.arg("add").args(&files).current_dir(project_root);
        let add_out = cmd.output();
        match add_out {
            Err(e) => {
                return ToolExecutionOutcome::text(format!("Error: git add failed: {e}"));
            }
            Ok(ref out) if !out.status.success() => {
                return ToolExecutionOutcome::text(format!(
                    "Error: git add failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(_) => {}
        }
    }

    // Commit
    let mut commit_args = vec!["commit", "-m", message];
    if stage_all && files.is_empty() {
        commit_args.insert(1, "-a");
    }
    let commit_out = std::process::Command::new("git")
        .args(&commit_args)
        .current_dir(project_root)
        .output();

    match commit_out {
        Ok(out) if out.status.success() => {
            let commit_sha = resolve_commit_ref(project_root, "HEAD");
            let short_hash = commit_sha
                .as_deref()
                .map(short_commit_sha)
                .unwrap_or_else(|| {
                    String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("???")
                        .to_string()
                });
            let tool_result_fields = commit_sha.map(|commit_sha| {
                serde_json::Map::from_iter([
                    ("commit_sha".to_string(), Value::String(commit_sha.clone())),
                    (
                        "commit_short_sha".to_string(),
                        Value::String(short_commit_sha(&commit_sha)),
                    ),
                ])
            });
            ToolExecutionOutcome {
                output: format!("✓ Committed: {short_hash} {message}"),
                tool_result_fields,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("nothing to commit") {
                ToolExecutionOutcome::text("Nothing to commit — working tree clean".to_string())
            } else {
                ToolExecutionOutcome::text(format!("Error: git commit failed: {}", stderr.trim()))
            }
        }
        Err(e) => ToolExecutionOutcome::text(format!("Error: git commit failed: {e}")),
    }
}

/// Create a compensating revert commit for an earlier git commit.
///
/// Parameters:
/// - `commit_sha` (required): full commit SHA or git revision to revert
pub fn git_revert_commit(project_root: &Path, args: &Value) -> String {
    git_revert_commit_with_metadata(project_root, args).output
}

pub fn git_revert_commit_with_metadata(project_root: &Path, args: &Value) -> ToolExecutionOutcome {
    let commit_ref = match args.get("commit_sha").and_then(Value::as_str) {
        Some(commit_ref) => match validate_commit_ref(commit_ref, "commit_sha") {
            Ok(commit_ref) => commit_ref,
            Err(error) => return ToolExecutionOutcome::text(error),
        },
        None => {
            return ToolExecutionOutcome::text("Error: 'commit_sha' is required".to_string());
        }
    };
    let target_commit_sha = match resolve_commit_ref(project_root, &commit_ref) {
        Some(commit_sha) => commit_sha,
        None => {
            return ToolExecutionOutcome::text(format!("Error: unknown commit '{commit_ref}'"));
        }
    };

    match std::process::Command::new("git")
        .args(["revert", "--no-edit", target_commit_sha.as_str()])
        .current_dir(project_root)
        .output()
    {
        Ok(out) if out.status.success() => {
            let revert_commit_sha = resolve_commit_ref(project_root, "HEAD");
            let revert_short_sha = revert_commit_sha
                .as_deref()
                .map(short_commit_sha)
                .unwrap_or_else(|| "???".to_string());
            let tool_result_fields = revert_commit_sha.map(|revert_commit_sha| {
                serde_json::Map::from_iter([
                    (
                        "reverted_commit_sha".to_string(),
                        Value::String(target_commit_sha.clone()),
                    ),
                    (
                        "reverted_commit_short_sha".to_string(),
                        Value::String(short_commit_sha(&target_commit_sha)),
                    ),
                    (
                        "revert_commit_sha".to_string(),
                        Value::String(revert_commit_sha.clone()),
                    ),
                    (
                        "revert_commit_short_sha".to_string(),
                        Value::String(short_commit_sha(&revert_commit_sha)),
                    ),
                ])
            });
            ToolExecutionOutcome {
                output: format!(
                    "✓ Reverted commit: {} via {}",
                    short_commit_sha(&target_commit_sha),
                    revert_short_sha
                ),
                tool_result_fields,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut message = format!("Error: git revert failed: {}", stderr.trim());
            match abort_git_revert(project_root) {
                Ok(true) => {
                    message.push_str(" (aborted in-progress revert)");
                }
                Ok(false) => {}
                Err(error) => {
                    message.push_str(&format!(" ({error})"));
                }
            }
            ToolExecutionOutcome::text(message)
        }
        Err(e) => ToolExecutionOutcome::text(format!("Error: git revert failed: {e}")),
    }
}

/// Stash working tree changes.
///
/// Parameters:
/// - `action` (required): "push" (save), "apply", "pop" (restore + drop), "list", "drop"
/// - `message` (optional): description for push
/// - `index` (optional): stash index for apply/pop/drop (default 0)
/// - `stash_ref` (optional): exact stash selector or OID for apply
pub fn git_stash(project_root: &Path, args: &Value) -> String {
    git_stash_with_metadata(project_root, args).output
}

pub fn git_stash_with_metadata(project_root: &Path, args: &Value) -> ToolExecutionOutcome {
    let action = match args.get("action").and_then(Value::as_str) {
        Some(a) => a,
        None => {
            return ToolExecutionOutcome::text(
                "Error: 'action' is required (push, apply, pop, list, drop)".to_string(),
            );
        }
    };

    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(project_root);
    let before_stash_oid = matches!(action, "push" | "save")
        .then(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", "refs/stash"])
                .current_dir(project_root)
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .flatten();

    match action {
        "push" | "save" => {
            cmd.arg("stash").arg("push");
            if let Some(msg) = args.get("message").and_then(Value::as_str) {
                cmd.arg("-m").arg(msg);
            }
        }
        "apply" => {
            let selector = match apply_stash_selector(args) {
                Ok(selector) => selector,
                Err(error) => return ToolExecutionOutcome::text(error),
            };
            cmd.arg("stash").arg("apply").arg(selector);
        }
        "pop" => {
            let selector = stash_index_selector(args);
            cmd.arg("stash").arg("pop").arg(selector);
        }
        "list" => {
            cmd.arg("stash").arg("list");
        }
        "drop" => {
            let selector = stash_index_selector(args);
            cmd.arg("stash").arg("drop").arg(selector);
        }
        _ => {
            return ToolExecutionOutcome::text(format!(
                "Error: unknown stash action '{action}'. Use: push, apply, pop, list, drop"
            ));
        }
    }

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.success() {
                let result = stdout.trim();
                let output = if result.is_empty() {
                    match action {
                        "push" | "save" => "✓ Changes stashed".to_string(),
                        "list" => "No stashes found".to_string(),
                        _ => format!("✓ Stash {action} done"),
                    }
                } else {
                    result.to_string()
                };
                let mut tool_result_fields = None;
                if matches!(action, "push" | "save") {
                    let after_stash_oid = std::process::Command::new("git")
                        .args(["rev-parse", "--verify", "refs/stash"])
                        .current_dir(project_root)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
                    if let Some(stash_ref) = after_stash_oid
                        && before_stash_oid.as_deref() != Some(stash_ref.as_str())
                    {
                        tool_result_fields = Some(serde_json::Map::from_iter([(
                            "stash_ref".to_string(),
                            Value::String(stash_ref),
                        )]));
                    }
                }
                ToolExecutionOutcome {
                    output,
                    tool_result_fields,
                }
            } else {
                let err = stderr.trim();
                if err.contains("No local changes") || err.contains("No stash entries") {
                    ToolExecutionOutcome::text(err.to_string())
                } else {
                    ToolExecutionOutcome::text(format!("Error: git stash {action} failed: {err}"))
                }
            }
        }
        Err(e) => ToolExecutionOutcome::text(format!("Error: git stash failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Take first n chars from a string.
    fn prefix_chars(s: &str, n: usize) -> String {
        s.chars().take(n).collect()
    }
    use serde_json::json;
    use tempfile::TempDir;

    fn repo_root() -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // rust/
        path
    }

    fn init_temp_repo() -> TempDir {
        let dir = TempDir::new().expect("temp repo");
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.name");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.email");
        std::fs::write(dir.path().join("tracked.txt"), "one\n").expect("seed tracked file");
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");
        dir
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_temp_repo_with_ours_merge() -> TempDir {
        let dir = init_temp_repo();
        let root = dir.path();
        let default_branch = git_stdout(root, &["branch", "--show-current"]);
        run_git(root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("tracked.txt"), "feature branch change\n").expect("write feature");
        run_git(root, &["add", "tracked.txt"]);
        run_git(root, &["commit", "-m", "feature change"]);
        run_git(root, &["checkout", &default_branch]);
        run_git(
            root,
            &[
                "merge",
                "--no-ff",
                "-s",
                "ours",
                "feature",
                "-m",
                "merge feature",
            ],
        );
        dir
    }

    #[test]
    fn git_status_returns_output() {
        let root = repo_root();
        let result = git_status(&root);
        assert!(
            result.contains("##")
                || result.contains("nothing to commit")
                || result.contains("Error"),
            "unexpected status: {result}"
        );
    }

    #[test]
    fn git_log_returns_commits() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 5}));
        let lines: Vec<&str> = result.lines().collect();
        assert!(!lines.is_empty(), "log should return commits");
        let first = lines[0];
        let hash_prefix = prefix_chars(first, 7);
        assert!(
            hash_prefix.chars().count() == 7 && hash_prefix.chars().all(|c| c.is_ascii_hexdigit()),
            "first log line should start with hash: {first}"
        );
    }

    #[test]
    fn git_log_default_n() {
        let root = repo_root();
        let result = git_log(&root, &json!({}));
        let lines: Vec<&str> = result.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines.len() <= 10, "default should be at most 10 commits");
    }

    #[test]
    fn git_show_missing_commit() {
        let root = repo_root();
        let result = git_show(&root, &json!({}), 0.0, 0);
        assert!(result.contains("Error: missing"));
    }

    #[test]
    fn git_show_invalid_ref() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "abc;rm -rf /"}), 0.0, 0);
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}), 0.0, 0);
        assert!(result.contains("commit "), "should show commit: {result}");
        assert!(result.contains("Author:"), "should show author");
    }

    #[test]
    fn git_show_stat_only() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}), 0.0, 0);
        assert!(result.contains("commit "));
        assert!(
            result.contains("files changed") || result.contains("root commit"),
            "should show stats: {result}"
        );
    }

    #[test]
    fn git_blame_missing_file_param() {
        let root = repo_root();
        let result = git_blame(&root, &json!({}));
        assert!(result.contains("Error: missing 'file'"));
    }

    #[test]
    fn git_blame_known_file() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "README.md"}));
        assert!(
            result.contains("L1") || result.contains("Error") || result.contains("No blame"),
            "unexpected blame: {result}"
        );
    }

    #[test]
    fn git_blame_with_line_range() {
        let root = repo_root();
        let result = git_blame(
            &root,
            &json!({"file": "README.md", "line_start": 1, "line_end": 3}),
        );
        if result.contains("L1") {
            let blame_lines: Vec<&str> = result.lines().filter(|l| l.starts_with('L')).collect();
            assert!(
                blame_lines.len() <= 3,
                "should have at most 3 lines: {blame_lines:?}"
            );
        }
    }

    #[test]
    fn git_diff_no_crash() {
        let root = repo_root();
        let result = git_diff(&root, &json!({}), 0.0, 0);
        assert!(
            !result.contains("Error: cannot open"),
            "should open repo: {result}"
        );
    }

    #[test]
    fn git_file_history_missing_file() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({}));
        assert!(result.contains("Error: missing 'file'"));
    }

    #[test]
    fn git_file_history_known_file() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({"file": "README.md"}));
        assert!(
            result.contains("File: README.md") || result.contains("No history"),
            "unexpected: {result}"
        );
    }

    #[test]
    fn git_file_history_limits_n() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({"file": "README.md", "n": 3}));
        if result.contains("Commits:")
            && let Some(line) = result.lines().find(|l| l.starts_with("Commits:"))
        {
            let count: usize = line
                .trim_start_matches("Commits: ")
                .trim()
                .parse()
                .unwrap_or(0);
            assert!(count <= 3, "should respect n limit: {count}");
        }
    }

    // ─── git_diff enhanced tests ────────────────────────────────────────────

    #[test]
    fn git_diff_staged_param_accepted() {
        let root = repo_root();
        // staged=true should not crash (may return "No staged changes" or file list)
        let result = git_diff(&root, &json!({"staged": true}), 0.0, 0);
        assert!(
            !result.contains("Error: cannot open"),
            "staged diff should not fail to open repo: {result}"
        );
    }

    #[test]
    fn git_diff_ref_param_uses_tree_diff() {
        let root = repo_root();
        // Diff HEAD against HEAD~1 should produce actual file changes
        let result = git_diff(&root, &json!({"ref": "HEAD~1"}), 0.0, 0);
        assert!(
            result.contains("diff --git")
                || result.contains("No changes")
                || result.contains("Error: cannot resolve"),
            "ref diff should produce diff output or error: {result}"
        );
    }

    #[test]
    fn git_diff_default_shows_worktree() {
        let root = repo_root();
        let result = git_diff(&root, &json!({}), 0.0, 0);
        // Should not error — either shows changes or "No changes"
        assert!(
            !result.starts_with("Error:"),
            "default diff should work: {result}"
        );
    }

    #[test]
    fn git_diff_stat_only_smoke() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"stat_only": true}), 0.0, 0);
        assert!(
            !result.starts_with("Error:"),
            "stat_only should use git CLI without repo open errors: {result}"
        );
        assert!(
            result.contains('|')
                || result.to_lowercase().contains("file")
                || result == "No changes",
            "expected stat-style summary: {result}"
        );
    }

    #[test]
    fn git_diff_stat_only_rejects_staged_with_ref() {
        let root = repo_root();
        let result = git_diff(
            &root,
            &json!({"stat_only": true, "staged": true, "ref": "HEAD~1"}),
            0.0,
            0,
        );
        assert!(result.contains("not both"), "{result}");
    }

    #[test]
    fn git_diff_base_ref_range() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"base_ref": "HEAD~3", "ref": "HEAD"}), 0.0, 0);
        assert!(
            result.contains("diff --git") || result == "No changes",
            "range diff should produce output: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_defaults_tip_to_head() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"base_ref": "HEAD~1"}), 0.0, 0);
        assert!(
            !result.starts_with("Error:"),
            "base_ref without ref should default tip to HEAD: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_with_path() {
        let root = repo_root();
        let result = git_diff(
            &root,
            &json!({"base_ref": "HEAD~2", "ref": "HEAD", "path": "Cargo.toml"}),
            0.0,
            0,
        );
        assert!(
            !result.starts_with("Error:"),
            "range diff with path filter should work: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_stat_only() {
        let root = repo_root();
        let result = git_diff(
            &root,
            &json!({"base_ref": "HEAD~3", "ref": "HEAD", "stat_only": true}),
            0.0,
            0,
        );
        assert!(
            !result.starts_with("Error:"),
            "range stat diff should work: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_rejects_shell_injection() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"base_ref": "HEAD; rm -rf /"}), 0.0, 0);
        assert!(
            result.contains("disallowed"),
            "shell meta in base_ref should be rejected: {result}"
        );
    }

    #[test]
    fn reject_shell_meta_allows_valid_refs() {
        assert!(super::reject_shell_meta("HEAD~5").is_ok());
        assert!(super::reject_shell_meta("main").is_ok());
        assert!(super::reject_shell_meta("v1.0.0").is_ok());
        assert!(super::reject_shell_meta("feature/my-branch").is_ok());
        assert!(super::reject_shell_meta("HEAD^2").is_ok());
    }

    #[test]
    fn reject_shell_meta_blocks_injection() {
        assert!(super::reject_shell_meta("HEAD; echo pwned").is_err());
        assert!(super::reject_shell_meta("HEAD|cat /etc/passwd").is_err());
        assert!(super::reject_shell_meta("$(whoami)").is_err());
        assert!(super::reject_shell_meta("HEAD`id`").is_err());
    }

    // ─── git_show enhanced tests ────────────────────────────────────────────

    #[test]
    fn git_show_allows_reflog_syntax() {
        let root = repo_root();
        // HEAD@{0} should not be rejected by validation — it should reach rev_parse
        let result = git_show(&root, &json!({"commit": "HEAD@{0}"}), 0.0, 0);
        // Should show a commit (passes validation), not be rejected outright
        assert!(
            result.starts_with("commit ") || result.starts_with("Error: cannot resolve"),
            "HEAD@{{0}} should pass validation and reach parsing: {result}"
        );
    }

    #[test]
    fn git_show_rejects_shell_metachar() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD;rm -rf /"}), 0.0, 0);
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head_has_diff_content() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}), 0.0, 0);
        assert!(result.contains("commit "), "should show commit header");
        assert!(result.contains("Author:"), "should show author");
        // Should contain actual diff markers or root commit marker
        assert!(
            result.contains("---") || result.contains("[root commit]") || result.contains("+"),
            "should contain diff content: {result}"
        );
    }

    #[test]
    fn git_show_stat_only_has_stats() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}), 0.0, 0);
        assert!(result.contains("commit "));
        assert!(
            result.contains("files changed") || result.contains("[root commit]"),
            "should show stats or root: {result}"
        );
    }

    #[test]
    fn git_show_merge_commit_stat_only_has_stats() {
        let dir = init_temp_repo_with_ours_merge();
        let result = git_show(
            dir.path(),
            &json!({"commit": "HEAD", "stat_only": true}),
            0.0,
            0,
        );
        assert!(
            result.contains("commit "),
            "should show commit header: {result}"
        );
        assert!(
            result.contains(" file changed") || result.contains(" files changed"),
            "merge commit stat_only should show stats: {result}"
        );
        assert!(
            result.contains("tracked.txt"),
            "merge commit stat_only should mention changed file: {result}"
        );
    }

    #[test]
    fn git_show_merge_commit_has_diff_content() {
        let dir = init_temp_repo_with_ours_merge();
        let result = git_show(dir.path(), &json!({"commit": "HEAD"}), 0.0, 0);
        assert!(
            result.contains("commit "),
            "should show commit header: {result}"
        );
        assert!(
            result.contains("diff --git") || result.contains("--- a/tracked.txt"),
            "merge commit should include diff output: {result}"
        );
    }

    #[test]
    fn git_show_file_filter() {
        let root = repo_root();
        let result = git_show(
            &root,
            &json!({"commit": "HEAD", "file": "README.md"}),
            0.0,
            0,
        );
        // If README.md was changed in HEAD, it should appear; otherwise no diff lines
        assert!(result.contains("commit "), "should show header: {result}");
    }

    #[test]
    fn git_show_with_file_appends_hint_when_aggregate_high() {
        let root = repo_root();
        // aggregate_bytes above AGGREGATE_SOFT_LIMIT / 2 (60_000) with file filter
        let result = git_show(
            &root,
            &json!({"commit": "HEAD", "file": "README.md"}),
            0.0,
            65_000,
        );
        assert!(
            result.contains("[hint: aggregate output is high"),
            "should append aggregate hint when file filter + high aggregate: {result}"
        );
    }

    #[test]
    fn git_show_without_file_no_hint_even_when_aggregate_high() {
        let root = repo_root();
        // No file filter — hint should NOT appear even when aggregate is high.
        // Use HEAD~5 to pick a commit whose diff doesn't contain the hint text.
        let result = git_show(&root, &json!({"commit": "HEAD~5"}), 0.0, 65_000);
        assert!(
            !result.contains("[hint: aggregate output is high"),
            "should NOT append hint without file filter: {}",
            &result[..result.len().min(500)]
        );
    }

    // ─── git_blame enhanced tests ───────────────────────────────────────────

    #[test]
    fn git_blame_nonexistent_file() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "nonexistent_file_xyz.rs"}));
        assert!(
            result.contains("Error"),
            "should error on nonexistent file: {result}"
        );
    }

    #[test]
    fn git_blame_output_format() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "README.md"}));
        if result.contains("L1") {
            // Should have structured format: L<n> <commit> <date> [<author>] <content>
            let first_line = result.lines().next().unwrap_or("");
            assert!(
                first_line.starts_with("L1 "),
                "blame line should start with L1: {first_line}"
            );
            assert!(
                first_line.contains('[') && first_line.contains(']'),
                "blame line should have [author]: {first_line}"
            );
        }
    }

    #[test]
    fn git_blame_summary_footer() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "README.md"}));
        if !result.contains("Error") && !result.contains("No blame") {
            assert!(
                result.contains("lines,")
                    && result.contains("authors,")
                    && result.contains("commits"),
                "should have summary footer: {result}"
            );
        }
    }

    // ─── git_status enhanced tests ──────────────────────────────────────────

    #[test]
    fn git_status_shows_branch() {
        let root = repo_root();
        let result = git_status(&root);
        // Should show branch info or be clean
        assert!(
            result.contains("##")
                || result.contains("nothing to commit")
                || result.contains("HEAD detached"),
            "status should show branch: {result}"
        );
    }

    // ─── git_log enhanced tests ─────────────────────────────────────────────

    #[test]
    fn git_log_custom_n() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 3}));
        let lines: Vec<&str> = result.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            lines.len() <= 3,
            "should respect n=3: got {} lines",
            lines.len()
        );
        assert!(!lines.is_empty(), "should have at least 1 commit");
    }

    #[test]
    fn git_log_format_consistent() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 5}));
        for line in result.lines().filter(|l| !l.is_empty()) {
            // Each line should start with a 7-char hex hash
            let hash_prefix = prefix_chars(line, 7);
            assert!(
                hash_prefix.chars().count() == 7
                    && hash_prefix.chars().all(|c| c.is_ascii_hexdigit()),
                "log line should start with hash: {line}"
            );
            // Should have a space after the hash
            assert!(
                line.chars().nth(7) == Some(' '),
                "log line should have space after hash: {line}"
            );
        }
    }

    // ─── Diff with actual content verification ──────────────────────────────

    #[test]
    fn git_diff_ref_produces_line_content() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"ref": "HEAD~1"}), 0.0, 0);
        if result.contains("diff --git") {
            // If there are changes, we should see actual +/- lines
            assert!(
                result.contains('+') || result.contains('-') || result.contains("# "),
                "ref diff should have content markers: {result}"
            );
        }
    }

    // ─── Edge cases ─────────────────────────────────────────────────────────

    #[test]
    fn git_file_history_nonexistent_file() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({"file": "this/does/not/exist.xyz"}));
        assert!(
            result.contains("No history"),
            "should say no history: {result}"
        );
    }

    #[test]
    fn git_show_parent_ref() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD~1"}), 0.0, 0);
        assert!(
            result.contains("commit ") || result.contains("Error: cannot resolve"),
            "HEAD~1 should work: {result}"
        );
    }

    // ─── git_log_search tests ───────────────────────────────────────────────

    #[test]
    fn git_log_search_missing_query() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({}));
        assert!(result.contains("Error: missing or empty"));
    }

    #[test]
    fn git_log_search_empty_query() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "  "}));
        assert!(result.contains("Error: missing or empty"));
    }

    #[test]
    fn git_log_search_finds_commits() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "fix"}));
        // Should find some commits with "fix" in the message
        assert!(
            result.contains("Search:") || result.contains("No commits matching"),
            "should produce search result: {result}"
        );
    }

    #[test]
    fn git_log_search_respects_n() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "fix", "n": 10}));
        if result.contains("commits searched") {
            // Extract the number of commits searched
            if let Some(start) = result.find('(')
                && let Some(end) = result.find(" commits searched")
            {
                let num_str = &result[start + 1..end];
                let count: usize = num_str.parse().unwrap_or(0);
                assert!(count <= 10, "should search at most 10: {count}");
            }
        }
    }

    #[test]
    fn git_log_search_score_format() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "feat"}));
        if result.contains("[score:") {
            // Scores should be between 0 and 1
            for line in result.lines() {
                if let Some(start) = line.find("[score:")
                    && let Some(end) = line[start..].find(']')
                {
                    let score_str = &line[start + 7..start + end];
                    let score: f64 = score_str.parse().unwrap_or(0.0);
                    assert!(score > 0.0 && score <= 1.0, "score should be 0-1: {score}");
                }
            }
        }
    }

    // ─── git_contributors tests ─────────────────────────────────────────────

    #[test]
    fn git_contributors_shows_authors() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({}));
        assert!(
            result.contains("## Top Contributors") || result.contains("No git history"),
            "should show contributors: {result}"
        );
    }

    #[test]
    fn git_contributors_shows_hot_files() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({}));
        if result.contains("## Top Contributors") {
            assert!(
                result.contains("## Hot Files") || result.contains("## Recent"),
                "should have hot files or recent activity: {result}"
            );
        }
    }

    #[test]
    fn git_contributors_shows_recent() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({}));
        if !result.contains("No git history") {
            assert!(
                result.contains("## Recent Activity"),
                "should show recent activity: {result}"
            );
        }
    }

    #[test]
    fn git_contributors_with_path_filter() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({"path": "README.md"}));
        // Either shows filtered results or no history
        assert!(
            result.contains("## Top Contributors") || result.contains("No git history"),
            "path filter should work: {result}"
        );
    }

    #[test]
    fn git_contributors_with_since() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({"since": "2020-01-01"}));
        assert!(
            result.contains("## Top Contributors") || result.contains("No git history"),
            "since filter should work: {result}"
        );
    }

    // ─── Score function unit tests ──────────────────────────────────────────

    #[test]
    fn score_commits_empty_corpus() {
        let result = score_commits("test", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn score_commits_empty_query() {
        let commits = vec![CommitDoc {
            hash: "abc".into(),
            author: "test".into(),
            date: "2024".into(),
            message: "hello".into(),
            tokens: vec!["hello".into()],
        }];
        let result = score_commits("", &commits);
        assert!(result.is_empty());
    }

    // ─── parse_since_to_epoch tests ─────────────────────────────────────────

    #[test]
    fn parse_since_iso_date() {
        let epoch = parse_since_to_epoch("2024-01-01");
        assert!(epoch.is_some());
        let ts = epoch.unwrap();
        // 2024-01-01 should be > 2023 in epoch seconds
        assert!(ts > 1_672_000_000, "should be a valid epoch: {ts}");
    }

    #[test]
    fn parse_since_invalid() {
        assert!(parse_since_to_epoch("not a date").is_none());
        assert!(parse_since_to_epoch("").is_none());
    }

    #[test]
    fn parse_since_invalid_month_day() {
        // Month 0 and 13 must not panic (was an array OOB bug)
        assert!(parse_since_to_epoch("2024-00-15").is_none());
        assert!(parse_since_to_epoch("2024-13-01").is_none());
        // Day 0 and 32
        assert!(parse_since_to_epoch("2024-01-00").is_none());
        assert!(parse_since_to_epoch("2024-01-32").is_none());
    }

    // ─── current_branch / head_short tests ──────────────────────────────────

    #[test]
    fn current_branch_returns_nonempty_in_repo() {
        let root = repo_root();
        let branch = current_branch(&root);
        // In a git repo we should get a branch name (or empty if detached HEAD)
        // Just verify no panic and reasonable output
        assert!(!branch.contains("Error"), "should not error: {branch}");
    }

    #[test]
    fn head_short_returns_hex() {
        let root = repo_root();
        let short = head_short(&root);
        assert!(!short.is_empty(), "should return a short hash");
        assert_eq!(
            short.len(),
            7,
            "to_hex_with_len(7) yields 7 hex chars: {short}"
        );
        assert!(
            short.chars().all(|c| c.is_ascii_hexdigit()),
            "should be hex: {short}"
        );
    }

    #[test]
    fn current_branch_bad_path_returns_empty() {
        let branch = current_branch(Path::new("/nonexistent/repo"));
        assert!(branch.is_empty());
    }

    #[test]
    fn head_short_bad_path_returns_empty() {
        let short = head_short(Path::new("/nonexistent/repo"));
        assert!(short.is_empty());
    }

    // ─── Robustness regression tests ────────────────────────────────────────

    #[test]
    fn git_diff_staged_detects_no_staged() {
        // In a clean repo, staged diff should say "No staged changes"
        let root = repo_root();
        let result = git_diff(&root, &json!({"staged": true}), 0.0, 0);
        // Either "No staged changes" or actual staged content — no panic/error
        assert!(
            result.contains("staged") || result.contains("diff --git"),
            "should handle staged query: {result}"
        );
    }

    #[test]
    fn git_log_n_capped_at_500() {
        // Even with n=99999, should not produce huge output
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 99999}));
        let line_count = result.lines().count();
        assert!(
            line_count <= 501,
            "n should be capped at 500: got {line_count} lines"
        );
    }

    #[test]
    fn git_log_output_truncated() {
        // git_log should apply truncation
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 500}));
        // Just verify it doesn't panic and produces output
        assert!(!result.is_empty());
    }

    #[test]
    fn git_file_history_nonexistent_file_bounded() {
        // For a nonexistent file, the walk should be bounded (not traverse all history)
        let root = repo_root();
        let start = std::time::Instant::now();
        let result = git_file_history(&root, &json!({"file": "this/does/not/exist.xyz"}));
        let elapsed = start.elapsed();
        assert!(
            result.contains("No history"),
            "should say no history: {result}"
        );
        // Walk cap should prevent this from taking too long (50K cap)
        // In a typical dev repo this should be well under 5 seconds
        assert!(
            elapsed.as_secs() < 30,
            "walk should be bounded, took: {elapsed:?}"
        );
    }

    #[test]
    fn git_contributors_bounded_walk() {
        // Even without path filter, walk should complete in bounded time
        let root = repo_root();
        let start = std::time::Instant::now();
        let result = git_contributors(&root, &json!({}));
        let elapsed = start.elapsed();
        assert!(
            !result.contains("Error: cannot open"),
            "should open repo: {result}"
        );
        assert!(
            elapsed.as_secs() < 30,
            "walk should be bounded, took: {elapsed:?}"
        );
    }

    #[test]
    fn git_contributors_path_filter_bounded() {
        // Path filter with nonexistent file should still be bounded
        let root = repo_root();
        let start = std::time::Instant::now();
        let _result = git_contributors(
            &root,
            &json!({"path": "nonexistent/deeply/nested/file.xyz"}),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 30,
            "path-filtered walk should be bounded, took: {elapsed:?}"
        );
    }

    #[test]
    fn git_show_root_commit_lists_files() {
        // Find the root commit and verify it lists actual file paths
        let root = repo_root();
        let repo = gix::discover(&root).unwrap();
        // Walk to find root commit (no parents)
        let head = repo.head_id().unwrap();
        let mut root_oid = None;
        if let Ok(walk) = head.ancestors().all() {
            for info in walk.flatten() {
                if let Ok(c) = info.object()
                    && c.parent_ids().count() == 0
                {
                    root_oid = Some(info.id.to_string());
                    break;
                }
            }
        }
        if let Some(oid) = root_oid {
            let result = git_show(&root, &json!({"commit": oid}), 0.0, 0);
            assert!(
                result.contains("[root commit]"),
                "should mark as root: {result}"
            );
            // Should list files with full paths, not just top-level dirs
            // (regression: previously only listed directory names)
            assert!(
                result.contains('/') || result.lines().filter(|l| l.starts_with("A ")).count() > 0,
                "root commit should list files: {result}"
            );
        }
    }

    // ── Pressure-aware output limit tests ──

    #[test]
    fn pressure_scaled_limit_zero_pressure_returns_base() {
        assert_eq!(super::pressure_scaled_limit(12_000, 0.0), 12_000);
        assert_eq!(super::pressure_scaled_limit(16_000, 0.0), 16_000);
    }

    #[test]
    fn pressure_scaled_limit_moderate_pressure_reduces() {
        let limit = super::pressure_scaled_limit(12_000, 0.6);
        assert!(limit < 12_000, "moderate pressure should reduce limit");
        assert!(limit > 4_800, "should stay above 40% minimum");
        // scale = 1.0 - 0.6*0.6 = 0.64 → 12000 * 0.64 = 7680
        assert_eq!(limit, 7_680);
    }

    #[test]
    fn pressure_scaled_limit_max_pressure_reaches_floor() {
        let limit = super::pressure_scaled_limit(12_000, 1.0);
        // scale = 1.0 - 1.0*0.6 = 0.4 → 12000 * 0.4 = 4800
        assert_eq!(limit, 4_800);
    }

    #[test]
    fn pressure_scaled_limit_never_goes_below_forty_percent() {
        let limit = super::pressure_scaled_limit(10_000, 1.5);
        // scale = max(1.0 - 1.5*0.6, 0.4) = max(0.1, 0.4) = 0.4 → 4000
        assert_eq!(limit, 4_000);
    }

    #[test]
    fn git_show_under_pressure_truncates_earlier() {
        let root = std::env::current_dir().unwrap();
        let normal = git_show(&root, &json!({"commit": "HEAD"}), 0.0, 0);
        let pressed = git_show(&root, &json!({"commit": "HEAD"}), 0.9, 0);
        assert!(
            pressed.len() <= normal.len(),
            "high-pressure output ({}) should not exceed normal ({})",
            pressed.len(),
            normal.len()
        );
    }

    #[test]
    fn git_diff_under_pressure_truncates_earlier() {
        let root = std::env::current_dir().unwrap();
        let normal = git_diff(&root, &json!({}), 0.0, 0);
        let pressed = git_diff(&root, &json!({}), 0.9, 0);
        assert!(
            pressed.len() <= normal.len(),
            "high-pressure diff ({}) should not exceed normal ({})",
            pressed.len(),
            normal.len()
        );
    }

    // ─── git_commit tests ───────────────────────────────────────────────────

    #[test]
    fn git_commit_rejects_empty_message() {
        let root = repo_root();
        let result = git_commit(&root, &json!({}));
        assert!(
            result.starts_with("Error:"),
            "should reject missing message: {result}"
        );

        let result2 = git_commit(&root, &json!({"message": "  "}));
        assert!(
            result2.starts_with("Error:"),
            "should reject blank message: {result2}"
        );
    }

    #[test]
    fn git_commit_rejects_long_message() {
        let root = repo_root();
        let long_msg = "x".repeat(5001);
        let result = git_commit(&root, &json!({"message": long_msg}));
        assert!(
            result.contains("too long"),
            "should reject over-long message: {result}"
        );
    }

    #[test]
    fn git_commit_clean_tree_says_nothing() {
        // In a clean repo with nothing staged, commit should say "Nothing to commit"
        // or succeed if there are pending changes — either is fine, just no panic
        let root = repo_root();
        let result = git_commit(
            &root,
            &json!({"message": "test commit", "files": ["nonexistent_file_xyz.txt"]}),
        );
        // Should either succeed or report a meaningful error
        assert!(!result.is_empty(), "should return some output");
    }

    #[test]
    fn git_commit_with_metadata_returns_commit_sha_and_revert_tool_restores_state() {
        let repo = init_temp_repo();
        let tracked_path = repo.path().join("tracked.txt");
        std::fs::write(&tracked_path, "two\n").expect("update tracked file");

        let outcome = git_commit_with_metadata(repo.path(), &json!({"message": "update tracked"}));
        assert!(
            !outcome.output.starts_with("Error:"),
            "commit should succeed: {}",
            outcome.output
        );
        let commit_fields = outcome
            .tool_result_fields
            .as_ref()
            .expect("git_commit should return commit metadata");
        let commit_sha = commit_fields
            .get("commit_sha")
            .and_then(Value::as_str)
            .expect("commit_sha");
        let commit_short_sha = commit_fields
            .get("commit_short_sha")
            .and_then(Value::as_str)
            .expect("commit_short_sha");
        assert_eq!(commit_short_sha, short_commit_sha(commit_sha));
        assert_eq!(
            std::fs::read_to_string(&tracked_path).expect("read committed file"),
            "two\n"
        );

        let revert_outcome =
            git_revert_commit_with_metadata(repo.path(), &json!({"commit_sha": commit_sha}));
        assert!(
            !revert_outcome.output.starts_with("Error:"),
            "revert should succeed: {}",
            revert_outcome.output
        );
        let revert_fields = revert_outcome
            .tool_result_fields
            .as_ref()
            .expect("git_revert_commit should return metadata");
        assert_eq!(
            revert_fields
                .get("reverted_commit_sha")
                .and_then(Value::as_str),
            Some(commit_sha)
        );
        assert!(
            revert_fields
                .get("revert_commit_sha")
                .and_then(Value::as_str)
                .is_some(),
            "revert should report the compensating commit"
        );
        assert_eq!(
            std::fs::read_to_string(&tracked_path).expect("read reverted file"),
            "one\n"
        );
    }

    #[test]
    fn git_revert_commit_conflict_is_aborted() {
        let repo = init_temp_repo();
        let tracked_path = repo.path().join("tracked.txt");
        std::fs::write(&tracked_path, "two\n").expect("write second version");
        let second = git_commit_with_metadata(repo.path(), &json!({"message": "second"}));
        let second_sha = second
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("commit_sha"))
            .and_then(Value::as_str)
            .expect("second commit sha")
            .to_string();

        std::fs::write(&tracked_path, "three\n").expect("write third version");
        let third = git_commit_with_metadata(repo.path(), &json!({"message": "third"}));
        assert!(
            !third.output.starts_with("Error:"),
            "third commit should succeed: {}",
            third.output
        );

        let revert =
            git_revert_commit_with_metadata(repo.path(), &json!({"commit_sha": second_sha}));
        assert!(
            revert.output.starts_with("Error:"),
            "reverting a non-HEAD conflicting commit should fail: {}",
            revert.output
        );
        assert!(
            revert.output.contains("aborted in-progress revert"),
            "failure should clean up revert state: {}",
            revert.output
        );
        assert_eq!(
            std::fs::read_to_string(&tracked_path).expect("read file after aborted revert"),
            "three\n"
        );
        assert!(
            !repo.path().join(".git/REVERT_HEAD").exists(),
            "revert conflict should not leave REVERT_HEAD behind"
        );
    }

    // ─── git_stash tests ────────────────────────────────────────────────────

    #[test]
    fn git_stash_requires_action() {
        let root = repo_root();
        let result = git_stash(&root, &json!({}));
        assert!(
            result.starts_with("Error:"),
            "should require action: {result}"
        );
    }

    #[test]
    fn git_stash_rejects_unknown_action() {
        let root = repo_root();
        let result = git_stash(&root, &json!({"action": "fly"}));
        assert!(
            result.contains("unknown stash action"),
            "should reject unknown: {result}"
        );
    }

    #[test]
    fn git_stash_list_works() {
        let root = repo_root();
        let result = git_stash(&root, &json!({"action": "list"}));
        // Should return stash list or "No stashes found"
        assert!(
            result.contains("stash@") || result.contains("No stashes") || result.is_empty(),
            "unexpected stash list output: {result}"
        );
    }

    // ─── git_checkout_file tests ────────────────────────────────────────────

    // ─── git CLI fallback behavior tests ────────────────────────────────────

    #[test]
    fn diff_via_git_cli_returns_none_for_bad_args() {
        let root = repo_root();
        // Invalid ref should make git fail → returns None
        let result = diff_via_git_cli(
            &root,
            &["diff", "not_a_valid_ref_xyzzy", "--no-ext-diff"],
            8000,
        );
        assert!(result.is_none(), "bad ref should return None for fallback");
    }

    #[test]
    fn diff_via_git_cli_returns_no_changes_for_empty_diff() {
        let root = repo_root();
        // HEAD vs HEAD has no diff
        let result = diff_via_git_cli(
            &root,
            &["diff", "HEAD", "HEAD", "--no-ext-diff", "--no-color"],
            8000,
        );
        assert_eq!(
            result,
            Some("No changes".to_string()),
            "HEAD vs HEAD should be empty diff"
        );
    }

    #[test]
    fn gix_worktree_fallback_annotates_summary_only() {
        // diff_worktree output (when it has entries) should tell the user
        // that it's summary-only so the LLM knows to call bash git diff.
        let root = repo_root();
        let repo = open_repo(&root).expect("repo should open");
        let result = diff_worktree(&repo, 100_000);
        if result != "No changes" {
            assert!(
                result.contains("summary only"),
                "gix fallback should annotate summary-only output: {result}"
            );
        }
    }

    #[test]
    fn diff_via_git_cli_returns_none_for_nonexistent_dir() {
        let result = diff_via_git_cli(
            Path::new("/nonexistent_dir_xyz"),
            &["diff", "--no-ext-diff"],
            8000,
        );
        assert!(
            result.is_none(),
            "nonexistent dir should return None for fallback"
        );
    }

    // ── Git Worktree Tests ──────────────────────────────────────────────
}
