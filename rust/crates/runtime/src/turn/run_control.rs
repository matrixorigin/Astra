use async_trait::async_trait;
use serde_json::Value;

/// Run status for cross-pod control polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunControlStatus {
    Cancelled,
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueuedRunInputEvent {
    pub event_index: usize,
    pub input: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunQueuedInputPoll {
    pub next_cursor: usize,
    pub inputs: Vec<QueuedRunInputEvent>,
    pub error: Option<String>,
}

/// Polls the database for the authoritative run status, enabling cross-pod
/// cancel/pause control without sticky sessions.
#[async_trait]
pub trait RunStatusProvider: Send + Sync {
    /// Returns `Some(Cancelled)`, `Some(Paused)`, or `None` if the run is
    /// still active (or doesn't exist). Transient lookup failures must be
    /// surfaced so callers do not confuse control-plane unavailability with a
    /// durable cancel/pause signal.
    async fn control_status(&self, run_id: &str) -> Result<Option<RunControlStatus>, String>;
}

/// Polls durable user input appended to a run while the agent is executing.
#[async_trait]
pub trait RunInputProvider: Send + Sync {
    /// Poll deferred `user_input` events appended to a durable run after the
    /// provided exclusive cursor.
    async fn poll_user_inputs(&self, run_id: &str, after_event_index: usize) -> RunQueuedInputPoll;

    /// Mark deferred inputs as released to the model. Durable providers use this
    /// to clear an `input-queued` run status once the queued input is no longer
    /// just pending at a future tool boundary.
    async fn mark_user_inputs_released(&self, run_id: &str, event_indices: &[usize]);
}

/// Full run-control surface required by the agentic loop.
///
/// This is intentionally a composition of the status and input traits instead
/// of a trait with optional no-op methods. Implementors must explicitly provide
/// both halves, so a missing deferred-input implementation fails at compile time.
pub trait RunControlProvider: RunStatusProvider + RunInputProvider {}

impl<T> RunControlProvider for T where T: RunStatusProvider + RunInputProvider + Send + Sync {}
