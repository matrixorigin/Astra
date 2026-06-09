//! CLI thin wrapper over the shared [`astra_tools::plan_task_mirror`].
//!
//! Delegates to the single source of truth in `astra_tools`. Exists only to
//! destructure [`SessionState`] into the primitives the shared module expects.

use crate::cli::session::session_state::SessionState;
use astra_services::task_orchestrator::TaskPlan;

/// Mirror an approved plan into the session's task board.
pub(crate) async fn mirror_plan_to_task_board(
    state: &SessionState,
    goal: &str,
    plan: &TaskPlan,
) -> Result<(), String> {
    let session_id = state.session_id.as_deref().unwrap_or("").to_string();
    astra_tools::plan_task_mirror::mirror_approved_plan_to_task_board(
        &state.task_manager,
        "cli",
        &session_id,
        goal, // plan_id uses the goal string (CLI doesn't have a separate plan_id)
        goal,
        plan,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::mirror_plan_to_task_board;
    use crate::cli::session::session_state::SessionState;
    use astra_services::task_orchestrator::SubtaskPlan;
    use astra_tools::task_mgmt::{
        MAX_CREATE_SUBTASKS, MAX_TASK_TITLE_CHARS, SessionTask, SessionTaskStatusKind, TaskManager,
        TaskMutation, TaskStore,
    };

    struct FailingLoadTaskStore;

    #[async_trait::async_trait]
    impl TaskStore for FailingLoadTaskStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err(format!("forced task-board load failure for {session_id}"))
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn mutate(
            &self,
            session_id: &str,
            _mutation: TaskMutation,
        ) -> Result<String, String> {
            Err(format!("forced task-board mutate failure for {session_id}"))
        }

        async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
            Err(format!("forced task id failure for {session_id}"))
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn set_next_task_id(&self, _session_id: &str, _next_id: u32) -> Result<(), String> {
            Ok(())
        }
    }

    fn is_approved_plan_goal(task: &SessionTask, goal: &str) -> bool {
        task.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("source"))
            .and_then(serde_json::Value::as_str)
            == Some("approved_plan")
            && task
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_goal"))
                .and_then(serde_json::Value::as_str)
                == Some(goal)
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_creates_one_top_level_task_per_plan_step() {
        let state = SessionState::default();
        let plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "Build backend".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Verify API".into(),
                    depends_on: vec!["s1".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        mirror_plan_to_task_board(&state, "Ship reimbursements", &plan)
            .await
            .unwrap();

        let tasks = state.task_manager.snapshot().await.unwrap();
        let approved: Vec<_> = tasks
            .iter()
            .filter(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
            })
            .collect();
        assert_eq!(
            approved.len(),
            2,
            "approved plan should create one top-level task per plan step, not one umbrella task: {tasks:?}"
        );
        assert!(
            approved.iter().all(|task| task.subtasks.is_empty()),
            "plan steps should be top-level tasks; approved-plan umbrella subtasks are not user-visible enough: {approved:?}"
        );
        assert!(approved.iter().any(|task| {
            task.title == "Build backend"
                && task.status == SessionTaskStatusKind::InProgress
                && task
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("s1")
        }));
        let verify = approved
            .iter()
            .find(|task| task.title == "Verify API")
            .expect("second plan step task");
        assert_eq!(verify.status, SessionTaskStatusKind::Pending);
        assert_eq!(
            verify.blocked_by.len(),
            1,
            "task dependencies should mirror plan step dependencies: {verify:?}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_does_not_reuse_same_goal_with_different_steps() {
        let state = SessionState::default();
        let first = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "one".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let second = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s2".into(),
                title: "two".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        mirror_plan_to_task_board(&state, "same goal", &first)
            .await
            .unwrap();
        mirror_plan_to_task_board(&state, "same goal", &second)
            .await
            .unwrap();

        let tasks: Vec<_> = state
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| is_approved_plan_goal(task, "same goal"))
            .collect();
        assert_eq!(
            tasks.len(),
            2,
            "same goal text with a different plan shape should create separate visible step tasks"
        );
        assert_eq!(
            tasks
                .iter()
                .filter(|task| task.status.is_in_progress())
                .count(),
            1,
            "plan handoff should leave exactly one in_progress task: {tasks:?}"
        );
        assert!(
            tasks
                .iter()
                .any(|task| task.status == SessionTaskStatusKind::Paused),
            "previous mirrored plan should be paused during handoff: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_fails_closed_when_task_board_load_fails() {
        let mut state = SessionState::default();
        state.task_manager = std::sync::Arc::new(TaskManager::new(
            "plan-load-fails",
            std::sync::Arc::new(FailingLoadTaskStore),
        ));
        let plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Do not duplicate automatically".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let error = mirror_plan_to_task_board(&state, "load-failing plan", &plan)
            .await
            .expect_err("task-board load failure should abort approved-plan mirror");

        assert!(
            error.contains("task board before approved-plan mirror")
                && error.contains("forced task-board load failure"),
            "approved-plan mirror must not treat task-board load failure as an empty board: {error}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_rejects_oversized_plan() {
        let state = SessionState::default();
        let plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: (0..=MAX_CREATE_SUBTASKS)
                .map(|index| SubtaskPlan {
                    id: format!("step-{index}"),
                    title: format!("step {index}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };

        let err = mirror_plan_to_task_board(&state, "oversized approved plan", &plan)
            .await
            .expect_err("oversized approved plans should not create one huge batch of step tasks");
        assert!(
            err.contains("subtasks") && err.contains("maximum"),
            "oversized approved plan should surface the task fan-out limit: {err}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_rolls_back_partial_step_create_failure() {
        let state = SessionState::default();
        let existing = state
            .task_manager
            .create(&serde_json::json!({
                "title": "Existing user task",
            }))
            .await;
        assert!(!existing.starts_with("Error:"), "{existing}");
        let plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "Create first mirrored step".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "x".repeat(MAX_TASK_TITLE_CHARS + 1),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let err = mirror_plan_to_task_board(&state, "rollback partial plan", &plan)
            .await
            .expect_err("invalid later step should abort approved-plan mirror");

        assert!(
            err.contains("title") && err.contains("exceeds"),
            "original create validation error should be surfaced: {err}"
        );
        let tasks = state.task_manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "failed approved-plan mirror must roll back partial plan tasks: {tasks:?}"
        );
        assert_eq!(tasks[0].title, "Existing user task");
        assert!(
            tasks[0].metadata.is_none(),
            "rollback must preserve unrelated user task exactly: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_does_not_reuse_different_goal_with_same_steps() {
        let state = SessionState::default();
        let plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "shared implementation step".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        mirror_plan_to_task_board(&state, "ship task UX", &plan)
            .await
            .unwrap();
        mirror_plan_to_task_board(&state, "ship plan UX", &plan)
            .await
            .unwrap();

        let tasks = state.task_manager.snapshot().await.unwrap();
        assert!(
            tasks
                .iter()
                .any(|task| is_approved_plan_goal(task, "ship task UX")),
            "first plan goal should remain visible: {tasks:?}"
        );
        assert!(
            tasks
                .iter()
                .any(|task| is_approved_plan_goal(task, "ship plan UX")),
            "second plan goal should create its own visible step task: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_does_not_reuse_same_steps_when_dependencies_change() {
        let state = SessionState::default();
        let first = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "build core".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "verify core".into(),
                    depends_on: vec!["s1".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut second = first.clone();
        second.subtasks[1].depends_on.clear();

        mirror_plan_to_task_board(&state, "ship dependency-sensitive plan", &first)
            .await
            .unwrap();
        mirror_plan_to_task_board(&state, "ship dependency-sensitive plan", &second)
            .await
            .unwrap();

        let tasks: Vec<_> = state
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| is_approved_plan_goal(task, "ship dependency-sensitive plan"))
            .collect();
        assert_eq!(
            tasks.len(),
            4,
            "dependency changes must create fresh visible step tasks instead of reusing stale ordering: {tasks:?}"
        );
    }

    #[tokio::test]
    async fn mirror_plan_to_task_board_does_not_reopen_completed_plan_history() {
        let state = SessionState::default();
        let plan = astra_services::task_orchestrator::TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "ship it".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        mirror_plan_to_task_board(&state, "repeatable plan", &plan)
            .await
            .unwrap();
        let completed = state
            .task_manager
            .update(&serde_json::json!({
                "task_id": "task-1",
                "new_status": "completed",
            }))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");

        mirror_plan_to_task_board(&state, "repeatable plan", &plan)
            .await
            .unwrap();

        let tasks: Vec<_> = state
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| is_approved_plan_goal(task, "repeatable plan"))
            .collect();
        assert_eq!(
            tasks.len(),
            2,
            "a completed approved-plan task is history and must not be reopened: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.status.is_completed()),
            "completed history should remain completed: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.status.is_in_progress()),
            "repeat approval should create a fresh in-progress task: {tasks:?}"
        );
    }
}
