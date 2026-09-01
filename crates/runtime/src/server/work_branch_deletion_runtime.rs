use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_services::{
    runs::RunLifecycleService,
    work::{
        DatabaseWorkBranchDeletionService, WorkBranchDeletionError,
        WorkBranchDeletionExecutionClaim, WorkBranchDeletionOperation, WorkBranchDeletionPhase,
    },
};
use axum::http::StatusCode;
use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;

const RECOVERY_BATCH: u16 = 16;
const RECOVERY_CONCURRENCY: usize = 4;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) enum WorkBranchDeletionDriveResult {
    Terminal(WorkBranchDeletionOperation),
    Deferred(WorkBranchDeletionOperation),
}

pub(crate) enum WorkBranchDeletionDriveError {
    CancellationUnavailable(StatusCode),
    Service(WorkBranchDeletionError),
}

pub(crate) async fn drive_claimed_work_branch_deletion(
    service: &DatabaseWorkBranchDeletionService,
    run_lifecycle: &Arc<dyn RunLifecycleService>,
    claim: &WorkBranchDeletionExecutionClaim,
) -> Result<WorkBranchDeletionDriveResult, WorkBranchDeletionDriveError> {
    let execution = async {
        let mut operation = claim.operation.clone();
        if operation.phase == WorkBranchDeletionPhase::Fence {
            run_lifecycle
                .cancel_session_runs(
                    claim.session_id.as_str().to_owned(),
                    claim.owner_id.as_str().to_owned(),
                )
                .await
                .map_err(|(status, _)| {
                    WorkBranchDeletionDriveError::CancellationUnavailable(status)
                })?;
            operation = service
                .fence_session(
                    &claim.owner_id,
                    &claim.work_id,
                    &claim.branch_id,
                    &claim.operation.operation_id,
                    &claim.executor_token,
                )
                .await
                .map_err(WorkBranchDeletionDriveError::Service)?;
        }
        if operation.phase == WorkBranchDeletionPhase::SessionCleanup {
            operation = service
                .cleanup_session(
                    &claim.owner_id,
                    &claim.work_id,
                    &claim.branch_id,
                    &claim.operation.operation_id,
                    &claim.executor_token,
                )
                .await
                .map_err(WorkBranchDeletionDriveError::Service)?;
        }
        if operation.phase == WorkBranchDeletionPhase::LineageGc {
            operation = service
                .reconcile_lineage(
                    &claim.owner_id,
                    &claim.work_id,
                    &claim.branch_id,
                    &claim.operation.operation_id,
                    &claim.executor_token,
                )
                .await
                .map_err(WorkBranchDeletionDriveError::Service)?;
        }
        if operation.phase == WorkBranchDeletionPhase::BranchCleanup {
            operation = service
                .complete_branch_cleanup(
                    &claim.owner_id,
                    &claim.work_id,
                    &claim.branch_id,
                    &claim.operation.operation_id,
                    &claim.executor_token,
                )
                .await
                .map_err(WorkBranchDeletionDriveError::Service)?;
        }
        Ok(operation)
    }
    .await;

    match execution {
        Ok(operation) => Ok(WorkBranchDeletionDriveResult::Terminal(operation)),
        Err(WorkBranchDeletionDriveError::Service(
            WorkBranchDeletionError::ActiveRuns | WorkBranchDeletionError::LineagePending { .. },
        )) => {
            release_claim(service, claim).await;
            let operation = service
                .load(
                    &claim.owner_id,
                    &claim.work_id,
                    &claim.branch_id,
                    &claim.operation.operation_id,
                )
                .await
                .map_err(WorkBranchDeletionDriveError::Service)?;
            Ok(WorkBranchDeletionDriveResult::Deferred(operation))
        }
        Err(error) => {
            release_claim(service, claim).await;
            Err(error)
        }
    }
}

async fn release_claim(
    service: &DatabaseWorkBranchDeletionService,
    claim: &WorkBranchDeletionExecutionClaim,
) {
    if let Err(error) = service
        .release_execution(
            &claim.owner_id,
            &claim.work_id,
            &claim.branch_id,
            &claim.operation.operation_id,
            &claim.executor_token,
        )
        .await
    {
        tracing::warn!(
            owner_id = claim.owner_id.as_str(),
            work_id = claim.work_id.as_str(),
            branch_id = claim.branch_id.as_str(),
            operation_id = claim.operation.operation_id,
            %error,
            "failed to release Work deletion executor"
        );
    }
}

pub(crate) fn spawn_work_branch_deletion_recovery(
    pool: SharedPool,
    run_lifecycle: Arc<dyn RunLifecycleService>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let service = DatabaseWorkBranchDeletionService::new(pool);
        let mut interval = tokio::time::interval(RECOVERY_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            let claims = match service.claim_pending_executions(RECOVERY_BATCH).await {
                Ok(claims) => claims,
                Err(error) => {
                    tracing::warn!(%error, "Work branch deletion recovery scan failed");
                    continue;
                }
            };
            stream::iter(claims)
                .for_each_concurrent(RECOVERY_CONCURRENCY, |claim| {
                    let service = service.clone();
                    let run_lifecycle = run_lifecycle.clone();
                    async move {
                        match drive_claimed_work_branch_deletion(&service, &run_lifecycle, &claim)
                            .await
                        {
                            Ok(WorkBranchDeletionDriveResult::Terminal(operation)) => {
                                tracing::info!(
                                    owner_id = claim.owner_id.as_str(),
                                    work_id = claim.work_id.as_str(),
                                    branch_id = claim.branch_id.as_str(),
                                    operation_id = operation.operation_id,
                                    "recovered Work branch deletion"
                                );
                            }
                            Ok(WorkBranchDeletionDriveResult::Deferred(_)) => {}
                            Err(WorkBranchDeletionDriveError::CancellationUnavailable(status)) => {
                                tracing::warn!(
                                    owner_id = claim.owner_id.as_str(),
                                    work_id = claim.work_id.as_str(),
                                    branch_id = claim.branch_id.as_str(),
                                    operation_id = claim.operation.operation_id,
                                    %status,
                                    "Work branch deletion cancellation is unavailable"
                                );
                            }
                            Err(WorkBranchDeletionDriveError::Service(error)) => {
                                tracing::warn!(
                                    owner_id = claim.owner_id.as_str(),
                                    work_id = claim.work_id.as_str(),
                                    branch_id = claim.branch_id.as_str(),
                                    operation_id = claim.operation.operation_id,
                                    %error,
                                    "Work branch deletion recovery attempt failed"
                                );
                            }
                        }
                    }
                })
                .await;
        }
    })
}
