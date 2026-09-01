//! Provider-neutral Git worktree patch materialization.
//!
//! The caller owns the workspace mutation lease. This module owns the exact
//! base observation and provider effect classification; it never interprets
//! stderr or human-readable command output as control state.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use astra_services::work::{
    WORK_PATCH_ARTIFACT_MAX_BYTES, WORK_PATCH_ARTIFACT_MAX_LINES, WorkContentHash,
    work_patch_line_count,
};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

const GIT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const REVISION_DOMAIN: &[u8] = b"astra.git-worktree.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitPatchNotAppliedCode {
    BaseChanged,
    PatchRejected,
    WorkspaceUnavailable,
    ProviderUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitPatchUnknownEffectCode {
    ApplyFailedAfterMutation,
    ObservationUnavailableAfterApply,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPatchMaterializationOutcome {
    Applied {
        observed_revision: WorkContentHash,
    },
    NotApplied {
        code: GitPatchNotAppliedCode,
        observed_revision: Option<WorkContentHash>,
    },
    UnknownEffect {
        code: GitPatchUnknownEffectCode,
        observed_revision: Option<WorkContentHash>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitWorktreePatchExport {
    pub base_subject_revision: WorkContentHash,
    pub result_subject_revision: WorkContentHash,
    pub patch: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitWorktreeCommitMetadata {
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitWorktreeCommitNotCreatedCode {
    InvalidMetadata,
    BaseChanged,
    ResultChanged,
    PatchRejected,
    CommitRejected,
    RefConflict,
    WorkspaceUnavailable,
    ProviderUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitWorktreeCommitOutcome {
    Committed {
        commit_sha: String,
        observed_revision: Option<WorkContentHash>,
        index_reconciled: bool,
    },
    NotCreated {
        code: GitWorktreeCommitNotCreatedCode,
        observed_revision: Option<WorkContentHash>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitReviewedCommitReconciliation {
    NotCommitted {
        observed_revision: WorkContentHash,
    },
    Committed {
        commit_sha: String,
        observed_revision: WorkContentHash,
        index_reconciled: bool,
    },
    Diverged {
        observed_revision: Option<WorkContentHash>,
    },
}

#[derive(Debug, Error)]
pub enum GitWorktreePatchExportError {
    #[error(transparent)]
    Observation(#[from] GitWorkspaceObservationError),
    #[error("worktree has no exportable changes")]
    NoChanges,
    #[error("exported patch exceeds the Work patch artifact limit")]
    PatchTooLarge,
    #[error("Git provider rejected patch export")]
    ExportRejected,
    #[error("Git patch export I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum GitWorkspaceObservationError {
    #[error("workspace path is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace is not a canonical Git worktree root")]
    NotWorktreeRoot,
    #[error("Git provider is unavailable")]
    ProviderUnavailable,
    #[error("Git provider operation timed out")]
    Timeout,
    #[error("Git provider rejected a read-only observation")]
    ObservationRejected,
    #[error("Git returned an unsafe worktree path")]
    UnsafePath,
    #[error("workspace observation I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Observe the committed, tracked-diff, and untracked-file state without
/// materializing repository history or loading a complete diff into memory.
/// Ignored files are intentionally outside the developer-work subject.
pub async fn observe_git_worktree_revision(
    workspace_root: &Path,
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    timeout(
        GIT_OPERATION_TIMEOUT,
        observe_git_worktree_revision_serialized(workspace_root),
    )
    .await
    .map_err(|_| GitWorkspaceObservationError::Timeout)?
}

/// Apply one bounded unified diff only on its exact observed base.
///
/// A failed preflight is definitively `NotApplied`. Once the mutating Git
/// process starts, a failure is classified by an exact post-observation: an
/// unchanged revision is `NotApplied`; a changed or unobservable revision is
/// `UnknownEffect` and must be reconciled rather than replayed.
pub async fn materialize_git_patch(
    workspace_root: &Path,
    expected_base_revision: &WorkContentHash,
    patch: &[u8],
) -> GitPatchMaterializationOutcome {
    if patch.len() as u64 > WORK_PATCH_ARTIFACT_MAX_BYTES
        || work_patch_line_count(patch) > WORK_PATCH_ARTIFACT_MAX_LINES
    {
        return GitPatchMaterializationOutcome::NotApplied {
            code: GitPatchNotAppliedCode::PatchRejected,
            observed_revision: None,
        };
    }
    let root = match canonical_git_worktree_root(workspace_root).await {
        Ok(root) => root,
        Err(error) => {
            return GitPatchMaterializationOutcome::NotApplied {
                code: observation_not_applied_code(&error),
                observed_revision: None,
            };
        }
    };
    let _workspace_lock = match acquire_workspace_lock(&root).await {
        Ok(lock) => lock,
        Err(error) => {
            return GitPatchMaterializationOutcome::NotApplied {
                code: observation_not_applied_code(&error),
                observed_revision: None,
            };
        }
    };
    let before = match observe_git_worktree_revision_locked(&root).await {
        Ok(revision) => revision,
        Err(error) => {
            return GitPatchMaterializationOutcome::NotApplied {
                code: observation_not_applied_code(&error),
                observed_revision: None,
            };
        }
    };
    if &before != expected_base_revision {
        return GitPatchMaterializationOutcome::NotApplied {
            code: GitPatchNotAppliedCode::BaseChanged,
            observed_revision: Some(before),
        };
    }
    match run_git_apply(&root, patch, true).await {
        GitApplyStatus::Succeeded => {}
        GitApplyStatus::Rejected => {
            return GitPatchMaterializationOutcome::NotApplied {
                code: GitPatchNotAppliedCode::PatchRejected,
                observed_revision: Some(before),
            };
        }
        GitApplyStatus::Unavailable => {
            return GitPatchMaterializationOutcome::NotApplied {
                code: GitPatchNotAppliedCode::ProviderUnavailable,
                observed_revision: Some(before),
            };
        }
    }

    let apply_status = run_git_apply(&root, patch, false).await;
    match observe_git_worktree_revision_locked(&root).await {
        Ok(observed_revision) if observed_revision == before => {
            GitPatchMaterializationOutcome::NotApplied {
                code: match apply_status {
                    GitApplyStatus::Unavailable => GitPatchNotAppliedCode::ProviderUnavailable,
                    GitApplyStatus::Succeeded | GitApplyStatus::Rejected => {
                        GitPatchNotAppliedCode::PatchRejected
                    }
                },
                observed_revision: Some(observed_revision),
            }
        }
        Ok(observed_revision) => match apply_status {
            GitApplyStatus::Succeeded => {
                GitPatchMaterializationOutcome::Applied { observed_revision }
            }
            GitApplyStatus::Rejected | GitApplyStatus::Unavailable => {
                GitPatchMaterializationOutcome::UnknownEffect {
                    code: GitPatchUnknownEffectCode::ApplyFailedAfterMutation,
                    observed_revision: Some(observed_revision),
                }
            }
        },
        Err(_) => GitPatchMaterializationOutcome::UnknownEffect {
            code: GitPatchUnknownEffectCode::ObservationUnavailableAfterApply,
            observed_revision: None,
        },
    }
}

/// Export the current developer-visible worktree delta from its clean `HEAD`
/// basis. The same workspace lock used by materialization and observation
/// keeps the payload and both subject revisions coherent.
pub async fn export_git_worktree_patch(
    workspace_root: &Path,
) -> Result<GitWorktreePatchExport, GitWorktreePatchExportError> {
    let root = canonical_git_worktree_root(workspace_root).await?;
    let _workspace_lock = acquire_workspace_lock(&root).await?;
    let result_subject_revision = observe_git_worktree_revision_locked(&root).await?;
    let head = git_small_output(&root, &["rev-parse", "HEAD"]).await?;
    let base_subject_revision = clean_head_subject_revision(trim_ascii(&head))?;
    if result_subject_revision == base_subject_revision {
        return Err(GitWorktreePatchExportError::NoChanges);
    }

    let mut patch = Vec::new();
    let mut tracked = Command::new("git");
    tracked
        .args(["-c", "core.quotePath=true", "-C"])
        .arg(&root)
        .args([
            "diff",
            "--binary",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--diff-algorithm=myers",
            "--ignore-submodules=none",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
            "--",
        ]);
    append_bounded_git_output(tracked, &[0], &mut patch).await?;
    append_untracked_patches(&root, &mut patch).await?;
    if patch.is_empty() {
        return Err(GitWorktreePatchExportError::ExportRejected);
    }
    if work_patch_line_count(&patch) > WORK_PATCH_ARTIFACT_MAX_LINES {
        return Err(GitWorktreePatchExportError::PatchTooLarge);
    }
    Ok(GitWorktreePatchExport {
        base_subject_revision,
        result_subject_revision,
        patch,
    })
}

/// Commits exactly one already-reviewed patch without staging the live index.
///
/// A temporary index is built from the pinned `HEAD`, the immutable patch is
/// applied to that index, and `HEAD` advances through one old-value CAS. Files
/// appearing after review can therefore neither leak into the commit nor be
/// overwritten by it.
pub async fn commit_reviewed_git_patch(
    workspace_root: &Path,
    expected_base_revision: &WorkContentHash,
    expected_result_revision: &WorkContentHash,
    patch: &[u8],
    metadata: &GitWorktreeCommitMetadata,
) -> GitWorktreeCommitOutcome {
    let not_created = |code, observed_revision| GitWorktreeCommitOutcome::NotCreated {
        code,
        observed_revision,
    };
    if !valid_commit_metadata(metadata) {
        return not_created(GitWorktreeCommitNotCreatedCode::InvalidMetadata, None);
    }
    if patch.len() as u64 > WORK_PATCH_ARTIFACT_MAX_BYTES
        || work_patch_line_count(patch) > WORK_PATCH_ARTIFACT_MAX_LINES
    {
        return not_created(GitWorktreeCommitNotCreatedCode::PatchRejected, None);
    }
    let root = match canonical_git_worktree_root(workspace_root).await {
        Ok(root) => root,
        Err(error) => return not_created(commit_observation_code(&error), None),
    };
    let _workspace_lock = match acquire_workspace_lock(&root).await {
        Ok(lock) => lock,
        Err(error) => return not_created(commit_observation_code(&error), None),
    };
    let before = match observe_git_worktree_revision_locked(&root).await {
        Ok(revision) => revision,
        Err(error) => return not_created(commit_observation_code(&error), None),
    };
    if &before != expected_result_revision {
        return not_created(GitWorktreeCommitNotCreatedCode::ResultChanged, Some(before));
    }
    let head = match git_small_output(&root, &["rev-parse", "HEAD"]).await {
        Ok(head) => head,
        Err(error) => return not_created(commit_observation_code(&error), Some(before)),
    };
    let head = trim_ascii(&head);
    let clean_base = match clean_head_subject_revision(head) {
        Ok(revision) => revision,
        Err(error) => return not_created(commit_observation_code(&error), Some(before)),
    };
    if &clean_base != expected_base_revision {
        return not_created(GitWorktreeCommitNotCreatedCode::BaseChanged, Some(before));
    }

    let index_name = format!("astra-reviewed-{}.index", Uuid::new_v4());
    let index_path = match git_small_output(&root, &["rev-parse", "--git-path", &index_name]).await
    {
        Ok(path) => match std::str::from_utf8(trim_ascii(&path)) {
            Ok(path) if !path.is_empty() => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            }
            _ => {
                return not_created(
                    GitWorktreeCommitNotCreatedCode::WorkspaceUnavailable,
                    Some(before),
                );
            }
        },
        Err(error) => return not_created(commit_observation_code(&error), Some(before)),
    };
    let temporary_index = TemporaryIndex(index_path);
    if !git_with_index(
        &root,
        &temporary_index.0,
        &["read-tree", "HEAD"],
        None,
        None,
    )
    .await
    {
        return not_created(
            GitWorktreeCommitNotCreatedCode::WorkspaceUnavailable,
            Some(before),
        );
    }
    if !git_with_index(
        &root,
        &temporary_index.0,
        &["apply", "--cached", "--binary", "--whitespace=nowarn", "-"],
        Some(patch),
        None,
    )
    .await
    {
        return not_created(GitWorktreeCommitNotCreatedCode::PatchRejected, Some(before));
    }
    let tree =
        match git_with_index_output(&root, &temporary_index.0, &["write-tree"], None, None).await {
            Some(tree) => tree,
            None => {
                return not_created(
                    GitWorktreeCommitNotCreatedCode::CommitRejected,
                    Some(before),
                );
            }
        };
    let tree_id = match std::str::from_utf8(trim_ascii(&tree)) {
        Ok(value) if is_git_object_id(value) => value,
        _ => {
            return not_created(
                GitWorktreeCommitNotCreatedCode::CommitRejected,
                Some(before),
            );
        }
    };
    let untracked_outside_tree = git_with_index_output(
        &root,
        &temporary_index.0,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
        None,
    )
    .await;
    if !git_with_index(
        &root,
        &temporary_index.0,
        &["diff", "--quiet", tree_id, "--"],
        None,
        None,
    )
    .await
        || !matches!(untracked_outside_tree, Some(output) if output.is_empty())
    {
        return not_created(GitWorktreeCommitNotCreatedCode::PatchRejected, Some(before));
    }
    let still_current = match observe_git_worktree_revision_locked(&root).await {
        Ok(revision) => revision,
        Err(error) => return not_created(commit_observation_code(&error), None),
    };
    if &still_current != expected_result_revision {
        return not_created(
            GitWorktreeCommitNotCreatedCode::ResultChanged,
            Some(still_current),
        );
    }
    let mut message = metadata.message.as_bytes().to_vec();
    message.push(b'\n');
    let commit = match git_with_index_output(
        &root,
        &temporary_index.0,
        &[
            "commit-tree",
            tree_id,
            "-p",
            std::str::from_utf8(head).unwrap_or_default(),
        ],
        Some(&message),
        Some(metadata),
    )
    .await
    {
        Some(commit) => commit,
        None => {
            return not_created(
                GitWorktreeCommitNotCreatedCode::CommitRejected,
                Some(still_current),
            );
        }
    };
    let commit_sha = match std::str::from_utf8(trim_ascii(&commit)) {
        Ok(value) if is_git_object_id(value) => value.to_owned(),
        _ => {
            return not_created(
                GitWorktreeCommitNotCreatedCode::CommitRejected,
                Some(still_current),
            );
        }
    };
    let head_text = match std::str::from_utf8(head) {
        Ok(value) if is_git_object_id(value) => value,
        _ => {
            return not_created(
                GitWorktreeCommitNotCreatedCode::CommitRejected,
                Some(still_current),
            );
        }
    };
    if !git_with_index(
        &root,
        &temporary_index.0,
        &["update-ref", "HEAD", &commit_sha, head_text],
        None,
        None,
    )
    .await
    {
        return not_created(
            GitWorktreeCommitNotCreatedCode::RefConflict,
            Some(still_current),
        );
    }
    let index_reconciled = git_small_output(&root, &["reset", "--mixed", "--quiet", &commit_sha])
        .await
        .is_ok();
    GitWorktreeCommitOutcome::Committed {
        commit_sha,
        observed_revision: observe_git_worktree_revision_locked(&root).await.ok(),
        index_reconciled,
    }
}

/// Reconciles the crash window after a reviewed commit may have advanced
/// `HEAD` but before its durable operation recorded the result. No reflog or
/// commit-message matching participates: the current commit must be a direct
/// child of the exact base and have the exact tree reconstructed from the
/// immutable patch.
pub async fn reconcile_reviewed_git_patch_commit(
    workspace_root: &Path,
    expected_base_revision: &WorkContentHash,
    expected_result_revision: &WorkContentHash,
    patch: &[u8],
) -> GitReviewedCommitReconciliation {
    let root = match canonical_git_worktree_root(workspace_root).await {
        Ok(root) => root,
        Err(_) => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: None,
            };
        }
    };
    let _workspace_lock = match acquire_workspace_lock(&root).await {
        Ok(lock) => lock,
        Err(_) => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: None,
            };
        }
    };
    let observed_revision = match observe_git_worktree_revision_locked(&root).await {
        Ok(revision) => revision,
        Err(_) => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: None,
            };
        }
    };
    if &observed_revision == expected_result_revision {
        return GitReviewedCommitReconciliation::NotCommitted { observed_revision };
    }
    if patch.len() as u64 > WORK_PATCH_ARTIFACT_MAX_BYTES
        || work_patch_line_count(patch) > WORK_PATCH_ARTIFACT_MAX_LINES
    {
        return GitReviewedCommitReconciliation::Diverged {
            observed_revision: Some(observed_revision),
        };
    }
    let head = match git_small_output(&root, &["rev-parse", "HEAD"]).await {
        Ok(value) => value,
        Err(_) => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: Some(observed_revision),
            };
        }
    };
    let head = match std::str::from_utf8(trim_ascii(&head)) {
        Ok(value) if is_git_object_id(value) => value,
        _ => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: Some(observed_revision),
            };
        }
    };
    let parent = match git_small_output(&root, &["rev-parse", "HEAD^"]).await {
        Ok(value) => value,
        Err(_) => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: Some(observed_revision),
            };
        }
    };
    let parent = match std::str::from_utf8(trim_ascii(&parent)) {
        Ok(value) if is_git_object_id(value) => value,
        _ => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: Some(observed_revision),
            };
        }
    };
    if clean_head_subject_revision(parent.as_bytes()).ok().as_ref() != Some(expected_base_revision)
    {
        return GitReviewedCommitReconciliation::Diverged {
            observed_revision: Some(observed_revision),
        };
    }
    let index_name = format!("astra-reconcile-{}.index", Uuid::new_v4());
    let index_path = match git_small_output(&root, &["rev-parse", "--git-path", &index_name]).await
    {
        Ok(path) => match std::str::from_utf8(trim_ascii(&path)) {
            Ok(path) if !path.is_empty() => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            }
            _ => {
                return GitReviewedCommitReconciliation::Diverged {
                    observed_revision: Some(observed_revision),
                };
            }
        },
        Err(_) => {
            return GitReviewedCommitReconciliation::Diverged {
                observed_revision: Some(observed_revision),
            };
        }
    };
    let temporary_index = TemporaryIndex(index_path);
    if !git_with_index(
        &root,
        &temporary_index.0,
        &["read-tree", parent],
        None,
        None,
    )
    .await
        || !git_with_index(
            &root,
            &temporary_index.0,
            &["apply", "--cached", "--binary", "--whitespace=nowarn", "-"],
            Some(patch),
            None,
        )
        .await
    {
        return GitReviewedCommitReconciliation::Diverged {
            observed_revision: Some(observed_revision),
        };
    }
    let expected_tree =
        git_with_index_output(&root, &temporary_index.0, &["write-tree"], None, None).await;
    let actual_tree = git_small_output(&root, &["rev-parse", "HEAD^{tree}"]).await;
    if expected_tree.as_deref().map(trim_ascii) != actual_tree.as_deref().ok().map(trim_ascii) {
        return GitReviewedCommitReconciliation::Diverged {
            observed_revision: Some(observed_revision),
        };
    }
    let clean_committed_revision = clean_head_subject_revision(head.as_bytes()).ok();
    let index_reconciled = if clean_committed_revision.as_ref() == Some(&observed_revision) {
        git_small_output(&root, &["reset", "--mixed", "--quiet", head])
            .await
            .is_ok()
    } else {
        false
    };
    GitReviewedCommitReconciliation::Committed {
        commit_sha: head.to_owned(),
        observed_revision,
        index_reconciled,
    }
}

struct TemporaryIndex(PathBuf);

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let lock = self.0.with_extension("index.lock");
        let _ = std::fs::remove_file(lock);
    }
}

fn valid_commit_metadata(metadata: &GitWorktreeCommitMetadata) -> bool {
    let valid_field = |value: &str, max_bytes: usize, allow_newline: bool| {
        !value.trim().is_empty()
            && value.len() <= max_bytes
            && value.chars().all(|character| {
                character != '\0'
                    && (!character.is_control()
                        || character == '\t'
                        || (allow_newline && character == '\n'))
            })
    };
    valid_field(&metadata.message, 4_096, true)
        && valid_field(&metadata.author_name, 256, false)
        && valid_field(&metadata.author_email, 320, false)
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn commit_observation_code(
    error: &GitWorkspaceObservationError,
) -> GitWorktreeCommitNotCreatedCode {
    match error {
        GitWorkspaceObservationError::ProviderUnavailable => {
            GitWorktreeCommitNotCreatedCode::ProviderUnavailable
        }
        GitWorkspaceObservationError::WorkspaceUnavailable
        | GitWorkspaceObservationError::NotWorktreeRoot
        | GitWorkspaceObservationError::Timeout
        | GitWorkspaceObservationError::ObservationRejected
        | GitWorkspaceObservationError::UnsafePath
        | GitWorkspaceObservationError::Io(_) => {
            GitWorktreeCommitNotCreatedCode::WorkspaceUnavailable
        }
    }
}

async fn git_with_index(
    root: &Path,
    index: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    metadata: Option<&GitWorktreeCommitMetadata>,
) -> bool {
    git_with_index_output(root, index, args, input, metadata)
        .await
        .is_some()
}

async fn git_with_index_output(
    root: &Path,
    index: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    metadata: Option<&GitWorktreeCommitMetadata>,
) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(metadata) = metadata {
        command
            .env("GIT_AUTHOR_NAME", &metadata.author_name)
            .env("GIT_AUTHOR_EMAIL", &metadata.author_email)
            .env("GIT_COMMITTER_NAME", &metadata.author_name)
            .env("GIT_COMMITTER_EMAIL", &metadata.author_email);
    }
    let mut child = command.spawn().ok()?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take()?;
        if stdin.write_all(input).await.is_err() || stdin.shutdown().await.is_err() {
            let _ = child.kill().await;
            return None;
        }
    }
    let output = child.wait_with_output().await.ok()?;
    output.status.success().then_some(output.stdout)
}

fn clean_head_subject_revision(
    head: &[u8],
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DOMAIN);
    hash_field(&mut hasher, b"head", head);
    hash_field(&mut hasher, b"diff", &Sha256::digest([]));
    WorkContentHash::parse(format!("sha256:{:x}", hasher.finalize()))
        .map_err(|_| GitWorkspaceObservationError::ObservationRejected)
}

async fn append_untracked_patches(
    root: &Path,
    patch: &mut Vec<u8>,
) -> Result<(), GitWorktreePatchExportError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--deduplicate",
            "-z",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_spawn_error)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(GitWorkspaceObservationError::ObservationRejected)?;
    let mut paths = BufReader::new(stdout);
    let mut encoded_path = Vec::new();
    loop {
        encoded_path.clear();
        if paths.read_until(0, &mut encoded_path).await? == 0 {
            break;
        }
        if encoded_path.last() != Some(&0) {
            return Err(GitWorkspaceObservationError::ObservationRejected.into());
        }
        encoded_path.pop();
        let relative = path_from_git_bytes(&encoded_path)?;
        if !safe_relative_path(&relative) {
            return Err(GitWorkspaceObservationError::UnsafePath.into());
        }
        let mut command = Command::new("git");
        command
            .args(["-c", "core.quotePath=true", "-C"])
            .arg(root)
            .args([
                "diff",
                "--no-index",
                "--binary",
                "--full-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--",
            ])
            .arg("/dev/null")
            .arg(&relative);
        append_bounded_git_output(command, &[1], patch).await?;
    }
    if !child.wait().await?.success() {
        return Err(GitWorktreePatchExportError::ExportRejected);
    }
    Ok(())
}

async fn append_bounded_git_output(
    mut command: Command,
    accepted_exit_codes: &[i32],
    output: &mut Vec<u8>,
) -> Result<(), GitWorktreePatchExportError> {
    let mut child = command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_spawn_error)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or(GitWorkspaceObservationError::ObservationRejected)?;
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    loop {
        let bytes = stdout.read(&mut buffer).await?;
        if bytes == 0 {
            break;
        }
        if output.len().saturating_add(bytes) > WORK_PATCH_ARTIFACT_MAX_BYTES as usize {
            let _ = child.kill().await;
            return Err(GitWorktreePatchExportError::PatchTooLarge);
        }
        output.extend_from_slice(&buffer[..bytes]);
    }
    let status = child.wait().await?;
    if !status
        .code()
        .is_some_and(|code| accepted_exit_codes.contains(&code))
    {
        return Err(GitWorktreePatchExportError::ExportRejected);
    }
    Ok(())
}

fn observation_not_applied_code(error: &GitWorkspaceObservationError) -> GitPatchNotAppliedCode {
    match error {
        GitWorkspaceObservationError::ProviderUnavailable => {
            GitPatchNotAppliedCode::ProviderUnavailable
        }
        GitWorkspaceObservationError::WorkspaceUnavailable
        | GitWorkspaceObservationError::NotWorktreeRoot
        | GitWorkspaceObservationError::Timeout
        | GitWorkspaceObservationError::ObservationRejected
        | GitWorkspaceObservationError::UnsafePath
        | GitWorkspaceObservationError::Io(_) => GitPatchNotAppliedCode::WorkspaceUnavailable,
    }
}

async fn observe_git_worktree_revision_serialized(
    workspace_root: &Path,
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    let root = canonical_git_worktree_root(workspace_root).await?;
    let _workspace_lock = acquire_workspace_lock(&root).await?;
    observe_git_worktree_revision_locked(&root).await
}

async fn canonical_git_worktree_root(
    workspace_root: &Path,
) -> Result<PathBuf, GitWorkspaceObservationError> {
    let root = workspace_root
        .canonicalize()
        .map_err(|_| GitWorkspaceObservationError::WorkspaceUnavailable)?;
    if !root.is_dir() {
        return Err(GitWorkspaceObservationError::WorkspaceUnavailable);
    }
    let discovered_root = git_small_output(&root, &["rev-parse", "--show-toplevel"]).await?;
    let discovered_root = PathBuf::from(
        std::str::from_utf8(trim_ascii(&discovered_root))
            .map_err(|_| GitWorkspaceObservationError::NotWorktreeRoot)?,
    )
    .canonicalize()
    .map_err(|_| GitWorkspaceObservationError::NotWorktreeRoot)?;
    if discovered_root != root {
        return Err(GitWorkspaceObservationError::NotWorktreeRoot);
    }
    Ok(root)
}

async fn observe_git_worktree_revision_locked(
    root: &Path,
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DOMAIN);
    let head = git_small_output(root, &["rev-parse", "HEAD"]).await?;
    hash_field(&mut hasher, b"head", trim_ascii(&head));
    hash_git_diff(root, &mut hasher).await?;
    hash_untracked_files(root, &mut hasher).await?;
    WorkContentHash::parse(format!("sha256:{:x}", hasher.finalize()))
        .map_err(|_| GitWorkspaceObservationError::ObservationRejected)
}

struct WorkspaceLock(std::fs::File);

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

async fn acquire_workspace_lock(
    root: &Path,
) -> Result<WorkspaceLock, GitWorkspaceObservationError> {
    let encoded_path =
        git_small_output(root, &["rev-parse", "--git-path", "astra-workspace.lock"]).await?;
    let lock_path = PathBuf::from(
        std::str::from_utf8(trim_ascii(&encoded_path))
            .map_err(|_| GitWorkspaceObservationError::UnsafePath)?,
    );
    let lock_path = if lock_path.is_absolute() {
        lock_path
    } else {
        root.join(lock_path)
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    tokio::task::spawn_blocking(move || {
        file.lock_exclusive()?;
        Ok(WorkspaceLock(file))
    })
    .await
    .map_err(|_| GitWorkspaceObservationError::ObservationRejected)?
}

async fn git_small_output(
    root: &Path,
    args: &[&str],
) -> Result<Vec<u8>, GitWorkspaceObservationError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitWorkspaceObservationError::ProviderUnavailable
            } else {
                GitWorkspaceObservationError::Io(error)
            }
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitWorkspaceObservationError::ObservationRejected)
    }
}

async fn hash_git_diff(
    root: &Path,
    hasher: &mut Sha256,
) -> Result<(), GitWorkspaceObservationError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--binary",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--diff-algorithm=myers",
            "--ignore-submodules=none",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
            "--",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_spawn_error)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or(GitWorkspaceObservationError::ObservationRejected)?;
    let digest = digest_reader(&mut stdout).await?;
    hash_field(hasher, b"diff", &digest);
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(GitWorkspaceObservationError::ObservationRejected)
    }
}

async fn hash_untracked_files(
    root: &Path,
    hasher: &mut Sha256,
) -> Result<(), GitWorkspaceObservationError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--deduplicate",
            "-z",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_spawn_error)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(GitWorkspaceObservationError::ObservationRejected)?;
    let mut paths = BufReader::new(stdout);
    let mut encoded_path = Vec::new();
    loop {
        encoded_path.clear();
        let bytes = paths.read_until(0, &mut encoded_path).await?;
        if bytes == 0 {
            break;
        }
        if encoded_path.last() != Some(&0) {
            return Err(GitWorkspaceObservationError::ObservationRejected);
        }
        encoded_path.pop();
        let relative = path_from_git_bytes(&encoded_path)?;
        if !safe_relative_path(&relative) {
            return Err(GitWorkspaceObservationError::UnsafePath);
        }
        hash_field(hasher, b"untracked-path", &encoded_path);
        let absolute = root.join(relative);
        let metadata = tokio::fs::symlink_metadata(&absolute).await?;
        if metadata.file_type().is_symlink() {
            let target = tokio::fs::read_link(&absolute).await?;
            hash_field(hasher, b"symlink", target.as_os_str().as_encoded_bytes());
        } else if metadata.is_file() {
            hash_regular_file_mode(hasher, &metadata);
            let mut file = tokio::fs::File::open(&absolute).await?;
            let digest = digest_reader(&mut file).await?;
            hash_field(hasher, b"file-content", &digest);
        } else {
            return Err(GitWorkspaceObservationError::UnsafePath);
        }
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(GitWorkspaceObservationError::ObservationRejected)
    }
}

async fn digest_reader(reader: &mut (impl AsyncRead + Unpin)) -> Result<[u8; 32], std::io::Error> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..bytes]);
    }
}

#[cfg(unix)]
fn hash_regular_file_mode(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if metadata.permissions().mode() & 0o111 == 0 {
        b"100644".as_slice()
    } else {
        b"100755".as_slice()
    };
    hash_field(hasher, b"file-mode", mode);
}

#[cfg(not(unix))]
fn hash_regular_file_mode(hasher: &mut Sha256, _metadata: &std::fs::Metadata) {
    hash_field(hasher, b"file-mode", b"100644");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitApplyStatus {
    Succeeded,
    Rejected,
    Unavailable,
}

async fn run_git_apply(root: &Path, patch: &[u8], check_only: bool) -> GitApplyStatus {
    timeout(
        GIT_OPERATION_TIMEOUT,
        run_git_apply_inner(root, patch, check_only),
    )
    .await
    .unwrap_or(GitApplyStatus::Unavailable)
}

async fn run_git_apply_inner(root: &Path, patch: &[u8], check_only: bool) -> GitApplyStatus {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .arg("apply")
        .arg("--whitespace=nowarn");
    if check_only {
        command.arg("--check");
    }
    let mut child = match command
        .arg("-")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return GitApplyStatus::Unavailable,
    };
    let Some(mut stdin) = child.stdin.take() else {
        return GitApplyStatus::Unavailable;
    };
    if stdin.write_all(patch).await.is_err() || stdin.shutdown().await.is_err() {
        return GitApplyStatus::Unavailable;
    }
    drop(stdin);
    match child.wait().await {
        Ok(status) if status.success() => GitApplyStatus::Succeeded,
        Ok(_) => GitApplyStatus::Rejected,
        Err(_) => GitApplyStatus::Unavailable,
    }
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update(name);
    hasher.update([0]);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    value
        .strip_suffix(b"\n")
        .and_then(|value| value.strip_suffix(b"\r").or(Some(value)))
        .unwrap_or(value)
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(unix)]
fn path_from_git_bytes(value: &[u8]) -> Result<PathBuf, GitWorkspaceObservationError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(value: &[u8]) -> Result<PathBuf, GitWorkspaceObservationError> {
    String::from_utf8(value.to_vec())
        .map(PathBuf::from)
        .map_err(|_| GitWorkspaceObservationError::UnsafePath)
}

fn map_spawn_error(error: std::io::Error) -> GitWorkspaceObservationError {
    if error.kind() == std::io::ErrorKind::NotFound {
        GitWorkspaceObservationError::ProviderUnavailable
    } else {
        GitWorkspaceObservationError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Astra Test")
            .env("GIT_AUTHOR_EMAIL", "astra@example.invalid")
            .env("GIT_COMMITTER_NAME", "Astra Test")
            .env("GIT_COMMITTER_EMAIL", "astra@example.invalid")
            .status()
            .expect("start git fixture command");
        assert!(status.success(), "git fixture command failed: {args:?}");
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp repository");
        git(directory.path(), &["init", "--quiet"]);
        fs::write(directory.path().join("file.txt"), "before\n").expect("seed file");
        git(directory.path(), &["add", "file.txt"]);
        git(directory.path(), &["commit", "--quiet", "-m", "initial"]);
        directory
    }

    const PATCH: &[u8] = b"diff --git a/file.txt b/file.txt\nindex 90be1bd..17db796 100644\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n";

    #[tokio::test]
    async fn exact_base_patch_changes_the_observed_subject() {
        let repository = repository();
        let before = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe base");
        let outcome = materialize_git_patch(repository.path(), &before, PATCH).await;
        let GitPatchMaterializationOutcome::Applied { observed_revision } = outcome else {
            panic!("expected applied outcome: {outcome:?}");
        };
        assert_ne!(observed_revision, before);
        assert_eq!(
            fs::read_to_string(repository.path().join("file.txt")).expect("result file"),
            "after\n"
        );
        assert_eq!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("repeat observation"),
            observed_revision,
            "the subject algorithm must be deterministic"
        );
    }

    #[tokio::test]
    async fn stale_base_and_rejected_patch_are_definitively_not_applied() {
        let repository = repository();
        let before = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe base");
        fs::write(repository.path().join("file.txt"), "concurrent\n").expect("advance base");
        let stale = materialize_git_patch(repository.path(), &before, PATCH).await;
        assert!(matches!(
            stale,
            GitPatchMaterializationOutcome::NotApplied {
                code: GitPatchNotAppliedCode::BaseChanged,
                observed_revision: Some(_)
            }
        ));
        assert_eq!(
            fs::read_to_string(repository.path().join("file.txt")).expect("unchanged file"),
            "concurrent\n"
        );

        let current = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe concurrent base");
        let rejected = materialize_git_patch(repository.path(), &current, PATCH).await;
        assert_eq!(
            rejected,
            GitPatchMaterializationOutcome::NotApplied {
                code: GitPatchNotAppliedCode::PatchRejected,
                observed_revision: Some(current),
            }
        );
    }

    #[tokio::test]
    async fn nested_directory_is_not_silently_promoted_to_repository_root() {
        let repository = repository();
        let nested = repository.path().join("nested");
        fs::create_dir(&nested).expect("nested directory");
        assert!(matches!(
            observe_git_worktree_revision(&nested).await,
            Err(GitWorkspaceObservationError::NotWorktreeRoot)
        ));
    }

    #[tokio::test]
    async fn observation_waits_for_the_workspace_mutation_boundary() {
        let repository = repository();
        let root = canonical_git_worktree_root(repository.path())
            .await
            .expect("canonical worktree");
        let mutation_lock = acquire_workspace_lock(&root)
            .await
            .expect("hold materialization boundary");
        let observed_root = root.clone();
        let mut observation =
            tokio::spawn(async move { observe_git_worktree_revision(&observed_root).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut observation)
                .await
                .is_err(),
            "reconciliation must not observe a worktree while mutation owns its boundary"
        );
        drop(mutation_lock);
        observation
            .await
            .expect("observation task")
            .expect("observation after mutation boundary");
    }

    #[tokio::test]
    async fn concurrent_exact_base_requests_mutate_the_worktree_once() {
        let repository = repository();
        let before = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe shared base");
        let (left, right) = tokio::join!(
            materialize_git_patch(repository.path(), &before, PATCH),
            materialize_git_patch(repository.path(), &before, PATCH),
        );
        let outcomes = [left, right];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, GitPatchMaterializationOutcome::Applied { .. }))
                .count(),
            1,
            "the workspace mutation boundary must serialize concurrent providers"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    GitPatchMaterializationOutcome::NotApplied {
                        code: GitPatchNotAppliedCode::BaseChanged,
                        observed_revision: Some(_),
                    }
                ))
                .count(),
            1,
            "the losing exact-base request must observe drift instead of replaying"
        );
        assert_eq!(
            fs::read_to_string(repository.path().join("file.txt")).expect("materialized file"),
            "after\n"
        );
    }

    #[tokio::test]
    async fn exported_patch_reproduces_the_exact_subject_on_its_clean_base() {
        let source = repository();
        assert!(matches!(
            export_git_worktree_patch(source.path()).await,
            Err(GitWorktreePatchExportError::NoChanges)
        ));
        let target = tempfile::tempdir().expect("target repository");
        let status = std::process::Command::new("git")
            .args(["clone", "--quiet"])
            .arg(source.path())
            .arg(target.path())
            .status()
            .expect("clone clean target");
        assert!(status.success());

        fs::write(source.path().join("file.txt"), "after\n").expect("tracked change");
        fs::write(source.path().join("new.txt"), "new file\n").expect("untracked change");
        let exported = export_git_worktree_patch(source.path())
            .await
            .expect("export exact patch");
        assert_eq!(
            observe_git_worktree_revision(target.path())
                .await
                .expect("observe clean target"),
            exported.base_subject_revision
        );
        let applied = materialize_git_patch(
            target.path(),
            &exported.base_subject_revision,
            &exported.patch,
        )
        .await;
        assert_eq!(
            applied,
            GitPatchMaterializationOutcome::Applied {
                observed_revision: exported.result_subject_revision,
            },
            "an admitted export must reproduce its exact canonical subject"
        );
        assert_eq!(
            fs::read_to_string(target.path().join("new.txt")).expect("materialized new file"),
            "new file\n"
        );
    }

    #[tokio::test]
    async fn reviewed_patch_commit_is_exact_and_leaves_the_live_index_clean() {
        let repository = repository();
        fs::write(repository.path().join("file.txt"), "after\n").expect("tracked change");
        fs::write(repository.path().join("new.txt"), "reviewed\n").expect("untracked change");
        let exported = export_git_worktree_patch(repository.path())
            .await
            .expect("export reviewed patch");
        assert_eq!(
            reconcile_reviewed_git_patch_commit(
                repository.path(),
                &exported.base_subject_revision,
                &exported.result_subject_revision,
                &exported.patch,
            )
            .await,
            GitReviewedCommitReconciliation::NotCommitted {
                observed_revision: exported.result_subject_revision.clone(),
            }
        );
        let outcome = commit_reviewed_git_patch(
            repository.path(),
            &exported.base_subject_revision,
            &exported.result_subject_revision,
            &exported.patch,
            &GitWorktreeCommitMetadata {
                message: "Apply reviewed result".into(),
                author_name: "Astra Test".into(),
                author_email: "astra@example.invalid".into(),
            },
        )
        .await;
        let GitWorktreeCommitOutcome::Committed {
            commit_sha,
            observed_revision: Some(observed_revision),
            index_reconciled: true,
        } = outcome
        else {
            panic!("expected exact committed outcome: {outcome:?}");
        };
        assert!(is_git_object_id(&commit_sha));
        assert_ne!(observed_revision, exported.result_subject_revision);
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repository.path())
            .args(["status", "--porcelain=v1"])
            .output()
            .expect("read final status");
        assert!(status.status.success());
        assert!(
            status.stdout.is_empty(),
            "reviewed commit must leave no staged residue"
        );
        assert_eq!(
            fs::read_to_string(repository.path().join("new.txt")).expect("committed new file"),
            "reviewed\n"
        );
        assert_eq!(
            reconcile_reviewed_git_patch_commit(
                repository.path(),
                &exported.base_subject_revision,
                &exported.result_subject_revision,
                &exported.patch,
            )
            .await,
            GitReviewedCommitReconciliation::Committed {
                commit_sha: commit_sha.clone(),
                observed_revision,
                index_reconciled: true,
            }
        );
        fs::write(repository.path().join("later.txt"), "later\n").expect("later change");
        git(repository.path(), &["add", "later.txt"]);
        git(
            repository.path(),
            &["commit", "--quiet", "-m", "later commit"],
        );
        assert!(matches!(
            reconcile_reviewed_git_patch_commit(
                repository.path(),
                &exported.base_subject_revision,
                &exported.result_subject_revision,
                &exported.patch,
            )
            .await,
            GitReviewedCommitReconciliation::Diverged {
                observed_revision: Some(_)
            }
        ));
    }

    #[tokio::test]
    async fn reviewed_patch_commit_rejects_drift_and_patch_result_mismatch() {
        let repository = repository();
        fs::write(repository.path().join("file.txt"), "after\n").expect("reviewed change");
        let exported = export_git_worktree_patch(repository.path())
            .await
            .expect("export reviewed patch");
        fs::write(repository.path().join("late.txt"), "not reviewed\n").expect("late change");
        let metadata = GitWorktreeCommitMetadata {
            message: "Apply reviewed result".into(),
            author_name: "Astra Test".into(),
            author_email: "astra@example.invalid".into(),
        };
        assert!(matches!(
            commit_reviewed_git_patch(
                repository.path(),
                &exported.base_subject_revision,
                &exported.result_subject_revision,
                &exported.patch,
                &metadata,
            )
            .await,
            GitWorktreeCommitOutcome::NotCreated {
                code: GitWorktreeCommitNotCreatedCode::ResultChanged,
                observed_revision: Some(_),
            }
        ));
        fs::remove_file(repository.path().join("late.txt")).expect("remove late change");
        fs::write(repository.path().join("file.txt"), "before\n").expect("change live result");
        let different_result = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe different result");
        assert!(matches!(
            commit_reviewed_git_patch(
                repository.path(),
                &exported.base_subject_revision,
                &different_result,
                PATCH,
                &metadata,
            )
            .await,
            GitWorktreeCommitOutcome::NotCreated {
                code: GitWorktreeCommitNotCreatedCode::PatchRejected,
                observed_revision: Some(_),
            }
        ));
    }

    #[tokio::test]
    async fn patch_export_rejects_payloads_above_the_admission_limit() {
        let repository = repository();
        fs::write(
            repository.path().join("oversized.bin"),
            vec![b'x'; WORK_PATCH_ARTIFACT_MAX_BYTES as usize + 1],
        )
        .expect("oversized untracked file");
        assert!(matches!(
            export_git_worktree_patch(repository.path()).await,
            Err(GitWorktreePatchExportError::PatchTooLarge)
        ));
    }

    #[tokio::test]
    async fn patch_provider_rejects_pathological_line_cardinality() {
        let repository = repository();
        let data = "x\n".repeat((WORK_PATCH_ARTIFACT_MAX_LINES + 1) as usize);
        fs::write(repository.path().join("too-many-lines.txt"), data)
            .expect("high-cardinality untracked file");
        assert!(matches!(
            export_git_worktree_patch(repository.path()).await,
            Err(GitWorktreePatchExportError::PatchTooLarge)
        ));

        let patch = "+\n".repeat((WORK_PATCH_ARTIFACT_MAX_LINES + 1) as usize);
        let expected =
            WorkContentHash::parse(format!("sha256:{}", "a".repeat(64))).expect("expected subject");
        assert!(matches!(
            materialize_git_patch(repository.path(), &expected, patch.as_bytes()).await,
            GitPatchMaterializationOutcome::NotApplied {
                code: GitPatchNotAppliedCode::PatchRejected,
                observed_revision: None,
            }
        ));
    }

    #[tokio::test]
    async fn subject_tracks_developer_files_but_excludes_ignored_outputs() {
        let repository = repository();
        fs::write(repository.path().join(".gitignore"), "build/\n").expect("gitignore");
        git(repository.path(), &["add", ".gitignore"]);
        git(
            repository.path(),
            &["commit", "--quiet", "-m", "ignore outputs"],
        );
        let base = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe base");
        git(
            repository.path(),
            &["config", "diff.algorithm", "histogram"],
        );
        git(repository.path(), &["config", "color.ui", "always"]);
        assert_eq!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe with hostile display config"),
            base,
            "local display/diff preferences must not change canonical identity"
        );

        fs::write(repository.path().join("file.txt"), "tracked change\n").expect("tracked edit");
        assert_ne!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe tracked edit"),
            base
        );
        fs::write(repository.path().join("file.txt"), "before\n").expect("restore tracked file");
        fs::write(repository.path().join("new.txt"), "untracked\n").expect("untracked file");
        assert_ne!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe untracked file"),
            base
        );
        fs::remove_file(repository.path().join("new.txt")).expect("remove untracked file");
        fs::create_dir(repository.path().join("build")).expect("ignored directory");
        fs::write(repository.path().join("build/output.bin"), b"ignored").expect("ignored output");
        assert_eq!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe ignored output"),
            base
        );
    }
}
