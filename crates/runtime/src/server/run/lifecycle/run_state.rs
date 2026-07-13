//! Run status, state, and event-persistence helpers extracted from lifecycle.
//!
//! This module defines [`RunStatus`] (the state machine for an individual agentic
//! run) and [`RunState`] (per-run in-memory bookkeeping). The helper functions
//! classify persisted and streaming events, merge terminal events, and enforce
//! the active-run live-event budget.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED, STATUS_PAUSED,
    STATUS_RUNNING, STATUS_WAITING,
};
use astra_runtime_env::CleanupReason as RuntimeCleanupReason;
use astra_services::runs::{
    DurableRunRecord, DurableRunStatusKind, durable_run_status_blocks_session,
    durable_run_status_kind,
};

use super::MAX_ACTIVE_RUN_LIVE_EVENTS;
use crate::server::run::engine::RunEngine;

pub const MAX_DURABLE_RUN_EVENT_BATCH_ROWS: usize = MAX_ACTIVE_RUN_LIVE_EVENTS;
pub const MAX_DURABLE_RUN_EVENT_BATCH_BYTES: usize = 2 * 1024 * 1024;
pub const MIN_DURABLE_RUN_EVENT_BATCH_ROWS: usize = 16;
pub const MIN_DURABLE_RUN_EVENT_BATCH_BYTES: usize = 64 * 1024;
const DURABLE_RUN_EVENT_COMPACTION_SUMMARY_BYTES_RESERVE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableRunEventBatchBudget {
    pub row_budget: usize,
    pub byte_budget: usize,
}

impl Default for DurableRunEventBatchBudget {
    fn default() -> Self {
        Self {
            row_budget: MAX_DURABLE_RUN_EVENT_BATCH_ROWS,
            byte_budget: MAX_DURABLE_RUN_EVENT_BATCH_BYTES,
        }
    }
}

impl DurableRunEventBatchBudget {
    pub fn new(row_budget: usize, byte_budget: usize) -> Self {
        Self {
            row_budget: row_budget.max(MIN_DURABLE_RUN_EVENT_BATCH_ROWS),
            byte_budget: byte_budget.max(MIN_DURABLE_RUN_EVENT_BATCH_BYTES),
        }
    }
}

/// Status of a single agentic run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    Waiting,
    Completed,
    Delegated,
    Failed,
    Cancelled,
}

/// Explicit controls a client may request for a durable run.
///
/// This intentionally distinguishes user controls from every legal state
/// transition: for example, a waiting run can return to `Running` when its
/// required input arrives, but that is not a generic user-facing Resume
/// control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunControlAction {
    Pause,
    Resume,
    Cancel,
}

impl RunStatus {
    #[cfg(test)]
    pub const ALL: [Self; 7] = [
        Self::Running,
        Self::Paused,
        Self::Waiting,
        Self::Completed,
        Self::Delegated,
        Self::Failed,
        Self::Cancelled,
    ];

    #[cfg(test)]
    pub const TERMINAL: [Self; 4] = [
        Self::Completed,
        Self::Delegated,
        Self::Failed,
        Self::Cancelled,
    ];

    /// Legal state-machine edges for an agentic run.
    ///
    /// Terminal states intentionally have no outgoing edges. A paused run does
    /// not transition to `Completed` during loop finalization; the terminal
    /// completion is buffered and promoted only by an explicit resume path.
    pub const TRANSITION_EDGES: [(Self, Self); 13] = [
        (Self::Running, Self::Paused),
        (Self::Running, Self::Waiting),
        (Self::Running, Self::Completed),
        (Self::Running, Self::Delegated),
        (Self::Running, Self::Failed),
        (Self::Running, Self::Cancelled),
        (Self::Paused, Self::Running),
        (Self::Paused, Self::Waiting),
        (Self::Paused, Self::Failed),
        (Self::Paused, Self::Cancelled),
        (Self::Waiting, Self::Running),
        (Self::Waiting, Self::Failed),
        (Self::Waiting, Self::Cancelled),
    ];

    /// User-requestable transitions. This is the single source of truth for
    /// both server control admission and the controls published in a run-tree
    /// snapshot.
    const CONTROL_TRANSITIONS: [(RunControlAction, Self, Self); 5] = [
        (RunControlAction::Pause, Self::Running, Self::Paused),
        (RunControlAction::Resume, Self::Paused, Self::Running),
        (RunControlAction::Cancel, Self::Running, Self::Cancelled),
        (RunControlAction::Cancel, Self::Paused, Self::Cancelled),
        (RunControlAction::Cancel, Self::Waiting, Self::Cancelled),
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => STATUS_RUNNING,
            Self::Paused => STATUS_PAUSED,
            Self::Waiting => STATUS_WAITING,
            Self::Completed => STATUS_COMPLETED,
            Self::Delegated => STATUS_DELEGATED,
            Self::Failed => STATUS_FAILED,
            Self::Cancelled => STATUS_CANCELLED,
        }
    }

    pub fn from_durable_status(status: &str) -> Option<Self> {
        match durable_run_status_kind(status) {
            DurableRunStatusKind::Running => Some(Self::Running),
            DurableRunStatusKind::Paused => Some(Self::Paused),
            DurableRunStatusKind::Waiting => Some(Self::Waiting),
            DurableRunStatusKind::Completed => Some(Self::Completed),
            DurableRunStatusKind::Delegated => Some(Self::Delegated),
            DurableRunStatusKind::Failed => Some(Self::Failed),
            DurableRunStatusKind::Cancelled => Some(Self::Cancelled),
            DurableRunStatusKind::Other => None,
        }
    }

    pub fn is_resumable(&self) -> bool {
        matches!(self, Self::Paused | Self::Waiting)
    }

    #[cfg(test)]
    pub fn is_terminal(&self) -> bool {
        Self::TERMINAL.contains(self)
    }

    pub fn blocks_session(&self, waiting_for: Option<&str>) -> bool {
        match self {
            Self::Running | Self::Waiting => true,
            Self::Paused => waiting_for.is_some(),
            Self::Completed | Self::Delegated | Self::Failed | Self::Cancelled => false,
        }
    }

    pub fn can_transition_to(&self, next: &RunStatus) -> bool {
        Self::TRANSITION_EDGES
            .iter()
            .any(|(from, to)| from == self && to == next)
    }

    /// Whether this durable state admits a user-requested control.
    pub fn can_apply_control_action(&self, action: RunControlAction) -> bool {
        Self::CONTROL_TRANSITIONS
            .iter()
            .any(|(candidate, from, _)| *candidate == action && from == self)
    }

    /// Nominal target state for a valid control. `resume` may still promote a
    /// buffered completion instead of restarting execution; the lifecycle
    /// handler owns that exceptional finalization path.
    pub fn control_action_target(&self, action: RunControlAction) -> Option<Self> {
        Self::CONTROL_TRANSITIONS
            .iter()
            .find_map(|(candidate, from, target)| {
                (*candidate == action && from == self).then_some(*target)
            })
    }

    pub fn available_control_actions(&self) -> impl Iterator<Item = RunControlAction> + '_ {
        [
            RunControlAction::Pause,
            RunControlAction::Resume,
            RunControlAction::Cancel,
        ]
        .into_iter()
        .filter(|action| self.can_apply_control_action(*action))
    }

    /// Validate a status transition. Returns `Err` if the transition is illegal.
    pub fn try_transition(&self, next: &RunStatus) -> Result<(), String> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(format!(
                "invalid run status transition: {:?} → {:?}",
                self, next
            ))
        }
    }
}

pub fn cleanup_reason_for_terminal_run_status(status: &RunStatus) -> Option<RuntimeCleanupReason> {
    match status {
        RunStatus::Completed => Some(RuntimeCleanupReason::Completed),
        RunStatus::Delegated => Some(RuntimeCleanupReason::Delegated),
        RunStatus::Failed => Some(RuntimeCleanupReason::Failed),
        RunStatus::Cancelled => Some(RuntimeCleanupReason::Cancelled),
        RunStatus::Running | RunStatus::Paused | RunStatus::Waiting => None,
    }
}

pub fn is_run_finished_event(event: &Value) -> bool {
    event.get("event_type").and_then(Value::as_str) == Some("run_finished")
}

pub fn user_intent_text_len(input: &Value) -> usize {
    input
        .get("content")
        .or_else(|| input.get("text"))
        .and_then(Value::as_str)
        .map(|text| text.chars().count())
        .unwrap_or(0)
}

pub fn is_completed_run_finished_event(event: &Value) -> bool {
    if !is_run_finished_event(event) {
        return false;
    }
    let data = event.get("data").and_then(Value::as_object);
    let cancelled = data
        .and_then(|obj| obj.get("cancelled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let interrupted = data
        .and_then(|obj| obj.get("interrupted"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    !cancelled && !interrupted
}

pub fn has_buffered_terminal_completion(events: &[Value]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| is_run_finished_event(event))
        .is_some_and(is_completed_run_finished_event)
}

/// Whether another live process can still observe a durable pause/resume
/// transition. A stored owner id without an unexpired lease is historical
/// metadata, not evidence that an executor exists.
pub fn durable_run_owner_lease_is_live(run: &DurableRunRecord) -> bool {
    durable_execution_fields_are_live(
        &run.status,
        run.waiting_for.as_deref(),
        run.owner_pod_id.as_deref(),
        run.owner_lease_expires_at.as_deref(),
    )
}

fn durable_execution_fields_are_live(
    status: &str,
    waiting_for: Option<&str>,
    owner_pod_id: Option<&str>,
    owner_lease_expires_at: Option<&str>,
) -> bool {
    if !durable_run_status_blocks_session(status, waiting_for) {
        return false;
    }
    owner_lease_fields_are_live(owner_pod_id, owner_lease_expires_at)
}

fn owner_lease_fields_are_live(owner_pod_id: Option<&str>, expires_at: Option<&str>) -> bool {
    if owner_pod_id.is_none_or(|owner| owner.trim().is_empty()) {
        return false;
    }
    let Some(expires_at) = expires_at else {
        return false;
    };
    let expires_at = chrono::DateTime::parse_from_rfc3339(expires_at)
        .map(|value| value.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%d %H:%M:%S%.f")
                .map(|value| value.and_utc())
        });
    expires_at.is_ok_and(|expires_at| expires_at > chrono::Utc::now())
}

pub fn should_preserve_manual_pause_on_completion(
    current_status: &RunStatus,
    final_status: &RunStatus,
) -> bool {
    *current_status == RunStatus::Paused && *final_status == RunStatus::Completed
}

pub async fn should_preserve_manual_pause_from_durable(
    run_engine: &RunEngine,
    user_id: &str,
    run_id: &str,
    final_status: &RunStatus,
) -> bool {
    if *final_status != RunStatus::Completed {
        return false;
    }
    match run_engine.load_run(user_id, run_id).await {
        Ok(Some(run)) => run.status == STATUS_PAUSED,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                %run_id,
                error = %error,
                "failed to reload durable run while checking late completion pause preservation"
            );
            false
        }
    }
}

pub fn merge_run_finished_event_data(target: &mut Value, source: &Value) {
    let source_data = source
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(target_obj) = target.as_object_mut() else {
        return;
    };
    let target_data = target_obj
        .entry("data".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(target_data_obj) = target_data.as_object_mut() else {
        return;
    };
    for (key, value) in source_data {
        target_data_obj.insert(key, value);
    }
}

pub fn merge_cancelled_run_events(run: &mut RunState, mut finalized_events: Vec<Value>) {
    let terminal_event = finalized_events
        .last()
        .filter(|event| is_run_finished_event(event))
        .cloned();
    if terminal_event.is_some() {
        finalized_events.pop();
    }

    let insert_at = run
        .events
        .last()
        .filter(|event| is_run_finished_event(event))
        .map(|_| run.events.len().saturating_sub(1))
        .unwrap_or(run.events.len());
    run.events.splice(insert_at..insert_at, finalized_events);

    if let Some(terminal_event) = terminal_event {
        if let Some(existing_terminal) = run
            .events
            .last_mut()
            .filter(|event| is_run_finished_event(event))
        {
            merge_run_finished_event_data(existing_terminal, &terminal_event);
        } else {
            run.events.push(terminal_event);
        }
    }
}

pub fn durable_event_type(event: &Value) -> Option<&str> {
    event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(Value::as_str)
}

pub fn terminal_events_for_persistence(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            matches!(
                durable_event_type(event),
                Some(
                    "text_done"
                        | "runtime.control.handoff.requested"
                        | "runtime.control.handoff.rejected"
                        | "run_error"
                        | "run_interrupted"
                        | "run_waiting"
                        | "run_finished"
                        // Keep reasoning/thinking completion markers, not raw
                        // chain-of-thought content or incremental deltas.
                        | "reasoning_done"
                        | "thinking_done"
                )
            )
        })
        .cloned()
        .collect()
}

pub fn streaming_final_event_for_replay(event: &Value) -> bool {
    matches!(
        durable_event_type(event),
        Some("text_done" | "run_error" | "run_interrupted" | "run_waiting" | "run_finished")
    )
}

pub fn streaming_event_for_persistence(event: &Value) -> bool {
    streaming_final_event_for_replay(event) || live_delta_event_for_persistence(event)
}

pub(super) fn durable_replay_boundary_event(event: &Value) -> bool {
    matches!(
        durable_event_type(event),
        Some(
            "tool_call"
                | "tool_call_start"
                | "tool_call_end"
                | "tool_result"
                | "run_blocked"
                | "workspace_bound"
                | "executor_bound"
                | "executor_status_changed"
                | "tool_routing_decision"
                | "tool_transport_started"
                | "tool_transport_completed"
                | "tool_transport_failed"
                | "approval_request"
                | "approval_required"
                | "approval_batch_required"
                | "ask_user_prompted"
                | "user_prompt_required"
                | "user_input"
                | "user_intent"
                | "agent_communication"
                | "agent_delegated"
                | "agent_spawned"
                | "agent_completed"
                | "agent_failed"
                | "agent_waiting"
                | "agent_cancelled"
                | "agent_interrupted"
                // Keep completion markers as replay boundaries. Raw
                // reasoning_message_content remains live-only.
                | "reasoning_done"
                | "thinking_done"
        )
    )
}

pub fn durable_run_event_estimated_bytes(event: &Value) -> usize {
    serde_json::to_vec(event)
        .map(|bytes| bytes.len())
        .unwrap_or_else(|_| event.to_string().len())
}

fn retain_budgeted_events(
    events: &[Value],
    event_bytes: &[usize],
    keep: &mut [bool],
    row_budget: usize,
    byte_budget: usize,
    retained_rows: &mut usize,
    retained_bytes: &mut usize,
    predicate: impl Fn(&Value) -> bool,
) {
    for idx in (0..events.len()).rev() {
        if keep[idx] || !predicate(&events[idx]) {
            continue;
        }
        if *retained_rows >= row_budget {
            continue;
        }
        let bytes = event_bytes[idx];
        if retained_bytes.saturating_add(bytes) > byte_budget {
            continue;
        }
        keep[idx] = true;
        *retained_rows += 1;
        *retained_bytes += bytes;
    }
}

pub fn enforce_durable_run_event_batch_budget(events: Vec<Value>) -> Vec<Value> {
    enforce_durable_run_event_batch_budget_with_budget(
        events,
        DurableRunEventBatchBudget::default(),
    )
}

pub fn enforce_durable_run_event_batch_budget_with_budget(
    events: Vec<Value>,
    budget: DurableRunEventBatchBudget,
) -> Vec<Value> {
    let budget = DurableRunEventBatchBudget::new(budget.row_budget, budget.byte_budget);
    if events.is_empty() {
        return events;
    }

    let event_bytes: Vec<usize> = events
        .iter()
        .map(durable_run_event_estimated_bytes)
        .collect();
    let original_bytes = event_bytes.iter().sum::<usize>();
    if events.len() <= budget.row_budget && original_bytes <= budget.byte_budget {
        return events;
    }

    let critical_events: Vec<bool> = events
        .iter()
        .map(streaming_final_event_for_replay)
        .collect();
    let critical_count = critical_events.iter().filter(|keep| **keep).count();
    let critical_bytes = event_bytes
        .iter()
        .zip(critical_events.iter())
        .filter_map(|(bytes, critical)| critical.then_some(*bytes))
        .sum::<usize>();

    let max_noncritical_rows = budget
        .row_budget
        .saturating_sub(critical_count)
        .saturating_sub(1);
    let max_noncritical_bytes = budget
        .byte_budget
        .saturating_sub(critical_bytes)
        .saturating_sub(DURABLE_RUN_EVENT_COMPACTION_SUMMARY_BYTES_RESERVE);

    let mut keep = critical_events;
    let mut kept_noncritical_rows = 0usize;
    let mut kept_noncritical_bytes = 0usize;
    retain_budgeted_events(
        &events,
        &event_bytes,
        &mut keep,
        max_noncritical_rows,
        max_noncritical_bytes,
        &mut kept_noncritical_rows,
        &mut kept_noncritical_bytes,
        durable_replay_boundary_event,
    );
    retain_budgeted_events(
        &events,
        &event_bytes,
        &mut keep,
        max_noncritical_rows,
        max_noncritical_bytes,
        &mut kept_noncritical_rows,
        &mut kept_noncritical_bytes,
        |_| true,
    );

    let dropped_count = keep.iter().filter(|keep| !**keep).count();
    if dropped_count == 0 {
        return events;
    }
    let dropped_bytes = event_bytes
        .iter()
        .zip(keep.iter())
        .filter_map(|(bytes, keep)| (!*keep).then_some(*bytes))
        .sum::<usize>();
    let summary = json!({
        "event_type": "durable_events_compacted",
        "data": {
            "reason": "durable_run_event_batch_budget_exceeded",
            "original_events": events.len(),
            "retained_events": events.len() - dropped_count,
            "dropped_events": dropped_count,
            "original_bytes_estimate": original_bytes,
            "dropped_bytes_estimate": dropped_bytes,
            "row_budget": budget.row_budget,
            "byte_budget": budget.byte_budget
        }
    });

    let mut out = Vec::with_capacity(events.len() - dropped_count + 1);
    let mut summary_inserted = false;
    for (idx, event) in events.into_iter().enumerate() {
        if keep[idx] {
            out.push(event);
        } else if !summary_inserted {
            out.push(summary.clone());
            summary_inserted = true;
        }
    }
    if !summary_inserted {
        out.push(summary);
    }
    out
}

pub fn live_delta_event_for_persistence(event: &Value) -> bool {
    let Some(event_type) = durable_event_type(event) else {
        return false;
    };
    if event_type == "run_blocked" {
        return true;
    }
    if event_type == "agent_live_event" {
        return !matches!(
            event.get("event_kind").and_then(Value::as_str),
            Some("output_delta" | "thinking_delta")
        );
    }
    matches!(
        event_type,
        // Reasoning/thinking deltas and raw reasoning_message_content are
        // live transport only; durable replay keeps completion markers.
        "reasoning_done"
            | "thinking_done"
            | "runtime.control.handoff.requested"
            | "runtime.control.handoff.rejected"
            | "workspace_bound"
            | "executor_bound"
            | "executor_status_changed"
            | "agent_communication"
            | "agent_delegated"
            | "agent_spawned"
            | "agent_progress"
            | "agent_completed"
            | "agent_failed"
            | "agent_waiting"
            | "agent_cancelled"
            | "agent_interrupted"
            | "task_board_snapshot"
            | "tool_call"
            | "tool_call_start"
            | "tool_routing_decision"
            | "tool_transport_started"
            | "tool_transport_completed"
            | "tool_transport_failed"
            | "tool_call_end"
    )
}

pub fn push_active_run_live_event(run: &mut RunState, event: Value) {
    run.events.push(event);
    while run
        .events
        .iter()
        .filter(|event| live_delta_event_for_persistence(event))
        .count()
        > MAX_ACTIVE_RUN_LIVE_EVENTS
    {
        let Some(oldest_live_event) = run.events.iter().position(live_delta_event_for_persistence)
        else {
            break;
        };
        run.events.remove(oldest_live_event);
    }
}

/// Per-run state held in the lifecycle service.
pub struct RunState {
    pub run_id: String,
    pub user_id: String,
    pub session_id: String,
    pub status: RunStatus,
    pub events: Vec<Value>,
    pub cancel_flag: Arc<AtomicBool>,
    pub pause_flag: Arc<AtomicBool>,
    /// Cancelled together with `cancel_flag` on `cancel_run` for low-latency LLM abort.
    pub llm_cancel_token: Arc<CancellationToken>,
    /// Live fanout for clients that reattach to an active run after navigating away.
    pub live_tx: Option<broadcast::Sender<Value>>,
    pub waiting_for: Option<String>,
    /// True only while an execution task still exists and can observe a pause
    /// flag or queued input. A resumable durable status alone is not proof that
    /// the process-local executor survived.
    pub execution_live: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_execution_liveness_requires_blocking_state_and_live_owner_lease() {
        let future = (chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc3339();
        let expired = (chrono::Utc::now() - chrono::Duration::minutes(2))
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string();

        assert!(owner_lease_fields_are_live(Some("pod-a"), Some(&future)));
        assert!(!owner_lease_fields_are_live(None, Some(&future)));
        assert!(!owner_lease_fields_are_live(Some(" "), Some(&future)));
        assert!(!owner_lease_fields_are_live(Some("pod-a"), None));
        assert!(!owner_lease_fields_are_live(
            Some("pod-a"),
            Some("not-a-timestamp")
        ));
        assert!(!owner_lease_fields_are_live(Some("pod-a"), Some(&expired)));

        assert!(durable_execution_fields_are_live(
            STATUS_RUNNING,
            None,
            Some("pod-a"),
            Some(&future)
        ));
        assert!(durable_execution_fields_are_live(
            STATUS_PAUSED,
            Some("user_resume"),
            Some("pod-a"),
            Some(&future)
        ));
        assert!(!durable_execution_fields_are_live(
            STATUS_PAUSED,
            None,
            Some("pod-a"),
            Some(&future)
        ));
        assert!(!durable_execution_fields_are_live(
            STATUS_COMPLETED,
            None,
            Some("pod-a"),
            Some(&future)
        ));
    }

    #[test]
    fn run_status_transition_matrix_is_exhaustive() {
        for from in RunStatus::ALL {
            for to in RunStatus::ALL {
                let expected = RunStatus::TRANSITION_EDGES
                    .iter()
                    .any(|(edge_from, edge_to)| *edge_from == from && *edge_to == to);
                assert_eq!(
                    from.can_transition_to(&to),
                    expected,
                    "can_transition_to mismatch for {from:?} -> {to:?}"
                );
                assert_eq!(
                    from.try_transition(&to).is_ok(),
                    expected,
                    "try_transition mismatch for {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn user_controls_are_derived_from_their_legal_transitions() {
        let cases = [
            (
                RunStatus::Running,
                vec![RunControlAction::Pause, RunControlAction::Cancel],
            ),
            (RunStatus::Waiting, vec![RunControlAction::Cancel]),
            (
                RunStatus::Paused,
                vec![RunControlAction::Resume, RunControlAction::Cancel],
            ),
            (RunStatus::Completed, vec![]),
            (RunStatus::Failed, vec![]),
            (RunStatus::Cancelled, vec![]),
        ];

        for (status, expected_actions) in cases {
            let actual_actions = status.available_control_actions().collect::<Vec<_>>();
            assert_eq!(actual_actions, expected_actions, "controls for {status:?}");
            for action in actual_actions {
                let target = status
                    .control_action_target(action)
                    .expect("published control has a target state");
                assert!(
                    status.can_transition_to(&target),
                    "control {action:?} must follow a legal transition: {status:?} -> {target:?}"
                );
            }
        }
    }

    #[test]
    fn terminal_run_statuses_have_no_outgoing_edges() {
        for from in RunStatus::TERMINAL {
            assert!(from.is_terminal(), "{from:?} must be terminal");
            for to in RunStatus::ALL {
                assert!(
                    !from.can_transition_to(&to),
                    "terminal state {from:?} must not transition to {to:?}"
                );
            }
        }
    }

    #[test]
    fn run_status_transition_matrix_has_no_duplicate_edges() {
        for (idx, edge) in RunStatus::TRANSITION_EDGES.iter().enumerate() {
            assert!(
                RunStatus::ALL.contains(&edge.0),
                "edge {idx} has unknown source {:?}",
                edge.0
            );
            assert!(
                RunStatus::ALL.contains(&edge.1),
                "edge {idx} has unknown target {:?}",
                edge.1
            );
            let duplicates = RunStatus::TRANSITION_EDGES
                .iter()
                .filter(|candidate| *candidate == edge)
                .count();
            assert_eq!(duplicates, 1, "duplicate transition edge {edge:?}");
        }
    }

    #[test]
    fn durable_run_event_batch_budget_compacts_old_semantic_rows() {
        let mut events: Vec<Value> = (0..(MAX_DURABLE_RUN_EVENT_BATCH_ROWS + 10))
            .map(|idx| json!({"type": "agent_progress", "seq": idx}))
            .collect();
        events.push(json!({"event_type": "text_done", "data": {"full_text": "answer"}}));
        events.push(json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}));

        let budgeted = enforce_durable_run_event_batch_budget_with_budget(
            events,
            DurableRunEventBatchBudget::default(),
        );

        assert_eq!(budgeted.len(), MAX_DURABLE_RUN_EVENT_BATCH_ROWS);
        assert_eq!(
            durable_event_type(&budgeted[0]),
            Some("durable_events_compacted")
        );
        assert_eq!(budgeted[0]["data"]["dropped_events"], json!(13));
        assert_eq!(
            durable_event_type(&budgeted[budgeted.len() - 2]),
            Some("text_done")
        );
        assert_eq!(
            durable_event_type(&budgeted[budgeted.len() - 1]),
            Some("run_finished")
        );
    }

    #[test]
    fn durable_run_event_batch_budget_prioritizes_replay_boundaries_over_progress() {
        let mut events = vec![
            json!({"type": "tool_call", "tool_call": {"id": "call-1", "name": "bash"}}),
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
        ];
        events.extend(
            (0..(MAX_DURABLE_RUN_EVENT_BATCH_ROWS + 20))
                .map(|idx| json!({"type": "agent_progress", "seq": idx})),
        );
        events.push(json!({"event_type": "text_done", "data": {"full_text": "answer"}}));
        events.push(json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}));

        let budgeted = enforce_durable_run_event_batch_budget_with_budget(
            events,
            DurableRunEventBatchBudget::default(),
        );

        assert_eq!(budgeted.len(), MAX_DURABLE_RUN_EVENT_BATCH_ROWS);
        assert!(
            budgeted
                .iter()
                .any(|event| durable_event_type(event) == Some("durable_events_compacted"))
        );
        assert!(
            budgeted
                .iter()
                .any(|event| durable_event_type(event) == Some("tool_call"))
        );
        assert!(
            budgeted
                .iter()
                .any(|event| durable_event_type(event) == Some("tool_call_end"))
        );
        assert_eq!(
            durable_event_type(&budgeted[budgeted.len() - 2]),
            Some("text_done")
        );
        assert_eq!(
            durable_event_type(&budgeted[budgeted.len() - 1]),
            Some("run_finished")
        );
        let retained_progress = budgeted
            .iter()
            .filter(|event| durable_event_type(event) == Some("agent_progress"))
            .count();
        assert!(retained_progress < MAX_DURABLE_RUN_EVENT_BATCH_ROWS);
    }

    #[test]
    fn durable_run_event_batch_budget_compacts_oversized_noncritical_bytes() {
        let huge_payload = "x".repeat(MAX_DURABLE_RUN_EVENT_BATCH_BYTES);
        let events = vec![
            json!({"type": "agent_progress", "payload": huge_payload}),
            json!({"event_type": "text_done", "data": {"full_text": "answer"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let budgeted = enforce_durable_run_event_batch_budget_with_budget(
            events,
            DurableRunEventBatchBudget::default(),
        );

        assert_eq!(budgeted.len(), 3);
        assert_eq!(
            durable_event_type(&budgeted[0]),
            Some("durable_events_compacted")
        );
        assert_eq!(budgeted[0]["data"]["dropped_events"], json!(1));
        assert_eq!(durable_event_type(&budgeted[1]), Some("text_done"));
        assert_eq!(durable_event_type(&budgeted[2]), Some("run_finished"));
        let total_bytes = budgeted
            .iter()
            .map(durable_run_event_estimated_bytes)
            .sum::<usize>();
        assert!(total_bytes < MAX_DURABLE_RUN_EVENT_BATCH_BYTES);
    }

    #[test]
    fn durable_run_event_batch_budget_accepts_custom_row_budget() {
        let row_budget = 32;
        let mut events: Vec<Value> = (0..(row_budget + 10))
            .map(|idx| json!({"type": "agent_progress", "seq": idx}))
            .collect();
        events.push(json!({"event_type": "text_done", "data": {"full_text": "answer"}}));
        events.push(json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}));

        let budgeted = enforce_durable_run_event_batch_budget_with_budget(
            events,
            DurableRunEventBatchBudget::new(row_budget, MAX_DURABLE_RUN_EVENT_BATCH_BYTES),
        );

        assert_eq!(budgeted.len(), row_budget);
        assert_eq!(
            durable_event_type(&budgeted[0]),
            Some("durable_events_compacted")
        );
        assert_eq!(budgeted[0]["data"]["row_budget"], json!(row_budget));
        assert_eq!(
            durable_event_type(&budgeted[budgeted.len() - 2]),
            Some("text_done")
        );
        assert_eq!(
            durable_event_type(&budgeted[budgeted.len() - 1]),
            Some("run_finished")
        );
    }
}
