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

    assert!(result.contains("error"));
    assert!(result.contains(".ipynb"));
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
