mod common;

use std::time::Duration;

use astra_services::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, ReserveTurnOutcome,
    SessionContextCoordinator, SessionContextCoordinatorError,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, ContextManifestNodeV1, ConversationSegmentV1,
    CoordinatorMutationV1, ResumeProviderProjectionV1, SessionKeyV1, SessionSurfaceV1,
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
        .reserve_turn(&lease, None, Duration::from_secs(30), "reserve")
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
async fn provider_projection_survives_reopen_and_replacement_fails_closed_on_tamper() {
    let pool = common::setup_pool().await;
    let owner_id = format!("provider-projection-owner-{}", Uuid::new_v4());
    let session_id = format!("provider-projection-session-{}", Uuid::new_v4());
    let key = SessionKeyV1::owner_session("server", &owner_id, &session_id, "main");
    let actor = ActorContextV1::owner_user(
        &owner_id,
        "provider-projection-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());

    let lease = match coordinator
        .acquire_writer(&key, None, &actor, Duration::from_secs(60), "acquire")
        .await
        .expect("acquire writer")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("unexpected writer outcome: {other:?}"),
    };
    let first_reservation = match coordinator
        .reserve_turn(&lease, None, Duration::from_secs(60), "reserve-first")
        .await
        .expect("reserve first turn")
    {
        ReserveTurnOutcome::Reserved(reservation) => reservation,
        other => panic!("unexpected first reservation outcome: {other:?}"),
    };
    let first_projection = ResumeProviderProjectionV1 {
        offering_id: Some("offering-runner-primary".into()),
        model: Some("private-model-primary".into()),
        permission_mode: Some("default".into()),
        config_version_id: Some("provider-config-primary".into()),
    };
    let first_messages = vec![serde_json::json!({
        "role": "user",
        "content": "first provider turn"
    })];
    let first_cursor = match coordinator
        .commit_turn(
            &first_reservation,
            CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                completed_turn: 1,
                journal_event_seq: 1,
                conversation_seq: 1,
                compaction_generation: 0,
                config_version_id: None,
                provider_projection: Some(first_projection.clone()),
                mode: CanonicalDeltaModeV1::Append,
                logical_segments: vec![first_messages.clone()],
            },
            "commit-first-provider",
        )
        .await
        .expect("commit first provider projection")
    {
        CoordinatorMutationV1::Applied { cursor } => cursor,
        other => panic!("unexpected first commit outcome: {other:?}"),
    };

    // A fresh coordinator instance must recover the exact causal Offering from
    // the durable head/manifest pair; no in-memory state is allowed to fill it
    // in from the display model.
    let reopened = DatabaseSessionContextCoordinator::new(pool.clone());
    let first_head = reopened
        .load_head(&key)
        .await
        .expect("load head after reopen")
        .expect("first head");
    assert_eq!(
        first_head.provider_projection,
        Some(first_projection.clone())
    );
    let first_materialized = reopened
        .materialize(&first_head)
        .await
        .expect("materialize exact provider projection after reopen");
    assert_eq!(
        first_materialized.head.provider_projection,
        Some(first_projection)
    );
    assert_eq!(first_materialized.messages, first_messages);

    // A replacement turn is a new causal boundary. Its exact Offering must
    // replace, rather than accidentally inherit, the previous identity.
    let second_reservation = match reopened
        .reserve_turn(
            &lease,
            Some(&first_cursor),
            Duration::from_secs(60),
            "reserve-second",
        )
        .await
        .expect("reserve replacement turn")
    {
        ReserveTurnOutcome::Reserved(reservation) => reservation,
        other => panic!("unexpected second reservation outcome: {other:?}"),
    };
    let second_projection = ResumeProviderProjectionV1 {
        offering_id: Some("offering-runner-replacement".into()),
        model: Some("private-model-replacement".into()),
        permission_mode: Some("restricted".into()),
        config_version_id: Some("provider-config-replacement".into()),
    };
    let second_messages = vec![serde_json::json!({
        "role": "assistant",
        "content": "replacement provider turn"
    })];
    let second_cursor = match reopened
        .commit_turn(
            &second_reservation,
            CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                completed_turn: 2,
                journal_event_seq: 2,
                conversation_seq: 2,
                compaction_generation: 1,
                config_version_id: None,
                provider_projection: Some(second_projection.clone()),
                mode: CanonicalDeltaModeV1::Replace,
                logical_segments: vec![second_messages.clone()],
            },
            "commit-replacement-provider",
        )
        .await
        .expect("commit replacement provider projection")
    {
        CoordinatorMutationV1::Applied { cursor } => cursor,
        other => panic!("unexpected replacement commit outcome: {other:?}"),
    };
    assert_eq!(second_cursor.compaction_generation, 1);

    let reopened_after_replacement = DatabaseSessionContextCoordinator::new(pool.clone());
    let second_head = reopened_after_replacement
        .load_head(&key)
        .await
        .expect("load replacement head after reopen")
        .expect("replacement head");
    assert_eq!(
        second_head.provider_projection,
        Some(second_projection.clone())
    );
    let second_materialized = reopened_after_replacement
        .materialize(&second_head)
        .await
        .expect("materialize replacement projection after reopen");
    assert_eq!(
        second_materialized.head.provider_projection,
        Some(second_projection)
    );
    assert_eq!(second_materialized.messages, second_messages);

    // Keep the tampered Offering syntactically valid so this exercises the
    // causal head/manifest equality check rather than the shape validator.
    let stored_head_json: String = sqlx::query_scalar(
        "SELECT head_json FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load stored replacement head JSON");
    let mut tampered: serde_json::Value =
        serde_json::from_str(&stored_head_json).expect("decode stored head JSON");
    tampered["provider_projection"]["offering_id"] = serde_json::json!("offering-runner-tampered");
    sqlx::query(
        "UPDATE session_context_heads SET head_json = ?
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(serde_json::to_string(&tampered).expect("encode tampered head JSON"))
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(pool.get())
    .await
    .expect("tamper head projection");

    let tampered_coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let tampered_head = tampered_coordinator
        .load_head(&key)
        .await
        .expect("load syntactically valid tampered head")
        .expect("tampered head");
    let tamper_error = tampered_coordinator
        .materialize(&tampered_head)
        .await
        .expect_err("a stale/tampered head Offering must not become resume authority");
    assert!(matches!(
        tamper_error,
        SessionContextCoordinatorError::NeedsRepair(reason)
            if reason == "context head provider projection does not match its canonical manifest"
    ));

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
        .expect("clean provider projection fixture");
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
        .reserve_turn(&lease, None, Duration::from_secs(30), "reserve")
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
                provider_projection: None,
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
