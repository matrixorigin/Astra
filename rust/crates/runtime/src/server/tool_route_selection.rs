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
        WorkspaceBindingKind::ServerSandbox => return ToolExecutionRouteKind::ServerLocal,
        WorkspaceBindingKind::CloudWorkspace
        | WorkspaceBindingKind::None
        | WorkspaceBindingKind::Unknown
        | WorkspaceBindingKind::LocalFilesystem => {}
    }
    ToolExecutionRouteKind::Unsupported
}
