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
        .snapshot()
        .await
        .into_iter()
        .find(|task| {
            task.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source"))
                .and_then(serde_json::Value::as_str)
                == Some("approved_plan")
                && task
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_fingerprint"))
                    .and_then(serde_json::Value::as_str)
                    == Some(plan_fingerprint.as_str())
        })
        .map(|task| task.id);

    let task_id = if let Some(task_id) = existing_task_id {
        task_id
    } else {
        let subtasks: Vec<serde_json::Value> = plan
            .subtasks
            .iter()
            .map(|subtask| {
                serde_json::json!({
                    "id": subtask.id,
                    "title": subtask.title,
                    "description": subtask.description,
                    "depends_on": subtask.depends_on,
                })
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
            .snapshot()
            .await
            .into_iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
                    && task
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("plan_fingerprint"))
                        .and_then(serde_json::Value::as_str)
                        == Some(plan_fingerprint.as_str())
            })
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
                .snapshot()
                .await
                .into_iter()
                .find(|task| {
                    task.metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("source"))
                        .and_then(serde_json::Value::as_str)
                        == Some("approved_plan")
                        && task
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("plan_fingerprint"))
                            .and_then(serde_json::Value::as_str)
                            == Some(plan_fingerprint.as_str())
                })
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

    let output = state
        .task_manager
        .update(&serde_json::json!({
            "task_id": task_id,
            "new_status": "in_progress",
            "metadata": {
                "source": "approved_plan",
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

pub(crate) fn plan_task_board_fingerprint(
    plan: &astra_services::task_orchestrator::TaskPlan,
) -> String {
    let mut parts = Vec::new();
    for subtask in &plan.subtasks {
        parts.push(format!("{}:{}", subtask.id, subtask.title));
    }
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::mirror_plan_to_task_board;
    use crate::cli::session::session_state::SessionState;
    use astra_services::task_orchestrator::SubtaskPlan;

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
    }
}
