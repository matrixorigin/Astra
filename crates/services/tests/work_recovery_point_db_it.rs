mod common;

use std::time::Duration;

use astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger;
use astra_services::work::{
    DatabaseWorkBranchDeletionService, DatabaseWorkRepository, NewWorkRecoveryPoint,
    WorkBranchDeletionRequest, WorkBranchId, WorkBranchRevision, WorkChangeRef,
    WorkConflictResource, WorkId, WorkOwnerId, WorkRecoveryPointCaptureRequest,
    WorkRecoveryPointQuery, WorkRecoveryPointStatus, WorkRepository, WorkRepositoryError,
    WorkRevision,
};
use astra_services::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, DatabaseSessionService,
    ReserveTurnOutcome, SessionContextCoordinator, SessionService,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, CoordinatorMutationV1,
    RECOVERY_POINT_MANIFEST_SCHEMA_VERSION, RecoveryPointEnvironmentRequirementsV1,
    RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1, RecoveryPointManifestV1,
    RecoveryPointReasonV1, SessionContextHeadV1, SessionCursorV1, SessionKeyV1, SessionSurfaceV1,
};
use axum::http::StatusCode;
use sha2::{Digest, Sha256};

async fn add_non_delivery_branch(
    pool: &astra_core::SharedPool,
    owner_id: &str,
    work_id: &str,
    delivery_branch_id: &str,
    branch_id: &str,
    session_id: &str,
) {
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id, origin_branch_id,
          fork_cursor, goal_revision_ref, criteria_set_revision_ref, basis_graph_revision,
          current_graph_revision, created_at, updated_at, archived_at)
         SELECT owner_id, work_id, ?, 1, ?, branch_id, ?, goal_revision_ref,
                criteria_set_revision_ref, basis_graph_revision, current_graph_revision,
                NOW(6), NOW(6), NULL
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(branch_id)
    .bind(session_id)
    .bind(format!("fork-{branch_id}"))
    .bind(owner_id)
    .bind(work_id)
    .bind(delivery_branch_id)
    .execute(pool.get())
    .await
    .expect("add non-delivery branch");
}

fn manifest(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    session_id: &str,
) -> RecoveryPointManifestV1 {
    let session_key = SessionKeyV1::owner_session("tenant", owner_id, session_id, "main");
    let session_cursor = SessionCursorV1 {
        schema_version: 1,
        owner_id: owner_id.to_owned(),
        session_id: session_id.to_owned(),
        branch_id: "main".to_owned(),
        completed_turn: 1,
        journal_event_seq: 1,
        conversation_seq: 1,
        canonical_root_hash: "a".repeat(64),
        projection_schema: 1,
        compaction_generation: 0,
        config_version_id: None,
    };
    let mut manifest = RecoveryPointManifestV1 {
        schema_version: RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
        recovery_point_id: common::id("recovery-point"),
        owner_id: owner_id.to_owned(),
        work_id: work_id.to_owned(),
        branch_id: branch_id.to_owned(),
        work_revision: 1,
        branch_revision: 1,
        graph_revision: 1,
        goal_revision: 1,
        criteria_set_revision: 1,
        session_key: session_key.clone(),
        session_cursor: session_cursor.clone(),
        context_head: SessionContextHeadV1 {
            schema_version: 1,
            key: session_key.clone(),
            cursor: session_cursor,
            latest_manifest_root: "a".repeat(64),
            total_canonical_bytes: 1,
            total_message_count: 1,
            writer_epoch: 1,
        },
        run: None,
        execution: RecoveryPointExecutionBindingV1 {
            binding_generation: 1,
            binding_state: astra_turn_types::RecoveryPointBindingStateV1::Ready,
            logical_workspace_id: common::id("workspace"),
            executor_kind: RecoveryPointExecutorKindV1::Server,
            executor_id: common::id("server"),
            binding_hash: String::new(),
            physical_workspace_id: None,
        },
        workspace: None,
        artifacts: vec![],
        environment: RecoveryPointEnvironmentRequirementsV1::default(),
        reason: RecoveryPointReasonV1::UserRequested,
        created_at: "2026-09-16T00:00:00Z".to_owned(),
    };
    manifest.execution.binding_hash = manifest.execution.content_hash();
    manifest
}

async fn commit_context_turn(
    pool: &astra_core::SharedPool,
    owner_id: &str,
    session_id: &str,
    turn: u32,
) -> SessionCursorV1 {
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let key = SessionKeyV1::owner_session("server", owner_id, session_id, "main");
    let actor = ActorContextV1::owner_user(
        owner_id,
        "work-recovery-point-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let lease = match coordinator
        .acquire_writer(
            &key,
            None,
            &actor,
            Duration::from_secs(30),
            &format!("recovery-point-lease-{turn}"),
        )
        .await
        .expect("acquire Session writer")
    {
        AcquireWriterOutcome::Acquired(lease) | AcquireWriterOutcome::AlreadyAcquired(lease) => {
            lease
        }
        other => panic!("unexpected writer outcome: {other:?}"),
    };
    let reservation = match coordinator
        .reserve_turn(
            &lease,
            None,
            Duration::from_secs(30),
            &format!("recovery-point-turn-{turn}"),
            None,
        )
        .await
        .expect("reserve Session turn")
    {
        ReserveTurnOutcome::Reserved(reservation)
        | ReserveTurnOutcome::AlreadyReserved(reservation) => reservation,
        other => panic!("unexpected reservation outcome: {other:?}"),
    };
    let outcome = coordinator
        .commit_turn(
            &reservation,
            CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                completed_turn: turn,
                journal_event_seq: u64::from(turn),
                conversation_seq: u64::from(turn),
                compaction_generation: 0,
                config_version_id: None,
                mode: CanonicalDeltaModeV1::Append,
                logical_segments: vec![vec![serde_json::json!({
                    "role": "user",
                    "content": format!("turn {turn}"),
                })]],
            },
            &format!("recovery-point-commit-{turn}"),
        )
        .await
        .expect("commit Session turn");
    let cursor = match outcome {
        CoordinatorMutationV1::Applied { cursor }
        | CoordinatorMutationV1::AlreadyApplied { cursor } => cursor,
        other => panic!("unexpected commit outcome: {other:?}"),
    };
    coordinator
        .release_writer(&lease)
        .await
        .expect("release Session writer");
    cursor
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn recovery_point_capture_is_preparing_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let other_owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &common::id("intent"),
            "Persist one owner-scoped recovery boundary.",
        ))
        .await
        .expect("create Work");

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let request = NewWorkRecoveryPoint {
        owner_id: owner.clone(),
        work_id: work.clone(),
        branch_id: branch,
        request_id: WorkChangeRef::parse(common::id("request")).expect("request"),
        manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
    };
    let record = repository
        .recovery_points()
        .record_preparing(request.clone())
        .await
        .expect("record recovery capture");
    assert_eq!(record.status, WorkRecoveryPointStatus::Preparing);
    assert!(record.ready_at.is_none());

    // A retry of the exact admitted request must return the same durable row,
    // while reusing the request identity for a different manifest is a typed
    // conflict rather than a second capture.
    let replay = repository
        .recovery_points()
        .record_preparing(request.clone())
        .await
        .expect("replay recovery capture");
    assert_eq!(replay, record);
    let mut changed_request = request.clone();
    changed_request.manifest.created_at = "2026-09-16T00:00:01Z".to_owned();
    assert!(matches!(
        repository
            .recovery_points()
            .record_preparing(changed_request)
            .await,
        Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::RecoveryPointRequest
        })
    ));

    let replay_repository = repository.recovery_points();
    let (left, right) = tokio::join!(
        replay_repository.record_preparing(request.clone()),
        replay_repository.record_preparing(request.clone()),
    );
    let left = left.expect("concurrent left replay");
    let right = right.expect("concurrent right replay");
    assert_eq!(left, record);
    assert_eq!(right, record);
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_recovery_points
         WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&record.recovery_point_id)
    .fetch_one(pool.get())
    .await
    .expect("count replayed recovery rows");
    assert_eq!(row_count, 1);

    let loaded = repository
        .recovery_points()
        .load(&owner, &work, &record.recovery_point_id)
        .await
        .expect("load owner recovery point")
        .expect("recovery point exists");
    assert_eq!(loaded.recovery_point_id, record.recovery_point_id);

    let other_owner = WorkOwnerId::parse(&other_owner_id).expect("other owner");
    let other_work_id = common::id("work");
    let other_session_id = common::id("session");
    repository
        .create_genesis(common::work_genesis(
            &other_owner_id,
            &other_work_id,
            &branch_id,
            &other_session_id,
            &common::id("intent"),
            "A second owner may use the same opaque recovery-point identifier in another Work.",
        ))
        .await
        .expect("create second owner Work");
    let mut other_manifest = manifest(
        &other_owner_id,
        &other_work_id,
        &branch_id,
        &other_session_id,
    );
    other_manifest.recovery_point_id = record.recovery_point_id.clone();
    other_manifest.execution.binding_hash = other_manifest.execution.content_hash();
    let other_record = repository
        .recovery_points()
        .record_preparing(NewWorkRecoveryPoint {
            owner_id: other_owner.clone(),
            work_id: WorkId::parse(&other_work_id).expect("other work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("other branch"),
            request_id: WorkChangeRef::parse(common::id("request")).expect("other request"),
            manifest: other_manifest,
        })
        .await
        .expect("record second owner recovery capture");
    assert_eq!(other_record.recovery_point_id, record.recovery_point_id);
    let other_loaded = repository
        .recovery_points()
        .load(
            &other_owner,
            &WorkId::parse(&other_work_id).expect("other work"),
            &record.recovery_point_id,
        )
        .await
        .expect("load second owner recovery point")
        .expect("second owner recovery point exists");
    assert_eq!(other_loaded.owner_id, other_owner);
    assert_eq!(other_loaded.branch_id.as_str(), branch_id);
    let unauthorized_owner = WorkOwnerId::parse(common::id("owner")).expect("unauthorized owner");
    assert!(
        repository
            .recovery_points()
            .load(&unauthorized_owner, &work, &record.recovery_point_id)
            .await
            .expect("load unauthorized recovery point")
            .is_none()
    );

    common::cleanup_work_owner(&pool, &owner_id).await;
    common::cleanup_work_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn canonical_recovery_point_capture_is_quiescent_idempotent_and_explicitly_logical() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("canonical-owner");
    let work_id = common::id("canonical-work");
    let branch_id = common::id("canonical-branch");
    let session_id = common::id("canonical-session");

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &common::id("canonical-intent"),
            "Capture one stable logical progress boundary.",
        ))
        .await
        .expect("create Work");
    let cursor = commit_context_turn(&pool, &owner_id, &session_id, 1).await;
    assert_eq!(cursor.completed_turn, 1);

    let request = WorkRecoveryPointCaptureRequest {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
        request_id: WorkChangeRef::parse("canonical-save-1").expect("request"),
        expected_work_revision: 1,
        expected_branch_revision: 1,
        reason: RecoveryPointReasonV1::UserRequested,
    };
    let captured = repository
        .recovery_points()
        .capture_canonical(request.clone())
        .await
        .expect("capture canonical boundary");
    assert_eq!(captured.status, WorkRecoveryPointStatus::Captured);
    assert_eq!(captured.manifest.as_ref().unwrap().session_cursor, cursor);
    assert!(captured.manifest.as_ref().unwrap().workspace.is_none());
    assert!(captured.manifest.as_ref().unwrap().run.is_none());
    assert!(captured.manifest.as_ref().unwrap().artifacts.is_empty());

    let replay = repository
        .recovery_points()
        .capture_canonical(request.clone())
        .await
        .expect("replay canonical boundary");
    assert_eq!(replay, captured);

    let stale = WorkRecoveryPointCaptureRequest {
        request_id: WorkChangeRef::parse("canonical-save-stale").expect("stale request"),
        expected_work_revision: 2,
        ..request
    };
    assert!(matches!(
        repository.recovery_points().capture_canonical(stale).await,
        Err(WorkRepositoryError::RecoveryPointNotCapturable { .. })
    ));

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn canonical_recovery_capture_still_sees_unknown_effect_after_compaction() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("unknown-effect-owner");
    let work_id = common::id("unknown-effect-work");
    let branch_id = common::id("unknown-effect-branch");
    let session_id = common::id("unknown-effect-session");
    let run_id = common::id("unknown-effect-run");

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &common::id("unknown-effect-intent"),
            "Do not capture a boundary while an effect outcome is unknown.",
        ))
        .await
        .expect("create Work");
    commit_context_turn(&pool, &owner_id, &session_id, 1).await;

    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status,
          owner_pod_id, run_generation, work_id, work_branch_id, work_graph_revision)
         VALUES (?, ?, ?, ?, ?, 'completed', 'unknown-effect-test-owner', 0, ?, ?, 1)",
    )
    .bind(&run_id)
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&run_id)
    .bind(&run_id)
    .bind(&work_id)
    .bind(&branch_id)
    .execute(pool.get())
    .await
    .expect("insert terminal effect run");
    let identity_key = format!("sha256:{}", "e".repeat(64));
    sqlx::query(
        "INSERT INTO tool_invocation_ledger
         (user_id, session_id, run_id, turn_chain_id, invocation_id,
          identity_key, fingerprint_json, decision_json, state,
          dispatch_certainty, attempt_count)
         VALUES (?, ?, ?, 'unknown-effect-turn', 'unknown-effect-call', ?, '{}', '{}',
                 'outcome_unknown', 'unknown', 1)",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&run_id)
    .bind(&identity_key)
    .execute(pool.get())
    .await
    .expect("insert unresolved effect");

    let ledger = DatabaseToolInvocationLedger::new(pool.clone());
    let compacted = ledger
        .compact_terminal_run_batch(&owner_id, &session_id, &run_id)
        .await
        .expect("compact terminal run");
    assert_eq!(compacted.archived_records, 0);
    assert_eq!(compacted.remaining_records, 1);
    assert!(compacted.artifact_id.is_none());

    let capture = WorkRecoveryPointCaptureRequest {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
        request_id: WorkChangeRef::parse("unknown-effect-save").expect("request"),
        expected_work_revision: 1,
        expected_branch_revision: 1,
        reason: RecoveryPointReasonV1::UserRequested,
    };
    assert!(matches!(
        repository
            .recovery_points()
            .capture_canonical(capture)
            .await,
        Err(WorkRepositoryError::RecoveryPointNotCapturable {
            reason: astra_services::work::WorkRecoveryPointBlocker::UnresolvedInvocation,
        })
    ));

    sqlx::query("DELETE FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean unresolved effect");
    sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean terminal effect run");
    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn recovery_point_capture_rejects_a_criterion_set_with_a_missing_member() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("branch");
    let session_id = common::id("session");

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &common::id("intent"),
            "Reject a recovery boundary when its criterion member disappeared.",
        ))
        .await
        .expect("create Work");

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let request = NewWorkRecoveryPoint {
        owner_id: owner.clone(),
        work_id: work.clone(),
        branch_id: branch.clone(),
        request_id: WorkChangeRef::parse(common::id("request")).expect("request"),
        manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
    };
    let record = repository
        .recovery_points()
        .record_preparing(request)
        .await
        .expect("record recovery capture");

    // Keep the set envelope internally self-consistent, but point it at a
    // revision that is absent. Capture must validate the complete immutable
    // member set in the same transaction; checking only revision/count/hash
    // would incorrectly publish this boundary as usable.
    let manifest_json =
        r#"{"schema_version":1,"members":[{"criterion_id":"missing-criterion","revision":1}]}"#;
    let manifest_hash = format!("sha256:{:x}", Sha256::digest(manifest_json.as_bytes()));
    sqlx::query(
        "UPDATE work_criterion_sets
         SET member_manifest_json = ?, member_manifest_hash = ?, member_count = 1
         WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(manifest_json)
    .bind(manifest_hash)
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("corrupt criterion-set member manifest");

    assert!(matches!(
        repository
            .recovery_points()
            .mark_captured(&owner, &work, &branch, &record.recovery_point_id)
            .await,
        Err(WorkRepositoryError::Corrupt { entity, .. }) if entity == "criterion definition"
    ));
    let loaded = repository
        .recovery_points()
        .load(&owner, &work, &record.recovery_point_id)
        .await
        .expect("load rejected recovery point")
        .expect("recovery point remains durable");
    assert_eq!(loaded.status, WorkRecoveryPointStatus::Preparing);

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn delivery_session_delete_explains_work_management_path() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let branch_id = common::id("delivery");
    let session_id = common::id("session");

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &common::id("intent"),
            "Explain why a delivery Session cannot be deleted while progress is saved.",
        ))
        .await
        .expect("create Work");
    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    repository
        .recovery_points()
        .record_preparing(NewWorkRecoveryPoint {
            owner_id: owner,
            work_id: work,
            branch_id: branch,
            request_id: WorkChangeRef::parse(common::id("request")).expect("request"),
            manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
        })
        .await
        .expect("record delivery recovery point");

    let service = DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(pool.clone());
    let (status, body) = service
        .delete_session(session_id.clone(), owner_id.clone())
        .await
        .expect_err("delivery Session deletion must remain protected");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.0.error_code.as_deref(), Some("session_has_saved_work"));
    assert!(body.0.detail.contains(&work_id));
    assert!(body.0.detail.contains(&branch_id));
    assert!(body.0.detail.contains("choose another delivery branch"));

    common::cleanup_work_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_removes_branch_recovery_points() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let deletion = DatabaseWorkBranchDeletionService::new(pool.clone());
    let owner_id = common::id("owner");
    let work_id = common::id("work");
    let delivery_branch_id = common::id("delivery");
    let delivery_session_id = common::id("session");
    let branch_id = common::id("branch");
    let session_id = common::id("session");

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
            &common::id("intent"),
            "Delete branch-owned recovery state with the branch.",
        ))
        .await
        .expect("create Work");
    add_non_delivery_branch(
        &pool,
        &owner_id,
        &work_id,
        &delivery_branch_id,
        &branch_id,
        &session_id,
    )
    .await;
    // Branch deletion fences the canonical Session before it removes the
    // branch-owned recovery point. Keep this fixture on that production
    // lifecycle instead of creating an orphaned work_branches row.
    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("create branch session");

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let record = repository
        .recovery_points()
        .record_preparing(NewWorkRecoveryPoint {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            request_id: WorkChangeRef::parse(common::id("request")).expect("request"),
            manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
        })
        .await
        .expect("record recovery point");

    let admission = deletion
        .admit(&WorkBranchDeletionRequest {
            request_id: common::id("delete"),
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            expected_work_revision: WorkRevision::new(1).expect("work revision"),
            expected_branch_revision: WorkBranchRevision::new(1).expect("branch revision"),
        })
        .await
        .expect("admit branch deletion");
    let token = deletion
        .claim_execution(&owner, &work, &branch, &admission.operation.operation_id)
        .await
        .expect("claim deletion executor")
        .expect("claim token");
    deletion
        .fence_session(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("fence branch session");
    deletion
        .cleanup_session(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("clean branch session");
    deletion
        .reconcile_lineage(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("reconcile branch lineage");
    deletion
        .complete_branch_cleanup(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("delete branch");

    assert!(
        repository
            .recovery_points()
            .load(&owner, &work, &record.recovery_point_id)
            .await
            .expect("load deleted recovery point")
            .is_none()
    );
    assert!(
        repository
            .recovery_points()
            .list(WorkRecoveryPointQuery::new(owner, work).branch(branch))
            .await
            .expect("list deleted branch recovery points")
            .is_empty()
    );

    common::cleanup_work_owner(&pool, &owner_id).await;
}
