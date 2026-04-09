use super::*;


    // ── ToolExecutor ──────────────────────────────────────────────────────────

    #[test]
    fn executor_tool_count_matches_schemas() {
        let executor = test_executor();
        assert_eq!(executor.tool_count(), all_tool_schemas().len());
    }

    #[test]
    fn executor_tool_names_match_schemas() {
        let executor = test_executor();
        let names = executor.tool_names();
        assert_eq!(names.len(), all_tool_schemas().len());
        assert!(names.contains(&"bash".to_string()));
    }

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let executor = test_executor();
        let result = executor.execute("nonexistent_tool", &json!({})).await;
        assert!(result.contains("Unknown tool"), "got: {result}");
    }

    #[tokio::test]
    async fn execute_delegate_without_runtime_returns_guidance() {
        let executor = test_executor();
        let result = executor.execute("delegate", &json!({})).await;
        assert!(result.contains("spawn_agent"), "got: {result}");
        assert!(result.contains("not available in this context"), "got: {result}");
    }

    #[tokio::test]
    async fn execute_reflect_returns_placeholder() {
        let executor = test_executor();
        let result = executor.execute("reflect", &json!({"focus": "auto"})).await;
        assert!(result.contains("reflect_requires_session"), "got: {result}");
    }

    #[test]
    fn budget_pressure_defaults_to_zero() {
        let executor = test_executor();
        assert_eq!(executor.get_budget_pressure(), 0.0);
    }

    #[test]
    fn budget_pressure_set_and_get() {
        let executor = test_executor();
        executor.set_budget_pressure(0.6);
        assert!((executor.get_budget_pressure() - 0.6).abs() < 1e-10);
    }

    #[test]
    fn budget_pressure_clamps_to_range() {
        let executor = test_executor();
        executor.set_budget_pressure(1.5);
        assert_eq!(executor.get_budget_pressure(), 1.0);
        executor.set_budget_pressure(-0.5);
        assert_eq!(executor.get_budget_pressure(), 0.0);
    }

    // ── truncate_output ─────────────────────────────────────────────────────

    #[test]
    fn truncate_output_ascii_no_change() {
        let input = "hello world".to_string();
        let result = truncate_output(input.clone(), 100);
        assert_eq!(result, input);
    }

    #[test]
    fn truncate_output_ascii_truncates() {
        let input = "hello world".to_string();
        let result = truncate_output(input, 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn truncate_output_utf8_boundary_no_panic() {
        // 🔥 is 4 bytes, "ab🔥cd" = 2+4+2 = 8 bytes
        let input = "ab🔥cd".to_string();
        // Truncate at byte 3 — inside the 🔥 (bytes 2..5)
        let result = truncate_output(input, 3);
        // Should truncate at char boundary (byte 2, before 🔥)
        assert!(result.starts_with("ab"), "got: {result}");
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn truncate_output_cjk_boundary_no_panic() {
        // Chinese chars are 3 bytes each
        let input = "你好世界".to_string(); // 12 bytes
        let result = truncate_output(input, 7); // Between 2nd and 3rd char
        assert!(result.contains("[truncated]"));
        // Should not panic — regression for char boundary issue
    }

