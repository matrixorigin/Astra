mod common;

use std::time::Duration;

use astra_core::SharedPool;
use astra_services::{
    AcquireWriterAndReserveTurnOutcome, DatabaseSessionContextCoordinator,
    SessionContextCoordinator, SessionContextCoordinatorError, SessionExecutionBindingStateV1,
    SessionExecutionBindingV1, session_context_coordinator::SessionExecutionBlocker,
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
                "shared-test-device",
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
            executor_id: Some("shared-test-edge".into()),
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
    let owner = format!("shared-workspace-{}", Uuid::new_v4());
    let mut keys = Vec::new();
    for name in ["first", "second", "third"] {
        let session = format!("{name}-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO agent_sessions
             (session_id, user_id, status, event_count, created_at, updated_at, last_active_at)
             VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))",
        )
        .bind(&session)
        .bind(&owner)
        .execute(pool.get())
        .await
        .expect("create real session");
        keys.push(SessionKeyV1::owner_session(
            "server", &owner, &session, "main",
        ));
    }
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    (pool, coordinator, keys)
}

fn actor(key: &SessionKeyV1) -> ActorContextV1 {
    ActorContextV1::owner_user(
        &key.owner_user_id,
        "shared-workspace-test",
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

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sessions_using_one_directory_keep_independent_authority() {
    let (pool, coordinator, keys) = fixture().await;
    for key in &keys {
        coordinator
            .load_or_initialize_execution_binding(key, &binding(&key.session_id))
            .await
            .expect("each Session can bind the same physical workspace");
    }

    let (first, second, third) = tokio::join!(
        reserve(&coordinator, &keys[0]),
        reserve(&coordinator, &keys[1]),
        reserve(&coordinator, &keys[2]),
    );
    let results = [first, second, third];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(AcquireWriterAndReserveTurnOutcome::Ready { .. })))
            .count(),
        3,
        "Session authority is independent even when physical workspace identity is shared: {results:?}"
    );

    let first_binding = coordinator
        .load_execution_binding(&keys[0])
        .await
        .expect("load first binding")
        .expect("first binding exists");
    let second_binding = coordinator
        .load_execution_binding(&keys[1])
        .await
        .expect("load second binding")
        .expect("second binding exists");
    assert_eq!(
        first_binding.physical_workspace_id, second_binding.physical_workspace_id,
        "the test must exercise one physical workspace"
    );

    cleanup(&pool, &keys[0].owner_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn one_session_unresolved_tool_does_not_block_another_session() {
    let (pool, coordinator, keys) = fixture().await;
    coordinator
        .load_or_initialize_execution_binding(&keys[0], &binding(&keys[0].session_id))
        .await
        .expect("initialize first Session");
    sqlx::query(
        "INSERT INTO tool_invocation_ledger
         (user_id, session_id, run_id, turn_chain_id, invocation_id, identity_key,
          fingerprint_json, decision_json, state, dispatch_certainty)
         VALUES (?, ?, 'old-run', 'turn', 'tool', 'identity', '{}', '{}', 'outcome_unknown', 'unknown')",
    )
    .bind(&keys[0].owner_user_id)
    .bind(&keys[0].session_id)
    .execute(pool.get())
    .await
    .expect("insert unresolved tool fixture");

    coordinator
        .load_or_initialize_execution_binding(&keys[1], &binding(&keys[1].session_id))
        .await
        .expect("another Session can open the same workspace");
    assert_eq!(
        coordinator
            .execution_reuse_blocker(&keys[0])
            .await
            .expect("inspect first Session"),
        Some(SessionExecutionBlocker::UnresolvedTool)
    );
    assert_eq!(
        coordinator
            .execution_reuse_blocker(&keys[1])
            .await
            .expect("inspect second Session"),
        None,
        "execution debt is scoped to its Session, not the shared directory"
    );

    cleanup(&pool, &keys[0].owner_user_id).await;
}
