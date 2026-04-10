//! `/agent` slash command — inspect spawned agents and recent delegations.
//!
//! Subcommands:
//! - `/agent` or `/agent list`: List active/recent spawned agents and delegations
//! - `/agent tree`: Show agent delegation tree (parent-child hierarchy)
//! - `/agent watch`: Watch tree with real-time updates on spawn/complete
//! - `/agent status <id>`: Show detailed status of an agent or delegation
//! - `/agent permissions <id>`: Show permission details of an agent
//! - `/agent stop <id>`: Send shutdown request to an agent
//! - `/agent logs <id>`: Show recent progress events from an agent
//! - `/agent help`: Show help

use super::*;
use astra_runtime::orchestration::{AgentStatus, DynamicAgentSpawner, PermissionSummary};
use astra_runtime::turn::delegation_tree::{AgentTreeNode, render_agent_forest};
use astra_services::session_journal::{self, JournalEventType};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;

/// Agent command context — passed from main.
pub struct AgentCommandContext {
    pub spawner: Option<Arc<DynamicAgentSpawner>>,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DelegationSubRunSummary {
    sub_run_id: String,
    agent_id: String,
    status: String,
    error: Option<String>,
    output_preview: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DelegationHistoryEntry {
    delegation_id: String,
    pattern: String,
    total_sub_runs: usize,
    succeeded: usize,
    failed: usize,
    status: String,
    aggregated_output_preview: Option<String>,
    sub_runs: Vec<DelegationSubRunSummary>,
    last_seen_index: usize,
}

/// Handle `/agent [subcommand]` command.
pub async fn handle_agent_command(arg: &str, ctx: &AgentCommandContext) {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let subcmd = parts.first().copied().unwrap_or("list");

    match subcmd {
        "" | "list" => show_list(ctx).await,
        "history" => show_history(ctx),
        "tree" => show_tree(ctx).await,
        "status" => {
            if let Some(id) = parts.get(1) {
                show_status(ctx, id).await;
            } else {
                eprintln!("  {}", "Usage: /agent status <agent_id>".yellow());
            }
        }
        "permissions" | "perms" => {
            if let Some(id) = parts.get(1) {
                show_permissions(ctx, id).await;
            } else {
                eprintln!("  {}", "Usage: /agent permissions <agent_id>".yellow());
            }
        }
        "stop" => {
            if let Some(id) = parts.get(1) {
                stop_agent(ctx, id).await;
            } else {
                eprintln!("  {}", "Usage: /agent stop <agent_id>".yellow());
            }
        }
        "logs" => {
            if let Some(id) = parts.get(1) {
                show_logs(ctx, id).await;
            } else {
                eprintln!(
                    "  {}",
                    "Usage: /agent logs <agent_id|delegation_id>".yellow()
                );
            }
        }
        "watch" => show_watch(ctx).await,
        "help" | "?" => show_help(),
        _ => {
            eprintln!(
                "  {}",
                format!("Unknown subcommand: {subcmd}. Try /agent help").yellow()
            );
        }
    }
}

fn show_history(ctx: &AgentCommandContext) {
    let delegations = load_recent_delegations(ctx.session_id.as_deref());
    if delegations.is_empty() {
        eprintln!(
            "\n  {}",
            "🤝 No delegation history in this session".cyan().bold()
        );
        eprintln!();
        return;
    }
    eprintln!("\n  {}", "🤝 Delegation History".cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    for entry in &delegations {
        eprintln!(
            "  {} {} {} [{} ok / {} failed]",
            sub_run_status_icon(&entry.status),
            entry.delegation_id.as_str().white().bold(),
            format!("[{}]", entry.pattern).dim(),
            entry.succeeded.to_string().green(),
            entry.failed.to_string().red()
        );
        if let Some(preview) = &entry.aggregated_output_preview {
            eprintln!("    {}", preview.as_str().cyan());
        }
        for sub_run in &entry.sub_runs {
            eprintln!(
                "    {} {} {}",
                sub_run_status_icon(&sub_run.status),
                sub_run.agent_id.as_str().white().bold(),
                format!("[{}]", sub_run.status).dim()
            );
            if let Some(preview) = &sub_run.output_preview {
                eprintln!("      {}", preview.as_str().dim());
            }
            if let Some(error) = &sub_run.error {
                eprintln!("      {}", error.as_str().red());
            }
        }
    }
    eprintln!();
}

async fn show_list(ctx: &AgentCommandContext) {
    let mut recent_agents = if let Some(ref spawner) = ctx.spawner {
        spawner.get_agent_history(None).await
    } else {
        Vec::new()
    };
    recent_agents.sort_by_key(|agent| Reverse(agent.started_at));
    let (active_agents, completed_agents): (Vec<_>, Vec<_>) = recent_agents
        .into_iter()
        .partition(|agent| !is_terminal_agent_status(&agent.status));
    let delegations = load_recent_delegations(ctx.session_id.as_deref());

    if active_agents.is_empty() && completed_agents.is_empty() && delegations.is_empty() {
        eprintln!("\n  {}", "🤖 No recent agents or delegations".cyan().bold());
        eprintln!(
            "  {}",
            "Use spawn_agent or delegate to start multi-agent work.".dim()
        );
        eprintln!();
        return;
    }

    if !active_agents.is_empty() {
        print_agent_section("🤖 Active Spawned Agents", &active_agents);
    }
    if !completed_agents.is_empty() {
        print_agent_section("🕘 Recent Spawned Agents", &completed_agents);
    }
    if !delegations.is_empty() {
        print_delegation_section(&delegations);
    }
    eprintln!();
}

async fn show_tree(ctx: &AgentCommandContext) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!(
            "  {}",
            "No agent spawner available. Use spawn_agent tool to create agents.".dim()
        );
        return;
    };

    let agents = spawner.list_all_agents().await;

    eprintln!("\n  {}", "🌲 Agent Delegation Tree".cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());

    let forest = AgentTreeNode::build_forest(&agents);
    let rendered = render_agent_forest(&forest);

    // Indent all output
    for line in rendered.lines() {
        eprintln!("  {}", line);
    }
}

async fn show_status(ctx: &AgentCommandContext, agent_id: &str) {
    if let Some(ref spawner) = ctx.spawner
        && let Some(state) = spawner.get_agent_state_any(agent_id).await
    {
        {
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
        return;
    }

    if let Some(entry) = find_delegation_entry(ctx.session_id.as_deref(), agent_id) {
        eprintln!(
            "\n  {} {}",
            "🤝 Delegation".cyan().bold(),
            entry.delegation_id.as_str().white().bold()
        );
        eprintln!("  {}", "─".repeat(50).dim());
        eprintln!("  {} {}", "Pattern:".white().bold(), entry.pattern);
        eprintln!("  {} {}", "Status:".white().bold(), entry.status.cyan());
        eprintln!("  {} {}", "Sub-runs:".white().bold(), entry.total_sub_runs);
        eprintln!(
            "  {} {} ok, {} failed",
            "Results:".white().bold(),
            entry.succeeded.to_string().green(),
            entry.failed.to_string().red()
        );
        if let Some(preview) = &entry.aggregated_output_preview {
            eprintln!(
                "  {} {}",
                "Summary:".white().bold(),
                preview.as_str().cyan()
            );
        }
        if !entry.sub_runs.is_empty() {
            eprintln!("\n  {}", "📦 Sub-runs".cyan().bold());
            eprintln!("  {}", "─".repeat(30).dim());
            for sub_run in &entry.sub_runs {
                eprintln!(
                    "  {} {} {}",
                    sub_run_status_icon(&sub_run.status),
                    sub_run.agent_id.as_str().white().bold(),
                    format!("[{}]", sub_run.status).dim()
                );
                eprintln!("    {}", sub_run.sub_run_id.as_str().dim());
                if let Some(preview) = &sub_run.output_preview {
                    eprintln!("    {}", preview.as_str().cyan());
                }
                if let Some(error) = &sub_run.error {
                    eprintln!("    {}", error.as_str().red());
                }
            }
        }
        eprintln!();
        return;
    }

    eprintln!(
        "  {}",
        format!("Agent or delegation not found: {agent_id}").yellow()
    );
}

async fn show_permissions(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };

    match spawner.get_agent_state_any(agent_id).await {
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

async fn stop_agent(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };

    // First check if agent exists
    let agent = spawner.get_agent_state_any(agent_id).await;
    if agent.is_none() {
        eprintln!("  {}", format!("Agent not found: {agent_id}").yellow());
        return;
    }

    // Update status to cancelled
    spawner
        .update_status(agent_id, AgentStatus::Cancelled)
        .await;

    eprintln!(
        "  {} Shutdown request sent to {}",
        "✓".green(),
        agent_id.white().bold()
    );
}

/// Watch agent tree with real-time updates on spawn/complete events.
/// Throttles rendering to max once per 500ms.
async fn show_watch(ctx: &AgentCommandContext) {
    use astra_runtime::orchestration::ProgressEventType;
    use std::time::{Duration, Instant};

    let Some(ref spawner) = ctx.spawner else {
        eprintln!(
            "  {}",
            "No agent spawner available. Use spawn_agent tool to create agents.".dim()
        );
        return;
    };

    eprintln!("\n  {} Watching agent tree (Ctrl+C to stop)\n", "👁".cyan());

    // Subscribe to progress events
    let mut rx = spawner.subscribe_progress();
    let spawner_clone = spawner.clone();

    let mut last_render = Instant::now() - Duration::from_secs(10);
    let mut last_agent_count = 0usize;
    let throttle_interval = Duration::from_millis(500);

    let agents = spawner_clone.list_all_agents().await;
    if !agents.is_empty() {
        let forest = AgentTreeNode::build_forest(&agents);
        let rendered = render_agent_forest(&forest);
        eprintln!("  {}", "🌲 Agent Delegation Tree".cyan().bold());
        eprintln!("  {}", "─".repeat(60).dim());
        for line in rendered.lines() {
            eprintln!("  {}", line);
        }
        last_agent_count = agents.len();
        last_render = Instant::now();
    } else {
        eprintln!("  {}", "No agents spawned yet. Waiting...".dim());
    }

    loop {
        match rx.recv().await {
            Ok(event) => {
                let is_tree_event = matches!(
                    event.event_type,
                    ProgressEventType::AgentSpawned { .. }
                        | ProgressEventType::Completed { .. }
                        | ProgressEventType::Failed { .. }
                        | ProgressEventType::Cancelled { .. }
                );

                if is_tree_event && last_render.elapsed() >= throttle_interval {
                    let agents = spawner_clone.list_all_agents().await;
                    if agents.len() != last_agent_count || agents.is_empty() {
                        eprintln!("\n  {}", "🌲 Agent Delegation Tree".cyan().bold());
                        eprintln!("  {}", "─".repeat(60).dim());

                        if agents.is_empty() {
                            eprintln!("  {}", "(no agents)".dim());
                        } else {
                            let forest = AgentTreeNode::build_forest(&agents);
                            let rendered = render_agent_forest(&forest);
                            for line in rendered.lines() {
                                eprintln!("  {}", line);
                            }
                        }

                        last_agent_count = agents.len();
                        last_render = Instant::now();
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

    eprintln!();
}

async fn show_logs(ctx: &AgentCommandContext, agent_id: &str) {
    let Some(ref spawner) = ctx.spawner else {
        if show_delegation_logs(ctx.session_id.as_deref(), agent_id) {
            return;
        }
        eprintln!("  {}", "No agent spawner available.".dim());
        return;
    };
    let agent = if let Some(ref spawner) = ctx.spawner {
        spawner.get_agent_state_any(agent_id).await
    } else {
        None
    };
    if agent.is_none() {
        if show_delegation_logs(ctx.session_id.as_deref(), agent_id) {
            return;
        }
        eprintln!(
            "  {}",
            format!("Agent or delegation not found: {agent_id}").yellow()
        );
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

    loop {
        match rx.recv().await {
            Ok(event) => {
                if event.agent_id == target_agent_id {
                    print_progress_event(&event);

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

    eprintln!();
}

fn show_delegation_logs(session_id: Option<&str>, query: &str) -> bool {
    let Some((delegation_id, events)) = load_delegation_events(session_id, query) else {
        return false;
    };
    eprintln!(
        "\n  {} {}\n",
        "📋 Delegation logs for".cyan(),
        delegation_id.as_str().white().bold()
    );
    for event in events {
        match event.event_type {
            JournalEventType::DelegationStarted => {
                let pattern = event
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("pattern"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                let count = event
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("agent_count"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                eprintln!(
                    "  [{}] {} started {} ({} agents)",
                    event.ts.dim(),
                    "▶".green(),
                    pattern,
                    count
                );
            }
            JournalEventType::DelegationRetry => {
                let metadata = event.metadata.as_ref();
                let agent_id = metadata
                    .and_then(|meta| meta.get("agent_id"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                let attempt = metadata
                    .and_then(|meta| meta.get("attempt"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let reason = metadata
                    .and_then(|meta| meta.get("reason"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                eprintln!(
                    "  [{}] {} {} retry #{} — {}",
                    event.ts.dim(),
                    "↻".yellow(),
                    agent_id,
                    attempt,
                    reason
                );
            }
            JournalEventType::DelegationSubRunCompleted => {
                let metadata = event.metadata.as_ref();
                let agent_id = metadata
                    .and_then(|meta| meta.get("agent_id"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                let status = metadata
                    .and_then(|meta| meta.get("status"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                let output_preview = metadata
                    .and_then(|meta| meta.get("output_preview"))
                    .and_then(|value| value.as_str());
                let error = metadata
                    .and_then(|meta| meta.get("error"))
                    .and_then(|value| value.as_str());
                eprintln!(
                    "  [{}] {} {} [{}]",
                    event.ts.dim(),
                    sub_run_status_icon(status),
                    agent_id,
                    status
                );
                if let Some(preview) = output_preview {
                    eprintln!("      {}", preview.cyan());
                }
                if let Some(error) = error {
                    eprintln!("      {}", error.red());
                }
            }
            JournalEventType::DelegationCompleted => {
                let metadata = event.metadata.as_ref();
                let status = metadata
                    .and_then(|meta| meta.get("aggregated_status"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                let succeeded = metadata
                    .and_then(|meta| meta.get("succeeded"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let failed = metadata
                    .and_then(|meta| meta.get("failed"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let preview = metadata
                    .and_then(|meta| meta.get("aggregated_output_preview"))
                    .and_then(|value| value.as_str());
                eprintln!(
                    "  [{}] {} completed [{} ok / {} failed, status={}]",
                    event.ts.dim(),
                    sub_run_status_icon(status),
                    succeeded,
                    failed,
                    status
                );
                if let Some(preview) = preview {
                    eprintln!("      {}", preview.cyan());
                }
            }
            _ => {}
        }
    }
    eprintln!();
    true
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
        ProgressEventType::MetricsUpdate {
            turn,
            max_turns,
            total_prompt_tokens,
            total_completion_tokens,
            total_tool_calls,
        } => {
            format!(
                "📊 {}/{} turns | {} tools | {}+{} tokens",
                turn, max_turns, total_tool_calls, total_prompt_tokens, total_completion_tokens
            )
        }
        ProgressEventType::AgentSpawned {
            agent_type,
            description,
            parent_run_id,
            ..
        } => {
            let parent_label = if parent_run_id.is_empty() {
                String::new()
            } else {
                format!(" ← {}", &parent_run_id[..8.min(parent_run_id.len())])
            };
            format!(
                "▶ 🌲 {} spawned: {}{}",
                agent_type.as_str().cyan(),
                description,
                parent_label.dim()
            )
        }
    };

    eprintln!("  [{}] {}", time_str.as_str().dim(), msg);
}

fn show_help() {
    eprintln!("\n  {}", "🤖 Agent Commands".cyan().bold());
    eprintln!("  {}", "─".repeat(50).dim());
    eprintln!(
        "  {}  List recent agents and delegations",
        "/agent".white().bold()
    );
    eprintln!(
        "  {}  List recent agents and delegations",
        "/agent list".white().bold()
    );
    eprintln!(
        "  {}  Show delegation history for this session",
        "/agent history".white().bold()
    );
    eprintln!(
        "  {}  Show delegation tree (hierarchy)",
        "/agent tree".white().bold()
    );
    eprintln!(
        "  {}  Watch tree with real-time updates",
        "/agent watch".white().bold()
    );
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
        "Spawned agents come from spawn_agent; delegations come from the delegate tool.".dim()
    );
    eprintln!(
        "  {}",
        "Use /agent status <agent_id|delegation_id> to inspect a specific item.".dim()
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
            let preview = if result.chars().count() > 50 {
                format!("{}...", result.chars().take(50).collect::<String>())
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

fn is_terminal_agent_status(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed { .. } | AgentStatus::Failed { .. } | AgentStatus::Cancelled
    )
}

fn print_agent_section(title: &str, agents: &[astra_runtime::orchestration::SpawnedAgentInfo]) {
    eprintln!("\n  {}", title.cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    for agent in agents {
        let elapsed = agent
            .started_at
            .elapsed()
            .map(|d| format_duration(d))
            .unwrap_or_else(|_| "?".to_string());
        eprintln!(
            "  {} {} {} ({}){}",
            status_icon(&agent.status),
            agent.agent_id.as_str().white().bold(),
            format!("[{}]", agent.agent_type).dim(),
            elapsed.dim(),
            if agent.has_permission_issues {
                format!(" {}", "🔒".red())
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
}

fn print_delegation_section(entries: &[DelegationHistoryEntry]) {
    eprintln!("\n  {}", "🤝 Recent Delegations".cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    for entry in entries {
        eprintln!(
            "  {} {} {} [{} ok / {} failed]",
            sub_run_status_icon(&entry.status),
            entry.delegation_id.as_str().white().bold(),
            format!("[{}]", entry.pattern).dim(),
            entry.succeeded.to_string().green(),
            entry.failed.to_string().red()
        );
        eprintln!("    {}", format!("status: {}", entry.status).dim());
        if let Some(preview) = &entry.aggregated_output_preview {
            eprintln!("    {}", preview.as_str().cyan());
        }
    }
}

fn sub_run_status_icon(status: &str) -> &'static str {
    match status {
        "completed" => "✅",
        "partial" => "🟡",
        "failed" => "❌",
        "cancelled" => "🛑",
        _ => "⑂",
    }
}

fn load_recent_delegations(session_id: Option<&str>) -> Vec<DelegationHistoryEntry> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    let Ok(events) = session_journal::read_journal(session_id) else {
        return Vec::new();
    };
    let mut delegations: HashMap<String, DelegationHistoryEntry> = HashMap::new();
    for (idx, event) in events.into_iter().enumerate() {
        let Some(metadata) = event.metadata else {
            continue;
        };
        let Some(delegation_id) = metadata
            .get("delegation_id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let entry =
            delegations
                .entry(delegation_id.clone())
                .or_insert_with(|| DelegationHistoryEntry {
                    delegation_id,
                    last_seen_index: idx,
                    ..DelegationHistoryEntry::default()
                });
        entry.last_seen_index = idx;
        match event.event_type {
            JournalEventType::DelegationStarted => {
                entry.pattern = metadata
                    .get("pattern")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?")
                    .to_string();
                entry.total_sub_runs = metadata
                    .get("agent_count")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                if entry.status.is_empty() {
                    entry.status = "running".to_string();
                }
            }
            JournalEventType::DelegationSubRunCompleted => {
                let sub_run_id = metadata
                    .get("sub_run_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                let sub_run = DelegationSubRunSummary {
                    sub_run_id: sub_run_id.clone(),
                    agent_id: metadata
                        .get("agent_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    status: metadata
                        .get("status")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    error: metadata
                        .get("error")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    output_preview: metadata
                        .get("output_preview")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                };
                if let Some(existing) = entry
                    .sub_runs
                    .iter_mut()
                    .find(|existing| existing.sub_run_id == sub_run_id)
                {
                    *existing = sub_run;
                } else {
                    entry.sub_runs.push(sub_run);
                }
            }
            JournalEventType::DelegationCompleted => {
                entry.pattern = metadata
                    .get("pattern")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?")
                    .to_string();
                entry.total_sub_runs = metadata
                    .get("total_sub_runs")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(entry.total_sub_runs as u64)
                    as usize;
                entry.succeeded = metadata
                    .get("succeeded")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                entry.failed = metadata
                    .get("failed")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                entry.status = metadata
                    .get("aggregated_status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?")
                    .to_string();
                entry.aggregated_output_preview = metadata
                    .get("aggregated_output_preview")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    let mut entries: Vec<_> = delegations.into_values().collect();
    entries.sort_by_key(|entry| Reverse(entry.last_seen_index));
    entries
}

fn find_delegation_entry(session_id: Option<&str>, query: &str) -> Option<DelegationHistoryEntry> {
    load_recent_delegations(session_id)
        .into_iter()
        .find(|entry| entry.delegation_id == query || entry.delegation_id.starts_with(query))
}

fn load_delegation_events(
    session_id: Option<&str>,
    query: &str,
) -> Option<(String, Vec<session_journal::JournalEvent>)> {
    let Some(session_id) = session_id else {
        return None;
    };
    let delegation_id = find_delegation_entry(Some(session_id), query)?.delegation_id;
    let Ok(events) = session_journal::read_journal(session_id) else {
        return None;
    };
    let filtered: Vec<_> = events
        .into_iter()
        .filter(|event| {
            event.metadata.as_ref().is_some_and(|metadata| {
                metadata
                    .get("delegation_id")
                    .and_then(|value| value.as_str())
                    == Some(delegation_id.as_str())
            })
        })
        .collect();
    Some((delegation_id, filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::messaging::{AgentMailboxRouter, InProcessTransport};
    use astra_runtime::server::delegation_engine::DelegationTracker;
    use astra_services::session_journal::JournalEvent;
    use std::sync::Arc;

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

    #[tokio::test]
    async fn handle_agent_list_does_not_block_on_runtime() {
        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));
        let spawner = Arc::new(DynamicAgentSpawner::new(router));
        let ctx = AgentCommandContext {
            spawner: Some(spawner),
            session_id: None,
        };

        handle_agent_command("list", &ctx).await;
    }

    #[test]
    fn load_recent_delegations_collects_summary_and_subruns() {
        let sid = format!("slash-agent-test-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::delegation_started(
                Some(&sid),
                "del-1",
                "run-parent",
                "fan_out",
                &["coder".to_string(), "reviewer".to_string()],
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_sub_run_completed(
                Some(&sid),
                "del-1",
                "run-1",
                "coder",
                "completed",
                None,
                Some("implemented fix"),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_completed(
                Some(&sid),
                "del-1",
                "fan_out",
                2,
                2,
                0,
                "completed",
                Some("merged final answer"),
            ))
            .unwrap();

        let delegations = load_recent_delegations(Some(&sid));
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].delegation_id, "del-1");
        assert_eq!(delegations[0].status, "completed");
        assert_eq!(
            delegations[0].aggregated_output_preview.as_deref(),
            Some("merged final answer")
        );
        assert_eq!(delegations[0].sub_runs.len(), 1);
        assert_eq!(
            delegations[0].sub_runs[0].output_preview.as_deref(),
            Some("implemented fix")
        );
    }

    #[test]
    fn load_delegation_events_filters_matching_delegation() {
        let sid = format!("slash-agent-logs-test-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::delegation_started(
                Some(&sid),
                "del-logs",
                "run-parent",
                "fan_out",
                &["coder".to_string()],
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_retry(
                Some(&sid),
                "del-logs",
                "run-1",
                "run-2",
                "coder",
                2,
                "needs another pass",
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_completed(
                Some(&sid),
                "del-logs",
                "fan_out",
                1,
                1,
                0,
                "completed",
                Some("done"),
            ))
            .unwrap();

        let (delegation_id, events) = load_delegation_events(Some(&sid), "del-log").unwrap();
        assert_eq!(delegation_id, "del-logs");
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].event_type, JournalEventType::DelegationRetry);
    }
}
