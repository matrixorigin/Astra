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
    policy.allowed_paths.clear(); // This fixture intentionally grants only the workspace.
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
        .allowed_paths
        .push(authorized.path().to_path_buf());
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
    let output = super::run_shell_output_with_config(config).unwrap();
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
    let (slot, _listener) = astra_tools::detach::new_slot_with_handle();
    let executor = workdir_executor(&root).with_bash_detach_slot(slot);
    let outcome = executor
        .bash_detachable_with_metadata(
            &serde_json::json!({"command":"pwd", "workdir":"nested"}),
            astra_tools::tool_engine::ToolInvocationMetadata::default(),
            None,
        )
        .await
        .expect("pwd enters detachable launch mode");
    assert!(!outcome.is_error, "{outcome:?}");
    assert_eq!(outcome.output.trim(), root.join("nested").to_str().unwrap());
    assert_eq!(
        outcome.tool_result_fields.unwrap()["bash_workdir"]["resolved_workspace_relative"],
        "nested"
    );
}

#[tokio::test]
#[serial_test::serial(source_preimage_env)]
async fn cli_bash_workdir_uses_pinned_inferred_sources_and_root_declared_sources() {
    let storage = tempfile::tempdir().unwrap();
    let old = std::env::var_os("_ASTRA_SOURCE_PREIMAGE_ROOT");
    // SAFETY: serialized test-local source storage configuration.
    unsafe {
        std::env::set_var("_ASTRA_SOURCE_PREIMAGE_ROOT", storage.path());
    }
    let mut outcomes = Vec::new();
    for explicit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("input.bin"), "root source").unwrap();
        std::fs::write(root.join("nested/input.bin"), "nested source").unwrap();
        let executor = workdir_executor(&root)
            .with_active_session_id(format!("workdir-source-{}", uuid::Uuid::new_v4()));
        let args = if explicit {
            serde_json::json!({"command":"true", "workdir":"nested", "source_artifacts":["input.bin"]})
        } else {
            serde_json::json!({"command":"cp input.bin output.bin", "workdir":"nested"})
        };
        let result = executor.execute_with_metadata("bash", &args).await;
        let nested_copy = std::fs::read_to_string(root.join("nested/output.bin")).ok();
        outcomes.push((explicit, result, nested_copy));
    }
    // A selected worktree is a different root from the original repository.
    // Capture and restore must retain the same pinned root identity.
    let original = tempfile::tempdir().unwrap();
    let selected = tempfile::tempdir().unwrap();
    std::fs::create_dir(selected.path().join("nested")).unwrap();
    std::fs::write(original.path().join("input.bin"), "original repository").unwrap();
    std::fs::write(
        selected.path().join("nested/input.bin"),
        "selected original",
    )
    .unwrap();
    let executor =
        workdir_executor(original.path()).with_active_session_id("workdir-active-source");
    *executor.worktree_session.lock().unwrap() = Some(super::super::worktree::WorktreeSession {
        worktree_path: selected.path().to_path_buf(),
        branch_name: "fixture".into(),
        original_root: original.path().to_path_buf(),
        original_head_commit: None,
    });
    let args = serde_json::json!({"command":"cp input.bin output.bin", "workdir":"nested"});
    let workdir = astra_tools::shell_ops::resolve_bash_workdir(selected.path(), &args).unwrap();
    let mut plan = super::prepare_source_preimages(
        &executor,
        &args,
        astra_tools::tool_engine::ToolInvocationMetadata::default(),
        true,
        &workdir,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.receipt.entries[0].path, "nested/input.bin");
    std::fs::write(
        selected.path().join("nested/input.bin"),
        "selected modified",
    )
    .unwrap();
    let receipt = plan.finish();
    assert_eq!(receipt["source_preimage"]["restore_available"], true);
    let restore = plan.restore();
    let restored = std::fs::read_to_string(selected.path().join("nested/input.bin")).unwrap();
    let original_unchanged = std::fs::read_to_string(original.path().join("input.bin")).unwrap();
    // SAFETY: restore serialized environment before asserting results.
    unsafe {
        match old {
            Some(value) => std::env::set_var("_ASTRA_SOURCE_PREIMAGE_ROOT", value),
            None => std::env::remove_var("_ASTRA_SOURCE_PREIMAGE_ROOT"),
        }
    }
    restore.expect("selected worktree capture and restore must agree on the pinned root");
    assert_eq!(restored, "selected original");
    assert_eq!(original_unchanged, "original repository");
    for (explicit, result, nested_copy) in outcomes {
        assert!(!result.is_error, "{result:?}");
        let fields = result.tool_result_fields.unwrap();
        assert_eq!(
            fields["source_preimage"]["entries"][0]["path"],
            if explicit {
                "input.bin"
            } else {
                "nested/input.bin"
            }
        );
        if !explicit {
            assert_eq!(nested_copy.as_deref(), Some("nested source"));
        }
    }
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
