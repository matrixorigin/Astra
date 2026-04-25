use super::*;

// ── Multi-turn aggregate output scenarios ─────────────────────────────────
//
// These tests simulate realistic multi-tool-call turns to verify that:
// 1. Progressive scaling reduces limits smoothly (not step-function)
// 2. Persist-to-disk triggers when aggregate is high + output is large
// 3. read_file auto-downgrades to outline under aggregate pressure
// 4. Ranged reads always work regardless of aggregate pressure
// 5. git_show/git_diff respect aggregate-aware limits

/// Helper: create a file with N lines of content in a temp dir.
fn make_large_file(dir: &std::path::Path, name: &str, lines: usize) -> PathBuf {
    use std::io::Write;
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    for i in 0..lines {
        writeln!(f, "line {i}: {}", "x".repeat(60)).unwrap();
    }
    drop(f);
    path
}

#[test]
fn progressive_scaling_smooth_curve() {
    let executor = test_executor();
    let base = executor.scaled_output_limit();

    // At 0 aggregate → full limit
    assert_eq!(executor.scaled_output_limit(), base);

    // At soft limit → still full (just below threshold)
    executor
        .aggregate_output_bytes
        .store(AGGREGATE_SOFT_LIMIT, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(executor.scaled_output_limit(), base);

    // Just above soft limit → slightly reduced
    executor.aggregate_output_bytes.store(
        AGGREGATE_SOFT_LIMIT + 1000,
        std::sync::atomic::Ordering::Relaxed,
    );
    let slightly_above = executor.scaled_output_limit();
    assert!(
        slightly_above < base,
        "should reduce above soft limit: {slightly_above} vs {base}"
    );
    assert!(
        slightly_above > base / 2,
        "should not halve just above soft limit: {slightly_above}"
    );

    // At 2x budget → significantly reduced
    executor.aggregate_output_bytes.store(
        AGGREGATE_OUTPUT_BUDGET * 2,
        std::sync::atomic::Ordering::Relaxed,
    );
    let at_2x = executor.scaled_output_limit();
    assert!(
        at_2x < base * 3 / 4,
        "should be well below 75% at 2x budget: {at_2x} vs base {base}"
    );
    assert!(at_2x >= 1024, "should never go below 1KB: {at_2x}");
}

#[test]
fn progressive_scaling_combines_token_and_aggregate_pressure() {
    let executor = test_executor();
    let base = executor.scaled_output_limit();

    // Token pressure alone
    executor.set_budget_pressure(0.6);
    let token_only = executor.scaled_output_limit();
    assert!(token_only < base);

    // Add aggregate pressure on top
    executor.aggregate_output_bytes.store(
        AGGREGATE_OUTPUT_BUDGET,
        std::sync::atomic::Ordering::Relaxed,
    );
    let both = executor.scaled_output_limit();
    assert!(
        both < token_only,
        "combined pressure should be tighter: {both} vs token-only {token_only}"
    );
}

#[test]
fn persist_to_disk_triggers_when_aggregate_high_and_output_large() {
    let executor = test_executor();

    // Simulate high aggregate output (above soft limit)
    executor.aggregate_output_bytes.store(
        AGGREGATE_SOFT_LIMIT + 10_000,
        std::sync::atomic::Ordering::Relaxed,
    );

    // Small output → not persisted
    let small = "x".repeat(1000);
    let result = executor.maybe_persist_large_output(small.clone(), "bash");
    assert_eq!(result, small, "small output should pass through");

    // Large output → persisted
    let large = "x\n".repeat(30_000); // ~60KB
    let result = executor.maybe_persist_large_output(large.clone(), "bash");
    assert!(
        result.contains("<persisted-output>"),
        "large output should be persisted, got first 200 chars: {}",
        &result[..result.len().min(200)]
    );
    assert!(result.contains("tool-results/"), "should contain file path");
    assert!(
        result.contains("</persisted-output>"),
        "should have closing tag"
    );
    assert!(
        result.contains("read_file"),
        "should suggest read_file for access"
    );
    assert!(
        result.len() < large.len() / 5,
        "persisted reference ({}) should be much smaller than original ({})",
        result.len(),
        large.len()
    );

    // Verify file was actually written
    let file_path = result
        .lines()
        .find_map(|line| {
            line.split_once("Full output saved to: ")
                .map(|(_, path)| path.trim())
        })
        .unwrap();
    assert!(
        std::path::Path::new(file_path).exists(),
        "persisted file should exist: {file_path}"
    );

    // Cleanup
    let _ = std::fs::remove_file(file_path);
}

#[test]
fn persist_to_disk_skipped_when_aggregate_low() {
    let executor = test_executor();

    // Aggregate below soft limit → no persist even for large output
    executor
        .aggregate_output_bytes
        .store(0, std::sync::atomic::Ordering::Relaxed);

    let large = "x\n".repeat(30_000);
    let result = executor.maybe_persist_large_output(large.clone(), "bash");
    assert!(
        !result.contains("<persisted-output>"),
        "should not persist when aggregate is low"
    );
}

#[test]
fn persist_to_disk_skipped_for_errors() {
    let executor = test_executor();
    executor.aggregate_output_bytes.store(
        AGGREGATE_SOFT_LIMIT + 10_000,
        std::sync::atomic::Ordering::Relaxed,
    );

    let error_output = format!("Error: {}", "x".repeat(60_000));
    let result = executor.maybe_persist_large_output(error_output.clone(), "bash");
    assert_eq!(
        result, error_output,
        "error outputs should never be persisted"
    );
}

#[test]
fn persist_to_disk_idempotent_same_content() {
    let executor = test_executor();
    executor.aggregate_output_bytes.store(
        AGGREGATE_SOFT_LIMIT + 10_000,
        std::sync::atomic::Ordering::Relaxed,
    );

    let large = "deterministic content\n".repeat(3000);
    let result1 = executor.maybe_persist_large_output(large.clone(), "bash");
    let result2 = executor.maybe_persist_large_output(large, "bash");
    assert_eq!(
        result1, result2,
        "same content should produce identical reference"
    );

    // Cleanup
    if let Some(path) = result1
        .split_whitespace()
        .find(|s| s.contains("tool-results/"))
    {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn multi_turn_review_scenario_read_file_downgrades_to_outline() {
    // Simulates: prior tools produced lots of output → read_file(large) should auto-downgrade
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Create a Rust file large enough to exceed remaining budget
    let rust_content = (0..2000)
        .map(|i| format!("pub fn func_{i}(x: i32) -> i32 {{ x + {i} }}\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("big.rs"), &rust_content).unwrap();
    let file_size = std::fs::metadata(dir.path().join("big.rs")).unwrap().len() as usize;

    // Set aggregate so remaining budget < file size
    let agg = AGGREGATE_OUTPUT_BUDGET - (file_size / 2);
    executor.aggregate_output_bytes.store(
        agg.max(AGGREGATE_SOFT_LIMIT + 1),
        std::sync::atomic::Ordering::Relaxed,
    );

    // Full read of the large file should auto-downgrade to outline
    let result = executor.read_file(&json!({"path": "big.rs"}));
    assert!(
        result.contains("Auto-downgraded to outline")
            || result.contains("too large")
            || result.contains("Outline"),
        "should downgrade or reject full read under aggregate pressure \
             (file={file_size}, agg={agg}, remaining={}), got first 300 chars: {}",
        AGGREGATE_OUTPUT_BUDGET.saturating_sub(agg),
        &result[..result.len().min(300)]
    );
}

#[test]
fn multi_turn_review_scenario_ranged_reads_always_work() {
    // Ranged reads must ALWAYS work regardless of aggregate pressure
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Create a file with known content
    let content: String = (0..1000)
        .map(|i| format!("line {i}: important data here\n"))
        .collect();
    std::fs::write(dir.path().join("data.txt"), &content).unwrap();

    // Simulate extreme aggregate pressure
    executor.aggregate_output_bytes.store(
        AGGREGATE_OUTPUT_BUDGET * 2,
        std::sync::atomic::Ordering::Relaxed,
    );

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
fn multi_turn_review_scenario_discontinuous_ranges() {
    // Reading 5 non-contiguous ranges from a large file should all succeed
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let content: String = (0..2000)
        .map(|i| format!("line {i}: content for section {}\n", i / 100))
        .collect();
    std::fs::write(dir.path().join("big.txt"), &content).unwrap();

    // Simulate moderate aggregate pressure
    executor.aggregate_output_bytes.store(
        AGGREGATE_SOFT_LIMIT + 50_000,
        std::sync::atomic::Ordering::Relaxed,
    );

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
            "ranged read {start}-{end} should succeed under aggregate pressure, got: {}",
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

#[tokio::test]
async fn multi_turn_full_execute_accumulates_aggregate() {
    // Verify that execute() accumulates aggregate_output_bytes across calls
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Create a small file
    std::fs::write(dir.path().join("small.txt"), "hello world\n").unwrap();

    let before = executor
        .aggregate_output_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(before, 0);

    // Execute a tool
    let output = executor
        .execute("read_file", &json!({"path": "small.txt"}))
        .await;
    assert!(!output.is_empty());

    let after = executor
        .aggregate_output_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        after > before,
        "aggregate should increase after tool execution: {after} vs {before}"
    );
    assert_eq!(after, output.len(), "aggregate should equal output size");

    // Execute another tool — aggregate should keep growing
    let output2 = executor
        .execute("read_file", &json!({"path": "small.txt"}))
        .await;
    let after2 = executor
        .aggregate_output_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        after2,
        after + output2.len(),
        "aggregate should accumulate across calls"
    );
}

#[tokio::test]
async fn multi_turn_persist_triggers_via_execute() {
    // End-to-end: execute() should persist large bash output when aggregate is high
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Pre-load aggregate to above soft limit
    executor.aggregate_output_bytes.store(
        AGGREGATE_SOFT_LIMIT + 10_000,
        std::sync::atomic::Ordering::Relaxed,
    );

    // Execute bash that produces large output (>50KB)
    let output = executor
            .execute(
                "bash",
                &json!({"command": format!("python3 -c \"print('x' * 70 + '\\n', end='')\" | head -c 60000; echo; seq 1 500")}),
            )
            .await;

    // If the output was large enough, it should have been persisted
    // (depends on actual bash output size — if python3 isn't available,
    // the error message will be small and won't trigger persist)
    if output.len() > PERSIST_THRESHOLD {
        assert!(
            output.contains("<persisted-output>"),
            "large execute output should be persisted when aggregate is high"
        );
    }
    // Either way, aggregate should have been updated
    let agg = executor
        .aggregate_output_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        agg > AGGREGATE_SOFT_LIMIT + 10_000,
        "aggregate should have increased"
    );
}

#[test]
fn multi_turn_scaled_limit_affects_read_file_truncation() {
    // When aggregate is high, read_file should produce less content or downgrade
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    // Create a medium file
    let content: String = (0..500)
        .map(|i| format!("line {i}: {}\n", "data".repeat(15)))
        .collect();
    std::fs::write(dir.path().join("medium.txt"), &content).unwrap();

    // Read with no pressure — should get full content
    executor
        .aggregate_output_bytes
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let normal_result = executor.read_file(&json!({"path": "medium.txt"}));
    assert!(
        !normal_result.contains("Auto-downgraded") && !normal_result.contains("too large"),
        "normal read should return full content"
    );

    // Read with high pressure — should downgrade or truncate
    executor.aggregate_output_bytes.store(
        AGGREGATE_OUTPUT_BUDGET,
        std::sync::atomic::Ordering::Relaxed,
    );
    executor.clear_file_state();
    let pressured_result = executor.read_file(&json!({"path": "medium.txt"}));

    // Under pressure, result should either be downgraded or truncated
    let is_downgraded = pressured_result.contains("Auto-downgraded")
        || pressured_result.contains("Auto-truncated")
        || pressured_result.contains("too large")
        || pressured_result.contains("[truncated");
    let is_smaller = pressured_result.len() <= normal_result.len();
    assert!(
        is_downgraded || is_smaller,
        "pressured read should be downgraded or smaller: pressured={}, normal={}",
        pressured_result.len(),
        normal_result.len()
    );
}

// ── Per-tool output limit tests ──────────────────────────────────────────

#[test]
fn per_tool_output_limit_grep_capped() {
    let limit = super::per_tool_output_limit("grep");
    assert!(
        limit <= 10_000,
        "grep should be capped at 10KB, got {limit}"
    );
    assert!(limit > 0);
}

#[test]
fn per_tool_output_limit_glob_capped() {
    let limit = super::per_tool_output_limit("glob");
    assert!(
        limit <= 100_000,
        "glob should be capped at 100KB, got {limit}"
    );
    assert!(limit > 0);
}

#[test]
fn per_tool_output_limit_code_analysis_capped() {
    for tool in &["find_definition", "find_references"] {
        let limit = super::per_tool_output_limit(tool);
        assert!(
            limit <= 15_000,
            "{tool} should be capped at 15KB, got {limit}"
        );
        assert!(limit > 0);
    }
}

#[test]
fn per_tool_output_limit_unknown_uses_global() {
    let global = super::tool_output_limit();
    let limit = super::per_tool_output_limit("unknown_tool");
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
