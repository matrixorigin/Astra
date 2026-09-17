use super::ToolExecutor;
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

// ── detect_git_remote_repos ──

// ── add_preferred_repo / get_preferred_repos ──

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
    let result = exe.worktree(&json!({"action": "enter"}));
    assert!(result.contains("Error"));
    assert!(result.contains("branch"));

    // exit when not in session
    let result = exe.worktree(&json!({"action": "exit"}));
    assert!(result.contains("Error"));
    assert!(result.contains("Not in a worktree session"));
}

#[tokio::test]
async fn git_worktree_enter_records_rollback_handle() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    exe.journal_turn_index
        .store(7, std::sync::atomic::Ordering::Relaxed);

    let outcome = exe.worktree_with_metadata(&json!({
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

    let cleanup = exe.worktree(&json!({
        "action": "exit",
        "exit_action": "remove",
        "discard_changes": true,
    }));
    assert!(!cleanup.starts_with("Error:"), "cleanup failed: {cleanup}");
}

#[tokio::test]
async fn session_worktree_tool_enters_and_exits_through_public_dispatch() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"session-lifecycle"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let session = exe.get_worktree_session().expect("session switched");
    assert!(session.worktree_path.join("tracked.txt").exists());
    // A dirty linked worktree must not be deleted through an incomplete or ignored status query.
    std::fs::write(session.worktree_path.join("tracked.txt"), "changed\n").unwrap();
    let denied = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove"}),
        )
        .await;
    assert!(denied.is_error, "{denied:?}");
    assert!(exe.in_worktree_session());
    let exited = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove", "discard_changes":true}),
        )
        .await;
    assert!(!exited.is_error, "{exited:?}");
    assert!(!exe.in_worktree_session());
    assert!(!session.worktree_path.exists());
    let invalid = exe
        .execute_with_metadata("worktree", &json!({"action":"push", "branch":"main"}))
        .await;
    assert!(invalid.is_error);
}
