//! Render slash/CLI subcommand structs back into stable textual argument lists.
//!
//! These helpers exist so features like plan replay, delegation, and audit can
//! reconstruct the operator-facing command line from parsed argument structs
//! without duplicating formatting logic at each call site.

use crate::cli::cli_config::cli_args::{
    AgentArgs, AgentSubcommand, BugArgs, BugSubcommand, DebugArgs, DiffArgs, DiffSubcommand,
    GrepArgs, GrepSubcommand, MemoryArgs, MemorySubcommand, MessagingArgs, MessagingSubcommand,
    PermissionsArgs, PermissionsSubcommand, ReviewArgs, ReviewSubcommand, TaskArgs, TaskSubcommand,
    TeamArgs, TeamSubcommand,
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
            let suffix = join_words(&cmd.description);
            if suffix.is_empty() {
                format!("add-member {} {}", cmd.team, cmd.role)
            } else {
                format!("add-member {} {} {}", cmd.team, cmd.role, suffix)
            }
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
        Some(TeamSubcommand::Run(cmd)) => format!("run {} {}", cmd.team, join_words(&cmd.task)),
        Some(TeamSubcommand::History(cmd)) => format!("history {}", cmd.name),
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

/// Render [`TaskArgs`] back into a stable textual argument list.
pub(crate) fn render_task_args(args: &TaskArgs) -> String {
    match &args.command {
        None | Some(TaskSubcommand::List) => String::new(),
        Some(TaskSubcommand::Pending) => "pending".to_string(),
        Some(TaskSubcommand::Status(cmd)) => format!("status {}", join_words(&cmd.query)),
        Some(TaskSubcommand::Run(cmd)) => format!("run {}", join_words(&cmd.text)),
        Some(TaskSubcommand::Queue(cmd)) => format!("queue {}", join_words(&cmd.text)),
        Some(TaskSubcommand::Worker(_)) => "worker".to_string(),
        Some(TaskSubcommand::Result(cmd)) => format!("result {}", join_words(&cmd.query)),
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
        Some(PermissionsSubcommand::Status) => "status".to_string(),
        Some(PermissionsSubcommand::Auto) => "auto".to_string(),
        Some(PermissionsSubcommand::AcceptEdits) => "accept_edits".to_string(),
        Some(PermissionsSubcommand::Plan) => "plan".to_string(),
        Some(PermissionsSubcommand::Prompt) => "prompt".to_string(),
        Some(PermissionsSubcommand::Deny) => "deny".to_string(),
        Some(PermissionsSubcommand::All) => "all".to_string(),
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
pub(crate) fn render_messaging_args(args: &MessagingArgs) -> String {
    match &args.command {
        None | Some(MessagingSubcommand::Metrics) => String::new(),
        Some(MessagingSubcommand::Dlq) => "dlq".to_string(),
        Some(MessagingSubcommand::Status) => "status".to_string(),
    }
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
    use super::{render_permissions_args, render_task_args};
    use crate::cli::cli_config::cli_args::{
        PermissionsArgs, PermissionsSubcommand, PermissionsTraceArgs, TaskArgs, TaskSubcommand,
    };

    #[test]
    fn bare_permissions_command_renders_empty_arg_for_mode_cycle() {
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
    fn permissions_trace_export_renders_path_arg() {
        let args = PermissionsArgs {
            command: Some(PermissionsSubcommand::Trace(PermissionsTraceArgs {
                export: Some(std::path::PathBuf::from("trace.jsonl")),
            })),
        };
        assert_eq!(render_permissions_args(&args), "trace --export trace.jsonl");
    }

    #[test]
    fn task_pending_renders_explicit_queue_view() {
        let args = TaskArgs {
            command: Some(TaskSubcommand::Pending),
        };
        assert_eq!(render_task_args(&args), "pending");
    }
}
