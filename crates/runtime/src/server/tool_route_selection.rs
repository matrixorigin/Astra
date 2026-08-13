use serde::{Deserialize, Serialize};

use super::tool_execution_binding::{
    ExecutorBindingKind, ExecutorStatus, ToolExecutionRequest, ToolTransportKind,
    WorkspaceBindingKind,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolExecutionClass {
    ServerControlPlane,
    ServerService,
    SharedServiceOrRuntime,
    RuntimeExecutor,
    RequestScopedMcp,
    /// Virtual turn-pipeline tools are advertised as schemas but are consumed
    /// before route execution. They are not executable by a workspace,
    /// executor, edge transport, or MCP provider.
    TurnPipelineIntercept,
    Unknown,
}

/// Server-local adapters for workspace/runtime tools.
///
/// This is adapter inventory, not fallback routing policy. A tool may be a
/// valid runtime-executor capability and still be unsupported by the API-server
/// binary if no in-process adapter exists here. Edge, gateway, and
/// orchestrator-managed runtimes advertise and execute their own adapters.
pub(crate) const SERVER_LOCAL_RUNTIME_TOOL_NAMES: &[&str] = &[
    "bash",
    "git",
    "glob",
    "grep",
    "list_dir",
    "publish_artifact",
    "read_file",
    "rollback_file_edits",
    "run_script",
    "str_replace",
    "symbols",
    "web_fetch",
    "web_search",
    "write_file",
];

pub(crate) fn server_local_runtime_tool_supported(tool_name: &str) -> bool {
    SERVER_LOCAL_RUNTIME_TOOL_NAMES.contains(&tool_name)
}

pub(crate) fn tool_execution_class(
    tool_name: &str,
    registry: &astra_runtime_env::ToolRegistry,
) -> ToolExecutionClass {
    if astra_runtime_env::is_mcp_namespaced_tool_name(tool_name) {
        return ToolExecutionClass::RequestScopedMcp;
    }
    if is_intercepted_turn_pipeline_tool(tool_name) {
        return ToolExecutionClass::TurnPipelineIntercept;
    }

    let Some(spec) = registry.get(tool_name) else {
        return ToolExecutionClass::Unknown;
    };

    match spec.required.executor {
        astra_runtime_env::RequiredExecutor::ControlPlane => ToolExecutionClass::ServerControlPlane,
        astra_runtime_env::RequiredExecutor::ServiceExecutor => ToolExecutionClass::ServerService,
        astra_runtime_env::RequiredExecutor::ServiceOrRuntimeExecutor => {
            ToolExecutionClass::SharedServiceOrRuntime
        }
        astra_runtime_env::RequiredExecutor::McpExecutor => ToolExecutionClass::RequestScopedMcp,
        astra_runtime_env::RequiredExecutor::RuntimeExecutor
        | astra_runtime_env::RequiredExecutor::None => ToolExecutionClass::RuntimeExecutor,
    }
}

fn is_intercepted_turn_pipeline_tool(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case(crate::turn::skill_tool::SKILL_TOOL_NAME)
        || tool_name.eq_ignore_ascii_case(crate::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME)
}

#[cfg(test)]
pub(crate) fn routing_decision(
    request: &ToolExecutionRequest,
    registry: &astra_runtime_env::ToolRegistry,
) -> ToolExecutionRouteKind {
    routing_decision_for_binding(
        &request.tool_name,
        request.workspace.kind,
        request.executor.transport,
        registry,
    )
}

pub(crate) fn routing_decision_for_binding(
    tool_name: &str,
    workspace_kind: WorkspaceBindingKind,
    executor_transport: ToolTransportKind,
    registry: &astra_runtime_env::ToolRegistry,
) -> ToolExecutionRouteKind {
    match tool_execution_class(tool_name, registry) {
        ToolExecutionClass::ServerControlPlane => {
            return ToolExecutionRouteKind::ServerControlPlane;
        }
        ToolExecutionClass::ServerService => return ToolExecutionRouteKind::ServerRuntime,
        ToolExecutionClass::RequestScopedMcp => {
            return if matches!(executor_transport, ToolTransportKind::McpHttp) {
                ToolExecutionRouteKind::RequestScopedMcp
            } else {
                ToolExecutionRouteKind::Unsupported
            };
        }
        ToolExecutionClass::TurnPipelineIntercept | ToolExecutionClass::Unknown => {
            return ToolExecutionRouteKind::Unsupported;
        }
        ToolExecutionClass::SharedServiceOrRuntime => {
            return shared_service_or_runtime_route_for_binding(
                tool_name,
                workspace_kind,
                executor_transport,
            );
        }
        ToolExecutionClass::RuntimeExecutor => {}
    }

    runtime_executor_route_for_binding(tool_name, workspace_kind, executor_transport)
}

fn shared_service_or_runtime_route_for_binding(
    tool_name: &str,
    workspace_kind: WorkspaceBindingKind,
    executor_transport: ToolTransportKind,
) -> ToolExecutionRouteKind {
    match runtime_executor_route_for_binding(tool_name, workspace_kind, executor_transport) {
        ToolExecutionRouteKind::Unsupported
            if matches!(workspace_kind, WorkspaceBindingKind::LocalFilesystem) =>
        {
            ToolExecutionRouteKind::Unsupported
        }
        ToolExecutionRouteKind::Unsupported => ToolExecutionRouteKind::ServerRuntime,
        runtime_route => runtime_route,
    }
}

fn runtime_executor_route_for_binding(
    tool_name: &str,
    workspace_kind: WorkspaceBindingKind,
    executor_transport: ToolTransportKind,
) -> ToolExecutionRouteKind {
    if matches!(executor_transport, ToolTransportKind::GatewayRelay) {
        return ToolExecutionRouteKind::GatewayRelay;
    }
    if matches!(executor_transport, ToolTransportKind::SandboxResidentAgent) {
        return ToolExecutionRouteKind::SandboxResidentAgent;
    }
    match workspace_kind {
        WorkspaceBindingKind::EdgeWorkspace => return ToolExecutionRouteKind::EdgeBound,
        WorkspaceBindingKind::ServerSandbox if server_local_runtime_tool_supported(tool_name) => {
            return ToolExecutionRouteKind::ServerLocal;
        }
        WorkspaceBindingKind::LocalFilesystem => return ToolExecutionRouteKind::Unsupported,
        WorkspaceBindingKind::CloudWorkspace
        | WorkspaceBindingKind::None
        | WorkspaceBindingKind::Unknown => {}
        WorkspaceBindingKind::ServerSandbox => {}
    }
    ToolExecutionRouteKind::Unsupported
}

pub(crate) fn runtime_binding_can_use_client_ledger(
    workspace_kind: WorkspaceBindingKind,
    executor_transport: ToolTransportKind,
) -> bool {
    matches!(workspace_kind, WorkspaceBindingKind::EdgeWorkspace)
        || matches!(executor_transport, ToolTransportKind::EdgeLedger)
}

pub(crate) fn edge_bound_route_is_offline_for_binding(
    tool_name: &str,
    workspace_kind: WorkspaceBindingKind,
    executor_status: ExecutorStatus,
    executor_transport: ToolTransportKind,
    registry: &astra_runtime_env::ToolRegistry,
) -> bool {
    matches!(
        routing_decision_for_binding(tool_name, workspace_kind, executor_transport, registry),
        ToolExecutionRouteKind::EdgeBound
    ) && matches!(
        executor_status,
        ExecutorStatus::Offline | ExecutorStatus::Unknown
    )
}

pub(crate) fn should_deliver_edge_bound_tools_via_client_ledger_for_binding(
    workspace_kind: WorkspaceBindingKind,
    executor_transport: ToolTransportKind,
    executor_status: ExecutorStatus,
    runtime_executor_available: bool,
    event_channel_available: bool,
) -> bool {
    if !event_channel_available {
        return false;
    }
    if runtime_binding_can_use_client_ledger(workspace_kind, executor_transport)
        && matches!(
            executor_status,
            ExecutorStatus::Offline | ExecutorStatus::Unknown
        )
    {
        return true;
    }
    matches!(executor_transport, ToolTransportKind::EdgeLedger) || !runtime_executor_available
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
            turn_chain_id: "chain-1".to_string(),
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
            },
            workspace_record: None,
            runtime: None,
            runtime_file_transfer: None,
            runtime_file_transfer_required: false,
            runtime_edge_dispatch_authorization: None,
            runtime_edge_dispatch_authorization_required: false,
            selected_offer: None,
            policy: ToolPolicySnapshot::default(),
        }
    }

    fn registry() -> astra_runtime_env::ToolRegistry {
        astra_runtime_env::ToolRegistry::builtins()
    }

    #[test]
    fn turn_pipeline_intercept_is_not_an_executor_owner() {
        assert_eq!(
            tool_execution_class(crate::turn::skill_tool::SKILL_TOOL_NAME, &registry()),
            ToolExecutionClass::TurnPipelineIntercept
        );
        assert_eq!(
            routing_decision_for_binding(
                crate::turn::skill_tool::SKILL_TOOL_NAME,
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeWs,
                &registry(),
            ),
            ToolExecutionRouteKind::Unsupported,
            "pipeline-intercepted tools must be consumed before executor routing"
        );
    }

    #[test]
    fn edge_provider_delivery_policy_lives_in_route_layer() {
        assert!(runtime_binding_can_use_client_ledger(
            WorkspaceBindingKind::EdgeWorkspace,
            ToolTransportKind::ServerLocal,
        ));
        assert!(runtime_binding_can_use_client_ledger(
            WorkspaceBindingKind::ServerSandbox,
            ToolTransportKind::EdgeLedger,
        ));
        assert!(!runtime_binding_can_use_client_ledger(
            WorkspaceBindingKind::ServerSandbox,
            ToolTransportKind::ServerLocal,
        ));

        assert!(edge_bound_route_is_offline_for_binding(
            "read_file",
            WorkspaceBindingKind::EdgeWorkspace,
            ExecutorStatus::Unknown,
            ToolTransportKind::EdgeWs,
            &registry(),
        ));
        assert!(edge_bound_route_is_offline_for_binding(
            "web_search",
            WorkspaceBindingKind::EdgeWorkspace,
            ExecutorStatus::Unknown,
            ToolTransportKind::EdgeWs,
            &registry(),
        ));
        assert!(!edge_bound_route_is_offline_for_binding(
            "memory",
            WorkspaceBindingKind::EdgeWorkspace,
            ExecutorStatus::Unknown,
            ToolTransportKind::EdgeWs,
            &registry(),
        ));

        assert!(
            !should_deliver_edge_bound_tools_via_client_ledger_for_binding(
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
                true,
                false,
            )
        );
        assert!(
            should_deliver_edge_bound_tools_via_client_ledger_for_binding(
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
                true,
                true,
            )
        );
        assert!(
            !should_deliver_edge_bound_tools_via_client_ledger_for_binding(
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
                true,
                true,
            )
        );
        assert!(
            should_deliver_edge_bound_tools_via_client_ledger_for_binding(
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
                false,
                true,
            )
        );
        assert!(
            should_deliver_edge_bound_tools_via_client_ledger_for_binding(
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeLedger,
                ExecutorStatus::Online,
                true,
                true,
            )
        );
    }

    #[test]
    fn edge_executor_label_without_edge_route_does_not_trigger_edge_policy() {
        assert!(!edge_bound_route_is_offline_for_binding(
            "read_file",
            WorkspaceBindingKind::ServerSandbox,
            ExecutorStatus::Offline,
            ToolTransportKind::EdgeWs,
            &registry(),
        ));
        assert!(
            !should_deliver_edge_bound_tools_via_client_ledger_for_binding(
                WorkspaceBindingKind::ServerSandbox,
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
                true,
                true,
            ),
            "edge delivery policy must follow the selected route, not executor labels"
        );
    }

    #[test]
    fn local_filesystem_does_not_route_to_server_local() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::LocalFilesystem,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::Unsupported
        );
    }

    #[test]
    fn server_sandbox_routes_to_server_local() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::ServerSandbox,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::ServerLocal
        );
    }

    #[test]
    fn edge_workspace_routes_to_edge_bound() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::EdgeWorkspace,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::EdgeBound
        );
    }

    #[test]
    fn mcp_prefix_does_not_route_without_mcp_transport() {
        let req = make_request(
            "mcp__foo",
            WorkspaceBindingKind::None,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::Unsupported
        );
    }

    #[test]
    fn mcp_executor_transport_routes_to_request_scoped_mcp() {
        let mut req = make_request(
            "mcp__foo",
            WorkspaceBindingKind::None,
            ToolTransportKind::McpHttp,
        );
        req.executor.kind = ExecutorBindingKind::Mcp;
        req.executor.executor_id = "request-scoped-mcp".to_string();

        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::RequestScopedMcp
        );
    }

    #[test]
    fn mcp_executor_kind_does_not_hijack_non_mcp_tools() {
        for name in ["skill", "not_a_tool"] {
            let mut req = make_request(
                name,
                WorkspaceBindingKind::ServerSandbox,
                ToolTransportKind::McpHttp,
            );
            req.executor.kind = ExecutorBindingKind::Mcp;

            assert_eq!(
                routing_decision(&req, &registry()),
                ToolExecutionRouteKind::Unsupported,
                "{name} must not be routed to MCP only because the executor binding says mcp"
            );
        }
    }

    #[test]
    fn server_owned_tools_ignore_workspace_runtime_transport() {
        let control = make_request(
            "agent",
            WorkspaceBindingKind::EdgeWorkspace,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&control, &registry()),
            ToolExecutionRouteKind::ServerControlPlane
        );

        let service = make_request(
            "memory",
            WorkspaceBindingKind::EdgeWorkspace,
            ToolTransportKind::ServerLocal,
        );
        assert_eq!(
            routing_decision(&service, &registry()),
            ToolExecutionRouteKind::ServerRuntime
        );
    }

    #[test]
    fn shared_network_tools_prefer_current_runtime_executor() {
        struct Case {
            tool: &'static str,
            workspace: WorkspaceBindingKind,
            transport: ToolTransportKind,
            expected: ToolExecutionRouteKind,
        }

        let cases = [
            Case {
                tool: "web_fetch",
                workspace: WorkspaceBindingKind::None,
                transport: ToolTransportKind::ServerLocal,
                expected: ToolExecutionRouteKind::ServerRuntime,
            },
            Case {
                tool: "web_search",
                workspace: WorkspaceBindingKind::None,
                transport: ToolTransportKind::ServerLocal,
                expected: ToolExecutionRouteKind::ServerRuntime,
            },
            Case {
                tool: "web_fetch",
                workspace: WorkspaceBindingKind::EdgeWorkspace,
                transport: ToolTransportKind::EdgeWs,
                expected: ToolExecutionRouteKind::EdgeBound,
            },
            Case {
                tool: "web_search",
                workspace: WorkspaceBindingKind::EdgeWorkspace,
                transport: ToolTransportKind::EdgeWs,
                expected: ToolExecutionRouteKind::EdgeBound,
            },
            Case {
                tool: "web_fetch",
                workspace: WorkspaceBindingKind::ServerSandbox,
                transport: ToolTransportKind::ServerLocal,
                expected: ToolExecutionRouteKind::ServerLocal,
            },
            Case {
                tool: "web_fetch",
                workspace: WorkspaceBindingKind::None,
                transport: ToolTransportKind::GatewayRelay,
                expected: ToolExecutionRouteKind::GatewayRelay,
            },
            Case {
                tool: "web_fetch",
                workspace: WorkspaceBindingKind::None,
                transport: ToolTransportKind::SandboxResidentAgent,
                expected: ToolExecutionRouteKind::SandboxResidentAgent,
            },
        ];

        for case in cases {
            let req = make_request(case.tool, case.workspace, case.transport);
            assert_eq!(
                routing_decision(&req, &registry()),
                case.expected,
                "{} with {:?}/{:?}",
                case.tool,
                case.workspace,
                case.transport
            );
        }
    }

    #[test]
    fn server_only_service_tools_do_not_follow_edge_executor() {
        for name in ["memory", "mo_query", "github"] {
            let req = make_request(
                name,
                WorkspaceBindingKind::EdgeWorkspace,
                ToolTransportKind::EdgeWs,
            );
            assert_eq!(
                routing_decision(&req, &registry()),
                ToolExecutionRouteKind::ServerRuntime,
                "{name} must remain server-service owned"
            );
        }
    }

    #[test]
    fn intercepted_and_unknown_tools_fail_closed_before_server_local() {
        for name in ["skill", "Skill", "discover_skills", "not_a_tool"] {
            let req = make_request(
                name,
                WorkspaceBindingKind::ServerSandbox,
                ToolTransportKind::ServerLocal,
            );
            assert_eq!(
                routing_decision(&req, &registry()),
                ToolExecutionRouteKind::Unsupported,
                "{name} must not reach ServerLocal"
            );
        }
    }

    #[test]
    fn runtime_tools_without_server_adapter_do_not_route_to_server_local() {
        for name in ["lsp", "powershell", "git_clone", "background_shell"] {
            let req = make_request(
                name,
                WorkspaceBindingKind::ServerSandbox,
                ToolTransportKind::ServerLocal,
            );
            assert_eq!(
                routing_decision(&req, &registry()),
                ToolExecutionRouteKind::Unsupported,
                "{name} is a runtime capability but this server binary has no local adapter"
            );
        }
    }

    #[test]
    fn gateway_relay_transport_routes_to_gateway_relay() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::None,
            ToolTransportKind::GatewayRelay,
        );
        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::GatewayRelay
        );
    }

    #[test]
    fn sandbox_resident_agent_transport_routes_correctly() {
        let req = make_request(
            "bash",
            WorkspaceBindingKind::None,
            ToolTransportKind::SandboxResidentAgent,
        );
        assert_eq!(
            routing_decision(&req, &registry()),
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
        assert_eq!(
            routing_decision(&req, &registry()),
            ToolExecutionRouteKind::Unsupported
        );
    }
}
