mod common;

use astra_services::{
    runs::{
        DatabaseRunStateStore, DurableRunRecord, DurableWorkItemRunBinding, DurableWorkRunBinding,
        RunStateStore,
    },
    work::{
        DatabaseWorkAttemptSettlementService, DatabaseWorkRepository, GraphRevision,
        NewWorkAttemptSettlement, NewWorkItem, NewWorkItemAttempt, PrimaryWorkAttemptAdvance,
        PrimaryWorkAttemptCarrierState, WorkAttemptBlockerKind, WorkAttemptExecutionMode,
        WorkAttemptOutcome, WorkAttemptSettlementError, WorkBranchId, WorkBranchRevision,
        WorkChangeRef, WorkGraphChange, WorkGraphItemChange, WorkId, WorkItemAttemptId,
        WorkItemDeliveryStatus, WorkItemId, WorkItemKind, WorkItemRevision, WorkItemRevisionRef,
        WorkItemText, WorkOwnerId, WorkRepository, WorkTaskGraphQuery,
    },
};
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn task(item_id: &str) -> WorkGraphItemChange {
    WorkGraphItemChange::New(NewWorkItem {
        item_id: WorkItemId::parse(item_id).expect("item id"),
        kind: WorkItemKind::Task,
        objective: WorkItemText::parse(format!("Execute {item_id}")).expect("objective"),
        expected_result: WorkItemText::parse(format!("Verify {item_id}")).expect("result"),
    })
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn one_primary_run_executes_multiple_attempts_without_child_run_identity_aliasing() {
    let pool = common::setup_pool().await;
    let owner_id = id("primary-attempt-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let run_id = id("root-run");
    let task_a = id("task-a");
    let task_b = id("task-b");
    let repository = DatabaseWorkRepository::new(pool.clone());
    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Execute two tasks in one primary session.",
        ))
        .await
        .expect("create Work");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![task(&task_a), task(&task_b)],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).unwrap(),
            reason: None,
        })
        .await
        .expect("replace graph");
    let mut root_run = item_run(&owner_id, &run_id, &session_id, &work_id, &branch_id);
    root_run.work_binding = None;
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(root_run)
        .await
        .expect("insert root executor run");

    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    let first_attempt_id = id("attempt-0");
    let second_attempt_id = WorkItemAttemptId::parse(id("attempt-1")).unwrap();
    service
        .begin_attempt(NewWorkItemAttempt {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            session_id: session_id.clone(),
            item: WorkItemRevisionRef {
                item_id: WorkItemId::parse(&task_a).unwrap(),
                revision: WorkItemRevision::INITIAL,
            },
            graph_revision: GraphRevision::new(2).unwrap(),
            attempt_id: WorkItemAttemptId::parse(&first_attempt_id).unwrap(),
            executor_run_id: run_id.clone(),
            execution_mode: WorkAttemptExecutionMode::Primary,
        })
        .await
        .expect("begin first primary attempt");

    let first_settlement = NewWorkAttemptSettlement {
        outcome: WorkAttemptOutcome::Delivered,
        summary: format!("{task_a} delivered"),
        blocker_kind: None,
        unavailable_capabilities: Vec::new(),
    };
    let advanced = service
        .record_and_advance_primary(
            &owner_id,
            &first_attempt_id,
            &run_id,
            -1,
            first_settlement.clone(),
            second_attempt_id.clone(),
        )
        .await
        .expect("settle first and atomically begin second");
    assert!(matches!(
        &advanced.advance,
        PrimaryWorkAttemptAdvance::Assigned {
            attempt_id,
            item_id,
            resumed: false,
            ..
        } if attempt_id == &second_attempt_id && item_id.as_str() == task_b
    ));
    let nonterminal_cuts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_terminal_cuts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("count nonterminal cuts");
    assert_eq!(
        nonterminal_cuts, 0,
        "a delivered nonterminal task must not publish a terminal cut"
    );

    let replay = service
        .record_and_advance_primary(
            &owner_id,
            &first_attempt_id,
            &run_id,
            -1,
            first_settlement,
            second_attempt_id.clone(),
        )
        .await
        .expect("lost-response replay returns the same successor");
    assert!(matches!(
        replay.advance,
        PrimaryWorkAttemptAdvance::Assigned {
            attempt_id,
            resumed: true,
            ..
        } if attempt_id == second_attempt_id
    ));
    let final_settlement = NewWorkAttemptSettlement {
        outcome: WorkAttemptOutcome::Delivered,
        summary: format!("{task_b} delivered"),
        blocker_kind: None,
        unavailable_capabilities: Vec::new(),
    };

    let terminal = service
        .record_and_advance_primary(
            &owner_id,
            second_attempt_id.as_str(),
            &run_id,
            7,
            final_settlement.clone(),
            WorkItemAttemptId::parse(id("unused-successor")).unwrap(),
        )
        .await
        .expect("settle second primary attempt");
    assert_eq!(terminal.advance, PrimaryWorkAttemptAdvance::Complete);
    let terminal_cut: (i64, String, i64) = sqlx::query_as(
        "SELECT graph_revision, attempt_id, control_epoch FROM work_terminal_cuts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND graph_revision = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(2_i64)
    .fetch_one(pool.get())
    .await
    .expect("load terminal graph cut");
    assert_eq!(terminal_cut, (2, second_attempt_id.as_str().to_owned(), 7));
    let replay = service
        .record_and_advance_primary(
            &owner_id,
            second_attempt_id.as_str(),
            &run_id,
            7,
            final_settlement.clone(),
            WorkItemAttemptId::parse(id("unused-replay-successor")).unwrap(),
        )
        .await
        .expect("exact lost-response replay");
    assert_eq!(replay.advance, PrimaryWorkAttemptAdvance::Complete);

    let replay_with_different_epoch = service
        .record_and_advance_primary(
            &owner_id,
            second_attempt_id.as_str(),
            &run_id,
            99,
            final_settlement.clone(),
            WorkItemAttemptId::parse(id("unused-epoch-successor")).unwrap(),
        )
        .await;
    assert!(
        matches!(
            replay_with_different_epoch,
            Err(WorkAttemptSettlementError::Conflict)
        ),
        "a replay with different trusted intent epoch must conflict"
    );
    let replayed_cut: (String, i64) = sqlx::query_as(
        "SELECT attempt_id, control_epoch FROM work_terminal_cuts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND graph_revision = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(2_i64)
    .fetch_one(pool.get())
    .await
    .expect("load idempotently retained terminal cut");
    assert_eq!(
        replayed_cut,
        (second_attempt_id.as_str().to_owned(), 7),
        "idempotent replay must never rewrite the trusted settlement epoch"
    );

    let duplicate_graph_cut = sqlx::query(
        "INSERT INTO work_terminal_cuts
         (owner_id, work_id, branch_id, graph_revision, attempt_id, control_epoch)
         VALUES (?, ?, ?, 2, ?, 88)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(id("duplicate-terminal-cut"))
    .execute(pool.get())
    .await;
    assert!(
        duplicate_graph_cut.is_err(),
        "the same terminal graph cut with a different epoch must violate uniqueness"
    );
    let duplicate_attempt_cut = sqlx::query(
        "INSERT INTO work_terminal_cuts
         (owner_id, work_id, branch_id, graph_revision, attempt_id, control_epoch)
         VALUES (?, ?, ?, 3, ?, 7)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(second_attempt_id.as_str())
    .execute(pool.get())
    .await;
    assert!(
        duplicate_attempt_cut.is_err(),
        "one immutable attempt must not publish a second graph cut"
    );

    // A replay reads the existing settlement and must never manufacture a
    // missing terminal cut. Recovery authority comes only from the first
    // branch-locked Delivered -> Complete transition.
    sqlx::query("DELETE FROM work_terminal_cuts WHERE owner_id = ? AND attempt_id = ?")
        .bind(&owner_id)
        .bind(second_attempt_id.as_str())
        .execute(pool.get())
        .await
        .expect("simulate a settlement without terminal-cut authority");
    let replay_without_cut = service
        .record_and_advance_primary(
            &owner_id,
            second_attempt_id.as_str(),
            &run_id,
            7,
            final_settlement,
            WorkItemAttemptId::parse(id("unused-replay-successor")).unwrap(),
        )
        .await;
    assert!(matches!(
        replay_without_cut,
        Err(WorkAttemptSettlementError::Conflict)
    ));
    let replay_cut: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_terminal_cuts
         WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(&owner_id)
    .bind(second_attempt_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("reload terminal graph cut after replay");
    assert_eq!(
        replay_cut, 0,
        "idempotent replay must not backfill authority"
    );

    sqlx::query(
        "INSERT INTO work_terminal_cuts
         (owner_id, work_id, branch_id, graph_revision, attempt_id, control_epoch)
         VALUES (?, ?, ?, 2, ?, -2)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(second_attempt_id.as_str())
    .execute(pool.get())
    .await
    .expect("inject corrupt terminal cut below the trusted epoch floor");
    let corrupt_snapshot = repository
        .load_task_execution_snapshot_for_session(
            &WorkOwnerId::parse(&owner_id).unwrap(),
            &astra_services::work::InternalSessionId::parse(&session_id).unwrap(),
        )
        .await;
    assert!(
        corrupt_snapshot.is_err(),
        "corrupt terminal authority must fail closed during durable validation"
    );

    let attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_item_attempts
         WHERE owner_id = ? AND executor_run_id = ? AND execution_mode = 'primary'
           AND status = 'completed' AND outcome = 'delivered'",
    )
    .bind(&owner_id)
    .bind(&run_id)
    .fetch_one(pool.get())
    .await
    .expect("count primary attempts");
    assert_eq!(
        attempts, 2,
        "one root Run must carry multiple task attempts"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn competing_terminal_cut_rolls_back_the_attempt_settlement() {
    let pool = common::setup_pool().await;
    let owner_id = id("terminal-conflict-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let run_id = id("root-run");
    let task_id = id("task");
    let attempt_id = id("attempt");
    let repository = DatabaseWorkRepository::new(pool.clone());
    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Prove terminal-cut conflicts roll back settlement.",
        ))
        .await
        .expect("create Work");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![task(&task_id)],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).unwrap(),
            reason: None,
        })
        .await
        .expect("replace graph");
    let mut root_run = item_run(&owner_id, &run_id, &session_id, &work_id, &branch_id);
    root_run.work_binding = None;
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(root_run)
        .await
        .expect("insert root executor run");
    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    service
        .begin_attempt(NewWorkItemAttempt {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            session_id: session_id.clone(),
            item: WorkItemRevisionRef {
                item_id: WorkItemId::parse(&task_id).unwrap(),
                revision: WorkItemRevision::INITIAL,
            },
            graph_revision: GraphRevision::new(2).unwrap(),
            attempt_id: WorkItemAttemptId::parse(&attempt_id).unwrap(),
            executor_run_id: run_id.clone(),
            execution_mode: WorkAttemptExecutionMode::Primary,
        })
        .await
        .expect("begin primary attempt");
    let competing_attempt_id = id("competing-attempt");
    sqlx::query(
        "INSERT INTO work_terminal_cuts
         (owner_id, work_id, branch_id, graph_revision, attempt_id, control_epoch)
         VALUES (?, ?, ?, 2, ?, 41)",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&competing_attempt_id)
    .execute(pool.get())
    .await
    .expect("publish competing terminal cut fixture");

    let conflict = service
        .record_and_advance_primary(
            &owner_id,
            &attempt_id,
            &run_id,
            7,
            NewWorkAttemptSettlement {
                outcome: WorkAttemptOutcome::Delivered,
                summary: "terminal task delivered".into(),
                blocker_kind: None,
                unavailable_capabilities: Vec::new(),
            },
            WorkItemAttemptId::parse(id("unused-successor")).unwrap(),
        )
        .await;
    assert!(matches!(
        conflict,
        Err(WorkAttemptSettlementError::Conflict)
    ));
    let attempt: (String, Option<String>) = sqlx::query_as(
        "SELECT status, outcome FROM work_item_attempts
         WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(&owner_id)
    .bind(&attempt_id)
    .fetch_one(pool.get())
    .await
    .expect("load rolled-back attempt");
    assert_eq!(
        attempt,
        ("running".into(), None),
        "terminal-cut conflict must roll back the settlement update"
    );
    let retained_cut: (String, i64) = sqlx::query_as(
        "SELECT attempt_id, control_epoch FROM work_terminal_cuts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ? AND graph_revision = 2",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load retained competing cut");
    assert_eq!(retained_cut, (competing_attempt_id, 41));
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn blocked_primary_settlement_does_not_start_a_successor() {
    let pool = common::setup_pool().await;
    let owner_id = id("blocked-primary-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let run_id = id("root-run");
    let task_a = id("task-a");
    let task_b = id("task-b");
    let repository = DatabaseWorkRepository::new(pool.clone());
    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Do not advance past a blocked task.",
        ))
        .await
        .expect("create Work");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![task(&task_a), task(&task_b)],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).unwrap(),
            reason: None,
        })
        .await
        .expect("replace graph");
    let mut root_run = item_run(&owner_id, &run_id, &session_id, &work_id, &branch_id);
    root_run.work_binding = None;
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(root_run)
        .await
        .expect("insert root executor run");

    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    let active_attempt_id = id("active-attempt");
    service
        .begin_attempt(NewWorkItemAttempt {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            session_id: session_id.clone(),
            item: WorkItemRevisionRef {
                item_id: WorkItemId::parse(&task_a).unwrap(),
                revision: WorkItemRevision::INITIAL,
            },
            graph_revision: GraphRevision::new(2).unwrap(),
            attempt_id: WorkItemAttemptId::parse(&active_attempt_id).unwrap(),
            executor_run_id: run_id.clone(),
            execution_mode: WorkAttemptExecutionMode::Primary,
        })
        .await
        .expect("begin primary attempt");
    let result = service
        .record_and_advance_primary(
            &owner_id,
            &active_attempt_id,
            &run_id,
            -1,
            NewWorkAttemptSettlement {
                outcome: WorkAttemptOutcome::Blocked,
                summary: "Required dependency is unavailable".into(),
                blocker_kind: Some(WorkAttemptBlockerKind::DependencyBlocked),
                unavailable_capabilities: Vec::new(),
            },
            WorkItemAttemptId::parse(id("must-not-start")).unwrap(),
        )
        .await
        .expect("record blocked settlement");
    assert_eq!(result.advance, PrimaryWorkAttemptAdvance::NeedsRecovery);
    let attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_item_attempts WHERE owner_id = ? AND executor_run_id = ?",
    )
    .bind(&owner_id)
    .bind(&run_id)
    .fetch_one(pool.get())
    .await
    .expect("count attempts");
    assert_eq!(attempts, 1, "blocked delivery must not start another task");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn paused_primary_attempt_takeover_requires_terminal_old_run_and_same_session() {
    let pool = common::setup_pool().await;
    let owner_id = id("takeover-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let old_run_id = id("old-run");
    let new_run_id = id("new-run");
    let other_session_run_id = id("other-session-run");
    let other_session_id = id("other-session");
    let task_id = id("task");
    let attempt_id = id("attempt");
    let repository = DatabaseWorkRepository::new(pool.clone());
    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Resume one paused primary task without changing its identity.",
        ))
        .await
        .expect("create Work");
    repository
        .replace_graph(WorkGraphChange {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            items: vec![task(&task_id)],
            edges: Vec::new(),
            source_ref: WorkChangeRef::parse(id("graph-change")).unwrap(),
            reason: None,
        })
        .await
        .expect("replace graph");
    let run_store = DatabaseRunStateStore::new(pool.clone());
    run_store
        .insert_run(item_run(
            &owner_id,
            &old_run_id,
            &session_id,
            &work_id,
            &branch_id,
        ))
        .await
        .expect("insert old run");
    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    service
        .begin_attempt(NewWorkItemAttempt {
            owner_id: WorkOwnerId::parse(&owner_id).unwrap(),
            work_id: WorkId::parse(&work_id).unwrap(),
            branch_id: WorkBranchId::parse(&branch_id).unwrap(),
            session_id: session_id.clone(),
            item: WorkItemRevisionRef {
                item_id: WorkItemId::parse(&task_id).unwrap(),
                revision: WorkItemRevision::INITIAL,
            },
            graph_revision: GraphRevision::new(2).unwrap(),
            attempt_id: WorkItemAttemptId::parse(&attempt_id).unwrap(),
            executor_run_id: old_run_id.clone(),
            execution_mode: WorkAttemptExecutionMode::Primary,
        })
        .await
        .expect("begin primary attempt");
    assert!(
        service
            .transition_primary_carriers_for_run(
                &owner_id,
                &old_run_id,
                PrimaryWorkAttemptCarrierState::Paused,
            )
            .await
            .expect("pause attempt carrier")
    );

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
         VALUES (?, ?, 'takeover isolation fixture', 'active', 0)",
    )
    .bind(&other_session_id)
    .bind(&owner_id)
    .execute(pool.get())
    .await
    .expect("create production-valid other session fixture");
    run_store
        .insert_run(item_run(
            &owner_id,
            &other_session_run_id,
            &other_session_id,
            &work_id,
            &branch_id,
        ))
        .await
        .expect("insert other-session run");
    assert!(
        matches!(
            service
                .take_over_paused_primary_attempt(&owner_id, &attempt_id, &other_session_run_id)
                .await,
            Err(WorkAttemptSettlementError::UnboundRun)
        ),
        "a run from another session must not acquire the attempt"
    );

    run_store
        .insert_run(item_run(
            &owner_id,
            &new_run_id,
            &session_id,
            &work_id,
            &branch_id,
        ))
        .await
        .expect("insert continuation run");
    assert!(
        matches!(
            service
                .take_over_paused_primary_attempt(&owner_id, &attempt_id, &new_run_id)
                .await,
            Err(WorkAttemptSettlementError::ActivePrimaryAttempt)
        ),
        "a still-live old run must retain exclusive ownership"
    );
    sqlx::query("UPDATE agent_runs SET status = 'failed' WHERE user_id = ? AND run_id = ?")
        .bind(&owner_id)
        .bind(&old_run_id)
        .execute(pool.get())
        .await
        .expect("terminate old run fixture");
    assert!(
        service
            .take_over_paused_primary_attempt(&owner_id, &attempt_id, &new_run_id)
            .await
            .expect("take over paused attempt")
    );
    let carrier: (String, String) = sqlx::query_as(
        "SELECT executor_run_id, status FROM work_item_attempts
         WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(&owner_id)
    .bind(&attempt_id)
    .fetch_one(pool.get())
    .await
    .expect("load transferred carrier");
    assert_eq!(carrier, (new_run_id, "running".to_string()));
}

fn item_run(
    owner_id: &str,
    run_id: &str,
    session_id: &str,
    work_id: &str,
    branch_id: &str,
) -> DurableRunRecord {
    let binding = DurableWorkRunBinding::new(
        WorkId::parse(work_id).expect("Work id"),
        WorkBranchId::parse(branch_id).expect("branch id"),
        GraphRevision::INITIAL,
    )
    .with_item(DurableWorkItemRunBinding::new(
        WorkItemId::root(),
        WorkItemRevision::INITIAL,
        WorkItemAttemptId::parse(run_id).expect("attempt id"),
    ));
    DurableRunRecord {
        run_id: run_id.into(),
        user_id: owner_id.into(),
        session_id: session_id.into(),
        parent_run_id: None,
        root_run_id: Some(run_id.into()),
        ancestor_path: Some(run_id.into()),
        depth: 0,
        delegation_id: None,
        agent_id: Some(id("agent")),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 0,
        last_event_idx: -1,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        agent_binding_id: None,
        agent_binding_name: None,
        agent_binding_schema_version: None,
        model_offering_id: None,
        resolved_model_name: None,
        capability_server_refs_json: None,
        runtime_profile: None,
        start_request_fingerprint: None,
        work_binding: Some(binding),
        events: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn settlement_is_exact_owner_scoped_immutable_and_idempotent() {
    let pool = common::setup_pool().await;
    let owner_id = id("settlement-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let run_id = id("run");
    DatabaseWorkRepository::new(pool.clone())
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Prove exact WorkItem attempt settlement.",
        ))
        .await
        .expect("create Work");
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(item_run(
            &owner_id,
            &run_id,
            &session_id,
            &work_id,
            &branch_id,
        ))
        .await
        .expect("insert item run");
    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    let blocked = NewWorkAttemptSettlement {
        outcome: WorkAttemptOutcome::Blocked,
        summary: "Required network fetch capability is unavailable".into(),
        blocker_kind: Some(WorkAttemptBlockerKind::CapabilityUnavailable),
        unavailable_capabilities: vec!["web_fetch".into()],
    };

    let first = service
        .record_for_run(&owner_id, &run_id, blocked.clone())
        .await
        .expect("record settlement");
    let replay = service
        .record_for_run(&owner_id, &run_id, blocked)
        .await
        .expect("idempotent replay");
    assert_eq!(first, replay);
    assert_eq!(first.attempt_id, run_id);
    assert_eq!(first.unavailable_capabilities, ["web_fetch"]);
    let graph = DatabaseWorkRepository::new(pool.clone())
        .load_task_graph_page(
            WorkTaskGraphQuery::new(
                WorkOwnerId::parse(&owner_id).expect("owner"),
                WorkId::parse(&work_id).expect("Work"),
                WorkBranchId::parse(&branch_id).expect("branch"),
                None,
                0,
                1,
                0,
                1,
            )
            .expect("graph query"),
        )
        .await
        .expect("project settlement");
    assert_eq!(
        graph.items().entries[0].delivery.status,
        WorkItemDeliveryStatus::Blocked
    );
    assert_eq!(
        graph.items().entries[0].delivery.unavailable_capabilities,
        ["web_fetch"]
    );

    let changed = service
        .record_for_run(
            &owner_id,
            &run_id,
            NewWorkAttemptSettlement {
                outcome: WorkAttemptOutcome::Delivered,
                summary: "pretend success".into(),
                blocker_kind: None,
                unavailable_capabilities: Vec::new(),
            },
        )
        .await;
    assert!(matches!(changed, Err(WorkAttemptSettlementError::Conflict)));
    assert!(matches!(
        service
            .record_for_run(
                &id("other-owner"),
                &run_id,
                NewWorkAttemptSettlement {
                    outcome: WorkAttemptOutcome::Failed,
                    summary: "cannot see another owner's run".into(),
                    blocker_kind: None,
                    unavailable_capabilities: Vec::new(),
                }
            )
            .await,
        Err(WorkAttemptSettlementError::UnboundRun)
    ));
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn terminal_fallback_settlement_is_atomic_and_never_overwrites_an_explicit_outcome() {
    let pool = common::setup_pool().await;
    let owner_id = id("terminal-settlement-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    let explicit_run_id = id("explicit-run");
    let fallback_run_id = id("fallback-run");
    DatabaseWorkRepository::new(pool.clone())
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Prove terminal WorkItem delivery reconciliation.",
        ))
        .await
        .expect("create Work");
    let run_store = DatabaseRunStateStore::new(pool.clone());
    for run_id in [&explicit_run_id, &fallback_run_id] {
        run_store
            .insert_run(item_run(
                &owner_id,
                run_id,
                &session_id,
                &work_id,
                &branch_id,
            ))
            .await
            .expect("insert item run");
    }
    let service = DatabaseWorkAttemptSettlementService::new(pool.clone());
    let delivered = NewWorkAttemptSettlement {
        outcome: WorkAttemptOutcome::Delivered,
        summary: "The assigned result was delivered".into(),
        blocker_kind: None,
        unavailable_capabilities: Vec::new(),
    };
    service
        .record_for_run(&owner_id, &explicit_run_id, delivered.clone())
        .await
        .expect("explicit settlement while the run is active");

    run_store
        .update_run_status(
            &owner_id,
            &session_id,
            &explicit_run_id,
            "completed",
            None,
            None,
        )
        .await
        .expect("finish explicit run through the durable store");
    let fallback = NewWorkAttemptSettlement {
        outcome: WorkAttemptOutcome::Failed,
        summary: "Worker ended without reporting a delivery outcome".into(),
        blocker_kind: None,
        unavailable_capabilities: Vec::new(),
    };
    assert!(
        !service
            .record_if_unsettled_for_terminal_run(&owner_id, &explicit_run_id, fallback.clone())
            .await
            .expect("existing explicit settlement wins"),
        "runtime fallback must not overwrite a truthful explicit settlement"
    );

    assert!(matches!(
        service
            .record_if_unsettled_for_terminal_run(&owner_id, &fallback_run_id, fallback.clone())
            .await,
        Err(WorkAttemptSettlementError::RunNotTerminal)
    ));
    run_store
        .update_run_status(
            &owner_id,
            &session_id,
            &fallback_run_id,
            "failed",
            None,
            None,
        )
        .await
        .expect("finish fallback run through the durable store");
    assert!(
        service
            .record_if_unsettled_for_terminal_run(&owner_id, &fallback_run_id, fallback.clone())
            .await
            .expect("write terminal fallback")
    );
    assert!(
        !service
            .record_if_unsettled_for_terminal_run(&owner_id, &fallback_run_id, delivered)
            .await
            .expect("terminal fallback is idempotently preserved")
    );
    assert!(matches!(
        service
            .record_for_run(
                &owner_id,
                &fallback_run_id,
                NewWorkAttemptSettlement {
                    outcome: WorkAttemptOutcome::Delivered,
                    summary: "conflicting replacement".into(),
                    blocker_kind: None,
                    unavailable_capabilities: Vec::new(),
                },
            )
            .await,
        Err(WorkAttemptSettlementError::Conflict)
    ));
}
