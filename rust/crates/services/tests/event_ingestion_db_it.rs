//! Live MatrixOne tests for event ingestion idempotency and concurrency.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test event_ingestion_db_it -- --ignored
//! ```
//!
//! Or via: `make test-online`

use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionEvent};
use sqlx::Row;
use uuid::Uuid;

mod common;

fn test_event(event_id: &str, session_id: &str, event_type: &str) -> IngestionEvent {
    IngestionEvent {
        event_id: event_id.to_string(),
        session_id: session_id.to_string(),
        user_id: "test-user".to_string(),
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
    }
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
    let row = sqlx::query("SELECT COUNT(*) AS cnt FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .fetch_one(&pool)
        .await
        .expect("count query");
    let count: i64 = row.get("cnt");
    assert_eq!(
        count, 1,
        "expected exactly 1 row for event_id {event_id}, got {count}"
    );

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;
}

/// Verifies that concurrent writes with the same event_id both succeed
/// without surfacing duplicate key errors to the caller.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_concurrent_duplicate_key_no_error() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let event_id = format!("evt-test-{}", Uuid::new_v4());
    let session_id = Uuid::new_v4().to_string();
    let event = test_event(&event_id, &session_id, "test_concurrent");

    let config = IngestionConfig::default();

    // Spawn two workers concurrently on the same pool
    let pool1 = pool.clone();
    let event1 = event.clone();
    let handle1 = tokio::spawn(async move {
        let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool1, config);
        sender.enqueue_async(event1).await;
        shutdown.signal();
        handle.await.unwrap();
    });

    let pool2 = pool.clone();
    let event2 = event.clone();
    let config = IngestionConfig::default();
    let handle2 = tokio::spawn(async move {
        let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool2, config);
        sender.enqueue_async(event2).await;
        shutdown.signal();
        handle.await.unwrap();
    });

    // Both should complete without panicking
    let (r1, r2) = tokio::join!(handle1, handle2);
    r1.expect("first concurrent worker panicked");
    r2.expect("second concurrent worker panicked");

    // Verify only one row exists (INSERT IGNORE deduplicates)
    let row = sqlx::query("SELECT COUNT(*) AS cnt FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .fetch_one(&pool)
        .await
        .expect("count query");
    let count: i64 = row.get("cnt");
    assert_eq!(
        count, 1,
        "expected exactly 1 row for event_id {event_id}, got {count}"
    );

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;
}

/// Verifies that batch insert with multiple events succeeds and
/// re-inserting a subset does not error.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_batch_partial_duplicate_no_error() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
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
        let row = sqlx::query("SELECT COUNT(*) AS cnt FROM agent_events WHERE event_id = ?")
            .bind(&event.event_id)
            .fetch_one(&pool)
            .await
            .expect("count query");
        let count: i64 = row.get("cnt");
        assert_eq!(count, 1, "expected 1 row for {}", event.event_id);
    }

    // Cleanup
    for event in [&event1, &event2] {
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
            .bind(&event.event_id)
            .execute(&pool)
            .await;
    }
}
