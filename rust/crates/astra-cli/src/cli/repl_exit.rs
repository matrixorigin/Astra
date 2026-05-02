//! Shared REPL exit helpers.

use crossterm::style::Stylize;

use super::{
    ReplState, auth_flow::clear_profile_last_session, cli_utils::prefix_chars,
    session_cleanup::finalize_session, session_guard::ShutdownSignal,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplExit {
    Command,
    Eof,
    InputError,
    BudgetLimit,
    Shutdown(ShutdownSignal),
}

pub(crate) async fn finalize_repl_exit(state: &mut ReplState, profile: Option<&str>, reason: ReplExit) {
    finalize_session(state).await;

    if should_show_resume_hint(reason)
        && state.turn > 0
        && let Some(ref sid) = state.session_id
    {
        let short = prefix_chars(sid, 8);
        eprintln!(
            "{}",
            format!("  Session {short}… saved. To resume: /resume {sid}").dim()
        );
    }

    if matches!(reason, ReplExit::Eof) && state.session_id.is_some() {
        let _ = clear_profile_last_session(profile);
    }
}

fn should_show_resume_hint(reason: ReplExit) -> bool {
    matches!(
        reason,
        ReplExit::Command | ReplExit::Eof | ReplExit::BudgetLimit | ReplExit::Shutdown(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_hint_is_shown_for_graceful_exit_paths() {
        assert!(should_show_resume_hint(ReplExit::Command));
        assert!(should_show_resume_hint(ReplExit::Eof));
        assert!(should_show_resume_hint(ReplExit::BudgetLimit));
        assert!(should_show_resume_hint(ReplExit::Shutdown(
            ShutdownSignal::Sigterm
        )));
        assert!(!should_show_resume_hint(ReplExit::InputError));
    }
}
