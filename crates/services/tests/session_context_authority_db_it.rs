mod common;

use std::{sync::Arc, time::Duration};

use astra_services::{
    AcquireWriterAndReserveTurnOutcome, AcquireWriterOutcome, BeginSessionExecutionSwitchV1,
    DatabaseSessionContextCoordinator, ReserveTurnOutcome, SessionContextCoordinator,
    SessionContextCoordinatorError, SessionExecutionBindingStateV1, SessionExecutionBindingV1,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, ContextManifestNodeV1, ConversationSegmentV1,
    CoordinatorMutationV1, SESSION_ATTACHMENT_SCHEMA_VERSION, SessionAttachmentModeV1,
    SessionAttachmentV1, SessionKeyV1, SessionPlacementV1, SessionSurfaceV1,
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn complete_turn_authority_renews_atomically_in_database() {
    let pool = common::setup_pool().await;
    let owner_id = format!("authority-owner-{}", Uuid::new_v4());
    let session_id = format!("authority-session-{}", Uuid::new_v4());
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "authority-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());

    let lease = match coordinator
        .acquire_writer(&key, None, &actor, Duration::from_secs(30), "acquire")
        .await
        .expect("acquire writer")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected writer outcome: {other:?}"),
    };
    let reservation = match coordinator
        .reserve_turn(&lease, None, Duration::from_secs(30), "reserve", None)
        .await
        .expect("reserve turn")
    {
        ReserveTurnOutcome::Reserved(reservation) => reservation,
        other => panic!("unexpected reservation outcome: {other:?}"),
    };

    tokio::time::sleep(Duration::from_millis(5)).await;
    let renewed = coordinator
        .renew_turn_authority(&lease, &reservation, Duration::from_secs(60))
        .await
        .expect("renew complete turn authority");
    assert!(renewed.writer_lease.expires_at_unix_ms > lease.expires_at_unix_ms);
    assert_eq!(
        renewed.writer_lease.expires_at_unix_ms,
        renewed.turn_reservation.expires_at_unix_ms
    );

    let stored: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT active_writer_expires_at_ms, active_reservation_expires_at_ms
         FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load stored authority expiries");
    assert_eq!(stored.0, Some(renewed.writer_lease.expires_at_unix_ms));
    assert_eq!(stored.0, stored.1);

    let audit: (String, String) = sqlx::query_as(
        "SELECT operation_kind, outcome
         FROM session_context_authority_events
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
         ORDER BY created_at DESC, event_id DESC LIMIT 1",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load authority renewal audit");
    assert_eq!(audit, ("renew_turn_authority".into(), "renewed".into()));

    for table in [
        "session_context_operation_receipts",
        "session_context_authority_events",
        "session_context_heads",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?"
        ))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .execute(pool.get())
        .await
        .expect("clean authority fixture");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn execution_binding_is_owner_scoped_fenced_and_quiescent_per_session() {
    let pool = common::setup_pool().await;
    let suffix = Uuid::new_v4();
    let owner_a = format!("execution-binding-owner-a-{suffix}");
    let owner_b = format!("execution-binding-owner-b-{suffix}");
    let shared_session = format!("execution-binding-session-{suffix}");
    let other_session = format!("execution-binding-other-session-{suffix}");
    let key_a = SessionKeyV1::owner_session("server", &owner_a, &shared_session, "main");
    let key_a_other = SessionKeyV1::owner_session("server", &owner_a, &other_session, "main");
    // Deliberately reuse the session id across owners to prove the owner key
    // remains part of selection identity.
    let key_b = SessionKeyV1::owner_session("server", &owner_b, &shared_session, "main");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let initial_a = SessionExecutionBindingV1::server_work_default("work:shared:branch:main");
    let initial_a_other = SessionExecutionBindingV1::server_work_default("work:other:branch:main");
    let initial_b = SessionExecutionBindingV1::server_work_default("work:shared:branch:main");

    for (key, initial) in [
        (&key_a, &initial_a),
        (&key_a_other, &initial_a_other),
        (&key_b, &initial_b),
    ] {
        let loaded = coordinator
            .load_or_initialize_execution_binding(key, initial)
            .await
            .expect("initialize owner-scoped execution binding");
        assert_eq!(loaded.generation, 1);
    }

    let mut switching = initial_a.clone();
    switching.generation = 2;
    switching.state = SessionExecutionBindingStateV1::Switching;
    let switched = coordinator
        .compare_and_swap_execution_binding(&key_a, 1, &switching)
        .await
        .expect("advance exactly the selected Session binding");
    assert_eq!(switched.state, SessionExecutionBindingStateV1::Switching);

    let actor_a = ActorContextV1::owner_user(
        &owner_a,
        "execution-binding-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    assert!(matches!(
        coordinator
            .acquire_writer_and_reserve_turn(
                &key_a,
                None,
                &actor_a,
                Duration::from_secs(30),
                "stale-writer",
                "stale-turn",
                Some(1),
            )
            .await,
        Err(SessionContextCoordinatorError::ExecutionBindingFenced {
            expected: 1,
            current: Some(2),
        })
    ));
    assert!(matches!(
        coordinator
            .acquire_writer_and_reserve_turn(
                &key_a,
                None,
                &actor_a,
                Duration::from_secs(30),
                "switching-writer",
                "switching-turn",
                Some(2),
            )
            .await,
        Err(SessionContextCoordinatorError::ExecutionBindingNotReady(
            SessionExecutionBindingStateV1::Switching
        ))
    ));

    let other_a = coordinator
        .load_execution_binding(&key_a_other)
        .await
        .expect("load second Session binding")
        .expect("second Session binding");
    let other_owner = coordinator
        .load_execution_binding(&key_b)
        .await
        .expect("load second owner binding")
        .expect("second owner binding");
    assert_eq!(other_a.generation, 1);
    assert_eq!(other_a.state, SessionExecutionBindingStateV1::Ready);
    assert_eq!(other_owner.generation, 1);
    assert_eq!(other_owner.state, SessionExecutionBindingStateV1::Ready);

    let lease = match coordinator
        .acquire_writer(
            &key_a_other,
            None,
            &actor_a,
            Duration::from_secs(30),
            "active-writer",
        )
        .await
        .expect("acquire a live writer on the second Session")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected writer outcome: {other:?}"),
    };
    let mut blocked_switch = other_a.clone();
    blocked_switch.generation = 2;
    blocked_switch.state = SessionExecutionBindingStateV1::Switching;
    assert!(matches!(
        coordinator
            .compare_and_swap_execution_binding(&key_a_other, 1, &blocked_switch)
            .await,
        Err(SessionContextCoordinatorError::ExecutionBindingBusy)
    ));

    // A live writer on one Session does not hold a global provider-selection
    // lock or block another Session owned by the same user.
    let mut ready_a = initial_a.clone();
    ready_a.generation = 3;
    let ready_a = coordinator
        .compare_and_swap_execution_binding(&key_a, 2, &ready_a)
        .await
        .expect("unrelated Session remains independently writable");
    assert_eq!(ready_a.state, SessionExecutionBindingStateV1::Ready);

    coordinator
        .release_writer(&lease)
        .await
        .expect("release fixture writer");

    for key in [&key_a, &key_a_other, &key_b] {
        sqlx::query(
            "DELETE FROM session_execution_bindings WHERE isolation_domain = ? \
             AND owner_user_id = ? AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .execute(pool.get())
        .await
        .expect("clean execution binding fixture");
        sqlx::query(
            "DELETE FROM session_context_heads WHERE isolation_domain = ? \
             AND owner_user_id = ? AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .execute(pool.get())
        .await
        .expect("clean Session coordination fixture");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn execution_switch_and_turn_admission_have_one_linearization_winner() {
    let pool = common::setup_pool().await;
    let suffix = Uuid::new_v4();
    let owner_id = format!("execution-race-owner-{suffix}");
    let session_id = format!("execution-race-session-{suffix}");
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let initial =
        SessionExecutionBindingV1::server_work_default(format!("session:{session_id}:branch:main"));
    coordinator
        .load_or_initialize_execution_binding(&key, &initial)
        .await
        .expect("initialize execution binding");

    let actor = ActorContextV1::owner_user(
        &owner_id,
        "execution-race-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let mut switching = initial.clone();
    switching.generation = 2;
    switching.state = SessionExecutionBindingStateV1::Switching;

    // Both paths start together.  The coordinator locks the canonical
    // Session head before checking the binding, so exactly one can cross the
    // admission/switch boundary and the other must observe the durable result.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let switch_coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let reserve_coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let reserve_task_coordinator = reserve_coordinator.clone();
    let switch_barrier = Arc::clone(&barrier);
    let reserve_barrier = Arc::clone(&barrier);
    let switch_key = key.clone();
    let reserve_key = key.clone();
    let switch = async move {
        switch_barrier.wait().await;
        switch_coordinator
            .compare_and_swap_execution_binding(&switch_key, 1, &switching)
            .await
    };
    let reserve = async move {
        reserve_barrier.wait().await;
        reserve_task_coordinator
            .acquire_writer_and_reserve_turn(
                &reserve_key,
                None,
                &actor,
                Duration::from_secs(30),
                "execution-race-writer",
                "execution-race-turn",
                Some(1),
            )
            .await
    };
    let (switch_result, reserve_result) = tokio::join!(switch, reserve);

    let switch_won = switch_result.is_ok();
    let reserve_won = matches!(
        &reserve_result,
        Ok(AcquireWriterAndReserveTurnOutcome::Ready { .. })
    );
    assert_eq!(
        usize::from(switch_won) + usize::from(reserve_won),
        1,
        "one and only one path may cross the binding fence: switch={switch_result:?}; reserve={reserve_result:?}"
    );
    match (switch_result, reserve_result) {
        (
            Ok(binding),
            Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: 1,
                current: Some(2),
            }),
        ) => {
            assert_eq!(binding.generation, 2);
            assert_eq!(binding.state, SessionExecutionBindingStateV1::Switching);
        }
        (
            Err(SessionContextCoordinatorError::ExecutionBindingBusy),
            Ok(AcquireWriterAndReserveTurnOutcome::Ready { lease, .. }),
        ) => {
            reserve_coordinator
                .release_writer(&lease)
                .await
                .expect("release race fixture writer");
            let current = coordinator
                .load_execution_binding(&key)
                .await
                .expect("load binding after race")
                .expect("binding after race");
            assert_eq!(current, initial);
        }
        (switch_result, reserve_result) => panic!(
            "unexpected switch/admission race outcome: switch={switch_result:?}; reserve={reserve_result:?}"
        ),
    }

    for table in [
        "session_execution_switches",
        "session_execution_workspace_claims",
        "session_execution_bindings",
        "session_context_operation_receipts",
        "session_context_authority_events",
        "session_context_heads",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?"
        ))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .execute(pool.get())
        .await
        .expect("clean execution race fixture");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn execution_read_is_non_mutating_during_an_active_turn() {
    let pool = common::setup_pool().await;
    let suffix = Uuid::new_v4().to_string();
    let owner_id = format!("execution-read-owner-{suffix}");
    let session_id = format!("execution-read-session-{suffix}");
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))",
    )
    .bind(&session_id)
    .bind(&owner_id)
    .execute(pool.get())
    .await
    .expect("create active read fixture session");

    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let initial =
        SessionExecutionBindingV1::server_work_default(format!("session:{session_id}:branch:main"));
    coordinator
        .load_or_initialize_execution_binding(&key, &initial)
        .await
        .expect("initialize execution binding");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "execution-read-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    coordinator
        .acquire_writer(&key, None, &actor, Duration::from_secs(30), "active-read")
        .await
        .expect("start active turn authority");

    // A read must remain available while the Session is active. The former
    // initialize-on-read path returned Busy before it could load this row.
    let loaded = coordinator
        .load_execution_binding(&key)
        .await
        .expect("read execution binding during active turn")
        .expect("binding remains present");
    assert_eq!(loaded, initial);
    let claim_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_execution_workspace_claims
         WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&owner_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("count read fixture claims");
    assert_eq!(claim_count, 0, "server-only reads do not create claims");

    for table in [
        "session_execution_workspace_claims",
        "session_execution_bindings",
        "session_context_operation_receipts",
        "session_context_authority_events",
        "session_context_heads",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?"
        ))
        .bind(&key.isolation_domain)
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean execution read fixture");
    }
    for table in ["agent_session_lifecycle_fences", "agent_sessions"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
        ))
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean execution read session fixture");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn execution_switch_is_idempotent_retriable_and_workspace_exclusive() {
    let pool = common::setup_pool().await;
    let suffix = Uuid::new_v4().to_string();
    let owner_id = format!("execution-switch-owner-{suffix}");
    let session_id = format!("execution-switch-session-{suffix}");
    let other_session_id = format!("execution-switch-other-{suffix}");
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let other_key = SessionKeyV1::owner_session("server", &owner_id, &other_session_id, "main");
    let logical = format!("session:{session_id}:branch:main");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let initial = SessionExecutionBindingV1::server_work_default(logical.clone());
    coordinator
        .load_or_initialize_execution_binding(&key, &initial)
        .await
        .expect("initialize switch binding");

    let actor = ActorContextV1::owner_user(
        &owner_id,
        "execution-switch-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let attachment = SessionAttachmentV1 {
        schema_version: SESSION_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: Uuid::new_v4().to_string(),
        attachment_epoch: 1,
        key: key.clone(),
        actor,
        mode: SessionAttachmentModeV1::Controller,
        placement: SessionPlacementV1::Server,
        observed_cursor: None,
        observed_manifest_root: None,
        workspace: None,
        attached_at_unix_ms: 1,
        expires_at_unix_ms: i64::MAX,
    };
    sqlx::query(
        "INSERT INTO session_attachments
         (isolation_domain, owner_user_id, session_id, branch_id, attachment_id,
          attachment_epoch, idempotency_hash, request_hash, actor_id, mode,
          placement, attachment_json, expires_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'controller', 'server', ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(&attachment.attachment_id)
    .bind(attachment.attachment_epoch as i64)
    .bind("a".repeat(64))
    .bind("b".repeat(64))
    .bind(&attachment.actor.actor_id)
    .bind(serde_json::to_string(&attachment).expect("encode attachment"))
    .bind(attachment.expires_at_unix_ms)
    .execute(pool.get())
    .await
    .expect("insert controller attachment");

    let edge_binding = |generation: u64,
                        state: SessionExecutionBindingStateV1,
                        executor_id: &str,
                        root: &str| SessionExecutionBindingV1 {
        schema_version: astra_services::SESSION_EXECUTION_BINDING_SCHEMA_VERSION,
        generation,
        state,
        logical_workspace_id: logical.to_string(),
        physical_workspace_id: Some(
            SessionExecutionBindingV1::edge_materialization_physical_identity(
                if root == "/workspace/target" {
                    "materialization-target"
                } else {
                    "materialization-source"
                },
                root,
            ),
        ),
        workspace: astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: Some(executor_id.to_string()),
            root: Some(root.to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
                path: root.to_string(),
            }),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        },
        executor: astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
            executor_id: Some(executor_id.to_string()),
            display_name: Some(executor_id.to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeLedger),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        },
    };
    let source_switching = edge_binding(
        2,
        SessionExecutionBindingStateV1::Switching,
        "edge-source",
        "/workspace/source",
    );
    coordinator
        .compare_and_swap_execution_binding(&key, 1, &source_switching)
        .await
        .expect("prepare source Edge");
    let source = edge_binding(
        3,
        SessionExecutionBindingStateV1::Ready,
        "edge-source",
        "/workspace/source",
    );
    coordinator
        .compare_and_swap_execution_binding(&key, 2, &source)
        .await
        .expect("confirm source Edge");
    let target = edge_binding(
        4,
        SessionExecutionBindingStateV1::Switching,
        "edge-target",
        "/workspace/target",
    );
    let evidence = serde_json::json!({
        "schema_version": 1,
        "root": "/workspace/source",
        "head": "head",
        "tree": "tree",
        "object_format": "sha1",
        "reference": "main",
        "repository": "repo",
        "clean": true
    });
    let begun = coordinator
        .begin_execution_switch(
            &key,
            &BeginSessionExecutionSwitchV1 {
                request_id: "switch-request".into(),
                operation_id: "switch-operation".into(),
                controller_attachment_id: attachment.attachment_id.clone(),
                expected_generation: 3,
                target,
                source_evidence: evidence.clone(),
            },
        )
        .await
        .expect("begin durable switch");
    assert_eq!(begun.attempt, 1);
    let failed = coordinator
        .complete_execution_switch(
            &key,
            &begun.operation_id,
            Some(&attachment.attachment_id),
            begun.attempt,
            begun.switching_generation,
            false,
            None,
            Some("edge_unavailable".into()),
        )
        .await
        .expect("settle failed switch");
    assert_eq!(
        failed.state,
        astra_services::SessionExecutionSwitchStateV1::Failed
    );
    assert_eq!(failed.completed_generation, Some(5));

    let retried = coordinator
        .retry_execution_switch(&key, &begun.operation_id, &attachment.attachment_id, 5)
        .await
        .expect("retry failed switch");
    assert_eq!(retried.attempt, 2);
    assert_eq!(retried.expected_generation, 3);
    assert_eq!(retried.attempt_expected_generation, 5);
    assert_eq!(retried.switching_generation, 6);
    assert!(
        coordinator
            .load_execution_switch(&key, &begun.operation_id)
            .await
            .expect("load retried receipt")
            .is_some()
    );
    let completed = coordinator
        .complete_execution_switch(
            &key,
            &begun.operation_id,
            Some(&attachment.attachment_id),
            retried.attempt,
            retried.switching_generation,
            true,
            Some(serde_json::json!({"source": evidence})),
            None,
        )
        .await
        .expect("complete retried switch");
    assert_eq!(
        completed.state,
        astra_services::SessionExecutionSwitchStateV1::Succeeded
    );
    assert_eq!(completed.completed_generation, Some(7));

    let duplicate = coordinator
        .begin_execution_switch(
            &key,
            &BeginSessionExecutionSwitchV1 {
                request_id: "switch-request".into(),
                operation_id: "switch-operation-replay".into(),
                controller_attachment_id: attachment.attachment_id.clone(),
                expected_generation: 3,
                target: edge_binding(
                    4,
                    SessionExecutionBindingStateV1::Switching,
                    "edge-target",
                    "/workspace/target",
                ),
                source_evidence: serde_json::json!({
                    "schema_version": 1,
                    "root": "/workspace/source",
                    "head": "head",
                    "tree": "tree",
                    "object_format": "sha1",
                    "reference": "main",
                    "repository": "repo",
                    "clean": true
                }),
            },
        )
        .await
        .expect("exact replay returns durable receipt");
    assert_eq!(duplicate.operation_id, begun.operation_id);
    assert_eq!(
        duplicate.state,
        astra_services::SessionExecutionSwitchStateV1::Succeeded
    );

    coordinator
        .load_or_initialize_execution_binding(&other_key, &initial)
        .await
        .expect("initialize second Session binding");
    let same_checkout = edge_binding(
        2,
        SessionExecutionBindingStateV1::Switching,
        "edge-other",
        "/workspace/target",
    );
    assert!(matches!(
        coordinator
            .compare_and_swap_execution_binding(&other_key, 1, &same_checkout)
            .await,
        Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            ref owner_session_id,
            ref owner_branch_id,
        }) if owner_session_id == &key.session_id && owner_branch_id == &key.branch_id
    ));

    for table in [
        "session_execution_switches",
        "session_execution_workspace_claims",
        "session_attachments",
        "session_execution_bindings",
        "session_context_heads",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ? AND session_id IN (?, ?)"
        ))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&other_key.session_id)
        .execute(pool.get())
        .await
        .expect("clean execution switch fixture");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn execution_workspace_claim_fences_work_and_ordinary_sessions_on_one_checkout() {
    let pool = common::setup_pool().await;
    let suffix = Uuid::new_v4().to_string();
    let owner_id = format!("execution-claim-owner-{suffix}");
    let work_session_id = format!("execution-claim-work-session-{suffix}");
    let ordinary_session_id = format!("execution-claim-ordinary-session-{suffix}");
    let other_device_session_id = format!("execution-claim-other-device-session-{suffix}");
    let work_key = SessionKeyV1::owner_session("server", &owner_id, &work_session_id, "main");
    let ordinary_key =
        SessionKeyV1::owner_session("server", &owner_id, &ordinary_session_id, "main");
    let other_device_key =
        SessionKeyV1::owner_session("server", &owner_id, &other_device_session_id, "main");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());

    let work_initial = SessionExecutionBindingV1::server_work_default("work:claim:branch:main");
    let ordinary_initial =
        SessionExecutionBindingV1::server_work_default("session:ordinary:branch:default");
    let other_device_initial =
        SessionExecutionBindingV1::server_work_default("session:other-device:branch:default");
    for (key, initial) in [
        (&work_key, &work_initial),
        (&ordinary_key, &ordinary_initial),
        (&other_device_key, &other_device_initial),
    ] {
        coordinator
            .load_or_initialize_execution_binding(key, initial)
            .await
            .expect("initialize both logical Sessions");
    }

    let edge_binding = |logical_workspace_id: &str,
                        generation: u64,
                        executor_id: &str,
                        materialization_id: &str| {
        SessionExecutionBindingV1 {
            schema_version: astra_services::SESSION_EXECUTION_BINDING_SCHEMA_VERSION,
            generation,
            state: SessionExecutionBindingStateV1::Ready,
            logical_workspace_id: logical_workspace_id.to_owned(),
            physical_workspace_id: Some(
                SessionExecutionBindingV1::edge_materialization_physical_identity(
                    materialization_id,
                    "/workspace/shared",
                ),
            ),
            workspace: astra_services::runs::WorkspaceBindingRequest {
                kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
                display_name: Some("shared-checkout".into()),
                root: Some("/workspace/shared".into()),
                source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
                    path: "/workspace/shared".into(),
                }),
                authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            },
            executor: astra_services::runs::ExecutorBindingRequest {
                kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
                executor_id: Some(executor_id.into()),
                display_name: Some(executor_id.into()),
                transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeLedger),
                status: Some(astra_services::runs::ExecutorStatusRequest::Online),
            },
        }
    };

    let mut work_preparing = edge_binding(
        &work_initial.logical_workspace_id,
        2,
        "edge-shared",
        "materialization-shared-device",
    );
    work_preparing.state = SessionExecutionBindingStateV1::Switching;
    coordinator
        .compare_and_swap_execution_binding(&work_key, 1, &work_preparing)
        .await
        .expect("the first Session prepares the checkout");
    let work_edge = edge_binding(
        &work_initial.logical_workspace_id,
        3,
        "edge-shared",
        "materialization-shared-device",
    );
    coordinator
        .compare_and_swap_execution_binding(&work_key, 2, &work_edge)
        .await
        .expect("the first Session claims the checkout");

    let mut ordinary_edge = edge_binding(
        &ordinary_initial.logical_workspace_id,
        2,
        "edge-renamed",
        "materialization-shared-device",
    );
    ordinary_edge.state = SessionExecutionBindingStateV1::Switching;
    let error = coordinator
        .compare_and_swap_execution_binding(&ordinary_key, 1, &ordinary_edge)
        .await
        .expect_err("a second Session must not share one physical checkout");
    assert!(matches!(
        error,
        SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            ref owner_session_id,
            ref owner_branch_id,
        } if owner_session_id == &work_key.session_id && owner_branch_id == &work_key.branch_id
    ));
    let ordinary_after = coordinator
        .load_execution_binding(&ordinary_key)
        .await
        .expect("load ordinary binding after rejected claim")
        .expect("ordinary binding remains present");
    assert_eq!(ordinary_after.generation, 1);
    assert_eq!(
        ordinary_after.workspace.kind,
        astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox
    );

    let mut other_device_preparing = edge_binding(
        &other_device_initial.logical_workspace_id,
        2,
        "edge-other-device",
        "materialization-independent-device",
    );
    other_device_preparing.state = SessionExecutionBindingStateV1::Switching;
    coordinator
        .compare_and_swap_execution_binding(&other_device_key, 1, &other_device_preparing)
        .await
        .expect("an independent device may prepare the same path");
    let other_device_edge = edge_binding(
        &other_device_initial.logical_workspace_id,
        3,
        "edge-other-device",
        "materialization-independent-device",
    );
    coordinator
        .compare_and_swap_execution_binding(&other_device_key, 2, &other_device_edge)
        .await
        .expect("an independent device may materialize the same path");

    // Moving one Session to another materialization must release its old
    // claim and install the new one in the same transaction. The old checkout
    // is then available to a different Session without a transient self-
    // conflict from the per-Session unique claim key.
    let mut work_preparing = edge_binding(
        &work_initial.logical_workspace_id,
        4,
        "edge-other-device",
        "materialization-handoff-device",
    );
    work_preparing.state = SessionExecutionBindingStateV1::Switching;
    let switched = coordinator
        .compare_and_swap_execution_binding(&work_key, 3, &work_preparing)
        .await
        .expect("the Session can prepare another materialization");
    assert_eq!(switched.generation, 4);
    let work_switching = edge_binding(
        &work_initial.logical_workspace_id,
        5,
        "edge-other-device",
        "materialization-handoff-device",
    );
    let switched = coordinator
        .compare_and_swap_execution_binding(&work_key, 4, &work_switching)
        .await
        .expect("the Session can move its claim to another materialization");
    assert_eq!(switched.generation, 5);
    let mut ordinary_reclaim = edge_binding(
        &ordinary_initial.logical_workspace_id,
        2,
        "edge-reclaimed",
        "materialization-shared-device",
    );
    ordinary_reclaim.state = SessionExecutionBindingStateV1::Switching;
    coordinator
        .compare_and_swap_execution_binding(&ordinary_key, 1, &ordinary_reclaim)
        .await
        .expect("the old materialization can be prepared after the handoff");
    let ordinary_reclaim = edge_binding(
        &ordinary_initial.logical_workspace_id,
        3,
        "edge-reclaimed",
        "materialization-shared-device",
    );
    coordinator
        .compare_and_swap_execution_binding(&ordinary_key, 2, &ordinary_reclaim)
        .await
        .expect("the old materialization is released after the handoff");

    for key in [&work_key, &ordinary_key, &other_device_key] {
        for table in [
            "session_execution_workspace_claims",
            "session_execution_bindings",
            "session_context_heads",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?"
            ))
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .execute(pool.get())
            .await
            .expect("clean execution claim fixture");
        }
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn commit_reactivates_matching_legacy_staged_manifest() {
    let pool = common::setup_pool().await;
    let owner_id = format!("staged-manifest-owner-{}", Uuid::new_v4());
    let session_id = format!("staged-manifest-session-{}", Uuid::new_v4());
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "staged-manifest-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let lease = match coordinator
        .acquire_writer(&key, None, &actor, Duration::from_secs(30), "acquire")
        .await
        .expect("acquire writer")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected writer outcome: {other:?}"),
    };
    let reservation = match coordinator
        .reserve_turn(&lease, None, Duration::from_secs(30), "reserve", None)
        .await
        .expect("reserve turn")
    {
        ReserveTurnOutcome::Reserved(reservation) => reservation,
        other => panic!("unexpected reservation outcome: {other:?}"),
    };
    let messages = vec![serde_json::json!({
        "role": "assistant",
        "content": "legacy staged manifest"
    })];
    let segment = ConversationSegmentV1::new(&key, messages.clone()).expect("segment");
    let node = ContextManifestNodeV1::new(
        key.clone(),
        None,
        1,
        1,
        1,
        0,
        None,
        vec![segment.reference()],
    )
    .expect("manifest");
    let segment_json = serde_json::to_string(&segment).expect("serialize segment");
    sqlx::query(
        "INSERT INTO conversation_segments
         (isolation_domain, owner_user_id, segment_hash, canonical_root_hash,
          canonical_bytes, message_count, segment_json)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&segment.segment_hash)
    .bind(&segment.canonical_root_hash)
    .bind(i64::try_from(segment.canonical_bytes).expect("segment bytes fit BIGINT"))
    .bind(i64::from(segment.message_count))
    .bind(segment_json)
    .execute(pool.get())
    .await
    .expect("stage legacy segment");
    sqlx::query(
        "INSERT INTO conversation_manifest_nodes
         (isolation_domain, owner_user_id, session_id, branch_id, manifest_root,
          parent_manifest_root, completed_turn, conversation_seq,
          compaction_generation, canonical_segment_bytes, total_canonical_bytes,
          total_message_count, manifest_json, reachable)
         VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, 0)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(&node.manifest_root)
    .bind(i64::from(node.completed_turn))
    .bind(i64::try_from(node.conversation_seq).expect("conversation sequence fits BIGINT"))
    .bind(i64::try_from(node.compaction_generation).expect("generation fits BIGINT"))
    .bind(i64::try_from(segment.canonical_bytes).expect("segment bytes fit BIGINT"))
    .bind(i64::try_from(segment.canonical_bytes).expect("total bytes fit BIGINT"))
    .bind(i64::from(segment.message_count))
    .bind(serde_json::to_string(&node).expect("serialize manifest"))
    .execute(pool.get())
    .await
    .expect("stage legacy unreachable manifest");
    sqlx::query(
        "INSERT INTO conversation_manifest_segments
         (isolation_domain, owner_user_id, session_id, branch_id,
          manifest_root, segment_position, segment_hash)
         VALUES (?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(&node.manifest_root)
    .bind(&segment.segment_hash)
    .execute(pool.get())
    .await
    .expect("stage legacy manifest reference");

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
                logical_segments: vec![messages.clone()],
            },
            "commit-staged-manifest",
        )
        .await
        .expect("commit over legacy staged manifest");
    let cursor = match outcome {
        CoordinatorMutationV1::Applied { cursor } => cursor,
        other => panic!("unexpected commit outcome: {other:?}"),
    };
    assert_eq!(cursor.canonical_root_hash, node.manifest_root);
    let reachable: i64 = sqlx::query_scalar(
        "SELECT reachable FROM conversation_manifest_nodes
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND manifest_root = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(&node.manifest_root)
    .fetch_one(pool.get())
    .await
    .expect("load manifest reachability");
    assert_eq!(reachable, 1);
    let head = coordinator
        .load_head(&key)
        .await
        .expect("load committed head")
        .expect("committed head");
    let materialized = coordinator
        .materialize(&head)
        .await
        .expect("materialize reactivated manifest");
    assert_eq!(materialized.messages, messages);

    for table in [
        "conversation_manifest_segments",
        "conversation_manifest_nodes",
        "conversation_segments",
        "session_context_operation_receipts",
        "session_context_authority_events",
        "session_context_heads",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE isolation_domain = ? AND owner_user_id = ?"
        ))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .execute(pool.get())
        .await
        .expect("clean staged manifest fixture");
    }
}
