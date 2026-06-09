//! Shared plan → task-board mirror logic.
//!
//! Extracted to avoid ~260 lines of duplicate code between [`plan_handlers`]
//! and [`server_tool_executor`]. Both callers provide a [`TaskManager`] and
//! relevant metadata; the pure functions and async operations below are the
//! single source of truth.

use std::collections::HashMap;

use astra_plan::PlanModeState;
use astra_services::task_orchestrator::{TaskPlan, TaskStatus};
use astra_tools::task_mgmt::{
    MAX_CREATE_SUBTASKS, SessionTask, SessionTaskStatusKind, TaskManager,
};

// ---------------------------------------------------------------------------
// Pure utility functions
// ---------------------------------------------------------------------------

/// Compute a fingerprint that identifies the *shape* of a plan's subtask tree.
///
/// When the plan changes its subtask structure the fingerprint changes, which
/// tells the mirror to re-create task-board entries from scratch.
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
                "plan task-board fingerprint serialization failed"
            );
            format!("__fingerprint_serialization_error__:{error}")
        }
    }
}

/// Does `task` already represent the approved plan identified by `plan_id`
/// / `goal` / `plan_fingerprint`?
pub fn approved_plan_task_matches(
    task: &SessionTask,
    plan_id: &str,
    _goal: &str,
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
    goal: &str,
    plan_fingerprint: &str,
    plan_subtask_id: &str,
) -> bool {
    if !approved_plan_task_matches(task, plan_id, goal, plan_fingerprint) {
        return false;
    }
    task.metadata
        .as_ref()
        .and_then(|m| m.get("plan_subtask_id"))
        .and_then(serde_json::Value::as_str)
        == Some(plan_subtask_id)
}

/// Strict identity check (ignores `is_open_work` filter) — used for
/// post-create verification where the task may already be in `in_progress`.
pub fn approved_plan_task_identity_matches(
    task: &SessionTask,
    plan_id: &str,
    _goal: &str,
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
    goal: &str,
    plan_fingerprint: &str,
    plan_subtask_id: &str,
) -> bool {
    if !approved_plan_task_identity_matches(task, plan_id, goal, plan_fingerprint) {
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
// Async mirror operations (takes `&TaskManager` directly — callers pass the
// manager and metadata they already own.)
// ---------------------------------------------------------------------------

/// Mirror an approved plan into the task board: create / reuse step tasks,
/// wire up dependency edges, pause the previous in-progress task, and start
/// the first runnable step.
///
/// Returns `Ok(())` on success or a user-readable error. On failure the task
/// board is rolled back via `TaskManager::try_snapshot_state` / `restore_snapshot`.
/// Mirror an approved plan into the session's task board so the user
/// sees actionable step-by-step work items.
///
/// ## Idempotency
///
/// This function is safe to retry after failure. Each step task carries a
/// `plan_subtask_id` in its metadata, and `ensure_approved_plan_step_task`
/// reuses existing tasks that match the same plan/step identity.  Edges
/// (`blocked_by`) are set only when missing.  No snapshot or rollback is
/// needed — partial progress from a failed mirror is valid state that the
/// next call will complete.
///
/// ## Concurrent safety
///
/// The session tool executor serializes all tool calls per session, so only
/// one caller can enter this function at a time. If that ever changes, the
/// identity check inside `ensure_approved_plan_step_task` must be made
/// atomic at the database level.
pub async fn mirror_approved_plan_to_task_board(
    manager: &TaskManager,
    owner: &str,
    session_id: &str,
    plan_id: &str,
    plan_state: &PlanModeState,
) -> Result<(), String> {
    if plan_state.plan.subtasks.is_empty() {
        return Ok(());
    }
    if plan_state.plan.subtasks.len() > MAX_CREATE_SUBTASKS {
        return Err(format!(
            "approved plan has {} step(s); maximum is {MAX_CREATE_SUBTASKS}. \
             Split oversized subtasks into separate plans.",
            plan_state.plan.subtasks.len()
        ));
    }

    mirror_approved_plan_to_task_board_inner(manager, owner, session_id, plan_id, plan_state)
        .await
        .map_err(|error| {
            // Mirror failures are surfaced as-is. The caller can retry
            // because partial progress (tasks already created, edges
            // already linked) will be picked up by the idempotent
            // identity checks inside ensure_approved_plan_step_task.
            format!("failed to mirror approved plan into task board: {error}")
        })
}

/// Inner mirror implementation — callers go through
/// [`mirror_approved_plan_to_task_board`] which adds snapshot/rollback.
pub async fn mirror_approved_plan_to_task_board_inner(
    manager: &TaskManager,
    owner: &str,
    session_id: &str,
    plan_id: &str,
    plan_state: &PlanModeState,
) -> Result<(), String> {
    let plan_fingerprint = plan_task_board_fingerprint(&plan_state.plan);

    let mut step_task_ids = HashMap::new();
    for (index, subtask) in plan_state.plan.subtasks.iter().enumerate() {
        let task_id = ensure_approved_plan_step_task(
            manager,
            owner,
            session_id,
            plan_id,
            &plan_state.goal,
            &plan_fingerprint,
            plan_state.plan.subtasks.len(),
            index,
            subtask,
        )
        .await?;
        step_task_ids.insert(subtask.id.clone(), task_id);
    }

    for subtask in &plan_state.plan.subtasks {
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

    let first_runnable_task_id = plan_state
        .plan
        .subtasks
        .iter()
        .find(|subtask| subtask.depends_on.is_empty())
        .or_else(|| plan_state.plan.subtasks.first())
        .and_then(|subtask| step_task_ids.get(&subtask.id))
        .cloned()
        .ok_or_else(|| {
            format!(
                "approved plan '{}' did not produce any task-board steps",
                plan_state.goal
            )
        })?;

    pause_other_in_progress_tasks_for_plan_handoff(manager, &first_runnable_task_id).await?;

    let output = manager
        .update(&serde_json::json!({
            "task_id": first_runnable_task_id,
            "new_status": "in_progress",
            "metadata": {
                "source": "approved_plan",
                "plan_id": plan_id,
                "plan_goal": plan_state.goal,
                "plan_fingerprint": plan_fingerprint,
                "session_id": session_id,
                "step_count": plan_state.plan.subtasks.len(),
            }
        }))
        .await;
    if output.starts_with("Error:") {
        return Err(output);
    }
    Ok(())
}

/// Create or re-use a single task-board entry for a plan step.
#[allow(clippy::too_many_arguments)]
/// Create or return an existing task for an approved plan step.
///
/// # Concurrency safety
///
/// The check-then-create pattern is safe because the session tool executor
/// serializes all tool calls (including `exit_plan_mode`) per session. No
/// two goroutines can call this function for the same session concurrently.
/// If the execution model ever changes to allow concurrent plan-mode tool
/// calls, this function MUST be replaced with a DB-level uniqueness
/// constraint on `(session_id, plan_subtask_id)` to prevent TOCTOU races.
pub async fn ensure_approved_plan_step_task(
    manager: &TaskManager,
    owner: &str,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    plan_fingerprint: &str,
    step_count: usize,
    step_index: usize,
    subtask: &astra_services::task_orchestrator::SubtaskPlan,
) -> Result<String, String> {
    let existing_task_id = manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board before approved-plan mirror: {error}"))?
        .into_iter()
        .find(|task| {
            approved_plan_step_task_matches(task, plan_id, goal, plan_fingerprint, &subtask.id)
        })
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

    let matching: Vec<_> = manager
        .load_active_tasks()
        .await
        .map_err(|error| format!("load task board after approved-plan step create: {error}"))?
        .into_iter()
        .filter(|task| {
            approved_plan_step_task_identity_matches(
                task,
                plan_id,
                goal,
                plan_fingerprint,
                &subtask.id,
            )
        })
        .collect();
    if matching.len() > 1 {
        tracing::warn!(
            target: "astra_runtime::plan_task_mirror",
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

/// Pause every task that is currently `in_progress` except `target_task_id`.
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
