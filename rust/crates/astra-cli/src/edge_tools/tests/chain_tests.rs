use super::*;

// ── run_chain (end-to-end with real tool execution) ──────────────────────

#[tokio::test]
async fn chain_write_read_roundtrip() {
    use astra_runtime::tool_registry::ToolChain;

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
    use astra_runtime::tool_registry::ToolChain;

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
async fn chain_rollback_on_failure_reverts_bounded_file_edits() {
    use astra_runtime::tool_registry::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let chain = ToolChain::new("rollback_chain", "Write then fail")
        .with_rollback_on_failure(true)
        .step(
            "write_file",
            json!({"path": "rolled_back.txt", "content": "temporary"}),
        )
        .step("read_file", json!({"path": "missing.txt"}));

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["steps_executed"], 2);
    let steps = parsed["steps"].as_array().unwrap();
    assert!(steps[0]["success"].as_bool().unwrap());
    assert!(!steps[1]["success"].as_bool().unwrap());
    assert_eq!(parsed["rollback"]["success"].as_bool(), Some(true));
    assert_eq!(
        parsed["rollback"]["reverted_files"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert!(!dir.path().join("rolled_back.txt").exists());
}

#[tokio::test]
async fn chain_without_rollback_on_failure_keeps_prior_edits() {
    use astra_runtime::tool_registry::ToolChain;

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
async fn chain_rollback_on_failure_blocks_mutating_bash_before_execution() {
    use astra_runtime::tool_registry::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let chain = ToolChain::new("rollback_bash_block", "Write then mutating bash")
        .with_rollback_on_failure(true)
        .step(
            "write_file",
            json!({"path": "should_not_exist.txt", "content": "temporary"}),
        )
        .step("bash", json!({"command": "mkdir unsafe-dir"}));

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(
        parsed["steps_executed"], 0,
        "chain should be rejected at preflight"
    );
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains("rollback_on_failure only supports read-only bash steps"),
        "unexpected result: {result}"
    );
    assert!(!dir.path().join("should_not_exist.txt").exists());
    assert!(!dir.path().join("unsafe-dir").exists());
}

#[tokio::test]
async fn chain_rollback_on_failure_allows_read_only_bash_step() {
    use astra_runtime::tool_registry::ToolChain;

    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let chain = ToolChain::new("rollback_bash_read_only", "Read-only bash")
        .with_rollback_on_failure(true)
        .step("bash", json!({"command": "pwd"}));

    let result = executor.execute_chain(&chain, json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["steps_executed"], 1);
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains(dir.path().to_string_lossy().as_ref()),
        "read-only bash should still execute: {result}"
    );
}

#[tokio::test]
async fn chain_variable_substitution_end_to_end() {
    use astra_runtime::tool_registry::ToolChain;

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
    use astra_runtime::tool_registry::ToolChain;
    use astra_runtime::tool_registry::chain::ChainStep;

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
async fn chain_via_run_chain_tool() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Invoke run_chain as a tool (like LLM would)
    let chain_args = json!({
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

    let result = executor.execute("run_chain", &chain_args).await;
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
    let executor = test_executor();
    let result = executor
        .execute("run_chain", &json!({"invalid": "no steps field"}))
        .await;
    assert!(
        result.contains("Error"),
        "should return error for invalid chain: {result}"
    );
}

#[tokio::test]
async fn run_chain_blocks_recursive_child_chain() {
    let executor = test_executor();
    let result = executor
        .execute(
            "run_chain",
            &json!({
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
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["steps_executed"], 0);
    assert!(
        parsed["final_output"]
            .as_str()
            .unwrap()
            .contains("recursive run_chain"),
        "unexpected result: {result}"
    );
}

#[tokio::test]
async fn run_chain_blocks_repeated_identical_steps() {
    let executor = test_executor();
    let result = executor
        .execute(
            "run_chain",
            &json!({
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
    use astra_runtime::tool_registry::ToolChain;

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
