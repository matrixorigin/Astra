use std::time::Instant;

use super::session_adaptation::{finalize_turn_adaptation, prepare_turn_adaptation};
use super::session_input::{
    build_effective_line, clear_pending_recovery_for_ordinary_chat_input, finalize_effective_line,
};
use super::turn_retry::settle_turn_attempt;
use super::turn_settlement::TurnDispatch;
use super::turn_stream_runner::{TurnAttempt, execute_stream_turn};
use super::*;

pub(crate) struct TurnContext<'a> {
    pub(crate) api: &'a astra_thin_client::ThinClient,
    pub(crate) profile: Option<&'a str>,
}

async fn run_chat_turn(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: &str,
    message: &str,
    session_id: Option<&str>,
) -> TurnAttempt {
    prepare_turn_adaptation(state, api, token, message).await;
    let attempt = execute_stream_turn(state, api, profile, token, message, session_id).await;
    finalize_turn_adaptation(state, matches!(attempt, TurnAttempt::Interrupted(_))).await;
    attempt
}

fn run_chat_turn_boxed<'a>(
    state: &'a mut SessionState,
    api: &'a astra_thin_client::ThinClient,
    profile: Option<&'a str>,
    token: &'a str,
    message: &'a str,
    session_id: Option<&'a str>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnAttempt> + 'a>> {
    Box::pin(run_chat_turn(
        state, api, profile, token, message, session_id,
    ))
}

pub(crate) async fn handle_chat_input(
    line: String,
    current_token: Option<&str>,
    state: &mut SessionState,
    ctx: TurnContext<'_>,
) -> Result<(), String> {
    handle_chat_input_with_ui(
        line,
        current_token,
        state,
        ctx,
        &mut crate::cli::ui_adapter::LineUiAdapter,
    )
    .await
}

pub(crate) async fn handle_chat_input_with_ui(
    line: String,
    current_token: Option<&str>,
    state: &mut SessionState,
    ctx: TurnContext<'_>,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> Result<(), String> {
    if let Some(cmd) = line.trim_start().strip_prefix('!') {
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            println!("! {cmd}");
            match std::process::Command::new("sh").arg("-c").arg(cmd).status() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    eprintln!("! {cmd}: exit {}", status.code().unwrap_or(-1));
                }
                Err(e) => {
                    eprintln!("! {cmd}: {e}");
                }
            }
        }
        return Ok(());
    }

    let token = match current_token {
        Some(token) => token,
        None => {
            ui.show_warning("  Not logged in. Use /login to authenticate.");
            return Ok(());
        }
    };

    if state.session_id.is_none() && state.pending_recovery.is_some() {
        clear_pending_recovery_for_ordinary_chat_input(state);
    }

    state.perm_manager.trust_explicit_user_paths(&line);

    ui.blank_line();

    if crate::cli::plan_lifecycle::looks_like_pending_local_plan_entry(state)
        && let Err(error) = crate::cli::plan_lifecycle::enter_remote_plan_mode(
            ctx.api,
            ctx.profile,
            token,
            state,
            &line,
        )
        .await
    {
        ui.show_error(&error);
        return Ok(());
    }

    let resume_guidance = state.resume_guidance.take();
    let effective_line = finalize_effective_line(
        build_effective_line(&line, state, ui),
        resume_guidance,
        state,
    )
    .await;
    let turn_start = Instant::now();
    let session_id = state.session_id.clone();
    let attempt = run_chat_turn(
        state,
        ctx.api,
        ctx.profile,
        token,
        &effective_line,
        session_id.as_deref(),
    )
    .await;
    let mut dispatch = TurnDispatch {
        ctx: &ctx,
        line: &line,
        effective_line: &effective_line,
        token,
        session_id: session_id.as_deref(),
        turn_start,
        ui,
    };

    settle_turn_attempt(state, &mut dispatch, attempt, run_chat_turn_boxed).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn pending_recovery_never_restores_from_ordinary_chat_input() {
        let source = include_str!("turn_entry.rs");
        let start = source
            .find("pub(crate) async fn handle_chat_input_with_ui")
            .expect("handle_chat_input_with_ui should exist");
        let body = &source[start..];
        let gate_end = body
            .find("ui.blank_line();")
            .expect("pre-turn gate should reach the blank-line boundary");
        let pre_turn_gate = &body[..gate_end];
        assert!(
            !pre_turn_gate.contains("restore_session_into_state(")
                && !pre_turn_gate.contains("is_low_information_followup(&line)")
                && pre_turn_gate.contains("clear_pending_recovery_for_ordinary_chat_input(state);"),
            "ordinary chat input must not auto-restore pending recovery; resume is explicit only"
        );
    }
}
