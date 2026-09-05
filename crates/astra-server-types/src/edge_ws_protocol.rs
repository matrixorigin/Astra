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
//! {"type": "edge_auth", "edge_agent_id": "...", "interaction_api_major": "3", "hostname": "...", "workspace_dir": "..."}
//! {"type": "edge_tool_result", "request_id": "...", "output": "...", "is_error": false, "tool_result_fields": {"exit_code": 0}}
//! {"type": "edge_ping"}
//! ```
//!
//! **Server → Edge** (JSON text frames):
//! ```text
//! {"type": "edge_auth_ok", "user_id": "...", "interaction_api_major": "3"}
//! {"type": "edge_auth_error", "message": "..."}
//! {"type": "edge_tool_request", "request_id": "...", "tool": "...", "args": {...}}
//! {"type": "edge_pong"}
//! {"type": "edge_closing", "reason": "..."}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use astra_turn_types::ToolInvocationIdentity;

/// Content-free parse diagnostic shared by both ends of the connection. Serde
/// error messages can quote secret-bearing malformed values or unknown fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeMessageDecodeDiagnostic {
    pub category: &'static str,
    pub line: usize,
    pub column: usize,
}

impl From<&serde_json::Error> for EdgeMessageDecodeDiagnostic {
    fn from(error: &serde_json::Error) -> Self {
        Self {
            category: match error.classify() {
                serde_json::error::Category::Io => "io",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "eof",
            },
            line: error.line(),
            column: error.column(),
        }
    }
}

/// Edge can inject request-scoped provider authorization into one bash
/// subprocess without receiving file-transfer metadata or bytes.
pub const RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY: &str = "runtime_process_authorization_v1";

/// Whether an Edge runtime advertisement supports request-scoped process
/// authorization. This protocol predicate is shared by registration, run
/// admission, and dispatch so an incompatible Edge cannot be offered Bash and
/// rejected only after the model selects it.
pub fn supports_runtime_process_authorization(capabilities: Option<&Value>) -> bool {
    capabilities
        .and_then(|value| value.get("protocol_capabilities"))
        .and_then(|items| items.get(RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Whether the process-authorization capability applies to this tool.
///
/// This is protocol semantics shared by the server and Edge, not a second
/// independently maintained tool-surface allowlist.
pub fn runtime_process_authorization_applies_to_tool(tool: &str) -> bool {
    tool == "bash"
}

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

/// Default timeout for tool execution on the edge agent.
pub const EDGE_TOOL_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Hard upper bound for one edge invocation.
///
/// Long-session policies may extend a normal command beyond the interactive
/// default, but every participant still needs a finite common custody window:
/// the server's socket waiter, edge executor, and callback ledger must agree
/// on this bound.  Thirty minutes covers ordinary dependency installation and
/// builds without turning a cancelled invocation into an unbounded lease.
pub const MAX_EDGE_TOOL_TIMEOUT_SECS: u64 = 1_800;

/// Time reserved after edge execution ends for its durable result callback.
/// This is transport settlement time, not additional tool execution time.
pub const EDGE_TOOL_RESULT_GRACE_SECS: u64 = 10;

/// Timeout for the initial auth message after WebSocket upgrade.
pub const EDGE_AUTH_TIMEOUT_SECS: u64 = 30;

/// Heartbeat interval for edge keep-alive.
pub const EDGE_HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Messages sent from edge agent to server.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum EdgeClientMessage {
    /// Declare the authenticated edge agent's identity and capabilities.
    ///
    /// HTTP authentication is completed before the WebSocket upgrade; secrets
    /// must never be repeated in an application frame.
    #[serde(rename = "edge_auth")]
    Auth {
        edge_agent_id: String,
        interaction_api_major: String,
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
    Ping {},

    /// Negotiate inference independently of tool execution. A declaration is
    /// not authority to publish an executable model.
    #[serde(rename = "inference_hello")]
    InferenceHello {
        protocol_version: u16,
        journal_id: crate::runner_inference::RunnerInferenceId,
        process_boot_nonce: crate::runner_inference::RunnerInferenceId,
    },

    /// Publish or disable one complete public binding revision. Transport
    /// secrets and caller-claimed principal/session ownership are forbidden.
    #[serde(rename = "inference_binding_publish")]
    InferenceBindingPublish {
        publication: Box<crate::runner_inference::RunnerInferenceBindingPublication>,
    },

    #[serde(rename = "inference_start_evidence")]
    InferenceStartEvidence {
        grant: Box<crate::runner_inference::RunnerInferenceDispatchGrant>,
        delivery_generation: u64,
        evidence: crate::runner_inference::RunnerInferenceStartEvidence,
    },

    #[serde(rename = "inference_terminal")]
    InferenceTerminal {
        transfer: Box<crate::runner_inference::RunnerInferenceTerminalTransfer>,
        delivery_generation: u64,
    },

    #[serde(rename = "inference_response_chunk")]
    InferenceResponseChunk {
        attempt_id: crate::runner_inference::RunnerInferenceId,
        delivery_generation: u64,
        chunk: crate::runner_inference::RunnerInferencePayloadChunk,
    },

    #[serde(rename = "inference_request_credit")]
    InferenceRequestCredit {
        attempt_id: crate::runner_inference::RunnerInferenceId,
        delivery_generation: u64,
        next_offset: u32,
        credit_bytes: u32,
    },
}

/// Messages sent from server to edge agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum EdgeServerMessage {
    /// Authentication succeeded.
    #[serde(rename = "edge_auth_ok")]
    AuthOk {
        user_id: String,
        interaction_api_major: String,
    },

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
    Pong {},

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

    #[serde(rename = "inference_hello_ack")]
    InferenceHelloAck {
        negotiation: crate::runner_inference::RunnerInferenceNegotiation,
    },

    #[serde(rename = "inference_binding_rejected")]
    InferenceBindingRejected {
        rejection: crate::runner_inference::RunnerInferenceBindingRejection,
    },

    #[serde(rename = "inference_binding_ack")]
    InferenceBindingAck {
        receipt: crate::runner_inference::RunnerInferenceBindingReceipt,
    },

    #[serde(rename = "inference_dispatch")]
    InferenceDispatch {
        grant: Box<crate::runner_inference::RunnerInferenceDispatchGrant>,
        delivery_generation: u64,
    },

    #[serde(rename = "inference_request_chunk")]
    InferenceRequestChunk {
        attempt_id: crate::runner_inference::RunnerInferenceId,
        delivery_generation: u64,
        chunk: crate::runner_inference::RunnerInferencePayloadChunk,
    },

    #[serde(rename = "inference_cancel")]
    InferenceCancel {
        grant: Box<crate::runner_inference::RunnerInferenceDispatchGrant>,
        delivery_generation: u64,
    },

    #[serde(rename = "inference_reconcile")]
    InferenceReconcile {
        grant: Box<crate::runner_inference::RunnerInferenceDispatchGrant>,
        delivery_generation: u64,
    },

    #[serde(rename = "inference_terminal_ack")]
    InferenceTerminalAck {
        ack: Box<crate::runner_inference::RunnerInferenceTerminalAck>,
        delivery_generation: u64,
    },

    #[serde(rename = "inference_response_credit")]
    InferenceResponseCredit {
        attempt_id: crate::runner_inference::RunnerInferenceId,
        delivery_generation: u64,
        next_offset: u32,
        credit_bytes: u32,
    },

    #[serde(rename = "inference_rejected")]
    InferenceRejected {
        attempt_id: Option<crate::runner_inference::RunnerInferenceId>,
        reason: crate::runner_inference::RunnerInferenceRejection,
    },
}

impl EdgeServerMessage {
    /// Short stable label for diagnostic logging (no payload).
    pub fn diagnostic_kind(&self) -> &'static str {
        match self {
            EdgeServerMessage::AuthOk { .. } => "auth_ok",
            EdgeServerMessage::AuthError { .. } => "auth_error",
            EdgeServerMessage::ToolRequest { .. } => "tool_request",
            EdgeServerMessage::Pong {} => "pong",
            EdgeServerMessage::Closing { .. } => "closing",
            EdgeServerMessage::ToolCancel { .. } => "tool_cancel",
            EdgeServerMessage::ToolResultAck { .. } => "tool_result_ack",
            EdgeServerMessage::InferenceHelloAck { .. } => "inference_hello_ack",
            EdgeServerMessage::InferenceBindingRejected { .. } => "inference_binding_rejected",
            EdgeServerMessage::InferenceBindingAck { .. } => "inference_binding_ack",
            EdgeServerMessage::InferenceDispatch { .. } => "inference_dispatch",
            EdgeServerMessage::InferenceRequestChunk { .. } => "inference_request_chunk",
            EdgeServerMessage::InferenceCancel { .. } => "inference_cancel",
            EdgeServerMessage::InferenceReconcile { .. } => "inference_reconcile",
            EdgeServerMessage::InferenceTerminalAck { .. } => "inference_terminal_ack",
            EdgeServerMessage::InferenceResponseCredit { .. } => "inference_response_credit",
            EdgeServerMessage::InferenceRejected { .. } => "inference_rejected",
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
    fn malformed_inference_values_never_enter_parse_diagnostics() {
        let canary = "private-canary-credential";
        let client_frame = format!(r#"{{"type":"inference_hello","protocol_version":"{canary}"}}"#);
        let server_frame =
            format!(r#"{{"type":"inference_hello_ack","negotiation":{{"status":"{canary}"}}}}"#);
        let client = serde_json::from_str::<EdgeClientMessage>(&client_frame).unwrap_err();
        let server = serde_json::from_str::<EdgeServerMessage>(&server_frame).unwrap_err();
        for (frame, error) in [(client_frame, client), (server_frame, server)] {
            assert!(
                error.to_string().contains(canary),
                "fixture must exercise a raw error that would leak"
            );
            let diagnostic = EdgeMessageDecodeDiagnostic::from(&error);
            assert_eq!(diagnostic.category, "data");
            // Derived-enum data errors may have no exact source location (0).
            assert!(diagnostic.line <= 1);
            assert!(diagnostic.column <= frame.len());
            assert!(!format!("{diagnostic:?}").contains(canary));
        }
    }

    #[test]
    fn inference_negotiation_is_a_separate_strict_protocol_facet() {
        let message: EdgeClientMessage = serde_json::from_value(json!({
            "type": "inference_hello", "protocol_version": 1,
            "journal_id": "journal", "process_boot_nonce": "boot"
        }))
        .unwrap();
        assert!(matches!(
            message,
            EdgeClientMessage::InferenceHello {
                protocol_version: 1,
                ..
            }
        ));
        for field in [
            "authorization",
            "endpoint_url",
            "user_id",
            "session_id",
            "tool",
            "args",
        ] {
            let mut frame = json!({"type": "inference_hello", "protocol_version": 1,
                "journal_id": "journal", "process_boot_nonce": "boot"});
            frame[field] = json!("canary-secret");
            assert!(serde_json::from_value::<EdgeClientMessage>(frame).is_err());
        }
        let response = EdgeServerMessage::InferenceHelloAck {
            negotiation: crate::runner_inference::RunnerInferenceNegotiation::accepted(1, 7, 1000),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "type": "inference_hello_ack", "negotiation": {"status": "accepted", "protocol_version": 1,
                    "delivery_generation": 7, "max_artifact_bytes": 16777216, "server_unix_ms": 1000}
            })
        );
    }

    #[test]
    fn edge_auth_deserializes() {
        let msg: EdgeClientMessage = serde_json::from_value(json!({
            "type": "edge_auth",
            "edge_agent_id": "my-edge",
            "interaction_api_major": crate::AGENT_INTERACTION_API_MAJOR,
            "hostname": "laptop",
            "workspace_dir": "/home/user/project"
        }))
        .unwrap();
        match msg {
            EdgeClientMessage::Auth {
                edge_agent_id,
                interaction_api_major,
                hostname,
                ..
            } => {
                assert_eq!(edge_agent_id, "my-edge");
                assert_eq!(interaction_api_major, crate::AGENT_INTERACTION_API_MAJOR);
                assert_eq!(hostname.as_deref(), Some("laptop"));
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn edge_auth_requires_interaction_contract_identity() {
        assert!(
            serde_json::from_value::<EdgeClientMessage>(json!({
                "type": "edge_auth",
                "edge_agent_id": "legacy-edge",
            }))
            .is_err()
        );
    }

    #[test]
    fn edge_messages_reject_unknown_or_retired_fields() {
        assert!(
            serde_json::from_value::<EdgeClientMessage>(json!({
                "type": "edge_auth",
                "edge_agent_id": "edge-a",
                "interaction_api_major": crate::AGENT_INTERACTION_API_MAJOR,
                "token": "retired-inline-secret"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<EdgeClientMessage>(json!({
                "type": "edge_ping",
                "request_id": "retired"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<EdgeServerMessage>(json!({
                "type": "edge_tool_cancel",
                "request_id": "req-1",
                "delivery_generation": 1,
                "tool_call_id": "retired"
            }))
            .is_err()
        );
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
    fn process_authorization_support_requires_explicit_v1_advertisement() {
        assert!(!supports_runtime_process_authorization(None));
        assert!(!supports_runtime_process_authorization(Some(&json!({
            "protocol_capabilities": {}
        }))));
        assert!(!supports_runtime_process_authorization(Some(&json!({
            "protocol_capabilities": {
                "runtime_process_authorization_v1": false
            }
        }))));
        assert!(supports_runtime_process_authorization(Some(&json!({
            "protocol_capabilities": {
                "runtime_process_authorization_v1": true
            }
        }))));
    }

    #[test]
    fn edge_ping_deserializes() {
        let msg: EdgeClientMessage = serde_json::from_value(json!({"type": "edge_ping"})).unwrap();
        assert!(matches!(msg, EdgeClientMessage::Ping {}));
    }

    #[test]
    fn edge_pong_serializes() {
        let msg = EdgeServerMessage::Pong {};
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_pong");
    }

    #[test]
    fn edge_auth_ok_serializes() {
        let msg = EdgeServerMessage::AuthOk {
            user_id: "u-123".into(),
            interaction_api_major: crate::AGENT_INTERACTION_API_MAJOR.into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_auth_ok");
        assert_eq!(v["user_id"], "u-123");
        assert_eq!(
            v["interaction_api_major"],
            crate::AGENT_INTERACTION_API_MAJOR
        );
    }

    #[test]
    fn edge_client_auth_serializes() {
        let msg = EdgeClientMessage::Auth {
            edge_agent_id: "e1".into(),
            interaction_api_major: crate::AGENT_INTERACTION_API_MAJOR.into(),
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
        let v = serde_json::to_value(&EdgeClientMessage::Ping {}).unwrap();
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
        let msg: EdgeServerMessage = serde_json::from_value(json!({
            "type": "edge_auth_ok",
            "user_id": "u1",
            "interaction_api_major": crate::AGENT_INTERACTION_API_MAJOR,
        }))
        .unwrap();
        assert!(matches!(msg, EdgeServerMessage::AuthOk { .. }));
    }

    #[test]
    fn edge_server_pong_deserializes() {
        let msg: EdgeServerMessage = serde_json::from_value(json!({"type": "edge_pong"})).unwrap();
        assert!(matches!(msg, EdgeServerMessage::Pong {}));
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
