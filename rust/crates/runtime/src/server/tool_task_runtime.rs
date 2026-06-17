use astra_tools::task_mgmt::{
    SessionTaskStatusKind, TaskManager, TaskManagerSnapshot, VALID_LIST_STATUS_FILTERS,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

use super::tool_execution_result::tool_result_from_output;
use crate::server::server_tool_executor::ServerToolExecutor;
use crate::server::tool_session_state_rollback::{self, SessionStateRollbackAction};

fn find_json_body_start(output: &str) -> Option<usize> {
    if output.starts_with('{') {
        return Some(0);
    }
    output.find("\n{").map(|pos| pos + 1)
}

pub(crate) fn task_output_success(output: &str) -> bool {
    if output.starts_with("Error:") {
        return false;
    }
    if let Some(pos) = find_json_body_start(output) {
        if let Ok(value) = serde_json::from_str::<Value>(&output[pos..]) {
            if let Some(false) = value.get("success").and_then(Value::as_bool) {
                return false;
            }
        }
    }
    true
}

const TASK_ACTIONS: &[&str] = &[
    "create",
    "list",
    "get",
    "update",
    "stop",
    "list_user",
    "adopt",
    "archive",
];

fn task_action_allowed_fields(action: &str) -> Option<&'static [&'static str]> {
    match action {
        "create" => Some(&[
            "action",
            "title",
            "description",
            "subtasks",
            "active_form",
            "owner",
            "metadata",
            "add_blocks",
            "add_blocked_by",
        ]),
        "list" => Some(&["action", "status_filter"]),
        "get" => Some(&["action", "task_id"]),
        "update" => Some(&[
            "action",
            "task_id",
            "new_status",
            "title",
            "description",
            "subtask_id",
            "active_form",
            "owner",
            "metadata",
            "add_blocks",
            "add_blocked_by",
            "remove_blocks",
            "remove_blocked_by",
            "reason",
            "error_message",
        ]),
        "stop" => Some(&["action", "task_id", "reason"]),
        "list_user" => Some(&["action", "user_status"]),
        "adopt" => Some(&["action", "source_session_id", "task_id"]),
        "archive" => Some(&["action", "task_id", "older_than_days", "reason"]),
        _ => None,
    }
}

fn task_actions_allowing_field(field: &str, current_action: &str) -> Vec<&'static str> {
    TASK_ACTIONS
        .iter()
        .copied()
        .filter(|action| *action != current_action)
        .filter(|action| {
            task_action_allowed_fields(action).is_some_and(|allowed| allowed.contains(&field))
        })
        .collect()
}

pub(crate) fn validate_task_tool_args_for_action(action: &str, args: &Value) -> Result<(), String> {
    let Some(allowed) = task_action_allowed_fields(action) else {
        return Ok(());
    };
    let Some(obj) = args.as_object() else {
        return Err(format!("task.{action} arguments must be an object"));
    };
    for key in obj.keys() {
        if key.starts_with('_') {
            continue;
        }
        if !allowed.contains(&key.as_str()) {
            let other_actions = task_actions_allowing_field(key, action);
            let action_hint = if other_actions.is_empty() {
                String::new()
            } else {
                format!(
                    "; field is valid for: {}",
                    other_actions
                        .iter()
                        .map(|action| format!("task.{action}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(format!(
                "unknown field '{key}' for task.{action} (valid: {}{})",
                allowed.join(", "),
                action_hint
            ));
        }
    }
    Ok(())
}

pub(crate) fn normalize_task_user_status(args: &Value) -> Result<&str, String> {
    let Some(raw) = args.get("user_status") else {
        return Ok("active");
    };
    let Some(status) = raw.as_str() else {
        return Err("Error: field 'user_status' must be a string".to_string());
    };
    if VALID_LIST_STATUS_FILTERS.contains(&status) {
        Ok(status)
    } else {
        Err(format!(
            "Error: invalid user_status '{}' (valid: {})",
            status,
            VALID_LIST_STATUS_FILTERS.join("|")
        ))
    }
}

pub(crate) fn task_user_status_matches(status_filter: &str, status: SessionTaskStatusKind) -> bool {
    match status_filter {
        "active" => status.blocks_duplicate_create(),
        "all" => true,
        status_filter => status == SessionTaskStatusKind::from(status_filter),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskMutationKind {
    Create,
    Update,
    Stop,
    Archive,
}

impl TaskMutationKind {
    pub(crate) fn event_reason(self) -> &'static str {
        match self {
            Self::Create => "task_create",
            Self::Update => "task_update",
            Self::Stop => "task_stop",
            Self::Archive => "task_archive",
        }
    }

    fn rollback_label(self, task_args: &Value) -> String {
        let (action, key, fallback) = match self {
            Self::Create => ("create", "title", "task"),
            Self::Update => ("update", "task_id", "task"),
            Self::Stop => ("stop", "task_id", "task"),
            Self::Archive => ("archive", "task_id", "bulk"),
        };
        format!(
            "task:{action}:{}",
            task_args
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or(fallback)
        )
    }
}

#[derive(Debug)]
pub(crate) struct TaskMutationRollback {
    pub(crate) snapshot: TaskManagerSnapshot,
    pub(crate) label: String,
    pub(crate) event_reason: &'static str,
}

#[derive(Debug)]
pub(crate) struct TaskMutationOutcome {
    pub(crate) output: String,
    pub(crate) rollback: Option<TaskMutationRollback>,
}

#[derive(Debug)]
pub(crate) struct TaskToolOutcome {
    pub(crate) result: astra_tools::ToolResult,
    pub(crate) rollback: Option<TaskMutationRollback>,
}

pub(crate) fn public_task_arguments(args: &Value) -> Value {
    crate::server::tool_exactly_once::public_tool_arguments(args)
}

pub(crate) async fn task_list(task_manager: &TaskManager, args: &Value) -> String {
    let task_args = public_task_arguments(args);
    task_manager.list(&task_args).await
}

pub(crate) async fn task_get(task_manager: &TaskManager, args: &Value) -> String {
    let task_args = public_task_arguments(args);
    task_manager.get(&task_args).await
}

pub(crate) async fn task_list_user(task_manager: &TaskManager, args: &Value) -> String {
    let status_filter = match normalize_task_user_status(args) {
        Ok(status) => status,
        Err(err) => return err,
    };
    let sessions = match task_manager.store().load_all_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => return format!("Error: {error}"),
    };

    let mut rows = Vec::new();
    for (session_id, tasks) in sessions {
        for task in tasks {
            if task_user_status_matches(status_filter, task.status) {
                rows.push(json!({
                    "session_id": session_id,
                    "todo_id": task.id,
                    "title": task.title,
                    "status": task.status.to_string(),
                    "updated_at": task.updated_at,
                }));
            }
        }
    }
    rows.sort_by(|a, b| {
        let a_updated = a.get("updated_at").and_then(Value::as_str).unwrap_or("");
        let b_updated = b.get("updated_at").and_then(Value::as_str).unwrap_or("");
        b_updated.cmp(a_updated)
    });
    rows.truncate(200);
    let total = rows.len();
    format!(
        "Cross-session todos: {total} item(s)\n{}",
        json!({
            "tasks": rows,
            "total": total,
        })
    )
}

pub(crate) fn task_adopt_requires_http_endpoint_result() -> String {
    "Error: task(action='adopt') requires the HTTP /sessions/{session_id}/todos:execute endpoint so the source migrate and target clone use the transactional MatrixOne CAS path"
        .to_string()
}

pub(crate) async fn execute_task_mutation(
    task_manager: &TaskManager,
    args: &Value,
    kind: TaskMutationKind,
) -> TaskMutationOutcome {
    let mut snapshot = match task_manager.try_snapshot_state().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return TaskMutationOutcome {
                output: format!("Error: failed to capture task rollback snapshot: {error}"),
                rollback: None,
            };
        }
    };
    let task_args = public_task_arguments(args);
    let output = match kind {
        TaskMutationKind::Create => task_manager.create(&task_args).await,
        TaskMutationKind::Update => task_manager.update(&task_args).await,
        TaskMutationKind::Stop => task_manager.stop(&task_args).await,
        TaskMutationKind::Archive => task_manager.archive(&task_args).await,
    };
    if !task_output_success(&output) {
        return TaskMutationOutcome {
            output,
            rollback: None,
        };
    }
    if let Err(error) = task_manager.seal_snapshot_for_restore(&mut snapshot).await {
        // Mutation succeeded but the snapshot is stale (concurrent mutation
        // or version read failure) — skip rollback without reporting an
        // error to the user. The mutation itself was applied correctly.
        tracing::warn!(
            target: "astra_runtime::task",
            error = %error,
            mutation = ?kind,
            "task mutation succeeded but rollback snapshot seal failed — skipping rollback"
        );
        return TaskMutationOutcome {
            output,
            rollback: None,
        };
    }
    TaskMutationOutcome {
        rollback: Some(TaskMutationRollback {
            snapshot,
            label: kind.rollback_label(&task_args),
            event_reason: kind.event_reason(),
        }),
        output,
    }
}

pub(crate) async fn execute_task_tool(task_manager: &TaskManager, args: &Value) -> TaskToolOutcome {
    let action_value = args.get("action");
    let action = match action_value {
        Some(Value::String(action)) => action.as_str(),
        Some(_) => {
            return task_tool_result("Error: field 'action' must be a string".to_string(), None);
        }
        None => {
            return task_tool_result(
                "Error: missing required parameter `action` for `task`. Use one of: create, update, list, get, stop, list_user, adopt, archive.".to_string(),
                None,
            );
        }
    };

    match action {
        "create" => {
            execute_validated_task_mutation(task_manager, args, TaskMutationKind::Create).await
        }
        "list" => execute_validated_task_read(task_manager, args, "list").await,
        "get" => execute_validated_task_read(task_manager, args, "get").await,
        "update" => {
            execute_validated_task_mutation(task_manager, args, TaskMutationKind::Update).await
        }
        "stop" => execute_validated_task_mutation(task_manager, args, TaskMutationKind::Stop).await,
        "list_user" => execute_validated_task_read(task_manager, args, "list_user").await,
        "adopt" => match validate_task_tool_args_for_action("adopt", args) {
            Ok(()) => task_tool_result(task_adopt_requires_http_endpoint_result(), None),
            Err(error) => task_tool_result(format!("Error: {error}"), None),
        },
        "archive" => {
            execute_validated_task_mutation(task_manager, args, TaskMutationKind::Archive).await
        }
        other => task_tool_result(
            format!(
                "Error: unknown `task` action '{other}'. Valid: create, update, list, get, stop, list_user, adopt, archive."
            ),
            None,
        ),
    }
}

async fn execute_validated_task_mutation(
    task_manager: &TaskManager,
    args: &Value,
    kind: TaskMutationKind,
) -> TaskToolOutcome {
    let action = match kind {
        TaskMutationKind::Create => "create",
        TaskMutationKind::Update => "update",
        TaskMutationKind::Stop => "stop",
        TaskMutationKind::Archive => "archive",
    };
    match validate_task_tool_args_for_action(action, args) {
        Ok(()) => {
            let outcome = execute_task_mutation(task_manager, args, kind).await;
            task_tool_result(outcome.output, outcome.rollback)
        }
        Err(error) => task_tool_result(format!("Error: {error}"), None),
    }
}

async fn execute_validated_task_read(
    task_manager: &TaskManager,
    args: &Value,
    action: &str,
) -> TaskToolOutcome {
    match validate_task_tool_args_for_action(action, args) {
        Ok(()) => {
            let output = match action {
                "list" => task_list(task_manager, args).await,
                "get" => task_get(task_manager, args).await,
                "list_user" => task_list_user(task_manager, args).await,
                _ => unreachable!("validated task read action must be known"),
            };
            task_tool_result(output, None)
        }
        Err(error) => task_tool_result(format!("Error: {error}"), None),
    }
}

fn task_tool_result(output: String, rollback: Option<TaskMutationRollback>) -> TaskToolOutcome {
    TaskToolOutcome {
        result: tool_result_from_output(output),
        rollback,
    }
}

/// Server-side entry point for the `task` tool. Delegates to
/// [`execute_task_tool`] and records rollback handles plus task-board
/// work-surface snapshots on the executor.
pub(super) async fn execute_with_executor(
    executor: &ServerToolExecutor,
    args: &Value,
) -> astra_tools::ToolResult {
    let outcome = execute_task_tool(&executor.task_manager(), args).await;
    if let Some(rollback) = outcome.rollback {
        tool_session_state_rollback::record(
            &executor.session_state_journal,
            executor.journal_turn_index.load(Ordering::Relaxed),
            rollback.label,
            SessionStateRollbackAction::TaskState {
                snapshot: rollback.snapshot,
            },
        );
        executor
            .emit_task_board_snapshot(rollback.event_reason, args)
            .await;
    }
    outcome.result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astra_tools::task_mgmt::{InMemoryTaskStore, TaskManager};
    use serde_json::json;

    use super::*;

    #[test]
    fn task_output_success_treats_plain_summary_as_success() {
        assert!(task_output_success("Created task #5: build PR"));
    }

    #[test]
    fn task_output_success_accepts_summary_plus_json_body() {
        let out = "Created task #5\n{\"success\": true, \"id\": 5}";
        assert!(task_output_success(out));
    }

    #[test]
    fn task_output_success_rejects_error_prefix() {
        assert!(!task_output_success("Error: title is required"));
    }

    #[test]
    fn task_output_success_rejects_explicit_success_false() {
        let out = "Failed to create\n{\"success\": false, \"reason\": \"dup\"}";
        assert!(!task_output_success(out));
    }

    #[test]
    fn task_output_success_accepts_unparseable_json_body() {
        let out = "Created task #5\n{not actually json";
        assert!(task_output_success(out));
    }

    #[test]
    fn validate_task_tool_args_rejects_unknown_user_field_but_allows_internal_fields() {
        let err = validate_task_tool_args_for_action(
            "create",
            &json!({"action": "create", "title": "ship", "unexpected": true}),
        )
        .expect_err("unknown user field must fail");
        assert!(err.contains("unknown field 'unexpected'"));

        validate_task_tool_args_for_action(
            "create",
            &json!({"action": "create", "title": "ship", "_internal": true}),
        )
        .expect("internal fields are reserved for runtime plumbing");
    }

    #[test]
    fn validate_task_tool_args_reports_action_that_owns_known_field() {
        let err = validate_task_tool_args_for_action(
            "update",
            &json!({"action": "update", "task_id": "task-1", "subtasks": []}),
        )
        .expect_err("subtasks are create-only");

        assert!(err.contains("unknown field 'subtasks' for task.update"));
        assert!(err.contains("field is valid for: task.create"), "{err}");
    }

    #[test]
    fn validate_task_tool_args_matches_task_manager_reason_contract() {
        validate_task_tool_args_for_action(
            "update",
            &json!({"action": "update", "task_id": "task-1", "new_status": "failed", "reason": "blocked"}),
        )
        .expect("task.update supports reason");

        validate_task_tool_args_for_action(
            "archive",
            &json!({"action": "archive", "task_id": "task-1", "reason": "old history"}),
        )
        .expect("task.archive supports reason");
    }

    #[test]
    fn validate_task_tool_args_rejects_non_object_arguments() {
        let err = validate_task_tool_args_for_action("create", &json!(["create"]))
            .expect_err("non-object task args must fail");
        assert!(err.contains("arguments must be an object"));
    }

    #[test]
    fn normalize_task_user_status_defaults_and_rejects_bad_values() {
        assert_eq!(normalize_task_user_status(&json!({})).unwrap(), "active");
        assert!(
            normalize_task_user_status(&json!({"user_status": 7}))
                .expect_err("non-string status must fail")
                .contains("must be a string")
        );
        assert!(
            normalize_task_user_status(&json!({"user_status": "not-a-status"}))
                .expect_err("unsupported status must fail")
                .contains("invalid user_status")
        );
    }

    #[test]
    fn task_user_status_matches_active_all_and_exact_statuses() {
        assert!(task_user_status_matches(
            "active",
            SessionTaskStatusKind::Pending
        ));
        assert!(!task_user_status_matches(
            "active",
            SessionTaskStatusKind::Completed
        ));
        assert!(task_user_status_matches(
            "all",
            SessionTaskStatusKind::Archived
        ));
        assert!(task_user_status_matches(
            "completed",
            SessionTaskStatusKind::Completed
        ));
        assert!(!task_user_status_matches(
            "completed",
            SessionTaskStatusKind::Failed
        ));
    }

    #[test]
    fn mutation_kind_produces_stable_event_reasons_and_rollback_labels() {
        assert_eq!(TaskMutationKind::Create.event_reason(), "task_create");
        assert_eq!(TaskMutationKind::Update.event_reason(), "task_update");
        assert_eq!(
            TaskMutationKind::Create.rollback_label(&json!({"title": "ship"})),
            "task:create:ship"
        );
        assert_eq!(
            TaskMutationKind::Archive.rollback_label(&json!({})),
            "task:archive:bulk"
        );
    }

    #[tokio::test]
    async fn execute_task_mutation_returns_rollback_for_successful_mutation() {
        let manager = TaskManager::new("task-mutation-success", Arc::new(InMemoryTaskStore::new()));

        let outcome = execute_task_mutation(
            &manager,
            &json!({"action": "create", "title": "ship"}),
            TaskMutationKind::Create,
        )
        .await;

        assert!(outcome.output.contains("created"), "{outcome:?}");
        assert!(outcome.output.contains("\"success\":true"), "{outcome:?}");
        let rollback = outcome
            .rollback
            .expect("successful mutation should produce rollback");
        assert_eq!(rollback.label, "task:create:ship");
        assert_eq!(rollback.event_reason, "task_create");
    }

    #[tokio::test]
    async fn execute_task_mutation_skips_rollback_for_failed_mutation_output() {
        let manager = TaskManager::new("task-mutation-failure", Arc::new(InMemoryTaskStore::new()));

        let outcome = execute_task_mutation(
            &manager,
            &json!({"action": "create"}),
            TaskMutationKind::Create,
        )
        .await;

        assert!(outcome.output.starts_with("Error:"), "{outcome:?}");
        assert!(
            outcome.rollback.is_none(),
            "failed mutation output must not snapshot rollback"
        );
    }

    #[tokio::test]
    async fn execute_task_tool_rejects_missing_and_invalid_action() {
        let manager = TaskManager::new("task-tool-invalid", Arc::new(InMemoryTaskStore::new()));

        let missing = execute_task_tool(&manager, &json!({})).await;
        assert!(missing.result.is_error, "{missing:?}");
        assert!(missing.result.output.contains("missing required parameter"));
        assert!(missing.rollback.is_none());

        let invalid = execute_task_tool(&manager, &json!({"action": 7})).await;
        assert!(invalid.result.is_error, "{invalid:?}");
        assert!(invalid.result.output.contains("must be a string"));
        assert!(invalid.rollback.is_none());
    }

    #[tokio::test]
    async fn execute_task_tool_rejects_unknown_user_field_before_mutation() {
        let manager = TaskManager::new("task-tool-bad-field", Arc::new(InMemoryTaskStore::new()));

        let outcome = execute_task_tool(
            &manager,
            &json!({"action": "create", "title": "ship", "unexpected": true}),
        )
        .await;

        assert!(outcome.result.is_error, "{outcome:?}");
        assert!(outcome.result.output.contains("unknown field 'unexpected'"));
        assert!(outcome.rollback.is_none());
        let tasks = manager.store().load("task-tool-bad-field").await.unwrap();
        assert!(
            tasks.is_empty(),
            "invalid task arguments must not mutate task state"
        );
    }

    #[tokio::test]
    async fn execute_task_tool_returns_rollback_for_successful_create() {
        let manager = TaskManager::new("task-tool-create", Arc::new(InMemoryTaskStore::new()));

        let outcome =
            execute_task_tool(&manager, &json!({"action": "create", "title": "ship"})).await;

        assert!(!outcome.result.is_error, "{outcome:?}");
        assert!(outcome.result.output.contains("\"success\":true"));
        let rollback = outcome.rollback.expect("successful create rollback");
        assert_eq!(rollback.label, "task:create:ship");
        assert_eq!(rollback.event_reason, "task_create");
    }
}
