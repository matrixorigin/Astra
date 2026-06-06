use crate::edge_tools::ToolExecutor;
use astra_services::session_journal::{JournalDirGuard, JournalEvent, JournalEventType};
use serde_json::json;

fn parse_task_json(response: &str) -> serde_json::Value {
    let body = response
        .find('{')
        .map(|pos| &response[pos..])
        .unwrap_or(response);
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("task response not JSON: {e}; raw: {response}"))
}

fn setup() -> (tempfile::TempDir, ToolExecutor) {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    (dir, exe)
}

// ── Task tool tests ─────────────────────────────────────────��────────────

#[tokio::test]
async fn task_create_returns_task_id() {
    let (_dir, exe) = setup();
    let result = exe.task_create(&json!({"title": "Test task"})).await;
    let parsed = parse_task_json(&result);
    assert_eq!(parsed["success"], true);
    assert_eq!(parsed["task_id"], "task-1");
    assert!(parsed["message"].as_str().unwrap().contains("Test task"));
}

#[tokio::test]
async fn task_list_shows_created_tasks() {
    let (_dir, exe) = setup();
    exe.task_create(&json!({"title": "First task"})).await;
    let list = exe.task_list(&json!({})).await;
    let parsed: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(parsed["count"], 1);
    let tasks = parsed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-1");
    assert_eq!(tasks[0]["title"], "First task");
    assert_eq!(tasks[0]["status"], "pending");
}

#[tokio::test]
async fn task_get_returns_details() {
    let (_dir, exe) = setup();
    exe.task_create(&json!({"title": "Detailed task", "description": "This is a test"}))
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
    let (_dir, exe) = setup();
    exe.task_create(&json!({"title": "Status test"})).await;
    let result = exe
        .task_update(&json!({"task_id": "task-1", "status": "in_progress"}))
        .await;
    let parsed = parse_task_json(&result);
    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["previous_status"], "pending");
    assert_eq!(parsed["status"], "in_progress");
    // Verify persisted
    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    assert!(details.contains("in_progress"));
}

#[tokio::test]
async fn task_with_subtasks_tracks_progress() {
    let (_dir, exe) = setup();
    exe.task_create(&json!({
        "title": "Multi-step task",
        "subtasks": [
            {"id": "step-1", "title": "First step"},
            {"id": "step-2", "title": "Second step", "depends_on": ["step-1"]}
        ]
    }))
    .await;
    assert!(exe.task_list(&json!({})).await.contains("[0/2]"));
    exe.task_update(&json!({"task_id": "task-1", "subtask_id": "step-1", "status": "completed"}))
        .await;
    assert!(exe.task_list(&json!({})).await.contains("[1/2]"));
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

// ── task_stop tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn task_stop_cancels_task() {
    for (pre_status, title) in [("pending", "Pending"), ("in_progress", "Running")] {
        let (_dir, exe) = setup();
        exe.task_create(&json!({"title": title})).await;
        if pre_status == "in_progress" {
            exe.task_update(&json!({"task_id": "task-1", "status": "in_progress"}))
                .await;
        }
        let result = exe
            .task_stop(&json!({"task_id": "task-1", "reason": "test"}))
            .await;
        let parsed = parse_task_json(&result);
        assert!(
            parsed["success"].as_bool().unwrap(),
            "stop {title} must succeed"
        );
        assert_eq!(
            parsed["previous_status"], pre_status,
            "previous_status for {title}"
        );
    }
}

#[tokio::test]
async fn task_stop_rejects_completed_task() {
    let (_dir, exe) = setup();
    exe.task_create(&json!({"title": "Done task"})).await;
    exe.task_update(&json!({"task_id": "task-1", "status": "completed"}))
        .await;
    let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
    let parsed = parse_task_json(&result);
    assert!(!parsed["success"].as_bool().unwrap());
    assert!(parsed["message"].as_str().unwrap().contains("Cannot stop"));
}

#[tokio::test]
async fn task_stop_cancels_subtasks() {
    let (_dir, exe) = setup();
    exe.task_create(&json!({
        "title": "Parent task",
        "subtasks": [
            {"id": "sub-1", "title": "Subtask 1"},
            {"id": "sub-2", "title": "Subtask 2"}
        ]
    }))
    .await;
    exe.task_update(&json!({"task_id": "task-1", "status": "in_progress"}))
        .await;
    let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
    let parsed = parse_task_json(&result);
    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["cancelled_subtasks"], 2);
}

// ── Error path tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn task_rejects_invalid_input() {
    let cases: &[(&str, serde_json::Value, &str)] = &[
        ("create", json!({}), "title"),
        ("stop", json!({"task_id": "nonexistent"}), "not found"),
        ("stop", json!({}), "required"),
    ];
    for (tool, input, expected) in cases {
        let (_dir, exe) = setup();
        let result = match *tool {
            "create" => exe.task_create(&input).await,
            "stop" => exe.task_stop(&input).await,
            _ => unreachable!(),
        };
        assert!(
            result.contains(expected),
            "task_{tool}({input}) should contain '{expected}' — got: {result}"
        );
    }
}
