//! Plan-mode UI helpers extracted from `event_loop.rs`.
//!
//! Handles plan-mode transitions, implicit plan request detection,
//! and UI snapshotting when entering/exiting plan mode.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlanModeUiSnapshot {
    pub(crate) active: bool,
    pub(crate) goal: String,
    pub(crate) executing: bool,
}

pub(crate) fn capture_plan_mode_ui_snapshot(
    state: &crate::cli::session::session_state::SessionState,
) -> PlanModeUiSnapshot {
    PlanModeUiSnapshot {
        active: state.plan_mode_active(),
        goal: state
            .cloud_plan_mirror
            .as_ref()
            .map(|ps| ps.goal.trim().to_string())
            .unwrap_or_default(),
        executing: state.executing_plan.is_some() || state.plan_handle.is_some(),
    }
}

pub(crate) fn summarize_plan_goal(goal: &str) -> String {
    let summary: String = goal.chars().take(80).collect();
    if goal.chars().count() > 80 {
        format!("{summary}...")
    } else {
        summary
    }
}

pub(crate) fn plan_transition_notice(
    before: &PlanModeUiSnapshot,
    after: &PlanModeUiSnapshot,
    _triggered_by_plan_request: bool,
) -> Option<String> {
    match (before.active, after.active) {
        (false, true) => {
            if after.goal.is_empty() {
                Some(
                    "Plan mode active - describe your goal. Use `go` to run once a plan is ready, or `/plan` to exit.".into(),
                )
            } else {
                Some(format!(
                    "Plan mode active - goal: {}. Send edits, `show` to inspect, `go` to run, `/plan` to exit.",
                    summarize_plan_goal(&after.goal)
                ))
            }
        }
        (true, true) if before.goal != after.goal && !after.goal.is_empty() => Some(format!(
            "Plan goal set - {}. Send edits, `show` to inspect, `go` to run, `/plan` to exit.",
            summarize_plan_goal(&after.goal)
        )),
        (true, false) if after.executing => {
            Some("Plan mode closed - execution is running in the background.".into())
        }
        (true, false) => Some("Plan mode closed - back to normal chat.".into()),
        _ => None,
    }
}

pub(crate) fn commit_plan_transition_notice(
    chat_widget: &mut super::chat_widget::ChatWidget,
    before: &PlanModeUiSnapshot,
    state: &crate::cli::session::session_state::SessionState,
    triggered_by_plan_request: bool,
) {
    let after = capture_plan_mode_ui_snapshot(state);
    if let Some(msg) = plan_transition_notice(before, &after, triggered_by_plan_request) {
        chat_widget.commit_system(super::history_cell::system::SystemCell::response(msg));
    }
}

pub(crate) fn looks_like_implicit_plan_request(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return false;
    }

    let lowered = trimmed.to_lowercase();
    let meta_queries = [
        "what is /plan",
        "how does /plan",
        "what does /plan",
        "plan mode",
        "plan模式",
        "现在plan是怎么",
        "什么是/plan",
        "/plan是什么意思",
        "怎么进入plan",
    ];
    if meta_queries.iter().any(|needle| lowered.contains(needle)) {
        return false;
    }

    let planning_requests = [
        "help me plan",
        "please plan",
        "plan how to",
        "make a plan",
        "draft a plan",
        "come up with a plan",
        "plan out",
        "帮我计划",
        "帮我规划",
        "给我一个计划",
        "给我个计划",
        "做个计划",
        "规划一下",
        "计划一下",
        "制定计划",
        "先计划",
    ];
    planning_requests
        .iter()
        .any(|needle| lowered.contains(needle))
}

pub(crate) fn slash_plan_goal(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("/plan")?;
    let goal = rest.trim();
    (!goal.is_empty()).then_some(goal)
}
