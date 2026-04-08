//! `/agent` slash command — manage dynamically spawned agents.
//!
//! Subcommands:
//! - `/agent` or `/agent list`: List all active spawned agents
//! - `/agent status <id>`: Show detailed status of an agent
//! - `/agent stop <id>`: Send shutdown request to an agent
//! - `/agent logs <id>`: Show recent progress events from an agent
//! - `/agent help`: Show help

use super::*;
use astra_runtime::orchestration::{
    AgentStatus, DynamicAgentSpawner,
};
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
        let elapsed = agent.started_at.elapsed()
            .map(|d| format_duration(d))
            .unwrap_or_else(|_| "?".to_string());

        eprintln!(
            "  {} {} {} ({})",
            status_icon(&agent.status),
            agent.agent_id.as_str().white().bold(),
            format!("[{}]", agent.agent_type).dim(),
            elapsed.dim()
        );
        eprintln!("    {}", agent.description.as_str().cyan());
        if agent.metrics.tool_calls > 0 {
            eprintln!(
                "    {} tools: {}, turns: {}",
                "📊".dim(),
                agent.metrics.tool_calls.to_string().green(),
                agent.metrics.turns_completed.to_string().green()
            );
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
            eprintln!("\n  {} {}", "🤖 Agent".cyan().bold(), state.agent_id.as_str().white().bold());
            eprintln!("  {}", "─".repeat(50).dim());
            eprintln!("  {} {}", "Type:".white().bold(), state.agent_type);
            eprintln!("  {} {}", "Description:".white().bold(), state.description.as_str().cyan());
            eprintln!("  {} {}", "Status:".white().bold(), format_status(&state.status));
            eprintln!("  {} {}", "Run ID:".white().bold(), state.run_id.as_str().dim());
            eprintln!("  {} {}", "Parent:".white().bold(), state.parent_run_id.as_str().dim());

            if let Some(ref addr) = state.messaging_address {
                eprintln!("  {} {}", "Address:".white().bold(), addr.to_string().green());
            }

            if let Some(ref path) = state.worktree_path {
                eprintln!("  {} {}", "Worktree:".white().bold(), path.display().to_string().dim());
            }

            let elapsed = state.started_at.elapsed()
                .map(|d| format_duration(d))
                .unwrap_or_else(|_| "?".to_string());
            eprintln!("  {} {}", "Running for:".white().bold(), elapsed);

            eprintln!("\n  {}", "📊 Metrics".cyan().bold());
            eprintln!("  {}", "─".repeat(30).dim());
            eprintln!("  {} {}", "Turns:".white().bold(), state.metrics.turns_completed);
            eprintln!("  {} {}", "Tool calls:".white().bold(), state.metrics.tool_calls);
            eprintln!(
                "  {} {} prompt, {} completion",
                "Tokens:".white().bold(),
                state.metrics.prompt_tokens,
                state.metrics.completion_tokens
            );
            eprintln!();
        }
        None => {
            eprintln!("  {}", format!("Agent not found: {agent_id}").yellow());
        }
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

    // Subscribe to progress events (for future streaming)
    let _rx = spawner.subscribe_progress();

    eprintln!(
        "\n  {} Showing logs for {} (Ctrl+C to stop)\n",
        "📋".cyan(),
        agent_id.white().bold()
    );

    // For now, just show that we're listening
    // In a full implementation, this would poll for events
    eprintln!(
        "  {}",
        format!("Waiting for events from {agent_id}...").dim()
    );
    eprintln!(
        "  {}",
        "Note: Live log streaming requires active agent execution.".dim()
    );
    eprintln!();
}

fn show_help() {
    eprintln!("\n  {}", "🤖 Agent Commands".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());
    eprintln!("  {}  List all active agents", "/agent".white().bold());
    eprintln!("  {}  List all active agents", "/agent list".white().bold());
    eprintln!("  {}  Show agent status", "/agent status <id>".white().bold());
    eprintln!("  {}  Stop an agent", "/agent stop <id>".white().bold());
    eprintln!("  {}  Show agent logs", "/agent logs <id>".white().bold());
    eprintln!("  {}  Show this help", "/agent help".white().bold());
    eprintln!();
    eprintln!(
        "  {}",
        "Agents are created via the spawn_agent tool during chat.".dim()
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
        assert_eq!(format_duration(std::time::Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(std::time::Duration::from_secs(3700)), "1h 1m");
    }

    #[test]
    fn test_format_status() {
        let status = AgentStatus::Running { activity: "reading files".to_string() };
        let formatted = format_status(&status);
        assert!(formatted.to_string().contains("running"));
    }
}
