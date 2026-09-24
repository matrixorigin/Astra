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
//! {"type": "edge_auth", "edge_agent_id": "...", "materialization_id": "...", "interaction_api_major": "3", "hostname": "...", "workspace_dir": "..."}
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

/// Maximum UTF-8 bytes of a complete serialized Edge WebSocket message,
/// including identity, delivery generation, metadata and JSON escaping.
pub const MAX_EDGE_MESSAGE_BYTES: usize = 256 * 1024;

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
        /// Stable identity persisted by the Edge beside its checkout. It
        /// survives reconnects and Edge-agent label changes, while separate
        /// devices materializing the same path receive different identities.
        materialization_id: String,
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

    /// Result of creating an isolated evaluation workspace clone.
    #[serde(rename = "edge_workspace_prepared")]
    WorkspacePrepared {
        request_id: String,
        connection_generation: u64,
        workspace_dir: String,
        allocation: Option<astra_runtime_env::EvaluationAllocationReceipt>,
        source_commit: Option<String>,
        source_tree: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },

    /// Fresh source proof for an already prepared evaluation workspace.
    #[serde(rename = "edge_workspace_snapshot")]
    WorkspaceSnapshot {
        request_id: String,
        connection_generation: u64,
        workspace_dir: String,
        allocation: Option<astra_runtime_env::EvaluationAllocationReceipt>,
        source_commit: Option<String>,
        source_tree: Option<String>,
        clean: bool,
        #[serde(default)]
        error: Option<String>,
    },

    /// Durable coding evidence captured after agent execution and before the
    /// per-trial workspace lease may be released.
    #[serde(rename = "edge_workspace_finalized")]
    WorkspaceFinalized {
        request_id: String,
        connection_generation: u64,
        workspace_dir: String,
        allocation: Option<astra_runtime_env::EvaluationAllocationReceipt>,
        source_commit: Option<String>,
        source_tree: Option<String>,
        base_revision: Option<String>,
        result_revision: Option<String>,
        patch: Option<String>,
        verifier_exit_code: Option<i32>,
        verifier_output: Option<String>,
        namespace_active: bool,
        scope_settled: bool,
        timed_out: bool,
        #[serde(default)]
        error: Option<String>,
    },

    /// Edge heartbeat.
    #[serde(rename = "edge_ping")]
    Ping {},
}

/// Server-frozen inputs for provisioning one Evaluation allocation.
#[derive(Debug, Clone, Copy)]
pub struct EdgeWorkspacePreparationRequest<'a> {
    pub connection_generation: u64,
    pub workspace_key: &'a str,
    pub session_id: &'a str,
    pub source_commit: &'a str,
    pub confinement: &'a astra_runtime_env::WorkspaceConfinementContract,
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
        evaluation_allocation: Option<Box<astra_runtime_env::EvaluationAllocationReceipt>>,
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

    /// Create a per-trial clone from the requested immutable source commit.
    #[serde(rename = "edge_workspace_prepare")]
    WorkspacePrepare {
        request_id: String,
        connection_generation: u64,
        workspace_key: String,
        session_id: String,
        source_commit: String,
        confinement: astra_runtime_env::WorkspaceConfinementContract,
    },

    /// Ask the Edge to prove the current source identity of a prepared clone.
    #[serde(rename = "edge_workspace_snapshot_request")]
    WorkspaceSnapshotRequest {
        request_id: String,
        connection_generation: u64,
        allocation: astra_runtime_env::EvaluationAllocationReceipt,
    },

    /// Capture the final patch and execute the server-frozen verifier in the
    /// exact trial workspace. This operation is independent from model tools.
    #[serde(rename = "edge_workspace_finalize")]
    WorkspaceFinalize {
        request_id: String,
        connection_generation: u64,
        allocation: astra_runtime_env::EvaluationAllocationReceipt,
        verifier_command: String,
        verifier_timeout_secs: u64,
        finalization_deadline_unix_ms: u64,
    },

    /// Cancel an in-flight workspace finalization owned by this connection.
    #[serde(rename = "edge_workspace_finalize_cancel")]
    WorkspaceFinalizeCancel {
        request_id: String,
        connection_generation: u64,
    },

    /// Release a clean per-trial clone after the Run has settled.
    #[serde(rename = "edge_workspace_release")]
    WorkspaceRelease {
        connection_generation: u64,
        allocation: astra_runtime_env::EvaluationAllocationReceipt,
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
}

impl EdgeServerMessage {
    /// Short stable label for diagnostic logging (no payload).
    pub fn diagnostic_kind(&self) -> &'static str {
        match self {
            EdgeServerMessage::AuthOk { .. } => "auth_ok",
            EdgeServerMessage::AuthError { .. } => "auth_error",
            EdgeServerMessage::ToolRequest { .. } => "tool_request",
            EdgeServerMessage::WorkspacePrepare { .. } => "workspace_prepare",
            EdgeServerMessage::WorkspaceSnapshotRequest { .. } => "workspace_snapshot_request",
            EdgeServerMessage::WorkspaceFinalize { .. } => "workspace_finalize",
            EdgeServerMessage::WorkspaceFinalizeCancel { .. } => "workspace_finalize_cancel",
            EdgeServerMessage::WorkspaceRelease { .. } => "workspace_release",
            EdgeServerMessage::Pong {} => "pong",
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
pub(crate) fn test_allocation_receipt() -> astra_runtime_env::EvaluationAllocationReceipt {
    astra_runtime_env::EvaluationAllocationReceipt {
        schema_version: 1,
        allocation_id: "allocation".into(),
        owner_user_id: "owner".into(),
        session_id: "session".into(),
        run_id: "run".into(),
        deployment_id: "deployment".into(),
        materialization_id: "materialization".into(),
        workspace_dir: "/workspace/trial".into(),
        source_commit: "a".repeat(40),
        source_tree: "b".repeat(40),
        confinement_fingerprint: format!("sha256:{}", "c".repeat(64)),
    }
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
            "materialization_id": "materialization-1",
            "interaction_api_major": crate::AGENT_INTERACTION_API_MAJOR,
            "hostname": "laptop",
            "workspace_dir": "/home/user/project"
        }))
        .unwrap();
        match msg {
            EdgeClientMessage::Auth {
                edge_agent_id,
                materialization_id,
                interaction_api_major,
                hostname,
                ..
            } => {
                assert_eq!(edge_agent_id, "my-edge");
                assert_eq!(materialization_id, "materialization-1");
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
            evaluation_allocation: None,
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
            evaluation_allocation: None,
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
            materialization_id: "materialization-1".into(),
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
    fn edge_workspace_operations_round_trip() {
        let prepare = EdgeServerMessage::WorkspacePrepare {
            request_id: "workspace-request".to_string(),
            connection_generation: 4,
            workspace_key: "trial-1".to_string(),
            session_id: "session".to_string(),
            source_commit: "a".repeat(40),
            confinement: serde_json::from_value(serde_json::json!({
                "profile_id": astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE,
                "toolchain_manifest": {
                    "schema_version": 1,
                    "inputs": [{"guest_mount_path": "/usr/bin", "content_digest": format!("sha256:{}", "a".repeat(64))}],
                    "launcher_digest": format!("sha256:{}", "b".repeat(64)),
                    "supervisor_digest": format!("sha256:{}", "c".repeat(64))
                }
            })).unwrap(),
        };
        let decoded: EdgeServerMessage =
            serde_json::from_value(serde_json::to_value(&prepare).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            EdgeServerMessage::WorkspacePrepare { .. }
        ));

        let snapshot = EdgeClientMessage::WorkspaceSnapshot {
            request_id: "workspace-request".to_string(),
            connection_generation: 4,
            workspace_dir: "/workspace/.astra-evaluation-trial-1".to_string(),
            allocation: Some(test_allocation_receipt()),
            source_commit: Some("a".repeat(40)),
            source_tree: Some("b".repeat(40)),
            clean: true,
            error: None,
        };
        let decoded: EdgeClientMessage =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            EdgeClientMessage::WorkspaceSnapshot { clean: true, .. }
        ));

        let finalize = EdgeServerMessage::WorkspaceFinalize {
            request_id: "finalize-request".to_string(),
            connection_generation: 4,
            allocation: test_allocation_receipt(),
            verifier_command: "make check".to_string(),
            verifier_timeout_secs: 120,
            finalization_deadline_unix_ms: 1_900_000_000_000,
        };
        let decoded: EdgeServerMessage =
            serde_json::from_value(serde_json::to_value(&finalize).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            EdgeServerMessage::WorkspaceFinalize {
                verifier_timeout_secs: 120,
                ..
            }
        ));

        let finalized = EdgeClientMessage::WorkspaceFinalized {
            request_id: "finalize-request".to_string(),
            connection_generation: 4,
            workspace_dir: "/workspace/.astra-evaluation-trial-1".to_string(),
            allocation: Some(test_allocation_receipt()),
            source_commit: Some("a".repeat(40)),
            source_tree: Some("b".repeat(40)),
            base_revision: Some(format!("sha256:{}", "c".repeat(64))),
            result_revision: Some(format!("sha256:{}", "d".repeat(64))),
            patch: Some("diff --git a/a b/a".to_string()),
            verifier_exit_code: Some(0),
            verifier_output: Some("ok".to_string()),
            namespace_active: true,
            scope_settled: true,
            timed_out: false,
            error: None,
        };
        let decoded: EdgeClientMessage =
            serde_json::from_value(serde_json::to_value(&finalized).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            EdgeClientMessage::WorkspaceFinalized {
                verifier_exit_code: Some(0),
                namespace_active: true,
                ..
            }
        ));
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
