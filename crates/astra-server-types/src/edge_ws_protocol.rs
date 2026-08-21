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

/// Versioned Edge handshake capability required for request-scoped managed
/// attachment materialization and artifact publication.
pub const MANAGED_FILE_TRANSFER_V1_CAPABILITY: &str = "managed_file_transfer_v1";

/// Produce the deterministic, collision-free catalog filename used for one
/// attachment inventory entry. Every entry is namespaced by its stable
/// inventory index, including entries whose original names are unique.
pub fn runtime_attachment_destination_name(index: usize, name: &str) -> Option<String> {
    let trimmed = name.trim();
    (!trimmed.is_empty()
        && name.len() <= 240
        && !name.contains('\0')
        && trimmed == name
        && std::path::Path::new(name)
            .file_name()
            .and_then(|part| part.to_str())
            == Some(name))
    .then(|| format!("{index:06}-{name}"))
    .filter(|destination| destination.len() <= 255)
}

/// Host-owned file-transfer contract sent to an edge agent.  This wire type
/// intentionally lives with the WebSocket protocol so the edge-only build
/// does not depend on server services.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFileTransferAttachment {
    pub file_id: String,
    pub name: String,
    pub size: i64,
    pub md5: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFileTransferContext {
    pub endpoint_url: String,
    pub authorization: String,
    pub workspace_root: String,
    pub layout: RuntimeFileTransferLayout,
    pub max_file_bytes: u64,
    pub attachments: Vec<RuntimeFileTransferAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "layout", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeFileTransferLayout {
    Legacy {
        task_id: String,
        root: String,
        catalog_dir: String,
        session_dir: String,
        scratch_dir: String,
    },
    Ephemeral {
        work_dir: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFilesystemBoundaryContext {
    pub workspace_root: String,
    pub read_only_paths: Vec<String>,
}

#[cfg(feature = "server")]
impl From<&astra_services::runs::RuntimeFilesystemBoundaryContext>
    for RuntimeFilesystemBoundaryContext
{
    fn from(context: &astra_services::runs::RuntimeFilesystemBoundaryContext) -> Self {
        Self {
            workspace_root: context.workspace_root.clone(),
            read_only_paths: context.read_only_paths.clone(),
        }
    }
}

impl std::fmt::Debug for RuntimeFileTransferContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeFileTransferContext")
            .field("endpoint_url", &self.endpoint_url)
            .field("authorization_present", &!self.authorization.is_empty())
            .field("workspace_root", &self.workspace_root)
            .field("layout", &self.layout)
            .field("attachment_count", &self.attachments.len())
            .finish()
    }
}

#[cfg(feature = "server")]
impl From<&astra_services::runs::RuntimeFileTransferContext> for RuntimeFileTransferContext {
    fn from(context: &astra_services::runs::RuntimeFileTransferContext) -> Self {
        Self {
            endpoint_url: context.endpoint_url.clone(),
            authorization: context.authorization.clone(),
            workspace_root: context.workspace_root.clone(),
            layout: match &context.layout {
                astra_services::runs::RuntimeFileTransferLayout::Legacy {
                    task_id,
                    root,
                    catalog_dir,
                    session_dir,
                    scratch_dir,
                } => RuntimeFileTransferLayout::Legacy {
                    task_id: task_id.clone(),
                    root: root.clone(),
                    catalog_dir: catalog_dir.clone(),
                    session_dir: session_dir.clone(),
                    scratch_dir: scratch_dir.clone(),
                },
                astra_services::runs::RuntimeFileTransferLayout::Ephemeral { work_dir } => {
                    RuntimeFileTransferLayout::Ephemeral {
                        work_dir: work_dir.clone(),
                    }
                }
            },
            max_file_bytes: context.max_file_bytes,
            attachments: context
                .attachments
                .iter()
                .map(|attachment| RuntimeFileTransferAttachment {
                    file_id: attachment.file_id.clone(),
                    name: attachment.name.clone(),
                    size: attachment.size,
                    md5: attachment.md5.clone(),
                })
                .collect(),
        }
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
        identity: ToolInvocationIdentity,
        delivery_generation: u64,
        tool: String,
        args: Value,
        /// Host-owned transfer credentials and path contract. This field is
        /// never part of model-visible tool arguments or the invocation
        /// journal; its Debug implementation redacts authorization.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_file_transfer: Option<Box<RuntimeFileTransferContext>>,
        /// Non-secret mount boundary for host-owned lanes within the writable
        /// workspace. Unlike transfer credentials this field is durable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_filesystem_boundary: Option<Box<RuntimeFilesystemBoundaryContext>>,
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
            identity: identity(),
            delivery_generation: 1,
            tool: "bash".into(),
            args: json!({"command": "echo hello"}),
            runtime_file_transfer: None,
            runtime_filesystem_boundary: None,
            timeout_secs: 120,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_tool_request");
        assert_eq!(v["tool"], "bash");
        assert_eq!(v["timeout_secs"], 120);
    }

    #[test]
    fn edge_tool_request_round_trips_hidden_file_transfer_context() {
        let msg = EdgeServerMessage::ToolRequest {
            request_id: "req-transfer".into(),
            identity: identity(),
            delivery_generation: 1,
            tool: "materialize_attachment".into(),
            args: json!({"file_id": "file-1"}),
            runtime_file_transfer: Some(Box::new(RuntimeFileTransferContext {
                endpoint_url: "https://moi.example/runtime-files".into(),
                authorization: "Bearer runtime-grant".into(),
                workspace_root: "/sandbox".into(),
                layout: RuntimeFileTransferLayout::Legacy {
                    task_id: "task-1".into(),
                    root: "/sandbox/.moi/runtime/task-1".into(),
                    catalog_dir: "/sandbox/.moi/runtime/task-1/catalog".into(),
                    session_dir: "/sandbox/.moi/sessions/session-1".into(),
                    scratch_dir: "/sandbox/.moi/runtime/task-1/scratch".into(),
                },
                max_file_bytes: 1024,
                attachments: vec![RuntimeFileTransferAttachment {
                    file_id: "file-1".into(),
                    name: "paper.pdf".into(),
                    size: 10,
                    md5: "0123456789abcdef0123456789abcdef".into(),
                }],
            })),
            runtime_filesystem_boundary: Some(Box::new(RuntimeFilesystemBoundaryContext {
                workspace_root: "/sandbox".into(),
                read_only_paths: vec![
                    "/sandbox/.moi/runtime/task-1".into(),
                    "/sandbox/.moi/sessions/session-1".into(),
                ],
            })),
            timeout_secs: 120,
        };

        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: EdgeServerMessage = serde_json::from_str(&encoded).unwrap();

        match decoded {
            EdgeServerMessage::ToolRequest {
                runtime_file_transfer: Some(context),
                runtime_filesystem_boundary: Some(boundary),
                ..
            } => {
                assert!(matches!(
                    context.layout,
                    RuntimeFileTransferLayout::Legacy { ref task_id, .. } if task_id == "task-1"
                ));
                assert_eq!(context.attachments[0].file_id, "file-1");
                assert_eq!(context.authorization, "Bearer runtime-grant");
                assert_eq!(boundary.workspace_root, "/sandbox");
            }
            other => panic!("expected tool request with transfer context, got {other:?}"),
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
