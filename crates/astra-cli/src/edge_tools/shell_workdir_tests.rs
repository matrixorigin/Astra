fn workdir_executor(root: &std::path::Path) -> ToolExecutor {
    let executor = test_executor_in(root);
    executor.set_current_visible_tool_schemas(&super::super::all_tool_schemas());
    *executor.sandbox_policy.write().unwrap() =
        Some(astra_runtime::tool_sandbox::SandboxPolicy::permissive(root));
    executor
}

#[tokio::test]
async fn cli_bash_workdir_is_call_scoped_and_reports_actual_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let nested = root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let executor = workdir_executor(&root);
    let result = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({"command":"pwd", "workdir":"nested"}),
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output.trim(), nested.to_str().unwrap());
    let fields = result.tool_result_fields.unwrap();
    assert_eq!(fields["bash_workdir"]["requested"], "nested");
    assert_eq!(
        fields["bash_workdir"]["resolved_workspace_relative"],
        "nested"
    );
    let result = executor
        .execute_with_metadata("bash", &serde_json::json!({"command":"pwd"}))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output.trim(), root.to_str().unwrap());
    let result = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({"command":"printf marker > marker.txt", "workdir":"nested"}),
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(nested.join("marker.txt")).unwrap(),
        "marker"
    );
    assert!(!root.join("marker.txt").exists());
}

#[tokio::test]
async fn cli_bash_invalid_workdir_never_starts_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::write(root.join("file"), "file").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.parent().unwrap(), root.join("escape")).unwrap();
    let executor = workdir_executor(&root);
    let mut policy = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
    policy.clear_allowed_paths();
    *executor.sandbox_policy.write().unwrap() = Some(policy);
    let mut invalid = vec![
        serde_json::json!("missing"),
        serde_json::json!("file"),
        serde_json::json!(""),
        serde_json::json!(".."),
        serde_json::json!(23),
        serde_json::json!({}),
    ];
    #[cfg(unix)]
    invalid.push(serde_json::json!("escape"));
    for workdir in invalid {
        let result = executor.execute_with_metadata("bash", &serde_json::json!({"command":"printf forbidden > should-not-exist", "workdir":workdir})).await;
        assert!(result.is_error, "{result:?}");
        if result
            .output
            .contains("not authorized by the current workspace policy")
        {
            assert!(result.output.contains("no command was run"), "{result:?}");
            assert!(result.output.contains("Omit `workdir`"), "{result:?}");
        }
        let fields = result.tool_result_fields.unwrap();
        assert_eq!(fields["execution_started"], false);
        assert_eq!(fields["disposition"], "rejected");
        assert!(!root.join("should-not-exist").exists());
    }
}

#[tokio::test]
async fn cli_bash_workdir_read_provenance_tracks_the_nested_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("note.txt"), "root-original").unwrap();
    std::fs::write(root.join("nested/note.txt"), "nested-original").unwrap();
    let executor = workdir_executor(&root);
    let result = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({"command":"cat note.txt", "workdir":"nested"}),
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output.trim(), "nested-original");
    assert!(
        executor
            .check_staleness(&root.join("nested/note.txt"))
            .is_ok()
    );
    assert!(executor.check_staleness(&root.join("note.txt")).is_err());
}

#[test]
fn cli_bash_workdir_validation_keeps_the_original_workspace_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::create_dir(root.join("sibling")).unwrap();
    std::fs::write(root.join("sibling/note.txt"), "inside").unwrap();
    let executor = test_executor_in(&root);
    let mut policy = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
    policy.clear_allowed_paths(); // This fixture intentionally grants only the workspace.
    *executor.sandbox_policy.write().unwrap() = Some(policy);
    assert!(
        executor
            .prepare_bash_invocation(
                &serde_json::json!({"command":"cat ../sibling/note.txt", "workdir":"nested"}),
                None
            )
            .is_ok()
    );
    assert!(
        executor
            .prepare_bash_invocation(
                &serde_json::json!({"command":"cat ../../outside.txt", "workdir":"nested"}),
                None
            )
            .is_err()
    );
    assert!(
        executor
            .prepare_bash_invocation(
                &serde_json::json!({"command":"pwd", "workdir":root.join("nested")}),
                None
            )
            .is_ok()
    );
    assert_eq!(
        executor
            .sandbox_policy
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .project_root,
        root
    );
    let authorized = tempfile::tempdir().unwrap();
    std::fs::write(authorized.path().join("ordinary.txt"), "authorized").unwrap();
    executor
        .sandbox_policy
        .write()
        .unwrap()
        .as_mut()
        .unwrap()
        .add_allowed_path(authorized.path())
        .unwrap();
    assert!(executor.prepare_bash_invocation(&serde_json::json!({
        "command":format!("cat {}", super::shell_escape(authorized.path().join("ordinary.txt").to_str().unwrap())), "workdir":"nested"
    }), None).is_ok(), "workdir cannot revoke an explicitly authorized command operand");
}

#[cfg(unix)]
#[test]
fn cli_bash_workdir_launch_uses_the_pinned_directory_after_path_retarget() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/value"), "pinned original").unwrap();
    let executor = workdir_executor(&root);
    let (command, timeout, workdir) = executor
        .prepare_bash_invocation(
            &serde_json::json!({"command":"cat value", "workdir":"nested"}),
            None,
        )
        .unwrap();
    let mut config = executor.shell_run_config("bash", "-c", &command, timeout, true, None);
    config.workdir = Some(workdir);
    std::fs::rename(root.join("nested"), root.join("original-moved")).unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/value"), "wrong replacement").unwrap();
    let output = super::run_shell_output_with_config(config, None, None).unwrap();
    assert!(output.output.status.success());
    assert_eq!(
        String::from_utf8(output.output.stdout).unwrap(),
        "pinned original"
    );
}

#[tokio::test]
async fn cli_bash_workdir_is_honored_by_the_detachable_launcher() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    let executor = workdir_executor(&root);
    let outcome = executor
        .bash_outcome_with_cancel_async(
            &serde_json::json!({"command":"pwd", "workdir":"nested"}),
            astra_tools::tool_engine::ToolInvocationMetadata::default(),
            None,
        )
        .await;
    assert!(!outcome.is_error, "{outcome:?}");
    assert_eq!(outcome.output.trim(), root.join("nested").to_str().unwrap());
    assert_eq!(
        outcome.tool_result_fields.unwrap()["bash_workdir"]["resolved_workspace_relative"],
        "nested"
    );
}

#[tokio::test]
async fn cli_bash_workdir_evidence_survives_a_started_nonzero_result() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    let executor = workdir_executor(&root);
    let result = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({"command":"printf partial; exit 7", "workdir":"nested"}),
        )
        .await;
    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("partial"));
    let fields = result.tool_result_fields.unwrap();
    assert_eq!(fields["exit_code"], 7);
    assert_eq!(fields["execution_started"], true);
    assert_eq!(
        fields["bash_workdir"]["resolved_workspace_relative"],
        "nested"
    );
}

#[tokio::test]
async fn cli_bash_workdir_preserves_permissive_literal_command_policy() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("nested")).unwrap();
    let executor = workdir_executor(directory.path());
    let result = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({"command":"printf netcat", "workdir":"nested"}),
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output.trim(), "netcat");
    // Directory preparation must not import the separate shared executor's
    // blanket command bans into the CLI's existing permissive policy.
    assert!(
        executor
            .prepare_bash_invocation(
                &serde_json::json!({"command":"rm -rf ordinary-target", "workdir":"nested"}),
                None
            )
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cli_bash_workdir_read_registration_preserves_authorized_external_and_symlink_reads() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    let external = outside.path().join("ordinary.txt");
    std::fs::write(&external, "external original\n").unwrap();
    std::fs::write(root.join("referent.txt"), "linked original\n").unwrap();
    std::os::unix::fs::symlink("../referent.txt", root.join("nested/link.txt")).unwrap();
    let executor = workdir_executor(&root);
    for (command, path, old, new) in [
        (
            format!("cat {}", super::shell_escape(external.to_str().unwrap())),
            external.clone(),
            "external original",
            "external edited",
        ),
        (
            "cat link.txt".into(),
            root.join("nested/link.txt"),
            "linked original",
            "linked edited",
        ),
    ] {
        let result = executor
            .execute_with_metadata(
                "bash",
                &serde_json::json!({"command":command, "workdir":"nested"}),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output.trim(), old);
        assert!(
            executor.check_staleness(&path).is_ok(),
            "successful authorized read must remain registered: {}",
            path.display()
        );
        let edited = executor
            .execute_with_metadata(
                "str_replace",
                &serde_json::json!({"path":path,"old_str":old,"new_str":new}),
            )
            .await;
        assert!(!edited.is_error, "{edited:?}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), format!("{new}\n"));
    }
}

#[tokio::test]
async fn cli_bash_authorized_external_worktree_preserves_checkout_and_session_root() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("project");
    let review = temporary.path().join("review");
    let denied = temporary.path().join("ungranted's checkout");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&denied).unwrap();
    std::fs::write(denied.join("note"), "not authorized").unwrap();
    let git = |directory: &std::path::Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&root, &["init", "--quiet"]);
    git(&root, &["commit", "--quiet", "--allow-empty", "-m", "base"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            review.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::write(review.join("checkout.txt"), "review checkout").unwrap();
    git(&review, &["add", "checkout.txt"]);
    git(&review, &["commit", "--quiet", "-m", "review"]);
    let revision = git(&review, &["rev-parse", "HEAD"]);
    let review = review.canonicalize().unwrap();
    let root = root.canonicalize().unwrap();
    for permissive in [true, false] {
        let executor = workdir_executor(&root);
        if !permissive {
            let mut policy = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
            policy
                .set_allowed_paths(std::slice::from_ref(&review))
                .unwrap();
            *executor.sandbox_policy.write().unwrap() = Some(policy);
        }
        let command = format!(
            "git rev-parse HEAD; git diff --name-only {base}...HEAD; cat checkout.txt; printf ran > executed.txt"
        );
        let result = executor
            .execute_with_metadata(
                "bash",
                &serde_json::json!({
                    "command":command, "workdir":review
                }),
            )
            .await;
        assert!(!result.is_error, "permissive={permissive}: {result:?}");
        assert!(result.output.contains(&revision), "{result:?}");
        assert!(result.output.contains("checkout.txt"), "{result:?}");
        assert!(result.output.contains("review checkout"), "{result:?}");
        assert!(
            executor
                .check_staleness(&review.join("checkout.txt"))
                .is_ok()
        );
        assert_eq!(
            std::fs::read_to_string(review.join("executed.txt")).unwrap(),
            "ran"
        );
        assert!(!root.join("executed.txt").exists());
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["bash_workdir"]["resolved"],
            review.to_str().unwrap()
        );
        let default = executor
            .execute_with_metadata("bash", &serde_json::json!({"command":"pwd -P"}))
            .await;
        assert!(!default.is_error, "{default:?}");
        assert_eq!(default.output.trim(), root.to_str().unwrap());
        assert_eq!(executor.effective_project_root(), root);
        if !permissive {
            for args in [
                serde_json::json!({"command":"printf forbidden > not-started", "workdir":denied}),
                serde_json::json!({"command":format!("cat {}", super::shell_escape(denied.join("note").to_str().unwrap())), "workdir":review}),
            ] {
                let rejected = executor.execute_with_metadata("bash", &args).await;
                assert!(rejected.is_error, "{rejected:?}");
                let fields = rejected.tool_result_fields.unwrap();
                assert_eq!(fields["execution_started"], false);
                assert_eq!(fields["disposition"], "rejected");
                assert_eq!(
                    fields[crate::sandbox_retry::SANDBOX_APPROVAL_DIRECTORY_FIELD],
                    denied.canonicalize().unwrap().to_str().unwrap().to_string()
                );
                assert!(!denied.join("not-started").exists());
            }
        }
    }
}

#[test]
fn cli_bash_external_cwd_cannot_move_outside_its_grant_before_launch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    let allowed = directory.path().join("allowed");
    let moved = directory.path().join("moved");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&allowed).unwrap();
    let executor = workdir_executor(&root);
    let mut policy = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
    policy
        .set_allowed_paths(std::slice::from_ref(&allowed))
        .unwrap();
    *executor.sandbox_policy.write().unwrap() = Some(policy);
    let (command, timeout, workdir) = executor
        .prepare_bash_invocation(
            &serde_json::json!({"command":"printf forbidden > marker", "workdir":allowed}),
            None,
        )
        .unwrap();
    let mut config = executor.shell_run_config("bash", "-c", &command, timeout, true, None);
    config.workdir = Some(workdir);
    std::fs::rename(&allowed, &moved).unwrap();
    std::fs::create_dir(&allowed).unwrap();
    let error = super::run_shell_output_with_config(config, None, None)
        .err()
        .expect("grant no longer covers the pinned directory");
    assert!(!error.execution_started);
    assert!(!allowed.join("marker").exists());
    assert!(!moved.join("marker").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn cli_bash_non_utf8_cwd_keeps_the_known_result() {
    use std::os::unix::ffi::OsStrExt;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    let target = temporary
        .path()
        .join(std::ffi::OsStr::from_bytes(b"review-\xff"));
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&target).unwrap();
    let alias = root.join("review");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let executor = workdir_executor(&root);
    let result = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({
                "command":"printf known-result", "workdir":alias
            }),
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "known-result");
    assert!(result.tool_result_fields.unwrap()["bash_workdir"]["resolved"].is_null());
    let denied = temporary.path().join("ungranted");
    std::fs::write(&denied, "must-not-read").unwrap();
    std::os::unix::fs::symlink(&denied, target.join("link")).unwrap();
    let mut policy = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
    policy
        .set_allowed_paths(std::slice::from_ref(&target))
        .unwrap();
    *executor.sandbox_policy.write().unwrap() = Some(policy);
    let rejected = executor
        .execute_with_metadata(
            "bash",
            &serde_json::json!({
                "command":"cat link; printf forbidden > marker", "workdir":alias
            }),
        )
        .await;
    assert!(rejected.is_error, "{rejected:?}");
    assert_eq!(
        rejected.tool_result_fields.unwrap()["execution_started"],
        false
    );
    assert!(!rejected.output.contains("must-not-read"));
    assert!(!target.join("marker").exists());
}

#[test]
fn cli_bash_moved_cwd_rechecks_relative_operands_before_launch() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("project");
    let grant = temporary.path().join("grant");
    let original = grant.join("deep/cwd");
    let moved = grant.join("cwd");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(&original).unwrap();
    std::fs::write(grant.join("target"), "allowed").unwrap();
    std::fs::write(temporary.path().join("target"), "ungranted").unwrap();
    let executor = workdir_executor(&root);
    let mut policy = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
    policy.set_allowed_paths(&[grant]).unwrap();
    *executor.sandbox_policy.write().unwrap() = Some(policy);
    let (command, timeout, workdir) = executor.prepare_bash_invocation(
        &serde_json::json!({"command":"cat ../../target; printf forbidden > marker", "workdir":original}), None,
    ).unwrap();
    let mut config = executor.shell_run_config("bash", "-c", &command, timeout, true, None);
    config.workdir = Some(workdir);
    std::fs::rename(&original, &moved).unwrap();
    let error = super::run_shell_output_with_config(config, None, None)
        .err()
        .expect("moved cwd changes the relative operand's authority");
    assert!(!error.execution_started);
    assert!(!moved.join("marker").exists());
    let outcome = error.into_preparation_outcome();
    let fields = outcome.tool_result_fields.unwrap();
    assert_eq!(fields["status"], "rejected");
    assert_eq!(
        fields[crate::sandbox_retry::SANDBOX_APPROVAL_DIRECTORY_FIELD],
        temporary.path().canonicalize().unwrap().to_str().unwrap()
    );
    let callback = astra_thin_client::ToolResultRequest::new_with_hash(
        astra_thin_client::ToolResultRequestParts {
            session_id: "session".into(),
            run_id: "run".into(),
            turn_chain_id: "chain".into(),
            request_id: "moved".into(),
            edge_agent_id: "edge".into(),
            status: "rejected".into(),
            output: outcome.output,
            duration_ms: 0,
            tool_result_fields: Some(fields),
        },
    );
    let wire = serde_json::to_value(callback).unwrap();
    assert_eq!(
        wire["tool_result_fields"][crate::sandbox_retry::SANDBOX_APPROVAL_DIRECTORY_FIELD],
        temporary.path().canonicalize().unwrap().to_str().unwrap()
    );
}
