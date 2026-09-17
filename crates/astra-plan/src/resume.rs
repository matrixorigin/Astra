//! Compact plan-resume summaries and prompt hints.

use crate::{repository::PlanRepository, state::PlanModeState};

const MAX_PLAN_RESUME_GOAL_CHARS: usize = 160;
const MAX_PLAN_RESUME_DRAFT_CHARS: usize = 320;

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
    let draft = state
        .plan_md
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if goal.is_empty() && draft.is_none() {
        return None;
    }

    let mut out = String::from("[plan-resume]");
    if !goal.is_empty() {
        out.push_str(&format!(
            " goal=\"{}\"",
            truncate_plan_resume_text(goal, MAX_PLAN_RESUME_GOAL_CHARS)
        ));
    }
    if let Some(draft) = draft {
        out.push_str(&format!(
            " · draft=\"{}\"",
            truncate_plan_resume_text(draft, MAX_PLAN_RESUME_DRAFT_CHARS)
        ));
    }
    Some(out)
}

pub fn plan_resume_prompt_hint(state: &PlanModeState) -> Option<String> {
    let digest = plan_resume_digest(state)?;
    let guidance = "A plan draft is awaiting trusted user review for this session. \
        Stay in read-only planning mode, refine the proposal, and call \
        `exit_plan_mode(plan='...')` to submit the current draft. Do not treat \
        task or Work execution progress as approval.";
    Some(format!("\n\n## Active Plan\n{digest}\n\n{guidance}"))
}

/// A loaded plan reached through the session's `active_plan_id` is authoring.
///
/// Approval is an explicit control-plane transition that clears the active
/// binding. Inferring approval from embedded task/subtask progress creates a
/// second authority and can silently release the write guard.
pub fn plan_mode_authoring_active(_state: &PlanModeState) -> bool {
    true
}

/// Build a plan-resume snapshot for the session's active plan, if any.
///
/// Repository failures degrade to `PlanResumeSnapshot::default()` (no hint,
/// no authoring flag) so a transient DB hiccup can't block a chat turn — the
/// worst-case failure mode is a missing hint on one turn. Failures are logged
/// at `warn!` level so they remain diagnosable without paging on a degraded
/// optional hint path.
pub async fn plan_resume_snapshot_for_session(
    repo: &dyn PlanRepository,
    user_id: &str,
    session_id: &str,
) -> PlanResumeSnapshot {
    let plan_id = match repo.active_plan_for_session(user_id, session_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return PlanResumeSnapshot::default(),
        Err(err) => {
            tracing::warn!(
                %session_id,
                error = %err,
                "plan resume: failed to query active plan; skipping hint"
            );
            return PlanResumeSnapshot::default();
        }
    };
    match repo.load(user_id, &plan_id).await {
        Ok(state) => PlanResumeSnapshot {
            authoring_active: plan_mode_authoring_active(&state),
            prompt_hint: plan_resume_prompt_hint(&state),
        },
        Err(err) => {
            tracing::warn!(
                %session_id,
                %plan_id,
                error = %err,
                "plan resume: active plan exists but draft load failed; retaining write guard without hint"
            );
            PlanResumeSnapshot {
                authoring_active: true,
                prompt_hint: None,
            }
        }
    }
}

/// Fetch the rendered system-prompt section for the session's active plan, if
/// one exists. Returns `None` when the session has no active plan or the
/// repository lookup fails (the failure is logged by
/// [`plan_resume_snapshot_for_session`]).
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
    use crate::{SubtaskPlan, TaskStatus};

    use super::*;

    #[test]
    fn plan_resume_prompt_hint_returns_none_for_empty_state() {
        assert!(plan_resume_prompt_hint(&PlanModeState::new(String::new())).is_none());
    }

    #[test]
    fn plan_resume_prompt_hint_formats_goal_and_draft_without_task_progress() {
        let mut state = PlanModeState::new("Ship auth overhaul".into());
        state.plan_md = Some("1. Inspect schema\n2. Refactor middleware".into());

        let hint = plan_resume_prompt_hint(&state).expect("hint");
        assert!(hint.contains("## Active Plan"), "{hint}");
        assert!(hint.contains("goal=\"Ship auth overhaul\""), "{hint}");
        assert!(hint.contains("draft=\"1. Inspect schema"), "{hint}");
        assert!(!hint.contains("in_progress"), "{hint}");
    }

    #[test]
    fn plan_resume_prompt_hint_handles_zero_subtasks_without_execution_language() {
        let state = PlanModeState::new("Design slash commands".into());
        let hint = plan_resume_prompt_hint(&state).expect("hint");
        assert!(hint.contains("goal=\"Design slash commands\""), "{hint}");
        assert!(hint.contains("awaiting trusted user review"), "{hint}");
    }

    #[test]
    fn active_plan_binding_remains_authoring_regardless_of_task_progress() {
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
        assert!(plan_mode_authoring_active(&executing_plan));
        assert!(plan_resume_prompt_hint(&executing_plan).is_some());

        let mut completed_plan = pending_draft;
        completed_plan.plan.subtasks[0].status = TaskStatus::Completed;
        assert!(plan_mode_authoring_active(&completed_plan));
        assert!(plan_resume_prompt_hint(&completed_plan).is_some());
    }
}
