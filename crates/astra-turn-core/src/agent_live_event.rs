use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLiveEvent {
    /// The exact execution that produced this event. Agent profiles can be
    /// reused concurrently or across retries; only a run id is a safe live
    /// transcript identity.
    pub run_id: String,
    pub agent_id: String,
    pub kind: AgentLiveEventKind,
}

/// A bounded transport lane dropped one or more live events for this durable
/// agent execution. This is not agent output and must never be reconstructed
/// into transcript text; consumers use it to mark their projection incomplete
/// and reconcile from the canonical run snapshot/transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLiveGap {
    pub run_id: String,
    pub agent_id: String,
    pub dropped_event_count: u64,
}

/// Terminal status for a sub-agent run.
///
/// Without `Failed` / `Cancelled` variants the parent's TaskCell row
/// stayed visually `live` forever after a child crash or user
/// interrupt — observed during the reviewer pass. Each terminal
/// state maps to a distinct status icon in the multi_agent strip
/// (✓ / ✗ / ⊘) so the user sees exactly what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLiveTermination {
    Completed,
    Delegated,
    Failed,
    /// Execution stopped with resumable state and may continue later.
    Interrupted,
    Cancelled,
}

#[derive(Debug, Clone)]
pub enum AgentLiveEventKind {
    OutputDelta(String),
    ThinkingDelta(String),
    Status(String),
    /// Structured runtime/coordination evidence. Presentation belongs to the
    /// consuming UI; no control decision may be recovered from status text.
    Signal(AgentLiveSignal),
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

// The public event format is internally tagged so consumers can route every
// live event without inspecting unbounded output. Serde cannot derive that
// representation for bare-string newtype variants, however. Keep the domain
// ergonomics (`ThinkingDelta(String)`) and make the transport shape explicit
// at this boundary instead of letting a valid runtime event panic a writer.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentLiveEventKindRef<'a> {
    OutputDelta {
        text: &'a str,
    },
    ThinkingDelta {
        text: &'a str,
    },
    Status {
        text: &'a str,
    },
    Signal {
        #[serde(flatten)]
        signal: &'a AgentLiveSignal,
    },
    ToolStarted {
        name: &'a str,
        description: &'a str,
        tool_use_id: &'a str,
    },
    ToolCompleted {
        name: &'a str,
        description: &'a str,
        status: &'a str,
        duration_ms: u64,
        output_summary: &'a Option<String>,
        output: &'a Option<String>,
        tool_use_id: &'a str,
    },
    AgentTerminated {
        termination: AgentLiveTermination,
        duration_ms: u64,
        reason: &'a Option<String>,
    },
}

impl<'a> From<&'a AgentLiveEventKind> for AgentLiveEventKindRef<'a> {
    fn from(value: &'a AgentLiveEventKind) -> Self {
        match value {
            AgentLiveEventKind::OutputDelta(text) => Self::OutputDelta { text },
            AgentLiveEventKind::ThinkingDelta(text) => Self::ThinkingDelta { text },
            AgentLiveEventKind::Status(text) => Self::Status { text },
            AgentLiveEventKind::Signal(signal) => Self::Signal { signal },
            AgentLiveEventKind::ToolStarted {
                name,
                description,
                tool_use_id,
            } => Self::ToolStarted {
                name,
                description,
                tool_use_id,
            },
            AgentLiveEventKind::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
            } => Self::ToolCompleted {
                name,
                description,
                status,
                duration_ms: *duration_ms,
                output_summary,
                output,
                tool_use_id,
            },
            AgentLiveEventKind::AgentTerminated {
                termination,
                duration_ms,
                reason,
            } => Self::AgentTerminated {
                termination: *termination,
                duration_ms: *duration_ms,
                reason,
            },
        }
    }
}

impl Serialize for AgentLiveEventKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        AgentLiveEventKindRef::from(self).serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentLiveEventKindWire {
    OutputDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    Status {
        text: String,
    },
    Signal {
        #[serde(flatten)]
        signal: AgentLiveSignal,
    },
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
    AgentTerminated {
        termination: AgentLiveTermination,
        duration_ms: u64,
        reason: Option<String>,
    },
}

impl From<AgentLiveEventKindWire> for AgentLiveEventKind {
    fn from(value: AgentLiveEventKindWire) -> Self {
        match value {
            AgentLiveEventKindWire::OutputDelta { text } => Self::OutputDelta(text),
            AgentLiveEventKindWire::ThinkingDelta { text } => Self::ThinkingDelta(text),
            AgentLiveEventKindWire::Status { text } => Self::Status(text),
            AgentLiveEventKindWire::Signal { signal } => Self::Signal(signal),
            AgentLiveEventKindWire::ToolStarted {
                name,
                description,
                tool_use_id,
            } => Self::ToolStarted {
                name,
                description,
                tool_use_id,
            },
            AgentLiveEventKindWire::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
            } => Self::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
            },
            AgentLiveEventKindWire::AgentTerminated {
                termination,
                duration_ms,
                reason,
            } => Self::AgentTerminated {
                termination,
                duration_ms,
                reason,
            },
        }
    }
}

impl<'de> Deserialize<'de> for AgentLiveEventKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        AgentLiveEventKindWire::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum AgentLiveSignal {
    /// Binds a live child stream to the durable conversation it represents.
    /// The enclosing [`AgentLiveEvent::run_id`] is the durable conversation
    /// identity. Keeping it in the envelope prevents a signal payload from
    /// contradicting the stream it belongs to.
    RunStarted {
        parent_run_id: Option<String>,
        depth: u32,
        /// Correlates the canonical child run with the parent control call
        /// that launched it. This is an execution identity, not display text:
        /// consumers use it to replace a provisional control row before any
        /// child output is rendered or a transcript is opened.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spawn_tool_call_id: Option<String>,
        /// Authoritative store for this run's conversation. The start signal
        /// makes transcript placement available independently of a tool
        /// receipt, so a live workbench can always open the real run.
        transcript_location: astra_turn_types::AgentTranscriptLocation,
    },
    WaitingForModel,
    /// The executor released this run at a recoverable runtime boundary.
    /// Unlike `WaitingForModel`, this is a durable lifecycle state: another
    /// executor can later resume the same canonical run.
    ExecutionWaiting {
        reason: String,
    },
    ModelResponding,
    /// The child has settled all model-visible output. Durable completion can
    /// still follow after its own local turn settlement.
    OutputSettled,
    /// A canonical transcript writer committed a concrete assistant item.
    /// Consumers can use this immutable identity to prove that a canonical
    /// page has caught up. It does not identify individual live deltas, so
    /// equal text or item counts must never be used to delete model output.
    TranscriptCommitted {
        source_event_id: String,
        transcript_location: astra_turn_types::AgentTranscriptLocation,
    },
    AskUserPrompted {
        request_id: String,
        prompt: serde_json::Value,
    },
    AskUserResolved {
        request_id: String,
        resolution: serde_json::Value,
    },
    UserIntentApplied {
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        event_index: usize,
        content: String,
    },
    UserIntentReturned {
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        event_index: usize,
        content: String,
    },
    AgentCommunication(astra_turn_types::AgentCommunicationEvent),
    PermissionAutoApproved {
        tool: String,
        reason: String,
    },
    ApprovalRequired {
        request_id: String,
        tool: String,
        approval_kind: String,
        path: Option<String>,
        detail: Option<String>,
        display_label: Option<String>,
    },
    AgentControlStarted {
        action: String,
        label: String,
        tool_use_id: String,
    },
    AgentControlCompleted {
        action: String,
        label: String,
        status: String,
        duration_ms: u64,
        output: Option<String>,
        tool_use_id: String,
        agent_id: Option<String>,
    },
    ToolProgress {
        name: String,
        lines: u64,
        bytes: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLiveSendError {
    Closed,
    Dropped,
}

pub trait AgentLiveEventSink: Send + Sync + std::fmt::Debug {
    fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError>;

    /// Propagate a transport-integrity fact independently from agent output.
    ///
    /// A live gap means the consumer must reconcile its projection from the
    /// durable run state. It must not be converted into a status string or
    /// synthetic transcript message along nested-agent boundaries.
    fn send_gap(&self, gap: AgentLiveGap) -> Result<(), AgentLiveSendError>;
}

pub type SharedAgentLiveEventSink = Arc<dyn AgentLiveEventSink>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_live_events_use_a_tagged_object_and_round_trip() {
        for (kind, expected_type, expected_text) in [
            (
                AgentLiveEventKind::OutputDelta("child output".into()),
                "output_delta",
                "child output",
            ),
            (
                AgentLiveEventKind::ThinkingDelta("child reasoning".into()),
                "thinking_delta",
                "child reasoning",
            ),
            (
                AgentLiveEventKind::Status("waiting on tool".into()),
                "status",
                "waiting on tool",
            ),
        ] {
            let wire = serde_json::to_value(&kind).expect("serialize text live event");
            assert_eq!(wire["type"], expected_type);
            assert_eq!(wire["text"], expected_text);
            let decoded: AgentLiveEventKind =
                serde_json::from_value(wire).expect("deserialize text live event");
            let text = match decoded {
                AgentLiveEventKind::OutputDelta(text)
                | AgentLiveEventKind::ThinkingDelta(text)
                | AgentLiveEventKind::Status(text) => text,
                unexpected => panic!("unexpected text live event: {unexpected:?}"),
            };
            assert_eq!(text, expected_text);
        }
    }

    #[test]
    fn structured_signal_round_trips_without_status_text() {
        let event = AgentLiveEvent {
            run_id: "run-1".into(),
            agent_id: "reviewer".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::ApprovalRequired {
                request_id: "approval-1".into(),
                tool: "bash".into(),
                approval_kind: "explicit".into(),
                path: None,
                detail: Some("cargo test".into()),
                display_label: Some("$ cargo test".into()),
            }),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["kind"]["type"], "signal");
        assert_eq!(wire["kind"]["signal"], "approval_required");
        assert_eq!(wire["kind"]["tool"], "bash");
        let decoded: AgentLiveEvent = serde_json::from_value(wire).unwrap();
        assert!(matches!(
            decoded.kind,
            AgentLiveEventKind::Signal(AgentLiveSignal::ApprovalRequired {
                request_id,
                ..
            }) if request_id == "approval-1"
        ));
    }

    #[test]
    fn run_start_carries_the_typed_transcript_location() {
        let signal = AgentLiveSignal::RunStarted {
            parent_run_id: Some("run-parent".into()),
            depth: 2,
            spawn_tool_call_id: Some("call-spawn-1".into()),
            transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
        };

        let wire = serde_json::to_value(&signal).unwrap();
        assert_eq!(wire["signal"], "run_started");
        assert_eq!(wire["spawn_tool_call_id"], "call-spawn-1");
        assert_eq!(wire["transcript_location"], "durable_server");
        assert!(matches!(
            serde_json::from_value::<AgentLiveSignal>(wire).unwrap(),
            AgentLiveSignal::RunStarted {
                parent_run_id: Some(parent_run_id),
                depth: 2,
                spawn_tool_call_id: Some(spawn_tool_call_id),
                transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
            } if parent_run_id == "run-parent" && spawn_tool_call_id == "call-spawn-1"
        ));
    }

    #[test]
    fn execution_waiting_round_trips_as_nonterminal_lifecycle_evidence() {
        let event = AgentLiveEvent {
            run_id: "run-waiting".into(),
            agent_id: "reviewer".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::ExecutionWaiting {
                reason: "executor_offline".into(),
            }),
        };

        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["kind"]["type"], "signal");
        assert_eq!(wire["kind"]["signal"], "execution_waiting");
        assert_eq!(wire["kind"]["reason"], "executor_offline");
        assert!(matches!(
            serde_json::from_value::<AgentLiveEvent>(wire).unwrap().kind,
            AgentLiveEventKind::Signal(AgentLiveSignal::ExecutionWaiting { reason })
                if reason == "executor_offline"
        ));
    }

    #[test]
    fn transcript_commit_carries_exact_reconciliation_identity() {
        let signal = AgentLiveSignal::TranscriptCommitted {
            source_event_id: "response:run-1:turn-7".into(),
            transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
        };

        let wire = serde_json::to_value(&signal).unwrap();
        assert_eq!(wire["signal"], "transcript_committed");
        assert_eq!(wire["source_event_id"], "response:run-1:turn-7");
        assert_eq!(wire["transcript_location"], "durable_server");
        assert!(matches!(
            serde_json::from_value::<AgentLiveSignal>(wire).unwrap(),
            AgentLiveSignal::TranscriptCommitted {
                source_event_id,
                transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
            } if source_event_id == "response:run-1:turn-7"
        ));
    }

    #[test]
    fn live_gap_round_trips_as_transport_fact_not_agent_content() {
        let gap = AgentLiveGap {
            run_id: "run-1".into(),
            agent_id: "reviewer".into(),
            dropped_event_count: 3,
        };
        let wire = serde_json::to_value(&gap).unwrap();
        assert_eq!(wire["run_id"], "run-1");
        assert_eq!(wire["agent_id"], "reviewer");
        assert_eq!(wire["dropped_event_count"], 3);
        assert_eq!(serde_json::from_value::<AgentLiveGap>(wire).unwrap(), gap);
    }
}
