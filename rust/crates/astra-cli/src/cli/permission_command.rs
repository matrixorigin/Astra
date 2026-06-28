use crate::cli::permission_manager::PermissionMode;
use crate::cli::session::session_state::SessionState;
use crate::cli::theme;
use crossterm::style::Stylize;

pub(crate) const PERMISSION_COMMAND_USAGE: &str =
    "auto, plan, accept_edits, prompt, deny, rules, trust, untrust, trace";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionCommandAction<'a> {
    Cycle,
    SetMode(PermissionMode),
    ShowRules,
    TrustWorkspace,
    UntrustWorkspace,
    ShowTrace,
    ExportTrace(&'a str),
    MissingTraceExport,
    Unknown(&'a str),
}

pub(crate) fn parse_permission_command(arg: &str) -> PermissionCommandAction<'_> {
    let arg = arg.trim();
    if arg.is_empty() {
        return PermissionCommandAction::Cycle;
    }

    if arg == "trace --export" {
        return PermissionCommandAction::MissingTraceExport;
    }
    if let Some(path) = arg.strip_prefix("trace --export ") {
        let path = path.trim();
        return if path.is_empty() {
            PermissionCommandAction::MissingTraceExport
        } else {
            PermissionCommandAction::ExportTrace(path)
        };
    }

    match arg {
        "rules" => PermissionCommandAction::ShowRules,
        "trust" => PermissionCommandAction::TrustWorkspace,
        "untrust" => PermissionCommandAction::UntrustWorkspace,
        "trace" => PermissionCommandAction::ShowTrace,
        _ => match arg.parse::<PermissionMode>() {
            Ok(mode) => PermissionCommandAction::SetMode(mode),
            Err(_) => PermissionCommandAction::Unknown(arg),
        },
    }
}

/// Next mode when cycling `/allow` with no argument.
///
/// `Deny` is intentionally sticky under the cycle: it is the most restrictive
/// mode and must only be exited by an explicit `/allow prompt` (or another
/// named mode). Cycling must never silently move a session out of `Deny`,
/// because that would widen permissions without an explicit user action.
/// `Deny` is likewise never a cycle *target* — it is reachable only via
/// `/allow deny`.
pub(crate) fn next_permission_mode_for_cycle(current: PermissionMode) -> PermissionMode {
    match current {
        PermissionMode::Deny => PermissionMode::Deny,
        PermissionMode::Prompt => PermissionMode::AcceptEdits,
        PermissionMode::AcceptEdits => PermissionMode::Plan,
        PermissionMode::Plan => PermissionMode::Auto,
        PermissionMode::Auto => PermissionMode::Prompt,
    }
}

pub(crate) fn permission_mode_display_label(mode: PermissionMode) -> &'static str {
    mode.chip_text()
}

pub(crate) fn permission_mode_cli_detail(mode: PermissionMode) -> Option<&'static str> {
    match mode {
        PermissionMode::Auto => Some("all tools auto-approved"),
        PermissionMode::Plan => Some("read-only investigation mode"),
        PermissionMode::AcceptEdits => Some("workspace-local edits auto-approved"),
        PermissionMode::Prompt | PermissionMode::Deny => None,
    }
}

pub(crate) fn permission_mode_feedback(mode: PermissionMode) -> String {
    format!("Mode → {}", permission_mode_display_label(mode))
}

pub(crate) fn handle_permission_command(arg: &str, state: &mut SessionState) {
    match parse_permission_command(arg) {
        PermissionCommandAction::Cycle => {
            let next = next_permission_mode_for_cycle(state.perm_manager.mode());
            state.perm_manager.set_mode(next);
            print_permission_mode(next);
        }
        PermissionCommandAction::SetMode(mode) => {
            state.perm_manager.set_mode(mode);
            print_permission_mode(mode);
        }
        PermissionCommandAction::ShowRules => {
            let summary = state.perm_manager.rules_summary();
            eprint!("{summary}");
        }
        PermissionCommandAction::TrustWorkspace => match state.perm_manager.trust_workspace() {
            Ok(message) => eprintln!("  {} {message}", theme::icon_info()),
            Err(err) => eprintln!("  {} Failed to trust workspace: {err}", theme::icon_warn()),
        },
        PermissionCommandAction::UntrustWorkspace => match state.perm_manager.untrust_workspace() {
            Ok(message) => eprintln!("  {} {message}", theme::icon_info()),
            Err(err) => eprintln!(
                "  {} Failed to mark workspace untrusted: {err}",
                theme::icon_warn()
            ),
        },
        PermissionCommandAction::ShowTrace => {
            for line in astra_turn_core::permission::audit::format_snapshot_lines(50) {
                eprintln!("{line}");
            }
        }
        PermissionCommandAction::ExportTrace(path) => {
            let lines = astra_turn_core::permission::audit::snapshot_redacted_jsonl_lines();
            let body = if lines.is_empty() {
                String::new()
            } else {
                format!("{}\n", lines.join("\n"))
            };
            match std::fs::write(path, body) {
                Ok(()) => eprintln!(
                    "  {} Permission trace exported to {path}",
                    theme::icon_info()
                ),
                Err(err) => eprintln!(
                    "  {} Failed to export permission trace to {path}: {err}",
                    theme::icon_warn()
                ),
            }
        }
        PermissionCommandAction::MissingTraceExport => {
            eprintln!("  {} Missing export path", theme::icon_warn());
        }
        PermissionCommandAction::Unknown(arg) => {
            eprintln!(
                "  {} Unknown mode '{}'. Use: {PERMISSION_COMMAND_USAGE}",
                theme::icon_warn(),
                arg
            );
        }
    }
}

fn print_permission_mode(mode: PermissionMode) {
    let detail = permission_mode_cli_detail(mode)
        .map(|detail| format!(" ({detail})"))
        .unwrap_or_default();
    eprintln!(
        "  {} Permission mode → {}{}",
        theme::icon_info(),
        permission_mode_display_label(mode).magenta(),
        detail
    );
}

#[cfg(test)]
mod tests {
    use super::{
        PermissionCommandAction, next_permission_mode_for_cycle, parse_permission_command,
    };
    use crate::cli::permission_manager::PermissionMode;

    #[test]
    fn parser_accepts_only_canonical_permission_modes() {
        let cases = [
            ("auto", PermissionMode::Auto),
            ("plan", PermissionMode::Plan),
            ("accept_edits", PermissionMode::AcceptEdits),
            ("prompt", PermissionMode::Prompt),
            ("deny", PermissionMode::Deny),
        ];

        for (input, mode) in cases {
            assert_eq!(
                parse_permission_command(input),
                PermissionCommandAction::SetMode(mode),
                "{input}"
            );
        }
    }

    #[test]
    fn parser_rejects_removed_permission_aliases() {
        for alias in ["all", "default", "ask", "status", "accept-edits"] {
            assert_eq!(
                parse_permission_command(alias),
                PermissionCommandAction::Unknown(alias),
                "removed alias must be unknown: {alias}"
            );
        }
    }

    #[test]
    fn parser_handles_non_mode_actions() {
        assert_eq!(parse_permission_command(""), PermissionCommandAction::Cycle);
        assert_eq!(
            parse_permission_command("rules"),
            PermissionCommandAction::ShowRules
        );
        assert_eq!(
            parse_permission_command("trust"),
            PermissionCommandAction::TrustWorkspace
        );
        assert_eq!(
            parse_permission_command("untrust"),
            PermissionCommandAction::UntrustWorkspace
        );
        assert_eq!(
            parse_permission_command("trace"),
            PermissionCommandAction::ShowTrace
        );
        assert_eq!(
            parse_permission_command("trace --export /tmp/permissions.jsonl"),
            PermissionCommandAction::ExportTrace("/tmp/permissions.jsonl")
        );
        assert_eq!(
            parse_permission_command("trace --export"),
            PermissionCommandAction::MissingTraceExport
        );
    }

    #[test]
    fn cycle_order_matches_user_facing_picker() {
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Prompt),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::AcceptEdits),
            PermissionMode::Plan
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Plan),
            PermissionMode::Auto
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Auto),
            PermissionMode::Prompt
        );
        // `Deny` is sticky under the cycle: it must only be exited by an
        // explicit `/allow <mode>`, never by a bare `/allow`.
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Deny),
            PermissionMode::Deny
        );
    }
}
