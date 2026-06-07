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
}

/// Polls the database for the authoritative run status, enabling cross-pod
/// cancel/pause control without sticky sessions.
#[async_trait]
pub trait RunStatusProvider: Send + Sync {
    /// Returns `Some(Cancelled)`, `Some(Paused)`, or `None` if the run is
    /// still active (or doesn't exist).
    async fn control_status(&self, run_id: &str) -> Option<RunControlStatus>;
}

/// Polls durable user input appended to a run while the agent is executing.
#[async_trait]
pub trait RunInputProvider: Send + Sync {
    /// Poll deferred `user_input` events appended to a durable run after the
    /// provided exclusive cursor.
    async fn poll_user_inputs(&self, run_id: &str, after_event_index: usize) -> RunQueuedInputPoll;
}

/// Full run-control surface required by the agentic loop.
///
/// This is intentionally a composition of the status and input traits instead
/// of a trait with optional no-op methods. Implementors must explicitly provide
/// both halves, so a missing deferred-input implementation fails at compile time.
pub trait RunControlProvider: RunStatusProvider + RunInputProvider {}

impl<T> RunControlProvider for T where T: RunStatusProvider + RunInputProvider + Send + Sync {}
