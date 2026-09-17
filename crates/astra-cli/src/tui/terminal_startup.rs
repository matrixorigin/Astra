//! One-shot terminal queries before the first theme-dependent output.
//!
//! The crossterm reader owns all input, including queries, cursor position and
//! the later EventStream. Keep echo disabled until the TUI takes over so late
//! replies cannot be echoed during asynchronous session initialization.

use std::io;

#[cfg(unix)]
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, query_startup_attributes},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
#[cfg(unix)]
use nix::sys::termios::{LocalFlags, OutputFlags, SetArg, tcgetattr, tcsetattr};

use super::terminal_palette::{self, TerminalColors};
use super::theme::ThemeProfile;

pub(crate) const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

#[derive(Default)]
pub(crate) struct StartupTerminal {
    #[cfg(unix)]
    owns_raw_mode: bool,
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    raw_output_flags: Option<OutputFlags>,
    #[cfg(unix)]
    raw_local_flags: Option<LocalFlags>,
}

fn should_query_colors(profile: Option<&str>, no_color: bool, background_override: bool) -> bool {
    !no_color
        && !background_override
        && profile
            .and_then(ThemeProfile::parse)
            .unwrap_or(ThemeProfile::Auto)
            == ThemeProfile::Auto
}

impl StartupTerminal {
    pub(crate) fn begin() -> io::Result<Self> {
        if !super::event_loop::can_run_tui() {
            return Ok(Self::default());
        }
        #[cfg(unix)]
        {
            // Register before raw mode. The owner polls this listener during
            // asynchronous startup, then transfers it to the shutdown monitor.
            // Tokio handlers persist, so never drop the listener at handoff.
            let interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            // can_run_tui requires a tty on stdin; crossterm's tty_fd() selects
            // this same fd. Crossterm remains the owner of the saved raw mode.
            let original_mode = tcgetattr(io::stdin())?;
            enable_raw_mode()?;
            let mut guard = Self {
                owns_raw_mode: true,
                interrupt: Some(interrupt),
                raw_output_flags: None,
                raw_local_flags: None,
            };
            match crate::cli::stream::output_sink::write_stdout_operation(|stdout| {
                execute!(stdout, EnableBracketedPaste)
            })? {
                crate::cli::stream::output_sink::OutputWriteStatus::Written => {}
                crate::cli::stream::output_sink::OutputWriteStatus::Closed => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stdout closed during terminal startup",
                    ));
                }
            }

            // Startup uses normal line-oriented output. Preserve its newline
            // processing while retaining raw, non-echoing input.
            let mut mode = tcgetattr(io::stdin())?;
            guard.raw_output_flags = Some(mode.output_flags);
            guard.raw_local_flags = Some(mode.local_flags);
            mode.output_flags = original_mode.output_flags;
            mode.local_flags |= original_mode.local_flags & LocalFlags::ISIG;
            tcsetattr(io::stdin(), SetArg::TCSANOW, &mode)?;

            let profile = std::env::var("ASTRA_TUI_THEME").ok();
            let colors = should_query_colors(
                profile.as_deref(),
                std::env::var_os("NO_COLOR").is_some(),
                terminal_palette::has_background_override(),
            );
            let mut queried = TerminalColors::default();
            let mut sixel = None;
            match query_startup_attributes(colors, QUERY_TIMEOUT) {
                Ok(response) => {
                    use terminal_palette::{OscColorSlot, parse_osc_color_response};
                    queried.fg = response.foreground.as_deref().and_then(|response| {
                        parse_osc_color_response(response, OscColorSlot::Foreground)
                    });
                    queried.bg = response.background.as_deref().and_then(|response| {
                        parse_osc_color_response(response, OscColorSlot::Background)
                    });
                    sixel = response
                        .device_attributes
                        .map(|params| params.iter().skip(1).any(|&param| param == 4));
                }
                Err(error) => tracing::debug!(%error, "terminal startup query unavailable"),
            }
            terminal_palette::initialize_default_colors(queried);
            if let Some(supported) = sixel {
                astra_tools::display_sixel::set_sixel_supported(supported);
            }
            Ok(guard)
        }
        #[cfg(not(unix))]
        {
            terminal_palette::initialize_default_colors(TerminalColors::default());
            Ok(Self::default())
        }
    }

    /// Polled during startup and by the TUI shutdown monitor after handoff.
    /// Keeping one listener alive avoids leaving Tokio's persistent SIGINT
    /// handler installed with no consumer.
    pub(crate) async fn interrupted(&mut self) {
        #[cfg(unix)]
        if let Some(interrupt) = self.interrupt.as_mut() {
            let _ = interrupt.recv().await;
            return;
        }
        std::future::pending::<()>().await;
    }

    /// Restore raw output before ratatui takes ownership; input stays raw.
    pub(crate) fn prepare_tui(&self) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(output_flags) = self.raw_output_flags {
            let mut mode = tcgetattr(io::stdin())?;
            mode.output_flags = output_flags;
            if let Some(local_flags) = self.raw_local_flags {
                mode.local_flags = local_flags;
            }
            tcsetattr(io::stdin(), SetArg::TCSANOW, &mode)?;
        }
        Ok(())
    }

    pub(crate) fn handoff(&mut self) {
        #[cfg(unix)]
        {
            self.owns_raw_mode = false;
        }
    }
}

impl Drop for StartupTerminal {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.owns_raw_mode {
            let _ = disable_raw_mode();
            let _ = crate::cli::stream::output_sink::write_stdout_operation(|stdout| {
                execute!(stdout, DisableBracketedPaste)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_only_for_auto_without_a_manual_background() {
        for profile in [None, Some("auto"), Some("invalid")] {
            assert!(should_query_colors(profile, false, false));
        }
        for profile in ["light", "dark", "light-ansi", "dark-ansi", "plain"] {
            assert!(!should_query_colors(Some(profile), false, false));
        }
        assert!(!should_query_colors(None, true, false));
        assert!(!should_query_colors(None, false, true));
    }
}

#[cfg(all(test, unix))]
#[path = "terminal_startup_pty_tests.rs"]
mod pty_tests;
