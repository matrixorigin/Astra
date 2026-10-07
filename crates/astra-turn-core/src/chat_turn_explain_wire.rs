//! Map host Explain settings to Server admission and local stderr behavior.

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
/// Hosts map their UI enum once into admission Explain settings and stderr hooks.
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

#[cfg(test)]
mod tests {
    use super::*;

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
