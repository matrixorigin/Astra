use super::ToolExecutor;
use serde_json::json;

#[cfg(target_os = "macos")]
struct KillOnDrop(i32);

#[cfg(target_os = "macos")]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

pub(crate) fn init_temp_git_repo() -> tempfile::TempDir {
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

pub(crate) fn remove_preserved_test_worktree(repo: &std::path::Path, checkout: &std::path::Path) {
    assert!(
        std::process::Command::new("git")
            .current_dir(repo)
            .args(["worktree", "remove", "--force"])
            .arg(checkout)
            .status()
            .unwrap()
            .success()
    );
}

pub(crate) fn has_authoritative_worktree_owner() -> bool {
    !cfg!(target_os = "macos")
        && astra_sandbox::BashInvocationOwner::prepare("git", &[])
            .is_ok_and(|(_, owner)| owner.can_authoritatively_observe())
}

async fn exit_and_clean_test_worktree(
    exe: &ToolExecutor,
    repo: &std::path::Path,
    checkout: &std::path::Path,
) {
    let result = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove", "discard_changes":true}),
        )
        .await;
    if !cfg!(target_os = "macos") && !has_authoritative_worktree_owner() {
        assert!(
            result.is_error && result.output.contains("may still be running"),
            "{result:?}"
        );
        let kept = exe
            .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
            .await;
        assert!(!kept.is_error, "{kept:?}");
    } else {
        assert!(!result.is_error, "{result:?}");
    }
    assert!(!exe.in_worktree_session());
    if checkout.exists() {
        remove_preserved_test_worktree(repo, checkout);
    }
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
    let checkout = exe.get_worktree_session().unwrap().worktree_path;

    let listed = exe.rollback_git_worktrees(&json!({"scope": "list"}));
    let listed_json: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed_json["entries"].as_array().map(Vec::len), Some(1));

    let cleanup = exe.worktree(&json!({
        "action": "exit",
        "exit_action": if cfg!(target_os = "macos") || has_authoritative_worktree_owner() { "remove" } else { "keep" },
        "discard_changes": true,
    }));
    assert!(!cleanup.starts_with("Error:"), "cleanup failed: {cleanup}");
    if checkout.exists() {
        remove_preserved_test_worktree(dir.path(), &checkout);
    }
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
    if cfg!(target_os = "macos") {
        assert!(!denied.is_error, "{denied:?}");
        assert!(denied.output.contains("preserved"), "{denied:?}");
        assert!(!exe.in_worktree_session());
        assert_eq!(
            std::fs::read_to_string(session.worktree_path.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        remove_preserved_test_worktree(dir.path(), &session.worktree_path);
    } else {
        assert!(denied.is_error, "{denied:?}");
        assert!(exe.in_worktree_session());
        if !has_authoritative_worktree_owner() {
            assert!(denied.output.contains("may still be running"), "{denied:?}");
        }
        exit_and_clean_test_worktree(&exe, dir.path(), &session.worktree_path).await;
    }
    let invalid = exe
        .execute_with_metadata("worktree", &json!({"action":"push", "branch":"main"}))
        .await;
    assert!(invalid.is_error);
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn removing_worktree_waits_for_other_executor_and_can_be_cancelled() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"busy-checkout"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = exe.get_worktree_session().unwrap().worktree_path;
    // This is the same workspace lease that another executor/process uses.
    let held = astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options(
        &selected,
        None,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let args = json!({"action":"exit", "exit_action":"remove", "discard_changes":true});
    let removal = exe.execute_with_metadata_cancelable("worktree", &args, Some(&cancel));
    tokio::pin!(removal);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut removal)
            .await
            .is_err()
    );
    cancel.cancel();
    let result = removal.await;
    assert!(result.is_error, "{result:?}");
    assert!(selected.exists());
    assert!(exe.in_worktree_session());
    drop(held);
    let kept = exe
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!kept.is_error, "{kept:?}");
    assert!(
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["worktree", "remove", "--force"])
            .arg(&selected)
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn session_worktree_display_sixel_uses_selected_root() {
    let dir = init_temp_git_repo();
    std::fs::write(dir.path().join("original-only.png"), b"original").unwrap();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"image-root"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = exe.get_worktree_session().unwrap().worktree_path;
    std::fs::write(selected.join("selected-only.png"), b"selected").unwrap();
    astra_tools::display_sixel::set_sixel_supported(false);
    let selected_result = exe
        .execute_with_metadata("display_sixel", &json!({"path":"selected-only.png"}))
        .await;
    assert!(
        !selected_result.output.contains("file not found"),
        "{selected_result:?}"
    );
    let original_result = exe
        .execute_with_metadata("display_sixel", &json!({"path":"original-only.png"}))
        .await;
    assert!(
        original_result.is_error && original_result.output.contains("file not found"),
        "{original_result:?}"
    );
    exit_and_clean_test_worktree(&exe, dir.path(), &selected).await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_checkout_hook_preserves_the_uncertain_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let dir = init_temp_git_repo();
    let hook = dir.path().join(".git/hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\ntouch .hook-started\ni=0\nwhile [ ! -e .hook-release ] && [ \"$i\" -lt 200 ]; do sleep .01; i=$((i+1)); done\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let checkout = dir.path().parent().unwrap().join(format!(
        "{}-wt-hook-failed",
        dir.path().file_name().unwrap().to_string_lossy()
    ));
    let exe = std::sync::Arc::new(ToolExecutor::new(dir.path()));
    let runner = exe.clone();
    let task = tokio::spawn(async move {
        runner
            .execute_with_metadata(
                "worktree",
                &json!({"action":"enter", "branch":"hook-failed"}),
            )
            .await
    });
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !checkout.join(".hook-started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if checkout.exists() {
        let _ = std::fs::write(checkout.join(".hook-release"), "");
    }
    let result = task.await.unwrap();
    if checkout.exists() {
        astra_tools::workspace_observation::confirm_workspace_stopped(&checkout)
            .await
            .unwrap();
        remove_preserved_test_worktree(dir.path(), &checkout);
    }
    assert!(started, "Git did not start the checkout hook");
    assert!(
        result.is_error && result.output.contains(&checkout.display().to_string()),
        "{result:?}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn checkout_hook_child_does_not_block_keeping_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let dir = init_temp_git_repo();
    let hook = dir.path().join(".git/hooks/post-checkout");
    let script = "import os,time\np=os.fork()\nif p:\n while not os.path.exists('.hook-child.pid'): time.sleep(.01)\nelse:\n os.setsid()\n f=os.open('/dev/null',os.O_RDWR)\n for n in (0,1,2): os.dup2(f,n)\n open('.hook-child.pid','w').write(str(os.getpid()))\n time.sleep(15)";
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\npython3 -c {}\n",
            crate::edge_tools::shell::shell_escape(script)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"hook-child"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let checkout = exe.get_worktree_session().unwrap().worktree_path;
    let pid: i32 = std::fs::read_to_string(checkout.join(".hook-child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let child = KillOnDrop(pid);
    let write = exe
        .execute_with_metadata("write_file", &json!({"path":"kept.txt", "content":"keep"}))
        .await;
    assert!(!write.is_error, "{write:?}");
    // An independent executor can still hold the checkout: keeping it does
    // not need its lease and should immediately restore the session root.
    let held = astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options(
        &checkout,
        None,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let exited = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        exe.execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove"}),
        ),
    )
    .await
    .expect("keeping a checkout should not wait for its lease");
    assert!(
        !exited.is_error && exited.output.contains("preserved"),
        "{exited:?}"
    );
    assert!(!exe.in_worktree_session());
    assert!(checkout.join("kept.txt").exists());
    drop(held);
    drop(child);
    remove_preserved_test_worktree(dir.path(), &checkout);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn session_worktree_preserves_checkout_for_escaped_foreground_child() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"escaped-child"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = exe.get_worktree_session().unwrap().worktree_path;
    let script = "import os,time\np=os.fork()\nif p == 0:\n os.setsid()\n f=os.open('/dev/null',os.O_RDWR)\n for n in (0,1,2): os.dup2(f,n)\n open('escaped.pid','w').write(str(os.getpid()))\n while not os.path.exists('release'): time.sleep(0.02)\n open('late-write','w').write('late')\nelse:\n while not os.path.exists('escaped.pid'): time.sleep(0.01)";
    let command = format!(
        "python3 -c {}",
        crate::edge_tools::shell::shell_escape(script)
    );
    let outcome = exe
        .execute_with_metadata("bash", &json!({"command":command}))
        .await;
    assert!(!outcome.is_error, "{outcome:?}");
    let pid: i32 = std::fs::read_to_string(selected.join("escaped.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let _child = KillOnDrop(pid);
    let removed = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove", "discard_changes":true}),
        )
        .await;
    assert!(
        !removed.is_error && removed.output.contains("preserved"),
        "{removed:?}"
    );
    assert!(
        selected.exists(),
        "a possibly active checkout must be retained"
    );
    assert!(!exe.in_worktree_session());
    drop(_child);
    assert!(
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["worktree", "remove", "--force"])
            .arg(&selected)
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn file_rollback_preserves_a_later_external_edit() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"weak-rollback"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = exe.get_worktree_session().unwrap().worktree_path;
    let write = exe
        .execute_with_metadata("write_file", &json!({"path":"new.txt", "content":"keep"}))
        .await;
    assert!(!write.is_error, "{write:?}");
    let shell = exe
        .execute_with_metadata("bash", &json!({"command":"pwd"}))
        .await;
    assert!(!shell.is_error, "{shell:?}");
    std::fs::write(selected.join("new.txt"), "external edit").unwrap();
    let rollback = exe
        .execute_with_metadata("rollback_file_edits", &json!({"scope":"current_turn"}))
        .await;
    assert!(rollback.is_error, "{rollback:?}");
    let body: serde_json::Value = serde_json::from_str(&rollback.output).unwrap();
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(
        std::fs::read_to_string(selected.join("new.txt")).unwrap(),
        "external edit"
    );
    let kept = exe
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!kept.is_error, "{kept:?}");
    assert!(
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["worktree", "remove", "--force"])
            .arg(&selected)
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn resume_confirmed_worktree_helper() {
    let Some(checkout) = std::env::var_os("ASTRA_TEST_RESUME_WORKTREE") else {
        return;
    };
    let checkpoints = std::env::var_os("ASTRA_TEST_RESUME_CHECKPOINTS").unwrap();
    let journal = astra_turn_core::file_edit_journal::FileEditJournal::load_from_dir(
        std::path::Path::new(&checkpoints),
        500,
    )
    .unwrap();
    let resumed = ToolExecutor::new(std::path::Path::new(&checkout))
        .with_shared_file_journal(std::sync::Arc::new(std::sync::Mutex::new(journal)));
    let restored = resumed
        .execute_with_metadata("rollback_file_edits", &json!({"scope":"current_turn"}))
        .await;
    assert!(!restored.is_error, "{restored:?}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn retained_worktree_can_resume_and_undo_file_edits() {
    let dir = init_temp_git_repo();
    let checkpoints = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    exe.file_journal
        .lock()
        .unwrap()
        .enable_persistence(checkpoints.path().to_path_buf());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"confirmed-undo"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let checkout = exe.get_worktree_session().unwrap().worktree_path;
    std::fs::write(dir.path().join("new.txt"), "original checkout").unwrap();
    let written = exe
        .execute_with_metadata(
            "write_file",
            &json!({"path":"new.txt", "content":"temporary"}),
        )
        .await;
    assert!(!written.is_error, "{written:?}");
    drop(exe);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "edge_tools::tests::worktree_tests::resume_confirmed_worktree_helper",
        ])
        .env("ASTRA_TEST_RESUME_WORKTREE", &checkout)
        .env("ASTRA_TEST_RESUME_CHECKPOINTS", checkpoints.path())
        .status()
        .unwrap();
    assert!(status.success(), "restarted worktree rollback failed");
    assert!(!checkout.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "original checkout"
    );
    remove_preserved_test_worktree(dir.path(), &checkout);
}

#[tokio::test]
async fn file_rollback_supports_a_current_turn_edit_in_the_default_temporary_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("scratch.txt");
    let exe = ToolExecutor::new(workspace.path());
    let write = exe
        .execute_with_metadata("write_file", &json!({"path": path, "content":"temporary"}))
        .await;
    assert!(!write.is_error, "{write:?}");
    let rollback = exe
        .execute_with_metadata("rollback_file_edits", &json!({"scope":"current_turn"}))
        .await;
    assert!(!rollback.is_error, "{rollback:?}");
    assert!(!path.exists());
}

#[tokio::test]
async fn file_rollback_refuses_edits_from_a_former_workspace() {
    let current = tempfile::tempdir().unwrap();
    let former = tempfile::tempdir().unwrap();
    let path = former.path().join("preserved.txt");
    let former_executor = ToolExecutor::new(former.path());
    let write = former_executor
        .execute_with_metadata(
            "write_file",
            &json!({"path":"preserved.txt", "content":"keep"}),
        )
        .await;
    assert!(!write.is_error, "{write:?}");
    let exe = ToolExecutor::new(current.path())
        .with_shared_file_journal(former_executor.file_journal.clone());
    let rollback = exe
        .execute_with_metadata("rollback_file_edits", &json!({"scope":"current_turn"}))
        .await;
    assert!(rollback.is_error, "{rollback:?}");
    let body: serde_json::Value = serde_json::from_str(&rollback.output).unwrap();
    assert_eq!(body["success"], false, "{body}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\n");
    let rollback_file = exe
        .execute_with_metadata("rollback_file_edits", &json!({"scope":"file", "path":path}))
        .await;
    assert!(rollback_file.is_error, "{rollback_file:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\n");
}

#[tokio::test]
#[serial_test::serial]
async fn resumed_session_keeps_its_selected_worktree() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let repo = init_temp_git_repo();
    let session_id = format!("worktree-resume-{}", uuid::Uuid::new_v4());
    let first = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let entered = first
        .execute_with_metadata(
            "worktree",
            &json!({"action": "enter", "branch": "resume-selected"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = first.get_worktree_session().unwrap().worktree_path;
    std::fs::write(selected.join("tracked.txt"), "worktree content\n").unwrap();

    // A new executor with no shared in-memory owner is what a resumed process
    // sees; it must not silently return to the original project directory.
    let resumed = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let resumed_root = resumed.effective_project_root();
    let read = resumed
        .execute_with_metadata("read_file", &json!({"path": "tracked.txt"}))
        .await;
    let other = ToolExecutor::new(repo.path()).with_active_session_id("other-session");
    let other_root = other.effective_project_root();

    exit_and_clean_test_worktree(&first, repo.path(), &selected).await;
    assert_eq!(resumed_root, selected);
    assert_eq!(other_root, repo.path());
    assert!(read.output.contains("worktree content"), "{read:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn exiting_after_resume_inside_worktree_restores_the_session_root() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let repo = init_temp_git_repo();
    let session_id = format!("worktree-resume-inside-{}", uuid::Uuid::new_v4());
    let first = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let entered = first
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"resume-inside"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = first.get_worktree_session().unwrap().worktree_path;
    let resumed = ToolExecutor::new(&selected).with_active_session_id(&session_id);
    let exited = resumed
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!exited.is_error, "{exited:?}");
    let written = resumed
        .execute_with_metadata(
            "write_file",
            &json!({"path":"after-exit.txt", "content":"original"}),
        )
        .await;
    assert!(!written.is_error, "{written:?}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("after-exit.txt")).unwrap(),
        "original\n"
    );
    assert!(!selected.join("after-exit.txt").exists());
    let restarted = ToolExecutor::new(&selected).with_active_session_id(&session_id);
    assert_eq!(
        restarted.effective_project_root().canonicalize().unwrap(),
        repo.path().canonicalize().unwrap()
    );
    exit_and_clean_test_worktree(&first, repo.path(), &selected).await;
}

#[tokio::test]
#[serial_test::serial]
async fn missing_resumed_worktree_requires_an_explicit_exit() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let repo = init_temp_git_repo();
    let session_id = format!("worktree-missing-{}", uuid::Uuid::new_v4());
    let first = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let entered = first
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"resume-missing"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = first.get_worktree_session().unwrap().worktree_path;
    std::fs::rename(selected.join(".git"), selected.join(".git.saved")).unwrap();

    let invalidated = first
        .execute_with_metadata("read_file", &json!({"path":"tracked.txt"}))
        .await;

    let resumed = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let read = resumed
        .execute_with_metadata("read_file", &json!({"path":"tracked.txt"}))
        .await;
    // An invalid query checks admission without making a network request.
    let web = resumed
        .execute_with_metadata("web_search", &json!({}))
        .await;
    std::fs::rename(selected.join(".git.saved"), selected.join(".git")).unwrap();
    let repaired = resumed
        .execute_with_metadata("read_file", &json!({"path":"tracked.txt"}))
        .await;
    let exited = resumed
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    let again = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let restored_root = again.effective_project_root();

    exit_and_clean_test_worktree(&first, repo.path(), &selected).await;
    assert!(
        invalidated.is_error
            && invalidated
                .output
                .contains("selected worktree is unavailable"),
        "{invalidated:?}"
    );
    assert!(
        read.is_error && read.output.contains("selected worktree is unavailable"),
        "{read:?}"
    );
    assert!(
        !web.output.contains("selected worktree is unavailable"),
        "{web:?}"
    );
    assert!(
        !repaired.is_error && repaired.output.contains("committed"),
        "{repaired:?}"
    );
    assert!(!exited.is_error, "{exited:?}");
    assert_eq!(restored_root, repo.path());
}

#[tokio::test]
#[serial_test::serial]
async fn replacement_worktree_from_same_repo_is_not_silently_selected() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let repo = init_temp_git_repo();
    let session_id = format!("worktree-replaced-{}", uuid::Uuid::new_v4());
    let executor = ToolExecutor::new(repo.path()).with_active_session_id(&session_id);
    let entered = executor
        .execute_with_metadata(
            "worktree",
            &json!({
                "action":"enter", "branch":"original-selection"
            }),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = executor.get_worktree_session().unwrap().worktree_path;
    let moved = selected.with_extension("moved");
    assert!(
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "move"])
            .arg(&selected)
            .arg(&moved)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "add", "-b", "replacement-selection"])
            .arg(&selected)
            .status()
            .unwrap()
            .success()
    );
    let result = executor
        .execute_with_metadata("read_file", &json!({"path":"tracked.txt"}))
        .await;
    assert!(
        result.is_error && result.output.contains("replaced"),
        "{result:?}"
    );
    let exited = executor
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!exited.is_error, "{exited:?}");
    remove_preserved_test_worktree(repo.path(), &selected);
    remove_preserved_test_worktree(repo.path(), &moved);
}

#[tokio::test]
#[serial_test::serial]
async fn replacement_during_workspace_confirmation_never_runs_the_pending_write() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let repo = init_temp_git_repo();
    let session_id = format!("worktree-confirm-replaced-{}", uuid::Uuid::new_v4());
    let executor =
        std::sync::Arc::new(ToolExecutor::new(repo.path()).with_active_session_id(&session_id));
    let entered = executor
        .execute_with_metadata(
            "worktree",
            &json!({
                "action":"enter", "branch":"confirmation-original"
            }),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let selected = executor.get_worktree_session().unwrap().worktree_path;
    let moved = selected.with_extension("moved");
    assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(&selected));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    executor.set_ask_user_request_tx(Some(tx));
    let writer = executor.clone();
    let pending = tokio::spawn(async move {
        writer
            .execute_with_metadata(
                "write_file",
                &json!({"path":"must-not-exist", "content":"unexpected"}),
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "move"])
            .arg(&selected)
            .arg(&moved)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "add", "-b", "confirmation-replacement"])
            .arg(&selected)
            .status()
            .unwrap()
            .success()
    );
    answer_workspace_recovery(prompt, "It has stopped");
    let result = pending.await.unwrap();
    let replacement_written = selected.join("must-not-exist").exists();
    let original_written = moved.join("must-not-exist").exists();

    remove_preserved_test_worktree(repo.path(), &selected);
    assert!(
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "move"])
            .arg(&moved)
            .arg(&selected)
            .status()
            .unwrap()
            .success()
    );
    astra_tools::workspace_observation::confirm_workspace_stopped(&selected)
        .await
        .unwrap();
    exit_and_clean_test_worktree(&executor, repo.path(), &selected).await;
    assert!(result.is_error, "{result:?}");
    assert!(!replacement_written && !original_written);
}

#[tokio::test]
#[serial_test::serial]
async fn direct_executor_recovers_corrupt_workspace_after_user_confirmation() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "visible").unwrap();
    let session_id = format!("worktree-corrupt-direct-{}", uuid::Uuid::new_v4());
    let metadata = astra_services::session_workspace::WorkspaceMetadata::with_context(
        &session_id,
        "model",
        &root.path().to_string_lossy(),
        None,
    );
    astra_services::session_workspace::write_workspace(&metadata).unwrap();
    let workspace = astra_services::session_workspace::workspace_file_path(&session_id).unwrap();
    std::fs::write(&workspace, "invalid: [").unwrap();
    let executor =
        std::sync::Arc::new(ToolExecutor::new(root.path()).with_active_session_id(&session_id));
    let blocked = executor
        .execute_with_metadata("read_file", &json!({"path":"file.txt"}))
        .await;
    assert!(blocked.is_error, "{blocked:?}");
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    executor.set_ask_user_request_tx(Some(tx));
    let rebind = executor.clone();
    let pending = tokio::spawn(async move {
        rebind
            .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    answer_workspace_recovery(prompt, "Use this directory");
    let result = pending.await.unwrap();
    let read = executor
        .execute_with_metadata("read_file", &json!({"path":"file.txt"}))
        .await;
    assert!(
        !result.is_error && !read.is_error && read.output.contains("visible"),
        "{result:?}, {read:?}"
    );
    assert!(astra_services::session_workspace::read_workspace(&session_id).is_ok());
    assert!(
        std::fs::read_dir(workspace.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("workspace.yaml.corrupt-")
            })
    );
}

#[tokio::test]
#[serial_test::serial]
async fn active_session_reuses_loaded_workspace_state_across_turns() {
    let (_sessions, _guard) = crate::tests::isolated_sessions_dir();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "contents").unwrap();
    let session_id = format!("worktree-cache-{}", uuid::Uuid::new_v4());
    let metadata = astra_services::session_workspace::WorkspaceMetadata::with_context(
        &session_id,
        "model",
        &root.path().to_string_lossy(),
        None,
    );
    astra_services::session_workspace::write_workspace(&metadata).unwrap();
    let first = ToolExecutor::new(root.path()).with_active_session_id(&session_id);
    let workspace = astra_services::session_workspace::workspace_file_path(&session_id).unwrap();
    std::fs::write(workspace, "invalid: [").unwrap();

    let next = ToolExecutor::new(root.path())
        .with_shared_git_worktree_journal(first.git_worktree_journal.clone())
        .with_active_session_id(&session_id);
    let read = next
        .execute_with_metadata("read_file", &json!({"path":"file.txt"}))
        .await;
    let fresh = ToolExecutor::new(root.path()).with_active_session_id(&session_id);
    let blocked = fresh
        .execute_with_metadata("read_file", &json!({"path":"file.txt"}))
        .await;

    assert!(
        !read.is_error && read.output.contains("contents"),
        "{read:?}"
    );
    assert!(
        blocked.is_error && blocked.output.contains("workspace metadata is unavailable"),
        "{blocked:?}"
    );
}

#[tokio::test]
async fn unsettled_workspace_explains_how_to_recover_without_running_tools() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(dir.path()));
    for (name, args) in [
        (
            "write_file",
            json!({"path":"must-not-exist", "content":"test"}),
        ),
        ("bash", json!({"command":"touch must-not-exist"})),
    ] {
        let result = exe.execute_with_metadata(name, &args).await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("astra workspace confirm-stopped"),
            "{result:?}"
        );
        assert!(
            result.output.contains(&dir.path().display().to_string()),
            "{result:?}"
        );
    }
    assert!(!dir.path().join("must-not-exist").exists());
    astra_tools::workspace_observation::confirm_workspace_stopped(dir.path())
        .await
        .unwrap();
}

fn answer_workspace_recovery(request: crate::cli::chat_stream::AskUserRequest, answer: &str) {
    let question = &request.prompt.questions[0];
    request
        .response_tx
        .send(crate::cli::chat_stream::AskUserResponse::Submitted(
            astra_tools::AskUserAnswers {
                answers: vec![astra_tools::AskUserQuestionAnswer {
                    id: question.id.clone(),
                    question: question.question.clone(),
                    answers: vec![answer.into()],
                    multi_select: false,
                    annotation: None,
                }],
            },
        ))
        .unwrap();
}

#[tokio::test]
async fn interactive_workspace_recovery_resumes_the_same_executor() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(dir.path()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    exe.set_ask_user_request_tx(Some(tx));
    assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(dir.path()));

    let writer = exe.clone();
    let first = tokio::spawn(async move {
        writer
            .execute_with_metadata(
                "write_file",
                &json!({"path":"first.txt", "content":"first"}),
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("recovery prompt was not shown")
        .unwrap();
    let second = exe.clone();
    let parallel = tokio::spawn(async move {
        second
            .execute_with_metadata(
                "write_file",
                &json!({"path":"second.txt", "content":"second"}),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !parallel.is_finished(),
        "parallel work should wait for the decision"
    );
    assert!(rx.try_recv().is_err());
    answer_workspace_recovery(prompt, "It has stopped");
    let result = first.await.unwrap();
    assert!(!result.is_error, "{result:?}");
    assert!(dir.path().join("first.txt").exists());
    let parallel = parallel.await.unwrap();
    assert!(!parallel.is_error, "{parallel:?}");
    assert!(dir.path().join("second.txt").exists());
}

#[tokio::test]
async fn shell_recovers_each_unsettled_workspace_before_running() {
    let primary = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(primary.path()));
    exe.expand_sandbox_path(extra.path().to_path_buf()).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    exe.set_ask_user_request_tx(Some(tx));
    for root in [primary.path(), extra.path()] {
        assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(root));
    }

    let run = |exe: std::sync::Arc<ToolExecutor>| {
        tokio::spawn(async move {
            exe.execute_with_metadata("bash", &json!({"command":"touch completed"}))
                .await
        })
    };
    let first = run(exe.clone());
    for answer in ["It has stopped", "Not sure"] {
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!primary.path().join("completed").exists());
        answer_workspace_recovery(prompt, answer);
    }
    assert!(first.await.unwrap().is_error);
    assert!(!primary.path().join("completed").exists());

    let second = run(exe);
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    answer_workspace_recovery(prompt, "It has stopped");
    let result = second.await.unwrap();
    assert!(!result.is_error, "{result:?}");
    assert!(primary.path().join("completed").exists());
}

#[tokio::test]
async fn shell_does_not_start_if_a_confirmed_workspace_becomes_unsettled_again() {
    let primary = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(primary.path()));
    exe.expand_sandbox_path(extra.path().to_path_buf()).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    exe.set_ask_user_request_tx(Some(tx));
    for root in [primary.path(), extra.path()] {
        assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(root));
    }
    let worker = exe.clone();
    let attempt = tokio::spawn(async move {
        worker
            .execute_with_metadata("bash", &json!({"command":"touch completed"}))
            .await
    });
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    answer_workspace_recovery(first, "It has stopped");
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(primary.path())
    );
    answer_workspace_recovery(second, "It has stopped");
    let result = attempt.await.unwrap();
    assert!(result.is_error, "{result:?}");
    assert!(!primary.path().join("completed").exists());
    astra_tools::workspace_observation::confirm_workspace_stopped(primary.path())
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn shell_rejects_workdir_moved_outside_workspace_during_recovery() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("nested")).unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(workspace.path()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    exe.set_ask_user_request_tx(Some(tx));
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(workspace.path())
    );
    let worker = exe.clone();
    let attempt = tokio::spawn(async move {
        worker
            .execute_with_metadata(
                "bash",
                &json!({"command":"touch must-not-run", "workdir":"nested"}),
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    std::fs::rename(
        workspace.path().join("nested"),
        outside.path().join("moved"),
    )
    .unwrap();
    answer_workspace_recovery(prompt, "It has stopped");
    let result = attempt.await.unwrap();
    assert!(result.is_error, "{result:?}");
    assert!(!outside.path().join("moved/must-not-run").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn script_recovers_each_workspace_before_platform_admission() {
    let primary = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(primary.path()));
    exe.expand_sandbox_path(extra.path().to_path_buf()).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    exe.set_ask_user_request_tx(Some(tx));
    for root in [primary.path(), extra.path()] {
        assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(root));
    }
    let worker = exe.clone();
    let attempt = tokio::spawn(async move {
        worker
            .execute_with_metadata(
                "run_script",
                &json!({"script":"open('completed','w').write('yes')"}),
            )
            .await
    });
    for answer in ["It has stopped", "Not sure"] {
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        answer_workspace_recovery(prompt, answer);
    }
    assert!(attempt.await.unwrap().is_error);
    assert!(!primary.path().join("completed").exists());

    let worker = exe.clone();
    let attempt = tokio::spawn(async move {
        worker
            .execute_with_metadata(
                "run_script",
                &json!({"script":"open('completed','w').write('yes')"}),
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    answer_workspace_recovery(prompt, "It has stopped");
    let result = attempt.await.unwrap();
    assert!(
        result.is_error,
        "macOS cannot prove the script process boundary: {result:?}"
    );
    assert!(!result.output.contains("confirm-stopped"), "{result:?}");
    assert!(!primary.path().join("completed").exists());
}

#[tokio::test]
async fn workspace_recovery_honors_admission_deadline() {
    let primary = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(primary.path()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    exe.set_ask_user_request_tx(Some(tx));
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(primary.path())
    );
    let worker = exe.clone();
    let attempt = tokio::spawn(async move {
        let invocation = astra_tools::tool_engine::ToolInvocationMetadata {
            admission_deadline: Some(
                std::time::Instant::now() + std::time::Duration::from_millis(200),
            ),
            ..Default::default()
        };
        worker
            .execute_with_invocation_metadata_cancelable(
                "bash",
                &json!({"command":"touch completed"}),
                invocation,
                None,
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_error, "{result:?}");
    assert!(!primary.path().join("completed").exists());
    assert!(prompt.response_tx.is_closed());
    assert_eq!(
        astra_tools::workspace_observation::workspace_ownership_is_unsettled(primary.path()),
        Some(true)
    );
    astra_tools::workspace_observation::confirm_workspace_stopped(primary.path())
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_recovery_waits_cancellably_and_keeps_distinct_roots_separate() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(first_root.path()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    exe.set_ask_user_request_tx(Some(tx));
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(first_root.path())
    );
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(
            second_root.path()
        )
    );
    let worker = exe.clone();
    let root = first_root.path().to_path_buf();
    let first =
        tokio::spawn(async move { worker.confirm_workspace_recovery_in_tui(&root, None).await });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let worker = exe.clone();
    let root = first_root.path().to_path_buf();
    let child_cancel = cancel.clone();
    let cancelled = tokio::spawn(async move {
        worker
            .confirm_workspace_recovery_in_tui(&root, Some(&child_cancel))
            .await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert!(!cancelled.await.unwrap());
    let worker = exe.clone();
    let root = second_root.path().to_path_buf();
    let second =
        tokio::spawn(async move { worker.confirm_workspace_recovery_in_tui(&root, None).await });
    tokio::task::yield_now().await;
    answer_workspace_recovery(prompt, "It has stopped");
    assert!(first.await.unwrap());
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        prompt
            .prompt
            .context
            .as_deref()
            .unwrap()
            .contains(&second_root.path().display().to_string())
    );
    answer_workspace_recovery(prompt, "It has stopped");
    assert!(second.await.unwrap());
}

#[tokio::test]
async fn declined_workspace_recovery_keeps_files_and_allows_reading() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "keep").unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(dir.path()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    exe.set_ask_user_request_tx(Some(tx));
    assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(dir.path()));
    let writer = exe.clone();
    let attempt = tokio::spawn(async move {
        writer
            .execute_with_metadata(
                "write_file",
                &json!({"path":"existing.txt", "content":"changed"}),
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("recovery prompt was not shown")
        .unwrap();
    let other = exe.clone();
    let mut parallel = tokio::spawn(async move {
        other
            .execute_with_metadata(
                "write_file",
                &json!({"path":"other.txt", "content":"changed"}),
            )
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut parallel)
            .await
            .is_err()
    );
    answer_workspace_recovery(prompt, "Not sure");
    assert!(attempt.await.unwrap().is_error);
    assert!(parallel.await.unwrap().is_error);
    assert!(
        rx.try_recv().is_err(),
        "the same decline must not trigger another prompt"
    );
    assert!(!dir.path().join("other.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
        "keep"
    );
    let read = exe
        .execute_with_metadata("read_file", &json!({"path":"existing.txt"}))
        .await;
    assert!(!read.is_error && read.output.contains("keep"), "{read:?}");
    astra_tools::workspace_observation::confirm_workspace_stopped(dir.path())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelling_workspace_recovery_leaves_the_session_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::sync::Arc::new(ToolExecutor::new(dir.path()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    exe.set_ask_user_request_tx(Some(tx));
    assert!(astra_tools::workspace_observation::mark_workspace_observation_unsettled(dir.path()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let worker = exe.clone();
    let child_cancel = cancel.clone();
    let attempt = tokio::spawn(async move {
        worker
            .execute_with_metadata_cancelable(
                "write_file",
                &json!({"path":"cancelled.txt", "content":"not written"}),
                Some(&child_cancel),
            )
            .await
    });
    let prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("recovery prompt was not shown")
        .unwrap();
    cancel.cancel();
    let result = attempt.await.unwrap();
    assert!(result.is_error, "{result:?}");
    assert!(!dir.path().join("cancelled.txt").exists());
    drop(prompt);
    astra_tools::workspace_observation::confirm_workspace_stopped(dir.path())
        .await
        .expect("cancelled prompt must release the recovery lease");
    let retried = exe
        .execute_with_metadata("write_file", &json!({"path":"after.txt", "content":"ok"}))
        .await;
    assert!(!retried.is_error, "{retried:?}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn cross_process_main_workspace_rollback_helper() {
    let Some(root) = std::env::var_os("ASTRA_TEST_ROLLBACK_WORKSPACE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let target_root = std::env::var_os("ASTRA_TEST_ROLLBACK_TARGET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let path = target_root.join("shared.txt");
    let before = std::fs::read(&path).unwrap();
    let exe = ToolExecutor::new(&root);
    if target_root != root {
        exe.expand_sandbox_path(target_root).unwrap();
    }
    exe.file_journal
        .lock()
        .unwrap()
        .record_committed_in_workspace(
            &root,
            &path,
            "edit",
            0,
            (Some(b"original"), b"agent edit\n"),
            astra_turn_core::file_edit_journal::EditType::Overwrite,
        );
    let args = if std::env::var_os("ASTRA_TEST_ROLLBACK_TARGET").is_some() {
        json!({"scope":"file", "path":path.to_string_lossy()})
    } else {
        json!({"scope":"current_turn"})
    };
    let outcome = exe
        .execute_with_metadata("rollback_file_edits", &args)
        .await;
    if std::env::var_os("ASTRA_TEST_ROLLBACK_EXPECT_SUCCESS").is_some() {
        assert!(!outcome.is_error, "{outcome:?}");
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    } else {
        assert!(outcome.is_error, "{outcome:?}");
        let body: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        if body["failed"]
            .as_array()
            .is_some_and(|failed| !failed.is_empty())
        {
            assert!(
                body["summary"].as_str().unwrap().contains("failure"),
                "{body}"
            );
        }
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn cross_process_rollback_preserves_later_edits() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let script = "import os,time\np=os.fork()\nif p:\n while not os.path.exists('escaped.pid'): time.sleep(.01)\nelse:\n os.setsid()\n f=os.open('/dev/null',os.O_RDWR)\n for n in (0,1,2): os.dup2(f,n)\n open('shared.txt','w').write('child progress')\n open('escaped.pid','w').write(str(os.getpid()))\n while not os.path.exists('release'): time.sleep(.02)";
    let command = format!(
        "python3 -c {}",
        crate::edge_tools::shell::shell_escape(script)
    );
    let result = exe
        .execute_with_metadata("bash", &json!({"command":command}))
        .await;
    assert!(!result.is_error, "{result:?}");
    let pid: i32 = std::fs::read_to_string(dir.path().join("escaped.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let child = KillOnDrop(pid);
    let run_helper = |expect_success: bool| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "edge_tools::tests::worktree_tests::cross_process_main_workspace_rollback_helper",
            ])
            .env("ASTRA_TEST_ROLLBACK_WORKSPACE", dir.path());
        if expect_success {
            command.env("ASTRA_TEST_ROLLBACK_EXPECT_SUCCESS", "1");
        }
        assert!(command.status().unwrap().success());
    };
    run_helper(false);
    drop(child);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test child did not exit");
    astra_tools::workspace_observation::confirm_workspace_stopped(dir.path())
        .await
        .unwrap();
    run_helper(false); // The stopped child's later edit must still be preserved.
    std::fs::write(dir.path().join("shared.txt"), "agent edit\n").unwrap();
    run_helper(true);
}

#[tokio::test]
async fn submodule_file_rollback_remains_available() {
    let parent = init_temp_git_repo();
    let source = init_temp_git_repo();
    let added = std::process::Command::new("git")
        .args(["-c", "protocol.file.allow=always", "submodule", "add"])
        .arg(source.path())
        .arg("child")
        .current_dir(parent.path())
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let submodule = parent.path().join("child");
    let exe = ToolExecutor::new(&submodule);
    let write = exe
        .execute_with_metadata(
            "write_file",
            &json!({"path":"new.txt", "content":"temporary"}),
        )
        .await;
    assert!(!write.is_error, "{write:?}");
    let rollback = exe
        .execute_with_metadata("rollback_file_edits", &json!({"scope":"current_turn"}))
        .await;
    assert!(!rollback.is_error, "{rollback:?}");
    assert!(!submodule.join("new.txt").exists());
}

#[tokio::test]
async fn session_worktree_is_the_workspace_for_file_tools() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata("worktree", &json!({"action":"enter", "branch":"file-root"}))
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let active = exe.get_worktree_session().unwrap().worktree_path;
    std::fs::write(active.join("tracked.txt"), "worktree content\n").unwrap();
    std::fs::write(dir.path().join("example.rs"), "fn original_symbol() {}\n").unwrap();
    std::fs::write(active.join("example.rs"), "fn worktree_symbol() {}\n").unwrap();

    let read = exe
        .execute_with_metadata("read_file", &json!({"path":"tracked.txt"}))
        .await;
    let write = exe
        .execute_with_metadata(
            "write_file",
            &json!({"path":"new.txt", "content":"new content\n"}),
        )
        .await;
    let wrote_active = active.join("new.txt").exists();
    let wrote_original = dir.path().join("new.txt").exists();
    let glob = exe
        .execute_with_metadata("glob", &json!({"pattern":"new.txt"}))
        .await;
    let grep = exe
        .execute_with_metadata(
            "grep",
            &json!({"pattern":"worktree content", "output_mode":"content"}),
        )
        .await;
    let bash = exe
        .execute_with_metadata("bash", &json!({"command":"pwd -P"}))
        .await;
    let symbols = exe
        .execute_with_metadata("symbols", &json!({"path":"example.rs"}))
        .await;
    let definition = exe
        .execute_with_metadata("find_definition", &json!({"symbol":"worktree_symbol"}))
        .await;
    let canonical_active = active.canonicalize().unwrap();
    let next_turn = ToolExecutor::new(dir.path())
        .with_shared_git_worktree_journal(exe.git_worktree_journal.clone());
    let next_turn_root = next_turn.effective_project_root();
    let next_read = next_turn
        .execute_with_metadata("read_file", &json!({"path":"tracked.txt"}))
        .await;
    let next_symbols = next_turn
        .execute_with_metadata("symbol_search", &json!({"query":"worktree_symbol"}))
        .await;

    exit_and_clean_test_worktree(&exe, dir.path(), &active).await;
    assert!(
        !read.is_error
            && read.output.contains("worktree content")
            && !write.is_error
            && wrote_active
            && !wrote_original
            && !glob.is_error
            && glob.output.contains("new.txt")
            && !grep.is_error
            && grep.output.contains("worktree content")
            && !bash.is_error
            && bash
                .output
                .contains(canonical_active.to_string_lossy().as_ref())
            && next_turn_root == active,
        "read={read:?}, write={write:?}, glob={glob:?}, grep={grep:?}, bash={bash:?}, \
         wrote_active={wrote_active}, wrote_original={wrote_original}, \
         next_turn_root={next_turn_root:?}, active={active:?}"
    );
    assert!(
        !next_read.is_error && next_read.output.contains("worktree content"),
        "{next_read:?}"
    );
    for result in [&symbols, &definition, &next_symbols] {
        assert!(
            !result.is_error
                && result.output.contains("worktree_symbol")
                && !result.output.contains("original_symbol"),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn session_worktree_checks_out_requested_commit_and_cleans_against_it() {
    let dir = init_temp_git_repo();
    let initial = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let initial = String::from_utf8(initial.stdout).unwrap().trim().to_owned();
    std::fs::write(dir.path().join("tracked.txt"), "newer\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-am", "second"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"at-revision", "start_point":&initial}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let active = exe.get_worktree_session().unwrap();
    assert_eq!(
        active.original_head_commit.as_deref(),
        Some(initial.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(active.worktree_path.join("tracked.txt")).unwrap(),
        "committed\n"
    );
    exit_and_clean_test_worktree(&exe, dir.path(), &active.worktree_path).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "newer\n"
    );
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn session_worktree_preserves_ignored_files_without_discard() {
    let dir = init_temp_git_repo();
    std::fs::write(dir.path().join(".git/info/exclude"), "ignored-output\n").unwrap();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"ignored-work"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let active = exe.get_worktree_session().unwrap().worktree_path;
    std::fs::write(active.join("ignored-output"), "data\n").unwrap();
    let refused = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove"}),
        )
        .await;
    let preserved = active.join("ignored-output").exists();
    assert!(refused.is_error && preserved, "{refused:?}");
    exit_and_clean_test_worktree(&exe, dir.path(), &active).await;
}

#[tokio::test]
async fn cancelled_worktree_switch_does_not_wait_for_active_workspace_operation() {
    let dir = init_temp_git_repo();
    let exe = std::sync::Arc::new(ToolExecutor::new(dir.path()));
    let active_operation = exe.worktree_transition.read().await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = {
        let exe = exe.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            exe.execute_with_invocation_metadata_cancelable(
                "worktree",
                &json!({"action":"enter", "branch":"cancelled-switch"}),
                astra_tools::tool_engine::ToolInvocationMetadata::default(),
                Some(&cancel),
            )
            .await
        })
    };
    tokio::task::yield_now().await;
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("cancelled switch must return while the operation is still active")
        .unwrap();
    assert!(outcome.is_error, "{outcome:?}");
    assert!(!exe.in_worktree_session());
    drop(active_operation);
}

#[tokio::test]
async fn busy_workspace_rejects_switch_until_its_operation_finishes() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let active_operation = exe.worktree_transition.read().await;
    let enter = json!({"action":"enter", "branch":"busy-switch"});
    let rejected = exe.execute_with_metadata("worktree", &enter).await;
    assert!(rejected.is_error, "{rejected:?}");
    assert!(rejected.output.contains("still in use"), "{rejected:?}");
    assert_eq!(
        rejected.tool_result_fields.as_ref().unwrap()["execution_started"],
        false
    );
    assert!(!exe.in_worktree_session());
    drop(active_operation);
    let accepted = exe.execute_with_metadata("worktree", &enter).await;
    assert!(!accepted.is_error, "{accepted:?}");
    let active = exe.get_worktree_session().unwrap().worktree_path;
    let written = exe
        .execute_with_metadata(
            "write_file",
            &json!({"path":"temporary.txt", "content":"keep until rollback"}),
        )
        .await;
    assert!(!written.is_error, "{written:?}");
    let active_operation = exe.worktree_transition.read().await;
    let rejected = exe
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(rejected.is_error, "{rejected:?}");
    assert!(active.join("temporary.txt").exists());
    drop(active_operation);
    let kept = exe
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!kept.is_error, "{kept:?}");
    remove_preserved_test_worktree(dir.path(), &active);
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn session_worktree_preserves_a_branch_reset_to_an_ancestor() {
    let dir = init_temp_git_repo();
    std::fs::write(dir.path().join("tracked.txt"), "second\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-am", "second"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({
                "action":"enter", "branch":"reset-history"
            }),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let active = exe.get_worktree_session().unwrap().worktree_path;
    assert!(
        std::process::Command::new("git")
            .args(["reset", "--hard", "HEAD~1"])
            .current_dir(&active)
            .status()
            .unwrap()
            .success()
    );
    let rejected = exe
        .execute_with_metadata(
            "worktree",
            &json!({
                "action":"exit", "exit_action":"remove"
            }),
        )
        .await;
    assert!(rejected.is_error, "{rejected:?}");
    assert!(active.exists());
    exit_and_clean_test_worktree(&exe, dir.path(), &active).await;
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn session_worktree_preserves_commits_on_a_detached_branch() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({
                "action":"enter", "branch":"detached-history"
            }),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let session = exe.get_worktree_session().unwrap();
    std::fs::write(session.worktree_path.join("tracked.txt"), "new commit\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-am", "new commit"])
            .current_dir(&session.worktree_path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "checkout",
                "--detach",
                session.original_head_commit.as_deref().unwrap()
            ])
            .current_dir(&session.worktree_path)
            .status()
            .unwrap()
            .success()
    );
    let rejected = exe
        .execute_with_metadata(
            "worktree",
            &json!({
                "action":"exit", "exit_action":"remove"
            }),
        )
        .await;
    assert!(rejected.is_error, "{rejected:?}");
    assert!(session.worktree_path.exists());
    exit_and_clean_test_worktree(&exe, dir.path(), &session.worktree_path).await;
}

#[test]
fn user_interrupt_local_fence_follows_actual_sse_executor_not_native_executor() {
    let original = tempfile::tempdir().unwrap();
    let active = tempfile::tempdir().unwrap();
    let native = std::sync::Arc::new(ToolExecutor::new(original.path()));
    let observer = crate::cli::chat_stream::CliExecutorObserver::default();
    // This factory returns the same Arc installed in CliServerAdmissionHost.
    let actual = crate::cli::chat_stream::retain_cli_executor(
        ToolExecutor::new(original.path()),
        Some(&observer),
    )
    .unwrap();
    assert!(std::sync::Arc::ptr_eq(&actual, observer.get().unwrap()));
    assert!(!std::sync::Arc::ptr_eq(&actual, &native));
    // Worktree entry mutates only the ordinary executor after publication.
    *actual.worktree_session.lock().unwrap() = Some(crate::edge_tools::WorktreeSession {
        worktree_path: active.path().to_path_buf(),
        branch_name: "synthetic-worktree".into(),
        original_root: original.path().to_path_buf(),
        original_head_commit: None,
        git_identity: String::new(),
        recovery_error: None,
    });
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(active.path())
    );
    assert_eq!(
        astra_tools::workspace_observation::workspace_ownership_is_unsettled(
            &native.effective_project_root()
        ),
        Some(false)
    );
    let observed = observer.get().unwrap().as_ref();
    let result = crate::cli::command_router::settle_one_shot_user_interrupt_result(
        Ok(crate::StreamResult::default()),
        Some(observed),
        Some(&native),
        true,
        &Default::default(),
    );
    assert!(
        result
            .unwrap_err()
            .error
            .contains("workspace_ownership_unsettled")
    );
    let closed = crate::TurnFailure {
        error: "stdout closed".into(),
        partial: crate::PartialTurnData {
            output_transport_failure: Some(
                crate::cli::stream::streaming_types::OutputTransportFailure::Closed,
            ),
            ..Default::default()
        },
    };
    assert!(
        !crate::cli::command_router::one_shot_stdout_closure_is_settled(
            &closed,
            true,
            true,
            Some(observed),
            Some(&native),
        )
    );
    // A former worktree's sticky fence cannot block the original unrelated root.
    actual.worktree_session.lock().unwrap().take();
    assert!(
        crate::cli::command_router::settle_one_shot_user_interrupt_result(
            Ok(crate::StreamResult::default()),
            Some(observed),
            Some(&native),
            true,
            &Default::default(),
        )
        .is_ok()
    );
    assert!(
        crate::cli::command_router::one_shot_stdout_closure_is_settled(
            &closed,
            true,
            true,
            Some(observed),
            Some(&native),
        )
    );
    // Native scope remains a separate negative barrier, never the ordinary owner.
    assert!(
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(original.path())
    );
    assert!(
        crate::cli::command_router::settle_one_shot_user_interrupt_result(
            Ok(crate::StreamResult::default()),
            None,
            Some(&native),
            true,
            &Default::default(),
        )
        .is_err()
    );
    assert!(
        !crate::cli::command_router::one_shot_stdout_closure_is_settled(
            &closed,
            true,
            true,
            None,
            Some(&native),
        )
    );
}
