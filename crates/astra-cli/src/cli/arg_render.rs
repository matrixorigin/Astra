//! Render slash/CLI subcommand structs back into stable textual argument lists.
//!
//! These helpers exist so features like plan replay, delegation, and audit can
//! reconstruct the operator-facing command line from parsed argument structs
//! without duplicating formatting logic at each call site.

use crate::cli::cli_config::cli_args::{
    AgentArgs, AgentSubcommand, BugArgs, BugSubcommand, DebugArgs, DiffArgs, DiffSubcommand,
    GrepArgs, GrepSubcommand, MemoryArgs, MemorySubcommand, MessagingArgs, PermissionsArgs,
    PermissionsSubcommand, ReviewArgs, ReviewSubcommand, TeamArgs, TeamSubcommand,
};

/// Prepend optional system instructions to a user message.
pub(crate) fn apply_system_prompt(message: &str, system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(sp) => format!("<system_instructions>\n{sp}\n</system_instructions>\n\n{message}"),
        None => message.to_string(),
    }
}

/// Join a slice of words with space separators.
pub(crate) fn join_words(words: &[String]) -> String {
    words.join(" ")
}

/// Render [`TeamArgs`] back into a stable textual argument list
/// for plan replay, delegation, and audit.
pub(crate) fn render_team_args(args: &TeamArgs) -> String {
    match &args.command {
        None | Some(TeamSubcommand::List) => String::new(),
        Some(TeamSubcommand::Create(cmd)) => {
            let suffix = join_words(&cmd.description);
            if suffix.is_empty() {
                format!("create {}", cmd.name)
            } else {
                format!("create {} {}", cmd.name, suffix)
            }
        }
        Some(TeamSubcommand::AddMember(cmd)) => {
            let mut rendered = format!(
                "add-member {} {}",
                shell_words::quote(&cmd.team),
                shell_words::quote(&cmd.role)
            );
            if cmd.can_delegate {
                rendered.push_str(" --can-delegate");
            }
            if let Some(depth) = cmd.max_delegation_depth {
                rendered.push_str(&format!(" --max-delegation-depth {depth}"));
            }
            if let Some(model) = cmd.model.as_deref() {
                rendered.push_str(&format!(" --model {}", shell_words::quote(model)));
            }
            if !cmd.description.is_empty() {
                rendered.push_str(" -- ");
                rendered.push_str(
                    &cmd.description
                        .iter()
                        .map(|word| shell_words::quote(word).into_owned())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
            rendered
        }
        Some(TeamSubcommand::Info(cmd)) => format!("info {}", cmd.name),
        Some(TeamSubcommand::Delete(cmd)) => format!("delete {}", cmd.name),
        Some(TeamSubcommand::Context(cmd)) => {
            format!(
                "context {} {} {}",
                cmd.team,
                cmd.key,
                join_words(&cmd.value)
            )
        }
        Some(TeamSubcommand::Run(cmd)) => {
            let lead = cmd
                .lead_agent_id
                .as_deref()
                .map(|id| format!(" --lead-agent-id {}", shell_words::quote(id)))
                .unwrap_or_default();
            let json = if cmd.json { " --json" } else { "" };
            let stream_events = cmd
                .stream_events
                .as_deref()
                .map(|path| {
                    let path = path.display().to_string();
                    format!(" --stream-events {}", shell_words::quote(&path))
                })
                .unwrap_or_default();
            format!(
                "run {}{}{}{} -- {}",
                shell_words::quote(&cmd.team),
                lead,
                json,
                stream_events,
                cmd.task
                    .iter()
                    .map(|word| shell_words::quote(word))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        Some(TeamSubcommand::Snapshot(cmd)) => {
            let suffix = join_words(&cmd.label);
            if suffix.is_empty() {
                format!("snapshot {}", cmd.team)
            } else {
                format!("snapshot {} {}", cmd.team, suffix)
            }
        }
        Some(TeamSubcommand::Restore(cmd)) => format!("restore {} {}", cmd.team, cmd.snapshot_id),
    }
}

/// Render [`MemoryArgs`] back into a stable textual argument list.
pub(crate) fn render_memory_args(args: &MemoryArgs) -> String {
    match &args.command {
        None => String::new(),
        Some(MemorySubcommand::List(cmd)) => {
            let mut parts = Vec::new();
            if let Some(ty) = &cmd.memory_type {
                parts.push(format!("--type {ty}"));
            }
            if cmd.limit != 20 {
                parts.push(format!("--limit {}", cmd.limit));
            }
            if parts.is_empty() {
                String::new()
            } else {
                parts.join(" ")
            }
        }
        Some(MemorySubcommand::Search(cmd)) => format!("search {}", join_words(&cmd.query)),
        Some(MemorySubcommand::Show(cmd)) => format!("show {}", cmd.memory_id),
        Some(MemorySubcommand::Forget(cmd)) => {
            if let Some(reason) = &cmd.reason {
                format!("forget {} --reason {}", cmd.memory_id, reason)
            } else {
                format!("forget {}", cmd.memory_id)
            }
        }
    }
}

/// Render [`ReviewArgs`] back into a stable textual argument list.
pub(crate) fn render_review_args(args: &ReviewArgs) -> String {
    match &args.command {
        Some(ReviewSubcommand::Head) => String::new(),
        Some(ReviewSubcommand::Working) => "working".to_string(),
        Some(ReviewSubcommand::Rev(cmd)) => join_words(&cmd.target),
        None => join_words(&args.target),
    }
}

/// Render [`GrepArgs`] back into a stable textual argument list.
pub(crate) fn render_grep_args(args: &GrepArgs) -> String {
    match &args.command {
        Some(GrepSubcommand::Content(cmd)) => join_words(&cmd.pattern),
        Some(GrepSubcommand::Files(cmd)) => format!("files {}", join_words(&cmd.pattern)),
        Some(GrepSubcommand::Review(cmd)) => format!("review {}", join_words(&cmd.pattern)),
        None => join_words(&args.pattern),
    }
}

/// Render [`PermissionsArgs`] back into a stable textual argument list.
pub(crate) fn render_permissions_args(args: &PermissionsArgs) -> String {
    match &args.command {
        None => String::new(),
        Some(PermissionsSubcommand::Auto) => "auto".to_string(),
        Some(PermissionsSubcommand::Bypass) => "bypass".to_string(),
        Some(PermissionsSubcommand::AcceptEdits) => "accept_edits".to_string(),
        Some(PermissionsSubcommand::Plan) => "plan".to_string(),
        Some(PermissionsSubcommand::Prompt) => "prompt".to_string(),
        Some(PermissionsSubcommand::Deny) => "deny".to_string(),
        Some(PermissionsSubcommand::Rules) => "rules".to_string(),
        Some(PermissionsSubcommand::Trust) => "trust".to_string(),
        Some(PermissionsSubcommand::Untrust) => "untrust".to_string(),
        Some(PermissionsSubcommand::Trace(cmd)) => match &cmd.export {
            Some(path) => format!("trace --export {}", path.display()),
            None => "trace".to_string(),
        },
    }
}

/// Render [`DebugArgs`] back into a stable textual argument list.
pub(crate) fn render_debug_args(args: &DebugArgs) -> String {
    args.session_id.clone().unwrap_or_default()
}

/// Render [`AgentArgs`] back into a stable textual argument list.
pub(crate) fn render_agent_args(args: &AgentArgs) -> String {
    match &args.command {
        None | Some(AgentSubcommand::List) => String::new(),
        Some(AgentSubcommand::Status(cmd)) => format!("status {}", cmd.agent_id),
        Some(AgentSubcommand::Stop(cmd)) => format!("stop {}", cmd.agent_id),
        Some(AgentSubcommand::Logs(cmd)) => format!("logs {}", cmd.agent_id),
    }
}

/// Render [`MessagingArgs`] back into a stable textual argument list.
pub(crate) fn render_messaging_args(_args: &MessagingArgs) -> String {
    String::new()
}

/// Render [`DiffArgs`] back into a stable textual argument list.
pub(crate) fn render_diff_args(args: &DiffArgs) -> String {
    match &args.command {
        None => join_words(&args.paths),
        Some(DiffSubcommand::Staged(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                "staged".to_string()
            } else {
                format!("staged {suffix}")
            }
        }
        Some(DiffSubcommand::Unstaged(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                "unstaged".to_string()
            } else {
                format!("unstaged {suffix}")
            }
        }
        Some(DiffSubcommand::Stat(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                "stat".to_string()
            } else {
                format!("stat {suffix}")
            }
        }
        Some(DiffSubcommand::Show(cmd)) => {
            let suffix = join_words(&cmd.paths);
            if suffix.is_empty() {
                format!("show {}", cmd.rev)
            } else {
                format!("show {} {}", cmd.rev, suffix)
            }
        }
    }
}

/// Render [`BugArgs`] back into a stable textual argument list.
pub(crate) fn render_bug_args(args: &BugArgs) -> String {
    match &args.command {
        None | Some(BugSubcommand::Print) => String::new(),
        Some(BugSubcommand::Copy) => "copy".to_string(),
        Some(BugSubcommand::Save) => "save".to_string(),
    }
}

#[cfg(test)]
mod arg_render_tests {
    use super::render_permissions_args;
    use crate::cli::cli_config::cli_args::{
        PermissionsArgs, PermissionsSubcommand, PermissionsTraceArgs,
    };

    #[test]
    fn team_member_rendering_preserves_explicit_controls_and_literal_description() {
        use crate::cli::cli_config::cli_args::{Cli, Command, TeamSubcommand};
        use clap::Parser;
        for delegate in [false, true] {
            let mut argv = vec!["astra", "team", "add-member", "team", "lead"];
            if delegate {
                argv.extend([
                    "--can-delegate",
                    "--max-delegation-depth",
                    "3",
                    "--model",
                    "flash",
                ]);
            }
            argv.extend(["--", "--can-delegate", "literal description"]);
            let Some(Command::Team(args)) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("Team command")
            };
            let rendered = super::render_team_args(&args);
            let Command::Team(reparsed) =
                crate::cli::command_router::parse_team_bridge_command(&rendered).unwrap()
            else {
                panic!("Team command")
            };
            let Some(TeamSubcommand::AddMember(member)) = reparsed.command else {
                panic!("AddMember command")
            };
            assert_eq!(member.can_delegate, delegate);
            assert_eq!(member.max_delegation_depth, delegate.then_some(3));
            assert_eq!(member.model.as_deref(), delegate.then_some("flash"));
            assert_eq!(
                member.description,
                ["--can-delegate", "literal description"]
            );
        }
        assert!(
            Cli::try_parse_from([
                "astra",
                "team",
                "add-member",
                "team",
                "lead",
                "--max-delegation-depth",
                "0"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "astra",
                "team",
                "add-member",
                "team",
                "lead",
                "--max-delegation-depth",
                "3"
            ])
            .is_err()
        );
    }

    #[test]
    fn team_run_rendering_roundtrips_capture_controls() {
        use crate::cli::cli_config::cli_args::{Cli, Command, TeamSubcommand};
        use clap::Parser;
        use std::path::Path;

        let Some(Command::Team(args)) = Cli::try_parse_from([
            "astra",
            "team",
            "run",
            "dev",
            "--lead-agent-id",
            "lead",
            "--json",
            "--stream-events",
            "events with spaces.jsonl",
            "--",
            "--json",
            "task's quoted text",
        ])
        .unwrap()
        .command
        else {
            panic!("Team command")
        };
        let rendered = super::render_team_args(&args);
        let Command::Team(reparsed) =
            crate::cli::command_router::parse_team_bridge_command(&rendered).unwrap()
        else {
            panic!("Team command")
        };
        let Some(TeamSubcommand::Run(run)) = reparsed.command else {
            panic!("Run command")
        };
        assert_eq!(run.team, "dev");
        assert_eq!(run.lead_agent_id.as_deref(), Some("lead"));
        assert!(run.json);
        assert_eq!(
            run.stream_events.as_deref(),
            Some(Path::new("events with spaces.jsonl"))
        );
        assert_eq!(run.task, ["--json", "task's quoted text"]);
    }

    #[test]
    fn bare_permissions_command_renders_empty_arg_for_explicit_selection() {
        let args = PermissionsArgs { command: None };
        assert_eq!(render_permissions_args(&args), "");
    }

    #[test]
    fn permissions_trace_renders_trace_arg() {
        let args = PermissionsArgs {
            command: Some(PermissionsSubcommand::Trace(PermissionsTraceArgs {
                export: None,
            })),
        };
        assert_eq!(render_permissions_args(&args), "trace");
    }

    #[test]
    fn permissions_trust_commands_render_args() {
        let trust = PermissionsArgs {
            command: Some(PermissionsSubcommand::Trust),
        };
        assert_eq!(render_permissions_args(&trust), "trust");

        let untrust = PermissionsArgs {
            command: Some(PermissionsSubcommand::Untrust),
        };
        assert_eq!(render_permissions_args(&untrust), "untrust");
    }

    #[test]
    fn permissions_bypass_renders_mode_arg() {
        let args = PermissionsArgs {
            command: Some(PermissionsSubcommand::Bypass),
        };
        assert_eq!(render_permissions_args(&args), "bypass");
    }

    #[test]
    fn permissions_trace_export_renders_path_arg() {
        let args = PermissionsArgs {
            command: Some(PermissionsSubcommand::Trace(PermissionsTraceArgs {
                export: Some(std::path::PathBuf::from("trace.jsonl")),
            })),
        };
        assert_eq!(render_permissions_args(&args), "trace --export trace.jsonl");
    }
}
