//! Shared plan → task-board mirror logic.
//!
//! Single source of truth used by both the cloud runtime
//! ([`plan_handlers`]) and the edge CLI ([`plan_task_board`]).
//!
//! ## Idempotency
//!
//! Every function is safe to retry. Step tasks carry a `plan_subtask_id` in
//! their metadata, and identity checks reuse existing tasks that match the
//! same plan/step identity. Failed mirrors are still rolled back: from the
//! user's perspective, approving a plan is one handoff transaction, not a
//! request to leave half-created board state behind.
//!
//! ## Concurrent safety
//!
//! The session tool executor serializes all tool calls per session, so only
//! one caller can enter the mirror at a time. If that ever changes, the
//! identity checks inside `ensure_plan_step_task` must be made atomic at the
//! database level.

use std::collections::HashMap;

use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

use crate::task_mgmt::{MAX_CREATE_SUBTASKS, SessionTask, SessionTaskStatusKind, TaskManager};

// ---------------------------------------------------------------------------
// Pure utility functions
// ---------------------------------------------------------------------------

/// Compute a fingerprint that identifies the *shape* of a plan's subtask tree.
///
/// When the plan changes its subtask structure the fingerprint changes, which
/// tells the mirror to re-create task-board entries from scratch.
/// On serialization failure (practically impossible for these simple types),
/// returns an error string that will never match any real plan fingerprint.
pub fn plan_task_board_fingerprint(plan: &TaskPlan) -> String {
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
    match serde_json::to_string(&parts) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error = %error,
                step_count = plan.subtasks.len(),
                "plan task-board fingerprint serialization failed — \
                 this should be impossible for simple string types"
            );
            format!("__fingerprint_serialization_error__:{error}")
        }
    }
}

/// Does `task` already represent the approved plan (open-work check)?
pub fn approved_plan_task_matches(
    task: &SessionTask,
    plan_id: &str,
    plan_fingerprint: &str,
) -> bool {
    if !task.status.is_open_work() {
        return false;
    }
    let metadata = task.metadata.as_ref();
    let source = metadata
        .and_then(|m| m.get("source"))
        .and_then(serde_json::Value::as_str);
    let task_plan_id = metadata
        .and_then(|m| m.get("plan_id"))
        .and_then(serde_json::Value::as_str);
    let task_fingerprint = metadata
        .and_then(|m| m.get("plan_fingerprint"))
        .and_then(serde_json::Value::as_str);

    source == Some("approved_plan")
        && task_fingerprint == Some(plan_fingerprint)
        && task_plan_id == Some(plan_id)
}

/// Like [`approved_plan_task_matches`] but also requires the specific
/// `plan_subtask_id` to match.
pub fn approved_plan_step_task_matches(
    task: &SessionTask,
    plan_id: &str,
    plan_fingerprint: &str,
    plan_subtask_id: &str,
) -> bool {
    if !approved_plan_task_matches(task, plan_id, plan_fingerprint) {
        return false;
    }
    task.metadata
        .as_ref()
        .and_then(|m| m.get("plan_subtask_id"))
        .and_then(serde_json::Value::as_str)
        == Some(plan_subtask_id)
}

/// Strict identity check — ignores `is_open_work` filter. Used for
/// post-create verification where the task may already be `in_progress`.
pub fn approved_plan_task_identity_matches(
    task: &SessionTask,
    plan_id: &str,
    plan_fingerprint: &str,
) -> bool {
    let metadata = task.metadata.as_ref();
    let source = metadata
        .and_then(|m| m.get("source"))
        .and_then(serde_json::Value::as_str);
    let task_plan_id = metadata
        .and_then(|m| m.get("plan_id"))
        .and_then(serde_json::Value::as_str);
    let task_fingerprint = metadata
        .and_then(|m| m.get("plan_fingerprint"))
        .and_then(serde_json::Value::as_str);

    source == Some("approved_plan")
        && task_fingerprint == Some(plan_fingerprint)
        && task_plan_id == Some(plan_id)
}

/// Strict identity check for a specific step within the plan.
pub fn approved_plan_step_task_identity_matches(
    task: &SessionTask,
    plan_id: &str,
    plan_fingerprint: &str,
    plan_subtask_id: &str,
) -> bool {
    if !approved_plan_task_identity_matches(task, plan_id, plan_fingerprint) {
        return false;
    }
    task.metadata
        .as_ref()
        .and_then(|m| m.get("plan_subtask_id"))
        .and_then(serde_json::Value::as_str)
        == Some(plan_subtask_id)
}

/// Map orchestrator [`TaskStatus`] to the session task-board status kind.
pub fn task_status_to_session_status(status: TaskStatus) -> SessionTaskStatusKind {
    match status {
        TaskStatus::Pending => SessionTaskStatusKind::Pending,
        TaskStatus::InProgress => SessionTaskStatusKind::InProgress,
        TaskStatus::Paused => SessionTaskStatusKind::Paused,
        TaskStatus::Completed => SessionTaskStatusKind::Completed,
        TaskStatus::Failed => SessionTaskStatusKind::Failed,
        TaskStatus::Cancelled => SessionTaskStatusKind::Cancelled,
    }
}

// ---------------------------------------------------------------------------
// Async mirror operations
// ---------------------------------------------------------------------------

/// Mirror an approved plan into the session's task board so the user sees
/// actionable step-by-step work items.
///
/// Returns `Ok(())` on success or a user-readable error.
///
/// ## Idempotency
///
/// Safe to retry after failure. Each step task carries a `plan_subtask_id`
/// in its metadata, and `ensure_plan_step_task` reuses existing tasks that
/// match the same plan/step identity.
pub async fn mirror_approved_plan_to_task_board(
    manager: &TaskManager,
    owner: &str,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    plan: &TaskPlan,
) -> Result<(), String> {
    if plan.subtasks.is_empty() {
        return Ok(());
    }
    if plan.subtasks.len() > MAX_CREATE_SUBTASKS {
        return Err(format!(
            "approved plan has {} step(s); maximum is {MAX_CREATE_SUBTASKS}. \
             Split oversized subtasks into separate plans.",
            plan.subtasks.len()
        ));
    }

    let mut snapshot = manager
        .try_snapshot_state()
        .await
        .map_err(|error| format!("snapshot task board before approved-plan mirror: {error}"))?;

    match mirror_approved_plan_to_task_board_inner(manager, owner, session_id, plan_id, goal, plan)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(seal_error) = manager.seal_snapshot_for_restore(&mut snapshot).await {
                return Err(format!(
                    "failed to mirror approved plan into task board: {error}; additionally failed to seal rollback snapshot: {seal_error}"
                ));
            }
            if let Err(restore_error) = manager.restore_snapshot(&snapshot).await {
                return Err(format!(
                    "failed to mirror approved plan into task board: {error}; additionally failed to roll back task board: {restore_error}"
                ));
            }
            Err(format!(
                "failed to mirror approved plan into task board: {error}"
            ))
        }
    }
}

/// Inner mirror implementation.
async fn mirror_approved_plan_to_task_board_inner(
    manager: &TaskManager,
    owner: &str,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    plan: &TaskPlan,
) -> Result<(), String> {
    let plan_fingerprint = plan_task_board_fingerprint(plan);

    let mut step_task_ids = HashMap::new();
    for (index, subtask) in plan.subtasks.iter().enumerate() {
        let task_id = ensure_plan_step_task(PlanStepTaskRequest {
            manager,
            owner,
            session_id,
            plan_id,
            goal,
            plan_fingerprint: &plan_fingerprint,
            step_count: plan.subtasks.len(),
            step_index: index,
            subtask,
        })
        .await?;
        step_task_ids.insert(subtask.id.clone(), task_id);
    }

    // Link dependency edges
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
        let needs_edges = manager
            .load_active_tasks()
            .await
            .map_err(|error| {
                format!("load task board before approved-plan dependency sync: {error}")
            })?
            .into_iter()
            .find(|task| task.id == *task_id)
            .is_none_or(|task| blockers.iter().any(|id| !task.blocked_by.contains(id)));
        if needs_edges {
            let output = manager
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

    // Find first runnable step and start it
    let first_runnable_task_id = plan
        .subtasks
        .iter()
        .find(|subtask| subtask.depends_on.is_empty())
        .or_else(|| plan.subtasks.first())
        .and_then(|subtask| step_task_ids.get(&subtask.id))
        .cloned()
        .ok_or_else(|| format!("approved plan '{goal}' did not produce any task-board steps"))?;

    pause_other_in_progress_tasks_for_plan_handoff(manager, &first_runnable_task_id).await?;

    let output = manager
        .update(&serde_json::json!({
            "task_id": first_runnable_task_id,
            "new_status": "in_progress",
            "metadata": {
                "source": "approved_plan",
                "plan_id": plan_id,
                "plan_goal": goal,
                "plan_fingerprint": plan_fingerprint,
                "session_id": session_id,
                "step_count": plan.subtasks.len(),
            }
        }))
        .await;
    if output.starts_with("Error:") {
        return Err(output);
    }
    Ok(())
}

/// Create or reuse a single task-board entry for a plan step.
///
/// # Concurrency safety
///
/// The check-then-create pattern is safe because the session tool executor
/// serializes all tool calls per session. If the execution model ever changes
/// to allow concurrent plan-mode tool calls, this function MUST be replaced
/// with a DB-level uniqueness constraint on `(session_id, plan_subtask_id)`.
pub struct PlanStepTaskRequest<'a> {
    pub manager: &'a TaskManager,
    pub owner: &'a str,
    pub session_id: &'a str,
    pub plan_id: &'a str,
    pub goal: &'a str,
    pub plan_fingerprint: &'a str,
    pub step_count: usize,
    pub step_index: usize,
    pub subtask: &'a SubtaskPlan,
}

pub async fn ensure_plan_step_task(request: PlanStepTaskRequest<'_>) -> Result<String, String> {
    let PlanStepTaskRequest {
        manager,
        owner,
        session_id,
        plan_id,
        goal,
        plan_fingerprint,
        step_count,
        step_index,
        subtask,
    } = request;
    // Check for existing matching task
    let existing_task_id = manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board before approved-plan mirror: {error}"))?
        .into_iter()
        .find(|task| approved_plan_step_task_matches(task, plan_id, plan_fingerprint, &subtask.id))
        .map(|task| task.id);
    if let Some(task_id) = existing_task_id {
        return Ok(task_id);
    }

    // Create new task
    let mut create_args = serde_json::json!({
        "title": subtask.title,
        "description": subtask.description.clone().unwrap_or_else(|| {
            format!("Approved plan step {} of {step_count} for: {goal}", step_index + 1)
        }),
        "active_form": format!("Executing: {}", subtask.title),
        "owner": owner,
        "metadata": {
            "source": "approved_plan",
            "plan_id": plan_id,
            "plan_goal": goal,
            "plan_fingerprint": plan_fingerprint,
            "plan_subtask_id": subtask.id,
            "plan_step_index": step_index,
            "session_id": session_id,
            "step_count": step_count,
        },
    });
    let output = manager.create(&create_args).await;
    if output.contains("already has this title") || output.contains("duplicate_of") {
        create_args["title"] = serde_json::json!(format!("{} ({})", subtask.title, subtask.id));
        let retry = manager.create(&create_args).await;
        if retry.starts_with("Error:") {
            return Err(retry);
        }
    } else if output.starts_with("Error:") {
        return Err(output);
    }

    // Post-create verification: ensure the task is visible.
    // Uses identity match (no is_open_work filter) because the task
    // may already be in_progress after creation.
    let matching: Vec<_> = manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board after approved-plan step create: {error}"))?
        .into_iter()
        .filter(|task| {
            approved_plan_step_task_identity_matches(task, plan_id, plan_fingerprint, &subtask.id)
        })
        .collect();
    if matching.len() > 1 {
        tracing::warn!(
            target: "astra_tools::plan_task_mirror",
            step_id = %subtask.id,
            goal = %goal,
            count = matching.len(),
            "Duplicate plan step tasks detected — TOCTOU race or concurrent \
             exit_plan_mode call. Using first match."
        );
    }
    matching
        .into_iter()
        .next()
        .map(|task| task.id)
        .ok_or_else(|| {
            format!(
                "approved plan step '{}' for '{goal}' was not visible in task board after \
                 task.create",
                subtask.id
            )
        })
}

/// Pause every task that is currently `in_progress` except `target_task_id`,
/// recording the reason in metadata for auditability.
pub async fn pause_other_in_progress_tasks_for_plan_handoff(
    manager: &TaskManager,
    target_task_id: &str,
) -> Result<(), String> {
    let running_task_ids: Vec<String> = manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board before approved-plan handoff: {error}"))?
        .into_iter()
        .filter(|task| task.id != target_task_id && task.status.is_in_progress())
        .map(|task| task.id)
        .collect();
    for running_task_id in running_task_ids {
        let output = manager
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_different_for_different_depends_on() {
        let plan_a = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "a".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "b".into(),
                    depends_on: vec!["s1".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut plan_b = plan_a.clone();
        plan_b.subtasks[1].depends_on.clear();

        let fp_a = plan_task_board_fingerprint(&plan_a);
        let fp_b = plan_task_board_fingerprint(&plan_b);
        assert_ne!(
            fp_a, fp_b,
            "dependency changes must produce different fingerprints"
        );
    }

    #[test]
    fn fingerprint_identical_for_same_shape() {
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "step".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            plan_task_board_fingerprint(&plan),
            plan_task_board_fingerprint(&plan)
        );
    }

    #[test]
    fn task_status_to_session_status_maps_all_variants() {
        use TaskStatus::*;
        for (ts, expected) in [
            (Pending, SessionTaskStatusKind::Pending),
            (InProgress, SessionTaskStatusKind::InProgress),
            (Paused, SessionTaskStatusKind::Paused),
            (Completed, SessionTaskStatusKind::Completed),
            (Failed, SessionTaskStatusKind::Failed),
            (Cancelled, SessionTaskStatusKind::Cancelled),
        ] {
            assert_eq!(task_status_to_session_status(ts), expected);
        }
    }
}
