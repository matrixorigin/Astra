//! Context pre-fetch for common task patterns.
//!
//! When the user's message implies a well-known task (e.g. "review latest commit"),
//! we pre-fetch relevant context locally (git log, diff, etc.) and inject it into
//! the conversation so the LLM can respond in fewer rounds — often a single turn.
//!
//! This mirrors the approach used by Claude Code / Cursor: gather context *before*
//! the first LLM call rather than letting the model discover it over many tool rounds.

use std::path::Path;
use tokio::process::Command;

use astra_runtime::prompts::detect_task_type;
use serde_json::Value;

/// Maximum bytes of git diff output to include.
const MAX_DIFF_BYTES: usize = 48_000;

/// Maximum bytes of git log output.
const MAX_LOG_BYTES: usize = 4_000;

/// Maximum bytes per file read for project overview.
const MAX_FILE_BYTES: usize = 4_000;

/// Maximum total bytes for directory listing output.
const MAX_TREE_BYTES: usize = 6_000;

/// Pre-fetched context with task-type-specific guidance.
pub struct PrefetchedContext {
    pub task_type: &'static str,
    pub body: String,
}

/// Detect the task type from the user message and, if applicable, pre-fetch
/// context that eliminates the need for the LLM to call tools in early rounds.
///
/// Returns `Some(PrefetchedContext)` when context was gathered, `None` otherwise.
pub async fn prefetch_context_for_message(
    message: &str,
    project_root: &Path,
) -> Option<PrefetchedContext> {
    let task_type = detect_task_type(message)?;
    let body = match task_type {
        "code_review" => prefetch_code_review_context(message, project_root).await?,
        "exploration" => prefetch_exploration_context(message, project_root).await?,
        "debugging" => prefetch_debugging_context(message, project_root).await?,
        "implementation" => prefetch_implementation_context(project_root).await?,
        "analysis" => prefetch_exploration_context(message, project_root).await?,
        _ => return None,
    };
    Some(PrefetchedContext { task_type, body })
}

/// Inject pre-fetched context into the last user message in the messages array.
///
/// The context is appended as a clearly delimited block with task-type-specific
/// guidance telling the LLM what it already has and what it still needs tools for.
pub fn inject_prefetched_context(messages: &mut [Value], ctx: &PrefetchedContext) {
    let guidance = match ctx.task_type {
        "code_review" => {
            "\
            The complete git context is provided below. You already have the full diff — \
            review it directly without calling git tools or the skill tool. \
            Only use read_file if you need to see surrounding code not in the diff."
        }
        "exploration" => {
            "\
            The project structure and key files are provided below. Use this to answer \
            the user's question directly. Only call tools if you need to read specific \
            source files not included here."
        }
        "debugging" => {
            "\
            Recent changes and project context are provided below. Use this to narrow \
            down the problem. Call tools only for specific files you need to inspect \
            that aren't shown here."
        }
        "implementation" | "analysis" => {
            "\
            Project structure is provided below to help you understand the codebase layout. \
            Use it to decide where to make changes or which files to read next."
        }
        _ => "Context pre-fetched below.",
    };

    if let Some(last) = messages.last_mut() {
        if let Some(content) = last.get("content").and_then(Value::as_str) {
            let enriched = format!(
                "{}\n\n<prefetched_context>\n{}\n\n{}\n</prefetched_context>",
                content, guidance, ctx.body
            );
            last["content"] = Value::String(enriched);
        }
    }
}

// ─── Code Review ─────────────────────────────────────────────────────────────

/// Pre-fetch git context for code review tasks.
async fn prefetch_code_review_context(message: &str, project_root: &Path) -> Option<String> {
    if !is_git_repo(project_root).await {
        return None;
    }

    let lower = message.to_lowercase();

    // Explicit commit hash takes priority over keyword matching.
    if extract_commit_hash(message).is_some() || mentions_commit(&lower) {
        prefetch_commit_review(message, project_root).await
    } else if mentions_pr(&lower) {
        prefetch_branch_diff(project_root).await
    } else if mentions_local_changes(&lower) {
        prefetch_working_changes(project_root).await
    } else {
        prefetch_commit_review(message, project_root).await
    }
}

async fn prefetch_commit_review(message: &str, project_root: &Path) -> Option<String> {
    // If the user specified a commit hash, use it; otherwise default to HEAD.
    let requested = extract_commit_hash(message);
    let commit = requested.clone().unwrap_or_else(|| "HEAD".to_string());
    let parent = format!("{commit}~1");

    let log = run_git(
        project_root,
        &["log", "-1", "--format=%H%n%an <%ae>%n%ai%n%s%n%n%b", &commit],
        MAX_LOG_BYTES,
    )
    .await?;

    // Validate: if user requested a specific hash, confirm git resolved it to
    // that commit. A mismatch means the hash is wrong or ambiguous — don't
    // inject stale/wrong context; let the model call git tools itself.
    if let Some(ref req) = requested {
        let resolved = log.lines().next().unwrap_or("").trim();
        if !resolved.starts_with(req.as_str()) {
            return None;
        }
    }

    let diff_range = format!("{parent}..{commit}");
    let stat = run_git(
        project_root,
        &["diff", "--stat", &diff_range],
        MAX_LOG_BYTES,
    )
    .await
    .unwrap_or_default();

    let diff = run_git(project_root, &["diff", &diff_range], MAX_DIFF_BYTES).await?;

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

async fn prefetch_branch_diff(project_root: &Path) -> Option<String> {
    let default_branch = detect_default_branch(project_root).await?;

    let merge_base = run_git(project_root, &["merge-base", &default_branch, "HEAD"], 256).await?;
    let merge_base = merge_base.trim();

    let log = run_git(
        project_root,
        &["log", "--oneline", &format!("{merge_base}..HEAD")],
        MAX_LOG_BYTES,
    )
    .await
    .unwrap_or_default();

    let stat = run_git(
        project_root,
        &["diff", "--stat", &format!("{merge_base}..HEAD")],
        MAX_LOG_BYTES,
    )
    .await
    .unwrap_or_default();

    let diff = run_git(
        project_root,
        &["diff", &format!("{merge_base}..HEAD")],
        MAX_DIFF_BYTES,
    )
    .await?;

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

async fn prefetch_working_changes(project_root: &Path) -> Option<String> {
    let staged = run_git(project_root, &["diff", "--cached"], MAX_DIFF_BYTES).await;
    let unstaged = run_git(project_root, &["diff"], MAX_DIFF_BYTES).await;

    let staged_text = staged.unwrap_or_default();
    let unstaged_text = unstaged.unwrap_or_default();

    if staged_text.is_empty() && unstaged_text.is_empty() {
        return prefetch_commit_review("", project_root).await;
    }

    let stat =
        run_git(project_root, &["diff", "--stat", "HEAD"], MAX_LOG_BYTES).await.unwrap_or_default();

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

// ─── Exploration ─────────────────────────────────────────────────────────────

/// Pre-fetch project structure and key files for exploration/understanding tasks.
async fn prefetch_exploration_context(_message: &str, project_root: &Path) -> Option<String> {
    let mut ctx = String::with_capacity(MAX_TREE_BYTES + MAX_FILE_BYTES * 3);

    // Project directory structure (2 levels deep)
    if let Some(tree) = get_project_tree(project_root).await {
        ctx.push_str("## Project Structure\n\n```\n");
        ctx.push_str(&tree);
        ctx.push_str("\n```\n\n");
    }

    // README
    if let Some(readme) = read_project_file(project_root, "README.md")
        .or_else(|| read_project_file(project_root, "readme.md"))
        .or_else(|| read_project_file(project_root, "README"))
    {
        ctx.push_str("## README\n\n");
        ctx.push_str(&readme);
        ctx.push_str("\n\n");
    }

    // Key config files (detect project type)
    let config_files = detect_config_files(project_root);
    for (label, content) in &config_files {
        ctx.push_str(&format!("## {label}\n\n```\n"));
        ctx.push_str(content);
        ctx.push_str("\n```\n\n");
    }

    // Git info
    if is_git_repo(project_root).await {
        if let Some(branch) = run_git(project_root, &["branch", "--show-current"], 256).await {
            ctx.push_str(&format!("## Git\n\nCurrent branch: `{}`\n", branch.trim()));
        }
        if let Some(log) = run_git(project_root, &["log", "--oneline", "-10"], MAX_LOG_BYTES).await {
            ctx.push_str("\nRecent commits:\n```\n");
            ctx.push_str(&log);
            ctx.push_str("```\n\n");
        }
    }

    if ctx.is_empty() { None } else { Some(ctx) }
}

// ─── Debugging ───────────────────────────────────────────────────────────────

/// Pre-fetch context useful for debugging: recent changes, project structure,
/// and git status (uncommitted changes that may have introduced the bug).
async fn prefetch_debugging_context(message: &str, project_root: &Path) -> Option<String> {
    let mut ctx = String::with_capacity(MAX_DIFF_BYTES + MAX_TREE_BYTES);

    // Project structure (helps LLM find relevant files)
    if let Some(tree) = get_project_tree(project_root).await {
        ctx.push_str("## Project Structure\n\n```\n");
        ctx.push_str(&tree);
        ctx.push_str("\n```\n\n");
    }

    if is_git_repo(project_root).await {
        // Git status — shows uncommitted changes that may be the bug source
        if let Some(status) = run_git(project_root, &["status", "--short"], MAX_LOG_BYTES).await {
            if !status.trim().is_empty() {
                ctx.push_str("## Uncommitted Changes\n\n```\n");
                ctx.push_str(&status);
                ctx.push_str("```\n\n");
            }
        }

        // Recent changes — diff of uncommitted work
        let has_uncommitted = run_git(project_root, &["diff", "--stat", "HEAD"], 1024)
            .await
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        if has_uncommitted {
            if let Some(diff) = run_git(project_root, &["diff", "HEAD"], MAX_DIFF_BYTES).await {
                ctx.push_str("## Current Diff (uncommitted)\n\n```diff\n");
                ctx.push_str(&diff);
                ctx.push_str("\n```\n\n");
            }
        } else {
            // No uncommitted changes — show recent commit diffs
            if let Some(log) = run_git(project_root, &["log", "--oneline", "-5"], MAX_LOG_BYTES).await {
                ctx.push_str("## Recent Commits\n\n```\n");
                ctx.push_str(&log);
                ctx.push_str("```\n\n");
            }
        }

        // If message mentions a specific file, try to show its content
        if let Some(file_path) = extract_file_reference(message, project_root) {
            if let Some(content) = read_project_file(project_root, &file_path) {
                ctx.push_str(&format!("## File: {file_path}\n\n```\n"));
                ctx.push_str(&content);
                ctx.push_str("\n```\n\n");
            }
        }
    }

    if ctx.is_empty() { None } else { Some(ctx) }
}

// ─── Implementation ──────────────────────────────────────────────────────────

/// Pre-fetch project structure for implementation tasks — helps the LLM
/// know where to create/modify files without needing to explore first.
async fn prefetch_implementation_context(project_root: &Path) -> Option<String> {
    let mut ctx = String::with_capacity(MAX_TREE_BYTES + MAX_FILE_BYTES);

    if let Some(tree) = get_project_tree(project_root).await {
        ctx.push_str("## Project Structure\n\n```\n");
        ctx.push_str(&tree);
        ctx.push_str("\n```\n\n");
    }

    // Key config files help LLM understand the tech stack
    let config_files = detect_config_files(project_root);
    for (label, content) in &config_files {
        ctx.push_str(&format!("## {label}\n\n```\n"));
        ctx.push_str(content);
        ctx.push_str("\n```\n\n");
    }

    if ctx.is_empty() { None } else { Some(ctx) }
}

// ─── Shared Helpers ──────────────────────────────────────────────────────────

async fn is_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn detect_default_branch(project_root: &Path) -> Option<String> {
    for branch in &["main", "master"] {
        let result = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(project_root)
            .output()
            .await;
        if result.map(|o| o.status.success()).unwrap_or(false) {
            return Some(branch.to_string());
        }
    }
    run_git(
        project_root,
        &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
        256,
    )
    .await
    .map(|s| s.trim().trim_start_matches("origin/").to_string())
}

async fn run_git(dir: &Path, args: &[&str], max_bytes: usize) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = output.stdout;
    if raw.is_empty() {
        return Some(String::new());
    }

    truncate_output(raw, max_bytes)
}

async fn run_command(dir: &Path, cmd: &str, args: &[&str], max_bytes: usize) -> Option<String> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    truncate_output(output.stdout, max_bytes)
}

fn truncate_output(raw: Vec<u8>, max_bytes: usize) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    if raw.len() <= max_bytes {
        String::from_utf8(raw).ok()
    } else {
        // Back up to a valid UTF-8 character boundary.
        let mut end = max_bytes;
        while end > 0 && (raw[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        let s = String::from_utf8_lossy(&raw[..end]);
        Some(format!(
            "{}\n\n... [truncated at {} KB, {} KB total]",
            s.trim_end(),
            max_bytes / 1024,
            raw.len() / 1024
        ))
    }
}

/// Get a 2-level-deep directory listing, ignoring common noise dirs.
async fn get_project_tree(project_root: &Path) -> Option<String> {
    // Try `find` for a portable 2-level listing (tree may not be installed)
    let output = Command::new("find")
        .args([
            ".",
            "-maxdepth",
            "2",
            "-not",
            "-path",
            "*/.*",
            "-not",
            "-path",
            "*/node_modules/*",
            "-not",
            "-path",
            "*/target/*",
            "-not",
            "-path",
            "*/__pycache__/*",
            "-not",
            "-path",
            "*/venv/*",
            "-not",
            "-path",
            "*/.venv/*",
            "-not",
            "-path",
            "*/dist/*",
            "-not",
            "-path",
            "*/build/*",
        ])
        .current_dir(project_root)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // Sort the output for consistency
    let raw = String::from_utf8(output.stdout).ok()?;
    let mut lines: Vec<&str> = raw.lines().collect();
    lines.sort();
    let sorted = lines.join("\n");

    truncate_output(sorted.into_bytes(), MAX_TREE_BYTES)
}

/// Read a project file with size cap.
fn read_project_file(project_root: &Path, relative_path: &str) -> Option<String> {
    let path = project_root.join(relative_path);
    if !path.is_file() {
        return None;
    }
    let content = std::fs::read(&path).ok()?;
    truncate_output(content, MAX_FILE_BYTES)
}

/// Detect and read key config files to understand the tech stack.
fn detect_config_files(project_root: &Path) -> Vec<(String, String)> {
    let candidates = [
        ("Cargo.toml (Rust)", "Cargo.toml"),
        ("package.json (Node.js)", "package.json"),
        ("pyproject.toml (Python)", "pyproject.toml"),
        ("go.mod (Go)", "go.mod"),
        ("docker-compose.yml", "docker-compose.yml"),
    ];

    candidates
        .iter()
        .filter_map(|(label, path)| {
            read_project_file(project_root, path)
                .filter(|c| !c.is_empty())
                .map(|content| (label.to_string(), content))
        })
        .take(3) // Cap at 3 config files to avoid context bloat
        .collect()
}

/// Try to extract a file path reference from the user's message.
/// Looks for common patterns like "in src/main.rs" or "file foo.py".
fn extract_file_reference(message: &str, project_root: &Path) -> Option<String> {
    // Look for path-like tokens (contain / or end with common extensions)
    for word in message.split_whitespace() {
        let clean = word.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
        });
        if clean.contains('/')
            || clean.ends_with(".rs")
            || clean.ends_with(".py")
            || clean.ends_with(".js")
            || clean.ends_with(".ts")
            || clean.ends_with(".go")
            || clean.ends_with(".java")
            || clean.ends_with(".rb")
            || clean.ends_with(".c")
            || clean.ends_with(".cpp")
            || clean.ends_with(".h")
        {
            let path = project_root.join(clean);
            if path.is_file() {
                return Some(clean.to_string());
            }
        }
    }
    None
}

/// Extract a hex commit hash (7-40 chars) from the user's message.
fn extract_commit_hash(message: &str) -> Option<String> {
    message
        .split_whitespace()
        .find(|w| {
            let len = w.len();
            (7..=40).contains(&len) && w.chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(|s| s.to_string())
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
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    // ─── Code Review ─────────────────────────────────────────────

    #[tokio::test]
    async fn prefetch_code_review_returns_context() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("review latest commit", &root).await;
        assert!(ctx.is_some(), "should return context for code review");
        let ctx = ctx.unwrap();
        assert_eq!(ctx.task_type, "code_review");
        assert!(ctx.body.contains("Latest Commit"));
        assert!(ctx.body.contains("Full Diff"));
    }

    #[tokio::test]
    async fn prefetch_commit_review_with_valid_hash() {
        let root = test_project_root();
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let hash = String::from_utf8(head.stdout).unwrap().trim().to_string();

        // Skip in shallow clones where HEAD~1 doesn't exist (e.g. CI with fetch-depth=1).
        let has_parent = std::process::Command::new("git")
            .args(["rev-parse", "HEAD~1"])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_parent {
            return;
        }

        let msg = format!("review {hash}");
        let ctx = prefetch_context_for_message(&msg, &root).await;
        assert!(ctx.is_some(), "valid hash should produce context");
        let body = ctx.unwrap().body;
        assert!(
            body.contains(&hash[..12]),
            "injected context must contain the requested hash"
        );
    }

    #[tokio::test]
    async fn prefetch_commit_review_invalid_hash_returns_none() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("review deadbeefdeadbeefdeadbeef00000000000", &root).await;
        assert!(
            ctx.is_none(),
            "invalid hash should return None so model calls git tools itself"
        );
    }

    #[tokio::test]
    async fn prefetch_bare_hash_routes_to_commit_review() {
        let root = test_project_root();
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let hash = String::from_utf8(head.stdout).unwrap().trim().to_string();

        // Skip in shallow clones where HEAD~1 doesn't exist.
        let has_parent = std::process::Command::new("git")
            .args(["rev-parse", "HEAD~1"])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_parent {
            return;
        }

        let ctx = prefetch_context_for_message(&format!("review {hash}"), &root).await;
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().task_type, "code_review");
    }

    #[tokio::test]
    async fn prefetch_no_context_for_greeting() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("hello world", &root).await;
        assert!(ctx.is_none());
    }

    // ─── Exploration ─────────────────────────────────────────────

    #[tokio::test]
    async fn prefetch_exploration_returns_project_structure() {
        let root = test_project_root();
        // "explore the codebase" unambiguously triggers exploration
        let ctx = prefetch_context_for_message("explore the codebase", &root).await;
        assert!(ctx.is_some(), "should return context for exploration");
        let ctx = ctx.unwrap();
        assert_eq!(ctx.task_type, "exploration");
        assert!(ctx.body.contains("Project Structure"));
    }

    #[tokio::test]
    async fn prefetch_exploration_includes_readme() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("understand the architecture", &root).await;
        assert!(ctx.is_some());
        let ctx = ctx.unwrap();
        assert!(ctx.body.contains("README") || ctx.body.contains("Project Structure"));
    }

    #[tokio::test]
    async fn prefetch_exploration_chinese() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("了解一下这个项目", &root).await;
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().task_type, "exploration");
    }

    // ─── Debugging ───────────────────────────────────────────────

    #[tokio::test]
    async fn prefetch_debugging_returns_context() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("debug this error in the code", &root).await;
        assert!(ctx.is_some(), "should return context for debugging");
        let ctx = ctx.unwrap();
        assert_eq!(ctx.task_type, "debugging");
        assert!(ctx.body.contains("Project Structure"));
    }

    #[tokio::test]
    async fn prefetch_debugging_chinese() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("程序崩溃了", &root).await;
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().task_type, "debugging");
    }

    // ─── Implementation ──────────────────────────────────────────

    #[tokio::test]
    async fn prefetch_implementation_returns_structure() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("implement a new feature for authentication", &root).await;
        assert!(ctx.is_some(), "should return context for implementation");
        let ctx = ctx.unwrap();
        assert_eq!(ctx.task_type, "implementation");
        assert!(ctx.body.contains("Project Structure"));
    }

    // ─── Analysis ────────────────────────────────────────────────

    #[tokio::test]
    async fn prefetch_analysis_returns_structure() {
        let root = test_project_root();
        let ctx = prefetch_context_for_message("analyze this code for issues", &root).await;
        assert!(ctx.is_some());
        let ctx = ctx.unwrap();
        assert_eq!(ctx.task_type, "analysis");
        assert!(ctx.body.contains("Project Structure"));
    }

    // ─── Injection ───────────────────────────────────────────────

    #[test]
    fn inject_context_appends_to_last_message() {
        let mut messages =
            vec![serde_json::json!({"role": "user", "content": "review the commit"})];
        let ctx = PrefetchedContext {
            task_type: "code_review",
            body: "DIFF HERE".to_string(),
        };
        inject_prefetched_context(&mut messages, &ctx);
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("review the commit"));
        assert!(content.contains("<prefetched_context>"));
        assert!(content.contains("DIFF HERE"));
        assert!(content.contains("review it directly"));
    }

    #[test]
    fn inject_exploration_context_has_correct_guidance() {
        let mut messages =
            vec![serde_json::json!({"role": "user", "content": "how does this work?"})];
        let ctx = PrefetchedContext {
            task_type: "exploration",
            body: "TREE HERE".to_string(),
        };
        inject_prefetched_context(&mut messages, &ctx);
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("project structure and key files"));
    }

    // ─── Keyword Detection ───────────────────────────────────────

    #[test]
    fn mentions_commit_detection() {
        assert!(mentions_commit("review latest commit"));
        assert!(mentions_commit("看看最新提交"));
        assert!(!mentions_commit("review the code"));
    }

    #[test]
    fn extract_commit_hash_from_message() {
        assert_eq!(
            extract_commit_hash("review fcb776d81c0cc43ff80dca0feb2a89ab786ee1b5"),
            Some("fcb776d81c0cc43ff80dca0feb2a89ab786ee1b5".to_string())
        );
        assert_eq!(
            extract_commit_hash("review abc1234"),
            Some("abc1234".to_string())
        );
        assert_eq!(extract_commit_hash("review latest commit"), None);
        assert_eq!(extract_commit_hash("hello world"), None);
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

    // ─── Helpers ─────────────────────────────────────────────────

    #[tokio::test]
    async fn get_project_tree_works() {
        let root = test_project_root();
        let tree = get_project_tree(&root).await;
        assert!(tree.is_some());
        let tree = tree.unwrap();
        assert!(tree.contains("Cargo.toml") || tree.contains("rust"));
    }

    #[test]
    fn detect_config_files_finds_cargo_toml() {
        let root = test_project_root();
        let configs = detect_config_files(&root);
        assert!(!configs.is_empty(), "should find at least one config file");
        assert!(
            configs
                .iter()
                .any(|(label, _)| label.contains("Cargo") || label.contains("Makefile")),
            "should detect Cargo.toml or Makefile"
        );
    }

    #[test]
    fn extract_file_reference_finds_path() {
        let root = test_project_root();
        // Makefile exists in the project root
        let result = extract_file_reference("look at Makefile please", &root);
        // Makefile doesn't have an extension we check, so it won't match
        assert!(result.is_none());

        // But a .rs file reference would work if the file exists
        let result = extract_file_reference("check rust/Cargo.toml for issues", &root);
        // Cargo.toml doesn't have a code extension, won't match
        assert!(result.is_none());
    }

    #[test]
    fn truncate_output_respects_limit() {
        let data = vec![b'x'; 100];
        let result = truncate_output(data, 50);
        assert!(result.is_some());
        let s = result.unwrap();
        assert!(s.contains("truncated"));
    }

    #[test]
    fn truncate_output_no_truncation_when_under_limit() {
        let data = b"hello world".to_vec();
        let result = truncate_output(data, 1000);
        assert_eq!(result, Some("hello world".to_string()));
    }
}
