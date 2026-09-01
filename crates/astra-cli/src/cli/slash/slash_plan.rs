use crate::cli::session::session_state::SessionState;
use crate::cli::theme;
use astra_runtime::plan;
use crossterm::style::Stylize;

pub(crate) fn enter_local_plan_mode(state: &mut SessionState) {
    enter_local_plan_mode_with_goal(state, "");
}

/// Enter the client-side permission overlay without inventing a second remote
/// plan lifecycle. The Server owns durable plan approval when the model calls
/// `enter_plan_mode`; an explicit `/plan` only chooses the mode for this
/// interactive client and preserves the user's goal for display/prompting.
pub(crate) fn enter_local_plan_mode_with_goal(state: &mut SessionState, goal: &str) {
    state.cloud_plan_mirror = Some(plan::PlanModeState::new(goal.trim().to_string()));
    state.plan_mode_sync_error = None;
    state
        .perm_manager
        .set_mode(crate::cli::permission_manager::PermissionMode::Plan);
}

pub(crate) fn exit_local_plan_mode(state: &mut SessionState) {
    state.cloud_plan_mirror = None;
    state.plan_mode_sync_error = None;
    state
        .perm_manager
        .set_mode(crate::cli::permission_manager::PermissionMode::Auto);
}

async fn resolve_plan_token(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
) -> Option<String> {
    if let Some(token) = token {
        return Some(token.to_string());
    }
    crate::cli::session::session_runtime::fresh_access_token(api, profile).await
}

pub(crate) async fn handle_plan_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
    token: Option<&str>,
) -> Result<(), String> {
    let plan_request = arg.trim();

    if plan_request.is_empty() {
        crate::cli::plan::plan_lifecycle::clear_pending_local_plan_entry_if_inactive(state);
    }

    if plan_request.is_empty()
        && crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(state)
    {
        exit_local_plan_mode(state);
        eprintln!("  {} Exited plan mode.", theme::icon_ok());
        return Ok(());
    }

    if plan_request.is_empty() && state.cloud_plan_mirror.is_some() {
        exit_local_plan_mode(state);
        eprintln!("  {} Exited plan mode.", theme::icon_ok());
        return Ok(());
    }

    if plan_request.is_empty() {
        enter_local_plan_mode(state);
        eprintln!();
        eprintln!(
            "  {} Plan mode active. Describe your goal.",
            theme::icon_ok()
        );
        return Ok(());
    }

    let Some(token) = resolve_plan_token(api, profile, token).await else {
        eprintln!("{}", "  Not logged in. Use /login.".yellow());
        return Ok(());
    };

    enter_local_plan_mode_with_goal(state, plan_request);
    eprintln!(
        "  {} Plan mode active. Goal: {}",
        theme::icon_ok(),
        plan_request
    );
    crate::cli::turn::turn_entry::handle_chat_input(
        plan_request.to_string(),
        Some(&token),
        state,
        crate::cli::turn::turn_entry::TurnContext {
            api,
            profile,
            post_commit_tx: None,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{enter_local_plan_mode_with_goal, handle_plan_command};
    use crate::cli::session::session_state::SessionState;
    use astra_runtime::plan;

    #[tokio::test]
    async fn bare_plan_arms_pending_local_entry() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();

        handle_plan_command("", &api, None, &mut state, None)
            .await
            .unwrap();

        assert!(crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(&state));
        assert!(
            state.plan_mode_active(),
            "bare /plan should switch UI into plan mode"
        );
    }

    #[tokio::test]
    async fn bare_plan_clears_stale_pending_local_entry_before_entering() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(plan::PlanModeState::new(String::new()));

        handle_plan_command("", &api, None, &mut state, None)
            .await
            .unwrap();

        assert!(crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(&state));
        assert!(
            state.plan_mode_active(),
            "stale inactive pending state should be replaced by a fresh local plan entry"
        );
    }

    #[tokio::test]
    async fn bare_plan_exits_pending_local_entry_without_remote_call() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(plan::PlanModeState::new(String::new()));
        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Plan);

        handle_plan_command("", &api, None, &mut state, None)
            .await
            .unwrap();

        assert!(state.cloud_plan_mirror.is_none());
        assert!(
            !state.plan_mode_active(),
            "exiting bare /plan should restore normal-chat mode"
        );
    }

    #[tokio::test]
    async fn bare_plan_exits_active_mode_without_remote_plan_lifecycle() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state.cloud_plan_mirror = Some(plan::PlanModeState::new("Ship auth".to_string()));
        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Plan);

        handle_plan_command("", &api, None, &mut state, Some("token"))
            .await
            .unwrap();

        assert!(state.cloud_plan_mirror.is_none());
        assert!(
            !state.plan_mode_active(),
            "explicit /plan exit should restore normal-chat mode"
        );
    }

    #[test]
    fn local_plan_goal_is_trimmed_and_enters_read_only_mode() {
        let mut state = SessionState::default();

        enter_local_plan_mode_with_goal(&mut state, "  Ship auth safely  ");

        let mirror = state.cloud_plan_mirror.as_ref().expect("plan mirror");
        assert_eq!(mirror.goal, "Ship auth safely");
        assert!(state.plan_mode_active());
    }
}
