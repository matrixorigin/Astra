use super::assert_tool_invalid_args;
use crate::edge_tools::ToolExecutor;
use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
use astra_tools::task_mgmt::{SessionTask, TaskManager, TaskMutation, TaskStore};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

async fn assert_invalid_task_args(executor: &ToolExecutor, args: serde_json::Value) {
    let result =
        astra_tools::ToolExecutor::execute_with_metadata(executor, "task_board", &args).await;
    assert_tool_invalid_args(&result);
}

// ── Task tool tests ─────────────────────────────────────────��────────────

#[tokio::test]
async fn task_tool_create_is_executable() {
    let (_dir, exe) = setup();

    let unified = exe
        .execute(
            "task_board",
            &json!({"action": "create", "title": "new surface"}),
        )
        .await;
    let parsed = parse_task_json(&unified);
    assert_eq!(parsed["success"], true);
    assert_eq!(parsed["task_id"], "task-1");
}

#[tokio::test]
async fn task_list_user_rejects_invalid_status_before_cloud_call() {
    let (_dir, exe) = setup();

    let typo = astra_tools::ToolExecutor::execute_with_metadata(
        &exe,
        "task_board",
        &json!({"action": "list_user", "user_status": "cancelledd"}),
    )
    .await;
    assert_tool_invalid_args(&typo);

    let wrong_type = astra_tools::ToolExecutor::execute_with_metadata(
        &exe,
        "task_board",
        &json!({"action": "list_user", "user_status": true}),
    )
    .await;
    assert_tool_invalid_args(&wrong_type);

    let unknown_field = astra_tools::ToolExecutor::execute_with_metadata(
        &exe,
        "task_board",
        &json!({"action": "list_user", "user_status": "active", "limit": 10}),
    )
    .await;
    assert_tool_invalid_args(&unknown_field);
}

#[tokio::test]
async fn cloud_task_notify_only_fires_for_successful_mutations() {
    let server = MockServer::start().await;
    let session_id = "s-task-notify";
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    let (_dir, exe) = setup();
    let exe = exe
        .with_active_session_id(session_id)
        .with_cloud(server.uri(), "test-token")
        .with_task_notify_tx(tx);
    let execute_path = format!("/sessions/{session_id}/todos:execute");

    Mock::given(method("POST"))
        .and(path(execute_path.as_str()))
        .and(body_json(json!({
            "action": "list",
            "args": {"action": "list", "status_filter": "active"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": "{\"count\":0,\"tasks\":[]}"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(execute_path.as_str()))
        .and(body_json(json!({
            "action": "update",
            "args": {"action": "update", "task_id": "task-2", "title": "duplicate"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": "Refused: open task #task-1 already has this title\n{\"success\":false,\"duplicate_of\":\"task-1\"}",
            "mutation": {
                "status": "refused",
                "success": false,
                "changed": false,
                "data": {"success": false, "duplicate_of": "task-1"}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(execute_path.as_str()))
        .and(body_json(json!({
            "action": "update",
            "args": {"action": "update", "task_id": "task-2", "new_status": "completed"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": "Task #task-2: in_progress -> completed\n{\"success\":true,\"task_id\":\"task-2\",\"status\":\"completed\"}",
            "mutation": {
                "status": "applied",
                "success": true,
                "changed": true,
                "data": {"success": true, "task_id": "task-2", "status": "completed"}
            }
        })))
        .mount(&server)
        .await;

    let list = exe
        .execute(
            "task_board",
            &json!({"action": "list", "status_filter": "active"}),
        )
        .await;
    assert!(list.contains("\"count\":0"), "{list}");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "read-only task_board.list must not wake the task-board observer"
    );

    let refused = exe
        .execute(
            "task_board",
            &json!({"action": "update", "task_id": "task-2", "title": "duplicate"}),
        )
        .await;
    assert!(refused.contains("\"success\":false"), "{refused}");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "refused session task-board update must not wake the observer"
    );

    let updated = exe
        .execute(
            "task_board",
            &json!({"action": "update", "task_id": "task-2", "new_status": "completed"}),
        )
        .await;
    assert!(updated.contains("\"success\":true"), "{updated}");
    let notified = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("successful mutation should notify promptly")
        .expect("notify sender should still be alive");
    assert_eq!(notified, session_id);
}

#[tokio::test]
async fn task_action_create_returns_task_id() {
    let (_dir, exe) = setup();
    let result = exe.task_action_create(&json!({"title": "Test task"})).await;
    let parsed = parse_task_json(&result);
    assert_eq!(parsed["success"], true);
    assert_eq!(parsed["task_id"], "task-1");
    assert!(parsed["message"].as_str().unwrap().contains("Test task"));
}

#[tokio::test]
async fn task_mutation_refuses_when_rollback_snapshot_load_fails() {
    struct LoadFailMutateWouldSucceedStore {
        mutate_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TaskStore for LoadFailMutateWouldSucceedStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err("simulated task board load failure".to_string())
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn mutate(
            &self,
            _session_id: &str,
            mutation: TaskMutation,
        ) -> Result<astra_tools::task_mgmt::TaskMutationOutcome, String> {
            self.mutate_calls.fetch_add(1, Ordering::SeqCst);
            let result = mutation(Vec::new(), 1)?;
            Ok(result.outcome)
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    let mutate_calls = Arc::new(AtomicUsize::new(0));
    let store: Arc<dyn TaskStore> = Arc::new(LoadFailMutateWouldSucceedStore {
        mutate_calls: Arc::clone(&mutate_calls),
    });
    let manager = Arc::new(TaskManager::new("snapshot-fail", store));
    let (_dir, exe) = setup();
    let exe = exe.with_shared_task_manager(manager);

    let out = exe
        .task_action_create(&json!({"title": "must not mutate"}))
        .await;

    assert!(
        out.starts_with("Error:")
            && out.contains("rollback snapshot")
            && out.contains("simulated task board load failure"),
        "task mutation should fail closed when rollback snapshot cannot be captured: {out}"
    );
    assert_eq!(
        mutate_calls.load(Ordering::SeqCst),
        0,
        "task mutation must not run after rollback snapshot capture fails"
    );
}

#[tokio::test]
async fn task_tool_rejects_unknown_fields_instead_of_ignoring_typos() {
    let (_dir, exe) = setup();

    assert_invalid_task_args(&exe, json!({"action": true, "title": "Typo"})).await;

    assert_invalid_task_args(&exe, json!({})).await;

    assert_invalid_task_args(&exe, json!({"action": "cancel", "task_id": "task-1"})).await;

    assert_invalid_task_args(
        &exe,
        json!({"action": "create", "title": "Typo", "titel": "wrong"}),
    )
    .await;

    assert_invalid_task_args(
        &exe,
        json!({
            "action": "create",
            "title": "Blocked task",
            "remove_blocked_by": ["task-1"]
        }),
    )
    .await;

    let too_many_subtasks = (0..=astra_tools::task_mgmt::MAX_CREATE_SUBTASKS)
        .map(|index| json!({ "id": format!("s{index}"), "title": format!("step {index}") }))
        .collect::<Vec<_>>();
    assert_invalid_task_args(
        &exe,
        json!({
            "action": "create",
            "title": "Oversized checklist",
            "subtasks": too_many_subtasks
        }),
    )
    .await;

    assert_invalid_task_args(
        &exe,
        json!({
            "action": "create",
            "title": "x".repeat(astra_tools::task_mgmt::MAX_TASK_TITLE_CHARS + 1)
        }),
    )
    .await;

    assert_invalid_task_args(
        &exe,
        json!({"action": "create", "title": "Blank owner", "owner": "   "}),
    )
    .await;

    let seed = exe
        .execute(
            "task_board",
            &json!({"action": "create", "title": "Seed task"}),
        )
        .await;
    assert!(
        !seed.starts_with("Error:") && seed.contains("\"task-1\""),
        "seed task should be created for recovery-alias checks: {seed}"
    );

    let create_with_dependency = exe
        .execute(
            "task_board",
            &json!({
                "action": "create",
                "title": "Blocked task",
                "add_blocked_by": ["task-1"]
            }),
        )
        .await;
    assert!(
        !create_with_dependency.starts_with("Error:")
            && create_with_dependency.contains("\"task-2\""),
        "task_board.create should accept dependency edges atomically: {create_with_dependency}"
    );

    assert_invalid_task_args(
        &exe,
        json!({"action": "update", "task_id": "task-1", "state": "paused"}),
    )
    .await;

    assert_invalid_task_args(
        &exe,
        json!({"action": "update", "task_id": "task-1", "status": "paused"}),
    )
    .await;

    assert_invalid_task_args(&exe, json!({"action": "list", "status": "active"})).await;

    assert_invalid_task_args(
        &exe,
        json!({
            "action": "adopt",
            "source_session_id": "source",
            "task_id": "task-1",
            "copy_edges": true
        }),
    )
    .await;

    assert_invalid_task_args(
        &exe,
        json!({"action": "background_shell", "command": "echo hi"}),
    )
    .await;
}

#[tokio::test]
async fn task_list_shows_created_tasks() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "First task"}))
        .await;
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
    exe.task_action_create(&json!({"title": "Detailed task", "description": "This is a test"}))
        .await;
    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    let parsed: serde_json::Value = serde_json::from_str(&details).unwrap();
    assert_eq!(parsed["id"], "task-1");
    assert_eq!(parsed["title"], "Detailed task");
    assert_eq!(parsed["description"], "This is a test");
    assert_eq!(parsed["status"], "pending");
}

#[tokio::test]
async fn task_action_update_changes_status() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "Status test"}))
        .await;
    let result = exe
        .task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
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
async fn task_action_update_rejects_reopening_terminal_task() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "Done task"})).await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;

    let result = exe
        .task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(
        result.starts_with("Error:")
            && result.contains("already terminal")
            && result.contains("create a new task"),
        "CLI task update should refuse terminal history reopening: {result}"
    );

    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    let details: serde_json::Value = serde_json::from_str(&details).unwrap();
    assert!(
        details["status"] == "completed",
        "refused reopen must not mutate task: {details}"
    );
}

#[tokio::test]
async fn task_action_update_rejects_empty_mutation() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "No-op task"}))
        .await;

    for args in [
        json!({"task_id": "task-1"}),
        json!({"task_id": "task-1", "metadata": {}}),
        json!({"task_id": "task-1", "remove_blocks": []}),
    ] {
        let result = exe.task_action_update(&args).await;
        assert!(
            result.starts_with("Error:") && result.contains("requires at least one update field"),
            "CLI task update should reject empty mutations: {result}"
        );
    }

    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    let details: serde_json::Value = serde_json::from_str(&details).unwrap();
    assert_eq!(details["title"], "No-op task");
    assert_eq!(details["status"], "pending");
}

#[tokio::test]
async fn task_with_subtasks_tracks_progress() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({
        "title": "Multi-step task",
        "subtasks": [
            {"id": "step-1", "title": "First step"},
            {"id": "step-2", "title": "Second step", "depends_on": ["step-1"]}
        ]
    }))
    .await;
    assert!(exe.task_list(&json!({})).await.contains("[0/2]"));
    exe.task_action_update(
        &json!({"task_id": "task-1", "subtask_id": "step-1", "new_status": "completed"}),
    )
    .await;
    assert!(exe.task_list(&json!({})).await.contains("[1/2]"));
}

#[tokio::test]
async fn subtask_update_rejects_ignored_fields() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({
        "title": "Multi-step task",
        "subtasks": [{"id": "step-1", "title": "First step"}]
    }))
    .await;

    let missing_status = exe
        .task_action_update(&json!({"task_id": "task-1", "subtask_id": "step-1"}))
        .await;
    assert!(
        missing_status.starts_with("Error:")
            && missing_status.contains("new_status")
            && missing_status.contains("required"),
        "CLI subtask update should reject missing status instead of reporting a no-op: {missing_status}"
    );

    let ignored_title = exe
        .task_action_update(&json!({
            "task_id": "task-1",
            "subtask_id": "step-1",
            "title": "Silently ignored"
        }))
        .await;
    assert!(
        ignored_title.starts_with("Error:")
            && ignored_title.contains("unsupported with subtask_id")
            && ignored_title.contains("title"),
        "CLI subtask update should reject fields it cannot apply: {ignored_title}"
    );

    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    let details: serde_json::Value = serde_json::from_str(&details).unwrap();
    assert_eq!(details["subtasks"][0]["title"], "First step");
    assert_eq!(details["subtasks"][0]["status"], "pending");
}

#[tokio::test]
async fn subtask_update_rejects_explicitly_completed_parent() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({
        "title": "Explicitly done parent",
        "subtasks": [
            {"id": "step-1", "title": "First step"},
            {"id": "step-2", "title": "Second step"}
        ]
    }))
    .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;

    let result = exe
        .task_action_update(
            &json!({"task_id": "task-1", "subtask_id": "step-1", "new_status": "pending"}),
        )
        .await;
    assert!(
        result.starts_with("Error:")
            && result.contains("already terminal")
            && result.contains("instead of editing its subtasks"),
        "CLI task update should refuse subtask edits under explicit terminal parent: {result}"
    );

    let details = exe.task_get(&json!({"task_id": "task-1"})).await;
    let details: serde_json::Value = serde_json::from_str(&details).unwrap();
    assert_eq!(
        details["status"], "completed",
        "subtask update should not reopen explicit terminal parent: {details}"
    );
    assert_eq!(
        details["subtasks"][0]["status"], "completed",
        "rejected subtask edit must not mutate terminal history: {details}"
    );
}

#[tokio::test]
async fn task_mutations_append_lifecycle_events_to_session_journal() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(dir.path());
    let exe = ToolExecutor::new(dir.path()).with_active_session_id("task-journal-session");
    exe.journal_turn_index
        .store(7, std::sync::atomic::Ordering::Release);

    exe.task_action_create(&json!({"title": "Journaled task"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_stop(&json!({"task_id": "task-1", "reason": "user cancelled"}))
        .await;
    exe.task_action_archive(&json!({"task_id": "task-1"})).await;

    let raw = std::fs::read_to_string(session_journal::journal_file_path("task-journal-session"))
        .unwrap();
    let events: Vec<JournalEvent> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|evt: &JournalEvent| evt.event_type == JournalEventType::TaskLifecycle)
        .collect();
    assert_eq!(events.len(), 4, "got lifecycle events: {events:#?}");
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
        vec![
            "task_created",
            "task_updated",
            "task_cancelled",
            "task_archived"
        ]
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

    let archive_detail = events[3]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("detail"))
        .expect("archive detail");
    assert_eq!(archive_detail["task_id"], "task-1");
    assert_eq!(archive_detail["status"], "archived");
}

// ── task stop action tests ───────────────────────────────────────────────

#[tokio::test]
async fn task_action_stop_cancels_task() {
    for (pre_status, title) in [("pending", "Pending"), ("in_progress", "Running")] {
        let (_dir, exe) = setup();
        exe.task_action_create(&json!({"title": title})).await;
        if pre_status == "in_progress" {
            exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
                .await;
        }
        let result = exe
            .task_action_stop(&json!({"task_id": "task-1", "reason": "test"}))
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
async fn task_action_stop_rejects_completed_task() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "Done task"})).await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;
    let result = exe.task_action_stop(&json!({"task_id": "task-1"})).await;
    assert!(
        result.starts_with("Refused:"),
        "completed stop refusal should include a readable summary: {result}"
    );
    let parsed = parse_task_json(&result);
    assert!(!parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["task_id"], "task-1");
    assert_eq!(parsed["status"], "completed");
    assert!(parsed["message"].as_str().unwrap().contains("Cannot stop"));
}

#[tokio::test]
async fn task_action_stop_cancels_subtasks() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({
        "title": "Parent task",
        "subtasks": [
            {"id": "sub-1", "title": "Subtask 1"},
            {"id": "sub-2", "title": "Subtask 2"}
        ]
    }))
    .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    let result = exe.task_action_stop(&json!({"task_id": "task-1"})).await;
    let parsed = parse_task_json(&result);
    assert!(parsed["success"].as_bool().unwrap());
    assert_eq!(parsed["cancelled_subtasks"], 2);
}

// ── task_board(action=archive) tests ────────────────────────────────────────────

#[tokio::test]
async fn task_archive_works_without_cloud_connection() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "Done task"})).await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;

    let result = exe.task_action_archive(&json!({"task_id": "task-1"})).await;
    let parsed = parse_task_json(&result);
    assert!(
        parsed["success"].as_bool().unwrap(),
        "offline single-task archive should succeed: {result}"
    );
    assert_eq!(parsed["task_id"], "task-1");
    assert_eq!(parsed["status"], "archived");

    let archived: serde_json::Value =
        serde_json::from_str(&exe.task_list(&json!({"status_filter": "archived"})).await).unwrap();
    assert_eq!(archived["count"], 1, "{archived}");
    assert_eq!(archived["tasks"][0]["id"], "task-1");
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
            "create" => exe.task_action_create(&input).await,
            "stop" => exe.task_action_stop(&input).await,
            _ => unreachable!(),
        };
        assert!(
            result.contains(expected),
            "task_{tool}({input}) should contain '{expected}' — got: {result}"
        );
    }
}
