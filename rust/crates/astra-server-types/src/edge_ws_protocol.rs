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
//! {"type": "edge_auth", "token": "Bearer ...", "edge_agent_id": "...", "hostname": "...", "workspace_dir": "..."}
//! {"type": "edge_tool_result", "request_id": "...", "output": "...", "is_error": false}
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
use serde_json::Value;

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
    /// Authenticate the edge agent (must be first message).
    #[serde(rename = "edge_auth")]
    Auth {
        token: String,
        edge_agent_id: String,
        #[serde(default)]
        hostname: Option<String>,
        #[serde(default)]
        workspace_dir: Option<String>,
        #[serde(default)]
        capabilities: Option<Vec<String>>,
    },

    /// Tool execution result from the edge.
    #[serde(rename = "edge_tool_result")]
    ToolResult {
        request_id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
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
        tool: String,
        args: Value,
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
}

fn default_tool_timeout_secs() -> u64 {
    EDGE_TOOL_TIMEOUT_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edge_auth_deserializes() {
        let msg: EdgeClientMessage = serde_json::from_value(json!({
            "type": "edge_auth",
            "token": "Bearer abc",
            "edge_agent_id": "my-edge",
            "hostname": "laptop",
            "workspace_dir": "/home/user/project"
        }))
        .unwrap();
        match msg {
            EdgeClientMessage::Auth {
                token,
                edge_agent_id,
                hostname,
                ..
            } => {
                assert_eq!(token, "Bearer abc");
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
            } => {
                assert_eq!(request_id, "req-123");
                assert_eq!(output, "file contents here");
                assert!(!is_error);
                assert_eq!(duration_ms, Some(42));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn edge_tool_request_serializes() {
        let msg = EdgeServerMessage::ToolRequest {
            request_id: "req-456".into(),
            tool: "bash".into(),
            args: json!({"command": "echo hello"}),
            timeout_secs: 120,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_tool_request");
        assert_eq!(v["tool"], "bash");
        assert_eq!(v["timeout_secs"], 120);
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
            token: "Bearer tok".into(),
            edge_agent_id: "e1".into(),
            hostname: Some("h".into()),
            workspace_dir: None,
            capabilities: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_auth");
        assert_eq!(v["token"], "Bearer tok");
        assert_eq!(v["edge_agent_id"], "e1");
    }

    #[test]
    fn edge_client_tool_result_serializes() {
        let msg = EdgeClientMessage::ToolResult {
            request_id: "r1".into(),
            output: "ok".into(),
            is_error: false,
            duration_ms: Some(42),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_tool_result");
        assert_eq!(v["duration_ms"], 42);
    }

    #[test]
    fn edge_client_tool_result_none_duration_serializes_as_null() {
        let msg = EdgeClientMessage::ToolResult {
            request_id: "r1".into(),
            output: "ok".into(),
            is_error: false,
            duration_ms: None,
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
            "tool": "bash",
            "args": {"command": "ls"},
            "timeout_secs": 120
        }))
        .unwrap();
        match msg {
            EdgeServerMessage::ToolRequest { tool, timeout_secs, .. } => {
                assert_eq!(tool, "bash");
                assert_eq!(timeout_secs, 120);
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn edge_server_tool_request_default_timeout() {
        let msg: EdgeServerMessage = serde_json::from_value(json!({
            "type": "edge_tool_request",
            "request_id": "r1",
            "tool": "bash",
            "args": {}
        }))
        .unwrap();
        match msg {
            EdgeServerMessage::ToolRequest { timeout_secs, .. } => {
                assert_eq!(timeout_secs, EDGE_TOOL_TIMEOUT_SECS);
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn edge_server_auth_ok_deserializes() {
        let msg: EdgeServerMessage =
            serde_json::from_value(json!({"type": "edge_auth_ok", "user_id": "u1"})).unwrap();
        assert!(matches!(msg, EdgeServerMessage::AuthOk { .. }));
    }

    #[test]
    fn edge_server_pong_deserializes() {
        let msg: EdgeServerMessage =
            serde_json::from_value(json!({"type": "edge_pong"})).unwrap();
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
