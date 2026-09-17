mod common;

use astra_services::work::{
    CheckCoverage, CheckEvidenceRef, CheckOutcome, CheckRunId, CheckVerifierKind, CriterionCommand,
    CriterionDefinition, CriterionId, CriterionRevision, CriterionRevisionRef,
    CriterionSetMemberChange, CriterionSetRevision, DatabaseWorkPatchCommitService,
    DatabaseWorkPatchMaterializationService, DatabaseWorkRepository, GraphRevision,
    NewWorkCheckRun, NewWorkCriterion, NewWorkPatchArtifact, WorkBranchBasisChange,
    WorkBranchSubjectChange, WorkBranchSubjectRevision, WorkChangeRef, WorkContentHash,
    WorkCriteriaChange, WorkItemAttemptId, WorkItemId, WorkItemRevision, WorkItemRevisionRef,
    WorkMaterializationProviderRef, WorkPatchArtifactBasisResource, WorkPatchArtifactId,
    WorkPatchCommitCommitted, WorkPatchCommitConflict, WorkPatchCommitError, WorkPatchCommitPhase,
    WorkPatchCommitProviderRef, WorkPatchCommitRequest, WorkPatchCommitState, WorkPatchFormat,
    WorkPatchMaterializationApplied, WorkPatchMaterializationApplyOutcome,
    WorkPatchMaterializationConflict, WorkPatchMaterializationError,
    WorkPatchMaterializationFailureCode, WorkPatchMaterializationNotApplied,
    WorkPatchMaterializationPageLimit, WorkPatchMaterializationPhase,
    WorkPatchMaterializationQuery, WorkPatchMaterializationRequest, WorkPatchMaterializationState,
    WorkPatchMaterializationVerificationOutcome, WorkProviderInvocationRef, WorkRepository,
    WorkRepositoryError, WorkRevision, WorkSubjectRef,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn hash(label: &str) -> WorkContentHash {
    WorkContentHash::parse(format!("sha256:{:x}", Sha256::digest(label.as_bytes())))
        .expect("canonical test hash")
}

fn patch_content(data: &str, declared_digest: Option<&str>) -> serde_json::Value {
    json!({
        "kind": "patch",
        "content_type": "text/x-diff",
        "encoding": "utf-8",
        "data": data,
        "byte_size": data.len(),
        "sha256": declared_digest
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{:x}", Sha256::digest(data.as_bytes()))),
    })
}

async fn insert_patch_payload(
    pool: &astra_core::SharedPool,
    owner_id: &str,
    session_id: &str,
    artifact_id: &str,
    content: serde_json::Value,
) {
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, source, content_json)
         VALUES (?, ?, ?, 'patch', 'work_patch_export', ?)",
    )
    .bind(artifact_id)
    .bind(session_id)
    .bind(owner_id)
    .bind(content.to_string())
    .execute(pool.get())
    .await
    .expect("insert patch payload fixture");
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("agent_runs", "user_id"),
        ("session_artifact_references", "user_id"),
        ("session_artifacts", "user_id"),
        ("work_patch_artifacts", "owner_id"),
        ("work_patch_materialization_operations", "owner_id"),
        ("work_patch_commit_operations", "owner_id"),
        ("work_check_runs", "owner_id"),
        ("work_events", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_branch_subjects", "owner_id"),
        ("work_branches", "owner_id"),
        ("work_item_edges", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_criterion_revisions", "owner_id"),
        ("work_criteria", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE {owner_column} = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn patch_export_binds_verified_payload_once_and_rejects_stale_or_tampered_output() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner = id("owner");
    let work = id("work");
    let branch = id("branch");
    let session = id("session");
    let criterion_id = id("criterion");
    let subject_ref = WorkSubjectRef::parse(id("workspace")).expect("subject ref");
    let base_revision = hash("base");
    let result_revision = hash("result");
    cleanup_owner(&pool, &owner).await;

    let created = repository
        .create_genesis(common::work_genesis(
            &owner,
            &work,
            &branch,
            &session,
            &id("intent"),
            "Export an exact patch for review.",
        ))
        .await
        .expect("create Work");
    let subject = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: created.work.parts().owner_id.clone(),
            work_id: created.work.parts().work_id.clone(),
            branch_id: created.delivery_branch.parts().branch_id.clone(),
            expected_branch_revision: created.delivery_branch.parts().branch_revision,
            graph_revision: GraphRevision::INITIAL,
            subject_ref: subject_ref.clone(),
            subject_revision: result_revision.clone(),
            source_ref: WorkChangeRef::parse(id("subject-event")).expect("subject source"),
        })
        .await
        .expect("record current subject");
    let payload_id = id("payload");
    let patch = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n";
    insert_patch_payload(
        &pool,
        &owner,
        &session,
        &payload_id,
        patch_content(patch, None),
    )
    .await;
    let request = NewWorkPatchArtifact {
        owner_id: created.work.parts().owner_id.clone(),
        work_id: created.work.parts().work_id.clone(),
        branch_id: created.delivery_branch.parts().branch_id.clone(),
        patch_artifact_id: WorkPatchArtifactId::parse(id("patch")).expect("patch id"),
        payload_artifact_id: payload_id.clone(),
        expected_branch_revision: subject.branch_revision,
        expected_graph_revision: subject.graph_revision,
        expected_subject_record_revision: subject.subject_record_revision,
        subject_ref: subject_ref.clone(),
        base_subject_revision: base_revision.clone(),
        result_subject_revision: result_revision.clone(),
        format: WorkPatchFormat::UnifiedDiffV1,
        provider_invocation_ref: WorkProviderInvocationRef::parse(id("invocation"))
            .expect("invocation ref"),
        source_ref: WorkChangeRef::parse(id("export-event")).expect("export source"),
    };
    sqlx::query(
        "INSERT INTO session_artifact_references
         (user_id, session_id, artifact_id, reference_kind, reference_id)
         VALUES (?, ?, ?, 'state_item', ?)",
    )
    .bind(&owner)
    .bind(&session)
    .bind(&payload_id)
    .bind(request.patch_artifact_id.as_str())
    .execute(pool.get())
    .await
    .expect("provider retains payload before Work admission");
    let recorded = repository
        .record_patch_artifact(request.clone())
        .await
        .expect("record verified patch");
    assert_eq!(recorded.payload_bytes, patch.len() as u64);
    assert_eq!(recorded.result_subject_revision, result_revision);
    assert_eq!(
        repository
            .record_patch_artifact(request.clone())
            .await
            .expect("idempotent patch replay"),
        recorded
    );
    let export_events: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'patch_artifact_exported'",
    )
    .bind(&owner)
    .bind(&work)
    .fetch_one(pool.get())
    .await
    .expect("count export events")
    .try_get("count")
    .expect("count column");
    assert_eq!(
        export_events, 1,
        "replay must not duplicate the audit event"
    );
    let durable_refs: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM session_artifact_references
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&owner)
    .bind(&session)
    .bind(&payload_id)
    .fetch_one(pool.get())
    .await
    .expect("count durable patch references")
    .try_get("count")
    .expect("count column");
    assert_eq!(durable_refs, 1);

    let commit_service = DatabaseWorkPatchCommitService::new(pool.clone());
    let commit_request = WorkPatchCommitRequest {
        owner_id: request.owner_id.clone(),
        work_id: request.work_id.clone(),
        request_id: WorkChangeRef::parse(id("commit-request")).expect("commit request"),
        target_branch_id: request.branch_id.clone(),
        patch_artifact_id: request.patch_artifact_id.clone(),
        expected_target_branch_revision: subject.branch_revision,
        expected_target_graph_revision: subject.graph_revision,
        message: "Commit the exact reviewed result".into(),
        author_name: "Test Developer".into(),
        author_email: "developer@example.test".into(),
        provider_ref: WorkPatchCommitProviderRef::parse(
            astra_services::work::SERVER_GIT_WORKTREE_COMMIT_PROVIDER_REF,
        )
        .expect("commit provider"),
        policy_decision_ref: WorkChangeRef::parse(id("commit-policy-decision"))
            .expect("commit policy decision"),
    };
    let commit_operation = commit_service
        .admit(&commit_request)
        .await
        .expect("admit exact reviewed commit");
    assert_eq!(commit_operation.state, WorkPatchCommitState::Pending);
    assert_eq!(commit_operation.result_subject_revision, result_revision);
    assert_eq!(
        commit_service
            .admit(&commit_request)
            .await
            .expect("idempotent commit admission"),
        commit_operation
    );
    assert_eq!(
        commit_service
            .load(
                &commit_request.owner_id,
                &commit_request.work_id,
                &commit_request.target_branch_id,
                &commit_operation.operation_id,
            )
            .await
            .expect("load exact commit operation"),
        commit_operation
    );
    let public_commit = serde_json::to_value(&commit_operation).expect("public commit operation");
    assert!(public_commit.get("subject_ref").is_none());
    assert!(
        public_commit
            .get("target_subject_record_revision")
            .is_none()
    );
    assert!(public_commit.get("author_name").is_none());
    assert!(public_commit.get("author_email").is_none());
    let other_owner =
        astra_services::work::WorkOwnerId::parse(id("other-owner")).expect("other owner");
    assert!(matches!(
        commit_service
            .load(
                &other_owner,
                &commit_request.work_id,
                &commit_request.target_branch_id,
                &commit_operation.operation_id,
            )
            .await,
        Err(WorkPatchCommitError::NotFound)
    ));
    let mut conflicting_commit = commit_request.clone();
    conflicting_commit.message = "A different commit".into();
    assert!(matches!(
        commit_service.admit(&conflicting_commit).await,
        Err(WorkPatchCommitError::Conflict(
            WorkPatchCommitConflict::RequestIdentity
        ))
    ));
    let commit_invocation =
        WorkProviderInvocationRef::parse(id("commit-invocation")).expect("commit invocation");
    let commit_executor = id("commit-executor");
    let claimed_commit = commit_service
        .claim_committing(
            &commit_request.owner_id,
            &commit_request.work_id,
            &commit_operation.operation_id,
            &commit_executor,
            &commit_invocation,
        )
        .await
        .expect("claim commit executor");
    assert_eq!(claimed_commit.phase, WorkPatchCommitPhase::Committing);
    assert_eq!(
        claimed_commit.commit_invocation_ref.as_ref(),
        Some(&commit_invocation)
    );
    assert!(matches!(
        commit_service
            .claim_committing(
                &commit_request.owner_id,
                &commit_request.work_id,
                &commit_operation.operation_id,
                &id("other-executor"),
                &commit_invocation,
            )
            .await,
        Err(WorkPatchCommitError::ExecutorConflict)
    ));
    assert_eq!(
        commit_service
            .load_patch_payload(
                &commit_request.owner_id,
                &commit_request.work_id,
                &commit_operation.operation_id,
            )
            .await
            .expect("load exact commit payload"),
        patch.as_bytes()
    );
    sqlx::query(
        "UPDATE work_patch_commit_operations
         SET executor_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
         WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
    )
    .bind(commit_request.owner_id.as_str())
    .bind(commit_request.work_id.as_str())
    .bind(commit_operation.operation_id.as_str())
    .execute(pool.get())
    .await
    .expect("expire crashed commit executor lease");
    let recovery_end = commit_service
        .recovery_cycle_upper_bound()
        .await
        .expect("load commit recovery cycle")
        .expect("expired commit is recoverable");
    let recovery_items = commit_service
        .list_pending_for_recovery(16, None, &recovery_end)
        .await
        .expect("scan bounded commit recovery cycle");
    assert!(recovery_items.iter().any(|item| {
        item.owner_id == commit_request.owner_id
            && item.operation.operation_id == commit_operation.operation_id
    }));
    let reconciliation_executor = id("commit-reconciler");
    let reconciling_commit = commit_service
        .claim_reconciliation(
            &commit_request.owner_id,
            &commit_request.work_id,
            &commit_operation.operation_id,
            &reconciliation_executor,
            &commit_invocation,
        )
        .await
        .expect("claim exact crashed commit reconciliation");
    assert_eq!(reconciling_commit.phase, WorkPatchCommitPhase::Reconciling);
    let committed_subject_revision = hash("clean committed subject");
    let expected_commit_sha = "a".repeat(40);
    let committed_report = WorkPatchCommitCommitted {
        owner_id: commit_request.owner_id.clone(),
        work_id: commit_request.work_id.clone(),
        operation_id: commit_operation.operation_id.clone(),
        executor_token: reconciliation_executor,
        provider_invocation_ref: commit_invocation,
        commit_sha: expected_commit_sha.clone(),
        observed_subject_revision: committed_subject_revision.clone(),
        index_reconciled: true,
    };
    let mut stale_commit = commit_request.clone();
    stale_commit.request_id =
        WorkChangeRef::parse(id("stale-commit-request")).expect("stale commit request");
    stale_commit.expected_target_branch_revision = stale_commit
        .expected_target_branch_revision
        .checked_next()
        .expect("different target branch revision");
    assert!(matches!(
        commit_service.admit(&stale_commit).await,
        Err(WorkPatchCommitError::Conflict(
            WorkPatchCommitConflict::TargetOperation
        ))
    ));
    let commit_rows: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_patch_commit_operations
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner)
    .bind(&work)
    .fetch_one(pool.get())
    .await
    .expect("count patch commit operations")
    .try_get("count")
    .expect("count column");
    assert_eq!(commit_rows, 1, "conflicts must not create operations");

    let target_branch = id("branch");
    let target_session = id("session");
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, title, status, event_count, metadata)
         VALUES (?, ?, 'patch target', 'active', 0, '{}')",
    )
    .bind(&target_session)
    .bind(&owner)
    .execute(pool.get())
    .await
    .expect("insert target session");
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id, origin_branch_id,
          fork_cursor, goal_revision_ref, criteria_set_revision_ref, basis_graph_revision,
          current_graph_revision, created_at, updated_at)
         SELECT owner_id, work_id, ?, 1, ?, branch_id, ?, goal_revision_ref,
                criteria_set_revision_ref, basis_graph_revision, current_graph_revision,
                NOW(6), NOW(6)
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&target_branch)
    .bind(&target_session)
    .bind(id("fork-cursor"))
    .bind(&owner)
    .bind(&work)
    .bind(&branch)
    .execute(pool.get())
    .await
    .expect("insert target branch");
    let initial_target_subject = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: request.owner_id.clone(),
            work_id: request.work_id.clone(),
            branch_id: astra_services::work::WorkBranchId::parse(&target_branch)
                .expect("target branch"),
            expected_branch_revision: astra_services::work::WorkBranchRevision::INITIAL,
            graph_revision: GraphRevision::INITIAL,
            subject_ref: subject_ref.clone(),
            subject_revision: base_revision.clone(),
            source_ref: WorkChangeRef::parse(id("target-subject-event"))
                .expect("target subject source"),
        })
        .await
        .expect("record target base subject");
    let materialization = DatabaseWorkPatchMaterializationService::new(pool.clone());
    let unverified_target_request = WorkPatchMaterializationRequest {
        owner_id: request.owner_id.clone(),
        work_id: request.work_id.clone(),
        request_id: WorkChangeRef::parse(id("materialize-without-criteria"))
            .expect("unverified request"),
        patch_artifact_id: request.patch_artifact_id.clone(),
        target_branch_id: initial_target_subject.branch_id.clone(),
        expected_target_branch_revision: initial_target_subject.branch_revision,
        expected_target_graph_revision: initial_target_subject.graph_revision,
        provider_ref: WorkMaterializationProviderRef::parse("edge://device-1/workspace")
            .expect("provider"),
        policy_decision_ref: WorkChangeRef::parse(id("policy-decision")).expect("policy"),
    };
    assert!(matches!(
        materialization.admit(&unverified_target_request).await,
        Err(WorkPatchMaterializationError::VerificationRequired)
    ));
    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: request.owner_id.clone(),
            work_id: request.work_id.clone(),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: vec![CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: astra_services::work::CriterionStatement::parse(
                        "The materialized result passes its exact registered verifier.",
                    )
                    .expect("criterion statement"),
                    command: CriterionCommand::parse("verify-materialized-result")
                        .expect("criterion command"),
                },
            })],
            source_ref: WorkChangeRef::parse(id("criteria-source")).expect("criteria source"),
            reason: None,
        })
        .await
        .expect("accept verification criterion");
    let adopted_target = repository
        .adopt_branch_basis(WorkBranchBasisChange {
            owner_id: request.owner_id.clone(),
            work_id: request.work_id.clone(),
            branch_id: initial_target_subject.branch_id.clone(),
            expected_work_revision: WorkRevision::new(2).expect("work r2"),
            expected_branch_revision: initial_target_subject.branch_revision,
            expected_goal_revision: astra_services::work::GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            target_goal_revision: astra_services::work::GoalRevision::INITIAL,
            target_criteria_set_revision: CriterionSetRevision::new(2).expect("criteria r2"),
            source_ref: WorkChangeRef::parse(id("target-basis-source")).expect("basis source"),
        })
        .await
        .expect("adopt verification criterion on target");
    let target_subject = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: request.owner_id.clone(),
            work_id: request.work_id.clone(),
            branch_id: initial_target_subject.branch_id,
            expected_branch_revision: adopted_target.parts().branch_revision,
            graph_revision: GraphRevision::INITIAL,
            subject_ref: subject_ref.clone(),
            subject_revision: base_revision.clone(),
            source_ref: WorkChangeRef::parse(id("aligned-target-subject"))
                .expect("aligned subject source"),
        })
        .await
        .expect("align target subject with adopted criteria");
    let materialization_request = WorkPatchMaterializationRequest {
        owner_id: request.owner_id.clone(),
        work_id: request.work_id.clone(),
        request_id: WorkChangeRef::parse(id("materialize-request"))
            .expect("materialization request"),
        patch_artifact_id: request.patch_artifact_id.clone(),
        target_branch_id: target_subject.branch_id.clone(),
        expected_target_branch_revision: target_subject.branch_revision,
        expected_target_graph_revision: target_subject.graph_revision,
        provider_ref: WorkMaterializationProviderRef::parse("edge://device-1/workspace")
            .expect("provider"),
        policy_decision_ref: WorkChangeRef::parse(id("policy-decision")).expect("policy"),
    };
    let admitted = materialization
        .admit(&materialization_request)
        .await
        .expect("admit exact-base materialization");
    assert_eq!(admitted.state, WorkPatchMaterializationState::Pending);
    assert_eq!(
        admitted.phase,
        WorkPatchMaterializationPhase::AwaitingDispatch
    );
    assert_eq!(admitted.base_subject_revision, base_revision);
    assert_eq!(admitted.result_subject_revision, result_revision);
    assert_eq!(
        materialization
            .load_patch_payload(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &admitted.operation_id,
            )
            .await
            .expect("reload and verify admitted patch bytes"),
        patch.as_bytes()
    );
    let recovery_cycle_end = materialization
        .recovery_cycle_upper_bound()
        .await
        .expect("freeze bounded recovery cycle")
        .expect("pending operation creates a recovery cycle");
    assert!(
        materialization
            .list_pending_for_recovery(64, None, &recovery_cycle_end)
            .await
            .expect("bounded recovery scan")
            .iter()
            .any(|item| {
                item.owner_id == materialization_request.owner_id
                    && item.operation.operation_id == admitted.operation_id
            }),
        "a durable pending operation must be discoverable after process restart"
    );
    assert!(
        materialization
            .list_pending_for_recovery(64, Some(&admitted.operation_id), &recovery_cycle_end,)
            .await
            .expect("keyset recovery scan")
            .iter()
            .all(|item| item.operation.operation_id != admitted.operation_id),
        "keyset recovery must advance instead of pinning one verifying operation forever"
    );
    materialization
        .defer_recovery(
            &materialization_request.owner_id,
            &materialization_request.work_id,
            &admitted.operation_id,
        )
        .await
        .expect("durably defer a transient recovery attempt");
    assert!(
        materialization
            .list_pending_for_recovery(64, None, &recovery_cycle_end)
            .await
            .expect("scan during durable retry floor")
            .iter()
            .all(|item| item.operation.operation_id != admitted.operation_id),
        "a deferred operation must not create a tight multi-pod polling loop"
    );
    sqlx::query(
        "UPDATE work_patch_materialization_operations
         SET recovery_after = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
         WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
    )
    .bind(materialization_request.owner_id.as_str())
    .bind(materialization_request.work_id.as_str())
    .bind(admitted.operation_id.as_str())
    .execute(pool.get())
    .await
    .expect("advance deferred recovery fixture");
    assert!(
        materialization
            .list_pending_for_recovery(64, None, &recovery_cycle_end)
            .await
            .expect("scan after durable retry floor")
            .iter()
            .any(|item| item.operation.operation_id == admitted.operation_id),
        "recovery must resume after its durable retry floor"
    );
    let mut overlapping_request = materialization_request.clone();
    overlapping_request.request_id =
        WorkChangeRef::parse(id("materialize-overlap")).expect("overlap request");
    assert!(matches!(
        materialization.admit(&overlapping_request).await,
        Err(WorkPatchMaterializationError::Conflict(
            WorkPatchMaterializationConflict::TargetOperation
        ))
    ));
    assert_eq!(
        materialization
            .load(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &materialization_request.target_branch_id,
                &admitted.operation_id,
            )
            .await
            .expect("load exact materialization operation"),
        admitted
    );
    assert!(matches!(
        materialization
            .load(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &request.branch_id,
                &admitted.operation_id,
            )
            .await,
        Err(WorkPatchMaterializationError::NotFound)
    ));
    assert_eq!(
        materialization
            .admit(&materialization_request)
            .await
            .expect("idempotent materialization replay"),
        admitted
    );
    let apply_invocation =
        WorkProviderInvocationRef::parse(id("apply-invocation")).expect("apply invocation");
    let applying = materialization
        .claim_applying(
            &materialization_request.owner_id,
            &materialization_request.work_id,
            &admitted.operation_id,
            "executor-a",
            &apply_invocation,
        )
        .await
        .expect("claim materialization executor");
    assert_eq!(applying.phase, WorkPatchMaterializationPhase::Applying);
    assert!(
        materialization
            .list_pending_for_recovery(64, None, &recovery_cycle_end)
            .await
            .expect("scan while executor lease is active")
            .iter()
            .all(|item| item.operation.operation_id != admitted.operation_id),
        "an active executor lease must not create cross-pod recovery churn"
    );
    assert_eq!(
        materialization
            .claim_applying(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &admitted.operation_id,
                "executor-a",
                &apply_invocation,
            )
            .await
            .expect("renew same executor"),
        applying
    );
    assert!(matches!(
        materialization
            .claim_applying(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &admitted.operation_id,
                "executor-b",
                &apply_invocation,
            )
            .await,
        Err(WorkPatchMaterializationError::ExecutorConflict)
    ));
    sqlx::query(
        "UPDATE work_patch_materialization_operations
         SET executor_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
         WHERE owner_id = ? AND work_id = ? AND operation_id = ?",
    )
    .bind(materialization_request.owner_id.as_str())
    .bind(materialization_request.work_id.as_str())
    .bind(admitted.operation_id.as_str())
    .execute(pool.get())
    .await
    .expect("expire executor lease fixture");
    assert!(
        materialization
            .list_pending_for_recovery(64, None, &recovery_cycle_end)
            .await
            .expect("scan after executor lease expires")
            .iter()
            .any(|item| item.operation.operation_id == admitted.operation_id),
        "an expired executor lease must become recoverable without process-local state"
    );
    assert!(matches!(
        materialization
            .claim_applying(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &admitted.operation_id,
                "executor-b",
                &apply_invocation,
            )
            .await,
        Err(WorkPatchMaterializationError::ExecutorConflict)
    ));
    let reconciling = materialization
        .claim_reconciliation(
            &materialization_request.owner_id,
            &materialization_request.work_id,
            &admitted.operation_id,
            "executor-b",
            &apply_invocation,
        )
        .await
        .expect("claim observation-only reconciliation");
    assert_eq!(
        reconciling.phase,
        WorkPatchMaterializationPhase::Reconciling
    );
    let apply_report = WorkPatchMaterializationApplied {
        owner_id: materialization_request.owner_id.clone(),
        work_id: materialization_request.work_id.clone(),
        operation_id: admitted.operation_id.clone(),
        executor_token: "executor-b".into(),
        provider_invocation_ref: apply_invocation,
        observed_subject_revision: result_revision.clone(),
    };
    let applied = materialization
        .record_applied(&apply_report)
        .await
        .expect("commit exact materialization result");
    assert_eq!(applied.state, WorkPatchMaterializationState::Pending);
    assert_eq!(applied.phase, WorkPatchMaterializationPhase::Verifying);
    assert_eq!(
        applied.apply_outcome,
        Some(WorkPatchMaterializationApplyOutcome::Applied)
    );
    assert_eq!(
        materialization
            .record_applied(&apply_report)
            .await
            .expect("idempotent apply report"),
        applied
    );
    let materialized_subject = repository
        .load_branch_subject(
            &materialization_request.owner_id,
            &materialization_request.work_id,
            &materialization_request.target_branch_id,
        )
        .await
        .expect("load materialized target")
        .expect("materialized target subject");
    assert_eq!(materialized_subject.subject_revision, result_revision);
    assert_eq!(
        materialized_subject.branch_revision,
        materialization_request
            .expected_target_branch_revision
            .checked_next()
            .expect("next target branch revision")
    );
    assert_eq!(
        materialized_subject.subject_record_revision,
        target_subject
            .subject_record_revision
            .checked_next()
            .expect("next target subject revision")
    );
    assert!(matches!(
        materialization
            .complete_verification(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &admitted.operation_id,
            )
            .await,
        Err(WorkPatchMaterializationError::VerificationRequired)
    ));
    let verification_run_id = id("verification-run");
    let verification_attempt =
        WorkItemAttemptId::parse(id("verification-attempt")).expect("verification attempt");
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status,
          work_id, work_branch_id, work_graph_revision,
          work_item_id, work_item_revision, work_item_attempt_id)
         VALUES (?, ?, ?, ?, ?, 0, 'completed', ?, ?, 1, 'root', 1, ?)",
    )
    .bind(&verification_run_id)
    .bind(&owner)
    .bind(&target_session)
    .bind(&verification_run_id)
    .bind(&verification_run_id)
    .bind(&work)
    .bind(&target_branch)
    .bind(verification_attempt.as_str())
    .execute(pool.get())
    .await
    .expect("persist exact verification run binding");
    repository
        .record_check_run(NewWorkCheckRun {
            owner_id: materialization_request.owner_id.clone(),
            work_id: materialization_request.work_id.clone(),
            branch_id: materialization_request.target_branch_id.clone(),
            check_run_id: CheckRunId::parse(id("materialization-check")).expect("check id"),
            graph_revision: materialization_request.expected_target_graph_revision,
            item: WorkItemRevisionRef {
                item_id: WorkItemId::root(),
                revision: WorkItemRevision::INITIAL,
            },
            attempt_id: verification_attempt,
            criterion_set_revision: CriterionSetRevision::new(2).expect("criteria r2"),
            criterion: CriterionRevisionRef {
                criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
                revision: CriterionRevision::INITIAL,
            },
            subject_ref: target_subject.subject_ref.clone(),
            subject_revision: result_revision.clone(),
            artifact_digest: Some(applied.payload_hash.clone()),
            run_ref: WorkChangeRef::parse(&verification_run_id).expect("verification run ref"),
            invocation_ref: WorkChangeRef::parse(id("verification-invocation"))
                .expect("verification invocation"),
            verifier_kind: CheckVerifierKind::Test,
            verifier_fingerprint: hash("verification-command"),
            environment_fingerprint: hash("verification-environment"),
            outcome: CheckOutcome::Passed,
            error_kind: None,
            coverage: CheckCoverage::Complete,
            coverage_gaps: Vec::new(),
            evidence_refs: vec![
                CheckEvidenceRef::parse("urn:astra:trace:cloud:materialization-check/invocation")
                    .expect("verification evidence"),
            ],
            source_cursor: WorkChangeRef::parse(id("verification-cursor"))
                .expect("verification cursor"),
            produced_at: "2026-08-02T00:00:00Z".parse().expect("verification time"),
            expires_at: None,
        })
        .await
        .expect("record fresh materialization verification");
    let verified = materialization
        .complete_verification(
            &materialization_request.owner_id,
            &materialization_request.work_id,
            &admitted.operation_id,
        )
        .await
        .expect("complete evidence-backed materialization");
    assert_eq!(verified.state, WorkPatchMaterializationState::Succeeded);
    assert_eq!(verified.phase, WorkPatchMaterializationPhase::Complete);
    assert_eq!(
        verified.verification_outcome,
        Some(WorkPatchMaterializationVerificationOutcome::Passed)
    );
    assert!(verified.verification_evidence_hash.is_some());
    assert_eq!(
        materialization
            .complete_verification(
                &materialization_request.owner_id,
                &materialization_request.work_id,
                &admitted.operation_id,
            )
            .await
            .expect("idempotent verification completion"),
        verified
    );
    let mut conflicting_report = apply_report.clone();
    conflicting_report.provider_invocation_ref =
        WorkProviderInvocationRef::parse(id("apply-invocation")).expect("different invocation");
    assert!(matches!(
        materialization.record_applied(&conflicting_report).await,
        Err(WorkPatchMaterializationError::Conflict(
            WorkPatchMaterializationConflict::ApplyReportIdentity
        ))
    ));
    let mut conflicting_replay = materialization_request.clone();
    conflicting_replay.provider_ref =
        WorkMaterializationProviderRef::parse("edge://device-2").expect("other provider");
    assert!(matches!(
        materialization.admit(&conflicting_replay).await,
        Err(WorkPatchMaterializationError::Conflict(
            WorkPatchMaterializationConflict::RequestIdentity
        ))
    ));

    let different_base = hash("different-current-base");
    let advanced_target = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: materialization_request.owner_id.clone(),
            work_id: materialization_request.work_id.clone(),
            branch_id: materialization_request.target_branch_id.clone(),
            expected_branch_revision: materialized_subject.branch_revision,
            graph_revision: materialization_request.expected_target_graph_revision,
            subject_ref: target_subject.subject_ref.clone(),
            subject_revision: different_base,
            source_ref: WorkChangeRef::parse(id("target-subject-event"))
                .expect("target subject source"),
        })
        .await
        .expect("advance target to a different exact base");
    let mut wrong_base = materialization_request.clone();
    wrong_base.request_id =
        WorkChangeRef::parse(id("materialize-request")).expect("new materialization request");
    wrong_base.expected_target_branch_revision = advanced_target.branch_revision;
    wrong_base.expected_target_graph_revision = advanced_target.graph_revision;
    assert!(matches!(
        materialization.admit(&wrong_base).await,
        Err(WorkPatchMaterializationError::Conflict(
            WorkPatchMaterializationConflict::TargetBase
        ))
    ));
    let operation_rows: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_patch_materialization_operations
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner)
    .bind(&work)
    .fetch_one(pool.get())
    .await
    .expect("count materialization operations")
    .try_get("count")
    .expect("count column");
    assert_eq!(operation_rows, 1, "conflicts must not create operations");

    let restored_base = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: materialization_request.owner_id.clone(),
            work_id: materialization_request.work_id.clone(),
            branch_id: materialization_request.target_branch_id.clone(),
            expected_branch_revision: advanced_target.branch_revision,
            graph_revision: advanced_target.graph_revision,
            subject_ref: target_subject.subject_ref.clone(),
            subject_revision: base_revision.clone(),
            source_ref: WorkChangeRef::parse(id("restore-target-base"))
                .expect("restore base source"),
        })
        .await
        .expect("restore an exact patch base");
    let mut abort_request = materialization_request.clone();
    abort_request.request_id =
        WorkChangeRef::parse(id("materialize-abort")).expect("abort request");
    abort_request.expected_target_branch_revision = restored_base.branch_revision;
    abort_request.expected_target_graph_revision = restored_base.graph_revision;
    let abort_operation = materialization
        .admit(&abort_request)
        .await
        .expect("admit abort proof operation");
    let aborted = materialization
        .abort(
            &abort_request.owner_id,
            &abort_request.work_id,
            &abort_request.target_branch_id,
            &abort_operation.operation_id,
        )
        .await
        .expect("abort before dispatch");
    assert_eq!(aborted.state, WorkPatchMaterializationState::Aborted);
    assert_eq!(aborted.phase, WorkPatchMaterializationPhase::Complete);
    assert_eq!(
        materialization
            .abort(
                &abort_request.owner_id,
                &abort_request.work_id,
                &abort_request.target_branch_id,
                &abort_operation.operation_id,
            )
            .await
            .expect("idempotent abort"),
        aborted
    );

    let mut no_effect_request = materialization_request.clone();
    no_effect_request.request_id =
        WorkChangeRef::parse(id("materialize-no-effect")).expect("no-effect request");
    no_effect_request.expected_target_branch_revision = restored_base.branch_revision;
    no_effect_request.expected_target_graph_revision = restored_base.graph_revision;
    let no_effect_operation = materialization
        .admit(&no_effect_request)
        .await
        .expect("admit no-effect proof operation");
    let no_effect_invocation =
        WorkProviderInvocationRef::parse(id("no-effect-invocation")).expect("no-effect invocation");
    materialization
        .claim_applying(
            &no_effect_request.owner_id,
            &no_effect_request.work_id,
            &no_effect_operation.operation_id,
            "no-effect-executor",
            &no_effect_invocation,
        )
        .await
        .expect("claim no-effect proof operation");
    assert!(matches!(
        materialization
            .abort(
                &no_effect_request.owner_id,
                &no_effect_request.work_id,
                &no_effect_request.target_branch_id,
                &no_effect_operation.operation_id,
            )
            .await,
        Err(WorkPatchMaterializationError::InvalidTransition)
    ));
    let not_applied_report = WorkPatchMaterializationNotApplied {
        owner_id: no_effect_request.owner_id.clone(),
        work_id: no_effect_request.work_id.clone(),
        operation_id: no_effect_operation.operation_id,
        executor_token: "no-effect-executor".into(),
        provider_invocation_ref: no_effect_invocation,
        failure_code: WorkPatchMaterializationFailureCode::PatchRejected,
    };
    let not_applied = materialization
        .record_not_applied(&not_applied_report)
        .await
        .expect("record typed no-effect failure");
    assert_eq!(not_applied.state, WorkPatchMaterializationState::Failed);
    assert_eq!(not_applied.phase, WorkPatchMaterializationPhase::Complete);
    assert_eq!(
        not_applied.apply_outcome,
        Some(WorkPatchMaterializationApplyOutcome::NotApplied)
    );
    assert_eq!(
        not_applied.failure_code,
        Some(WorkPatchMaterializationFailureCode::PatchRejected)
    );
    assert_eq!(
        materialization
            .record_not_applied(&not_applied_report)
            .await
            .expect("idempotent no-effect report"),
        not_applied
    );
    assert_eq!(
        repository
            .load_branch_subject(
                &no_effect_request.owner_id,
                &no_effect_request.work_id,
                &no_effect_request.target_branch_id,
            )
            .await
            .expect("load no-effect target")
            .expect("target subject"),
        restored_base,
        "a typed no-effect failure must not advance the canonical subject"
    );
    let mut mismatch_request = materialization_request.clone();
    mismatch_request.request_id =
        WorkChangeRef::parse(id("materialize-mismatch")).expect("mismatch request");
    mismatch_request.expected_target_branch_revision = restored_base.branch_revision;
    mismatch_request.expected_target_graph_revision = restored_base.graph_revision;
    let mismatch_operation = materialization
        .admit(&mismatch_request)
        .await
        .expect("admit mismatch proof operation");
    let mismatch_invocation =
        WorkProviderInvocationRef::parse(id("mismatch-invocation")).expect("mismatch invocation");
    materialization
        .claim_applying(
            &mismatch_request.owner_id,
            &mismatch_request.work_id,
            &mismatch_operation.operation_id,
            "mismatch-executor",
            &mismatch_invocation,
        )
        .await
        .expect("claim mismatch proof operation");
    let unexpected_revision = hash("unexpected-applied-result");
    let mismatch_report = WorkPatchMaterializationApplied {
        owner_id: mismatch_request.owner_id.clone(),
        work_id: mismatch_request.work_id.clone(),
        operation_id: mismatch_operation.operation_id,
        executor_token: "mismatch-executor".into(),
        provider_invocation_ref: mismatch_invocation,
        observed_subject_revision: unexpected_revision.clone(),
    };
    let mismatch = materialization
        .record_applied(&mismatch_report)
        .await
        .expect("persist known result mismatch");
    assert_eq!(mismatch.state, WorkPatchMaterializationState::Conflict);
    assert_eq!(mismatch.phase, WorkPatchMaterializationPhase::Complete);
    assert_eq!(
        mismatch.apply_outcome,
        Some(WorkPatchMaterializationApplyOutcome::ResultMismatch)
    );
    assert!(mismatch.completed_at.is_some());
    let mismatched_subject = repository
        .load_branch_subject(
            &mismatch_request.owner_id,
            &mismatch_request.work_id,
            &mismatch_request.target_branch_id,
        )
        .await
        .expect("load mismatched target")
        .expect("mismatched target subject");
    assert_eq!(mismatched_subject.subject_revision, unexpected_revision);

    let reset_for_target_change = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: materialization_request.owner_id.clone(),
            work_id: materialization_request.work_id.clone(),
            branch_id: materialization_request.target_branch_id.clone(),
            expected_branch_revision: mismatched_subject.branch_revision,
            graph_revision: mismatched_subject.graph_revision,
            subject_ref: target_subject.subject_ref.clone(),
            subject_revision: base_revision.clone(),
            source_ref: WorkChangeRef::parse(id("reset-target-base")).expect("reset base source"),
        })
        .await
        .expect("reset target before drift proof");
    let mut drift_request = materialization_request.clone();
    drift_request.request_id =
        WorkChangeRef::parse(id("materialize-target-drift")).expect("drift request");
    drift_request.expected_target_branch_revision = reset_for_target_change.branch_revision;
    drift_request.expected_target_graph_revision = reset_for_target_change.graph_revision;
    let drift_operation = materialization
        .admit(&drift_request)
        .await
        .expect("admit target drift proof operation");
    let drift_invocation =
        WorkProviderInvocationRef::parse(id("drift-invocation")).expect("drift invocation");
    materialization
        .claim_applying(
            &drift_request.owner_id,
            &drift_request.work_id,
            &drift_operation.operation_id,
            "drift-executor",
            &drift_invocation,
        )
        .await
        .expect("claim target drift proof operation");
    let concurrent_subject = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: drift_request.owner_id.clone(),
            work_id: drift_request.work_id.clone(),
            branch_id: drift_request.target_branch_id.clone(),
            expected_branch_revision: drift_request.expected_target_branch_revision,
            graph_revision: drift_request.expected_target_graph_revision,
            subject_ref: target_subject.subject_ref.clone(),
            subject_revision: hash("concurrent-target-change"),
            source_ref: WorkChangeRef::parse(id("concurrent-target-change"))
                .expect("concurrent source"),
        })
        .await
        .expect("advance target concurrently");
    let drift_report = WorkPatchMaterializationApplied {
        owner_id: drift_request.owner_id.clone(),
        work_id: drift_request.work_id.clone(),
        operation_id: drift_operation.operation_id,
        executor_token: "drift-executor".into(),
        provider_invocation_ref: drift_invocation,
        observed_subject_revision: result_revision.clone(),
    };
    let drifted = materialization
        .record_applied(&drift_report)
        .await
        .expect("persist target drift without overwriting it");
    assert_eq!(drifted.state, WorkPatchMaterializationState::Conflict);
    assert_eq!(
        drifted.apply_outcome,
        Some(WorkPatchMaterializationApplyOutcome::TargetChanged)
    );
    assert_eq!(
        repository
            .load_branch_subject(
                &drift_request.owner_id,
                &drift_request.work_id,
                &drift_request.target_branch_id,
            )
            .await
            .expect("load drifted target")
            .expect("drifted target subject"),
        concurrent_subject,
        "a late provider report must not overwrite a newer canonical target"
    );

    let first_operation_page = materialization
        .list_for_source(WorkPatchMaterializationQuery {
            owner_id: materialization_request.owner_id.clone(),
            work_id: materialization_request.work_id.clone(),
            target_branch_id: materialization_request.target_branch_id.clone(),
            source_branch_id: request.branch_id.clone(),
            before: None,
            limit: WorkPatchMaterializationPageLimit::new(2).expect("page limit"),
        })
        .await
        .expect("list latest durable materializations");
    assert_eq!(first_operation_page.operations.len(), 2);
    let cursor = first_operation_page
        .next_cursor
        .clone()
        .expect("more durable operation history");
    assert_eq!(
        cursor.operation_id,
        first_operation_page.operations[1].operation_id
    );
    let second_operation_page = materialization
        .list_for_source(WorkPatchMaterializationQuery {
            owner_id: materialization_request.owner_id.clone(),
            work_id: materialization_request.work_id.clone(),
            target_branch_id: materialization_request.target_branch_id.clone(),
            source_branch_id: request.branch_id.clone(),
            before: Some(cursor),
            limit: WorkPatchMaterializationPageLimit::new(2).expect("page limit"),
        })
        .await
        .expect("continue durable materialization history");
    assert!(
        second_operation_page.operations.iter().all(|operation| {
            first_operation_page
                .operations
                .iter()
                .all(|first| first.operation_id != operation.operation_id)
        }),
        "keyset pages must not overlap"
    );
    assert!(matches!(
        materialization
            .list_for_source(WorkPatchMaterializationQuery {
                owner_id: astra_services::work::WorkOwnerId::parse(id("other-owner"))
                    .expect("other owner"),
                work_id: materialization_request.work_id.clone(),
                target_branch_id: materialization_request.target_branch_id.clone(),
                source_branch_id: request.branch_id.clone(),
                before: None,
                limit: WorkPatchMaterializationPageLimit::new(2).expect("page limit"),
            })
            .await,
        Err(WorkPatchMaterializationError::NotFound)
    ));

    let tampered_payload_id = id("payload");
    insert_patch_payload(
        &pool,
        &owner,
        &session,
        &tampered_payload_id,
        patch_content("tampered", Some(&"0".repeat(64))),
    )
    .await;
    let mut tampered = request.clone();
    tampered.patch_artifact_id = WorkPatchArtifactId::parse(id("patch")).expect("patch id");
    tampered.payload_artifact_id = tampered_payload_id;
    tampered.source_ref = WorkChangeRef::parse(id("export-event")).expect("export source");
    assert!(matches!(
        repository.record_patch_artifact(tampered).await,
        Err(WorkRepositoryError::PatchArtifactConflict {
            resource: WorkPatchArtifactBasisResource::PayloadContract
        })
    ));

    let stale_payload_id = id("payload");
    insert_patch_payload(
        &pool,
        &owner,
        &session,
        &stale_payload_id,
        patch_content(patch, None),
    )
    .await;
    let advanced = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: request.owner_id.clone(),
            work_id: request.work_id.clone(),
            branch_id: request.branch_id.clone(),
            expected_branch_revision: subject.branch_revision,
            graph_revision: subject.graph_revision,
            subject_ref,
            subject_revision: hash("new-result"),
            source_ref: WorkChangeRef::parse(id("subject-event")).expect("subject source"),
        })
        .await
        .expect("advance subject after export");
    assert_eq!(
        advanced.subject_record_revision,
        WorkBranchSubjectRevision::new(2).expect("revision two")
    );
    let mut stale = request;
    stale.patch_artifact_id = WorkPatchArtifactId::parse(id("patch")).expect("patch id");
    stale.payload_artifact_id = stale_payload_id;
    stale.source_ref = WorkChangeRef::parse(id("export-event")).expect("export source");
    assert!(matches!(
        repository.record_patch_artifact(stale).await,
        Err(WorkRepositoryError::PatchArtifactConflict {
            resource: WorkPatchArtifactBasisResource::BranchRevision
        })
    ));
    let patch_rows: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_patch_artifacts
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner)
    .bind(&work)
    .fetch_one(pool.get())
    .await
    .expect("count patch bindings")
    .try_get("count")
    .expect("count column");
    assert_eq!(patch_rows, 1, "failed admissions must leave no binding");

    let conflicted_commit = commit_service
        .record_committed(&committed_report)
        .await
        .expect("retain committed provider receipt without overwriting changed target");
    assert_eq!(conflicted_commit.state, WorkPatchCommitState::Conflict);
    assert_eq!(conflicted_commit.phase, WorkPatchCommitPhase::Complete);
    assert_eq!(
        conflicted_commit.commit_sha.as_deref(),
        Some(expected_commit_sha.as_str())
    );
    assert_eq!(conflicted_commit.index_reconciled, Some(true));
    assert_eq!(
        commit_service
            .record_committed(&committed_report)
            .await
            .expect("idempotent conflicted commit receipt"),
        conflicted_commit
    );
    let preserved_subject = repository
        .load_branch_subject(
            &commit_request.owner_id,
            &commit_request.work_id,
            &commit_request.target_branch_id,
        )
        .await
        .expect("load preserved target subject")
        .expect("preserved target subject");
    assert_eq!(preserved_subject, advanced);
    stale_commit.expected_target_branch_revision = commit_request.expected_target_branch_revision;
    assert!(matches!(
        commit_service.admit(&stale_commit).await,
        Err(WorkPatchCommitError::Conflict(
            WorkPatchCommitConflict::TargetBranchRevision
        ))
    ));

    let second_payload_id = id("payload");
    insert_patch_payload(
        &pool,
        &owner,
        &session,
        &second_payload_id,
        patch_content(patch, None),
    )
    .await;
    let second_patch_id = WorkPatchArtifactId::parse(id("patch")).expect("second patch id");
    sqlx::query(
        "INSERT INTO session_artifact_references
         (user_id, session_id, artifact_id, reference_kind, reference_id)
         VALUES (?, ?, ?, 'state_item', ?)",
    )
    .bind(&owner)
    .bind(&session)
    .bind(&second_payload_id)
    .bind(second_patch_id.as_str())
    .execute(pool.get())
    .await
    .expect("retain second patch payload");
    repository
        .record_patch_artifact(NewWorkPatchArtifact {
            owner_id: commit_request.owner_id.clone(),
            work_id: commit_request.work_id.clone(),
            branch_id: commit_request.target_branch_id.clone(),
            patch_artifact_id: second_patch_id.clone(),
            payload_artifact_id: second_payload_id,
            expected_branch_revision: advanced.branch_revision,
            expected_graph_revision: advanced.graph_revision,
            expected_subject_record_revision: advanced.subject_record_revision,
            subject_ref: advanced.subject_ref.clone(),
            base_subject_revision: hash("second clean base"),
            result_subject_revision: advanced.subject_revision.clone(),
            format: WorkPatchFormat::UnifiedDiffV1,
            provider_invocation_ref: WorkProviderInvocationRef::parse(id("second-export"))
                .expect("second export invocation"),
            source_ref: WorkChangeRef::parse(id("second-export-event"))
                .expect("second export source"),
        })
        .await
        .expect("record second exact patch");
    let second_commit_request = WorkPatchCommitRequest {
        request_id: WorkChangeRef::parse(id("second-commit-request"))
            .expect("second commit request"),
        patch_artifact_id: second_patch_id,
        expected_target_branch_revision: advanced.branch_revision,
        expected_target_graph_revision: advanced.graph_revision,
        message: "Commit the second reviewed result".into(),
        policy_decision_ref: WorkChangeRef::parse(id("second-commit-policy"))
            .expect("second commit policy"),
        ..commit_request.clone()
    };
    let second_commit = commit_service
        .admit(&second_commit_request)
        .await
        .expect("admit second exact commit");
    let second_invocation = WorkProviderInvocationRef::parse(id("second-commit-invocation"))
        .expect("second commit invocation");
    let second_executor = id("second-commit-executor");
    commit_service
        .claim_committing(
            &second_commit_request.owner_id,
            &second_commit_request.work_id,
            &second_commit.operation_id,
            &second_executor,
            &second_invocation,
        )
        .await
        .expect("claim second commit");
    let second_committed_revision = hash("second clean committed subject");
    let succeeded_commit = commit_service
        .record_committed(&WorkPatchCommitCommitted {
            owner_id: second_commit_request.owner_id.clone(),
            work_id: second_commit_request.work_id.clone(),
            operation_id: second_commit.operation_id,
            executor_token: second_executor,
            provider_invocation_ref: second_invocation,
            commit_sha: "b".repeat(40),
            observed_subject_revision: second_committed_revision.clone(),
            index_reconciled: true,
        })
        .await
        .expect("commit receipt and canonical subject atomically");
    assert_eq!(succeeded_commit.state, WorkPatchCommitState::Succeeded);
    assert_eq!(succeeded_commit.phase, WorkPatchCommitPhase::Complete);
    let succeeded_subject = repository
        .load_branch_subject(
            &second_commit_request.owner_id,
            &second_commit_request.work_id,
            &second_commit_request.target_branch_id,
        )
        .await
        .expect("load successful committed subject")
        .expect("successful committed subject");
    assert_eq!(
        succeeded_subject.subject_revision,
        second_committed_revision
    );
    assert_eq!(
        succeeded_subject.branch_revision,
        advanced
            .branch_revision
            .checked_next()
            .expect("next successful commit branch revision")
    );

    cleanup_owner(&pool, &owner).await;
}
