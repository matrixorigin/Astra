use super::*;

// ─── diagnose tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn diagnose_all_categories() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.diagnose(&json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // Should have all categories
    assert!(parsed["system"].is_object());
    assert!(parsed["environment"].is_object());
    assert!(parsed["tools"].is_object());
    assert!(parsed["tasks"].is_object());
    assert!(parsed["session"].is_object());
}

#[tokio::test]
async fn diagnose_system_only() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.diagnose(&json!({"category": "system"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["system"].is_object());
    assert!(parsed["environment"].is_null());
    assert!(parsed["tools"].is_null());
}

#[tokio::test]
async fn diagnose_contains_os_info() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.diagnose(&json!({"category": "system"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["system"]["os"].is_string());
    assert!(parsed["system"]["arch"].is_string());
    assert!(parsed["system"]["project_root"].is_string());
}

#[tokio::test]
async fn diagnose_tools_info() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.diagnose(&json!({"category": "tools"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["tools"]["count"].as_u64().unwrap() > 10);
    assert!(parsed["tools"]["categories"].is_object());
}

#[tokio::test]
async fn diagnose_tools_verbose() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe
        .diagnose(&json!({"category": "tools", "verbose": true}))
        .await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // Verbose mode should list all tools
    assert!(parsed["tools"]["available"].is_array());
    let tools = parsed["tools"]["available"].as_array().unwrap();
    assert!(tools.contains(&json!("bash")));
    assert!(tools.contains(&json!("diagnose")));
}

#[tokio::test]
async fn diagnose_tasks_with_items() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create some tasks
    exe.task_create(&json!({"title": "Task 1"})).await;
    exe.task_create(&json!({"title": "Task 2"})).await;
    exe.task_update(&json!({"task_id": "task-1", "status": "completed"}))
        .await;

    let result = exe.diagnose(&json!({"category": "tasks"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["total"], 2);
    assert_eq!(parsed["tasks"]["completed"], 1);
    assert_eq!(parsed["tasks"]["pending"], 1);
}

#[tokio::test]
async fn diagnose_session_info() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.diagnose(&json!({"category": "session"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["session"]["output_bytes_this_turn"].is_number());
    assert!(parsed["session"]["output_budget"].is_number());
    assert!(parsed["session"]["output_utilization"].is_string());
}

#[tokio::test]
async fn diagnose_environment_does_not_leak_secret_values() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.diagnose(&json!({"category": "environment"})).await;
    // No whitelisted env var should ever surface with an OpenAI-style key prefix.
    assert!(
        !result.contains("sk-"),
        "diagnose must not leak OpenAI-style key prefix: {result}"
    );
}
