use serde::{Deserialize, Serialize};
use std::fmt;

pub const AGENT_COMMUNICATION_SCHEMA_VERSION: &str = "astra.agent_communication.v1";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentCommunicationDirection {
    Sent,
    Received,
}

/// Semantic class of an inter-agent payload.
///
/// This stays typed all the way through persistence policy so retention does
/// not depend on ad-hoc string matching at a storage boundary.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentCommunicationPayloadKind {
    Text,
    Progress,
    Request,
    Response,
    Signal,
    Ack,
    Nack,
}

impl AgentCommunicationPayloadKind {
    /// Progress is an observation of current execution, not recoverable
    /// conversation state. The dedicated live progress projection owns it.
    pub fn is_durable(self) -> bool {
        !matches!(self, Self::Progress)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Progress => "progress",
            Self::Request => "request",
            Self::Response => "response",
            Self::Signal => "signal",
            Self::Ack => "ack",
            Self::Nack => "nack",
        }
    }
}

impl fmt::Display for AgentCommunicationPayloadKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCommunicationParty {
    pub run_id: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentCommunicationTarget {
    Direct { address: AgentCommunicationParty },
    Broadcast { delegation_id: String },
    Parent,
}

/// Bounded, transport-independent evidence for one agent message lifecycle.
///
/// Full message payloads remain in the mailbox transport. Run timelines retain
/// this reviewable evidence so every deployment mode can explain who exchanged
/// what kind of coordination signal without turning arbitrary message bodies
/// into an unbounded durable-event or prompt-facing lane.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCommunicationEvent {
    pub schema_version: String,
    /// The run/agent whose lifecycle emitted this observation. This is not
    /// always the sender: received evidence is emitted by the receiver.
    pub observed_by: AgentCommunicationParty,
    pub direction: AgentCommunicationDirection,
    pub message_id: String,
    pub from: AgentCommunicationParty,
    pub to: AgentCommunicationTarget,
    pub payload_kind: AgentCommunicationPayloadKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_accepted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_message_id: Option<String>,
    pub timestamp_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub requires_ack: bool,
}
