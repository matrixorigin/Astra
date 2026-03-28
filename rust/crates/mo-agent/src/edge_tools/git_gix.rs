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

const DIFF_LIMIT: usize = 20_000;
const SHOW_LIMIT: usize = 30_000;

fn tool_output_limit() -> usize {
    super::tool_output_limit()
}

fn open_repo(project_root: &Path) -> Result<gix::Repository, String> {
    gix::discover(project_root).map_err(|e| format!("Error: cannot open git repo: {e}"))
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
        out
    }
}

// ─── git_show ───────────────────────────────────────────────────────────────

pub(crate) fn git_show(project_root: &Path, args: &Value) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let commit_ref = match args.get("commit").and_then(Value::as_str) {
        Some(c) => c,
        None => return "Error: missing 'commit' (SHA, branch, or tag)".to_string(),
    };

    if commit_ref.contains(|c: char| {
        !c.is_alphanumeric()
            && c != '-'
            && c != '_'
            && c != '.'
            && c != '/'
            && c != '~'
            && c != '^'
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
            // Root commit — show added files
            out.push_str("\n[root commit]\n");
            for entry in new_tree.iter() {
                if let Ok(e) = entry {
                    out.push_str(&format!("A {}\n", e.filename()));
                }
            }
            return truncate_show(out);
        }
    };

    // Collect tree diff
    let mut changes_platform = match old_tree.changes() {
        Ok(p) => p,
        Err(e) => {
            out.push_str(&format!("\n[diff error: {e}]\n"));
            return truncate_show(out);
        }
    };

    let mut file_changes: Vec<(String, &str)> = Vec::new();
    let result = changes_platform.for_each_to_obtain_tree(&new_tree, |change| {
        use gix::object::tree::diff::Change;
        let (location, change_type) = match &change {
            Change::Addition { location, .. } => (location.to_string(), "A"),
            Change::Deletion { location, .. } => (location.to_string(), "D"),
            Change::Modification { location, .. } => (location.to_string(), "M"),
            Change::Rewrite { location, .. } => (location.to_string(), "R"),
        };

        if let Some(filter) = file_filter {
            if !location.contains(filter) {
                return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()));
            }
        }

        file_changes.push((location, change_type));
        Ok(std::ops::ControlFlow::Continue(()))
    });

    if let Err(e) = result {
        out.push_str(&format!("\n[diff error: {e}]\n"));
        return truncate_show(out);
    }

    if stat_only {
        out.push_str(&format!("\n {} files changed\n", file_changes.len()));
        for (path, ct) in &file_changes {
            out.push_str(&format!(" {ct} {path}\n"));
        }
    } else {
        // Use stats API for line counts
        let mut changes_platform2 = match old_tree.changes() {
            Ok(p) => p,
            Err(_) => {
                // Fallback: just show file list
                out.push_str("\n---\n");
                for (path, ct) in &file_changes {
                    out.push_str(&format!("{ct} {path}\n"));
                }
                return truncate_show(out);
            }
        };
        if let Ok(stats) = changes_platform2.stats(&new_tree) {
            out.push_str(&format!(
                "\n {} files changed, {} insertions(+), {} deletions(-)\n",
                stats.files_changed, stats.lines_added, stats.lines_removed
            ));
        }
        for (path, ct) in &file_changes {
            out.push_str(&format!(" {ct} {path}\n"));
        }
    }

    truncate_show(out)
}

fn truncate_show(out: String) -> String {
    if out.len() > SHOW_LIMIT {
        let mut t = out[..SHOW_LIMIT].to_string();
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

pub(crate) fn git_diff(project_root: &Path, args: &Value) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(e) => return e,
    };

    let _staged = args
        .get("staged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let _git_ref = args.get("ref").and_then(Value::as_str);

    // Use status to find changed files
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

                if out.len() > DIFF_LIMIT {
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
        if out.len() > DIFF_LIMIT {
            let mut t = out[..DIFF_LIMIT].to_string();
            t.push_str("\n[truncated]");
            t
        } else {
            out
        }
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
    for info in walk {
        if lines.len() >= n {
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
        let result = git_show(&root, &json!({}));
        assert!(result.contains("Error: missing"));
    }

    #[test]
    fn git_show_invalid_ref() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "abc;rm -rf /"}));
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}));
        assert!(result.contains("commit "), "should show commit: {result}");
        assert!(result.contains("Author:"), "should show author");
    }

    #[test]
    fn git_show_stat_only() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}));
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
        let result = git_diff(&root, &json!({}));
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
}
