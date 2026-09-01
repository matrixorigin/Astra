use std::{path::PathBuf, time::Duration};

use astra_core::SharedPool;
use astra_runtime_env::{WorkspaceAuthority, WorkspaceBindingKind, WorkspacePersistence};
use astra_services::{
    DatabaseWorkspaceRecordStore, WorkspaceRecordStore, WorkspaceRecordStoreError,
    work::{
        DatabaseWorkPatchMaterializationService, DatabaseWorkRepository,
        SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF, WorkMaterializationProviderRef,
        WorkPatchMaterializationApplied, WorkPatchMaterializationError,
        WorkPatchMaterializationFailureCode, WorkPatchMaterializationNotApplied,
        WorkPatchMaterializationPhase, WorkPatchMaterializationRecoveryItem,
        WorkProviderInvocationRef, WorkRepository,
    },
};
use astra_tools::patch_materialization::{
    GitPatchMaterializationOutcome, GitPatchNotAppliedCode, materialize_git_patch,
    observe_git_worktree_revision,
};
use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RECOVERY_BATCH: u16 = 16;
const RECOVERY_CONCURRENCY: usize = 4;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn spawn_work_patch_materialization_recovery(
    pool: SharedPool,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let service = DatabaseWorkPatchMaterializationService::new(pool.clone());
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
                        tracing::warn!(%error, "Work patch materialization recovery scan failed");
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
                    tracing::warn!(%error, "Work patch materialization recovery scan failed");
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
                        if let Err(error) = drive_materialization(pool, item.clone()).await
                            && !matches!(
                                error,
                                WorkPatchMaterializationError::ExecutorConflict
                                    | WorkPatchMaterializationError::VerificationRequired
                            )
                        {
                            tracing::warn!(
                                owner_id = item.owner_id.as_str(),
                                work_id = item.operation.work_id.as_str(),
                                operation_id = item.operation.operation_id.as_str(),
                                %error,
                                "Work patch materialization recovery attempt failed"
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

async fn drive_materialization(
    pool: SharedPool,
    item: WorkPatchMaterializationRecoveryItem,
) -> Result<(), WorkPatchMaterializationError> {
    let service = DatabaseWorkPatchMaterializationService::new(pool.clone());
    match item.operation.phase {
        WorkPatchMaterializationPhase::AwaitingDispatch => {
            drive_awaiting_dispatch(&service, &pool, &item).await
        }
        WorkPatchMaterializationPhase::Applying | WorkPatchMaterializationPhase::Reconciling => {
            drive_reconciliation(&service, &pool, &item).await
        }
        WorkPatchMaterializationPhase::Verifying => {
            drive_verification(&service, &pool, &item).await
        }
        WorkPatchMaterializationPhase::Complete => Ok(()),
    }
}

async fn drive_verification(
    service: &DatabaseWorkPatchMaterializationService,
    pool: &SharedPool,
    item: &WorkPatchMaterializationRecoveryItem,
) -> Result<(), WorkPatchMaterializationError> {
    let workspace = match resolve_workspace(pool, item).await {
        Ok(workspace) => workspace,
        Err(_) => {
            defer_recovery(service, item).await?;
            return Ok(());
        }
    };
    let Ok(observed_revision) = observe_git_worktree_revision(&workspace).await else {
        defer_recovery(service, item).await?;
        return Ok(());
    };
    if observed_revision != item.operation.result_subject_revision {
        let expected_branch_revision = item
            .operation
            .target_branch_revision
            .checked_next()
            .map_err(|error| WorkPatchMaterializationError::NeedsRepair(error.to_string()))?;
        DatabaseWorkRepository::new(pool.clone())
            .invalidate_branch_subject(astra_services::work::WorkBranchSubjectInvalidation {
                owner_id: item.owner_id.clone(),
                work_id: item.operation.work_id.clone(),
                branch_id: item.operation.target_branch_id.clone(),
                expected_branch_revision,
                graph_revision: item.operation.target_graph_revision,
                source_ref: astra_services::work::WorkChangeRef::parse(format!(
                    "materialization-drift:{}",
                    item.operation.operation_id.as_str()
                ))
                .map_err(|error| WorkPatchMaterializationError::NeedsRepair(error.to_string()))?,
            })
            .await?;
    }
    match service
        .complete_verification(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(WorkPatchMaterializationError::VerificationRequired) => {
            defer_recovery(service, item).await
        }
        Err(error) => Err(error),
    }
}

async fn drive_awaiting_dispatch(
    service: &DatabaseWorkPatchMaterializationService,
    pool: &SharedPool,
    item: &WorkPatchMaterializationRecoveryItem,
) -> Result<(), WorkPatchMaterializationError> {
    let executor_token = format!("server-materializer-{}", Uuid::now_v7());
    let invocation = provider_invocation_ref(item);
    let workspace = match resolve_workspace(pool, item).await {
        Ok(workspace) => workspace,
        Err(WorkspaceResolutionError::Definitive(code)) => {
            service
                .claim_applying(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                    &executor_token,
                    &invocation,
                )
                .await?;
            record_not_applied(service, item, executor_token, invocation, code).await?;
            return Ok(());
        }
        Err(WorkspaceResolutionError::Retry(error)) => {
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch workspace resolution will be retried before dispatch"
            );
            defer_recovery(service, item).await?;
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
        Err(WorkPatchMaterializationError::Database(error)) => {
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch payload read will be retried before dispatch"
            );
            defer_recovery(service, item).await?;
            return Ok(());
        }
        Err(error) => {
            service
                .claim_applying(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                    &executor_token,
                    &invocation,
                )
                .await?;
            record_not_applied(
                service,
                item,
                executor_token,
                invocation,
                WorkPatchMaterializationFailureCode::ProviderInternal,
            )
            .await?;
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch payload failed durable validation"
            );
            return Ok(());
        }
    };
    service
        .claim_applying(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
            &executor_token,
            &invocation,
        )
        .await?;
    match materialize_git_patch(&workspace, &item.operation.base_subject_revision, &patch).await {
        GitPatchMaterializationOutcome::Applied { observed_revision } => {
            record_observed(service, item, executor_token, invocation, observed_revision).await?;
        }
        GitPatchMaterializationOutcome::NotApplied {
            code: GitPatchNotAppliedCode::BaseChanged,
            observed_revision: Some(observed_revision),
        }
        | GitPatchMaterializationOutcome::UnknownEffect {
            observed_revision: Some(observed_revision),
            ..
        } => {
            // The provider did not prove the requested result, but it did
            // prove the exact current target. Persisting that observation
            // invalidates the stale canonical subject and ends in conflict.
            record_observed(service, item, executor_token, invocation, observed_revision).await?;
        }
        GitPatchMaterializationOutcome::NotApplied { code, .. } => {
            record_not_applied(
                service,
                item,
                executor_token,
                invocation,
                map_not_applied(code),
            )
            .await?;
        }
        GitPatchMaterializationOutcome::UnknownEffect {
            observed_revision: None,
            ..
        } => {
            // Keep the exact invocation in Applying. After its lease expires,
            // reconciliation observes the workspace and never invokes apply again.
        }
    }
    Ok(())
}

async fn drive_reconciliation(
    service: &DatabaseWorkPatchMaterializationService,
    pool: &SharedPool,
    item: &WorkPatchMaterializationRecoveryItem,
) -> Result<(), WorkPatchMaterializationError> {
    let invocation =
        item.operation.apply_invocation_ref.clone().ok_or_else(|| {
            WorkPatchMaterializationError::NeedsRepair("missing invocation".into())
        })?;
    let executor_token = format!("server-reconciler-{}", Uuid::now_v7());
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
    let Ok(observed_revision) = observe_git_worktree_revision(&workspace).await else {
        return Ok(());
    };
    if observed_revision == item.operation.base_subject_revision {
        record_not_applied(
            service,
            item,
            executor_token,
            invocation,
            WorkPatchMaterializationFailureCode::ProviderInternal,
        )
        .await?;
    } else {
        record_observed(service, item, executor_token, invocation, observed_revision).await?;
    }
    Ok(())
}

async fn resolve_workspace(
    pool: &SharedPool,
    item: &WorkPatchMaterializationRecoveryItem,
) -> Result<PathBuf, WorkspaceResolutionError> {
    if item.operation.provider_ref
        != WorkMaterializationProviderRef::parse(SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF)
            .expect("static provider ref")
    {
        return Err(WorkspaceResolutionError::Definitive(
            WorkPatchMaterializationFailureCode::AuthorizationDenied,
        ));
    }
    let repository = DatabaseWorkRepository::new(pool.clone());
    let binding = repository
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
                WorkPatchMaterializationFailureCode::WorkspaceUnavailable,
            ),
        })?;
    let store = DatabaseWorkspaceRecordStore::new(pool.clone());
    let entry = store
        .load_workspace_record(item.owner_id.as_str(), binding.session_id.as_str())
        .await
        .map_err(|error| match error {
            WorkspaceRecordStoreError::Database(_) | WorkspaceRecordStoreError::Unavailable(_) => {
                WorkspaceResolutionError::Retry(error.to_string())
            }
            _ => WorkspaceResolutionError::Definitive(
                WorkPatchMaterializationFailureCode::WorkspaceUnavailable,
            ),
        })?
        .filter(|entry| entry.session_id.as_deref() == Some(binding.session_id.as_str()))
        .ok_or(WorkspaceResolutionError::Definitive(
            WorkPatchMaterializationFailureCode::WorkspaceUnavailable,
        ))?;
    if entry.record.kind != WorkspaceBindingKind::ServerSandbox
        || entry.record.authority != WorkspaceAuthority::ReadWrite
        || entry.record.persistence != WorkspacePersistence::Session
    {
        return Err(WorkspaceResolutionError::Definitive(
            WorkPatchMaterializationFailureCode::AuthorizationDenied,
        ));
    }
    Ok(PathBuf::from(entry.record.root_or_volume_ref))
}

enum WorkspaceResolutionError {
    Definitive(WorkPatchMaterializationFailureCode),
    Retry(String),
}

async fn record_observed(
    service: &DatabaseWorkPatchMaterializationService,
    item: &WorkPatchMaterializationRecoveryItem,
    executor_token: String,
    invocation: WorkProviderInvocationRef,
    observed_subject_revision: astra_services::work::WorkContentHash,
) -> Result<(), WorkPatchMaterializationError> {
    let operation = service
        .record_applied(&WorkPatchMaterializationApplied {
            owner_id: item.owner_id.clone(),
            work_id: item.operation.work_id.clone(),
            operation_id: item.operation.operation_id.clone(),
            executor_token,
            provider_invocation_ref: invocation,
            observed_subject_revision,
        })
        .await?;
    if operation.phase == WorkPatchMaterializationPhase::Verifying {
        match service
            .complete_verification(
                &item.owner_id,
                &item.operation.work_id,
                &item.operation.operation_id,
            )
            .await
        {
            Ok(_) | Err(WorkPatchMaterializationError::VerificationRequired) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn record_not_applied(
    service: &DatabaseWorkPatchMaterializationService,
    item: &WorkPatchMaterializationRecoveryItem,
    executor_token: String,
    invocation: WorkProviderInvocationRef,
    failure_code: WorkPatchMaterializationFailureCode,
) -> Result<(), WorkPatchMaterializationError> {
    service
        .record_not_applied(&WorkPatchMaterializationNotApplied {
            owner_id: item.owner_id.clone(),
            work_id: item.operation.work_id.clone(),
            operation_id: item.operation.operation_id.clone(),
            executor_token,
            provider_invocation_ref: invocation,
            failure_code,
        })
        .await?;
    Ok(())
}

async fn defer_recovery(
    service: &DatabaseWorkPatchMaterializationService,
    item: &WorkPatchMaterializationRecoveryItem,
) -> Result<(), WorkPatchMaterializationError> {
    service
        .defer_recovery(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
        )
        .await
}

fn provider_invocation_ref(
    item: &WorkPatchMaterializationRecoveryItem,
) -> WorkProviderInvocationRef {
    WorkProviderInvocationRef::parse(format!(
        "server-git:{}",
        item.operation.operation_id.as_str()
    ))
    .expect("bounded operation identity creates a valid provider invocation")
}

fn map_not_applied(code: GitPatchNotAppliedCode) -> WorkPatchMaterializationFailureCode {
    match code {
        GitPatchNotAppliedCode::ProviderUnavailable => {
            WorkPatchMaterializationFailureCode::ProviderUnavailable
        }
        GitPatchNotAppliedCode::WorkspaceUnavailable | GitPatchNotAppliedCode::BaseChanged => {
            WorkPatchMaterializationFailureCode::WorkspaceUnavailable
        }
        GitPatchNotAppliedCode::PatchRejected => WorkPatchMaterializationFailureCode::PatchRejected,
    }
}
