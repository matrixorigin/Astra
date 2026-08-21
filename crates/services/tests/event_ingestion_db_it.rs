//! Live MatrixOne tests for event ingestion idempotency and concurrency.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test event_ingestion_db_it -- --ignored
//! ```
//!
//! Or via: `make test-online`

use astra_services::auth::session::{DatabaseSessionService, SessionService};
use astra_services::config_version_cloud::{CONFIG_VERSIONS_SELECT_TOML_SQL, ConfigVersionPayload};
use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionEvent};
use astra_services::events::{
    DatabaseEventService, EventCreateRequestData, EventIngestionSource, EventService,
};
use astra_services::storage::{insert_agent_event_edges, load_agent_event_parent_ids};
use axum::http::StatusCode;
use sqlx::Row;
use uuid::Uuid;

mod common;

const TEST_USER_ID: &str = "test-user";

fn test_event(event_id: &str, session_id: &str, event_type: &str) -> IngestionEvent {
    test_event_for_user(TEST_USER_ID, event_id, session_id, event_type)
}

fn config_version_fixture_id() -> String {
    let uuid_hex = Uuid::new_v4().simple().to_string();
    format!("cfg_{}", &uuid_hex[..20])
}

fn test_event_for_user(
    user_id: &str,
    event_id: &str,
    session_id: &str,
    event_type: &str,
) -> IngestionEvent {
    IngestionEvent {
        event_id: event_id.to_string(),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        event_type: event_type.to_string(),
        content: None,
        token_usage: None,
        llm_model_used: None,
        skill_name: None,
        metadata: None,
        created_at: "2025-01-15T10:30:00Z".to_string(),
        parent_event_id: None,
        parent_event_ids: vec![],
        causal_chain_id: None,
        history_work_queue_reservation: None,
    }
}

async fn insert_session_root(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str) {
    insert_session_root_with_count(pool, user_id, session_id, 0).await;
}

async fn insert_session_root_with_count(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    event_count: i64,
) {
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'event-ingestion-test', 'active', ?)",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(event_count)
    .execute(pool)
    .await
    .expect("insert session root");
}

async fn assert_session_event_count(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    expected: i64,
) {
    let row =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("load session event_count");
    let actual: i64 = row.get("event_count");
    assert_eq!(
        actual, expected,
        "agent_sessions.event_count for {user_id}/{session_id}"
    );
}

async fn cleanup_session(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str) {
    let _ = sqlx::query("DELETE FROM agent_event_edges WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
    let event_rows =
        sqlx::query("SELECT event_id FROM agent_events WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .fetch_all(pool)
            .await;
    if let Ok(event_rows) = event_rows {
        for row in event_rows {
            let event_id: String = row.get("event_id");
            let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ? AND user_id = ?")
                .bind(&event_id)
                .bind(user_id)
                .execute(pool)
                .await;
        }
    }
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM agent_session_lifecycle_fences WHERE session_id = ? AND user_id = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await;
}

async fn cleanup_config_version(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, version_id: &str) {
    let _ = sqlx::query("DELETE FROM config_versions WHERE user_id = ? AND version_id = ?")
        .bind(user_id)
        .bind(version_id)
        .execute(pool)
        .await;
}

/// Verifies that inserting the same event_id twice does not surface a
/// duplicate key error. This guards against the MySQL 1062 error that
/// occurred when INSERT IGNORE was not properly handling idempotent retries.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_idempotent_duplicate_key_no_error() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let event_id = format!("evt-test-{}", Uuid::new_v4());
    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    insert_session_root(&pool, TEST_USER_ID, &session_id).await;
    let event = test_event(&event_id, &session_id, "test_idempotent");

    // Spawn worker, send event, shutdown — first insert
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(event.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    // Spawn worker again, send same event — INSERT IGNORE should make it idempotent
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(event.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    // Verify only one row exists
    let row =
        sqlx::query("SELECT COUNT(*) AS cnt FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&event_id)
            .bind(TEST_USER_ID)
            .fetch_one(&pool)
            .await
            .expect("count query");
    let count: i64 = row.get("cnt");
    assert_eq!(
        count, 1,
        "expected exactly 1 row for event_id {event_id}, got {count}"
    );
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 1).await;

    // Cleanup
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn queued_ingestion_cannot_resurrect_a_hard_deleted_session() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("delete-fence-user-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4().to_string();
    let event_id = format!("queued-after-delete-{}", Uuid::new_v4().simple());
    cleanup_session(&pool, &user_id, &session_id).await;
    insert_session_root(&pool, &user_id, &session_id).await;

    let config = IngestionConfig {
        batch_size: 20,
        flush_interval_secs: 300,
        channel_capacity: 8,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender
        .enqueue_async(test_event_for_user(
            &user_id,
            &event_id,
            &session_id,
            "queued_before_delete",
        ))
        .await;

    let session_service = DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
        .with_pool(shared.clone());
    session_service
        .delete_session(session_id.clone(), user_id.clone())
        .await
        .expect("hard delete session while ingestion event remains queued");

    shutdown.signal();
    sender.shutdown();
    handle.await.expect("drain queued ingestion after delete");

    let session_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("count deleted session roots");
    let event_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&event_id)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("count rejected queued event");
    let completed_fence: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_session_lifecycle_fences
         WHERE session_id = ? AND user_id = ?
           AND delete_requested_at IS NOT NULL AND database_deleted_at IS NOT NULL",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load durable completed deletion fence");
    assert_eq!(
        session_rows, 0,
        "queued ingestion must not recreate the root"
    );
    assert_eq!(
        event_rows, 0,
        "queued ingestion must roll back its event row"
    );
    assert_eq!(
        completed_fence, 1,
        "deletion fence must survive root removal"
    );
    assert_eq!(
        stats
            .lock()
            .expect("ingestion stats")
            .events_dropped_permanent,
        1,
        "the deletion-fenced event is permanently invalid, not retryable"
    );

    cleanup_session(&pool, &user_id, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn direct_event_api_rejects_a_session_with_a_pending_delete_fence() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("direct-delete-fence-user-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, &user_id, &session_id).await;
    insert_session_root(&pool, &user_id, &session_id).await;
    let mut tx = pool.begin().await.expect("begin lifecycle fence fixture");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &user_id,
        0,
        None,
    )
    .await
    .expect("create lifecycle fence fixture");
    tx.commit().await.expect("commit lifecycle fence fixture");
    sqlx::query(
        "UPDATE agent_session_lifecycle_fences
         SET delete_requested_at = NOW(6)
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("mark pending session delete");

    let event_service =
        DatabaseEventService::new(astra_core::MatrixOneSettings::from_env()).with_pool(shared);
    let error = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                ingestion_source: EventIngestionSource::Client,
                event_id: None,
                session_id: session_id.clone(),
                event_type: "write_during_delete".to_string(),
                content: "must not persist".to_string(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: None,
            },
        )
        .await
        .expect_err("a pending delete fence must reject the direct event API");
    assert_eq!(error.0, StatusCode::NOT_FOUND);

    let event_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("count rejected direct events");
    assert_eq!(event_rows, 0, "the rejected event must not persist");

    cleanup_session(&pool, &user_id, &session_id).await;
}

/// Verifies that concurrent writes with the same event_id both succeed
/// without surfacing duplicate key errors to the caller.
///
/// Uses a multi-threaded runtime with a Barrier so both workers actually
/// race their INSERT IGNORE at the same time — proving the DB layer
/// handles the collision gracefully.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_concurrent_duplicate_key_no_error() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let event_id = format!("evt-test-{}", Uuid::new_v4());
    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    insert_session_root(&pool, TEST_USER_ID, &session_id).await;
    let event = test_event(&event_id, &session_id, "test_concurrent");

    let config = IngestionConfig::default();

    // Barrier ensures both workers actually race their INSERT, rather than
    // one completing before the other starts (which would mask races).
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    let pool1 = pool.clone();
    let event1 = event.clone();
    let b1 = barrier.clone();
    let handle1 = tokio::spawn(async move {
        let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool1, config);
        sender.enqueue_async(event1).await;
        b1.wait().await;
        shutdown.signal();
        handle.await.unwrap();
    });

    let pool2 = pool.clone();
    let event2 = event.clone();
    let config = IngestionConfig::default();
    let b2 = barrier.clone();
    let handle2 = tokio::spawn(async move {
        let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool2, config);
        sender.enqueue_async(event2).await;
        b2.wait().await;
        shutdown.signal();
        handle.await.unwrap();
    });

    // Both should complete without panicking — INSERT IGNORE handles the race
    let (r1, r2) = tokio::join!(handle1, handle2);
    r1.expect("first concurrent worker panicked");
    r2.expect("second concurrent worker panicked");

    // Verify only one row exists (INSERT IGNORE deduplicates)
    let row =
        sqlx::query("SELECT COUNT(*) AS cnt FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&event_id)
            .bind(TEST_USER_ID)
            .fetch_one(&pool)
            .await
            .expect("count query");
    let count: i64 = row.get("cnt");
    assert_eq!(
        count, 1,
        "expected exactly 1 row for event_id {event_id}, got {count}"
    );
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 1).await;

    // Cleanup
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_same_event_id_isolated_by_user() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let event_id = format!("evt-cross-user-{}", Uuid::new_v4());
    let user_a = "test-user-a";
    let user_b = "test-user-b";
    let session_a = Uuid::new_v4().to_string();
    let session_b = Uuid::new_v4().to_string();
    cleanup_session(&pool, user_a, &session_a).await;
    cleanup_session(&pool, user_b, &session_b).await;
    insert_session_root(&pool, user_a, &session_a).await;
    insert_session_root(&pool, user_b, &session_b).await;

    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender
        .enqueue_async(test_event_for_user(
            user_a,
            &event_id,
            &session_a,
            "test_cross_user",
        ))
        .await;
    sender
        .enqueue_async(test_event_for_user(
            user_b,
            &event_id,
            &session_b,
            "test_cross_user",
        ))
        .await;
    shutdown.signal();
    handle.await.unwrap();

    for (user_id, session_id) in [(user_a, &session_a), (user_b, &session_b)] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE event_id = ? AND user_id = ?",
        )
        .bind(&event_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("count cross-user event");
        assert_eq!(
            count, 1,
            "same event_id must be idempotent per user, not global"
        );
        assert_session_event_count(&pool, user_id, session_id, 1).await;
    }

    cleanup_session(&pool, user_a, &session_a).await;
    cleanup_session(&pool, user_b, &session_b).await;
}

/// Verifies that batch insert with multiple events succeeds and
/// re-inserting a subset does not error.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_batch_partial_duplicate_no_error() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    insert_session_root(&pool, TEST_USER_ID, &session_id).await;
    let event1 = test_event(
        &format!("evt-batch1-{}", Uuid::new_v4()),
        &session_id,
        "test_batch",
    );
    let event2 = test_event(
        &format!("evt-batch2-{}", Uuid::new_v4()),
        &session_id,
        "test_batch",
    );

    // Insert batch of 2
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(event1.clone()).await;
    sender.enqueue_async(event2.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    // Re-insert subset (event1 only) — should not error
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(event1.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    // Verify both rows exist
    for event in [&event1, &event2] {
        let row = sqlx::query(
            "SELECT COUNT(*) AS cnt FROM agent_events WHERE event_id = ? AND user_id = ?",
        )
        .bind(&event.event_id)
        .bind(TEST_USER_ID)
        .fetch_one(&pool)
        .await
        .expect("count query");
        let count: i64 = row.get("cnt");
        assert_eq!(count, 1, "expected 1 row for {}", event.event_id);
    }
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 2).await;

    // Cleanup
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_closes_session_only_for_inserted_session_end() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    insert_session_root(&pool, TEST_USER_ID, &session_id).await;

    let duplicate_event_id = format!("evt-session-end-dup-{}", Uuid::new_v4());
    let existing = test_event(&duplicate_event_id, &session_id, "ordinary_event");
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(existing).await;
    shutdown.signal();
    handle.await.unwrap();
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 1).await;

    let ignored_session_end = test_event(&duplicate_event_id, &session_id, "session_end");
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(ignored_session_end).await;
    shutdown.signal();
    handle.await.unwrap();

    let status: String = sqlx::query_scalar(
        "SELECT status FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(TEST_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("load session status after ignored session_end");
    assert_eq!(
        status, "active",
        "ignored duplicate session_end input must not close the session"
    );
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 1).await;

    let inserted_session_end = test_event(
        &format!("evt-session-end-new-{}", Uuid::new_v4()),
        &session_id,
        "session_end",
    );
    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(inserted_session_end).await;
    shutdown.signal();
    handle.await.unwrap();

    let status: String = sqlx::query_scalar(
        "SELECT status FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(TEST_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("load session status after inserted session_end");
    assert_eq!(status, "ended");
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 2).await;

    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_config_version_dual_writes_config_versions_once() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let user_id = format!("test-user-{}", Uuid::new_v4());
    let session_id = Uuid::new_v4().to_string();
    let version_id = config_version_fixture_id();
    cleanup_session(&pool, &user_id, &session_id).await;
    cleanup_config_version(&pool, &user_id, &version_id).await;
    insert_session_root(&pool, &user_id, &session_id).await;

    let row = ConfigVersionPayload {
        version_id: version_id.clone(),
        user_id: user_id.clone(),
        toml_body: format!("model = \"worker-config\"\n# {}\n", "x".repeat(70_000)),
        first_seen_session: Some(session_id.clone()),
    };
    let event = IngestionEvent::for_config_version(&row).expect("config version event");

    let config = IngestionConfig {
        batch_size: 20,
        flush_interval_secs: 300,
        channel_capacity: 8,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(event.clone()).await;
    sender.enqueue_async(event.clone()).await;
    shutdown.signal();
    sender.shutdown();
    handle.await.expect("config version ingestion worker join");

    {
        let stats = stats.lock().expect("config version ingestion stats");
        assert!(
            stats.last_error.is_none(),
            "config version ingestion must not record MatrixOne errors: {:?}",
            stats.last_error
        );
    }

    let config_toml: String = sqlx::query_scalar(CONFIG_VERSIONS_SELECT_TOML_SQL)
        .bind(&user_id)
        .bind(&version_id)
        .fetch_one(&pool)
        .await
        .expect("load config version body");
    assert_eq!(config_toml, row.toml_body);

    let config_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM config_versions WHERE user_id = ? AND version_id = ?",
    )
    .bind(&user_id)
    .bind(&version_id)
    .fetch_one(&pool)
    .await
    .expect("count config version rows");
    assert_eq!(
        config_rows, 1,
        "duplicate config events must be idempotent in config_versions"
    );

    let event_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&version_id)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("count agent event rows");
    assert_eq!(
        event_rows, 1,
        "duplicate config events must be idempotent in agent_events"
    );
    assert_session_event_count(&pool, &user_id, &session_id, 1).await;

    cleanup_config_version(&pool, &user_id, &version_id).await;
    cleanup_session(&pool, &user_id, &session_id).await;
}

/// Verifies that one mixed batch updates each session from its own inserted
/// rows, while lazily creating missing session roots.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_multi_session_batch_uses_per_session_insert_delta_and_lazy_roots() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let session_a = Uuid::new_v4().to_string();
    let session_b = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_a).await;
    cleanup_session(&pool, TEST_USER_ID, &session_b).await;

    let duplicate_a = test_event(
        &format!("evt-lazy-dup-{}", Uuid::new_v4()),
        &session_a,
        "test_multi_session",
    );
    let unique_a = test_event(
        &format!("evt-lazy-a-{}", Uuid::new_v4()),
        &session_a,
        "test_multi_session",
    );
    let unique_b1 = test_event(
        &format!("evt-lazy-b1-{}", Uuid::new_v4()),
        &session_b,
        "test_multi_session",
    );
    let unique_b2 = test_event(
        &format!("evt-lazy-b2-{}", Uuid::new_v4()),
        &session_b,
        "test_multi_session",
    );

    let config = IngestionConfig {
        batch_size: 50,
        flush_interval_secs: 300,
        channel_capacity: 8,
        ..Default::default()
    };
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(duplicate_a.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    assert_session_event_count(&pool, TEST_USER_ID, &session_a, 1).await;

    let config = IngestionConfig {
        batch_size: 50,
        flush_interval_secs: 300,
        channel_capacity: 8,
        ..Default::default()
    };
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    for event in [
        duplicate_a.clone(),
        unique_a.clone(),
        unique_b1.clone(),
        unique_b2.clone(),
    ] {
        sender.enqueue_async(event).await;
    }
    shutdown.signal();
    handle.await.unwrap();

    assert_session_event_count(&pool, TEST_USER_ID, &session_a, 2).await;
    assert_session_event_count(&pool, TEST_USER_ID, &session_b, 2).await;

    for (session_id, expected) in [(&session_a, 2_i64), (&session_b, 2_i64)] {
        let actual: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(TEST_USER_ID)
        .fetch_one(&pool)
        .await
        .expect("count session events");
        assert_eq!(actual, expected, "persisted agent_events for {session_id}");
    }

    cleanup_session(&pool, TEST_USER_ID, &session_a).await;
    cleanup_session(&pool, TEST_USER_ID, &session_b).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_drops_foreign_owned_session_without_blocking_valid_events() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let foreign_session_id = Uuid::new_v4().to_string();
    let valid_session_id = Uuid::new_v4().to_string();
    let foreign_user_id = format!("foreign-user-{}", Uuid::new_v4());
    cleanup_session(&pool, TEST_USER_ID, &foreign_session_id).await;
    cleanup_session(&pool, &foreign_user_id, &foreign_session_id).await;
    cleanup_session(&pool, TEST_USER_ID, &valid_session_id).await;
    insert_session_root_with_count(&pool, &foreign_user_id, &foreign_session_id, 7).await;
    insert_session_root(&pool, TEST_USER_ID, &valid_session_id).await;

    let foreign_event_id = format!("evt-foreign-session-{}", Uuid::new_v4());
    let valid_event_id = format!("evt-valid-session-{}", Uuid::new_v4());
    let foreign_event = test_event_for_user(
        TEST_USER_ID,
        &foreign_event_id,
        &foreign_session_id,
        "test_foreign_session_rejected",
    );
    let valid_event = test_event_for_user(
        TEST_USER_ID,
        &valid_event_id,
        &valid_session_id,
        "test_valid_session_persists",
    );
    let config = IngestionConfig::default();
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(foreign_event).await;
    sender.enqueue_async(valid_event).await;
    shutdown.signal();
    handle.await.unwrap();

    let test_user_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE session_id = ? AND user_id = ?",
    )
    .bind(&foreign_session_id)
    .bind(TEST_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("count test-user events");
    assert_eq!(
        test_user_event_count, 0,
        "foreign-owned session must reject test-user event rows"
    );

    let test_user_session_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&foreign_session_id)
    .bind(TEST_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("count test-user sessions");
    assert_eq!(
        test_user_session_count, 0,
        "foreign-owned session must not create a test-user session root"
    );
    assert_session_event_count(&pool, &foreign_user_id, &foreign_session_id, 7).await;
    assert_session_event_count(&pool, TEST_USER_ID, &valid_session_id, 1).await;

    let valid_event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&valid_event_id)
            .bind(TEST_USER_ID)
            .fetch_one(&pool)
            .await
            .expect("count valid event");
    assert_eq!(
        valid_event_count, 1,
        "valid event in the same flush must persist even when another session group is invalid"
    );

    let stats = stats.lock().expect("stats lock").clone();
    assert_eq!(stats.events_flushed, 2);
    assert_eq!(stats.events_dropped_permanent, 1);
    assert_eq!(stats.flush_count, 1);
    assert_eq!(stats.errors, 1);
    assert!(
        stats
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("permanently invalid ingestion events")),
        "unexpected ingestion error: {:?}",
        stats.last_error
    );

    cleanup_session(&pool, &foreign_user_id, &foreign_session_id).await;
    cleanup_session(&pool, TEST_USER_ID, &valid_session_id).await;
}

/// Verifies that a duplicate event in a mixed batch cannot mutate causal
/// edges just because another event in the same session was inserted.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_parent_edges_only_for_rows_inserted_in_this_flush() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    insert_session_root(&pool, TEST_USER_ID, &session_id).await;

    let duplicate_event_id = format!("evt-edge-dup-{}", Uuid::new_v4());
    let unique_event_id = format!("evt-edge-new-{}", Uuid::new_v4());
    let first = test_event(&duplicate_event_id, &session_id, "test_edge");

    let config = IngestionConfig::default();
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(first.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    let mut duplicate_with_parent = first;
    duplicate_with_parent.parent_event_id = Some(format!("parent-stale-{}", Uuid::new_v4()));

    let mut unique_with_parent = test_event(&unique_event_id, &session_id, "test_edge");
    let unique_parent_id = format!("parent-new-{}", Uuid::new_v4());
    unique_with_parent.parent_event_id = Some(unique_parent_id.clone());

    let config = IngestionConfig {
        batch_size: 50,
        flush_interval_secs: 300,
        channel_capacity: 8,
        ..Default::default()
    };
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(duplicate_with_parent).await;
    sender.enqueue_async(unique_with_parent).await;
    shutdown.signal();
    handle.await.unwrap();

    let duplicate_edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_event_edges WHERE user_id = ? AND child_event_id = ?",
    )
    .bind(TEST_USER_ID)
    .bind(&duplicate_event_id)
    .fetch_one(&pool)
    .await
    .expect("count duplicate edges");
    assert_eq!(
        duplicate_edges, 0,
        "duplicate event rows must not gain parent edges from a later ignored retry"
    );

    let unique_parent: Option<String> = sqlx::query_scalar(
        "SELECT parent_event_id FROM agent_event_edges WHERE user_id = ? AND child_event_id = ?",
    )
    .bind(TEST_USER_ID)
    .bind(&unique_event_id)
    .fetch_optional(&pool)
    .await
    .expect("load unique edge");
    assert_eq!(unique_parent.as_deref(), Some(unique_parent_id.as_str()));
    assert_session_event_count(&pool, TEST_USER_ID, &session_id, 2).await;

    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn agent_event_edges_concurrent_inserts_preserve_distinct_parents() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    insert_session_root(&pool, TEST_USER_ID, &session_id).await;

    let child_event_id = format!("evt-edge-child-{}", Uuid::new_v4());
    let parent_a = format!("evt-edge-parent-a-{}", Uuid::new_v4());
    let parent_b = format!("evt-edge-parent-b-{}", Uuid::new_v4());

    let pool_a = pool.clone();
    let session_a = session_id.clone();
    let child_a = child_event_id.clone();
    let parent_a_task = parent_a.clone();
    let write_a = tokio::spawn(async move {
        insert_agent_event_edges(
            &pool_a,
            TEST_USER_ID,
            &session_a,
            &child_a,
            Some(&parent_a_task),
            &[],
        )
        .await
    });

    let pool_b = pool.clone();
    let session_b = session_id.clone();
    let child_b = child_event_id.clone();
    let parent_b_task = parent_b.clone();
    let write_b = tokio::spawn(async move {
        insert_agent_event_edges(
            &pool_b,
            TEST_USER_ID,
            &session_b,
            &child_b,
            Some(&parent_b_task),
            &[],
        )
        .await
    });

    write_a.await.expect("edge insert task a").expect("edge a");
    write_b.await.expect("edge insert task b").expect("edge b");

    let mut by_child =
        load_agent_event_parent_ids(&pool, TEST_USER_ID, std::slice::from_ref(&child_event_id))
            .await
            .expect("load event parents");
    let mut actual = by_child
        .remove(&child_event_id)
        .expect("child should have parent edges");
    actual.sort();

    let mut expected = vec![parent_a, parent_b];
    expected.sort();
    assert_eq!(
        actual, expected,
        "concurrent inserts for distinct parents must not drop edges"
    );

    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}
