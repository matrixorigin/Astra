//! Context pre-fetch for common task patterns.
//!
//! When the user's message implies a well-known task (e.g. "review latest commit"),
//! we pre-fetch relevant context locally (git log, diff, etc.) and inject it into
//! the conversation so the LLM can respond in fewer rounds — often a single turn.
//!
//! This mirrors the approach used by Claude Code / Cursor: gather context *before*
//! the first LLM call rather than letting the model discover it over many tool rounds.

use std::path::Path;
use std::process::Command;

use astra_runtime::prompts::detect_task_type;
use serde_json::Value;

/// Maximum bytes of git diff output to include.
const MAX_DIFF_BYTES: usize = 48_000; // ~48 KB — fits comfortably in most context windows

/// Maximum bytes of git log output.
const MAX_LOG_BYTES: usize = 4_000;

/// Detect the task type from the user message and, if applicable, pre-fetch
/// context that eliminates the need for the LLM to call tools in early rounds.
///
/// Returns `Some(context_string)` when context was gathered, `None` otherwise.
pub fn prefetch_context_for_message(message: &str, project_root: &Path) -> Option<String> {
    match detect_task_type(message) {
        Some("code_review") => prefetch_code_review_context(message, project_root),
        _ => None,
    }
}

/// Inject pre-fetched context into the last user message in the messages array.
///
/// The context is appended as a clearly delimited block so the LLM knows it's
/// supplementary material, not part of the user's original request.
pub fn inject_prefetched_context(messages: &mut [Value], context: &str) {
    if let Some(last) = messages.last_mut() {
        if let Some(content) = last.get("content").and_then(Value::as_str) {
            let enriched = format!(
                "{}\n\n<prefetched_context>\n\
                 The complete git context is provided below. You already have the full diff — \
                 review it directly without calling git tools or the skill tool. \
                 Only use read_file if you need to see surrounding code not in the diff.\n\n\
                 {}\n</prefetched_context>",
                content, context
            );
            last["content"] = Value::String(enriched);
        }
    }
}

// ─── Code Review ─────────────────────────────────────────────────────────────

/// Pre-fetch git context for code review tasks.
///
/// Strategy:
/// - If the message mentions "commit" / "提交" → fetch latest commit log + diff
/// - If the message mentions "PR" / "pull request" → fetch current branch diff vs main
/// - If the message mentions "changes" / "改动" / "diff" → fetch unstaged + staged changes
/// - Default: fetch latest commit (most common review scenario)
fn prefetch_code_review_context(message: &str, project_root: &Path) -> Option<String> {
    // Check if we're in a git repository
    if !is_git_repo(project_root) {
        return None;
    }

    let lower = message.to_lowercase();

    if mentions_commit(&lower) {
        prefetch_commit_review(project_root)
    } else if mentions_pr(&lower) {
        prefetch_branch_diff(project_root)
    } else if mentions_local_changes(&lower) {
        prefetch_working_changes(project_root)
    } else {
        // Default: review latest commit
        prefetch_commit_review(project_root)
    }
}

/// Fetch the latest commit's log + diff for review.
fn prefetch_commit_review(project_root: &Path) -> Option<String> {
    let log = run_git(
        project_root,
        &[
            "log",
            "-1",
            "--format=%H%n%an <%ae>%n%ai%n%s%n%n%b",
        ],
        MAX_LOG_BYTES,
    )?;

    let stat = run_git(
        project_root,
        &["diff", "--stat", "HEAD~1..HEAD"],
        MAX_LOG_BYTES,
    )
    .unwrap_or_default();

    let diff = run_git(
        project_root,
        &["diff", "HEAD~1..HEAD"],
        MAX_DIFF_BYTES,
    )?;

    let mut ctx = String::with_capacity(log.len() + stat.len() + diff.len() + 200);
    ctx.push_str("## Latest Commit\n\n```\n");
    ctx.push_str(&log);
    ctx.push_str("```\n\n## Changed Files\n\n```\n");
    ctx.push_str(&stat);
    ctx.push_str("```\n\n## Full Diff\n\n```diff\n");
    ctx.push_str(&diff);
    ctx.push_str("\n```\n");

    Some(ctx)
}

/// Fetch the diff of the current branch vs its merge-base with main/master.
fn prefetch_branch_diff(project_root: &Path) -> Option<String> {
    // Determine the default branch (main or master)
    let default_branch = detect_default_branch(project_root)?;

    let merge_base = run_git(
        project_root,
        &["merge-base", &default_branch, "HEAD"],
        256,
    )?;
    let merge_base = merge_base.trim();

    let log = run_git(
        project_root,
        &[
            "log",
            "--oneline",
            &format!("{merge_base}..HEAD"),
        ],
        MAX_LOG_BYTES,
    )
    .unwrap_or_default();

    let stat = run_git(
        project_root,
        &["diff", "--stat", &format!("{merge_base}..HEAD")],
        MAX_LOG_BYTES,
    )
    .unwrap_or_default();

    let diff = run_git(
        project_root,
        &["diff", &format!("{merge_base}..HEAD")],
        MAX_DIFF_BYTES,
    )?;

    let mut ctx = String::with_capacity(log.len() + stat.len() + diff.len() + 300);
    ctx.push_str(&format!(
        "## Branch Diff (vs {default_branch})\n\n### Commits\n\n```\n"
    ));
    ctx.push_str(&log);
    ctx.push_str("```\n\n### Changed Files\n\n```\n");
    ctx.push_str(&stat);
    ctx.push_str("```\n\n### Full Diff\n\n```diff\n");
    ctx.push_str(&diff);
    ctx.push_str("\n```\n");

    Some(ctx)
}

/// Fetch unstaged + staged working directory changes for review.
fn prefetch_working_changes(project_root: &Path) -> Option<String> {
    let staged = run_git(project_root, &["diff", "--cached"], MAX_DIFF_BYTES);
    let unstaged = run_git(project_root, &["diff"], MAX_DIFF_BYTES);

    let staged_text = staged.unwrap_or_default();
    let unstaged_text = unstaged.unwrap_or_default();

    if staged_text.is_empty() && unstaged_text.is_empty() {
        // No local changes — fall back to latest commit review
        return prefetch_commit_review(project_root);
    }

    let stat = run_git(project_root, &["diff", "--stat", "HEAD"], MAX_LOG_BYTES)
        .unwrap_or_default();

    let mut ctx = String::with_capacity(staged_text.len() + unstaged_text.len() + stat.len() + 200);
    ctx.push_str("## Working Directory Changes\n\n### Changed Files\n\n```\n");
    ctx.push_str(&stat);
    ctx.push_str("```\n\n");

    if !staged_text.is_empty() {
        ctx.push_str("### Staged Changes\n\n```diff\n");
        ctx.push_str(&staged_text);
        ctx.push_str("\n```\n\n");
    }

    if !unstaged_text.is_empty() {
        ctx.push_str("### Unstaged Changes\n\n```diff\n");
        ctx.push_str(&unstaged_text);
        ctx.push_str("\n```\n");
    }

    Some(ctx)
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn is_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn detect_default_branch(project_root: &Path) -> Option<String> {
    // Try "main" first, then "master"
    for branch in &["main", "master"] {
        let result = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(project_root)
            .output();
        if result.map(|o| o.status.success()).unwrap_or(false) {
            return Some(branch.to_string());
        }
    }
    // Fall back to remote default
    run_git(
        project_root,
        &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
        256,
    )
    .map(|s| s.trim().trim_start_matches("origin/").to_string())
}

fn run_git(dir: &Path, args: &[&str], max_bytes: usize) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = output.stdout;
    if raw.is_empty() {
        return Some(String::new());
    }

    if raw.len() <= max_bytes {
        String::from_utf8(raw).ok()
    } else {
        // Truncate at a valid UTF-8 boundary and add indicator
        let truncated = &raw[..max_bytes];
        let s = String::from_utf8_lossy(truncated);
        Some(format!(
            "{}\n\n... [truncated at {} KB, {} KB total]",
            s.trim_end(),
            max_bytes / 1024,
            raw.len() / 1024
        ))
    }
}

fn mentions_commit(lower: &str) -> bool {
    lower.contains("commit")
        || lower.contains("提交")
        || lower.contains("latest")
        || lower.contains("最新")
        || lower.contains("last")
}

fn mentions_pr(lower: &str) -> bool {
    lower.contains("pr")
        || lower.contains("pull request")
        || lower.contains("merge request")
        || lower.contains("branch")
        || lower.contains("分支")
}

fn mentions_local_changes(lower: &str) -> bool {
    lower.contains("changes")
        || lower.contains("改动")
        || lower.contains("改了")
        || lower.contains("staged")
        || lower.contains("unstaged")
        || lower.contains("working")
        || lower.contains("local")
        || lower.contains("本地")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_project_root() -> PathBuf {
        // Use the actual project root (we're inside a git repo)
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn detect_code_review_returns_context() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("review latest commit", &root);
        assert!(ctx.is_some(), "should return context for code review");
        let ctx = ctx.unwrap();
        assert!(ctx.contains("Latest Commit"), "should contain commit info");
        assert!(ctx.contains("Full Diff"), "should contain diff");
    }

    #[test]
    fn detect_non_review_returns_none() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("hello world", &root);
        assert!(ctx.is_none(), "should not return context for greeting");
    }

    #[test]
    fn inject_context_appends_to_last_message() {
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "review the commit"}),
        ];
        inject_prefetched_context(&mut messages, "DIFF HERE");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("review the commit"));
        assert!(content.contains("<prefetched_context>"));
        assert!(content.contains("DIFF HERE"));
        assert!(content.contains("review it directly"));
    }

    #[test]
    fn mentions_commit_detection() {
        assert!(mentions_commit("review latest commit"));
        assert!(mentions_commit("看看最新提交"));
        assert!(!mentions_commit("review the code"));
    }

    #[test]
    fn mentions_pr_detection() {
        assert!(mentions_pr("review this pr"));
        assert!(mentions_pr("check the pull request"));
        assert!(mentions_pr("看看分支改动"));
        assert!(!mentions_pr("review latest commit"));
    }

    #[test]
    fn mentions_local_changes_detection() {
        assert!(mentions_local_changes("review my changes"));
        assert!(mentions_local_changes("看看本地改动"));
        assert!(!mentions_local_changes("review the commit"));
    }
}
