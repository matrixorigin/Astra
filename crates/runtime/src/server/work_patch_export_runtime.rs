use std::path::Path;

use astra_core::SharedPool;
use astra_services::work::{
    DatabaseWorkRepository, GraphRevision, NewWorkPatchArtifact, WorkBranchId, WorkBranchRevision,
    WorkChangeRef, WorkId, WorkOwnerId, WorkPatchArtifact, WorkPatchArtifactId, WorkPatchFormat,
    WorkProviderInvocationRef, WorkRepository, WorkRepositoryError,
};
use astra_services::{
    DatabaseSessionArtifactStore, DatabaseWorkspaceRecordStore, SessionArtifactJsonRecord,
    SessionArtifactJsonStore, SessionArtifactReference, SessionArtifactReferenceKind,
    WorkspaceRecordStore,
};
use astra_tools::patch_materialization::{GitWorktreePatchExportError, export_git_worktree_patch};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub(crate) struct WorkPatchExportCommand {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub expected_branch_revision: WorkBranchRevision,
    pub expected_graph_revision: GraphRevision,
}

#[derive(Debug, Error)]
pub(crate) enum WorkPatchExportError {
    #[error("Work branch was not found")]
    NotFound,
    #[error("Work patch export basis changed")]
    BasisConflict,
    #[error("Work patch export request identity was reused with another payload")]
    IdempotencyConflict,
    #[error("Work workspace has no exportable changes")]
    NoChanges,
    #[error("Work patch exceeds the artifact limit")]
    TooLarge,
    #[error("Work patch payload cannot satisfy the UTF-8 artifact contract")]
    PayloadUnsupported,
    #[error("Work patch export unavailable: {0}")]
    Unavailable(String),
}

pub(crate) async fn export_work_patch(
    pool: SharedPool,
    command: WorkPatchExportCommand,
) -> Result<WorkPatchArtifact, WorkPatchExportError> {
    let repository = DatabaseWorkRepository::new(pool.clone());
    let binding = repository
        .load_branch_runtime_binding(&command.owner_id, &command.work_id, &command.branch_id)
        .await
        .map_err(map_repository_error)?;
    let subject = repository
        .load_branch_subject(&command.owner_id, &command.work_id, &command.branch_id)
        .await
        .map_err(map_repository_error)?
        .ok_or(WorkPatchExportError::BasisConflict)?;
    if binding.branch_revision != command.expected_branch_revision
        || binding.graph_revision != command.expected_graph_revision
        || subject.branch_revision != command.expected_branch_revision
        || subject.graph_revision != command.expected_graph_revision
    {
        return Err(WorkPatchExportError::BasisConflict);
    }
    let workspace = DatabaseWorkspaceRecordStore::new(pool.clone())
        .load_workspace_record(command.owner_id.as_str(), binding.session_id.as_str())
        .await
        .map_err(|error| WorkPatchExportError::Unavailable(error.to_string()))?
        .filter(|entry| entry.session_id.as_deref() == Some(binding.session_id.as_str()))
        .filter(|entry| {
            entry.record.kind == astra_runtime_env::WorkspaceBindingKind::ServerSandbox
                && entry.record.authority == astra_runtime_env::WorkspaceAuthority::ReadWrite
                && entry.record.persistence == astra_runtime_env::WorkspacePersistence::Session
        })
        .ok_or_else(|| WorkPatchExportError::Unavailable("workspace is unavailable".into()))?;
    let exported = export_git_worktree_patch(Path::new(&workspace.record.root_or_volume_ref))
        .await
        .map_err(map_provider_error)?;
    if exported.result_subject_revision != subject.subject_revision {
        return Err(WorkPatchExportError::BasisConflict);
    }

    let identity = export_identity(&command);
    let patch_artifact_id = WorkPatchArtifactId::parse(format!("patch-{identity}"))
        .expect("SHA-256 export identity is a valid patch artifact id");
    let payload_artifact_id = format!("patch-payload-{identity}");
    let invocation = WorkProviderInvocationRef::parse(format!("server-git-export:{identity}"))
        .expect("SHA-256 export identity is a valid provider invocation ref");
    let patch =
        String::from_utf8(exported.patch).map_err(|_| WorkPatchExportError::PayloadUnsupported)?;
    let patch_digest = format!("{:x}", Sha256::digest(patch.as_bytes()));
    let patch_bytes = patch.len();
    let artifact_content = serde_json::json!({
        "kind": "patch",
        "content_type": "text/x-diff",
        "encoding": "utf-8",
        "data": patch,
        "byte_size": patch_bytes,
        "sha256": patch_digest,
    });
    persist_payload(
        &pool,
        command.owner_id.as_str(),
        binding.session_id.as_str(),
        &payload_artifact_id,
        patch_artifact_id.as_str(),
        artifact_content,
    )
    .await?;
    repository
        .record_patch_artifact(NewWorkPatchArtifact {
            owner_id: command.owner_id,
            work_id: command.work_id,
            branch_id: command.branch_id,
            patch_artifact_id,
            payload_artifact_id,
            expected_branch_revision: command.expected_branch_revision,
            expected_graph_revision: command.expected_graph_revision,
            expected_subject_record_revision: subject.subject_record_revision,
            subject_ref: subject.subject_ref,
            base_subject_revision: exported.base_subject_revision,
            result_subject_revision: exported.result_subject_revision,
            format: WorkPatchFormat::UnifiedDiffV1,
            provider_invocation_ref: invocation,
            source_ref: command.request_id,
        })
        .await
        .map_err(map_repository_error)
}

async fn persist_payload(
    pool: &SharedPool,
    owner_id: &str,
    session_id: &str,
    payload_artifact_id: &str,
    patch_artifact_id: &str,
    content: Value,
) -> Result<(), WorkPatchExportError> {
    let store = DatabaseSessionArtifactStore::new(astra_core::MatrixOneSettings::default())
        .with_pool(pool.clone());
    if let Some(existing) = store
        .load_json_artifact(owner_id, session_id, payload_artifact_id)
        .await
        .map_err(|error| WorkPatchExportError::Unavailable(error.to_string()))?
    {
        return validate_existing_payload(existing, &content);
    }
    let result = store
        .persist_json_artifact(SessionArtifactJsonRecord {
            artifact_id: payload_artifact_id.to_string(),
            session_id: session_id.to_string(),
            user_id: owner_id.to_string(),
            artifact_kind: "patch".into(),
            source: Some("work_patch_export".into()),
            turn: None,
            round: None,
            content: content.clone(),
            metadata: None,
            references: vec![SessionArtifactReference {
                kind: SessionArtifactReferenceKind::StateItem,
                reference_id: patch_artifact_id.to_string(),
            }],
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) => match store
            .load_json_artifact(owner_id, session_id, payload_artifact_id)
            .await
        {
            Ok(Some(existing)) => validate_existing_payload(existing, &content),
            _ => Err(WorkPatchExportError::Unavailable(error.to_string())),
        },
    }
}

fn validate_existing_payload(
    existing: astra_services::StoredSessionArtifact,
    expected_content: &Value,
) -> Result<(), WorkPatchExportError> {
    if existing.artifact_kind == "patch"
        && existing.source.as_deref() == Some("work_patch_export")
        && existing.content == *expected_content
    {
        Ok(())
    } else {
        Err(WorkPatchExportError::IdempotencyConflict)
    }
}

fn export_identity(command: &WorkPatchExportCommand) -> String {
    let mut hasher = Sha256::new();
    for field in [
        command.owner_id.as_str(),
        command.work_id.as_str(),
        command.branch_id.as_str(),
        command.request_id.as_str(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{:x}", hasher.finalize())[..48].to_string()
}

fn map_provider_error(error: GitWorktreePatchExportError) -> WorkPatchExportError {
    match error {
        GitWorktreePatchExportError::NoChanges => WorkPatchExportError::NoChanges,
        GitWorktreePatchExportError::PatchTooLarge => WorkPatchExportError::TooLarge,
        GitWorktreePatchExportError::Observation(_)
        | GitWorktreePatchExportError::ExportRejected
        | GitWorktreePatchExportError::Io(_) => {
            WorkPatchExportError::Unavailable(error.to_string())
        }
    }
}

fn map_repository_error(error: WorkRepositoryError) -> WorkPatchExportError {
    match error {
        WorkRepositoryError::NotFound => WorkPatchExportError::NotFound,
        WorkRepositoryError::PatchArtifactConflict { .. }
        | WorkRepositoryError::StaleSubjectBasis { .. }
        | WorkRepositoryError::Conflict { .. } => WorkPatchExportError::BasisConflict,
        error => WorkPatchExportError::Unavailable(error.to_string()),
    }
}
