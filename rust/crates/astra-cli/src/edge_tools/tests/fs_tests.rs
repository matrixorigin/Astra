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
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "bar", "new_str": "qux"}));
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
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "aaa", "new_str": "bbb"}));
        assert!(result.contains("2 times"), "got: {result}");
    }

    #[test]
    fn str_replace_rejects_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "nf_test.txt";

        executor.write_file(&json!({"path": path, "content": "hello"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "xyz", "new_str": "abc"}));
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
        executor.write_file(&json!({"path": "lines.txt", "content": "line1\nline2\nline3\nline4"}));

        let result =
            executor.read_file(&json!({"path": "lines.txt", "start_line": 2, "end_line": 3}));
        assert_eq!(result, "2\tline2\n3\tline3");
    }

