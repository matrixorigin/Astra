pub(crate) fn looks_like_pending_local_plan_entry(
    state: &crate::cli::session::session_state::SessionState,
) -> bool {
    state.plan_mode_active()
        && state
            .cloud_plan_mirror
            .as_ref()
            .is_some_and(|plan| plan.goal.trim().is_empty())
}

pub(crate) fn clear_pending_local_plan_entry_if_inactive(
    state: &mut crate::cli::session::session_state::SessionState,
) {
    if state.plan_mode_active() {
        return;
    }
    if state
        .cloud_plan_mirror
        .as_ref()
        .is_some_and(|plan| plan.goal.trim().is_empty())
    {
        state.cloud_plan_mirror = None;
        state.plan_mode_sync_error = None;
    }
}

#[cfg(test)]
mod tests {
    use astra_runtime::plan::PlanModeState;

    use super::{clear_pending_local_plan_entry_if_inactive, looks_like_pending_local_plan_entry};
    use crate::cli::permission_manager::PermissionMode;
    use crate::cli::session::session_state::SessionState;

    #[test]
    fn only_an_active_empty_local_entry_is_pending() {
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(PlanModeState::new(String::new()));
        assert!(!looks_like_pending_local_plan_entry(&state));

        state.perm_manager.set_mode(PermissionMode::Plan);
        assert!(looks_like_pending_local_plan_entry(&state));

        state.cloud_plan_mirror = Some(PlanModeState::new("ship it".to_string()));
        assert!(!looks_like_pending_local_plan_entry(&state));
    }

    #[test]
    fn inactive_empty_entry_is_removed_without_touching_real_plan_state() {
        let mut inactive_empty = SessionState::default();
        inactive_empty.cloud_plan_mirror = Some(PlanModeState::new(String::new()));
        clear_pending_local_plan_entry_if_inactive(&mut inactive_empty);
        assert!(inactive_empty.cloud_plan_mirror.is_none());

        let mut inactive_plan = SessionState::default();
        inactive_plan.cloud_plan_mirror = Some(PlanModeState::new("ship it".to_string()));
        clear_pending_local_plan_entry_if_inactive(&mut inactive_plan);
        assert!(inactive_plan.cloud_plan_mirror.is_some());
    }
}
