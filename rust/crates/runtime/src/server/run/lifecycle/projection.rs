//! Agent progress streaming and SSE projection.
//!
//! This module handles:
//! - Agent progress event streaming to SSE clients
//! - Lifecycle event deduplication
//! - Agent live event to work surface SSE conversion
//! - Agent spawner state to progress event conversion

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};

use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveSendError,
    AgentLiveTermination,
};

use super::run_state::{
    RunStatus, durable_event_type, streaming_event_for_persistence,
    streaming_final_event_for_replay,
};
use crate::orchestration::{
    AgentProgressEvent, DynamicAgentSpawner, ProgressEventType, SpawnedAgentState,
};
use crate::server::server_loop_host;

pub(super) fn should_emit_stream_turn_complete(final_status: &RunStatus) -> bool {
    matches!(final_status, RunStatus::Completed | RunStatus::Paused)
}

pub(super) struct AgentProgressStreamBridge {
    pub(super) stop_tx: oneshot::Sender<()>,
    pub(super) join: tokio::task::JoinHandle<()>,
    pub(super) sent_lifecycle_events: AgentProgressLifecycleLedger,
}

impl AgentProgressStreamBridge {
    pub(super) async fn stop_and_drain(self) -> AgentProgressLifecycleLedger {
        let _ = self.stop_tx.send(());
        if let Err(e) = self.join.await {
            tracing::warn!(
                target: "astra_runtime::projection",
                "agent progress stream bridge task panicked or was cancelled: {:?}",
                e,
            );
        }
        self.sent_lifecycle_events
    }
}

pub(super) type AgentProgressLifecycleLedger =
    Arc<std::sync::Mutex<HashSet<AgentProgressLifecycleEventKey>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct AgentProgressLifecycleEventKey {
    agent_id: String,
    kind: AgentProgressLifecycleEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AgentProgressLifecycleEventKind {
    Spawned { run_id: String },
    Completed,
    Interrupted,
    Failed,
    Waiting,
    Cancelled,
}

fn agent_progress_lifecycle_event_key(
    event: &AgentProgressEvent,
) -> Option<AgentProgressLifecycleEventKey> {
    let kind = match &event.event_type {
        ProgressEventType::AgentSpawned { run_id, .. } => {
            AgentProgressLifecycleEventKind::Spawned {
                run_id: run_id.clone(),
            }
        }
        ProgressEventType::Completed { .. } => AgentProgressLifecycleEventKind::Completed,
        ProgressEventType::Interrupted { .. } => AgentProgressLifecycleEventKind::Interrupted,
        ProgressEventType::Failed { .. } => AgentProgressLifecycleEventKind::Failed,
        ProgressEventType::Waiting { .. } => AgentProgressLifecycleEventKind::Waiting,
        ProgressEventType::Cancelled { .. } => AgentProgressLifecycleEventKind::Cancelled,
        _ => return None,
    };
    Some(AgentProgressLifecycleEventKey {
        agent_id: event.agent_id.clone(),
        kind,
    })
}

fn mark_agent_progress_lifecycle_event_sent(
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    key: AgentProgressLifecycleEventKey,
) {
    sent_lifecycle_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key);
}

fn has_agent_progress_lifecycle_event_sent(
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    key: &AgentProgressLifecycleEventKey,
) -> bool {
    sent_lifecycle_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(key)
}

#[derive(Debug, Clone)]
pub(super) struct WorkSurfaceAgentLiveEventSink {
    tx: mpsc::Sender<Value>,
    execution_metadata: Option<Value>,
}

impl WorkSurfaceAgentLiveEventSink {
    pub(super) fn new(tx: mpsc::Sender<Value>, execution_metadata: Option<Value>) -> Self {
        Self {
            tx,
            execution_metadata,
        }
    }
}

impl AgentLiveEventSink for WorkSurfaceAgentLiveEventSink {
    fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError> {
        let value = agent_live_event_to_work_surface_sse(&event, self.execution_metadata.as_ref());
        match self.tx.try_send(value) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // Work surface receiver is behind — drop rather than block the
                // SSE emitter thread. The frontend will catch up on the next
                // poll / refresh.
                Err(AgentLiveSendError::Dropped)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(AgentLiveSendError::Closed)
            }
        }
    }
}

pub(super) fn agent_live_event_to_work_surface_sse(
    event: &AgentLiveEvent,
    execution_metadata: Option<&Value>,
) -> Value {
    let timestamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let mut value = match &event.kind {
        AgentLiveEventKind::OutputDelta(content) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "output_delta",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ThinkingDelta(content) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "thinking_delta",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::Status(content) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "status",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ToolStarted {
            name,
            description,
            tool_use_id,
        } => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "tool_started",
            "name": name,
            "description": description,
            "tool_use_id": tool_use_id,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
        } => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "tool_completed",
            "name": name,
            "description": description,
            "status": status,
            "duration_ms": duration_ms,
            "output_summary": output_summary,
            "output": output,
            "tool_use_id": tool_use_id,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::AgentTerminated {
            termination,
            duration_ms,
            reason,
        } => {
            let termination = match termination {
                AgentLiveTermination::Completed => "completed",
                AgentLiveTermination::Failed => "failed",
                AgentLiveTermination::Cancelled => "cancelled",
            };
            json!({
                "type": "agent_live_event",
                "agent_id": event.agent_id.as_str(),
                "event_kind": "agent_terminated",
                "termination": termination,
                "status": termination,
                "duration_ms": duration_ms,
                "reason": reason,
                "timestamp": timestamp,
            })
        }
    };
    merge_agent_live_execution_metadata(&mut value, execution_metadata);
    value
}

fn merge_agent_live_execution_metadata(event: &mut Value, execution_metadata: Option<&Value>) {
    let Some(event_obj) = event.as_object_mut() else {
        return;
    };
    let Some(metadata_obj) = execution_metadata.and_then(Value::as_object) else {
        return;
    };
    for key in ["workspace", "executor", "transport", "fallback_policy"] {
        if let Some(value) = metadata_obj.get(key).cloned() {
            event_obj.entry(key.to_string()).or_insert(value);
        }
    }
}

pub(super) async fn forward_agent_progress_event_to_stream(
    filter: &mut server_loop_host::RunScopedAgentProgressFilter,
    event_tx: &mpsc::Sender<Value>,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    evt: AgentProgressEvent,
) -> bool {
    for evt in filter.accept(evt) {
        let lifecycle_key = agent_progress_lifecycle_event_key(&evt);
        let Some(event) = server_loop_host::progress_event_to_sse(&evt) else {
            continue;
        };
        if event_tx.send(event).await.is_err() {
            return false;
        }
        if let Some(key) = lifecycle_key {
            mark_agent_progress_lifecycle_event_sent(sent_lifecycle_events, key);
        }
    }
    true
}

pub(super) async fn drain_ready_agent_progress_events(
    progress_rx: &mut broadcast::Receiver<AgentProgressEvent>,
    filter: &mut server_loop_host::RunScopedAgentProgressFilter,
    event_tx: &mpsc::Sender<Value>,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
) -> bool {
    loop {
        match progress_rx.try_recv() {
            Ok(evt) => {
                if !forward_agent_progress_event_to_stream(
                    filter,
                    event_tx,
                    sent_lifecycle_events,
                    evt,
                )
                .await
                {
                    return false;
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                tracing::warn!(
                    target: "astra_runtime::work_surface",
                    dropped,
                    "agent progress live stream lagged while draining ready events"
                );
                continue;
            }
            Err(broadcast::error::TryRecvError::Empty) => return true,
            Err(broadcast::error::TryRecvError::Closed) => return true,
        }
    }
}

fn system_time_epoch_ms(time: SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn agent_spawned_progress_event_from_state(state: &SpawnedAgentState) -> AgentProgressEvent {
    AgentProgressEvent {
        agent_id: state.agent_id.clone(),
        event_type: ProgressEventType::AgentSpawned {
            run_id: state.run_id.clone(),
            parent_run_id: state.parent_run_id.clone(),
            agent_type: state.agent_type.clone(),
            description: state.description.clone(),
            fanout_slot: state.fanout_slot.clone(),
        },
        timestamp_epoch_ms: system_time_epoch_ms(state.started_at),
        metadata: state.execution_metadata.clone(),
    }
}

fn agent_lifecycle_progress_event_from_state(
    state: &SpawnedAgentState,
) -> Option<AgentProgressEvent> {
    use crate::orchestration::spawner::agent_status_to_progress_event;

    let event_type =
        agent_status_to_progress_event(&state.status, &state.metrics, state.started_at)?;
    if !event_type.is_terminal() && !matches!(event_type, ProgressEventType::Waiting { .. }) {
        return None;
    }
    Some(AgentProgressEvent {
        agent_id: state.agent_id.clone(),
        event_type,
        timestamp_epoch_ms: SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0),
        metadata: state.execution_metadata.clone(),
    })
}

fn missing_agent_lifecycle_sse_event(
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    event: AgentProgressEvent,
) -> Option<Value> {
    let key = agent_progress_lifecycle_event_key(&event)?;
    if has_agent_progress_lifecycle_event_sent(sent_lifecycle_events, &key) {
        return None;
    }
    let sse = server_loop_host::progress_event_to_sse(&event)?;
    mark_agent_progress_lifecycle_event_sent(sent_lifecycle_events, key);
    Some(sse)
}

pub(super) async fn collect_missing_agent_lifecycle_events(
    spawner: &DynamicAgentSpawner,
    root_run_id: &str,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
) -> Vec<Value> {
    let states = spawner.get_agent_states_for_run_tree(root_run_id).await;
    let mut events = Vec::new();
    for state in states {
        if let Some(event) = missing_agent_lifecycle_sse_event(
            sent_lifecycle_events,
            agent_spawned_progress_event_from_state(&state),
        ) {
            events.push(event);
        }
        if let Some(event) = agent_lifecycle_progress_event_from_state(&state)
            .and_then(|event| missing_agent_lifecycle_sse_event(sent_lifecycle_events, event))
        {
            events.push(event);
        }
    }
    events
}

pub(super) async fn collect_agent_lifecycle_events_for_persistence(
    spawner: &DynamicAgentSpawner,
    root_run_id: &str,
) -> Vec<Value> {
    let states = spawner.get_agent_states_for_run_tree(root_run_id).await;
    let mut events = Vec::new();
    for state in states {
        if let Some(event) = server_loop_host::progress_event_to_sse(
            &agent_spawned_progress_event_from_state(&state),
        ) {
            events.push(event);
        }
        if let Some(event) = agent_lifecycle_progress_event_from_state(&state)
            .and_then(|event| server_loop_host::progress_event_to_sse(&event))
        {
            events.push(event);
        }
    }
    events
}

fn agent_lifecycle_dedupe_key(event: &Value) -> Option<String> {
    let event_type = durable_event_type(event)?;
    if !matches!(
        event_type,
        "agent_spawned"
            | "agent_completed"
            | "agent_failed"
            | "agent_waiting"
            | "agent_cancelled"
            | "agent_interrupted"
    ) {
        return None;
    }
    let agent_id = event.get("agent_id").and_then(Value::as_str)?;
    let status = event
        .get("status")
        .or_else(|| event.get("reason"))
        .or_else(|| event.get("termination"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!("{event_type}:{agent_id}:{status}"))
}

pub(super) fn merge_agent_lifecycle_before_terminal_events(
    final_events: &[Value],
    agent_lifecycle_events: &[Value],
) -> Vec<Value> {
    let mut out = Vec::new();
    let existing_lifecycle_keys: HashSet<String> = final_events
        .iter()
        .filter_map(agent_lifecycle_dedupe_key)
        .collect();
    let agent_lifecycle_events: Vec<Value> = agent_lifecycle_events
        .iter()
        .filter(|event| match agent_lifecycle_dedupe_key(event) {
            Some(key) => !existing_lifecycle_keys.contains(&key),
            None => true,
        })
        .cloned()
        .collect();
    let mut inserted_lifecycle = false;
    for event in final_events {
        if streaming_final_event_for_replay(event) && !inserted_lifecycle {
            out.extend(agent_lifecycle_events.iter().cloned());
            inserted_lifecycle = true;
        }
        if streaming_event_for_persistence(event) {
            out.push(event.clone());
        }
    }
    if !inserted_lifecycle {
        out.extend(agent_lifecycle_events);
    }
    out
}
