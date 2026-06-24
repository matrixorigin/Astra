use super::{ToolExecutor, detect_git_remote_repos, extract_github_owner_repo, test_executor};
use serde_json::json;

fn init_temp_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp repo");
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
    std::fs::write(dir.path().join("tracked.txt"), "committed\n").expect("seed tracked file");
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

// ── extract_github_owner_repo edge cases ──

#[test]
fn extract_github_owner_repo_parsing() {
    // HTTPS without .git suffix
    let line = "origin\thttps://github.com/MatrixOrigin/Memoria (fetch)";
    assert_eq!(
        extract_github_owner_repo(line),
        Some("MatrixOrigin/Memoria".to_string())
    );

    // SSH without .git
    let ssh = "upstream\tgit@github.com:org/repo (push)";
    assert_eq!(extract_github_owner_repo(ssh), Some("org/repo".to_string()));

    // Malformed / non-GitHub URLs
    assert_eq!(extract_github_owner_repo("origin"), None);
    assert_eq!(extract_github_owner_repo(""), None);
    assert_eq!(
        extract_github_owner_repo("origin\thttps://not-github.com/a/b.git (fetch)"),
        None
    );
}

// ── detect_git_remote_repos ──

#[test]
fn detect_git_remote_repos_basics() {
    // From current repo — should find at least one remote
    let repos = detect_git_remote_repos(std::path::Path::new("."));
    for repo in &repos {
        assert!(repo.contains('/'), "repo should be owner/name: {repo}");
        assert_eq!(repo, &repo.to_lowercase(), "should be lowercased: {repo}");
    }
    // No duplicates (same remote appears for fetch and push)
    let mut seen = std::collections::HashSet::new();
    for repo in &repos {
        assert!(
            seen.insert(repo.as_str()),
            "duplicate preferred repo: {repo}"
        );
    }

    // Nonexistent dir → empty
    assert!(detect_git_remote_repos(std::path::Path::new("/nonexistent/path")).is_empty());
}

// ── add_preferred_repo / get_preferred_repos ──

#[test]
fn add_preferred_repo_deduplicates_and_normalizes() {
    let exec = test_executor();
    exec.add_preferred_repo("MatrixOrigin/Memoria");
    exec.add_preferred_repo("MatrixOrigin/Memoria");
    exec.add_preferred_repo("matrixorigin/memoria"); // same after lowercasing
    let repos = exec.get_preferred_repos();
    let memoria_count = repos
        .iter()
        .filter(|r| r == &"matrixorigin/memoria")
        .count();
    assert_eq!(
        memoria_count, 1,
        "should deduplicate case-insensitively: {repos:?}"
    );
    // also: normalized to lowercase
    assert!(
        repos.contains(&"matrixorigin/memoria".to_string()),
        "should lowercase: {repos:?}"
    );
}

#[test]
fn preferred_repos_initialized_from_git_remote() {
    // test_executor uses "." as root; if in a git repo, should have remotes
    let exec = test_executor();
    let repos = exec.get_preferred_repos();
    // Can't assert specific content, but structure should be valid
    for repo in &repos {
        assert!(repo.contains('/'), "malformed: {repo}");
    }
}

// ── Worktree session tests ────────────────────────────────────────────────

#[test]
fn worktree_session_initial_state() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    assert!(!exe.in_worktree_session());
    assert!(exe.get_worktree_session().is_none());
    assert_eq!(exe.effective_project_root(), dir.path());
}

#[test]
fn enter_and_exit_worktree_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // empty branch
    let result = exe.enter_worktree("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("required"));

    // shell injection
    for dangerous in &["test;rm", "test|cat", "test&", "test`id`", "$(whoami)"] {
        let result = exe.enter_worktree(dangerous);
        assert!(
            result.is_err(),
            "should reject dangerous branch: {dangerous}"
        );
        assert!(result.unwrap_err().contains("Invalid"));
    }

    // exit when not in session
    let result = exe.exit_worktree("keep", false);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Not in a worktree session"));
}

#[test]
fn git_worktree_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // enter without branch
    let result = exe.git_worktree(&json!({"action": "enter"}));
    assert!(result.contains("Error"));
    assert!(result.contains("branch"));

    // exit when not in session
    let result = exe.git_worktree(&json!({"action": "exit"}));
    assert!(result.contains("Error"));
    assert!(result.contains("Not in a worktree session"));
}

#[tokio::test]
async fn git_worktree_enter_records_rollback_handle() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    exe.journal_turn_index
        .store(7, std::sync::atomic::Ordering::Relaxed);

    let outcome = exe.git_worktree_with_metadata(&json!({
        "action": "enter",
        "branch": "session-demo",
    }));
    assert!(
        !outcome.output.starts_with("Error:"),
        "enter failed: {}",
        outcome.output
    );
    assert!(exe.in_worktree_session(), "should enter worktree session");

    let listed = exe
        .rollback_recorded_turn_mutations(&json!({"scope": "list"}))
        .await;
    let listed_json: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed_json["total_git_worktree_entries"].as_u64(), Some(1));

    let cleanup = exe.git_worktree(&json!({
        "action": "exit",
        "exit_action": "remove",
        "discard_changes": true,
    }));
    assert!(!cleanup.starts_with("Error:"), "cleanup failed: {cleanup}");
}
