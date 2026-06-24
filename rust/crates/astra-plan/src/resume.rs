//! Compact plan-resume summaries and prompt hints.

use astra_services::task_orchestrator::TaskStatus;

use crate::{repository::PlanRepository, state::PlanModeState};

const MAX_PLAN_RESUME_GOAL_CHARS: usize = 160;
const MAX_PLAN_RESUME_SUBTASK_CHARS: usize = 80;

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

/// Fetch the rendered system-prompt section for the session's active plan, if
/// one exists. Returns `None` when the session has no active plan. Swallows
/// any repo errors to `None` so that a transient DB hiccup does not block chat
/// turns — the worst-case failure mode is a missing hint on one turn.
pub async fn plan_resume_hint_for_session(
    repo: &dyn PlanRepository,
    user_id: &str,
    session_id: &str,
) -> Option<String> {
    let plan_id = repo
        .active_plan_for_session(user_id, session_id)
        .await
        .ok()
        .flatten()?;
    let state = repo.load_owned(&plan_id, user_id).await.ok()?;
    plan_resume_prompt_hint(&state)
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
}
