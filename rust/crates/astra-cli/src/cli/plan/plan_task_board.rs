use crate::cli::session::session_state::SessionState;
use astra_tools::task_mgmt::MAX_CREATE_SUBTASKS;

pub(crate) async fn mirror_plan_to_task_board(
    state: &SessionState,
    goal: &str,
    plan: &astra_services::task_orchestrator::TaskPlan,
) -> Result<(), String> {
    if plan.subtasks.is_empty() {
        return Ok(());
    }
    if plan.subtasks.len() > MAX_CREATE_SUBTASKS {
        return Err(format!(
            "approved plan has {} step(s); maximum is {MAX_CREATE_SUBTASKS}. Split oversized subtasks into separate plans.",
            plan.subtasks.len()
        ));
    }
    let snapshot = state
        .task_manager
        .try_snapshot_state()
        .await
        .map_err(|error| format!("snapshot task board before approved-plan mirror: {error}"))?;
    match mirror_plan_to_task_board_inner(state, goal, plan).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(restore_error) = state.task_manager.restore_snapshot(&snapshot).await {
                return Err(format!(
                    "{error}; additionally failed to roll back approved-plan task-board mirror: {restore_error}"
                ));
            }
            Err(error)
        }
    }
}

async fn mirror_plan_to_task_board_inner(
    state: &SessionState,
    goal: &str,
    plan: &astra_services::task_orchestrator::TaskPlan,
) -> Result<(), String> {
    let plan_fingerprint = plan_task_board_fingerprint(plan);

    let mut step_task_ids = std::collections::HashMap::new();
    for (index, subtask) in plan.subtasks.iter().enumerate() {
        let task_id = ensure_plan_step_task(
            state,
            goal,
            &plan_fingerprint,
            plan.subtasks.len(),
            index,
            subtask,
        )
        .await?;
        step_task_ids.insert(subtask.id.clone(), task_id);
    }

    for subtask in &plan.subtasks {
        let Some(task_id) = step_task_ids.get(&subtask.id) else {
            continue;
        };
        let blockers: Vec<String> = subtask
            .depends_on
            .iter()
            .filter_map(|dep_id| step_task_ids.get(dep_id).cloned())
            .collect();
        if blockers.is_empty() {
            continue;
        }
        let needs_edges = state
            .task_manager
            .load_active_tasks()
            .await
            .map_err(|error| {
                format!("load task board before approved-plan dependency sync: {error}")
            })?
            .into_iter()
            .find(|task| task.id == *task_id)
            .is_none_or(|task| blockers.iter().any(|id| !task.blocked_by.contains(id)));
        if needs_edges {
            let output = state
                .task_manager
                .update(&serde_json::json!({
                    "task_id": task_id,
                    "add_blocked_by": blockers,
                }))
                .await;
            if output.starts_with("Error:") {
                return Err(output);
            }
        }
    }

    let first_runnable_task_id = plan
        .subtasks
        .iter()
        .find(|subtask| subtask.depends_on.is_empty())
        .or_else(|| plan.subtasks.first())
        .and_then(|subtask| step_task_ids.get(&subtask.id))
        .cloned()
        .ok_or_else(|| format!("approved plan '{goal}' did not produce any task-board steps"))?;

    pause_other_in_progress_tasks_for_plan_handoff(state, &first_runnable_task_id).await?;

    let output = state
        .task_manager
        .update(&serde_json::json!({
            "task_id": first_runnable_task_id,
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

async fn ensure_plan_step_task(
    state: &SessionState,
    goal: &str,
    plan_fingerprint: &str,
    step_count: usize,
    step_index: usize,
    subtask: &astra_services::task_orchestrator::SubtaskPlan,
) -> Result<String, String> {
    let existing_task_id = state
        .task_manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board before approved-plan mirror: {error}"))?
        .into_iter()
        .find(|task| approved_plan_step_task_matches(task, goal, plan_fingerprint, &subtask.id))
        .map(|task| task.id);
    if let Some(task_id) = existing_task_id {
        return Ok(task_id);
    }

    let mut create_args = serde_json::json!({
        "title": subtask.title,
        "description": subtask.description.clone().unwrap_or_else(|| {
            format!("Approved plan step {} of {step_count} for: {goal}", step_index + 1)
        }),
        "active_form": format!("Executing: {}", subtask.title),
        "metadata": {
            "source": "approved_plan",
            "plan_goal": goal,
            "plan_fingerprint": plan_fingerprint,
            "plan_subtask_id": subtask.id,
            "plan_step_index": step_index,
            "step_count": step_count,
        },
    });
    let output = state.task_manager.create(&create_args).await;
    if output.contains("already has this title") || output.contains("duplicate_of") {
        create_args["title"] = serde_json::json!(format!("{} ({})", subtask.title, subtask.id));
        let retry = state.task_manager.create(&create_args).await;
        if retry.starts_with("Error:") {
            return Err(retry);
        }
    } else if output.starts_with("Error:") {
        return Err(output);
    }

    state
        .task_manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board after approved-plan step create: {error}"))?
        .into_iter()
        .find(|task| approved_plan_step_task_matches(task, goal, plan_fingerprint, &subtask.id))
        .map(|task| task.id)
        .ok_or_else(|| {
            format!(
                "approved plan step '{}' for '{goal}' was not visible in task board after task.create",
                subtask.id
            )
        })
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

pub(crate) fn approved_plan_step_task_matches(
    task: &astra_tools::task_mgmt::SessionTask,
    goal: &str,
    plan_fingerprint: &str,
    plan_subtask_id: &str,
) -> bool {
    if !approved_plan_task_matches(task, goal, plan_fingerprint) {
        return false;
    }
    task.metadata
        .as_ref()
        .and_then(|metadata| metadata.get("plan_subtask_id"))
        .and_then(serde_json::Value::as_str)
        == Some(plan_subtask_id)
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

        let tasks = state.task_manager.snapshot().await;
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
            error.contains("snapshot task board before approved-plan mirror")
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
        assert!(
            state.task_manager.snapshot().await.is_empty(),
            "rejected oversized plan should not leave partial task-board work"
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
        let tasks = state.task_manager.snapshot().await;
        assert_eq!(
            tasks.len(),
            1,
            "failed approved-plan mirror must roll back tasks created by earlier steps: {tasks:?}"
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

        let tasks = state.task_manager.snapshot().await;
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
