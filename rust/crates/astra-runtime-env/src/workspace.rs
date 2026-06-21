use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{RuntimeBinding, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOwnerScope {
    None,
    User,
    Team,
    Organization,
    Tenant,
    ServerSession,
    Executor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkspaceSource {
    None,
    LocalPath {
        path: String,
    },
    EdgePath {
        executor_id: String,
        path: String,
    },
    ServerSandbox {
        session_id: String,
    },
    GitCheckout {
        repository: String,
        reference: Option<String>,
    },
    UploadedSnapshot {
        artifact_id: String,
    },
    Template {
        template_id: String,
    },
    DatasetBundle {
        dataset_id: String,
    },
    ArtifactBundle {
        artifact_id: String,
    },
    Scratch,
    PersistentVolume {
        volume_id: String,
    },
    ProviderManaged {
        provider: String,
        reference: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePersistence {
    None,
    Ephemeral,
    Session,
    Persistent,
    ImmutableSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceProvisionRequest {
    pub workspace_id: String,
    pub owner_scope: WorkspaceOwnerScope,
    pub kind: WorkspaceBindingKind,
    pub authority: WorkspaceAuthority,
    pub source: WorkspaceSource,
    pub persistence: WorkspacePersistence,
    pub requested_root: Option<String>,
    pub display_name: Option<String>,
}

impl WorkspaceProvisionRequest {
    pub fn server_sandbox(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        Self {
            workspace_id: session_id.clone(),
            owner_scope: WorkspaceOwnerScope::ServerSession,
            kind: WorkspaceBindingKind::ServerSandbox,
            authority: WorkspaceAuthority::ReadWrite,
            source: WorkspaceSource::ServerSandbox { session_id },
            persistence: WorkspacePersistence::Session,
            requested_root: None,
            display_name: Some("Server sandbox".to_string()),
        }
    }

    pub fn validate(&self) -> Result<(), WorkspaceProvisionError> {
        validate_workspace_id(&self.workspace_id)?;
        if self.authority == WorkspaceAuthority::None && self.kind != WorkspaceBindingKind::None {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::AuthorityDenied,
                message: "workspace authority is required for a bound workspace".to_string(),
                workspace_id: Some(self.workspace_id.clone()),
            });
        }
        if !source_matches_kind(&self.source, self.kind) {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::SourceKindMismatch,
                message: format!(
                    "workspace source {:?} does not match binding kind {:?}",
                    self.source, self.kind
                ),
                workspace_id: Some(self.workspace_id.clone()),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub workspace_id: String,
    pub owner_scope: WorkspaceOwnerScope,
    pub kind: WorkspaceBindingKind,
    pub authority: WorkspaceAuthority,
    pub root_or_volume_ref: String,
    pub source: WorkspaceSource,
    pub persistence: WorkspacePersistence,
    pub revision: String,
    pub display_name: String,
}

impl WorkspaceRecord {
    pub fn binding(&self) -> WorkspaceBinding {
        WorkspaceBinding {
            kind: self.kind,
            display_name: self.display_name.clone(),
            cwd: if self.kind == WorkspaceBindingKind::None {
                None
            } else {
                Some(self.root_or_volume_ref.clone())
            },
            authority: self.authority,
            persistent: matches!(
                self.persistence,
                WorkspacePersistence::Session | WorkspacePersistence::Persistent
            ),
        }
    }

    pub fn mount_plan(
        &self,
        runtime: &RuntimeBinding,
        target: impl Into<String>,
    ) -> Result<WorkspaceMountPlan, WorkspaceProvisionError> {
        let target = target.into();
        validate_absolute_mount_target(&target)?;
        if self.kind == WorkspaceBindingKind::None || self.authority == WorkspaceAuthority::None {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::AuthorityDenied,
                message: "workspace mount requires workspace authority".to_string(),
                workspace_id: Some(self.workspace_id.clone()),
            });
        }
        Ok(WorkspaceMountPlan {
            workspace_id: self.workspace_id.clone(),
            workspace_kind: self.kind,
            source: self.root_or_volume_ref.clone(),
            target,
            authority: self.authority,
            writable: self.authority == WorkspaceAuthority::ReadWrite,
            persistent: matches!(
                self.persistence,
                WorkspacePersistence::Session | WorkspacePersistence::Persistent
            ),
            runtime_session_manager: runtime.session_manager,
            runtime_isolation_backend: runtime.isolation_backend,
            launch_driver: runtime.launch_driver,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceMountPlan {
    pub workspace_id: String,
    pub workspace_kind: WorkspaceBindingKind,
    pub source: String,
    pub target: String,
    pub authority: WorkspaceAuthority,
    pub writable: bool,
    pub persistent: bool,
    pub runtime_session_manager: crate::RuntimeSessionManager,
    pub runtime_isolation_backend: crate::RuntimeIsolationBackend,
    pub launch_driver: crate::RuntimeLaunchDriver,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CleanupReason {
    Completed,
    Cancelled,
    Failed,
    LeaseExpired,
    OperatorRequested,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[error("{kind}: {message}")]
pub struct WorkspaceProvisionError {
    pub kind: WorkspaceProvisionErrorKind,
    pub message: String,
    pub workspace_id: Option<String>,
}

impl WorkspaceProvisionError {
    pub fn invalid_workspace_id(workspace_id: impl Into<String>) -> Self {
        let workspace_id = workspace_id.into();
        Self {
            kind: WorkspaceProvisionErrorKind::InvalidWorkspaceId,
            message: "workspace id is not a safe filesystem segment".to_string(),
            workspace_id: Some(workspace_id),
        }
    }

    pub fn unavailable(workspace_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceProvisionErrorKind::WorkspaceUnavailable,
            message: message.into(),
            workspace_id: Some(workspace_id.into()),
        }
    }

    pub fn mount_failed(workspace_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceProvisionErrorKind::MountFailed,
            message: message.into(),
            workspace_id: Some(workspace_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceProvisionErrorKind {
    #[error("invalid_workspace_id")]
    InvalidWorkspaceId,
    #[error("source_kind_mismatch")]
    SourceKindMismatch,
    #[error("authority_denied")]
    AuthorityDenied,
    #[error("workspace_unavailable")]
    WorkspaceUnavailable,
    #[error("mount_failed")]
    MountFailed,
    #[error("cleanup_failed")]
    CleanupFailed,
    #[error("internal")]
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct WorkspaceCleanupError {
    pub workspace_id: String,
    pub reason: CleanupReason,
    pub message: String,
}

#[async_trait]
pub trait WorkspaceProvisioner: Send + Sync {
    async fn provision(
        &self,
        request: WorkspaceProvisionRequest,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError>;

    async fn mount_plan(
        &self,
        workspace: &WorkspaceRecord,
        runtime: &RuntimeBinding,
        target: &str,
    ) -> Result<WorkspaceMountPlan, WorkspaceProvisionError>;

    async fn cleanup(
        &self,
        workspace: &WorkspaceRecord,
        reason: CleanupReason,
    ) -> Result<(), WorkspaceCleanupError>;
}

pub fn validate_workspace_id(workspace_id: &str) -> Result<(), WorkspaceProvisionError> {
    if workspace_id.is_empty()
        || !workspace_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(WorkspaceProvisionError::invalid_workspace_id(workspace_id));
    }
    Ok(())
}

pub fn workspace_mount_plan_from_binding(
    workspace: &WorkspaceBinding,
    runtime: &RuntimeBinding,
    target: impl Into<String>,
) -> Result<WorkspaceMountPlan, WorkspaceProvisionError> {
    let target = target.into();
    validate_absolute_mount_target(&target)?;
    let Some(source) = workspace.cwd.clone() else {
        return Err(WorkspaceProvisionError::unavailable(
            "unbound",
            "workspace mount requires a workspace binding",
        ));
    };
    if workspace.authority == WorkspaceAuthority::None {
        return Err(WorkspaceProvisionError {
            kind: WorkspaceProvisionErrorKind::AuthorityDenied,
            message: "workspace mount requires workspace authority".to_string(),
            workspace_id: Some("unbound".to_string()),
        });
    }

    Ok(WorkspaceMountPlan {
        workspace_id: "binding-workspace".to_string(),
        workspace_kind: workspace.kind,
        source,
        target,
        authority: workspace.authority,
        writable: workspace.authority == WorkspaceAuthority::ReadWrite,
        persistent: workspace.persistent,
        runtime_session_manager: runtime.session_manager,
        runtime_isolation_backend: runtime.isolation_backend,
        launch_driver: runtime.launch_driver,
    })
}

fn source_matches_kind(source: &WorkspaceSource, kind: WorkspaceBindingKind) -> bool {
    matches!(
        (source, kind),
        (WorkspaceSource::None, WorkspaceBindingKind::None)
            | (
                WorkspaceSource::LocalPath { .. },
                WorkspaceBindingKind::LocalFilesystem
            )
            | (
                WorkspaceSource::EdgePath { .. },
                WorkspaceBindingKind::EdgeWorkspace
            )
            | (
                WorkspaceSource::ServerSandbox { .. },
                WorkspaceBindingKind::ServerSandbox
            )
            | (
                WorkspaceSource::PersistentVolume { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::GitCheckout { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::UploadedSnapshot { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::Template { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::DatasetBundle { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::ArtifactBundle { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::Scratch,
                WorkspaceBindingKind::CloudWorkspace
            )
            | (
                WorkspaceSource::ProviderManaged { .. },
                WorkspaceBindingKind::CloudWorkspace
            )
    )
}

fn validate_absolute_mount_target(target: &str) -> Result<(), WorkspaceProvisionError> {
    if target.starts_with('/') {
        Ok(())
    } else {
        Err(WorkspaceProvisionError {
            kind: WorkspaceProvisionErrorKind::MountFailed,
            message: "workspace mount target must be absolute".to_string(),
            workspace_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeBinding, RuntimeLaunchDriver};

    #[test]
    fn server_sandbox_request_validates_safe_workspace_id() {
        WorkspaceProvisionRequest::server_sandbox("session-1")
            .validate()
            .expect("safe id");

        let error = WorkspaceProvisionRequest::server_sandbox("../session")
            .validate()
            .expect_err("unsafe id should fail");

        assert_eq!(error.kind, WorkspaceProvisionErrorKind::InvalidWorkspaceId);
    }

    #[test]
    fn workspace_record_builds_binding_and_mount_plan() {
        let record = WorkspaceRecord {
            workspace_id: "session-1".to_string(),
            owner_scope: WorkspaceOwnerScope::ServerSession,
            kind: WorkspaceBindingKind::ServerSandbox,
            authority: WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/tmp/astra-workspaces/session-1".to_string(),
            source: WorkspaceSource::ServerSandbox {
                session_id: "session-1".to_string(),
            },
            persistence: WorkspacePersistence::Session,
            revision: "1".to_string(),
            display_name: "Server sandbox".to_string(),
        };

        let binding = record.binding();
        assert_eq!(binding.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(
            binding.cwd.as_deref(),
            Some("/tmp/astra-workspaces/session-1")
        );
        assert!(binding.persistent);

        let runtime =
            RuntimeBinding::gvisor("gvisor-1").with_launch_driver(RuntimeLaunchDriver::Kubernetes);
        let mount = record
            .mount_plan(&runtime, "/workspace")
            .expect("mount plan");
        assert_eq!(mount.workspace_id, "session-1");
        assert_eq!(mount.source, "/tmp/astra-workspaces/session-1");
        assert_eq!(mount.target, "/workspace");
        assert_eq!(mount.launch_driver, RuntimeLaunchDriver::Kubernetes);
        assert!(mount.writable);
    }

    #[test]
    fn workspace_mount_plan_rejects_relative_target() {
        let workspace =
            WorkspaceBinding::cloud_workspace("/tenant/ws-1", WorkspaceAuthority::ReadWrite);
        let error = workspace_mount_plan_from_binding(
            &workspace,
            &RuntimeBinding::oci_container("oci-1"),
            "workspace",
        )
        .expect_err("relative target");

        assert_eq!(error.kind, WorkspaceProvisionErrorKind::MountFailed);
    }

    #[test]
    fn workspace_request_rejects_source_kind_mismatch() {
        let mut request = WorkspaceProvisionRequest::server_sandbox("session-1");
        request.source = WorkspaceSource::GitCheckout {
            repository: "https://example.test/repo.git".to_string(),
            reference: None,
        };

        let error = request.validate().expect_err("mismatch");

        assert_eq!(error.kind, WorkspaceProvisionErrorKind::SourceKindMismatch);
    }

    #[test]
    fn source_materialization_variants_use_cloud_workspace_kind() {
        for source in [
            WorkspaceSource::GitCheckout {
                repository: "https://example.test/repo.git".to_string(),
                reference: None,
            },
            WorkspaceSource::UploadedSnapshot {
                artifact_id: "artifact-1".to_string(),
            },
            WorkspaceSource::Template {
                template_id: "template-1".to_string(),
            },
            WorkspaceSource::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            },
            WorkspaceSource::ArtifactBundle {
                artifact_id: "artifact-2".to_string(),
            },
            WorkspaceSource::Scratch,
            WorkspaceSource::PersistentVolume {
                volume_id: "volume-1".to_string(),
            },
        ] {
            let request = WorkspaceProvisionRequest {
                workspace_id: "workspace-1".to_string(),
                owner_scope: WorkspaceOwnerScope::Tenant,
                kind: WorkspaceBindingKind::CloudWorkspace,
                authority: WorkspaceAuthority::ReadWrite,
                source,
                persistence: WorkspacePersistence::Session,
                requested_root: None,
                display_name: None,
            };

            request
                .validate()
                .expect("source should materialize as cloud workspace");
        }
    }
}
