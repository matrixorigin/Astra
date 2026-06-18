use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use astra_runtime_env::{
    CleanupReason, RuntimeBinding, WorkspaceAuthority, WorkspaceBindingKind, WorkspaceCleanupError,
    WorkspaceMountPlan, WorkspaceOwnerScope, WorkspacePersistence, WorkspaceProvisionError,
    WorkspaceProvisionErrorKind, WorkspaceProvisionRequest, WorkspaceProvisioner, WorkspaceRecord,
    WorkspaceSource,
};
use async_trait::async_trait;

#[derive(Clone)]
pub(crate) struct CloudWorkspaceProvisioner {
    storage: Arc<dyn CloudWorkspaceStorage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilesystemCloudWorkspaceStorage {
    base_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaterializedCloudSourceKind {
    UploadedSnapshot,
    Template,
    DatasetBundle,
    ArtifactBundle,
}

impl MaterializedCloudSourceKind {
    fn storage_category(self) -> &'static str {
        match self {
            Self::UploadedSnapshot => "snapshots",
            Self::Template => "templates",
            Self::DatasetBundle => "datasets",
            Self::ArtifactBundle => "artifacts",
        }
    }
}

trait CloudWorkspaceStorage: Send + Sync {
    fn ensure_persistent_volume(
        &self,
        workspace_id: &str,
        volume_id: &str,
    ) -> Result<PathBuf, WorkspaceProvisionError>;

    fn materialized_source_root(
        &self,
        workspace_id: &str,
        source_kind: MaterializedCloudSourceKind,
        source_id: &str,
        requested_root: Option<&str>,
    ) -> Result<PathBuf, WorkspaceProvisionError>;

    fn create_session_clone_from_source(
        &self,
        workspace_id: &str,
        source_root: &Path,
    ) -> Result<PathBuf, WorkspaceProvisionError>;

    fn create_git_checkout(
        &self,
        workspace_id: &str,
        repository: &str,
        reference: Option<&str>,
    ) -> Result<PathBuf, WorkspaceProvisionError>;

    fn create_scratch_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<PathBuf, WorkspaceProvisionError>;

    fn cleanup_session_workspace(
        &self,
        workspace: &WorkspaceRecord,
        reason: CleanupReason,
    ) -> Result<(), WorkspaceCleanupError>;
}

impl CloudWorkspaceProvisioner {
    pub(crate) fn from_env() -> Self {
        let base_dir = std::env::var("ASTRA_CLOUD_WORKSPACES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-cloud-workspaces"));
        Self {
            storage: Arc::new(FilesystemCloudWorkspaceStorage { base_dir }),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self {
            storage: Arc::new(FilesystemCloudWorkspaceStorage { base_dir }),
        }
    }

    #[cfg(test)]
    fn with_storage(storage: Arc<dyn CloudWorkspaceStorage>) -> Self {
        Self { storage }
    }

    fn record(
        &self,
        request: WorkspaceProvisionRequest,
        root: PathBuf,
        source: WorkspaceSource,
    ) -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: request.workspace_id,
            owner_scope: request.owner_scope,
            kind: request.kind,
            authority: request.authority,
            root_or_volume_ref: root.display().to_string(),
            source,
            persistence: request.persistence,
            revision: "1".to_string(),
            display_name: request
                .display_name
                .unwrap_or_else(|| default_display_name(request.kind).to_string()),
        }
    }

    fn provision_persistent_volume(
        &self,
        request: WorkspaceProvisionRequest,
        volume_id: String,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        let root = self
            .storage
            .ensure_persistent_volume(&request.workspace_id, &volume_id)?;
        Ok(self.record(
            request,
            root,
            WorkspaceSource::PersistentVolume { volume_id },
        ))
    }

    fn provision_uploaded_snapshot(
        &self,
        request: WorkspaceProvisionRequest,
        artifact_id: String,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        self.provision_materialized_source(
            request,
            MaterializedCloudSourceKind::UploadedSnapshot,
            artifact_id.clone(),
            WorkspaceSource::UploadedSnapshot { artifact_id },
        )
    }

    fn provision_materialized_source(
        &self,
        request: WorkspaceProvisionRequest,
        source_kind: MaterializedCloudSourceKind,
        source_id: String,
        source: WorkspaceSource,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        let source_root = self.storage.materialized_source_root(
            &request.workspace_id,
            source_kind,
            &source_id,
            request.requested_root.as_deref(),
        )?;
        if request.authority == WorkspaceAuthority::ReadWrite {
            let root = self
                .storage
                .create_session_clone_from_source(&request.workspace_id, &source_root)?;
            Ok(self.record(request, root, source))
        } else {
            Ok(self.record(request, source_root, source))
        }
    }

    fn provision_git_checkout(
        &self,
        request: WorkspaceProvisionRequest,
        repository: String,
        reference: Option<String>,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        if repository.trim().is_empty() {
            return Err(WorkspaceProvisionError::unavailable(
                &request.workspace_id,
                "git repository must not be empty",
            ));
        }
        let root = self.storage.create_git_checkout(
            &request.workspace_id,
            &repository,
            reference.as_deref(),
        )?;
        Ok(self.record(
            request,
            root,
            WorkspaceSource::GitCheckout {
                repository,
                reference,
            },
        ))
    }

    fn provision_scratch(
        &self,
        request: WorkspaceProvisionRequest,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        let root = self
            .storage
            .create_scratch_workspace(&request.workspace_id)?;
        Ok(self.record(request, root, WorkspaceSource::Scratch))
    }
}

impl FilesystemCloudWorkspaceStorage {
    fn canonical_base(&self) -> Result<PathBuf, WorkspaceProvisionError> {
        fs::create_dir_all(&self.base_dir).map_err(|error| {
            WorkspaceProvisionError::unavailable(
                "cloud_workspace_base",
                format!(
                    "failed to create cloud workspace base '{}': {error}",
                    self.base_dir.display()
                ),
            )
        })?;
        self.base_dir.canonicalize().map_err(|error| {
            WorkspaceProvisionError::unavailable(
                "cloud_workspace_base",
                format!(
                    "failed to resolve cloud workspace base '{}': {error}",
                    self.base_dir.display()
                ),
            )
        })
    }

    fn path_under_base(
        &self,
        base: &Path,
        category: &str,
        id: &str,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        validate_safe_segment(id)?;
        Ok(base.join(category).join(id))
    }

    fn canonical_workspace_path(
        &self,
        base: &Path,
        workspace_id: &str,
        path: &Path,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        let root = path.canonicalize().map_err(|error| {
            WorkspaceProvisionError::unavailable(
                workspace_id,
                format!("failed to resolve workspace '{}': {error}", path.display()),
            )
        })?;
        if !root.starts_with(base) {
            return Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::MountFailed,
                message: format!(
                    "workspace '{}' escaped cloud workspace base '{}'",
                    root.display(),
                    base.display()
                ),
                workspace_id: Some(workspace_id.to_string()),
            });
        }
        if !root.is_dir() {
            return Err(WorkspaceProvisionError::unavailable(
                workspace_id,
                format!("workspace '{}' is not a directory", root.display()),
            ));
        }
        Ok(root)
    }
}

impl CloudWorkspaceStorage for FilesystemCloudWorkspaceStorage {
    fn ensure_persistent_volume(
        &self,
        workspace_id: &str,
        volume_id: &str,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        let base = self.canonical_base()?;
        let path = self.path_under_base(&base, "volumes", volume_id)?;
        fs::create_dir_all(&path).map_err(|error| {
            WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "failed to create persistent workspace volume '{}': {error}",
                    path.display()
                ),
            )
        })?;
        self.canonical_workspace_path(&base, workspace_id, &path)
    }

    fn materialized_source_root(
        &self,
        workspace_id: &str,
        source_kind: MaterializedCloudSourceKind,
        source_id: &str,
        requested_root: Option<&str>,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        let base = self.canonical_base()?;
        validate_safe_segment(source_id)?;
        let source_path = requested_root
            .map(PathBuf::from)
            .unwrap_or_else(|| base.join(source_kind.storage_category()).join(source_id));
        self.canonical_workspace_path(&base, workspace_id, &source_path)
    }

    fn create_session_clone_from_source(
        &self,
        workspace_id: &str,
        source_root: &Path,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        let base = self.canonical_base()?;
        let clone_path = self.path_under_base(&base, "clones", workspace_id)?;
        // Use atomic directory creation instead of exists()+copy to prevent
        // TOCTOU races when two provisioner calls target the same workspace.
        fs::create_dir_all(clone_path.parent().unwrap_or(&clone_path)).map_err(|error| {
            WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "failed to create session clone workspace parent '{}': {error}",
                    clone_path.display()
                ),
            )
        })?;
        fs::create_dir(&clone_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                WorkspaceProvisionError::unavailable(
                    workspace_id,
                    format!(
                        "session clone workspace '{}' already exists",
                        clone_path.display()
                    ),
                )
            } else {
                WorkspaceProvisionError::unavailable(
                    workspace_id,
                    format!(
                        "failed to create session clone workspace '{}': {error}",
                        clone_path.display()
                    ),
                )
            }
        })?;
        copy_dir_recursive(source_root, &clone_path).map_err(|error| {
            // Clean up partial clone to avoid orphan directories.
            let _ = fs::remove_dir_all(&clone_path);
            WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "failed to clone materialized workspace source '{}' into '{}': {error}",
                    source_root.display(),
                    clone_path.display()
                ),
            )
        })?;
        self.canonical_workspace_path(&base, workspace_id, &clone_path)
    }

    fn create_git_checkout(
        &self,
        workspace_id: &str,
        repository: &str,
        reference: Option<&str>,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        let base = self.canonical_base()?;
        let checkout_path = self.path_under_base(&base, "checkouts", workspace_id)?;
        if checkout_path.exists() {
            return Err(WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "git checkout workspace '{}' already exists",
                    checkout_path.display()
                ),
            ));
        }
        let output = Command::new("git")
            .arg("clone")
            .arg("--")
            .arg(repository)
            .arg(&checkout_path)
            .output()
            .map_err(|error| {
                WorkspaceProvisionError::unavailable(
                    workspace_id,
                    format!("failed to start git clone: {error}"),
                )
            })?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&checkout_path);
            return Err(WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "git clone failed with status {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }
        if let Some(reference) = reference.filter(|value| !value.trim().is_empty()) {
            let output = Command::new("git")
                .arg("-C")
                .arg(&checkout_path)
                .arg("checkout")
                .arg("--")
                .arg(reference)
                .output()
                .map_err(|error| {
                    WorkspaceProvisionError::unavailable(
                        workspace_id,
                        format!("failed to start git checkout: {error}"),
                    )
                })?;
            if !output.status.success() {
                let _ = fs::remove_dir_all(&checkout_path);
                return Err(WorkspaceProvisionError::unavailable(
                    workspace_id,
                    format!(
                        "git checkout '{reference}' failed with status {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ));
            }
        }
        self.canonical_workspace_path(&base, workspace_id, &checkout_path)
    }

    fn create_scratch_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<PathBuf, WorkspaceProvisionError> {
        let base = self.canonical_base()?;
        let scratch_path = self.path_under_base(&base, "scratch", workspace_id)?;
        if scratch_path.exists() {
            return Err(WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "scratch workspace '{}' already exists",
                    scratch_path.display()
                ),
            ));
        }
        fs::create_dir_all(&scratch_path).map_err(|error| {
            WorkspaceProvisionError::unavailable(
                workspace_id,
                format!(
                    "failed to create scratch workspace '{}': {error}",
                    scratch_path.display()
                ),
            )
        })?;
        self.canonical_workspace_path(&base, workspace_id, &scratch_path)
    }

    fn cleanup_session_workspace(
        &self,
        workspace: &WorkspaceRecord,
        reason: CleanupReason,
    ) -> Result<(), WorkspaceCleanupError> {
        let base = self
            .base_dir
            .canonicalize()
            .map_err(|error| WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: format!(
                    "failed to resolve cloud workspace base '{}': {error}",
                    self.base_dir.display()
                ),
            })?;
        let root = PathBuf::from(&workspace.root_or_volume_ref)
            .canonicalize()
            .map_err(|error| WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: format!(
                    "failed to resolve workspace '{}': {error}",
                    workspace.root_or_volume_ref
                ),
            })?;
        if !root.starts_with(&base) {
            return Err(WorkspaceCleanupError {
                workspace_id: workspace.workspace_id.clone(),
                reason,
                message: format!(
                    "workspace '{}' is outside cloud workspace base '{}'",
                    root.display(),
                    base.display()
                ),
            });
        }
        fs::remove_dir_all(&root).map_err(|error| WorkspaceCleanupError {
            workspace_id: workspace.workspace_id.clone(),
            reason,
            message: format!("failed to remove workspace '{}': {error}", root.display()),
        })
    }
}

#[async_trait]
impl WorkspaceProvisioner for CloudWorkspaceProvisioner {
    async fn provision(
        &self,
        request: WorkspaceProvisionRequest,
    ) -> Result<WorkspaceRecord, WorkspaceProvisionError> {
        request.validate()?;
        match request.source.clone() {
            WorkspaceSource::PersistentVolume { volume_id } => {
                self.provision_persistent_volume(request, volume_id)
            }
            WorkspaceSource::UploadedSnapshot { artifact_id } => {
                self.provision_uploaded_snapshot(request, artifact_id)
            }
            WorkspaceSource::Template { template_id } => self.provision_materialized_source(
                request,
                MaterializedCloudSourceKind::Template,
                template_id.clone(),
                WorkspaceSource::Template { template_id },
            ),
            WorkspaceSource::DatasetBundle { dataset_id } => self.provision_materialized_source(
                request,
                MaterializedCloudSourceKind::DatasetBundle,
                dataset_id.clone(),
                WorkspaceSource::DatasetBundle { dataset_id },
            ),
            WorkspaceSource::ArtifactBundle { artifact_id } => self.provision_materialized_source(
                request,
                MaterializedCloudSourceKind::ArtifactBundle,
                artifact_id.clone(),
                WorkspaceSource::ArtifactBundle { artifact_id },
            ),
            WorkspaceSource::GitCheckout {
                repository,
                reference,
            } => self.provision_git_checkout(request, repository, reference),
            WorkspaceSource::Scratch => self.provision_scratch(request),
            other => Err(WorkspaceProvisionError {
                kind: WorkspaceProvisionErrorKind::SourceKindMismatch,
                message: format!("cloud workspace provisioner does not support source {other:?}"),
                workspace_id: Some(request.workspace_id),
            }),
        }
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
    ) -> Result<(), WorkspaceCleanupError> {
        if matches!(
            workspace.persistence,
            WorkspacePersistence::Persistent | WorkspacePersistence::ImmutableSnapshot
        ) {
            return Ok(());
        }
        self.storage.cleanup_session_workspace(workspace, reason)
    }
}

fn default_display_name(kind: WorkspaceBindingKind) -> &'static str {
    match kind {
        WorkspaceBindingKind::CloudWorkspace => "Cloud workspace",
        _ => "Workspace",
    }
}

fn validate_safe_segment(segment: &str) -> Result<(), WorkspaceProvisionError> {
    astra_runtime_env::validate_workspace_id(segment)
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source, &target)?;
        } else if file_type.is_file() {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::RuntimeBinding;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeCloudWorkspaceStorage {
        calls: Mutex<Vec<String>>,
    }

    impl CloudWorkspaceStorage for FakeCloudWorkspaceStorage {
        fn ensure_persistent_volume(
            &self,
            workspace_id: &str,
            volume_id: &str,
        ) -> Result<PathBuf, WorkspaceProvisionError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("volume:{workspace_id}:{volume_id}"));
            Ok(PathBuf::from(format!("/service/volumes/{volume_id}")))
        }

        fn materialized_source_root(
            &self,
            workspace_id: &str,
            source_kind: MaterializedCloudSourceKind,
            source_id: &str,
            _requested_root: Option<&str>,
        ) -> Result<PathBuf, WorkspaceProvisionError> {
            self.calls.lock().expect("calls").push(format!(
                "materialized:{source_kind:?}:{workspace_id}:{source_id}"
            ));
            Ok(PathBuf::from(format!(
                "/service/{}/{source_id}",
                source_kind.storage_category()
            )))
        }

        fn create_session_clone_from_source(
            &self,
            workspace_id: &str,
            source_root: &Path,
        ) -> Result<PathBuf, WorkspaceProvisionError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("clone:{workspace_id}:{}", source_root.display()));
            Ok(PathBuf::from(format!("/service/clones/{workspace_id}")))
        }

        fn create_git_checkout(
            &self,
            workspace_id: &str,
            repository: &str,
            reference: Option<&str>,
        ) -> Result<PathBuf, WorkspaceProvisionError> {
            self.calls.lock().expect("calls").push(format!(
                "checkout:{workspace_id}:{repository}:{}",
                reference.unwrap_or("")
            ));
            Ok(PathBuf::from(format!("/service/checkouts/{workspace_id}")))
        }

        fn create_scratch_workspace(
            &self,
            workspace_id: &str,
        ) -> Result<PathBuf, WorkspaceProvisionError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("scratch:{workspace_id}"));
            Ok(PathBuf::from(format!("/service/scratch/{workspace_id}")))
        }

        fn cleanup_session_workspace(
            &self,
            workspace: &WorkspaceRecord,
            _reason: CleanupReason,
        ) -> Result<(), WorkspaceCleanupError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("cleanup:{}", workspace.workspace_id));
            Ok(())
        }
    }

    fn persistent_volume_request(volume_id: &str) -> WorkspaceProvisionRequest {
        WorkspaceProvisionRequest {
            workspace_id: "workspace-1".to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            source: WorkspaceSource::PersistentVolume {
                volume_id: volume_id.to_string(),
            },
            persistence: WorkspacePersistence::Persistent,
            requested_root: None,
            display_name: None,
        }
    }

    fn scratch_request() -> WorkspaceProvisionRequest {
        WorkspaceProvisionRequest {
            workspace_id: "scratch-workspace-1".to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            source: WorkspaceSource::Scratch,
            persistence: WorkspacePersistence::Session,
            requested_root: None,
            display_name: None,
        }
    }

    fn uploaded_snapshot_request(
        artifact_id: &str,
        authority: WorkspaceAuthority,
        persistence: WorkspacePersistence,
    ) -> WorkspaceProvisionRequest {
        WorkspaceProvisionRequest {
            workspace_id: "snapshot-workspace-1".to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority,
            source: WorkspaceSource::UploadedSnapshot {
                artifact_id: artifact_id.to_string(),
            },
            persistence,
            requested_root: None,
            display_name: None,
        }
    }

    fn materialized_source_request(
        workspace_id: &str,
        source: WorkspaceSource,
        authority: WorkspaceAuthority,
        persistence: WorkspacePersistence,
    ) -> WorkspaceProvisionRequest {
        WorkspaceProvisionRequest {
            workspace_id: workspace_id.to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority,
            source,
            persistence,
            requested_root: None,
            display_name: None,
        }
    }

    #[tokio::test]
    async fn provisioner_uses_replaceable_storage_boundary_for_volume_records() {
        let storage = Arc::new(FakeCloudWorkspaceStorage::default());
        let provider = CloudWorkspaceProvisioner::with_storage(storage.clone());

        let record = provider
            .provision(persistent_volume_request("volume-1"))
            .await
            .expect("volume record");

        assert_eq!(record.workspace_id, "workspace-1");
        assert_eq!(record.root_or_volume_ref, "/service/volumes/volume-1");
        assert_eq!(
            *storage.calls.lock().expect("calls"),
            vec!["volume:workspace-1:volume-1".to_string()]
        );
    }

    #[tokio::test]
    async fn scratch_source_creates_generic_cloud_workspace_record() {
        let storage = Arc::new(FakeCloudWorkspaceStorage::default());
        let provider = CloudWorkspaceProvisioner::with_storage(storage.clone());

        let record = provider
            .provision(scratch_request())
            .await
            .expect("scratch record");

        assert_eq!(record.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(record.source, WorkspaceSource::Scratch);
        assert_eq!(
            record.root_or_volume_ref,
            "/service/scratch/scratch-workspace-1"
        );
        assert_eq!(
            *storage.calls.lock().expect("calls"),
            vec!["scratch:scratch-workspace-1".to_string()]
        );
    }

    #[tokio::test]
    async fn materialized_cloud_sources_use_single_storage_boundary() {
        let storage = Arc::new(FakeCloudWorkspaceStorage::default());
        let provider = CloudWorkspaceProvisioner::with_storage(storage.clone());

        let cases = [
            (
                "template-workspace-1",
                WorkspaceSource::Template {
                    template_id: "template-1".to_string(),
                },
                "materialized:Template:template-workspace-1:template-1",
                "/service/templates/template-1",
            ),
            (
                "dataset-workspace-1",
                WorkspaceSource::DatasetBundle {
                    dataset_id: "dataset-1".to_string(),
                },
                "materialized:DatasetBundle:dataset-workspace-1:dataset-1",
                "/service/datasets/dataset-1",
            ),
            (
                "artifact-workspace-1",
                WorkspaceSource::ArtifactBundle {
                    artifact_id: "artifact-1".to_string(),
                },
                "materialized:ArtifactBundle:artifact-workspace-1:artifact-1",
                "/service/artifacts/artifact-1",
            ),
        ];

        for (workspace_id, source, expected_call, expected_root) in cases {
            let record = provider
                .provision(materialized_source_request(
                    workspace_id,
                    source.clone(),
                    WorkspaceAuthority::ReadOnly,
                    WorkspacePersistence::ImmutableSnapshot,
                ))
                .await
                .expect("materialized source record");

            assert_eq!(record.kind, WorkspaceBindingKind::CloudWorkspace);
            assert_eq!(record.source, source);
            assert_eq!(record.root_or_volume_ref, expected_root);
            assert!(
                storage
                    .calls
                    .lock()
                    .expect("calls")
                    .contains(&expected_call.to_string()),
                "missing call {expected_call}"
            );
        }
    }

    #[tokio::test]
    async fn read_write_materialized_source_creates_session_clone() {
        let storage = Arc::new(FakeCloudWorkspaceStorage::default());
        let provider = CloudWorkspaceProvisioner::with_storage(storage.clone());

        let record = provider
            .provision(materialized_source_request(
                "template-workspace-1",
                WorkspaceSource::Template {
                    template_id: "template-1".to_string(),
                },
                WorkspaceAuthority::ReadWrite,
                WorkspacePersistence::Session,
            ))
            .await
            .expect("template clone");

        assert_eq!(
            record.root_or_volume_ref,
            "/service/clones/template-workspace-1"
        );
        assert_eq!(
            *storage.calls.lock().expect("calls"),
            vec![
                "materialized:Template:template-workspace-1:template-1".to_string(),
                "clone:template-workspace-1:/service/templates/template-1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn persistent_volume_creates_cloud_workspace_record() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));

        let record = provider
            .provision(persistent_volume_request("volume-1"))
            .await
            .expect("volume provisioned");

        assert_eq!(record.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(record.authority, WorkspaceAuthority::ReadWrite);
        assert_eq!(record.persistence, WorkspacePersistence::Persistent);
        assert!(Path::new(&record.root_or_volume_ref).is_dir());

        provider
            .cleanup(&record, CleanupReason::Completed)
            .await
            .expect("persistent cleanup should be a no-op");
        assert!(Path::new(&record.root_or_volume_ref).is_dir());
    }

    #[tokio::test]
    async fn uploaded_snapshot_requires_existing_artifact_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));

        let error = provider
            .provision(uploaded_snapshot_request(
                "missing-snapshot",
                WorkspaceAuthority::ReadOnly,
                WorkspacePersistence::ImmutableSnapshot,
            ))
            .await
            .expect_err("missing snapshot should fail");

        assert_eq!(
            error.kind,
            WorkspaceProvisionErrorKind::WorkspaceUnavailable
        );
    }

    #[tokio::test]
    async fn read_only_uploaded_snapshot_uses_immutable_snapshot_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let snapshot = temp
            .path()
            .join("cloud")
            .join("snapshots")
            .join("artifact-1");
        fs::create_dir_all(&snapshot).expect("snapshot dir");
        fs::write(snapshot.join("README.md"), "snapshot").expect("snapshot file");

        let record = provider
            .provision(uploaded_snapshot_request(
                "artifact-1",
                WorkspaceAuthority::ReadOnly,
                WorkspacePersistence::ImmutableSnapshot,
            ))
            .await
            .expect("snapshot provisioned");

        assert_eq!(record.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(record.authority, WorkspaceAuthority::ReadOnly);
        assert_eq!(record.persistence, WorkspacePersistence::ImmutableSnapshot);
        assert_eq!(
            Path::new(&record.root_or_volume_ref)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("artifact-1")
        );
        assert!(
            !record
                .mount_plan(&RuntimeBinding::oci_container("runtime"), "/workspace")
                .expect("mount plan")
                .persistent
        );
    }

    #[tokio::test]
    async fn read_write_uploaded_snapshot_creates_session_clone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let snapshot = temp
            .path()
            .join("cloud")
            .join("snapshots")
            .join("artifact-1");
        fs::create_dir_all(&snapshot).expect("snapshot dir");
        fs::write(snapshot.join("data.txt"), "snapshot").expect("snapshot file");

        let record = provider
            .provision(uploaded_snapshot_request(
                "artifact-1",
                WorkspaceAuthority::ReadWrite,
                WorkspacePersistence::Session,
            ))
            .await
            .expect("clone provisioned");

        assert!(record.root_or_volume_ref.contains("/clones/"));
        assert_eq!(
            fs::read_to_string(Path::new(&record.root_or_volume_ref).join("data.txt"))
                .expect("cloned file"),
            "snapshot"
        );
        provider
            .cleanup(&record, CleanupReason::Completed)
            .await
            .expect("clone cleanup");
        assert!(!Path::new(&record.root_or_volume_ref).exists());
    }

    #[tokio::test]
    async fn read_write_uploaded_snapshot_rejects_existing_session_clone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let snapshot = temp
            .path()
            .join("cloud")
            .join("snapshots")
            .join("artifact-1");
        fs::create_dir_all(&snapshot).expect("snapshot dir");
        let clone = temp
            .path()
            .join("cloud")
            .join("clones")
            .join("snapshot-workspace-1");
        fs::create_dir_all(&clone).expect("preexisting clone dir");

        let error = provider
            .provision(uploaded_snapshot_request(
                "artifact-1",
                WorkspaceAuthority::ReadWrite,
                WorkspacePersistence::Session,
            ))
            .await
            .expect_err("preexisting session clone should fail");

        assert_eq!(
            error.kind,
            WorkspaceProvisionErrorKind::WorkspaceUnavailable
        );
        assert!(error.message.contains("session clone workspace"));
    }

    #[tokio::test]
    async fn read_only_dataset_bundle_uses_materialized_bundle_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let dataset = temp.path().join("cloud").join("datasets").join("dataset-1");
        fs::create_dir_all(&dataset).expect("dataset dir");
        fs::write(dataset.join("manifest.json"), "{}").expect("dataset file");

        let record = provider
            .provision(materialized_source_request(
                "dataset-workspace-1",
                WorkspaceSource::DatasetBundle {
                    dataset_id: "dataset-1".to_string(),
                },
                WorkspaceAuthority::ReadOnly,
                WorkspacePersistence::ImmutableSnapshot,
            ))
            .await
            .expect("dataset provisioned");

        assert_eq!(
            record.source,
            WorkspaceSource::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            }
        );
        assert_eq!(
            PathBuf::from(&record.root_or_volume_ref),
            dataset.canonicalize().expect("dataset")
        );
        assert_eq!(record.authority, WorkspaceAuthority::ReadOnly);
    }

    #[tokio::test]
    async fn read_write_artifact_bundle_creates_session_clone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let artifact = temp
            .path()
            .join("cloud")
            .join("artifacts")
            .join("artifact-1");
        fs::create_dir_all(&artifact).expect("artifact dir");
        fs::write(artifact.join("result.txt"), "artifact").expect("artifact file");

        let record = provider
            .provision(materialized_source_request(
                "artifact-workspace-1",
                WorkspaceSource::ArtifactBundle {
                    artifact_id: "artifact-1".to_string(),
                },
                WorkspaceAuthority::ReadWrite,
                WorkspacePersistence::Session,
            ))
            .await
            .expect("artifact clone");

        assert!(record.root_or_volume_ref.contains("/clones/"));
        assert_eq!(
            fs::read_to_string(Path::new(&record.root_or_volume_ref).join("result.txt"))
                .expect("cloned artifact file"),
            "artifact"
        );
    }

    #[tokio::test]
    async fn materialized_source_requested_root_must_stay_under_cloud_base() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let outside = temp.path().join("outside-template");
        fs::create_dir_all(&outside).expect("outside dir");
        let mut request = materialized_source_request(
            "template-workspace-1",
            WorkspaceSource::Template {
                template_id: "template-1".to_string(),
            },
            WorkspaceAuthority::ReadOnly,
            WorkspacePersistence::ImmutableSnapshot,
        );
        request.requested_root = Some(outside.display().to_string());

        let error = provider
            .provision(request)
            .await
            .expect_err("outside source root should fail");

        assert_eq!(error.kind, WorkspaceProvisionErrorKind::MountFailed);
        assert!(error.message.contains("escaped cloud workspace base"));
    }

    #[tokio::test]
    async fn git_checkout_rejects_empty_repository_before_launch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let request = WorkspaceProvisionRequest {
            workspace_id: "checkout-1".to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            source: WorkspaceSource::GitCheckout {
                repository: String::new(),
                reference: None,
            },
            persistence: WorkspacePersistence::Session,
            requested_root: None,
            display_name: None,
        };

        let error = provider
            .provision(request)
            .await
            .expect_err("empty repository should fail");

        assert_eq!(
            error.kind,
            WorkspaceProvisionErrorKind::WorkspaceUnavailable
        );
    }

    #[tokio::test]
    async fn git_checkout_clones_local_repository_when_git_is_available() {
        if !git_available() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        assert!(git(&repo, ["init"]));
        assert!(git(&repo, ["config", "user.email", "astra@example.com"]));
        assert!(git(&repo, ["config", "user.name", "Astra Test"]));
        fs::write(repo.join("README.md"), "checkout").expect("repo file");
        assert!(git(&repo, ["add", "README.md"]));
        assert!(git(&repo, ["commit", "-m", "initial"]));

        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let request = WorkspaceProvisionRequest {
            workspace_id: "checkout-1".to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            source: WorkspaceSource::GitCheckout {
                repository: repo.display().to_string(),
                reference: None,
            },
            persistence: WorkspacePersistence::Session,
            requested_root: None,
            display_name: None,
        };

        let record = provider.provision(request).await.expect("git checkout");

        assert_eq!(record.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(
            fs::read_to_string(Path::new(&record.root_or_volume_ref).join("README.md"))
                .expect("cloned file"),
            "checkout"
        );
    }

    #[tokio::test]
    async fn cleanup_rejects_workspace_outside_cloud_base() {
        let temp = tempfile::tempdir().expect("tempdir");
        let provider = CloudWorkspaceProvisioner::new(temp.path().join("cloud"));
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        let mut record = provider
            .provision(persistent_volume_request("volume-1"))
            .await
            .expect("volume");
        record.persistence = WorkspacePersistence::Session;
        record.root_or_volume_ref = outside.display().to_string();

        let error = provider
            .cleanup(&record, CleanupReason::Failed)
            .await
            .expect_err("outside cleanup should fail");

        assert!(error.message.contains("outside cloud workspace base"));
    }

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) -> bool {
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
