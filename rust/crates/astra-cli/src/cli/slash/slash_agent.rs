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

use crate::cli::surface::delegation_event_surface::{
    delegation_event_id, delegation_sub_run_detail, project_delegation_completed,
    project_delegation_retry, project_delegation_started, project_delegation_sub_run_completed,
    project_delegation_sub_run_started,
};
use crate::cli::surface::run_status_surface::{
    RunStatusKind, run_status_icon, run_status_is_active, run_status_is_completed,
    run_status_is_done, run_status_is_failed, run_status_kind,
};
use crate::cli::theme;
use astra_runtime::orchestration::{AgentStatus, DynamicAgentSpawner, PermissionSummary};
use astra_services::session_journal::{self, JournalEventType};
use astra_turn_core::delegation_tree::{AgentTreeNode, render_agent_forest};
use astra_turn_core::orchestration_fanout_group::{
    AgentFanoutGroupProjection, AgentFanoutSlotStatus,
};
use crossterm::style::Stylize;
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
    let delegations = match load_recent_delegations(ctx.session_id.as_deref()) {
        Ok(delegations) => delegations,
        Err(error) => {
            eprintln!("  {}", error.yellow());
            return;
        }
    };
    if delegations.is_empty() {
        eprintln!(
            "\n  {}",
            "🤝 No delegation history in this session".magenta().bold()
        );
        eprintln!();
        return;
    }
    eprintln!("\n  {}", "🤝 Delegation History".magenta().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    for entry in &delegations {
        eprintln!(
            "  {} {} {} [{} ok / {} failed]",
            run_status_icon(&entry.status),
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
            eprintln!("    {}", preview.as_str().magenta());
        }
        for sub_run in &entry.sub_runs {
            eprintln!(
                "    {} {} {}",
                run_status_icon(&sub_run.status),
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
    let (mut recent_agents, fanout_groups) = if let Some(ref spawner) = ctx.spawner {
        (
            spawner.get_agent_history(None).await,
            spawner.list_fanout_groups().await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    recent_agents.sort_by_key(|agent| Reverse(agent.started_at));
    let (active_agents, completed_agents): (Vec<_>, Vec<_>) = recent_agents
        .into_iter()
        .partition(|agent| !is_terminal_agent_status(&agent.status));
    let (delegations, delegation_error) = match load_recent_delegations(ctx.session_id.as_deref()) {
        Ok(delegations) => (delegations, None),
        Err(error) => (Vec::new(), Some(error)),
    };

    if active_agents.is_empty()
        && completed_agents.is_empty()
        && fanout_groups.is_empty()
        && delegations.is_empty()
        && delegation_error.is_none()
    {
        eprintln!(
            "\n  {}",
            "🤖 No recent agents or delegations".magenta().bold()
        );
        eprintln!(
            "  {}",
            "Use `agent(action='spawn', ...)` or delegate to start multi-agent work.".dim()
        );
        eprintln!();
        return;
    }

    if !active_agents.is_empty() {
        print_agent_section("🤖 Active Spawned Agents", &active_agents);
    }
    if !fanout_groups.is_empty() {
        print_fanout_section(&fanout_groups);
    }
    if !completed_agents.is_empty() {
        print_agent_section("🕘 Recent Spawned Agents", &completed_agents);
    }
    if !delegations.is_empty() {
        print_delegation_section(&delegations);
    }
    if let Some(error) = delegation_error {
        eprintln!("\n  {}", error.yellow());
    }
    eprintln!();
}

async fn show_tree(ctx: &AgentCommandContext) {
    let (agents, fanout_groups) = if let Some(ref spawner) = ctx.spawner {
        (
            spawner.list_all_agents().await,
            spawner.list_fanout_groups().await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let (delegations, delegation_error) = match load_recent_delegations(ctx.session_id.as_deref()) {
        Ok(delegations) => (delegations, None),
        Err(error) => (Vec::new(), Some(error)),
    };

    if agents.is_empty()
        && fanout_groups.is_empty()
        && delegations.is_empty()
        && delegation_error.is_none()
    {
        eprintln!(
            "\n  {}",
            "🌲 No agent or delegation tree available".magenta().bold()
        );
        eprintln!();
        return;
    }

    eprintln!("\n  {}", "🌲 Agent Delegation Tree".magenta().bold());
    eprintln!("  {}", "─".repeat(60).dim());

    if !fanout_groups.is_empty() {
        eprintln!("  {}", "Agent fanout groups".white().bold());
        for line in render_fanout_groups(&fanout_groups) {
            eprintln!("  {}", line);
        }
    }

    if !agents.is_empty() {
        if !fanout_groups.is_empty() {
            eprintln!();
        }
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
    if let Some(error) = delegation_error {
        eprintln!();
        eprintln!("  {}", error.yellow());
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
                "🤖 Agent".magenta().bold(),
                state.agent_id.as_str().white().bold()
            );
            eprintln!("  {}", "─".repeat(50).dim());
            eprintln!("  {} {}", "Type:".white().bold(), state.agent_type);
            eprintln!(
                "  {} {}",
                "Description:".white().bold(),
                state.description.as_str().magenta()
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
            if let Some(slot) = state.fanout_slot.as_ref() {
                eprintln!(
                    "  {} {} slot {}/{}",
                    "Fanout:".white().bold(),
                    slot.group_id.as_str().magenta(),
                    slot.slot_index + 1,
                    slot.target_count
                );
            }

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

            eprintln!("\n  {}", "📊 Metrics".magenta().bold());
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
            eprintln!("\n  {}", "🔐 Permissions".magenta().bold());
            eprintln!("  {}", "─".repeat(30).dim());
            print_permission_summary(&state.permission_summary, &state.metrics);

            eprintln!();
        }
        return;
    }

    let delegation = match load_delegation_entry_with_events(ctx.session_id.as_deref(), agent_id) {
        Ok(delegation) => delegation,
        Err(error) => {
            eprintln!("  {}", error.yellow());
            return;
        }
    };
    if let Some((entry, events)) = delegation {
        eprintln!(
            "\n  {} {}",
            "🤝 Delegation".magenta().bold(),
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
                "🔐 Permissions for".magenta().bold(),
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
                    state.metrics.permission_requests.to_string().magenta(),
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
        None => match find_delegation_entry(ctx.session_id.as_deref(), agent_id) {
            Ok(Some(entry)) => {
                eprintln!(
                        "  {}",
                        format!(
                            "Permissions are only tracked for spawned agents. Delegation '{}' is journal-backed; use /agent status or /agent logs.",
                            entry.delegation_id
                        )
                        .yellow()
                    );
            }
            Ok(None) => {
                eprintln!(
                    "  {}",
                    format!("Agent or delegation not found: {agent_id}").yellow()
                );
            }
            Err(error) => {
                eprintln!("  {}", error.yellow());
            }
        },
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
        match find_delegation_entry(ctx.session_id.as_deref(), agent_id) {
            Ok(Some(entry)) => {
                eprintln!(
                    "  {}",
                    format!(
                        "Delegation '{}' runs synchronously under its parent agent and cannot be stopped via /agent stop. Cancel the parent run while it is active instead.",
                        entry.delegation_id
                    )
                    .yellow()
                );
            }
            Ok(None) => {
                eprintln!(
                    "  {}",
                    format!("Agent or delegation not found: {agent_id}").yellow()
                );
            }
            Err(error) => {
                eprintln!("  {}", error.yellow());
            }
        }
        return;
    }

    // Update status to cancelled (user-driven via `/agent cancel`).
    spawner
        .update_status(
            agent_id,
            AgentStatus::cancelled_by_user("user-requested via /agent cancel"),
        )
        .await;

    eprintln!(
        "  {} Shutdown request sent to {}",
        theme::icon_ok(),
        agent_id.white().bold()
    );
}

/// Watch live spawned-agent updates plus journal-backed delegation changes.
/// Throttles rendering to max once per 500ms.
async fn show_watch(ctx: &AgentCommandContext) {
    use std::time::Duration;

    eprintln!(
        "\n  {} Watching agent tree (Ctrl+C to stop)\n",
        "👁".magenta()
    );
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
    let mut last_snapshot = build_watch_snapshot_for_session(
        &load_watch_agents(spawner_clone.as_ref()).await,
        &load_watch_fanout_groups(spawner_clone.as_ref()).await,
        ctx.session_id.as_deref(),
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
        match show_delegation_logs(ctx.session_id.as_deref(), agent_id) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                eprintln!("  {}", error.yellow());
                return;
            }
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
        match show_delegation_logs(ctx.session_id.as_deref(), agent_id) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                eprintln!("  {}", error.yellow());
                return;
            }
        }
        eprintln!(
            "  {}",
            format!("Agent or delegation not found: {agent_id}").yellow()
        );
        return;
    }

    eprintln!(
        "\n  {} Streaming logs for {} (Ctrl+C to stop)\n",
        "📋".magenta(),
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

                    if progress_event_ends_log_stream(&event.event_type) {
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

fn progress_event_ends_log_stream(
    event_type: &astra_runtime::orchestration::ProgressEventType,
) -> bool {
    event_type.is_terminal()
}

fn progress_event_should_refresh_watch_snapshot(
    event_type: &astra_runtime::orchestration::ProgressEventType,
) -> bool {
    event_type.is_terminal()
        || matches!(
            event_type,
            astra_runtime::orchestration::ProgressEventType::AgentSpawned { .. }
                | astra_runtime::orchestration::ProgressEventType::PermissionDenied { .. }
        )
}

fn show_delegation_logs(session_id: Option<&str>, query: &str) -> Result<bool, String> {
    let Some((delegation_id, events)) = load_delegation_events(session_id, query)? else {
        return Ok(false);
    };
    eprintln!(
        "\n  {} {}\n",
        "📋 Delegation logs for".magenta(),
        delegation_id.as_str().white().bold()
    );
    for event in events {
        match event.event_type {
            JournalEventType::DelegationStarted => {
                let projection = project_delegation_started(event.metadata.as_ref());
                eprintln!(
                    "  [{}] {} started {} ({} agents)",
                    event.ts.dim(),
                    "▶".green(),
                    projection.pattern,
                    projection.agent_count
                );
            }
            JournalEventType::DelegationRetry => {
                let projection = project_delegation_retry(event.metadata.as_ref());
                eprintln!(
                    "  [{}] {} {} retry #{} — {}",
                    event.ts.dim(),
                    "↻".yellow(),
                    projection.agent_id,
                    projection.attempt,
                    projection.reason
                );
            }
            JournalEventType::DelegationSubRunCompleted => {
                let projection = project_delegation_sub_run_completed(event.metadata.as_ref());
                eprintln!(
                    "  [{}] {} {} [{}]",
                    event.ts.dim(),
                    run_status_icon(&projection.status),
                    projection.agent_id,
                    projection.status
                );
                if let Some(preview) = projection.output_preview.as_deref() {
                    eprintln!("      {}", preview.magenta());
                }
                if let Some(error) = projection.error.as_deref() {
                    eprintln!("      {}", error.red());
                }
            }
            JournalEventType::DelegationCompleted => {
                let projection = project_delegation_completed(event.metadata.as_ref());
                eprintln!(
                    "  [{}] {} completed [{} ok / {} failed, status={}]",
                    event.ts.dim(),
                    run_status_icon(&projection.aggregated_status),
                    projection.succeeded,
                    projection.failed,
                    projection.aggregated_status
                );
                if let Some(preview) = projection.aggregated_output_preview.as_deref() {
                    eprintln!("      {}", preview.magenta());
                }
            }
            _ => {}
        }
    }
    eprintln!();
    Ok(true)
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
                "◆".magenta(),
                turn.to_string().as_str().magenta(),
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
        ProgressEventType::Interrupted {
            reason,
            partial_summary,
            total_tool_calls,
            duration_ms,
            ..
        } => {
            format!(
                "{} ({} tools, {}ms): {} [{}]",
                "⏸ Interrupted".yellow(),
                total_tool_calls,
                duration_ms,
                partial_summary,
                reason
            )
        }
        ProgressEventType::Failed { error } => format!("{} {}", "✗ Failed:".red(), error),
        ProgressEventType::Waiting { reason } => {
            format!("{} {}", "⏸ Waiting:".yellow(), reason)
        }
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
            format!("🔧 turn={} → {}", turn, tool_name.as_str().magenta())
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
                agent_type.as_str().magenta(),
                description,
                parent_label.dim()
            )
        }
    };

    eprintln!("  [{}] {}", time_str.as_str().dim(), msg);
}

fn show_help() {
    eprintln!("\n  {}", "🤖 Agent Commands".magenta().bold());
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
        "Spawned agents come from `agent(action='spawn', ...)`; delegations come from the delegate tool.".dim()
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
        AgentStatus::Waiting { .. } => "⏸",
        AgentStatus::Cancelled { .. } => "🛑",
    }
}

fn format_status(status: &AgentStatus) -> String {
    match status {
        AgentStatus::Initializing => "initializing".to_string(),
        AgentStatus::Running { activity } => format!("running: {activity}"),
        AgentStatus::Idle => "idle".to_string(),
        AgentStatus::Completed { result, .. } => {
            let preview = if result.chars().count() > 50 {
                format!("{}...", result.chars().take(50).collect::<String>())
            } else {
                result.clone()
            };
            format!("completed: {preview}")
        }
        AgentStatus::Failed { error, .. } => format!("failed: {error}"),
        AgentStatus::Waiting { reason } => format!("waiting: {reason}"),
        AgentStatus::Cancelled { by_user, reason } => {
            if reason.is_empty() {
                if *by_user {
                    "cancelled by user".to_string()
                } else {
                    "cancelled".to_string()
                }
            } else {
                format!("cancelled: {reason}")
            }
        }
    }
}

pub(crate) fn format_duration(d: std::time::Duration) -> String {
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
        AgentStatus::Completed { .. } | AgentStatus::Failed { .. } | AgentStatus::Cancelled { .. }
    )
}

fn print_agent_section(title: &str, agents: &[astra_runtime::orchestration::SpawnedAgentInfo]) {
    eprintln!("\n  {}", title.magenta().bold());
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
        eprintln!("    {}", agent.description.as_str().magenta());
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

fn print_fanout_section(groups: &[AgentFanoutGroupProjection]) {
    eprintln!("\n  {}", "🧩 Agent Fanout Groups".magenta().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    for line in render_fanout_groups(groups) {
        eprintln!("  {line}");
    }
}

fn print_delegation_section(entries: &[DelegationHistoryEntry]) {
    eprintln!("\n  {}", "🤝 Recent Delegations".magenta().bold());
    eprintln!("  {}", "─".repeat(60).dim());
    for entry in entries {
        eprintln!(
            "  {} {} {} [{} ok / {} failed]",
            run_status_icon(&entry.status),
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
            eprintln!("    {}", preview.as_str().magenta());
        }
    }
}

fn render_fanout_groups(groups: &[AgentFanoutGroupProjection]) -> Vec<String> {
    let mut groups = groups.to_vec();
    groups.sort_by(|a, b| a.group_id.cmp(&b.group_id));
    let mut lines = Vec::new();
    for group in groups {
        lines.push(format!("{} {}", "▣".magenta(), group.summary_sentence()));
        for slot in &group.slots {
            let agent = slot
                .agent_id
                .as_deref()
                .map(shorten_run_id)
                .unwrap_or_else(|| "-".to_string());
            let description = if slot.requested_description.trim().is_empty() {
                slot.role.as_str()
            } else {
                slot.requested_description.as_str()
            };
            lines.push(format!(
                "  {}. {:<22} {:<26} {}",
                slot.slot_index + 1,
                fanout_slot_status_label(slot.status).dim(),
                description,
                agent.dim()
            ));
        }
    }
    lines
}

fn fanout_slot_status_label(status: AgentFanoutSlotStatus) -> &'static str {
    match status {
        AgentFanoutSlotStatus::Planned => "planned",
        AgentFanoutSlotStatus::SpawnAccepted | AgentFanoutSlotStatus::Running => "running",
        AgentFanoutSlotStatus::SpawnRejected => "spawn rejected",
        AgentFanoutSlotStatus::Completed => "completed",
        AgentFanoutSlotStatus::Failed => "failed",
        AgentFanoutSlotStatus::CancelledByUser => "stopped by user",
        AgentFanoutSlotStatus::CancelledByParentBudget => "cancelled by parent budget",
        AgentFanoutSlotStatus::TimedOut => "timed out",
    }
}

fn render_delegation_tree(entries: &[DelegationHistoryEntry]) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        // Build aggregated progress summary: "3/5 done, 1 failed"
        let done_count = entry
            .sub_runs
            .iter()
            .filter(|s| run_status_is_done(&s.status))
            .count();
        let failed_count = entry
            .sub_runs
            .iter()
            .filter(|s| run_status_is_failed(&s.status))
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
            run_status_icon(&entry.status),
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
                run_status_icon(&sub_run.status),
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

async fn load_watch_fanout_groups(
    spawner: Option<&Arc<DynamicAgentSpawner>>,
) -> Vec<AgentFanoutGroupProjection> {
    if let Some(spawner) = spawner {
        spawner.list_fanout_groups().await
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
    use tokio::sync::broadcast::error::RecvError;

    let mut interval = tokio::time::interval(poll_interval);
    loop {
        let should_refresh = tokio::select! {
            _ = interval.tick() => true,
            event = recv_watch_event(rx) => {
                match event {
                    Ok(event) => progress_event_should_refresh_watch_snapshot(&event.event_type),
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
        let snapshot = build_watch_snapshot_for_session(
            &load_watch_agents(spawner).await,
            &load_watch_fanout_groups(spawner).await,
            session_id,
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
    build_watch_snapshot_with_fanout(agents, &[], delegations)
}

fn build_watch_snapshot_with_fanout(
    agents: &[astra_runtime::orchestration::SpawnedAgentInfo],
    fanout_groups: &[AgentFanoutGroupProjection],
    delegations: &[DelegationHistoryEntry],
) -> String {
    let mut lines = Vec::new();

    // Add overall summary at the top when there are delegations
    if !delegations.is_empty() {
        let total_subruns: usize = delegations
            .iter()
            .map(|d| d.total_sub_runs.max(d.sub_runs.len()))
            .sum();
        let done_subruns: usize = delegations
            .iter()
            .flat_map(|d| &d.sub_runs)
            .filter(|s| run_status_is_done(&s.status))
            .count();
        let failed_subruns: usize = delegations
            .iter()
            .flat_map(|d| &d.sub_runs)
            .filter(|s| run_status_is_failed(&s.status))
            .count();
        let running_subruns: usize = delegations
            .iter()
            .flat_map(|d| &d.sub_runs)
            .filter(|s| run_status_is_active(&s.status))
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

    if !fanout_groups.is_empty() {
        lines.push(format!("  Agent fanout groups ({})", fanout_groups.len()));
        lines.extend(
            render_fanout_groups(fanout_groups)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    if !agents.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
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

fn build_watch_snapshot_for_session(
    agents: &[astra_runtime::orchestration::SpawnedAgentInfo],
    fanout_groups: &[AgentFanoutGroupProjection],
    session_id: Option<&str>,
) -> String {
    match load_recent_delegations(session_id) {
        Ok(delegations) => build_watch_snapshot_with_fanout(agents, fanout_groups, &delegations),
        Err(error) => {
            let mut snapshot = build_watch_snapshot_with_fanout(agents, fanout_groups, &[]);
            snapshot.push_str("\n\n");
            snapshot.push_str(&format!("  warning: {error}"));
            snapshot
        }
    }
}

fn print_watch_snapshot(snapshot: &str) {
    eprint!("\x1b[2J\x1b[H");
    eprintln!("  {}", "🌲 Agent Delegation Tree".magenta().bold());
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

fn read_session_journal_events(
    session_id: &str,
) -> Result<Vec<session_journal::JournalEvent>, String> {
    session_journal::read_journal(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))
}

fn collect_recent_delegations(
    events: &[session_journal::JournalEvent],
) -> Vec<DelegationHistoryEntry> {
    let mut delegations: HashMap<String, DelegationHistoryEntry> = HashMap::new();
    for (idx, event) in events.iter().enumerate() {
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        let Some(delegation_id) = delegation_event_id(Some(metadata)) else {
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
                let projection = project_delegation_started(Some(metadata));
                entry.parent_run_id = projection.parent_run_id;
                entry.pattern = projection.pattern;
                entry.agent_ids = projection.agent_ids;
                entry.total_sub_runs = projection.agent_count;
                if entry.status.is_empty() {
                    entry.status = "running".to_string();
                }
            }
            JournalEventType::DelegationSubRunStarted => {
                let started = project_delegation_sub_run_started(Some(metadata));
                let sub_run_id = started.sub_run_id.clone();
                let started = DelegationSubRunSummary {
                    sub_run_id: sub_run_id.clone(),
                    agent_id: started.agent_id,
                    status: started.status,
                    retry_of: started.retry_of,
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
                let projection = project_delegation_sub_run_completed(Some(metadata));
                let sub_run_id = projection.sub_run_id.clone();
                let sub_run = DelegationSubRunSummary {
                    sub_run_id: sub_run_id.clone(),
                    agent_id: projection.agent_id,
                    status: projection.status,
                    retry_of: None,
                    attempt: 1,
                    retry_reason: None,
                    error: projection.error,
                    output_preview: projection.output_preview,
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
                let projection = project_delegation_retry(Some(metadata));
                let retry = DelegationRetrySummary {
                    original_run_id: projection.original_run_id,
                    retry_run_id: projection.retry_run_id,
                    agent_id: projection.agent_id,
                    attempt: projection.attempt,
                    reason: projection.reason,
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
                let projection = project_delegation_completed(Some(metadata));
                entry.pattern = projection.pattern;
                entry.total_sub_runs = if projection.total_sub_runs == 0 {
                    entry.total_sub_runs
                } else {
                    projection.total_sub_runs
                };
                entry.succeeded = projection.succeeded;
                entry.failed = projection.failed;
                entry.status = projection.aggregated_status;
                entry.aggregated_output_preview = projection.aggregated_output_preview;
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

fn load_recent_delegations(
    session_id: Option<&str>,
) -> Result<Vec<DelegationHistoryEntry>, String> {
    let Some(session_id) = session_id else {
        return Ok(Vec::new());
    };
    let events = read_session_journal_events(session_id)?;
    Ok(collect_recent_delegations(&events))
}

fn load_delegation_entry_with_events(
    session_id: Option<&str>,
    query: &str,
) -> Result<Option<(DelegationHistoryEntry, Vec<session_journal::JournalEvent>)>, String> {
    let Some(session_id) = session_id else {
        return Ok(None);
    };
    let events = read_session_journal_events(session_id)?;
    let Some(entry) = collect_recent_delegations(&events)
        .into_iter()
        .find(|entry| entry.delegation_id == query || entry.delegation_id.starts_with(query))
    else {
        return Ok(None);
    };
    let delegation_id = entry.delegation_id.clone();
    let filtered = events
        .into_iter()
        .filter(|event| {
            delegation_event_id(event.metadata.as_ref()).as_deref() == Some(delegation_id.as_str())
        })
        .collect();
    Ok(Some((entry, filtered)))
}

fn find_delegation_entry(
    session_id: Option<&str>,
    query: &str,
) -> Result<Option<DelegationHistoryEntry>, String> {
    Ok(load_recent_delegations(session_id)?
        .into_iter()
        .find(|entry| entry.delegation_id == query || entry.delegation_id.starts_with(query)))
}

fn load_delegation_events(
    session_id: Option<&str>,
    query: &str,
) -> Result<Option<(String, Vec<session_journal::JournalEvent>)>, String> {
    Ok(load_delegation_entry_with_events(session_id, query)?
        .map(|(entry, events)| (entry.delegation_id, events)))
}

fn delegation_parent_lifecycle_note(status: &str) -> &'static str {
    match run_status_kind(status) {
        RunStatusKind::Running => {
            "Parent agent is paused while delegated sub-runs execute and wait for aggregation."
        }
        RunStatusKind::Completed => {
            "Delegated sub-runs finished and the parent agent can continue from the aggregated result below."
        }
        RunStatusKind::Unfinished => {
            "Delegated sub-runs yielded unfinished results (waiting/paused); no final aggregate was produced."
        }
        RunStatusKind::Partial
        | RunStatusKind::CompletedWithConflicts
        | RunStatusKind::CompletedOverBudget => {
            "Delegated sub-runs finished with failures; the parent agent can continue, but review the failed sub-runs below."
        }
        RunStatusKind::Failed | RunStatusKind::Timeout => {
            "Delegation failed before a clean aggregate was produced; review the lifecycle and failures below."
        }
        RunStatusKind::Cancelled | RunStatusKind::Interrupted => {
            "Delegation was cancelled before aggregation completed; the parent agent cannot use a final delegated result."
        }
        _ => {
            "Delegation state is journal-backed; inspect the lifecycle below for the latest progress."
        }
    }
}

fn format_delegation_event_brief(event: &session_journal::JournalEvent) -> Option<String> {
    match event.event_type {
        JournalEventType::DelegationStarted => {
            let projection = project_delegation_started(event.metadata.as_ref());
            Some(format!(
                "[{}] ▶ started {} ({} agents)",
                event.ts, projection.pattern, projection.agent_count
            ))
        }
        JournalEventType::DelegationSubRunStarted => {
            let projection = project_delegation_sub_run_started(event.metadata.as_ref());
            let retry_suffix = projection
                .retry_of
                .as_deref()
                .map(|retry_of| format!(" (retry of {})", retry_of))
                .unwrap_or_default();
            Some(format!(
                "[{}] ↳ {} running {}{}",
                event.ts, projection.agent_id, projection.sub_run_id, retry_suffix
            ))
        }
        JournalEventType::DelegationRetry => {
            let projection = project_delegation_retry(event.metadata.as_ref());
            Some(format!(
                "[{}] ↻ {} retry #{}{}",
                event.ts,
                projection.agent_id,
                projection.attempt,
                if projection.reason.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", projection.reason)
                }
            ))
        }
        JournalEventType::DelegationSubRunCompleted => {
            let projection = project_delegation_sub_run_completed(event.metadata.as_ref());
            let detail = delegation_sub_run_detail(&projection)
                .map(|msg| crate::cli::effects::truncate_label(msg, 120))
                .unwrap_or_default();
            Some(format!(
                "[{}] {} {} [{}]{}",
                event.ts,
                run_status_icon(&projection.status),
                projection.agent_id,
                projection.status,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            ))
        }
        JournalEventType::DelegationCompleted => {
            let projection = project_delegation_completed(event.metadata.as_ref());
            let preview = projection
                .aggregated_output_preview
                .as_deref()
                .map(|msg| crate::cli::effects::truncate_label(msg, 120))
                .unwrap_or_default();
            Some(format!(
                "[{}] {} completed [{} ok / {} failed, status={}]{}",
                event.ts,
                run_status_icon(&projection.aggregated_status),
                projection.succeeded,
                projection.failed,
                projection.aggregated_status,
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
    } else if run_status_is_active(&entry.status) {
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
        .filter(|sub_run| run_status_is_failed(&sub_run.status) || sub_run.error.is_some())
        .collect();
    if !failed_sub_runs.is_empty() {
        lines.push(String::new());
        lines.push("❌ Failures".to_string());
        for sub_run in failed_sub_runs {
            lines.push(format!(
                "  {} {} [{}]",
                run_status_icon(&sub_run.status),
                sub_run.agent_id,
                sub_run.status
            ));
            lines.push(format!(
                "    {}",
                sub_run
                    .error
                    .as_deref()
                    .map(|error| crate::cli::effects::truncate_label(error, 160))
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
                    crate::cli::effects::truncate_label(&retry.reason, 160)
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
                run_status_icon(&sub_run.status),
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
                    crate::cli::effects::truncate_label(retry_reason, 160)
                ));
            }
            if let Some(preview) = &sub_run.output_preview {
                lines.push(format!(
                    "    output: {}",
                    crate::cli::effects::truncate_label(preview, 160)
                ));
            }
            if let Some(error) = &sub_run.error {
                lines.push(format!(
                    "    error: {}",
                    crate::cli::effects::truncate_label(error, 160)
                ));
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
        .filter(|sub_run| run_status_is_completed(&sub_run.status))
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
    use super::{
        AgentCommandContext, AgentFanoutGroupProjection, DelegationHistoryEntry,
        DelegationRetrySummary, DelegationSubRunSummary, build_watch_snapshot,
        build_watch_snapshot_for_session, build_watch_snapshot_with_fanout,
        delegation_parent_lifecycle_note, format_delegation_event_brief, format_duration,
        format_status, handle_agent_command, load_delegation_events, load_recent_delegations,
        progress_event_ends_log_stream, progress_event_should_refresh_watch_snapshot,
        render_delegation_status_lines, render_delegation_tree, wait_for_watch_snapshot_change,
    };
    use astra_messaging::{AgentMailboxRouter, InProcessTransport};
    use astra_runtime::orchestration::{
        AgentStatus, DynamicAgentSpawner, SpawnAgentInput, SpawnContext, SpawnedAgentInfo,
        SpawnedAgentMetrics,
    };
    use astra_runtime::server::delegation::engine::DelegationTracker;
    use astra_services::session_journal::{self, JournalEvent, JournalEventType};
    use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotStatus;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;

    struct PendingWatchExecutor;

    #[async_trait::async_trait]
    impl astra_runtime::orchestration::SpawnAgentExecutor for PendingWatchExecutor {
        async fn execute(
            &self,
            _config: astra_runtime::orchestration::SpawnRunConfig,
        ) -> Result<astra_runtime::orchestration::SpawnRunResult, String> {
            std::future::pending::<Result<astra_runtime::orchestration::SpawnRunResult, String>>()
                .await
        }
    }

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
            run_in_background: false,
            fanout_slot: None,
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

        let delegations = load_recent_delegations(Some(&sid)).unwrap();
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

        let (delegation_id, events) = load_delegation_events(Some(&sid), "del-log")
            .unwrap()
            .unwrap();
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

        let delegations = load_recent_delegations(Some(&sid)).unwrap();
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].status, "running");
        assert_eq!(delegations[0].sub_runs.len(), 1);
        assert_eq!(delegations[0].sub_runs[0].sub_run_id, "run-1");
        assert_eq!(delegations[0].sub_runs[0].status, "running");
    }

    #[test]
    fn load_recent_delegations_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("slash-agent-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let error = load_recent_delegations(Some(&sid))
            .expect_err("directory journal path should surface an error");

        assert!(error.contains("failed to read session journal"), "{error}");
    }

    #[test]
    fn load_delegation_events_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("slash-agent-events-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let error = load_delegation_events(Some(&sid), "del-1")
            .expect_err("directory journal path should surface an error");

        assert!(error.contains("failed to read session journal"), "{error}");
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
    fn build_watch_snapshot_with_fanout_shows_group_summary_and_slots() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "review fanout", 3);
        group
            .set_slot_request(0, "auth", "review auth flow")
            .unwrap();
        group.record_spawn_accepted(0, "auth@run-1").unwrap();
        group
            .set_slot_request(1, "storage", "review storage")
            .unwrap();
        group.record_spawn_rejected(1, "model denied").unwrap();

        let snapshot = build_watch_snapshot_with_fanout(&[], &[group], &[]);

        assert!(snapshot.contains("Agent fanout groups (1)"), "{snapshot}");
        assert!(
            snapshot.contains("3-agent fanout failed to start fully"),
            "{snapshot}"
        );
        assert!(snapshot.contains("review auth flow"), "{snapshot}");
        assert!(snapshot.contains("review storage"), "{snapshot}");
        assert!(snapshot.contains("spawn rejected"), "{snapshot}");
    }

    #[test]
    fn build_watch_snapshot_with_fanout_names_parent_budget_cancellation() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "review fanout", 2);
        group
            .set_slot_request(0, "auth", "review auth flow")
            .unwrap();
        group.record_spawn_accepted(0, "auth@run-1").unwrap();
        group
            .record_terminal_by_agent(
                "auth@run-1",
                AgentFanoutSlotStatus::CancelledByParentBudget,
                Some("turn budget exhausted".to_string()),
            )
            .unwrap();

        let snapshot = build_watch_snapshot_with_fanout(&[], &[group], &[]);

        assert!(
            snapshot.contains("cancelled by parent budget"),
            "{snapshot}"
        );
        assert!(!snapshot.contains("cancelled by budget"), "{snapshot}");
    }

    #[test]
    fn build_watch_snapshot_shows_empty_state() {
        let snapshot = build_watch_snapshot(&[], &[]);
        assert!(snapshot.contains("no agents or delegations yet"));
    }

    #[test]
    fn build_watch_snapshot_for_session_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("slash-agent-watch-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let snapshot = build_watch_snapshot_for_session(&[], &[], Some(&sid));

        assert!(
            snapshot.contains("warning: failed to read session journal"),
            "{snapshot}"
        );
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

    #[test]
    fn delegation_parent_lifecycle_note_uses_shared_run_status_taxonomy() {
        assert!(
            delegation_parent_lifecycle_note("unfinished").contains("yielded unfinished results")
        );
        assert!(
            delegation_parent_lifecycle_note("partial_failure").contains("finished with failures")
        );
        assert!(
            delegation_parent_lifecycle_note("completed_over_budget")
                .contains("finished with failures")
        );
        assert!(
            delegation_parent_lifecycle_note("interrupted")
                .contains("cancelled before aggregation completed")
        );
    }

    #[test]
    fn render_delegation_status_lines_handles_legacy_and_interrupted_statuses() {
        let entry = DelegationHistoryEntry {
            delegation_id: "del-legacy".to_string(),
            parent_run_id: Some("run-parent".to_string()),
            pattern: "fan_out".to_string(),
            agent_ids: vec!["coder".to_string(), "reviewer".to_string()],
            total_sub_runs: 2,
            succeeded: 1,
            failed: 1,
            retry_count: 0,
            status: "partial_failure".to_string(),
            aggregated_output_preview: Some("partial aggregate".to_string()),
            retries: vec![],
            sub_runs: vec![
                DelegationSubRunSummary {
                    sub_run_id: "run-1".to_string(),
                    agent_id: "coder".to_string(),
                    status: "completed".to_string(),
                    retry_of: None,
                    attempt: 1,
                    retry_reason: None,
                    error: None,
                    output_preview: Some("ok".to_string()),
                },
                DelegationSubRunSummary {
                    sub_run_id: "run-2".to_string(),
                    agent_id: "reviewer".to_string(),
                    status: "interrupted".to_string(),
                    retry_of: None,
                    attempt: 1,
                    retry_reason: None,
                    error: Some("cancelled by parent".to_string()),
                    output_preview: None,
                },
            ],
            last_seen_index: 0,
        };
        let events = vec![JournalEvent::delegation_completed(
            Some("sid"),
            "del-legacy",
            "fan_out",
            2,
            1,
            1,
            "interrupted",
            None,
        )];

        let rendered = render_delegation_status_lines(&entry, &events).join("\n");

        assert!(rendered.contains("Status: partial_failure"));
        assert!(rendered.contains("finished with failures"));
        assert!(rendered.contains("partial aggregate"));
        assert!(rendered.contains("reviewer [interrupted]"));
        assert!(rendered.contains("cancelled by parent"));
        assert!(rendered.contains("status=interrupted"));
    }

    #[tokio::test]
    async fn wait_for_watch_snapshot_change_detects_spawned_agent_events() {
        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));
        let spawner = Arc::new(
            DynamicAgentSpawner::new(router).with_executor(Arc::new(PendingWatchExecutor)),
        );
        let mut rx = Some(spawner.subscribe_progress());
        let last_snapshot = build_watch_snapshot(&[], &[]);
        let context = SpawnContext {
            parent_run_id: "root-run".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
        };
        let input = SpawnAgentInput {
            description: "watch test agent".to_string(),
            prompt: "do nothing".to_string(),
            agent_type: "task".to_string(),
            run_in_background: true,
            ..Default::default()
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

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
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

    #[test]
    fn progress_event_helpers_treat_interrupted_as_terminal_refresh() {
        use astra_runtime::orchestration::ProgressEventType;

        let interrupted = ProgressEventType::Interrupted {
            reason: "budget_exhausted".to_string(),
            partial_summary: "partial".to_string(),
            total_tool_calls: 1,
            total_tokens: (2, 3),
            duration_ms: 4,
        };

        assert!(progress_event_ends_log_stream(&interrupted));
        assert!(progress_event_should_refresh_watch_snapshot(&interrupted));
    }
}
