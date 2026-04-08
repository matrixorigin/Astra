//! Agent messaging types — the common vocabulary for inter-agent communication.
//!
//! All message types are serializable so they can be persisted to event logs
//! or transmitted across process boundaries.

use serde::{Deserialize, Serialize};
use std::time::Duration;

// ─── Agent Address ──────────────────────────────────────────────────────────

/// Uniquely identifies an agent within a delegation hierarchy.
///
/// Combines `run_id` (the specific execution run) with `agent_id` (the role,
/// e.g. "coder", "reviewer"). Two agents in the same delegation share a parent
/// `delegation_id` but have distinct addresses.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentAddress {
    pub run_id: String,
    pub agent_id: String,
}

impl AgentAddress {
    pub fn new(run_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            agent_id: agent_id.into(),
        }
    }
}

impl std::fmt::Display for AgentAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.agent_id, self.run_id)
    }
}

// ─── Message Target ─────────────────────────────────────────────────────────

/// Where to deliver a message.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageTarget {
    /// Send to a specific agent.
    Direct { address: AgentAddress },
    /// Broadcast to all agents in a delegation group.
    Broadcast { delegation_id: String },
    /// Send to the parent agent (resolved via DelegationTracker).
    Parent,
}

// ─── Message Payload ────────────────────────────────────────────────────────

/// The content of an agent message.
///
/// Tagged union — each variant serializes with a `"type"` discriminator
/// so payloads are self-describing in JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePayload {
    /// Free-form text message between agents.
    Text {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },

    /// Progress update from a running sub-agent.
    Progress {
        turn_index: u32,
        tool_calls: u32,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },

    /// Structured request that expects a response.
    Request {
        request_type: RequestType,
        #[serde(default)]
        data: serde_json::Value,
    },

    /// Response to a prior request (correlated via `AgentMessage.correlation_id`).
    Response {
        request_id: String,
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },

    /// Coordination signal (lightweight, no LLM context needed).
    Signal(AgentSignal),

    /// Acknowledgment of a received message.
    Ack {
        /// The ID of the message being acknowledged.
        message_id: String,
    },

    /// Negative acknowledgment — message could not be processed.
    Nack {
        /// The ID of the message being rejected.
        message_id: String,
        /// Reason for rejection.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Request types for structured request–response exchanges.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestType {
    /// Request graceful shutdown.
    Shutdown,
    /// Request permission to use a specific tool.
    ToolPermission,
    /// Request shared context from another agent.
    ContextShare,
    /// Custom/extensible request type.
    Custom(String),
}

/// Lightweight coordination signals.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSignal {
    /// Periodic heartbeat.
    Heartbeat,
    /// Agent is idle / waiting for work.
    Idle,
    /// Agent detected a stall condition.
    Stalled { reason: String },
    /// Agent completed successfully.
    Completed { output: String },
    /// Agent failed.
    Failed { error: String },
}

// ─── Agent Message ──────────────────────────────────────────────────────────

/// A single message between agents.
///
/// Designed to be:
/// - **Serializable**: JSON-round-trippable for persistence / cross-process transport.
/// - **Correlatable**: `correlation_id` links request–response pairs.
/// - **Expirable**: Optional `ttl_ms` for time-sensitive messages.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMessage {
    /// Unique message ID (UUID v4).
    pub id: String,
    /// Sender address.
    pub from: AgentAddress,
    /// Delivery target.
    pub to: MessageTarget,
    /// Message content.
    pub payload: MessagePayload,
    /// When the message was created (milliseconds since Unix epoch).
    pub timestamp_ms: i64,
    /// Optional correlation ID for request–response pairing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Time-to-live in milliseconds. `None` = no expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<i64>,

    /// Whether the sender expects an acknowledgment for this message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_ack: bool,
}

impl AgentMessage {
    /// Create a new message with auto-generated ID and current timestamp.
    pub fn new(from: AgentAddress, to: MessageTarget, payload: MessagePayload) -> Self {
        let timestamp_ms = chrono::Utc::now().timestamp_millis();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            from,
            to,
            payload,
            timestamp_ms,
            correlation_id: None,
            ttl_ms: None,
            requires_ack: false,
        }
    }

    /// Attach a correlation ID (for request–response).
    pub fn with_correlation(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Set a TTL. Saturates at i64::MAX milliseconds (~292 million years).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        let millis = ttl.as_millis();
        self.ttl_ms = Some(if millis > i64::MAX as u128 {
            i64::MAX
        } else {
            millis as i64
        });
        self
    }

    /// Mark this message as requiring acknowledgment from the receiver.
    pub fn with_ack_required(mut self) -> Self {
        self.requires_ack = true;
        self
    }

    /// Create an Ack reply for this message (from receiver back to sender).
    pub fn make_ack(&self, from: AgentAddress) -> Self {
        Self::new(
            from,
            MessageTarget::Direct {
                address: self.from.clone(),
            },
            MessagePayload::Ack {
                message_id: self.id.clone(),
            },
        )
    }

    /// Create a Nack reply for this message.
    pub fn make_nack(&self, from: AgentAddress, reason: Option<String>) -> Self {
        Self::new(
            from,
            MessageTarget::Direct {
                address: self.from.clone(),
            },
            MessagePayload::Nack {
                message_id: self.id.clone(),
                reason,
            },
        )
    }

    /// Whether this message has expired.
    pub fn is_expired(&self) -> bool {
        if let Some(ttl_ms) = self.ttl_ms {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let elapsed = now_ms.saturating_sub(self.timestamp_ms);
            elapsed >= ttl_ms
        } else {
            false
        }
    }
}

// ─── Error Types ────────────────────────────────────────────────────────────

/// Errors that can occur during message delivery.
#[derive(Debug, Clone)]
pub enum MailboxError {
    /// Target agent is not registered.
    AgentNotFound(AgentAddress),
    /// The agent's receive channel has been closed.
    ChannelClosed,
    /// No parent agent found for `MessageTarget::Parent`.
    NoParent,
    /// Transport-layer error.
    Transport(String),
    /// Message delivery was not acknowledged within timeout.
    AckTimeout {
        message_id: String,
        attempts: u32,
    },
    /// Message was explicitly rejected (Nack'd) by the receiver.
    Rejected {
        message_id: String,
        reason: Option<String>,
    },
}

impl std::fmt::Display for MailboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentNotFound(addr) => write!(f, "agent not found: {addr}"),
            Self::ChannelClosed => write!(f, "message channel closed"),
            Self::NoParent => write!(f, "no parent agent in delegation hierarchy"),
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::AckTimeout { message_id, attempts } => {
                write!(f, "ack timeout for message {message_id} after {attempts} attempts")
            }
            Self::Rejected { message_id, reason } => {
                let r = reason.as_deref().unwrap_or("no reason");
                write!(f, "message {message_id} rejected: {r}")
            }
        }
    }
}

impl std::error::Error for MailboxError {}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn message_roundtrip_json() {
        let msg = AgentMessage::new(
            AgentAddress::new("run-1", "coder"),
            MessageTarget::Direct {
                address: AgentAddress::new("run-2", "reviewer"),
            },
            MessagePayload::Text {
                content: "Please review this change.".into(),
                summary: None,
            },
        );

        let json = serde_json::to_string(&msg).unwrap();
        let restored: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, msg.id);
        assert_eq!(restored.from.agent_id, "coder");
    }

    #[test]
    fn message_expiry() {
        let mut msg = AgentMessage::new(
            AgentAddress::new("r", "a"),
            MessageTarget::Parent,
            MessagePayload::Signal(AgentSignal::Heartbeat),
        );
        assert!(!msg.is_expired());

        // Set TTL to 0 → already expired
        msg.ttl_ms = Some(0);
        assert!(msg.is_expired());
    }

    #[test]
    fn progress_payload_serialization() {
        let payload = MessagePayload::Progress {
            turn_index: 3,
            tool_calls: 7,
            status: "running".into(),
            detail: Some("executing bash".into()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "progress");
        assert_eq!(json["turn_index"], 3);
    }

    #[test]
    fn broadcast_target_serialization() {
        let target = MessageTarget::Broadcast {
            delegation_id: "del-123".into(),
        };
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(json["kind"], "broadcast");
        assert_eq!(json["delegation_id"], "del-123");
    }

    #[test]
    fn with_correlation_and_ttl() {
        let msg = AgentMessage::new(
            AgentAddress::new("r1", "a"),
            MessageTarget::Parent,
            MessagePayload::Request {
                request_type: RequestType::Shutdown,
                data: serde_json::Value::Null,
            },
        )
        .with_correlation("req-001")
        .with_ttl(Duration::from_secs(30));

        assert_eq!(msg.correlation_id.as_deref(), Some("req-001"));
        assert_eq!(msg.ttl_ms, Some(30_000));
        assert!(!msg.is_expired());
    }

    #[test]
    fn ack_payload_roundtrip() {
        let ack = MessagePayload::Ack {
            message_id: "msg-123".into(),
        };
        let json = serde_json::to_value(&ack).unwrap();
        assert_eq!(json["type"], "ack");
        assert_eq!(json["message_id"], "msg-123");

        let restored: MessagePayload = serde_json::from_value(json).unwrap();
        match restored {
            MessagePayload::Ack { message_id } => assert_eq!(message_id, "msg-123"),
            _ => panic!("expected Ack"),
        }
    }

    #[test]
    fn nack_payload_roundtrip() {
        let nack = MessagePayload::Nack {
            message_id: "msg-456".into(),
            reason: Some("invalid format".into()),
        };
        let json = serde_json::to_value(&nack).unwrap();
        assert_eq!(json["type"], "nack");
        assert_eq!(json["reason"], "invalid format");

        let restored: MessagePayload = serde_json::from_value(json).unwrap();
        match restored {
            MessagePayload::Nack { message_id, reason } => {
                assert_eq!(message_id, "msg-456");
                assert_eq!(reason.as_deref(), Some("invalid format"));
            }
            _ => panic!("expected Nack"),
        }
    }

    #[test]
    fn make_ack_creates_reply() {
        let original = AgentMessage::new(
            AgentAddress::new("r1", "sender"),
            MessageTarget::Direct {
                address: AgentAddress::new("r2", "receiver"),
            },
            MessagePayload::Text {
                content: "hello".into(),
                summary: None,
            },
        )
        .with_ack_required();

        assert!(original.requires_ack);

        let ack = original.make_ack(AgentAddress::new("r2", "receiver"));
        assert_eq!(ack.from.agent_id, "receiver");
        match &ack.to {
            MessageTarget::Direct { address } => {
                assert_eq!(address.agent_id, "sender");
            }
            _ => panic!("expected Direct target"),
        }
        match &ack.payload {
            MessagePayload::Ack { message_id } => assert_eq!(message_id, &original.id),
            _ => panic!("expected Ack payload"),
        }
    }

    #[test]
    fn requires_ack_not_serialized_when_false() {
        let msg = AgentMessage::new(
            AgentAddress::new("r", "a"),
            MessageTarget::Parent,
            MessagePayload::Signal(AgentSignal::Heartbeat),
        );
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("requires_ack"));
    }
}
