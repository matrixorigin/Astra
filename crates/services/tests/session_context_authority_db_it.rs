mod common;

use std::time::Duration;

use astra_services::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, ReserveTurnOutcome,
    SessionContextCoordinator, SessionContextCoordinatorError, SessionExecutionBindingStateV1,
    SessionExecutionBindingV1,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, ContextManifestNodeV1, ConversationSegmentV1,
    CoordinatorMutationV1, SessionKeyV1, SessionSurfaceV1,
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
