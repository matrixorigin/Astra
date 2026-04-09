//! Inter-agent messaging tools for multi-agent coordination.
//!
//! Provides `send_message` tool for point-to-point and broadcast communication
//! between agents during delegation or team collaboration.
//!
//! Features (vs Claude Code's file-based mailbox):
//! - Database-backed transport (optional) with ack/nack
//! - Dead letter queue for failed deliveries
//! - Structured message types with priorities
//! - Real-time metrics via MessagingMetrics
//!
//! # Message Types
//!
//! - `text`: Plain text message
//! - `question`: Asking for input/decision
//! - `answer`: Response to a question
//! - `instruction`: Task assignment or directive
//! - `progress`: Progress update
//! - `result`: Task completion result
//! - `shutdown_request`: Request to terminate
//! - `shutdown_response`: Approval/rejection of shutdown
//!
//! # Example
//!
//! ```json
//! {
//!   "to": "reviewer-agent",
//!   "message": "Please review the code changes",
//!   "message_type": "instruction",
//!   "priority": "high"
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use astra_runtime::messaging::{
    AgentMessage, MessagingMetrics,
    router::AgentMailboxRouter,
    types::{AgentAddress, MessagePayload, MessageTarget},
};

// ─── Message Types ─────────────────────────────────────────────────────────

/// Structured message types for inter-agent communication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    /// Plain text message
    Text,
    /// Asking for input or decision
    Question,
    /// Response to a question
    Answer,
    /// Task assignment or directive
    Instruction,
    /// Progress update
    Progress,
    /// Task completion result
    Result,
    /// Request to terminate gracefully
    ShutdownRequest,
    /// Approval/rejection of shutdown
    ShutdownResponse,
}

impl Default for MessageType {
    fn default() -> Self {
        Self::Text
    }
}

impl std::fmt::Display for MessageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text => write!(f, "text"),
            Self::Question => write!(f, "question"),
            Self::Answer => write!(f, "answer"),
            Self::Instruction => write!(f, "instruction"),
            Self::Progress => write!(f, "progress"),
            Self::Result => write!(f, "result"),
            Self::ShutdownRequest => write!(f, "shutdown_request"),
            Self::ShutdownResponse => write!(f, "shutdown_response"),
        }
    }
}

// ─── Tool Input/Output ─────────────────────────────────────────────────────

/// Input for the send_message tool.
#[derive(Debug, Deserialize)]
pub struct SendMessageInput {
    /// Recipient: agent_id, or "*" for broadcast to all peers
    pub to: String,
    /// Message content (string or JSON object)
    pub message: Value,
    /// Optional 5-10 word summary shown in UI preview
    #[serde(default)]
    pub summary: Option<String>,
    /// Message type for structured handling
    #[serde(default)]
    pub message_type: MessageType,
    /// Message priority (default: normal)
    #[serde(default)]
    pub priority: Option<String>,
    /// Optional request_id for request/response correlation
    #[serde(default)]
    pub request_id: Option<String>,
}

/// Output from the send_message tool.
#[derive(Debug, Serialize)]
pub struct SendMessageOutput {
    pub success: bool,
    pub message: String,
    /// Message ID for tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Recipients for broadcast
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipients: Option<Vec<String>>,
    /// Routing metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing: Option<MessageRouting>,
}

/// Routing metadata for message display.
#[derive(Debug, Serialize)]
pub struct MessageRouting {
    pub sender: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_type: Option<String>,
}

// ─── Tool Execution Context ────────────────────────────────────────────────

/// Runtime context for send_message tool execution.
#[derive(Clone)]
pub struct SendMessageRuntimeContext {
    /// Current agent's ID
    pub agent_id: String,
    /// Mailbox router for message delivery
    pub router: Arc<AgentMailboxRouter>,
    /// Metrics for observability
    pub metrics: Option<Arc<MessagingMetrics>>,
    /// Current delegation ID (if in delegation context)
    pub delegation_id: Option<String>,
}

// ─── Tool Implementation ───────────────────────────────────────────────────

/// Execute the send_message tool.
pub async fn execute_send_message(
    input: SendMessageInput,
    ctx: &SendMessageRuntimeContext,
) -> SendMessageOutput {
    let is_broadcast = input.to == "*";

    // Build the content string
    let content = match &input.message {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    };

    if is_broadcast {
        // Broadcast to all peers in delegation
        match broadcast_message(ctx, &content, &input).await {
            Ok(recipients) => SendMessageOutput {
                success: true,
                message: format!("Message broadcast to {} recipients", recipients.len()),
                message_id: None,
                recipients: Some(recipients.clone()),
                routing: Some(MessageRouting {
                    sender: ctx.agent_id.clone(),
                    target: "*".to_string(),
                    summary: input.summary,
                    message_type: Some(input.message_type.to_string()),
                }),
            },
            Err(e) => SendMessageOutput {
                success: false,
                message: format!("Broadcast failed: {}", e),
                message_id: None,
                recipients: None,
                routing: None,
            },
        }
    } else {
        // Direct message to specific agent
        match send_direct_message(ctx, &input.to, &content, &input).await {
            Ok(msg_id) => SendMessageOutput {
                success: true,
                message: format!("Message sent to {}", input.to),
                message_id: Some(msg_id),
                recipients: None,
                routing: Some(MessageRouting {
                    sender: ctx.agent_id.clone(),
                    target: format!("@{}", input.to),
                    summary: input.summary,
                    message_type: Some(input.message_type.to_string()),
                }),
            },
            Err(e) => SendMessageOutput {
                success: false,
                message: format!("Send failed: {}", e),
                message_id: None,
                recipients: None,
                routing: None,
            },
        }
    }
}

async fn send_direct_message(
    ctx: &SendMessageRuntimeContext,
    recipient: &str,
    content: &str,
    input: &SendMessageInput,
) -> Result<String, String> {
    // Create AgentAddress for sender (use run_id if available, else agent_id as pseudo-run)
    let run_id = ctx.delegation_id.as_deref().unwrap_or(&ctx.agent_id);
    let from_addr = AgentAddress::new(run_id, &ctx.agent_id);
    let to_addr = AgentAddress::new("", recipient);

    // Build payload based on message_type
    let payload = MessagePayload::Text {
        content: content.to_string(),
        summary: input.summary.clone(),
    };

    // Create the agent message
    let msg = AgentMessage::new(
        from_addr,
        MessageTarget::Direct { address: to_addr },
        payload,
    );

    // Add correlation ID if request_id provided
    let msg = if let Some(ref request_id) = input.request_id {
        msg.with_correlation(request_id)
    } else {
        msg
    };

    let msg_id = msg.id.clone();

    // Send via router
    ctx.router
        .send(msg)
        .await
        .map_err(|e| format!("Router error: {:?}", e))?;

    // Update metrics
    if let Some(metrics) = &ctx.metrics {
        metrics.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    Ok(msg_id)
}

async fn broadcast_message(
    ctx: &SendMessageRuntimeContext,
    content: &str,
    input: &SendMessageInput,
) -> Result<Vec<String>, String> {
    // Get delegation_id for broadcast
    let delegation_id = ctx
        .delegation_id
        .as_ref()
        .ok_or("Broadcast requires active delegation context")?;

    // Create sender address
    let from_addr = AgentAddress::new(delegation_id, &ctx.agent_id);

    // Build payload
    let payload = MessagePayload::Text {
        content: content.to_string(),
        summary: input.summary.clone(),
    };

    // Create broadcast message
    let msg = AgentMessage::new(
        from_addr,
        MessageTarget::Broadcast {
            delegation_id: delegation_id.clone(),
        },
        payload,
    );

    // Add correlation ID if provided
    let msg = if let Some(ref request_id) = input.request_id {
        msg.with_correlation(request_id)
    } else {
        msg
    };

    // Send via router (router handles broadcast distribution)
    ctx.router
        .send(msg)
        .await
        .map_err(|e| format!("Broadcast error: {:?}", e))?;

    // Get list of recipients from router (excluding self)
    let recipients = ctx
        .router
        .list_registered_agents()
        .await
        .into_iter()
        .filter(|a| a != &ctx.agent_id)
        .collect();

    // Update metrics
    if let Some(metrics) = &ctx.metrics {
        metrics.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    Ok(recipients)
}

// ─── Tool Schema ───────────────────────────────────────────────────────────

/// Generate the JSON schema for send_message tool.
pub fn send_message_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "send_message",
            "description": "Send a message to another agent in the current delegation or team. Use for coordination, asking questions, reporting progress, or requesting approvals.",
            "parameters": {
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient: agent_id of the target agent, or \"*\" for broadcast to all peers in the current delegation"
                    },
                    "message": {
                        "oneOf": [
                            { "type": "string", "description": "Plain text message" },
                            { "type": "object", "description": "Structured JSON message" }
                        ],
                        "description": "Message content"
                    },
                    "summary": {
                        "type": "string",
                        "description": "A 5-10 word summary shown as preview (recommended for long messages)"
                    },
                    "message_type": {
                        "type": "string",
                        "enum": ["text", "question", "answer", "instruction", "progress", "result", "shutdown_request", "shutdown_response"],
                        "default": "text",
                        "description": "Message type for structured handling"
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "normal", "high"],
                        "default": "normal",
                        "description": "Message priority"
                    },
                    "request_id": {
                        "type": "string",
                        "description": "Optional ID for request/response correlation"
                    }
                },
                "required": ["to", "message"]
            }
        }
    })
}

/// Handle send_message tool call from agentic loop.
///
/// This is called by the tool executor when the LLM invokes send_message.
pub async fn handle_send_message_tool(
    args: &serde_json::Value,
    ctx: Option<&SendMessageRuntimeContext>,
) -> String {
    // Parse input
    let input: SendMessageInput = match serde_json::from_value(args.clone()) {
        Ok(i) => i,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": format!("Invalid input: {}", e)
            })
            .to_string();
        }
    };

    let Some(ctx) = ctx else {
        return serde_json::json!({
            "success": false,
            "message": "Messaging not available in this context. send_message requires an active agent mailbox."
        }).to_string();
    };

    // Execute
    let output = execute_send_message(input, ctx).await;

    serde_json::to_string(&output).unwrap_or_else(|_| {
        serde_json::json!({
            "success": false,
            "message": "Failed to serialize output"
        })
        .to_string()
    })
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::messaging::{AgentMailboxRouter, InProcessTransport, types::AgentAddress};
    use astra_runtime::server::delegation_engine::DelegationTracker;
    use std::sync::Arc;

    #[test]
    fn message_type_display() {
        assert_eq!(MessageType::Text.to_string(), "text");
        assert_eq!(MessageType::Question.to_string(), "question");
        assert_eq!(MessageType::ShutdownRequest.to_string(), "shutdown_request");
    }

    #[test]
    fn schema_has_required_fields() {
        let schema = send_message_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "send_message");
        assert!(schema["function"]["parameters"]["properties"]["to"].is_object());
        assert!(schema["function"]["parameters"]["properties"]["message"].is_object());

        let required = schema["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert!(required.contains(&serde_json::json!("to")));
        assert!(required.contains(&serde_json::json!("message")));
    }

    #[test]
    fn parse_send_message_input() {
        let json = serde_json::json!({
            "to": "reviewer",
            "message": "Please review",
            "message_type": "instruction",
            "priority": "high"
        });

        let input: SendMessageInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.to, "reviewer");
        assert_eq!(input.message_type, MessageType::Instruction);
        assert_eq!(input.priority, Some("high".to_string()));
    }

    #[test]
    fn parse_broadcast_input() {
        let json = serde_json::json!({
            "to": "*",
            "message": {"status": "in_progress", "percent": 50},
            "message_type": "progress"
        });

        let input: SendMessageInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.to, "*");
        assert_eq!(input.message_type, MessageType::Progress);
        assert!(input.message.is_object());
    }

    #[tokio::test]
    async fn direct_message_resolves_recipient_by_agent_id() {
        let transport = Arc::new(InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));
        let mut recipient = router
            .register(AgentAddress::new("run-123", "worker"), None)
            .await
            .expect("recipient mailbox");

        let ctx = SendMessageRuntimeContext {
            agent_id: "main".to_string(),
            router: router.clone(),
            metrics: None,
            delegation_id: None,
        };
        let output = execute_send_message(
            SendMessageInput {
                to: "worker".to_string(),
                message: Value::String("hello".to_string()),
                summary: None,
                message_type: MessageType::Text,
                priority: None,
                request_id: None,
            },
            &ctx,
        )
        .await;

        assert!(output.success, "{output:?}");
        let msg = recipient
            .try_recv()
            .expect("recipient should receive message");
        assert_eq!(msg.from.agent_id, "main");
        match &msg.to {
            MessageTarget::Direct { address } => {
                assert_eq!(address.agent_id, "worker");
                assert_eq!(address.run_id, "run-123");
            }
            other => panic!("unexpected target: {other:?}"),
        }
    }
}
