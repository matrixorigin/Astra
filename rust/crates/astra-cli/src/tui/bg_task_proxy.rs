//! Background task proxy — data projection, persistence, and output queries.
//!
//! Pure data layer. No TUI type dependencies.
//! For TUI rendering, see [`super::bg_task_rendering`].

//! Background task proxy and projection functions.
//!
//! Bridges between [`super::background_tasks::BackgroundTaskRegistry`] and
//! the TUI rendering layer ([`super::bottom_pane::background_task_view`]).
//! Extracted from `event_loop.rs` to keep the event loop focused on
//! orchestration.

use std::{collections::BTreeMap, sync::Arc};

pub(crate) fn background_task_rejected_fanout_slot_id(group_id: &str, slot_index: usize) -> String {
    format!("fanout:{group_id}:slot:{slot_index}:spawn_rejected")
}

pub(crate) fn background_task_rejected_fanout_slot_label(
    slot: &astra_turn_core::orchestration_fanout_group::AgentFanoutSlot,
) -> String {
    let requested = slot.requested_description.trim();
    if !requested.is_empty() {
        return requested.to_string();
    }
    let role = slot.role.trim();
    if !role.is_empty() {
        return role.to_string();
    }
    let ordinal = slot.slot_index.saturating_add(1);
    format!("fanout slot {ordinal}")
}

pub(crate) fn background_local_agent_fanout_projection(
    slot: &astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity,
    group_title: Option<&str>,
    slot_label: &str,
) -> astra_services::session_workspace::BackgroundLocalAgentFanoutProjection {
    let group_title = group_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(slot.group_id.as_str())
        .to_string();
    astra_services::session_workspace::BackgroundLocalAgentFanoutProjection {
        group_id: slot.group_id.clone(),
        group_title,
        target_count: slot.target_count,
        slot_index: slot.slot_index,
        slot_label: slot_label.to_string(),
    }
}

pub(crate) const LOCAL_AGENT_OUTPUT_TAIL_CHARS: usize = 8192;

pub(crate) fn local_agent_status_projection(
    status: &astra_turn_core::orchestration_types::AgentStatus,
) -> (&'static str, Option<String>, Option<String>) {
    use astra_turn_core::orchestration_types::AgentStatus;

    match status {
        AgentStatus::Initializing => ("pending", None, None),
        AgentStatus::Running { activity } => (
            "running",
            Some(activity.clone()).filter(|activity| !activity.trim().is_empty()),
            None,
        ),
        AgentStatus::Idle => (
            "waiting_for_input",
            Some("Agent is waiting for input.".to_string()),
            None,
        ),
        AgentStatus::Waiting { reason } => (
            "waiting_for_input",
            Some(format!("Agent is waiting: {reason}")),
            None,
        ),
        AgentStatus::Completed {
            result,
            finish_reason,
        } => (
            "completed",
            Some(result.clone()).filter(|result| !result.trim().is_empty()),
            finish_reason.clone(),
        ),
        AgentStatus::Interrupted {
            partial_result,
            finish_reason,
        } => (
            "interrupted",
            Some(partial_result.clone()).filter(|result| !result.trim().is_empty()),
            Some(finish_reason.clone()),
        ),
        AgentStatus::Failed {
            error,
            finish_reason,
        } => (
            "failed",
            Some(error.clone()).filter(|error| !error.trim().is_empty()),
            finish_reason.clone().or_else(|| Some(error.clone())),
        ),
        AgentStatus::Cancelled { reason, .. } => (
            "killed",
            Some(reason.clone()).filter(|reason| !reason.trim().is_empty()),
            Some(if reason.trim().is_empty() {
                "cancelled".to_string()
            } else {
                reason.clone()
            }),
        ),
    }
}

pub(crate) fn local_agent_output_tail(output: Option<String>) -> Option<String> {
    output.and_then(|output| {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return None;
        }
        let char_count = output.chars().count();
        if char_count <= LOCAL_AGENT_OUTPUT_TAIL_CHARS {
            Some(output)
        } else {
            Some(
                output
                    .chars()
                    .skip(char_count.saturating_sub(LOCAL_AGENT_OUTPUT_TAIL_CHARS))
                    .collect(),
            )
        }
    })
}

pub(crate) fn background_local_agent_projection_from_info(
    agent: &astra_turn_core::orchestration_types::SpawnedAgentInfo,
    fanout_title: Option<&str>,
) -> Option<astra_services::session_workspace::BackgroundLocalAgentTaskProjection> {
    if !agent.run_in_background {
        return None;
    }

    let (status, output, terminal_reason) = local_agent_status_projection(&agent.status);
    let started_at_ms = agent
        .started_at
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);

    Some(
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: agent.agent_id.clone(),
            status: status.to_string(),
            title: agent.description.clone(),
            started_at_ms,
            ended_at_ms: None,
            output_tail: local_agent_output_tail(output),
            terminal_reason,
            fanout: agent.fanout_slot.as_ref().map(|slot| {
                background_local_agent_fanout_projection(slot, fanout_title, &agent.description)
            }),
        },
    )
}

pub(crate) async fn export_background_local_agent_task_projections(
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
) -> Vec<astra_services::session_workspace::BackgroundLocalAgentTaskProjection> {
    let mut by_id: BTreeMap<
        String,
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection,
    > = restored
        .iter()
        .cloned()
        .map(|projection| (projection.id.clone(), projection))
        .collect();

    if let Some(spawner) = agent_spawner {
        let fanout_titles = spawner
            .list_fanout_groups()
            .await
            .into_iter()
            .map(|group| (group.group_id, group.title))
            .collect::<BTreeMap<_, _>>();
        for agent in spawner.get_agent_history(None).await {
            let fanout_title = agent
                .fanout_slot
                .as_ref()
                .and_then(|slot| fanout_titles.get(&slot.group_id).map(String::as_str));
            if let Some(projection) =
                background_local_agent_projection_from_info(&agent, fanout_title)
            {
                by_id.insert(projection.id.clone(), projection);
            }
        }
    }

    let mut projections: Vec<_> = by_id.into_values().collect();
    projections.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms).then(a.id.cmp(&b.id)));
    projections
}

pub(crate) fn background_task_output_dir(session_id: Option<&str>) -> std::path::PathBuf {
    std::env::temp_dir().join("astra").join("bg_tasks").join(
        session_id
            .filter(|sid| !sid.is_empty())
            .unwrap_or("default"),
    )
}

pub(crate) fn restore_background_task_projections(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    session_id: Option<&str>,
) -> Vec<astra_services::session_workspace::BackgroundLocalAgentTaskProjection> {
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return Vec::new();
    };
    let workspace = match astra_services::session_workspace::read_workspace_optional(session_id) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read workspace background shell projections"
            );
            return Vec::new();
        }
    };
    let local_agent_projections = workspace.background_local_agent_tasks.clone();
    if let Err(error) =
        background_registry.restore_shell_task_projections(workspace.background_shell_tasks)
    {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "failed to restore workspace background shell projections"
        );
    }
    local_agent_projections
}

pub(crate) fn persist_background_task_projections_if_changed(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    session_id: Option<&str>,
    model: Option<&str>,
    last_persisted: &mut Vec<astra_services::session_workspace::BackgroundShellTaskProjection>,
) {
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return;
    };
    let projections = background_registry.export_shell_task_projections();
    if projections == *last_persisted {
        return;
    }

    let mut workspace = match astra_services::session_workspace::read_workspace_optional(session_id)
    {
        Ok(Some(workspace)) => workspace,
        Ok(None) => astra_services::session_workspace::WorkspaceMetadata::new(
            session_id,
            model.unwrap_or("default"),
        ),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read workspace before persisting background shell projections"
            );
            return;
        }
    };
    workspace.background_shell_tasks = projections.clone();
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    match astra_services::session_workspace::write_workspace(&workspace) {
        Ok(()) => *last_persisted = projections,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to persist background shell projections"
            );
        }
    }
}

pub(crate) async fn persist_background_local_agent_task_projections_if_changed(
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    session_id: Option<&str>,
    model: Option<&str>,
    last_persisted: &mut Vec<astra_services::session_workspace::BackgroundLocalAgentTaskProjection>,
) -> Vec<astra_services::session_workspace::BackgroundLocalAgentTaskProjection> {
    let projections =
        export_background_local_agent_task_projections(agent_spawner, restored_local_agents).await;
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return projections;
    };
    if projections == *last_persisted {
        return projections;
    }

    let mut workspace = match astra_services::session_workspace::read_workspace_optional(session_id)
    {
        Ok(Some(workspace)) => workspace,
        Ok(None) => astra_services::session_workspace::WorkspaceMetadata::new(
            session_id,
            model.unwrap_or("default"),
        ),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read workspace before persisting background local agent projections"
            );
            return projections;
        }
    };
    workspace.background_local_agent_tasks = projections.clone();
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    match astra_services::session_workspace::write_workspace(&workspace) {
        Ok(()) => *last_persisted = projections.clone(),
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to persist background local agent projections"
            );
        }
    }
    projections
}

pub(crate) fn format_background_task_output_read_error(task_id: &str, error: &str) -> String {
    if error.contains("no background shell with id") || error.contains("no background task with id")
    {
        format!("Background task not found: {task_id}")
    } else if let Some(detail) = error.strip_prefix("output artifact missing:") {
        format!("Output artifact missing ·{}", detail)
    } else {
        format!("Output unavailable · {error}")
    }
}

pub(crate) fn is_background_task_terminal_race_error(error: &str) -> bool {
    error.contains("already terminated")
}

pub(crate) fn format_background_task_stop_error_system_message(
    task_id: &str,
    error: &str,
) -> String {
    if error.contains("no background shell with id") || error.contains("no background task with id")
    {
        format!("Background task not found: {task_id}")
    } else if is_background_task_terminal_race_error(error) {
        format!("Background task {task_id} already finished.")
    } else if error.contains("stale handle") {
        format!(
            "Background task {task_id} cannot be stopped because it was restored from a previous session and no live process handle is available."
        )
    } else {
        format!("Failed to stop background task {task_id}: {error}")
    }
}

pub(crate) fn background_task_output_snapshot(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    task_id: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<crate::edge_tools::BgTaskOutputSnapshot, String> {
    background_registry.drain_join_set();
    let handle = background_registry
        .get(task_id)
        .ok_or_else(|| format!("no background shell with id '{task_id}'"))?;
    let status = handle.projected_status().to_string();
    let terminal = background_task_status_is_terminal(&status);
    let output_ref = format!(
        "stdout: {} · stderr: {}",
        handle.stdout_path.display(),
        handle.stderr_path.display()
    );
    let (output, end_offset, total_bytes, total_lines) =
        background_registry.get_combined_output_since(task_id, offset, max_bytes)?;

    Ok(crate::edge_tools::BgTaskOutputSnapshot {
        kind: "shell".to_string(),
        title: Some(handle.description.clone()),
        output,
        end_offset,
        total_bytes,
        total_lines,
        status,
        terminal,
        output_ref,
    })
}

pub(crate) fn background_task_output_snapshot_for_local_agent(
    agent: &astra_turn_core::orchestration_types::SpawnedAgentInfo,
    offset: u64,
    max_bytes: usize,
) -> crate::edge_tools::BgTaskOutputSnapshot {
    use astra_turn_core::orchestration_types::AgentStatus;

    let (status, full_output) = match &agent.status {
        AgentStatus::Initializing => ("pending", String::new()),
        AgentStatus::Running { activity } => ("running", activity.clone()),
        AgentStatus::Idle => (
            "waiting_for_input",
            "Agent is waiting for input.".to_string(),
        ),
        AgentStatus::Waiting { reason } => {
            ("waiting_for_input", format!("Agent is waiting: {reason}"))
        }
        AgentStatus::Completed { result, .. } => ("completed", result.clone()),
        AgentStatus::Interrupted { partial_result, .. } => ("interrupted", partial_result.clone()),
        AgentStatus::Failed { error, .. } => ("failed", error.clone()),
        AgentStatus::Cancelled { reason, .. } => ("killed", reason.clone()),
    };
    let total_bytes = full_output.len() as u64;
    let start = offset.min(total_bytes) as usize;
    let end = start.saturating_add(max_bytes).min(full_output.len());
    let output = String::from_utf8_lossy(&full_output.as_bytes()[start..end]).into_owned();
    crate::edge_tools::BgTaskOutputSnapshot {
        kind: "local agent".to_string(),
        title: Some(agent.description.clone()),
        output,
        end_offset: end as u64,
        total_bytes,
        total_lines: full_output.lines().count() as u64,
        status: status.to_string(),
        terminal: background_task_status_is_terminal(status),
        output_ref: format!("agent_state: {}", agent.agent_id),
    }
}

pub(crate) fn background_task_output_snapshot_for_local_agent_projection(
    projection: &astra_services::session_workspace::BackgroundLocalAgentTaskProjection,
    offset: u64,
    max_bytes: usize,
) -> crate::edge_tools::BgTaskOutputSnapshot {
    let full_output = projection.output_tail.clone().unwrap_or_default();
    let total_bytes = full_output.len() as u64;
    let start = offset.min(total_bytes) as usize;
    let end = start.saturating_add(max_bytes).min(full_output.len());
    let output = String::from_utf8_lossy(&full_output.as_bytes()[start..end]).into_owned();
    let status = if matches!(
        projection.status.as_str(),
        "pending" | "running" | "waiting_for_input"
    ) {
        "unavailable"
    } else {
        projection.status.as_str()
    };

    crate::edge_tools::BgTaskOutputSnapshot {
        kind: "local agent".to_string(),
        title: Some(projection.title.clone()),
        output,
        end_offset: end as u64,
        total_bytes,
        total_lines: full_output.lines().count() as u64,
        status: status.to_string(),
        terminal: background_task_status_is_terminal(status),
        output_ref: format!("workspace_projection: {}", projection.id),
    }
}

pub(crate) fn background_task_status_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "unavailable")
}

pub(crate) fn format_background_task_output_system_message(
    task_id: &str,
    title: &str,
    status: &str,
    offset: u64,
    total_bytes: u64,
    total_lines: u64,
    output: &str,
) -> String {
    format_background_task_output_system_message_for_kind(
        "shell",
        BackgroundTaskOutputSystemMessage {
            task_id,
            title,
            status,
            offset,
            total_bytes,
            total_lines,
            output,
        },
    )
}

pub(crate) struct BackgroundTaskOutputSystemMessage<'a> {
    pub task_id: &'a str,
    pub title: &'a str,
    pub status: &'a str,
    pub offset: u64,
    pub total_bytes: u64,
    pub total_lines: u64,
    pub output: &'a str,
}

pub(crate) fn format_background_task_output_system_message_for_kind(
    kind: &str,
    message: BackgroundTaskOutputSystemMessage<'_>,
) -> String {
    let BackgroundTaskOutputSystemMessage {
        task_id,
        title,
        status,
        offset,
        total_bytes,
        total_lines,
        output,
    } = message;
    let label = background_shell_notification_label(task_id, title);
    let read_label = match kind.trim() {
        "" | "shell" => "Read shell output".to_string(),
        "local agent" => "Read local agent output".to_string(),
        "cloud session" => "Read cloud session output".to_string(),
        "main session" => "Read main session output".to_string(),
        "monitor" => "Read monitor output".to_string(),
        other => format!("Read {other} output"),
    };
    let tail = output.trim_end();
    if tail.is_empty() {
        return format!(
            "{read_label} {task_id} · {label}\n{} · offset {offset} -> {total_bytes} · total {total_bytes} bytes · {total_lines} total lines",
            background_task_empty_output_state(status)
        );
    }

    let line_count = tail.lines().count();
    format!(
        "{read_label} {task_id} · {label}\n{line_count} new {} · offset {offset} -> {total_bytes} · total {total_bytes} bytes · {total_lines} total lines · {}\nOutput chunk:\n{tail}",
        if line_count == 1 { "line" } else { "lines" },
        background_task_status_label(status)
    )
}

pub(crate) fn background_task_empty_output_state(status: &str) -> &'static str {
    match status {
        "pending" => "Pending · no output yet",
        "running" => "No output yet · still running",
        "waiting_for_input" => "Waiting for input · no new output",
        "completed" => "Completed with no output",
        "failed" => "Failed with no output",
        "killed" => "Stopped with no output",
        "unavailable" => "Unavailable · stale handle or unsupported runner",
        _ => "No output yet",
    }
}

pub(crate) fn background_task_status_label(status: &str) -> &'static str {
    match status {
        "pending" => "pending",
        "running" => "still running",
        "waiting_for_input" => "needs input",
        "completed" => "completed",
        "failed" => "failed",
        "killed" => "stopped",
        "unavailable" => "unavailable",
        _ => "unknown",
    }
}

pub(crate) fn background_task_event_system_message(
    ev: &super::background_tasks::BgTaskEvent,
) -> Option<String> {
    match ev {
        super::background_tasks::BgTaskEvent::Completed {
            id,
            title,
            exit_code,
            summary,
        } => {
            let label = background_shell_notification_label(id, title);
            let exit = exit_code
                .map(|code| format!(" (exit {code})"))
                .unwrap_or_default();
            if summary.trim().is_empty() {
                Some(format!("Background shell {label} completed{exit}"))
            } else {
                Some(format!(
                    "Background shell {label} completed{exit}: {summary}"
                ))
            }
        }
        super::background_tasks::BgTaskEvent::Failed { id, title, error } => Some(format!(
            "Background shell {} failed: {error}",
            background_shell_notification_label(id, title)
        )),
        super::background_tasks::BgTaskEvent::WaitingForInput { id, title, .. } => Some(format!(
            "Background shell {} appears to be waiting for input",
            background_shell_notification_label(id, title)
        )),
        super::background_tasks::BgTaskEvent::Killed { id, title } => Some(format!(
            "Background shell {} was stopped",
            background_shell_notification_label(id, title)
        )),
        super::background_tasks::BgTaskEvent::Started { .. } => None,
    }
}

pub(crate) fn background_shell_notification_label(id: &str, title: &str) -> String {
    let title = title.trim();
    if title.is_empty() || title == id {
        id.to_string()
    } else {
        format!("\"{}\"", title.replace('"', "\\\""))
    }
}

pub(crate) fn background_task_event_system_messages(
    events: &[super::background_tasks::BgTaskEvent],
) -> Vec<String> {
    let low_risk_successes = events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                super::background_tasks::BgTaskEvent::Completed {
                    exit_code: Some(0),
                    ..
                }
            )
        })
        .count();
    let collapse_successes = low_risk_successes > 1;
    let mut emitted_collapsed_success = false;
    let mut messages = Vec::new();

    for ev in events {
        let is_low_risk_success = matches!(
            ev,
            super::background_tasks::BgTaskEvent::Completed {
                exit_code: Some(0),
                ..
            }
        );
        if collapse_successes && is_low_risk_success {
            if !emitted_collapsed_success {
                messages.push(format!("{low_risk_successes} background shells completed"));
                emitted_collapsed_success = true;
            }
            continue;
        }

        if let Some(msg) = background_task_event_system_message(ev) {
            messages.push(msg);
        }
    }

    messages
}
