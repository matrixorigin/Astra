use super::work_patch_workspace::{WorkspaceResolutionError, resolve_workspace};
use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_services::{
    runs::RunLifecycleService,
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
    GitPatchMaterializationOutcome, GitPatchNotAppliedCode, GitWorkspaceObservationError,
    materialize_git_patch_with_workspace_lease, observe_git_worktree_revision_with_workspace_lease,
};
use astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options;
use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RECOVERY_BATCH: u16 = 16;
const RECOVERY_CONCURRENCY: usize = 4;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn spawn_work_patch_materialization_recovery(
    pool: SharedPool,
    lifecycle: Arc<dyn RunLifecycleService>,
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
                    let lifecycle = lifecycle.clone();
                    let cancel = cancel.clone();
                    async move {
                        if let Err(error) =
                            drive_materialization(pool, lifecycle.as_ref(), item.clone(), &cancel)
                                .await
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
    lifecycle: &dyn RunLifecycleService,
    item: WorkPatchMaterializationRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchMaterializationError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    if lifecycle.workspace_executor_id().is_none()
        || item.operation.provider_ref
            != WorkMaterializationProviderRef::parse(
                SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF,
            )
            .expect("static provider ref")
    {
        return Ok(());
    }
    let service = DatabaseWorkPatchMaterializationService::new(pool.clone());
    match item.operation.phase {
        WorkPatchMaterializationPhase::AwaitingDispatch => {
            drive_awaiting_dispatch(&service, &pool, lifecycle, &item, cancel).await
        }
        WorkPatchMaterializationPhase::Applying | WorkPatchMaterializationPhase::Reconciling => {
            drive_reconciliation(&service, &pool, lifecycle, &item, cancel).await
        }
        WorkPatchMaterializationPhase::Verifying => {
            drive_verification(&service, &pool, lifecycle, &item, cancel).await
        }
        WorkPatchMaterializationPhase::Complete => Ok(()),
    }
}

async fn drive_verification(
    service: &DatabaseWorkPatchMaterializationService,
    pool: &SharedPool,
    lifecycle: &dyn RunLifecycleService,
    item: &WorkPatchMaterializationRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchMaterializationError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let workspace = match resolve_workspace(
        pool,
        lifecycle,
        &item.owner_id,
        &item.operation.work_id,
        &item.operation.target_branch_id,
    )
    .await
    {
        Ok(workspace) => workspace,
        Err(WorkspaceResolutionError::NotThisExecutor) => return Ok(()),
        Err(WorkspaceResolutionError::UnverifiedUnavailable(error)) => {
            tracing::warn!(%error, "Work workspace ownership could not be established");
            return Ok(());
        }
    };
    let Ok(workspace_lease) = acquire_workspace_mutation_lease_with_options(
        &workspace,
        Some(cancel),
        Duration::from_secs(120),
    )
    .await
    else {
        if cancel.is_cancelled() {
            return Ok(());
        }
        defer_recovery(service, item).await?;
        return Ok(());
    };
    if !matches!(resolve_workspace(pool, lifecycle, &item.owner_id, &item.operation.work_id, &item.operation.target_branch_id).await, Ok(current) if current == workspace)
    {
        return Ok(());
    }

    let Ok(observed_revision) =
        observe_git_worktree_revision_with_workspace_lease(&workspace, &workspace_lease).await
    else {
        if !cancel.is_cancelled() {
            defer_recovery(service, item).await?;
        }
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
    lifecycle: &dyn RunLifecycleService,
    item: &WorkPatchMaterializationRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchMaterializationError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let executor_token = format!("server-materializer-{}", Uuid::now_v7());
    let invocation = provider_invocation_ref(item);
    let workspace = match resolve_workspace(
        pool,
        lifecycle,
        &item.owner_id,
        &item.operation.work_id,
        &item.operation.target_branch_id,
    )
    .await
    {
        Ok(workspace) => workspace,
        Err(WorkspaceResolutionError::NotThisExecutor) => return Ok(()),
        Err(WorkspaceResolutionError::UnverifiedUnavailable(error)) => {
            tracing::warn!(%error, "Work workspace ownership could not be established");
            return Ok(());
        }
    };
    if cancel.is_cancelled() {
        return Ok(());
    }
    let Ok(workspace_lease) = acquire_workspace_mutation_lease_with_options(
        &workspace,
        Some(cancel),
        Duration::from_secs(120),
    )
    .await
    else {
        // No provider invocation has started. Keep the durable operation in
        // its dispatch phase; the recovery scanner will retry without
        // fabricating a terminal no-op result.
        if !cancel.is_cancelled() {
            defer_recovery(service, item).await?;
        }
        return Ok(());
    };
    if !matches!(resolve_workspace(pool, lifecycle, &item.owner_id, &item.operation.work_id, &item.operation.target_branch_id).await, Ok(current) if current == workspace)
    {
        return Ok(());
    }
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
            if !cancel.is_cancelled() {
                defer_recovery(service, item).await?;
            }
            return Ok(());
        }
        Err(error) => {
            if cancel.is_cancelled() {
                return Ok(());
            }
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

    if cancel.is_cancelled() {
        return Ok(());
    }
    service
        .claim_applying(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
            &executor_token,
            &invocation,
        )
        .await?;
    match materialize_git_patch_with_workspace_lease(
        &workspace,
        &item.operation.base_subject_revision,
        &patch,
        &workspace_lease,
    )
    .await
    {
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
            if code != GitPatchNotAppliedCode::WorkspaceUnavailable {
                record_not_applied(
                    service,
                    item,
                    executor_token,
                    invocation,
                    map_not_applied(code),
                )
                .await?;
            }
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
    lifecycle: &dyn RunLifecycleService,
    item: &WorkPatchMaterializationRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchMaterializationError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let invocation =
        item.operation.apply_invocation_ref.clone().ok_or_else(|| {
            WorkPatchMaterializationError::NeedsRepair("missing invocation".into())
        })?;
    let executor_token = format!("server-reconciler-{}", Uuid::now_v7());
    let workspace = match resolve_workspace(
        pool,
        lifecycle,
        &item.owner_id,
        &item.operation.work_id,
        &item.operation.target_branch_id,
    )
    .await
    {
        Ok(workspace) => workspace,
        Err(_) => return Ok(()),
    };
    let Ok(workspace_lease) = acquire_workspace_mutation_lease_with_options(
        &workspace,
        Some(cancel),
        Duration::from_secs(120),
    )
    .await
    else {
        return Ok(());
    };
    if !matches!(resolve_workspace(pool, lifecycle, &item.owner_id, &item.operation.work_id, &item.operation.target_branch_id).await, Ok(current) if current == workspace)
    {
        return Ok(());
    }

    if cancel.is_cancelled() {
        return Ok(());
    }
    service
        .claim_reconciliation(
            &item.owner_id,
            &item.operation.work_id,
            &item.operation.operation_id,
            &executor_token,
            &invocation,
        )
        .await?;
    let observed_revision = match observe_git_worktree_revision_with_workspace_lease(
        &workspace,
        &workspace_lease,
    )
    .await
    {
        Ok(observed_revision) => observed_revision,
        Err(
            error @ (GitWorkspaceObservationError::NotWorktreeRoot
            | GitWorkspaceObservationError::ObservationRejected
            | GitWorkspaceObservationError::UnsafePath),
        ) => {
            let _ = error;
            record_not_applied(
                service,
                item,
                executor_token,
                invocation,
                WorkPatchMaterializationFailureCode::InvalidWorkspace,
            )
            .await?;
            return Ok(());
        }
        Err(_) => return Ok(()),
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
        GitPatchNotAppliedCode::InvalidWorkspace => {
            WorkPatchMaterializationFailureCode::InvalidWorkspace
        }
        GitPatchNotAppliedCode::PatchRejected => WorkPatchMaterializationFailureCode::PatchRejected,
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::server::work_test_support::{
        PatchRuntimeFixture, cleanup_work_owner, patch_operation_ownership,
    };

    #[tokio::test]
    #[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
    async fn recovery_applies_real_git_only_on_selected_executor_and_waits_for_verification() {
        use astra_services::work::*;
        let fixture = PatchRuntimeFixture::new().await;
        fixture.reset_to_patch_base().await;
        let repository = DatabaseWorkRepository::new(fixture.pool.clone());
        let binding = repository
            .load_branch_runtime_binding(&fixture.owner, &fixture.work, &fixture.branch)
            .await
            .unwrap();
        let service = DatabaseWorkPatchMaterializationService::new(fixture.pool.clone());
        let operation = service
            .admit(&WorkPatchMaterializationRequest {
                owner_id: fixture.owner.clone(),
                work_id: fixture.work.clone(),
                target_branch_id: fixture.branch.clone(),
                request_id: WorkChangeRef::parse("materialize-real-patch").unwrap(),
                patch_artifact_id: fixture.patch.patch_artifact_id.clone(),
                expected_target_branch_revision: binding.branch_revision,
                expected_target_graph_revision: binding.graph_revision,
                provider_ref: WorkMaterializationProviderRef::parse(
                    SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF,
                )
                .unwrap(),
                policy_decision_ref: WorkChangeRef::parse("fixture-approved").unwrap(),
            })
            .await
            .unwrap();
        let item = WorkPatchMaterializationRecoveryItem {
            owner_id: fixture.owner.clone(),
            operation: operation.clone(),
        };
        let cancel = CancellationToken::new();
        let physical_lease = acquire_workspace_mutation_lease_with_options(
            &fixture.workspace,
            None,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let before_wait = patch_operation_ownership(
            &fixture.pool,
            "work_patch_materialization_operations",
            operation.operation_id.as_str(),
        )
        .await;
        {
            let waiting_cancel = CancellationToken::new();
            let attempt = drive_materialization(
                fixture.pool.clone(),
                &fixture.lifecycle,
                item.clone(),
                &waiting_cancel,
            );
            tokio::pin!(attempt);
            tokio::select! {
                result = &mut attempt => panic!("execution bypassed physical lease: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
            assert_eq!(
                patch_operation_ownership(
                    &fixture.pool,
                    "work_patch_materialization_operations",
                    operation.operation_id.as_str()
                )
                .await,
                before_wait
            );
            waiting_cancel.cancel();
            tokio::time::timeout(Duration::from_secs(2), &mut attempt)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                patch_operation_ownership(
                    &fixture.pool,
                    "work_patch_materialization_operations",
                    operation.operation_id.as_str()
                )
                .await,
                before_wait
            );
        }
        drop(physical_lease);

        let ownership = patch_operation_ownership(
            &fixture.pool,
            "work_patch_materialization_operations",
            operation.operation_id.as_str(),
        )
        .await;
        drive_materialization(
            fixture.pool.clone(),
            &fixture.foreign,
            item.clone(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("file.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(
            patch_operation_ownership(
                &fixture.pool,
                "work_patch_materialization_operations",
                operation.operation_id.as_str()
            )
            .await,
            ownership
        );
        drive_materialization(
            fixture.pool.clone(),
            &fixture.lifecycle,
            item.clone(),
            &cancel,
        )
        .await
        .unwrap();
        let applied = service
            .load(
                &fixture.owner,
                &fixture.work,
                &fixture.branch,
                &operation.operation_id,
            )
            .await
            .unwrap();
        assert_eq!(applied.phase, WorkPatchMaterializationPhase::Verifying);
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("file.txt")).unwrap(),
            "after\n"
        );
        let subject = repository
            .load_branch_subject(&fixture.owner, &fixture.work, &fixture.branch)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            subject.subject_revision,
            fixture.patch.result_subject_revision
        );
        let ownership = patch_operation_ownership(
            &fixture.pool,
            "work_patch_materialization_operations",
            operation.operation_id.as_str(),
        )
        .await;
        drive_materialization(
            fixture.pool.clone(),
            &fixture.foreign,
            WorkPatchMaterializationRecoveryItem {
                owner_id: fixture.owner.clone(),
                operation: applied.clone(),
            },
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            patch_operation_ownership(
                &fixture.pool,
                "work_patch_materialization_operations",
                operation.operation_id.as_str()
            )
            .await,
            ownership
        );
        assert!(matches!(
            drive_materialization(fixture.pool.clone(), &fixture.lifecycle, item, &cancel).await,
            Err(WorkPatchMaterializationError::ExecutorConflict)
        ));
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("file.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "1");
        cleanup_work_owner(&fixture.pool, fixture.owner.as_str()).await;
    }
    #[tokio::test]
    #[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
    async fn expired_invocation_reconciles_real_effect_without_repeating_mutation() {
        use astra_services::work::*;
        let fixture = PatchRuntimeFixture::new().await;
        fixture.reset_to_patch_base().await;
        let repository = DatabaseWorkRepository::new(fixture.pool.clone());
        let binding = repository
            .load_branch_runtime_binding(&fixture.owner, &fixture.work, &fixture.branch)
            .await
            .unwrap();
        let service = DatabaseWorkPatchMaterializationService::new(fixture.pool.clone());
        let operation = service
            .admit(&WorkPatchMaterializationRequest {
                owner_id: fixture.owner.clone(),
                work_id: fixture.work.clone(),
                target_branch_id: fixture.branch.clone(),
                request_id: WorkChangeRef::parse("materialize-real-patch").unwrap(),
                patch_artifact_id: fixture.patch.patch_artifact_id.clone(),
                expected_target_branch_revision: binding.branch_revision,
                expected_target_graph_revision: binding.graph_revision,
                provider_ref: WorkMaterializationProviderRef::parse(
                    SERVER_GIT_WORKTREE_MATERIALIZATION_PROVIDER_REF,
                )
                .unwrap(),
                policy_decision_ref: WorkChangeRef::parse("fixture-approved").unwrap(),
            })
            .await
            .unwrap();

        let invocation = WorkProviderInvocationRef::parse("effect-before-crash").unwrap();
        let operation = service
            .claim_applying(
                &fixture.owner,
                &fixture.work,
                &operation.operation_id,
                "lost-executor",
                &invocation,
            )
            .await
            .unwrap();
        let patch = service
            .load_patch_payload(&fixture.owner, &fixture.work, &operation.operation_id)
            .await
            .unwrap();
        let effect = astra_tools::patch_materialization::materialize_git_patch(
            &fixture.workspace,
            &operation.base_subject_revision,
            &patch,
        )
        .await;
        assert!(matches!(
            effect,
            GitPatchMaterializationOutcome::Applied { .. }
        ));
        sqlx::query("UPDATE work_patch_materialization_operations SET executor_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE operation_id = ?")
            .bind(operation.operation_id.as_str()).execute(fixture.pool.get()).await.unwrap();
        let item = WorkPatchMaterializationRecoveryItem {
            owner_id: fixture.owner.clone(),
            operation: operation.clone(),
        };
        let cancel = CancellationToken::new();
        let before = patch_operation_ownership(
            &fixture.pool,
            "work_patch_materialization_operations",
            operation.operation_id.as_str(),
        )
        .await;
        let head = fixture.git(&["rev-parse", "HEAD"]);
        drive_materialization(
            fixture.pool.clone(),
            &fixture.foreign,
            item.clone(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            patch_operation_ownership(
                &fixture.pool,
                "work_patch_materialization_operations",
                operation.operation_id.as_str()
            )
            .await,
            before
        );
        drive_materialization(fixture.pool.clone(), &fixture.lifecycle, item, &cancel)
            .await
            .unwrap();
        let reconciled = service
            .load(
                &fixture.owner,
                &fixture.work,
                &fixture.branch,
                &operation.operation_id,
            )
            .await
            .unwrap();
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("file.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(reconciled.phase, WorkPatchMaterializationPhase::Verifying);
        assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "1");
        cleanup_work_owner(&fixture.pool, fixture.owner.as_str()).await;
    }
}
