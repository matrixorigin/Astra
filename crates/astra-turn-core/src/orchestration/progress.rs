//! Progress events for spawned agents.

use super::fanout_group::AgentFanoutSlotIdentity;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::broadcast;

/// Real-time progress event from a spawned agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentProgressEvent {
    pub agent_id: String,
    /// Immutable execution identity for every agent event. The event kind may
    /// add descriptive hierarchy data, but consumers must not have to wait
    /// for a separate `AgentSpawned` event before routing progress, controls,
    /// or transcript updates to the correct run.
    pub run_id: String,
    /// Parent execution identity paired with `run_id` when known.
    pub parent_run_id: String,
    pub event_type: ProgressEventType,
    pub timestamp_epoch_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
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
    /// Agent produced partial output but stopped before normal completion.
    Interrupted {
        reason: String,
        partial_summary: String,
        total_tool_calls: u32,
        total_tokens: (u64, u64),
        duration_ms: u64,
    },
    /// Agent failed.
    Failed { error: String },
    /// Agent is waiting for external input or an execution boundary to recover.
    Waiting { reason: String },
    /// Agent cancelled.
    Cancelled { reason: String },
    /// Tool blocked by permission policy (emitted so parent/UI can surface warnings).
    PermissionDenied {
        tool_name: String,
        reason: String,
        turn: u32,
    },
    /// A tool is about to execute (emitted before invocation so UI can show activity).
    ToolExecuting { tool_name: String, turn: u32 },
    /// LLM call started (waiting for model response).
    LlmCallStarted { turn: u32 },
    /// LLM call completed.
    LlmCallCompleted {
        turn: u32,
        ttft_ms: Option<u64>,
        duration_ms: u64,
    },
    /// Intermediate metrics snapshot after each turn (allows UI to show progress).
    MetricsUpdate {
        turn: u32,
        max_turns: u32,
        total_prompt_tokens: u64,
        total_completion_tokens: u64,
        total_tool_calls: u32,
    },
    /// Agent spawned with hierarchy information (enables real-time tree updates).
    AgentSpawned {
        agent_type: String,
        description: String,
        fanout_slot: Option<AgentFanoutSlotIdentity>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentProgressTerminalKind {
    Completed,
    Interrupted,
    Failed,
    Cancelled,
}

impl ProgressEventType {
    pub fn terminal_kind(&self) -> Option<AgentProgressTerminalKind> {
        match self {
            Self::Completed { .. } => Some(AgentProgressTerminalKind::Completed),
            Self::Interrupted { .. } => Some(AgentProgressTerminalKind::Interrupted),
            Self::Failed { .. } => Some(AgentProgressTerminalKind::Failed),
            Self::Cancelled { .. } => Some(AgentProgressTerminalKind::Cancelled),
            Self::Waiting { .. } => None, // Waiting is recoverable, not terminal
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal_kind().is_some()
    }
}

/// Broadcasts progress events to multiple subscribers.
#[derive(Debug)]
pub struct ProgressBroadcaster {
    tx: broadcast::Sender<AgentProgressEvent>,
}

impl Default for ProgressBroadcaster {
    fn default() -> Self {
        // Bumped from 256 → 1024 so high-fanout multi-agent runs do not silently
        // overflow the broadcast buffer between drains in `take_emitted_events`.
        Self::new(1024)
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

    /// Create a scoped emitter for a specific agent execution.
    pub fn for_agent_with_run_context(
        self: &Arc<Self>,
        agent_id: String,
        run_id: String,
        parent_run_id: String,
        metadata: Option<Value>,
    ) -> AgentProgressEmitter {
        AgentProgressEmitter {
            broadcaster: Arc::clone(self),
            agent_id,
            run_id,
            parent_run_id,
            metadata,
        }
    }
}

/// Emitter scoped to a specific agent.
#[derive(Debug, Clone)]
pub struct AgentProgressEmitter {
    broadcaster: Arc<ProgressBroadcaster>,
    agent_id: String,
    run_id: String,
    parent_run_id: String,
    metadata: Option<Value>,
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

    pub fn waiting(&self, reason: impl Into<String>) {
        self.emit(ProgressEventType::Waiting {
            reason: reason.into(),
        });
    }

    pub fn busy(&self, activity: impl Into<String>) {
        self.emit(ProgressEventType::Busy {
            activity: activity.into(),
        });
    }

    pub fn completed(
        &self,
        summary: impl Into<String>,
        tool_calls: u32,
        tokens: (u64, u64),
        duration_ms: u64,
    ) {
        self.emit(ProgressEventType::Completed {
            result_summary: summary.into(),
            total_tool_calls: tool_calls,
            total_tokens: tokens,
            duration_ms,
        });
    }

    pub fn interrupted(
        &self,
        reason: impl Into<String>,
        partial_summary: impl Into<String>,
        tool_calls: u32,
        tokens: (u64, u64),
        duration_ms: u64,
    ) {
        self.emit(ProgressEventType::Interrupted {
            reason: reason.into(),
            partial_summary: partial_summary.into(),
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

    pub fn permission_denied(
        &self,
        tool_name: impl Into<String>,
        reason: impl Into<String>,
        turn: u32,
    ) {
        self.emit(ProgressEventType::PermissionDenied {
            tool_name: tool_name.into(),
            reason: reason.into(),
            turn,
        });
    }

    pub fn tool_executing(&self, tool_name: impl Into<String>, turn: u32) {
        self.emit(ProgressEventType::ToolExecuting {
            tool_name: tool_name.into(),
            turn,
        });
    }

    pub fn llm_call_started(&self, turn: u32) {
        self.emit(ProgressEventType::LlmCallStarted { turn });
    }

    pub fn llm_call_completed(&self, turn: u32, ttft_ms: Option<u64>, duration_ms: u64) {
        self.emit(ProgressEventType::LlmCallCompleted {
            turn,
            ttft_ms,
            duration_ms,
        });
    }

    pub fn metrics_update(
        &self,
        turn: u32,
        max_turns: u32,
        total_prompt_tokens: u64,
        total_completion_tokens: u64,
        total_tool_calls: u32,
    ) {
        self.emit(ProgressEventType::MetricsUpdate {
            turn,
            max_turns,
            total_prompt_tokens,
            total_completion_tokens,
            total_tool_calls,
        });
    }

    /// Emit agent spawned event with hierarchy info for tree visualization.
    pub fn agent_spawned(&self, agent_type: impl Into<String>, description: impl Into<String>) {
        self.agent_spawned_with_fanout(agent_type, description, None);
    }

    /// Emit agent spawned event with fanout slot identity.
    pub fn agent_spawned_with_fanout(
        &self,
        agent_type: impl Into<String>,
        description: impl Into<String>,
        fanout_slot: Option<AgentFanoutSlotIdentity>,
    ) {
        self.emit(ProgressEventType::AgentSpawned {
            agent_type: agent_type.into(),
            description: description.into(),
            fanout_slot,
        });
    }

    fn emit(&self, event_type: ProgressEventType) {
        let timestamp_epoch_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.broadcaster.emit(AgentProgressEvent {
            agent_id: self.agent_id.clone(),
            run_id: self.run_id.clone(),
            parent_run_id: self.parent_run_id.clone(),
            event_type,
            timestamp_epoch_ms,
            metadata: self.metadata.clone(),
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

        let emitter = broadcaster.for_agent_with_run_context(
            "test-agent".to_string(),
            "run-test".to_string(),
            "run-root".to_string(),
            None,
        );
        emitter.started("Testing");

        let event = rx.recv().await.unwrap();
        assert_eq!(event.agent_id, "test-agent");
        assert!(matches!(
            event.event_type,
            ProgressEventType::Started { .. }
        ));
    }

    #[tokio::test]
    async fn scoped_emitter_attaches_run_identity_before_the_spawn_event() {
        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let mut rx = broadcaster.subscribe();
        let emitter = broadcaster.for_agent_with_run_context(
            "reviewer@child".to_string(),
            "run-child".to_string(),
            "run-root".to_string(),
            None,
        );

        emitter.started("review the storage layer");
        let event = rx.recv().await.expect("started event");

        assert_eq!(event.run_id, "run-child");
        assert_eq!(event.parent_run_id, "run-root");
        assert!(matches!(
            event.event_type,
            ProgressEventType::Started { description } if description == "review the storage layer"
        ));
    }

    #[tokio::test]
    async fn test_all_event_types() {
        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let mut rx = broadcaster.subscribe();

        let emitter = broadcaster.for_agent_with_run_context(
            "agent-1".to_string(),
            "run-1".to_string(),
            "run-root".to_string(),
            None,
        );

        // Emit all event types
        emitter.started("task description");
        emitter.turn_completed(1, 3, "reading files");
        emitter.busy("processing data");
        emitter.idle();
        emitter.waiting("executor_offline");
        emitter.completed("done", 10, (1000, 500), 5000);

        // Verify all events received
        let e1 = rx.recv().await.unwrap();
        assert!(
            matches!(e1.event_type, ProgressEventType::Started { description } if description == "task description")
        );

        let e2 = rx.recv().await.unwrap();
        assert!(matches!(
            e2.event_type,
            ProgressEventType::TurnCompleted {
                turn: 1,
                tool_calls_this_turn: 3,
                ..
            }
        ));

        let e3 = rx.recv().await.unwrap();
        assert!(matches!(e3.event_type, ProgressEventType::Busy { .. }));

        let e4 = rx.recv().await.unwrap();
        assert!(matches!(e4.event_type, ProgressEventType::Idle));

        let e5 = rx.recv().await.unwrap();
        assert!(matches!(
            e5.event_type,
            ProgressEventType::Waiting { reason } if reason == "executor_offline"
        ));

        let e6 = rx.recv().await.unwrap();
        assert!(matches!(
            e6.event_type,
            ProgressEventType::Completed {
                total_tool_calls: 10,
                duration_ms: 5000,
                ..
            }
        ));
    }

    #[test]
    fn terminal_kind_distinguishes_interrupted_from_completed() {
        let completed = ProgressEventType::Completed {
            result_summary: "done".into(),
            total_tool_calls: 1,
            total_tokens: (1, 2),
            duration_ms: 3,
        };
        let interrupted = ProgressEventType::Interrupted {
            reason: "budget_exhausted".into(),
            partial_summary: "partial".into(),
            total_tool_calls: 1,
            total_tokens: (1, 2),
            duration_ms: 3,
        };
        assert_eq!(
            completed.terminal_kind(),
            Some(AgentProgressTerminalKind::Completed)
        );
        assert_eq!(
            interrupted.terminal_kind(),
            Some(AgentProgressTerminalKind::Interrupted)
        );
        assert!(interrupted.is_terminal());
    }

    #[tokio::test]
    async fn test_failed_and_cancelled_events() {
        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let mut rx = broadcaster.subscribe();

        let emitter = broadcaster.for_agent_with_run_context(
            "agent-2".to_string(),
            "run-2".to_string(),
            "run-root".to_string(),
            None,
        );

        emitter.failed("connection timeout");
        let e1 = rx.recv().await.unwrap();
        assert!(
            matches!(e1.event_type, ProgressEventType::Failed { error } if error == "connection timeout")
        );

        let emitter2 = broadcaster.for_agent_with_run_context(
            "agent-3".to_string(),
            "run-3".to_string(),
            "run-root".to_string(),
            None,
        );
        emitter2.cancelled("user request");
        let e2 = rx.recv().await.unwrap();
        assert_eq!(e2.agent_id, "agent-3");
        assert!(
            matches!(e2.event_type, ProgressEventType::Cancelled { reason } if reason == "user request")
        );

        let emitter3 = broadcaster.for_agent_with_run_context(
            "agent-4".to_string(),
            "run-4".to_string(),
            "run-root".to_string(),
            None,
        );
        emitter3.interrupted("budget_exhausted", "partial output", 4, (10, 11), 1200);
        let e3 = rx.recv().await.unwrap();
        assert_eq!(e3.agent_id, "agent-4");
        assert!(matches!(
            e3.event_type,
            ProgressEventType::Interrupted {
                reason,
                partial_summary,
                total_tool_calls: 4,
                total_tokens: (10, 11),
                duration_ms: 1200,
            } if reason == "budget_exhausted" && partial_summary == "partial output"
        ));
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let mut rx1 = broadcaster.subscribe();
        let mut rx2 = broadcaster.subscribe();

        let emitter = broadcaster.for_agent_with_run_context(
            "shared-agent".to_string(),
            "run-shared".to_string(),
            "run-root".to_string(),
            None,
        );
        emitter.started("shared task");

        // Both subscribers receive the event
        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();

        assert_eq!(e1.agent_id, e2.agent_id);
        assert_eq!(e1.timestamp_epoch_ms, e2.timestamp_epoch_ms);
    }

    #[test]
    fn test_no_panic_when_no_subscribers() {
        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let emitter = broadcaster.for_agent_with_run_context(
            "orphan-agent".to_string(),
            "run-orphan".to_string(),
            "run-root".to_string(),
            None,
        );

        // Should not panic even without subscribers
        emitter.started("no one listening");
        emitter.turn_completed(1, 5, "working");
        emitter.completed("finished", 5, (100, 50), 1000);
    }
}
