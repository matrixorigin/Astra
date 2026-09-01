//! Binding resolution logic for server-side runs.
//!
//! Extracted from `lifecycle.rs` to keep the run lifecycle module focused on
//! state-machine concerns. This module owns the mapping from API requests
//! (and edge profiles / persisted metadata) to concrete workspace + executor
//! bindings that flow through the agentic loop.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::server::run::engine::RunStartContext;
use crate::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, ExecutorBindingKind, ExecutorStatus,
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
) -> bool {
    match request
        .workspace_binding
        .as_ref()
        .map(|binding| binding.kind)
    {
        Some(astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox) => true,
        Some(_) | None => false,
    }
}

/// MOI runner chat supplies `allow_tools` with executor_binding.transport=edge_ws.
/// ServerToolExecutor still needs an internal scratch workspace even though tool
/// execution is routed to the connected edge agent.
#[allow(dead_code)]
pub(crate) fn request_needs_edge_bound_server_executor(
    request: &astra_services::runs::ChatRequestData,
    execution_bindings: Option<&ExecutionBindingSnapshot>,
) -> bool {
    let has_allow_tools = request
        .allow_tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty());
    if !has_allow_tools {
        return false;
    }
    execution_bindings.is_some_and(|snapshot| {
        matches!(snapshot.executor.kind, ExecutorBindingKind::EdgeAgent)
            && matches!(snapshot.executor.transport, ToolTransportKind::EdgeWs)
    })
}

pub(crate) fn resolve_request_execution_bindings_without_server_workspace(
    request: &astra_services::runs::ChatRequestData,
    _edge_profile: &Map<String, Value>,
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
            Some(_) => WorkspaceBinding::none(),
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
    execution_bindings_from_metadata_with_authority(metadata, server_workspace, None)
}

/// Reconstruct a child binding from the parent's durable metadata while
/// enforcing the child's typed workspace boundary.  A read-only child must
/// never inherit a read-write workspace authority merely because its parent
/// had one; doing so widens provider policy and turns harmless inspection
/// commands into explicit approval requests.
pub(crate) fn execution_bindings_from_metadata_with_authority(
    metadata: Option<&Value>,
    server_workspace: &Path,
    authority_override: Option<WorkspaceAuthority>,
) -> Option<ExecutionBindingSnapshot> {
    let metadata = metadata?.as_object()?;
    let mut workspace: WorkspaceBinding =
        serde_json::from_value(metadata.get("workspace")?.clone()).ok()?;
    if matches!(workspace.kind, WorkspaceBindingKind::ServerSandbox) {
        workspace.cwd = Some(server_workspace.display().to_string());
    }
    if let Some(authority) = authority_override {
        workspace.authority = authority;
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
    agent_bindings: Option<&[astra_services::AgentBindingRecord]>,
) -> RunStartContext {
    let resolved_primary_binding = agent_bindings.and_then(|bindings| bindings.last());
    let requested_primary_binding = request
        .agent_bindings
        .last()
        .or(request.agent_binding.as_ref());
    let agent_binding_ids = agent_bindings
        .filter(|bindings| !bindings.is_empty())
        .map(|bindings| bindings.iter().map(|binding| binding.id.clone()).collect())
        .unwrap_or_else(|| {
            request
                .agent_bindings
                .iter()
                .map(|binding| binding.id.clone())
                .chain(
                    request
                        .agent_binding
                        .iter()
                        .map(|binding| binding.id.clone()),
                )
                .collect()
        });
    RunStartContext {
        interaction_mode: super::engine::effective_requested_interaction_mode(
            request.interaction_mode,
            request.interactive_client,
        ),
        interactive_client: Some(request.interactive_client),
        turn_intent_policy: request.execution_policy.turn_intent,
        skill_auto_route_policy: request.execution_policy.skill_auto_route,
        execution_metadata: execution_bindings
            .map(|snapshot| binding_event_fields(&snapshot.workspace, &snapshot.executor)),
        agent_binding_ids,
        agent_binding_id: resolved_primary_binding
            .map(|binding| binding.id.clone())
            .or_else(|| requested_primary_binding.map(|binding| binding.id.clone())),
        agent_binding_name: resolved_primary_binding.map(|binding| binding.binding_name.clone()),
        agent_binding_schema_version: resolved_primary_binding
            .map(|binding| binding.binding_schema_version.clone()),
        model_selection: request.model_selection.clone(),
        resolved_model_selection: request.resolved_model_selection.clone(),
        capability_server_refs: requested_primary_binding
            .map(|binding| binding.capability_server_refs.clone()),
        runtime_profile: effective_runtime_profile(request),
        provider_request_fingerprint: None,
        provider_run_owner: request.provider_run_owner.clone(),
        start_request_fingerprint: None,
        work_binding: None,
        validated_work_item_assignment: false,
    }
}

fn effective_runtime_profile(
    request: &astra_services::runs::ChatRequestData,
) -> Option<astra_services::runs::RuntimeProfileRequest> {
    if request.has_agent_binding_runtime() {
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
            // When the caller explicitly requests an edge agent executor (e.g. a sandbox
            // connected via WebSocket), honour that kind instead of forcing server_local.
            if matches!(
                binding,
                Some(b) if b.kind == astra_services::runs::ExecutorBindingRequestKind::EdgeAgent
            ) {
                return edge_executor_binding_from_request(binding);
            }
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
            })
        }
        astra_services::runs::WorkspaceBindingRequestKind::None => {
            let mut workspace = WorkspaceBinding::none();
            if let Some(display_name) = non_empty_string(binding.display_name.as_deref()) {
                workspace.display_name = display_name;
            }
            Some(workspace)
        }
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

fn tool_transport_from_request(
    transport: astra_services::runs::ToolTransportKindRequest,
) -> ToolTransportKind {
    match transport {
        astra_services::runs::ToolTransportKindRequest::ServerLocal => {
            ToolTransportKind::ServerLocal
        }
        astra_services::runs::ToolTransportKindRequest::EdgeWs
        | astra_services::runs::ToolTransportKindRequest::EdgeWsAuthorized => {
            ToolTransportKind::EdgeWs
        }
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
            conversation_authority: None,
            user_intent: None,
            parts: Vec::new(),
            attachments: Vec::new(),
            stable_runtime_system_prompt: None,
            runtime_system_prompt: None,
            session_id: None,
            work_binding: None,
            run_start_idempotency: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            model_selection_mode: astra_services::runs::ModelSelectionMode::ExplicitOffering,
            model_selection: None,
            resolved_model_selection: None,
            admitted_model_execution: None,
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_bindings: Vec::new(),
            agent_binding: None,
            runtime_auth: None,
            runtime_skill_binding: None,
            runtime_profile: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            enabled_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            execution_time_budget: None,
            execution_policy: Default::default(),
            explain: false,
            interaction_mode: None,
            interactive_client: false,
            provider_run_owner: None,
            provider_workspace_id: None,
            agent_binding_owner_scope: None,
        }
    }

    #[test]
    fn run_start_context_records_effective_agent_binding_profile_when_omitted() {
        let mut request = test_request("hello");
        request.agent_binding = Some(astra_services::runs::AgentBindingRuntimeRequest {
            id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391".to_string(),
            capability_server_refs: astra_services::runs::CapabilityServerRefs {
                mcp: "mcp-main".to_string(),
                skills: "skills-main".to_string(),
            },
        });

        let context = run_start_context_from_request(&request, None, None);

        assert_eq!(
            context.runtime_profile,
            Some(astra_services::runs::RuntimeProfileRequest::AgentBindingRegistry)
        );
    }

    #[test]
    fn run_start_context_records_skill_auto_route_policy() {
        let mut request = test_request("hello");
        request.execution_policy.skill_auto_route =
            astra_services::runs::SkillAutoRouteExecutionPolicy::Disabled;

        let context = run_start_context_from_request(&request, None, None);

        assert_eq!(
            context.skill_auto_route_policy,
            astra_services::runs::SkillAutoRouteExecutionPolicy::Disabled
        );
    }

    #[test]
    fn run_start_context_records_ordered_binding_set_and_extension_primary() {
        let mut request = test_request("hello");
        request.agent_bindings = vec![
            astra_services::runs::AgentBindingRuntimeRequest {
                id: "binding-foundation".to_string(),
                capability_server_refs: astra_services::runs::CapabilityServerRefs {
                    mcp: "mcp-main".to_string(),
                    skills: "skills-main".to_string(),
                },
            },
            astra_services::runs::AgentBindingRuntimeRequest {
                id: "binding-extension".to_string(),
                capability_server_refs: astra_services::runs::CapabilityServerRefs {
                    mcp: "mcp-main".to_string(),
                    skills: "skills-main".to_string(),
                },
            },
        ];

        let context = run_start_context_from_request(&request, None, None);

        assert_eq!(
            context.agent_binding_ids,
            vec!["binding-foundation", "binding-extension"]
        );
        assert_eq!(
            context.agent_binding_id.as_deref(),
            Some("binding-extension")
        );
    }

    #[test]
    fn run_start_context_normalizes_effective_interaction_mode_at_request_boundary() {
        let mut request = test_request("hello");
        let headless = run_start_context_from_request(&request, None, None);
        assert_eq!(
            headless.interaction_mode,
            astra_services::runs::RequestedTurnInteractionMode::Headless
        );

        request.interactive_client = true;
        let prompt = run_start_context_from_request(&request, None, None);
        assert_eq!(
            prompt.interaction_mode,
            astra_services::runs::RequestedTurnInteractionMode::Prompt
        );

        request.interaction_mode = Some(astra_services::runs::RequestedTurnInteractionMode::Deny);
        let explicit = run_start_context_from_request(&request, None, None);
        assert_eq!(
            explicit.interaction_mode,
            astra_services::runs::RequestedTurnInteractionMode::Deny
        );
    }

    #[test]
    fn run_start_context_records_effective_request_scoped_profile_when_implicit_mcp_allowed() {
        let mut request = test_request("hello");
        request.runtime_mcp_bindings = vec![astra_services::runs::RuntimeMcpBindingRequest {
            id: "request-tools".to_string(),
            transport: "streamable_http".to_string(),
            url: "https://tools.example.test/mcp".to_string(),
            auth_token: None,
            headers: std::collections::HashMap::new(),
        }];

        let context = run_start_context_from_request(&request, None, None);

        assert_eq!(
            context.runtime_profile,
            Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp)
        );
    }

    #[test]
    fn request_uses_server_workspace_only_for_explicit_server_sandbox() {
        let request = test_request("hello");

        assert!(!request_uses_server_workspace(&request));
    }

    #[test]
    fn default_request_uses_no_file_environment_even_when_server_workspace_exists() {
        let request = test_request("web-only control plane");

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::None);
        assert_eq!(workspace.display_name, "No file environment");
        assert_eq!(workspace.cwd, None);
        assert_eq!(workspace.authority, WorkspaceAuthority::None);
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert_eq!(executor.executor_id, "server-control-plane");
        assert_eq!(executor.display_name, "Server control plane");
        assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
        assert_eq!(executor.status, ExecutorStatus::Online);
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
    fn request_bindings_without_server_workspace_require_typed_binding() {
        let mut edge_profile = Map::new();
        edge_profile.insert("cwd".to_string(), Value::String("/repo".to_string()));
        edge_profile.insert(
            "edge_agent_id".to_string(),
            Value::String("edge-1".to_string()),
        );
        edge_profile.insert("hostname".to_string(), Value::String("devbox".to_string()));
        let request = test_request("hello");

        assert!(
            resolve_request_execution_bindings_without_server_workspace(&request, &edge_profile)
                .is_none()
        );
    }

    #[test]
    fn request_edge_identity_without_typed_workspace_is_rejected() {
        let edge_profile = serde_json::json!({
            "cwd": "/repo",
            "hostname": "devbox"
        });
        let mut request = test_request("hello");
        request.edge_executor_id = Some("edge-process-42".to_string());

        assert!(
            resolve_request_execution_bindings_without_server_workspace(
                &request,
                edge_profile.as_object().unwrap(),
            )
            .is_none()
        );
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
        });

        assert!(
            resolve_request_execution_bindings_without_server_workspace(&request, &Map::new())
                .is_none()
        );
    }

    #[test]
    fn edge_tools_without_profile_do_not_create_provider_binding() {
        let request = test_request("use client tools");

        let (workspace, executor) =
            resolve_request_execution_bindings_without_server_workspace(&request, &Map::new())
                .expect("missing profile should resolve to no-file control-plane bindings");

        assert_eq!(workspace.kind, WorkspaceBindingKind::None);
        assert_eq!(workspace.display_name, "No file environment");
        assert_eq!(workspace.cwd, None);
        assert_eq!(workspace.authority, WorkspaceAuthority::None);
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert_eq!(executor.executor_id, "server-control-plane");
        assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn metadata_server_sandbox_binding_rewrites_cwd_to_current_workspace() {
        let metadata = json!({
            "workspace": {
                "kind": "server_sandbox",
                "display_name": "Server sandbox",
                "cwd": "/stale/workspace",
                "authority": "read_write"
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

    #[test]
    fn child_authority_override_narrows_inherited_read_write_binding() {
        let metadata = json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "Edge workspace",
                "cwd": "/workspace/project",
                "authority": "read_write"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-agent",
                "display_name": "Edge workspace",
                "transport": "edge_ledger",
                "status": "online"
            }
        });

        let snapshot = execution_bindings_from_metadata_with_authority(
            Some(&metadata),
            Path::new("/current/workspace"),
            Some(WorkspaceAuthority::ReadOnly),
        )
        .expect("metadata should resolve");
        assert_eq!(snapshot.workspace.authority, WorkspaceAuthority::ReadOnly);
        assert_eq!(snapshot.executor.kind, ExecutorBindingKind::EdgeAgent);
    }
}
