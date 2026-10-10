use super::work_patch_workspace::{WorkspaceResolutionError, resolve_workspace};
use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_services::{
    runs::RunLifecycleService,
    work::{
        DatabaseWorkPatchCommitService, SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF,
        WorkPatchCommitCommitted, WorkPatchCommitError, WorkPatchCommitFailure,
        WorkPatchCommitFailureCode, WorkPatchCommitPhase, WorkPatchCommitProviderRef,
        WorkPatchCommitRecoveryItem, WorkProviderInvocationRef,
    },
};
use astra_tools::patch_materialization::{
    GitReviewedCommitReconciliation, GitReviewedCommitReconciliationReason,
    GitWorktreeCommitMetadata, GitWorktreeCommitNotCreatedCode, GitWorktreeCommitOutcome,
    commit_reviewed_git_patch_with_workspace_lease,
    reconcile_reviewed_git_patch_commit_with_workspace_lease,
};
use astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options;
use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RECOVERY_BATCH: u16 = 16;
const RECOVERY_CONCURRENCY: usize = 4;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn spawn_work_patch_commit_recovery(
    pool: SharedPool,
    lifecycle: Arc<dyn RunLifecycleService>,
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
                    let lifecycle = lifecycle.clone();
                    let cancel = cancel.clone();
                    async move {
                        if let Err(error) =
                            drive_commit(pool, lifecycle.as_ref(), item.clone(), &cancel).await
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
    lifecycle: &dyn RunLifecycleService,
    item: WorkPatchCommitRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchCommitError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    if lifecycle.workspace_executor_id().is_none()
        || item.operation.provider_ref
            != WorkPatchCommitProviderRef::parse(SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF)
                .expect("static provider ref")
    {
        return Ok(());
    }
    let service = DatabaseWorkPatchCommitService::new(pool.clone());
    match item.operation.phase {
        WorkPatchCommitPhase::AwaitingDispatch => {
            drive_awaiting_dispatch(&service, &pool, lifecycle, &item, cancel).await
        }
        WorkPatchCommitPhase::Committing | WorkPatchCommitPhase::Reconciling => {
            drive_reconciliation(&service, &pool, lifecycle, &item, cancel).await
        }
        WorkPatchCommitPhase::Complete => Ok(()),
    }
}

async fn drive_awaiting_dispatch(
    service: &DatabaseWorkPatchCommitService,
    pool: &SharedPool,
    lifecycle: &dyn RunLifecycleService,
    item: &WorkPatchCommitRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchCommitError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let executor_token = format!("server-commit-{}", Uuid::now_v7());
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
        if !cancel.is_cancelled() {
            service
                .defer_recovery(
                    &item.owner_id,
                    &item.operation.work_id,
                    &item.operation.operation_id,
                )
                .await?;
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
        Err(WorkPatchCommitError::Database(error)) => {
            tracing::warn!(
                operation_id = item.operation.operation_id.as_str(),
                %error,
                "Work patch commit payload read will be retried before dispatch"
            );
            if !cancel.is_cancelled() {
                service
                    .defer_recovery(
                        &item.owner_id,
                        &item.operation.work_id,
                        &item.operation.operation_id,
                    )
                    .await?;
            }
            return Ok(());
        }
        Err(error) => {
            if cancel.is_cancelled() {
                return Ok(());
            }
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

    if cancel.is_cancelled() {
        return Ok(());
    }
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
    match commit_reviewed_git_patch_with_workspace_lease(
        &workspace,
        &item.operation.base_subject_revision,
        &item.operation.result_subject_revision,
        &patch,
        &metadata,
        &workspace_lease,
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
            if code != GitWorktreeCommitNotCreatedCode::WorkspaceUnavailable {
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
    }
    Ok(())
}

async fn drive_reconciliation(
    service: &DatabaseWorkPatchCommitService,
    pool: &SharedPool,
    lifecycle: &dyn RunLifecycleService,
    item: &WorkPatchCommitRecoveryItem,
    cancel: &CancellationToken,
) -> Result<(), WorkPatchCommitError> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let invocation = item
        .operation
        .commit_invocation_ref
        .clone()
        .ok_or_else(|| WorkPatchCommitError::NeedsRepair("missing commit invocation".into()))?;
    let executor_token = format!("server-commit-reconciler-{}", Uuid::now_v7());
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
    match reconcile_reviewed_git_patch_commit_with_workspace_lease(
        &workspace,
        &item.operation.base_subject_revision,
        &item.operation.result_subject_revision,
        &patch,
        &workspace_lease,
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
        GitReviewedCommitReconciliation::Diverged {
            observed_revision,
            reason,
        } => match reason {
            GitReviewedCommitReconciliationReason::TargetChanged => {
                if observed_revision.is_some() {
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
            GitReviewedCommitReconciliationReason::InvalidPatch => {
                record_failure(
                    service,
                    item,
                    executor_token,
                    invocation,
                    WorkPatchCommitFailureCode::PatchRejected,
                    observed_revision,
                )
                .await?;
            }
            GitReviewedCommitReconciliationReason::InvalidWorkspace => {
                record_failure(
                    service,
                    item,
                    executor_token,
                    invocation,
                    WorkPatchCommitFailureCode::InvalidWorkspace,
                    observed_revision,
                )
                .await?;
            }
            GitReviewedCommitReconciliationReason::WorkspaceUnavailable
            | GitReviewedCommitReconciliationReason::ProviderUnavailable => {}
        },
    }
    Ok(())
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
        GitWorktreeCommitNotCreatedCode::InvalidWorkspace => {
            WorkPatchCommitFailureCode::InvalidWorkspace
        }
        GitWorktreeCommitNotCreatedCode::WorkspaceUnavailable => {
            WorkPatchCommitFailureCode::WorkspaceUnavailable
        }
        GitWorktreeCommitNotCreatedCode::ProviderUnavailable => {
            WorkPatchCommitFailureCode::ProviderUnavailable
        }
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
    async fn recovery_commits_real_git_only_on_selected_executor() {
        use astra_services::work::*;
        let fixture = PatchRuntimeFixture::new().await;
        let repository = DatabaseWorkRepository::new(fixture.pool.clone());
        let binding = repository
            .load_branch_runtime_binding(&fixture.owner, &fixture.work, &fixture.branch)
            .await
            .unwrap();
        let service = DatabaseWorkPatchCommitService::new(fixture.pool.clone());
        let operation = service
            .admit(&WorkPatchCommitRequest {
                owner_id: fixture.owner.clone(),
                work_id: fixture.work.clone(),
                target_branch_id: fixture.branch.clone(),
                request_id: WorkChangeRef::parse("commit-real-patch").unwrap(),
                patch_artifact_id: fixture.patch.patch_artifact_id.clone(),
                expected_target_branch_revision: binding.branch_revision,
                expected_target_graph_revision: binding.graph_revision,
                message: "Apply reviewed patch".into(),
                author_name: "Astra Test".into(),
                author_email: "astra@example.invalid".into(),
                provider_ref: WorkPatchCommitProviderRef::parse(
                    SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF,
                )
                .unwrap(),
                policy_decision_ref: WorkChangeRef::parse("fixture-approved").unwrap(),
            })
            .await
            .unwrap();
        let item = WorkPatchCommitRecoveryItem {
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
            "work_patch_commit_operations",
            operation.operation_id.as_str(),
        )
        .await;
        {
            let waiting_cancel = CancellationToken::new();
            let attempt = drive_commit(
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
                    "work_patch_commit_operations",
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
                    "work_patch_commit_operations",
                    operation.operation_id.as_str()
                )
                .await,
                before_wait
            );
        }
        drop(physical_lease);

        let head = fixture.git(&["rev-parse", "HEAD"]);
        let ownership = patch_operation_ownership(
            &fixture.pool,
            "work_patch_commit_operations",
            operation.operation_id.as_str(),
        )
        .await;

        let original_record: String = sqlx::query_scalar(
            "SELECT CAST(record_json AS CHAR) FROM workspace_records WHERE owner_id = ?",
        )
        .bind(fixture.owner.as_str())
        .fetch_one(fixture.pool.get())
        .await
        .unwrap();
        let mut missing_owner: serde_json::Value = serde_json::from_str(&original_record).unwrap();
        missing_owner["source"]
            .as_object_mut()
            .unwrap()
            .remove("executor_id");
        sqlx::query("UPDATE workspace_records SET record_json = ? WHERE owner_id = ?")
            .bind(missing_owner.to_string())
            .bind(fixture.owner.as_str())
            .execute(fixture.pool.get())
            .await
            .unwrap();
        drive_commit(
            fixture.pool.clone(),
            &fixture.lifecycle,
            item.clone(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            patch_operation_ownership(
                &fixture.pool,
                "work_patch_commit_operations",
                operation.operation_id.as_str()
            )
            .await,
            ownership
        );
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
        sqlx::query("UPDATE workspace_records SET record_json = ? WHERE owner_id = ?")
            .bind(original_record)
            .bind(fixture.owner.as_str())
            .execute(fixture.pool.get())
            .await
            .unwrap();
        drive_commit(
            fixture.pool.clone(),
            &fixture.foreign,
            item.clone(),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            patch_operation_ownership(
                &fixture.pool,
                "work_patch_commit_operations",
                operation.operation_id.as_str()
            )
            .await,
            ownership
        );
        drive_commit(
            fixture.pool.clone(),
            &fixture.lifecycle,
            item.clone(),
            &cancel,
        )
        .await
        .unwrap();
        let committed = service
            .load(
                &fixture.owner,
                &fixture.work,
                &fixture.branch,
                &operation.operation_id,
            )
            .await
            .unwrap();
        assert_eq!(committed.state, WorkPatchCommitState::Succeeded);
        assert_eq!(committed.phase, WorkPatchCommitPhase::Complete);
        assert_eq!(committed.index_reconciled, Some(true));
        assert_eq!(
            committed.observed_subject_revision,
            Some(
                astra_tools::patch_materialization::observe_git_worktree_revision(
                    &fixture.workspace
                )
                .await
                .unwrap()
            )
        );
        assert_eq!(
            committed.commit_sha.as_deref(),
            Some(fixture.git(&["rev-parse", "HEAD"]).as_str())
        );
        assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "2");
        assert_eq!(fixture.git(&["show", "HEAD:file.txt"]), "after");
        assert!(fixture.git(&["diff", "--cached", "--name-only"]).is_empty());
        assert!(matches!(
            drive_commit(fixture.pool.clone(), &fixture.lifecycle, item, &cancel).await,
            Err(WorkPatchCommitError::ExecutorConflict)
        ));
        assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "2");
        cleanup_work_owner(&fixture.pool, fixture.owner.as_str()).await;
    }
    #[tokio::test]
    #[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
    async fn expired_invocation_reconciles_real_effect_without_repeating_mutation() {
        use astra_services::work::*;
        let fixture = PatchRuntimeFixture::new().await;
        let repository = DatabaseWorkRepository::new(fixture.pool.clone());
        let binding = repository
            .load_branch_runtime_binding(&fixture.owner, &fixture.work, &fixture.branch)
            .await
            .unwrap();
        let service = DatabaseWorkPatchCommitService::new(fixture.pool.clone());
        let operation = service
            .admit(&WorkPatchCommitRequest {
                owner_id: fixture.owner.clone(),
                work_id: fixture.work.clone(),
                target_branch_id: fixture.branch.clone(),
                request_id: WorkChangeRef::parse("commit-real-patch").unwrap(),
                patch_artifact_id: fixture.patch.patch_artifact_id.clone(),
                expected_target_branch_revision: binding.branch_revision,
                expected_target_graph_revision: binding.graph_revision,
                message: "Apply reviewed patch".into(),
                author_name: "Astra Test".into(),
                author_email: "astra@example.invalid".into(),
                provider_ref: WorkPatchCommitProviderRef::parse(
                    SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF,
                )
                .unwrap(),
                policy_decision_ref: WorkChangeRef::parse("fixture-approved").unwrap(),
            })
            .await
            .unwrap();

        let invocation = WorkProviderInvocationRef::parse("effect-before-crash").unwrap();
        let operation = service
            .claim_committing(
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
        let effect = astra_tools::patch_materialization::commit_reviewed_git_patch(
            &fixture.workspace,
            &operation.base_subject_revision,
            &operation.result_subject_revision,
            &patch,
            &GitWorktreeCommitMetadata {
                message: operation.message.clone(),
                author_name: operation.author_name.clone(),
                author_email: operation.author_email.clone(),
            },
        )
        .await;
        assert!(matches!(effect, GitWorktreeCommitOutcome::Committed { .. }));
        sqlx::query("UPDATE work_patch_commit_operations SET executor_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE operation_id = ?")
            .bind(operation.operation_id.as_str()).execute(fixture.pool.get()).await.unwrap();
        let item = WorkPatchCommitRecoveryItem {
            owner_id: fixture.owner.clone(),
            operation: operation.clone(),
        };
        let cancel = CancellationToken::new();
        let before = patch_operation_ownership(
            &fixture.pool,
            "work_patch_commit_operations",
            operation.operation_id.as_str(),
        )
        .await;
        let head = fixture.git(&["rev-parse", "HEAD"]);
        drive_commit(
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
                "work_patch_commit_operations",
                operation.operation_id.as_str()
            )
            .await,
            before
        );
        drive_commit(fixture.pool.clone(), &fixture.lifecycle, item, &cancel)
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
        assert_eq!(reconciled.phase, WorkPatchCommitPhase::Complete);
        assert_eq!(reconciled.state, WorkPatchCommitState::Succeeded);
        assert_eq!(reconciled.commit_sha.as_deref(), Some(head.as_str()));
        assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), "2");
        cleanup_work_owner(&fixture.pool, fixture.owner.as_str()).await;
    }
}
