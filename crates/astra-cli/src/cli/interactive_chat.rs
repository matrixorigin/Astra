//! Shared interactive-chat launcher for both the bin entrypoint and CLI router.
//!
//! Keeping this in one place prevents drift between `main.rs` and
//! `command_router.rs` when TTY gating or TUI startup behavior changes.

use crossterm::style::Stylize;

use crate::cli::cli_config::cli_context::CliContext;

pub(crate) async fn run_interactive_chat(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    resume_session_id: Option<&str>,
    no_instructions: bool,
    cli_context: &CliContext,
) -> Result<(), String> {
    if !crate::tui::can_run_tui() {
        eprintln!(
            "{}",
            "astra requires an interactive terminal (TTY) for interactive chat.".red()
        );
        eprintln!();
        eprintln!("For one-shot use:   astra chat -m \"your message\"");
        eprintln!("For SSH:            ssh -t host \"astra\"");
        eprintln!("For scripts/CI:     astra --print \"your message\"");
        return Err("no TTY".into());
    }
    // Box::pin to reduce the parent future's stack frame size (avoids stack
    // overflow in debug-mode tests that instantiate execute_cli_command).
    Box::pin(crate::tui::run_tui(
        api,
        profile,
        initial_model,
        resume_session_id,
        no_instructions,
        cli_context,
    ))
    .await
}
