//! P3.3 — plan-mode tighter re-entry helpers.
//!
//! Provides pure functions used by the CLI at startup / on every user turn:
//!
//! * [`plan_resume_digest`] — format a compact one-line digest from a
//!   `PlanModeState` so the next turn's system prompt can remind the model
//!   where the plan left off (goal + open/in-progress subtasks).
//! * [`message_signals_resume`] — detect whether a user line signals intent
//!   to continue a paused plan (e.g. "继续", "continue", "resume",
//!   `@resume-plan`).
//!
//! Both functions are side-effect free and cheap to call per-turn.

use crate::decompose::PlanModeState;
use astra_services::task_orchestrator::TaskStatus;

/// Maximum characters of the plan goal to surface in the digest.
const MAX_GOAL_CHARS: usize = 160;
/// Maximum characters of a subtask title to surface in the digest.
const MAX_SUBTASK_CHARS: usize = 80;

/// Build a one-line plan-resume digest for system-prompt injection.
///
/// Returns `None` when there is nothing worth surfacing (no goal **and**
/// no subtasks). Successful output looks like:
///
/// ```text
/// [plan-resume] goal="Fix auth bug" · in_progress="Add unit test" · open=3 · done=1/5
/// ```
pub fn plan_resume_digest(state: &PlanModeState) -> Option<String> {
    let goal = state.goal.trim();
    let subtasks = &state.plan.subtasks;
    if goal.is_empty() && subtasks.is_empty() {
        return None;
    }

    let total = subtasks.len();
    let done = subtasks
        .iter()
        .filter(|st| st.status == TaskStatus::Completed)
        .count();
    let open = subtasks
        .iter()
        .filter(|st| !st.status.is_terminal() && st.status != TaskStatus::InProgress)
        .count();
    let in_progress_title = subtasks
        .iter()
        .find(|st| st.status == TaskStatus::InProgress)
        .map(|st| truncate(&st.title, MAX_SUBTASK_CHARS));

    let mut out = String::from("[plan-resume]");
    if !goal.is_empty() {
        out.push_str(&format!(" goal=\"{}\"", truncate(goal, MAX_GOAL_CHARS)));
    }
    if let Some(title) = in_progress_title {
        out.push_str(&format!(" · in_progress=\"{title}\""));
    }
    if total > 0 {
        out.push_str(&format!(" · open={open} · done={done}/{total}"));
    }
    Some(out)
}

/// Build a system-prompt section that reminds the LLM a plan is still open.
///
/// Returns `None` when the plan has nothing worth surfacing (no goal **and**
/// no subtasks — same guard as [`plan_resume_digest`]).
///
/// The output is multi-line markdown intended to be appended to the dynamic
/// portion of the system prompt on every turn while a session has an active
/// plan. It includes the one-line digest plus an instruction paragraph that
/// steers the model back toward plan execution semantics.
///
/// Example:
///
/// ```text
/// ## Active Plan
/// [plan-resume] goal="Fix auth bug" · in_progress="Add unit test" · open=3 · done=1/5
///
/// A plan is currently in-flight for this session. Treat the next turn as a
/// continuation — resume from the in-progress subtask, respect the approved
/// plan structure, and call `exit_plan_mode` only if the plan needs to be
/// abandoned before completion.
/// ```
pub fn plan_resume_system_prompt_section(state: &PlanModeState) -> Option<String> {
    let digest = plan_resume_digest(state)?;
    Some(format!(
        "\n\n## Active Plan\n{digest}\n\n\
         A plan is currently in-flight for this session. Treat the next turn as a \
         continuation — resume from the in-progress subtask, respect the approved \
         plan structure, and call `exit_plan_mode` only if the plan needs to be \
         abandoned before completion."
    ))
}

/// Returns `true` when the user line signals intent to resume a paused plan.
///
/// Matches are intentionally narrow to avoid false positives on unrelated
/// "continue" mentions mid-sentence. The rules are:
///
/// * Explicit tag `@resume-plan` anywhere in the message.
/// * Trimmed, short (≤ 24 chars) messages whose lower-cased form is one of
///   the canonical resume phrases in Chinese / English.
pub fn message_signals_resume(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("@resume-plan") {
        return true;
    }
    if trimmed.chars().count() > 24 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "继续"
            | "继续!"
            | "继续。"
            | "继续计划"
            | "继续 plan"
            | "继续plan"
            | "恢复"
            | "恢复计划"
            | "resume"
            | "resume."
            | "resume plan"
            | "resume-plan"
            | "continue plan"
            | "continue-plan"
    )
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::PlanModeState;
    use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

    fn make_state(goal: &str, subtasks: Vec<SubtaskPlan>) -> PlanModeState {
        let mut st = PlanModeState::new(goal.to_string(), Default::default());
        st.plan = TaskPlan {
            subtasks,
            notes: None,
        };
        st
    }

    fn subtask(id: &str, title: &str, status: TaskStatus) -> SubtaskPlan {
        SubtaskPlan {
            id: id.into(),
            title: title.into(),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn digest_returns_none_for_empty_state() {
        let st = make_state("", vec![]);
        assert!(plan_resume_digest(&st).is_none());
    }

    #[test]
    fn digest_includes_goal_counts_and_in_progress_title() {
        let st = make_state(
            "Fix login bug",
            vec![
                subtask("t1", "Write failing test", TaskStatus::Completed),
                subtask("t2", "Patch handler", TaskStatus::InProgress),
                subtask("t3", "Update docs", TaskStatus::Pending),
                subtask("t4", "Ship release note", TaskStatus::Pending),
            ],
        );
        let digest = plan_resume_digest(&st).expect("digest");
        assert!(digest.contains("goal=\"Fix login bug\""), "{digest}");
        assert!(digest.contains("in_progress=\"Patch handler\""), "{digest}");
        assert!(digest.contains("open=2"), "{digest}");
        assert!(digest.contains("done=1/4"), "{digest}");
    }

    #[test]
    fn digest_truncates_overlong_goal() {
        let long_goal = "g".repeat(400);
        let st = make_state(&long_goal, vec![]);
        let digest = plan_resume_digest(&st).expect("digest");
        assert!(digest.ends_with("…\""), "{digest}");
        assert!(digest.chars().count() < long_goal.len() + 40);
    }

    #[test]
    fn digest_omits_in_progress_when_none() {
        let st = make_state("Task", vec![subtask("t1", "Draft", TaskStatus::Pending)]);
        let digest = plan_resume_digest(&st).expect("digest");
        assert!(!digest.contains("in_progress="), "{digest}");
        assert!(digest.contains("open=1"), "{digest}");
    }

    #[test]
    fn resume_signal_matches_canonical_phrases() {
        for phrase in ["继续", "resume", "Resume.", "继续计划", "CONTINUE PLAN"] {
            assert!(
                message_signals_resume(phrase),
                "expected resume signal for {phrase:?}"
            );
        }
    }

    #[test]
    fn resume_signal_matches_explicit_tag() {
        assert!(message_signals_resume("please @resume-plan now"));
    }

    #[test]
    fn resume_system_prompt_section_returns_none_for_empty_state() {
        let st = make_state("", vec![]);
        assert!(plan_resume_system_prompt_section(&st).is_none());
    }

    #[test]
    fn resume_system_prompt_section_contains_digest_and_continuation_cue() {
        let st = make_state(
            "Ship auth overhaul",
            vec![
                subtask("t1", "schema migration", TaskStatus::Completed),
                subtask("t2", "middleware refactor", TaskStatus::InProgress),
                subtask("t3", "tests", TaskStatus::Pending),
            ],
        );
        let section = plan_resume_system_prompt_section(&st).expect("section");
        // Includes the digest line and the continuation instruction.
        assert!(
            section.contains("[plan-resume]"),
            "section missing digest: {section}"
        );
        assert!(
            section.contains("goal=\"Ship auth overhaul\""),
            "section missing goal: {section}"
        );
        assert!(
            section.contains("in_progress=\"middleware refactor\""),
            "section missing in_progress subtask: {section}"
        );
        assert!(
            section.contains("Active Plan"),
            "section missing header: {section}"
        );
        assert!(
            section.contains("exit_plan_mode"),
            "section must mention the exit-plan-mode escape hatch"
        );
    }

    #[test]
    fn resume_signal_ignores_long_unrelated_messages() {
        assert!(!message_signals_resume(
            "let me continue investigating the failing test"
        ));
        assert!(!message_signals_resume(""));
        assert!(!message_signals_resume("resume the job later, not now"));
    }
}
