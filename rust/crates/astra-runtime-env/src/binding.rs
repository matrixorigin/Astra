use serde::{Deserialize, Serialize};

use crate::{
    AvailableToolSurface, CapabilityResolver, EffectiveCapabilitySet, PolicyIntent, ToolDenial,
    ToolRegistry, ToolUnavailableReason,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBindingKind {
    None,
    LocalFilesystem,
    EdgeWorkspace,
    ServerSandbox,
    CloudWorkspace,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkspaceAuthority {
    None,
    ReadOnly,
    ReadWrite,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub kind: WorkspaceBindingKind,
    pub display_name: String,
    pub cwd: Option<String>,
    pub authority: WorkspaceAuthority,
    pub persistent: bool,
}

impl WorkspaceBinding {
    pub fn none() -> Self {
        Self {
            kind: WorkspaceBindingKind::None,
            display_name: "No workspace".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
            persistent: false,
        }
    }

    pub fn local_filesystem(cwd: impl Into<String>, authority: WorkspaceAuthority) -> Self {
        Self {
            kind: WorkspaceBindingKind::LocalFilesystem,
            display_name: "Local workspace".to_string(),
            cwd: Some(cwd.into()),
            authority,
            persistent: true,
        }
    }

    pub fn edge_workspace(cwd: impl Into<String>, authority: WorkspaceAuthority) -> Self {
        Self {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: "Edge workspace".to_string(),
            cwd: Some(cwd.into()),
            authority,
            persistent: true,
        }
    }

    pub fn cloud_workspace(root: impl Into<String>, authority: WorkspaceAuthority) -> Self {
        Self {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Cloud workspace".to_string(),
            cwd: Some(root.into()),
            authority,
            persistent: true,
        }
    }

    pub fn server_sandbox(root: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceBindingKind::ServerSandbox,
            display_name: "Server sandbox".to_string(),
            cwd: Some(root.into()),
            authority: WorkspaceAuthority::ReadWrite,
            persistent: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutorBindingKind {
    None,
    ControlPlane,
    ServerRuntime,
    LocalCli,
    PersonalRunner,
    EdgeAgent,
    HostedRunner,
    EnterpriseRunner,
    RequestScopedMcp,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolTransportKind {
    None,
    InProcess,
    EdgeWebSocket,
    EdgeLedger,
    RunnerRpc,
    GatewayRelay,
    SandboxResidentAgent,
    McpHttp,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorStatus {
    Online,
    Offline,
    Degraded,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutorBinding {
    pub kind: ExecutorBindingKind,
    pub executor_id: String,
    pub display_name: String,
    pub transport: ToolTransportKind,
    pub status: ExecutorStatus,
}

impl ExecutorBinding {
    pub fn none() -> Self {
        Self {
            kind: ExecutorBindingKind::None,
            executor_id: "none".to_string(),
            display_name: "No executor".to_string(),
            transport: ToolTransportKind::None,
            status: ExecutorStatus::Offline,
        }
    }

    pub fn control_plane() -> Self {
        Self {
            kind: ExecutorBindingKind::ControlPlane,
            executor_id: "control-plane".to_string(),
            display_name: "Control plane".to_string(),
            transport: ToolTransportKind::InProcess,
            status: ExecutorStatus::Online,
        }
    }

    pub fn local_cli() -> Self {
        Self {
            kind: ExecutorBindingKind::LocalCli,
            executor_id: "local-cli".to_string(),
            display_name: "Local CLI".to_string(),
            transport: ToolTransportKind::InProcess,
            status: ExecutorStatus::Online,
        }
    }

    pub fn personal_runner(executor_id: impl Into<String>) -> Self {
        Self {
            kind: ExecutorBindingKind::PersonalRunner,
            executor_id: executor_id.into(),
            display_name: "Personal runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        }
    }

    pub fn edge_agent(executor_id: impl Into<String>) -> Self {
        Self {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: executor_id.into(),
            display_name: "Edge agent".to_string(),
            transport: ToolTransportKind::EdgeWebSocket,
            status: ExecutorStatus::Online,
        }
    }

    pub fn hosted_runner(executor_id: impl Into<String>) -> Self {
        Self {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: executor_id.into(),
            display_name: "Hosted runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        }
    }

    /// Returns `true` when this executor is an edge agent — a remote
    /// process running on a developer's machine that connects via WebSocket.
    pub fn is_edge_agent(&self) -> bool {
        matches!(self.kind, ExecutorBindingKind::EdgeAgent)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSessionManager {
    None,
    HostProcess,
    AstraManaged,
    NvidiaOpenShell,
    MicrosoftMxc,
    ProviderManaged,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeIsolationBackend {
    None,
    HostProcess,
    LinuxProcessIsolation,
    OciRuntime,
    GVisorRunsc,
    MicrosoftMxc,
    MicroVm,
    ProviderManaged,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeLaunchDriver {
    None,
    InProcess,
    HostService,
    Docker,
    Podman,
    Containerd,
    Kubernetes,
    OpenShellGateway,
    MxcSdk,
    RunnerRpc,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeInteractionChannel {
    Exec,
    StdinPipe,
    InteractiveTty,
    SshSession,
    EditorWorkspace,
    ServiceExpose,
    FileTransfer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Ready,
    Starting,
    Offline,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeBinding {
    pub session_manager: RuntimeSessionManager,
    pub isolation_backend: RuntimeIsolationBackend,
    pub launch_driver: RuntimeLaunchDriver,
    pub runtime_id: String,
    pub display_name: String,
    pub status: RuntimeStatus,
    pub ephemeral: bool,
    pub supports_long_sessions: bool,
    #[serde(default)]
    pub interaction_channels: Vec<RuntimeInteractionChannel>,
}

impl RuntimeBinding {
    pub fn none() -> Self {
        Self {
            session_manager: RuntimeSessionManager::None,
            isolation_backend: RuntimeIsolationBackend::None,
            launch_driver: RuntimeLaunchDriver::None,
            runtime_id: "none".to_string(),
            display_name: "No runtime".to_string(),
            status: RuntimeStatus::Offline,
            ephemeral: true,
            supports_long_sessions: false,
            interaction_channels: Vec::new(),
        }
    }

    pub fn host_process(runtime_id: impl Into<String>) -> Self {
        Self {
            session_manager: RuntimeSessionManager::HostProcess,
            isolation_backend: RuntimeIsolationBackend::HostProcess,
            launch_driver: RuntimeLaunchDriver::InProcess,
            runtime_id: runtime_id.into(),
            display_name: "Host process".to_string(),
            status: RuntimeStatus::Ready,
            ephemeral: false,
            supports_long_sessions: true,
            interaction_channels: vec![
                RuntimeInteractionChannel::Exec,
                RuntimeInteractionChannel::StdinPipe,
                RuntimeInteractionChannel::InteractiveTty,
            ],
        }
    }

    pub fn gvisor(runtime_id: impl Into<String>) -> Self {
        Self {
            session_manager: RuntimeSessionManager::AstraManaged,
            isolation_backend: RuntimeIsolationBackend::GVisorRunsc,
            launch_driver: RuntimeLaunchDriver::Containerd,
            runtime_id: runtime_id.into(),
            display_name: "gVisor runsc".to_string(),
            status: RuntimeStatus::Ready,
            ephemeral: true,
            supports_long_sessions: true,
            interaction_channels: vec![
                RuntimeInteractionChannel::Exec,
                RuntimeInteractionChannel::StdinPipe,
            ],
        }
    }

    pub fn nvidia_openshell(runtime_id: impl Into<String>) -> Self {
        Self {
            session_manager: RuntimeSessionManager::NvidiaOpenShell,
            isolation_backend: RuntimeIsolationBackend::ProviderManaged,
            launch_driver: RuntimeLaunchDriver::OpenShellGateway,
            runtime_id: runtime_id.into(),
            display_name: "NVIDIA OpenShell".to_string(),
            status: RuntimeStatus::Ready,
            ephemeral: true,
            supports_long_sessions: false,
            interaction_channels: vec![
                RuntimeInteractionChannel::Exec,
                RuntimeInteractionChannel::StdinPipe,
                RuntimeInteractionChannel::InteractiveTty,
                RuntimeInteractionChannel::SshSession,
                RuntimeInteractionChannel::EditorWorkspace,
                RuntimeInteractionChannel::ServiceExpose,
                RuntimeInteractionChannel::FileTransfer,
            ],
        }
    }

    pub fn oci_container(runtime_id: impl Into<String>) -> Self {
        Self {
            session_manager: RuntimeSessionManager::AstraManaged,
            isolation_backend: RuntimeIsolationBackend::OciRuntime,
            launch_driver: RuntimeLaunchDriver::RunnerRpc,
            runtime_id: runtime_id.into(),
            display_name: "OCI container".to_string(),
            status: RuntimeStatus::Ready,
            ephemeral: true,
            supports_long_sessions: true,
            interaction_channels: vec![
                RuntimeInteractionChannel::Exec,
                RuntimeInteractionChannel::StdinPipe,
            ],
        }
    }

    pub fn with_launch_driver(mut self, launch_driver: RuntimeLaunchDriver) -> Self {
        self.launch_driver = launch_driver;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunBinding {
    pub workspace: WorkspaceBinding,
    pub executor: ExecutorBinding,
    pub runtime: RuntimeBinding,
    pub policy: PolicyIntent,
    pub capabilities: EffectiveCapabilitySet,
    pub tool_surface: AvailableToolSurface,
}

impl RunBinding {
    pub fn resolve(
        workspace: WorkspaceBinding,
        executor: ExecutorBinding,
        runtime: RuntimeBinding,
        policy: PolicyIntent,
        registry: &ToolRegistry,
    ) -> Self {
        let capabilities =
            EffectiveCapabilitySet::from_bindings(&workspace, &executor, &runtime, &policy);
        let mut tool_surface = CapabilityResolver.available_tool_surface(registry, &capabilities);
        apply_policy_tool_allowlist(&policy, &mut tool_surface);
        Self {
            workspace,
            executor,
            runtime,
            policy,
            capabilities,
            tool_surface,
        }
    }

    pub fn cloud_control_plane(registry: &ToolRegistry) -> Self {
        Self::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding::control_plane(),
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            registry,
        )
    }

    pub fn local_developer(cwd: impl Into<String>, registry: &ToolRegistry) -> Self {
        Self::resolve(
            WorkspaceBinding::local_filesystem(cwd, WorkspaceAuthority::ReadWrite),
            ExecutorBinding::local_cli(),
            RuntimeBinding::host_process("local-host"),
            PolicyIntent::local_developer(),
            registry,
        )
    }

    pub fn edge_developer(cwd: impl Into<String>, registry: &ToolRegistry) -> Self {
        Self::resolve(
            WorkspaceBinding::edge_workspace(cwd, WorkspaceAuthority::ReadWrite),
            ExecutorBinding::edge_agent("edge-agent"),
            RuntimeBinding::host_process("edge-host"),
            PolicyIntent::local_developer(),
            registry,
        )
    }

    pub fn read_only_snapshot(root: impl Into<String>, registry: &ToolRegistry) -> Self {
        Self::resolve(
            WorkspaceBinding::cloud_workspace(root, WorkspaceAuthority::ReadOnly),
            ExecutorBinding::hosted_runner("snapshot-runner"),
            RuntimeBinding::oci_container("snapshot-runtime"),
            PolicyIntent::read_only_review(),
            registry,
        )
    }
}

fn apply_policy_tool_allowlist(policy: &PolicyIntent, tool_surface: &mut AvailableToolSurface) {
    if !policy.has_restricted_tool_allowlist() {
        return;
    }

    let mut policy_denials = Vec::new();
    tool_surface.tool_names.retain(|tool_name| {
        if policy.allows_tool(tool_name) {
            return true;
        }
        policy_denials.push(ToolDenial {
            tool_name: tool_name.clone(),
            reason: ToolUnavailableReason::PolicyDenied(PolicyIntent::disallowed_tool_reason(
                tool_name,
            )),
        });
        false
    });

    for denial in &mut tool_surface.denials {
        if !policy.allows_tool(&denial.tool_name) {
            denial.reason = ToolUnavailableReason::PolicyDenied(
                PolicyIntent::disallowed_tool_reason(&denial.tool_name),
            );
        }
    }
    tool_surface.denials.extend(policy_denials);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeEnvironmentAdvertisement {
    pub schema_version: u32,
    pub binding: RunBinding,
}

impl RuntimeEnvironmentAdvertisement {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn new(binding: RunBinding) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            binding,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_environment_advertisement_is_structured_json() {
        let registry = ToolRegistry::builtins();
        let advert = RuntimeEnvironmentAdvertisement::new(RunBinding::edge_developer(
            "/workspace/project",
            &registry,
        ));

        let value = serde_json::to_value(&advert).expect("serialize advertisement");

        assert_eq!(
            value["schema_version"],
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(value["binding"]["workspace"]["kind"], "edge_workspace");
        assert_eq!(
            value["binding"]["capabilities"]["runtime"]["runtime_has_shell"],
            true
        );
        assert!(value["binding"]["tool_surface"]["tool_names"].is_array());
    }

    #[test]
    fn openshell_runtime_advertises_interactive_gateway_channels() {
        let runtime = RuntimeBinding::nvidia_openshell("openshell-runtime");

        assert!(
            !runtime.supports_long_sessions,
            "OpenShell support for long sessions must come from runtime/profile evidence"
        );

        for channel in [
            RuntimeInteractionChannel::Exec,
            RuntimeInteractionChannel::StdinPipe,
            RuntimeInteractionChannel::InteractiveTty,
            RuntimeInteractionChannel::SshSession,
            RuntimeInteractionChannel::EditorWorkspace,
            RuntimeInteractionChannel::ServiceExpose,
            RuntimeInteractionChannel::FileTransfer,
        ] {
            assert!(
                runtime.interaction_channels.contains(&channel),
                "OpenShell runtime should advertise {channel:?}"
            );
        }
    }

    #[test]
    fn oci_and_gvisor_runtimes_do_not_imply_openshell_interaction_channels() {
        for runtime in [
            RuntimeBinding::oci_container("oci-runtime"),
            RuntimeBinding::gvisor("gvisor-runtime"),
        ] {
            assert!(
                runtime
                    .interaction_channels
                    .contains(&RuntimeInteractionChannel::Exec)
            );
            assert!(
                runtime
                    .interaction_channels
                    .contains(&RuntimeInteractionChannel::StdinPipe)
            );
            for provider_channel in [
                RuntimeInteractionChannel::SshSession,
                RuntimeInteractionChannel::EditorWorkspace,
                RuntimeInteractionChannel::ServiceExpose,
                RuntimeInteractionChannel::FileTransfer,
            ] {
                assert!(
                    !runtime.interaction_channels.contains(&provider_channel),
                    "{provider_channel:?} should require explicit provider evidence"
                );
            }
        }
    }

    #[test]
    fn workspace_server_sandbox_constructor() {
        let ws = WorkspaceBinding::server_sandbox("/sandbox/root");
        assert_eq!(ws.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(ws.cwd, Some("/sandbox/root".to_string()));
        assert_eq!(ws.authority, WorkspaceAuthority::ReadWrite);
        assert!(ws.persistent);
        assert_eq!(ws.display_name, "Server sandbox");
    }

    #[test]
    fn workspace_kind_serde_roundtrips() {
        // Verify that adding LocalFilesystem doesn't break serde compatibility
        // with existing serialized data (snake_case renaming still works).
        let kind = WorkspaceBindingKind::ServerSandbox;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"server_sandbox\"");

        let kind = WorkspaceBindingKind::LocalFilesystem;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"local_filesystem\"");
    }
}
