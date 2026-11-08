use super::ToolExecutor;
use serde_json::json;

#[tokio::test]
async fn retired_tools_are_rejected_without_execution() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    for name in [
        "run_build_test",
        "context_analysis",
        "diagnose",
        "brief",
        "share_context",
        "query_context",
        "notebook_edit",
        "config",
    ] {
        assert!(!executor.tool_names().iter().any(|tool| tool == name));
        let result = executor
            .execute(
                name,
                &json!({"command": "touch retired-tool-ran", "auto_fix": true}),
            )
            .await;
        assert!(result.starts_with("Error:"), "{name}: {result}");
        assert!(!dir.path().join("retired-tool-ran").exists());
    }
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

// call_graph, delete_file, multi_edit are no longer in the advertised schema set.
// call_graph is an internal tool still executable but not LLM-facing.
// delete_file and multi_edit are subsumed by the write_file and str_replace schemas.
