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
//! {"type": "edge_auth_ok", "user_id": "...", "edge_id": "ws-...", "interaction_api_major": "3"}
//! {"type": "edge_auth_error", "message": "..."}
//! {"type": "edge_tool_request", "request_id": "...", "tool": "...", "args": {...}}
//! {"type": "edge_tool_input", "request_id": "...", "delivery_generation": 1, "input": {...}}
//! {"type": "edge_pong"}
//! {"type": "edge_closing", "reason": "..."}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_SEALED_CAPACITY_BYTES: usize = 128 * 1024;
pub const MAX_CAPACITY_RPC_TIMEOUT_SECS: u64 = 130;
pub const MAX_CAPACITY_RPC_INFLIGHT: usize = 64;

pub fn valid_capacity_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EdgeCapacityProvider {
    pub id: String,
    /// Public recipient key only; local endpoint and bearer key stay on Edge.
    pub public_key: String,
}

// The payload is sealed by the capacity owner, not interpreted by Astra.
// These wire types are available to both server and Edge feature profiles.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SealedCapacityPayload(pub String);

impl std::fmt::Debug for SealedCapacityPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[sealed capacity payload]")
    }
}

impl SealedCapacityPayload {
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
            && self.0.len() <= MAX_SEALED_CAPACITY_BYTES
            && self
                .0
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapacityRpcFailure {
    Invalid,
    Offline,
    BindingChanged,
    Unsupported,
    Busy,
    OutcomeUnknown,
}

pub use astra_turn_types::ToolInvocationIdentity;
use astra_turn_types::{ProviderStageInput, ProviderStageInputAck};

/// Edge can inject request-scoped provider authorization into one bash
/// subprocess without receiving file-transfer metadata or bytes.
pub const RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY: &str = "runtime_process_authorization_v1";

/// Edge accepts optional W3C `traceparent` and `tracestate` on a tool request.
/// Older edges use `deny_unknown_fields` and reject the message when these
/// fields are present, so the server sends them only after this advertisement.
pub const W3C_TRACE_CONTEXT_V1_CAPABILITY: &str = "w3c_trace_context_v1";

/// Whether an Edge runtime advertisement supports request-scoped process
/// authorization. This protocol predicate is shared by registration, run
/// admission, and dispatch so an incompatible Edge cannot be offered Bash and
/// rejected only after the model selects it.
pub fn supports_runtime_process_authorization(capabilities: Option<&Value>) -> bool {
    protocol_capability(capabilities, RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY)
}

/// Whether this connected edge can decode a W3C carrier on `ToolRequest`.
pub fn supports_w3c_trace_context(capabilities: Option<&Value>) -> bool {
    protocol_capability(capabilities, W3C_TRACE_CONTEXT_V1_CAPABILITY)
}

fn protocol_capability(capabilities: Option<&Value>, name: &str) -> bool {
    capabilities
        .and_then(|value| value.get("protocol_capabilities"))
        .and_then(|items| items.get(name))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Keep a call's W3C carrier only when the live edge advertised support.
/// An older edge must receive the previous message shape.
pub fn trace_carrier_for_connection<'a>(
    capabilities: Option<&Value>,
    traceparent: Option<&'a str>,
    tracestate: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    if supports_w3c_trace_context(capabilities) {
        (traceparent, tracestate)
    } else {
        (None, None)
    }
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

    /// Acknowledgement that an input sent to an active provider stage was
    /// accepted or rejected by the provider adapter. A missing acknowledgement
    /// is a transport failure; this message does not claim model output was
    /// produced.
    #[serde(rename = "edge_tool_input_ack")]
    ToolInputAck {
        request_id: String,
        delivery_generation: u64,
        ack: ProviderStageInputAck,
    },

    /// Edge heartbeat.
    #[serde(rename = "edge_capacity_result")]
    CapacityResult {
        request_id: String,
        connection_generation: u64,
        result: Result<SealedCapacityPayload, CapacityRpcFailure>,
    },

    /// Edge heartbeat.
    #[serde(rename = "edge_ping")]
    Ping {},
}

/// Concrete execution grant, frozen by admission rather than inferred from
/// tool arguments. Workspace authority is confined to the exact selected
/// materialization; it does not authorize reading arbitrary host files or
/// provider credentials. Consumers intersect it with local authority and
/// mandatory sensitive-path restrictions, never expand it via approval.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EdgeExecutionCeiling {
    pub workspace_root: String,
    pub workspace_id: Option<String>,
    pub materialization_id: Option<String>,
    pub execution_binding_generation: u64,
    /// Locally approved provider executable/bootstrap paths, frozen by
    /// admission. Files grant only that file; directories grant descendants
    /// subject to mandatory sensitive-path denial. Not arbitrary data roots.
    /// An empty list is explicit; consumers must never add fallback paths.
    pub runtime_read_paths: Vec<String>,
    pub workspace_write_allowed: bool,
    pub network_allowed: bool,
}

/// Messages sent from server to edge agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum EdgeServerMessage {
    /// Authentication succeeded.
    #[serde(rename = "edge_auth_ok")]
    AuthOk {
        user_id: String,
        /// Server-issued identity of this authenticated WebSocket registry
        /// generation. REST callbacks must echo this exact value; an
        /// edge-local agent label is not a transport identity.
        edge_id: String,
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
        /// Frozen per-invocation authority. Native executors require it;
        /// ordinary command execution still uses its existing local boundary.
        execution_ceiling: Option<Box<EdgeExecutionCeiling>>,
        /// Opaque provider authorization injected only for this bash call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_process_authorization: Option<Box<RuntimeProcessAuthorizationContext>>,
        /// Non-secret replay fence. A request that requires process
        /// authorization must never execute without its live credential.
        #[serde(default, skip_serializing_if = "is_false")]
        runtime_process_authorization_required: bool,
        /// Finite command limit; an authenticated task may have no cutoff.
        timeout_secs: Option<u64>,
        /// Immutable server-admitted work cutoff, excluding settlement grace.
        execution_deadline_unix_ms: Option<u64>,
        /// Work remaining when this dispatch was produced. Replay must also
        /// enforce the absolute cutoff; this value never renews authority.
        execution_timeout_ms: Option<u64>,
        /// Independent command policy ceiling, not the native job budget.
        command_timeout_cap_ms: Option<u64>,
        /// W3C context for this tool call. Absent when tracing is off or the
        /// connected edge did not advertise `w3c_trace_context_v1`. An older
        /// edge rejects unknown fields, so the server omits both until it does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        traceparent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tracestate: Option<String>,
    },

    /// Deliver one already-admitted semantic input to the active provider
    /// stage for this invocation. The request identity and delivery
    /// generation fence it to the exact in-flight execution.
    #[serde(rename = "edge_tool_input")]
    ToolInput {
        request_id: String,
        delivery_generation: u64,
        input: ProviderStageInput,
    },

    /// Server heartbeat response.
    #[serde(rename = "edge_capacity_request")]
    CapacityRequest {
        request_id: String,
        connection_generation: u64,
        provider_id: String,
        payload: SealedCapacityPayload,
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
}

impl EdgeServerMessage {
    /// Short stable label for diagnostic logging (no payload).
    pub fn diagnostic_kind(&self) -> &'static str {
        match self {
            EdgeServerMessage::AuthOk { .. } => "auth_ok",
            EdgeServerMessage::AuthError { .. } => "auth_error",
            EdgeServerMessage::ToolRequest { .. } => "tool_request",
            EdgeServerMessage::ToolInput { .. } => "tool_input",
            EdgeServerMessage::Pong {} => "pong",
            EdgeServerMessage::Closing { .. } => "closing",
            EdgeServerMessage::ToolCancel { .. } => "tool_cancel",
            EdgeServerMessage::ToolResultAck { .. } => "tool_result_ack",
            EdgeServerMessage::CapacityRequest { .. } => "capacity_request",
        }
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
            execution_ceiling: None,
            request_id: "req-456".into(),
            identity: Box::new(identity()),
            delivery_generation: 1,
            tool: "bash".into(),
            args: json!({"command": "echo hello"}),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: Some(120),
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,

            traceparent: None,
            tracestate: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_tool_request");
        assert_eq!(v["tool"], "bash");
        assert_eq!(v["timeout_secs"], 120);
        assert!(v.get("traceparent").is_none());
        assert!(v.get("tracestate").is_none());
    }

    #[test]
    fn edge_tool_request_accepts_a_missing_trace_carrier() {
        let msg = EdgeServerMessage::ToolRequest {
            execution_ceiling: None,
            request_id: "req-old".into(),
            identity: Box::new(identity()),
            delivery_generation: 1,
            tool: "bash".into(),
            args: json!({"command": "true"}),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: Some(30),
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,
            traceparent: None,
            tracestate: None,
        };
        let encoded = serde_json::to_value(&msg).unwrap();
        let parsed: EdgeServerMessage = serde_json::from_value(encoded).unwrap();
        let EdgeServerMessage::ToolRequest {
            traceparent,
            tracestate,
            ..
        } = parsed
        else {
            panic!("expected tool request");
        };
        assert_eq!(traceparent, None);
        assert_eq!(tracestate, None);
    }

    #[test]
    fn edge_tool_request_round_trips_hidden_process_authorization() {
        let msg = EdgeServerMessage::ToolRequest {
            execution_ceiling: None,
            request_id: "req-process-auth".into(),
            identity: Box::new(identity()),
            delivery_generation: 1,
            tool: "bash".into(),
            args: json!({"command": "moi-cli file list"}),
            runtime_process_authorization: Some(Box::new(RuntimeProcessAuthorizationContext {
                authorization: "Bearer runtime-grant".into(),
            })),
            runtime_process_authorization_required: true,
            timeout_secs: Some(120),
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,

            traceparent: None,
            tracestate: None,
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
    fn trace_carrier_stays_off_the_wire_without_edge_support() {
        let parent = Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01");
        let state = Some("vendor=value");
        assert_eq!(
            trace_carrier_for_connection(None, parent, state),
            (None, None)
        );
        assert_eq!(
            trace_carrier_for_connection(
                Some(&json!({"protocol_capabilities": {"w3c_trace_context_v1": false}})),
                parent,
                state,
            ),
            (None, None)
        );
        assert_eq!(
            trace_carrier_for_connection(
                Some(&json!({"protocol_capabilities": {"w3c_trace_context_v1": true}})),
                parent,
                state,
            ),
            (parent, state)
        );
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
            edge_id: "ws-test".into(),
            interaction_api_major: crate::AGENT_INTERACTION_API_MAJOR.into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "edge_auth_ok");
        assert_eq!(v["user_id"], "u-123");
        assert_eq!(v["edge_id"], "ws-test");
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
    fn provider_stage_input_round_trips_with_exact_turn_fence() {
        let input = ProviderStageInput::Text {
            input_id: "message-1".into(),
            content: "continue".into(),
            correlation_id: Some("parent-turn".into()),
            expected_turn_id: Some("turn-7".into()),
        };
        let message = EdgeServerMessage::ToolInput {
            request_id: "run/session/call".into(),
            delivery_generation: 9,
            input: input.clone(),
        };
        let decoded: EdgeServerMessage =
            serde_json::from_value(serde_json::to_value(message).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            EdgeServerMessage::ToolInput {
                request_id,
                delivery_generation: 9,
                input: decoded_input,
            } if request_id == "run/session/call" && decoded_input == input
        ));

        let ack = EdgeClientMessage::ToolInputAck {
            request_id: "run/session/call".into(),
            delivery_generation: 9,
            ack: ProviderStageInputAck::accepted(&input, Some("turn-7".into())),
        };
        let decoded: EdgeClientMessage =
            serde_json::from_value(serde_json::to_value(ack).unwrap()).unwrap();
        assert!(matches!(
            decoded,
            EdgeClientMessage::ToolInputAck {
                request_id,
                delivery_generation: 9,
                ack,
            } if request_id == "run/session/call"
                && ack.input_id == "message-1"
                && ack.provider_turn_id.as_deref() == Some("turn-7")
        ));
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
                assert_eq!(timeout_secs, Some(120));
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
            "edge_id": "ws-u1",
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
