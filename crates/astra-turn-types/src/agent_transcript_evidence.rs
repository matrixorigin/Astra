use serde::{Deserialize, Serialize};

use crate::{AgentCommunicationDirection, AgentCommunicationEvent};

/// Durable, non-conversational evidence shown inline with an individual
/// agent's transcript.
///
/// This is deliberately distinct from user/assistant/tool messages. It
/// explains a permission boundary or a coordination event without becoming
/// prompt-facing history or an unstructured status line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentTranscriptEvidence {
    ApprovalRequired {
        request_id: String,
        tool: String,
        approval_kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    AgentCommunication {
        event: AgentCommunicationEvent,
    },
}

impl AgentTranscriptEvidence {
    /// Stable semantic identity for reconciling a live observation with its
    /// later durable transcript projection. This never derives identity from
    /// rendered text.
    pub fn stable_key(&self) -> String {
        match self {
            Self::ApprovalRequired { request_id, .. } => format!("approval:{request_id}"),
            Self::AgentCommunication { event } => format!(
                "agent_communication:{}:{}",
                event.message_id,
                match event.direction {
                    AgentCommunicationDirection::Sent => "sent",
                    AgentCommunicationDirection::Received => "received",
                }
            ),
        }
    }
}
