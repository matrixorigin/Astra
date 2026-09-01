use std::{path::PathBuf, time::Duration};

use astra_core::SharedPool;
use astra_runtime_env::{WorkspaceAuthority, WorkspaceBindingKind, WorkspacePersistence};
use astra_services::{
    DatabaseWorkspaceRecordStore, WorkspaceRecordStore, WorkspaceRecordStoreError,
    work::{
        DatabaseWorkPatchCommitService, DatabaseWorkRepository,
        SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF, WorkPatchCommitCommitted, WorkPatchCommitError,
        WorkPatchCommitFailure, WorkPatchCommitFailureCode, WorkPatchCommitPhase,
        WorkPatchCommitProviderRef, WorkPatchCommitRecoveryItem, WorkProviderInvocationRef,
        WorkRepository,
    },
};
use astra_tools::patch_materialization::{
    GitReviewedCommitReconciliation, GitWorktreeCommitMetadata, GitWorktreeCommitNotCreatedCode,
    GitWorktreeCommitOutcome, commit_reviewed_git_patch, reconcile_reviewed_git_patch_commit,
};
use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RECOVERY_BATCH: u16 = 16;
const RECOVERY_CONCURRENCY: usize = 4;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn spawn_work_patch_commit_recovery(
    pool: SharedPool,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let service = DatabaseWorkPatchCommitService::new(pool.clone());
        let mut interval = tokio::time::interval(RECOVERY_INTERVAL);
        let mut recovery_cursor = None;
        let mut recovery_cycle_end = None;
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            if recovery_cycle_end.is_none() {
                recovery_cycle_end = match service.recovery_cycle_upper_bound().await {
                    Ok(cycle_end) => cycle_end,
                    Err(error) => {
                        tracing::warn!(%error, "Work patch commit recovery scan failed");
                        continue;
                    }
                };
            }
            let Some(cycle_end) = recovery_cycle_end.as_ref() else {
                recovery_cursor = None;
                continue;
            };
            let pending = match service
                .list_pending_for_recovery(RECOVERY_BATCH, recovery_cursor.as_ref(), cycle_end)
                .await
            {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::warn!(%error, "Work patch commit recovery scan failed");
                    continue;
                }
            };
            let cycle_complete = pending.len() < usize::from(RECOVERY_BATCH)
                || pending
                    .last()
                    .is_some_and(|item| item.operation.operation_id == *cycle_end);
            recovery_cursor = pending
                .last()
                .map(|item| item.operation.operation_id.clone());
            stream::iter(pending)
                .for_each_concurrent(RECOVERY_CONCURRENCY, |item| {
                    let pool = pool.clone();
                    async move {
                        if let Err(error) = drive_commit(pool, item.clone()).await
                            && !matches!(error, WorkPatchCommitError::ExecutorConflict)
                        {
                            tracing::warn!(
                                owner_id = item.owner_id.as_str(),
                                work_id = item.operation.work_id.as_str(),
                                operation_id = item.operation.operation_id.as_str(),
                                %error,
                                "Work patch commit recovery attempt failed"
                            );
                        }
                    }
                })
                .await;
            if cycle_complete {
                recovery_cursor = None;
                recovery_cycle_end = None;
            }
        }
    })
}

async fn drive_commit(
    pool: SharedPool,
    item: WorkPatchCommitRecoveryItem,
) -> Result<(), WorkPatchCommitError> {
    let service = DatabaseWorkPatchCommitService::new(pool.clone());
    match item.operation.phase {
        WorkPatchCommitPhase::AwaitingDispatch => {
            drive_awaiting_dispatch(&service, &pool, &item).await
        }
        WorkPatchCommitPhase::Committing | WorkPatchCommitPhase::Reconciling => {
            drive_reconciliation(&service, &pool, &item).await
        }
        WorkPatchCommitPhase::Complete => Ok(()),
    }
}

async fn drive_awaiting_dispatch(
    service: &DatabaseWorkPatchCommitService,
    pool: &SharedPool,
    item: &WorkPatchCommitRecoveryItem,
) -> Result<(), WorkPatchCommitError> {
    let executor_token = format!("server-commit-{}", Uuid::now_v7());
    let invocation = provider_invocation_ref(item);
    let workspace = match resolve_workspace(pool, item).await {
        Ok(workspace) => workspace,
        Err(WorkspaceResolutionError::Definitive(code)) => {
            service
                .claim_committing(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                    &executor_token,
                    &invocation,
                )
                .await?;
            record_failure(service, item, executor_token, invocation, code, None).await?;
            return Ok(());
        }
        Err(WorkspaceResolutionError::Retry(error)) => {
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch commit workspace resolution will be retried before dispatch"
            );
            service
                .defer_recovery(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                )
                .await?;
            return Ok(());
        }
    };
    let patch = match service
        .load_patch_payload(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
        )
        .await
    {
        Ok(patch) => patch,
        Err(WorkPatchCommitError::Database(error)) => {
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch commit payload read will be retried before dispatch"
            );
            service
                .defer_recovery(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                )
                .await?;
            return Ok(());
        }
        Err(error) => {
            service
                .claim_committing(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                    &executor_token,
                    &invocation,
                )
                .await?;
            record_failure(
                service,
                item,
                executor_token,
                invocation,
                WorkPatchCommitFailureCode::PatchRejected,
                None,
            )
            .await?;
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch commit payload failed durable validation"
            );
            return Ok(());
        }
    };
    service
        .claim_committing(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
            &executor_token,
            &invocation,
        )
        .await?;
    let metadata = GitWorktreeCommitMetadata {
        message: item.operation.message.clone(),
        author_name: item.operation.author_name.clone(),
        author_email: item.operation.author_email.clone(),
    };
    match commit_reviewed_git_patch(
        &workspace,
        &item.operation.base_subject_revision,
        &item.operation.result_subject_revision,
        &patch,
        &metadata,
    )
    .await
    {
        GitWorktreeCommitOutcome::Committed {
            commit_sha,
            observed_revision: Some(observed_subject_revision),
            index_reconciled,
        } => {
            service
                .record_committed(&WorkPatchCommitCommitted {
                    owner_id: item.owner_id.clone(),
                    work_id: item.operation.work_id.clone(),
                    operation_id: item.operation.operation_id.clone(),
                    executor_token,
                    provider_invocation_ref: invocation,
                    commit_sha,
                    observed_subject_revision,
                    index_reconciled,
                })
                .await?;
        }
        GitWorktreeCommitOutcome::Committed {
            observed_revision: None,
            ..
        } => {
            // HEAD may already have advanced. Preserve the invocation and let
            // the expired lease enter exact tree/parent reconciliation.
        }
        GitWorktreeCommitOutcome::NotCreated {
            code,
            observed_revision,
        } => {
            record_failure(
                service,
                item,
                executor_token,
                invocation,
                map_not_created(code),
                observed_revision,
            )
            .await?;
        }
    }
    Ok(())
}

async fn drive_reconciliation(
    service: &DatabaseWorkPatchCommitService,
    pool: &SharedPool,
    item: &WorkPatchCommitRecoveryItem,
) -> Result<(), WorkPatchCommitError> {
    let invocation = item
        .operation
        .commit_invocation_ref
        .clone()
        .ok_or_else(|| WorkPatchCommitError::NeedsRepair("missing commit invocation".into()))?;
    let executor_token = format!("server-commit-reconciler-{}", Uuid::now_v7());
    service
        .claim_reconciliation(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
            &executor_token,
            &invocation,
        )
        .await?;
    let workspace = match resolve_workspace(pool, item).await {
        Ok(workspace) => workspace,
        Err(_) => return Ok(()),
    };
    let patch = match service
        .load_patch_payload(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
        )
        .await
    {
        Ok(patch) => patch,
        Err(_) => return Ok(()),
    };
    match reconcile_reviewed_git_patch_commit(
        &workspace,
        &item.operation.base_subject_revision,
        &item.operation.result_subject_revision,
        &patch,
    )
    .await
    {
        GitReviewedCommitReconciliation::Committed {
            commit_sha,
            observed_revision,
            index_reconciled,
        } => {
            service
                .record_committed(&WorkPatchCommitCommitted {
                    owner_id: item.owner_id.clone(),
                    work_id: item.operation.work_id.clone(),
                    operation_id: item.operation.operation_id.clone(),
                    executor_token,
                    provider_invocation_ref: invocation,
                    commit_sha,
                    observed_subject_revision: observed_revision,
                    index_reconciled,
                })
                .await?;
        }
        GitReviewedCommitReconciliation::NotCommitted { observed_revision } => {
            record_failure(
                service,
                item,
                executor_token,
                invocation,
                WorkPatchCommitFailureCode::CommitRejected,
                Some(observed_revision),
            )
            .await?;
        }
        GitReviewedCommitReconciliation::Diverged { observed_revision } => {
            record_failure(
                service,
                item,
                executor_token,
                invocation,
                WorkPatchCommitFailureCode::ResultChanged,
                observed_revision,
            )
            .await?;
        }
    }
    Ok(())
}

async fn resolve_workspace(
    pool: &SharedPool,
    item: &WorkPatchCommitRecoveryItem,
) -> Result<PathBuf, WorkspaceResolutionError> {
    if item.operation.provider_ref
        != WorkPatchCommitProviderRef::parse(SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF)
            .expect("static provider ref")
    {
        return Err(WorkspaceResolutionError::Definitive(
            WorkPatchCommitFailureCode::AuthorizationDenied,
        ));
    }
    let binding = DatabaseWorkRepository::new(pool.clone())
        .load_branch_runtime_binding(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.target_branch_id,
        )
        .await
        .map_err(|error| match error {
            astra_services::work::WorkRepositoryError::Persistence { .. } => {
                WorkspaceResolutionError::Retry(error.to_string())
            }
            _ => WorkspaceResolutionError::Definitive(
                WorkPatchCommitFailureCode::WorkspaceUnavailable,
            ),
        })?;
    let entry = DatabaseWorkspaceRecordStore::new(pool.clone())
        .load_workspace_record(item.owner_id.as_str(), binding.session_id.as_str())
        .await
        .map_err(|error| match error {
            WorkspaceRecordStoreError::Database(_) | WorkspaceRecordStoreError::Unavailable(_) => {
                WorkspaceResolutionError::Retry(error.to_string())
            }
            _ => WorkspaceResolutionError::Definitive(
                WorkPatchCommitFailureCode::WorkspaceUnavailable,
            ),
        })?
        .filter(|entry| entry.session_id.as_deref() == Some(binding.session_id.as_str()))
        .ok_or(WorkspaceResolutionError::Definitive(
            WorkPatchCommitFailureCode::WorkspaceUnavailable,
        ))?;
    if entry.record.kind != WorkspaceBindingKind::ServerSandbox
        || entry.record.authority != WorkspaceAuthority::ReadWrite
        || entry.record.persistence != WorkspacePersistence::Session
    {
        return Err(WorkspaceResolutionError::Definitive(
            WorkPatchCommitFailureCode::AuthorizationDenied,
        ));
    }
    Ok(PathBuf::from(entry.record.root_or_volume_ref))
}

enum WorkspaceResolutionError {
    Definitive(WorkPatchCommitFailureCode),
    Retry(String),
}

async fn record_failure(
    service: &DatabaseWorkPatchCommitService,
    item: &WorkPatchCommitRecoveryItem,
    executor_token: String,
    invocation: WorkProviderInvocationRef,
    failure_code: WorkPatchCommitFailureCode,
    observed_subject_revision: Option<astra_services::work::WorkContentHash>,
) -> Result<(), WorkPatchCommitError> {
    service
        .record_failure(&WorkPatchCommitFailure {
            owner_id: item.owner_id.clone(),
            work_id: item.operation.work_id.clone(),
            operation_id: item.operation.operation_id.clone(),
            executor_token,
            provider_invocation_ref: invocation,
            failure_code,
            observed_subject_revision,
        })
        .await?;
    Ok(())
}

fn provider_invocation_ref(item: &WorkPatchCommitRecoveryItem) -> WorkProviderInvocationRef {
    WorkProviderInvocationRef::parse(format!(
        "server-git-commit:{}",
        item.operation.operation_id.as_str()
    ))
    .expect("bounded operation identity creates a valid provider invocation")
}

fn map_not_created(code: GitWorktreeCommitNotCreatedCode) -> WorkPatchCommitFailureCode {
    match code {
        GitWorktreeCommitNotCreatedCode::InvalidMetadata => {
            WorkPatchCommitFailureCode::InvalidMetadata
        }
        GitWorktreeCommitNotCreatedCode::BaseChanged => WorkPatchCommitFailureCode::BaseChanged,
        GitWorktreeCommitNotCreatedCode::ResultChanged => WorkPatchCommitFailureCode::ResultChanged,
        GitWorktreeCommitNotCreatedCode::PatchRejected => WorkPatchCommitFailureCode::PatchRejected,
        GitWorktreeCommitNotCreatedCode::CommitRejected => {
            WorkPatchCommitFailureCode::CommitRejected
        }
        GitWorktreeCommitNotCreatedCode::RefConflict => WorkPatchCommitFailureCode::RefConflict,
        GitWorktreeCommitNotCreatedCode::WorkspaceUnavailable => {
            WorkPatchCommitFailureCode::WorkspaceUnavailable
        }
        GitWorktreeCommitNotCreatedCode::ProviderUnavailable => {
            WorkPatchCommitFailureCode::ProviderUnavailable
        }
    }
}
