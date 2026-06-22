use crate::edge_tools::ToolExecutor;
use astra_services::session_journal::{JournalDirGuard, JournalEvent, JournalEventType};
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

// ── Task tool tests ─────────────────────────────────────────��────────────

#[tokio::test]
async fn legacy_task_tool_names_are_not_executable() {
    let (_dir, exe) = setup();

    let legacy = exe
        .execute("task_create", &json!({"title": "old surface"}))
        .await;
    assert!(
        legacy.starts_with("Error:") || legacy.contains("Unknown"),
        "legacy task_create must not remain an executable task surface: {legacy}"
    );

    let unified = exe
        .execute("task", &json!({"action": "create", "title": "new surface"}))
        .await;
    let parsed = parse_task_json(&unified);
    assert_eq!(parsed["success"], true);
    assert_eq!(parsed["task_id"], "task-1");
}

#[tokio::test]
async fn task_list_user_rejects_invalid_status_before_cloud_call() {
    let (_dir, exe) = setup();

    let typo = exe
        .execute(
            "task",
            &json!({"action": "list_user", "user_status": "cancelledd"}),
        )
        .await;
    assert!(
        typo.contains("invalid user_status") && typo.contains("cancelled"),
        "invalid user_status should be rejected locally instead of turning into an empty cloud list: {typo}"
    );

    let wrong_type = exe
        .execute("task", &json!({"action": "list_user", "user_status": true}))
        .await;
    assert!(
        wrong_type.contains("user_status") && wrong_type.contains("string"),
        "wrong-type user_status should be actionable before any cloud dependency: {wrong_type}"
    );

    let unknown_field = exe
        .execute(
            "task",
            &json!({"action": "list_user", "user_status": "active", "limit": 10}),
        )
        .await;
    assert!(
        unknown_field.starts_with("Error:")
            && unknown_field.contains("unknown field")
            && unknown_field.contains("limit")
            && !unknown_field.contains("requires a cloud connection"),
        "unknown list_user fields should be rejected before any cloud dependency: {unknown_field}"
    );
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
            "output": "Refused: open task #task-1 already has this title\n{\"success\":false,\"duplicate_of\":\"task-1\"}"
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
            "output": "Task #task-2: in_progress -> completed\n{\"success\":true,\"task_id\":\"task-2\",\"status\":\"completed\"}"
        })))
        .mount(&server)
        .await;

    let list = exe
        .execute(
            "task",
            &json!({"action": "list", "status_filter": "active"}),
        )
        .await;
    assert!(list.contains("\"count\":0"), "{list}");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "read-only task.list must not wake the task-board observer"
    );

    let refused = exe
        .execute(
            "task",
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
            "task",
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
        ) -> Result<String, String> {
            self.mutate_calls.fetch_add(1, Ordering::SeqCst);
            let result = mutation(Vec::new(), 1)?;
            Ok(result.response)
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

    let bad_action_type = exe
        .execute("task", &json!({"action": true, "title": "Typo"}))
        .await;
    assert!(
        bad_action_type.starts_with("Error:")
            && bad_action_type.contains("field 'action'")
            && bad_action_type.contains("string"),
        "wrong-type action must be actionable: {bad_action_type}"
    );

    let create_typo = exe
        .execute(
            "task",
            &json!({"action": "create", "title": "Typo", "titel": "wrong"}),
        )
        .await;
    assert!(
        create_typo.starts_with("Error:")
            && create_typo.contains("unknown field")
            && create_typo.contains("titel"),
        "create typo must be actionable: {create_typo}"
    );

    let create_dependency_removal_field = exe
        .execute(
            "task",
            &json!({
                "action": "create",
                "title": "Blocked task",
                "remove_blocked_by": ["task-1"]
            }),
        )
        .await;
    assert!(
        create_dependency_removal_field.starts_with("Error:")
            && create_dependency_removal_field.contains("task.create")
            && create_dependency_removal_field.contains("task.update")
            && create_dependency_removal_field.contains("unknown field 'remove_blocked_by'"),
        "create dependency-removal misuse should explain the two-step repair: {create_dependency_removal_field}"
    );

    let too_many_subtasks = (0..=astra_tools::task_mgmt::MAX_CREATE_SUBTASKS)
        .map(|index| json!({ "id": format!("s{index}"), "title": format!("step {index}") }))
        .collect::<Vec<_>>();
    let oversized = exe
        .execute(
            "task",
            &json!({
                "action": "create",
                "title": "Oversized checklist",
                "subtasks": too_many_subtasks
            }),
        )
        .await;
    assert!(
        oversized.starts_with("Error:")
            && oversized.contains("subtasks")
            && oversized.contains("maximum"),
        "oversized create should be rejected with an actionable limit: {oversized}"
    );

    let oversized_title = exe
        .execute(
            "task",
            &json!({
                "action": "create",
                "title": "x".repeat(astra_tools::task_mgmt::MAX_TASK_TITLE_CHARS + 1)
            }),
        )
        .await;
    assert!(
        oversized_title.starts_with("Error:")
            && oversized_title.contains("title")
            && oversized_title.contains("exceeds"),
        "oversized title should be rejected with an actionable limit: {oversized_title}"
    );

    let blank_owner = exe
        .execute(
            "task",
            &json!({"action": "create", "title": "Blank owner", "owner": "   "}),
        )
        .await;
    assert!(
        blank_owner.starts_with("Error:")
            && blank_owner.contains("owner")
            && blank_owner.contains("non-empty"),
        "blank owner should be rejected with an actionable error: {blank_owner}"
    );

    let seed = exe
        .execute("task", &json!({"action": "create", "title": "Seed task"}))
        .await;
    assert!(
        !seed.starts_with("Error:") && seed.contains("\"task-1\""),
        "seed task should be created for recovery-alias checks: {seed}"
    );

    let create_with_dependency = exe
        .execute(
            "task",
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
        "task.create should accept dependency edges atomically: {create_with_dependency}"
    );

    let update_typo = exe
        .execute(
            "task",
            &json!({"action": "update", "task_id": "task-1", "state": "paused"}),
        )
        .await;
    assert!(
        update_typo.starts_with("Error:")
            && update_typo.contains("unknown field")
            && update_typo.contains("state"),
        "update typo must be actionable before lookup/mutation: {update_typo}"
    );

    let update_status_field = exe
        .execute(
            "task",
            &json!({"action": "update", "task_id": "task-1", "status": "paused"}),
        )
        .await;
    assert!(
        update_status_field.starts_with("Error:")
            && update_status_field.contains("unknown field")
            && update_status_field.contains("status")
            && update_status_field.contains("new_status"),
        "task.update should reject the old status alias with an actionable hint: {update_status_field}"
    );

    let list_status_field = exe
        .execute("task", &json!({"action": "list", "status": "active"}))
        .await;
    assert!(
        list_status_field.starts_with("Error:")
            && list_status_field.contains("unknown field")
            && list_status_field.contains("status")
            && !list_status_field.contains("status_filter, status"),
        "status must not remain a recognized task.list argument: {list_status_field}"
    );

    let adopt_typo = exe
        .execute(
            "task",
            &json!({
                "action": "adopt",
                "source_session_id": "source",
                "task_id": "task-1",
                "copy_edges": true
            }),
        )
        .await;
    assert!(
        adopt_typo.starts_with("Error:")
            && adopt_typo.contains("unknown field")
            && adopt_typo.contains("copy_edges")
            && !adopt_typo.contains("requires a cloud connection"),
        "adopt typo should be rejected locally before cloud checks: {adopt_typo}"
    );

    let old_background_action = exe
        .execute(
            "task",
            &json!({"action": "background_shell", "command": "echo hi"}),
        )
        .await;
    assert!(
        old_background_action.starts_with("Error:")
            && old_background_action.contains("unknown `task` action")
            && old_background_action.contains("background_shell")
            && !old_background_action.contains("agent_job(action='"),
        "old task background actions should be ordinary unknown task actions: {old_background_action}"
    );
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
async fn session_summary_surfaces_paused_open_work_with_canonical_update_hint() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path()).with_active_session_id("summary-paused-session");
    exe.task_action_create(&json!({"title": "Paused investigation"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "paused"}))
        .await;

    let summary = exe.execute("session", &json!({"action": "summary"})).await;

    assert!(summary.contains("Open tasks: 1"), "{summary}");
    assert!(summary.contains("⏸ task-1"), "{summary}");
    assert!(summary.contains("Paused investigation"), "{summary}");
    assert!(
        summary.contains("new_status=\"...\""),
        "summary must teach the canonical status field: {summary}"
    );
    assert!(
        !summary.contains(", status=\"...\""),
        "summary must not advertise the old status field: {summary}"
    );
}

#[tokio::test]
async fn session_summary_surfaces_task_board_load_failure() {
    struct FailingTaskStore;

    #[async_trait]
    impl TaskStore for FailingTaskStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err("simulated summary task-board outage".to_string())
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn mutate(
            &self,
            _session_id: &str,
            mutation: TaskMutation,
        ) -> Result<String, String> {
            let result = mutation(Vec::new(), 1)?;
            Ok(result.response)
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(dir.path());
    std::fs::write(
        dir.path().join("summary-task-load-fail.jsonl"),
        r#"{"type":"turn","turn":1,"tokens_in":1,"tokens_out":2}"#,
    )
    .unwrap();
    let exe = ToolExecutor::new(dir.path())
        .with_active_session_id("summary-task-load-fail")
        .with_shared_task_manager(Arc::new(TaskManager::new(
            "summary-task-load-fail",
            Arc::new(FailingTaskStore),
        )));

    let summary = exe.execute("session", &json!({"action": "summary"})).await;

    assert!(
        summary.contains("Task board unavailable")
            && summary.contains("simulated summary task-board outage")
            && summary.contains("Do not assume there are no open tasks"),
        "session summary should surface task-board load failure instead of silently omitting open tasks: {summary}"
    );
    assert!(
        !summary.contains("Open tasks: 0"),
        "session summary must not report zero open tasks when task board is unreadable: {summary}"
    );
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
    exe.task_archive(&json!({"task_id": "task-1"})).await;

    let raw = std::fs::read_to_string(dir.path().join("task-journal-session.jsonl")).unwrap();
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

// ── task_archive tests ───────────────────────────────────────────────────

#[tokio::test]
async fn task_archive_works_without_cloud_connection() {
    let (_dir, exe) = setup();
    exe.task_action_create(&json!({"title": "Done task"})).await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    exe.task_action_update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;

    let result = exe.task_archive(&json!({"task_id": "task-1"})).await;
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
