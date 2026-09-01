mod common;

use astra_services::work::{
    DatabaseWorkBranchCatalogService, DatabaseWorkBranchDeletionService, DatabaseWorkRepository,
    WorkBranchCatalogError, WorkBranchDeletionError, WorkBranchDeletionOutcome,
    WorkBranchDeletionRequest, WorkBranchDeletionState, WorkBranchId,
    WorkBranchRetentionBasisResource, WorkBranchRetentionChange, WorkBranchRetentionKind,
    WorkBranchRetentionOutcome, WorkBranchRevision, WorkChangeRef, WorkGenesis, WorkId,
    WorkOwnerId, WorkRepository, WorkRepositoryError, WorkRevision,
};
use astra_services::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, ReserveTurnOutcome,
    SessionContextCoordinator, SessionContextCoordinatorError,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, SessionKeyV1, SessionSurfaceV1,
};
use sqlx::Row;
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn genesis(owner_id: &str, work_id: &str, branch_id: &str, session_id: &str) -> WorkGenesis {
    common::work_genesis(
        owner_id,
        work_id,
        branch_id,
        session_id,
        &id("intent"),
        "Deliver a verified result without losing branch history.",
    )
}

fn retention_change(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    request_id: &str,
    kind: WorkBranchRetentionKind,
    work_revision: i64,
    branch_revision: i64,
) -> WorkBranchRetentionChange {
    WorkBranchRetentionChange {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner id"),
        work_id: WorkId::parse(work_id).expect("work id"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch id"),
        request_id: WorkChangeRef::parse(request_id).expect("request id"),
        kind,
        expected_work_revision: WorkRevision::new(work_revision).expect("work revision"),
        expected_branch_revision: WorkBranchRevision::new(branch_revision)
            .expect("branch revision"),
    }
}

fn deletion_request(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    request_id: &str,
    work_revision: i64,
    branch_revision: i64,
) -> WorkBranchDeletionRequest {
    WorkBranchDeletionRequest {
        request_id: request_id.to_string(),
        owner_id: WorkOwnerId::parse(owner_id).expect("owner id"),
        work_id: WorkId::parse(work_id).expect("work id"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch id"),
        expected_work_revision: WorkRevision::new(work_revision).expect("work revision"),
        expected_branch_revision: WorkBranchRevision::new(branch_revision)
            .expect("branch revision"),
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
    .expect("add non-delivery branch fixture");
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("agent_session_execution_slots", "user_id"),
        ("session_context_operation_receipts", "owner_user_id"),
        ("session_context_authority_events", "owner_user_id"),
        ("session_context_heads", "owner_user_id"),
        ("work_branch_deletion_operations", "owner_id"),
        ("work_runtime_event_outbox", "owner_id"),
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_terminal_cuts", "owner_id"),
        ("work_item_attempts", "owner_id"),
        ("work_events", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_branches", "owner_id"),
        ("work_item_edges", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE {owner_column} = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table} for test owner: {error}"));
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_fence_waits_for_runs_and_invalidates_old_writer_authority() {
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
        .create_genesis(genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
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
    sqlx::query(
        "INSERT INTO work_branch_subjects
         (owner_id, work_id, branch_id, subject_record_revision, branch_revision,
          graph_revision, subject_ref, subject_revision, source_ref, created_at, updated_at)
         VALUES (?, ?, ?, 1, 1, 1, 'workspace:test', ?, 'test-subject', NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(format!("sha256:{}", "a".repeat(64)))
    .execute(pool.get())
    .await
    .expect("seed branch-owned subject");
    sqlx::query(
        "INSERT INTO work_proposal_sequences (owner_id, work_id, branch_id, last_proposal_seq)
         VALUES (?, ?, ?, 0)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .execute(pool.get())
    .await
    .expect("seed branch-owned proposal sequence");
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

    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "branch-deletion-test",
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
            std::time::Duration::from_secs(60),
            "branch-deletion-writer",
        )
        .await
        .expect("acquire writer")
    {
        AcquireWriterOutcome::Acquired(lease) | AcquireWriterOutcome::AlreadyAcquired(lease) => {
            lease
        }
        other => panic!("unexpected writer acquisition: {other:?}"),
    };
    let reservation = match coordinator
        .reserve_turn(
            &lease,
            None,
            std::time::Duration::from_secs(60),
            "branch-deletion-turn",
        )
        .await
        .expect("reserve turn")
    {
        ReserveTurnOutcome::Reserved(reservation)
        | ReserveTurnOutcome::AlreadyReserved(reservation) => reservation,
        other => panic!("unexpected turn reservation: {other:?}"),
    };
    let admission = deletion
        .admit(&deletion_request(
            &owner_id,
            &work_id,
            &branch_id,
            &id("delete"),
            1,
            1,
        ))
        .await
        .expect("admit deletion");
    let token = deletion
        .claim_execution(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &WorkId::parse(&work_id).unwrap(),
            &WorkBranchId::parse(&branch_id).unwrap(),
            &admission.operation.operation_id,
        )
        .await
        .expect("claim deletion")
        .expect("executor token");
    assert!(
        deletion
            .claim_execution(
                &WorkOwnerId::parse(&owner_id).unwrap(),
                &WorkId::parse(&work_id).unwrap(),
                &WorkBranchId::parse(&branch_id).unwrap(),
                &admission.operation.operation_id,
            )
            .await
            .expect("concurrent claim")
            .is_none()
    );
    sqlx::query(
        "INSERT INTO agent_session_execution_slots
         (user_id, session_id, run_id, acquired_at, updated_at)
         VALUES (?, ?, ?, NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .bind(id("run"))
    .execute(pool.get())
    .await
    .expect("add active run slot");
    assert!(matches!(
        deletion
            .fence_session(
                &WorkOwnerId::parse(&owner_id).unwrap(),
                &WorkId::parse(&work_id).unwrap(),
                &WorkBranchId::parse(&branch_id).unwrap(),
                &admission.operation.operation_id,
                &token,
            )
            .await,
        Err(WorkBranchDeletionError::ActiveRuns)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner_id)
        .bind(&session_id)
        .fetch_one(pool.get())
        .await
        .unwrap(),
        "active"
    );
    sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("settle active run");
    let fenced = deletion
        .fence_session(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &WorkId::parse(&work_id).unwrap(),
            &WorkBranchId::parse(&branch_id).unwrap(),
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("fence branch session");
    assert_eq!(
        fenced.phase,
        astra_services::work::WorkBranchDeletionPhase::SessionCleanup
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner_id)
        .bind(&session_id)
        .fetch_one(pool.get())
        .await
        .unwrap(),
        "deleting"
    );
    assert!(matches!(
        coordinator
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
                    logical_segments: vec![vec![serde_json::json!({
                        "role": "assistant",
                        "content": "late result"
                    })]],
                },
                "branch-deletion-late-commit",
            )
            .await,
        Err(SessionContextCoordinatorError::Fenced)
    ));
    let cleaned = deletion
        .cleanup_session(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &WorkId::parse(&work_id).unwrap(),
            &WorkBranchId::parse(&branch_id).unwrap(),
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("hard-delete fenced branch session");
    assert_eq!(
        cleaned.phase,
        astra_services::work::WorkBranchDeletionPhase::LineageGc
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner_id)
        .bind(&session_id)
        .fetch_one(pool.get())
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
        )
        .bind(&owner_id)
        .bind(&work_id)
        .bind(&branch_id)
        .fetch_one(pool.get())
        .await
        .unwrap(),
        1,
        "session cleanup must not make the branch disappear before GC terminal"
    );

    // Simulate a crash after hard-delete commit but before its phase receipt.
    sqlx::query(
        "UPDATE work_branch_deletion_operations SET operation_phase = 'session_cleanup'
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND operation_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&admission.operation.operation_id)
    .execute(pool.get())
    .await
    .expect("rewind operation phase to crash window");
    let resumed = deletion
        .cleanup_session(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &WorkId::parse(&work_id).unwrap(),
            &WorkBranchId::parse(&branch_id).unwrap(),
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("resume after committed session deletion");
    assert_eq!(
        resumed.phase,
        astra_services::work::WorkBranchDeletionPhase::LineageGc
    );
    let orphan_pin_id = id("orphan-pin");
    sqlx::query(
        "INSERT INTO conversation_manifest_pins
         (isolation_domain, owner_user_id, pin_id, parent_session_id,
          parent_branch_id, manifest_root, pin_state, grace_expires_at_ms,
          created_at, updated_at)
         VALUES ('server', ?, ?, ?, 'main', ?, 'active', NULL, NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&orphan_pin_id)
    .bind(&session_id)
    .bind("0".repeat(64))
    .execute(pool.get())
    .await
    .expect("insert orphan lineage pin");
    assert!(matches!(
        deletion
            .reconcile_lineage(
                &WorkOwnerId::parse(&owner_id).unwrap(),
                &WorkId::parse(&work_id).unwrap(),
                &WorkBranchId::parse(&branch_id).unwrap(),
                &admission.operation.operation_id,
                &token,
            )
            .await,
        Err(WorkBranchDeletionError::LineagePending {
            orphaned_pins: 1,
            ..
        })
    ));
    sqlx::query(
        "DELETE FROM conversation_manifest_pins
         WHERE isolation_domain = 'server' AND owner_user_id = ? AND pin_id = ?",
    )
    .bind(&owner_id)
    .bind(&orphan_pin_id)
    .execute(pool.get())
    .await
    .expect("remove orphan lineage pin");
    let reconciled = deletion
        .reconcile_lineage(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &WorkId::parse(&work_id).unwrap(),
            &WorkBranchId::parse(&branch_id).unwrap(),
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("reconcile branch lineage");
    assert_eq!(
        reconciled.phase,
        astra_services::work::WorkBranchDeletionPhase::BranchCleanup
    );
    let patch_commit_operation_id = id("patch-commit");
    sqlx::query(
        "INSERT INTO work_patch_commit_operations
         (owner_id, work_id, operation_id, request_id, request_digest,
          patch_artifact_id, source_branch_id, target_branch_id,
          target_branch_revision, target_graph_revision,
          target_subject_record_revision, subject_ref, base_subject_revision,
          result_subject_revision, payload_hash, commit_message,
          commit_author_name, commit_author_email, provider_ref,
          policy_decision_ref, operation_state, operation_phase, completed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 1, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                 'aborted', 'complete', NOW(6))",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&patch_commit_operation_id)
    .bind(id("patch-commit-request"))
    .bind("0".repeat(64))
    .bind(id("patch-artifact"))
    .bind(&branch_id)
    .bind(&branch_id)
    .bind("workspace/branch-delete")
    .bind(format!("sha256:{}", "a".repeat(64)))
    .bind(format!("sha256:{}", "b".repeat(64)))
    .bind(format!("sha256:{}", "c".repeat(64)))
    .bind("Commit reviewed changes")
    .bind("Astra Test")
    .bind("astra@example.test")
    .bind("server-git-worktree-commit-v1")
    .bind(id("patch-commit-policy"))
    .execute(pool.get())
    .await
    .expect("insert branch-owned patch commit history");
    let settled_attempt_id = id("settled-attempt");
    sqlx::query(
        "INSERT INTO work_item_attempts
         (owner_id, work_id, branch_id, work_item_id, work_item_revision,
          attempt_id, executor_run_id, execution_mode, status, graph_revision,
          outcome, summary_text, blocker_kind, unavailable_capabilities_json, settled_at)
         VALUES (?, ?, ?, ?, 1, ?, ?, 'delegated', 'completed', 1,
                 'delivered', 'branch result', NULL, '[]', NOW(6))",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(id("settled-item"))
    .bind(&settled_attempt_id)
    .bind(id("settled-run"))
    .execute(pool.get())
    .await
    .expect("insert branch-owned attempt settlement");
    sqlx::query(
        "INSERT INTO work_terminal_cuts
         (owner_id, work_id, branch_id, graph_revision, attempt_id, control_epoch)
         VALUES (?, ?, ?, 1, ?, -1)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&settled_attempt_id)
    .execute(pool.get())
    .await
    .expect("insert branch-owned terminal cut");
    let completed = deletion
        .complete_branch_cleanup(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &WorkId::parse(&work_id).unwrap(),
            &WorkBranchId::parse(&branch_id).unwrap(),
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("complete Work branch deletion");
    assert_eq!(completed.state, WorkBranchDeletionState::Succeeded);
    assert_eq!(completed.outcome, WorkBranchDeletionOutcome::Deleted);
    assert_eq!(
        completed.phase,
        astra_services::work::WorkBranchDeletionPhase::Complete
    );
    assert!(completed.completed_at.is_some());
    assert_eq!(
        deletion
            .complete_branch_cleanup(
                &WorkOwnerId::parse(&owner_id).unwrap(),
                &WorkId::parse(&work_id).unwrap(),
                &WorkBranchId::parse(&branch_id).unwrap(),
                &admission.operation.operation_id,
                "stale-token-is-irrelevant-after-terminal",
            )
            .await
            .expect("terminal deletion replay"),
        completed
    );
    let terminal = sqlx::query(
        "SELECT w.delivery_branch_id,
                (SELECT COUNT(*) FROM work_branches b
                 WHERE b.owner_id = w.owner_id AND b.work_id = w.work_id
                   AND b.branch_id = ?) AS branch_rows,
                (SELECT COUNT(*) FROM work_branch_subjects s
                 WHERE s.owner_id = w.owner_id AND s.work_id = w.work_id
                   AND s.branch_id = ?) AS subject_rows,
                (SELECT COUNT(*) FROM work_proposal_sequences p
                 WHERE p.owner_id = w.owner_id AND p.work_id = w.work_id
                   AND p.branch_id = ?) AS proposal_sequence_rows,
                (SELECT COUNT(*) FROM work_patch_commit_operations c
                 WHERE c.owner_id = w.owner_id AND c.work_id = w.work_id
                   AND ? IN (c.source_branch_id, c.target_branch_id)) AS patch_commit_rows,
                (SELECT COUNT(*) FROM work_item_attempts s
                 WHERE s.owner_id = w.owner_id AND s.work_id = w.work_id
                   AND s.branch_id = ?) AS settlement_rows,
                (SELECT COUNT(*) FROM work_terminal_cuts c
                 WHERE c.owner_id = w.owner_id AND c.work_id = w.work_id
                   AND c.branch_id = ?) AS terminal_cut_rows
         FROM works w WHERE w.owner_id = ? AND w.work_id = ?",
    )
    .bind(&branch_id)
    .bind(&branch_id)
    .bind(&branch_id)
    .bind(&branch_id)
    .bind(&branch_id)
    .bind(&branch_id)
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("load terminal Work branch deletion state");
    assert_eq!(
        terminal.try_get::<String, _>("delivery_branch_id").unwrap(),
        delivery_branch_id
    );
    assert_eq!(terminal.try_get::<i64, _>("branch_rows").unwrap(), 0);
    assert_eq!(terminal.try_get::<i64, _>("subject_rows").unwrap(), 0);
    assert_eq!(
        terminal
            .try_get::<i64, _>("proposal_sequence_rows")
            .unwrap(),
        0
    );
    assert_eq!(terminal.try_get::<i64, _>("patch_commit_rows").unwrap(), 0);
    assert_eq!(terminal.try_get::<i64, _>("settlement_rows").unwrap(), 0);
    assert_eq!(terminal.try_get::<i64, _>("terminal_cut_rows").unwrap(), 0);
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_admission_is_single_owner_replayable_and_non_destructive() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let deletion = DatabaseWorkBranchDeletionService::new(pool.clone());
    let concurrent_deletion = DatabaseWorkBranchDeletionService::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let delivery_session_id = id("session");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
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

    let request = deletion_request(&owner_id, &work_id, &branch_id, &id("delete"), 1, 1);
    let (left, right) = tokio::join!(
        deletion.admit(&request),
        concurrent_deletion.admit(&request)
    );
    let left = left.expect("first deletion admission");
    let right = right.expect("concurrent deletion replay");
    assert_eq!(left, right);
    assert_eq!(left.operation.state, WorkBranchDeletionState::Pending);
    assert_eq!(left.operation.outcome, WorkBranchDeletionOutcome::Pending);
    assert_eq!(left.operation.work_revision.get(), 2);
    assert_eq!(left.operation.branch_revision.get(), 2);
    assert_eq!(left.session_id.as_str(), session_id);

    let state = sqlx::query(
        "SELECT w.work_revision, b.branch_revision, b.session_id,
                b.deletion_operation_id, b.deletion_requested_at,
                (SELECT COUNT(*) FROM work_branch_deletion_operations d
                 WHERE d.owner_id = w.owner_id AND d.work_id = w.work_id
                   AND d.branch_id = b.branch_id) AS deletion_operations
         FROM works w JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id
         WHERE w.owner_id = ? AND w.work_id = ? AND b.branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load admitted deletion state");
    assert_eq!(state.try_get::<i64, _>("work_revision").unwrap(), 2);
    assert_eq!(state.try_get::<i64, _>("branch_revision").unwrap(), 2);
    assert_eq!(
        state.try_get::<String, _>("session_id").unwrap(),
        session_id
    );
    assert_eq!(
        state
            .try_get::<Option<String>, _>("deletion_operation_id")
            .unwrap()
            .as_deref(),
        Some(left.operation.operation_id.as_str())
    );
    assert!(
        state
            .try_get::<Option<chrono::NaiveDateTime>, _>("deletion_requested_at")
            .unwrap()
            .is_some()
    );
    assert_eq!(state.try_get::<i64, _>("deletion_operations").unwrap(), 1);
    assert!(matches!(
        repository
            .load_branch_runtime_binding(
                &WorkOwnerId::parse(&owner_id).unwrap(),
                &WorkId::parse(&work_id).unwrap(),
                &WorkBranchId::parse(&branch_id).unwrap(),
            )
            .await,
        Err(WorkRepositoryError::BranchDeleting)
    ));
    assert_eq!(
        deletion
            .load(
                &WorkOwnerId::parse(&owner_id).unwrap(),
                &WorkId::parse(&work_id).unwrap(),
                &WorkBranchId::parse(&branch_id).unwrap(),
                &left.operation.operation_id,
            )
            .await
            .expect("load deletion operation"),
        left.operation
    );
    let mut reused_identity = request.clone();
    reused_identity.expected_work_revision = WorkRevision::new(2).unwrap();
    assert!(matches!(
        deletion.admit(&reused_identity).await,
        Err(WorkBranchDeletionError::IdempotencyMismatch)
    ));
    assert!(matches!(
        deletion
            .admit(&deletion_request(
                &owner_id,
                &work_id,
                &branch_id,
                &id("delete"),
                2,
                2,
            ))
            .await,
        Err(WorkBranchDeletionError::DeletionInProgress)
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_recovery_claims_are_bounded_disjoint_and_lease_safe() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let left = DatabaseWorkBranchDeletionService::new(pool.clone());
    let right = DatabaseWorkBranchDeletionService::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let first_branch_id = id("branch");
    let second_branch_id = id("branch");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &id("delivery-session"),
        ))
        .await
        .expect("create Work");
    add_non_delivery_branch(
        &pool,
        &owner_id,
        &work_id,
        &delivery_branch_id,
        &first_branch_id,
        &id("session"),
    )
    .await;
    add_non_delivery_branch(
        &pool,
        &owner_id,
        &work_id,
        &delivery_branch_id,
        &second_branch_id,
        &id("session"),
    )
    .await;
    let first = left
        .admit(&deletion_request(
            &owner_id,
            &work_id,
            &first_branch_id,
            &id("delete"),
            1,
            1,
        ))
        .await
        .expect("admit first deletion");
    let second = left
        .admit(&deletion_request(
            &owner_id,
            &work_id,
            &second_branch_id,
            &id("delete"),
            2,
            1,
        ))
        .await
        .expect("admit second deletion");
    sqlx::query(
        "UPDATE work_branch_deletion_operations
         SET created_at = '2000-01-01 00:00:00.000000'
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("order recovery fixtures first");

    let (left_claims, right_claims) = tokio::join!(
        left.claim_pending_executions(2),
        right.claim_pending_executions(2)
    );
    let mut claims = left_claims.expect("left claims");
    claims.extend(right_claims.expect("right claims"));
    let mut claimed_operation_ids = claims
        .iter()
        .filter(|claim| claim.owner_id.as_str() == owner_id)
        .map(|claim| claim.operation.operation_id.clone())
        .collect::<Vec<_>>();
    claimed_operation_ids.sort();
    claimed_operation_ids.dedup();
    let mut expected_operation_ids = vec![
        first.operation.operation_id.clone(),
        second.operation.operation_id.clone(),
    ];
    expected_operation_ids.sort();
    assert_eq!(claimed_operation_ids, expected_operation_ids);
    assert_eq!(
        claims
            .iter()
            .filter(|claim| claim.owner_id.as_str() == owner_id)
            .count(),
        2,
        "concurrent workers must never claim one operation twice"
    );

    let leased = left
        .claim_pending_executions(2)
        .await
        .expect("scan while leased");
    assert!(
        leased
            .iter()
            .all(|claim| claim.owner_id.as_str() != owner_id),
        "unexpired executor leases must exclude already claimed operations"
    );
    for claim in claims
        .iter()
        .filter(|claim| claim.owner_id.as_str() == owner_id)
    {
        left.release_execution(
            &claim.owner_id,
            &claim.work_id,
            &claim.branch_id,
            &claim.operation.operation_id,
            &claim.executor_token,
        )
        .await
        .expect("release recovery claim");
    }
    let bounded = left
        .claim_pending_executions(1)
        .await
        .expect("bounded recovery claim");
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].owner_id.as_str(), owner_id);
    assert!(matches!(
        left.claim_pending_executions(0).await,
        Err(WorkBranchDeletionError::Invalid(_))
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_guard_and_conflicts_do_not_claim_or_remove_the_branch() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let deletion = DatabaseWorkBranchDeletionService::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let delivery_session_id = id("session");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
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

    let protected = deletion
        .admit(&deletion_request(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &id("delete"),
            1,
            1,
        ))
        .await
        .expect("delivery guard receipt");
    assert_eq!(protected.operation.state, WorkBranchDeletionState::Conflict);
    assert_eq!(
        protected.operation.outcome,
        WorkBranchDeletionOutcome::DeliveryBranchProtected
    );
    let stale = deletion
        .admit(&deletion_request(
            &owner_id,
            &work_id,
            &branch_id,
            &id("delete"),
            2,
            1,
        ))
        .await
        .expect("stale basis receipt");
    assert_eq!(stale.operation.state, WorkBranchDeletionState::Conflict);
    assert_eq!(
        stale.operation.outcome,
        WorkBranchDeletionOutcome::WorkRevisionConflict
    );
    assert!(matches!(
        deletion
            .admit(&deletion_request(
                &other_owner_id,
                &work_id,
                &branch_id,
                &id("delete"),
                1,
                1,
            ))
            .await,
        Err(WorkBranchDeletionError::NotFound)
    ));
    let branch = sqlx::query(
        "SELECT w.work_revision, b.branch_revision, b.deletion_operation_id, b.session_id
         FROM works w JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id
         WHERE w.owner_id = ? AND w.work_id = ? AND b.branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load unclaimed branch");
    assert_eq!(branch.try_get::<i64, _>("work_revision").unwrap(), 1);
    assert_eq!(branch.try_get::<i64, _>("branch_revision").unwrap(), 1);
    assert!(
        branch
            .try_get::<Option<String>, _>("deletion_operation_id")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        branch.try_get::<String, _>("session_id").unwrap(),
        session_id
    );
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn archive_restore_is_atomic_replayable_and_runtime_visible() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let concurrent_repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let delivery_session_id = id("session");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    repository
        .create_genesis(genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
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

    let archive_request_id = id("archive");
    let archive = retention_change(
        &owner_id,
        &work_id,
        &branch_id,
        &archive_request_id,
        WorkBranchRetentionKind::Archive,
        1,
        1,
    );
    let (left, right) = tokio::join!(
        repository.change_branch_retention(archive.clone()),
        concurrent_repository.change_branch_retention(archive.clone()),
    );
    let left = left.expect("first concurrent archive");
    let right = right.expect("replayed concurrent archive");
    assert_eq!(left, right);
    assert_eq!(left.outcome, WorkBranchRetentionOutcome::Applied);
    assert_eq!(left.work_revision.get(), 2);
    assert_eq!(left.branch_revision.get(), 2);
    assert!(matches!(
        repository
            .load_branch_runtime_binding(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                &WorkBranchId::parse(&branch_id).expect("branch"),
            )
            .await,
        Err(WorkRepositoryError::NotFound)
    ));
    let archive_event_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS count FROM work_events
         WHERE owner_id = ? AND work_id = ? AND source_ref = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&archive_request_id)
    .fetch_one(pool.get())
    .await
    .expect("count archive events")
    .try_get("count")
    .expect("event count");
    assert_eq!(archive_event_count, 1);
    let archive_catalog = DatabaseWorkBranchCatalogService::new(pool.clone());
    let archived_page = archive_catalog
        .load_archived(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
            None,
            1,
        )
        .await
        .expect("load archived branch page");
    assert_eq!(archived_page.work_revision.get(), 2);
    assert_eq!(archived_page.branches.len(), 1);
    assert_eq!(archived_page.branches[0].branch_id.as_str(), branch_id);
    assert!(archived_page.next_cursor.is_none());
    assert!(matches!(
        archive_catalog
            .load_archived(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                None,
                0,
            )
            .await,
        Err(WorkBranchCatalogError::InvalidQuery)
    ));

    let restore = retention_change(
        &owner_id,
        &work_id,
        &branch_id,
        &id("restore"),
        WorkBranchRetentionKind::Restore,
        2,
        2,
    );
    let restored = repository
        .change_branch_retention(restore)
        .await
        .expect("restore branch");
    assert_eq!(restored.outcome, WorkBranchRetentionOutcome::Applied);
    assert_eq!(restored.work_revision.get(), 3);
    assert_eq!(restored.branch_revision.get(), 3);
    let binding = repository
        .load_branch_runtime_binding(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(&work_id).expect("work"),
            &WorkBranchId::parse(&branch_id).expect("branch"),
        )
        .await
        .expect("restored runtime binding");
    assert_eq!(binding.session_id.as_str(), session_id);
    assert!(
        archive_catalog
            .load_archived(
                &WorkOwnerId::parse(&owner_id).expect("owner"),
                &WorkId::parse(&work_id).expect("work"),
                None,
                20,
            )
            .await
            .expect("empty archived branch page")
            .branches
            .is_empty()
    );

    let already_active = repository
        .change_branch_retention(retention_change(
            &owner_id,
            &work_id,
            &branch_id,
            &id("restore"),
            WorkBranchRetentionKind::Restore,
            3,
            3,
        ))
        .await
        .expect("idempotent state convergence");
    assert_eq!(
        already_active.outcome,
        WorkBranchRetentionOutcome::AlreadyInState
    );
    assert_eq!(already_active.work_revision.get(), 3);
    assert_eq!(already_active.branch_revision.get(), 3);

    let reused_request = retention_change(
        &owner_id,
        &work_id,
        &branch_id,
        &archive_request_id,
        WorkBranchRetentionKind::Restore,
        3,
        3,
    );
    assert!(matches!(
        repository.change_branch_retention(reused_request).await,
        Err(WorkRepositoryError::StaleBranchRetention {
            resource: WorkBranchRetentionBasisResource::RequestPayload
        })
    ));
    cleanup_owner(&pool, &owner_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn archive_rejects_delivery_active_stale_and_foreign_branches_without_mutation() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let delivery_session_id = id("session");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    repository
        .create_genesis(genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
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

    let delivery_archive = retention_change(
        &owner_id,
        &work_id,
        &delivery_branch_id,
        &id("archive"),
        WorkBranchRetentionKind::Archive,
        1,
        1,
    );
    assert!(matches!(
        repository.change_branch_retention(delivery_archive).await,
        Err(WorkRepositoryError::DeliveryBranchProtected)
    ));

    sqlx::query(
        "INSERT INTO agent_session_execution_slots
         (user_id, session_id, run_id, acquired_at, updated_at)
         VALUES (?, ?, ?, NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .bind(id("run"))
    .execute(pool.get())
    .await
    .expect("own active branch session");
    let active_archive = retention_change(
        &owner_id,
        &work_id,
        &branch_id,
        &id("archive"),
        WorkBranchRetentionKind::Archive,
        1,
        1,
    );
    assert!(matches!(
        repository.change_branch_retention(active_archive).await,
        Err(WorkRepositoryError::BranchActive)
    ));
    sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("release branch session");

    let stale_archive = retention_change(
        &owner_id,
        &work_id,
        &branch_id,
        &id("archive"),
        WorkBranchRetentionKind::Archive,
        2,
        1,
    );
    assert!(matches!(
        repository.change_branch_retention(stale_archive).await,
        Err(WorkRepositoryError::StaleBranchRetention {
            resource: WorkBranchRetentionBasisResource::WorkRevision
        })
    ));
    let foreign_archive = retention_change(
        &other_owner_id,
        &work_id,
        &branch_id,
        &id("archive"),
        WorkBranchRetentionKind::Archive,
        1,
        1,
    );
    assert!(matches!(
        repository.change_branch_retention(foreign_archive).await,
        Err(WorkRepositoryError::NotFound)
    ));

    let state = sqlx::query(
        "SELECT w.work_revision, b.branch_revision, b.archived_at,
                (SELECT COUNT(*) FROM work_events e
                 WHERE e.owner_id = w.owner_id AND e.work_id = w.work_id
                   AND e.event_kind IN ('branch_archived', 'branch_restored')) AS retention_events
         FROM works w JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id
         WHERE w.owner_id = ? AND w.work_id = ? AND b.branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load unchanged retention state");
    assert_eq!(
        state
            .try_get::<i64, _>("work_revision")
            .expect("work revision"),
        1
    );
    assert_eq!(
        state
            .try_get::<i64, _>("branch_revision")
            .expect("branch revision"),
        1
    );
    assert!(
        state
            .try_get::<Option<chrono::NaiveDateTime>, _>("archived_at")
            .expect("archived at")
            .is_none()
    );
    assert_eq!(
        state.try_get::<i64, _>("retention_events").expect("events"),
        0
    );
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}
