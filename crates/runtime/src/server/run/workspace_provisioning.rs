use std::path::{Path, PathBuf};

use astra_runtime_env::{
    CleanupReason, RuntimeBinding, WorkspaceAuthority, WorkspaceBindingKind, WorkspaceMountPlan,
    WorkspaceOwnerScope, WorkspacePersistence, WorkspaceProvisionError,
    WorkspaceProvisionErrorKind, WorkspaceProvisionRequest, WorkspaceProvisioner, WorkspaceRecord,
    WorkspaceSource, validate_workspace_id,
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
    #[error("workspace belongs to another executor")]
    NotThisExecutor,
    #[error("workspace source is not managed by this provider")]
    UnsupportedWorkspace,
    #[error("invalid owned workspace record: {0}")]
    InvalidWorkspaceRecord(String),
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

#[derive(Debug, Clone)]
pub(crate) struct ServerWorkspaceProvisioner {
    base_dir: PathBuf,
    executor_id: String,
    #[cfg(any(test, feature = "e2e-hooks"))]
    _fixture_directory: Option<std::sync::Arc<tempfile::TempDir>>,
}

impl ServerWorkspaceProvisioner {
    pub(crate) fn from_env(executor_id: String) -> Self {
        let base_dir = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        Self {
            base_dir,
            executor_id,
            #[cfg(any(test, feature = "e2e-hooks"))]
            _fixture_directory: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(base_dir: PathBuf, executor_id: &str) -> Self {
        Self {
            base_dir,
            executor_id: executor_id.into(),
            _fixture_directory: None,
        }
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn fixture(directory: std::sync::Arc<tempfile::TempDir>, executor_id: &str) -> Self {
        Self {
            base_dir: directory.path().to_owned(),
            executor_id: executor_id.into(),
            _fixture_directory: Some(directory),
        }
    }

    pub(crate) fn executor_id(&self) -> &str {
        &self.executor_id
    }

    fn validate_record_identity(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<String, ServerWorkspaceProvisionError> {
        let WorkspaceSource::ServerSandbox {
            session_id,
            executor_id,
        } = &record.source
        else {
            return Err(ServerWorkspaceProvisionError::UnsupportedWorkspace);
        };
        if executor_id != &self.executor_id {
            return Err(ServerWorkspaceProvisionError::NotThisExecutor);
        }
        record.source.validate().map_err(|error| {
            ServerWorkspaceProvisionError::InvalidWorkspaceRecord(error.to_string())
        })?;
        let safe_id = safe_workspace_id(session_id)?;
        if record.workspace_id != safe_id
            || record.kind != WorkspaceBindingKind::ServerSandbox
            || record.owner_scope != WorkspaceOwnerScope::ServerSession
            || record.authority != WorkspaceAuthority::ReadWrite
            || record.persistence != WorkspacePersistence::Session
        {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "source/session/capability mismatch".into(),
            ));
        }
        Ok(safe_id)
    }

    fn expected_record_root(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<PathBuf, ServerWorkspaceProvisionError> {
        let safe_id = self.validate_record_identity(record)?;
        let base = canonicalize_path(
            &self.base_dir,
            ServerWorkspaceProvisionError::BaseCanonicalizeFailed {
                path: self.base_dir.clone(),
                message: String::new(),
            },
        )?;
        let expected = base.join(safe_id);
        if Path::new(&record.root_or_volume_ref) != expected {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "root does not match the provisioned session".into(),
            ));
        }
        Ok(expected)
    }

    /// Resolve only a workspace this selected instance already owns. Never
    /// create a directory or adopt a persisted root from another executor.
    pub(crate) fn resolve_existing(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<PathBuf, ServerWorkspaceProvisionError> {
        let expected = self.expected_record_root(record)?;
        let actual = canonicalize_path(
            &expected,
            ServerWorkspaceProvisionError::WorkspaceCanonicalizeFailed {
                path: expected.clone(),
                message: String::new(),
            },
        )?;
        if actual != expected || !actual.is_dir() {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "workspace root is not the exact managed directory".into(),
            ));
        }
        Ok(actual)
    }

    /// Internal scratch is disjoint from legal product workspace IDs.
    /// The admitted durable run identity owns this directory, never a session name.
    pub(crate) fn provision_scratch_subrun(
        &self,
        run_id: &str,
    ) -> Result<PathBuf, ServerWorkspaceProvisionError> {
        let run_id = safe_workspace_id(run_id)?;
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
        let scratch = create_managed_directory(&base, ".scratch")?;
        create_managed_directory(&scratch, &run_id)
    }

    /// Create a child directory only beneath the session owned by this provider.
    /// A parent binding may name the session root or its exact run directory.
    pub(crate) fn provision_subrun(
        &self,
        session_id: &str,
        run_id: &str,
        parent_run_id: &str,
        record: &WorkspaceRecord,
        parent_cwd: &Path,
    ) -> Result<PathBuf, ServerWorkspaceProvisionError> {
        let session_root = self.resolve_existing(record)?;
        if record.workspace_id != session_id {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "child session does not match the owned workspace".into(),
            ));
        }
        let parent_id = safe_workspace_id(parent_run_id)?;
        let child_id = safe_workspace_id(run_id)?;
        if child_id == parent_id {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "child run must differ from its parent".into(),
            ));
        }
        let expected_parent = session_root.join(parent_id);
        if parent_cwd != session_root && parent_cwd != expected_parent {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "parent cwd is not its exact managed directory".into(),
            ));
        }
        let actual_parent = canonicalize_path(
            parent_cwd,
            ServerWorkspaceProvisionError::WorkspaceCanonicalizeFailed {
                path: parent_cwd.to_owned(),
                message: String::new(),
            },
        )?;
        if actual_parent != parent_cwd || !actual_parent.is_dir() {
            return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                "parent cwd is not its exact managed directory".into(),
            ));
        }
        create_managed_directory(&session_root, &child_id)
    }

    pub(crate) fn provision(
        &self,
        session_id: &str,
    ) -> Result<ServerWorkspaceRecord, ServerWorkspaceProvisionError> {
        WorkspaceSource::ServerSandbox {
            session_id: session_id.into(),
            executor_id: self.executor_id.clone(),
        }
        .validate()
        .map_err(|error| {
            ServerWorkspaceProvisionError::InvalidWorkspaceRecord(error.to_string())
        })?;
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
        let root = create_managed_directory(&base, &safe_id)?;
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
                    executor_id: self.executor_id.clone(),
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
        let WorkspaceSource::ServerSandbox {
            session_id,
            executor_id,
        } = request.source
        else {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::SourceKindMismatch,
                message: "server sandbox workspace source is required".to_string(),
                workspace_id: Some(request.workspace_id),
            });
        };
        if executor_id != self.executor_id {
            return Err(server_error_to_workspace_error(
                ServerWorkspaceProvisionError::NotThisExecutor,
            ));
        }
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
        self.resolve_existing(workspace)
            .map_err(server_error_to_workspace_error)?;
        workspace.mount_plan(runtime, target)
    }

    async fn cleanup(
        &self,
        workspace: &WorkspaceRecord,
        reason: CleanupReason,
    ) -> Result<(), astra_runtime_env::WorkspaceCleanupError> {
        let error =
            |failure: ServerWorkspaceProvisionError| astra_runtime_env::WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: failure.to_string(),
            };
        let safe_id = self.validate_record_identity(workspace).map_err(error)?;
        // After an owned workspace and its base have been removed there is
        // nothing to clean. Still reject a foreign or forged root first.
        if !self.base_dir.exists() {
            let base = self
                .base_dir
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .zip(self.base_dir.file_name())
                .map(|(parent, name)| parent.join(name))
                .ok_or_else(|| {
                    error(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                        "selected base parent is unavailable".into(),
                    ))
                })?;
            if Path::new(&workspace.root_or_volume_ref) == base.join(safe_id) {
                return Ok(());
            }
            return Err(error(
                ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
                    "root does not match the selected base".into(),
                ),
            ));
        }
        let root = self.expected_record_root(workspace).map_err(error)?;
        if !root.exists() {
            return Ok(());
        }
        let root = self.resolve_existing(workspace).map_err(error)?;
        std::fs::remove_dir_all(&root).map_err(|error| astra_runtime_env::WorkspaceCleanupError {
            workspace_id: workspace.workspace_id.clone(),
            reason,
            message: format!("failed to remove workspace '{}': {error}", root.display()),
        })
    }
}

pub(crate) fn server_error_to_workspace_error(
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

/// Rebase declared directories after the provider proves the parent and child roots.
/// Commands and completion phases retain their original meaning.
pub(crate) fn rebase_completion_checks(
    mut obligations: astra_turn_types::StopHookObligations,
    parent_root: &Path,
    child_root: &Path,
) -> Result<astra_turn_types::StopHookObligations, String> {
    for check in obligations
        .declarations
        .stop
        .iter_mut()
        .chain(obligations.declarations.task_completed.iter_mut())
    {
        let Some(directory) = check.working_dir.as_deref() else {
            continue;
        };
        let path = Path::new(directory);
        if path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        }) {
            return Err("completion check directory contains traversal".into());
        }
        let relative = if path.is_absolute() {
            path.strip_prefix(parent_root)
                .map_err(|_| "completion check directory is outside the parent workspace")?
        } else {
            path
        };
        let relocated = if relative.as_os_str().is_empty() {
            child_root.to_owned()
        } else {
            child_root.join(relative)
        };
        check.working_dir = Some(
            relocated
                .to_str()
                .ok_or("completion check directory is not UTF-8")?
                .to_owned(),
        );
    }
    astra_turn_types::validate_completion_check_declarations(&obligations.declarations)?;
    Ok(obligations)
}

fn create_managed_directory(
    base: &Path,
    safe_id: &str,
) -> Result<PathBuf, ServerWorkspaceProvisionError> {
    let workspace = base.join(safe_id);
    let created = match std::fs::create_dir(&workspace) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(ServerWorkspaceProvisionError::WorkspaceCreateFailed {
                path: workspace.clone(),
                message: error.to_string(),
            });
        }
    };
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
        consumed: !created,
    };
    let root = canonicalize_path(
        &workspace,
        ServerWorkspaceProvisionError::WorkspaceCanonicalizeFailed {
            path: workspace.clone(),
            message: String::new(),
        },
    )?;
    if root != workspace {
        return Err(ServerWorkspaceProvisionError::WorkspaceEscapedBase {
            workspace: root,
            base: base.to_owned(),
        });
    }

    if !root.is_dir() {
        return Err(ServerWorkspaceProvisionError::InvalidWorkspaceRecord(
            "workspace is not a directory".into(),
        ));
    }
    guard.consumed = true;
    Ok(root)
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
    validate_workspace_id(session_id)
        .map_err(|_| ServerWorkspaceProvisionError::InvalidSessionId)?;
    Ok(session_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_creates_workspace_under_base_with_safe_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");

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
    fn subrun_provision_requires_exact_parent_and_selected_executor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = ServerWorkspaceProvisioner::new(temp.path().join("owned"), "executor");
        let session = provider.provision("session").expect("session");
        let child = provider
            .provision_subrun(
                "session",
                "child",
                "parent",
                &session.workspace,
                &session.root,
            )
            .expect("child");
        assert_eq!(child, session.root.join("child"));
        for (child_id, parent_id) in [("../child", "parent"), ("child", "../parent")] {
            assert!(
                provider
                    .provision_subrun(
                        "session",
                        child_id,
                        parent_id,
                        &session.workspace,
                        &session.root
                    )
                    .is_err()
            );
        }
        let grandchild = provider
            .provision_subrun("session", "grandchild", "child", &session.workspace, &child)
            .expect("grandchild");
        assert_eq!(grandchild, session.root.join("grandchild"));
        for parent in [
            session.root.join("sibling"),
            child.join(".."),
            child.clone(),
        ] {
            assert!(
                provider
                    .provision_subrun("session", "rejected", "parent", &session.workspace, &parent)
                    .is_err()
            );
            assert!(!session.root.join("rejected").exists());
        }
        let foreign = ServerWorkspaceProvisioner::new(temp.path().join("owned"), "other");
        assert_eq!(
            foreign
                .provision_subrun("session", "rejected", "child", &session.workspace, &child)
                .expect_err("foreign executor"),
            ServerWorkspaceProvisionError::NotThisExecutor
        );
        assert!(
            provider
                .provision_subrun(
                    "other-session",
                    "rejected",
                    "child",
                    &session.workspace,
                    &child
                )
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&child, session.root.join("alias")).expect("alias");
            assert!(
                provider
                    .provision_subrun(
                        "session",
                        "rejected",
                        "alias",
                        &session.workspace,
                        &session.root.join("alias")
                    )
                    .is_err()
            );
            assert!(!session.root.join("rejected").exists());
        }
    }

    #[test]
    fn completion_check_rebase_preserves_both_phases_and_rejects_external_directories() {
        use astra_turn_types::{
            CompletionCheckDeclarations, CompletionCheckPhase, StopHook, StopHookObligations,
        };
        let check = |label: &str, directory: Option<&str>| StopHook {
            label: label.into(),
            command: "verify --all".into(),
            working_dir: directory.map(str::to_owned),
            depends_on: vec![],
            timeout_secs: Some(10),
            authoritative: true,
        };
        let original = StopHookObligations {
            declarations: CompletionCheckDeclarations {
                stop: vec![
                    check("stop", Some("/owned/parent/tests")),
                    check("implicit", None),
                ],
                task_completed: vec![check("task", Some("relative"))],
            },
            phase: CompletionCheckPhase::TaskCompleted,
        };
        let migrated = rebase_completion_checks(
            original.clone(),
            Path::new("/owned/parent"),
            Path::new("/owned/child"),
        )
        .expect("rebase");
        let mut expected = original.clone();
        expected.declarations.stop[0].working_dir = Some("/owned/child/tests".into());
        expected.declarations.task_completed[0].working_dir = Some("/owned/child/relative".into());
        assert_eq!(migrated, expected);
        for directory in ["../outside", "/owned/parent/../outside", "/owned/sibling"] {
            let mut invalid = original.clone();
            invalid.declarations.task_completed[0].working_dir = Some(directory.into());
            assert!(
                rebase_completion_checks(
                    invalid,
                    Path::new("/owned/parent"),
                    Path::new("/owned/child")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn scratch_never_reuses_product_session_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = ServerWorkspaceProvisioner::new(temp.path().join("owned"), "executor");
        let product = provider.provision("same-name").expect("product");
        std::fs::write(product.root.join("private"), "owner A").expect("private data");
        let scratch = provider
            .provision_scratch_subrun("owner-b-run")
            .expect("scratch");
        assert_eq!(scratch, product.base.join(".scratch/owner-b-run"));
        assert!(!scratch.starts_with(&product.root));
        assert!(!scratch.join("private").exists());
        assert_eq!(
            std::fs::read_to_string(product.root.join("private")).expect("unchanged"),
            "owner A"
        );
        assert!(provider.provision(".scratch").is_err());
        assert!(provider.provision_scratch_subrun("../escape").is_err());
        #[cfg(unix)]
        {
            std::fs::remove_dir(&scratch).expect("remove empty run");
            std::os::unix::fs::symlink(&product.root, &scratch).expect("hostile alias");
            assert!(provider.provision_scratch_subrun("owner-b-run").is_err());
            assert!(product.root.join("private").exists());
        }
    }

    #[test]
    fn provision_rejects_empty_session_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");

        let error = provisioner
            .provision("")
            .expect_err("invalid session id should fail");

        assert_eq!(error, ServerWorkspaceProvisionError::InvalidSessionId);
    }

    #[test]
    fn provision_rejects_session_id_with_unsafe_characters() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");

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
        let provisioner = ServerWorkspaceProvisioner::new(base, "test-executor");

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
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");
        let request = WorkspaceProvisionRequest::server_sandbox("session-1", "test-executor");

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
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");
        let mut request = WorkspaceProvisionRequest::server_sandbox("session-1", "test-executor");
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
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");
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

        assert!(error.message.contains("root does not match"));
    }

    #[tokio::test]
    async fn trait_cleanup_rejects_non_server_workspace_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provisioner =
            ServerWorkspaceProvisioner::new(temp.path().join("workspaces"), "test-executor");
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

        assert!(error.message.contains("not managed by this provider"));
    }

    #[tokio::test]
    async fn trait_cleanup_is_idempotent_when_base_and_root_are_gone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().join("workspaces");
        let provisioner = ServerWorkspaceProvisioner::new(base.clone(), "test-executor");
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
    #[test]
    fn resolve_requires_exact_executor_session_and_root() {
        let directory = tempfile::tempdir().unwrap();
        let owner = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-a");
        let foreign = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-b");
        let record = owner.provision("session-a").unwrap().workspace;
        assert_eq!(
            owner.resolve_existing(&record).unwrap(),
            PathBuf::from(&record.root_or_volume_ref)
        );
        assert_eq!(
            foreign.resolve_existing(&record).unwrap_err(),
            ServerWorkspaceProvisionError::NotThisExecutor
        );
        let mut wrong = record.clone();
        wrong.workspace_id = "session-b".into();
        assert!(owner.resolve_existing(&wrong).is_err());
        wrong = record.clone();
        wrong.source = WorkspaceSource::Scratch;
        assert_eq!(
            owner.resolve_existing(&wrong).unwrap_err(),
            ServerWorkspaceProvisionError::UnsupportedWorkspace
        );
        wrong = record.clone();
        wrong.root_or_volume_ref = directory.path().display().to_string();
        assert!(owner.resolve_existing(&wrong).is_err());
        let mut serialized = serde_json::to_value(&record).unwrap();
        serialized["source"]
            .as_object_mut()
            .unwrap()
            .remove("executor_id");
        assert!(serde_json::from_value::<WorkspaceRecord>(serialized).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_rejects_internal_alias_without_removing_existing_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let provider = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-a");
        let target = provider.provision("session-b").unwrap();
        std::fs::write(target.root.join("source.txt"), "retained").unwrap();
        std::os::unix::fs::symlink(&target.root, directory.path().join("session-a")).unwrap();
        assert!(provider.provision("session-a").is_err());
        assert_eq!(
            std::fs::read_to_string(target.root.join("source.txt")).unwrap(),
            "retained"
        );
        assert!(
            directory
                .path()
                .join("session-a")
                .symlink_metadata()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn foreign_cleanup_never_removes_owned_directory() {
        let directory = tempfile::tempdir().unwrap();
        let owner = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-a");
        let foreign = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-b");
        let record = owner.provision("session-a").unwrap().workspace;
        assert!(
            foreign
                .cleanup(&record, CleanupReason::Completed)
                .await
                .is_err()
        );
        assert!(Path::new(&record.root_or_volume_ref).is_dir());
    }
    #[test]
    fn provisioning_rejects_existing_file_without_removing_it() {
        let directory = tempfile::tempdir().unwrap();
        let provider = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-a");
        let file = directory.path().join("session-a");
        std::fs::write(&file, "retained").unwrap();
        assert!(provider.provision("session-a").is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "retained");
    }
    #[tokio::test]
    async fn existing_workspace_replaced_by_file_cannot_resolve_mount_or_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let provider = ServerWorkspaceProvisioner::new(directory.path().to_owned(), "executor-a");
        let record = provider.provision("session-a").unwrap().workspace;
        let root = Path::new(&record.root_or_volume_ref);
        std::fs::remove_dir(root).unwrap();
        std::fs::write(root, "retained").unwrap();
        assert!(provider.resolve_existing(&record).is_err());
        assert!(
            provider
                .mount_plan(
                    &record,
                    &RuntimeBinding::host_process("server-host"),
                    "/workspace"
                )
                .await
                .is_err()
        );
        assert!(
            provider
                .cleanup(&record, CleanupReason::Completed)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(root).unwrap(), "retained");
    }
}
