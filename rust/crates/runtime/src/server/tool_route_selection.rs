use serde::{Deserialize, Serialize};

use super::tool_binding_projection::{is_server_control_plane_tool, is_server_runtime_tool};
use super::tool_execution_binding::{
    ExecutorBindingKind, ToolExecutionRequest, ToolTransportKind, WorkspaceBindingKind,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionRouteKind {
    ServerLocal,
    ServerControlPlane,
    ServerRuntime,
    EdgeBound,
    GatewayRelay,
    SandboxResidentAgent,
    RequestScopedMcp,
    Unsupported,
}

impl ToolExecutionRouteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerLocal => "server_local",
            Self::ServerControlPlane => "server_control_plane",
            Self::ServerRuntime => "server_runtime",
            Self::EdgeBound => "edge_bound",
            Self::GatewayRelay => "gateway_relay",
            Self::SandboxResidentAgent => "sandbox_resident_agent",
            Self::RequestScopedMcp => "request_scoped_mcp",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Fallback routing via hardcoded tool-name matching.
///
/// This function runs **only** when [`CapabilityRegistry::resolve`] does not
/// find a provider for the tool.  Every tool returned here should eventually be
/// migrated to a registered capability so that `routing_decision` shrinks to
/// the executor-kind / transport-kind / workspace-kind branches that cannot
/// (by construction) be expressed as capability declarations.
///
/// Track migration: search for `TODO(routing-fallback)` in this file.
pub(crate) fn routing_decision(request: &ToolExecutionRequest) -> ToolExecutionRouteKind {
    if request.tool_name.starts_with("mcp__")
        || matches!(request.executor.kind, ExecutorBindingKind::Mcp)
    {
        return ToolExecutionRouteKind::RequestScopedMcp;
    }
    // TODO(routing-fallback): migrate these hardcoded matches to CapabilityRegistry.
    // When a new server-control-plane tool is added, its capability should be
    // registered in the registry — do NOT add another branch here.
    if is_server_control_plane_tool(&request.tool_name) {
        return ToolExecutionRouteKind::ServerControlPlane;
    }
    if is_server_runtime_tool(&request.tool_name) {
        return ToolExecutionRouteKind::ServerRuntime;
    }
    if matches!(request.executor.transport, ToolTransportKind::GatewayRelay) {
        return ToolExecutionRouteKind::GatewayRelay;
    }
    if matches!(
        request.executor.transport,
        ToolTransportKind::SandboxResidentAgent
    ) {
        return ToolExecutionRouteKind::SandboxResidentAgent;
    }
    match request.workspace.kind {
        WorkspaceBindingKind::EdgeWorkspace => return ToolExecutionRouteKind::EdgeBound,
        WorkspaceBindingKind::ServerSandbox | WorkspaceBindingKind::LocalFilesystem => {
            return ToolExecutionRouteKind::ServerLocal;
        }
        WorkspaceBindingKind::CloudWorkspace
        | WorkspaceBindingKind::None
        | WorkspaceBindingKind::Unknown => {}
    }
    ToolExecutionRouteKind::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_execution_binding::*;
    use serde_json::json;

    fn make_request(
        tool_name: &str,
        ws_kind: WorkspaceBindingKind,
        transport: ToolTransportKind,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest {
            user_id: "test".to_string(),
            run_id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            args: json!({}),
            executor: ExecutorBinding {
                kind: ExecutorBindingKind::ServerLocal,
                executor_id: String::new(),
                display_name: String::new(),
                transport,
                status: ExecutorStatus::Online,
            },
            workspace: WorkspaceBinding {
                kind: ws_kind,
                display_name: String::new(),
                cwd: None,
                authority: WorkspaceAuthority::ReadWrite,
                fallback_policy: FallbackPolicy::Disabled,
            },
            workspace_record: None,
            runtime: None,
            policy: ToolPolicySnapshot::default(),
        }
    }

    #[test]
    fn local_filesystem_routes_to_server_local() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::LocalFilesystem,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(routing_decision(&req), ToolExecutionRouteKind::ServerLocal);
    }

    #[test]
    fn server_sandbox_routes_to_server_local() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::ServerSandbox,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(routing_decision(&req), ToolExecutionRouteKind::ServerLocal);
    }

    #[test]
    fn edge_workspace_routes_to_edge_bound() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::EdgeWorkspace,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(routing_decision(&req), ToolExecutionRouteKind::EdgeBound);
    }

    #[test]
    fn mcp_prefix_routes_to_request_scoped_mcp() {
        let req = make_request(
            "mcp__foo",
            WorkspaceBindingKind::None,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&req),
            ToolExecutionRouteKind::RequestScopedMcp
        );
    }

    #[test]
    fn gateway_relay_transport_routes_to_gateway_relay() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::None,
            ToolTransportKind::GatewayRelay,
        );
        assert_eq!(routing_decision(&req), ToolExecutionRouteKind::GatewayRelay);
    }

    #[test]
    fn sandbox_resident_agent_transport_routes_correctly() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::None,
            ToolTransportKind::SandboxResidentAgent,
        );
        assert_eq!(
            routing_decision(&req),
            ToolExecutionRouteKind::SandboxResidentAgent
        );
    }

    #[test]
    fn unknown_workspace_with_local_transport_routes_unsupported() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::Unknown,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(routing_decision(&req), ToolExecutionRouteKind::Unsupported);
    }
}
