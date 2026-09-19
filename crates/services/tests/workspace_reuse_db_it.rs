mod common;

use std::time::Duration;

use astra_core::SharedPool;
use astra_services::{
    AcquireWriterAndReserveTurnOutcome, DatabaseSessionContextCoordinator,
    SessionContextCoordinator, SessionContextCoordinatorError, SessionExecutionBindingStateV1,
    SessionExecutionBindingV1,
    runs::{DatabaseRunStateStore, DurableRunRecord, RunStateStore},
    session_context_coordinator::WorkspaceReuseBlocker,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, SessionKeyV1, SessionSurfaceV1,
};
use uuid::Uuid;

fn binding(session: &str) -> SessionExecutionBindingV1 {
    use astra_services::runs::*;
    SessionExecutionBindingV1 {
        schema_version: astra_services::SESSION_EXECUTION_BINDING_SCHEMA_VERSION,
        generation: 1,
        state: SessionExecutionBindingStateV1::Ready,
        logical_workspace_id: format!("session:{session}"),
        physical_workspace_id: Some(
            SessionExecutionBindingV1::edge_materialization_physical_identity(
                "reuse-test-device",
                "/workspace/shared",
            ),
        ),
        workspace: WorkspaceBindingRequest {
            kind: WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: None,
            root: Some("/workspace/shared".into()),
            source: Some(WorkspaceSourceRequest::EdgePath {
                path: "/workspace/shared".into(),
            }),
            authority: Some(WorkspaceAuthorityRequest::ReadWrite),
        },
        executor: ExecutorBindingRequest {
            kind: ExecutorBindingRequestKind::EdgeAgent,
            executor_id: Some("reuse-test-edge".into()),
            display_name: None,
            transport: Some(ToolTransportKindRequest::EdgeLedger),
            status: Some(ExecutorStatusRequest::Online),
        },
    }
}

async fn fixture() -> (
    SharedPool,
    DatabaseSessionContextCoordinator,
    Vec<SessionKeyV1>,
) {
    let pool = common::setup_pool().await;
    let owner = format!("reuse-{}", Uuid::new_v4());
    let mut keys = Vec::new();
    for name in ["old", "new", "parallel"] {
        let session = format!("{name}-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status, event_count, created_at, updated_at, last_active_at) VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))")
            .bind(&session).bind(&owner).execute(pool.get()).await.expect("create real session");
        keys.push(SessionKeyV1::owner_session(
            "server", &owner, &session, "main",
        ));
    }
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    coordinator
        .load_or_initialize_execution_binding(&keys[0], &binding(&keys[0].session_id))
        .await
        .expect("initial checkout selection");
    (pool, coordinator, keys)
}

fn actor(key: &SessionKeyV1) -> ActorContextV1 {
    ActorContextV1::owner_user(
        &key.owner_user_id,
        "reuse-test",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    )
}

async fn reserve(
    coordinator: &DatabaseSessionContextCoordinator,
    key: &SessionKeyV1,
) -> Result<AcquireWriterAndReserveTurnOutcome, SessionContextCoordinatorError> {
    coordinator
        .acquire_writer_and_reserve_turn(
            key,
            None,
            &actor(key),
            Duration::from_secs(60),
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
            Some(1),
        )
        .await
}

async fn cleanup(pool: &SharedPool, owner: &str) {
    for (table, column) in [
        ("run_display_projections", "user_id"),
        ("agent_run_events", "user_id"),
        ("tool_invocation_ledger", "user_id"),
        ("agent_session_execution_slots", "user_id"),
        ("agent_runs", "user_id"),
        ("session_execution_workspace_claims", "owner_user_id"),
        ("session_execution_bindings", "owner_user_id"),
        ("session_context_operation_receipts", "owner_user_id"),
        ("session_context_authority_events", "owner_user_id"),
        ("session_context_heads", "owner_user_id"),
        ("agent_session_lifecycle_fences", "user_id"),
        ("agent_sessions", "user_id"),
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE {column} = ?"))
            .bind(owner)
            .execute(pool.get())
            .await
            .expect("clean fixture");
    }
}

async fn retire_executor(store: &DatabaseRunStateStore, key: &SessionKeyV1, run_id: &str) {
    let run = store
        .load_run(&key.owner_user_id, run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .release_owner_lease(
                &key.owner_user_id,
                &key.session_id,
                run_id,
                run.run_generation,
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn idle_checkout_reuse_preserves_sessions_and_allows_explicit_return() {
    let (pool, coordinator, keys) = fixture().await;
    let old_binding = coordinator
        .load_execution_binding(&keys[0])
        .await
        .unwrap()
        .unwrap();
    coordinator
        .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
        .await
        .expect("new session reuses idle checkout without resume");
    assert_eq!(
        coordinator.load_execution_binding(&keys[0]).await.unwrap(),
        Some(old_binding)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE user_id = ?")
        .bind(&keys[0].owner_user_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(
        count, 3,
        "reuse must not delete or merge session identities"
    );
    let lease = match reserve(&coordinator, &keys[0])
        .await
        .expect("old session can explicitly run again")
    {
        AcquireWriterAndReserveTurnOutcome::Ready { lease, .. } => lease,
        other => panic!("unexpected admission {other:?}"),
    };
    assert!(matches!(
        reserve(&coordinator, &keys[1]).await,
        Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            blocker: WorkspaceReuseBlocker::WriterOrReservation,
            ..
        })
    ));
    coordinator.release_writer(&lease).await.unwrap();
    assert!(matches!(
        reserve(&coordinator, &keys[1]).await.unwrap(),
        AcquireWriterAndReserveTurnOutcome::Ready { .. }
    ));
    cleanup(&pool, &keys[0].owner_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn checkout_reuse_blocks_unresolved_tools_even_without_live_authority() {
    let (pool, coordinator, keys) = fixture().await;
    for state in ["prepared", "dispatched", "outcome_unknown"] {
        sqlx::query("INSERT INTO tool_invocation_ledger (user_id, session_id, run_id, turn_chain_id, invocation_id, identity_key, fingerprint_json, decision_json, state, dispatch_certainty) VALUES (?, ?, 'old-run', 'turn', 'tool', 'identity', '{}', '{}', ?, 'unknown')")
            .bind(&keys[0].owner_user_id).bind(&keys[0].session_id).bind(state)
            .execute(pool.get()).await.unwrap();
        assert!(
            matches!(
                coordinator
                    .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
                    .await,
                Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
                    blocker: WorkspaceReuseBlocker::UnresolvedTool,
                    ..
                })
            ),
            "must block {state}"
        );
        sqlx::query("DELETE FROM tool_invocation_ledger WHERE user_id = ?")
            .bind(&keys[0].owner_user_id)
            .execute(pool.get())
            .await
            .unwrap();
    }
    coordinator
        .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
        .await
        .expect("resolved tools release checkout for new conversation");
    cleanup(&pool, &keys[0].owner_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn concurrent_sessions_cannot_both_reserve_shared_checkout() {
    let (pool, coordinator, keys) = fixture().await;
    for key in &keys[1..] {
        coordinator
            .load_or_initialize_execution_binding(key, &binding(&key.session_id))
            .await
            .unwrap();
    }
    let (first, second, third) = tokio::join!(
        reserve(&coordinator, &keys[0]),
        reserve(&coordinator, &keys[1]),
        reserve(&coordinator, &keys[2])
    );
    let results = [first, second, third];
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Ok(AcquireWriterAndReserveTurnOutcome::Ready { .. })))
            .count(),
        1,
        "{results:?}"
    );
    cleanup(&pool, &keys[0].owner_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn checkout_reuse_blocks_execution_slots_and_fences_delayed_old_run() {
    let (pool, coordinator, keys) = fixture().await;
    let store = DatabaseRunStateStore::new(pool.clone());
    store
        .insert_run(run(&keys[0], "old-run"))
        .await
        .expect("start old run");
    assert!(matches!(
        coordinator
            .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
            .await,
        Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            blocker: WorkspaceReuseBlocker::ExecutionSlot,
            ..
        })
    ));
    // Model settlement already happened; retain the slot to prove it independently blocks reuse.
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ?")
        .bind(&keys[0].owner_user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(matches!(
        coordinator
            .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
            .await,
        Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            blocker: WorkspaceReuseBlocker::ExecutionSlot,
            ..
        })
    ));
    sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ?")
        .bind(&keys[0].owner_user_id)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(
        matches!(
            coordinator
                .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
                .await,
            Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
                blocker: WorkspaceReuseBlocker::ActiveRun,
                ..
            })
        ),
        "terminal status and slot retirement do not imply executor exit"
    );
    retire_executor(&store, &keys[0], "old-run").await;
    coordinator
        .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
        .await
        .unwrap();
    assert!(
        store
            .insert_run(run(&keys[0], "delayed-old-run"))
            .await
            .expect_err("old admission must be fenced")
            .contains("execution workspace is already claimed"),
        "a delayed old admission must not run after another session claimed the checkout"
    );
    cleanup(&pool, &keys[0].owner_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn checkout_reuse_blocks_provider_switch_and_active_child_without_root_slot() {
    let (pool, coordinator, keys) = fixture().await;
    let mut switching = binding(&keys[0].session_id);
    switching.generation = 2;
    switching.state = SessionExecutionBindingStateV1::Switching;
    coordinator
        .compare_and_swap_execution_binding(&keys[0], 1, &switching)
        .await
        .unwrap();
    assert!(matches!(
        coordinator
            .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
            .await,
        Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            blocker: WorkspaceReuseBlocker::BindingNotReady,
            ..
        })
    ));
    switching.generation = 3;
    switching.state = SessionExecutionBindingStateV1::Ready;
    coordinator
        .compare_and_swap_execution_binding(&keys[0], 2, &switching)
        .await
        .unwrap();
    let store = DatabaseRunStateStore::new(pool.clone());
    let mut child = run(&keys[0], "child-run");
    child.parent_run_id = Some("parent-run".into());
    child.root_run_id = Some("parent-run".into());
    child.ancestor_path = Some("parent-run/child-run".into());
    child.depth = 1;
    store
        .insert_run(child.clone())
        .await
        .expect("child starts without a root slot");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_execution_slots WHERE user_id = ?")
            .bind(&keys[0].owner_user_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert!(matches!(
        coordinator
            .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
            .await,
        Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            blocker: WorkspaceReuseBlocker::ActiveRun,
            ..
        })
    ));
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ?")
        .bind(&keys[0].owner_user_id)
        .execute(pool.get())
        .await
        .unwrap();
    retire_executor(&store, &keys[0], "child-run").await;
    coordinator
        .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
        .await
        .unwrap();
    child.run_id = "delayed-child".into();
    assert!(
        store
            .insert_run(child)
            .await
            .expect_err("child must be fenced")
            .contains("execution workspace is already claimed"),
        "children cannot bypass the checkout fence"
    );
    cleanup(&pool, &keys[0].owner_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn checkout_reuse_fences_root_and_child_resume_without_changing_run_state() {
    for child in [false, true] {
        let (pool, coordinator, keys) = fixture().await;
        let store = DatabaseRunStateStore::new(pool.clone());
        let mut record = run(&keys[0], "paused-run");
        if child {
            record.parent_run_id = Some("parent".into());
            record.depth = 1;
        }
        store.insert_run(record).await.unwrap();
        assert!(
            store
                .update_run_status(
                    &keys[0].owner_user_id,
                    &keys[0].session_id,
                    "paused-run",
                    "paused",
                    None,
                    None
                )
                .await
                .unwrap()
        );
        retire_executor(&store, &keys[0], "paused-run").await;
        coordinator
            .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
            .await
            .unwrap();
        assert!(
            store
                .update_run_status(
                    &keys[0].owner_user_id,
                    &keys[0].session_id,
                    "paused-run",
                    "running",
                    None,
                    None
                )
                .await
                .expect_err("resume must be fenced")
                .contains("execution workspace is already claimed"),
            "resume must reacquire checkout, child={child}"
        );
        let status: String = sqlx::query_scalar(
            "SELECT status FROM agent_runs WHERE user_id = ? AND run_id = 'paused-run'",
        )
        .bind(&keys[0].owner_user_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(status, "paused", "rejected resume must roll back status");
        cleanup(&pool, &keys[0].owner_user_id).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn idle_reuse_does_not_release_another_users_claim() {
    let (pool, coordinator, keys) = fixture().await;
    let (other_pool, other_coordinator, other_keys) = fixture().await;
    let other_lease = match reserve(&other_coordinator, &other_keys[0]).await.unwrap() {
        AcquireWriterAndReserveTurnOutcome::Ready { lease, .. } => lease,
        result => panic!("{result:?}"),
    };
    coordinator
        .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
        .await
        .unwrap();
    let claim_owner: String = sqlx::query_scalar(
        "SELECT session_id FROM session_execution_workspace_claims WHERE owner_user_id = ?",
    )
    .bind(&other_keys[0].owner_user_id)
    .fetch_one(other_pool.get())
    .await
    .unwrap();
    assert_eq!(claim_owner, other_keys[0].session_id);
    other_coordinator
        .renew_writer(&other_lease, Duration::from_secs(60))
        .await
        .unwrap();
    cleanup(&pool, &keys[0].owner_user_id).await;
    cleanup(&other_pool, &other_keys[0].owner_user_id).await;
}

fn run(key: &SessionKeyV1, run_id: &str) -> DurableRunRecord {
    DurableRunRecord {
        run_id: run_id.to_owned(),
        user_id: key.owner_user_id.clone(),
        session_id: key.session_id.clone(),
        parent_run_id: None,
        root_run_id: Some(run_id.to_owned()),
        ancestor_path: Some(run_id.to_owned()),
        depth: 0,
        delegation_id: None,
        agent_id: None,
        retry_of: None,
        retry_scope: None,
        status: "running".to_owned(),
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
        runtime_profile: None,
        start_request_fingerprint: None,
        work_binding: None,
        events: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}
