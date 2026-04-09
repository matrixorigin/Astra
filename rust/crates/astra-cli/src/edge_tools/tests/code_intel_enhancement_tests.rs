use super::*;

// ── Code Intelligence Enhancement Tests ──

#[tokio::test]
async fn symbols_with_calls_shows_callees() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
fn helper() -> i32 { 42 }

fn process(x: i32) -> i32 {
    let a = helper();
    println!("{}", a + x);
    a + x
}

fn main() {
    let result = process(10);
    std::process::exit(result);
}
"#;
    std::fs::write(dir.path().join("demo.rs"), code).unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Without calls=true — no call info
    let r1 = executor
        .execute("symbols", &json!({"path": "demo.rs"}))
        .await;
    assert!(
        !r1.contains("→"),
        "without calls should not show arrows: {r1}"
    );

    // With calls=true — should show callees inline
    let r2 = executor
        .execute("symbols", &json!({"path": "demo.rs", "calls": true}))
        .await;
    assert!(r2.contains("→ helper()"), "should show helper() call: {r2}");
    assert!(
        r2.contains("→ process("),
        "should show process() call: {r2}"
    );
    assert!(
        r2.contains("→ std::process::exit()") || r2.contains("→ exit()"),
        "should show exit call: {r2}"
    );
}

#[tokio::test]
async fn symbols_calls_empty_for_leaf_functions() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn leaf() -> i32 { 42 }\n";
    std::fs::write(dir.path().join("leaf.rs"), code).unwrap();
    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute("symbols", &json!({"path": "leaf.rs", "calls": true}))
        .await;
    // Should not have any call arrows since leaf() calls nothing
    assert!(
        !result.contains("→"),
        "leaf function should have no calls: {result}"
    );
}

#[tokio::test]
async fn call_graph_with_callers() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
fn target() -> i32 { 42 }

fn caller_a() {
    let x = target();
    println!("{}", x);
}

fn caller_b() {
    target();
}

fn unrelated() {
    println!("hello");
}
"#;
    std::fs::write(dir.path().join("callers.rs"), code).unwrap();
    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "call_graph",
            &json!({
                "path": "callers.rs",
                "symbol": "target",
                "callers": true
            }),
        )
        .await;

    // Should show callers section
    assert!(
        result.contains("Callers OF 'target'"),
        "should have callers section: {result}"
    );
    assert!(
        result.contains("caller_a"),
        "should find caller_a: {result}"
    );
    assert!(
        result.contains("caller_b"),
        "should find caller_b: {result}"
    );
    assert!(
        !result.contains("unrelated"),
        "should not include unrelated: {result}"
    );
}

#[tokio::test]
async fn call_graph_callers_without_symbol_name() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn foo() { bar(); }\nfn bar() {}\n";
    std::fs::write(dir.path().join("test.rs"), code).unwrap();
    let executor = ToolExecutor::new(dir.path());
    // Using line range instead of symbol name — callers should note it needs symbol
    let result = executor
        .execute(
            "call_graph",
            &json!({
                "path": "test.rs",
                "start_line": 1,
                "end_line": 1,
                "callers": true
            }),
        )
        .await;
    assert!(
        result.contains("requires symbol name"),
        "should warn about symbol requirement: {result}"
    );
}

#[test]
fn categorize_reference_definitions() {
    assert_eq!(
        categorize_reference("foo.rs:10:fn helper() -> i32 {", "helper"),
        "definition"
    );
    assert_eq!(
        categorize_reference("foo.rs:10:pub fn process(x: i32) {", "process"),
        "definition"
    );
    assert_eq!(
        categorize_reference("foo.py:5:def calculate(n):", "calculate"),
        "definition"
    );
    assert_eq!(
        categorize_reference("foo.rs:3:pub struct Config {", "Config"),
        "definition"
    );
    assert_eq!(
        categorize_reference("foo.rs:3:pub enum Status {", "Status"),
        "definition"
    );
}

#[test]
fn categorize_reference_imports() {
    assert_eq!(
        categorize_reference("foo.rs:1:use crate::helper;", "helper"),
        "import"
    );
    assert_eq!(
        categorize_reference("foo.py:1:from module import helper", "helper"),
        "import"
    );
    assert_eq!(
        categorize_reference("foo.py:1:import helper", "helper"),
        "import"
    );
    assert_eq!(
        categorize_reference("foo.js:1:const x = require('helper')", "helper"),
        "import"
    );
}

#[test]
fn categorize_reference_calls() {
    assert_eq!(
        categorize_reference("foo.rs:20:    let x = helper();", "helper"),
        "call"
    );
    assert_eq!(
        categorize_reference("foo.rs:20:    helper(42, true);", "helper"),
        "call"
    );
    assert_eq!(
        categorize_reference("foo.py:20:    result = calculate(n)", "calculate"),
        "call"
    );
}

#[test]
fn categorize_reference_usage() {
    // Type annotations, field access, etc. — no parens, not a definition/import
    assert_eq!(
        categorize_reference("foo.rs:10:    let x: Config = default;", "Config"),
        "usage"
    );
}

#[test]
fn schemas_include_new_params() {
    let schemas = all_tool_schemas();
    let symbols_schema = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                == Some("symbols")
        })
        .expect("symbols schema should exist");
    let props = &symbols_schema["function"]["parameters"]["properties"];
    assert!(
        props.get("calls").is_some(),
        "symbols should have 'calls' param"
    );

    let call_graph_schema = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                == Some("call_graph")
        })
        .expect("call_graph schema should exist");
    let cg_props = &call_graph_schema["function"]["parameters"]["properties"];
    assert!(
        cg_props.get("callers").is_some(),
        "call_graph should have 'callers' param"
    );
    assert!(
        cg_props.get("scope").is_some(),
        "call_graph should have 'scope' param"
    );

    let ref_schema = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                == Some("find_references")
        })
        .expect("find_references schema should exist");
    let ref_props = &ref_schema["function"]["parameters"]["properties"];
    assert!(
        ref_props.get("kind").is_some(),
        "find_references should have 'kind' param"
    );
}
