//! Auto-suggest prompt for entering plan mode.
//!
//! Replaces the previous `stdin().read_line` flow which blocked the REPL
//! indefinitely until the user typed a full line and pressed Enter. The new
//! flow:
//!
//!   * Renders a single-line prompt with a live countdown (default 5s).
//!   * Reads keystrokes in raw mode without requiring Enter.
//!   * Defaults to **No** on timeout, on Esc, on Enter, or on any character
//!     other than the explicit accept set (`y`, `Y`, `是`).
//!   * Never blocks longer than the timeout — one `event::poll` slice per
//!     250ms keeps Ctrl-C responsive.
//!
//! Terminal IO is isolated behind [`prompt_auto_suggest`]. The classification
//! rule is exposed as [`classify_keystroke`] so it can be unit-tested without
//! touching crossterm.

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Stylize;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSuggestDecision {
    /// User pressed an accept key (`y`, `Y`, `是`).
    Accepted,
    /// User pressed an explicit decline key (`n`, `N`, `Enter`, `Esc`,
    /// or any non-accept printable).
    Declined,
    /// No key arrived before the deadline.
    TimedOut,
    /// Prompt was interrupted by Ctrl-C / Ctrl-D.
    Interrupted,
}

/// Pure classification of a single keystroke.
///
/// Returns `None` if the key is ignored (e.g. shift release, modifier-only
/// events) so the caller can keep polling until the deadline.
pub fn classify_keystroke(key: KeyEvent) -> Option<AutoSuggestDecision> {
    // Treat Ctrl-C / Ctrl-D as an explicit interrupt so the caller can
    // propagate the cancellation signal upstream rather than silently
    // declining.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('d') => {
                return Some(AutoSuggestDecision::Interrupted);
            }
            _ => return None,
        }
    }

    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(AutoSuggestDecision::Accepted),
        KeyCode::Char('是') => Some(AutoSuggestDecision::Accepted),
        // Bare Enter / Esc / N / any other printable is a Decline so the
        // user can dismiss the prompt with a single keystroke.
        KeyCode::Enter
        | KeyCode::Esc
        | KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char(_) => Some(AutoSuggestDecision::Declined),
        _ => None,
    }
}

/// Default timeout for the auto-suggest prompt.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Show the suggestion prompt and wait for a keystroke up to `timeout`.
///
/// Renders a single line `Enter plan mode? [y/N] (5s) — ⏎ to skip` on
/// stderr and updates the countdown in place. On any IO failure the
/// prompt degrades to a non-interactive single-line print and returns
/// `Declined` (matching the safe default).
pub fn prompt_auto_suggest(reason: &str, timeout: Duration) -> AutoSuggestDecision {
    use std::io::Write;
    let mut stderr = std::io::stderr();

    eprintln!();
    eprintln!("{}  {}", "📋".yellow(), reason);

    // Try to enter raw mode. If it fails (e.g. non-tty), fall back to
    // the safe default of Declined and tell the user.
    if enable_raw_mode().is_err() {
        eprintln!(
            "{}  Plan mode suggestion ignored (no interactive terminal). \
             Type `/plan` to enter manually.",
            theme::icon_warn()
        );
        return AutoSuggestDecision::Declined;
    }

    let deadline = Instant::now() + timeout;
    let tick = Duration::from_millis(250);
    let mut last_remaining_secs: i64 = -1;
    let mut decision = AutoSuggestDecision::TimedOut;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let remaining_secs = remaining.as_secs() as i64 + 1;
        if remaining_secs != last_remaining_secs {
            // Carriage return + clear-to-eol so the countdown animates in
            // place without scrolling the terminal.
            let _ = write!(
                stderr,
                "\r\x1b[2K{}  Enter plan mode? [y/N] ({}s) — ⏎/n to skip ",
                "💡".cyan(),
                remaining_secs
            );
            let _ = stderr.flush();
            last_remaining_secs = remaining_secs;
        }

        match event::poll(tick.min(remaining)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) => {
                    if let Some(d) = classify_keystroke(k) {
                        decision = d;
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => continue,
            Err(_) => break,
        }
    }

    let _ = disable_raw_mode();
    // Clear the countdown line and emit a final result line so the
    // transcript is readable.
    let _ = write!(stderr, "\r\x1b[2K");
    let _ = stderr.flush();
    match decision {
        AutoSuggestDecision::Accepted => {
            eprintln!("{}  Entering plan mode…", "📋".green());
        }
        AutoSuggestDecision::TimedOut => {
            eprintln!(
                "{}  No response — proceeding with normal chat. (Use `/plan` to enter manually.)",
                "→".dim()
            );
        }
        AutoSuggestDecision::Declined => {
            eprintln!(
                "{}  Skipping plan mode — proceeding with normal chat.",
                "→".dim()
            );
        }
        AutoSuggestDecision::Interrupted => {
            eprintln!("{}  Cancelled.", theme::icon_warn());
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn lowercase_y_accepts() {
        assert_eq!(
            classify_keystroke(key(KeyCode::Char('y'))),
            Some(AutoSuggestDecision::Accepted)
        );
    }

    #[test]
    fn uppercase_y_accepts() {
        assert_eq!(
            classify_keystroke(key(KeyCode::Char('Y'))),
            Some(AutoSuggestDecision::Accepted)
        );
    }

    #[test]
    fn chinese_yes_accepts() {
        assert_eq!(
            classify_keystroke(key(KeyCode::Char('是'))),
            Some(AutoSuggestDecision::Accepted)
        );
    }

    #[test]
    fn enter_declines() {
        // Bare Enter must NOT auto-accept — the user's complaint was that
        // a stray Enter from typing more text would commit them to plan mode.
        assert_eq!(
            classify_keystroke(key(KeyCode::Enter)),
            Some(AutoSuggestDecision::Declined)
        );
    }

    #[test]
    fn esc_declines() {
        assert_eq!(
            classify_keystroke(key(KeyCode::Esc)),
            Some(AutoSuggestDecision::Declined)
        );
    }

    #[test]
    fn n_declines() {
        assert_eq!(
            classify_keystroke(key(KeyCode::Char('n'))),
            Some(AutoSuggestDecision::Declined)
        );
        assert_eq!(
            classify_keystroke(key(KeyCode::Char('N'))),
            Some(AutoSuggestDecision::Declined)
        );
    }

    #[test]
    fn other_printables_decline() {
        // Any non-accept printable is a Decline so a stray keystroke
        // doesn't get held in a "still waiting" state.
        assert_eq!(
            classify_keystroke(key(KeyCode::Char('x'))),
            Some(AutoSuggestDecision::Declined)
        );
        assert_eq!(
            classify_keystroke(key(KeyCode::Char(' '))),
            Some(AutoSuggestDecision::Declined)
        );
    }

    #[test]
    fn ctrl_c_interrupts() {
        assert_eq!(
            classify_keystroke(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(AutoSuggestDecision::Interrupted)
        );
        assert_eq!(
            classify_keystroke(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(AutoSuggestDecision::Interrupted)
        );
    }

    #[test]
    fn ignored_keys_return_none() {
        // Modifier-only / function-key events are ignored so the prompt
        // keeps polling until the real timeout.
        assert_eq!(classify_keystroke(key(KeyCode::F(5))), None);
        assert_eq!(
            classify_keystroke(key_mod(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            None,
            "ctrl+alt combinations are not yes/no"
        );
    }

    #[test]
    fn default_timeout_is_five_seconds() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(5));
    }
}
