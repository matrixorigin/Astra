use std::sync::Arc;

use serde_json::Value;

/// Snapshot used by the plan-mode write guard and the system-prompt
/// injector. Populated on first access per plan-mode state change; cleared
/// by the enter/exit tools so the next call sees fresh DB state.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlanModeSnapshot {
    /// Whether the session currently has an active plan still in authoring.
    pub(crate) authoring_active: Option<bool>,
    /// Rendered system-prompt section to inject on the next turn (`None`
    /// when there's no active plan or it's already executing).
    pub(crate) resume_hint: Option<String>,
}

/// Tools that mutate the world outside the session. Blocked while plan mode
/// is active (`PlanPhase` = Planning|Refining) to mirror Claude Code's
/// `prepareContextForPlanMode` behaviour: the model must call ExitPlanMode
/// before writing anything.
///
/// Read-only tools (grep, glob, read_file, git action=status/diff/log,
/// web_search) and session-scoped authoring tools (`task`, memory_retrieve,
/// ...) stay available so the agent can continue exploring while authoring a
/// plan.
pub(crate) fn is_plan_mode_blocked_tool(tool: &str, args: &Value) -> bool {
    if tool == "task_stop" {
        return true;
    }

    if tool == "task" {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        return action == "stop";
    }

    if tool == "git" {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        return match action {
            "commit" | "revert_commit" | "push" => true,
            "stash" => args
                .get("stash_action")
                .and_then(Value::as_str)
                .is_some_and(|stash_action| {
                    matches!(
                        stash_action,
                        "push" | "save" | "apply" | "pop" | "drop" | "branch"
                    )
                }),
            _ => false,
        };
    }

    if tool == "github" {
        return args
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| action == "create_issue");
    }

    matches!(
        tool,
        "bash" | "write_file" | "str_replace" | "mo" | "rollback_database_snapshots"
    )
}

pub(crate) fn plan_mode_blocked_tool_result(tool: &str) -> astra_tools::ToolResult {
    astra_tools::ToolResult::error(format!(
        "Tool '{tool}' is blocked while plan mode is active. \
         The agent must call `exit_plan_mode` with an approved plan \
         before any write operation. This mirrors Claude Code's plan \
         mode: the plan is authored with read-only tools, approved by \
         the user, then execution proceeds with writes unlocked."
    ))
}

pub(crate) async fn plan_mode_authoring_active(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    session_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
) -> bool {
    if let Some(cached) = cache.read().await.authoring_active {
        return cached;
    }
    let (authoring, hint) = recompute_plan_mode_snapshot(repo, session_id).await;
    let mut writer = cache.write().await;
    writer.authoring_active = Some(authoring);
    writer.resume_hint = hint;
    authoring
}

pub(crate) async fn recompute_plan_mode_snapshot(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    session_id: &str,
) -> (bool, Option<String>) {
    let Some(repo) = repo else {
        return (false, None);
    };
    let Ok(Some(plan_id)) = repo.active_plan_for_session(session_id).await else {
        return (false, None);
    };
    match repo.load(&plan_id).await {
        Ok(state) => {
            let has_subtasks = !state.plan.subtasks.is_empty();
            let any_in_progress = state.plan.subtasks.iter().any(|subtask| {
                subtask.status == astra_services::task_orchestrator::TaskStatus::InProgress
            });
            let items_done = state.plan.items_done() > 0;
            let progress_complete = state.plan.progress_pct() == 100;
            let authoring =
                !has_subtasks || (!any_in_progress && !items_done && !progress_complete);
            let hint = astra_plan::plan_resume_prompt_hint(&state);
            (authoring, hint)
        }
        Err(_) => (false, None),
    }
}

pub(crate) async fn invalidate_plan_mode_cache(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    session_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    resume_hint_handle: Option<&Arc<std::sync::RwLock<Option<String>>>>,
) {
    {
        let mut writer = cache.write().await;
        *writer = PlanModeSnapshot::default();
    }
    if let Some(handle) = resume_hint_handle {
        let (authoring, hint) = recompute_plan_mode_snapshot(repo, session_id).await;
        if let Ok(mut slot) = handle.write() {
            *slot = hint.clone();
        }
        let mut writer = cache.write().await;
        writer.authoring_active = Some(authoring);
        writer.resume_hint = hint;
    }
}

pub(crate) async fn execute_enter_plan_mode(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    session_id: &str,
    user_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    resume_hint_handle: Option<&Arc<std::sync::RwLock<Option<String>>>>,
    args: &Value,
) -> String {
    let Some(repo) = repo.cloned() else {
        return "Error: plan repository not configured on this executor".to_string();
    };
    let goal = args
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or("(pending)")
        .trim()
        .to_string();
    let goal = if goal.is_empty() {
        "(pending)".to_string()
    } else {
        goal
    };

    let plan_id = args
        .get("plan_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| astra_plan::PlanModeState::generate_plan_id(&goal));

    const MAX_CAS_RETRIES: u32 = 3;
    let mut last_conflict: Option<String> = None;
    for _attempt in 0..MAX_CAS_RETRIES {
        let (mut state, expected_version) = match repo.load(&plan_id).await {
            Ok(mut state) => {
                let version = state.version;
                state.session_hint = Some(session_id.to_string());
                (state, Some(version))
            }
            Err(astra_plan::PlanLoadError::NotFound(_)) => {
                let mut state =
                    astra_plan::PlanModeState::new_with_owner(goal.clone(), user_id.to_string());
                state.session_hint = Some(session_id.to_string());
                (state, None)
            }
            Err(error) => return format!("Error: load plan: {error}"),
        };

        match repo.save(&plan_id, &mut state, expected_version).await {
            Ok(()) => {
                last_conflict = None;
                break;
            }
            Err(astra_plan::PlanLoadError::Conflict { expected, actual }) => {
                last_conflict = Some(format!(
                    "version conflict (expected {expected}, stored {actual})"
                ));
                continue;
            }
            Err(error) => return format!("Error: save plan: {error}"),
        }
    }
    if let Some(conflict) = last_conflict {
        return format!("Error: save plan after {MAX_CAS_RETRIES} retries: {conflict}");
    }

    if let Err(error) = repo.set_active_plan(session_id, Some(&plan_id)).await {
        return format!("Error: link plan to session: {error}");
    }

    invalidate_plan_mode_cache(Some(&repo), session_id, cache, resume_hint_handle).await;

    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) {
        let _ = writer.append(
            &astra_services::session_journal::JournalEvent::plan_lifecycle(
                Some(session_id),
                "plan_mode_entered",
                Some(serde_json::json!({
                    "plan_id": plan_id,
                    "goal": goal,
                })),
            ),
        );
    }

    format!(
        "Entered plan mode. plan_id={} goal=\"{}\". Write tools are now blocked — \
         author the plan, then call `exit_plan_mode` with `approved=true` when ready.",
        plan_id, goal
    )
}

pub(crate) async fn execute_exit_plan_mode(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    session_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    resume_hint_handle: Option<&Arc<std::sync::RwLock<Option<String>>>>,
    args: &Value,
) -> String {
    let Some(repo) = repo.cloned() else {
        return "Error: plan repository not configured on this executor".to_string();
    };

    let active = match repo.active_plan_for_session(session_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            return "Note: session has no active plan; nothing to exit.".to_string();
        }
        Err(error) => return format!("Error: lookup active plan: {error}"),
    };

    if let Some(plan_md) = args
        .get("plan")
        .and_then(Value::as_str)
        .or_else(|| args.get("plan_md").and_then(Value::as_str))
    {
        const MAX_CAS_RETRIES: u32 = 3;
        let mut last_conflict: Option<String> = None;
        for _attempt in 0..MAX_CAS_RETRIES {
            let mut state = match repo.load(&active).await {
                Ok(state) => state,
                Err(error) => return format!("Error: load active plan: {error}"),
            };
            state.plan_md = Some(plan_md.to_string());
            let expected = Some(state.version);
            match repo.save(&active, &mut state, expected).await {
                Ok(()) => {
                    last_conflict = None;
                    break;
                }
                Err(astra_plan::PlanLoadError::Conflict { expected, actual }) => {
                    last_conflict = Some(format!(
                        "version conflict (expected {expected}, stored {actual})"
                    ));
                    continue;
                }
                Err(error) => return format!("Error: save submitted plan markdown: {error}"),
            }
        }
        if let Some(conflict) = last_conflict {
            return format!(
                "Error: save submitted plan markdown after {MAX_CAS_RETRIES} retries: {conflict}"
            );
        }
    }

    invalidate_plan_mode_cache(Some(&repo), session_id, cache, resume_hint_handle).await;

    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) {
        let _ = writer.append(
            &astra_services::session_journal::JournalEvent::plan_lifecycle(
                Some(session_id),
                "plan_submitted_for_approval",
                Some(serde_json::json!({ "plan_id": active })),
            ),
        );
    }

    format!(
        "Plan {active} submitted for trusted user approval. Write tools remain blocked until the UI/control plane records the user's approval."
    )
}
