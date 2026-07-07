use serde::{Deserialize, Serialize};

use crate::{
    CredentialPolicy, ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FilesystemPolicy,
    NetworkPolicy, PolicyIntent, RuntimeBinding, RuntimeIsolationBackend, RuntimeLaunchDriver,
    RuntimeSessionManager, RuntimeStatus, ToolTransportKind, WorkspaceAuthority, WorkspaceBinding,
    WorkspaceBindingKind,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NetworkCapability {
    Disabled,
    AllowList,
    Open,
}

impl From<NetworkPolicy> for NetworkCapability {
    fn from(value: NetworkPolicy) -> Self {
        match value {
            NetworkPolicy::Disabled => Self::Disabled,
            NetworkPolicy::AllowList => Self::AllowList,
            NetworkPolicy::Open => Self::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceCapabilities {
    pub present: bool,
    pub readable: bool,
    pub writable: bool,
    pub persistent: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    pub reachable: bool,
    pub control_plane: bool,
    pub server_service: bool,
    pub runtime_executor: bool,
    pub mcp_executor: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    pub runtime_has_process: bool,
    pub runtime_has_shell: bool,
    pub runtime_has_git: bool,
    pub runtime_has_lsp: bool,
    pub runtime_has_network: bool,
    pub runtime_has_credentials: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyCapabilities {
    pub filesystem_read: bool,
    pub filesystem_write: bool,
    pub network: NetworkCapability,
    pub credentials: bool,
    pub approvals_required: bool,
    pub audit_required: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffectiveCapabilitySet {
    pub workspace: WorkspaceCapabilities,
    pub executor: ExecutorCapabilities,
    pub runtime: RuntimeCapabilities,
    pub policy: PolicyCapabilities,
}

impl EffectiveCapabilitySet {
    pub fn from_bindings(
        workspace: &WorkspaceBinding,
        executor: &ExecutorBinding,
        runtime: &RuntimeBinding,
        policy: &PolicyIntent,
    ) -> Self {
        let policy_filesystem_read = matches!(
            policy.filesystem,
            FilesystemPolicy::ReadOnlyWorkspace
                | FilesystemPolicy::ReadWriteWorkspace
                | FilesystemPolicy::ExplicitAllowList
        );
        let policy_filesystem_write = matches!(
            policy.filesystem,
            FilesystemPolicy::ReadWriteWorkspace | FilesystemPolicy::ExplicitAllowList
        );
        let workspace_bound = matches!(
            workspace.kind,
            WorkspaceBindingKind::LocalFilesystem
                | WorkspaceBindingKind::EdgeWorkspace
                | WorkspaceBindingKind::ServerSandbox
                | WorkspaceBindingKind::CloudWorkspace
        ) && workspace.cwd.is_some();
        let workspace_readable = workspace_bound
            && matches!(
                workspace.authority,
                WorkspaceAuthority::ReadOnly | WorkspaceAuthority::ReadWrite
            )
            && policy_filesystem_read;
        let workspace_writable = workspace_bound
            && matches!(workspace.authority, WorkspaceAuthority::ReadWrite)
            && policy_filesystem_write;
        let runtime_topology_known = !matches!(
            runtime.session_manager,
            RuntimeSessionManager::None | RuntimeSessionManager::Unknown
        ) && !matches!(
            runtime.isolation_backend,
            RuntimeIsolationBackend::None | RuntimeIsolationBackend::Unknown
        ) && !matches!(
            runtime.launch_driver,
            RuntimeLaunchDriver::None | RuntimeLaunchDriver::Unknown
        );
        let runtime_ready = runtime.status == RuntimeStatus::Ready && runtime_topology_known;
        let runtime_has_process = runtime_ready
            && matches!(
                runtime.isolation_backend,
                RuntimeIsolationBackend::HostProcess
                    | RuntimeIsolationBackend::LinuxProcessIsolation
                    | RuntimeIsolationBackend::OciRuntime
                    | RuntimeIsolationBackend::GVisorRunsc
                    | RuntimeIsolationBackend::MicrosoftMxc
                    | RuntimeIsolationBackend::MicroVm
                    | RuntimeIsolationBackend::ProviderManaged
            );
        let executor_reachable = matches!(
            executor.status,
            ExecutorStatus::Online | ExecutorStatus::Degraded
        );
        let executor_transport_known = !matches!(
            executor.transport,
            ToolTransportKind::None | ToolTransportKind::Unknown
        );
        let network = NetworkCapability::from(policy.network);
        let credentials = matches!(
            policy.credentials,
            CredentialPolicy::UserApproved | CredentialPolicy::ScopedInjection
        );

        Self {
            workspace: WorkspaceCapabilities {
                present: workspace_bound,
                readable: workspace_readable,
                writable: workspace_writable,
                persistent: workspace.persistent,
            },
            executor: ExecutorCapabilities {
                reachable: executor_reachable,
                control_plane: executor_reachable
                    && executor_transport_known
                    && matches!(
                        executor.kind,
                        ExecutorBindingKind::ControlPlane | ExecutorBindingKind::LocalCli
                    ),
                server_service: executor_reachable
                    && executor_transport_known
                    && matches!(
                        executor.kind,
                        ExecutorBindingKind::ControlPlane
                            | ExecutorBindingKind::ServerRuntime
                            | ExecutorBindingKind::LocalCli
                    ),
                runtime_executor: executor_reachable
                    && executor_transport_known
                    && matches!(
                        executor.kind,
                        ExecutorBindingKind::ServerRuntime
                            | ExecutorBindingKind::LocalCli
                            | ExecutorBindingKind::EdgeAgent
                            | ExecutorBindingKind::OrchestratorManaged
                    ),
                mcp_executor: executor_reachable
                    && executor_transport_known
                    && matches!(executor.kind, ExecutorBindingKind::Mcp),
            },
            runtime: RuntimeCapabilities {
                runtime_has_process,
                runtime_has_shell: runtime_has_process,
                runtime_has_git: runtime_has_process && workspace_readable,
                runtime_has_lsp: runtime_has_process && workspace_readable,
                runtime_has_network: !matches!(network, NetworkCapability::Disabled),
                runtime_has_credentials: credentials,
            },
            policy: PolicyCapabilities {
                filesystem_read: policy_filesystem_read,
                filesystem_write: policy_filesystem_write,
                network,
                credentials,
                approvals_required: !matches!(policy.approval, crate::ApprovalPolicy::Never),
                audit_required: policy.audit.record_invocations || policy.audit.record_denials,
            },
        }
    }
}
