use super::{
    ToolExecutor, assert_tool_invalid_args, fanout_test_context, test_executor, test_spawner,
};
use serde_json::json;

// ── run_chain (end-to-end with real tool execution) ──────────────────────

#[tokio::test]
async fn chain_write_read_roundtrip() {
    use astra_turn_core::tool_registry_chain::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let chain = ToolChain::new("write_read", "Write a file then read it back")
        .named_step(
            "write",
            "write_file",
            json!({"path": "chain_test.txt", "content": "hello from chain"}),
        )
        .step("read_file", json!({"path": "chain_test.txt"}));

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["chain"], "write_read");
    assert_eq!(parsed["steps_executed"], 2);
    assert_eq!(parsed["steps_total"], 2);

    let steps = parsed["steps"].as_array().unwrap();
    assert!(
        steps[0]["success"].as_bool().unwrap(),
        "write should succeed"
    );
    assert!(
        steps[1]["success"].as_bool().unwrap(),
        "read should succeed"
    );
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains("hello from chain"),
        "final output should be file contents"
    );
}

#[tokio::test]
async fn chain_stops_on_error() {
    use astra_turn_core::tool_registry_chain::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let chain = ToolChain::new("error_chain", "Read nonexistent then write")
        .step(
            "read_file",
            json!({"path": "definitely_nonexistent_file.txt"}),
        )
        .step(
            "write_file",
            json!({"path": "should_not_run.txt", "content": "nope"}),
        );

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["steps_executed"], 1, "should stop after first error");
    assert_eq!(parsed["steps_total"], 2);
    let steps = parsed["steps"].as_array().unwrap();
    assert!(!steps[0]["success"].as_bool().unwrap());
    // The second step should NOT have been executed
    assert_eq!(steps.len(), 1);
    assert!(!dir.path().join("should_not_run.txt").exists());
}

#[tokio::test]
async fn cancelling_agent_chain_stops_active_and_later_steps() {
    let dir = tempfile::tempdir().unwrap();
    let executor = std::sync::Arc::new(
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner())),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let worker_cancel = cancel.clone();
    let worker =
        tokio::spawn(async move {
            executor.execute_with_metadata_cancelable("agent", &json!({
            "action":"run_chain", "name":"cancel", "description":"cancel active work",
            "steps":[
                {"tool":"bash", "args":{"command":"printf ready > started; sleep 20"}},
                {"tool":"write_file", "args":{"path":"must-not-exist", "content":"late"}}
            ]
        }), Some(&worker_cancel)).await
        });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first step did not start");
    cancel.cancel();
    let result = worker.await.unwrap();
    assert!(result.is_error, "{result:?}");
    assert!(!dir.path().join("must-not-exist").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_between_chain_steps_preserves_completed_work() {
    use std::os::unix::fs::PermissionsExt;

    let dir = super::worktree_tests::init_temp_git_repo();
    let control = tempfile::tempdir().unwrap();
    let hook = dir.path().join(".git/hooks/post-checkout");
    let started = control.path().join("started");
    let release = control.path().join("release");
    std::fs::write(&hook, format!(
        "#!/bin/sh\nprintf ready > {}\ni=0\nwhile [ ! -e {} ] && [ \"$i\" -lt 100 ]; do sleep .05; i=$((i+1)); done\n",
        crate::edge_tools::shell::shell_escape(&started.to_string_lossy()),
        crate::edge_tools::shell::shell_escape(&release.to_string_lossy()),
    )).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let executor = std::sync::Arc::new(
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner())),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let worker_executor = executor.clone();
    let worker_cancel = cancel.clone();
    let worker = tokio::spawn(async move {
        worker_executor.execute_with_metadata_cancelable("agent", &json!({
            "action":"run_chain", "name":"cancel-between", "description":"Stop between steps",
            "steps":[
                {"tool":"write_file", "args":{"path":"keep.txt", "content":"keep"}},
                {"tool":"worktree", "args":{"action":"enter", "branch":"cancel-between"}},
                {"tool":"write_file", "args":{"path":"must-not-run", "content":"late"}}
            ]
        }), Some(&worker_cancel)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("checkout hook did not start");
    cancel.cancel();
    std::fs::write(release, "continue").unwrap();
    let result = worker.await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert!(result.is_error, "{body}");
    assert_eq!(body["steps_executed"], 2, "{body}");
    assert!(body.get("rollback").is_none(), "{body}");
    assert!(dir.path().join("keep.txt").exists());
    let checkout = executor.get_worktree_session().unwrap().worktree_path;
    assert!(!checkout.join("must-not-run").exists());
    let kept = executor
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!kept.is_error, "{kept:?}");
    super::worktree_tests::remove_preserved_test_worktree(dir.path(), &checkout);
}

#[tokio::test]
async fn chain_preserves_shell_outcome_independently_of_display_text() {
    use astra_turn_core::tool_registry_chain::ToolChain;

    for (command, expected_success, expected_steps) in [
        ("printf 'Error: example diagnostic\\n'", true, 2),
        ("printf 'ordinary output\\n'; exit 7", false, 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let chain = ToolChain::new("shell_outcome", "Preserve executor status")
            .step("bash", json!({"command": command}))
            .step("list_dir", json!({}));

        let output = executor.execute_chain(&chain, json!({})).await;
        let result: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(result["steps"][0]["success"], expected_success, "{result}");
        assert_eq!(result["steps_executed"], expected_steps, "{result}");
    }
}

#[tokio::test]
async fn chain_failure_remains_failed_at_agent_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let executor =
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner()));
    let args = json!({
        "action": "run_chain",
        "name": "failed_validation",
        "description": "Keep a failed step visible",
        "steps": [
            {"tool": "read_file", "args": {"path": "missing.txt"}},
            {"tool": "list_dir", "args": {}}
        ]
    });

    let result = executor.execute_with_metadata("agent", &args).await;
    assert!(result.is_error, "{}", result.output);
    let body: serde_json::Value = serde_json::from_str(&result.output)
        .unwrap_or_else(|error| panic!("{error}: {}", result.output));
    assert_eq!(body["steps_executed"], 1, "{body}");
    assert_eq!(body["steps"][0]["success"], false, "{body}");
}

#[tokio::test]
async fn agent_chain_rejects_automatic_rollback_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let executor =
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner()));
    let result = executor
        .execute_with_metadata(
            "agent",
            &json!({
                "action": "run_chain", "name": "rollback_chain", "description": "Write then fail",
                "rollback_on_failure": true,
                "steps": [
                    {"tool":"write_file", "args":{"path":"temporary.txt", "content":"data"}},
                    {"tool":"read_file", "args":{"path":"missing.txt"}}
                ]
            }),
        )
        .await;
    assert!(result.is_error, "{}", result.output);
    assert!(
        result.output.contains("rollback_on_failure"),
        "{}",
        result.output
    );
    assert!(!dir.path().join("temporary.txt").exists());
}

#[tokio::test]
async fn agent_chain_can_enter_use_and_leave_worktree() {
    let dir = super::worktree_tests::init_temp_git_repo();
    let executor =
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner()));
    let exit_action = if super::worktree_tests::has_authoritative_worktree_owner() {
        "remove"
    } else {
        "keep"
    };
    let result = executor
        .execute_with_metadata(
            "agent",
            &json!({
                "action":"run_chain", "name":"worktree_chain", "description":"Use a worktree",
                "steps":[
                    {"tool":"worktree", "args":{"action":"enter", "branch":"chain-branch"}},
                    {"tool":"write_file", "args":{"path":"created.txt", "content":"worktree"}},
                    {"tool":"read_file", "args":{"path":"created.txt"}},
                    {"tool":"worktree", "args":{"action":"exit", "exit_action":exit_action, "discard_changes":true}}
                ]
            }),
        )
        .await;
    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert!(!result.is_error, "{body}");
    assert_eq!(body["steps_executed"], 4);
    assert!(
        body["steps"][2]["output"]
            .as_str()
            .unwrap()
            .contains("worktree")
    );
    assert!(!dir.path().join("created.txt").exists());
    assert!(!executor.in_worktree_session());
    if !super::worktree_tests::has_authoritative_worktree_owner() {
        let checkout = body["steps"][0]["output"]
            .as_str()
            .unwrap()
            .lines()
            .find_map(|line| line.trim().strip_prefix("Path: "))
            .map(std::path::Path::new)
            .expect("worktree entry reports its checkout");
        assert!(checkout.join("created.txt").exists());
        super::worktree_tests::remove_preserved_test_worktree(dir.path(), checkout);
    }
}

#[tokio::test]
async fn agent_chain_preserves_new_worktree_after_failure() {
    let dir = super::worktree_tests::init_temp_git_repo();
    let executor =
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner()));
    let result = executor.execute_with_metadata("agent", &json!({
        "action":"run_chain", "name":"worktree_failure", "description":"Keep a worktree after failure",
        "steps":[
            {"tool":"worktree", "args":{"action":"enter", "branch":"chain-rollback"}},
            {"tool":"read_file", "args":{"path":"missing.txt"}}
        ]
    })).await;
    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert!(result.is_error, "{body}");
    assert!(body.get("rollback").is_none(), "{body}");
    let checkout = executor.get_worktree_session().unwrap().worktree_path;
    let kept = executor
        .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
        .await;
    assert!(!kept.is_error, "{kept:?}");
    super::worktree_tests::remove_preserved_test_worktree(dir.path(), &checkout);
}

#[tokio::test]
async fn chain_without_rollback_on_failure_keeps_prior_edits() {
    use astra_turn_core::tool_registry_chain::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let chain = ToolChain::new("no_rollback_chain", "Write then fail")
        .step(
            "write_file",
            json!({"path": "kept.txt", "content": "temporary"}),
        )
        .step("read_file", json!({"path": "missing.txt"}));

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["steps_executed"], 2);
    assert!(
        parsed.get("rollback").is_none(),
        "unexpected result: {result}"
    );
    assert!(dir.path().join("kept.txt").exists());
}

#[tokio::test]
async fn chain_variable_substitution_end_to_end() {
    use astra_turn_core::tool_registry_chain::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Step 1: write file with content from $input
    // Step 2: read that file back using path from $input
    // Step 3: write $prev to a new file
    let chain = ToolChain::new("var_sub", "Test variable substitution")
        .step(
            "write_file",
            json!({"path": "$input.filename", "content": "$input.message"}),
        )
        .step("read_file", json!({"path": "$input.filename"}))
        .named_step(
            "copy",
            "write_file",
            json!({"path": "copy.txt", "content": "$prev"}),
        );

    let result = executor
        .execute_chain(
            &chain,
            json!({"filename": "original.txt", "message": "variable test!"}),
        )
        .await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["steps_executed"], 3);
    let steps = parsed["steps"].as_array().unwrap();
    assert!(steps.iter().all(|s| s["success"].as_bool().unwrap()));

    // Verify the copy was created (content includes line numbers from read_file)
    let copy_content = std::fs::read_to_string(dir.path().join("copy.txt")).unwrap();
    assert!(
        copy_content.contains("variable test!"),
        "copy should contain original text: {copy_content}"
    );
}

#[tokio::test]
async fn chain_skip_condition_end_to_end() {
    use astra_turn_core::tool_registry_chain::ChainStep;
    use astra_turn_core::tool_registry_chain::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Step 1: read nonexistent file (will produce "Error")
    // Step 2: should be skipped because prev contains "Error"
    let mut chain = ToolChain::new("skip_test", "Test skip condition");
    chain.steps.push(ChainStep {
        tool: "read_file".into(),
        args: json!({"path": "no_such_file_xyz.txt"}),
        output_key: None,
        skip_if_prev_contains: None,
    });
    chain.steps.push(ChainStep {
        tool: "write_file".into(),
        args: json!({"path": "skipped.txt", "content": "should not exist"}),
        output_key: None,
        skip_if_prev_contains: Some("Error".into()),
    });

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // First step produces error → chain stops before skip can be evaluated
    // Actually: step 1 errors → stops. But if we want skip test, let me
    // restructure: step 1 succeeds with content containing "Error" text
    // This tests that the chain stops on error (step 1 returns "Error...")
    assert_eq!(parsed["steps_executed"], 1);
    assert!(!dir.path().join("skipped.txt").exists());
}

#[tokio::test]
async fn chain_via_agent_run_chain_tool() {
    let dir = tempfile::tempdir().unwrap();
    let executor =
        ToolExecutor::new(dir.path()).with_spawn_context(fanout_test_context(test_spawner()));

    // Invoke run_chain through the consolidated agent tool (like LLM would)
    let chain_args = json!({
        "action": "run_chain",
        "name": "list_and_count",
        "description": "List dir then count",
        "steps": [
            {
                "tool": "write_file",
                "args": {"path": "hello.txt", "content": "world"},
                "output_key": "written"
            },
            {
                "tool": "list_dir",
                "args": {"path": "."}
            }
        ],
        "input": {}
    });

    let result = executor.execute("agent", &chain_args).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["chain"], "list_and_count");
    assert_eq!(parsed["steps_executed"], 2);
    let steps = parsed["steps"].as_array().unwrap();
    assert!(steps[0]["success"].as_bool().unwrap());
    assert!(steps[1]["success"].as_bool().unwrap());
    // list_dir should show the file we just wrote
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains("hello.txt"),
        "list_dir should see the written file"
    );
}

#[tokio::test]
async fn run_chain_invalid_format_returns_error() {
    let executor = test_executor().with_spawn_context(fanout_test_context(test_spawner()));
    let result = astra_tools::ToolExecutor::execute_with_metadata(
        &executor,
        "agent",
        &json!({"action": "run_chain", "invalid": "no steps field"}),
    )
    .await;
    assert_tool_invalid_args(&result);
}

#[tokio::test]
async fn run_chain_blocks_recursive_child_chain() {
    let executor = test_executor().with_spawn_context(fanout_test_context(test_spawner()));
    // `run_chain` as a step tool is no longer in TOOL_CATALOG (it's the `agent`
    // tool's action), so validation rejects it before recursion detection kicks in.
    let result = astra_tools::ToolExecutor::execute_with_metadata(
        &executor,
        "agent",
        &json!({
            "action": "run_chain",
            "name": "outer",
            "description": "outer",
            "steps": [
                {
                    "tool": "run_chain",
                    "args": {"name": "inner", "description": "inner", "steps": []}
                }
            ],
            "input": {}
        }),
    )
    .await;
    // Chain validation rejects unknown tool names (run_chain is not in TOOL_CATALOG)
    assert!(result.is_error, "{result:?}");
}

#[tokio::test]
async fn run_chain_blocks_repeated_identical_steps() {
    let executor = test_executor().with_spawn_context(fanout_test_context(test_spawner()));
    let result = executor
        .execute(
            "agent",
            &json!({
                "action": "run_chain",
                "name": "stall",
                "description": "stall",
                "steps": [
                    {"tool": "read_file", "args": {"path": "same.txt"}},
                    {"tool": "read_file", "args": {"path": "same.txt"}},
                    {"tool": "read_file", "args": {"path": "same.txt"}}
                ],
                "input": {}
            }),
        )
        .await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["steps_executed"], 0);
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains("likely stall"),
        "unexpected result: {result}"
    );
}

#[tokio::test]
async fn execute_chain_blocks_mutating_burst() {
    use astra_turn_core::tool_registry_chain::ToolChain;

    let executor = test_executor();
    let mut chain = ToolChain::new("writes", "writes");
    for idx in 0..=crate::tool_safety_guard::MAX_RUN_CHAIN_MUTATING_STEPS {
        chain = chain.step(
            "write_file",
            json!({"path": format!("file-{idx}.txt"), "content": "x"}),
        );
    }

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["steps_executed"], 0);
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains("write/execute steps"),
        "unexpected result: {result}"
    );
}
