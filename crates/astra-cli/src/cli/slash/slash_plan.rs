use crate::cli::session::session_state::SessionState;
use crate::cli::theme;
use astra_runtime::plan;
use crossterm::style::Stylize;

/// Enter the client-side permission overlay without inventing a second remote
/// plan lifecycle. The Server owns durable plan approval when the model calls
/// `enter_plan_mode`; `/plan` chooses this client's mode and sends the goal
/// through the normal conversation path.
pub(crate) fn enter_local_plan_mode_with_goal(state: &mut SessionState, goal: &str) {
    state.cloud_plan_mirror = Some(plan::PlanModeState::new(goal.trim().to_string()));
    state.plan_mode_sync_error = None;
    state
        .perm_manager
        .set_mode(crate::cli::permission_manager::PermissionMode::Plan);
}

pub(crate) fn ensure_local_plan_mode(state: &mut SessionState, goal: &str) {
    if !state.plan_mode_active() || state.cloud_plan_mirror.is_none() {
        enter_local_plan_mode_with_goal(state, goal);
    }
}

pub(crate) fn planning_guidance(goal: &str) -> String {
    if goal.trim().is_empty() {
        "Pause implementation and prepare a plan for the current task. Present the plan for approval.".to_string()
    } else {
        format!(
            "Prepare a plan for this goal before making changes: {}",
            goal.trim()
        )
    }
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

    ensure_local_plan_mode(state, plan_request);

    let Some(token) = resolve_plan_token(api, profile, token).await else {
        eprintln!("{}", "  Not logged in. Use /login.".yellow());
        return Ok(());
    };

    eprintln!("  {} Plan mode active.", theme::icon_ok());
    let guidance = planning_guidance(plan_request);
    crate::cli::turn::turn_entry::handle_chat_input(
        crate::cli::turn::turn_entry::ChatInput::Conversation(guidance),
        Some(&token),
        state,
        crate::cli::turn::turn_entry::TurnContext {
            api,
            profile,
            post_commit_tx: None,
            admission: None,
            explain_analyze_terminal_degraded: None,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ensure_local_plan_mode, enter_local_plan_mode_with_goal, planning_guidance};
    use crate::cli::session::session_state::SessionState;
    use astra_runtime::plan;

    #[test]
    fn plan_entry_is_idempotent_and_preserves_an_existing_goal() {
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(plan::PlanModeState::new("stale".into()));
        ensure_local_plan_mode(&mut state, "Ship auth");
        assert!(state.plan_mode_active());
        assert_eq!(state.cloud_plan_mirror.as_ref().unwrap().goal, "Ship auth");
        ensure_local_plan_mode(&mut state, "");
        assert!(state.plan_mode_active());
        assert_eq!(state.cloud_plan_mirror.as_ref().unwrap().goal, "Ship auth");
        assert!(planning_guidance("").contains("current task"));
        assert!(planning_guidance("Ship auth").contains("Ship auth"));
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
