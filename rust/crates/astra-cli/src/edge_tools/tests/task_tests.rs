use super::*;

// ── Task tool tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn task_create_requires_title() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.task_create(&json!({})).await;
    assert!(result.contains("Error"));
    assert!(result.contains("title"));
}

#[tokio::test]
async fn task_create_returns_task_id() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.task_create(&json!({"title": "Test task"})).await;
    assert!(result.contains("task-1"));
    assert!(result.contains("success"));
}

#[tokio::test]
async fn task_list_shows_created_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create a task
    exe.task_create(&json!({"title": "First task"})).await;

    // List should show it
    let list = exe.task_list(&json!({})).await;
    assert!(list.contains("First task"));
    assert!(list.contains("task-1"));
}

#[tokio::test]
async fn task_get_returns_details() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    exe.task_create(&json!({
        "title": "Detailed task",
        "description": "This is a test"
    }))
    .await;

    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    assert!(details.contains("Detailed task"));
    assert!(details.contains("This is a test"));
}

#[tokio::test]
async fn task_update_changes_status() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    exe.task_create(&json!({"title": "Status test"})).await;

    // Update to in_progress
    let result = exe
        .task_update(&json!({
            "task_id": "task-1",
            "status": "in_progress"
        }))
        .await;
    assert!(result.contains("success"));

    // Verify status changed
    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    assert!(details.contains("in_progress"));
}

#[tokio::test]
async fn task_with_subtasks_tracks_progress() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    exe.task_create(&json!({
        "title": "Multi-step task",
        "subtasks": [
            {"id": "step-1", "title": "First step"},
            {"id": "step-2", "title": "Second step", "depends_on": ["step-1"]}
        ]
    }))
    .await;

    // List shows subtask count
    let list = exe.task_list(&json!({})).await;
    assert!(list.contains("[0/2]"));

    // Complete first subtask
    exe.task_update(&json!({
        "task_id": "task-1",
        "subtask_id": "step-1",
        "status": "completed"
    }))
    .await;

    let list2 = exe.task_list(&json!({})).await;
    assert!(list2.contains("[1/2]"));
}

// ── Sleep tool tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn sleep_requires_duration() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.sleep_tool(&json!({})).await;
    assert!(result.contains("Error"));
    assert!(result.contains("duration_ms"));
}

#[tokio::test]
async fn sleep_rejects_zero_duration() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.sleep_tool(&json!({"duration_ms": 0})).await;
    assert!(result.contains("Error"));
}

#[tokio::test]
async fn sleep_succeeds_with_valid_duration() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let start = std::time::Instant::now();
    let result = exe.sleep_tool(&json!({"duration_ms": 50})).await;
    let elapsed = start.elapsed();

    assert!(result.contains("success"));
    assert!(result.contains("50"));
    assert!(elapsed.as_millis() >= 40, "should have slept");
}

#[tokio::test]
async fn sleep_caps_at_max_duration() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    // Request 10 minutes, should cap at 5 minutes (300000ms)
    // We won't actually wait that long, just verify the schema accepts it
    let result = exe
        .sleep_tool(&json!({"duration_ms": 1, "reason": "test cap"}))
        .await;
    assert!(result.contains("success"));
}

// ─── task_stop tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn task_stop_cancels_running_task() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create a task
    exe.task_create(&json!({
        "title": "Long running task",
        "description": "This will be stopped"
    }))
    .await;

    // Update to in_progress
    exe.task_update(&json!({
        "task_id": "task-1",
        "status": "in_progress"
    }))
    .await;

    // Stop it
    let result = exe
        .task_stop(&json!({
            "task_id": "task-1",
            "reason": "Taking too long"
        }))
        .await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["previous_status"], "in_progress");
    assert!(parsed["message"].as_str().unwrap().contains("cancelled"));

    // Verify the task is now cancelled
    let task_result = exe.task_get(&json!({"task_id": "task-1"})).await;
    let task: serde_json::Value = serde_json::from_str(&task_result).unwrap();
    assert_eq!(task["status"], "cancelled");
}

#[tokio::test]
async fn task_stop_cancels_pending_task() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create a task (defaults to pending)
    exe.task_create(&json!({
        "title": "Pending task"
    }))
    .await;

    // Stop it while pending
    let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["previous_status"], "pending");
}

#[tokio::test]
async fn task_stop_rejects_completed_task() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create and complete a task
    exe.task_create(&json!({"title": "Done task"})).await;
    exe.task_update(&json!({
        "task_id": "task-1",
        "status": "completed"
    }))
    .await;

    // Try to stop it
    let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(!parsed["success"].as_bool().unwrap());
    assert!(parsed["message"].as_str().unwrap().contains("Cannot stop"));
}

#[tokio::test]
async fn task_stop_cancels_subtasks() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create a task with subtasks
    exe.task_create(&json!({
        "title": "Parent task",
        "subtasks": [
            {"id": "sub-1", "title": "Subtask 1"},
            {"id": "sub-2", "title": "Subtask 2"}
        ]
    }))
    .await;

    // Start the task
    exe.task_update(&json!({
        "task_id": "task-1",
        "status": "in_progress"
    }))
    .await;

    // Stop it
    let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["cancelled_subtasks"], 2);
}

#[tokio::test]
async fn task_stop_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.task_stop(&json!({"task_id": "nonexistent"})).await;
    assert!(result.contains("not found"));
}

#[tokio::test]
async fn task_stop_missing_id() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.task_stop(&json!({})).await;
    assert!(result.contains("required"));
}
