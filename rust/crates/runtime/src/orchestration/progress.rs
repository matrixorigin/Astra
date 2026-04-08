//! Progress events for spawned agents.

use serde::Serialize;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::broadcast;

/// Real-time progress event from a spawned agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentProgressEvent {
    pub agent_id: String,
    pub event_type: ProgressEventType,
    pub timestamp_epoch_ms: u64,
}

/// Types of progress events.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressEventType {
    /// Agent started execution.
    Started { description: String },
    /// Turn completed.
    TurnCompleted {
        turn: u32,
        tool_calls_this_turn: u32,
        activity: String,
    },
    /// Agent is idle (waiting for messages).
    Idle,
    /// Agent is busy with activity.
    Busy { activity: String },
    /// Agent completed successfully.
    Completed {
        result_summary: String,
        total_tool_calls: u32,
        total_tokens: (u64, u64),
        duration_ms: u64,
    },
    /// Agent failed.
    Failed { error: String },
    /// Agent cancelled.
    Cancelled { reason: String },
}

/// Broadcasts progress events to multiple subscribers.
#[derive(Debug)]
pub struct ProgressBroadcaster {
    tx: broadcast::Sender<AgentProgressEvent>,
}

impl Default for ProgressBroadcaster {
    fn default() -> Self {
        Self::new(256)
    }
}

impl ProgressBroadcaster {
    /// Create a new broadcaster with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit a progress event.
    pub fn emit(&self, event: AgentProgressEvent) {
        // Ignore send errors (no subscribers is fine)
        let _ = self.tx.send(event);
    }

    /// Subscribe to progress events.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentProgressEvent> {
        self.tx.subscribe()
    }

    /// Create a scoped emitter for a specific agent.
    pub fn for_agent(self: &Arc<Self>, agent_id: String) -> AgentProgressEmitter {
        AgentProgressEmitter {
            broadcaster: Arc::clone(self),
            agent_id,
        }
    }
}

/// Emitter scoped to a specific agent.
#[derive(Debug, Clone)]
pub struct AgentProgressEmitter {
    broadcaster: Arc<ProgressBroadcaster>,
    agent_id: String,
}

impl AgentProgressEmitter {
    pub fn started(&self, description: impl Into<String>) {
        self.emit(ProgressEventType::Started {
            description: description.into(),
        });
    }

    pub fn turn_completed(&self, turn: u32, tool_calls: u32, activity: impl Into<String>) {
        self.emit(ProgressEventType::TurnCompleted {
            turn,
            tool_calls_this_turn: tool_calls,
            activity: activity.into(),
        });
    }

    pub fn idle(&self) {
        self.emit(ProgressEventType::Idle);
    }

    pub fn busy(&self, activity: impl Into<String>) {
        self.emit(ProgressEventType::Busy {
            activity: activity.into(),
        });
    }

    pub fn completed(&self, summary: impl Into<String>, tool_calls: u32, tokens: (u64, u64), duration_ms: u64) {
        self.emit(ProgressEventType::Completed {
            result_summary: summary.into(),
            total_tool_calls: tool_calls,
            total_tokens: tokens,
            duration_ms,
        });
    }

    pub fn failed(&self, error: impl Into<String>) {
        self.emit(ProgressEventType::Failed {
            error: error.into(),
        });
    }

    pub fn cancelled(&self, reason: impl Into<String>) {
        self.emit(ProgressEventType::Cancelled {
            reason: reason.into(),
        });
    }

    fn emit(&self, event_type: ProgressEventType) {
        let timestamp_epoch_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.broadcaster.emit(AgentProgressEvent {
            agent_id: self.agent_id.clone(),
            event_type,
            timestamp_epoch_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_progress_broadcast() {
        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let mut rx = broadcaster.subscribe();

        let emitter = broadcaster.for_agent("test-agent".to_string());
        emitter.started("Testing");

        let event = rx.recv().await.unwrap();
        assert_eq!(event.agent_id, "test-agent");
        assert!(matches!(event.event_type, ProgressEventType::Started { .. }));
    }
}
