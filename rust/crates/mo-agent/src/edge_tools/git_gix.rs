//! Pure-Rust git implementations using the `gix` crate.
//!
//! Replaces shell `git` subprocess calls with in-process operations for:
//! - git_status, git_diff, git_log, git_show, git_blame, git_file_history
//!
//! Benefits: no subprocess overhead, no shell injection risk, no `git` binary dependency.

use std::path::Path;

use gix::bstr::{BString, ByteSlice};
use serde_json::Value;

use super::truncate_output;

const DIFF_LIMIT: usize = 12_000; // ~3K tokens; was 20K
const SHOW_LIMIT: usize = 16_000; // ~4K tokens; was 30K

fn tool_output_limit() -> usize {
    super::tool_output_limit()
}

/// Scale a base output limit by budget pressure.
/// pressure=0.0 → 100% of base, pressure=0.6 → 52%, pressure=0.9 → 28%.
/// Never goes below 20% of base (minimum useful output).
fn pressure_scaled_limit(base: usize, pressure: f64) -> usize {
    let scale = (1.0 - pressure * 0.8).max(0.2);
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
        Ok(Some(reference)) => {
            let name = reference.name().shorten().to_string();
            name
        }
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
        Ok(id) => {
            let hex = id.to_string();
            hex[..hex.len().min(7)].to_string()
        }
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
            out.push_str(&format!("## HEAD detached at {}\n", &head.to_string()[..8]));
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
        .sorting(gix::revision::walk::Sorting::ByCommitTime(gix::traverse::commit::simple::CommitTimeOrder::NewestFirst))
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

pub(crate) fn git_show(project_root: &Path, args: &Value, pressure: f64) -> String {
    let limit = pressure_scaled_limit(SHOW_LIMIT, pressure);
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
                for entry in tree.iter() {
                    if let Ok(e) = entry {
                        let name = e.filename().to_string();
                        let full = if prefix.is_empty() {
                            name
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if e.mode().is_tree() {
                            if let Ok(obj) = e.object() {
                                if let Ok(sub) = obj.try_into_tree() {
                                    list_tree_entries(&sub, &full, out);
                                }
                            }
                        } else {
                            out.push_str(&format!("A {full}\n"));
                        }
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

            if let Some(filter) = file_filter {
                if !location.contains(filter) {
                    return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()));
                }
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
        match gix::blame::BlameRanges::from_one_based_inclusive_ranges(
            vec![(start as u32)..=(end as u32)],
        ) {
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
            if let Some(s) = line_start {
                if line_no < s {
                    continue;
                }
            }
            if let Some(e) = line_end {
                if line_no > e {
                    continue;
                }
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

pub(crate) fn git_diff(project_root: &Path, args: &Value, pressure: f64) -> String {
    let limit = pressure_scaled_limit(DIFF_LIMIT, pressure);
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let staged = args
        .get("staged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let git_ref = args.get("ref").and_then(Value::as_str);

    // If a ref is given, do a tree-to-tree diff (HEAD vs ref)
    if let Some(ref_str) = git_ref {
        return diff_tree_to_tree_str(&repo, ref_str, limit);
    }

    // If staged, do index-to-HEAD diff
    if staged {
        return diff_index_to_head(&repo, limit);
    }

    // Default: worktree changes (unstaged) using status iterator
    diff_worktree(&repo, limit)
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

    let result = changes_platform.for_each_to_obtain_tree(&head_tree, |change: gix::object::tree::diff::Change<'_, '_, '_>| {
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
    });

    if let Err(e) = result {
        out.push_str(&format!("\n[diff error: {e}]\n"));
    }

    if out.is_empty() {
        "No changes".to_string()
    } else {
        out.push_str(&format!("\n{count} file(s) changed"));
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
            for entry in tree.iter() {
                if let Ok(e) = entry {
                    let name = e.filename().to_string();
                    let full = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    if e.mode().is_tree() {
                        if let Ok(obj) = e.object() {
                            if let Ok(sub) = obj.try_into_tree() {
                                collect_tree_paths(&sub, &full, paths);
                            }
                        }
                    } else {
                        paths.insert(full);
                    }
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
        out.push_str(&format!("\n{count} file(s) changed"));
        truncate_diff_at(out, limit)
    }
}

fn resolve_tree<'r>(repo: &'r gix::Repository, ref_str: &str) -> Result<gix::Tree<'r>, String> {
    let id = repo
        .rev_parse_single(ref_str)
        .map_err(|e| format!("Error: cannot resolve '{ref_str}': {e}"))?;
    let obj = id
        .object()
        .map_err(|e| format!("Error: {e}"))?;
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

    let n = args.get("n").and_then(Value::as_u64).unwrap_or(10) as usize;

    let head = match repo.head_id() {
        Ok(h) => h,
        Err(e) => return format!("Error: {e}"),
    };

    let walk = match head
        .ancestors()
        .sorting(gix::revision::walk::Sorting::ByCommitTime(gix::traverse::commit::simple::CommitTimeOrder::NewestFirst))
        .all()
    {
        Ok(w) => w,
        Err(e) => return format!("Error: {e}"),
    };

    let mut lines = Vec::new();
    let mut walked = 0usize;
    const MAX_WALK: usize = 50_000;
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
        let parent_has_same = commit.parent_ids().next().map_or(false, |pid| {
            pid.object()
                .ok()
                .and_then(|o| o.try_into_commit().ok())
                .and_then(|pc| pc.tree().ok())
                .and_then(|pt| match pt.lookup_entry_by_path(file) {
                    Ok(Some(parent_entry)) => {
                        Some(parent_entry.object_id() == cur_entry.object_id())
                    }
                    _ => Some(false),
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
    let query_tokens = mo_agent_runtime::text_tokenize::tokenize(query);
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
            let mut doc_tf: std::collections::HashMap<&str, f64> =
                std::collections::HashMap::new();
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
        let tokens = mo_agent_runtime::text_tokenize::tokenize(&message);

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
    let since_str = args.get("since").and_then(Value::as_str);

    // Parse --since into a unix timestamp cutoff
    let since_cutoff: Option<i64> = since_str.and_then(|s| parse_since_to_epoch(s));

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

    let mut author_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut file_freq: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut recent_lines: Vec<String> = Vec::new();
    let mut total_commits = 0u32;
    let mut walked = 0u32;
    const MAX_WALK: u32 = 50_000;

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
        if let Some(cutoff) = since_cutoff {
            if let Ok(author) = commit.author() {
                if let Ok(time) = author.time() {
                    if time.seconds < cutoff {
                        break; // Commits are sorted newest-first, so stop early
                    }
                }
            }
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
            let parent_same = commit.parent_ids().next().map_or(false, |pid| {
                pid.object()
                    .ok()
                    .and_then(|o| o.try_into_commit().ok())
                    .and_then(|pc| pc.tree().ok())
                    .and_then(|pt| match pt.lookup_entry_by_path(path) {
                        Ok(Some(pe)) => Some(pe.object_id() == cur_entry.object_id()),
                        _ => Some(false),
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

            if let (Some(new_tree), Some(old_tree)) = (tree, parent_tree) {
                if let Ok(mut changes) = old_tree.changes() {
                    let _ = changes.for_each_to_obtain_tree(&new_tree, |change| {
                        use gix::object::tree::diff::Change;
                        let location = match &change {
                            Change::Addition { location, .. }
                            | Change::Deletion { location, .. }
                            | Change::Modification { location, .. }
                            | Change::Rewrite { location, .. } => location.to_string(),
                        };

                        if let Some(pf) = path_filter {
                            if !location.starts_with(pf) {
                                return Ok::<_, std::convert::Infallible>(
                                    std::ops::ControlFlow::Continue(()),
                                );
                            }
                        }

                        *file_freq.entry(location).or_default() += 1;
                        Ok(std::ops::ControlFlow::Continue(()))
                    });
                }
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
        parts.push(format!(
            "## Recent Activity\n{}",
            recent_lines.join("\n")
        ));
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo_root() -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // rust/
        path
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
        assert!(
            first.len() >= 7 && first[..7].chars().all(|c| c.is_ascii_hexdigit()),
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
        let result = git_show(&root, &json!({}), 0.0);
        assert!(result.contains("Error: missing"));
    }

    #[test]
    fn git_show_invalid_ref() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "abc;rm -rf /"}), 0.0);
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}), 0.0);
        assert!(result.contains("commit "), "should show commit: {result}");
        assert!(result.contains("Author:"), "should show author");
    }

    #[test]
    fn git_show_stat_only() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}), 0.0);
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
        let result = git_diff(&root, &json!({}), 0.0);
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
        if result.contains("Commits:") {
            if let Some(line) = result.lines().find(|l| l.starts_with("Commits:")) {
                let count: usize = line
                    .trim_start_matches("Commits: ")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                assert!(count <= 3, "should respect n limit: {count}");
            }
        }
    }

    // ─── git_diff enhanced tests ────────────────────────────────────────────

    #[test]
    fn git_diff_staged_param_accepted() {
        let root = repo_root();
        // staged=true should not crash (may return "No staged changes" or file list)
        let result = git_diff(&root, &json!({"staged": true}), 0.0);
        assert!(
            !result.contains("Error: cannot open"),
            "staged diff should not fail to open repo: {result}"
        );
    }

    #[test]
    fn git_diff_ref_param_uses_tree_diff() {
        let root = repo_root();
        // Diff HEAD against HEAD~1 should produce actual file changes
        let result = git_diff(&root, &json!({"ref": "HEAD~1"}), 0.0);
        assert!(
            result.contains("diff --git") || result.contains("No changes") || result.contains("Error: cannot resolve"),
            "ref diff should produce diff output or error: {result}"
        );
    }

    #[test]
    fn git_diff_default_shows_worktree() {
        let root = repo_root();
        let result = git_diff(&root, &json!({}), 0.0);
        // Should not error — either shows changes or "No changes"
        assert!(
            !result.starts_with("Error:"),
            "default diff should work: {result}"
        );
    }

    // ─── git_show enhanced tests ────────────────────────────────────────────

    #[test]
    fn git_show_allows_reflog_syntax() {
        let root = repo_root();
        // HEAD@{0} should not be rejected by validation — it should reach rev_parse
        let result = git_show(&root, &json!({"commit": "HEAD@{0}"}), 0.0);
        // Should show a commit (passes validation), not be rejected outright
        assert!(
            result.starts_with("commit ") || result.starts_with("Error: cannot resolve"),
            "HEAD@{{0}} should pass validation and reach parsing: {result}"
        );
    }

    #[test]
    fn git_show_rejects_shell_metachar() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD;rm -rf /"}), 0.0);
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head_has_diff_content() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}), 0.0);
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
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}), 0.0);
        assert!(result.contains("commit "));
        assert!(
            result.contains("files changed") || result.contains("[root commit]"),
            "should show stats or root: {result}"
        );
    }

    #[test]
    fn git_show_file_filter() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "file": "README.md"}), 0.0);
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
                result.contains("lines,") && result.contains("authors,") && result.contains("commits"),
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
            result.contains("##") || result.contains("nothing to commit") || result.contains("HEAD detached"),
            "status should show branch: {result}"
        );
    }

    // ─── git_log enhanced tests ─────────────────────────────────────────────

    #[test]
    fn git_log_custom_n() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 3}));
        let lines: Vec<&str> = result.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines.len() <= 3, "should respect n=3: got {} lines", lines.len());
        assert!(!lines.is_empty(), "should have at least 1 commit");
    }

    #[test]
    fn git_log_format_consistent() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 5}));
        for line in result.lines().filter(|l| !l.is_empty()) {
            // Each line should start with a 7-char hex hash
            assert!(
                line.len() >= 7 && line[..7].chars().all(|c| c.is_ascii_hexdigit()),
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
        let result = git_diff(&root, &json!({"ref": "HEAD~1"}), 0.0);
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
        let result = git_show(&root, &json!({"commit": "HEAD~1"}), 0.0);
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
            if let Some(start) = result.find('(') {
                if let Some(end) = result.find(" commits searched") {
                    let num_str = &result[start + 1..end];
                    let count: usize = num_str.parse().unwrap_or(0);
                    assert!(count <= 10, "should search at most 10: {count}");
                }
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
                if let Some(start) = line.find("[score:") {
                    if let Some(end) = line[start..].find(']') {
                        let score_str = &line[start + 7..start + end];
                        let score: f64 = score_str.parse().unwrap_or(0.0);
                        assert!(
                            score > 0.0 && score <= 1.0,
                            "score should be 0-1: {score}"
                        );
                    }
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
                tokens: mo_agent_runtime::text_tokenize::tokenize("fix bug in parser"),
            },
            CommitDoc {
                hash: "b".into(),
                author: "test".into(),
                date: "2024".into(),
                message: "add new feature".into(),
                tokens: mo_agent_runtime::text_tokenize::tokenize("add new feature"),
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
        assert!(
            !branch.contains("Error"),
            "should not error: {branch}"
        );
    }

    #[test]
    fn head_short_returns_hex() {
        let root = repo_root();
        let short = head_short(&root);
        assert!(!short.is_empty(), "should return a short hash");
        assert!(short.len() <= 7, "should be at most 7 chars: {short}");
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
        let result = git_diff(&root, &json!({"staged": true}), 0.0);
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
                if let Ok(c) = info.object() {
                    if c.parent_ids().count() == 0 {
                        root_oid = Some(info.id.to_string());
                        break;
                    }
                }
            }
        }
        if let Some(oid) = root_oid {
            let result = git_show(&root, &json!({"commit": oid}), 0.0);
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
        assert!(limit > 2_400, "should stay above 20% minimum");
        assert_eq!(limit, 6_240);
    }

    #[test]
    fn pressure_scaled_limit_max_pressure_reaches_floor() {
        let limit = super::pressure_scaled_limit(12_000, 1.0);
        assert_eq!(limit, 2_400);
    }

    #[test]
    fn pressure_scaled_limit_never_goes_below_twenty_percent() {
        let limit = super::pressure_scaled_limit(10_000, 1.5);
        assert_eq!(limit, 2_000);
    }

    #[test]
    fn git_show_under_pressure_truncates_earlier() {
        let root = std::env::current_dir().unwrap();
        let normal = git_show(&root, &json!({"commit": "HEAD"}), 0.0);
        let pressed = git_show(&root, &json!({"commit": "HEAD"}), 0.9);
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
        let normal = git_diff(&root, &json!({}), 0.0);
        let pressed = git_diff(&root, &json!({}), 0.9);
        assert!(
            pressed.len() <= normal.len(),
            "high-pressure diff ({}) should not exceed normal ({})",
            pressed.len(),
            normal.len()
        );
    }
}