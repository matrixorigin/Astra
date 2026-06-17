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
    ExecutionBindingSnapshot, ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FallbackPolicy,
    ToolTransportKind, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
    binding_event_fields,
};

pub(crate) fn resolve_request_execution_bindings(
    request: &astra_services::runs::ChatRequestData,
    server_workspace: &Path,
) -> (WorkspaceBinding, ExecutorBinding) {
    resolve_request_execution_bindings_from_request(request, Some(server_workspace), None)
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
    edge_profile: &Map<String, Value>,
) -> Option<(WorkspaceBinding, ExecutorBinding)> {
    resolve_request_execution_bindings_from_request(request, None, Some(edge_profile))
}

fn resolve_request_execution_bindings_from_request(
    request: &astra_services::runs::ChatRequestData,
    server_workspace: Option<&Path>,
    edge_profile: Option<&Map<String, Value>>,
) -> Option<(WorkspaceBinding, ExecutorBinding)> {
    let workspace = match request.workspace_binding.as_ref() {
        Some(binding) => workspace_binding_from_request(binding, server_workspace)?,
        None => match server_workspace {
            Some(server_workspace) => WorkspaceBinding::server_sandbox(server_workspace),
            None => return edge_profile.map(execution_bindings_from_edge_profile),
        },
    };
    let executor = executor_binding_from_request(request.executor_binding.as_ref(), &workspace);
    Some((workspace, executor))
}

pub(crate) fn execution_bindings_from_edge_profile(
    edge_profile: &Map<String, Value>,
) -> (WorkspaceBinding, ExecutorBinding) {
    let cwd = first_non_empty_profile_string(edge_profile, &["cwd", "git_root"]);
    let executor_id =
        first_non_empty_profile_string(edge_profile, &["edge_agent_id", "agent_id", "edge_id"])
            .unwrap_or_else(|| "edge-profile".to_string());
    let display_name = first_non_empty_profile_string(
        edge_profile,
        &["display_name", "hostname", "edge_agent_id", "agent_id"],
    )
    .unwrap_or_else(|| {
        if cwd.is_some() {
            "Edge workspace".to_string()
        } else {
            "No workspace".to_string()
        }
    });

    let workspace = if let Some(cwd) = cwd {
        WorkspaceBinding {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: display_name.clone(),
            cwd: Some(cwd),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        }
    } else {
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No workspace".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
            fallback_policy: FallbackPolicy::Disabled,
        }
    };
    let executor = match workspace.kind {
        WorkspaceBindingKind::EdgeWorkspace => ExecutorBinding::edge_agent(
            executor_id,
            display_name,
            ToolTransportKind::EdgeLedger,
            ExecutorStatus::Unknown,
        ),
        WorkspaceBindingKind::None => ExecutorBinding::server_control_plane(),
        WorkspaceBindingKind::ServerSandbox
        | WorkspaceBindingKind::CloudWorkspace
        | WorkspaceBindingKind::Unknown
        | WorkspaceBindingKind::LocalFilesystem => ExecutorBinding {
            kind: ExecutorBindingKind::Unknown,
            executor_id: "unknown".to_string(),
            display_name: "Unknown executor".to_string(),
            transport: ToolTransportKind::Unknown,
            status: ExecutorStatus::Unknown,
        },
    };
    (workspace, executor)
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
) -> RunStartContext {
    RunStartContext {
        interaction_mode: request.interaction_mode,
        interactive_client: Some(request.interactive_client),
        execution_metadata: execution_bindings
            .map(|snapshot| binding_event_fields(&snapshot.workspace, &snapshot.executor)),
    }
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
            hosted_executor_binding_from_request(binding, &workspace.display_name)
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

fn hosted_executor_binding_from_request(
    binding: Option<&astra_services::runs::ExecutorBindingRequest>,
    workspace_display_name: &str,
) -> ExecutorBinding {
    let mut executor = ExecutorBinding {
        kind: ExecutorBindingKind::HostedRunner,
        executor_id: "hosted-runner".to_string(),
        display_name: workspace_display_name.to_string(),
        transport: ToolTransportKind::RunnerRpc,
        status: ExecutorStatus::Unknown,
    };
    if let Some(binding) = binding {
        executor.kind = executor_kind_from_request(binding.kind);
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

fn first_non_empty_profile_string(
    edge_profile: &Map<String, Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        edge_profile
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
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

fn executor_kind_from_request(
    kind: astra_services::runs::ExecutorBindingRequestKind,
) -> ExecutorBindingKind {
    match kind {
        astra_services::runs::ExecutorBindingRequestKind::ServerLocal => {
            ExecutorBindingKind::ServerLocal
        }
        astra_services::runs::ExecutorBindingRequestKind::EdgeAgent => {
            ExecutorBindingKind::EdgeAgent
        }
        astra_services::runs::ExecutorBindingRequestKind::ThinClient => {
            ExecutorBindingKind::ThinClient
        }
        astra_services::runs::ExecutorBindingRequestKind::Mcp => ExecutorBindingKind::Mcp,
        astra_services::runs::ExecutorBindingRequestKind::PersonalRunner => {
            ExecutorBindingKind::PersonalRunner
        }
        astra_services::runs::ExecutorBindingRequestKind::HostedRunner => {
            ExecutorBindingKind::HostedRunner
        }
        astra_services::runs::ExecutorBindingRequestKind::EnterpriseRunner => {
            ExecutorBindingKind::EnterpriseRunner
        }
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
        astra_services::runs::ToolTransportKindRequest::McpHttp => ToolTransportKind::McpHttp,
        astra_services::runs::ToolTransportKindRequest::RunnerRpc => ToolTransportKind::RunnerRpc,
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
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: None,
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        }
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
    fn request_bindings_without_server_workspace_use_edge_profile_for_default_request() {
        let mut edge_profile = Map::new();
        edge_profile.insert("cwd".to_string(), Value::String("/repo".to_string()));
        edge_profile.insert(
            "edge_agent_id".to_string(),
            Value::String("edge-1".to_string()),
        );
        edge_profile.insert("hostname".to_string(), Value::String("devbox".to_string()));
        let request = test_request("hello");

        let (workspace, executor) =
            resolve_request_execution_bindings_without_server_workspace(&request, &edge_profile)
                .expect("edge profile should resolve");

        assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
        assert_eq!(workspace.cwd.as_deref(), Some("/repo"));
        assert_eq!(workspace.display_name, "devbox");
        assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(executor.executor_id, "edge-1");
        assert_eq!(executor.transport, ToolTransportKind::EdgeLedger);
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

        assert!(
            resolve_request_execution_bindings_without_server_workspace(&request, &Map::new())
                .is_none()
        );
    }

    #[test]
    fn request_execution_bindings_preserve_hosted_runner_for_git_source() {
        let mut request = test_request("review checkout");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Cloud checkout".to_string()),
            root: Some("/workspace/repo".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
                repository: "https://example.com/org/repo.git".to_string(),
                reference: None,
            }),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadOnly),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::HostedRunner,
            executor_id: Some("runner-1".to_string()),
            display_name: Some("Hosted runner".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::RunnerRpc),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(workspace.cwd.as_deref(), Some("/workspace/repo"));
        assert_eq!(workspace.authority, WorkspaceAuthority::ReadOnly);
        assert_eq!(executor.kind, ExecutorBindingKind::HostedRunner);
        assert_eq!(executor.executor_id, "runner-1");
        assert_eq!(executor.transport, ToolTransportKind::RunnerRpc);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn request_execution_bindings_preserve_hosted_runner_for_cloud_workspace() {
        let mut request = test_request("work in cloud volume");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Team workspace".to_string()),
            root: Some("/workspace/team".to_string()),
            source: Some(
                astra_services::runs::WorkspaceSourceRequest::PersistentVolume {
                    volume_id: "team-volume-1".to_string(),
                },
            ),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::HostedRunner,
            executor_id: Some("runner-1".to_string()),
            display_name: Some("Hosted runner".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::RunnerRpc),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(workspace.cwd.as_deref(), Some("/workspace/team"));
        assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
        assert_eq!(executor.kind, ExecutorBindingKind::HostedRunner);
        assert_eq!(executor.executor_id, "runner-1");
        assert_eq!(executor.transport, ToolTransportKind::RunnerRpc);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn request_execution_bindings_preserve_personal_runner_kind() {
        let mut request = test_request("work through personal runner");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Personal workspace".to_string()),
            root: Some("/workspace/personal".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::Scratch),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadOnly),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::PersonalRunner,
            executor_id: Some("personal-runner-1".to_string()),
            display_name: Some("Personal runner".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::RunnerRpc),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(workspace.cwd.as_deref(), Some("/workspace/personal"));
        assert_eq!(executor.kind, ExecutorBindingKind::PersonalRunner);
        assert_eq!(executor.executor_id, "personal-runner-1");
        assert_eq!(executor.transport, ToolTransportKind::RunnerRpc);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn request_execution_bindings_preserve_enterprise_runner_kind() {
        let mut request = test_request("work through enterprise runner");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Team workspace".to_string()),
            root: Some("/workspace/team".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::Scratch),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::EnterpriseRunner,
            executor_id: Some("enterprise-runner-1".to_string()),
            display_name: Some("Enterprise runner".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::RunnerRpc),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(workspace.cwd.as_deref(), Some("/workspace/team"));
        assert_eq!(executor.kind, ExecutorBindingKind::EnterpriseRunner);
        assert_eq!(executor.executor_id, "enterprise-runner-1");
        assert_eq!(executor.transport, ToolTransportKind::RunnerRpc);
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
