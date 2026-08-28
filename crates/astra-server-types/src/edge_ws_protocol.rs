//! Edge WebSocket protocol types for remote tool execution.
//!
//! Defines the bidirectional message protocol between the Astra server and
//! remote edge agents. Edge agents connect via `GET /edge/ws`, authenticate,
//! and then receive tool execution requests and return results.
//!
//! ## Protocol
//!
//! **Edge → Server** (JSON text frames):
//! ```text
//! {"type": "edge_auth", "edge_agent_id": "...", "hostname": "...", "workspace_dir": "..."}
//! {"type": "edge_tool_result", "request_id": "...", "output": "...", "is_error": false, "tool_result_fields": {"exit_code": 0}}
//! {"type": "edge_ping"}
//! ```
//!
//! **Server → Edge** (JSON text frames):
//! ```text
//! {"type": "edge_auth_ok", "user_id": "..."}
//! {"type": "edge_auth_error", "message": "..."}
//! {"type": "edge_tool_request", "request_id": "...", "tool": "...", "args": {...}}
//! {"type": "edge_pong"}
//! {"type": "edge_closing", "reason": "..."}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use astra_turn_types::ToolInvocationIdentity;

/// Edge can inject request-scoped provider authorization into one bash
/// subprocess without receiving file-transfer metadata or bytes.
pub const RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY: &str = "runtime_process_authorization_v1";

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProcessAuthorizationContext {
    pub authorization: String,
}

impl std::fmt::Debug for RuntimeProcessAuthorizationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeProcessAuthorizationContext")
            .field("authorization_present", &!self.authorization.is_empty())
            .finish()
    }
}

/// Timeout for tool execution on the edge agent.
pub const EDGE_TOOL_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Timeout for the initial auth message after WebSocket upgrade.
pub const EDGE_AUTH_TIMEOUT_SECS: u64 = 30;

/// Heartbeat interval for edge keep-alive.
pub const EDGE_HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Messages sent from edge agent to server.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum EdgeClientMessage {
    /// Declare the authenticated edge agent's identity and capabilities.
    ///
    /// HTTP authentication is completed before the WebSocket upgrade; secrets
    /// must never be repeated in an application frame.
    #[serde(rename = "edge_auth")]
    Auth {
        edge_agent_id: String,
        #[serde(default)]
        hostname: Option<String>,
        #[serde(default)]
        workspace_dir: Option<String>,
        #[serde(default)]
        capabilities: Option<Value>,
    },

    /// Tool execution result from the edge.
    #[serde(rename = "edge_tool_result")]
    ToolResult {
        request_id: String,
        identity: ToolInvocationIdentity,
        delivery_generation: u64,
        output: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_result_fields: Option<Map<String, Value>>,
    },

    /// Edge heartbeat.
    #[serde(rename = "edge_ping")]
    Ping,
}

/// Messages sent from server to edge agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum EdgeServerMessage {
    /// Authentication succeeded.
    #[serde(rename = "edge_auth_ok")]
    AuthOk { user_id: String },

    /// Authentication failed.
    #[serde(rename = "edge_auth_error")]
    AuthError { message: String },

    /// Request the edge to execute a tool.
    #[serde(rename = "edge_tool_request")]
    ToolRequest {
        request_id: String,
        /// Kept indirect so the WebSocket envelope remains compact while the
        /// serialized wire field stays the exact established identity shape.
        identity: Box<ToolInvocationIdentity>,
        delivery_generation: u64,
        tool: String,
        args: Value,
        /// Opaque provider authorization injected only for this bash call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_process_authorization: Option<Box<RuntimeProcessAuthorizationContext>>,
        /// Non-secret replay fence. A request that requires process
        /// authorization must never execute without its live credential.
        #[serde(default, skip_serializing_if = "is_false")]
        runtime_process_authorization_required: bool,
        /// Maximum execution time in seconds.
        #[serde(default = "default_tool_timeout_secs")]
        timeout_secs: u64,
    },

    /// Server heartbeat response.
    #[serde(rename = "edge_pong")]
    Pong,

    /// Server is closing the connection.
    #[serde(rename = "edge_closing")]
    Closing { reason: String },

    /// Cancel an in-flight tool execution request.
    ///
    /// Sent when the caller times out or cancels via `CancellationToken`. The
    /// generation prevents a delayed cancel from targeting a newer delivery.
    #[serde(rename = "edge_tool_cancel")]
    ToolCancel {
        request_id: String,
        delivery_generation: u64,
    },

    /// The server durably accepted this exact result delivery. The edge must
    /// retain and replay a completed result until this acknowledgement arrives.
    #[serde(rename = "edge_tool_result_ack")]
    ToolResultAck {
        request_id: String,
        delivery_generation: u64,
    },
}

impl EdgeServerMessage {
    /// Short stable label for diagnostic logging (no payload).
    pub fn diagnostic_kind(&self) -> &'static str {
        match self {
            EdgeServerMessage::AuthOk { .. } => "auth_ok",
            EdgeServerMessage::AuthError { .. } => "auth_error",
            EdgeServerMessage::ToolRequest { .. } => "tool_request",
            EdgeServerMessage::Pong => "pong",
            EdgeServerMessage::Closing { .. } => "closing",
            EdgeServerMessage::ToolCancel { .. } => "tool_cancel",
            EdgeServerMessage::ToolResultAck { .. } => "tool_result_ack",
        }
    }
}

fn default_tool_timeout_secs() -> u64 {
    EDGE_TOOL_TIMEOUT_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", "call").unwrap()
    }

    #[test]
    fn edge_auth_deserializes() {
        let msg: EdgeClientMessage = serde_json::from_value(json!({
            "type": "edge_auth",
            "edge_agent_id": "my-edge",
            "hostname": "laptop",
            "workspace_dir": "/home/user/project"
        }))
        .unwrap();
        match msg {
            EdgeClientMessage::Auth {
                edge_agent_id,
                hostname,
                ..
            } => {
                assert_eq!(edge_agent_id, "my-edge");
                assert_eq!(hostname.as_deref(), Some("laptop"));
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn edge_tool_result_deserializes() {
        let msg: EdgeClientMessage = serde_json::from_value(json!({
            "type": "edge_tool_result",
            "request_id": "req-123",
            "identity": identity(),
            "delivery_generation": 3,
            "output": "file contents here",
            "is_error": false,
            "duration_ms": 42
        }))
        .unwrap();
        match msg {
            EdgeClientMessage::ToolResult {
                request_id,
                output,
                is_error,
                duration_ms,
                tool_result_fields,
                ..
            } => {
                assert_eq!(request_id, "req-123");
                assert_eq!(output, "file contents here");
                assert!(!is_error);
                assert_eq!(duration_ms, Some(42));
                assert!(tool_result_fields.is_none());
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn edge_tool_request_serializes() {
        let msg = EdgeServerMessage::ToolRequest {
            request_id: "req-456".into(),
            identity: Box::new(identity()),
            delivery_generation: 1,
            tool: "bash".into(),
            args: json!({"command": "echo hello"}),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: 120,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_tool_request");
        assert_eq!(v["tool"], "bash");
        assert_eq!(v["timeout_secs"], 120);
    }

    #[test]
    fn edge_tool_request_round_trips_hidden_process_authorization() {
        let msg = EdgeServerMessage::ToolRequest {
            request_id: "req-process-auth".into(),
            identity: Box::new(identity()),
            delivery_generation: 1,
            tool: "bash".into(),
            args: json!({"command": "moi-cli file list"}),
            runtime_process_authorization: Some(Box::new(RuntimeProcessAuthorizationContext {
                authorization: "Bearer runtime-grant".into(),
            })),
            runtime_process_authorization_required: true,
            timeout_secs: 120,
        };

        assert!(!format!("{msg:?}").contains("runtime-grant"));
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: EdgeServerMessage = serde_json::from_str(&encoded).unwrap();
        match decoded {
            EdgeServerMessage::ToolRequest {
                runtime_process_authorization: Some(context),
                runtime_process_authorization_required: true,
                ..
            } => assert_eq!(context.authorization, "Bearer runtime-grant"),
            other => panic!("expected tool request with process authorization, got {other:?}"),
        }
    }

    #[test]
    fn edge_ping_deserializes() {
        let msg: EdgeClientMessage = serde_json::from_value(json!({"type": "edge_ping"})).unwrap();
        assert!(matches!(msg, EdgeClientMessage::Ping));
    }

    #[test]
    fn edge_pong_serializes() {
        let msg = EdgeServerMessage::Pong;
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_pong");
    }

    #[test]
    fn edge_auth_ok_serializes() {
        let msg = EdgeServerMessage::AuthOk {
            user_id: "u-123".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_auth_ok");
        assert_eq!(v["user_id"], "u-123");
    }

    #[test]
    fn edge_client_auth_serializes() {
        let msg = EdgeClientMessage::Auth {
            edge_agent_id: "e1".into(),
            hostname: Some("h".into()),
            workspace_dir: None,
            capabilities: Some(json!({
                "schema_version": 1,
                "binding": {
                    "executor": {"kind": "edge_agent"}
                }
            })),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_auth");
        assert!(v.get("token").is_none());
        assert_eq!(v["edge_agent_id"], "e1");
        assert_eq!(v["capabilities"]["schema_version"], 1);
    }

    #[test]
    fn edge_client_tool_result_serializes() {
        let msg = EdgeClientMessage::ToolResult {
            request_id: "r1".into(),
            identity: identity(),
            delivery_generation: 1,
            output: "ok".into(),
            is_error: false,
            duration_ms: Some(42),
            tool_result_fields: Some(Map::from_iter([("exit_code".into(), json!(0))])),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_tool_result");
        assert_eq!(v["duration_ms"], 42);
        assert_eq!(v["tool_result_fields"]["exit_code"], 0);
    }

    #[test]
    fn edge_client_tool_result_none_duration_serializes_as_null() {
        let msg = EdgeClientMessage::ToolResult {
            request_id: "r1".into(),
            identity: identity(),
            delivery_generation: 1,
            output: "ok".into(),
            is_error: false,
            duration_ms: None,
            tool_result_fields: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert!(v["duration_ms"].is_null());
        let rt: EdgeClientMessage = serde_json::from_value(v).unwrap();
        match rt {
            EdgeClientMessage::ToolResult { duration_ms, .. } => assert_eq!(duration_ms, None),
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn edge_client_ping_serializes() {
        let v = serde_json::to_value(&EdgeClientMessage::Ping).unwrap();
        assert_eq!(v["type"], "edge_ping");
    }

    #[test]
    fn edge_server_tool_request_deserializes() {
        let msg: EdgeServerMessage = serde_json::from_value(json!({
            "type": "edge_tool_request",
            "request_id": "r1",
            "identity": identity(),
            "delivery_generation": 1,
            "tool": "bash",
            "args": {"command": "ls"},
            "timeout_secs": 120
        }))
        .unwrap();
        match msg {
            EdgeServerMessage::ToolRequest {
                tool, timeout_secs, ..
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(timeout_secs, 120);
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn edge_server_tool_request_requires_durable_identity() {
        let result = serde_json::from_value::<EdgeServerMessage>(json!({
            "type": "edge_tool_request",
            "request_id": "r1",
            "tool": "bash",
            "args": {}
        }));
        assert!(result.is_err());
    }

    #[test]
    fn edge_server_auth_ok_deserializes() {
        let msg: EdgeServerMessage =
            serde_json::from_value(json!({"type": "edge_auth_ok", "user_id": "u1"})).unwrap();
        assert!(matches!(msg, EdgeServerMessage::AuthOk { .. }));
    }

    #[test]
    fn edge_server_pong_deserializes() {
        let msg: EdgeServerMessage = serde_json::from_value(json!({"type": "edge_pong"})).unwrap();
        assert!(matches!(msg, EdgeServerMessage::Pong));
    }

    #[test]
    fn edge_server_closing_deserializes() {
        let msg: EdgeServerMessage =
            serde_json::from_value(json!({"type": "edge_closing", "reason": "shutdown"})).unwrap();
        assert!(matches!(msg, EdgeServerMessage::Closing { .. }));
    }

    const _: () = {
        assert!(EDGE_TOOL_TIMEOUT_SECS >= 60);
        assert!(EDGE_AUTH_TIMEOUT_SECS >= 10);
        assert!(EDGE_HEARTBEAT_INTERVAL_SECS >= 10);
    };
}
