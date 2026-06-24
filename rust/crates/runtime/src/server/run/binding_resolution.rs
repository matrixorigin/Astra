//! Binding resolution logic for server-side runs.
//!
//! Extracted from `lifecycle.rs` to keep the run lifecycle module focused on
//! state-machine concerns. This module owns the mapping from explicit API
//! request bindings and persisted metadata to concrete workspace + executor
//! bindings that flow through the agentic loop.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::server::run::engine::RunStartContext;
use crate::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FallbackPolicy,
    ToolTransportKind, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
    binding_event_fields,
};

pub(crate) fn resolve_request_execution_bindings(
    request: &astra_services::runs::ChatRequestData,
    server_workspace: &Path,
) -> (WorkspaceBinding, ExecutorBinding) {
    resolve_request_execution_bindings_from_request(request, Some(server_workspace))
        .expect("server workspace binding resolution should always succeed")
}

pub(crate) fn request_uses_server_workspace(
    request: &astra_services::runs::ChatRequestData,
    has_edge_tools: bool,
) -> bool {
    match request
        .workspace_binding
        .as_ref()
        .map(|binding| binding.kind)
    {
        Some(astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox) => true,
        Some(_) => false,
        None => !has_edge_tools,
    }
}

pub(crate) fn resolve_request_execution_bindings_without_server_workspace(
    request: &astra_services::runs::ChatRequestData,
) -> Option<(WorkspaceBinding, ExecutorBinding)> {
    resolve_request_execution_bindings_from_request(request, None)
}

fn resolve_request_execution_bindings_from_request(
    request: &astra_services::runs::ChatRequestData,
    server_workspace: Option<&Path>,
) -> Option<(WorkspaceBinding, ExecutorBinding)> {
    let workspace = match request.workspace_binding.as_ref() {
        Some(binding) => workspace_binding_from_request(binding, server_workspace)?,
        None => match server_workspace {
            Some(server_workspace) => WorkspaceBinding::server_sandbox(server_workspace),
            None => return None,
        },
    };
    let executor = executor_binding_from_request(request.executor_binding.as_ref(), &workspace);
    Some((workspace, executor))
}

pub(crate) fn execution_bindings_from_metadata(
    metadata: Option<&Value>,
    server_workspace: &Path,
) -> Option<ExecutionBindingSnapshot> {
    let metadata = metadata?.as_object()?;
    let mut workspace: WorkspaceBinding =
        serde_json::from_value(metadata.get("workspace")?.clone()).ok()?;
    if matches!(workspace.kind, WorkspaceBindingKind::ServerSandbox) {
        workspace.cwd = Some(server_workspace.display().to_string());
    }
    let executor: ExecutorBinding =
        serde_json::from_value(metadata.get("executor")?.clone()).ok()?;
    Some(ExecutionBindingSnapshot::inferred(workspace, executor))
}

#[derive(Default)]
pub(crate) struct RunExecutionBindingSnapshot {
    pub workspace: Option<Value>,
    pub executor: Option<Value>,
    pub transport: Option<String>,
    pub fallback_policy: Option<String>,
}

pub(crate) fn agent_working_dir_for_bindings(
    execution_bindings: Option<&ExecutionBindingSnapshot>,
    runtime_workspace: &Path,
) -> PathBuf {
    let Some(snapshot) = execution_bindings else {
        return runtime_workspace.to_path_buf();
    };
    let workspace = &snapshot.workspace;
    if matches!(workspace.kind, WorkspaceBindingKind::ServerSandbox) {
        return runtime_workspace.to_path_buf();
    }
    workspace
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_workspace.to_path_buf())
}

pub(crate) fn binding_snapshot_events(
    run_id: &str,
    session_id: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> [Value; 2] {
    let mut workspace_event = Map::new();
    workspace_event.insert(
        "type".to_string(),
        Value::String("workspace_bound".to_string()),
    );
    workspace_event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    workspace_event.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    for (key, value) in binding_event_fields(workspace, executor) {
        workspace_event.insert(key, value);
    }

    let mut executor_event = Map::new();
    executor_event.insert(
        "type".to_string(),
        Value::String("executor_bound".to_string()),
    );
    executor_event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    executor_event.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    for (key, value) in binding_event_fields(workspace, executor) {
        executor_event.insert(key, value);
    }

    [
        Value::Object(workspace_event),
        Value::Object(executor_event),
    ]
}

pub(crate) fn run_start_context_from_request(
    request: &astra_services::runs::ChatRequestData,
    execution_bindings: Option<&ExecutionBindingSnapshot>,
    agent_binding: Option<&astra_services::AgentBindingRecord>,
) -> RunStartContext {
    RunStartContext {
        interaction_mode: request.interaction_mode,
        interactive_client: Some(request.interactive_client),
        execution_metadata: execution_bindings
            .map(|snapshot| binding_event_fields(&snapshot.workspace, &snapshot.executor)),
        agent_binding_id: agent_binding.map(|binding| binding.id.clone()).or_else(|| {
            request
                .agent_binding
                .as_ref()
                .map(|binding| binding.id.clone())
        }),
        agent_binding_name: agent_binding.map(|binding| binding.binding_name.clone()),
        agent_binding_schema_version: agent_binding
            .map(|binding| binding.binding_schema_version.clone()),
        selected_model: request.selected_model.clone(),
        capability_server_refs: request
            .agent_binding
            .as_ref()
            .map(|binding| binding.capability_server_refs.clone()),
        runtime_profile: effective_runtime_profile(request),
    }
}

fn effective_runtime_profile(
    request: &astra_services::runs::ChatRequestData,
) -> Option<astra_services::runs::RuntimeProfileRequest> {
    if request.agent_binding.is_some() {
        return Some(astra_services::runs::RuntimeProfileRequest::AgentBindingRegistry);
    }
    if !request.runtime_mcp_bindings.is_empty()
        || matches!(
            request.runtime_profile,
            Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp)
        )
    {
        return Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp);
    }
    request.runtime_profile
}

pub(crate) fn executor_binding_from_request(
    binding: Option<&astra_services::runs::ExecutorBindingRequest>,
    workspace: &WorkspaceBinding,
) -> ExecutorBinding {
    match workspace.kind {
        WorkspaceBindingKind::ServerSandbox => {
            let mut executor = ExecutorBinding::server_local();
            if let Some(binding) = binding {
                if let Some(executor_id) = non_empty_string(binding.executor_id.as_deref()) {
                    executor.executor_id = executor_id;
                }
                if let Some(display_name) = non_empty_string(binding.display_name.as_deref()) {
                    executor.display_name = display_name;
                }
                if let Some(status) = binding.status {
                    executor.status = executor_status_from_request(status);
                }
            }
            executor
        }
        WorkspaceBindingKind::EdgeWorkspace => edge_executor_binding_from_request(binding),
        WorkspaceBindingKind::CloudWorkspace => {
            cloud_executor_binding_from_request(binding, &workspace.display_name)
        }
        WorkspaceBindingKind::None => ExecutorBinding::server_control_plane(),
        WorkspaceBindingKind::Unknown | WorkspaceBindingKind::LocalFilesystem => ExecutorBinding {
            kind: ExecutorBindingKind::Unknown,
            executor_id: "unknown".to_string(),
            display_name: "Unknown executor".to_string(),
            transport: ToolTransportKind::Unknown,
            status: ExecutorStatus::Unknown,
        },
    }
}

fn workspace_binding_from_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    server_workspace: Option<&Path>,
) -> Option<WorkspaceBinding> {
    match binding.kind {
        astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox => {
            let mut workspace = WorkspaceBinding::server_sandbox(server_workspace?);
            if let Some(display_name) = non_empty_string(binding.display_name.as_deref()) {
                workspace.display_name = display_name;
            }
            if let Some(authority) = binding.authority {
                workspace.authority = workspace_authority_from_request(authority);
            }
            if let Some(fallback_policy) = binding.fallback_policy {
                workspace.fallback_policy = fallback_policy_from_request(fallback_policy);
            }
            Some(workspace)
        }
        astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace => {
            Some(WorkspaceBinding {
                kind: WorkspaceBindingKind::EdgeWorkspace,
                display_name: non_empty_string(binding.display_name.as_deref())
                    .unwrap_or_else(|| "Edge workspace".to_string()),
                cwd: workspace_request_root(binding),
                authority: binding
                    .authority
                    .map(workspace_authority_from_request)
                    .unwrap_or(WorkspaceAuthority::ReadWrite),
                fallback_policy: binding
                    .fallback_policy
                    .map(fallback_policy_from_request)
                    .unwrap_or(FallbackPolicy::Disabled),
            })
        }
        astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace => {
            Some(WorkspaceBinding {
                kind: WorkspaceBindingKind::CloudWorkspace,
                display_name: non_empty_string(binding.display_name.as_deref())
                    .unwrap_or_else(|| cloud_workspace_display_name(binding).to_string()),
                cwd: workspace_request_root(binding),
                authority: binding
                    .authority
                    .map(workspace_authority_from_request)
                    .unwrap_or_else(|| cloud_workspace_default_authority(binding)),
                fallback_policy: binding
                    .fallback_policy
                    .map(fallback_policy_from_request)
                    .unwrap_or(FallbackPolicy::Disabled),
            })
        }
        astra_services::runs::WorkspaceBindingRequestKind::None => Some(WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: non_empty_string(binding.display_name.as_deref())
                .unwrap_or_else(|| "No workspace".to_string()),
            cwd: None,
            authority: WorkspaceAuthority::None,
            fallback_policy: binding
                .fallback_policy
                .map(fallback_policy_from_request)
                .unwrap_or(FallbackPolicy::Disabled),
        }),
    }
}

fn cloud_workspace_display_name(
    binding: &astra_services::runs::WorkspaceBindingRequest,
) -> &'static str {
    match binding.source.as_ref() {
        Some(astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { .. }) => {
            "Uploaded snapshot"
        }
        Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout { .. }) => "Git checkout",
        Some(astra_services::runs::WorkspaceSourceRequest::Scratch) => "Scratch workspace",
        Some(astra_services::runs::WorkspaceSourceRequest::DatasetBundle { .. }) => {
            "Dataset workspace"
        }
        Some(astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { .. }) => {
            "Artifact workspace"
        }
        Some(astra_services::runs::WorkspaceSourceRequest::Template { .. }) => "Template workspace",
        _ => "Cloud workspace",
    }
}

fn cloud_workspace_default_authority(
    binding: &astra_services::runs::WorkspaceBindingRequest,
) -> WorkspaceAuthority {
    match binding.source.as_ref() {
        Some(astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { .. })
        | Some(astra_services::runs::WorkspaceSourceRequest::DatasetBundle { .. })
        | Some(astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { .. }) => {
            WorkspaceAuthority::ReadOnly
        }
        _ => WorkspaceAuthority::ReadWrite,
    }
}

fn workspace_request_root(
    binding: &astra_services::runs::WorkspaceBindingRequest,
) -> Option<String> {
    non_empty_string(binding.root.as_deref()).or_else(|| match binding.source.as_ref()? {
        astra_services::runs::WorkspaceSourceRequest::EdgePath { path } => {
            non_empty_string(Some(path.as_str()))
        }
        astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { root, .. } => {
            non_empty_string(root.as_deref())
        }
        astra_services::runs::WorkspaceSourceRequest::GitCheckout { .. }
        | astra_services::runs::WorkspaceSourceRequest::Template { .. }
        | astra_services::runs::WorkspaceSourceRequest::DatasetBundle { .. }
        | astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { .. }
        | astra_services::runs::WorkspaceSourceRequest::Scratch
        | astra_services::runs::WorkspaceSourceRequest::PersistentVolume { .. } => None,
    })
}

fn edge_executor_binding_from_request(
    binding: Option<&astra_services::runs::ExecutorBindingRequest>,
) -> ExecutorBinding {
    let mut executor = ExecutorBinding::edge_agent(
        "edge-agent",
        "Edge workspace",
        ToolTransportKind::EdgeWs,
        ExecutorStatus::Unknown,
    );
    if let Some(binding) = binding {
        if let Some(executor_id) = non_empty_string(binding.executor_id.as_deref()) {
            executor.executor_id = executor_id;
        }
        if let Some(display_name) = non_empty_string(binding.display_name.as_deref()) {
            executor.display_name = display_name;
        }
        if let Some(transport) = binding.transport {
            executor.transport = tool_transport_from_request(transport);
        }
        if let Some(status) = binding.status {
            executor.status = executor_status_from_request(status);
        }
    }
    executor
}

fn cloud_executor_binding_from_request(
    binding: Option<&astra_services::runs::ExecutorBindingRequest>,
    workspace_display_name: &str,
) -> ExecutorBinding {
    let mut executor = ExecutorBinding {
        kind: ExecutorBindingKind::OrchestratorManaged,
        executor_id: "orchestrator-managed".to_string(),
        display_name: workspace_display_name.to_string(),
        transport: ToolTransportKind::SandboxResidentAgent,
        status: ExecutorStatus::Online,
    };
    if let Some(binding) = binding {
        if let Some(executor_id) = non_empty_string(binding.executor_id.as_deref()) {
            executor.executor_id = executor_id;
        }
        if let Some(display_name) = non_empty_string(binding.display_name.as_deref()) {
            executor.display_name = display_name;
        }
        if let Some(transport) = binding.transport {
            executor.transport = cloud_tool_transport_from_request(transport);
        }
        if let Some(status) = binding.status {
            executor.status = executor_status_from_request(status);
        }
    }
    executor
}

fn cloud_tool_transport_from_request(
    transport: astra_services::runs::ToolTransportKindRequest,
) -> ToolTransportKind {
    match transport {
        astra_services::runs::ToolTransportKindRequest::GatewayRelay => {
            ToolTransportKind::GatewayRelay
        }
        astra_services::runs::ToolTransportKindRequest::SandboxResidentAgent => {
            ToolTransportKind::SandboxResidentAgent
        }
        _ => ToolTransportKind::SandboxResidentAgent,
    }
}

fn workspace_authority_from_request(
    authority: astra_services::runs::WorkspaceAuthorityRequest,
) -> WorkspaceAuthority {
    match authority {
        astra_services::runs::WorkspaceAuthorityRequest::ReadOnly => WorkspaceAuthority::ReadOnly,
        astra_services::runs::WorkspaceAuthorityRequest::ReadWrite => WorkspaceAuthority::ReadWrite,
        astra_services::runs::WorkspaceAuthorityRequest::None => WorkspaceAuthority::None,
    }
}

fn fallback_policy_from_request(
    fallback_policy: astra_services::runs::FallbackPolicyRequest,
) -> FallbackPolicy {
    match fallback_policy {
        astra_services::runs::FallbackPolicyRequest::Disabled => FallbackPolicy::Disabled,
    }
}

fn tool_transport_from_request(
    transport: astra_services::runs::ToolTransportKindRequest,
) -> ToolTransportKind {
    match transport {
        astra_services::runs::ToolTransportKindRequest::ServerLocal => {
            ToolTransportKind::ServerLocal
        }
        astra_services::runs::ToolTransportKindRequest::EdgeWs => ToolTransportKind::EdgeWs,
        astra_services::runs::ToolTransportKindRequest::EdgeLedger => ToolTransportKind::EdgeLedger,
        astra_services::runs::ToolTransportKindRequest::GatewayRelay => {
            ToolTransportKind::GatewayRelay
        }
        astra_services::runs::ToolTransportKindRequest::SandboxResidentAgent => {
            ToolTransportKind::SandboxResidentAgent
        }
        astra_services::runs::ToolTransportKindRequest::McpHttp => ToolTransportKind::McpHttp,
    }
}

fn executor_status_from_request(
    status: astra_services::runs::ExecutorStatusRequest,
) -> ExecutorStatus {
    match status {
        astra_services::runs::ExecutorStatusRequest::Online => ExecutorStatus::Online,
        astra_services::runs::ExecutorStatusRequest::Offline => ExecutorStatus::Offline,
        astra_services::runs::ExecutorStatusRequest::Degraded => ExecutorStatus::Degraded,
        astra_services::runs::ExecutorStatusRequest::Unknown => ExecutorStatus::Unknown,
    }
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn test_request(message: &str) -> astra_services::runs::ChatRequestData {
        astra_services::runs::ChatRequestData {
            message: message.to_string(),
            parts: Vec::new(),
            attachments: Vec::new(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            selected_model: None,
            agent_binding: None,
            runtime_auth: None,
            runtime_profile: None,
            llm_token_service: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        }
    }

    #[test]
    fn run_start_context_records_effective_agent_binding_profile_when_omitted() {
        let mut request = test_request("hello");
        request.agent_binding = Some(astra_services::runs::AgentBindingRuntimeRequest {
            id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391".to_string(),
            capability_server_refs: astra_services::runs::CapabilityServerRefs {
                mcp: "tools".to_string(),
                skills: "skills".to_string(),
            },
        });

        let context = run_start_context_from_request(&request, None, None);

        assert_eq!(
            context.runtime_profile,
            Some(astra_services::runs::RuntimeProfileRequest::AgentBindingRegistry)
        );
    }

    #[test]
    fn run_start_context_records_effective_request_scoped_profile_when_explicit_mcp_profile() {
        let mut request = test_request("hello");
        request.runtime_mcp_bindings = vec![astra_services::runs::RuntimeMcpBindingRequest {
            id: "request-tools".to_string(),
            transport: "streamable_http".to_string(),
            url: "https://tools.example.test/mcp".to_string(),
            auth_token: None,
            headers: std::collections::HashMap::new(),
        }];
        request.runtime_profile =
            Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp);

        let context = run_start_context_from_request(&request, None, None);

        assert_eq!(
            context.runtime_profile,
            Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp)
        );
    }

    #[test]
    fn fallback_policy_request_maps_all_runtime_variants() {
        assert_eq!(
            fallback_policy_from_request(astra_services::runs::FallbackPolicyRequest::Disabled),
            FallbackPolicy::Disabled
        );
    }

    #[test]
    fn request_uses_server_workspace_only_when_no_edge_workspace_is_available() {
        let request = test_request("hello");

        assert!(request_uses_server_workspace(&request, false));
        assert!(!request_uses_server_workspace(&request, true));
    }

    #[test]
    fn request_execution_bindings_ignore_client_cwd_for_server_sandbox() {
        let mut request = test_request("hello");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: Some("Requested server".to_string()),
            root: Some("/client/claimed/path".to_string()),
            source: None,
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::ServerLocal,
            executor_id: Some("server-local-2".to_string()),
            display_name: Some("Requested executor".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::ServerLocal),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let server_workspace = Path::new("/tmp/astra-runtime-workspace");
        let (workspace, executor) = resolve_request_execution_bindings(&request, server_workspace);

        assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(
            workspace.cwd.as_deref(),
            Some("/tmp/astra-runtime-workspace")
        );
        assert_eq!(workspace.display_name, "Requested server");
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert_eq!(executor.executor_id, "server-local-2");
        assert_eq!(executor.display_name, "Requested executor");
    }

    #[test]
    fn cloud_workspace_executor_binding_ignores_local_executor_spoofing() {
        let mut request = test_request("run in cloud");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Cloud checkout".to_string()),
            root: Some("/cloud/checkouts/run-1".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
                repository: "https://example.com/org/repo.git".to_string(),
                reference: None,
            }),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::ServerLocal,
            executor_id: Some("client-claimed-local".to_string()),
            display_name: Some("Client claimed local executor".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::ServerLocal),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
        assert_eq!(executor.executor_id, "client-claimed-local");
        assert_eq!(executor.display_name, "Client claimed local executor");
        assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn cloud_workspace_executor_binding_allows_gateway_relay_transport() {
        let mut request = test_request("run through provider gateway");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("OpenShell workspace".to_string()),
            root: Some("/sandbox".to_string()),
            source: Some(
                astra_services::runs::WorkspaceSourceRequest::PersistentVolume {
                    volume_id: "openshell-workspace-1".to_string(),
                },
            ),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::OrchestratorManaged,
            executor_id: Some("openshell-gateway".to_string()),
            display_name: Some("OpenShell Gateway".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::GatewayRelay),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (_workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
        assert_eq!(executor.transport, ToolTransportKind::GatewayRelay);
        assert_eq!(executor.executor_id, "openshell-gateway");
    }

    #[test]
    fn request_bindings_without_server_workspace_do_not_infer_from_edge_profile() {
        let request = test_request("hello");

        assert!(resolve_request_execution_bindings_without_server_workspace(&request).is_none());
    }

    #[test]
    fn request_bindings_without_server_workspace_reject_server_sandbox_binding() {
        let mut request = test_request("hello");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: None,
            root: None,
            source: None,
            authority: None,
            fallback_policy: None,
        });

        assert!(resolve_request_execution_bindings_without_server_workspace(&request).is_none());
    }

    #[test]
    fn explicit_edge_workspace_binding_without_server_workspace_binds_edge_executor() {
        let mut request = test_request("use client tools");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: Some("Edge workspace".to_string()),
            root: Some("/repo".to_string()),
            source: None,
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
            executor_id: Some("edge-1".to_string()),
            display_name: Some("Edge workspace".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeLedger),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings_without_server_workspace(&request)
                .expect("explicit edge binding should resolve");

        assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
        assert_eq!(workspace.display_name, "Edge workspace");
        assert_eq!(workspace.cwd.as_deref(), Some("/repo"));
        assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
        assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(executor.executor_id, "edge-1");
        assert_eq!(executor.transport, ToolTransportKind::EdgeLedger);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn metadata_server_sandbox_binding_rewrites_cwd_to_current_workspace() {
        let metadata = json!({
            "workspace": {
                "kind": "server_sandbox",
                "display_name": "Server sandbox",
                "cwd": "/stale/workspace",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "server_local",
                "executor_id": "server-local",
                "display_name": "Server sandbox",
                "transport": "server_local",
                "status": "online"
            }
        });

        let snapshot =
            execution_bindings_from_metadata(Some(&metadata), Path::new("/current/workspace"))
                .expect("metadata should resolve");
        let workspace = &snapshot.workspace;
        let executor = &snapshot.executor;

        assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(workspace.cwd.as_deref(), Some("/current/workspace"));
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert!(snapshot.runtime.is_none());
    }
}
