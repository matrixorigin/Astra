mod common;

use std::time::Duration;

use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationDispatchAdmission,
};
use astra_services::work::{
    DatabaseWorkBranchDeletionService, DatabaseWorkRepository, NewWorkRecoveryPoint,
    WorkBranchDeletionRequest, WorkBranchId, WorkBranchRevision, WorkChangeRef,
    WorkConflictResource, WorkId, WorkOwnerId, WorkRecoveryPointQuery, WorkRecoveryPointStatus,
    WorkRepository, WorkRepositoryError, WorkRevision,
};
use astra_services::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, ReserveTurnOutcome,
    SessionContextCoordinator, SessionExecutionBindingV1,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, CoordinatorMutationV1, DurableToolReference,
    RECOVERY_POINT_MANIFEST_SCHEMA_VERSION, RecoveryPointEnvironmentRequirementsV1,
    RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1, RecoveryPointManifestV1,
    RecoveryPointReasonV1, SessionContextHeadV1, SessionCursorV1, SessionKeyV1, SessionSurfaceV1,
    ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
};
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("work_recovery_points", "owner_id"),
        ("work_branch_deletion_operations", "owner_id"),
        ("work_branches", "owner_id"),
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

/// Establish the canonical Session facts required by the server publisher:
/// one materialized conversation head and one ready execution binding. The
/// helper uses the coordinator APIs so the publisher integration test does not
/// recreate a second context state machine with raw head JSON.
async fn establish_server_boundary(
    pool: &astra_core::SharedPool,
    owner_id: &str,
    session_id: &str,
) {
    let key = SessionKeyV1::owner_session("server", owner_id, session_id, "main");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let initial_binding =
        SessionExecutionBindingV1::server_work_default(format!("session:{session_id}:branch:main"));
    coordinator
        .load_or_initialize_execution_binding(&key, &initial_binding)
        .await
        .expect("initialize publisher execution binding");

    let actor = ActorContextV1::owner_user(
        owner_id,
        "work-recovery-point-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let lease = match coordinator
        .acquire_writer(&key, None, &actor, Duration::from_secs(30), &id("writer"))
        .await
        .expect("acquire publisher fixture writer")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected publisher writer outcome: {other:?}"),
    };
    let reservation = match coordinator
        .reserve_turn(
            &lease,
            None,
            Duration::from_secs(30),
            &id("reservation"),
            None,
        )
        .await
        .expect("reserve publisher fixture turn")
    {
        ReserveTurnOutcome::Reserved(reservation) => reservation,
        other => panic!("unexpected publisher reservation outcome: {other:?}"),
    };
    let messages = vec![serde_json::json!({
        "role": "assistant",
        "content": "safe recovery boundary"
    })];
    let outcome = coordinator
        .commit_turn(
            &reservation,
            CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                completed_turn: 1,
                journal_event_seq: 1,
                conversation_seq: 1,
                compaction_generation: 0,
                config_version_id: None,
                mode: CanonicalDeltaModeV1::Append,
                logical_segments: vec![messages],
            },
            &id("commit"),
        )
        .await
        .expect("commit publisher fixture boundary");
    assert!(matches!(outcome, CoordinatorMutationV1::Applied { .. }));
    coordinator
        .release_writer(&lease)
        .await
        .expect("release publisher fixture writer");
}

async fn cleanup_session_context(pool: &astra_core::SharedPool, owner_id: &str) {
    for table in [
        "session_context_operation_receipts",
        "session_context_authority_events",
        "session_execution_workspace_claims",
        "session_execution_bindings",
        "session_context_heads",
        "conversation_manifest_segments",
        "conversation_manifest_nodes",
        "conversation_segments",
    ] {
        let statement = format!("DELETE FROM {table} WHERE owner_user_id = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
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
        recovery_point_id: id("recovery-point"),
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
            logical_workspace_id: id("workspace"),
            executor_kind: RecoveryPointExecutorKindV1::Server,
            executor_id: id("server"),
            canonical_binding_hash:
                "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
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

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn recovery_point_capture_is_preparing_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
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
        request_id: WorkChangeRef::parse(id("request")).expect("request"),
        manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
    };
    let record = repository
        .recovery_points()
        .record_preparing(request.clone())
        .await
        .expect("record recovery capture");
    assert_eq!(record.status, WorkRecoveryPointStatus::Preparing);
    assert!(record.published_at.is_none());

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
    let other_work_id = id("work");
    let other_session_id = id("session");
    repository
        .create_genesis(common::work_genesis(
            &other_owner_id,
            &other_work_id,
            &branch_id,
            &other_session_id,
            &id("intent"),
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
            request_id: WorkChangeRef::parse(id("request")).expect("other request"),
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
    let unauthorized_owner = WorkOwnerId::parse(id("owner")).expect("unauthorized owner");
    assert!(
        repository
            .recovery_points()
            .load(&unauthorized_owner, &work, &record.recovery_point_id)
            .await
            .expect("load unauthorized recovery point")
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn server_recovery_publisher_is_verified_idempotent_and_race_safe() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("publisher-owner");
    let work_id = id("publisher-work");
    let branch_id = id("publisher-branch");
    let session_id = id("publisher-session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_session_context(&pool, &owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Publish a canonical server recovery boundary.",
        ))
        .await
        .expect("create publisher Work");
    establish_server_boundary(&pool, &owner_id, &session_id).await;

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let request_id = WorkChangeRef::parse(id("publish-request")).expect("request");
    let request = astra_services::work::NewServerWorkRecoveryPoint {
        owner_id: owner.clone(),
        work_id: work.clone(),
        branch_id: branch.clone(),
        request_id: request_id.clone(),
        reason: RecoveryPointReasonV1::SafeBoundary,
        expected_work_revision: Some(1),
        expected_branch_revision: Some(1),
        expected_graph_revision: Some(1),
    };
    let first = repository
        .recovery_points()
        .publish_server_capture(request.clone())
        .await
        .expect("publish canonical recovery point");
    assert_eq!(first.status, WorkRecoveryPointStatus::Published);
    let manifest = first.manifest.as_ref().expect("published manifest");
    assert_eq!(manifest.owner_id, owner_id);
    assert_eq!(manifest.work_id, work_id);
    assert_eq!(manifest.branch_id, branch_id);
    assert_eq!(
        first.assessment.coverage.work,
        astra_turn_types::RecoveryPointCoverageStatusV1::Verified
    );
    assert_eq!(
        first.assessment.coverage.conversation,
        astra_turn_types::RecoveryPointCoverageStatusV1::Verified
    );
    assert_eq!(
        first.assessment.coverage.workspace,
        astra_turn_types::RecoveryPointCoverageStatusV1::NotCaptured
    );
    assert!(!first.assessment.restore_action_available);

    let replay = repository
        .recovery_points()
        .publish_server_capture(request.clone())
        .await
        .expect("replay canonical recovery point");
    assert_eq!(replay, first);

    let mut changed = request.clone();
    changed.reason = RecoveryPointReasonV1::BeforeEnvironmentChange;
    assert!(matches!(
        repository
            .recovery_points()
            .publish_server_capture(changed)
            .await,
        Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::RecoveryPointRequest
        })
    ));

    // Start two first admissions for a fresh request before either caller has
    // inserted a row. The Work lock and request re-check must converge both
    // callers on one published identity.
    let race_request = astra_services::work::NewServerWorkRecoveryPoint {
        request_id: WorkChangeRef::parse(id("publish-race-request")).expect("race request"),
        reason: RecoveryPointReasonV1::UserRequested,
        ..request
    };
    let left_repository = repository.recovery_points();
    let right_repository = repository.recovery_points();
    let (left, right) = tokio::join!(
        left_repository.publish_server_capture(race_request.clone()),
        right_repository.publish_server_capture(race_request.clone()),
    );
    let left = left.expect("first concurrent publication");
    let right = right.expect("second concurrent publication");
    assert_eq!(left, right);
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_recovery_points
         WHERE owner_id = ? AND work_id = ? AND request_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(race_request.request_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("count concurrent published rows");
    assert_eq!(row_count, 1);

    // A bad persisted manifest is surfaced as the exact point's typed
    // corruption assessment; it must not make the healthy concurrent point
    // disappear from the bounded branch list.
    sqlx::query(
        "UPDATE work_recovery_points SET manifest_json = ?
         WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ?",
    )
    .bind("{")
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&first.recovery_point_id)
    .execute(pool.get())
    .await
    .expect("corrupt one persisted recovery manifest");
    let listed = repository
        .recovery_points()
        .list_published(
            WorkRecoveryPointQuery::new(owner.clone(), work.clone()).branch(branch.clone()),
        )
        .await
        .expect("list around a corrupt recovery point");
    assert_eq!(listed.len(), 2);
    let damaged = listed
        .iter()
        .find(|point| point.recovery_point_id == first.recovery_point_id)
        .expect("damaged point remains identifiable");
    assert!(damaged.manifest.is_none());
    assert_eq!(
        damaged.assessment.coverage.conversation,
        astra_turn_types::RecoveryPointCoverageStatusV1::Corrupt
    );

    // Keep the manifest bytes and its content hash intact, but move them to a
    // different storage identity. The row must remain visible as corrupt;
    // otherwise a copied valid manifest could be presented as the requested
    // recovery point.
    let mismatched_row_id = id("mismatched-row");
    sqlx::query(
        "UPDATE work_recovery_points SET recovery_point_id = ?
         WHERE owner_id = ? AND work_id = ? AND request_id = ?",
    )
    .bind(&mismatched_row_id)
    .bind(&owner_id)
    .bind(&work_id)
    .bind(race_request.request_id.as_str())
    .execute(pool.get())
    .await
    .expect("move a valid manifest to a different recovery-point row identity");
    let listed = repository
        .recovery_points()
        .list_published(WorkRecoveryPointQuery::new(owner.clone(), work.clone()).branch(branch))
        .await
        .expect("list recovery point with mismatched row identity");
    let mismatched = listed
        .iter()
        .find(|point| point.recovery_point_id == mismatched_row_id)
        .expect("mismatched row remains identifiable");
    assert!(mismatched.manifest.is_none());
    assert_eq!(
        mismatched.assessment.coverage.conversation,
        astra_turn_types::RecoveryPointCoverageStatusV1::Corrupt
    );

    cleanup_session_context(&pool, &owner_id).await;
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn unknown_effect_survives_terminal_compaction_and_blocks_publication() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("publisher-effect-owner");
    let work_id = id("publisher-effect-work");
    let branch_id = id("publisher-effect-branch");
    let session_id = id("publisher-effect-session");
    let run_id = id("publisher-effect-run");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_session_context(&pool, &owner_id).await;
    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Keep unknown external effects visible during recovery publication.",
        ))
        .await
        .expect("create effect Work");
    establish_server_boundary(&pool, &owner_id, &session_id).await;

    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status,
          owner_pod_id, owner_lease_expires_at, run_generation)
         VALUES (?, ?, ?, ?, ?, 'running', 'tool-invocation-ledger-test-owner',
                 TIMESTAMPADD(MINUTE, 10, NOW(6)), 0)",
    )
    .bind(&run_id)
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&run_id)
    .bind(&run_id)
    .execute(pool.get())
    .await
    .expect("insert effect run");
    let identity = ToolInvocationIdentity::new(
        &owner_id,
        &session_id,
        &run_id,
        "effect-turn",
        "unknown-effect",
    )
    .expect("effect identity");
    let decision = ToolInvocationDecision::new(&serde_json::json!({
        "route": "server_local"
    }))
    .expect("effect decision");
    let fingerprint = ToolInvocationFingerprint::new(
        DurableToolReference::built_in("bash", "registry-v1").expect("tool reference"),
        &serde_json::json!({"command": "unknown effect"}),
        &decision.decision_id,
    )
    .expect("effect fingerprint");
    let ledger = DatabaseToolInvocationLedger::new(pool.clone());
    ledger
        .prepare(&identity, &fingerprint, &decision)
        .await
        .expect("prepare effect");
    ledger
        .claim_dispatch(
            &identity,
            "effect-worker",
            90_000,
            ToolInvocationDispatchAdmission {
                expected_control_epoch: -1,
                expected_owner_generation: 0,
                expected_owner_pod_id: "tool-invocation-ledger-test-owner".into(),
                expected_execution_binding_generation: None,
            },
        )
        .await
        .expect("dispatch effect");
    ledger
        .mark_outcome_unknown(&identity, "effect-worker")
        .await
        .expect("mark unknown effect");
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(&owner_id)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .expect("close effect run");

    let compacted = ledger
        .compact_terminal_run_batch(&owner_id, &session_id, &run_id)
        .await
        .expect("compact resolved portion while retaining unknown effect");
    assert_eq!(compacted.archived_records, 0);
    assert_eq!(compacted.remaining_records, 1);

    let request = astra_services::work::NewServerWorkRecoveryPoint {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&work_id).expect("work"),
        branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
        request_id: WorkChangeRef::parse(id("effect-publish-request")).expect("request"),
        reason: RecoveryPointReasonV1::SafeBoundary,
        expected_work_revision: None,
        expected_branch_revision: None,
        expected_graph_revision: None,
    };
    assert!(matches!(
        repository
            .recovery_points()
            .publish_server_capture(request)
            .await,
        Err(WorkRepositoryError::RecoveryPointUnavailable {
            code: "effect_review_required"
        })
    ));

    sqlx::query("DELETE FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean unknown effect");
    sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
        .bind(&owner_id)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .expect("clean effect run");
    cleanup_session_context(&pool, &owner_id).await;
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn server_recovery_publisher_rejects_busy_slots_and_missing_bindings() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("publisher-boundary-owner");
    let work_id = id("publisher-boundary-work");
    let branch_id = id("publisher-boundary-branch");
    let session_id = id("publisher-boundary-session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_session_context(&pool, &owner_id).await;
    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Reject recovery publication while the Session boundary is busy.",
        ))
        .await
        .expect("create boundary Work");
    establish_server_boundary(&pool, &owner_id, &session_id).await;

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let request = || astra_services::work::NewServerWorkRecoveryPoint {
        owner_id: owner.clone(),
        work_id: work.clone(),
        branch_id: branch.clone(),
        request_id: WorkChangeRef::parse(id("boundary-request")).expect("request"),
        reason: RecoveryPointReasonV1::SafeBoundary,
        expected_work_revision: None,
        expected_branch_revision: None,
        expected_graph_revision: None,
    };
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let expected_cursor = coordinator
        .load_head(&key)
        .await
        .expect("load boundary head")
        .expect("publisher boundary head")
        .cursor;
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "work-recovery-boundary-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let lease = match coordinator
        .acquire_writer(
            &key,
            Some(&expected_cursor),
            &actor,
            Duration::from_secs(30),
            &id("busy-writer"),
        )
        .await
        .expect("acquire busy fixture writer")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected busy writer outcome: {other:?}"),
    };
    assert!(matches!(
        repository
            .recovery_points()
            .publish_server_capture(request())
            .await,
        Err(WorkRepositoryError::SessionBusy)
    ));
    coordinator
        .release_writer(&lease)
        .await
        .expect("release busy fixture writer");

    sqlx::query(
        "INSERT INTO agent_session_execution_slots (user_id, session_id, run_id)
         VALUES (?, ?, ?)",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .bind(id("active-run"))
    .execute(pool.get())
    .await
    .expect("insert active execution slot");
    assert!(matches!(
        repository
            .recovery_points()
            .publish_server_capture(request())
            .await,
        Err(WorkRepositoryError::SessionBusy)
    ));
    sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("remove active execution slot");

    sqlx::query(
        "DELETE FROM session_execution_bindings
         WHERE isolation_domain = 'server' AND owner_user_id = ?
           AND session_id = ? AND branch_id = 'main'",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("remove execution binding");
    assert!(matches!(
        repository
            .recovery_points()
            .publish_server_capture(request())
            .await,
        Err(WorkRepositoryError::RecoveryPointUnavailable {
            code: "execution_binding_missing"
        })
    ));

    cleanup_session_context(&pool, &owner_id).await;
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_removes_branch_recovery_points() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let deletion = DatabaseWorkBranchDeletionService::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let delivery_session_id = id("session");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
            &id("intent"),
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
            request_id: WorkChangeRef::parse(id("request")).expect("request"),
            manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
        })
        .await
        .expect("record recovery point");

    let admission = deletion
        .admit(&WorkBranchDeletionRequest {
            request_id: id("delete"),
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
}
