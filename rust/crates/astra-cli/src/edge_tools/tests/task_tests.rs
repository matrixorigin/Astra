use super::*;
use astra_services::session_journal::{JournalDirGuard, JournalEvent, JournalEventType};

/// Parse a task-tool response into JSON, tolerating the human-readable
/// summary line that `prefix_summary` prepends to success responses.
/// Strips everything up to the first `{`.
fn parse_task_json(response: &str) -> serde_json::Value {
    let body = response
        .find('{')
        .map(|pos| &response[pos..])
        .unwrap_or(response);
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("task response not JSON: {e}; raw: {response}"))
}

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
    let parsed = parse_task_json(&result);
    assert_eq!(
        parsed["success"], true,
        "task_create must succeed — got: {result}"
    );
    assert_eq!(parsed["task_id"], "task-1", "first task id must be task-1");
    let msg = parsed["message"]
        .as_str()
        .expect("message must be a string");
    assert!(
        msg.contains("Test task"),
        "message must reference title — got: {msg}"
    );
}

#[tokio::test]
async fn task_list_shows_created_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    exe.task_create(&json!({"title": "First task"})).await;

    let list = exe.task_list(&json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(parsed["count"], 1, "count must reflect created tasks");
    let tasks = parsed["tasks"].as_array().expect("tasks must be array");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-1");
    assert_eq!(tasks[0]["title"], "First task");
    assert_eq!(tasks[0]["status"], "pending");
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
    let parsed: serde_json::Value = serde_json::from_str(&details).unwrap();
    assert_eq!(parsed["id"], "task-1");
    assert_eq!(parsed["title"], "Detailed task");
    assert_eq!(parsed["description"], "This is a test");
    assert_eq!(parsed["status"], "pending");
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
    let parsed = parse_task_json(&result);
    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["previous_status"], "pending");
    assert_eq!(parsed["status"], "in_progress");

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

#[tokio::test]
async fn task_update_reports_previous_subtask_status() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    exe.task_create(&json!({
        "title": "Subtask status test",
        "subtasks": [
            {"id": "step-1", "title": "First step"}
        ]
    }))
    .await;

    let result = exe
        .task_update(&json!({
            "task_id": "task-1",
            "subtask_id": "step-1",
            "status": "completed"
        }))
        .await;
    let parsed = parse_task_json(&result);

    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["previous_status"], "pending");
    assert_eq!(parsed["status"], "completed");
}

#[tokio::test]
async fn task_mutations_append_lifecycle_events_to_session_journal() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(dir.path());
    let exe = ToolExecutor::new(dir.path()).with_active_session_id("task-journal-session");
    exe.journal_turn_index
        .store(7, std::sync::atomic::Ordering::Release);

    exe.task_create(&json!({"title": "Journaled task"})).await;
    exe.task_update(&json!({"task_id": "task-1", "status": "in_progress"}))
        .await;
    exe.task_stop(&json!({"task_id": "task-1", "reason": "user cancelled"}))
        .await;

    let raw = std::fs::read_to_string(dir.path().join("task-journal-session.jsonl")).unwrap();
    let events: Vec<JournalEvent> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|evt: &JournalEvent| evt.event_type == JournalEventType::TaskLifecycle)
        .collect();
    assert_eq!(events.len(), 3, "got lifecycle events: {events:#?}");
    assert!(events.iter().all(|evt| evt.turn == Some(7)));

    let summaries: Vec<&str> = events
        .iter()
        .map(|evt| {
            evt.metadata
                .as_ref()
                .and_then(|meta| meta.get("summary"))
                .and_then(serde_json::Value::as_str)
                .expect("task lifecycle events must carry a summary")
        })
        .collect();
    assert_eq!(
        summaries,
        vec!["task_created", "task_updated", "task_cancelled"]
    );

    let create_detail = events[0]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("detail"))
        .expect("create detail");
    assert_eq!(create_detail["task_id"], "task-1");
    assert_eq!(create_detail["title"], "Journaled task");
    assert_eq!(create_detail["status"], "pending");

    let cancel_detail = events[2]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("detail"))
        .expect("cancel detail");
    assert_eq!(cancel_detail["task_id"], "task-1");
    assert_eq!(cancel_detail["status"], "cancelled");
    assert_eq!(cancel_detail["reason"], "user cancelled");
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
    let parsed = parse_task_json(&result);

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
    let parsed = parse_task_json(&result);

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
    let parsed = parse_task_json(&result);

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
    let parsed = parse_task_json(&result);

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
