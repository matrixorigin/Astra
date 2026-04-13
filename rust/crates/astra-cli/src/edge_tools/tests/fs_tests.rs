use super::*;

// ── fs tools ──────────────────────────────────────────────────────────────

#[test]
fn read_file_missing_path_returns_error() {
    let executor = test_executor();
    let result = executor.read_file(&json!({}));
    assert!(result.contains("Error"), "got: {result}");
}

#[test]
fn read_file_nonexistent_returns_error() {
    let executor = test_executor();
    // Use path within project root (temp_dir) that doesn't exist
    let result = executor.read_file(&json!({"path": "nonexistent_file_xyz.txt"}));
    assert!(
        result.contains("Error") || result.contains("Sandbox"),
        "got: {result}"
    );
}

#[test]
fn write_and_read_file_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let path = "test_roundtrip.txt";

    let write_result = executor.write_file(&json!({"path": path, "content": "hello world"}));
    assert!(
        write_result.contains("\"success\":true") || write_result.contains("\"success\": true"),
        "write failed: {write_result}"
    );

    let read_result = executor.read_file(&json!({"path": path}));
    assert!(
        read_result.contains("hello world"),
        "should contain content: {read_result}"
    );
    assert!(
        read_result.starts_with("1\t"),
        "should have line numbers: {read_result}"
    );
}

#[test]
fn str_replace_works() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let path = "replace_test.txt";

    executor.write_file(&json!({"path": path, "content": "foo bar baz"}));
    let result = executor.str_replace(&json!({"path": path, "old_str": "bar", "new_str": "qux"}));
    assert!(result.contains("Replaced"), "got: {result}");

    let content = executor.read_file(&json!({"path": path}));
    assert!(
        content.contains("foo qux baz"),
        "should contain replaced content: {content}"
    );
}

#[test]
fn str_replace_rejects_non_unique() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let path = "dup_test.txt";

    executor.write_file(&json!({"path": path, "content": "aaa aaa"}));
    let result = executor.str_replace(&json!({"path": path, "old_str": "aaa", "new_str": "bbb"}));
    assert!(result.contains("2 times"), "got: {result}");
}

#[test]
fn str_replace_rejects_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let path = "nf_test.txt";

    executor.write_file(&json!({"path": path, "content": "hello"}));
    let result = executor.str_replace(&json!({"path": path, "old_str": "xyz", "new_str": "abc"}));
    assert!(result.contains("not found"), "got: {result}");
}

#[test]
fn list_dir_returns_entries() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();

    let result = executor.list_dir(&json!({"path": "."}));
    assert!(result.contains("a.txt"), "got: {result}");
    assert!(result.contains("subdir/"), "got: {result}");
}

#[test]
fn list_dir_skips_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    std::fs::write(dir.path().join(".hidden"), "").unwrap();
    std::fs::write(dir.path().join("visible.txt"), "").unwrap();

    let result = executor.list_dir(&json!({"path": "."}));
    assert!(!result.contains(".hidden"), "should skip hidden: {result}");
    assert!(result.contains("visible.txt"));
}

// ── read_file with line ranges ────────────────────────────────────────────

#[test]
fn read_file_with_line_range() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    // File must be >16KB to avoid auto-expand promoting ranged read to full
    {
        use std::io::Write;
        let mut f = std::fs::File::create(dir.path().join("lines.txt")).unwrap();
        for i in 1..=200 {
            writeln!(f, "line{i}: {}", "x".repeat(80)).unwrap();
        }
    }

    let result = executor.read_file(&json!({"path": "lines.txt", "start_line": 2, "end_line": 3}));
    assert!(result.contains("2\tline2:"), "should have line 2: {result}");
    assert!(result.contains("3\tline3:"), "should have line 3: {result}");
    assert!(!result.contains("4\t"), "should not have line 4");
}

#[test]
fn rollback_file_edits_restores_latest_file_version() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let path = "rollback.txt";

    let create_result = executor.write_file(&json!({"path": path, "content": "original"}));
    assert!(
        create_result.contains("\"success\":true"),
        "got: {create_result}"
    );

    let _ = executor.read_file(&json!({"path": path}));
    let overwrite_result = executor.write_file(&json!({"path": path, "content": "mutated"}));
    assert!(
        overwrite_result.contains("\"success\":true"),
        "got: {overwrite_result}"
    );

    let rollback = executor.rollback_file_edits(&json!({"scope": "file", "path": path}));
    let rollback_json: serde_json::Value = serde_json::from_str(&rollback).unwrap();
    assert_eq!(
        rollback_json["success"].as_bool(),
        Some(true),
        "got: {rollback}"
    );
    assert_eq!(rollback_json["scope"].as_str(), Some("file"));
    assert_eq!(rollback_json["path"].as_str(), Some(path));
    assert_eq!(rollback_json["edit_type"].as_str(), Some("overwrite"));

    let content = executor.read_file(&json!({"path": path}));
    assert!(content.contains("original"), "got: {content}");
    assert!(!content.contains("mutated"), "got: {content}");
}

#[test]
fn rollback_file_edits_reverts_current_turn_creates() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    executor
        .journal_turn_index
        .store(7, std::sync::atomic::Ordering::Relaxed);

    let first = executor.write_file(&json!({"path": "a.txt", "content": "A"}));
    let second = executor.write_file(&json!({"path": "b.txt", "content": "B"}));
    assert!(first.contains("\"success\":true"), "got: {first}");
    assert!(second.contains("\"success\":true"), "got: {second}");

    let rollback = executor.rollback_file_edits(&json!({"scope": "current_turn"}));
    let rollback_json: serde_json::Value = serde_json::from_str(&rollback).unwrap();
    assert_eq!(
        rollback_json["success"].as_bool(),
        Some(true),
        "got: {rollback}"
    );
    assert_eq!(rollback_json["turn_index"].as_u64(), Some(7));
    assert_eq!(rollback_json["reverted"].as_array().map(Vec::len), Some(2));

    assert!(!dir.path().join("a.txt").exists());
    assert!(!dir.path().join("b.txt").exists());
}

#[test]
fn rollback_turn_actions_reverts_file_edits_when_no_db_snapshots_exist() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    executor
        .journal_turn_index
        .store(11, std::sync::atomic::Ordering::Relaxed);

    let write_result = executor.write_file(&json!({"path": "mixed.txt", "content": "changed"}));
    assert!(
        write_result.contains("\"success\":true"),
        "got: {write_result}"
    );

    let rollback = executor.rollback_turn_actions(&json!({"scope": "current_turn"}));
    let rollback_json: serde_json::Value = serde_json::from_str(&rollback).unwrap();
    assert_eq!(
        rollback_json["success"].as_bool(),
        Some(true),
        "got: {rollback}"
    );
    assert_eq!(rollback_json["turn_index"].as_u64(), Some(11));
    assert_eq!(
        rollback_json["reverted_files"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        rollback_json["restored_database_snapshots"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    assert!(
        rollback_json["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("1 file edit")
    );
    assert!(!dir.path().join("mixed.txt").exists());
}

#[test]
fn rollback_turn_actions_list_combines_file_and_db_journals() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let write_result = executor.write_file(&json!({"path": "listed.txt", "content": "tracked"}));
    assert!(
        write_result.contains("\"success\":true"),
        "got: {write_result}"
    );

    let rollback = executor.rollback_turn_actions(&json!({"scope": "list"}));
    let rollback_json: serde_json::Value = serde_json::from_str(&rollback).unwrap();
    assert_eq!(
        rollback_json["success"].as_bool(),
        Some(true),
        "got: {rollback}"
    );
    assert_eq!(rollback_json["scope"].as_str(), Some("list"));
    assert_eq!(rollback_json["total_file_entries"].as_u64(), Some(1));
    assert_eq!(rollback_json["total_database_entries"].as_u64(), Some(0));
    assert_eq!(
        rollback_json["file_entries"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        rollback_json["database_entries"].as_array().map(Vec::len),
        Some(0)
    );
}

#[test]
fn rollback_turn_actions_turn_scope_requires_turn_index() {
    let executor = test_executor();
    let rollback = executor.rollback_turn_actions(&json!({"scope": "turn"}));
    let rollback_json: serde_json::Value = serde_json::from_str(&rollback).unwrap();
    assert_eq!(
        rollback_json["success"].as_bool(),
        Some(false),
        "got: {rollback}"
    );
    assert_eq!(
        rollback_json["error"].as_str(),
        Some("missing 'turn_index' for scope=turn")
    );
}
