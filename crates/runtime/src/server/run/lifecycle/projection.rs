//! Agent progress streaming and SSE projection.
//!
//! This module handles:
//! - Agent progress event streaming to SSE clients
//! - Lifecycle event deduplication
//! - Agent live event to work surface SSE conversion
//! - Agent spawner state to progress event conversion

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveGap, AgentLiveSendError,
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
        ProgressEventType::AgentSpawned { .. } => AgentProgressLifecycleEventKind::Spawned {
            run_id: event.run_id.clone(),
        },
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
    let mut guard = sent_lifecycle_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Hard cap: in pathological runs with many fan-out slots, the HashSet can
    // grow unboundedly. The cap is far above any realistic run size; eviction
    // here means duplicate events could be re-sent, which is harmless (SSE
    // clients are idempotent for lifecycle events).
    const MAX_LIFECYCLE_DEDUP_ENTRIES: usize = 10_000;
    if guard.len() >= MAX_LIFECYCLE_DEDUP_ENTRIES {
        guard.clear();
        tracing::warn!(
            "sent_lifecycle_events reached cap ({MAX_LIFECYCLE_DEDUP_ENTRIES}); clearing dedup set"
        );
    }
    guard.insert(key);
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
    gap_tracker: WorkSurfaceAgentLiveGapTracker,
}

impl WorkSurfaceAgentLiveEventSink {
    pub(super) fn new(
        tx: mpsc::Sender<Value>,
        execution_metadata: Option<Value>,
        gap_tracker: WorkSurfaceAgentLiveGapTracker,
    ) -> Self {
        Self {
            tx,
            execution_metadata,
            gap_tracker,
        }
    }

    fn record_gap(&self, gap: AgentLiveGap) {
        self.gap_tracker.record(gap);
    }
}

impl AgentLiveEventSink for WorkSurfaceAgentLiveEventSink {
    fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError> {
        let value = agent_live_event_to_work_surface_sse(&event, self.execution_metadata.as_ref());
        match self.tx.try_send(value) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // Progress is coalescible, but a drop must be observable. A
                // an independent gap tracker wakes the stream fanout, so the
                // client repairs from authoritative state even if this was
                // the final event emitted by the child.
                self.record_gap(AgentLiveGap {
                    run_id: event.run_id.clone(),
                    agent_id: event.agent_id.clone(),
                    dropped_event_count: 1,
                });
                Err(AgentLiveSendError::Dropped)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(AgentLiveSendError::Closed)
            }
        }
    }

    fn send_gap(&self, gap: AgentLiveGap) -> Result<(), AgentLiveSendError> {
        self.record_gap(gap);
        Ok(())
    }
}

/// Coalesces lost live activity outside the bounded event queue. A watch
/// revision wakes the stream fanout even when the queue stays full and no
/// later agent event arrives, while the map retains one repair fact per
/// durable run/agent identity.
#[derive(Debug, Clone)]
pub(super) struct WorkSurfaceAgentLiveGapTracker {
    pending_gaps: Arc<Mutex<PendingAgentLiveGaps>>,
    revision_tx: watch::Sender<u64>,
}

impl WorkSurfaceAgentLiveGapTracker {
    pub(super) fn new() -> (Self, watch::Receiver<u64>) {
        let (revision_tx, revision_rx) = watch::channel(0);
        (
            Self {
                pending_gaps: Arc::new(Mutex::new(PendingAgentLiveGaps::default())),
                revision_tx,
            },
            revision_rx,
        )
    }

    fn record(&self, gap: AgentLiveGap) {
        lock_pending_agent_live_gaps(&self.pending_gaps).record(gap);
        self.revision_tx.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }

    pub(super) fn drain(&self) -> Vec<AgentLiveGap> {
        lock_pending_agent_live_gaps(&self.pending_gaps).drain()
    }
}

#[derive(Debug, Default)]
struct PendingAgentLiveGaps {
    by_run_and_agent: BTreeMap<(String, String), u64>,
}

impl PendingAgentLiveGaps {
    fn record(&mut self, gap: AgentLiveGap) {
        let count = self
            .by_run_and_agent
            .entry((gap.run_id, gap.agent_id))
            .or_default();
        *count = count.saturating_add(gap.dropped_event_count);
    }

    fn drain(&mut self) -> Vec<AgentLiveGap> {
        std::mem::take(&mut self.by_run_and_agent)
            .into_iter()
            .map(|((run_id, agent_id), dropped_event_count)| AgentLiveGap {
                run_id,
                agent_id,
                dropped_event_count,
            })
            .collect()
    }
}

fn lock_pending_agent_live_gaps(
    pending_gaps: &Arc<Mutex<PendingAgentLiveGaps>>,
) -> std::sync::MutexGuard<'_, PendingAgentLiveGaps> {
    match pending_gaps.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(
                target: "astra_runtime::work_surface",
                "agent live gap tracker lock poisoned; recovering pending gap facts"
            );
            poisoned.into_inner()
        }
    }
}

pub(super) fn agent_live_gap_to_work_surface_sse(gap: AgentLiveGap) -> Value {
    json!({
        "type": "agent_live_gap",
        "run_id": gap.run_id,
        "agent_id": gap.agent_id,
        "dropped_event_count": gap.dropped_event_count,
        "repair": "refresh_run_snapshot",
    })
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
        AgentLiveEventKind::OutputDelta {
            model_item_id,
            text: content,
        } => json!({
            "model_item_id": model_item_id,
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "output_delta",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ThinkingDelta {
            model_item_id,
            text: content,
        } => json!({
            "model_item_id": model_item_id,
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "thinking_delta",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::Status { text: content } => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "status",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::Signal(signal) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "signal",
            "signal": signal,
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
                AgentLiveTermination::Delegated => "delegated",
                AgentLiveTermination::Failed => "failed",
                AgentLiveTermination::Interrupted => "interrupted",
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
    if let Some(fields) = value.as_object_mut() {
        fields.insert("run_id".to_string(), Value::String(event.run_id.clone()));
    }
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
    for key in ["workspace", "executor", "transport"] {
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
        run_id: state.run_id.clone(),
        parent_run_id: state.parent_run_id.clone(),
        event_type: ProgressEventType::AgentSpawned {
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
        run_id: state.run_id.clone(),
        parent_run_id: state.parent_run_id.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn live_event(run_id: &str, agent_id: &str, content: &str) -> AgentLiveEvent {
        AgentLiveEvent {
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            kind: AgentLiveEventKind::Status {
                text: content.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn dropped_live_events_wake_the_independent_gap_tracker_without_followup() {
        let (tx, mut rx) = mpsc::channel(2);
        let (gap_tracker, mut gap_revision) = WorkSurfaceAgentLiveGapTracker::new();
        let sink = WorkSurfaceAgentLiveEventSink::new(tx.clone(), None, gap_tracker.clone());
        tx.try_send(json!({"type": "backlog"}))
            .expect("first backlog item");
        tx.try_send(json!({"type": "backlog"}))
            .expect("second backlog item");

        assert!(matches!(
            sink.send(live_event("run-a", "reviewer", "dropped")),
            Err(AgentLiveSendError::Dropped)
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), gap_revision.changed())
            .await
            .expect("drop must wake the independent gap lane")
            .expect("gap tracker remains alive");
        assert_eq!(
            gap_tracker.drain(),
            vec![AgentLiveGap {
                run_id: "run-a".into(),
                agent_id: "reviewer".into(),
                dropped_event_count: 1,
            }]
        );
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn forwarded_live_gap_preserves_its_reconciliation_identity_and_count() {
        let (tx, _rx) = mpsc::channel(1);
        let (gap_tracker, mut gap_revision) = WorkSurfaceAgentLiveGapTracker::new();
        let sink = WorkSurfaceAgentLiveEventSink::new(tx, None, gap_tracker.clone());

        sink.send_gap(AgentLiveGap {
            run_id: "run-nested".into(),
            agent_id: "reviewer@child".into(),
            dropped_event_count: 4,
        })
        .expect("a forwarded gap is a durable-reconciliation fact, not a stream write");

        tokio::time::timeout(std::time::Duration::from_secs(1), gap_revision.changed())
            .await
            .expect("forwarded gap must wake the independent repair lane")
            .expect("gap tracker remains alive");
        assert_eq!(
            gap_tracker.drain(),
            vec![AgentLiveGap {
                run_id: "run-nested".into(),
                agent_id: "reviewer@child".into(),
                dropped_event_count: 4,
            }]
        );
    }
}

use super::spawn_observed;
use super::{
    AttachedStreamDelivery, DURABLE_LIVE_BATCH_FLUSH_INTERVAL, DurableLiveFanoutControl,
    DurableToolTerminalTracker, PendingDurableLiveEvents, deliver_live_fanout_event,
    flush_durable_live_events, flush_host_event_gap_recovery, process_ordered_live_fanout_event,
    publish_live_persistence_failure, record_unforwarded_host_event,
    record_unforwarded_host_event_tail,
};

impl super::AgenticRunLifecycleService {
    pub(super) fn prepare_stream_delivery(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        run_state: &mut super::RunState,
        host: &mut server_loop_host::ServerAgenticLoopHost,
        event_channel: (mpsc::Sender<Value>, mpsc::Receiver<Value>),
    ) -> (super::OwnedStreamDelivery, mpsc::Receiver<Value>) {
        let user_id = user_id.to_owned();
        let session_id = session_id.to_owned();
        let run_id = run_id.to_owned();
        // Network observer delivery is bounded. Internal producers are
        // drained independently below so browser backpressure cannot drop an
        // approval or permanently detach later host progress.
        const SSE_CHANNEL_CAPACITY: usize = 512;
        let (client_event_tx, event_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (event_tx, mut fanout_rx) = event_channel;
        let (fanout_control_tx, mut fanout_control_rx) =
            mpsc::channel::<DurableLiveFanoutControl>(1);
        let durable_tool_terminals = DurableToolTerminalTracker::default();
        let fanout_durable_tool_terminals = durable_tool_terminals.clone();
        let (agent_live_gap_tracker, mut agent_live_gap_rx) = WorkSurfaceAgentLiveGapTracker::new();
        let (live_tx, _) = broadcast::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let live_tx_for_fanout = live_tx.clone();
        let mut client_event_tx_for_fanout = AttachedStreamDelivery::new(client_event_tx.clone());
        let fanout_runs = self.runs_handle();
        let fanout_run_engine = self.run_engine.clone();
        let fanout_user_id = user_id.clone();
        let fanout_session_id = session_id.clone();
        let fanout_run_id = run_id.clone();
        let fanout_gap_tracker = agent_live_gap_tracker.clone();
        let _ = spawn_observed(
            async move {
                let mut gap_watch_open = true;
                let mut control_open = true;
                let mut pending = PendingDurableLiveEvents::default();
                let flush_deadline = tokio::time::sleep(DURABLE_LIVE_BATCH_FLUSH_INTERVAL);
                tokio::pin!(flush_deadline);
                loop {
                    tokio::select! {
                        event = fanout_rx.recv() => {
                            let Some(event) = event else {
                                if let Err(error) = flush_durable_live_events(
                                    &mut pending,
                                    &fanout_run_engine,
                                    &fanout_runs,
                                    &fanout_user_id,
                                    &fanout_session_id,
                                    &fanout_run_id,
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_durable_tool_terminals,
                                ).await {
                                    publish_live_persistence_failure(
                                        &fanout_runs,
                                        &live_tx_for_fanout,
                                        &mut client_event_tx_for_fanout,
                                        &fanout_user_id,
                                        &fanout_run_id,
                                        "live_event_persistence_failed",
                                        "live run event could not be recorded durably",
                                        &error,
                                    ).await;
                                }
                                for gap in fanout_gap_tracker.drain() {
                                    let event = agent_live_gap_to_work_surface_sse(gap);
                                    deliver_live_fanout_event(
                                        &live_tx_for_fanout,
                                        &mut client_event_tx_for_fanout,
                                        &fanout_run_id,
                                        event,
                                    )
                                    .await;
                                }
                                break;
                            };
                            let starts_new_batch = pending.is_empty();
                            if let Err(error) = process_ordered_live_fanout_event(
                                event,
                                &mut pending,
                                &fanout_run_engine,
                                &fanout_runs,
                                &fanout_user_id,
                                &fanout_session_id,
                                &fanout_run_id,
                                &live_tx_for_fanout,
                                &mut client_event_tx_for_fanout,
                                &fanout_durable_tool_terminals,
                            ).await {
                                publish_live_persistence_failure(
                                    &fanout_runs,
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_user_id,
                                    &fanout_run_id,
                                    error.code,
                                    error.message,
                                    &error.detail,
                                ).await;
                                break;
                            }
                            if starts_new_batch && !pending.is_empty() {
                                flush_deadline.as_mut().reset(
                                    tokio::time::Instant::now() + DURABLE_LIVE_BATCH_FLUSH_INTERVAL,
                                );
                            }
                        }
                        control = fanout_control_rx.recv(), if control_open => {
                            let Some(DurableLiveFanoutControl::Flush { ack }) = control else {
                                control_open = false;
                                continue;
                            };
                            let mut result = Ok(());
                            while let Ok(event) = fanout_rx.try_recv() {
                                if let Err(error) = process_ordered_live_fanout_event(
                                    event,
                                    &mut pending,
                                    &fanout_run_engine,
                                    &fanout_runs,
                                    &fanout_user_id,
                                    &fanout_session_id,
                                    &fanout_run_id,
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_durable_tool_terminals,
                                ).await {
                                    publish_live_persistence_failure(
                                        &fanout_runs,
                                        &live_tx_for_fanout,
                                        &mut client_event_tx_for_fanout,
                                        &fanout_user_id,
                                        &fanout_run_id,
                                        error.code,
                                        error.message,
                                        &error.detail,
                                    ).await;
                                    result = Err(error.detail);
                                    break;
                                }
                            }
                            if result.is_ok() {
                                result = flush_durable_live_events(
                                    &mut pending,
                                    &fanout_run_engine,
                                    &fanout_runs,
                                    &fanout_user_id,
                                    &fanout_session_id,
                                    &fanout_run_id,
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_durable_tool_terminals,
                                ).await;
                                if let Err(error) = &result {
                                    publish_live_persistence_failure(
                                        &fanout_runs,
                                        &live_tx_for_fanout,
                                        &mut client_event_tx_for_fanout,
                                        &fanout_user_id,
                                        &fanout_run_id,
                                        "live_event_persistence_failed",
                                        "live run event could not be recorded durably",
                                        error,
                                    ).await;
                                }
                            }
                            let failed = result.is_err();
                            let _ = ack.send(result);
                            if failed {
                                break;
                            }
                        }
                        _ = &mut flush_deadline, if !pending.is_empty() => {
                            if let Err(error) = flush_durable_live_events(
                                &mut pending,
                                &fanout_run_engine,
                                &fanout_runs,
                                &fanout_user_id,
                                &fanout_session_id,
                                &fanout_run_id,
                                &live_tx_for_fanout,
                                &mut client_event_tx_for_fanout,
                                &fanout_durable_tool_terminals,
                            ).await {
                                publish_live_persistence_failure(
                                    &fanout_runs,
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_user_id,
                                    &fanout_run_id,
                                    "live_event_persistence_failed",
                                    "live run event could not be recorded durably",
                                    &error,
                                ).await;
                                break;
                            }
                        }
                        changed = agent_live_gap_rx.changed(), if gap_watch_open => {
                            if changed.is_err() {
                                gap_watch_open = false;
                                continue;
                            }
                            for gap in fanout_gap_tracker.drain() {
                                let event = agent_live_gap_to_work_surface_sse(gap);
                                if let Err(error) = flush_durable_live_events(
                                    &mut pending,
                                    &fanout_run_engine,
                                    &fanout_runs,
                                    &fanout_user_id,
                                    &fanout_session_id,
                                    &fanout_run_id,
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_durable_tool_terminals,
                                ).await {
                                    publish_live_persistence_failure(
                                        &fanout_runs,
                                        &live_tx_for_fanout,
                                        &mut client_event_tx_for_fanout,
                                        &fanout_user_id,
                                        &fanout_run_id,
                                        "live_event_persistence_failed",
                                        "live run event could not be recorded durably",
                                        &error,
                                    ).await;
                                    break;
                                }
                                deliver_live_fanout_event(
                                    &live_tx_for_fanout,
                                    &mut client_event_tx_for_fanout,
                                    &fanout_run_id,
                                    event,
                                ).await;
                            }
                        }
                    }
                }
            },
            "sse_fanout",
        );
        let progress_bridge =
            self.spawn_agent_progress_stream_bridge(run_id.clone(), event_tx.clone());

        run_state.live_tx = Some(live_tx.clone());
        run_state.attached_event_tx = Some(client_event_tx.downgrade());

        const HOST_EVENT_CHANNEL_CAPACITY: usize = 256;
        let (host_event_tx, mut host_event_rx) =
            mpsc::channel::<Value>(HOST_EVENT_CHANNEL_CAPACITY);
        let host_event_gap = server_loop_host::HostEventGapTracker::default();
        let bridge_gap = host_event_gap.clone();
        let host_event_bridge_tx = event_tx.clone();
        let host_event_server_run_id = run_id.clone();
        let host_event_bridge = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = host_event_rx.recv() => {
                        let Some(event) = event else { break; };
                        if !flush_host_event_gap_recovery(
                            &host_event_bridge_tx,
                            &bridge_gap,
                            &host_event_server_run_id,
                        )
                        .await
                        {
                            record_unforwarded_host_event_tail(&mut host_event_rx, &bridge_gap);
                            return;
                        }
                        let explain_event_id = (event.get("type").and_then(Value::as_str)
                            == Some("explain_analyze"))
                            .then(|| event.get("event_id").and_then(Value::as_str))
                            .flatten()
                            .map(str::to_owned);
                        if let Err(error) = host_event_bridge_tx.send(event).await {
                            record_unforwarded_host_event(&bridge_gap, error.0);
                            record_unforwarded_host_event_tail(&mut host_event_rx, &bridge_gap);
                            return;
                        }
                        if let Some(event_id) = explain_event_id {
                            bridge_gap.acknowledge_explain_analyze_delivery(&event_id);
                        }
                    }
                    _ = bridge_gap.notified() => {
                        if !flush_host_event_gap_recovery(
                            &host_event_bridge_tx,
                            &bridge_gap,
                            &host_event_server_run_id,
                        )
                        .await
                        {
                            record_unforwarded_host_event_tail(&mut host_event_rx, &bridge_gap);
                            return;
                        }
                    }
                }
            }
            let _ = flush_host_event_gap_recovery(
                &host_event_bridge_tx,
                &bridge_gap,
                &host_event_server_run_id,
            )
            .await;
        });
        host.set_event_tx_with_gap(host_event_tx, host_event_gap.clone());

        (
            super::OwnedStreamDelivery {
                event_tx,
                fanout_control_tx,
                durable_tool_terminals,
                host_event_bridge,
                host_event_gap,
                progress_bridge,
                agent_live_gap_tracker,
            },
            event_rx,
        )
    }
}
