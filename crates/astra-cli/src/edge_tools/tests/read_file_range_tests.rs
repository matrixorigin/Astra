use crate::edge_tools::{ToolExecutor, all_tool_schemas};
use serde_json::json;
use std::path::Path;

fn executor(root: &Path) -> ToolExecutor {
    let executor = ToolExecutor::new(root);
    let schema = all_tool_schemas()
        .into_iter()
        .find(|schema| schema["function"]["name"] == "read_file")
        .unwrap();
    executor.set_current_visible_tool_schemas(&[schema]);
    executor
}

fn write_file(root: &Path, name: &str, lines: usize, padding: usize) {
    let content = (1..=lines)
        .map(|line| format!("line {line}: {}\n", "x".repeat(padding)))
        .collect::<String>();
    std::fs::write(root.join(name), content).unwrap();
}

async fn assert_past_eof(executor: &ToolExecutor, path: &str, lines: usize) {
    let start = lines + 1;
    for args in [
        json!({"path":path, "start_line":start}),
        json!({"path":path, "start_line":start, "end_line":start + 2}),
    ] {
        let result = executor.execute_with_metadata("read_file", &args).await;
        assert!(result.is_error, "{args}: {result:?}");
        assert!(
            result
                .output
                .contains(&format!("start_line {start} exceeds file length {lines}")),
            "{args}: {result:?}"
        );
        assert!(!result.output.contains("Auto-expanded"), "{result:?}");
    }
}

#[tokio::test]
async fn small_file_start_past_eof_is_rejected_before_auto_expansion() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "small.txt", 6, 0);
    assert_past_eof(&executor(root.path()), "small.txt", 6).await;
}

#[tokio::test]
async fn large_file_start_past_eof_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "large.txt", 200, 80);
    assert_past_eof(&executor(root.path()), "large.txt", 200).await;
}

#[tokio::test]
async fn small_file_valid_range_and_exact_eof_keep_auto_expansion() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "small.txt", 6, 0);
    let executor = executor(root.path());
    for args in [
        json!({"path":"small.txt", "start_line":2, "end_line":3}),
        json!({"path":"small.txt", "start_line":6}),
    ] {
        let result = executor.execute_with_metadata("read_file", &args).await;
        assert!(!result.is_error, "{args}: {result:?}");
        assert!(
            result
                .output
                .contains("Auto-expanded to full file (6 lines)"),
            "{result:?}"
        );
        assert!(result.output.contains("1\tline 1:"), "{result:?}");
        assert!(result.output.contains("6\tline 6:"), "{result:?}");
        assert!(result.output.chars().count() <= 4_000, "{result:?}");
    }
}

#[tokio::test]
async fn large_file_valid_range_and_exact_eof_keep_requested_lines() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "large.txt", 200, 80);
    let executor = executor(root.path());
    let result = executor
        .execute_with_metadata(
            "read_file",
            &json!({"path":"large.txt", "start_line":2, "end_line":3}),
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert!(!result.output.contains("Auto-expanded"), "{result:?}");
    assert!(result.output.contains("2\tline 2:"), "{result:?}");
    assert!(result.output.contains("3\tline 3:"), "{result:?}");
    assert!(!result.output.contains("line 1:"), "{result:?}");
    assert!(!result.output.contains("line 4:"), "{result:?}");

    let result = executor
        .execute_with_metadata("read_file", &json!({"path":"large.txt", "start_line":200}))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, format!("200\tline 200: {}", "x".repeat(80)));
}

#[tokio::test]
async fn reversed_ranges_are_rejected_for_small_large_and_empty_files() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "small.txt", 6, 0);
    write_file(root.path(), "large.txt", 200, 80);
    write_file(root.path(), "empty.txt", 0, 0);
    let executor = executor(root.path());
    for path in ["small.txt", "large.txt", "empty.txt"] {
        let result = executor
            .execute_with_metadata(
                "read_file",
                &json!({"path":path, "start_line":3, "end_line":2}),
            )
            .await;
        assert!(result.is_error, "{path}: {result:?}");
        assert!(result.output.contains("must not exceed"), "{result:?}");
        assert!(!result.output.contains("Auto-expanded"), "{result:?}");
    }
}

#[tokio::test]
async fn empty_file_full_read_succeeds_but_ranged_read_exceeds_eof() {
    let root = tempfile::tempdir().unwrap();
    write_file(root.path(), "empty.txt", 0, 0);
    let executor = executor(root.path());
    let result = executor
        .execute_with_metadata("read_file", &json!({"path":"empty.txt"}))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_past_eof(&executor, "empty.txt", 0).await;
}
