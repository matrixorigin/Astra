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

use super::{ToolExecutor, truncate_output};

const DIFF_LIMIT: usize = 40_000; // ~10K tokens — diff is the primary input for code review
const SHOW_LIMIT: usize = 16_000; // ~4K tokens; was 30K

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
pub(crate) struct GitStashRollbackEntry {
    sequence: u64,
    pub stash_ref: String,
    pub turn_index: u32,
    pub timestamp: SystemTime,
    pub message: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct GitStashRollbackJournal {
    entries: Vec<GitStashRollbackEntry>,
    next_sequence: u64,
}

impl GitStashRollbackJournal {
    fn record(&mut self, stash_ref: impl Into<String>, turn_index: u32, message: Option<String>) {
        self.entries.push(GitStashRollbackEntry {
            sequence: self.next_sequence,
            stash_ref: stash_ref.into(),
            turn_index,
            timestamp: SystemTime::now(),
            message,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    fn list(&self) -> Vec<GitStashRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitStashRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    fn restore_plan_for_turn_since(
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

    fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    fn remove_stash(&mut self, stash_ref: &str) -> bool {
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
pub(crate) fn current_branch(project_root: &Path) -> String {
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
pub(crate) fn head_short(project_root: &Path) -> String {
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

pub(crate) fn git_status(project_root: &Path) -> String {
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

pub(crate) fn git_log(project_root: &Path, args: &Value) -> String {
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

pub(crate) fn git_show(
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

    // Merge commits: gix first-parent diff produces useless tree-level output.
    // Fall back to `git show --first-parent` via CLI for an actual code diff.
    let is_merge = commit.parent_ids().count() > 1;
    if is_merge {
        let mut cli_args = vec!["show", "--first-parent", "--no-ext-diff", "--no-color"];
        if stat_only {
            cli_args.push("--stat");
        } else {
            cli_args.push("-p");
        }
        // commit_ref is already validated above
        let cli_ref = commit_ref.to_string();
        cli_args.push(&cli_ref);
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
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                if !stdout.trim().is_empty() {
                    return truncate_show_at(stdout, limit);
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

    truncate_show_at(out, limit)
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

pub(crate) fn git_blame(project_root: &Path, args: &Value) -> String {
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

pub(crate) fn git_diff(
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

pub(crate) fn git_file_history(project_root: &Path, args: &Value) -> String {
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
    let query_tokens = astra_runtime::text_tokenize::tokenize(query);
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

pub(crate) fn git_log_search(project_root: &Path, args: &Value) -> String {
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
        let tokens = astra_runtime::text_tokenize::tokenize(&message);

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

pub(crate) fn git_contributors(project_root: &Path, args: &Value) -> String {
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
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
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
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
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
    let message = match args.get("message").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return "Error: 'message' is required and must not be empty".to_string(),
    };

    // Validate message length (prevent absurdly long messages)
    if message.len() > 5000 {
        return "Error: commit message too long (max 5000 chars)".to_string();
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
            Err(e) => return format!("Error: git add failed: {e}"),
            Ok(ref out) if !out.status.success() => {
                return format!(
                    "Error: git add -A failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(_) => {}
        }
    } else if !files.is_empty() {
        // Stage specific files
        let mut cmd = std::process::Command::new("git");
        cmd.arg("add").args(&files).current_dir(project_root);
        let add_out = cmd.output();
        match add_out {
            Err(e) => return format!("Error: git add failed: {e}"),
            Ok(ref out) if !out.status.success() => {
                return format!(
                    "Error: git add failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
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
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Extract commit hash from output
            let short_hash = stdout
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("???");
            format!("✓ Committed: {short_hash} {message}")
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("nothing to commit") {
                "Nothing to commit — working tree clean".to_string()
            } else {
                format!("Error: git commit failed: {}", stderr.trim())
            }
        }
        Err(e) => format!("Error: git commit failed: {e}"),
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

pub(crate) fn git_stash_with_metadata(
    project_root: &Path,
    args: &Value,
) -> super::ToolExecutionOutcome {
    let action = match args.get("action").and_then(Value::as_str) {
        Some(a) => a,
        None => {
            return super::ToolExecutionOutcome::text(
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
                Err(error) => return super::ToolExecutionOutcome::text(error),
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
            return super::ToolExecutionOutcome::text(format!(
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
                super::ToolExecutionOutcome {
                    output,
                    tool_result_fields,
                }
            } else {
                let err = stderr.trim();
                if err.contains("No local changes") || err.contains("No stash entries") {
                    super::ToolExecutionOutcome::text(err.to_string())
                } else {
                    super::ToolExecutionOutcome::text(format!(
                        "Error: git stash {action} failed: {err}"
                    ))
                }
            }
        }
        Err(e) => super::ToolExecutionOutcome::text(format!("Error: git stash failed: {e}")),
    }
}

impl ToolExecutor {
    fn record_git_stash_rollback(&self, stash_ref: impl Into<String>, message: Option<String>) {
        let turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        match self.git_stash_journal.lock() {
            Ok(mut journal) => journal.record(stash_ref, turn_index, message),
            Err(poisoned) => poisoned.into_inner().record(stash_ref, turn_index, message),
        }
    }

    fn git_stash_entries(&self) -> Vec<GitStashRollbackEntry> {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn git_stash_restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitStashRollbackEntry> {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn git_stash_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitStashRollbackEntry> {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    pub(crate) fn git_stash_journal_checkpoint(&self) -> u64 {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn remove_git_stash_rollback(&self, stash_ref: &str) {
        match self.git_stash_journal.lock() {
            Ok(mut journal) => {
                journal.remove_stash(stash_ref);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_stash(stash_ref);
            }
        }
    }

    fn rollback_git_stash_entry_json(entry: &GitStashRollbackEntry) -> Value {
        let mut value = serde_json::Map::from_iter([
            (
                "stash_ref".to_string(),
                Value::String(entry.stash_ref.clone()),
            ),
            (
                "turn_index".to_string(),
                Value::Number(serde_json::Number::from(entry.turn_index)),
            ),
        ]);
        if let Some(message) = entry.message.as_ref() {
            value.insert("message".to_string(), Value::String(message.clone()));
        }
        Value::Object(value)
    }

    pub(crate) fn rollback_git_stashes(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let explicit_turn_index = if scope == "turn" {
            match args.get("turn_index").and_then(Value::as_u64) {
                Some(turn_index) => Some(turn_index),
                None => {
                    return serde_json::json!({
                        "success": false,
                        "error": "missing 'turn_index' for scope=turn",
                    })
                    .to_string();
                }
            }
        } else {
            None
        };
        let after_sequence = args
            .get("stash_after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match scope {
            "list" => {
                let entries = self
                    .git_stash_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_git_stash_entry_json(&entry))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                    "summary": format!("Listed {} recorded git stash rollback entr{}", entries.len(), if entries.len() == 1 { "y" } else { "ies" }),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let turn_index = explicit_turn_index.unwrap_or_else(|| {
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed) as u64
                }) as u32;
                let plan = if after_sequence > 0 {
                    self.git_stash_restore_plan_for_turn_since(turn_index, after_sequence)
                } else {
                    self.git_stash_restore_plan_for_turn(turn_index)
                };
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for entry in &plan {
                    let apply_output = git_stash(
                        &self.project_root,
                        &serde_json::json!({
                            "action": "apply",
                            "stash_ref": entry.stash_ref,
                        }),
                    );
                    if apply_output.starts_with("Error:") {
                        let mut failed_entry = Self::rollback_git_stash_entry_json(entry)
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        failed_entry.insert("error".to_string(), Value::String(apply_output));
                        failed.push(Value::Object(failed_entry));
                    } else {
                        self.remove_git_stash_rollback(&entry.stash_ref);
                        restored.push(Self::rollback_git_stash_entry_json(entry));
                    }
                }
                let success = !restored.is_empty() && failed.is_empty();
                let summary = if plan.is_empty() {
                    format!("No recorded git stash rollback handles found for turn {turn_index}")
                } else if failed.is_empty() {
                    format!(
                        "Re-applied {} recorded git stash{} for turn {turn_index}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "es" }
                    )
                } else {
                    format!(
                        "Re-applied {} recorded git stash{} for turn {turn_index} with {} failure{}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "es" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
                    )
                };
                serde_json::json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "restored": restored,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => serde_json::json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, list"
                ),
            })
            .to_string(),
        }
    }

    pub(crate) fn git_stash(&self, args: &Value) -> String {
        self.git_stash_with_metadata(args).output
    }

    pub(crate) fn git_stash_with_metadata(&self, args: &Value) -> super::ToolExecutionOutcome {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .map(|action| action.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let outcome = git_stash_with_metadata(&self.project_root, args);
        if matches!(action.as_str(), "push" | "save")
            && let Some(stash_ref) = outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("stash_ref"))
                .and_then(Value::as_str)
        {
            self.record_git_stash_rollback(
                stash_ref.to_string(),
                args.get("message")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            );
            self.clear_file_state();
        } else if matches!(action.as_str(), "apply" | "pop")
            && !outcome.output.starts_with("Error:")
        {
            self.clear_file_state();
        }
        outcome
    }
}

/// Revert a file to its last committed state (discard working tree changes).
///
/// Parameters:
/// - `path` (required): file path relative to project root
/// - `ref` (optional): restore from a specific commit/ref (default: HEAD)
pub fn git_checkout_file(project_root: &Path, args: &Value) -> String {
    let file_path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => return "Error: 'path' is required".to_string(),
    };

    // Security: reject path traversal
    if file_path.contains("..") {
        return "Error: path traversal not allowed".to_string();
    }

    let git_ref = args.get("ref").and_then(Value::as_str).unwrap_or("HEAD");

    // Validate ref doesn't contain shell-dangerous chars
    if git_ref.contains(';')
        || git_ref.contains('|')
        || git_ref.contains('&')
        || git_ref.contains('`')
        || git_ref.contains("$(")
        || git_ref.contains("${")
    {
        return "Error: invalid ref".to_string();
    }

    let out = std::process::Command::new("git")
        .args(["checkout", git_ref, "--", file_path])
        .current_dir(project_root)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            format!("✓ Restored {file_path} from {git_ref}")
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git checkout failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git checkout failed: {e}"),
    }
}

impl ToolExecutor {
    pub(crate) fn git_checkout_file(&self, args: &Value) -> String {
        let file_arg = match args.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => path,
            _ => return crate::edge_tools::git_gix::git_checkout_file(&self.project_root, args),
        };
        let path = match self.resolve_checked(file_arg) {
            Ok(path) => path,
            Err(error) => return error,
        };

        let turn_idx = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let journal_call_id = format!("git_checkout_file:{}", path.display());
        match self.file_journal.lock() {
            Ok(mut journal) => journal.record_before(&path, &journal_call_id, turn_idx),
            Err(poisoned) => poisoned
                .into_inner()
                .record_before(&path, &journal_call_id, turn_idx),
        }

        let output = crate::edge_tools::git_gix::git_checkout_file(&self.project_root, args);
        if output.starts_with("Error:") {
            return output;
        }

        let after_content = std::fs::read(&path).unwrap_or_default();
        match self.file_journal.lock() {
            Ok(mut journal) => journal.record_after(&path, &journal_call_id, &after_content),
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .record_after(&path, &journal_call_id, &after_content)
            }
        }

        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => self.record_write_with_content(&path, &content),
                Err(_) => self.record_write(&path),
            }
        } else {
            self.remove_file_state(&path);
        }

        output
    }
}

// ─── Git Worktree Management ────────────────────────────────────────────────

/// Create, list, or remove git worktrees for isolated parallel work.
pub fn git_worktree(project_root: &Path, args: &Value) -> String {
    let action = match args.get("action").and_then(Value::as_str) {
        Some(a) => a,
        None => return "Error: 'action' is required (add, list, remove)".to_string(),
    };

    match action {
        "add" | "create" => worktree_add(project_root, args),
        "list" | "ls" => worktree_list(project_root),
        "remove" | "rm" | "delete" => worktree_remove(project_root, args),
        _ => format!("Error: unknown worktree action '{action}'. Use: add, list, remove"),
    }
}

pub(crate) fn worktree_add(project_root: &Path, args: &Value) -> String {
    let branch = match args.get("branch").and_then(Value::as_str) {
        Some(b) if !b.is_empty() => b,
        _ => return "Error: 'branch' is required for add".to_string(),
    };

    // Security: reject shell-dangerous chars in branch name
    if branch
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '{' | '}'))
    {
        return "Error: invalid branch name".to_string();
    }

    // Determine worktree path: user-provided or auto-generated sibling directory
    let worktree_path = if let Some(p) = args.get("path").and_then(Value::as_str) {
        std::path::PathBuf::from(p)
    } else {
        // Default: sibling directory named <repo>-<branch>
        let repo_name = project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let sanitized_branch = branch.replace('/', "-");
        project_root
            .parent()
            .unwrap_or(project_root)
            .join(format!("{repo_name}-{sanitized_branch}"))
    };

    // Check if path already exists
    if worktree_path.exists() {
        return format!(
            "Error: worktree path already exists: {}",
            worktree_path.display()
        );
    }

    // Determine if we create a new branch or use existing
    let create_new = args
        .get("new_branch")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(project_root);
    cmd.arg("worktree").arg("add");

    if create_new {
        cmd.arg("-b").arg(branch);
    }

    cmd.arg(worktree_path.to_string_lossy().as_ref());

    if !create_new {
        cmd.arg(branch);
    }

    match cmd.output() {
        Ok(o) if o.status.success() => {
            format!(
                "✓ Worktree created\n  Branch: {branch}\n  Path: {}\n  Use `cd {}` or set project_root to work in this worktree.",
                worktree_path.display(),
                worktree_path.display()
            )
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git worktree add failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git worktree add failed: {e}"),
    }
}

pub(crate) fn worktree_list(project_root: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout);
            if raw.trim().is_empty() {
                return "No worktrees found".to_string();
            }

            // Parse porcelain output into structured display
            let mut result = String::from("Git Worktrees:\n");
            let mut current_path = "";
            let mut current_branch = String::new();
            let mut current_head = String::new();
            let mut is_bare = false;

            for line in raw.lines() {
                if let Some(path) = line.strip_prefix("worktree ") {
                    if !current_path.is_empty() {
                        result.push_str(&format_worktree_entry(
                            current_path,
                            &current_branch,
                            &current_head,
                            is_bare,
                        ));
                    }
                    current_path = path;
                    current_branch.clear();
                    current_head.clear();
                    is_bare = false;
                } else if let Some(head) = line.strip_prefix("HEAD ") {
                    current_head = head[..7.min(head.len())].to_string();
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                    current_branch = branch.to_string();
                } else if line == "bare" {
                    is_bare = true;
                } else if line == "detached" {
                    current_branch = "(detached)".to_string();
                }
            }
            // Flush last entry
            if !current_path.is_empty() {
                result.push_str(&format_worktree_entry(
                    current_path,
                    &current_branch,
                    &current_head,
                    is_bare,
                ));
            }

            result
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git worktree list failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git worktree list failed: {e}"),
    }
}

fn format_worktree_entry(path: &str, branch: &str, head: &str, bare: bool) -> String {
    if bare {
        format!("  {path} (bare)\n")
    } else if branch.is_empty() {
        format!("  {path}  [{head}]\n")
    } else {
        format!("  {path}  [{branch}] {head}\n")
    }
}

pub(crate) fn worktree_remove(project_root: &Path, args: &Value) -> String {
    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => return "Error: 'path' is required for remove".to_string(),
    };

    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

    // Also delete the branch if requested
    let delete_branch = args
        .get("delete_branch")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // First, get the branch name before removal (for optional branch deletion)
    let branch_name = if delete_branch {
        let out = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(project_root)
            .output();
        out.ok().and_then(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let mut found_path = false;
            for line in stdout.lines() {
                if let Some(wt_path) = line.strip_prefix("worktree ") {
                    found_path = wt_path == path
                        || std::path::Path::new(wt_path) == std::path::Path::new(path);
                }
                if found_path {
                    if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                        return Some(branch.to_string());
                    }
                }
            }
            None
        })
    } else {
        None
    };

    // Remove worktree
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(project_root).arg("worktree").arg("remove");
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path);

    match cmd.output() {
        Ok(o) if o.status.success() => {
            let mut msg = format!("✓ Worktree removed: {path}");

            // Optionally delete the branch
            if let Some(ref branch) = branch_name {
                let del = std::process::Command::new("git")
                    .args(["branch", "-D", branch])
                    .current_dir(project_root)
                    .output();
                match del {
                    Ok(d) if d.status.success() => {
                        msg.push_str(&format!("\n  ✓ Branch '{branch}' deleted"));
                    }
                    _ => {
                        msg.push_str(&format!("\n  ⚠ Could not delete branch '{branch}'"));
                    }
                }
            }

            msg
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git worktree remove failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git worktree remove failed: {e}"),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::str_preview::prefix_chars;
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

    #[test]
    fn score_commits_exact_match_ranks_higher() {
        let commits = vec![
            CommitDoc {
                hash: "a".into(),
                author: "test".into(),
                date: "2024".into(),
                message: "fix bug in parser".into(),
                tokens: astra_runtime::text_tokenize::tokenize("fix bug in parser"),
            },
            CommitDoc {
                hash: "b".into(),
                author: "test".into(),
                date: "2024".into(),
                message: "add new feature".into(),
                tokens: astra_runtime::text_tokenize::tokenize("add new feature"),
            },
        ];
        let result = score_commits("fix", &commits);
        assert!(!result.is_empty(), "should find 'fix'");
        assert_eq!(result[0].0, 0, "commit about 'fix' should rank first");
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

    #[tokio::test]
    async fn execute_with_metadata_returns_stash_ref_for_push_and_apply_accepts_it() {
        let repo = init_temp_repo();
        let tracked = repo.path().join("tracked.txt");
        std::fs::write(&tracked, "two\n").expect("modify tracked file");
        let executor = ToolExecutor::new(repo.path());

        let outcome = executor
            .execute_with_metadata("git_stash", &json!({"action": "push", "message": "demo"}))
            .await;
        assert!(
            !outcome.output.starts_with("Error:"),
            "stash push failed: {}",
            outcome.output
        );
        let stash_ref = outcome
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("stash_ref"))
            .and_then(Value::as_str)
            .expect("stash_ref");
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("clean worktree after stash"),
            "one\n"
        );

        let apply = git_stash(
            repo.path(),
            &json!({"action": "apply", "stash_ref": stash_ref}),
        );
        assert!(!apply.starts_with("Error:"), "stash apply failed: {apply}");
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("restored working tree"),
            "two\n"
        );
    }

    // ─── git_checkout_file tests ────────────────────────────────────────────

    #[test]
    fn git_checkout_file_requires_path() {
        let root = repo_root();
        let result = git_checkout_file(&root, &json!({}));
        assert!(
            result.starts_with("Error:"),
            "should require path: {result}"
        );

        let result2 = git_checkout_file(&root, &json!({"path": ""}));
        assert!(
            result2.starts_with("Error:"),
            "should reject empty path: {result2}"
        );
    }

    #[test]
    fn git_checkout_file_rejects_path_traversal() {
        let root = repo_root();
        let result = git_checkout_file(&root, &json!({"path": "../../../etc/passwd"}));
        assert!(
            result.contains("path traversal"),
            "should reject traversal: {result}"
        );
    }

    #[test]
    fn git_checkout_file_rejects_dangerous_ref() {
        let root = repo_root();
        let result = git_checkout_file(
            &root,
            &json!({"path": "README.md", "ref": "HEAD; rm -rf /"}),
        );
        assert!(
            result.contains("invalid ref"),
            "should reject dangerous ref: {result}"
        );

        let result2 = git_checkout_file(
            &root,
            &json!({"path": "README.md", "ref": "main|cat /etc/passwd"}),
        );
        assert!(
            result2.contains("invalid ref"),
            "should reject pipe ref: {result2}"
        );
    }

    #[test]
    fn git_checkout_file_known_file() {
        let root = repo_root();
        // Checkout a known file at HEAD — should succeed (idempotent)
        let result = git_checkout_file(&root, &json!({"path": "README.md"}));
        assert!(
            result.contains("Restored") || result.contains("Error"),
            "should restore or report error: {result}"
        );
    }

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

    #[test]
    fn git_worktree_missing_action() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({}));
        assert!(
            result.contains("Error") && result.contains("action"),
            "should require action: {result}"
        );
    }

    #[test]
    fn git_worktree_unknown_action() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "teleport"}));
        assert!(
            result.contains("Error") && result.contains("unknown"),
            "should reject unknown action: {result}"
        );
    }

    #[test]
    fn git_worktree_add_missing_branch() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "add"}));
        assert!(
            result.contains("Error") && result.contains("branch"),
            "should require branch: {result}"
        );
    }

    #[test]
    fn git_worktree_add_rejects_shell_injection() {
        let root = repo_root();
        for dangerous in &[
            "test;rm -rf /",
            "test|cat /etc/passwd",
            "test&whoami",
            "test`id`",
            "test$(whoami)",
            "test()",
            "test{}",
        ] {
            let result = git_worktree(&root, &json!({"action": "add", "branch": dangerous}));
            assert!(
                result.contains("Error") && result.contains("invalid branch name"),
                "should reject '{dangerous}': {result}"
            );
        }
    }

    #[test]
    fn git_worktree_list_runs() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "list"}));
        // Should contain at least the main worktree path
        assert!(
            !result.contains("Error: git") || result.contains("worktree"),
            "list should succeed or show worktree info: {result}"
        );
    }

    #[test]
    fn git_worktree_list_alias_ls() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "ls"}));
        assert!(
            !result.contains("unknown"),
            "ls should be accepted as alias for list: {result}"
        );
    }

    #[test]
    fn git_worktree_remove_missing_path() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "remove"}));
        assert!(
            result.contains("Error") && result.contains("path"),
            "should require path for remove: {result}"
        );
    }

    #[test]
    fn git_worktree_remove_nonexistent() {
        let root = repo_root();
        let result = git_worktree(
            &root,
            &json!({"action": "remove", "path": "/tmp/nonexistent-worktree-xyz"}),
        );
        assert!(
            result.contains("Error") || result.contains("error"),
            "removing nonexistent worktree should fail: {result}"
        );
    }

    #[test]
    fn git_worktree_add_existing_path_fails() {
        let root = repo_root();
        // Use /tmp which always exists — should fail with "already exists"
        let result = git_worktree(
            &root,
            &json!({"action": "add", "branch": "test-existing", "path": "/tmp"}),
        );
        assert!(
            result.contains("Error") && result.contains("already exists"),
            "should reject existing path: {result}"
        );
    }

    #[test]
    fn git_worktree_action_aliases() {
        let root = repo_root();
        // "create" should alias to "add" (needs branch param, so will error on missing branch)
        let r1 = git_worktree(&root, &json!({"action": "create"}));
        assert!(r1.contains("branch"), "create should route to add: {r1}");

        // "rm" and "delete" should alias to "remove" (needs path param)
        let r2 = git_worktree(&root, &json!({"action": "rm"}));
        assert!(r2.contains("path"), "rm should route to remove: {r2}");

        let r3 = git_worktree(&root, &json!({"action": "delete"}));
        assert!(r3.contains("path"), "delete should route to remove: {r3}");
    }
}
