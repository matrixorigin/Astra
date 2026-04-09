use super::*;

// ── extract_github_owner_repo edge cases ──

#[test]
fn extract_github_owner_repo_without_git_suffix() {
    let line = "origin\thttps://github.com/MatrixOrigin/Memoria (fetch)";
    assert_eq!(
        super::extract_github_owner_repo(line),
        Some("MatrixOrigin/Memoria".to_string())
    );
}

#[test]
fn extract_github_owner_repo_malformed_url() {
    assert_eq!(super::extract_github_owner_repo("origin"), None);
    assert_eq!(super::extract_github_owner_repo(""), None);
    assert_eq!(
        super::extract_github_owner_repo("origin\thttps://not-github.com/a/b.git (fetch)"),
        None
    );
}

#[test]
fn extract_github_owner_repo_ssh_no_dot_git() {
    let line = "upstream\tgit@github.com:org/repo (push)";
    assert_eq!(
        super::extract_github_owner_repo(line),
        Some("org/repo".to_string())
    );
}

// ── detect_git_remote_repos ──

#[test]
fn detect_git_remote_repos_from_current_dir() {
    // This test runs in the actual repo — should find at least one remote
    let repos = super::detect_git_remote_repos(std::path::Path::new("."));
    // We're in the mo-dev-agent repo, so at least one GitHub remote should exist
    // (unless running in a non-git context, in which case empty is acceptable)
    for repo in &repos {
        assert!(repo.contains('/'), "repo should be owner/name: {repo}");
        assert_eq!(repo, &repo.to_lowercase(), "should be lowercased: {repo}");
    }
}

#[test]
fn detect_git_remote_repos_nonexistent_dir() {
    let repos = super::detect_git_remote_repos(std::path::Path::new("/nonexistent/path"));
    assert!(repos.is_empty());
}

#[test]
fn detect_git_remote_repos_deduplicates() {
    // The same remote appears for both fetch and push — should be deduplicated
    // This is an implicit invariant; verify by checking no duplicates
    let repos = super::detect_git_remote_repos(std::path::Path::new("."));
    let mut seen = std::collections::HashSet::new();
    for repo in &repos {
        assert!(
            seen.insert(repo.as_str()),
            "duplicate preferred repo: {repo}"
        );
    }
}

// ── add_preferred_repo / get_preferred_repos ──

#[test]
fn add_preferred_repo_deduplicates() {
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
}

#[test]
fn add_preferred_repo_normalizes_case() {
    let exec = test_executor();
    exec.add_preferred_repo("MatrixOrigin/Memoria");
    let repos = exec.get_preferred_repos();
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
fn worktree_session_initially_none() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    assert!(!exe.in_worktree_session());
    assert!(exe.get_worktree_session().is_none());
}

#[test]
fn effective_project_root_returns_original_when_no_session() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    assert_eq!(exe.effective_project_root(), dir.path());
}

#[test]
fn enter_worktree_requires_branch() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.enter_worktree("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("required"));
}

#[test]
fn enter_worktree_rejects_shell_injection() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    for dangerous in &["test;rm", "test|cat", "test&", "test`id`", "$(whoami)"] {
        let result = exe.enter_worktree(dangerous);
        assert!(
            result.is_err(),
            "should reject dangerous branch: {dangerous}"
        );
        assert!(result.unwrap_err().contains("Invalid"));
    }
}

#[test]
fn exit_worktree_fails_when_not_in_session() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.exit_worktree("keep", false);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Not in a worktree session"));
}

#[test]
fn git_worktree_enter_requires_branch() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.git_worktree(&json!({"action": "enter"}));
    assert!(result.contains("Error"));
    assert!(result.contains("branch"));
}

#[test]
fn git_worktree_exit_when_not_in_session() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.git_worktree(&json!({"action": "exit"}));
    assert!(result.contains("Error"));
    assert!(result.contains("Not in a worktree session"));
}
