//! Real MatrixOne contracts for bounded identity-collision diagnostics.

use astra_services::observation_capture::{
    ObservationCollisionReceipt, ObservationPayloadDomain, canonical_observation_payload_hash,
    record_observation_collisions,
};
use sqlx::Row;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires MatrixOne; creates and removes one randomly named isolated test database"]
async fn schema_rejects_missing_capture_columns_in_existing_table() {
    use sqlx::Connection;
    let mut settings = common::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let mut admin_settings = settings.clone();
    admin_settings.database = catalog.clone();
    let mut admin = sqlx::MySqlConnection::connect(&admin_settings.database_url_with_password())
        .await
        .expect("connect bootstrap catalog");
    settings.database = format!("astra_test_probe_capture_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE `{}`", settings.database))
        .execute(&mut admin)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE `{}`.agent_events (user_id VARCHAR(128) NOT NULL, event_id VARCHAR(128) NOT NULL, PRIMARY KEY(user_id, event_id))",
        settings.database,
    )).execute(&mut admin).await.unwrap();
    // There is deliberately no collision table in this fixture. An empty
    // receipt batch must succeed without issuing a collision statement.
    let mut empty_connection =
        sqlx::MySqlConnection::connect(&settings.database_url_with_password())
            .await
            .unwrap();
    let mut empty_tx = empty_connection.begin().await.unwrap();
    record_observation_collisions(&mut empty_tx, &[])
        .await
        .unwrap();
    empty_tx.rollback().await.unwrap();
    empty_connection.close().await.unwrap();
    let bootstrap = astra_services::storage::ensure_core_schema(&settings, &catalog).await;
    // The only destructive target is the unique database this test created.
    sqlx::query(&format!("DROP DATABASE `{}`", settings.database))
        .execute(&mut admin)
        .await
        .expect("remove isolated schema probe");
    let error = bootstrap.expect_err("missing capture columns must prevent startup");
    assert!(
        error.to_string().contains("payload_hash"),
        "unexpected schema rejection: {error}"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1 and a fresh ASTRA_DATABASE"]
async fn event_api_replays_exact_request_and_rejects_changed_agent_or_lineage() {
    use astra_services::events::{
        DatabaseEventService, EventCreateRequestData, EventIngestionSource, EventService,
    };
    let (pool, settings) = common::setup_pool_and_settings().await;
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status, event_count) VALUES (?, ?, 'active', 0)")
        .bind(&session).bind(&owner).execute(pool.get()).await.unwrap();
    let service = DatabaseEventService::new(settings).with_pool(pool.clone());
    let request = EventCreateRequestData {
        ingestion_source: EventIngestionSource::Client,
        event_id: Some(Uuid::new_v4().to_string()),
        session_id: session.clone(),
        event_type: "capture-test".into(),
        content: "immutable content".into(),
        agent_id: Some("original-agent".into()),
        agent_version: None,
        parent_event_id: None,
        parent_event_ids: None,
        causal_chain_id: None,
        metadata: None,
    };
    assert!(
        !service
            .create_event(owner.clone(), request.clone())
            .await
            .unwrap()
            .idempotent_replay
    );
    assert!(
        service
            .create_event(owner.clone(), request.clone())
            .await
            .unwrap()
            .idempotent_replay
    );
    let mut changed_agent = request.clone();
    changed_agent.agent_id = Some("different-agent".into());
    assert_eq!(
        service
            .create_event(owner.clone(), changed_agent)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::CONFLICT
    );
    let mut changed_parent = request.clone();
    changed_parent.parent_event_id = Some("different-parent".into());
    assert_eq!(
        service
            .create_event(owner.clone(), changed_parent)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(count, 1);
    let collisions: u64 = sqlx::query_scalar("SELECT collision_count FROM observation_identity_collisions WHERE user_id = ? AND identity_kind = 'agent_event' AND identity_id = ?")
        .bind(&owner).bind(&request.event_id).fetch_one(pool.get()).await.unwrap();
    assert_eq!(collisions, 2);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1 and a fresh ASTRA_DATABASE"]
async fn collision_batches_preserve_database_identity_order_expiry_and_rollback() {
    let pool = common::setup_pool().await;
    let owner = format!("batch-{}", Uuid::new_v4());
    // Readback of sequential inserts is the collation oracle: do not assume
    // that this database treats case or trailing spaces as equal or distinct.
    let identities = ["Mixed-key", "mixed-key", "Mixed-key ", "mixed-key "];
    let hashes = ["variant-0", "variant-1", "variant-2", "variant-3"];
    let later = (0..260)
        .map(|index| format!("later-{index}"))
        .collect::<Vec<_>>();
    let mut tx = pool.get().begin().await.unwrap();
    record_observation_collisions(&mut tx, &[]).await.unwrap();
    let empty: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&owner)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(empty, 0);
    for index in 0..4 {
        record_observation_collisions(
            &mut tx,
            &[ObservationCollisionReceipt {
                user_id: &owner,
                domain: ObservationPayloadDomain::AgentEvent,
                identity_id: identities[index],
                session_id: hashes[index],
                stored_payload_hash: hashes[index],
                attempted_payload_hash: hashes[index],
                source: hashes[index],
            }],
        )
        .await
        .unwrap();
    }
    // Make expiry preservation observable even if adjacent statements share a clock tick.
    sqlx::query("UPDATE observation_identity_collisions SET first_seen_at = DATE_SUB(NOW(6), INTERVAL 1 DAY), expires_at = DATE_ADD(NOW(6), INTERVAL 6 DAY) WHERE user_id = ?")
        .bind(&owner).execute(&mut *tx).await.unwrap();
    let select = "SELECT identity_id, collision_count, stored_payload_hash, attempted_payload_hash, session_id, source,
        CAST(first_seen_at AS CHAR) AS first_seen, CAST(expires_at AS CHAR) AS expiry,
        TIMESTAMPDIFF(SECOND, first_seen_at, expires_at) AS retention_seconds
        FROM observation_identity_collisions WHERE user_id = ? AND identity_kind = 'agent_event' ORDER BY identity_id";
    let before = sqlx::query(select)
        .bind(&owner)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let receipts = (0..260)
        .map(|index| ObservationCollisionReceipt {
            user_id: &owner,
            domain: ObservationPayloadDomain::AgentEvent,
            identity_id: identities[index % 4],
            session_id: "changed-session",
            stored_payload_hash: "changed-stored",
            attempted_payload_hash: &later[index],
            source: "changed-source",
        })
        .collect::<Vec<_>>();
    // Three chunks, including equivalent identities on both chunk boundaries.
    record_observation_collisions(&mut tx, &receipts)
        .await
        .unwrap();
    let after = sqlx::query(select)
        .bind(&owner)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(before.len(), after.len());
    for (first, last) in before.iter().zip(&after) {
        assert_eq!(
            last.get::<u64, _>("collision_count"),
            first.get::<u64, _>("collision_count") * 66
        );
        let last_variant = first
            .get::<String, _>("attempted_payload_hash")
            .strip_prefix("variant-")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            last.get::<String, _>("attempted_payload_hash"),
            later[256 + last_variant]
        );
        for column in [
            "identity_id",
            "stored_payload_hash",
            "session_id",
            "source",
            "first_seen",
            "expiry",
        ] {
            assert_eq!(
                first.get::<String, _>(column),
                last.get::<String, _>(column),
                "immutable {column}"
            );
        }
        assert_eq!(last.get::<i64, _>("retention_seconds"), 7 * 24 * 60 * 60);
    }
    // A fresh domain exercises first-insert metadata within the batch itself,
    // using the sequential writes above as the database-collation oracle.
    let manifests = (0..260)
        .map(|index| ObservationCollisionReceipt {
            user_id: &owner,
            domain: ObservationPayloadDomain::ContextManifest,
            identity_id: identities[index % 4],
            session_id: hashes[index % 4],
            stored_payload_hash: hashes[index % 4],
            attempted_payload_hash: &later[index],
            source: hashes[index % 4],
        })
        .collect::<Vec<_>>();
    record_observation_collisions(&mut tx, &manifests)
        .await
        .unwrap();
    let manifest_select = select.replace("'agent_event'", "'context_manifest'");
    let manifest_rows = sqlx::query(&manifest_select)
        .bind(&owner)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(manifest_rows.len(), before.len());
    for (first, last) in before.iter().zip(&manifest_rows) {
        assert_eq!(
            last.get::<u64, _>("collision_count"),
            first.get::<u64, _>("collision_count") * 65
        );
        for column in ["identity_id", "stored_payload_hash", "session_id", "source"] {
            assert_eq!(
                first.get::<String, _>(column),
                last.get::<String, _>(column),
                "first metadata {column}"
            );
        }
        let last_variant = first
            .get::<String, _>("attempted_payload_hash")
            .strip_prefix("variant-")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            last.get::<String, _>("attempted_payload_hash"),
            later[256 + last_variant]
        );
        assert_eq!(last.get::<i64, _>("retention_seconds"), 7 * 24 * 60 * 60);
    }
    let oversized_hash = "x".repeat(81);
    // Reduction cannot hide an invalid first, intermediate, or last receipt.
    for invalid_index in 0..3 {
        let invalid = (0..3)
            .map(|index| ObservationCollisionReceipt {
                user_id: &owner,
                domain: ObservationPayloadDomain::AgentEvent,
                identity_id: "invalid-repeated-key",
                session_id: "session",
                stored_payload_hash: if index == invalid_index {
                    &oversized_hash
                } else {
                    "valid"
                },
                attempted_payload_hash: "attempt",
                source: "failure-test",
            })
            .collect::<Vec<_>>();
        assert!(
            record_observation_collisions(&mut tx, &invalid)
                .await
                .is_err()
        );
    }
    let mut invalid_batch = receipts;
    invalid_batch.push(ObservationCollisionReceipt {
        user_id: &owner,
        domain: ObservationPayloadDomain::AgentEvent,
        identity_id: "invalid-final-chunk",
        session_id: "session",
        stored_payload_hash: &oversized_hash,
        attempted_payload_hash: "attempt",
        source: "failure-test",
    });
    assert!(
        record_observation_collisions(&mut tx, &invalid_batch)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&owner)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(remaining, 0, "rollback must remove every chunk and domain");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1 and a fresh ASTRA_DATABASE"]
async fn collision_receipts_bound_distinct_hashes_and_isolate_owners() {
    let pool = common::setup_pool().await;
    let owner = format!("collision-{}", Uuid::new_v4());
    let other_owner = format!("collision-other-{}", Uuid::new_v4());
    let stored_hash = canonical_observation_payload_hash(
        ObservationPayloadDomain::AgentEvent,
        &serde_json::json!({"original": true}),
    );
    // Keep a small explicit single-receipt concurrency contract before the
    // 1024-attempt workload. The opt-in scale path keeps one transaction per attempt.
    let scale = std::env::var("ASTRA_TEST_STORAGE_SCALE").as_deref() == Ok("1");
    let mut singles = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let pool = pool.clone();
        let owner = owner.clone();
        let stored_hash = stored_hash.clone();
        singles.spawn(async move {
            for _ in 0..2 {
                let mut tx = pool.get().begin().await.unwrap();
                record_observation_collisions(
                    &mut tx,
                    &[ObservationCollisionReceipt {
                        user_id: &owner,
                        domain: ObservationPayloadDomain::AgentEvent,
                        identity_id: "single-id",
                        session_id: "session",
                        stored_payload_hash: &stored_hash,
                        attempted_payload_hash: &stored_hash,
                        source: "observation_capture_db_it",
                    }],
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
            }
        });
    }
    while let Some(result) = singles.join_next().await {
        result.unwrap();
    }
    let single_count: u64 = sqlx::query_scalar("SELECT collision_count FROM observation_identity_collisions WHERE user_id = ? AND identity_id = 'single-id'")
        .bind(&owner).fetch_one(pool.get()).await.unwrap();
    assert_eq!(single_count, 8);
    sqlx::query("DELETE FROM observation_identity_collisions WHERE user_id = ? AND identity_id = 'single-id'")
        .bind(&owner).execute(pool.get()).await.unwrap();
    let write_started = std::time::Instant::now();
    // Separate connections contend on one durable identity. Distinct attacker
    // payloads must increase a counter, never create one receipt per payload.
    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..4 {
        let pool = pool.clone();
        let owner = owner.clone();
        let stored_hash = stored_hash.clone();
        tasks.spawn(async move {
            let hashes = (0..256)
                .map(|sequence| {
                    canonical_observation_payload_hash(
                        ObservationPayloadDomain::AgentEvent,
                        &serde_json::json!({"worker": worker, "sequence": sequence}),
                    )
                })
                .collect::<Vec<_>>();
            for chunk in hashes.chunks(if scale { 1 } else { 128 }) {
                let receipts = chunk
                    .iter()
                    .map(|hash| ObservationCollisionReceipt {
                        user_id: &owner,
                        domain: ObservationPayloadDomain::AgentEvent,
                        identity_id: "shared-id",
                        session_id: "session",
                        stored_payload_hash: &stored_hash,
                        attempted_payload_hash: hash,
                        source: "observation_capture_db_it",
                    })
                    .collect::<Vec<_>>();
                let mut tx = pool.get().begin().await.unwrap();
                record_observation_collisions(&mut tx, &receipts)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    eprintln!(
        "collision workload: attempts=1024 transactions={} workers=4 write_wall={:?}",
        if scale { 1024 } else { 8 },
        write_started.elapsed()
    );
    let mut tx = pool.get().begin().await.unwrap();
    record_observation_collisions(
        &mut tx,
        &[ObservationCollisionReceipt {
            user_id: &other_owner,
            domain: ObservationPayloadDomain::AgentEvent,
            identity_id: "shared-id",
            session_id: "session",
            stored_payload_hash: &stored_hash,
            attempted_payload_hash: &stored_hash,
            source: "observation_capture_db_it",
        }],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let rows = sqlx::query(
        "SELECT collision_count, stored_payload_hash,
                TIMESTAMPDIFF(SECOND, first_seen_at, expires_at) AS retention_seconds
         FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&owner)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "1024 distinct hashes must occupy one row");
    assert_eq!(rows[0].get::<u64, _>("collision_count"), 1024);
    assert_eq!(rows[0].get::<String, _>("stored_payload_hash"), stored_hash);
    assert_eq!(rows[0].get::<i64, _>("retention_seconds"), 7 * 24 * 60 * 60);
    let other_count: u64 = sqlx::query_scalar(
        "SELECT collision_count FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&other_owner)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(other_count, 1);

    sqlx::query("DELETE FROM observation_identity_collisions WHERE user_id IN (?, ?)")
        .bind(&owner)
        .bind(&other_owner)
        .execute(pool.get())
        .await
        .unwrap();
}
