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
        if self
            .agent_id
            .strip_suffix(&self.run_id)
            .is_some_and(|prefix| prefix.ends_with('@'))
        {
            f.write_str(&self.agent_id)
        } else {
            write!(f, "{}@{}", self.agent_id, self.run_id)
        }
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
    /// Agent detected a stall condition (likely permanent without intervention).
    Stalled { reason: String },
    /// Agent is waiting on an external dependency (may resolve without intervention).
    Waiting { reason: String },
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
    /// Unique message envelope ID (UUID).
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
}

const AGENT_COMMUNICATION_SUMMARY_CHARS: usize = 1_000;

pub fn agent_communication_event(
    observed_by: &AgentAddress,
    direction: astra_turn_types::AgentCommunicationDirection,
    message: &AgentMessage,
) -> astra_turn_types::AgentCommunicationEvent {
    let (payload_kind, summary, response_accepted, related_message_id) =
        communication_payload_evidence(&message.payload);
    astra_turn_types::AgentCommunicationEvent {
        schema_version: astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION.to_string(),
        observed_by: communication_party(observed_by),
        direction,
        message_id: message.id.clone(),
        from: communication_party(&message.from),
        to: communication_target(&message.to),
        payload_kind,
        summary,
        response_accepted,
        related_message_id,
        timestamp_ms: message.timestamp_ms,
        correlation_id: message.correlation_id.clone(),
    }
}

fn communication_party(address: &AgentAddress) -> astra_turn_types::AgentCommunicationParty {
    astra_turn_types::AgentCommunicationParty {
        run_id: address.run_id.clone(),
        agent_id: address.agent_id.clone(),
    }
}

fn communication_target(target: &MessageTarget) -> astra_turn_types::AgentCommunicationTarget {
    match target {
        MessageTarget::Direct { address } => astra_turn_types::AgentCommunicationTarget::Direct {
            address: communication_party(address),
        },
        MessageTarget::Broadcast { delegation_id } => {
            astra_turn_types::AgentCommunicationTarget::Broadcast {
                delegation_id: delegation_id.clone(),
            }
        }
        MessageTarget::Parent => astra_turn_types::AgentCommunicationTarget::Parent,
    }
}

fn communication_payload_evidence(
    payload: &MessagePayload,
) -> (
    astra_turn_types::AgentCommunicationPayloadKind,
    Option<String>,
    Option<bool>,
    Option<String>,
) {
    use astra_turn_types::AgentCommunicationPayloadKind as Kind;
    match payload {
        MessagePayload::Text { content, .. } => (
            Kind::Text,
            Some(bounded_communication_summary(content)),
            None,
            None,
        ),
        MessagePayload::Progress { status, detail, .. } => (
            Kind::Progress,
            Some(bounded_communication_summary(
                &detail
                    .as_deref()
                    .map_or_else(|| status.clone(), |detail| format!("{status} · {detail}")),
            )),
            None,
            None,
        ),
        MessagePayload::Request { request_type, .. } => (
            Kind::Request,
            Some(bounded_communication_summary(&format!("{request_type:?}"))),
            None,
            None,
        ),
        MessagePayload::Response {
            request_id,
            accepted,
            ..
        } => (
            Kind::Response,
            None,
            Some(*accepted),
            Some(request_id.clone()),
        ),
        MessagePayload::Signal(signal) => (
            Kind::Signal,
            Some(bounded_communication_summary(&format!("{signal:?}"))),
            None,
            None,
        ),
    }
}

fn bounded_communication_summary(value: &str) -> String {
    let mut summary: String = value
        .chars()
        .take(AGENT_COMMUNICATION_SUMMARY_CHARS)
        .collect();
    if value.chars().count() > AGENT_COMMUNICATION_SUMMARY_CHARS {
        summary.push('…');
    }
    summary
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
    /// A direct address is incomplete and cannot be routed canonically.
    InvalidAddress(AgentAddress),
    /// Another live mailbox owns the same agent identity in this delegation.
    AgentIdentityConflict {
        delegation_id: String,
        agent_id: String,
    },
    /// The agent's receive channel has been closed.
    ChannelClosed,
    /// No parent agent found for `MessageTarget::Parent`.
    NoParent,
    /// This send was rejected before the envelope entered transport custody.
    DeliveryRejected(String),
    /// Transport-layer error.
    Transport(String),
    /// Request/response timeout (e.g., permission request).
    Timeout(String),
    /// The mailbox was disconnected while waiting.
    Disconnected,
    /// Received a message that violated mailbox request/response protocol.
    Protocol(String),
}

impl std::fmt::Display for MailboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentNotFound(addr) => write!(f, "agent not found: {addr}"),
            Self::InvalidAddress(addr) => write!(f, "invalid agent address: {addr}"),
            Self::AgentIdentityConflict {
                delegation_id,
                agent_id,
            } => write!(
                f,
                "agent '{agent_id}' already has a live mailbox in delegation '{delegation_id}'"
            ),
            Self::ChannelClosed => write!(f, "message channel closed"),
            Self::NoParent => write!(f, "no parent agent in delegation hierarchy"),
            Self::DeliveryRejected(msg) => write!(f, "delivery rejected: {msg}"),
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::Timeout(msg) => write!(f, "request timeout: {msg}"),
            Self::Disconnected => write!(f, "mailbox disconnected"),
            Self::Protocol(msg) => write!(f, "mailbox protocol error: {msg}"),
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
    fn address_display_does_not_duplicate_embedded_run_identity() {
        assert_eq!(
            AgentAddress::new("run-1", "reviewer@run-1").to_string(),
            "reviewer@run-1"
        );
        assert_eq!(
            AgentAddress::new("run-1", "reviewer").to_string(),
            "reviewer@run-1"
        );
    }

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
        let wire: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(wire.get("requires_ack").is_none());
        assert!(wire.get("ack_message_id").is_none());
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
    fn waiting_signal_roundtrip_preserves_recoverable_state() {
        let payload = MessagePayload::Signal(AgentSignal::Waiting {
            reason: "executor_offline".into(),
        });
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "signal");
        assert_eq!(json["waiting"]["reason"], "executor_offline");

        let restored: MessagePayload = serde_json::from_value(json).unwrap();
        assert!(matches!(
            restored,
            MessagePayload::Signal(AgentSignal::Waiting { reason })
                if reason == "executor_offline"
        ));
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
    fn application_receipt_payloads_are_not_supported() {
        for kind in ["ack", "nack"] {
            let payload = serde_json::json!({"type": kind, "message_id": "msg-1"});
            assert!(serde_json::from_value::<MessagePayload>(payload).is_err());
            assert!(
                serde_json::from_value::<astra_turn_types::AgentCommunicationPayloadKind>(
                    serde_json::json!(kind)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn response_roundtrip_preserves_semantic_decision_and_correlation() {
        for accepted in [false, true] {
            let receiver = AgentAddress::new("run-child", "worker");
            let message = AgentMessage::new(
                AgentAddress::new("run-parent", "parent"),
                MessageTarget::Direct {
                    address: receiver.clone(),
                },
                MessagePayload::Response {
                    request_id: "request-1".into(),
                    accepted,
                    data: Some(serde_json::json!({"reason": "permission decision"})),
                },
            )
            .with_correlation("request-1");
            let restored: AgentMessage =
                serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
            let evidence = agent_communication_event(
                &receiver,
                astra_turn_types::AgentCommunicationDirection::Received,
                &restored,
            );
            assert_eq!(
                evidence.payload_kind,
                astra_turn_types::AgentCommunicationPayloadKind::Response
            );
            assert_eq!(evidence.response_accepted, Some(accepted));
            assert_eq!(evidence.related_message_id.as_deref(), Some("request-1"));
            assert_eq!(evidence.correlation_id.as_deref(), Some("request-1"));
            let wire = serde_json::to_value(&evidence).unwrap();
            assert!(wire.get("requires_ack").is_none());
            assert_eq!(
                serde_json::from_value::<astra_turn_types::AgentCommunicationEvent>(wire).unwrap(),
                evidence
            );
        }
    }

    #[test]
    fn communication_evidence_is_bounded_and_names_the_observer() {
        let sender = AgentAddress::new("run-coder", "coder");
        let receiver = AgentAddress::new("run-reviewer", "reviewer");
        let message = AgentMessage::new(
            sender.clone(),
            MessageTarget::Direct {
                address: receiver.clone(),
            },
            MessagePayload::Text {
                content: "界".repeat(1_500),
                summary: None,
            },
        );

        let evidence = agent_communication_event(
            &receiver,
            astra_turn_types::AgentCommunicationDirection::Received,
            &message,
        );

        assert_eq!(evidence.observed_by.run_id, "run-reviewer");
        assert_eq!(evidence.observed_by.agent_id, "reviewer");
        assert_eq!(evidence.from.run_id, "run-coder");
        assert_eq!(
            evidence.payload_kind,
            astra_turn_types::AgentCommunicationPayloadKind::Text
        );
        let summary = evidence.summary.expect("text evidence summary");
        assert_eq!(summary.chars().count(), 1_001);
        assert!(summary.ends_with('…'));
    }
}
