use super::*;

#[tokio::test]
async fn sleep_succeeds_with_valid_duration() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let start = std::time::Instant::now();
    let result = exe.sleep_tool(&json!({"duration_ms": 50})).await;
    assert!(start.elapsed().as_millis() >= 40, "should have slept");
    assert!(result.contains("success"));
    assert!(result.contains("50"));
}

#[tokio::test]
async fn sleep_rejects_invalid_input() {
    let cases = &[
        (json!({}), "duration_ms"),
        (json!({"duration_ms": 0}), "Error"),
    ];
    for (input, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.sleep_tool(&input).await;
        assert!(
            result.contains(expected),
            "sleep({input}) should contain '{expected}' — got: {result}"
        );
    }
}
