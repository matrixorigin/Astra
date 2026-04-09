use super::*;


    // ── Tool search tests ─────────────────────────────────────────────────────

    #[test]
    fn tool_search_requires_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({}));
        assert!(result.contains("Error"));
        assert!(result.contains("query"));
    }

    #[test]
    fn tool_search_select_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:bash"}));
        assert!(result.contains("bash"));
        assert!(result.contains("\"missing\":[]"));
    }

    #[test]
    fn tool_search_select_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:READ_FILE"}));
        assert!(result.contains("read_file"));
    }

    #[test]
    fn tool_search_select_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:bash,grep,glob"}));
        assert!(result.contains("bash"));
        assert!(result.contains("grep"));
        assert!(result.contains("glob"));
    }

    #[test]
    fn tool_search_select_missing_tool() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:nonexistent_tool_xyz"}));
        assert!(result.contains("nonexistent_tool_xyz"));
        assert!(result.contains("missing"));
    }

    #[test]
    fn tool_search_keyword_finds_git_tools() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "git", "max_results": 10}));
        // Should find multiple git-related tools
        assert!(result.contains("git_status") || result.contains("git_diff") || result.contains("git_log"));
    }

    #[test]
    fn tool_search_keyword_file_operations() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "file read"}));
        assert!(result.contains("read_file"));
    }

    #[test]
    fn tool_search_respects_max_results() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        // Search for a broad term that matches many tools
        let result = exe.tool_search(&json!({"query": "file", "max_results": 2}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert!(matches.len() <= 2);
    }

    #[test]
    fn tool_search_reports_total_tools() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "bash"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let total = parsed["total_tools"].as_u64().unwrap();
        assert!(total >= 10, "should have many tools registered");
    }

