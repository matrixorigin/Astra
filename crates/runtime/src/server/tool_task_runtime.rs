use astra_tools::task_mgmt::{
    SessionTaskStatusKind, TaskManager, TaskManagerSnapshot, VALID_LIST_STATUS_FILTERS,
};
use astra_tools::task_tool_contract;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

use super::tool_execution_result::tool_result_from_output;
use crate::server::runtime_tool_executor::RuntimeToolExecutor;
use crate::server::tool_session_state_rollback::{self, SessionStateRollbackAction};

pub(crate) fn validate_task_tool_args_for_action(action: &str, args: &Value) -> Result<(), String> {
    task_tool_contract::validate_runtime_task_tool_args_for_action(action, args)
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
            Self::Create => "task_board.create",
            Self::Update => "task_board.update",
            Self::Stop => "task_board.stop",
            Self::Archive => "task_board.archive",
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
}

#[derive(Debug)]
pub(crate) struct TaskMutationExecution {
    pub(crate) output: String,
    pub(crate) rollback: Option<TaskMutationRollback>,
    /// Present exactly when the mutation durably changed the board. This is
    /// independent of whether an optional rollback snapshot could be sealed.
    pub(crate) event_reason: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) struct TaskToolOutcome {
    pub(crate) result: astra_tools::ToolResult,
    pub(crate) rollback: Option<TaskMutationRollback>,
    pub(crate) event_reason: Option<&'static str>,
}

pub(crate) fn public_task_arguments(args: &Value) -> Value {
    astra_turn_types::canonical_public_tool_arguments(args)
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
    "Error: task_board(action='adopt') requires the HTTP /sessions/{session_id}/todos:execute endpoint so the source migrate and target clone use the transactional MatrixOne CAS path"
        .to_string()
}

pub(crate) async fn execute_task_mutation(
    task_manager: &TaskManager,
    args: &Value,
    kind: TaskMutationKind,
) -> TaskMutationExecution {
    let mut snapshot = match task_manager.try_snapshot_state().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return TaskMutationExecution {
                output: format!("Error: failed to capture task rollback snapshot: {error}"),
                rollback: None,
                event_reason: None,
            };
        }
    };
    let task_args = public_task_arguments(args);
    let mutation = match kind {
        TaskMutationKind::Create => task_manager.create_outcome(&task_args).await,
        TaskMutationKind::Update => task_manager.update_outcome(&task_args).await,
        TaskMutationKind::Stop => task_manager.stop_outcome(&task_args).await,
        TaskMutationKind::Archive => task_manager.archive_outcome(&task_args).await,
    };
    if !mutation.status.changed() {
        return TaskMutationExecution {
            output: mutation.output,
            rollback: None,
            event_reason: None,
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
        return TaskMutationExecution {
            output: mutation.output,
            rollback: None,
            event_reason: Some(kind.event_reason()),
        };
    }
    TaskMutationExecution {
        rollback: Some(TaskMutationRollback {
            snapshot,
            label: kind.rollback_label(&task_args),
        }),
        output: mutation.output,
        event_reason: Some(kind.event_reason()),
    }
}

pub(crate) async fn execute_task_tool(task_manager: &TaskManager, args: &Value) -> TaskToolOutcome {
    let action = match task_tool_contract::task_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return task_tool_result(format!("Error: {error}"), None, None),
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
            Ok(()) => task_tool_result(task_adopt_requires_http_endpoint_result(), None, None),
            Err(error) => task_tool_result(format!("Error: {error}"), None, None),
        },
        "archive" => {
            execute_validated_task_mutation(task_manager, args, TaskMutationKind::Archive).await
        }
        other => task_tool_result(
            format!(
                "Error: {}",
                task_tool_contract::task_unknown_action_message(other)
            ),
            None,
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
            task_tool_result(outcome.output, outcome.rollback, outcome.event_reason)
        }
        Err(error) => task_tool_result(format!("Error: {error}"), None, None),
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
            task_tool_result(output, None, None)
        }
        Err(error) => task_tool_result(format!("Error: {error}"), None, None),
    }
}

fn task_tool_result(
    output: String,
    rollback: Option<TaskMutationRollback>,
    event_reason: Option<&'static str>,
) -> TaskToolOutcome {
    TaskToolOutcome {
        result: tool_result_from_output(output),
        rollback,
        event_reason,
    }
}

/// Server-side entry point for the `task_board` tool. Delegates to
/// [`execute_task_tool`] and records rollback handles plus task-board
/// work-surface snapshots on the executor.
pub(super) async fn execute_with_executor(
    executor: &RuntimeToolExecutor,
    args: &Value,
    run_id: Option<&str>,
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
    }
    if let Some(event_reason) = outcome.event_reason {
        executor
            .emit_task_board_snapshot(event_reason, run_id, args)
            .await;
    }
    outcome.result
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use astra_tools::task_mgmt::{
        InMemoryTaskStore, SessionTask, TaskManager, TaskMutation, TaskMutationOutcome, TaskStore,
    };
    use serde_json::json;

    use super::*;

    struct SealConflictStore {
        inner: InMemoryTaskStore,
        version_reads: AtomicUsize,
    }

    impl SealConflictStore {
        fn new() -> Self {
            Self {
                inner: InMemoryTaskStore::new(),
                version_reads: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl TaskStore for SealConflictStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            self.inner.load(session_id).await
        }

        async fn save(&self, session_id: &str, tasks: Vec<SessionTask>) -> Result<(), String> {
            self.inner.save(session_id, tasks).await
        }

        async fn mutate(
            &self,
            session_id: &str,
            mutation: TaskMutation,
        ) -> Result<TaskMutationOutcome, String> {
            self.inner.mutate(session_id, mutation).await
        }

        async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
            self.inner.next_task_id(session_id).await
        }

        async fn peek_next_task_id(&self, session_id: &str) -> Result<u32, String> {
            self.inner.peek_next_task_id(session_id).await
        }

        async fn get_session_version(&self, _session_id: &str) -> Result<u64, String> {
            let read = self.version_reads.fetch_add(1, Ordering::SeqCst);
            Ok(if read < 2 { 0 } else { 2 })
        }
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

        assert!(err.contains("unknown field 'subtasks' for task_board.update"));
        assert!(
            err.contains("field is valid for: task_board.create"),
            "{err}"
        );
    }

    #[test]
    fn validate_task_tool_args_matches_task_manager_reason_contract() {
        validate_task_tool_args_for_action(
            "update",
            &json!({"action": "update", "task_id": "task-1", "new_status": "failed", "reason": "blocked"}),
        )
        .expect("task_board.update supports reason");

        validate_task_tool_args_for_action(
            "archive",
            &json!({"action": "archive", "task_id": "task-1", "reason": "old history"}),
        )
        .expect("task_board.archive supports reason");
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
        assert_eq!(TaskMutationKind::Create.event_reason(), "task_board.create");
        assert_eq!(TaskMutationKind::Update.event_reason(), "task_board.update");
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
        assert_eq!(outcome.event_reason, Some("task_board.create"));
        let rollback = outcome
            .rollback
            .expect("successful mutation should produce rollback");
        assert_eq!(rollback.label, "task:create:ship");
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
        assert_eq!(outcome.event_reason, None);
    }

    #[tokio::test]
    async fn successful_mutation_remains_observable_when_rollback_seal_conflicts() {
        let store = Arc::new(SealConflictStore::new());
        let manager = TaskManager::new("task-mutation-seal-conflict", store.clone());

        let outcome = execute_task_mutation(
            &manager,
            &json!({"action": "create", "title": "ship"}),
            TaskMutationKind::Create,
        )
        .await;

        assert!(outcome.rollback.is_none(), "{outcome:?}");
        assert_eq!(outcome.event_reason, Some("task_board.create"));
        assert_eq!(
            store
                .load("task-mutation-seal-conflict")
                .await
                .expect("persisted task")
                .len(),
            1
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

        let hidden_alias =
            execute_task_tool(&manager, &json!({"action": "cancel", "task_id": "task-1"})).await;
        assert!(hidden_alias.result.is_error, "{hidden_alias:?}");
        assert!(
            hidden_alias
                .result
                .output
                .contains("unknown `task_board` action")
                && hidden_alias.result.output.contains("cancel"),
            "schema-hidden action aliases must fail closed: {hidden_alias:?}"
        );
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
        assert_eq!(outcome.event_reason, Some("task_board.create"));
        let rollback = outcome.rollback.expect("successful create rollback");
        assert_eq!(rollback.label, "task:create:ship");
    }
}
