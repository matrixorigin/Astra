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
pub fn plan_task_board_fingerprint(plan: &TaskPlan) -> String {
    let mut hash_buf = String::with_capacity(plan.subtasks.len() * 64);
    for subtask in &plan.subtasks {
        use std::fmt::Write;
        let _ = write!(hash_buf, "{}|{}|", subtask.id, subtask.title);
        if let Some(ref desc) = subtask.description {
            let _ = write!(hash_buf, "{desc}|");
        }
        let mut deps = subtask.depends_on.clone();
        deps.sort();
        for dep in &deps {
            let _ = write!(hash_buf, "{dep},");
        }
        let _ = writeln!(hash_buf);
    }
    hash_buf
}

/// Single identity check for approved-plan tasks.
///
/// - `plan_subtask_id`: if `Some`, additionally verify the subtask id matches.
/// - `require_open_work`: if `true`, the task must be in an open-work status
///   (pending, in_progress, paused). Pass `false` for post-create verification
///   where the task may already be `in_progress`.
pub fn approved_plan_task_identity(
    task: &SessionTask,
    plan_id: &str,
    plan_fingerprint: &str,
    plan_subtask_id: Option<&str>,
    require_open_work: bool,
) -> bool {
    if require_open_work && !task.status.is_open_work() {
        return false;
    }
    let md = task.metadata.as_ref();
    let get_field = |key: &str| {
        md.and_then(|m| m.get(key))
            .and_then(serde_json::Value::as_str)
    };

    if get_field("source") != Some("approved_plan")
        || get_field("plan_fingerprint") != Some(plan_fingerprint)
        || get_field("plan_id") != Some(plan_id)
        || (plan_subtask_id.is_some_and(|id| get_field("plan_subtask_id") != Some(id)))
    {
        return false;
    }
    true
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
        .find(|task| {
            approved_plan_task_identity(task, plan_id, plan_fingerprint, Some(&subtask.id), true)
        })
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
            approved_plan_task_identity(task, plan_id, plan_fingerprint, Some(&subtask.id), false)
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

    // ── Plan markdown parser ─────────────────────────────────────────

    #[test]
    fn parse_plan_markdown_empty_and_no_numbered_steps_returns_none() {
        assert!(parse_plan_markdown_to_task_plan("").is_none());
        assert!(parse_plan_markdown_to_task_plan("no steps here\njust text\n").is_none());
    }

    #[test]
    fn parse_plan_markdown_dot_numbered() {
        let md = "1. Refactor DB\n2. Add tests\n3. Update docs\n";
        let plan = parse_plan_markdown_to_task_plan(md).unwrap();
        assert_eq!(plan.subtasks.len(), 3);
        assert_eq!(plan.subtasks[0].id, "step-1");
        assert_eq!(plan.subtasks[0].title, "Refactor DB");
        assert!(
            plan.subtasks[0].depends_on.is_empty(),
            "first step has no deps"
        );
        assert_eq!(plan.subtasks[1].id, "step-2");
        assert_eq!(plan.subtasks[1].depends_on, vec!["step-1"]);
        assert_eq!(plan.subtasks[2].id, "step-3");
        assert_eq!(plan.subtasks[2].depends_on, vec!["step-2"]);
    }

    #[test]
    fn parse_plan_markdown_multi_digit_steps() {
        // Regression: strip_prefix(is_ascii_digit) only stripped 1 digit,
        // so "10. Step" → "0. Step" → failed to match.
        let md = "9. Ninth step\n10. Tenth step\n11. Eleventh step\n";
        let plan = parse_plan_markdown_to_task_plan(md).unwrap();
        assert_eq!(plan.subtasks.len(), 3, "must parse all 3 steps");
        assert_eq!(plan.subtasks[0].title, "Ninth step");
        assert_eq!(plan.subtasks[1].title, "Tenth step");
        assert_eq!(plan.subtasks[2].title, "Eleventh step");
    }

    #[test]
    fn parse_plan_markdown_paren_numbered() {
        let md = "1) First\n2) Second\n";
        let plan = parse_plan_markdown_to_task_plan(md).unwrap();
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[0].title, "First");
    }

    #[test]
    fn parse_plan_markdown_dash_numbered() {
        let md = "1 - One\n2 - Two\n";
        let plan = parse_plan_markdown_to_task_plan(md).unwrap();
        assert_eq!(plan.subtasks.len(), 2);
    }

    #[test]
    fn parse_plan_markdown_skips_non_numbered_lines() {
        let md = "Preamble\n1. Step one\nIntermediate\n2. Step two\n";
        let plan = parse_plan_markdown_to_task_plan(md).unwrap();
        assert_eq!(plan.subtasks.len(), 2);
    }

    #[test]
    fn parse_plan_markdown_truncates_long_title() {
        let long = "x".repeat(250);
        let md = format!("1. {long}");
        let plan = parse_plan_markdown_to_task_plan(&md).unwrap();
        assert_eq!(plan.subtasks[0].title.len(), 200);
        assert!(plan.subtasks[0].title.ends_with('…'));
    }

    #[test]
    fn parse_plan_markdown_skips_empty_title_after_number() {
        let md = "1.\n2. Real step\n";
        let plan = parse_plan_markdown_to_task_plan(md).unwrap();
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.subtasks[0].title, "Real step");
        // It becomes step-1 since it's the first valid step
        assert_eq!(plan.subtasks[0].id, "step-1");
    }

    // ── Mirror idempotency (integration via in-memory TaskManager) ──

    #[tokio::test]
    async fn mirror_approved_plan_creates_tasks_and_links_dependencies() {
        use crate::task_mgmt::TaskManager;

        let manager = TaskManager::in_memory();
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "s1".into(),
                    title: "Add index".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "s2".into(),
                    title: "Migrate data".into(),
                    depends_on: vec!["s1".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        mirror_approved_plan_to_task_board(
            &manager,
            "test",
            "session-1",
            "plan-1",
            "Optimize queries",
            &plan,
        )
        .await
        .unwrap();

        let tasks = manager.load_active_tasks().await.unwrap();
        assert_eq!(tasks.len(), 2, "should create 2 tasks");
        assert!(tasks.iter().any(|t| t.title == "Add index"));
        assert!(tasks.iter().any(|t| t.title == "Migrate data"));

        // s2 should block on s1
        let s2 = tasks.iter().find(|t| t.title == "Migrate data").unwrap();
        let s1 = tasks.iter().find(|t| t.title == "Add index").unwrap();
        assert!(
            s2.blocked_by.contains(&s1.id),
            "s2 should be blocked by s1: {s2:?}"
        );
    }

    #[tokio::test]
    async fn mirror_approved_plan_idempotent_on_retry() {
        use crate::task_mgmt::TaskManager;

        let manager = TaskManager::in_memory();
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "only".into(),
                title: "Do it".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        // First mirror
        mirror_approved_plan_to_task_board(&manager, "test", "session-2", "plan-2", "Goal", &plan)
            .await
            .unwrap();
        let after_first = manager.load_active_tasks().await.unwrap().len();

        // Second mirror (retry)
        mirror_approved_plan_to_task_board(&manager, "test", "session-2", "plan-2", "Goal", &plan)
            .await
            .unwrap();
        let after_second = manager.load_active_tasks().await.unwrap().len();

        assert_eq!(
            after_first, after_second,
            "idempotent: retry should not create duplicate tasks"
        );
    }

    #[tokio::test]
    async fn mirror_approved_plan_empty_subtasks_is_noop() {
        use crate::task_mgmt::TaskManager;

        let manager = TaskManager::in_memory();
        let plan: TaskPlan = Default::default();
        mirror_approved_plan_to_task_board(&manager, "test", "session-3", "plan-3", "Goal", &plan)
            .await
            .unwrap();
        assert!(manager.load_active_tasks().await.unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Plan markdown → TaskPlan parser
// ---------------------------------------------------------------------------

/// Parse a numbered-list plan markdown into a [`TaskPlan`].
///
/// Recognizes lines like `1. Do the thing` or `1) Do the thing` as plan
/// steps. Each step becomes a `SubtaskPlan` with sequential dependencies
/// (step N depends on step N-1).
///
/// Returns `None` when no numbered steps are found.
pub fn parse_plan_markdown_to_task_plan(markdown: &str) -> Option<TaskPlan> {
    let mut subtasks: Vec<SubtaskPlan> = Vec::new();

    for line in markdown.lines() {
        let trimmed = line.trim();
        // Match patterns: "1. Title", "1) Title", "1 - Title"
        // Use trim_start_matches to handle multi-digit numbers (10, 100, etc.)
        let title = if let Some(rest) = trimmed
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .strip_prefix('.')
            .or_else(|| {
                trimmed
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .strip_prefix(')')
            })
            .or_else(|| {
                trimmed
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .strip_prefix(" -")
            }) {
            rest.trim()
        } else {
            continue;
        };

        if title.is_empty() {
            continue;
        }

        // Truncate title to a reasonable length
        let title = if title.len() > 200 {
            format!("{}…", &title[..197])
        } else {
            title.to_string()
        };

        let depends_on: Vec<String> = if subtasks.is_empty() {
            Vec::new()
        } else {
            vec![subtasks.last().unwrap().id.clone()]
        };

        subtasks.push(SubtaskPlan {
            id: format!("step-{}", subtasks.len() + 1),
            title,
            description: Some(String::new()),
            depends_on,
            ..Default::default()
        });
    }

    if subtasks.is_empty() {
        return None;
    }

    Some(TaskPlan {
        subtasks,
        ..Default::default()
    })
}
