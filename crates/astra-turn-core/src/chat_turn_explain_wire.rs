//! Serialize the `explain` field for edge `/chat` JSON payloads (thin-client protocol).

use serde_json::{Value, json};

/// High-level explain setting from a host UI (REPL flag, CLI arg, etc.).
///
/// Maps to [`AgenticChatExplainFlags`] for `/chat` JSON and stderr explain lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgenticExplainUiMode {
    Off,
    On,
    Verbose,
}

/// Booleans for JSON `explain` plus whether to print selector/restricted lines to stderr.
///
/// `explain_stderr` is **verbose-only**: `/explain on` still enables server-side explain
/// traces without flooding the terminal; use **verbose** when you want selector stderr.
///
/// Hosts map their UI enum once → this struct → [`chat_turn_base_payload`](super::chat_turn_payload::chat_turn_base_payload) + stderr hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgenticChatExplainFlags {
    pub explain_verbose: bool,
    pub explain_on: bool,
    pub explain_stderr: bool,
}

impl AgenticChatExplainFlags {
    #[must_use]
    pub fn from_explain_ui_mode(mode: AgenticExplainUiMode) -> Self {
        match mode {
            AgenticExplainUiMode::Off => Self {
                explain_verbose: false,
                explain_on: false,
                explain_stderr: false,
            },
            AgenticExplainUiMode::On => Self {
                explain_verbose: false,
                explain_on: true,
                explain_stderr: false,
            },
            AgenticExplainUiMode::Verbose => Self {
                explain_verbose: true,
                explain_on: false,
                explain_stderr: true,
            },
        }
    }
}

/// Build the `explain` field: `false` | `true` | `"verbose"`.
///
/// Callers map their enum to the two booleans (`verbose` wins over `on`).
#[must_use]
pub fn chat_turn_explain_field_json(verbose: bool, on: bool) -> Value {
    if verbose {
        json!("verbose")
    } else if on {
        json!(true)
    } else {
        json!(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agentic_flags_round_trip_to_json_field() {
        let off = AgenticChatExplainFlags {
            explain_verbose: false,
            explain_on: false,
            explain_stderr: false,
        };
        assert_eq!(
            chat_turn_explain_field_json(off.explain_verbose, off.explain_on),
            json!(false)
        );
        let on = AgenticChatExplainFlags {
            explain_verbose: false,
            explain_on: true,
            explain_stderr: false,
        };
        assert_eq!(
            chat_turn_explain_field_json(on.explain_verbose, on.explain_on),
            json!(true)
        );
        let verb = AgenticChatExplainFlags {
            explain_verbose: true,
            explain_on: false,
            explain_stderr: true,
        };
        assert_eq!(
            chat_turn_explain_field_json(verb.explain_verbose, verb.explain_on),
            json!("verbose")
        );
    }

    #[test]
    fn off_is_false() {
        assert_eq!(chat_turn_explain_field_json(false, false), json!(false));
    }

    #[test]
    fn on_is_true() {
        assert_eq!(chat_turn_explain_field_json(false, true), json!(true));
    }

    #[test]
    fn verbose_string() {
        assert_eq!(chat_turn_explain_field_json(true, false), json!("verbose"));
    }

    #[test]
    fn verbose_over_on() {
        assert_eq!(chat_turn_explain_field_json(true, true), json!("verbose"));
    }

    #[test]
    fn explain_ui_mode_maps_like_cli() {
        let off = AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Off);
        assert!(!off.explain_verbose && !off.explain_on && !off.explain_stderr);
        let on = AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::On);
        assert!(!on.explain_verbose && on.explain_on && !on.explain_stderr);
        let v = AgenticChatExplainFlags::from_explain_ui_mode(AgenticExplainUiMode::Verbose);
        assert!(v.explain_verbose && !v.explain_on && v.explain_stderr);
    }
}
