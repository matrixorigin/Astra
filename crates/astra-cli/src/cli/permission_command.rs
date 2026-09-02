use crate::cli::permission_manager::PermissionMode;
use crate::cli::session::session_state::SessionState;
use crate::cli::theme;
use crossterm::style::Stylize;

pub(crate) const PERMISSION_COMMAND_USAGE: &str =
    "auto, bypass, read_only, accept_edits, prompt, deny, rules, trust, untrust, trace";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionCommandAction<'a> {
    /// No mode was named. Interactive surfaces may open the typed picker;
    /// line-mode clients must show the current setting and explicit choices.
    ChooseMode,
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
        return PermissionCommandAction::ChooseMode;
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
        "read_only" => PermissionCommandAction::SetMode(PermissionMode::Plan),
        // Plan lifecycle is not a permission-preset command. `/plan` owns
        // authoring, approval and execution transitions; its read-only policy
        // is represented here as `read_only`.
        "plan" => PermissionCommandAction::Unknown(arg),
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

pub(crate) fn permission_mode_display_label(mode: PermissionMode) -> &'static str {
    mode.chip_text()
}

pub(crate) fn permission_mode_cli_detail(mode: PermissionMode) -> Option<&'static str> {
    match mode {
        PermissionMode::Auto => {
            Some("normal tool risk auto-approved; some git/sensitive gates may still stop")
        }
        PermissionMode::Bypass => {
            Some("skip approval prompts; catastrophic and policy hard-denies still apply")
        }
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
        PermissionCommandAction::ChooseMode => {
            let current = permission_mode_display_label(state.perm_manager.mode());
            eprintln!("  Permission mode: {current}");
            eprintln!(
                "  Choose explicitly: /allow prompt | accept_edits | read_only | auto | bypass | deny\n  Planning workflow: /plan"
            );
        }
        PermissionCommandAction::SetMode(mode) => {
            state.perm_manager.set_mode(mode);
            crate::cli::plan::plan_lifecycle::clear_pending_local_plan_entry_if_inactive(state);
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
    use super::{PermissionCommandAction, handle_permission_command, parse_permission_command};
    use crate::cli::permission_manager::PermissionMode;
    use crate::cli::session::session_state::SessionState;
    use astra_runtime::plan;

    #[test]
    fn parser_accepts_only_canonical_permission_modes() {
        let cases = [
            ("auto", PermissionMode::Auto),
            ("bypass", PermissionMode::Bypass),
            ("read_only", PermissionMode::Plan),
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
        for alias in [
            "all",
            "default",
            "ask",
            "status",
            "accept-edits",
            "plan",
            "skip",
            "readonly",
        ] {
            assert_eq!(
                parse_permission_command(alias),
                PermissionCommandAction::Unknown(alias),
                "removed alias must be unknown: {alias}"
            );
        }
    }

    #[test]
    fn parser_handles_non_mode_actions() {
        assert_eq!(
            parse_permission_command(""),
            PermissionCommandAction::ChooseMode
        );
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
    fn unspecified_permission_command_does_not_mutate_current_mode() {
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(plan::PlanModeState::new(String::new()));
        state.perm_manager.set_mode(PermissionMode::Plan);

        handle_permission_command("", &mut state);

        assert_eq!(state.perm_manager.mode(), PermissionMode::Plan);
        assert!(
            state.cloud_plan_mirror.is_some(),
            "choosing no mode must not mutate plan state"
        );
    }
}
