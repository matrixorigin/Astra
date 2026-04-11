//! `/agent` slash command — inspect spawned agents and recent delegations.
//!
//! Subcommands:
//! - `/agent` or `/agent list`: List active/recent spawned agents and delegations
//! - `/agent tree`: Show agent delegation tree (parent-child hierarchy)
//! - `/agent watch`: Watch live spawned-agent updates plus journal-backed delegation changes
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
    retry_of: Option<String>,
    attempt: u32,
    retry_reason: Option<String>,
    error: Option<String>,
    output_preview: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DelegationRetrySummary {
    original_run_id: String,
    retry_run_id: String,
    agent_id: String,
    attempt: u32,
    reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DelegationHistoryEntry {
    delegation_id: String,
    parent_run_id: Option<String>,
    pattern: String,
    agent_ids: Vec<String>,
    total_sub_runs: usize,
    succeeded: usize,
    failed: usize,
    retry_count: usize,
    status: String,
    aggregated_output_preview: Option<String>,
    retries: Vec<DelegationRetrySummary>,
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
        if let Some(parent_run_id) = &entry.parent_run_id {
            eprintln!(
                "    {}",
                format!("parent run: {}", parent_run_id.as_str()).dim()
            );
        }
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
    let agents = if let Some(ref spawner) = ctx.spawner {
        spawner.list_all_agents().await
    } else {
        Vec::new()
    };
    let delegations = load_recent_delegations(ctx.session_id.as_deref());

    if agents.is_empty() && delegations.is_empty() {
        eprintln!(
            "\n  {}",
            "🌲 No agent or delegation tree available".cyan().bold()
        );
        eprintln!();
        return;
    }

    eprintln!("\n  {}", "🌲 Agent Delegation Tree".cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());

    if !agents.is_empty() {
        eprintln!("  {}", "Spawned agents".white().bold());
        let forest = AgentTreeNode::build_forest(&agents);
        let rendered = render_agent_forest(&forest);
        for line in rendered.lines() {
            eprintln!("  {}", line);
        }
    }

    if !delegations.is_empty() {
        if !agents.is_empty() {
            eprintln!();
        }
        eprintln!("  {}", "Journal-backed delegations".white().bold());
        for line in render_delegation_tree(&delegations) {
            eprintln!("  {}", line);
        }
    }
    eprintln!();
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
        let events = load_delegation_events(ctx.session_id.as_deref(), agent_id)
            .map(|(_, events)| events)
            .unwrap_or_default();
        eprintln!(
            "\n  {} {}",
            "🤝 Delegation".cyan().bold(),
            entry.delegation_id.as_str().white().bold()
        );
        eprintln!("  {}", "─".repeat(50).dim());
        for line in render_delegation_status_lines(&entry, &events) {
            if line.is_empty() {
                eprintln!();
            } else {
                eprintln!("  {}", line);
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
            if let Some(entry) = find_delegation_entry(ctx.session_id.as_deref(), agent_id) {
                eprintln!(
                    "  {}",
                    format!(
                        "Permissions are only tracked for spawned agents. Delegation '{}' is journal-backed; use /agent status or /agent logs.",
                        entry.delegation_id
                    )
                    .yellow()
                );
            } else {
                eprintln!(
                    "  {}",
                    format!("Agent or delegation not found: {agent_id}").yellow()
                );
            }
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
        if let Some(entry) = find_delegation_entry(ctx.session_id.as_deref(), agent_id) {
            eprintln!(
                "  {}",
                format!(
                    "Delegation '{}' runs synchronously under its parent agent and cannot be stopped via /agent stop. Cancel the parent run while it is active instead.",
                    entry.delegation_id
                )
                .yellow()
            );
        } else {
            eprintln!(
                "  {}",
                format!("Agent or delegation not found: {agent_id}").yellow()
            );
        }
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

/// Watch live spawned-agent updates plus journal-backed delegation changes.
/// Throttles rendering to max once per 500ms.
async fn show_watch(ctx: &AgentCommandContext) {
    use std::time::Duration;

    eprintln!("\n  {} Watching agent tree (Ctrl+C to stop)\n", "👁".cyan());
    eprintln!(
        "  {}",
        "Watch follows live spawned agents; delegations appear through journal-backed lifecycle changes, not per-turn sub-agent output."
            .dim()
    );
    eprintln!();

    let spawner_clone = ctx.spawner.clone();
    let mut rx = spawner_clone
        .as_ref()
        .map(|spawner| spawner.subscribe_progress());
    let throttle_interval = Duration::from_millis(500);
    let mut last_snapshot = build_watch_snapshot(
        &load_watch_agents(spawner_clone.as_ref()).await,
        &load_recent_delegations(ctx.session_id.as_deref()),
    );
    print_watch_snapshot(&last_snapshot);

    loop {
        let snapshot = wait_for_watch_snapshot_change(
            spawner_clone.as_ref(),
            &mut rx,
            ctx.session_id.as_deref(),
            &last_snapshot,
            throttle_interval,
        )
        .await;
        print_watch_snapshot(&snapshot);
        last_snapshot = snapshot;
    }
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
    eprintln!(
        "  {}",
        "Delegations run synchronously: the parent agent pauses until sub-runs finish and results aggregate."
            .dim()
    );
    eprintln!(
        "  {}",
        "/agent watch shows live spawned-agent progress plus journal-backed delegation lifecycle changes; /agent stop and /agent permissions remain spawned-agent-only."
            .dim()
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
        if let Some(parent_run_id) = &entry.parent_run_id {
            eprintln!(
                "    {}",
                format!("parent run: {}", shorten_run_id(parent_run_id))
                    .dim()
                    .italic()
            );
        }
        if entry.retry_count > 0 {
            eprintln!(
                "    {}",
                format!("retries: {}", entry.retry_count).yellow().dim()
            );
        }
        if let Some(preview) = &entry.aggregated_output_preview {
            eprintln!("    {}", preview.as_str().cyan());
        }
    }
}

fn render_delegation_tree(entries: &[DelegationHistoryEntry]) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        // Build aggregated progress summary: "3/5 done, 1 failed"
        let done_count = entry
            .sub_runs
            .iter()
            .filter(|s| s.status == "completed" || s.status == "partial")
            .count();
        let failed_count = entry
            .sub_runs
            .iter()
            .filter(|s| s.status == "failed" || s.status == "cancelled")
            .count();
        let total = entry.total_sub_runs.max(entry.sub_runs.len());
        let progress = if total > 0 {
            let mut parts = vec![format!("{}/{}", done_count, total)];
            if failed_count > 0 {
                parts.push(format!("{} failed", failed_count));
            }
            format!(" ({})", parts.join(", "))
        } else {
            String::new()
        };

        lines.push(format!(
            "{} {} [{}{}]{}{}",
            sub_run_status_icon(&entry.status),
            entry.delegation_id,
            entry.pattern,
            if entry.retry_count > 0 {
                format!(
                    ", {} retr{}",
                    entry.retry_count,
                    if entry.retry_count == 1 { "y" } else { "ies" }
                )
            } else {
                String::new()
            },
            progress,
            entry
                .parent_run_id
                .as_ref()
                .map(|parent_run_id| format!(" ← parent {}", shorten_run_id(parent_run_id)))
                .unwrap_or_default()
        ));
        for (idx, sub_run) in entry.sub_runs.iter().enumerate() {
            let branch = if idx + 1 == entry.sub_runs.len() {
                "└──"
            } else {
                "├──"
            };
            lines.push(format!(
                "{} {} {} [{}]",
                branch,
                sub_run_status_icon(&sub_run.status),
                sub_run.agent_id,
                sub_run_tree_label(sub_run)
            ));
        }
    }
    lines
}

async fn load_watch_agents(
    spawner: Option<&Arc<DynamicAgentSpawner>>,
) -> Vec<astra_runtime::orchestration::SpawnedAgentInfo> {
    if let Some(spawner) = spawner {
        spawner.list_all_agents().await
    } else {
        Vec::new()
    }
}

async fn recv_watch_event(
    rx: &mut Option<
        tokio::sync::broadcast::Receiver<astra_runtime::orchestration::AgentProgressEvent>,
    >,
) -> Result<
    astra_runtime::orchestration::AgentProgressEvent,
    tokio::sync::broadcast::error::RecvError,
> {
    if let Some(rx) = rx {
        rx.recv().await
    } else {
        std::future::pending::<
            Result<
                astra_runtime::orchestration::AgentProgressEvent,
                tokio::sync::broadcast::error::RecvError,
            >,
        >()
        .await
    }
}

async fn wait_for_watch_snapshot_change(
    spawner: Option<&Arc<DynamicAgentSpawner>>,
    rx: &mut Option<
        tokio::sync::broadcast::Receiver<astra_runtime::orchestration::AgentProgressEvent>,
    >,
    session_id: Option<&str>,
    last_snapshot: &str,
    poll_interval: std::time::Duration,
) -> String {
    use astra_runtime::orchestration::ProgressEventType;
    use tokio::sync::broadcast::error::RecvError;

    let mut interval = tokio::time::interval(poll_interval);
    loop {
        let should_refresh = tokio::select! {
            _ = interval.tick() => true,
            event = recv_watch_event(rx) => {
                match event {
                    Ok(event) => matches!(
                        event.event_type,
                        ProgressEventType::AgentSpawned { .. }
                            | ProgressEventType::Completed { .. }
                            | ProgressEventType::Failed { .. }
                            | ProgressEventType::Cancelled { .. }
                            | ProgressEventType::PermissionDenied { .. }
                    ),
                    Err(RecvError::Lagged(_)) => true,
                    Err(RecvError::Closed) => {
                        *rx = None;
                        true
                    }
                }
            }
        };
        if !should_refresh {
            continue;
        }
        let snapshot = build_watch_snapshot(
            &load_watch_agents(spawner).await,
            &load_recent_delegations(session_id),
        );
        if snapshot != last_snapshot {
            return snapshot;
        }
    }
}

fn build_watch_snapshot(
    agents: &[astra_runtime::orchestration::SpawnedAgentInfo],
    delegations: &[DelegationHistoryEntry],
) -> String {
    let mut lines = Vec::new();

    // Add overall summary at the top when there are delegations
    if !delegations.is_empty() {
        let total_subruns: usize = delegations.iter().map(|d| d.total_sub_runs.max(d.sub_runs.len())).sum();
        let done_subruns: usize = delegations
            .iter()
            .flat_map(|d| &d.sub_runs)
            .filter(|s| s.status == "completed" || s.status == "partial")
            .count();
        let failed_subruns: usize = delegations
            .iter()
            .flat_map(|d| &d.sub_runs)
            .filter(|s| s.status == "failed" || s.status == "cancelled")
            .count();
        let running_subruns: usize = delegations
            .iter()
            .flat_map(|d| &d.sub_runs)
            .filter(|s| s.status == "running" || s.status == "created")
            .count();

        let mut summary_parts = vec![format!("{} delegations", delegations.len())];
        if total_subruns > 0 {
            summary_parts.push(format!("{}/{} sub-runs done", done_subruns, total_subruns));
        }
        if running_subruns > 0 {
            summary_parts.push(format!("{} running", running_subruns));
        }
        if failed_subruns > 0 {
            summary_parts.push(format!("{} failed", failed_subruns));
        }
        lines.push(format!("  📊 {}", summary_parts.join(" • ")));
        lines.push(String::new());
    }

    if !agents.is_empty() {
        lines.push(format!("  Spawned agents ({})", agents.len()));
        let forest = AgentTreeNode::build_forest(agents);
        let rendered = render_agent_forest(&forest);
        lines.extend(rendered.lines().map(|line| format!("  {line}")));
    }
    if !delegations.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!(
            "  Journal-backed delegations ({})",
            delegations.len()
        ));
        lines.extend(
            render_delegation_tree(delegations)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    if lines.is_empty() {
        lines.push("  (no agents or delegations yet)".to_string());
    }
    lines.join("\n")
}

fn print_watch_snapshot(snapshot: &str) {
    eprint!("\x1b[2J\x1b[H");
    eprintln!("  {}", "🌲 Agent Delegation Tree".cyan().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    eprintln!(
        "  {}",
        "Refreshing every 500ms; spawned agents update live, while delegations reflect journal-backed lifecycle changes.".dim()
    );
    eprintln!();
    for line in snapshot.lines() {
        eprintln!("  {}", line);
    }
}

fn sub_run_status_icon(status: &str) -> &'static str {
    match status {
        "created" => "⏳",
        "running" => "🔄",
        "completed" => "✅",
        "partial" => "🟡",
        "failed" => "❌",
        "cancelled" => "🛑",
        _ => "⑂",
    }
}

fn shorten_run_id(run_id: &str) -> String {
    run_id[..8.min(run_id.len())].to_string()
}

fn hydrate_retry_metadata(entry: &mut DelegationHistoryEntry) {
    let retry_by_run: HashMap<_, _> = entry
        .retries
        .iter()
        .map(|retry| (retry.retry_run_id.clone(), retry.clone()))
        .collect();
    for sub_run in &mut entry.sub_runs {
        sub_run.attempt = 1;
        sub_run.retry_of = None;
        sub_run.retry_reason = None;
        if let Some(retry) = retry_by_run.get(&sub_run.sub_run_id) {
            sub_run.retry_of = Some(retry.original_run_id.clone());
            sub_run.attempt = retry.attempt.max(1);
            if !retry.reason.is_empty() {
                sub_run.retry_reason = Some(retry.reason.clone());
            }
        }
    }
}

fn sub_run_tree_label(sub_run: &DelegationSubRunSummary) -> String {
    let mut label = format!(
        "{} | run {}",
        sub_run.status,
        shorten_run_id(&sub_run.sub_run_id)
    );
    if let Some(retry_of) = &sub_run.retry_of {
        label.push_str(&format!(
            ", retry #{} of {}",
            sub_run.attempt.max(1),
            shorten_run_id(retry_of)
        ));
    }
    label
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
                entry.parent_run_id = metadata
                    .get("parent_run_id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                entry.pattern = metadata
                    .get("pattern")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?")
                    .to_string();
                entry.agent_ids = metadata
                    .get("agent_ids")
                    .and_then(|value| value.as_array())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                entry.total_sub_runs = metadata
                    .get("agent_count")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                if entry.status.is_empty() {
                    entry.status = "running".to_string();
                }
            }
            JournalEventType::DelegationSubRunStarted => {
                let sub_run_id = metadata
                    .get("sub_run_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                let started = DelegationSubRunSummary {
                    sub_run_id: sub_run_id.clone(),
                    agent_id: metadata
                        .get("agent_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    status: metadata
                        .get("status")
                        .and_then(|value| value.as_str())
                        .unwrap_or("running")
                        .to_string(),
                    retry_of: metadata
                        .get("retry_of")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    attempt: 1,
                    retry_reason: None,
                    error: None,
                    output_preview: None,
                };
                if let Some(existing) = entry
                    .sub_runs
                    .iter_mut()
                    .find(|existing| existing.sub_run_id == sub_run_id)
                {
                    existing.agent_id = started.agent_id;
                    existing.status = started.status;
                    existing.retry_of = started.retry_of;
                } else {
                    entry.sub_runs.push(started);
                }
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
                    retry_of: None,
                    attempt: 1,
                    retry_reason: None,
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
            JournalEventType::DelegationRetry => {
                entry.retry_count = entry.retry_count.saturating_add(1);
                let retry = DelegationRetrySummary {
                    original_run_id: metadata
                        .get("original_run_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    retry_run_id: metadata
                        .get("retry_run_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    agent_id: metadata
                        .get("agent_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    attempt: metadata
                        .get("attempt")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(2) as u32,
                    reason: metadata
                        .get("reason")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                };
                if let Some(existing) = entry
                    .retries
                    .iter_mut()
                    .find(|existing| existing.retry_run_id == retry.retry_run_id)
                {
                    *existing = retry;
                } else {
                    entry.retries.push(retry);
                }
                if entry.status.is_empty() {
                    entry.status = "running".to_string();
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
    for entry in &mut entries {
        hydrate_retry_metadata(entry);
    }
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
    let session_id = session_id?;
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

fn delegation_parent_lifecycle_note(status: &str) -> &'static str {
    match status {
        "running" => {
            "Parent agent is paused while delegated sub-runs execute and wait for aggregation."
        }
        "completed" => {
            "Delegated sub-runs finished and the parent agent can continue from the aggregated result below."
        }
        "partial" | "partial_failure" => {
            "Delegated sub-runs finished with failures; the parent agent can continue, but review the failed sub-runs below."
        }
        "failed" => {
            "Delegation failed before a clean aggregate was produced; review the lifecycle and failures below."
        }
        "cancelled" => {
            "Delegation was cancelled before aggregation completed; the parent agent cannot use a final delegated result."
        }
        _ => {
            "Delegation state is journal-backed; inspect the lifecycle below for the latest progress."
        }
    }
}

fn truncate_display(text: &str, limit: usize) -> String {
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit).collect::<String>())
    } else {
        text.to_string()
    }
}

fn format_delegation_event_brief(event: &session_journal::JournalEvent) -> Option<String> {
    let metadata = event.metadata.as_ref()?;
    match event.event_type {
        JournalEventType::DelegationStarted => {
            let pattern = metadata
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let count = metadata
                .get("agent_count")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            Some(format!(
                "[{}] ▶ started {} ({} agents)",
                event.ts, pattern, count
            ))
        }
        JournalEventType::DelegationSubRunStarted => {
            let agent_id = metadata
                .get("agent_id")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let sub_run_id = metadata
                .get("sub_run_id")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let retry_suffix = metadata
                .get("retry_of")
                .and_then(|value| value.as_str())
                .map(|retry_of| format!(" (retry of {})", retry_of))
                .unwrap_or_default();
            Some(format!(
                "[{}] ↳ {} running {}{}",
                event.ts, agent_id, sub_run_id, retry_suffix
            ))
        }
        JournalEventType::DelegationRetry => {
            let agent_id = metadata
                .get("agent_id")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let attempt = metadata
                .get("attempt")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let reason = metadata
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(format!(
                "[{}] ↻ {} retry #{}{}",
                event.ts,
                agent_id,
                attempt,
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" — {reason}")
                }
            ))
        }
        JournalEventType::DelegationSubRunCompleted => {
            let agent_id = metadata
                .get("agent_id")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let status = metadata
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let detail = metadata
                .get("error")
                .and_then(|value| value.as_str())
                .or_else(|| {
                    metadata
                        .get("output_preview")
                        .and_then(|value| value.as_str())
                })
                .map(|msg| truncate_display(msg, 120))
                .unwrap_or_default();
            Some(format!(
                "[{}] {} {} [{}]{}",
                event.ts,
                sub_run_status_icon(status),
                agent_id,
                status,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            ))
        }
        JournalEventType::DelegationCompleted => {
            let status = metadata
                .get("aggregated_status")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let succeeded = metadata
                .get("succeeded")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let failed = metadata
                .get("failed")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let preview = metadata
                .get("aggregated_output_preview")
                .and_then(|value| value.as_str())
                .map(|msg| truncate_display(msg, 120))
                .unwrap_or_default();
            Some(format!(
                "[{}] {} completed [{} ok / {} failed, status={}]{}",
                event.ts,
                sub_run_status_icon(status),
                succeeded,
                failed,
                status,
                if preview.is_empty() {
                    String::new()
                } else {
                    format!(" — {preview}")
                }
            ))
        }
        _ => None,
    }
}

fn render_delegation_status_lines(
    entry: &DelegationHistoryEntry,
    events: &[session_journal::JournalEvent],
) -> Vec<String> {
    let mut lines = vec![
        format!("Pattern: {}", entry.pattern),
        format!(
            "Parent run: {}",
            entry.parent_run_id.as_deref().unwrap_or("unknown")
        ),
        format!(
            "Agents: {}",
            if entry.agent_ids.is_empty() {
                "?".to_string()
            } else {
                entry.agent_ids.join(", ")
            }
        ),
        format!("Status: {}", entry.status),
        format!("Sub-runs: {}", entry.total_sub_runs),
        format!("Results: {} ok, {} failed", entry.succeeded, entry.failed),
    ];
    if entry.retry_count > 0 {
        lines.push(format!("Retries: {}", entry.retry_count));
    }
    lines.push(format!(
        "Lifecycle: {}",
        delegation_parent_lifecycle_note(&entry.status)
    ));

    lines.push(String::new());
    lines.push("📋 Final result".to_string());
    if let Some(preview) = delegation_final_preview(entry) {
        if entry.aggregated_output_preview.is_none() {
            lines.push(
                "  (using successful sub-run output because no aggregate preview was recorded)"
                    .to_string(),
            );
        }
        for line in preview.lines() {
            lines.push(format!("  {}", line));
        }
    } else if entry.status == "running" {
        lines
            .push("  Waiting for aggregated output; the parent agent is still paused.".to_string());
    } else {
        lines.push(
            "  No aggregated result preview was recorded; use /agent logs for the full lifecycle."
                .to_string(),
        );
    }

    let failed_sub_runs: Vec<_> = entry
        .sub_runs
        .iter()
        .filter(|sub_run| {
            sub_run.status == "failed" || sub_run.status == "cancelled" || sub_run.error.is_some()
        })
        .collect();
    if !failed_sub_runs.is_empty() {
        lines.push(String::new());
        lines.push("❌ Failures".to_string());
        for sub_run in failed_sub_runs {
            lines.push(format!(
                "  {} {} [{}]",
                sub_run_status_icon(&sub_run.status),
                sub_run.agent_id,
                sub_run.status
            ));
            lines.push(format!(
                "    {}",
                sub_run
                    .error
                    .as_deref()
                    .map(|error| truncate_display(error, 160))
                    .unwrap_or_else(|| {
                        "No explicit error recorded; inspect /agent logs for details.".to_string()
                    })
            ));
        }
    }

    if !entry.retries.is_empty() {
        lines.push(String::new());
        lines.push("🔁 Retry lineage".to_string());
        for retry in &entry.retries {
            lines.push(format!(
                "  {}: {} -> {} (retry #{})",
                retry.agent_id,
                retry.original_run_id,
                retry.retry_run_id,
                retry.attempt.max(1)
            ));
            if !retry.reason.is_empty() {
                lines.push(format!(
                    "    reason: {}",
                    truncate_display(&retry.reason, 160)
                ));
            }
        }
    }

    let lifecycle_lines: Vec<_> = events
        .iter()
        .filter_map(format_delegation_event_brief)
        .collect();
    if !lifecycle_lines.is_empty() {
        lines.push(String::new());
        lines.push("🕒 Recent lifecycle".to_string());
        for line in lifecycle_lines
            .iter()
            .skip(lifecycle_lines.len().saturating_sub(5))
        {
            lines.push(format!("  {}", line));
        }
    }

    if !entry.sub_runs.is_empty() {
        lines.push(String::new());
        lines.push("📦 Sub-runs".to_string());
        for sub_run in &entry.sub_runs {
            let mut header = format!(
                "  {} {} [{}]",
                sub_run_status_icon(&sub_run.status),
                sub_run.agent_id,
                sub_run.status
            );
            if sub_run.retry_of.is_some() {
                header.push_str(&format!(" retry #{}", sub_run.attempt.max(1)));
            }
            lines.push(header);
            lines.push(format!("    run: {}", sub_run.sub_run_id));
            if let Some(retry_of) = &sub_run.retry_of {
                lines.push(format!("    retry of: {}", retry_of));
            }
            if let Some(retry_reason) = &sub_run.retry_reason {
                lines.push(format!(
                    "    retry reason: {}",
                    truncate_display(retry_reason, 160)
                ));
            }
            if let Some(preview) = &sub_run.output_preview {
                lines.push(format!("    output: {}", truncate_display(preview, 160)));
            }
            if let Some(error) = &sub_run.error {
                lines.push(format!("    error: {}", truncate_display(error, 160)));
            }
        }
    }

    lines
}

fn delegation_final_preview(entry: &DelegationHistoryEntry) -> Option<String> {
    if let Some(preview) = &entry.aggregated_output_preview {
        return Some(preview.clone());
    }
    let successful_outputs: Vec<_> = entry
        .sub_runs
        .iter()
        .filter(|sub_run| sub_run.status == "completed")
        .filter_map(|sub_run| sub_run.output_preview.as_ref())
        .collect();
    if successful_outputs.len() == 1 {
        Some(successful_outputs[0].clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::messaging::{AgentMailboxRouter, InProcessTransport};
    use astra_runtime::orchestration::{
        SpawnAgentInput, SpawnContext, SpawnedAgentInfo, SpawnedAgentMetrics,
    };
    use astra_runtime::server::delegation_engine::DelegationTracker;
    use astra_services::session_journal::JournalEvent;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn make_agent(agent_id: &str, run_id: &str, status: AgentStatus) -> SpawnedAgentInfo {
        SpawnedAgentInfo {
            agent_id: agent_id.to_string(),
            run_id: run_id.to_string(),
            parent_run_id: "root-run".to_string(),
            agent_type: "task".to_string(),
            description: "test agent".to_string(),
            status,
            started_at: SystemTime::now(),
            metrics: SpawnedAgentMetrics::default(),
            has_permission_issues: false,
        }
    }

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
                "failed",
                Some("needs retry"),
                None,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_retry(
                Some(&sid),
                "del-1",
                "run-1",
                "run-2",
                "coder",
                2,
                "needs another pass",
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_sub_run_completed(
                Some(&sid),
                "del-1",
                "run-2",
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
                1,
                1,
                "completed",
                Some("merged final answer"),
            ))
            .unwrap();

        let delegations = load_recent_delegations(Some(&sid));
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].delegation_id, "del-1");
        assert_eq!(delegations[0].parent_run_id.as_deref(), Some("run-parent"));
        assert_eq!(delegations[0].agent_ids, vec!["coder", "reviewer"]);
        assert_eq!(delegations[0].retry_count, 1);
        assert_eq!(delegations[0].status, "completed");
        assert_eq!(
            delegations[0].aggregated_output_preview.as_deref(),
            Some("merged final answer")
        );
        assert_eq!(delegations[0].sub_runs.len(), 2);
        assert_eq!(
            delegations[0].sub_runs[1].output_preview.as_deref(),
            Some("implemented fix")
        );
        assert_eq!(
            delegations[0].sub_runs[1].retry_of.as_deref(),
            Some("run-1")
        );
        assert_eq!(delegations[0].sub_runs[1].attempt, 2);
        assert_eq!(
            delegations[0].sub_runs[1].retry_reason.as_deref(),
            Some("needs another pass")
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

    #[test]
    fn load_recent_delegations_includes_running_subruns() {
        let sid = format!("slash-agent-running-test-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::delegation_started(
                Some(&sid),
                "del-live",
                "run-parent",
                "fan_out",
                &["coder".to_string()],
            ))
            .unwrap();
        writer
            .append(&JournalEvent::delegation_sub_run_started(
                Some(&sid),
                "del-live",
                "run-1",
                "run-parent",
                "coder",
                "running",
                1,
                None,
            ))
            .unwrap();

        let delegations = load_recent_delegations(Some(&sid));
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].status, "running");
        assert_eq!(delegations[0].sub_runs.len(), 1);
        assert_eq!(delegations[0].sub_runs[0].sub_run_id, "run-1");
        assert_eq!(delegations[0].sub_runs[0].status, "running");
    }

    #[test]
    fn render_delegation_tree_includes_parent_and_retry_annotations() {
        let lines = render_delegation_tree(&[DelegationHistoryEntry {
            delegation_id: "del-1".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            pattern: "fan_out".to_string(),
            status: "completed".to_string(),
            sub_runs: vec![
                DelegationSubRunSummary {
                    sub_run_id: "run-1".to_string(),
                    agent_id: "coder".to_string(),
                    status: "completed".to_string(),
                    retry_of: None,
                    attempt: 1,
                    retry_reason: None,
                    error: None,
                    output_preview: None,
                },
                DelegationSubRunSummary {
                    sub_run_id: "run-2".to_string(),
                    agent_id: "reviewer".to_string(),
                    status: "failed".to_string(),
                    retry_of: Some("run-1".to_string()),
                    attempt: 2,
                    retry_reason: Some("oops".to_string()),
                    error: Some("oops".to_string()),
                    output_preview: None,
                },
            ],
            ..DelegationHistoryEntry::default()
        }]);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("del-1"));
        assert!(lines[0].contains("parent"));
        assert!(lines[1].contains("coder"));
        assert!(lines[2].contains("reviewer"));
        assert!(lines[2].contains("retry #2"));
    }

    #[test]
    fn format_delegation_event_brief_formats_sub_run_started() {
        let event = JournalEvent::delegation_sub_run_started(
            Some("sid"),
            "del-1",
            "run-2",
            "run-parent",
            "coder",
            "running",
            1,
            Some("run-1"),
        );
        let rendered = format_delegation_event_brief(&event).unwrap();
        assert!(rendered.contains("coder"));
        assert!(rendered.contains("run-2"));
        assert!(rendered.contains("retry of run-1"));
    }

    #[test]
    fn build_watch_snapshot_includes_delegations() {
        let snapshot = build_watch_snapshot(
            &[],
            &[DelegationHistoryEntry {
                delegation_id: "del-1".to_string(),
                pattern: "fan_out".to_string(),
                status: "completed".to_string(),
                ..DelegationHistoryEntry::default()
            }],
        );
        assert!(snapshot.contains("Journal-backed delegations (1)"));
        assert!(snapshot.contains("del-1"));
    }

    #[test]
    fn build_watch_snapshot_combines_spawned_agents_and_delegations() {
        let snapshot = build_watch_snapshot(
            &[make_agent(
                "coder",
                "run-1",
                AgentStatus::Running {
                    activity: "implementing".to_string(),
                },
            )],
            &[DelegationHistoryEntry {
                delegation_id: "del-1".to_string(),
                pattern: "fan_out".to_string(),
                status: "running".to_string(),
                ..DelegationHistoryEntry::default()
            }],
        );
        assert!(snapshot.contains("Spawned agents (1)"));
        assert!(snapshot.contains("coder"));
        assert!(snapshot.contains("Summary: 1 running, 0 completed, 0 failed"));
        assert!(snapshot.contains("Journal-backed delegations (1)"));
        assert!(snapshot.contains("del-1"));
    }

    #[test]
    fn build_watch_snapshot_shows_empty_state() {
        let snapshot = build_watch_snapshot(&[], &[]);
        assert!(snapshot.contains("no agents or delegations yet"));
    }

    #[test]
    fn render_delegation_status_lines_highlights_parent_wait_and_failures() {
        let entry = DelegationHistoryEntry {
            delegation_id: "del-1".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            pattern: "fan_out".to_string(),
            agent_ids: vec!["coder".to_string(), "reviewer".to_string()],
            total_sub_runs: 2,
            succeeded: 1,
            failed: 1,
            retry_count: 1,
            status: "running".to_string(),
            aggregated_output_preview: None,
            retries: vec![DelegationRetrySummary {
                original_run_id: "run-1".to_string(),
                retry_run_id: "run-2".to_string(),
                agent_id: "reviewer".to_string(),
                attempt: 2,
                reason: "needs another pass".to_string(),
            }],
            sub_runs: vec![DelegationSubRunSummary {
                sub_run_id: "run-2".to_string(),
                agent_id: "reviewer".to_string(),
                status: "failed".to_string(),
                retry_of: Some("run-1".to_string()),
                attempt: 2,
                retry_reason: Some("needs another pass".to_string()),
                error: Some("permission denied".to_string()),
                output_preview: None,
            }],
            last_seen_index: 0,
        };
        let events = vec![
            JournalEvent::delegation_started(
                Some("sid"),
                "del-1",
                "run-parent",
                "fan_out",
                &["coder".to_string(), "reviewer".to_string()],
            ),
            JournalEvent::delegation_retry(
                Some("sid"),
                "del-1",
                "run-1",
                "run-2",
                "reviewer",
                2,
                "needs another pass",
            ),
        ];

        let rendered = render_delegation_status_lines(&entry, &events).join("\n");
        assert!(rendered.contains("Parent run: run-parent"));
        assert!(rendered.contains("Parent agent is paused"));
        assert!(rendered.contains("Waiting for aggregated output"));
        assert!(rendered.contains("Retries: 1"));
        assert!(rendered.contains("Retry lineage"));
        assert!(rendered.contains("run-1 -> run-2"));
        assert!(rendered.contains("permission denied"));
        assert!(rendered.contains("Recent lifecycle"));
        assert!(rendered.contains("retry #2"));
    }

    #[test]
    fn render_delegation_status_lines_falls_back_to_single_success_output() {
        let entry = DelegationHistoryEntry {
            delegation_id: "del-2".to_string(),
            parent_run_id: None,
            pattern: "sequential".to_string(),
            agent_ids: vec!["coder".to_string()],
            total_sub_runs: 1,
            succeeded: 1,
            failed: 0,
            retry_count: 0,
            status: "completed".to_string(),
            aggregated_output_preview: None,
            retries: vec![],
            sub_runs: vec![DelegationSubRunSummary {
                sub_run_id: "run-1".to_string(),
                agent_id: "coder".to_string(),
                status: "completed".to_string(),
                retry_of: None,
                attempt: 1,
                retry_reason: None,
                error: None,
                output_preview: Some("single-agent final answer".to_string()),
            }],
            last_seen_index: 0,
        };

        let rendered = render_delegation_status_lines(&entry, &[]).join("\n");
        assert!(rendered.contains("using successful sub-run output"));
        assert!(rendered.contains("single-agent final answer"));
    }

    #[tokio::test]
    async fn wait_for_watch_snapshot_change_detects_spawned_agent_events() {
        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));
        let spawner = Arc::new(DynamicAgentSpawner::new(router));
        let mut rx = Some(spawner.subscribe_progress());
        let last_snapshot = build_watch_snapshot(&[], &[]);
        let context = SpawnContext {
            parent_run_id: "root-run".to_string(),
            parent_agent_id: "main".to_string(),
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
        };
        let input = SpawnAgentInput {
            description: "watch test agent".to_string(),
            prompt: "do nothing".to_string(),
            agent_type: "task".to_string(),
            model: None,
            background: true,
            name: None,
            max_turns: None,
            isolated: false,
            allowed_tools: None,
        };

        let output = spawner.spawn(input, &context).await.unwrap();
        let agent_id = match output {
            astra_runtime::orchestration::SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };

        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_watch_snapshot_change(
                Some(&spawner),
                &mut rx,
                None,
                &last_snapshot,
                std::time::Duration::from_millis(10),
            ),
        )
        .await
        .unwrap();

        assert!(snapshot.contains("Spawned agents (1)"));
        assert!(snapshot.contains(agent_id.split('@').next().unwrap_or("watch")));
    }

    #[tokio::test]
    async fn wait_for_watch_snapshot_change_detects_journal_updates() {
        let sid = format!("slash-agent-watch-test-{}", uuid::Uuid::new_v4());
        let last_snapshot = build_watch_snapshot(&[], &[]);
        let mut rx = None;
        let sid_for_writer = sid.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let writer = session_journal::JournalWriter::new(&sid_for_writer).unwrap();
            writer
                .append(&JournalEvent::delegation_started(
                    Some(&sid_for_writer),
                    "del-watch",
                    "run-parent",
                    "fan_out",
                    &["coder".to_string()],
                ))
                .unwrap();
        });

        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_watch_snapshot_change(
                None,
                &mut rx,
                Some(&sid),
                &last_snapshot,
                std::time::Duration::from_millis(10),
            ),
        )
        .await
        .unwrap();

        assert!(snapshot.contains("Journal-backed delegations (1)"));
        assert!(snapshot.contains("del-watch"));
    }
}
