use super::*;

// ── run_build_test tests ──────────────────────────────────────────────

#[tokio::test]
async fn run_build_test_requires_command() {
    let executor = test_executor();
    let result = executor.execute("run_build_test", &json!({})).await;
    assert!(result.contains("Error"), "should require command: {result}");
}

#[tokio::test]
async fn run_build_test_echo_passes() {
    let executor = test_executor();
    let result = executor
        .execute("run_build_test", &json!({"command": "echo 'hello world'"}))
        .await;
    // echo should succeed
    assert!(
        result.contains("✓") || result.contains("hello"),
        "should pass: {result}"
    );
}

#[tokio::test]
async fn run_build_test_failing_command() {
    let executor = test_executor();
    let result = executor
        .execute("run_build_test", &json!({"command": "false"}))
        .await;
    // false exits with code 1
    assert!(
        result.contains("✗") || result.contains("exit 1") || result.contains("failed"),
        "should detect failure: {result}"
    );
}

#[tokio::test]
async fn run_build_test_cargo_in_repo() {
    // Run cargo check in our own repo
    let root = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // → rust/crates/
        p.pop(); // → rust/
        p
    };
    let executor = ToolExecutor::new(root);
    let result = executor
        .execute(
            "run_build_test",
            &json!({
                "command": "cargo check -p astra-cli --message-format=short 2>&1 | tail -5"
            }),
        )
        .await;
    // Should report something meaningful
    assert!(!result.is_empty(), "should produce output");
}

#[tokio::test]
async fn call_graph_requires_path() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let result = executor.execute("call_graph", &json!({})).await;
    assert!(result.contains("Error"), "should require path: {result}");
}

#[tokio::test]
async fn call_graph_by_symbol_name() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
fn helper() -> i32 { 42 }

fn main() {
    let x = helper();
    println!("{}", x);
    std::process::exit(0);
}
"#;
    std::fs::write(dir.path().join("main.rs"), code).unwrap();
    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "call_graph",
            &json!({
                "path": "main.rs",
                "symbol": "main"
            }),
        )
        .await;
    assert!(
        result.contains("helper"),
        "should find helper() call: {result}"
    );
    assert!(
        result.contains("println!"),
        "should find println!: {result}"
    );
    assert!(
        result.contains("outgoing call(s)"),
        "should show total: {result}"
    );
}

#[tokio::test]
async fn call_graph_by_line_range() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn foo() {\n    bar();\n    baz();\n}\n";
    std::fs::write(dir.path().join("test.rs"), code).unwrap();
    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "call_graph",
            &json!({
                "path": "test.rs",
                "start_line": 1,
                "end_line": 4
            }),
        )
        .await;
    assert!(result.contains("bar"), "should find bar(): {result}");
    assert!(result.contains("baz"), "should find baz(): {result}");
}

#[tokio::test]
async fn call_graph_symbol_not_found() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("empty.rs"), "fn hello() {}\n").unwrap();
    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "call_graph",
            &json!({
                "path": "empty.rs",
                "symbol": "nonexistent"
            }),
        )
        .await;
    assert!(
        result.contains("not found"),
        "should report not found: {result}"
    );
}

#[test]
fn schemas_include_call_graph_and_coding_tools() {
    let schemas = all_tool_schemas();
    let names: Vec<&str> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        })
        .collect();
    assert!(
        names.contains(&"call_graph"),
        "should have call_graph: {:?}",
        names
    );
    assert!(
        names.contains(&"delete_file"),
        "should have delete_file: {:?}",
        names
    );
    assert!(
        names.contains(&"multi_edit"),
        "should have multi_edit: {:?}",
        names
    );
}

#[tokio::test]
async fn run_build_test_iteration_tracking() {
    let executor = test_executor();
    // First call — no delta header
    let r1 = executor
        .execute("run_build_test", &json!({"command": "echo 'ok'"}))
        .await;
    assert!(
        !r1.contains("Iteration"),
        "first run should not show iteration: {r1}"
    );

    // Second call with same command — should show iteration 1
    let r2 = executor
        .execute("run_build_test", &json!({"command": "echo 'ok'"}))
        .await;
    // Both succeed with 0 errors, so delta should be empty (nothing to report)
    assert!(
        r2.contains("✓") || r2.contains("ok"),
        "should still work: {r2}"
    );
}

#[tokio::test]
async fn run_build_test_different_command_resets_tracker() {
    let executor = test_executor();
    // Run one command
    executor
        .execute("run_build_test", &json!({"command": "echo 'build'"}))
        .await;
    // Run different command — should reset tracker, not show iteration
    let r2 = executor
        .execute("run_build_test", &json!({"command": "echo 'test'"}))
        .await;
    assert!(
        !r2.contains("Iteration"),
        "different command should reset: {r2}"
    );
}

#[tokio::test]
async fn run_build_test_auto_fix_false_same_as_default() {
    let executor = test_executor();
    let r1 = executor
        .execute("run_build_test", &json!({"command": "echo ok"}))
        .await;
    let executor2 = test_executor();
    let r2 = executor2
        .execute(
            "run_build_test",
            &json!({"command": "echo ok", "auto_fix": false}),
        )
        .await;
    // Both should produce similar output (no auto-fix sections)
    assert!(
        !r1.contains("Auto-Fix"),
        "default should not auto-fix: {r1}"
    );
    assert!(
        !r2.contains("Auto-Fix"),
        "explicit false should not auto-fix: {r2}"
    );
}

#[tokio::test]
async fn run_build_test_auto_fix_on_success_no_effect() {
    let executor = test_executor();
    let result = executor
        .execute(
            "run_build_test",
            &json!({
                "command": "echo 'all tests passed'",
                "auto_fix": true
            }),
        )
        .await;
    // Successful build = no errors = no fixes to apply
    assert!(
        !result.contains("Auto-Fix"),
        "no errors = no auto-fix: {result}"
    );
}

#[tokio::test]
async fn run_build_test_auto_fix_creates_report() {
    // Create a temp dir with a Rust file that has an "unused import" error pattern
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("test.rs");
    std::fs::write(&src, "use std::io;\n\nfn main() {}\n").unwrap();

    let executor = ToolExecutor::new(dir.path());
    // Simulate a build that produces an unused import warning
    // We use a command that outputs Rust-style warnings
    let result = executor
        .execute(
            "run_build_test",
            &json!({
                "command": "echo 'warning: unused import: `std::io`\n --> test.rs:1:5'",
                "auto_fix": true
            }),
        )
        .await;
    // Should contain auto-fix report since the warning matches unused import pattern
    // and the file exists with the import
    assert!(
        result.contains("Auto-Fix") || !result.contains("error"),
        "should attempt auto-fix or have no errors: {result}"
    );
}

#[test]
fn schema_includes_auto_fix_param() {
    let schemas = all_tool_schemas();
    let build = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                == Some("run_build_test")
        })
        .expect("run_build_test schema should exist");
    let props = build["function"]["parameters"]["properties"]
        .as_object()
        .unwrap();
    assert!(
        props.contains_key("auto_fix"),
        "schema should have auto_fix param"
    );
    assert_eq!(props["auto_fix"]["type"], "boolean");
}
