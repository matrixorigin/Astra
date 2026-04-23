use super::*;

// ── Notebook edit tests ───────────────────────────────────────────────────

#[test]
fn notebook_edit_requires_ipynb_extension() {
    let exe = test_executor();
    let result = exe.notebook_edit(&json!({
        "notebook_path": "test.py",
        "edit_mode": "insert",
        "new_source": "print('hello')"
    }));

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let err = parsed["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected `error` string for non-.ipynb path — got: {result}"));
    assert!(
        err.contains(".ipynb"),
        "error must mention .ipynb extension — got: {err}"
    );
    assert!(
        parsed.get("success").is_none() || parsed["success"] == false,
        "must not claim success — got: {parsed}"
    );
}

#[test]
fn notebook_edit_unknown_mode_rejected() {
    let exe = test_executor();
    // Create a temporary notebook
    let temp_dir = std::env::temp_dir();
    let notebook_path = temp_dir.join("test_unknown_mode.ipynb");
    std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();

    let result = exe.notebook_edit(&json!({
        "notebook_path": notebook_path.display().to_string(),
        "edit_mode": "unknown",
        "cell_id": "cell-1",
        "new_source": "test"
    }));

    // Cleanup
    let _ = std::fs::remove_file(&notebook_path);

    assert!(
        result.contains("error"),
        "Expected error in result: {}",
        result
    );
    assert!(
        result.contains("Unknown edit_mode"),
        "Expected 'Unknown edit_mode' in result: {}",
        result
    );
}

#[test]
fn notebook_edit_requires_full_read_first() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let notebook_path = dir.path().join("needs_read.ipynb");
    std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();

    let result = exe.notebook_edit(&json!({
        "notebook_path": "needs_read.ipynb",
        "edit_mode": "replace",
        "cell_id": "cell-1",
        "new_source": "x=2"
    }));

    assert!(
        result.contains("read"),
        "Expected read-before-write error, got: {result}"
    );
}

#[test]
fn notebook_edit_succeeds_after_full_read() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let notebook_path = dir.path().join("edit_ok.ipynb");
    std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();

    let _ = exe.read_file(&json!({ "path": "edit_ok.ipynb" }));
    let result = exe.notebook_edit(&json!({
        "notebook_path": "edit_ok.ipynb",
        "edit_mode": "replace",
        "cell_id": "cell-1",
        "new_source": "x=2"
    }));

    assert!(
        result.contains("\"success\":true"),
        "Expected success, got: {result}"
    );
    let updated = std::fs::read_to_string(&notebook_path).unwrap();
    assert!(
        updated.contains("x=2"),
        "Expected notebook update, got: {updated}"
    );
}

#[test]
fn notebook_edit_can_be_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    exe.journal_turn_index
        .store(14, std::sync::atomic::Ordering::Relaxed);
    let notebook_path = dir.path().join("rollback.ipynb");
    std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();

    let _ = exe.read_file(&json!({ "path": "rollback.ipynb" }));
    let result = exe.notebook_edit(&json!({
        "notebook_path": "rollback.ipynb",
        "edit_mode": "replace",
        "cell_id": "cell-1",
        "new_source": "x=2"
    }));
    assert!(
        result.contains("\"success\":true"),
        "Expected success, got: {result}"
    );

    let rollback = exe.rollback_file_edits(&json!({"scope": "file", "path": "rollback.ipynb"}));
    let rollback_json: serde_json::Value =
        serde_json::from_str(&rollback).expect("rollback_file_edits json");
    assert_eq!(
        rollback_json["success"].as_bool(),
        Some(true),
        "got: {rollback}"
    );

    let restored = std::fs::read_to_string(&notebook_path).unwrap();
    assert!(
        restored.contains("x=1"),
        "Expected original notebook, got: {restored}"
    );
    assert!(
        !restored.contains("x=2"),
        "Expected rollback to remove edit, got: {restored}"
    );
}
