use std::collections::HashSet;
use std::sync::OnceLock;

use astra_runtime_env::CapacityProvider;
use astra_turn_core::tool::schema::tool_schema_name;
use serde_json::Value;

use crate::server::tool_admission::{
    ToolAdmissionContext, active_provider_declarations_for_binding,
    has_explicit_runtime_executor_provider,
};

use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ExecutorStatus, ToolExecutionRequest, ToolPolicySnapshot,
    ToolTransportKind, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
};

const EDGE_CLIENT_WORKSPACE_SENTINEL_CWD: &str = "__edge_client_provided_workspace__";

pub fn capability_filter_tool_schemas_for_binding(
    schemas: Vec<Value>,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
) -> Vec<Value> {
    capability_filter_tool_schemas_for_binding_with_context(
        schemas,
        workspace,
        executor,
        runtime,
        ToolAdmissionContext::default(),
    )
}

pub(crate) fn capability_filter_tool_schemas_for_binding_with_context(
    schemas: Vec<Value>,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    admission_context: ToolAdmissionContext,
) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let providers = active_provider_declarations_for_binding(
        &schemas,
        workspace,
        executor,
        runtime,
        &registry,
        &admission_context,
    );
    let prompt_schema_conflicts =
        astra_core::tool_schema::prompt_schema_conflicting_tool_names(&schemas);
    let mut seen_mcp_tool_names = HashSet::new();
    schemas
        .into_iter()
        .filter(|schema| {
            let Some(tool_name) = tool_schema_name(schema) else {
                return false;
            };
            if astra_runtime_env::is_mcp_namespaced_tool_name(tool_name) {
                if prompt_schema_conflicts.contains(tool_name) {
                    return false;
                }
                if !seen_mcp_tool_names.insert(tool_name.to_string()) {
                    return false;
                }
            }
            if !providers
                .iter()
                .any(|provider| provider.declares_tool(tool_name))
            {
                return false;
            }
            let admission =
                crate::server::tool_admission::resolve_tool_admission_for_providers_with_context(
                    tool_name,
                    workspace,
                    executor,
                    &providers,
                    &registry,
                    &admission_context,
                );
            if !admission.visible {
                return false;
            }
            if admission.selected_route()
                == super::tool_route_selection::ToolExecutionRouteKind::ServerRuntime
                && registry.get(tool_name).is_some_and(|spec| {
                    matches!(
                        spec.required.executor,
                        astra_runtime_env::RequiredExecutor::ServiceOrRuntimeExecutor
                    )
                })
            {
                return true;
            }
            let binding = runtime_environment_binding_for_parts(
                tool_name,
                workspace,
                executor,
                runtime.cloned(),
                &ToolPolicySnapshot::default(),
                &registry,
            );
            astra_runtime_env::CapabilityResolver
                .check_tool_call(
                    &registry,
                    tool_name,
                    &serde_json::json!({}),
                    &binding.capabilities,
                )
                .is_ok()
        })
        .collect()
}

pub fn capability_filter_edge_provided_tool_schemas_for_binding(
    schemas: Vec<Value>,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
) -> Vec<Value> {
    capability_filter_tool_schemas_for_binding(schemas, workspace, executor, runtime)
}

pub(crate) fn capability_filter_edge_provided_tool_schemas_for_binding_with_context(
    schemas: Vec<Value>,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    admission_context: ToolAdmissionContext,
) -> Vec<Value> {
    capability_filter_tool_schemas_for_binding_with_context(
        schemas,
        workspace,
        executor,
        runtime,
        admission_context,
    )
}

pub fn capability_filtered_server_tool_schemas(
    capabilities: &astra_turn_core::capability::CapabilitySet,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
) -> Vec<Value> {
    capability_filtered_server_tool_schemas_with_context(
        capabilities,
        workspace,
        executor,
        runtime,
        ToolAdmissionContext::default(),
    )
}

pub(crate) fn capability_filtered_server_tool_schemas_with_context(
    capabilities: &astra_turn_core::capability::CapabilitySet,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    admission_context: ToolAdmissionContext,
) -> Vec<Value> {
    let mut pool = crate::capabilities::server_builtin_tool_schemas(capabilities);
    if has_explicit_runtime_executor_provider(workspace, executor, runtime) {
        extend_tool_schema_pool_unique(
            &mut pool,
            crate::capabilities::runtime_executor_tool_schemas(capabilities),
        );
    }
    capability_filter_tool_schemas_for_binding_with_context(
        pool,
        workspace,
        executor,
        runtime,
        admission_context,
    )
}

fn extend_tool_schema_pool_unique(pool: &mut Vec<Value>, extra: Vec<Value>) {
    let mut seen: HashSet<String> = pool
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect();
    for schema in extra {
        if let Some(name) = tool_schema_name(&schema)
            && seen.insert(name.to_string())
        {
            pool.push(schema);
        }
    }
}

impl ToolExecutionRequest {
    pub fn runtime_environment_binding(
        &self,
        registry: &astra_runtime_env::ToolRegistry,
    ) -> astra_runtime_env::RunBinding {
        runtime_environment_binding_for_parts(
            &self.tool_name,
            &self.workspace,
            &self.executor,
            self.runtime.clone(),
            &self.policy,
            registry,
        )
    }
}

pub(crate) fn runtime_environment_binding_for_parts(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<astra_runtime_env::RuntimeBinding>,
    policy: &ToolPolicySnapshot,
    registry: &astra_runtime_env::ToolRegistry,
) -> astra_runtime_env::RunBinding {
    let runtime = runtime.unwrap_or_else(|| runtime_env_runtime_binding(workspace, executor));
    let workspace_binding = runtime_env_workspace_binding(workspace);
    let executor_binding = runtime_env_executor_binding(tool_name, workspace, executor);
    let policy_intent = runtime_env_policy_intent(workspace, policy);
    if is_explicit_request_scoped_mcp_binding(tool_name, executor) {
        let providers = vec![astra_runtime_env::request_scoped_mcp_provider(
            "request-scoped-mcp",
            [tool_name.to_string()],
        )];
        return astra_runtime_env::RunBinding::resolve_with_provider_declarations(
            workspace_binding,
            executor_binding,
            runtime,
            policy_intent,
            registry,
            &providers,
        );
    }
    astra_runtime_env::RunBinding::resolve(
        workspace_binding,
        executor_binding,
        runtime,
        policy_intent,
        registry,
    )
}

fn is_explicit_request_scoped_mcp_binding(tool_name: &str, executor: &ExecutorBinding) -> bool {
    astra_runtime_env::is_mcp_namespaced_tool_name(tool_name)
        && matches!(executor.kind, ExecutorBindingKind::Mcp)
}

fn runtime_env_workspace_binding(
    workspace: &WorkspaceBinding,
) -> astra_runtime_env::WorkspaceBinding {
    // WorkspaceAuthority and WorkspaceBindingKind are now the same type
    // (re-exported from astra_runtime_env).  LocalFilesystem is a
    // client-only variant; map it to Unknown on the server side.
    let authority = workspace.authority;
    let kind = match workspace.kind {
        WorkspaceBindingKind::LocalFilesystem => astra_runtime_env::WorkspaceBindingKind::Unknown,
        other => other,
    };
    astra_runtime_env::WorkspaceBinding {
        kind,
        display_name: workspace.display_name.clone(),
        cwd: runtime_env_workspace_cwd(workspace),
        authority,
        persistent: matches!(
            workspace.kind,
            WorkspaceBindingKind::ServerSandbox
                | WorkspaceBindingKind::EdgeWorkspace
                | WorkspaceBindingKind::CloudWorkspace
        ),
    }
}

fn runtime_env_workspace_cwd(workspace: &WorkspaceBinding) -> Option<String> {
    workspace.cwd.clone().or_else(|| {
        matches!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace)
            .then(|| EDGE_CLIENT_WORKSPACE_SENTINEL_CWD.to_string())
    })
}

fn runtime_env_executor_binding(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> astra_runtime_env::ExecutorBinding {
    let request_scoped_mcp = is_explicit_request_scoped_mcp_binding(tool_name, executor);
    let server_owned_tool =
        is_server_control_plane_tool(tool_name) || is_server_runtime_tool(tool_name);
    let no_workspace_control_plane = matches!(workspace.kind, WorkspaceBindingKind::None)
        && matches!(
            executor.kind,
            ExecutorBindingKind::ServerLocal
                | ExecutorBindingKind::Mcp
                | ExecutorBindingKind::Unknown
        );
    let kind = if request_scoped_mcp {
        astra_runtime_env::ExecutorBindingKind::RequestScopedMcp
    } else if server_owned_tool || no_workspace_control_plane {
        astra_runtime_env::ExecutorBindingKind::ControlPlane
    } else {
        match executor.kind {
            ExecutorBindingKind::ServerLocal => {
                astra_runtime_env::ExecutorBindingKind::ServerRuntime
            }
            ExecutorBindingKind::EdgeAgent => astra_runtime_env::ExecutorBindingKind::EdgeAgent,
            ExecutorBindingKind::OrchestratorManaged => {
                astra_runtime_env::ExecutorBindingKind::OrchestratorManaged
            }
            ExecutorBindingKind::ThinClient => astra_runtime_env::ExecutorBindingKind::ControlPlane,
            ExecutorBindingKind::Mcp => astra_runtime_env::ExecutorBindingKind::ControlPlane,
            ExecutorBindingKind::Unknown => astra_runtime_env::ExecutorBindingKind::Unknown,
        }
    };
    let transport = if request_scoped_mcp {
        astra_runtime_env::ToolTransportKind::McpHttp
    } else if server_owned_tool {
        astra_runtime_env::ToolTransportKind::InProcess
    } else {
        match executor.transport {
            ToolTransportKind::ServerLocal => astra_runtime_env::ToolTransportKind::InProcess,
            ToolTransportKind::EdgeWs => astra_runtime_env::ToolTransportKind::EdgeWebSocket,
            ToolTransportKind::EdgeLedger => astra_runtime_env::ToolTransportKind::EdgeLedger,
            ToolTransportKind::McpHttp => astra_runtime_env::ToolTransportKind::McpHttp,
            ToolTransportKind::GatewayRelay => astra_runtime_env::ToolTransportKind::GatewayRelay,
            ToolTransportKind::SandboxResidentAgent => {
                astra_runtime_env::ToolTransportKind::SandboxResidentAgent
            }
            ToolTransportKind::Unknown => astra_runtime_env::ToolTransportKind::Unknown,
        }
    };
    astra_runtime_env::ExecutorBinding {
        kind,
        executor_id: if request_scoped_mcp {
            "request-scoped-mcp".to_string()
        } else if server_owned_tool {
            "server-runtime".to_string()
        } else {
            executor.executor_id.clone()
        },
        display_name: if request_scoped_mcp {
            "Request-scoped MCP".to_string()
        } else if server_owned_tool {
            "Server runtime".to_string()
        } else {
            executor.display_name.clone()
        },
        transport,
        status: if request_scoped_mcp || server_owned_tool {
            astra_runtime_env::ExecutorStatus::Online
        } else {
            match executor.status {
                ExecutorStatus::Online => astra_runtime_env::ExecutorStatus::Online,
                ExecutorStatus::Offline => astra_runtime_env::ExecutorStatus::Offline,
                ExecutorStatus::Degraded => astra_runtime_env::ExecutorStatus::Degraded,
                ExecutorStatus::Unknown => astra_runtime_env::ExecutorStatus::Unknown,
            }
        },
    }
}

fn runtime_env_runtime_binding(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> astra_runtime_env::RuntimeBinding {
    if matches!(workspace.kind, WorkspaceBindingKind::None)
        || matches!(
            executor.status,
            ExecutorStatus::Offline | ExecutorStatus::Unknown
        )
    {
        return astra_runtime_env::RuntimeBinding::none();
    }

    let (session_manager, isolation_backend) = match executor.kind {
        ExecutorBindingKind::ServerLocal => (
            astra_runtime_env::RuntimeSessionManager::HostProcess,
            astra_runtime_env::RuntimeIsolationBackend::HostProcess,
        ),
        ExecutorBindingKind::EdgeAgent => (
            astra_runtime_env::RuntimeSessionManager::HostProcess,
            astra_runtime_env::RuntimeIsolationBackend::HostProcess,
        ),
        ExecutorBindingKind::OrchestratorManaged => (
            astra_runtime_env::RuntimeSessionManager::ProviderManaged,
            astra_runtime_env::RuntimeIsolationBackend::ProviderManaged,
        ),
        ExecutorBindingKind::ThinClient | ExecutorBindingKind::Mcp => (
            astra_runtime_env::RuntimeSessionManager::None,
            astra_runtime_env::RuntimeIsolationBackend::None,
        ),
        ExecutorBindingKind::Unknown => (
            astra_runtime_env::RuntimeSessionManager::Unknown,
            astra_runtime_env::RuntimeIsolationBackend::Unknown,
        ),
    };
    if matches!(
        session_manager,
        astra_runtime_env::RuntimeSessionManager::None
    ) {
        return astra_runtime_env::RuntimeBinding::none();
    }

    astra_runtime_env::RuntimeBinding {
        session_manager,
        isolation_backend,
        launch_driver: match executor.kind {
            ExecutorBindingKind::ServerLocal => astra_runtime_env::RuntimeLaunchDriver::InProcess,
            ExecutorBindingKind::EdgeAgent => astra_runtime_env::RuntimeLaunchDriver::HostService,
            ExecutorBindingKind::OrchestratorManaged => {
                astra_runtime_env::RuntimeLaunchDriver::Kubernetes
            }
            ExecutorBindingKind::ThinClient => astra_runtime_env::RuntimeLaunchDriver::InProcess,
            _ => astra_runtime_env::RuntimeLaunchDriver::Unknown,
        },
        runtime_id: format!("runtime:{}", executor.executor_id),
        display_name: executor.display_name.clone(),
        status: astra_runtime_env::RuntimeStatus::Ready,
        ephemeral: !matches!(executor.kind, ExecutorBindingKind::EdgeAgent),
        supports_long_sessions: true,
        platform: astra_runtime_env::RuntimePlatform::Unknown,
        interaction_channels: vec![
            astra_runtime_env::RuntimeInteractionChannel::Exec,
            astra_runtime_env::RuntimeInteractionChannel::StdinPipe,
        ],
    }
}

fn runtime_env_policy_intent(
    workspace: &WorkspaceBinding,
    policy: &ToolPolicySnapshot,
) -> astra_runtime_env::PolicyIntent {
    let mut intent = match workspace.authority {
        WorkspaceAuthority::None | WorkspaceAuthority::Unknown => {
            astra_runtime_env::PolicyIntent::cloud_control_plane()
        }
        WorkspaceAuthority::ReadOnly => astra_runtime_env::PolicyIntent::read_only_review(),
        WorkspaceAuthority::ReadWrite => astra_runtime_env::PolicyIntent::local_developer(),
        _ => astra_runtime_env::PolicyIntent::cloud_control_plane(),
    };

    if let Some(network_policy) = policy.network_policy.as_deref().map(str::trim) {
        intent.network = match network_policy {
            "disabled" | "deny" | "none" | "off" => astra_runtime_env::NetworkPolicy::Disabled,
            "open" | "allow_all" | "permissive" => astra_runtime_env::NetworkPolicy::Open,
            _ => intent.network,
        };
    }
    if let Some(secret_policy) = policy.secret_policy.as_deref().map(str::trim) {
        intent.credentials = match secret_policy {
            "disabled" | "deny" | "none" | "off" => astra_runtime_env::CredentialPolicy::Disabled,
            "scoped" | "scoped_injection" => astra_runtime_env::CredentialPolicy::ScopedInjection,
            "approved" | "user_approved" => astra_runtime_env::CredentialPolicy::UserApproved,
            _ => intent.credentials,
        };
    }
    intent.allowed_tools = policy
        .allowed_tools
        .iter()
        .filter_map(|tool| astra_runtime_env::ToolName::new(tool))
        .collect();
    if let Some(max_execution_secs) = policy.max_execution_secs {
        intent.resources.max_execution_secs = max_execution_secs
            .is_finite()
            .then_some(max_execution_secs.max(0.0));
    }
    if let Some(max_output_bytes) = policy.max_output_bytes {
        intent.resources.max_output_bytes = Some(max_output_bytes);
    }
    if let Some(max_background_session_secs) = policy.max_background_session_secs {
        intent.resources.max_background_session_secs = max_background_session_secs
            .is_finite()
            .then_some(max_background_session_secs.max(0.0));
    }
    intent
}

pub(crate) fn is_server_control_plane_tool(tool_name: &str) -> bool {
    builtin_tool_registry().get(tool_name).is_some_and(|spec| {
        matches!(
            spec.required.executor,
            astra_runtime_env::RequiredExecutor::ControlPlane
        )
    })
}

pub(crate) fn is_server_runtime_tool(tool_name: &str) -> bool {
    builtin_tool_registry().get(tool_name).is_some_and(|spec| {
        matches!(
            spec.required.executor,
            astra_runtime_env::RequiredExecutor::ServiceExecutor
        )
    })
}

fn builtin_tool_registry() -> &'static astra_runtime_env::ToolRegistry {
    static REGISTRY: OnceLock<astra_runtime_env::ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(astra_runtime_env::ToolRegistry::builtins)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use serde_json::{Value, json};

    use super::*;

    fn schema(tool_name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": tool_name,
                "description": "test schema",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        })
    }

    fn schema_names(schemas: Vec<Value>) -> HashSet<String> {
        schemas
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect()
    }

    fn no_workspace() -> WorkspaceBinding {
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No file environment".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
        }
    }

    fn mcp_executor() -> ExecutorBinding {
        ExecutorBinding {
            kind: ExecutorBindingKind::Mcp,
            executor_id: "mcp".to_string(),
            display_name: "MCP".to_string(),
            transport: ToolTransportKind::McpHttp,
            status: ExecutorStatus::Online,
        }
    }

    fn edge_workspace() -> WorkspaceBinding {
        WorkspaceBinding {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: "Edge workspace".to_string(),
            cwd: Some("/Users/test/repo".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
        }
    }

    fn edge_executor(executor_id: &str) -> ExecutorBinding {
        ExecutorBinding {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: executor_id.to_string(),
            display_name: "Edge workspace".to_string(),
            transport: ToolTransportKind::EdgeWs,
            status: ExecutorStatus::Online,
        }
    }

    #[test]
    fn filters_project_tools_without_workspace_runtime() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![
                schema("ask_user"),
                schema("skill"),
                schema("tool_search"),
                schema("enter_plan_mode"),
                schema("exit_plan_mode"),
                schema("bash"),
            ],
            &no_workspace(),
            &ExecutorBinding::server_local(),
            None,
        ));

        assert!(names.contains("ask_user"));
        assert!(names.contains("skill"));
        assert!(names.contains("tool_search"));
        assert!(names.contains("enter_plan_mode"));
        assert!(names.contains("exit_plan_mode"));
        assert!(!names.contains("bash"));
    }

    #[test]
    fn filters_malformed_and_unknown_schemas_closed() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![
                schema("ask_user"),
                json!({"function": {}}),
                json!({"type": "custom", "function": {"name": "ask_user"}}),
                json!({"function": {"name": "ask_user"}}),
                json!({"function": {"name": "not_registered"}}),
                json!({"bad": "schema"}),
            ],
            &no_workspace(),
            &ExecutorBinding::server_local(),
            None,
        ));

        assert_eq!(names, HashSet::from(["ask_user".to_string()]));
    }

    #[test]
    fn mcp_schema_is_hidden_without_request_scoped_mcp_provider() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![schema("ask_user"), schema("mcp__weather")],
            &no_workspace(),
            &ExecutorBinding::server_local(),
            None,
        ));

        assert!(names.contains("ask_user"));
        assert!(!names.contains("mcp__weather"));
    }

    #[test]
    fn mcp_executor_provider_declares_request_scoped_mcp_schemas() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![schema("mcp__weather")],
            &no_workspace(),
            &mcp_executor(),
            None,
        ));

        assert_eq!(names, HashSet::from(["mcp__weather".to_string()]));
    }

    #[test]
    fn mcp_executor_fails_closed_for_conflicting_mcp_schemas() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![
                schema("ask_user"),
                json!({
                    "type": "function",
                    "function": {
                        "name": "mcp__weather",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "city": {"type": "string"}
                            }
                        }
                    }
                }),
                json!({
                    "type": "function",
                    "function": {
                        "name": "mcp__weather",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "location": {"type": "string"}
                            }
                        }
                    }
                }),
            ],
            &no_workspace(),
            &mcp_executor(),
            None,
        ));

        assert!(names.contains("ask_user"));
        assert!(
            !names.contains("mcp__weather"),
            "conflicting MCP schemas for one canonical tool must fail closed"
        );
    }

    #[test]
    fn request_scoped_mcp_provider_does_not_hide_server_control_plane_tools() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![
                schema("ask_user"),
                schema("tool_search"),
                schema("enter_plan_mode"),
                schema("mcp__weather"),
            ],
            &no_workspace(),
            &mcp_executor(),
            None,
        ));

        for expected in ["ask_user", "tool_search", "enter_plan_mode", "mcp__weather"] {
            assert!(
                names.contains(expected),
                "{expected} should remain visible when request-scoped MCP is also bound: {names:?}"
            );
        }
    }

    #[test]
    fn non_mcp_tools_keep_server_executor_binding_when_mcp_provider_is_bound() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let binding = runtime_environment_binding_for_parts(
            "ask_user",
            &no_workspace(),
            &mcp_executor(),
            None,
            &ToolPolicySnapshot::default(),
            &registry,
        );

        assert_eq!(
            binding.executor.kind,
            astra_runtime_env::ExecutorBindingKind::ControlPlane
        );
        assert!(
            astra_runtime_env::CapabilityResolver
                .check_tool_call(
                    &registry,
                    "ask_user",
                    &serde_json::json!({}),
                    &binding.capabilities,
                )
                .is_ok(),
            "server/control-plane tools must not be denied only because a request-scoped MCP provider is present"
        );
    }

    #[test]
    fn server_sandbox_binding_exposes_project_runtime_tools() {
        let names = schema_names(capability_filter_tool_schemas_for_binding(
            vec![
                schema("ask_user"),
                schema("read_file"),
                schema("write_file"),
                schema("bash"),
                schema("git"),
            ],
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::server_local(),
            None,
        ));

        for expected in ["ask_user", "read_file", "write_file", "bash", "git"] {
            assert!(
                names.contains(expected),
                "{expected} should be visible for a read-write server sandbox runtime"
            );
        }
    }

    #[test]
    fn server_provider_surface_does_not_start_from_workspace_tools() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &no_workspace(),
            &ExecutorBinding::server_local(),
            None,
        ));

        for expected in ["ask_user", "agent", "tool_search", "web_fetch", "memory"] {
            assert!(
                names.contains(expected),
                "{expected} should be visible from server builtin provider capacity"
            );
        }
        for hidden in [
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "git",
            "run_script",
            "symbols",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must be absent until an explicit runtime provider is bound"
            );
        }
    }

    #[test]
    fn explicit_server_sandbox_provider_adds_workspace_tools() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::server_local(),
            None,
        ));

        for expected in [
            "ask_user",
            "tool_search",
            "web_fetch",
            "read_file",
            "write_file",
            "bash",
            "git",
        ] {
            assert!(
                names.contains(expected),
                "{expected} should be visible for explicit server sandbox capacity"
            );
        }
    }

    #[test]
    fn mismatched_workspace_executor_does_not_expose_runtime_tools() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::edge_agent(
                "edge-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
        ));

        for expected in ["ask_user", "tool_search"] {
            assert!(
                names.contains(expected),
                "{expected} should remain visible from server/control-plane providers"
            );
        }
        for hidden in ["web_fetch", "read_file", "write_file", "bash", "git"] {
            assert!(
                !names.contains(hidden),
                "{hidden} must be invisible when workspace and executor provider ownership disagree"
            );
        }
    }

    #[test]
    fn explicit_offline_runtime_binding_does_not_expose_runtime_tools() {
        let mut runtime = astra_runtime_env::RuntimeBinding::host_process("server-host");
        runtime.status = astra_runtime_env::RuntimeStatus::Offline;

        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::server_local(),
            Some(&runtime),
        ));

        for expected in ["ask_user", "tool_search"] {
            assert!(
                names.contains(expected),
                "{expected} should remain visible from server/control-plane providers"
            );
        }
        for hidden in ["web_fetch", "read_file", "write_file", "bash", "git"] {
            assert!(
                !names.contains(hidden),
                "{hidden} must be invisible when the explicit runtime binding is offline"
            );
        }
    }

    #[test]
    fn server_sandbox_hides_runtime_tools_without_server_local_adapter() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::server_local(),
            None,
        ));

        assert!(names.contains("symbols"));
        for hidden in ["lsp", "powershell"] {
            assert!(
                !names.contains(hidden),
                "{hidden} must not be visible for server-local sandbox without an adapter"
            );
        }
    }

    #[test]
    fn edge_provider_exposes_runtime_tools_without_server_local_adapter() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: Some("/Users/test/repo".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-1".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeWs,
                status: ExecutorStatus::Online,
            },
            None,
        ));

        for expected in ["bash", "read_file", "lsp"] {
            assert!(
                names.contains(expected),
                "{expected} should be visible when the edge provider owns runtime execution"
            );
        }
        for hidden in ["powershell", "display_sixel"] {
            assert!(
                !names.contains(hidden),
                "{hidden} requires an explicit terminal/platform-local provider, not a generic edge workspace"
            );
        }
    }

    #[test]
    fn server_edge_composition_exposes_server_services_and_edge_runtime_tools() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: Some("/Users/test/repo".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-1".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeWs,
                status: ExecutorStatus::Online,
            },
            None,
        ));

        for expected in [
            "ask_user",
            "task",
            "session",
            "tool_search",
            "memory",
            "web_fetch",
            "bash",
            "read_file",
            "write_file",
            "git",
        ] {
            assert!(
                names.contains(expected),
                "{expected} must be visible in the composed server+edge runtime surface: {names:?}"
            );
        }
        for hidden in ["mcp__weather", "not_registered"] {
            assert!(
                !names.contains(hidden),
                "{hidden} must still require an explicit request-scoped provider"
            );
        }
    }

    #[test]
    fn provider_allowlist_filters_prompt_visible_schema_without_server_reroute() {
        let names = schema_names(capability_filter_tool_schemas_for_binding_with_context(
            vec![
                schema("ask_user"),
                schema("memory"),
                schema("web_fetch"),
                schema("bash"),
            ],
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: Some("/Users/test/repo".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-1".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeWs,
                status: ExecutorStatus::Online,
            },
            None,
            ToolAdmissionContext {
                provider_allowed_tools: HashMap::from([
                    (
                        "server-builtin".to_string(),
                        HashSet::from(["memory".to_string()]),
                    ),
                    ("edge-1".to_string(), HashSet::from(["bash".to_string()])),
                ]),
                ..ToolAdmissionContext::default()
            },
        ));

        assert!(names.contains("ask_user"));
        assert!(names.contains("memory"));
        assert!(names.contains("bash"));
        assert!(
            !names.contains("web_fetch"),
            "web_fetch is disallowed on the selected edge offer and must not reroute to server: {names:?}"
        );
    }

    #[test]
    fn shared_network_tool_runtime_binding_follows_selected_executor() {
        let registry = astra_runtime_env::ToolRegistry::builtins();

        let server_only = runtime_environment_binding_for_parts(
            "web_fetch",
            &no_workspace(),
            &ExecutorBinding::server_local(),
            None,
            &ToolPolicySnapshot::default(),
            &registry,
        );
        assert_eq!(
            server_only.executor.kind,
            astra_runtime_env::ExecutorBindingKind::ControlPlane
        );
        assert!(server_only.capabilities.executor.server_service);
        assert!(!server_only.capabilities.executor.runtime_executor);

        let server_sandbox = runtime_environment_binding_for_parts(
            "web_fetch",
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::server_local(),
            None,
            &ToolPolicySnapshot::default(),
            &registry,
        );
        assert_eq!(
            server_sandbox.executor.kind,
            astra_runtime_env::ExecutorBindingKind::ServerRuntime
        );
        assert!(server_sandbox.capabilities.executor.server_service);
        assert!(server_sandbox.capabilities.executor.runtime_executor);

        let edge = runtime_environment_binding_for_parts(
            "web_fetch",
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: Some("/Users/test/repo".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-1".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeWs,
                status: ExecutorStatus::Online,
            },
            None,
            &ToolPolicySnapshot::default(),
            &registry,
        );
        assert_eq!(
            edge.executor.kind,
            astra_runtime_env::ExecutorBindingKind::EdgeAgent
        );
        assert!(!edge.capabilities.executor.server_service);
        assert!(edge.capabilities.executor.runtime_executor);
        assert!(
            astra_runtime_env::CapabilityResolver
                .check_tool_call(
                    &registry,
                    "web_fetch",
                    &serde_json::json!({}),
                    &edge.capabilities,
                )
                .is_ok()
        );
    }

    #[test]
    fn shared_network_tool_schema_is_stable_across_selected_offers() {
        let canonical_schema = schema("web_fetch");
        let server_visible = capability_filter_tool_schemas_for_binding(
            vec![canonical_schema.clone()],
            &no_workspace(),
            &ExecutorBinding::server_local(),
            None,
        );
        let edge_visible = capability_filter_tool_schemas_for_binding(
            vec![canonical_schema.clone()],
            &edge_workspace(),
            &edge_executor("edge-1"),
            None,
        );

        assert_eq!(server_visible, vec![canonical_schema.clone()]);
        assert_eq!(edge_visible, vec![canonical_schema]);
        assert_eq!(
            server_visible, edge_visible,
            "provider/route selection must not mutate prompt-visible canonical schema"
        );
    }

    #[test]
    fn same_tool_schema_stays_stable_while_offer_selection_is_provider_scoped() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let canonical_schema = schema("web_fetch");
        let workspace = edge_workspace();
        let alice = edge_executor("edge-alice");
        let bob = edge_executor("edge-bob");

        let alice_visible = capability_filter_tool_schemas_for_binding(
            vec![canonical_schema.clone()],
            &workspace,
            &alice,
            None,
        );
        let bob_visible = capability_filter_tool_schemas_for_binding(
            vec![canonical_schema.clone()],
            &workspace,
            &bob,
            None,
        );

        assert_eq!(alice_visible, vec![canonical_schema.clone()]);
        assert_eq!(
            alice_visible, bob_visible,
            "provider identity must not mutate prompt-visible canonical schema"
        );

        let alice_admission = crate::server::tool_admission::resolve_tool_admission_for_binding(
            "web_fetch",
            &[canonical_schema.clone()],
            &workspace,
            &alice,
            None,
            &registry,
        );
        let bob_admission = crate::server::tool_admission::resolve_tool_admission_for_binding(
            "web_fetch",
            &[canonical_schema],
            &workspace,
            &bob,
            None,
            &registry,
        );

        assert_eq!(
            alice_admission.selected_offer_id(),
            Some("web_fetch@edge-alice")
        );
        assert_eq!(
            bob_admission.selected_offer_id(),
            Some("web_fetch@edge-bob")
        );
    }

    #[test]
    fn provider_allowlist_is_provider_scoped_not_global_tool_scoped() {
        let canonical_schema = schema("web_fetch");
        let workspace = edge_workspace();
        let alice = edge_executor("edge-alice");
        let bob = edge_executor("edge-bob");
        let context = ToolAdmissionContext {
            provider_allowed_tools: HashMap::from([(
                "edge-alice".to_string(),
                HashSet::from(["bash".to_string()]),
            )]),
            ..ToolAdmissionContext::default()
        };

        let alice_visible = capability_filter_tool_schemas_for_binding_with_context(
            vec![canonical_schema.clone()],
            &workspace,
            &alice,
            None,
            context.clone(),
        );
        let bob_visible = capability_filter_tool_schemas_for_binding_with_context(
            vec![canonical_schema.clone()],
            &workspace,
            &bob,
            None,
            context,
        );

        assert!(
            alice_visible.is_empty(),
            "edge-alice allowlist excludes web_fetch and must hide only that provider's offer"
        );
        assert_eq!(
            bob_visible,
            vec![canonical_schema],
            "edge-bob has no provider allowlist entry and must not inherit edge-alice policy"
        );
    }

    #[test]
    fn production_server_edge_surface_hides_admin_and_platform_local_tools() {
        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::lifecycle_server_capabilities(true, true),
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: Some("/Users/test/repo".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-1".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeWs,
                status: ExecutorStatus::Online,
            },
            None,
        ));

        for expected in [
            "task",
            "session",
            "introspect",
            "reflect",
            "tool_search",
            "bash",
            "read_file",
        ] {
            assert!(
                names.contains(expected),
                "{expected} must be visible in production server+edge surface: {names:?}"
            );
        }
        for hidden in [
            "mo_query",
            "rollback_database_snapshots",
            "powershell",
            "display_sixel",
            "task_output",
            "task_stop",
            "task_list",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must not be visible in ordinary production server+edge surface: {names:?}"
            );
        }
    }

    #[test]
    fn edge_provided_tools_allow_registered_project_tools_without_server_cwd() {
        let names = schema_names(capability_filter_edge_provided_tool_schemas_for_binding(
            vec![
                schema("read_file"),
                schema("write_file"),
                schema("not_registered"),
            ],
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-ledger".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeLedger,
                status: ExecutorStatus::Online,
            },
            None,
        ));

        assert!(names.contains("read_file"));
        assert!(names.contains("write_file"));
        assert!(!names.contains("not_registered"));
    }

    #[test]
    fn edge_workspace_without_server_cwd_projects_runtime_capabilities() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let binding = runtime_environment_binding_for_parts(
            "read_file",
            &WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: "Edge workspace".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::ReadWrite,
            },
            &ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-ledger".to_string(),
                display_name: "Edge workspace".to_string(),
                transport: ToolTransportKind::EdgeLedger,
                status: ExecutorStatus::Online,
            },
            None,
            &ToolPolicySnapshot::default(),
            &registry,
        );

        assert_eq!(
            binding.workspace.cwd.as_deref(),
            Some(EDGE_CLIENT_WORKSPACE_SENTINEL_CWD)
        );
        assert!(binding.capabilities.workspace.readable);
        assert!(binding.capabilities.workspace.writable);
        assert!(
            astra_runtime_env::CapabilityResolver
                .check_tool_call(
                    &registry,
                    "read_file",
                    &serde_json::json!({"path": "src/lib.rs"}),
                    &binding.capabilities,
                )
                .is_ok()
        );
    }

    #[test]
    fn runtime_without_long_session_support_does_not_hide_ready_runtime_provider() {
        let mut runtime = astra_runtime_env::RuntimeBinding::host_process("runtime-short-session");
        runtime.supports_long_sessions = false;

        let names = schema_names(capability_filtered_server_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
            &WorkspaceBinding::server_sandbox("/workspace"),
            &ExecutorBinding::server_local(),
            Some(&runtime),
        ));

        for expected in ["bash", "read_file", "write_file", "git"] {
            assert!(
                names.contains(expected),
                "{expected} must remain visible when the runtime is ready and isolated even if it does not support long sessions"
            );
        }
    }

    #[test]
    fn local_filesystem_maps_to_unknown_in_conversion() {
        // When the server receives a client-only LocalFilesystem workspace
        // kind, it should map to Unknown rather than propagating the variant.
        let workspace = WorkspaceBinding {
            kind: WorkspaceBindingKind::LocalFilesystem,
            display_name: "local".into(),
            cwd: Some("/tmp/test".into()),
            authority: WorkspaceAuthority::ReadWrite,
        };
        let runtime_ws = runtime_env_workspace_binding(&workspace);
        assert_eq!(
            runtime_ws.kind,
            astra_runtime_env::WorkspaceBindingKind::Unknown
        );
    }
}
