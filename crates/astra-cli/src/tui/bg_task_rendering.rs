//! Background task rendering.
//!
//! TUI rendering layer: row constructors, XML output, and view orchestration.

use std::sync::Arc;

use super::bg_task_proxy::{
    background_task_output_snapshot, background_task_output_snapshot_for_local_agent,
    background_task_output_snapshot_for_local_agent_projection,
    background_task_rejected_fanout_slot_id, background_task_rejected_fanout_slot_label,
    format_background_task_output_read_error, format_background_task_stop_error_system_message,
};
use super::bottom_pane::BottomPane;
use super::frame_requester::FrameRequester;
use crate::background_task_error::BackgroundTaskError;

pub(crate) fn background_task_rows(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
) -> Vec<super::bottom_pane::background_task_view::BackgroundTaskRow> {
    background_registry.drain_join_set();
    let snapshots: Vec<_> = background_registry
        .all_tasks()
        .map(|handle| {
            (
                handle.id.clone(),
                handle.projected_status().to_string(),
                handle.live_control,
                handle.elapsed_ms(),
                handle.no_recent_output_ms(),
                handle.started_at_ms,
                handle.ended_at_ms,
                handle.description.clone(),
                handle.exit_code,
                handle.terminal_reason.clone(),
                handle.output_tail().map(str::to_string),
                handle.output_error().cloned(),
                handle.observed_output_bytes(),
                format!(
                    "stdout: {} · stderr: {}",
                    handle.stdout_path.display(),
                    handle.stderr_path.display()
                ),
            )
        })
        .collect();

    snapshots
        .into_iter()
        .map(
            |(
                id,
                status,
                live_control,
                elapsed_ms,
                no_recent_output_ms,
                started_at_ms,
                ended_at_ms,
                description,
                exit_code,
                terminal_reason,
                output_tail,
                output_error,
                total_bytes,
                output_ref,
            )| {
                let output_error = output_error
                    .as_ref()
                    .map(format_background_task_output_read_error);
                let total_bytes = output_error.is_none().then_some(total_bytes);
                let output_tail =
                    output_error.or_else(|| output_tail.filter(|tail| !tail.is_empty()));
                super::bottom_pane::background_task_view::BackgroundTaskRow::shell(
                    id,
                    status,
                    elapsed_ms,
                    description,
                    Some(output_ref),
                    output_tail,
                    total_bytes,
                )
                .with_live_control(background_task_live_control_state(live_control))
                .with_no_recent_output(no_recent_output_ms)
                .with_output_stats(None, None)
                .with_terminal(exit_code, terminal_reason)
                .with_timing(Some(started_at_ms), ended_at_ms)
            },
        )
        .collect()
}

pub(crate) fn background_task_fanout_membership(
    slot: &astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity,
    group_title: Option<&str>,
    slot_label: &str,
) -> super::bottom_pane::background_task_view::BackgroundTaskFanoutMembership {
    let group_title = group_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(slot.group_id.as_str())
        .to_string();
    super::bottom_pane::background_task_view::BackgroundTaskFanoutMembership {
        group_id: slot.group_id.clone(),
        group_title,
        target_count: slot.target_count,
        slot_index: slot.slot_index,
        slot_label: slot_label.to_string(),
    }
}

pub(crate) fn background_task_fanout_membership_from_projection(
    projection: &astra_services::session_workspace::BackgroundLocalAgentFanoutProjection,
) -> super::bottom_pane::background_task_view::BackgroundTaskFanoutMembership {
    super::bottom_pane::background_task_view::BackgroundTaskFanoutMembership {
        group_id: projection.group_id.clone(),
        group_title: projection.group_title.clone(),
        target_count: projection.target_count,
        slot_index: projection.slot_index,
        slot_label: projection.slot_label.clone(),
    }
}

pub(crate) fn background_task_row_for_rejected_fanout_slot(
    group: &astra_turn_core::orchestration_fanout_group::AgentFanoutGroupProjection,
    slot: &astra_turn_core::orchestration_fanout_group::AgentFanoutSlot,
) -> Option<super::bottom_pane::background_task_view::BackgroundTaskRow> {
    use super::bottom_pane::background_task_view::{
        BackgroundTaskFanoutMembership, BackgroundTaskKind, BackgroundTaskRow,
        BackgroundTaskRowInit, LiveControlState,
    };
    use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotStatus;

    if slot.status != AgentFanoutSlotStatus::SpawnRejected || slot.agent_id.is_some() {
        return None;
    }
    let label = background_task_rejected_fanout_slot_label(slot);
    let reason = slot
        .terminal_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("spawn rejected")
        .to_string();
    let requested_description = slot.requested_description.trim();
    let title = if requested_description.is_empty() {
        label.clone()
    } else {
        requested_description.to_string()
    };
    let total_bytes = reason.len() as u64;
    let total_lines = reason.lines().count() as u64;
    Some(
        BackgroundTaskRow::new(
            BackgroundTaskRowInit::new(
                background_task_rejected_fanout_slot_id(&group.group_id, slot.slot_index),
                BackgroundTaskKind::LocalAgent,
                "failed",
                0,
                title,
            )
            .with_output(
                Some(format!(
                    "fanout_spawn_rejected: {}#{}",
                    group.group_id, slot.slot_index
                )),
                Some(reason.clone()),
                Some(total_bytes),
            ),
        )
        .with_output_stats(None, Some(total_lines))
        .with_terminal(None, Some(reason))
        .with_live_control(LiveControlState::UnsupportedInMode)
        .with_fanout(BackgroundTaskFanoutMembership {
            group_id: group.group_id.clone(),
            group_title: if group.title.trim().is_empty() {
                group.group_id.clone()
            } else {
                group.title.clone()
            },
            target_count: group.target_count,
            slot_index: slot.slot_index,
            slot_label: label,
        }),
    )
}

pub(crate) fn background_task_output_snapshot_for_rejected_fanout_slot(
    group: &astra_turn_core::orchestration_fanout_group::AgentFanoutGroupProjection,
    slot: &astra_turn_core::orchestration_fanout_group::AgentFanoutSlot,
    offset: u64,
    max_bytes: usize,
) -> Option<crate::edge_tools::BgTaskOutputSnapshot> {
    let row = background_task_row_for_rejected_fanout_slot(group, slot)?;
    let full_output = row.output_tail.clone().unwrap_or_default();
    let total_bytes = full_output.len() as u64;
    let start = offset.min(total_bytes) as usize;
    let end = start.saturating_add(max_bytes).min(full_output.len());
    let output = String::from_utf8_lossy(&full_output.as_bytes()[start..end]).into_owned();
    Some(crate::edge_tools::BgTaskOutputSnapshot {
        kind: "local agent".to_string(),
        title: Some(row.title),
        output,
        end_offset: end as u64,
        total_bytes,
        total_lines: full_output.lines().count() as u64,
        status: crate::edge_tools::BgTaskOutputStatus::Failed,
        output_ref: row.output_ref.unwrap_or_else(|| {
            format!(
                "fanout_spawn_rejected: {}#{}",
                group.group_id, slot.slot_index
            )
        }),
    })
}

pub(crate) fn background_task_row_for_local_agent_with_fanout_title(
    agent: &astra_turn_core::orchestration_types::SpawnedAgentInfo,
    fanout_title: Option<&str>,
) -> Option<super::bottom_pane::background_task_view::BackgroundTaskRow> {
    use super::bottom_pane::background_task_view::{
        BackgroundTaskKind, BackgroundTaskRow, BackgroundTaskRowInit, BackgroundTaskStatus,
    };
    use astra_turn_core::orchestration_types::AgentStatus;

    if !agent.run_in_background {
        return None;
    }

    let (status, tail, terminal_reason) = match &agent.status {
        AgentStatus::Initializing => (BackgroundTaskStatus::Pending, None, None),
        AgentStatus::Running { activity } => (
            BackgroundTaskStatus::Running,
            Some(activity.clone()).filter(|activity| !activity.trim().is_empty()),
            None,
        ),
        AgentStatus::Idle => (
            BackgroundTaskStatus::WaitingForInput,
            Some("Agent is waiting for input.".to_string()),
            None,
        ),
        AgentStatus::Waiting { reason } => (
            BackgroundTaskStatus::WaitingForInput,
            Some(format!("Agent is waiting: {reason}")),
            None,
        ),
        AgentStatus::Completed {
            result,
            finish_reason,
        } => (
            BackgroundTaskStatus::Completed,
            Some(result.clone()).filter(|result| !result.trim().is_empty()),
            finish_reason.clone(),
        ),
        AgentStatus::Interrupted {
            partial_result,
            finish_reason,
        } => (
            BackgroundTaskStatus::Interrupted,
            Some(partial_result.clone()).filter(|result| !result.trim().is_empty()),
            Some(finish_reason.clone()),
        ),
        AgentStatus::Failed {
            error,
            finish_reason,
        } => (
            BackgroundTaskStatus::Failed,
            Some(error.clone()).filter(|error| !error.trim().is_empty()),
            finish_reason.clone().or_else(|| Some(error.clone())),
        ),
        AgentStatus::Cancelled { reason, .. } => (
            BackgroundTaskStatus::Killed,
            Some(reason.clone()).filter(|reason| !reason.trim().is_empty()),
            Some(if reason.trim().is_empty() {
                "cancelled".to_string()
            } else {
                reason.clone()
            }),
        ),
    };

    let elapsed_ms = agent
        .ended_at
        .unwrap_or_else(std::time::SystemTime::now)
        .duration_since(agent.started_at)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let started_at_ms = agent
        .started_at
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64);
    let ended_at_ms = agent
        .ended_at
        .and_then(|ended_at| ended_at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64);
    let total_bytes = tail.as_ref().map(|tail| tail.len() as u64);
    let total_lines = tail.as_ref().map(|tail| tail.lines().count() as u64);

    let row = BackgroundTaskRow::new(
        BackgroundTaskRowInit::new(
            agent.agent_id.clone(),
            BackgroundTaskKind::LocalAgent,
            status.as_str(),
            elapsed_ms,
            agent.description.clone(),
        )
        .with_output(None, tail, total_bytes),
    )
    .with_output_stats(None, total_lines)
    .with_terminal(None, terminal_reason)
    .with_timing(started_at_ms, ended_at_ms);

    Some(if let Some(slot) = agent.fanout_slot.as_ref() {
        row.with_fanout(background_task_fanout_membership(
            slot,
            fanout_title,
            &agent.description,
        ))
    } else {
        row
    })
}

pub(crate) fn background_task_row_for_local_agent(
    agent: &astra_turn_core::orchestration_types::SpawnedAgentInfo,
) -> Option<super::bottom_pane::background_task_view::BackgroundTaskRow> {
    background_task_row_for_local_agent_with_fanout_title(agent, None)
}

pub(crate) fn background_task_row_for_local_agent_projection(
    projection: &astra_services::session_workspace::BackgroundLocalAgentTaskProjection,
) -> super::bottom_pane::background_task_view::BackgroundTaskRow {
    use super::bottom_pane::background_task_view::{
        BackgroundTaskKind, BackgroundTaskRow, BackgroundTaskRowInit, LiveControlState,
    };

    let total_bytes = projection
        .output_tail
        .as_ref()
        .map(|tail| tail.len() as u64);
    let total_lines = projection
        .output_tail
        .as_ref()
        .map(|tail| tail.lines().count() as u64);
    let elapsed_ms = projection
        .ended_at_ms
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(projection.started_at_ms)
        })
        .saturating_sub(projection.started_at_ms);

    let row = BackgroundTaskRow::new(
        BackgroundTaskRowInit::new(
            projection.id.clone(),
            BackgroundTaskKind::LocalAgent,
            projection.status.as_str(),
            elapsed_ms,
            projection.title.clone(),
        )
        .with_output(
            Some(format!("workspace_projection: {}", projection.id)),
            projection.output_tail.clone(),
            total_bytes,
        ),
    )
    .with_output_stats(None, total_lines)
    .with_terminal(None, projection.terminal_reason.clone())
    .with_timing(Some(projection.started_at_ms), projection.ended_at_ms)
    .with_live_control(LiveControlState::StaleHandle);

    if let Some(fanout) = projection.fanout.as_ref() {
        row.with_fanout(background_task_fanout_membership_from_projection(fanout))
    } else {
        row
    }
}

pub(crate) async fn background_task_rows_with_agents(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
) -> Vec<super::bottom_pane::background_task_view::BackgroundTaskRow> {
    let snapshot = super::local_agent_snapshot::LocalAgentSnapshot::capture(agent_spawner).await;
    background_task_rows_with_agent_snapshot(background_registry, &snapshot, restored_local_agents)
}

pub(crate) fn background_task_rows_with_agent_snapshot(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    snapshot: &super::local_agent_snapshot::LocalAgentSnapshot,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
) -> Vec<super::bottom_pane::background_task_view::BackgroundTaskRow> {
    let mut rows = background_task_rows(background_registry);
    let mut live_agent_ids = std::collections::HashSet::new();
    if snapshot.available {
        let fanout_titles = snapshot.fanout_titles();
        for agent in &snapshot.agents {
            let fanout_title = agent
                .fanout_slot
                .as_ref()
                .and_then(|slot| fanout_titles.get(&slot.group_id).map(String::as_str));
            if let Some(row) =
                background_task_row_for_local_agent_with_fanout_title(agent, fanout_title)
            {
                live_agent_ids.insert(row.id.clone());
                rows.push(row);
            }
        }
        for group in &snapshot.fanout_groups {
            rows.extend(
                group
                    .slots
                    .iter()
                    .filter_map(|slot| background_task_row_for_rejected_fanout_slot(group, slot)),
            );
        }
    }
    rows.extend(
        restored_local_agents
            .iter()
            .filter(|projection| !live_agent_ids.contains(projection.id.as_str()))
            .map(background_task_row_for_local_agent_projection),
    );
    rows
}

pub(crate) async fn render_background_task_list_xml_with_agents(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
) -> String {
    let rows =
        background_task_rows_with_agents(background_registry, agent_spawner, restored_local_agents)
            .await;
    render_background_task_rows_xml(&rows)
}

pub(crate) fn background_task_live_control_state(
    live_control: super::background_tasks::BgTaskLiveControl,
) -> super::bottom_pane::background_task_view::LiveControlState {
    match live_control {
        super::background_tasks::BgTaskLiveControl::Available => {
            super::bottom_pane::background_task_view::LiveControlState::Available
        }
        super::background_tasks::BgTaskLiveControl::StaleHandle => {
            super::bottom_pane::background_task_view::LiveControlState::StaleHandle
        }
    }
}

pub(crate) const PENDING_BASH_HANDOFF_TASK_ID: &str = "bg-shell-handoff";

pub(crate) fn pending_bash_handoff_row(
    title: &str,
    elapsed_ms: u64,
) -> super::bottom_pane::background_task_view::BackgroundTaskRow {
    let title = title.trim();
    let title = if title.is_empty() {
        "Bash handoff"
    } else {
        title
    };
    let tail = "Waiting for foreground Bash to hand off its process.".to_string();
    super::bottom_pane::background_task_view::BackgroundTaskRow::shell(
        PENDING_BASH_HANDOFF_TASK_ID,
        "pending",
        elapsed_ms,
        title,
        None,
        Some(tail.clone()),
        Some(tail.len() as u64),
    )
    .with_output_stats(None, Some(1))
}

pub(crate) fn sync_background_task_footer_from_rows(
    bottom_pane: &mut BottomPane,
    rows: &[super::bottom_pane::background_task_view::BackgroundTaskRow],
) {
    let counts = super::status_line::BackgroundTaskCounts::from_rows(rows);
    bottom_pane.footer.bg_task_counts = if counts.is_empty() {
        None
    } else {
        Some(counts)
    };
    bottom_pane.footer.bg_fanout_summaries =
        super::status_line::BackgroundTaskFanoutSummary::from_rows(rows);
}

pub(crate) async fn open_background_task_view(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    reveal_background_task_view(
        background_registry,
        agent_spawner,
        restored_local_agents,
        bottom_pane,
        frame_requester,
        None,
    )
    .await
}

pub(crate) async fn reveal_background_task_view(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
    selected_id: Option<&str>,
) -> bool {
    reveal_background_task_view_rows_inner(
        background_registry,
        agent_spawner,
        restored_local_agents,
        bottom_pane,
        frame_requester,
        Vec::new(),
        selected_id,
        false,
    )
    .await
}

pub(crate) async fn reveal_background_task_view_with_extra_rows(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
    extra_rows: Vec<super::bottom_pane::background_task_view::BackgroundTaskRow>,
    selected_id: Option<&str>,
) -> bool {
    reveal_background_task_view_rows_inner(
        background_registry,
        agent_spawner,
        restored_local_agents,
        bottom_pane,
        frame_requester,
        extra_rows,
        selected_id,
        false,
    )
    .await
}

/// Always open the background task panel, even if the registry is empty.
/// Used by Ctrl+B so the user always lands in a panel they can navigate or
/// dismiss. Empty-state rendering lives in `BackgroundTaskView`.
pub(crate) async fn force_open_background_task_view(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    reveal_background_task_view_rows_inner(
        background_registry,
        agent_spawner,
        restored_local_agents,
        bottom_pane,
        frame_requester,
        Vec::new(),
        None,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn reveal_background_task_view_rows_inner(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
    extra_rows: Vec<super::bottom_pane::background_task_view::BackgroundTaskRow>,
    selected_id: Option<&str>,
    force_open: bool,
) -> bool {
    let mut rows =
        background_task_rows_with_agents(background_registry, agent_spawner, restored_local_agents)
            .await;
    rows.extend(extra_rows);
    sync_background_task_footer_from_rows(bottom_pane, &rows);
    if bottom_pane.accepts_background_task_rows() {
        bottom_pane.refresh_background_task_rows_selecting(rows, selected_id);
        bottom_pane.sync_popups();
        frame_requester.schedule_frame();
        return true;
    }
    if !force_open {
        let counts = super::status_line::BackgroundTaskCounts::from_rows(&rows);
        if counts.is_empty() {
            bottom_pane.sync_popups();
            frame_requester.schedule_frame();
            return false;
        }
    }
    use super::bottom_pane::background_task_view::BackgroundTaskView;
    bottom_pane.push_view(Box::new(BackgroundTaskView::new_with_selected(
        rows,
        selected_id,
    )));
    bottom_pane.sync_popups();
    frame_requester.schedule_frame();
    true
}

pub(crate) async fn dispatch_background_task_stop(
    task_id: &str,
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    chat_widget: &mut super::chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) {
    let task_id = task_id.to_string();
    match stop_background_task_with_agents(
        background_registry,
        agent_spawner.as_ref(),
        restored_local_agents,
        &task_id,
    )
    .await
    {
        Ok(BackgroundTaskStopTarget::Shell) => {
            chat_widget.commit_system(super::history_cell::system::SystemCell::info(format!(
                "Stopping background task {task_id}."
            )));
        }
        Ok(BackgroundTaskStopTarget::LocalAgent) => {
            chat_widget.commit_system(super::history_cell::system::SystemCell::info(format!(
                "Stopping local agent {task_id}."
            )));
        }
        Err(error) => {
            let message = format_background_task_stop_error_system_message(&error);
            if matches!(error, BackgroundTaskError::AlreadyTerminated { .. }) {
                chat_widget.commit_system(super::history_cell::system::SystemCell::info(message));
            } else {
                chat_widget.commit_system(super::history_cell::system::SystemCell::error(message));
            }
        }
    }
    let rows = background_task_rows_with_agents(
        background_registry,
        agent_spawner.as_ref(),
        restored_local_agents,
    )
    .await;
    sync_background_task_footer_from_rows(bottom_pane, &rows);
    bottom_pane.refresh_background_task_rows(rows);
    bottom_pane.sync_popups();
    frame_requester.schedule_frame();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundTaskStopTarget {
    Shell,
    LocalAgent,
}

pub(crate) async fn stop_background_task_with_agents(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    task_id: &str,
) -> Result<BackgroundTaskStopTarget, BackgroundTaskError> {
    match background_registry.kill(task_id) {
        Ok(()) => Ok(BackgroundTaskStopTarget::Shell),
        Err(BackgroundTaskError::NotFound { .. }) => {
            if let Some(spawner) = agent_spawner {
                if spawner
                    .cancel_agent(task_id, "user-requested via background task control")
                    .await
                {
                    return Ok(BackgroundTaskStopTarget::LocalAgent);
                }
                match spawner.get_agent_state_any(task_id).await {
                    Some(state) if !state.run_in_background => {
                        return Err(BackgroundTaskError::not_found(task_id));
                    }
                    Some(state) if state.status.is_terminal() => {
                        return Err(BackgroundTaskError::AlreadyTerminated {
                            task_id: task_id.to_string(),
                        });
                    }
                    Some(_) => {
                        return Err(BackgroundTaskError::CannotStop {
                            task_id: task_id.to_string(),
                        });
                    }
                    None => {}
                }
            }
            if restored_local_agents
                .iter()
                .any(|projection| projection.id == task_id)
            {
                return Err(BackgroundTaskError::StaleHandle {
                    task_id: task_id.to_string(),
                });
            }
            Err(BackgroundTaskError::not_found(task_id))
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn background_task_output_snapshot_with_agents(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    task_id: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<crate::edge_tools::BgTaskOutputSnapshot, BackgroundTaskError> {
    match background_task_output_snapshot(background_registry, task_id, offset, max_bytes).await {
        Ok(snapshot) => Ok(snapshot),
        Err(BackgroundTaskError::NotFound { .. }) => {
            if let Some(spawner) = agent_spawner {
                if let Some(state) = spawner.get_agent_state_any(task_id).await {
                    if !state.run_in_background {
                        return Err(BackgroundTaskError::not_found(task_id));
                    }
                    let info = astra_turn_core::orchestration_types::SpawnedAgentInfo::from(&state);
                    return Ok(background_task_output_snapshot_for_local_agent(
                        &info, offset, max_bytes,
                    ));
                }
                for group in spawner.list_fanout_groups().await {
                    for slot in &group.slots {
                        if background_task_rejected_fanout_slot_id(&group.group_id, slot.slot_index)
                            == task_id
                            && let Some(snapshot) =
                                background_task_output_snapshot_for_rejected_fanout_slot(
                                    &group, slot, offset, max_bytes,
                                )
                        {
                            return Ok(snapshot);
                        }
                    }
                }
            }
            if let Some(projection) = restored_local_agents
                .iter()
                .find(|projection| projection.id == task_id)
            {
                return Ok(background_task_output_snapshot_for_local_agent_projection(
                    projection, offset, max_bytes,
                ));
            }
            Err(BackgroundTaskError::not_found(task_id))
        }
        Err(error) => Err(error),
    }
}

// ── XML rendering ──

pub(crate) fn xml_escape_attr(value: &str) -> String {
    astra_text_utils::xml_escape::xml_escape_attr(value).into_owned()
}

pub(crate) fn truncate_xml_attr(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(crate) fn live_control_xml_value(
    live_control: crate::tui::bottom_pane::background_task_view::LiveControlState,
) -> &'static str {
    match live_control {
        crate::tui::bottom_pane::background_task_view::LiveControlState::Available => "available",
        crate::tui::bottom_pane::background_task_view::LiveControlState::StaleHandle => {
            "stale_handle"
        }
        crate::tui::bottom_pane::background_task_view::LiveControlState::UnsupportedInMode => {
            "unsupported_in_mode"
        }
    }
}

pub(crate) fn render_background_task_rows_xml(
    rows: &[crate::tui::bottom_pane::background_task_view::BackgroundTaskRow],
) -> String {
    use crate::tui::bottom_pane::background_task_view::BackgroundTaskKind;

    if rows.is_empty() {
        return "<background_tasks count=\"0\" />".to_string();
    }

    let mut rows = rows.to_vec();
    rows.sort_by_key(|row| {
        let attention_rank = match row.status {
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::WaitingForInput
            | crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Interrupted
            | crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Failed => 0,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Running
            | crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Pending => 1,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Stopping
            | crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Killed => 2,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Completed => 3,
            crate::tui::bottom_pane::background_task_view::BackgroundTaskStatus::Unavailable => 4,
        };
        (
            attention_rank,
            row.started_at_ms.unwrap_or(u64::MAX),
            row.id.clone(),
        )
    });

    let mut out = format!("<background_tasks count=\"{}\">", rows.len());
    for row in rows {
        let mut attrs = vec![
            ("id", xml_escape_attr(&row.id)),
            ("kind", row.kind.as_str().to_string()),
            ("status", row.status.as_str().to_string()),
            (
                "live_control",
                live_control_xml_value(row.live_control).to_string(),
            ),
            ("elapsed_ms", row.elapsed_ms.to_string()),
            ("title", xml_escape_attr(&row.title)),
        ];
        match row.kind {
            BackgroundTaskKind::Shell => attrs.push(("command", xml_escape_attr(&row.title))),
            _ => attrs.push(("description", xml_escape_attr(&row.title))),
        }
        if let Some(started_at_ms) = row.started_at_ms {
            attrs.push(("started_at_ms", started_at_ms.to_string()));
        }
        if let Some(ended_at_ms) = row.ended_at_ms {
            attrs.push(("ended_at_ms", ended_at_ms.to_string()));
        }
        if let Some(inactive_ms) = row.no_recent_output_ms {
            attrs.push(("no_recent_output_ms", inactive_ms.to_string()));
        }
        if let Some(output_ref) = row.output_ref.as_deref() {
            attrs.push(("output_ref", xml_escape_attr(output_ref)));
        }
        if let Some(output_offset) = row.output_offset {
            attrs.push(("output_offset", output_offset.to_string()));
        }
        if let Some(total_bytes) = row.total_bytes {
            attrs.push(("total_output_bytes", total_bytes.to_string()));
        }
        if let Some(total_lines) = row.total_lines {
            attrs.push(("total_output_lines", total_lines.to_string()));
        }
        if let Some(preview) = row
            .output_tail
            .as_deref()
            .and_then(|tail| tail.lines().next_back())
            .map(str::trim)
            .filter(|preview| !preview.is_empty())
        {
            attrs.push(("preview", xml_escape_attr(&truncate_xml_attr(preview, 160))));
        }
        if let Some(exit_code) = row.exit_code {
            attrs.push(("exit_code", exit_code.to_string()));
        }
        if let Some(reason) = row.terminal_reason.as_deref() {
            attrs.push(("terminal_reason", xml_escape_attr(reason)));
        }
        if let Some(fanout) = row.fanout.as_ref() {
            attrs.push(("fanout_group_id", xml_escape_attr(&fanout.group_id)));
            attrs.push(("fanout_group_title", xml_escape_attr(&fanout.group_title)));
            attrs.push(("fanout_target_count", fanout.target_count.to_string()));
            attrs.push(("fanout_slot_index", fanout.slot_index.to_string()));
            attrs.push(("fanout_slot_label", xml_escape_attr(&fanout.slot_label)));
        }

        out.push_str("\n<task");
        for (key, value) in attrs {
            out.push_str(&format!(" {key}=\"{value}\""));
        }
        out.push_str(" />");
    }
    out.push_str("\n</background_tasks>");
    out
}
