use crate::cli::session::session_state::SessionState;

pub(crate) async fn mirror_plan_to_task_board(
    state: &SessionState,
    goal: &str,
    plan: &astra_services::task_orchestrator::TaskPlan,
) -> Result<(), String> {
    if plan.subtasks.is_empty() {
        return Ok(());
    }
    let plan_fingerprint = plan_task_board_fingerprint(plan);

    let existing_task_id = state
        .task_manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board before approved-plan mirror: {error}"))?
        .into_iter()
        .find(|task| approved_plan_task_matches(task, goal, &plan_fingerprint))
        .map(|task| task.id);

    let task_id = if let Some(task_id) = existing_task_id {
        task_id
    } else {
        let subtasks: Vec<serde_json::Value> = plan
            .subtasks
            .iter()
            .map(|subtask| {
                let mut value = serde_json::json!({
                    "id": subtask.id,
                    "title": subtask.title,
                    "depends_on": subtask.depends_on,
                });
                if let Some(description) = subtask.description.as_deref() {
                    value["description"] = serde_json::json!(description);
                }
                value
            })
            .collect();
        let output = state
            .task_manager
            .create(&serde_json::json!({
                "title": goal,
                "description": format!(
                    "Approved plan: {} step(s). This is the user-visible task tree for plan execution.",
                    plan.subtasks.len()
                ),
                "active_form": "Executing approved plan",
                "metadata": {
                    "source": "approved_plan",
                    "plan_goal": goal,
                    "plan_fingerprint": plan_fingerprint,
                    "step_count": plan.subtasks.len(),
                },
                "subtasks": subtasks,
            }))
            .await;
        if output.starts_with("Error:") {
            return Err(output);
        }
        let created_task_id = state
            .task_manager
            .load_active_tasks()
            .await
            .map_err(|error| format!("load task board after approved-plan create: {error}"))?
            .into_iter()
            .find(|task| approved_plan_task_matches(task, goal, &plan_fingerprint))
            .map(|task| task.id);
        if let Some(task_id) = created_task_id {
            task_id
        } else if output.contains("already has this title") || output.contains("duplicate_of") {
            let suffix: String = plan_fingerprint.chars().take(16).collect();
            let disambiguated_title = if suffix.is_empty() {
                format!("{goal} (new plan)")
            } else {
                format!("{goal} ({suffix})")
            };
            let output = state
                .task_manager
                .create(&serde_json::json!({
                    "title": disambiguated_title,
                    "description": format!(
                        "Approved plan: {} step(s). This is the user-visible task tree for plan execution.",
                        plan.subtasks.len()
                    ),
                    "active_form": "Executing approved plan",
                    "metadata": {
                        "source": "approved_plan",
                        "plan_goal": goal,
                        "plan_fingerprint": plan_fingerprint,
                        "step_count": plan.subtasks.len(),
                    },
                    "subtasks": subtasks,
                }))
                .await;
            if output.starts_with("Error:") {
                return Err(output);
            }
            state
                .task_manager
                .load_active_tasks()
                .await
                .map_err(|error| {
                    format!("load task board after approved-plan duplicate-title retry: {error}")
                })?
                .into_iter()
                .find(|task| approved_plan_task_matches(task, goal, &plan_fingerprint))
                .map(|task| task.id)
                .ok_or_else(|| {
                    format!(
                        "approved plan '{goal}' was not visible in task board after duplicate-title retry"
                    )
                })?
        } else {
            return Err(format!(
                "approved plan '{goal}' was not visible in task board after task.create"
            ));
        }
    };

    pause_other_in_progress_tasks_for_plan_handoff(state, &task_id).await?;

    let output = state
        .task_manager
        .update(&serde_json::json!({
            "task_id": task_id,
            "new_status": "in_progress",
            "metadata": {
                "source": "approved_plan",
                "plan_goal": goal,
                "plan_fingerprint": plan_fingerprint,
                "step_count": plan.subtasks.len(),
            }
        }))
        .await;
    if output.starts_with("Error:") {
        return Err(output);
    }
    Ok(())
}

async fn pause_other_in_progress_tasks_for_plan_handoff(
    state: &SessionState,
    target_task_id: &str,
) -> Result<(), String> {
    let running_task_ids: Vec<String> = state
        .task_manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board before approved-plan handoff: {error}"))?
        .into_iter()
        .filter(|task| task.id != target_task_id && task.status.is_in_progress())
        .map(|task| task.id)
        .collect();
    for running_task_id in running_task_ids {
        let output = state
            .task_manager
            .update(&serde_json::json!({
                "task_id": running_task_id,
                "new_status": "paused",
                "metadata": {
                    "auto_paused_reason": "approved_plan_handoff",
                    "handoff_to_task_id": target_task_id,
                }
            }))
            .await;
        if output.starts_with("Error:") {
            return Err(output);
        }
    }
    Ok(())
}

pub(crate) fn approved_plan_task_matches(
    task: &astra_tools::task_mgmt::SessionTask,
    goal: &str,
    plan_fingerprint: &str,
) -> bool {
    if !task.status.is_open_work() {
        return false;
    }
    let metadata = task.metadata.as_ref();
    let source = metadata
        .and_then(|metadata| metadata.get("source"))
        .and_then(serde_json::Value::as_str);
    let task_goal = metadata
        .and_then(|metadata| metadata.get("plan_goal"))
        .and_then(serde_json::Value::as_str);
    let task_fingerprint = metadata
        .and_then(|metadata| metadata.get("plan_fingerprint"))
        .and_then(serde_json::Value::as_str);

    source == Some("approved_plan")
        && task_fingerprint == Some(plan_fingerprint)
        && match task_goal {
            Some(existing_goal) => existing_goal == goal,
            None => task.title == goal,
        }
}

pub(crate) fn plan_task_board_fingerprint(
    plan: &astra_services::task_orchestrator::TaskPlan,
) -> String {
    let parts: Vec<serde_json::Value> = plan
        .subtasks
        .iter()
        .map(|subtask| {
            let mut depends_on = subtask.depends_on.clone();
            depends_on.sort();
            serde_json::json!({
                "id": subtask.id,
                "title": subtask.title,
                "description": subtask.description,
                "depends_on": depends_on,
            })
        })
        .collect();
    serde_json::to_string(&parts).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::mirror_plan_to_task_board;
    use crate::cli::session::session_state::SessionState;
    use astra_services::task_orchestrator::SubtaskPlan;
    use astra_tools::task_mgmt::{
        MAX_CREATE_SUBTASKS, SessionTask, SessionTaskStatusKind, TaskManager, TaskMutation,
        TaskStore,
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
            .into_iter()
            .filter(|task| task.title.starts_with("same goal"))
            .collect();
        assert_eq!(
            tasks.len(),
            2,
            "same goal text with a different plan shape should create a separate visible task tree"
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
            error.contains("load task board before approved-plan mirror")
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
            .expect_err("oversized approved plans should not create one huge task tree");
        assert!(
            err.contains("subtasks") && err.contains("maximum"),
            "oversized approved plan should surface the task fan-out limit: {err}"
        );
        assert!(
            state.task_manager.snapshot().await.is_empty(),
            "rejected oversized plan should not leave a partial task tree"
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

        let tasks = state.task_manager.snapshot().await;
        assert!(
            tasks.iter().any(|task| task.title == "ship task UX"),
            "first plan goal should remain visible: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|task| task.title == "ship plan UX"),
            "second plan goal should create its own visible task tree: {tasks:?}"
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
            .into_iter()
            .filter(|task| task.title.starts_with("ship dependency-sensitive plan"))
            .collect();
        assert_eq!(
            tasks.len(),
            2,
            "dependency changes must create a fresh visible task tree instead of reusing stale ordering: {tasks:?}"
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
            .into_iter()
            .filter(|task| task.title == "repeatable plan")
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
