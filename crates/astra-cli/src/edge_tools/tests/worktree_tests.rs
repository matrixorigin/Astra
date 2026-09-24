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

#[test]
fn worktree_enter_pins_requested_commit_and_reports_tree() {
    let repo = init_temp_git_repo();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    let original = git(&["rev-parse", "HEAD"]);
    let tree = git(&["rev-parse", "HEAD^{tree}"]);
    std::fs::write(repo.path().join("tracked.txt"), "newer\n").unwrap();
    git(&["commit", "-am", "new head"]);
    let exe = ToolExecutor::new(repo.path());
    let result = exe.worktree_with_metadata(&json!({
        "action": "enter", "branch": "pinned-source", "source_commit": original
    }));
    assert!(!result.is_error, "{}", result.output);
    let fields = result.tool_result_fields.unwrap();
    assert_eq!(fields["source_commit"], original);
    assert_eq!(fields["source_tree"], tree);
    assert_eq!(
        std::fs::read_to_string(exe.effective_project_root().join("tracked.txt")).unwrap(),
        "committed\n"
    );
    exe.exit_worktree("remove", false).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
        "newer\n"
    );
}

#[test]
fn worktree_invalid_source_does_not_switch_session() {
    let repo = init_temp_git_repo();
    let exe = ToolExecutor::new(repo.path());
    for source in [
        json!("missing-ref"),
        json!("HEAD^{tree}"),
        json!("--help"),
        json!(""),
        json!(42),
    ] {
        let result = exe.worktree_with_metadata(&json!({
            "action": "enter", "branch": "invalid-source", "source_commit": source
        }));
        assert!(result.is_error, "{}", result.output);
        assert!(!exe.in_worktree_session());
    }
}

#[test]
fn pinned_worktrees_keep_trial_file_changes_separate() {
    let repo = init_temp_git_repo();
    let baseline = ToolExecutor::new(repo.path());
    let candidate = ToolExecutor::new(repo.path());
    let first = baseline.worktree_with_metadata(&json!({
        "action": "enter", "branch": "eval-baseline"
    }));
    assert!(!first.is_error, "{}", first.output);
    let first_fields = first.tool_result_fields.unwrap();
    let second = candidate.worktree_with_metadata(&json!({
        "action": "enter", "branch": "eval-candidate",
        "source_commit": first_fields["source_commit"]
    }));
    assert!(!second.is_error, "{}", second.output);
    let second_fields = second.tool_result_fields.unwrap();
    assert_eq!(
        first_fields["source_commit"],
        second_fields["source_commit"]
    );
    assert_eq!(first_fields["source_tree"], second_fields["source_tree"]);
    assert_ne!(
        baseline.effective_project_root(),
        candidate.effective_project_root()
    );
    std::fs::write(
        candidate.effective_project_root().join("tracked.txt"),
        "candidate\n",
    )
    .unwrap();
    for root in [baseline.effective_project_root(), repo.path().to_path_buf()] {
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "committed\n"
        );
    }
    // This proves file separation only; linked worktrees still share Git metadata.
    candidate.exit_worktree("remove", true).unwrap();
    baseline.exit_worktree("remove", false).unwrap();
}
