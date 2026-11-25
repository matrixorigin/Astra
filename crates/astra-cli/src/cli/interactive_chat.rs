//! Shared interactive-chat launcher for both the bin entrypoint and CLI router.
//!
//! Keeping this in one place prevents drift between `main.rs` and
//! `command_router.rs` when TTY gating or TUI startup behavior changes.

use crossterm::style::Stylize;

use crate::cli::cli_config::cli_context::CliContext;
use crate::cli::session::session_state::ExplainMode;

pub(crate) async fn run_interactive_chat(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    initial_explain: Option<ExplainMode>,
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
    #[cfg(unix)]
    let (device_runner, startup_failed) =
        match crate::cli::device_models::start_for_model(initial_model, false).await {
            Ok(runner) => (runner, false),
            Err(error) => {
                eprintln!("Device ChatGPT unavailable: {error}");
                (None, true)
            }
        };
    // The picker can switch to a device model even when the initial model is
    // organizational. Make saved models available without waiting to open TUI.
    #[cfg(unix)]
    let _picker_device_runner = if device_runner.is_none() && !startup_failed {
        Some(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
            async {
                match crate::cli::device_models::start_for_model(None, true).await {
                    Ok(runner) => runner,
                    Err(error) => {
                        tracing::debug!(%error, "device model host unavailable for the picker");
                        None
                    }
                }
            },
        )))
    } else {
        None
    };
    // Box::pin to reduce the parent future's stack frame size (avoids stack
    // overflow in debug-mode tests that instantiate execute_cli_command).
    Box::pin(crate::tui::run_tui(
        api,
        profile,
        initial_model,
        initial_explain,
        resume_session_id,
        no_instructions,
        cli_context,
    ))
    .await
}
