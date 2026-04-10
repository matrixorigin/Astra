//! `/agent` slash command — manage dynamically spawned agents.
//!
//! Subcommands:
//! - `/agent` or `/agent list`: List all active spawned agents
//! - `/agent status <id>`: Show detailed status of an agent
//! - `/agent permissions <id>`: Show permission details of an agent
//! - `/agent stop <id>`: Send shutdown request to an agent
//! - `/agent logs <id>`: Show recent progress events from an agent
//! - `/agent help`: Show help

use super::*;
use astra_runtime::orchestration::{AgentStatus, DynamicAgentSpawner, PermissionSummary};
use std::sync::Arc;

/// Agent command context — passed from main.
pub struct AgentCommandContext {
    pub spawner: Option<Arc<DynamicAgentSpawner>>,
}

/// Handle `/agent [subcommand]` command.
pub fn handle_agent_command(arg: &str, ctx: &AgentCommandContext) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = parts.first().copied().unwrap_or("list");

    match subcmd {
        "" | "list" => show_list(ctx),
        "status" => {
            if let Some(id) = parts.get(1) {
                show_status(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /agent status <agent_id>".yellow());
            }
        }
        "permissions" | "perms" => {
            if let Some(id) = parts.get(1) {
                show_permissions(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /agent permissions <agent_id>".yellow());
            }
        }
        "stop" => {
            if let Some(id) = parts.get(1) {
                stop_agent(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /agent stop <agent_id>".yellow());
            }
        }
        "logs" => {
            if let Some(id) = parts.get(1) {
                show_logs(ctx, id);
            } else {
                eprintln!("  {}", "Usage: /agent logs <agent_id>".yellow());
            }
        }
        "help" | "?" => show_help(),
        _ => {
            eprintln!(
                "  {}",
                format!("Unknown subcommand: {subcmd}. Try /agent help").yellow()
            );
        }
    }
}

fn show_list(ctx: &AgentCommandContext) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!(
            "  {}",
            "No agent spawner available. Use spawn_agent tool to create agents.".dim()
        );
        return;
    };

    let rt = match tokio::runtime::Handle::try_current() {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("  {}", "No tokio runtime available.".red());
            return;
        }
    };

    let agents = rt.block_on(spawner.list_all_agents());

    if agents.is_empty() {
        eprintln!("\n  {}", "🤖 No active agents".cyan().bold());
        eprintln!("  {}", "Use spawn_agent tool to create sub-agents.".dim());
        eprintln!();
        return;
    }

    eprintln!("\n  {}", "🤖 Active Agents".cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());

    for agent in &agents {
        let _status_str = format_status(&agent.status);
        let elapsed = agent
            .started_at
            .elapsed()
            .map(|d| format_duration(d))
            .unwrap_or_else(|_| "?".to_string());

        // Permission indicator
        let perm_icon = if agent.has_permission_issues {
            "🔒"
        } else {
            ""
        };

        eprintln!(
            "  {} {} {} ({}){}",
            status_icon(&agent.status),
            agent.agent_id.as_str().white().bold(),
            format!("[{}]", agent.agent_type).dim(),
            elapsed.dim(),
            if agent.has_permission_issues {
                format!(" {}", perm_icon.red())
            } else {
                String::new()
            }
        );
        eprintln!("    {}", agent.description.as_str().cyan());
        if agent.metrics.tool_calls > 0 || agent.metrics.tools_blocked > 0 {
            let mut metrics_parts = vec![];
            if agent.metrics.tool_calls > 0 {
                metrics_parts.push(format!(
                    "tools: {}",
                    agent.metrics.tool_calls.to_string().green()
                ));
            }
            if agent.metrics.turns_completed > 0 {
                metrics_parts.push(format!(
                    "turns: {}",
                    agent.metrics.turns_completed.to_string().green()
                ));
            }
            if agent.metrics.tools_blocked > 0 {
                metrics_parts.push(format!(
                    "blocked: {}",
                    agent.metrics.tools_blocked.to_string().red()
                ));
            }
            eprintln!("    {} {}", "📊".dim(), metrics_parts.join(", "));
        }
    }
    eprintln!();
}

fn show_status(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };

    let rt = match tokio::runtime::Handle::try_current() {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("  {}", "No tokio runtime available.".red());
            return;
        }
    };

    match rt.block_on(spawner.get_agent_state(agent_id)) {
        Some(state) => {
            eprintln!(
                "\n  {} {}",
                "🤖 Agent".cyan().bold(),
                state.agent_id.as_str().white().bold()
            );
            eprintln!("  {}", "─".repeat(50).dim());
            eprintln!("  {} {}", "Type:".white().bold(), state.agent_type);
            eprintln!(
                "  {} {}",
                "Description:".white().bold(),
                state.description.as_str().cyan()
            );
            eprintln!(
                "  {} {}",
                "Status:".white().bold(),
                format_status(&state.status)
            );
            eprintln!(
                "  {} {}",
                "Run ID:".white().bold(),
                state.run_id.as_str().dim()
            );
            eprintln!(
                "  {} {}",
                "Parent:".white().bold(),
                state.parent_run_id.as_str().dim()
            );

            if let Some(ref addr) = state.messaging_address {
                eprintln!(
                    "  {} {}",
                    "Address:".white().bold(),
                    addr.to_string().green()
                );
            }

            if let Some(ref path) = state.worktree_path {
                eprintln!(
                    "  {} {}",
                    "Worktree:".white().bold(),
                    path.display().to_string().dim()
                );
            }

            let elapsed = state
                .started_at
                .elapsed()
                .map(|d| format_duration(d))
                .unwrap_or_else(|_| "?".to_string());
            eprintln!("  {} {}", "Running for:".white().bold(), elapsed);

            eprintln!("\n  {}", "📊 Metrics".cyan().bold());
            eprintln!("  {}", "─".repeat(30).dim());
            eprintln!(
                "  {} {}",
                "Turns:".white().bold(),
                state.metrics.turns_completed
            );
            eprintln!(
                "  {} {}",
                "Tool calls:".white().bold(),
                state.metrics.tool_calls
            );
            eprintln!(
                "  {} {} prompt, {} completion",
                "Tokens:".white().bold(),
                state.metrics.prompt_tokens,
                state.metrics.completion_tokens
            );

            // Permission summary in status
            eprintln!("\n  {}", "🔐 Permissions".cyan().bold());
            eprintln!("  {}", "─".repeat(30).dim());
            print_permission_summary(&state.permission_summary, &state.metrics);

            eprintln!();
        }
        None => {
            eprintln!("  {}", format!("Agent not found: {agent_id}").yellow());
        }
    }
}

fn show_permissions(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };

    let rt = match tokio::runtime::Handle::try_current() {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("  {}", "No tokio runtime available.".red());
            return;
        }
    };

    match rt.block_on(spawner.get_agent_state(agent_id)) {
        Some(state) => {
            eprintln!(
                "\n  {} {} {}",
                "🔐 Permissions for".cyan().bold(),
                state.agent_id.as_str().white().bold(),
                format!("[{}]", state.agent_type).dim()
            );
            eprintln!("  {}", "─".repeat(50).dim());

            print_permission_summary(&state.permission_summary, &state.metrics);

            // Permission request stats
            if state.metrics.permission_requests > 0 {
                eprintln!("\n  {}", "📮 Permission Requests".white().bold());
                eprintln!(
                    "    Sent: {}, Approved: {}, Denied: {}",
                    state.metrics.permission_requests.to_string().cyan(),
                    state
                        .metrics
                        .permission_requests_approved
                        .to_string()
                        .green(),
                    (state.metrics.permission_requests
                        - state.metrics.permission_requests_approved)
                        .to_string()
                        .red()
                );
            }

            // Recent denials
            if !state.permission_summary.recent_denials.is_empty() {
                eprintln!("\n  {}", "🚫 Recent Denials".white().bold());
                for tool in &state.permission_summary.recent_denials {
                    eprintln!("    {} {}", "•".red(), tool);
                }
            }

            eprintln!();
        }
        None => {
            eprintln!("  {}", format!("Agent not found: {agent_id}").yellow());
        }
    }
}

fn print_permission_summary(
    summary: &PermissionSummary,
    metrics: &astra_runtime::orchestration::SpawnedAgentMetrics,
) {
    let mode_styled = match summary.mode.as_str() {
        "auto" => "auto".green(),
        "prompt" => "prompt".yellow(),
        "deny" => "deny".red(),
        _ => summary.mode.as_str().dim(),
    };
    eprintln!("  {} {}", "Mode:".white().bold(), mode_styled);
    eprintln!(
        "  {} {} allow, {} deny",
        "Rules:".white().bold(),
        summary.allow_rules.to_string().green(),
        summary.deny_rules.to_string().red()
    );
    eprintln!(
        "  {} {}",
        "Parent escalation:".white().bold(),
        if summary.has_parent {
            "enabled".green()
        } else {
            "disabled".dim()
        }
    );
    if metrics.tools_blocked > 0 {
        eprintln!(
            "  {} {}",
            "Tools blocked:".white().bold(),
            metrics.tools_blocked.to_string().red()
        );
    }
}

fn stop_agent(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };

    let rt = match tokio::runtime::Handle::try_current() {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("  {}", "No tokio runtime available.".red());
            return;
        }
    };

    // First check if agent exists
    let agent = rt.block_on(spawner.get_agent_state(agent_id));
    if agent.is_none() {
        eprintln!("  {}", format!("Agent not found: {agent_id}").yellow());
        return;
    }

    // Update status to cancelled
    rt.block_on(spawner.update_status(agent_id, AgentStatus::Cancelled));

    eprintln!(
        "  {} Shutdown request sent to {}",
        "✓".green(),
        agent_id.white().bold()
    );
}

fn show_logs(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };

    // Check if agent exists
    let rt = match tokio::runtime::Handle::try_current() {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("  {}", "No tokio runtime available.".red());
            return;
        }
    };

    let agent = rt.block_on(spawner.get_agent_state(agent_id));
    if agent.is_none() {
        eprintln!("  {}", format!("Agent not found: {agent_id}").yellow());
        return;
    }

    eprintln!(
        "\n  {} Streaming logs for {} (Ctrl+C to stop)\n",
        "📋".cyan(),
        agent_id.white().bold()
    );

    // Subscribe to progress events
    let mut rx = spawner.subscribe_progress();
    let target_agent_id = agent_id.to_string();

    // Block and stream events
    rt.block_on(async {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if event.agent_id == target_agent_id {
                        print_progress_event(&event);

                        // Stop on terminal events
                        if matches!(
                            event.event_type,
                            astra_runtime::orchestration::ProgressEventType::Completed { .. }
                                | astra_runtime::orchestration::ProgressEventType::Failed { .. }
                                | astra_runtime::orchestration::ProgressEventType::Cancelled { .. }
                        ) {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("  {}", format!("(skipped {n} events)").dim());
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    eprintln!("  {}", "Event stream closed.".dim());
                    break;
                }
            }
        }
    });

    eprintln!();
}

fn print_progress_event(event: &astra_runtime::orchestration::AgentProgressEvent) {
    use astra_runtime::orchestration::ProgressEventType;
    use crossterm::style::Stylize;

    let timestamp =
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(event.timestamp_epoch_ms);
    let time_str = timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| {
            let secs = d.as_secs();
            let mins = (secs / 60) % 60;
            let hours = (secs / 3600) % 24;
            format!("{:02}:{:02}:{:02}", hours, mins, secs % 60)
        })
        .unwrap_or_else(|_| "??:??:??".to_string());

    let msg = match &event.event_type {
        ProgressEventType::Started { description } => {
            format!("{} {}", "▶ Started:".green(), description)
        }
        ProgressEventType::TurnCompleted {
            turn,
            tool_calls_this_turn,
            activity,
        } => {
            format!(
                "{} Turn {} ({} tools): {}",
                "◆".cyan(),
                turn.to_string().as_str().cyan(),
                tool_calls_this_turn,
                activity
            )
        }
        ProgressEventType::Idle => format!("{}", "⏸ Idle".blue()),
        ProgressEventType::Busy { activity } => format!("⚡ Busy: {}", activity),
        ProgressEventType::Completed {
            result_summary,
            total_tool_calls,
            duration_ms,
            ..
        } => {
            format!(
                "{} ({} tools, {}ms): {}",
                "✓ Completed".green(),
                total_tool_calls,
                duration_ms,
                result_summary
            )
        }
        ProgressEventType::Failed { error } => format!("{} {}", "✗ Failed:".red(), error),
        ProgressEventType::Cancelled { reason } => {
            format!("{} {}", "⊘ Cancelled:".yellow(), reason)
        }
        ProgressEventType::PermissionDenied {
            tool_name,
            reason,
            turn,
        } => {
            format!(
                "{} tool={} turn={} — {}",
                "🔒 Permission denied:".red(),
                tool_name,
                turn,
                reason
            )
        }
        ProgressEventType::ToolExecuting { tool_name, turn } => {
            format!("🔧 turn={} → {}", turn, tool_name.as_str().cyan())
        }
        ProgressEventType::LlmCallStarted { turn } => {
            format!("🧠 turn={} {}", turn, "thinking…".dim())
        }
        ProgressEventType::LlmCallCompleted {
            turn,
            ttft_ms,
            duration_ms,
        } => {
            let ttft = ttft_ms
                .map(|t| format!(" ttft={}ms", t))
                .unwrap_or_default();
            format!("🧠 turn={} done {}ms{}", turn, duration_ms, ttft)
        }
    };

    eprintln!("  [{}] {}", time_str.as_str().dim(), msg);
}

fn show_help() {
    eprintln!("\n  {}", "🤖 Agent Commands".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());
    eprintln!("  {}  List all active agents", "/agent".white().bold());
    eprintln!("  {}  List all active agents", "/agent list".white().bold());
    eprintln!(
        "  {}  Show agent status",
        "/agent status <id>".white().bold()
    );
    eprintln!(
        "  {}  Show permission details",
        "/agent permissions <id>".white().bold()
    );
    eprintln!("  {}  Stop an agent", "/agent stop <id>".white().bold());
    eprintln!("  {}  Show agent logs", "/agent logs <id>".white().bold());
    eprintln!("  {}  Show this help", "/agent help".white().bold());
    eprintln!();
    eprintln!(
        "  {}",
        "Agents are created via the spawn_agent tool during chat.".dim()
    );
    eprintln!(
        "  {}",
        "Permission issues are indicated by 🔒 in the agent list.".dim()
    );
    eprintln!();
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn status_icon(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Initializing => "⏳",
        AgentStatus::Running { .. } => "🔄",
        AgentStatus::Idle => "💤",
        AgentStatus::Completed { .. } => "✅",
        AgentStatus::Failed { .. } => "❌",
        AgentStatus::Cancelled => "🛑",
    }
}

fn format_status(status: &AgentStatus) -> String {
    match status {
        AgentStatus::Initializing => "initializing".to_string(),
        AgentStatus::Running { activity } => format!("running: {activity}"),
        AgentStatus::Idle => "idle".to_string(),
        AgentStatus::Completed { result } => {
            let preview = if result.len() > 50 {
                format!("{}...", &result[..50])
            } else {
                result.clone()
            };
            format!("completed: {preview}")
        }
        AgentStatus::Failed { error } => format!("failed: {error}"),
        AgentStatus::Cancelled => "cancelled".to_string(),
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(std::time::Duration::from_secs(30)), "30s");
        assert_eq!(
            format_duration(std::time::Duration::from_secs(90)),
            "1m 30s"
        );
        assert_eq!(
            format_duration(std::time::Duration::from_secs(3700)),
            "1h 1m"
        );
    }

    #[test]
    fn test_format_status() {
        let status = AgentStatus::Running {
            activity: "reading files".to_string(),
        };
        let formatted = format_status(&status);
        assert!(formatted.to_string().contains("running"));
    }
}
