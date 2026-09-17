//! Plan-mode UI helpers extracted from `event_loop.rs`.
//!
//! Handles plan-mode transition notices, explicit `/plan <goal>` parsing,
//! and UI snapshotting when entering/exiting plan mode.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlanModeUiSnapshot {
    pub(crate) active: bool,
    pub(crate) goal: String,
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
                    "Plan mode active - describe your goal. Execution begins only after you approve the plan review; use `/plan` to exit.".into(),
                )
            } else {
                Some(format!(
                    "Plan mode active - goal: {}. Send refinements; execution begins only after you approve the plan review. Use `/plan` to exit.",
                    summarize_plan_goal(&after.goal)
                ))
            }
        }
        (true, true) if before.goal != after.goal && !after.goal.is_empty() => Some(format!(
            "Plan goal set - {}. Send refinements; execution begins only after you approve the plan review. Use `/plan` to exit.",
            summarize_plan_goal(&after.goal)
        )),
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

pub(crate) fn slash_plan_goal(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("/plan")?;
    if !rest.is_empty() && !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let goal = rest.trim();
    (!goal.is_empty()).then_some(goal)
}

#[cfg(test)]
mod tests {
    use super::slash_plan_goal;

    #[test]
    fn inline_plan_goal_requires_an_exact_command_token() {
        assert_eq!(slash_plan_goal("/plan ship it"), Some("ship it"));
        assert_eq!(slash_plan_goal("  /plan   ship it  "), Some("ship it"));
        assert_eq!(slash_plan_goal("/plan"), None);
        assert_eq!(slash_plan_goal("/planner ship it"), None);
    }
}
