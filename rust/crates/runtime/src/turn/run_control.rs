use async_trait::async_trait;

/// Run status for cross-pod control polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunControlStatus {
    Cancelled,
    Paused,
}

/// This trait lets the agentic loop periodically poll the database for the
/// authoritative run status, enabling cross-pod control without sticky sessions.
#[async_trait]
pub trait RunControlProvider: Send + Sync {
    /// Returns `Some(Cancelled)`, `Some(Paused)`, or `None` if the run is
    /// still active (or doesn't exist).
    async fn control_status(&self, run_id: &str) -> Option<RunControlStatus>;
}
