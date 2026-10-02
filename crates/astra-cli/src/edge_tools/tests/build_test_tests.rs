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

// ── Tier 1 expiry regression (session f85a02bb) ──────────────────────
//
// The fix: a successful build/test run must actively clear any
// previously-recorded `recent_failing_tests` entries. Without this
// rule, a transient failure (e.g. mis-cwd producing "could not find
// Cargo.toml") persists in the self-awareness block for every
// subsequent turn even after every later invocation succeeds — seen
// persisting 58 consecutive rounds in the diagnostic session.
//
// Tests target the pure `apply_build_test_outcome_to_session` function
// so they do not depend on spawning a real cargo/pytest subprocess
// (which would be slow and environment-sensitive).

use crate::edge_tools::shell::apply_build_test_outcome_to_session;
use astra_tools::build_test::BuildTestResult;

fn make_session() -> astra_runtime::observability::ObservabilitySession {
    astra_runtime::observability::ObservabilitySession::new_simple("tier1-test-session")
}

#[test]
fn tier1_pass_clears_prior_failing_tests() {
    let mut session = make_session();
    session.record_failing_test_names(vec![
        "could not find Cargo.toml in /home/foo".into(),
        "error: process exited with code 1".into(),
    ]);
    assert_eq!(session.recent_failing_tests.len(), 2);

    let parsed = BuildTestResult {
        passed: true,
        exit_code: Some(0),
        framework: "cargo".into(),
        error_count: 0,
        error_messages: Vec::new(),
        error_locations: Vec::new(),
        tests_passed: 5,
        tests_failed: 0,
        tests_skipped: 0,
        summary: "ok. 5 passed".into(),
        truncated: false,
    };
    apply_build_test_outcome_to_session(&mut session, &parsed);

    assert!(
        session.recent_failing_tests.is_empty(),
        "a passing build/test run must clear prior failing-test signals so they do not persist into self-awareness for future turns; got: {:?}",
        session.recent_failing_tests
    );
}

#[test]
fn tier1_failure_preserves_pre_existing_and_appends() {
    let mut session = make_session();
    session.record_failing_test_names(vec!["pre-existing::failure".into()]);

    let parsed = BuildTestResult {
        passed: false,
        exit_code: Some(1),
        framework: "cargo".into(),
        error_count: 1,
        error_messages: vec![
            "error[E0277]: trait bound not satisfied".into(),
            "error: aborting due to previous error".into(),
        ],
        error_locations: Vec::new(),
        tests_passed: 0,
        tests_failed: 1,
        tests_skipped: 0,
        summary: "1 failed".into(),
        truncated: false,
    };
    apply_build_test_outcome_to_session(&mut session, &parsed);

    assert!(
        session
            .recent_failing_tests
            .iter()
            .any(|n| n == "pre-existing::failure"),
        "non-passing run must preserve pre-existing failing-test signals (Tier 1 expiry is pass-only). got: {:?}",
        session.recent_failing_tests
    );
    assert!(
        session
            .recent_failing_tests
            .iter()
            .any(|n| n.contains("E0277")),
        "new errors must still be recorded: {:?}",
        session.recent_failing_tests
    );
}

#[test]
fn tier1_pass_is_noop_when_ring_already_empty() {
    let mut session = make_session();
    let parsed = BuildTestResult {
        passed: true,
        exit_code: Some(0),
        framework: "cargo".into(),
        error_count: 0,
        error_messages: Vec::new(),
        error_locations: Vec::new(),
        tests_passed: 1,
        tests_failed: 0,
        tests_skipped: 0,
        summary: "ok".into(),
        truncated: false,
    };
    apply_build_test_outcome_to_session(&mut session, &parsed);
    assert!(session.recent_failing_tests.is_empty());
}

#[test]
fn tier1_failed_with_empty_error_messages_is_noop_on_ring() {
    // Defensive: a run parsed as failed but with empty error_messages
    // and tests_failed == 0 is neither record nor clear — we have no
    // evidence to act on, so pre-existing state is preserved.
    let mut session = make_session();
    session.record_failing_test_names(vec!["pre-existing::failure".into()]);
    let parsed = BuildTestResult {
        passed: false,
        exit_code: Some(1),
        framework: "cargo".into(),
        error_count: 0,
        error_messages: Vec::new(),
        error_locations: Vec::new(),
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        summary: "inconclusive".into(),
        truncated: false,
    };
    apply_build_test_outcome_to_session(&mut session, &parsed);
    assert_eq!(
        session.recent_failing_tests,
        vec!["pre-existing::failure".to_string()],
        "no-evidence runs must not touch the ring"
    );
}
