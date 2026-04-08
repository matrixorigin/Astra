//! `send_message` tool — lets an agent send messages to peers during conversation.
//!
//! Integrates with the agentic loop via tool-call interception (like `delegate`
//! and `skill` tools).  The LLM emits a `send_message` function call, the loop
//! intercepts it, and this module routes the message through the agent's mailbox.

use std::sync::Arc;
use serde_json::Value;

use super::router::AgentMailbox;
use super::types::{AgentMessage, AgentSignal, MessagePayload, MessageTarget};

/// Tool name used by the LLM to invoke agent messaging.
pub const SEND_MESSAGE_TOOL_NAME: &str = "send_message";

/// Generate the OpenAI-compatible tool schema for the `send_message` tool.
pub fn send_message_tool_schema() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": SEND_MESSAGE_TOOL_NAME,
            "description": "Send a message to another agent in the current delegation group. Use this to coordinate, share findings, ask questions, or report progress to peers or the parent orchestrator.",
            "parameters": {
                "type": "object",
                "required": ["target", "content"],
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Who to send to: 'parent' (orchestrator), 'broadcast' (all peers), or an agent_id like 'coder', 'reviewer'."
                    },
                    "content": {
                        "type": "string",
                        "description": "The message content to send."
                    },
                    "message_type": {
                        "type": "string",
                        "enum": ["text", "progress", "question", "result"],
                        "description": "Message type. 'text' (default): general message. 'progress': status update. 'question': ask for info. 'result': share a work product."
                    },
                    "requires_ack": {
                        "type": "boolean",
                        "description": "If true, expect the receiver to acknowledge this message. Defaults to false."
                    }
                }
            }
        }
    })
}

/// Parse LLM tool-call arguments and send via the agent's mailbox.
///
/// Returns a human-readable result string (shown to the LLM as tool output).
/// Result of executing send_message — contains the display string and optional message_id for ack tracking.
pub struct SendResult {
    /// Human-readable result text to return to the LLM.
    pub display: String,
    /// If the message was sent successfully with requires_ack, the message_id.
    pub tracked_message: Option<Arc<AgentMessage>>,
}

pub async fn execute_send_message(
    mailbox: &AgentMailbox,
    args: &Value,
) -> SendResult {
    let target_str = match args.get("target").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return SendResult { display: "Error: 'target' parameter is required.".to_string(), tracked_message: None },
    };

    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return SendResult { display: "Error: 'content' parameter is required.".to_string(), tracked_message: None },
    };

    let message_type = args
        .get("message_type")
        .and_then(|v| v.as_str())
        .unwrap_or("text");

    let requires_ack = args
        .get("requires_ack")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Resolve target
    let target = match target_str.to_lowercase().as_str() {
        "parent" | "orchestrator" => MessageTarget::Parent,
        "broadcast" | "all" | "peers" => {
            match mailbox.delegation_id.clone().filter(|s| !s.is_empty()) {
                Some(did) => MessageTarget::Broadcast { delegation_id: did },
                None => return SendResult {
                    display: "Error: cannot broadcast — agent is not part of a delegation group.".to_string(),
                    tracked_message: None,
                },
            }
        }
        agent_id => MessageTarget::Direct {
            address: super::types::AgentAddress {
                run_id: String::new(), // Router resolves by agent_id within delegation group
                agent_id: agent_id.to_string(),
            },
        },
    };

    // Build payload based on message_type
    let payload = match message_type {
        "progress" => MessagePayload::Progress {
            turn_index: 0,
            tool_calls: 0,
            status: "in_progress".to_string(),
            detail: Some(content.to_string()),
        },
        "question" => MessagePayload::Request {
            request_type: super::types::RequestType::Custom(content.to_string()),
            data: serde_json::Value::Null,
        },
        "result" => MessagePayload::Signal(AgentSignal::Completed {
            output: content.to_string(),
        }),
        _ => MessagePayload::Text {
            content: content.to_string(),
            summary: None,
        },
    };

    let target_display = match &target {
        MessageTarget::Parent => "parent".to_string(),
        MessageTarget::Broadcast { .. } => "all peers".to_string(),
        MessageTarget::Direct { address } => format!("agent '{}'", address.agent_id),
    };

    let mut msg = AgentMessage::new(mailbox.address.clone(), target, payload);
    if requires_ack {
        msg = msg.with_ack_required();
    }
    let msg = Arc::new(msg);

    match mailbox.send((*msg).clone()).await {
        Ok(()) => {
            let tracked = if requires_ack { Some(msg) } else { None };
            SendResult {
                display: format!("✓ Message sent to {target_display}."),
                tracked_message: tracked,
            }
        }
        Err(e) => SendResult {
            display: format!("Failed to send message to {target_display}: {e}"),
            tracked_message: None,
        },
    }
}

/// Check whether a tool call is a `send_message` invocation.
pub fn is_send_message_call(tool_call: &Value) -> bool {
    tool_call
        .pointer("/function/name")
        .and_then(|v| v.as_str())
        == Some(SEND_MESSAGE_TOOL_NAME)
}

/// Extract the call ID and arguments from a send_message tool call.
pub fn parse_send_message_call(tool_call: &Value) -> Option<(String, Value)> {
    let id = tool_call.get("id")?.as_str()?.to_string();
    let args_str = tool_call
        .pointer("/function/arguments")
        .and_then(|v| v.as_str())?;
    let args: Value = serde_json::from_str(args_str).ok()?;
    Some((id, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_fields() {
        let schema = send_message_tool_schema();
        let func = &schema["function"];
        assert_eq!(func["name"], "send_message");
        let required = func["parameters"]["required"].as_array().unwrap();
        assert!(required.contains(&Value::String("target".into())));
        assert!(required.contains(&Value::String("content".into())));
    }

    #[test]
    fn is_send_message_matches() {
        let call = serde_json::json!({
            "id": "call_123",
            "type": "function",
            "function": {
                "name": "send_message",
                "arguments": "{\"target\": \"parent\", \"content\": \"done\"}"
            }
        });
        assert!(is_send_message_call(&call));
    }

    #[test]
    fn is_send_message_rejects_other() {
        let call = serde_json::json!({
            "id": "call_456",
            "type": "function",
            "function": { "name": "delegate", "arguments": "{}" }
        });
        assert!(!is_send_message_call(&call));
    }

    #[test]
    fn parse_extracts_args() {
        let call = serde_json::json!({
            "id": "call_789",
            "type": "function",
            "function": {
                "name": "send_message",
                "arguments": "{\"target\": \"broadcast\", \"content\": \"hello peers\"}"
            }
        });
        let (id, args) = parse_send_message_call(&call).unwrap();
        assert_eq!(id, "call_789");
        assert_eq!(args["target"], "broadcast");
        assert_eq!(args["content"], "hello peers");
    }

    #[test]
    fn parse_fails_on_bad_json() {
        let call = serde_json::json!({
            "id": "call_bad",
            "type": "function",
            "function": {
                "name": "send_message",
                "arguments": "not json"
            }
        });
        assert!(parse_send_message_call(&call).is_none());
    }
}
