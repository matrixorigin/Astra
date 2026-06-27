//! Run status, state, and event-persistence helpers extracted from lifecycle.
//!
//! This module defines [`RunStatus`] (the state machine for an individual agentic
//! run) and [`RunState`] (per-run in-memory bookkeeping). The helper functions
//! classify persisted and streaming events, merge terminal events, and enforce
//! the active-run live-event budget.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_INPUT_QUEUED, STATUS_PAUSED,
    STATUS_RUNNING, STATUS_WAITING,
};
use astra_runtime_env::CleanupReason as RuntimeCleanupReason;
use astra_services::runs::{durable_run_status_kind, DurableRunStatusKind};

use super::MAX_ACTIVE_RUN_LIVE_EVENTS;
use crate::server::run::engine::RunEngine;

/// Status of a single agentic run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    InputQueued,
    Paused,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => STATUS_RUNNING,
            Self::InputQueued => STATUS_INPUT_QUEUED,
            Self::Paused => STATUS_PAUSED,
            Self::Waiting => STATUS_WAITING,
            Self::Completed => STATUS_COMPLETED,
            Self::Failed => STATUS_FAILED,
            Self::Cancelled => STATUS_CANCELLED,
        }
    }

    pub fn from_durable_status(status: &str) -> Option<Self> {
        match durable_run_status_kind(status) {
            DurableRunStatusKind::Running => Some(Self::Running),
            DurableRunStatusKind::InputQueued => Some(Self::InputQueued),
            DurableRunStatusKind::Paused => Some(Self::Paused),
            DurableRunStatusKind::Waiting => Some(Self::Waiting),
            DurableRunStatusKind::Completed => Some(Self::Completed),
            DurableRunStatusKind::Failed => Some(Self::Failed),
            DurableRunStatusKind::Cancelled => Some(Self::Cancelled),
            DurableRunStatusKind::Other => None,
        }
    }

    pub fn is_resumable(&self) -> bool {
        matches!(self, Self::Paused | Self::Waiting)
    }

    pub fn blocks_session(&self, waiting_for: Option<&str>) -> bool {
        match self {
            Self::Running | Self::InputQueued | Self::Waiting => true,
            Self::Paused => waiting_for.is_some(),
            Self::Completed | Self::Failed | Self::Cancelled => false,
        }
    }

    /// Validate a status transition. Returns `Err` if the transition is illegal.
    ///
    /// Rules:
    /// - Terminal states (Completed, Failed, Cancelled) cannot transition to anything.
    /// - Running → InputQueued, Paused, Waiting, Completed, Failed, Cancelled
    /// - InputQueued → Running, Paused, Waiting, Completed, Failed, Cancelled
    /// - Paused → Running, Waiting, Cancelled, Failed
    /// - Waiting → InputQueued, Running, Cancelled, Failed (external input resumes to Running)
    pub fn try_transition(&self, next: &RunStatus) -> Result<(), String> {
        let allowed = match self {
            Self::Running => matches!(
                next,
                Self::InputQueued
                    | Self::Paused
                    | Self::Waiting
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::InputQueued => matches!(
                next,
                Self::InputQueued
                    | Self::Running
                    | Self::Paused
                    | Self::Waiting
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
            ),
            Self::Paused => matches!(
                next,
                Self::Running | Self::Waiting | Self::Cancelled | Self::Failed
            ),
            Self::Waiting => matches!(
                next,
                Self::InputQueued | Self::Running | Self::Cancelled | Self::Failed
            ),
            Self::Completed | Self::Failed | Self::Cancelled => false,
        };
        if allowed {
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
        RunStatus::Failed => Some(RuntimeCleanupReason::Failed),
        RunStatus::Cancelled => Some(RuntimeCleanupReason::Cancelled),
        RunStatus::Running | RunStatus::InputQueued | RunStatus::Paused | RunStatus::Waiting => {
            None
        }
    }
}

pub fn is_run_finished_event(event: &Value) -> bool {
    event.get("event_type").and_then(Value::as_str) == Some("run_finished")
}

pub fn deferred_input_text_len(input: &Value) -> usize {
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

pub fn should_preserve_manual_pause_on_completion(
    current_status: &RunStatus,
    final_status: &RunStatus,
) -> bool {
    *current_status == RunStatus::Paused && *final_status == RunStatus::Completed
}

pub async fn should_preserve_manual_pause_from_durable(
    run_engine: &RunEngine,
    run_id: &str,
    final_status: &RunStatus,
) -> bool {
    if *final_status != RunStatus::Completed {
        return false;
    }
    match run_engine.load_run(run_id).await {
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
                        | "run_error"
                        | "run_interrupted"
                        | "run_waiting"
                        | "run_finished"
                        | "reasoning_delta"
                        | "reasoning_message_content"
                        | "reasoning_done"
                        | "thinking_delta"
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

pub fn live_delta_event_for_persistence(event: &Value) -> bool {
    if durable_event_type(event).is_some_and(|event_type| event_type == "run_blocked") {
        return true;
    }
    matches!(
        durable_event_type(event),
        Some(
            "text_delta"
                | "reasoning_delta"
                | "reasoning_message_content"
                | "reasoning_done"
                | "thinking_delta"
                | "thinking_done"
                | "workspace_bound"
                | "executor_bound"
                | "executor_status_changed"
                | "agent_delegated"
                | "agent_spawned"
                | "agent_live_event"
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
}
