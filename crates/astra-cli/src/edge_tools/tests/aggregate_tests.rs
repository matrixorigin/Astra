use super::test_executor;
use crate::edge_tools::{ToolExecutor, per_tool_output_limit, tool_output_limit};
use serde_json::json;

#[test]
fn supplied_context_pressure_preserves_per_tool_caps() {
    let executor = test_executor();
    for tool in ["grep", "glob", "bash", "read_file"] {
        let normal = executor.scaled_output_limit_for(tool);
        executor.set_budget_pressure(1.0);
        let pressured = executor.scaled_output_limit_for(tool);
        assert!(pressured < normal);
        assert!(pressured >= 1024);
        assert!(pressured <= per_tool_output_limit(tool));
        executor.set_budget_pressure(0.0);
        assert_eq!(executor.scaled_output_limit_for(tool), normal);
    }
}

#[test]
fn oversized_full_file_read_uses_existing_size_gate() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let content = (0..2000)
        .map(|i| format!("pub fn func_{i}(x: i32) -> i32 {{ x + {i} }}\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("big.rs"), content).unwrap();
    let result = executor.read_file(&json!({"path":"big.rs"}));
    assert!(result.contains("File is large"), "{result}");
    assert!(result.contains("start_line"));
}

#[test]
fn ranged_reads_remain_available_under_context_pressure() {
    // Ranged reads must ALWAYS work regardless of context pressure
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Create a file with known content
    let content: String = (0..1000)
        .map(|i| format!("line {i}: important data here\n"))
        .collect();
    std::fs::write(dir.path().join("data.txt"), &content).unwrap();

    executor.set_budget_pressure(1.0);

    // Ranged read should still work
    let result = executor.read_file(&json!({
        "path": "data.txt",
        "start_line": 100,
        "end_line": 110
    }));
    assert!(
        result.contains("line 99:") || result.contains("line 100:"),
        "ranged read should return content even under extreme pressure, got: {}",
        &result[..result.len().min(300)]
    );
    assert!(
        !result.contains("Error:") && !result.contains("Auto-downgraded"),
        "ranged read should not be blocked or downgraded, got: {}",
        &result[..result.len().min(300)]
    );
}

#[test]
fn discontinuous_ranges_preserve_requested_content() {
    // Reading 5 non-contiguous ranges from a large file should all succeed
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let content: String = (0..2000)
        .map(|i| format!("line {i}: content for section {}\n", i / 100))
        .collect();
    std::fs::write(dir.path().join("big.txt"), &content).unwrap();

    executor.set_budget_pressure(1.0);

    // Read 5 non-contiguous ranges — all should succeed
    let ranges = [(10, 20), (200, 210), (500, 510), (800, 810), (1500, 1510)];
    for (start, end) in &ranges {
        let result = executor.read_file(&json!({
            "path": "big.txt",
            "start_line": start,
            "end_line": end
        }));
        assert!(
            !result.starts_with("Error:"),
            "ranged read {start}-{end} should succeed under context pressure, got: {}",
            &result[..result.len().min(200)]
        );
        // Verify we got actual content
        assert!(
            result.contains("line "),
            "ranged read {start}-{end} should return file content, got: {}",
            &result[..result.len().min(200)]
        );
    }
}

// ── Per-tool output limit tests ──────────────────────────────────────────

#[test]
fn per_tool_output_limit_grep_capped() {
    let limit = per_tool_output_limit("grep");
    assert!(
        limit <= 10_000,
        "grep should be capped at 10KB, got {limit}"
    );
    assert!(limit > 0);
}

#[test]
fn per_tool_output_limit_glob_capped() {
    let limit = per_tool_output_limit("glob");
    assert!(
        limit <= 100_000,
        "glob should be capped at 100KB, got {limit}"
    );
    assert!(limit > 0);
}

#[test]
fn per_tool_output_limit_code_analysis_capped() {
    for tool in &["find_definition", "find_references"] {
        let limit = per_tool_output_limit(tool);
        assert!(
            limit <= 15_000,
            "{tool} should be capped at 15KB, got {limit}"
        );
        assert!(limit > 0);
    }
}

#[test]
fn per_tool_output_limit_unknown_uses_global() {
    let global = tool_output_limit();
    let limit = per_tool_output_limit("unknown_tool");
    assert_eq!(limit, global, "unknown tools should use global limit");
}

#[test]
fn scaled_output_limit_for_respects_per_tool_cap() {
    let executor = test_executor();
    let grep_limit = executor.scaled_output_limit_for("grep");
    let global_limit = executor.scaled_output_limit();
    // Under zero pressure, grep cap (10KB) should be lower than global
    assert!(
        grep_limit <= 10_000,
        "grep scaled limit should respect 10KB cap, got {grep_limit}"
    );
    assert!(
        grep_limit < global_limit || global_limit <= 10_000,
        "grep limit ({grep_limit}) should be below global ({global_limit}) \
         unless global is already under 10KB"
    );
}

#[tokio::test]
async fn prior_output_does_not_downgrade_a_fitting_current_file_read() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());
    let content = (0..120)
        .map(|line| format!("line {line}: {}\n", "captured evidence ".repeat(4)))
        .collect::<String>();
    let mut delivered = 0usize;
    for index in 0..80 {
        let file = format!("evidence-{index}.txt");
        std::fs::write(dir.path().join(&file), &content).unwrap();
        let outcome = executor
            .execute_with_metadata(
                "read_file",
                &json!({
                    "path":file,"start_line":1,"end_line":120,
                }),
            )
            .await;
        assert!(
            !outcome.is_error,
            "bounded evidence read failed: {}",
            outcome.output
        );
        delivered += outcome.output.len();
    }
    assert!(
        delivered > 200_000,
        "exercise a long tool turn with actual delivered output: {delivered}"
    );
    std::fs::write(
        dir.path().join("current.rs"),
        "pub fn current_fact() -> &'static str { \"FULL_TEXT_STILL_AVAILABLE\" }\n",
    )
    .unwrap();
    // The model has a fresh working set; past network output is not context occupancy.
    executor.set_budget_pressure(0.0);
    let args = json!({"path":"current.rs"});
    let fresh = ToolExecutor::new(dir.path())
        .execute("read_file", &args)
        .await;
    let continued = executor.execute("read_file", &args).await;
    assert_eq!(
        continued, fresh,
        "a fitting file read must not depend on executor-lifetime output history"
    );
}
