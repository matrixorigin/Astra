mod common;

use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;
use astra_services::{
    AcquireWriterAndReserveTurnOutcome, AcquireWriterOutcome, BeginSessionExecutionSwitchV1,
    DatabaseSessionContextCoordinator, ReserveTurnOutcome, SessionContextCoordinator,
    SessionContextCoordinatorError, SessionExecutionBindingStateV1, SessionExecutionBindingV1,
    SessionService,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, ContextManifestNodeV1, ConversationSegmentV1,
    CoordinatorMutationV1, SESSION_ATTACHMENT_SCHEMA_VERSION, SessionAttachmentModeV1,
    SessionAttachmentV1, SessionKeyV1, SessionPlacementV1, SessionSurfaceV1,
};
use serial_test::serial;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
#[serial]
async fn cancelled_session_authority_lock_releases_its_physical_checkout() {
    let observer_pool = common::setup_pool().await;
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 0;
    settings.db_pool_max_connections = 2;
    let shared_pool = SharedPool::new(&settings)
        .await
        .expect("create two-connection cancellation pool");
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = format!("authority-cancel-owner-{suffix}");
    let session_id = format!("authority-cancel-session-{suffix}");
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let coordinator = DatabaseSessionContextCoordinator::new(shared_pool.clone());
    coordinator
        .load_or_initialize_execution_binding(
            &key,
            &SessionExecutionBindingV1::server_work_default(format!(
                "authority-cancel-work-{suffix}"
            )),
        )
        .await
        .expect("initialize cancellation fixture");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "authority-cancellation-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );

    let mut blocker = pool.begin().await.expect("begin session-head blocker");
    let blocker_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&mut *blocker)
        .await
        .expect("read blocker connection id");
    sqlx::query(
        "SELECT head_json FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_one(&mut *blocker)
    .await
    .expect("hold the canonical session-head lock");

    // The blocker owns one of the two pool slots. Prime the only other slot
    // and record its server identity before the coordinator starts; observing
    // that exact connection avoids mistaking another test's query for the
    // cancelled operation.
    let worker_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("identify the coordinator connection slot");

    let blocked = tokio::spawn({
        let coordinator = coordinator.clone();
        let key = key.clone();
        let actor = actor.clone();
        async move {
            coordinator
                .acquire_writer(
                    &key,
                    None,
                    &actor,
                    Duration::from_secs(30),
                    "blocked-writer",
                )
                .await
        }
    });
    let database_name = settings.database.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut observed_wait = false;
    let mut last_processlist = Vec::new();
    let mut observation_error = None;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let processlist = tokio::time::timeout(
            remaining,
            sqlx::query_as::<_, (u64, Option<String>)>(
                "SELECT conn_id, info FROM information_schema.processlist
                 WHERE db = ? AND conn_id = ?
                   AND info IS NOT NULL",
            )
            .bind(&database_name)
            .bind(worker_connection_id)
            .fetch_all(observer_pool.get()),
        )
        .await;
        let processlist = match processlist {
            Ok(Ok(processlist)) => processlist,
            Ok(Err(error)) => {
                observation_error = Some(format!("processlist query failed: {error}"));
                break;
            }
            Err(_) => {
                observation_error = Some("processlist query exceeded its deadline".into());
                break;
            }
        };
        last_processlist = processlist;
        if last_processlist.iter().any(|(_, info)| {
            info.as_deref()
                .is_some_and(|info| info.contains("session_context_heads"))
        }) {
            observed_wait = true;
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let _ =
            tokio::time::timeout(remaining, tokio::time::sleep(Duration::from_millis(10))).await;
    }

    blocked.abort();
    let blocked_result = blocked.await;
    let replacement_connection_id = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()").fetch_one(pool),
    )
    .await;

    blocker
        .rollback()
        .await
        .expect("release the canonical session-head blocker");
    for table in [
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
        .execute(pool)
        .await
        .expect("clean cancellation fixture");
    }

    assert!(
        observation_error.is_none(),
        "processlist observation must finish before its deadline: error={observation_error:?}"
    );
    assert!(
        observed_wait,
        "cancellation regression must observe the exact coordinator connection waiting on the locked session head; worker_connection_id={worker_connection_id}, blocker_connection_id={blocker_connection_id}, processlist={last_processlist:?}"
    );
    assert!(
        blocked_result
            .as_ref()
            .is_err_and(|error| error.is_cancelled()),
        "blocked coordinator must be externally cancellable: result={blocked_result:?}"
    );
    let replacement_connection_id = replacement_connection_id
        .expect("a replacement checkout must remain available after cancellation")
        .expect("replacement checkout health query");
    assert_ne!(
        replacement_connection_id, worker_connection_id,
        "cancellation must close the physical connection instead of returning it to the pool"
    );
}

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
    let other_session = format!("execution-other-{suffix}");
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
async fn session_lifecycle_fence_rejects_context_admission_before_child_locks() {
    let pool = common::setup_pool().await;
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = format!("lifecycle-fence-owner-{suffix}");
    let session_id = format!("lifecycle-fence-session-{suffix}");
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
    .expect("create lifecycle fence fixture session");

    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    coordinator
        .load_or_initialize_execution_binding(
            &key,
            &SessionExecutionBindingV1::server_work_default(format!(
                "lifecycle-fence-work-{suffix}"
            )),
        )
        .await
        .expect("initialize lifecycle fence fixture");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "lifecycle-fence-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );

    sqlx::query(
        "UPDATE agent_session_lifecycle_fences
         SET delete_requested_at = NOW(6), updated_at = NOW(6)
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("mark lifecycle fence pending");
    assert!(matches!(
        coordinator
            .acquire_writer(&key, None, &actor, Duration::from_secs(30), "fenced-writer")
            .await,
        Err(SessionContextCoordinatorError::SessionLifecycleFenced)
    ));

    sqlx::query(
        "UPDATE agent_session_lifecycle_fences
         SET database_deleted_at = NOW(6), updated_at = NOW(6)
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("mark lifecycle fence completed");
    assert!(matches!(
        coordinator
            .acquire_writer(
                &key,
                None,
                &actor,
                Duration::from_secs(30),
                "completed-fenced-writer",
            )
            .await,
        Err(SessionContextCoordinatorError::SessionLifecycleFenced)
    ));

    let receipt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_context_operation_receipts
         WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&owner_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("count fenced receipts");
    assert_eq!(
        receipt_count, 0,
        "fenced admission must not create receipts"
    );

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
        .expect("clean lifecycle fence context fixture");
    }
    for table in ["agent_session_lifecycle_fences", "agent_sessions"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
        ))
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean lifecycle fence session fixture");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_lifecycle_fence_serializes_delete_and_late_writer() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = format!("lifecycle-race-owner-{suffix}");
    let session_id = format!("lifecycle-race-session-{suffix}");
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))",
    )
    .bind(&session_id)
    .bind(&owner_id)
    .execute(&pool)
    .await
    .expect("create lifecycle race fixture session");

    let coordinator = DatabaseSessionContextCoordinator::new(shared.clone());
    coordinator
        .load_or_initialize_execution_binding(
            &key,
            &SessionExecutionBindingV1::server_work_default(format!(
                "lifecycle-race-work-{suffix}"
            )),
        )
        .await
        .expect("initialize lifecycle race fixture");

    // Hold the exact fence used by a coordinator writer. The real delete path
    // must wait here rather than deleting child rows out from under the
    // admitted transaction.
    let mut admitted_writer = pool.begin().await.expect("begin admitted writer");
    astra_services::storage::lock_agent_session_write_fence(
        &mut admitted_writer,
        &session_id,
        &owner_id,
    )
    .await
    .expect("admit writer lifecycle fence");

    let delete_session_id = session_id.clone();
    let delete_owner_id = owner_id.clone();
    let mut delete_task = tokio::spawn(async move {
        astra_services::DatabaseSessionService::new(settings)
            .with_pool(shared)
            .delete_session(delete_session_id, delete_owner_id)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut delete_task)
            .await
            .is_err(),
        "delete must wait for an already-admitted writer fence"
    );
    admitted_writer
        .commit()
        .await
        .expect("commit admitted writer before delete");
    tokio::time::timeout(Duration::from_secs(5), delete_task)
        .await
        .expect("delete must finish after the writer fence is released")
        .expect("delete task must not panic")
        .expect("delete session after admitted writer");

    let actor = ActorContextV1::owner_user(
        &owner_id,
        "lifecycle-race-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    assert!(matches!(
        coordinator
            .acquire_writer(&key, None, &actor, Duration::from_secs(30), "late-writer")
            .await,
        Err(SessionContextCoordinatorError::SessionLifecycleFenced)
    ));
    let head_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&owner_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("count deleted context heads");
    let receipt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_context_operation_receipts
         WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&owner_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("count deleted context receipts");
    assert_eq!(
        head_count, 0,
        "late writer must not recreate the context head"
    );
    assert_eq!(
        receipt_count, 0,
        "late writer must not create an operation receipt"
    );

    sqlx::query(
        "DELETE FROM agent_session_lifecycle_fences
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("clean lifecycle race fence fixture");
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

    // Simulate the original handler exiting after the durable begin commit.
    // A controller-authorized retry must be able to resume this permanently
    // switching receipt instead of treating it as a terminal failure.
    let stranded = coordinator
        .authorize_execution_switch_retry(&key, &begun.operation_id, &attachment.attachment_id)
        .await
        .expect("authorize recovery of stranded switch");
    assert_eq!(
        stranded.state,
        astra_services::SessionExecutionSwitchStateV1::Switching
    );
    assert_eq!(stranded.operation_id, begun.operation_id);

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
            ..
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
    let work_session_id = format!("claim-work-{suffix}");
    let ordinary_session_id = format!("claim-chat-{suffix}");
    let other_device_session_id = format!("claim-device-{suffix}");
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
        sqlx::query(
            "INSERT INTO agent_sessions
             (session_id, user_id, status, event_count, created_at, updated_at, last_active_at)
             VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))",
        )
        .bind(&key.session_id)
        .bind(&key.owner_user_id)
        .execute(pool.get())
        .await
        .expect("create durable checkout owner");
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

    let active_writer = match coordinator
        .acquire_writer(
            &work_key,
            None,
            &ActorContextV1::owner_user(
                &owner_id,
                "checkout-owner",
                ActorKindV1::Server,
                SessionSurfaceV1::Server,
                None,
                AuthorityEpochsV1::default(),
            ),
            Duration::from_secs(60),
            "active-checkout-writer",
        )
        .await
        .expect("make checkout genuinely active")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected writer outcome: {other:?}"),
    };

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
        .expect_err("a second Session must not execute in an actively owned checkout");
    assert!(
        matches!(
            error,
            SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
                ref owner_session_id,
                ref owner_branch_id,
                ..
            } if owner_session_id == &work_key.session_id && owner_branch_id == &work_key.branch_id
        ),
        "unexpected claim failure: {error:?}"
    );
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
    coordinator.release_writer(&active_writer).await.unwrap();

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
            .expect("clean execution claim fixture");
        }
        for table in ["agent_session_lifecycle_fences", "agent_sessions"] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
            ))
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .execute(pool.get())
            .await
            .expect("clean durable checkout owner");
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
