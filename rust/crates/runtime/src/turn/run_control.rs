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

/// This trait lets the agentic loop periodically poll the database for the
/// authoritative run status, enabling cross-pod control without sticky sessions.
#[async_trait]
pub trait RunControlProvider: Send + Sync {
    /// Returns `Some(Cancelled)`, `Some(Paused)`, or `None` if the run is
    /// still active (or doesn't exist).
    async fn control_status(&self, run_id: &str) -> Option<RunControlStatus>;

    /// Poll deferred `user_input` events appended to a durable run after the
    /// provided exclusive cursor.
    async fn poll_user_inputs(
        &self,
        _run_id: &str,
        after_event_index: usize,
    ) -> RunQueuedInputPoll {
        RunQueuedInputPoll {
            next_cursor: after_event_index,
            inputs: Vec::new(),
        }
    }
}
