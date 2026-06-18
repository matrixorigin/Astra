use std::path::{Path, PathBuf};

use astra_runtime_env::{
    CleanupReason, RuntimeBinding, WorkspaceAuthority, WorkspaceBindingKind, WorkspaceMountPlan,
    WorkspaceOwnerScope, WorkspacePersistence, WorkspaceProvisionError,
    WorkspaceProvisionErrorKind, WorkspaceProvisionRequest, WorkspaceProvisioner, WorkspaceRecord,
    WorkspaceSource,
};
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerWorkspaceRecord {
    pub(crate) session_id: String,
    pub(crate) safe_id: String,
    pub(crate) root: PathBuf,
    pub(crate) base: PathBuf,
    pub(crate) workspace: WorkspaceRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ServerWorkspaceProvisionError {
    #[error("session id does not contain any filesystem-safe characters")]
    InvalidSessionId,
    #[error("failed to create workspace base '{path}': {message}")]
    BaseCreateFailed { path: PathBuf, message: String },
    #[error("failed to resolve workspace base '{path}': {message}")]
    BaseCanonicalizeFailed { path: PathBuf, message: String },
    #[error("failed to create workspace '{path}': {message}")]
    WorkspaceCreateFailed { path: PathBuf, message: String },
    #[error("failed to resolve workspace '{path}': {message}")]
    WorkspaceCanonicalizeFailed { path: PathBuf, message: String },
    #[error("resolved workspace '{workspace}' escaped base '{base}'")]
    WorkspaceEscapedBase { workspace: PathBuf, base: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerWorkspaceProvisioner {
    base_dir: PathBuf,
}

impl ServerWorkspaceProvisioner {
    pub(crate) fn from_env() -> Self {
        let base_dir = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        Self { base_dir }
    }

    #[cfg(test)]
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub(crate) fn provision(
        &self,
        session_id: &str,
    ) -> Result<ServerWorkspaceRecord, ServerWorkspaceProvisionError> {
        let safe_id = safe_workspace_id(session_id)?;
        std::fs::create_dir_all(&self.base_dir).map_err(|error| {
            ServerWorkspaceProvisionError::BaseCreateFailed {
                path: self.base_dir.clone(),
                message: error.to_string(),
            }
        })?;
        let base = canonicalize_path(
            &self.base_dir,
            ServerWorkspaceProvisionError::BaseCanonicalizeFailed {
                path: self.base_dir.clone(),
                message: String::new(),
            },
        )?;
        let workspace = base.join(&safe_id);
        std::fs::create_dir_all(&workspace).map_err(|error| {
            ServerWorkspaceProvisionError::WorkspaceCreateFailed {
                path: workspace.clone(),
                message: error.to_string(),
            }
        })?;
        // Guard: if any step after directory creation fails, clean up the
        // orphan workspace so partially provisioned directories don't
        // accumulate.
        struct WorkspaceGuard {
            path: PathBuf,
            consumed: bool,
        }
        impl Drop for WorkspaceGuard {
            fn drop(&mut self) {
                if !self.consumed && self.path.exists() {
                    let _ = std::fs::remove_dir_all(&self.path);
                }
            }
        }
        let mut guard = WorkspaceGuard {
            path: workspace.clone(),
            consumed: false,
        };
        let root = canonicalize_path(
            &workspace,
            ServerWorkspaceProvisionError::WorkspaceCanonicalizeFailed {
                path: workspace.clone(),
                message: String::new(),
            },
        )?;
        if !root.starts_with(&base) {
            return Err(ServerWorkspaceProvisionError::WorkspaceEscapedBase {
                workspace: root,
                base,
            });
        }

        guard.consumed = true;
        Ok(ServerWorkspaceRecord {
            session_id: session_id.to_string(),
            safe_id: safe_id.clone(),
            root: root.clone(),
            base,
            workspace: WorkspaceRecord {
                workspace_id: safe_id,
                owner_scope: WorkspaceOwnerScope::ServerSession,
                kind: WorkspaceBindingKind::ServerSandbox,
                authority: WorkspaceAuthority::ReadWrite,
                root_or_volume_ref: root.display().to_string(),
                source: WorkspaceSource::ServerSandbox {
                    session_id: session_id.to_string(),
                },
                persistence: WorkspacePersistence::Session,
                revision: "1".to_string(),
                display_name: "Server sandbox".to_string(),
            },
        })
    }
}

#[async_trait]
impl WorkspaceProvisioner for ServerWorkspaceProvisioner {
    async fn provision(
        &self,
        request: WorkspaceProvisionRequest,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        request.validate()?;
        if request.kind != WorkspaceBindingKind::ServerSandbox {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::SourceKindMismatch,
                message: "server workspace provisioner only supports server sandbox workspaces"
                    .to_string(),
                workspace_id: Some(request.workspace_id),
            });
        }
        let WorkspaceSource::ServerSandbox { session_id } = request.source else {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::SourceKindMismatch,
                message: "server sandbox workspace source is required".to_string(),
                workspace_id: Some(request.workspace_id),
            });
        };
        let safe_id = safe_workspace_id(&session_id).map_err(server_error_to_workspace_error)?;
        if request.workspace_id != safe_id {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::SourceKindMismatch,
                message: format!(
                    "server sandbox workspace_id '{}' must match source.session_id '{}'",
                    request.workspace_id, safe_id
                ),
                workspace_id: Some(request.workspace_id),
            });
        }
        let record = self
            .provision(&session_id)
            .map_err(server_error_to_workspace_error)?;
        Ok(record.workspace)
    }

    async fn mount_plan(
        &self,
        workspace: &WorkspaceRecord,
        runtime: &RuntimeBinding,
        target: &str,
    ) -> Result<WorkspaceMountPlan, WorkspaceProvisionError> {
        workspace.mount_plan(runtime, target)
    }

    async fn cleanup(
        &self,
        workspace: &WorkspaceRecord,
        reason: CleanupReason,
    ) -> Result<(), astra_runtime_env::WorkspaceCleanupError> {
        if workspace.kind != WorkspaceBindingKind::ServerSandbox {
            return Err(astra_runtime_env::WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: format!(
                    "server workspace provisioner cannot clean {:?} workspace records",
                    workspace.kind
                ),
            });
        }
        if !matches!(workspace.source, WorkspaceSource::ServerSandbox { .. }) {
            return Err(astra_runtime_env::WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: "server workspace cleanup requires a server sandbox source".to_string(),
            });
        }
        let root = PathBuf::from(&workspace.root_or_volume_ref);
        if !root.exists() {
            return Ok(());
        }
        let base = self.base_dir.canonicalize().map_err(|error| {
            astra_runtime_env::WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: format!(
                    "failed to resolve workspace base '{}': {error}",
                    self.base_dir.display()
                ),
            }
        })?;
        let root =
            root.canonicalize()
                .map_err(|error| astra_runtime_env::WorkspaceCleanupError {
                    workspace_id: workspace.workspace_id.clone(),
                    reason,
                    message: format!(
                        "failed to resolve workspace '{}': {error}",
                        workspace.root_or_volume_ref
                    ),
                })?;
        if !root.starts_with(&base) {
            return Err(astra_runtime_env::WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: format!(
                    "workspace '{}' is outside base '{}'",
                    root.display(),
                    base.display()
                ),
            });
        }
        std::fs::remove_dir_all(&root).map_err(|error| astra_runtime_env::WorkspaceCleanupError {
            workspace_id: workspace.workspace_id.clone(),
            reason,
            message: format!("failed to remove workspace '{}': {error}", root.display()),
        })
    }
}

fn server_error_to_workspace_error(
    error: ServerWorkspaceProvisionError,
) -> WorkspaceProvisionError {
    match error {
        ServerWorkspaceProvisionError::InvalidSessionId => {
            WorkspaceProvisionError::invalid_workspace_id("")
        }
        ServerWorkspaceProvisionError::WorkspaceEscapedBase { workspace, base } => {
            WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::MountFailed,
                message: format!(
                    "resolved workspace '{}' escaped base '{}'",
                    workspace.display(),
                    base.display()
                ),
                workspace_id: None,
            }
        }
        other => WorkspaceProvisionError::unavailable("server_sandbox", other.to_string()),
    }
}

fn canonicalize_path(
    path: &Path,
    template: ServerWorkspaceProvisionError,
) -> Result<PathBuf, ServerWorkspaceProvisionError> {
    path.canonicalize().map_err(|error| match template {
        ServerWorkspaceProvisionError::BaseCanonicalizeFailed { path, .. } => {
            ServerWorkspaceProvisionError::BaseCanonicalizeFailed {
                path,
                message: error.to_string(),
            }
        }
        ServerWorkspaceProvisionError::WorkspaceCanonicalizeFailed { path, .. } => {
            ServerWorkspaceProvisionError::WorkspaceCanonicalizeFailed {
                path,
                message: error.to_string(),
            }
        }
        other => other,
    })
}

fn safe_workspace_id(session_id: &str) -> Result<String, ServerWorkspaceProvisionError> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ServerWorkspaceProvisionError::InvalidSessionId);
    }
    Ok(session_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_creates_workspace_under_base_with_safe_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));

        let record = provisioner
            .provision("session-abc_123")
            .expect("provision workspace");

        assert_eq!(record.safe_id, "session-abc_123");
        assert!(record.root.starts_with(&record.base));
        assert!(record.root.is_dir());
        assert_eq!(record.workspace.workspace_id, "session-abc_123");
        assert_eq!(record.workspace.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(record.workspace.authority, WorkspaceAuthority::ReadWrite);
    }

    #[test]
    fn provision_rejects_empty_session_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));

        let error = provisioner
            .provision("")
            .expect_err("invalid session id should fail");

        assert_eq!(error, ServerWorkspaceProvisionError::InvalidSessionId);
    }

    #[test]
    fn provision_rejects_session_id_with_unsafe_characters() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));

        let error = provisioner
            .provision("../session:abc_123")
            .expect_err("unsafe session id should fail");

        assert_eq!(error, ServerWorkspaceProvisionError::InvalidSessionId);
    }

    #[cfg(unix)]
    #[test]
    fn provision_rejects_existing_symlink_that_escapes_base() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("workspaces");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&base).expect("base");
        std::fs::create_dir_all(&outside).expect("outside");
        symlink(&outside, base.join("session-1")).expect("symlink");
        let provisioner = ServerWorkspaceProvisioner::new(base);

        let error = provisioner
            .provision("session-1")
            .expect_err("escaping symlink should fail");

        assert!(matches!(
            error,
            ServerWorkspaceProvisionError::WorkspaceEscapedBase { .. }
        ));
    }

    #[tokio::test]
    async fn trait_provision_returns_workspace_record_and_mount_plan() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));
        let request = WorkspaceProvisionRequest::server_sandbox("session-1");

        let record = WorkspaceProvisioner::provision(&provisioner, request)
            .await
            .expect("workspace record");
        let mount = provisioner
            .mount_plan(
                &record,
                &RuntimeBinding::host_process("server-host"),
                "/workspace",
            )
            .await
            .expect("mount plan");

        assert_eq!(record.workspace_id, "session-1");
        assert_eq!(mount.workspace_id, "session-1");
        assert!(mount.writable);
        assert_eq!(mount.target, "/workspace");
    }

    #[tokio::test]
    async fn trait_provision_rejects_mismatched_workspace_id_and_source_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));
        let mut request = WorkspaceProvisionRequest::server_sandbox("session-1");
        request.workspace_id = "session-2".to_string();

        let error = WorkspaceProvisioner::provision(&provisioner, request)
            .await
            .expect_err("mismatched request should fail");

        assert_eq!(error.kind, WorkspaceProvisionErrorKind::SourceKindMismatch);
        assert!(error.message.contains("must match source.session_id"));
    }

    #[tokio::test]
    async fn trait_cleanup_rejects_workspace_outside_base() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));
        let mut record = provisioner
            .provision("session-1")
            .expect("workspace")
            .workspace;
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        record.root_or_volume_ref = outside.display().to_string();

        let error = provisioner
            .cleanup(&record, CleanupReason::Failed)
            .await
            .expect_err("outside cleanup should fail");

        assert!(error.message.contains("outside base"));
    }

    #[tokio::test]
    async fn trait_cleanup_rejects_non_server_workspace_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner = ServerWorkspaceProvisioner::new(temp.path().join("workspaces"));
        let mut record = provisioner
            .provision("session-1")
            .expect("workspace")
            .workspace;
        record.kind = WorkspaceBindingKind::CloudWorkspace;
        record.source = WorkspaceSource::Scratch;

        let error = provisioner
            .cleanup(&record, CleanupReason::Failed)
            .await
            .expect_err("wrong owner should fail");

        assert!(error.message.contains("cannot clean"));
    }

    #[tokio::test]
    async fn trait_cleanup_is_idempotent_when_base_and_root_are_gone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("workspaces");
        let provisioner = ServerWorkspaceProvisioner::new(base.clone());
        let record = provisioner
            .provision("session-1")
            .expect("workspace")
            .workspace;
        std::fs::remove_dir_all(&base).expect("remove base");

        provisioner
            .cleanup(&record, CleanupReason::Completed)
            .await
            .expect("missing root cleanup should be idempotent");
    }
}
