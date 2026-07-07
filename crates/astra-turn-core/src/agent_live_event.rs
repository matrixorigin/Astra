use std::sync::Arc;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AgentLiveEvent {
    pub agent_id: String,
    pub kind: AgentLiveEventKind,
}

/// Terminal status for a sub-agent run.
///
/// Without `Failed` / `Cancelled` variants the parent's TaskCell row
/// stayed visually `live` forever after a child crash or user
/// interrupt — observed during the reviewer pass. Each terminal
/// state maps to a distinct status icon in the multi_agent strip
/// (✓ / ✗ / ⊘) so the user sees exactly what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLiveTermination {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentLiveEventKind {
    OutputDelta(String),
    ThinkingDelta(String),
    Status(String),
    ToolStarted {
        name: String,
        description: String,
        tool_use_id: String,
    },
    ToolCompleted {
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
        tool_use_id: String,
    },
    /// The sub-agent itself reached a terminal state. Reason carries
    /// a short user-facing string (e.g. "agent timed out", "killed by
    /// signal", or the model's own finish_reason).
    AgentTerminated {
        termination: AgentLiveTermination,
        duration_ms: u64,
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLiveSendError {
    Closed,
    Dropped,
}

pub trait AgentLiveEventSink: Send + Sync + std::fmt::Debug {
    fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError>;
}

pub type SharedAgentLiveEventSink = Arc<dyn AgentLiveEventSink>;
