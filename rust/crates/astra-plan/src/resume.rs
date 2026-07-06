//! Compact plan-resume summaries and prompt hints.

use astra_services::task_orchestrator::TaskStatus;

use crate::{repository::PlanRepository, state::PlanModeState};

const MAX_PLAN_RESUME_GOAL_CHARS: usize = 160;
const MAX_PLAN_RESUME_SUBTASK_CHARS: usize = 80;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanResumeSnapshot {
    pub authoring_active: bool,
    pub prompt_hint: Option<String>,
}

fn truncate_plan_resume_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub fn plan_resume_digest(state: &PlanModeState) -> Option<String> {
    let goal = state.goal.trim();
    let subtasks = &state.plan.subtasks;
    if goal.is_empty() && subtasks.is_empty() {
        return None;
    }

    let total = subtasks.len();
    let done = subtasks
        .iter()
        .filter(|subtask| subtask.status == TaskStatus::Completed)
        .count();
    let open = subtasks
        .iter()
        .filter(|subtask| !subtask.status.is_terminal() && subtask.status != TaskStatus::InProgress)
        .count();
    let in_progress_title = subtasks
        .iter()
        .find(|subtask| subtask.status == TaskStatus::InProgress)
        .map(|subtask| truncate_plan_resume_text(&subtask.title, MAX_PLAN_RESUME_SUBTASK_CHARS));

    let mut out = String::from("[plan-resume]");
    if !goal.is_empty() {
        out.push_str(&format!(
            " goal=\"{}\"",
            truncate_plan_resume_text(goal, MAX_PLAN_RESUME_GOAL_CHARS)
        ));
    }
    if let Some(title) = in_progress_title {
        out.push_str(&format!(" · in_progress=\"{title}\""));
    }
    if total > 0 {
        out.push_str(&format!(" · open={open} · done={done}/{total}"));
    }
    Some(out)
}

pub fn plan_resume_prompt_hint(state: &PlanModeState) -> Option<String> {
    let digest = plan_resume_digest(state)?;
    let guidance = if state.plan.subtasks.is_empty() {
        "A plan draft is currently attached to this session, but it has no executable \
         subtasks yet. Stay in planning/decomposition mode until the work is broken \
         down, and only call `exit_plan_mode` once the draft is either approved or \
         intentionally abandoned."
    } else {
        "A plan is currently in-flight for this session. Treat the next turn as a \
         continuation — resume from the in-progress subtask, respect the approved \
         plan structure, and call `exit_plan_mode` only if the plan needs to be \
         abandoned before completion."
    };
    Some(format!("\n\n## Active Plan\n{digest}\n\n{guidance}"))
}

pub fn plan_mode_authoring_active(state: &PlanModeState) -> bool {
    let has_subtasks = !state.plan.subtasks.is_empty();
    let any_in_progress = state
        .plan
        .subtasks
        .iter()
        .any(|subtask| subtask.status == TaskStatus::InProgress);
    let items_done = state.plan.items_done() > 0;
    let progress_complete = state.plan.progress_pct() == 100;
    !has_subtasks || (!any_in_progress && !items_done && !progress_complete)
}

pub async fn plan_resume_snapshot_for_session(
    repo: &dyn PlanRepository,
    user_id: &str,
    session_id: &str,
) -> PlanResumeSnapshot {
    let Some(plan_id) = repo
        .active_plan_for_session(user_id, session_id)
        .await
        .ok()
        .flatten()
    else {
        return PlanResumeSnapshot::default();
    };
    let Ok(state) = repo.load(user_id, &plan_id).await else {
        return PlanResumeSnapshot::default();
    };
    PlanResumeSnapshot {
        authoring_active: plan_mode_authoring_active(&state),
        prompt_hint: plan_resume_prompt_hint(&state),
    }
}

/// Fetch the rendered system-prompt section for the session's active plan, if
/// one exists. Returns `None` when the session has no active plan. Swallows
/// any repo errors to `None` so that a transient DB hiccup does not block chat
/// turns — the worst-case failure mode is a missing hint on one turn.
pub async fn plan_resume_hint_for_session(
    repo: &dyn PlanRepository,
    user_id: &str,
    session_id: &str,
) -> Option<String> {
    plan_resume_snapshot_for_session(repo, user_id, session_id)
        .await
        .prompt_hint
}

#[cfg(test)]
mod tests {
    use astra_services::task_orchestrator::{SubtaskPlan, TaskStatus};

    use super::*;

    #[test]
    fn plan_resume_prompt_hint_returns_none_for_empty_state() {
        assert!(plan_resume_prompt_hint(&PlanModeState::new(String::new())).is_none());
    }

    #[test]
    fn plan_resume_prompt_hint_formats_goal_and_active_subtask() {
        let mut state = PlanModeState::new("Ship auth overhaul".into());
        state.plan.subtasks = vec![
            SubtaskPlan {
                id: "a".into(),
                title: "schema".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            },
            SubtaskPlan {
                id: "b".into(),
                title: "middleware refactor".into(),
                status: TaskStatus::InProgress,
                ..Default::default()
            },
            SubtaskPlan {
                id: "c".into(),
                title: "tests".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
        ];

        let hint = plan_resume_prompt_hint(&state).expect("hint");
        assert!(hint.contains("## Active Plan"), "{hint}");
        assert!(hint.contains("goal=\"Ship auth overhaul\""), "{hint}");
        assert!(
            hint.contains("in_progress=\"middleware refactor\""),
            "{hint}"
        );
    }

    #[test]
    fn plan_resume_prompt_hint_handles_zero_subtasks_without_execution_language() {
        let state = PlanModeState::new("Design slash commands".into());
        let hint = plan_resume_prompt_hint(&state).expect("hint");
        assert!(hint.contains("goal=\"Design slash commands\""), "{hint}");
        assert!(hint.contains("no executable subtasks yet"), "{hint}");
        assert!(
            !hint.contains("resume from the in-progress subtask"),
            "{hint}"
        );
    }

    #[test]
    fn plan_mode_authoring_active_tracks_authoring_not_prompt_presence() {
        let empty_draft = PlanModeState::new("Draft provider model".into());
        assert!(plan_mode_authoring_active(&empty_draft));
        assert!(plan_resume_prompt_hint(&empty_draft).is_some());

        let mut pending_draft = PlanModeState::new("Draft provider model".into());
        pending_draft.plan.subtasks = vec![SubtaskPlan {
            id: "design".into(),
            title: "Design provider routing".into(),
            status: TaskStatus::Pending,
            ..Default::default()
        }];
        assert!(plan_mode_authoring_active(&pending_draft));

        let mut executing_plan = pending_draft.clone();
        executing_plan.plan.subtasks[0].status = TaskStatus::InProgress;
        assert!(!plan_mode_authoring_active(&executing_plan));
        assert!(plan_resume_prompt_hint(&executing_plan).is_some());

        let mut completed_plan = pending_draft;
        completed_plan.plan.subtasks[0].status = TaskStatus::Completed;
        assert!(!plan_mode_authoring_active(&completed_plan));
        assert!(plan_resume_prompt_hint(&completed_plan).is_some());
    }
}
