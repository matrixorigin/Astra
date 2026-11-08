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
