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
use astra_services::storage::{
    admit_session_event_write, insert_agent_event_edges, load_agent_event_parent_ids,
};
use axum::http::StatusCode;
use sqlx::Row;
use uuid::Uuid;

mod common;
#[path = "event_ingestion/ingestion_process.rs"]
mod ingestion_process;

const TEST_USER_ID: &str = "test-user";

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn shared_limiter_workers_recover_from_fences_without_blocking_foreground() {
    use astra_services::event_ingestion::IngestionDbLimiter;
    use astra_services::event_ingestion::measurement::{
        IngestionDeliveryKey, IngestionDeliveryTerminal, IngestionMeasurementSink,
    };
    use std::time::Duration;

    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let owner = format!("shared-limiter-{}", Uuid::new_v4());
    let sessions = ["blocked-a", "blocked-b", "healthy", "foreground"];
    for session in sessions {
        insert_session_root(&pool, &owner, session).await;
    }
    let mut fences = Vec::new();
    for session in &sessions[..2] {
        let mut fence = pool.begin().await.unwrap();
        admit_session_event_write(&mut fence, session, &owner, true)
            .await
            .unwrap();
        fences.push(fence);
    }
    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 8,
        max_retries: 1,
        max_concurrent_session_flushes: 2,
        db_attempt_timeout_secs: 3,
        ..Default::default()
    };
    let limiter = IngestionDbLimiter::new(2);
    let (sender_a, shutdown_a, stats_a, worker_a) =
        EventIngestionWorker::spawn_with_db_limiter(pool.clone(), config.clone(), limiter.clone());
    let (sender_b, shutdown_b, stats_b, worker_b) =
        EventIngestionWorker::spawn_with_db_limiter(pool.clone(), config, limiter);
    let (sink, mut reports) = IngestionMeasurementSink::bounded(3);
    for (key, session) in sessions[..2].iter().enumerate() {
        let (token, _) = sink.try_start(IngestionDeliveryKey(key as u64)).unwrap();
        sender_a.enqueue_observed(
            test_event_for_user(&owner, session, session, "user_query"),
            token,
        );
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while astra_core::sync_poison::recover_mutex_lock(&stats_a).db_attempts_current != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker A occupies both aggregate DB slots");

    let (token, healthy_probe) = sink.try_start(IngestionDeliveryKey(2)).unwrap();
    sender_b.enqueue_observed(
        test_event_for_user(&owner, "healthy", "healthy", "user_query"),
        token,
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while healthy_probe.snapshot().first_dispatched_at.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker B dispatches while A holds the slots");
    // Assert the contention precondition instead of inferring it from a sleep.
    assert_eq!(
        astra_core::sync_poison::recover_mutex_lock(&stats_a).db_attempts_current,
        2
    );
    assert_eq!(
        astra_core::sync_poison::recover_mutex_lock(&stats_b).db_attempts_current,
        0
    );
    assert_eq!(healthy_probe.snapshot().pool_wait, Duration::ZERO);
    assert!(
        reports.try_recv().is_err(),
        "blocked deliveries must not report success or rejection"
    );

    tokio::time::timeout(Duration::from_secs(1), async {
        for index in 0..4 {
            let mut connection = pool.acquire().await.expect("foreground pool acquisition");
            let title = format!("foreground-{index}");
            sqlx::query("UPDATE agent_sessions SET title = ? WHERE user_id = ? AND session_id = ?")
                .bind(&title)
                .bind(&owner)
                .bind("foreground")
                .execute(&mut *connection)
                .await
                .unwrap();
            let actual: String = sqlx::query_scalar(
                "SELECT title FROM agent_sessions WHERE user_id = ? AND session_id = ?",
            )
            .bind(&owner)
            .bind("foreground")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
            assert_eq!(actual, title);
        }
    })
    .await
    .expect("unrelated foreground reads and writes retain shared-pool capacity");
    assert_eq!(
        astra_core::sync_poison::recover_mutex_lock(&stats_a).db_attempts_current,
        2,
        "foreground work completed before either blocked attempt released its slot"
    );
    assert_eq!(
        astra_core::sync_poison::recover_mutex_lock(&stats_b).db_attempts_current,
        0,
        "worker B cannot bypass the aggregate limiter"
    );

    let healthy = tokio::time::timeout(Duration::from_secs(5), reports.recv())
        .await
        .expect("worker B progresses after blocked attempts expire")
        .unwrap();
    assert_eq!(healthy.key, IngestionDeliveryKey(2));
    assert_eq!(
        healthy.terminal,
        IngestionDeliveryTerminal::CommittedInserted
    );
    assert!(healthy.progress.limiter_wait >= Duration::from_millis(10));
    assert!(
        reports.try_recv().is_err(),
        "held fences retain retryable deliveries"
    );
    assert_eq!(
        astra_core::sync_poison::recover_mutex_lock(&stats_a).resident_events_current,
        2
    );

    // Timed-out attempts must return every connection other than the two
    // intentionally held fence transactions, even across separate workers.
    let mut recovered = Vec::new();
    for _ in 0..pool.options().get_max_connections() - 2 {
        recovered.push(
            tokio::time::timeout(Duration::from_secs(2), pool.acquire())
                .await
                .expect("no leaked connection after timeout")
                .unwrap(),
        );
    }
    drop(recovered);
    for fence in fences {
        fence.rollback().await.unwrap();
    }
    shutdown_a.signal();
    shutdown_b.signal();
    sender_a.shutdown();
    sender_b.shutdown();
    tokio::time::timeout(Duration::from_secs(5), async {
        worker_a.await.unwrap();
        worker_b.await.unwrap();
    })
    .await
    .expect("both workers drain after fence release");
    let mut remaining = std::collections::BTreeSet::new();
    while let Ok(report) = reports.try_recv() {
        assert_eq!(
            report.terminal,
            IngestionDeliveryTerminal::CommittedInserted
        );
        assert!(
            remaining.insert(report.key.0),
            "one terminal report per delivery"
        );
    }
    assert_eq!(remaining, std::collections::BTreeSet::from([0, 1]));
    for stats in [&stats_a, &stats_b] {
        let stats = astra_core::sync_poison::recover_mutex_lock(stats);
        assert_eq!(stats.resident_events_current, 0);
        assert_eq!(stats.db_attempts_current, 0);
        assert_eq!(stats.events_abandoned_shutdown, 0);
    }
    for session in &sessions[..3] {
        assert_session_event_count(&pool, &owner, session, 1).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_id = ?")
            .bind(&owner).bind(session).bind(session).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }
    assert_session_event_count(&pool, &owner, "foreground", 0).await;
    for session in sessions {
        cleanup_session(&pool, &owner, session).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn observed_pool_wait_cancellation_retains_elapsed_time_and_unknown_outcome() {
    use astra_services::event_ingestion::measurement::{
        IngestionDeliveryKey, IngestionDeliveryTerminal, IngestionMeasurementSink,
        IngestionUnknownReason,
    };
    use std::time::Duration;
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let mut held = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        held.push(pool.acquire().await.expect("hold every pool connection"));
    }
    let (sender, _, _, worker) = EventIngestionWorker::spawn(
        pool.clone(),
        IngestionConfig {
            batch_size: 1,
            ..Default::default()
        },
    );
    let (sink, mut reports) = IngestionMeasurementSink::bounded(1);
    let (token, probe) = sink.try_start(IngestionDeliveryKey(1)).unwrap();
    sender.enqueue_observed(
        test_event("pool-cancel", "no-db-write", "user_query"),
        token,
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while probe.snapshot().first_dispatched_at.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker dispatched");
    tokio::time::sleep(Duration::from_millis(10)).await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    let report = reports.recv().await.unwrap();
    assert_eq!(
        report.terminal,
        IngestionDeliveryTerminal::Unknown(IngestionUnknownReason::DeliveryDropped)
    );
    assert!(report.progress.pool_wait >= Duration::from_millis(10));
    assert_eq!(report.progress.transaction_time, Duration::ZERO);
    drop(held);
    tokio::time::timeout(Duration::from_secs(1), pool.acquire())
        .await
        .expect("pool remains usable")
        .expect("acquire after cancellation");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn observed_deliveries_distinguish_insertion_replay_collision_and_session_rejection() {
    use astra_services::event_ingestion::measurement::{
        IngestionDeliveryKey, IngestionDeliveryTerminal, IngestionMeasurementSink,
        IngestionRejectionReason,
    };
    use std::time::Duration;

    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("observed-{}", Uuid::new_v4());
    let session_id = Uuid::new_v4().to_string();
    insert_session_root(&pool, &user_id, &session_id).await;
    let config = IngestionConfig {
        batch_size: 4,
        flush_interval_secs: 1,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    let (sink, mut reports) = IngestionMeasurementSink::bounded(5);
    let first = test_event_for_user(&user_id, "same-event", &session_id, "user_query");
    let replay = first.clone();
    let mut collision = first.clone();
    collision.content = Some("conflicting payload".to_string());
    let sibling = test_event_for_user(&user_id, "sibling", &session_id, "user_query");
    for (index, event) in [first, replay, collision, sibling].into_iter().enumerate() {
        let (token, _) = sink.try_start(IngestionDeliveryKey(index as u64)).unwrap();
        sender.enqueue_observed(event, token);
    }
    let mut outcomes = std::collections::BTreeMap::new();
    for _ in 0..4 {
        let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
            .await
            .expect("observed commit deadline")
            .expect("terminal report");
        assert!(report.progress.channel_accepted_at.is_some());
        assert!(report.progress.worker_received_at.is_some());
        assert_eq!(report.progress.attempt_count, 1);
        outcomes.insert(report.key.0, report.terminal);
    }
    assert_eq!(outcomes[&0], IngestionDeliveryTerminal::CommittedInserted);
    assert_eq!(outcomes[&1], IngestionDeliveryTerminal::CommittedReplayed);
    assert_eq!(
        outcomes[&2],
        IngestionDeliveryTerminal::Rejected(IngestionRejectionReason::IdentityCollision)
    );
    assert_eq!(outcomes[&3], IngestionDeliveryTerminal::CommittedInserted);
    assert_session_event_count(&pool, &user_id, &session_id, 2).await;
    let receipts: u64 = sqlx::query_scalar(
        "SELECT collision_count FROM observation_identity_collisions WHERE user_id = ? AND identity_kind = 'agent_event' AND identity_id = 'same-event'",
    ).bind(&user_id).fetch_one(&pool).await.expect("receipt committed before terminal report");
    assert_eq!(receipts, 1);

    sqlx::query(
        "UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("fence deleted session");
    let (token, _) = sink.try_start(IngestionDeliveryKey(4)).unwrap();
    sender.enqueue_observed(
        test_event_for_user(&user_id, "rejected", &session_id, "user_query"),
        token,
    );
    let report = tokio::time::timeout(Duration::from_secs(10), reports.recv())
        .await
        .expect("session rejection deadline")
        .expect("session rejection report");
    assert_eq!(
        report.terminal,
        IngestionDeliveryTerminal::Rejected(IngestionRejectionReason::SessionAdmission)
    );
    assert!(report.progress.channel_accepted_at.is_some());
    assert_session_event_count(&pool, &user_id, &session_id, 2).await;

    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("worker shutdown")
        .expect("worker joined");
    {
        let stats = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(stats.events_unresolved_shutdown, 0);
        assert_eq!(stats.resident_events_current, 0);
    }
    cleanup_session(&pool, &user_id, &session_id).await;
}

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
        ingestion_enqueued_at: None,
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

async fn event_count(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, event_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND event_id = ?")
        .bind(user_id)
        .bind(event_id)
        .fetch_one(pool)
        .await
        .expect("query owner-scoped event count")
}

async fn wait_for_event(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    event_id: &str,
    failure: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while event_count(pool, user_id, event_id).await != 1 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect(failure);
}

async fn cleanup_session(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str) {
    let _ = sqlx::query(
        "DELETE FROM observation_identity_collisions \
         WHERE user_id = ? AND session_id = ? AND identity_kind = 'agent_event'",
    )
    .bind(user_id)
    .bind(session_id)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM agent_event_edges WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_events WHERE session_id = ? AND user_id = ?")
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await;
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
    let _ =
        sqlx::query("DELETE FROM session_deletion_tombstones WHERE session_id = ? AND user_id = ?")
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
async fn blocked_session_fence_does_not_delay_an_unrelated_session() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("ingestion-isolation-user-{}", Uuid::new_v4().simple());
    let blocked_session = format!("a-blocked-{}", Uuid::new_v4().simple());
    let healthy_session = format!("z-healthy-{}", Uuid::new_v4().simple());
    let blocked_event = format!("blocked-event-{}", Uuid::new_v4().simple());
    let healthy_event = format!("healthy-event-{}", Uuid::new_v4().simple());
    insert_session_root(&pool, &user_id, &blocked_session).await;
    insert_session_root(&pool, &user_id, &healthy_session).await;

    let mut fence_holder = pool.begin().await.expect("begin fence holder");
    admit_session_event_write(&mut fence_holder, &blocked_session, &user_id, true)
        .await
        .expect("hold blocked session lifecycle fence");

    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 4,
        max_retries: 1,
        max_concurrent_session_flushes: 2,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender
        .enqueue_async(test_event_for_user(
            &user_id,
            &blocked_event,
            &blocked_session,
            "blocked_trace",
        ))
        .await;
    sender
        .enqueue_async(test_event_for_user(
            &user_id,
            &healthy_event,
            &healthy_session,
            "healthy_trace",
        ))
        .await;

    wait_for_event(
        &pool,
        &user_id,
        &healthy_event,
        "healthy session must commit while the unrelated fence remains held",
    )
    .await;

    let blocked_visible: i64 = event_count(&pool, &user_id, &blocked_event).await;
    assert_eq!(blocked_visible, 0);

    fence_holder
        .rollback()
        .await
        .expect("release blocked session fence");
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("worker shutdown after fence release")
        .expect("worker task");

    let blocked_visible: i64 = event_count(&pool, &user_id, &blocked_event).await;
    assert_eq!(blocked_visible, 1);
    {
        let stats = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(stats.events_flushed, 2, "{stats:?}");
    }

    cleanup_session(&pool, &user_id, &blocked_session).await;
    cleanup_session(&pool, &user_id, &healthy_session).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn late_arriving_session_commits_while_an_earlier_transaction_is_blocked() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("ingestion-late-user-{}", Uuid::new_v4().simple());
    let blocked_session = format!("a-blocked-{}", Uuid::new_v4().simple());
    let late_session = format!("z-late-{}", Uuid::new_v4().simple());
    let blocked_event = format!("blocked-event-{}", Uuid::new_v4().simple());
    let late_event = format!("late-event-{}", Uuid::new_v4().simple());
    insert_session_root(&pool, &user_id, &blocked_session).await;
    insert_session_root(&pool, &user_id, &late_session).await;

    let mut fence_holder = pool.begin().await.expect("begin fence holder");
    admit_session_event_write(&mut fence_holder, &blocked_session, &user_id, true)
        .await
        .expect("hold blocked session lifecycle fence");
    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 4,
        max_concurrent_session_flushes: 2,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender
        .enqueue_async(test_event_for_user(
            &user_id,
            &blocked_event,
            &blocked_session,
            "blocked_first",
        ))
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if astra_core::sync_poison::recover_mutex_lock(&stats).events_received == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker must receive the first event before the late arrival");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    sender
        .enqueue_async(test_event_for_user(
            &user_id,
            &late_event,
            &late_session,
            "late_healthy",
        ))
        .await;
    wait_for_event(
        &pool,
        &user_id,
        &late_event,
        "late session must commit without waiting for the blocked transaction",
    )
    .await;

    let blocked_visible: i64 = event_count(&pool, &user_id, &blocked_event).await;
    assert_eq!(blocked_visible, 0);

    fence_holder
        .rollback()
        .await
        .expect("release blocked fence");
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("scheduler shutdown")
        .expect("scheduler task");
    assert_session_event_count(&pool, &user_id, &blocked_session, 1).await;
    assert_session_event_count(&pool, &user_id, &late_session, 1).await;
    cleanup_session(&pool, &user_id, &blocked_session).await;
    cleanup_session(&pool, &user_id, &late_session).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_flush_concurrency_is_bounded_to_two_transactions() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("ingestion-bound-user-{}", Uuid::new_v4().simple());
    let session_a = format!("a-blocked-{}", Uuid::new_v4().simple());
    let session_b = format!("b-blocked-{}", Uuid::new_v4().simple());
    let session_c = format!("z-healthy-{}", Uuid::new_v4().simple());
    for session_id in [&session_a, &session_b, &session_c] {
        insert_session_root(&pool, &user_id, session_id).await;
    }

    let mut fence_a = pool.begin().await.expect("begin fence A");
    admit_session_event_write(&mut fence_a, &session_a, &user_id, true)
        .await
        .expect("hold session A fence");
    let mut fence_b = pool.begin().await.expect("begin fence B");
    admit_session_event_write(&mut fence_b, &session_b, &user_id, true)
        .await
        .expect("hold session B fence");

    let event_a = format!("event-a-{}", Uuid::new_v4().simple());
    let event_b = format!("event-b-{}", Uuid::new_v4().simple());
    let event_c = format!("event-c-{}", Uuid::new_v4().simple());
    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 4,
        max_retries: 1,
        max_concurrent_session_flushes: 2,
        ..Default::default()
    };
    let (sender, shutdown, _stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    for (event_id, session_id) in [
        (&event_a, &session_a),
        (&event_b, &session_b),
        (&event_c, &session_c),
    ] {
        sender
            .enqueue_async(test_event_for_user(
                &user_id,
                event_id,
                session_id,
                "bounded_trace",
            ))
            .await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let healthy_visible: i64 = event_count(&pool, &user_id, &event_c).await;
    assert_eq!(
        healthy_visible, 0,
        "a third session transaction must wait for one of the two bounded slots"
    );

    fence_a.rollback().await.expect("release session A fence");
    wait_for_event(
        &pool,
        &user_id,
        &event_c,
        "the third group must progress while session B remains blocked",
    )
    .await;

    let blocked_b_visible: i64 = event_count(&pool, &user_id, &event_b).await;
    assert_eq!(blocked_b_visible, 0);

    fence_b.rollback().await.expect("release session B fence");
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("worker shutdown after both fences release")
        .expect("worker task");

    for session_id in [&session_a, &session_b, &session_c] {
        assert_session_event_count(&pool, &user_id, session_id, 1).await;
        cleanup_session(&pool, &user_id, session_id).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn all_ingestion_slots_timeout_without_starving_a_healthy_session_or_leaking_pool_capacity() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("ingestion-timeout-user-{}", Uuid::new_v4().simple());
    let blocked_a = format!("a-blocked-{}", Uuid::new_v4().simple());
    let blocked_b = format!("b-blocked-{}", Uuid::new_v4().simple());
    let healthy = format!("z-healthy-{}", Uuid::new_v4().simple());
    for session_id in [&blocked_a, &blocked_b, &healthy] {
        insert_session_root(&pool, &user_id, session_id).await;
    }

    let mut fence_a = pool.begin().await.expect("begin timeout fence A");
    admit_session_event_write(&mut fence_a, &blocked_a, &user_id, true)
        .await
        .expect("hold timeout fence A");
    let mut fence_b = pool.begin().await.expect("begin timeout fence B");
    admit_session_event_write(&mut fence_b, &blocked_b, &user_id, true)
        .await
        .expect("hold timeout fence B");

    let event_a = format!("timeout-a-{}", Uuid::new_v4().simple());
    let event_b = format!("timeout-b-{}", Uuid::new_v4().simple());
    let healthy_event = format!("timeout-healthy-{}", Uuid::new_v4().simple());
    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 8,
        max_retries: 1,
        max_concurrent_session_flushes: 2,
        db_attempt_timeout_secs: 1,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    for (event_id, session_id) in [(&event_a, &blocked_a), (&event_b, &blocked_b)] {
        sender
            .enqueue_async(test_event_for_user(
                &user_id,
                event_id,
                session_id,
                "timeout_blocked",
            ))
            .await;
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let snapshot = astra_core::sync_poison::recover_mutex_lock(&stats).clone();
            if snapshot.db_attempts_current == 2 && snapshot.db_attempts_peak == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both ingestion attempt slots must be occupied by held fences");

    sender
        .enqueue_async(test_event_for_user(
            &user_id,
            &healthy_event,
            &healthy,
            "timeout_healthy",
        ))
        .await;
    wait_for_event(
        &pool,
        &user_id,
        &healthy_event,
        "healthy session must progress after both blocked attempts time out",
    )
    .await;
    {
        let snapshot = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(snapshot.db_attempts_current, 0, "{snapshot:?}");
        assert_eq!(snapshot.db_attempts_peak, 2, "{snapshot:?}");
        assert_eq!(snapshot.events_flushed, 1, "{snapshot:?}");
        assert_eq!(snapshot.resident_events_current, 2, "{snapshot:?}");
    }

    // The two fence holders consume two of the eight test-pool connections.
    // Acquiring all six remaining permits simultaneously proves timed-out
    // ingestion attempts were detached instead of leaking pool capacity.
    let mut recovered = Vec::new();
    for _ in 0..6 {
        recovered.push(
            tokio::time::timeout(std::time::Duration::from_secs(2), pool.acquire())
                .await
                .expect("timed-out attempt must restore pool capacity")
                .expect("acquire recovered pool connection"),
        );
    }
    drop(recovered);

    fence_a.rollback().await.expect("release timeout fence A");
    fence_b.rollback().await.expect("release timeout fence B");
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("drain retained retry heads after releasing fences")
        .expect("timeout worker task");
    {
        let snapshot = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(snapshot.events_flushed, 3, "{snapshot:?}");
        assert_eq!(snapshot.events_abandoned_shutdown, 0, "{snapshot:?}");
        assert_eq!(snapshot.resident_events_current, 0, "{snapshot:?}");
        assert_eq!(snapshot.db_attempts_current, 0, "{snapshot:?}");
    }
    for session_id in [&blocked_a, &blocked_b, &healthy] {
        assert_session_event_count(&pool, &user_id, session_id, 1).await;
        cleanup_session(&pool, &user_id, session_id).await;
    }
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

/// Verifies that concurrent lazy-root writers with the same event_id both
/// succeed without surfacing duplicate key or false permanent-drop errors.
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
    let event = test_event(&event_id, &session_id, "test_concurrent");

    let config = IngestionConfig::default();

    // Barrier ensures both workers actually race their INSERT, rather than
    // one completing before the other starts (which would mask races).
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    let pool1 = pool.clone();
    let event1 = event.clone();
    let b1 = barrier.clone();
    let handle1 = tokio::spawn(async move {
        let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool1, config);
        sender.enqueue_async(event1).await;
        b1.wait().await;
        shutdown.signal();
        handle.await.unwrap();
        stats.lock().expect("first worker stats").clone()
    });

    let pool2 = pool.clone();
    let event2 = event.clone();
    let config = IngestionConfig::default();
    let b2 = barrier.clone();
    let handle2 = tokio::spawn(async move {
        let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool2, config);
        sender.enqueue_async(event2).await;
        b2.wait().await;
        shutdown.signal();
        handle.await.unwrap();
        stats.lock().expect("second worker stats").clone()
    });

    // Both should complete without panicking — INSERT IGNORE handles the race
    let (r1, r2) = tokio::join!(handle1, handle2);
    let stats1 = r1.expect("first concurrent worker panicked");
    let stats2 = r2.expect("second concurrent worker panicked");
    assert_eq!(stats1.errors, 0, "first worker: {stats1:?}");
    assert_eq!(stats2.errors, 0, "second worker: {stats2:?}");
    assert_eq!(stats1.events_dropped_permanent, 0, "first worker");
    assert_eq!(stats2.events_dropped_permanent, 0, "second worker");

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
    let mut reconstructed_retry =
        IngestionEvent::for_config_version(&row).expect("reconstruct retry");
    reconstructed_retry.created_at = "2026-01-02T00:00:00Z".into();
    sender.enqueue_async(reconstructed_retry).await;
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
    let (sender, shutdown, first_stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(duplicate_a.clone()).await;
    shutdown.signal();
    handle.await.unwrap();

    let first_stats = first_stats.lock().expect("first ingestion stats").clone();
    assert_eq!(first_stats.errors, 0, "first ingestion: {first_stats:?}");

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
async fn event_ingest_drops_late_events_for_deleted_session_without_recreating_root() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let session_id = Uuid::new_v4().to_string();
    let event_id = format!("evt-deleted-session-{}", Uuid::new_v4());
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
    sqlx::query(
        "INSERT INTO session_deletion_tombstones (user_id, session_id, deleted_at)
         VALUES (?, ?, CURRENT_TIMESTAMP(6))",
    )
    .bind(TEST_USER_ID)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("seed deletion tombstone");

    let (sender, shutdown, stats, handle) =
        EventIngestionWorker::spawn(pool.clone(), IngestionConfig::default());
    sender
        .enqueue_async(test_event(&event_id, &session_id, "late_after_delete"))
        .await;
    shutdown.signal();
    handle.await.expect("join ingestion worker");

    let stats = stats.lock().expect("ingestion stats").clone();
    assert_eq!(stats.events_dropped_permanent, 1, "{stats:?}");
    let session_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(TEST_USER_ID)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("count recreated session roots");
    let event_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(TEST_USER_ID)
            .bind(&event_id)
            .fetch_one(&pool)
            .await
            .expect("count late events");
    assert_eq!(session_rows, 0);
    assert_eq!(event_rows, 0);
    cleanup_session(&pool, TEST_USER_ID, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn rejected_session_group_cannot_publish_config_side_effects_or_block_a_peer() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("rejected-config-user-{}", Uuid::new_v4().simple());
    let rejected_session = format!("a-rejected-{}", Uuid::new_v4().simple());
    let healthy_session = format!("z-healthy-{}", Uuid::new_v4().simple());
    let version_id = config_version_fixture_id();
    let healthy_event = format!("healthy-event-{}", Uuid::new_v4().simple());
    cleanup_session(&pool, &user_id, &rejected_session).await;
    cleanup_session(&pool, &user_id, &healthy_session).await;
    cleanup_config_version(&pool, &user_id, &version_id).await;
    sqlx::query(
        "INSERT INTO session_deletion_tombstones (user_id, session_id, deleted_at)
         VALUES (?, ?, CURRENT_TIMESTAMP(6))",
    )
    .bind(&user_id)
    .bind(&rejected_session)
    .execute(&pool)
    .await
    .expect("seed rejected session tombstone");
    insert_session_root(&pool, &user_id, &healthy_session).await;

    let payload = ConfigVersionPayload {
        version_id: version_id.clone(),
        user_id: user_id.clone(),
        toml_body: "model = \"must-not-persist\"\n".to_string(),
        first_seen_session: Some(rejected_session.clone()),
    };
    let rejected_config =
        IngestionEvent::for_config_version(&payload).expect("rejected config event");
    let healthy = test_event_for_user(&user_id, &healthy_event, &healthy_session, "healthy_peer");
    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 4,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(rejected_config).await;
    sender.enqueue_async(healthy).await;
    shutdown.signal();
    sender.shutdown();
    handle.await.expect("join mixed rejected ingestion worker");

    let config_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM config_versions WHERE user_id = ? AND version_id = ?",
    )
    .bind(&user_id)
    .bind(&version_id)
    .fetch_one(&pool)
    .await
    .expect("count rejected config projection");
    let rejected_event_rows: i64 = event_count(&pool, &user_id, &version_id).await;
    let healthy_event_rows: i64 = event_count(&pool, &user_id, &healthy_event).await;
    assert_eq!(config_rows, 0);
    assert_eq!(rejected_event_rows, 0);
    assert_eq!(healthy_event_rows, 1);
    assert_session_event_count(&pool, &user_id, &healthy_session, 1).await;
    {
        let stats = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(stats.events_flushed, 2, "{stats:?}");
        assert_eq!(stats.events_dropped_permanent, 1, "{stats:?}");
    }

    cleanup_config_version(&pool, &user_id, &version_id).await;
    cleanup_session(&pool, &user_id, &rejected_session).await;
    cleanup_session(&pool, &user_id, &healthy_session).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn retryable_group_failure_retains_only_that_session_and_commits_its_peer_once() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("retry-isolation-user-{}", Uuid::new_v4().simple());
    let failing_session = format!("a-failing-{}", Uuid::new_v4().simple());
    let healthy_session = format!("z-healthy-{}", Uuid::new_v4().simple());
    let failing_event_id = format!("failing-event-{}", Uuid::new_v4().simple());
    let healthy_event_id = format!("healthy-event-{}", Uuid::new_v4().simple());
    insert_session_root(&pool, &user_id, &failing_session).await;
    insert_session_root(&pool, &user_id, &healthy_session).await;

    let mut failing = test_event_for_user(
        &user_id,
        &failing_event_id,
        &failing_session,
        "invalid_datetime",
    );
    failing.created_at = "not-a-datetime".to_string();
    let healthy = test_event_for_user(
        &user_id,
        &healthy_event_id,
        &healthy_session,
        "healthy_peer",
    );
    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 4,
        max_retries: 1,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue_async(failing).await;
    sender.enqueue_async(healthy).await;

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let healthy_rows: i64 = event_count(&pool, &user_id, &healthy_event_id).await;
            let snapshot = astra_core::sync_poison::recover_mutex_lock(&stats).clone();
            if healthy_rows == 1 && snapshot.errors == 1 {
                assert_eq!(snapshot.events_flushed, 1, "{snapshot:?}");
                assert_eq!(snapshot.flush_count, 1, "{snapshot:?}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("healthy group must commit while only the malformed group is retained");

    let failing_rows: i64 = event_count(&pool, &user_id, &failing_event_id).await;
    assert_eq!(failing_rows, 0);
    assert_session_event_count(&pool, &user_id, &failing_session, 0).await;
    assert_session_event_count(&pool, &user_id, &healthy_session, 1).await;

    shutdown.signal();
    sender.shutdown();
    handle.await.expect("shutdown malformed group worker");
    let final_stats = astra_core::sync_poison::recover_mutex_lock(&stats).clone();
    assert_eq!(final_stats.events_flushed, 1, "{final_stats:?}");
    assert_eq!(final_stats.flush_count, 1, "{final_stats:?}");
    assert_eq!(final_stats.errors, 2, "{final_stats:?}");
    assert!(
        final_stats
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("shutdown flush failed"),
        "{final_stats:?}"
    );

    cleanup_session(&pool, &user_id, &failing_session).await;
    cleanup_session(&pool, &user_id, &healthy_session).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne load profile"]
async fn event_ingest_100_users_by_10_sessions_finishes_finite_burst() {
    // This is a finite-burst smoke/load baseline, not evidence of sustained
    // open-loop capacity or starvation freedom: it offers one event per
    // session and asserts exact eventual persistence.
    const USERS: usize = 100;
    const SESSIONS_PER_USER: usize = 10;
    const TOTAL: usize = USERS * SESSIONS_PER_USER;

    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let run = Uuid::new_v4().simple().to_string();
    let user_prefix = format!("ingestion-load-{run}-");
    let event_type = format!("load_profile_{run}");

    for user_chunk_start in (0..USERS).step_by(20) {
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) ",
        );
        builder.push_values(
            (user_chunk_start..(user_chunk_start + 20).min(USERS))
                .flat_map(|user| (0..SESSIONS_PER_USER).map(move |session| (user, session))),
            |mut row, (user, session)| {
                row.push_bind(format!("session-{user:03}-{session:02}-{run}"))
                    .push_bind(format!("{user_prefix}{user:03}"))
                    .push_bind("event-ingestion-load")
                    .push_bind("active")
                    .push_bind(0_i64);
            },
        );
        builder
            .build()
            .execute(&pool)
            .await
            .expect("insert load-profile session roots");
    }

    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 2_000,
        max_concurrent_session_flushes: 32,
        ..Default::default()
    };
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
    let started = std::time::Instant::now();
    for user in 0..USERS {
        for session in 0..SESSIONS_PER_USER {
            let user_id = format!("{user_prefix}{user:03}");
            let session_id = format!("session-{user:03}-{session:02}-{run}");
            let event_id = format!("event-{user:03}-{session:02}-{run}");
            sender
                .enqueue_async(test_event_for_user(
                    &user_id,
                    &event_id,
                    &session_id,
                    &event_type,
                ))
                .await;
        }
    }

    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let visible: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_events WHERE event_type = ? AND user_id LIKE ?",
            )
            .bind(&event_type)
            .bind(format!("{user_prefix}%"))
            .fetch_one(&pool)
            .await
            .expect("count visible load-profile events");
            if visible == TOTAL as i64 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 1000 session events must become visible");
    let elapsed = started.elapsed();

    let owner_counts = sqlx::query(
        "SELECT user_id, COUNT(*) AS event_count
         FROM agent_events
         WHERE event_type = ? AND user_id LIKE ?
         GROUP BY user_id",
    )
    .bind(&event_type)
    .bind(format!("{user_prefix}%"))
    .fetch_all(&pool)
    .await
    .expect("load per-owner progress");
    assert_eq!(owner_counts.len(), USERS);
    for row in owner_counts {
        let count: i64 = row.get("event_count");
        assert_eq!(count, SESSIONS_PER_USER as i64);
    }
    let (p50_ms, p95_ms, p99_ms) = {
        let stats = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(stats.events_received, TOTAL as u64, "{stats:?}");
        assert_eq!(stats.events_flushed, TOTAL as u64, "{stats:?}");
        assert_eq!(stats.events_dropped_permanent, 0, "{stats:?}");
        assert_eq!(stats.resident_events_current, 0, "{stats:?}");
        assert!(stats.resident_events_peak <= 2_000, "{stats:?}");
        (
            stats
                .enqueue_to_terminal_percentile_ms(0.50)
                .expect("p50 latency"),
            stats
                .enqueue_to_terminal_percentile_ms(0.95)
                .expect("p95 latency"),
            stats
                .enqueue_to_terminal_percentile_ms(0.99)
                .expect("p99 latency"),
        )
    };
    println!(
        "PERF_RESULT benchmark=event_ingestion_100x10 sessions={TOTAL} elapsed_ms={} throughput_events_per_sec={:.2} enqueue_to_terminal_p50_ms={p50_ms} enqueue_to_terminal_p95_ms={p95_ms} enqueue_to_terminal_p99_ms={p99_ms}",
        elapsed.as_millis(),
        TOTAL as f64 / elapsed.as_secs_f64()
    );

    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("load-profile scheduler shutdown")
        .expect("load-profile scheduler task");
    let like = format!("{user_prefix}%");
    for statement in [
        "DELETE FROM agent_event_edges WHERE user_id LIKE ?",
        "DELETE FROM agent_events WHERE user_id LIKE ?",
        "DELETE FROM agent_session_lifecycle_fences WHERE user_id LIKE ?",
        "DELETE FROM agent_sessions WHERE user_id LIKE ?",
    ] {
        sqlx::query(statement)
            .bind(&like)
            .execute(&pool)
            .await
            .expect("clean load-profile fixtures");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_ingest_isolates_same_session_id_across_owners_without_blocking_valid_events() {
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
        test_user_event_count, 1,
        "the same logical session id must be independently writable by another owner"
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
        test_user_session_count, 1,
        "session identity is the owner-scoped (user_id, session_id) pair"
    );
    assert_session_event_count(&pool, &foreign_user_id, &foreign_session_id, 7).await;
    assert_session_event_count(&pool, TEST_USER_ID, &foreign_session_id, 1).await;
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
    assert_eq!(stats.events_dropped_permanent, 0);
    assert_eq!(stats.flush_count, 2);
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.last_error, None);

    cleanup_session(&pool, &foreign_user_id, &foreign_session_id).await;
    cleanup_session(&pool, TEST_USER_ID, &foreign_session_id).await;
    cleanup_session(&pool, TEST_USER_ID, &valid_session_id).await;
}

/// Verifies that a colliding event in a mixed batch cannot mutate causal
/// edges or prevent a valid sibling from being inserted.
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
    let (sender, shutdown, _initial_stats, handle) =
        EventIngestionWorker::spawn(pool.clone(), config);
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
    let (sender, shutdown, stats, handle) = EventIngestionWorker::spawn(pool.clone(), config);
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
        "colliding event rows must not gain parent edges from a later attempt"
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

    let receipt = sqlx::query(
        "SELECT collision_count, source FROM observation_identity_collisions \
         WHERE user_id = ? AND identity_kind = 'agent_event' AND identity_id = ?",
    )
    .bind(TEST_USER_ID)
    .bind(&duplicate_event_id)
    .fetch_one(&pool)
    .await
    .expect("load collision receipt");
    assert_eq!(receipt.get::<u64, _>("collision_count"), 1);
    assert_eq!(receipt.get::<String, _>("source"), "event_ingestion");

    let stats = stats.lock().expect("stats lock").clone();
    assert_eq!(stats.events_flushed, 2);
    assert_eq!(stats.events_dropped_permanent, 1);
    assert_eq!(stats.errors, 1);

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
