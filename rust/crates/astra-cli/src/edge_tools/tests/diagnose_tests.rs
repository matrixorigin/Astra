use super::ToolExecutor;
use astra_tools::task_mgmt::{SessionTask, TaskManager, TaskStore};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

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

    assert!(parsed["tools"]["available"].is_array());
    let tools = parsed["tools"]["available"].as_array().unwrap();
    assert!(tools.contains(&json!("bash")));
    assert!(tools.contains(&json!("introspect")));
    for internal in [
        "delete_file",
        "multi_edit",
        "background_shell",
        "git_clone",
        "find_definition",
        "find_references",
    ] {
        assert!(
            !tools.contains(&json!(internal)),
            "diagnose tools must report provider-visible public schemas, not internal helpers: {internal}"
        );
    }
}

#[tokio::test]
async fn diagnose_tasks_with_items() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create some tasks
    exe.task_action_create(&json!({"title": "Task 1"})).await;
    exe.task_action_create(&json!({"title": "Task 2"})).await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;

    let result = exe.diagnose(&json!({"category": "tasks"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["total"], 2);
    assert_eq!(parsed["tasks"]["completed"], 1);
    assert_eq!(parsed["tasks"]["pending"], 1);
    assert_eq!(parsed["tasks"]["paused"], 0);
    assert_eq!(parsed["tasks"]["open_work"], 1);
}

#[tokio::test]
async fn diagnose_tasks_counts_paused_as_open_work_and_cancelled_as_unsuccessful() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    exe.task_action_create(&json!({"title": "Task 1"})).await;
    exe.task_action_create(&json!({"title": "Task 2"})).await;
    exe.task_action_create(&json!({"title": "Task 3"})).await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_stop(&json!({"task_id": "task-1", "reason": "user cancelled"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-3", "new_status": "paused"}))
        .await;

    let result = exe.diagnose(&json!({"category": "tasks"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["failed_or_cancelled"], 1);
    assert_eq!(parsed["tasks"]["completed"], 0);
    assert_eq!(parsed["tasks"]["pending"], 1);
    assert_eq!(parsed["tasks"]["paused"], 1);
    assert_eq!(parsed["tasks"]["open_work"], 2);
}

#[tokio::test]
async fn diagnose_tasks_surfaces_task_board_load_failure() {
    struct FailingTaskStore;

    #[async_trait]
    impl TaskStore for FailingTaskStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err("simulated task diagnostics outage".to_string())
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path()).with_shared_task_manager(Arc::new(TaskManager::new(
        "diag-fail",
        Arc::new(FailingTaskStore),
    )));

    let result = exe.diagnose(&json!({"category": "tasks"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["tasks"]["available"], false, "{parsed}");
    assert!(
        parsed["tasks"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("simulated task diagnostics outage"),
        "diagnose(tasks) must surface task-board load failure: {parsed}"
    );
    assert!(
        parsed["tasks"].get("total").is_none(),
        "diagnose(tasks) must not report total=0 when the task board is unreadable: {parsed}"
    );
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
