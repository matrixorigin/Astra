//! MatrixOne-backed integration tests for batch event writes.
//!
//! Run with: ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test runs_db_batch_it -- --ignored --test-threads=1

mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;

use astra_core::SharedPool;
use astra_services::auth::session::{DatabaseSessionService, SessionService};
use astra_services::runs::{
    CanonicalRunTerminalReceipt, CanonicalRunTerminalRequest, CanonicalTerminalContext,
    CanonicalTerminalOutcome, CanonicalTerminalTranscript, DatabaseRunStateStore, DurableRunRecord,
    RunControlReadKey, RunStateStore,
};
use astra_turn_types::CanonicalTokenUsage;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, mysql::MySqlConnection};
use std::{sync::Arc, time::Duration};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

/// Capture actual statements only within the instrumented store call.
#[derive(Clone, Default)]
struct Queries(Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::field::Visit for Queries {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "db.statement" {
            self.0.lock().unwrap().push(value.to_owned());
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Queries {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            event.record(&mut self.clone());
        }
    }
}

async fn setup() -> (SharedPool, Arc<DatabaseRunStateStore>) {
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let store = Arc::new(DatabaseRunStateStore::new(pool.clone()));
    (pool, store)
}

fn make_event(event_type: &str, data: serde_json::Value) -> serde_json::Value {
    let mut event = data;
    event["event_type"] = json!(event_type);
    event["id"] = json!(uuid::Uuid::new_v4().to_string());
    event
}

fn make_idempotent_event(
    event_type: &str,
    idempotency_key: &str,
    data: serde_json::Value,
) -> serde_json::Value {
    let mut event = make_event(event_type, data);
    event["idempotency_key"] = json!(idempotency_key);
    event
}

fn durable_run_record(run_id: String, user_id: String, session_id: String) -> DurableRunRecord {
    DurableRunRecord {
        opening: None,
        opening_hash: None,
        context_admission_hash: None,
        terminal_receipt_json: None,
        run_id,
        user_id,
        session_id,
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: Some("batch-test-agent".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: -1,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,

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
        events: vec![],
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn durable_run_record_with_events(
    run_id: String,
    user_id: String,
    session_id: String,
    agent_id: &str,
    events: Vec<serde_json::Value>,
) -> DurableRunRecord {
    let mut record = durable_run_record(run_id, user_id, session_id);
    record.agent_id = Some(agent_id.to_string());
    record.events = events;
    record
}

async fn insert_run_fixture(
    pool: &SharedPool,
    store: &DatabaseRunStateStore,
    record: DurableRunRecord,
) {
    let session_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM agent_sessions WHERE user_id = ? AND session_id = ?
         )",
    )
    .bind(&record.user_id)
    .bind(&record.session_id)
    .fetch_one(pool.get())
    .await
    .expect("check active session fixture");
    if !session_exists {
        sqlx::query(
            "INSERT INTO agent_sessions
             (user_id, session_id, status, created_at, updated_at, last_active_at)
             VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
        )
        .bind(&record.user_id)
        .bind(&record.session_id)
        .execute(pool.get())
        .await
        .expect("insert active session fixture");
    }
    store.insert_run(record).await.expect("insert run fixture");
}

/// Complete through C0, the accounting/lifecycle owner, rather than inventing
/// a second accounting-finalized event in the presentation log.
async fn complete_run_fixture(
    pool: &SharedPool,
    store: &DatabaseRunStateStore,
    user: &str,
    session: &str,
    run: &str,
    prompt: Option<u64>,
) {
    let opened = store.load_run_admission(user, run).await.unwrap().unwrap();
    let opened = opened.run();
    let mut digest = Sha256::new();
    digest.update(b"astra.run-terminal-transcript.v1\0");
    digest.update(0_u64.to_be_bytes());
    let receipt = CanonicalRunTerminalReceipt {
        schema_version: 1,
        user_id: user.into(),
        session_id: session.into(),
        run_id: run.into(),
        generation: opened.run_generation,
        opening_hash: opened.opening_hash.clone().unwrap(),
        context_admission_hash: opened.context_admission_hash.clone(),
        outcome: CanonicalTerminalOutcome::Completed,
        error_hash: None,
        error_code: None,
        error_kind: None,
        error_metadata: None,
        interruption: None,
        context: CanonicalTerminalContext::Unowned,
        transcript: CanonicalTerminalTranscript {
            item_count: 0,
            digest: format!("{:x}", digest.finalize()),
            assistant_source_event_id: None,
        },
        usage: prompt
            .map(|n| CanonicalTokenUsage::new(Some(n), Some(0), Some(0), Some(0)).unwrap()),
        tool_ledger: None,
    };
    let mut tx = pool.get().begin().await.unwrap();
    astra_services::storage::admit_session_execution_write(&mut tx, session, user)
        .await
        .unwrap();
    store
        .claim_canonical_terminal_in_transaction(
            &mut tx,
            CanonicalRunTerminalRequest {
                user_id: user,
                run_id: run,
                expected_session_id: session,
                expected_statuses: &["running"],
                expected_owner_generation: opened.run_generation,
                status: "completed",
                waiting_for: None,
                error_message: None,
                error_code: None,
                error_kind: None,
                error_metadata: None,
                interruption: None,
                cancellation_origin: None,
                prompt_tokens: prompt.unwrap_or(0),
                completion_tokens: 0,
                tool_calls: 0,
            },
            &receipt,
        )
        .await
        .unwrap()
        .expect("canonical terminal claim");
    tx.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn load_run_does_not_mix_metadata_with_later_committed_events() {
    // Pause after SQLx has received the metadata row, but before load_run
    // reads its events. The independent writer can then commit a later event
    // deterministically, without sleeps or production test hooks.
    struct MetadataReadBarrier {
        ready: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MetadataReadBarrier {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "sqlx::query" {
                return;
            }
            let mut queries = Queries::default();
            event.record(&mut queries);
            if !queries
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|sql| sql.contains("FROM agent_runs WHERE user_id = ? AND run_id = ?"))
            {
                return;
            }
            if let Some(ready) = self.ready.lock().unwrap().take() {
                ready.send(()).expect("notify metadata reader barrier");
                // The synchronous SQLx callback must hand off its Tokio
                // worker so the notified writer and database I/O can advance.
                tokio::task::block_in_place(|| {
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release metadata reader after concurrent commit");
                });
            }
        }
    }

    let (pool, store) = setup().await;
    let user_id = common::id("run-read-user");
    let session_id = common::id("run-read-session");
    let run_id = common::id("run-read-run");
    insert_run_fixture(
        &pool,
        store.as_ref(),
        durable_run_record(run_id.clone(), user_id.clone(), session_id.clone()),
    )
    .await;
    let before = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let subscriber = tracing_subscriber::registry().with(MetadataReadBarrier {
        ready: std::sync::Mutex::new(Some(ready_tx)),
        release: std::sync::Mutex::new(release_rx),
    });
    let reader = {
        let store = Arc::clone(&store);
        let user_id = user_id.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            store
                .load_run(&user_id, &run_id)
                .with_subscriber(subscriber)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("reader reaches metadata boundary")
        .expect("metadata boundary remains observable");
    let later_event = make_event("tool_result", json!({"output": "committed after metadata"}));
    let append = tokio::time::timeout(
        Duration::from_secs(5),
        store.append_event(&user_id, &session_id, &run_id, later_event.clone()),
    )
    .await;
    release_tx.send(()).expect("release paused reader");
    append
        .expect("independent writer can commit while reader is paused")
        .expect("append concurrent event");
    let snapshot = reader.await.unwrap().unwrap().unwrap();
    assert_eq!(snapshot.last_event_idx, before.last_event_idx);
    assert_eq!(
        snapshot.events, before.events,
        "events must belong to the metadata frontier, not a later commit"
    );
    let current = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert!(current.last_event_idx > snapshot.last_event_idx);
    assert_eq!(current.events.last().unwrap()["id"], later_event["id"]);
    assert_eq!(
        current.events.last().unwrap()["index"],
        current.last_event_idx
    );
    common::cleanup_work_owner(&pool, &user_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn cancellation_intent_is_independent_from_a_locked_session_row() {
    let (pool, store) = setup().await;
    let user_id = format!("cancel-lock-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("cancel-lock-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("cancel-lock-run-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("insert session fixture");
    store
        .insert_run(durable_run_record(
            run_id.clone(),
            user_id.clone(),
            session_id.clone(),
        ))
        .await
        .expect("insert run fixture");

    let settings = common::require_db_it_env();
    let mut lock_holder = MySqlConnection::connect(&settings.database_url_with_password())
        .await
        .expect("connect independent lock holder");
    let mut lock_transaction = lock_holder
        .begin()
        .await
        .expect("begin lock holder transaction");
    sqlx::query(
        "SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ? FOR UPDATE",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&mut *lock_transaction)
    .await
    .expect("lock session fixture row");

    let cancellation = tokio::time::timeout(
        Duration::from_millis(750),
        store.request_run_cancellation(&user_id, &run_id),
    )
    .await;
    lock_transaction
        .rollback()
        .await
        .expect("release session fixture lock");

    let cancellation = cancellation.expect("cancellation must not wait on agent_sessions lock");
    assert!(cancellation.expect("write cancellation intent"));
    assert!(
        store
            .request_run_cancellation(&user_id, &run_id)
            .await
            .expect("repeat cancellation intent")
    );
    assert!(
        store
            .is_run_cancellation_requested(&user_id, &run_id)
            .await
            .expect("read cancellation intent")
    );

    for (sql, scoped_id) in [
        (
            "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
            &run_id,
        ),
        (
            "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
            &run_id,
        ),
        (
            "DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?",
            &session_id,
        ),
    ] {
        sqlx::query(sql)
            .bind(&user_id)
            .bind(scoped_id)
            .execute(pool.get())
            .await
            .expect("cleanup fixture");
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn durable_run_round_trips_offering_identity_without_legacy_route_columns() {
    let (pool, store) = setup().await;
    let user_id = format!("model-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("model-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("model-run-{}", uuid::Uuid::new_v4());
    let mut record = durable_run_record(run_id.clone(), user_id.clone(), session_id);
    record.model_offering_id = Some("offer-model-primary".to_string());
    record.resolved_model_name = Some("provider-model-v2".to_string());

    insert_run_fixture(&pool, store.as_ref(), record).await;
    let loaded = store
        .load_run(&user_id, &run_id)
        .await
        .expect("load durable run")
        .expect("durable run exists");
    assert_eq!(
        loaded.model_offering_id.as_deref(),
        Some("offer-model-primary")
    );
    assert_eq!(
        loaded.resolved_model_name.as_deref(),
        Some("provider-model-v2")
    );

    let legacy_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'agent_runs' \
         AND COLUMN_NAME IN ('selected_model_json', 'selected_model_name', 'selected_model_gateway')",
    )
    .fetch_one(pool.get())
    .await
    .expect("inspect agent_runs schema");
    assert_eq!(legacy_column_count, 0);

    sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
        .bind(&user_id)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .expect("clean durable run");
}

/// Batch write: insert events, then load back and verify count.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn batch_write_stores_and_loads_events() {
    let (_pool, store) = setup().await;
    let user_id = format!("bw-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("bw-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("bw-run-{}", uuid::Uuid::new_v4());

    let events: Vec<serde_json::Value> = (0..5)
        .map(|i| {
            make_event(
                "tool_result",
                json!({"tool_name": format!("tool-{i}"), "output": format!("result-{i}")}),
            )
        })
        .collect();

    let record = durable_run_record_with_events(
        run_id.clone(),
        user_id.clone(),
        session_id.clone(),
        "batch-test-agent",
        events.clone(),
    );
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    // load_run returns a record without events (events stored separately in agent_run_events).
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(_pool.get())
        .await
        .unwrap();
    assert_eq!(count, 5, "all 5 events should be stored");

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Batch write: events are stored with sequential event_idx.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn batch_write_preserves_event_idx_ordering() {
    let (_pool, store) = setup().await;
    let user_id = format!("bwo-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("bwo-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("bwo-run-{}", uuid::Uuid::new_v4());

    let events: Vec<serde_json::Value> = vec![
        make_event("run_started", json!({})),
        make_event("tool_call", json!({"name": "read_file"})),
        make_event("tool_result", json!({"output": "content"})),
        make_event("text_delta", json!({"text": "hello"})),
        make_event("run_finished", json!({})),
    ];

    let record = durable_run_record_with_events(
        run_id.clone(),
        user_id.clone(),
        session_id.clone(),
        "bwo-agent",
        events.clone(),
    );
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    // Read event_idx from DB
    let rows = sqlx::query(
        "SELECT event_idx, event_type, payload_json FROM agent_run_events WHERE run_id = ? ORDER BY event_idx ASC",
    )
    .bind(&run_id)
    .fetch_all(_pool.get())
    .await
    .unwrap();

    assert_eq!(rows.len(), 4);

    let types: Vec<String> = rows.iter().map(|r| r.get("event_type")).collect();
    assert_eq!(
        types,
        vec!["tool_call", "tool_result", "text_delta", "run_finished"]
    );

    let indices: Vec<i64> = rows.iter().map(|r| r.get("event_idx")).collect();
    for (i, idx) in indices.iter().enumerate() {
        assert_eq!(*idx, i as i64 + 1, "event_idx should be sequential");
    }

    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(loaded.events.len(), 5);
    assert_eq!(loaded.events[0]["event_type"], "run_started");
    for (index, event) in loaded.events.iter().enumerate() {
        assert_eq!(event["index"], index as i64);
    }

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Batch write: idempotency_key dedup skips already-inserted events.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn batch_write_idempotency_dedup_skips_duplicates() {
    let (_pool, store) = setup().await;
    let user_id = format!("bwid-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("bwid-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("bwid-run-{}", uuid::Uuid::new_v4());

    let events: Vec<serde_json::Value> = vec![
        make_idempotent_event("run_started", "bwid-key-1", json!({})),
        make_idempotent_event("tool_call", "bwid-key-2", json!({"name": "read_file"})),
        make_idempotent_event("tool_result", "bwid-key-3", json!({"output": "A"})),
    ];

    let record = durable_run_record_with_events(
        run_id.clone(),
        user_id.clone(),
        session_id.clone(),
        "bwid-agent",
        events.clone(),
    );
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    // Now try to append the same events again via append_events_batch
    // (should be deduped — no new events inserted).
    store
        .append_events_batch(&user_id, &session_id, &run_id, &events)
        .await
        .expect("second append_events_batch should succeed (all deduped)");

    // Verify still only 3 events.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(_pool.get())
        .await
        .unwrap();
    assert_eq!(count, 3, "dedup should prevent duplicate events");

    // Now append a mix of new and duplicate events.
    let mixed_events: Vec<serde_json::Value> = vec![
        // Exact immutable replay — should be skipped.
        events[0].clone(),
        // New — should be inserted.
        make_idempotent_event("run_finished", "bwid-key-4", json!({"exit_code": 0})),
        // No idempotency key — always inserted.
        make_event("tool_call", json!({"name": "bash"})),
    ];
    store
        .append_events_batch(&user_id, &session_id, &run_id, &mixed_events)
        .await
        .expect("mixed append_events_batch should succeed");

    // Verify count = 5 (3 original + 2 new; 1 deduped).
    let count2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(_pool.get())
        .await
        .unwrap();
    assert_eq!(
        count2, 5,
        "mixed batch should insert 2 new and skip 1 duplicate"
    );

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Owner isolation: a dirty event row for another user with the same run_id and
/// idempotency_key must not suppress the owner's append or appear in replay.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn append_events_batch_isolates_idempotency_and_replay_by_owner() {
    let (_pool, store) = setup().await;
    let owner_user_id = format!("owner-{}", uuid::Uuid::new_v4());
    let foreign_user_id = format!("foreign-{}", uuid::Uuid::new_v4());
    let owner_session_id = format!("owner-session-{}", uuid::Uuid::new_v4());
    let foreign_session_id = format!("foreign-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("owner-bound-run-{}", uuid::Uuid::new_v4());
    let idempotency_key = format!("idem-{}", uuid::Uuid::new_v4());

    let record = durable_run_record(
        run_id.clone(),
        owner_user_id.clone(),
        owner_session_id.clone(),
    );
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    sqlx::query(
        "INSERT INTO agent_run_events
         (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
          idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&run_id)
    .bind(0_i64)
    .bind(&foreign_user_id)
    .bind(&foreign_session_id)
    .bind("foreign_noise")
    .bind(uuid::Uuid::new_v4().to_string())
    .bind("foreign-agent")
    .bind(&idempotency_key)
    .bind("foreign-hash")
    .bind("foreign-pod")
    .bind(r#"{"event_type":"foreign_noise","source":"dirty_row"}"#)
    .execute(_pool.get())
    .await
    .expect("insert foreign dirty row");

    let owner_event = make_idempotent_event(
        "tool_result",
        &idempotency_key,
        json!({"output": "owner result"}),
    );
    store
        .append_events_batch(
            &owner_user_id,
            &owner_session_id,
            &run_id,
            std::slice::from_ref(&owner_event),
        )
        .await
        .expect("owner append must ignore foreign idempotency row");

    let owner_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND idempotency_key = ?",
    )
    .bind(&owner_user_id)
    .bind(&run_id)
    .bind(&idempotency_key)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert_eq!(owner_count, 1, "owner event should be inserted");

    let foreign_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND idempotency_key = ?",
    )
    .bind(&foreign_user_id)
    .bind(&run_id)
    .bind(&idempotency_key)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert_eq!(foreign_count, 1, "foreign dirty row remains isolated");

    let same_key_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events WHERE run_id = ? AND idempotency_key = ?",
    )
    .bind(&run_id)
    .bind(&idempotency_key)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert_eq!(
        same_key_count, 2,
        "the unique idempotency identity must include owner"
    );

    let loaded = store
        .load_run(&owner_user_id, &run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.events.len(), 2, "replay must only return owner rows");
    assert_eq!(
        loaded.events[1]
            .get("event_type")
            .and_then(|value| value.as_str()),
        Some("tool_result")
    );
    assert!(
        loaded
            .events
            .iter()
            .all(|event| event.get("source").and_then(|value| value.as_str()) != Some("dirty_row")),
        "foreign rows must never enter owner replay"
    );

    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;

    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Event appends must not fetch a recovery body they never consume.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn event_append_keeps_checkpoint_durable_without_reading_its_body() {
    let (pool, store) = setup().await;
    let user = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    let run = uuid::Uuid::new_v4().to_string();
    let other_session = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES (?, ?, 'active')")
        .bind(&user)
        .bind(&other_session)
        .execute(pool.get())
        .await
        .unwrap();
    insert_run_fixture(
        &pool,
        &store,
        durable_run_record(run.clone(), user.clone(), session.clone()),
    )
    .await;
    let baseline_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user)
    .bind(&run)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let baseline_index: i64 = sqlx::query_scalar(
        "SELECT last_event_idx FROM agent_runs WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user)
    .bind(&run)
    .fetch_one(pool.get())
    .await
    .unwrap();
    for bytes in [0, 128, 512 * 1024] {
        let body = (bytes > 0).then(|| {
            json!({"version":"checkpoint_v1", "graceful":true, "payload":"x".repeat(bytes)})
                .to_string()
        });
        sqlx::query("UPDATE agent_runs SET checkpoint_json = ? WHERE user_id = ? AND run_id = ?")
            .bind(&body)
            .bind(&user)
            .bind(&run)
            .execute(pool.get())
            .await
            .unwrap();
        let event = make_idempotent_event(
            "agent_progress",
            &format!("body-{bytes}"),
            json!({"bytes":bytes}),
        );
        for replay in [false, true] {
            let queries = Queries::default();
            let subscriber = tracing_subscriber::registry().with(queries.clone());
            let started = std::time::Instant::now();
            store
                .append_events_batch(&user, &session, &run, std::slice::from_ref(&event))
                .with_subscriber(subscriber)
                .await
                .unwrap();
            let statements = queries.0.lock().unwrap();
            let locked_run = statements
                .iter()
                .find(|sql| sql.contains("FROM agent_runs") && sql.contains("FOR UPDATE"))
                .expect("actual run-lock statement");
            assert!(
                locked_run.contains("CAST(NULL AS CHAR) AS checkpoint_json"),
                "append must omit recovery body: {locked_run}"
            );
            eprintln!(
                "event_append checkpoint_bytes={bytes} replay={replay} statements={} elapsed_us={}",
                statements.len(),
                started.elapsed().as_micros()
            );
        }
        let retained: Option<String> = sqlx::query_scalar(
            "SELECT checkpoint_json FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user)
        .bind(&run)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(
            retained, body,
            "read optimization must not remove recovery data"
        );
        assert!(
            store
                .append_events_batch(&user, &other_session, &run, std::slice::from_ref(&event))
                .await
                .is_err()
        );
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user)
    .bind(&run)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        count,
        baseline_count + 3,
        "replay and rejected requests cannot insert events"
    );
    let index: i64 = sqlx::query_scalar(
        "SELECT last_event_idx FROM agent_runs WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user)
    .bind(&run)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(index, baseline_index + 3);
    for table in [
        "agent_run_events",
        "agent_runs",
        "agent_session_lifecycle_fences",
        "agent_sessions",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ? AND session_id IN (?, ?)"
        )))
        .bind(&user)
        .bind(&session)
        .bind(&other_session)
        .execute(pool.get())
        .await
        .unwrap();
    }
}

/// Owner isolation: a dirty checkpoint row for another user with the same run_id
/// and idempotency key must not suppress the owner's save or win latest-load.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn checkpoints_isolate_idempotency_and_latest_load_by_owner() {
    let (_pool, store) = setup().await;
    let owner_user_id = format!("checkpoint-owner-{}", uuid::Uuid::new_v4());
    let foreign_user_id = format!("checkpoint-foreign-{}", uuid::Uuid::new_v4());
    let owner_session_id = format!("checkpoint-owner-session-{}", uuid::Uuid::new_v4());
    let foreign_session_id = format!("checkpoint-foreign-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("checkpoint-run-{}", uuid::Uuid::new_v4());
    let last_batch_id = format!("batch-{}", uuid::Uuid::new_v4());
    let idempotency_key = format!("checkpoint:{run_id}:resume:{last_batch_id}");

    insert_run_fixture(
        &_pool,
        store.as_ref(),
        durable_run_record(
            run_id.clone(),
            owner_user_id.clone(),
            owner_session_id.clone(),
        ),
    )
    .await;

    sqlx::query(
        "INSERT INTO run_checkpoints
         (checkpoint_id, run_id, user_id, session_id, node_seq, checkpoint_kind,
          checkpoint_version, idempotency_key, checkpoint_json, created_at)
         VALUES (?, ?, ?, ?, 99, 'resume', 'checkpoint_v1', ?, ?, '2099-01-01 00:00:00.999999')",
    )
    .bind(format!("ckpt-foreign-{}", uuid::Uuid::new_v4()))
    .bind(&run_id)
    .bind(&foreign_user_id)
    .bind(&foreign_session_id)
    .bind(&idempotency_key)
    .bind(r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":"foreign","source":"dirty_row"}"#)
    .execute(_pool.get())
    .await
    .expect("insert foreign dirty checkpoint");

    let owner_checkpoint = json!({
        "version": "checkpoint_v1",
        "graceful": true,
        "last_batch_id": last_batch_id,
        "source": "owner_row"
    })
    .to_string();
    assert!(
        store
            .save_checkpoint(astra_services::runs::RunCheckpointWriteRequest {
                user_id: &owner_user_id,
                expected_session_id: &owner_session_id,
                run_id: &run_id,
                checkpoint_json: &owner_checkpoint,
                authority: astra_services::runs::CheckpointWriteAuthority::ControlPlane
            })
            .await
            .expect("save owner checkpoint")
            .is_some(),
        "owner checkpoint save must ignore foreign idempotency row"
    );

    let owner_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_checkpoints
         WHERE user_id = ? AND run_id = ? AND checkpoint_kind = 'resume' AND idempotency_key = ?",
    )
    .bind(&owner_user_id)
    .bind(&run_id)
    .bind(&idempotency_key)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert_eq!(owner_count, 1, "owner checkpoint should be inserted");

    let same_key_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_checkpoints
         WHERE run_id = ? AND checkpoint_kind = 'resume' AND idempotency_key = ?",
    )
    .bind(&run_id)
    .bind(&idempotency_key)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert_eq!(
        same_key_count, 2,
        "checkpoint idempotency identity must include owner"
    );

    for (checkpoint_id, checkpoint_kind, idempotency_key, checkpoint_json, created_at) in [
        (
            "ckpt-owner-z",
            "resume",
            "checkpoint:owner:older",
            r#"{"version":"checkpoint_v1","graceful":true,"source":"older_resume"}"#,
            "2099-01-01 00:00:00.000001",
        ),
        (
            "ckpt-owner-a",
            "resume",
            "checkpoint:owner:newer",
            r#"{"version":"checkpoint_v1","graceful":true,"source":"newer_resume"}"#,
            "2099-01-01 00:00:00.000002",
        ),
        (
            "ckpt-owner-latest",
            "checkpoint",
            "checkpoint:owner:latest",
            r#"{"version":"checkpoint_v1","graceful":false,"source":"newer_checkpoint"}"#,
            "2099-01-01 00:00:00.000003",
        ),
    ] {
        sqlx::query(
            "INSERT INTO run_checkpoints
             (checkpoint_id, run_id, user_id, session_id, node_seq, checkpoint_kind,
              checkpoint_version, idempotency_key, checkpoint_json, created_at)
             VALUES (?, ?, ?, ?, 100, ?, 'checkpoint_v1', ?, ?, ?)",
        )
        .bind(checkpoint_id)
        .bind(&run_id)
        .bind(&owner_user_id)
        .bind(&owner_session_id)
        .bind(checkpoint_kind)
        .bind(idempotency_key)
        .bind(checkpoint_json)
        .bind(created_at)
        .execute(_pool.get())
        .await
        .expect("insert owner checkpoint ordering fixture");
    }

    let latest = store
        .load_latest_checkpoint(&owner_user_id, &run_id, Some("resume"))
        .await
        .expect("load latest owner checkpoint")
        .expect("checkpoint exists");
    assert_eq!(latest.user_id, owner_user_id);
    assert!(
        latest
            .checkpoint_json
            .contains(r#""source":"newer_resume""#),
        "owner latest resume checkpoint should use microsecond ordering and ignore foreign rows: {:?}",
        latest
    );

    let latest_any_kind = store
        .load_latest_checkpoint(&owner_user_id, &run_id, None)
        .await
        .expect("load latest owner checkpoint of any kind")
        .expect("checkpoint exists");
    assert!(
        latest_any_kind
            .checkpoint_json
            .contains(r#""source":"newer_checkpoint""#)
    );

    assert!(
        store
            .load_latest_checkpoint(&foreign_user_id, &run_id, Some("resume"))
            .await
            .expect("load foreign orphan checkpoint")
            .is_none()
    );

    let _ = sqlx::query("DELETE FROM run_checkpoints WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;

    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Recovery claims derive the continuation decision from canonical checkpoint
/// history even when the denormalized run-row snapshot is absent.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn recovery_claim_reads_canonical_resume_history_without_embedded_snapshot() {
    let (_pool, store) = setup().await;
    let user_id = format!("recovery-history-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("recovery-history-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("recovery-history-run-{}", uuid::Uuid::new_v4());
    let malformed_run_id = format!("recovery-history-malformed-{}", uuid::Uuid::new_v4());
    insert_run_fixture(
        &_pool,
        store.as_ref(),
        durable_run_record(run_id.clone(), user_id.clone(), session_id.clone()),
    )
    .await;
    let mut malformed_record = durable_run_record(
        malformed_run_id.clone(),
        user_id.clone(),
        session_id.clone(),
    );
    malformed_record.checkpoint_version = Some("checkpoint_v1".into());
    malformed_record.checkpoint_json =
        Some(r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":7}"#.into());
    insert_run_fixture(&_pool, store.as_ref(), malformed_record).await;

    let checkpoint_json =
        r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":"canonical-only"}"#;
    for (run_id, checkpoint_id, idempotency_key, checkpoint_json) in [
        (
            run_id.as_str(),
            format!("ckpt-recovery-history-{}", uuid::Uuid::new_v4()),
            format!("checkpoint:{run_id}:resume:canonical-only"),
            checkpoint_json.to_string(),
        ),
        (
            malformed_run_id.as_str(),
            format!("ckpt-recovery-malformed-{}", uuid::Uuid::new_v4()),
            format!("checkpoint:{malformed_run_id}:resume:malformed"),
            r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":7}"#.to_string(),
        ),
    ] {
        sqlx::query(
            "INSERT INTO run_checkpoints
             (checkpoint_id, run_id, user_id, session_id, node_seq, checkpoint_kind,
              checkpoint_version, idempotency_key, checkpoint_json, created_at)
             VALUES (?, ?, ?, ?, 4, 'resume', 'checkpoint_v1', ?, ?, NOW(6))",
        )
        .bind(checkpoint_id)
        .bind(run_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(idempotency_key)
        .bind(checkpoint_json)
        .execute(_pool.get())
        .await
        .expect("insert recovery checkpoint fixture");
    }

    let claims = store
        .claim_recoverable_active_runs(256)
        .await
        .expect("claim recovery candidates");
    let claim = claims
        .iter()
        .find(|claim| claim.run.user_id == user_id && claim.run.run_id == run_id)
        .expect("canonical-only recovery run should be claimed");
    assert!(claim.has_graceful_resume_checkpoint);
    assert!(claim.run.checkpoint_version.is_none());
    assert!(claim.run.checkpoint_json.is_none());

    let malformed_claim = claims
        .iter()
        .find(|claim| claim.run.run_id == malformed_run_id)
        .expect("malformed canonical recovery run should be claimed");
    assert!(!malformed_claim.has_graceful_resume_checkpoint);

    for run_id in [run_id.as_str(), malformed_run_id.as_str()] {
        for statement in [
            "DELETE FROM run_checkpoints WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
        ] {
            let _ = sqlx::query(statement)
                .bind(&user_id)
                .bind(run_id)
                .execute(_pool.get())
                .await;
        }
    }
}

/// Batch write: single-event append_event uses batch path and works.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn single_event_append_uses_batch_path() {
    let (_pool, store) = setup().await;
    let user_id = format!("bwse-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("bwse-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("bwse-run-{}", uuid::Uuid::new_v4());

    // Insert the run first.
    let mut record = durable_run_record(run_id.clone(), user_id.clone(), session_id.clone());
    record.agent_id = Some("bwse-agent".to_string());
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    // Append single events (this should use the batch path internally).
    store
        .append_event(
            &user_id,
            &session_id,
            &run_id,
            make_event("tool_call", json!({"name": "read_file"})),
        )
        .await
        .unwrap();
    store
        .append_event(
            &user_id,
            &session_id,
            &run_id,
            make_event("tool_result", json!({"output": "hello"})),
        )
        .await
        .unwrap();
    store
        .append_event(
            &user_id,
            &session_id,
            &run_id,
            make_event("run_finished", json!({})),
        )
        .await
        .unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(_pool.get())
        .await
        .unwrap();
    assert_eq!(
        count, 4,
        "genesis plus three appended events must be stored"
    );

    for (limit, expected) in [(1, 1), (2, 2), (4, 4), (64, 4)] {
        let observation = store
            .load_run_observation(&user_id, &run_id, limit)
            .await
            .unwrap()
            .expect("existing run observation");
        assert_eq!(observation.total_event_count, 4);
        assert_eq!(observation.run.events.len(), expected);
        assert_eq!(observation.run.events[0]["event_type"], "run_created");
        if limit > 1 {
            assert_eq!(
                observation.run.events.last().unwrap()["event_type"],
                "run_finished"
            );
        }
    }
    assert!(
        store
            .load_run_observation("unrelated-owner", &run_id, 4)
            .await
            .unwrap()
            .is_none()
    );
    store.append_event(&user_id, &session_id, &run_id, make_event(
        "agent_progress",
        json!({"large_payload": "x".repeat(astra_services::runs::MAX_RUN_OBSERVATION_BYTES / 4 + 1)}),
    )).await.unwrap();
    let observation = store
        .load_run_observation(&user_id, &run_id, 4)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observation.total_event_count, 5);
    assert_eq!(observation.run.events.len(), 3);
    assert!(
        observation
            .run
            .events
            .iter()
            .all(|event| event.get("large_payload").is_none()
                && event.pointer("/data/large_payload").is_none())
    );

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// CAS contention: two concurrent `append_events_batch` calls on the same run
/// must not produce gaps in event_idx.
#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_append_no_event_idx_gaps() {
    let (_pool, store) = setup().await;
    let run_id = format!("concurrent_gap_{}", uuid::Uuid::new_v4());
    let user_id = "test_user".to_string();
    let session_id = format!("sess_c_{}", uuid::Uuid::new_v4());
    insert_run_fixture(
        &_pool,
        store.as_ref(),
        durable_run_record_with_events(
            run_id.clone(),
            user_id.clone(),
            session_id.clone(),
            "agent_0",
            Vec::new(),
        ),
    )
    .await;

    let store_a = store.clone();
    let store_b = store.clone();
    let rid_a = run_id.clone();
    let rid_b = run_id.clone();
    let uid_a = user_id.clone();
    let uid_b = user_id.clone();
    let sid_a = session_id.clone();
    let sid_b = session_id.clone();

    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            let batch: Vec<_> = (0..5)
                .map(|i| make_event("task_start", json!({"n": i})))
                .collect();
            store_a
                .append_events_batch(&uid_a, &sid_a, &rid_a, &batch)
                .await
        }),
        tokio::spawn(async move {
            let batch: Vec<_> = (0..7)
                .map(|i| make_event("tool_call", json!({"n": i})))
                .collect();
            store_b
                .append_events_batch(&uid_b, &sid_b, &rid_b, &batch)
                .await
        }),
    );

    r1.unwrap().unwrap();
    r2.unwrap().unwrap();

    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(
        loaded.events.len(),
        13,
        "genesis plus all 12 appended events must be present"
    );

    let mut indices: Vec<i64> = loaded
        .events
        .iter()
        .filter_map(|e| e.get("index").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(indices.len(), 13);
    indices.sort_unstable();
    for (i, idx) in indices.iter().enumerate() {
        assert_eq!(*idx, i as i64, "gap at position {i}");
    }

    assert_eq!(loaded.last_event_idx, 12, "last_event_idx must be 12");

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Run creation admits the session and execution slot under one lock order.
/// Two different run identities may race, but only one can commit a blocking
/// run for the session; the losing transaction must roll back its run row and
/// retain the first transaction's lifecycle fence.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn concurrent_run_inserts_share_one_session_slot() {
    let (pool, store) = setup().await;
    let user_id = format!("run-admission-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("run-admission-session-{}", uuid::Uuid::new_v4());
    let run_a = format!("run-admission-a-{}", uuid::Uuid::new_v4());
    let run_b = format!("run-admission-b-{}", uuid::Uuid::new_v4());

    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("insert concurrent run admission session");

    let mut record_a = durable_run_record(run_a, user_id.clone(), session_id.clone());
    let mut record_b = durable_run_record(run_b, user_id.clone(), session_id.clone());
    record_a.agent_id = None;
    record_b.agent_id = None;
    let store_a = store.clone();
    let store_b = store.clone();
    let (result_a, result_b) =
        tokio::join!(store_a.insert_run(record_a), store_b.insert_run(record_b),);
    assert_eq!(
        [result_a.as_ref(), result_b.as_ref()]
            .iter()
            .filter(|result| result.is_ok())
            .count(),
        1,
        "exactly one run may win the session slot: {result_a:?}; {result_b:?}"
    );

    let run_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ?")
            .bind(&user_id)
            .bind(&session_id)
            .fetch_one(pool.get())
            .await
            .expect("count admitted runs");
    assert_eq!(
        run_count, 1,
        "the losing transaction must roll back its run"
    );

    let slot_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ? AND (status IN ('running', 'waiting') OR (status = 'paused' AND waiting_for IS NOT NULL) OR (owner_pod_id IS NOT NULL AND owner_lease_expires_at >= NOW(6)))",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("count session execution slots");
    assert_eq!(slot_count, 1, "one durable slot must own the winning run");

    let fence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_session_lifecycle_fences
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("count session lifecycle fences");
    assert_eq!(fence_count, 1, "the first admission must create one fence");

    for table in [
        "agent_run_events",
        "agent_runs",
        "agent_session_lifecycle_fences",
        "agent_sessions",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id = ? AND session_id = ?");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("cleanup {table}: {error}"));
    }
}

/// A failed admission must not leave behind the lifecycle fence that it
/// inserted before discovering that the session does not exist.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn failed_run_admission_rolls_back_a_new_lifecycle_fence() {
    let (pool, store) = setup().await;
    let user_id = format!("run-admission-rollback-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("run-admission-rollback-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("run-admission-rollback-run-{}", uuid::Uuid::new_v4());

    let error = store
        .insert_run(durable_run_record(
            run_id,
            user_id.clone(),
            session_id.clone(),
        ))
        .await
        .expect_err("a missing session must fail run admission");
    assert_eq!(error, "session is not active");

    let fence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_session_lifecycle_fences
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("count rolled-back lifecycle fences");
    assert_eq!(
        fence_count, 0,
        "failed admission must roll back its new fence"
    );
}

/// The lifecycle fence must serialize deletion with run admission. Regardless
/// of which transaction wins the first lock, a completed deletion may not
/// leave an admitted run, slot, or display projection behind.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn session_deletion_competes_safely_with_run_admission() {
    let (pool, store) = setup().await;
    let settings = astra_core::MatrixOneSettings::from_env();
    let user_id = format!("run-admission-delete-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("delete-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("run-admission-delete-run-{}", uuid::Uuid::new_v4());

    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("insert deletion race session");

    let delete_service = DatabaseSessionService::new(settings).with_pool(pool.clone());
    let delete_user_id = user_id.clone();
    let delete_session_id = session_id.clone();
    let delete_task = tokio::spawn(async move {
        delete_service
            .delete_session(delete_session_id, delete_user_id)
            .await
    });
    let insert_store = store.clone();
    let insert_user_id = user_id.clone();
    let insert_session_id = session_id.clone();
    let insert_run_id = run_id.clone();
    let insert_task = tokio::spawn(async move {
        let mut record = durable_run_record(insert_run_id, insert_user_id, insert_session_id);
        record.agent_id = None;
        insert_store.insert_run(record).await
    });
    let (delete_result, insert_result) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(delete_task, insert_task)
    })
    .await
    .expect("deletion and admission must not deadlock");
    delete_result
        .expect("deletion task join")
        .expect("session deletion must complete");
    let _ = insert_result.expect("run admission task join");

    for (table, predicate) in [
        (
            "agent_runs",
            "user_id = ? AND session_id = ? AND run_id = ?",
        ),
        (
            "agent_run_events",
            "user_id = ? AND session_id = ? AND run_id = ?",
        ),
    ] {
        let statement = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
            .bind(&user_id)
            .bind(&session_id)
            .bind(&run_id)
            .fetch_one(pool.get())
            .await
            .unwrap_or_else(|error| panic!("count {table} after deletion race: {error}"));
        assert_eq!(count, 0, "deleted session must not retain rows in {table}");
    }
    let session_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("count deleted session root");
    assert_eq!(session_count, 0, "session deletion must remove the root");

    for table in ["agent_session_lifecycle_fences"] {
        let statement = format!("DELETE FROM {table} WHERE user_id = ? AND session_id = ?");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("cleanup {table}: {error}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn control_batch_preserves_database_identity_positions_and_isolates_corruption() {
    let (pool, store) = setup().await;
    let nonce = uuid::Uuid::new_v4();
    let (user, other) = (format!("control-a-{nonce}"), format!("control-b-{nonce}"));
    let (run, bad) = (
        format!("control-run-{nonce}"),
        format!("control-bad-{nonce}"),
    );
    for (owner, id) in [(&user, &run), (&other, &bad)] {
        insert_run_fixture(
            &pool,
            &store,
            durable_run_record(id.clone(), owner.clone(), format!("session-{id}")),
        )
        .await;
    }
    // MatrixOne inputRows counts storage rows before filtering, including
    // unrelated rows sharing a candidate block. Bound scans by the population
    // before injecting history, rather than by the number of requested keys.
    let candidate_population: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs")
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert!(candidate_population >= 2);
    // More history than the entire initial population ensures a missing
    // candidate filter cannot pass the scan bound by reading that history.
    let history_count = candidate_population + 256;
    // Historical runs must not become work for unrelated active observations.
    let mut history = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO agent_runs (user_id,session_id,run_id,root_run_id,ancestor_path,status) ",
    );
    history.push_values(0..history_count, |mut row, i| {
        let id = format!("control-history-{nonce}-{i}");
        row.push_bind(&other)
            .push_bind(format!("session-{bad}"))
            .push_bind(id.clone())
            .push_bind(id.clone())
            .push_bind(format!("/{id}"))
            .push_bind("completed");
    });
    history.build().execute(pool.get()).await.unwrap();
    let key = |user: &str, run: &str| RunControlReadKey {
        user_id: user.into(),
        run_id: run.into(),
    };
    let keys = vec![
        key(&user, &run),
        key(&other, &run),
        key(&user, "missing"),
        key(&other, &bad),
        key(&user.to_uppercase(), &run.to_uppercase()),
        key(&user, &run),
    ];
    let mut expected = Vec::new();
    for key in &keys {
        expected.push(
            store
                .load_run_control(&key.user_id, &key.run_id)
                .await
                .unwrap(),
        );
    }
    let queries = Queries::default();
    let subscriber = tracing_subscriber::registry().with(queries.clone());
    let results = store
        .load_run_control_batch(&keys)
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(
        results.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
    assert!(expected[0].is_some() && expected[1].is_none() && expected[2].is_none());
    assert_eq!(
        queries
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains("agent_runs"))
            .count(),
        1
    );
    assert!(store.load_run_control_batch(&[]).await.unwrap().is_empty());
    assert!(
        store
            .load_run_control_batch(&vec![key(&user, &run); 129])
            .await
            .is_err()
    );

    for size in [1, 128] {
        let requested = keys.iter().cycle().take(size).cloned().collect::<Vec<_>>();
        let queries = Queries::default();
        let subscriber = tracing_subscriber::registry().with(queries.clone());
        let result = store
            .load_run_control_batch(&requested)
            .with_subscriber(subscriber)
            .await
            .unwrap();
        assert_eq!(
            result.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            expected
                .iter()
                .cycle()
                .take(size)
                .cloned()
                .collect::<Vec<_>>()
        );
        let statements = queries
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains("agent_runs"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(statements.len(), 1);
        let mut connection = pool.get().acquire().await.unwrap();
        let mut arguments = Vec::with_capacity(size * 3);
        for (ordinal, key) in requested.iter().enumerate() {
            for value in [&key.user_id, &key.run_id] {
                assert!(
                    value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                );
            }
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "SET @control_u{ordinal}='{}',@control_r{ordinal}='{}'",
                key.user_id, key.run_id
            )))
            .execute(&mut *connection)
            .await
            .unwrap();
            arguments.push(format!("@control_u{ordinal}"));
            arguments.push(format!("@control_r{ordinal}"));
        }
        arguments.extend((0..size).map(|ordinal| format!("@control_r{ordinal}")));
        // A removed candidate predicate must fail on scans, not bind count.
        arguments.truncate(statements[0].matches('?').count());
        // Inspect the emitted production SELECT, not a test-owned query copy.
        // MatrixOne supports this prepared diagnostic but not JSON EXPLAIN.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "PREPARE control_scan FROM '{}'",
            statements[0].replace('\'', "''")
        )))
        .execute(&mut *connection)
        .await
        .unwrap();
        let rows = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "EXPLAIN ANALYZE FORCE EXECUTE control_scan USING {}",
            arguments.join(",")
        )))
        .fetch_all(&mut *connection)
        .await
        .unwrap();
        sqlx::raw_sql("DEALLOCATE PREPARE control_scan")
            .execute(&mut *connection)
            .await
            .unwrap();
        let plan = rows
            .iter()
            .map(|row| row.get::<String, _>(0))
            .collect::<Vec<_>>();
        let table = format!("Table Scan on {}.agent_runs", pool.settings().database);
        let scans = plan
            .windows(2)
            .filter(|lines| lines[0].contains(&table))
            .map(|lines| {
                lines[1]
                    .split("inputRows=")
                    .nth(1)
                    .expect("base scan must report input rows")
                    .split_ascii_whitespace()
                    .next()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(!scans.is_empty(), "missing base scan counters: {plan:?}");
        let scanned = scans.iter().sum::<u64>();
        assert!(
            (1..=candidate_population as u64).contains(&scanned),
            "{size} lookups scanned {scanned} rows, exceeding the pre-history population {candidate_population}: {plan:?}"
        );
        eprintln!("control_batch requests={size} base_scan_rows={scanned}");
    }

    let independent = SharedPool::new(pool.settings()).await.unwrap();
    sqlx::query("UPDATE agent_runs SET status='paused', last_event_idx=7, cancellation_requested_at=NOW(6) WHERE user_id=? AND run_id=?")
        .bind(&user).bind(&run).execute(independent.get()).await.unwrap();
    sqlx::query(
        "UPDATE agent_runs SET run_generation=-1,status='failed' WHERE user_id=? AND run_id=?",
    )
    .bind(&other)
    .bind(&bad)
    .execute(independent.get())
    .await
    .unwrap();
    let results = store.load_run_control_batch(&keys).await.unwrap();
    let fresh = results[0].as_ref().unwrap().as_ref().unwrap();
    assert_eq!(
        (
            fresh.status.as_str(),
            fresh.last_event_idx,
            fresh.cancellation_requested
        ),
        ("paused", 7, true)
    );
    assert!(
        results[3].is_err(),
        "malformed row fails only its request position"
    );
    assert_eq!(results[0], results[5]);
    assert!(results[1].as_ref().unwrap().is_none());
    for table in ["agent_runs", "agent_sessions"] {
        for owner in [&user, &other] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE user_id=?"
            )))
            .bind(owner)
            .execute(independent.get())
            .await
            .unwrap();
        }
    }
    independent.close().await;
}

/// Status returns bounded facts without hydrating history or checkpoint bodies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn status_snapshot_batches_facts_and_preserves_binding_and_accounting() {
    let (pool, store) = setup().await;
    let id = uuid::Uuid::new_v4().to_string();
    let user = format!("status-{id}");
    let session = format!("session-{id}");
    let run = format!("run-{id}");
    let mut events = vec![
        json!({"event_type":"run_started", "data":{"explain_analyze_requested":true,"transport":"edge"}}),
        json!({"event_type":"executor_bound", "data":{"executor":{"id":"original"}}}),
        json!({"event_type":"workspace_bound", "data":{"workspace":{"id":"original"},"transport":"ignored"}}),
    ];
    events.extend((0..256).map(
        |i| json!({"event_type":"agent_progress", "data":{"index":i,"text":"x".repeat(2048)}}),
    ));
    insert_run_fixture(
        &pool,
        &store,
        durable_run_record_with_events(run.clone(), user.clone(), session.clone(), "agent", events),
    )
    .await;
    // Opening facts are immutable; later presentation cannot replace them.
    store
        .append_events_batch(
            &user,
            &session,
            &run,
            &[
                json!({"event_type":"executor_bound","data":{"executor":{"id":"replacement"}}}),
                json!({"event_type":"workspace_bound","data":{"workspace":{"id":"replacement"}}}),
            ],
        )
        .await
        .unwrap();
    complete_run_fixture(&pool, &store, &user, &session, &run, Some(20)).await;
    let terminal_tail = store
        .load_run_control(&user, &run)
        .await
        .unwrap()
        .unwrap()
        .last_event_idx;
    let checkpoint = "x".repeat(256 * 1024);
    sqlx::query("UPDATE agent_runs SET checkpoint_json = ? WHERE user_id = ? AND run_id = ?")
        .bind(&checkpoint)
        .bind(&user)
        .bind(&run)
        .execute(pool.get())
        .await
        .unwrap();
    let (_, replica) = setup().await;
    for phase in 0..2 {
        if phase == 1 {
            store
                .append_events_batch(
                    &user,
                    &session,
                    &run,
                    &[
                        json!({"event_type":"agent_progress","data":{"index":257}}),
                        json!({"event_type":"agent_progress","data":{"index":258}}),
                        json!({"event_type":"agent_progress","data":{"index":259}}),
                    ],
                )
                .await
                .unwrap();
        }
        let queries = Queries::default();
        let subscriber =
            tracing::Dispatch::new(tracing_subscriber::registry().with(queries.clone()));
        let snapshot = replica
            .load_run_status_snapshot(&user, &run)
            .with_subscriber(subscriber)
            .await
            .unwrap()
            .unwrap();
        assert!(snapshot.explain_requested);
        assert_eq!(snapshot.workspace.unwrap()["id"], "original");
        assert_eq!(snapshot.executor.unwrap()["id"], "original");
        assert_eq!(snapshot.transport.as_deref(), Some("edge"));
        assert_eq!(snapshot.accounting.unwrap()["prompt_tokens"], 20);
        assert_eq!(snapshot.last_event_idx, terminal_tail + phase * 3);
        let queries = queries.0.lock().unwrap();
        let reads: Vec<_> = queries
            .iter()
            .filter(|q| q.contains("FROM agent_runs") || q.contains("FROM agent_run_events"))
            .collect();
        eprintln!(
            "status_snapshot phase={phase} store_reads={} checkpoint_bytes={}",
            reads.len(),
            checkpoint.len()
        );
        assert_eq!(reads.len(), 2, "one metadata and one bounded facts read");
        assert!(
            queries
                .iter()
                .all(|q| !q.trim_start().starts_with("UPDATE")),
            "status reads cannot write"
        );
        assert!(
            reads[0].contains("CAST(NULL AS CHAR) AS checkpoint_json"),
            "status must not hydrate recovery bodies"
        );
    }
    for (owner, requested_run) in [("foreign", run.as_str()), (user.as_str(), "missing")] {
        let queries = Queries::default();
        let subscriber =
            tracing::Dispatch::new(tracing_subscriber::registry().with(queries.clone()));
        assert!(
            replica
                .load_run_status_snapshot(owner, requested_run)
                .with_subscriber(subscriber)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            queries
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|q| q.contains("FROM agent_runs") || q.contains("FROM agent_run_events"))
                .count(),
            1,
            "missing authority cannot read events"
        );
    }
    let retained: String = sqlx::query_scalar(
        "SELECT checkpoint_json FROM agent_runs WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user)
    .bind(&run)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        retained, checkpoint,
        "read optimization cannot delete recovery data"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn status_snapshot_preserves_selected_fact_failure_semantics() {
    let (pool, store) = setup().await;
    let id = uuid::Uuid::new_v4().to_string();
    let user = format!("status-{id}");
    let session = format!("session-{id}");
    let run = format!("run-{id}");
    insert_run_fixture(
        &pool,
        &store,
        durable_run_record_with_events(
            run.clone(),
            user.clone(),
            session.clone(),
            "agent",
            vec![json!({"event_type":"run_started","data":{}})],
        ),
    )
    .await;
    let empty = store
        .load_run_status_snapshot(&user, &run)
        .await
        .unwrap()
        .unwrap();
    assert!(!empty.explain_requested);
    assert!(empty.workspace.is_none() && empty.executor.is_none() && empty.accounting.is_none());
    store
        .append_events_batch(
            &user,
            &session,
            &run,
            &[json!({"event_type":"agent_progress","data":{}})],
        )
        .await
        .unwrap();
    complete_run_fixture(&pool, &store, &user, &session, &run, Some(19)).await;
    // A bad unrelated payload cannot poison the bounded status read.
    sqlx::query("UPDATE agent_run_events SET payload_json = '{' WHERE user_id = ? AND run_id = ? AND event_type = 'agent_progress'")
        .bind(&user).bind(&run).execute(pool.get()).await.unwrap();
    let finished = store
        .load_run_status_snapshot(&user, &run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished.accounting.unwrap()["prompt_tokens"], 19);
    sqlx::query(
        "UPDATE agent_runs SET terminal_receipt_json = '{' WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user)
    .bind(&run)
    .execute(pool.get())
    .await
    .unwrap();
    assert!(store.load_run_status_snapshot(&user, &run).await.is_err());
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn status_snapshot_keeps_zero_positions_and_ignores_unselected_same_type_corruption() {
    let (pool, store) = setup().await;
    let user = common::id("status-zero");
    for prompt in [None, Some(0)] {
        let run = common::id("status-run");
        let session = common::id("status-session");
        insert_run_fixture(
            &pool,
            &store,
            durable_run_record_with_events(
                run.clone(),
                user.clone(),
                session.clone(),
                "agent",
                vec![json!({"event_type":"run_started","data":{}})],
            ),
        )
        .await;
        complete_run_fixture(&pool, &store, &user, &session, &run, prompt).await;
        let snapshot = store
            .load_run_status_snapshot(&user, &run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.last_event_idx, 1);
        let accounting = snapshot.accounting.unwrap();
        assert_eq!(
            accounting
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_u64),
            prompt
        );
        assert_eq!(accounting["qualified_usage"].is_null(), prompt.is_none());
        assert!(snapshot.workspace.is_none() && snapshot.executor.is_none());
    }
    let run = common::id("status-run");
    let session = common::id("status-session");
    insert_run_fixture(
        &pool,
        &store,
        durable_run_record_with_events(run.clone(), user.clone(), session.clone(), "agent", vec![json!({"event_type":"workspace_bound","data":{"workspace":{"id":"only-workspace"}}})]),
    )
    .await;
    let first_index: i64 = sqlx::query_scalar("SELECT event_idx FROM agent_run_events WHERE user_id = ? AND run_id = ? AND event_type = 'workspace_bound'")
        .bind(&user).bind(&run).fetch_one(pool.get()).await.unwrap();
    assert_eq!(first_index, 0, "generic store facts can occupy index zero");
    store
        .append_events_batch(
            &user,
            &session,
            &run,
            &[json!({"event_type":"workspace_bound","data":{"workspace":{"id":"later"}}})],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE agent_run_events SET payload_json = '{' WHERE user_id = ? AND run_id = ? AND event_idx = 1")
        .bind(&user).bind(&run).execute(pool.get()).await.unwrap();
    let snapshot = store
        .load_run_status_snapshot(&user, &run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.workspace.unwrap()["id"], "only-workspace");
    assert!(snapshot.executor.is_none() && snapshot.accounting.is_none());
    sqlx::query("UPDATE agent_run_events SET payload_json = '{' WHERE user_id = ? AND run_id = ? AND event_idx = 0")
        .bind(&user).bind(&run).execute(pool.get()).await.unwrap();
    assert!(
        store.load_run_status_snapshot(&user, &run).await.is_err(),
        "selected index-zero corruption must fail"
    );
}

/// Count real SQL rather than simulating the batching implementation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn keyed_batches_bound_probe_queries_and_preserve_replay() {
    let (_pool, store) = setup().await;
    let run_id = format!("large_batch_{}", uuid::Uuid::new_v4());
    let user_id = "test_user".to_string();
    let session_id = format!("sess_c_{}", uuid::Uuid::new_v4());
    insert_run_fixture(
        &_pool,
        store.as_ref(),
        durable_run_record_with_events(
            run_id.clone(),
            user_id.clone(),
            session_id.clone(),
            "agent_0",
            Vec::new(),
        ),
    )
    .await;

    let mut n = 0;
    let wide_prefix = "界".repeat(112);
    for (size, prefix, expected_probes) in [
        (0_usize, "", 0),
        (1, "", 1),
        (64, "", 1),
        (65, "", 2),
        (129, "", 3),
        (64, wide_prefix.as_str(), 2),
    ] {
        let batch: Vec<_> = (0..size)
            .map(|i| {
                make_idempotent_event(
                    "test_event",
                    &format!("{prefix}batch-{size}-{i}"),
                    json!({"n": i}),
                )
            })
            .collect();
        for replay in [false, true] {
            let queries = Queries::default();
            let subscriber =
                tracing::Dispatch::new(tracing_subscriber::registry().with(queries.clone()));
            let started = std::time::Instant::now();
            store
                .append_events_batch(&user_id, &session_id, &run_id, &batch)
                .with_subscriber(subscriber)
                .await
                .unwrap();
            let queries = queries.0.lock().unwrap();
            let probes = queries
                .iter()
                .filter(|sql| {
                    let sql = sql.trim_start();
                    (sql.starts_with("SELECT") || sql.starts_with("WITH"))
                        && sql.contains("agent_run_events")
                        && sql.contains("idempotency_key")
                        && sql.contains("payload_json")
                })
                .count();
            eprintln!(
                "run_event_probe size={size} wide={} replay={replay} probes={probes} statements={} elapsed_us={}",
                !prefix.is_empty(),
                queries.len(),
                started.elapsed().as_micros()
            );
            assert_eq!(
                probes, expected_probes,
                "idempotency reads must be bounded per window"
            );
            if size == 0 {
                assert!(
                    queries.is_empty(),
                    "empty append must not access the database"
                );
            }
        }
        n += size;
        let shape: (i64, i64) = sqlx::query_as(
            "SELECT CAST(COUNT(*) AS SIGNED), CAST(SUM(OCTET_LENGTH(payload_json)) AS SIGNED) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        ).bind(&user_id).bind(&run_id).fetch_one(_pool.get()).await.unwrap();
        assert_eq!(shape.0, n as i64 + 1);
        eprintln!(
            "run_event_storage size={size} wide={} rows={} payload_bytes={}",
            !prefix.is_empty(),
            shape.0,
            shape.1
        );
    }

    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(
        loaded.events.len(),
        n + 1,
        "genesis plus all {n} events present"
    );

    let mut indices: Vec<i64> = loaded
        .events
        .iter()
        .filter_map(|e| e.get("index").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(indices.len(), n + 1);
    indices.sort_unstable();
    for (i, idx) in indices.iter().enumerate() {
        assert_eq!(*idx, i as i64, "gap at position {i}");
    }
    assert_eq!(loaded.last_event_idx, n as i64);

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn batched_probe_conflicts_collation_and_overlapping_writers_preserve_atomicity() {
    let (pool, store) = setup().await;
    let user = common::id("batch-owner");
    let session = common::id("batch-session");
    let run = common::id("batch-run");
    insert_run_fixture(
        &pool,
        store.as_ref(),
        durable_run_record(run.clone(), user.clone(), session.clone()),
    )
    .await;
    let original: Vec<_> = (0..130)
        .map(|n| make_idempotent_event("agent_progress", &format!("original-{n}"), json!({"n": n})))
        .collect();
    store
        .append_events_batch(&user, &session, &run, &original)
        .await
        .unwrap();
    let before = store.load_run(&user, &run).await.unwrap().unwrap();
    for persisted in [false, true] {
        let mut incoming: Vec<_> = (0..130)
            .map(|n| make_idempotent_event("agent_progress", &format!("new-{n}"), json!({"n": n})))
            .collect();
        let mut conflict = if persisted {
            original[129].clone()
        } else {
            incoming[0].clone()
        };
        conflict["data"]["n"] = json!("conflict");
        incoming.push(conflict);
        assert!(
            store
                .append_events_batch(&user, &session, &run, &incoming)
                .await
                .is_err()
        );
        assert_eq!(
            store.load_run(&user, &run).await.unwrap().unwrap(),
            before,
            "a later-window conflict must not change events, counter or run metadata"
        );
    }
    // Compare with the column itself, not an assumption about the DB's
    // case/accent/trailing-space collation or Rust's string equality.
    for alias in ["ORIGINAL-0", "original-0 "] {
        let equal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ? AND idempotency_key = ?",
        ).bind(&user).bind(&run).bind(alias).fetch_one(pool.get()).await.unwrap();
        let mut event = original[0].clone();
        event["idempotency_key"] = json!(alias);
        let before_alias = store.load_run(&user, &run).await.unwrap().unwrap();
        let result = store
            .append_events_batch(&user, &session, &run, &[event])
            .await;
        if equal > 0 {
            assert!(
                result.is_err(),
                "SQL-equivalent key with different immutable JSON must conflict"
            );
            assert_eq!(
                store.load_run(&user, &run).await.unwrap().unwrap(),
                before_alias
            );
        } else {
            result.unwrap();
        }
    }
    let repeated = make_idempotent_event("agent_progress", "repeated", json!({"n": 42}));
    store
        .append_events_batch(&user, &session, &run, &vec![repeated; 130])
        .await
        .unwrap();
    let before_parallel = store.load_run(&user, &run).await.unwrap().unwrap();
    let parallel: Vec<_> = (0..130)
        .map(|n| make_idempotent_event("agent_progress", &format!("parallel-{n}"), json!({"n": n})))
        .collect();
    let another_store = DatabaseRunStateStore::new(pool.clone());
    let (left, right) = tokio::join!(
        store.append_events_batch(&user, &session, &run, &parallel),
        another_store.append_events_batch(&user, &session, &run, &parallel),
    );
    left.unwrap();
    right.unwrap();
    let after = store.load_run(&user, &run).await.unwrap().unwrap();
    assert_eq!(
        after.events.len(),
        before_parallel.events.len() + parallel.len()
    );
    assert_eq!(after.last_event_idx, after.events.len() as i64 - 1);
    for (index, event) in after.events.iter().enumerate() {
        assert_eq!(event["index"], index as i64);
    }
    for table in [
        "agent_run_events",
        "agent_runs",
        "agent_sessions",
        "agent_session_lifecycle_fences",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
    }
}

/// Opt-in database capacity probe, not a claim about 1,000 running LLM loops.
/// Compare one and four independent pool/store owners at the same total quota.
#[cfg(feature = "capacity-probes")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne; opt in with --features capacity-probes"]
async fn thousand_session_append_preserves_results_across_independent_pools() {
    use futures_util::{StreamExt, future::join_all, stream};
    let (bootstrap, mut settings) = common::setup_pool_and_settings().await;
    isolated_database::require_isolated_database(&settings.database);
    for replicas in [1_usize, 4] {
        let prefix = common::id("append-scale");
        settings.db_pool_max_connections = 32 / replicas as u32;
        settings.db_pool_min_connections = 0;
        let mut pools = Vec::new();
        let mut stores = Vec::new();
        for replica in 0..replicas {
            let pool = SharedPool::new(&settings).await.unwrap();
            stores.push(
                DatabaseRunStateStore::new(pool.clone())
                    .with_owner_pod_id(format!("scale-{replica}")),
            );
            pools.push(pool);
        }
        let identities: Vec<_> = (0..1000)
            .map(|n| {
                (
                    format!("{prefix}-u{}", n / 10),
                    format!("{prefix}-s{n}"),
                    format!("{prefix}-r{n}"),
                )
            })
            .collect();
        stream::iter(identities.iter().enumerate())
            .for_each_concurrent(32, |(n, (user, session, run))| {
                let pool = &pools[n % replicas];
                let store = &stores[n % replicas];
                async move {
                    insert_run_fixture(
                        pool,
                        store,
                        durable_run_record(run.clone(), user.clone(), session.clone()),
                    )
                    .await;
                }
            })
            .await;
        let batch: Vec<_> = (0..16)
            .map(|n| make_idempotent_event("agent_progress", &format!("scale-{n}"), json!({"n":n})))
            .collect();
        for replay in [false, true] {
            let barrier = Arc::new(tokio::sync::Barrier::new(1000));
            let started = std::time::Instant::now();
            let calls = identities
                .iter()
                .enumerate()
                .map(|(n, (user, session, run))| {
                    // Replay through a different replica, not an instance-local cache.
                    let store = &stores[(n + usize::from(replay)) % replicas];
                    let batch = &batch;
                    let barrier = barrier.clone();
                    async move {
                        barrier.wait().await;
                        let started = std::time::Instant::now();
                        let result = store.append_events_batch(user, session, run, batch).await;
                        (result, started.elapsed().as_micros())
                    }
                });
            let results = tokio::time::timeout(Duration::from_secs(120), join_all(calls))
                .await
                .unwrap();
            let mut latencies: Vec<_> = results
                .into_iter()
                .map(|(result, elapsed)| {
                    result.unwrap();
                    elapsed
                })
                .collect();
            latencies.sort_unstable();
            eprintln!(
                "append_scale replicas={replicas} total_connections=32 sessions=1000 owners=100 replay={replay} wall_ms={} p50_us={} p95_us={} p99_us={}",
                started.elapsed().as_millis(),
                latencies[499],
                latencies[949],
                latencies[989]
            );
            let bad: i64 = sqlx::query_scalar(
                "SELECT CAST(COUNT(*) AS SIGNED) FROM (
                   SELECT runs.run_id FROM agent_runs runs
                   LEFT JOIN agent_run_events events
                     ON events.user_id = runs.user_id AND events.run_id = runs.run_id
                   WHERE runs.user_id LIKE ? GROUP BY runs.run_id
                   HAVING COUNT(events.id) <> 17 OR MIN(events.event_idx) <> 0
                       OR MAX(events.event_idx) <> 16 OR MAX(runs.last_event_idx) <> 16
                 ) invalid_runs",
            )
            .bind(format!("{prefix}%"))
            .fetch_one(bootstrap.get())
            .await
            .unwrap();
            assert_eq!(
                bad, 0,
                "every run must retain its exact contiguous result after cross-replica replay"
            );
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id LIKE ?")
                    .bind(format!("{prefix}%"))
                    .fetch_one(bootstrap.get())
                    .await
                    .unwrap();
            assert_eq!(count, 1000);
        }
        let facts = [
            json!({"event_type":"run_started","idempotency_key":"status-start","data":{"transport":"edge","explain_analyze_requested":true}}),
            json!({"event_type":"workspace_bound","idempotency_key":"status-workspace","data":{"workspace":{"id":"scale-workspace"}}}),
            json!({"event_type":"executor_bound","idempotency_key":"status-executor","data":{"executor":{"id":"scale-executor"}}}),
            json!({"event_type":"run_finished","idempotency_key":"status-finish","data":{"prompt_tokens":10}}),
            json!({"event_type":"run_accounting_finalized","idempotency_key":"status-accounting","data":{"prompt_tokens":20}}),
            json!({"event_type":"run_finished","idempotency_key":"status-later-finish","data":{"prompt_tokens":99}}),
        ];
        eprintln!("status_fixture replicas={replicas} prefix={prefix}");
        // Both empty and populated facts must work through another pool owner.
        for populated in [false, true] {
            if populated {
                stream::iter(identities.iter().enumerate())
                    .for_each_concurrent(32, |(n, (user, session, run))| {
                        let store = &stores[n % replicas];
                        let facts = &facts;
                        async move {
                            store
                                .append_events_batch(user, session, run, facts)
                                .await
                                .unwrap();
                        }
                    })
                    .await;
            }
            let started = std::time::Instant::now();
            let statuses = identities
                .iter()
                .enumerate()
                .map(|(n, (user, session, run))| {
                    let store = &stores[(n + 1) % replicas];
                    async move {
                        let started = std::time::Instant::now();
                        let status = store
                            .load_run_status_snapshot(user, run)
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            (&status.user_id, &status.session_id, &status.run_id),
                            (user, session, run)
                        );
                        assert_eq!(status.last_event_idx, if populated { 22 } else { 16 });
                        assert_eq!(status.explain_requested, populated);
                        if populated {
                            assert_eq!(status.workspace.unwrap()["id"], "scale-workspace");
                            assert_eq!(status.executor.unwrap()["id"], "scale-executor");
                            assert_eq!(status.transport.as_deref(), Some("edge"));
                            assert_eq!(status.accounting.unwrap()["prompt_tokens"], 20);
                        } else {
                            assert!(
                                status.workspace.is_none()
                                    && status.executor.is_none()
                                    && status.accounting.is_none()
                            );
                        }
                        started.elapsed().as_micros()
                    }
                });
            let mut latencies = tokio::time::timeout(Duration::from_secs(120), join_all(statuses))
                .await
                .unwrap();
            latencies.sort_unstable();
            eprintln!(
                "status_scale replicas={replicas} populated={populated} total_connections=32 sessions=1000 owners=100 wall_ms={} p50_us={} p95_us={}",
                started.elapsed().as_millis(),
                latencies[499],
                latencies[949]
            );
        }
        drop(stores);
        for pool in pools {
            pool.close().await;
        }
    }
}

/// Dedup + non-keyed events: events without idempotency_key are never affected
/// by dedup, even when mixed with keyed events in the same batch.
#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn dedup_preserves_non_keyed_events() {
    let (_pool, store) = setup().await;
    let run_id = format!("mix_dedup_{}", uuid::Uuid::new_v4());
    let user_id = "test_user".to_string();
    let session_id = format!("sess_c_{}", uuid::Uuid::new_v4());
    insert_run_fixture(
        &_pool,
        store.as_ref(),
        durable_run_record_with_events(
            run_id.clone(),
            user_id.clone(),
            session_id.clone(),
            "agent_0",
            Vec::new(),
        ),
    )
    .await;

    // Batch 1: 1 non-keyed + 2 keyed
    let batch1 = vec![
        make_event("text_delta", json!({"text": "first"})),
        make_idempotent_event("task_start", "task_A", json!({"n": 0})),
        make_idempotent_event("tool_call", "tool_A", json!({"n": 0})),
    ];
    store
        .append_events_batch(&user_id, &session_id, &run_id, &batch1)
        .await
        .unwrap();

    // Batch 2: keyed duplicates + 2 new non-keyed
    let batch2 = vec![
        make_event("heartbeat", json!({"ts": 1})),
        batch1[1].clone(),
        batch1[2].clone(),
        make_event("heartbeat", json!({"ts": 2})),
    ];
    store
        .append_events_batch(&user_id, &session_id, &run_id, &batch2)
        .await
        .unwrap();

    // Re-send batch2 — keyed skipped, non-keyed re-inserted
    store
        .append_events_batch(&user_id, &session_id, &run_id, &batch2)
        .await
        .unwrap();

    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    // Expected: genesis(1) + batch1(3) + batch2-nonkeyed(2) + repeat-nonkeyed(2) = 8
    assert_eq!(loaded.events.len(), 8);

    let types: Vec<String> = loaded
        .events
        .iter()
        .filter_map(|e| {
            e.get("event_type")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();

    let task_starts: Vec<_> = types.iter().filter(|t| *t == "task_start").collect();
    let tool_calls: Vec<_> = types.iter().filter(|t| *t == "tool_call").collect();
    let _heartbeats: Vec<_> = types.iter().filter(|t| *t == "heartbeat").collect();

    assert_eq!(task_starts.len(), 1);
    assert_eq!(tool_calls.len(), 1);
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Trait delegation: `append_event` (single) correctly delegates to `append_events_batch`
/// via the trait default. Also verifies that the DB `RunStateStore::append_event`
/// produces the same result as `DatabaseRunStateStore::append_events_batch` with one element.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn append_event_delegates_to_append_events_batch() {
    let (_pool, store) = setup().await;
    let user_id = format!("ae-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("ae-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("ae-run-{}", uuid::Uuid::new_v4());

    let mut record = durable_run_record(run_id.clone(), user_id.clone(), session_id.clone());
    record.agent_id = Some("agent-ae".to_string());
    // Use the RunStateStore trait method (not DatabaseRunStateStore directly)
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    // append via the trait's `append_event` (delegates to append_events_batch)
    let event = make_event("user_query", serde_json::json!({"message": "hello"}));
    store
        .append_event(&user_id, &session_id, &run_id, event)
        .await
        .expect("append_event via trait");

    // Verify the event was stored
    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(loaded.events.len(), 2);
    assert_eq!(loaded.events[1]["event_type"], "user_query");
    assert!(loaded.events[1].get("message").and_then(|v| v.as_str()) == Some("hello"));

    // Verify last_event_idx was updated
    assert_eq!(loaded.last_event_idx, 1);

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

/// Exact immutable replay preserves idempotency and event index accounting.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn insert_ignore_toctou_dedup_and_index_accounting() {
    let (_pool, store) = setup().await;
    let user_id = format!("tctou-u-{}", uuid::Uuid::new_v4());
    let session_id = format!("tctou-s-{}", uuid::Uuid::new_v4());
    let run_id = format!("tctou-r-{}", uuid::Uuid::new_v4());

    let events: Vec<serde_json::Value> = vec![
        make_idempotent_event("run_started", "tctou-k1", json!({})),
        make_idempotent_event("tool_call", "tctou-k2", json!({"name": "ls"})),
    ];

    let record = durable_run_record_with_events(
        run_id.clone(),
        user_id.clone(),
        session_id.clone(),
        "tctou-agent",
        events.clone(),
    );
    insert_run_fixture(&_pool, store.as_ref(), record).await;

    let count_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(count_before, 1);

    // Re-append the physical event; the opening is owned by immutable Run metadata.
    store
        .append_events_batch(&user_id, &session_id, &run_id, &events[1..])
        .await
        .expect("re-append same keys");

    let count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(count_after, 1, "no new events — all deduped");

    // last_event_idx unchanged: actually_inserted=0 → early return.
    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(loaded.last_event_idx, 1);

    // Mixed batch: one exact immutable replay plus one new event advances by one.
    let mixed: Vec<serde_json::Value> = vec![
        events[1].clone(),
        make_idempotent_event("tool_result", "tctou-k3", json!({"output": "done"})),
    ];
    store
        .append_events_batch(&user_id, &session_id, &run_id, &mixed)
        .await
        .expect("mixed batch");

    let count_final: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(count_final, 2, "one new event plus one physical original");

    let loaded2 = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(
        loaded2.last_event_idx, 2,
        "last_event_idx advanced by exactly 1 (for the single new event)"
    );

    // Cleanup
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn terminal_transition_persists_error_code_with_event_batch() {
    let (pool, store) = setup().await;
    let user_id = format!("tc-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("tc-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("tc-run-{}", uuid::Uuid::new_v4());

    insert_run_fixture(
        &pool,
        store.as_ref(),
        durable_run_record(run_id.clone(), user_id.clone(), session_id.clone()),
    )
    .await;

    let events = vec![
        make_event(
            "run_error",
            json!({
                "error": "[network] LLM request failed",
                "error_code": "network",
                "error_kind": "network"
            }),
        ),
        make_event(
            "run_finished",
            json!({
                "status": "failed",
                "error_code": "network",
                "error_kind": "network"
            }),
        ),
    ];

    let updated = store
        .update_run_status_with_events_if_current(
            &user_id,
            &session_id,
            &run_id,
            &["running"],
            None,
            "failed",
            None,
            Some("[network] LLM request failed"),
            &events,
        )
        .await
        .expect("terminal transition");
    assert!(updated);

    let loaded = store
        .load_run(&user_id, &run_id)
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(loaded.status, "failed");
    assert_eq!(loaded.error_code.as_deref(), Some("network"));
    assert_eq!(
        loaded.error_message.as_deref(),
        Some("[network] LLM request failed")
    );
    assert_eq!(loaded.events.len(), 3);
    assert_eq!(loaded.events[1]["event_type"], "run_error");
    assert_eq!(loaded.events[2]["event_type"], "run_finished");

    let db_error_code: Option<String> =
        sqlx::query_scalar("SELECT error_code FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .fetch_one(pool.get())
            .await
            .expect("select error_code");
    assert_eq!(db_error_code.as_deref(), Some("network"));

    let _ = sqlx::query("DELETE FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(pool.get())
        .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn root_discovery_uses_admission_order_and_decodes_narrow_owner_scoped_identity() {
    let (pool, store) = setup().await;
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("explain-discovery-user-{suffix}");
    let session_id = format!("explain-discovery-session-{suffix}");
    let older_root = format!("explain-older-{suffix}");
    let tied_root = format!("ordinary-a-{suffix}");
    let newer_root = format!("ordinary-z-{suffix}");
    let current_root = format!("current-root-{suffix}");
    let later_root = format!("later-root-{suffix}");
    let child = format!("explain-child-{suffix}");
    let other_user = format!("other-user-{suffix}");
    let other_session = format!("other-session-{suffix}");
    let foreign_user_root = format!("foreign-user-root-{suffix}");
    let foreign_session_root = format!("foreign-session-root-{suffix}");
    let fixtures = [
        (&older_root, &user_id, &session_id, 1),
        (&tied_root, &user_id, &session_id, 2),
        (&newer_root, &user_id, &session_id, 2),
        (&current_root, &user_id, &session_id, 3),
        (&later_root, &user_id, &session_id, 4),
        (&child, &user_id, &session_id, 5),
        (&foreign_user_root, &other_user, &session_id, 5),
        (&foreign_session_root, &user_id, &other_session, 5),
    ];

    for (run_id, owner, session, day) in fixtures {
        let mut run = durable_run_record(run_id.clone(), owner.clone(), session.clone());
        run.status = "completed".into();
        run.checkpoint_json = Some("x".repeat(512 * 1024));
        if run_id == &child {
            run.depth = 1;
            run.parent_run_id = Some(newer_root.clone());
            run.root_run_id = Some(newer_root.clone());
            run.ancestor_path = Some(format!("{newer_root}/{child}"));
        }
        insert_run_fixture(&pool, store.as_ref(), run).await;
        store
            .append_event(
                owner,
                session,
                run_id,
                make_event(
                    "run_started",
                    json!({"data": {"explain_analyze_requested": run_id == &older_root}}),
                ),
            )
            .await
            .expect("append run start");
        let created_at = format!("2026-01-{day:02} 00:00:00.000000");
        let updated_at = if run_id == &older_root {
            "2026-02-01 00:00:00.000000"
        } else {
            &created_at
        };
        sqlx::query(
            "UPDATE agent_runs SET created_at = ?, updated_at = ? WHERE user_id = ? AND run_id = ?",
        )
        .bind(&created_at)
        .bind(updated_at)
        .bind(owner)
        .bind(run_id)
        .execute(pool.get())
        .await
        .expect("set deterministic admission and update ordering");
    }

    assert_eq!(
        store
            .find_latest_root_run(&user_id, &session_id, None)
            .await
            .expect("discover latest root"),
        Some((later_root.clone(), 1)),
        "newer child and foreign roots must not affect selection"
    );
    for (owner, session) in [
        ("not-the-owner", session_id.as_str()),
        (user_id.as_str(), "not-the-session"),
    ] {
        assert_eq!(
            store
                .find_latest_root_run(owner, session, None)
                .await
                .unwrap(),
            None
        );
    }

    assert_eq!(
        store
            .find_latest_root_run(&user_id, &session_id, Some(&current_root))
            .await
            .expect("select strictly before current root"),
        Some((newer_root.clone(), 1)),
        "ordinary previous wins over later roots and the later-updated older Explain root"
    );
    assert_eq!(
        store
            .find_latest_root_run(&user_id, &session_id, Some(&newer_root))
            .await
            .unwrap(),
        Some((tied_root.clone(), 1)),
        "run ID is the strict upper bound when admission timestamps tie"
    );
    let missing_root = format!("missing-root-{suffix}");
    for current in [
        &older_root,
        &missing_root,
        &foreign_user_root,
        &foreign_session_root,
        &child,
    ] {
        assert_eq!(
            store
                .find_latest_root_run(&user_id, &session_id, Some(current))
                .await
                .unwrap(),
            None,
            "earliest or invalid current root must not select a later root or older Explain: {current}"
        );
    }

    sqlx::query("UPDATE agent_runs SET run_generation = -1 WHERE user_id = ? AND run_id = ?")
        .bind(&user_id)
        .bind(&newer_root)
        .execute(pool.get())
        .await
        .expect("seed invalid stored generation");
    let error = store
        .find_latest_root_run(&user_id, &session_id, Some(&current_root))
        .await
        .expect_err(
            "invalid selected generation must fail closed, not select an older Explain root",
        );
    assert!(error.contains("run_generation") || error.contains("negative"));

    for (run_id, owner, _, _) in fixtures {
        sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?")
            .bind(owner)
            .bind(run_id)
            .execute(pool.get())
            .await
            .expect("clean discovery events");
        sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(owner)
            .bind(run_id)
            .execute(pool.get())
            .await
            .expect("clean discovery runs");
    }
    for (owner, session) in [
        (&user_id, &session_id),
        (&other_user, &session_id),
        (&user_id, &other_session),
    ] {
        sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
            .bind(owner)
            .bind(session)
            .execute(pool.get())
            .await
            .expect("clean discovery session");
    }
}

async fn assert_auto_model_routing_commit(store: &dyn RunStateStore, run: &DurableRunRecord) {
    use astra_services::model_routing::{DECISION_KEY, EVENT_TYPE, ModelRoutingDecision};
    use astra_turn_types::model_routing::{AutoModelRoutingPolicy, ModelRoutingReason};
    let decision = ModelRoutingDecision {
        rollout: None,
        schema_version: 2,
        features: None,
        policy_version: "easy-read-only-v1".into(),
        policy: AutoModelRoutingPolicy {
            revision: "test-qualified-v1".into(),
            economy_offering_id: "economy".into(),
            strong_offering_id: "strong".into(),
        },
        run_id: run.run_id.clone(), session_id: run.session_id.clone(),
        selected_offering_id: "economy".into(), selected_model: "economy-model".into(),
        selected_contract_root: "test-contract".into(), input_reference: None,
        assessment: None, reason: ModelRoutingReason::EasyReadOnly,
        work_admission: Some(astra_services::parse_work_admission_response(r#"{"work_lifecycle":"not_required","workspace_mutation":"read_only","execution_topology":"primary"}"#).unwrap()),
        work_admission_skill_revision: 0,
        delegation_model_requirement: None,
        delegation_model_source: None,
    };
    let event =
        json!({"event_type": EVENT_TYPE, "idempotency_key": DECISION_KEY, "data": decision});
    let mut batch = vec![event.clone()];
    batch.extend((0..24).map(|index| {
        json!({
            "event_type":"explain_analyze", "idempotency_key":format!("explain-{index}"),
            "data": {"schema_version":1, "event_id":format!("fact-{index}"),
                "run_id":run.run_id, "turn_id":"turn", "node_id":format!("node-{index}"),
                "producer_id":"server", "clock_domain_id":"clock", "kind":"turn",
                "label":"Turn", "transition":"finished", "elapsed_ms":10,
                "start_elapsed_ms":0, "duration_ms":10, "outcome":"completed"}
        })
    }));
    batch.push(batch[1].clone());
    for (owner, session, generation, status) in [
        (
            "wrong-owner",
            run.session_id.as_str(),
            run.run_generation,
            "running",
        ),
        (
            run.user_id.as_str(),
            "wrong-session",
            run.run_generation,
            "running",
        ),
        (
            run.user_id.as_str(),
            run.session_id.as_str(),
            run.run_generation + 1,
            "running",
        ),
        (
            run.user_id.as_str(),
            run.session_id.as_str(),
            run.run_generation,
            "paused",
        ),
    ] {
        assert!(
            !store
                .append_events_if_current_generation_and_status(
                    owner,
                    session,
                    &run.run_id,
                    generation,
                    &[status],
                    &batch,
                )
                .await
                .unwrap()
        );
    }
    let mut invalid = event.clone();
    invalid["data"]["policy"]["strong_offering_id"] = json!("wrong-baseline");
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &run.user_id,
                &run.session_id,
                &run.run_id,
                run.run_generation,
                &["running"],
                &[invalid, batch[1].clone()],
            )
            .await
            .is_err()
    );
    let mut staged_conflict = batch[1].clone();
    staged_conflict["data"]["outcome"] = json!("failed");
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &run.user_id,
                &run.session_id,
                &run.run_id,
                run.run_generation,
                &["running"],
                &[event.clone(), batch[1].clone(), staged_conflict],
            )
            .await
            .is_err()
    );
    let unchanged = store
        .load_run(&run.user_id, &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.model_offering_id.as_deref(), Some("strong"));
    assert!(
        !unchanged
            .events
            .iter()
            .any(|event| event["event_type"] == "explain_analyze")
    );
    assert!(
        !unchanged
            .events
            .iter()
            .any(|e| e["event_type"] == EVENT_TYPE)
    );
    for _ in 0..2 {
        assert!(
            store
                .append_events_if_current_generation_and_status(
                    &run.user_id,
                    &run.session_id,
                    &run.run_id,
                    run.run_generation,
                    &["running"],
                    &batch,
                )
                .await
                .unwrap()
        );
    }
    let committed = store
        .load_run(&run.user_id, &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.model_offering_id.as_deref(), Some("economy"));
    assert_eq!(
        committed.resolved_model_name.as_deref(),
        Some("economy-model")
    );
    assert_eq!(
        committed
            .events
            .iter()
            .filter(|e| e["event_type"] == EVENT_TYPE)
            .count(),
        1
    );
    let facts = committed
        .events
        .iter()
        .filter(|event| event["event_type"] == "explain_analyze")
        .collect::<Vec<_>>();
    assert_eq!(
        facts.len(),
        24,
        "same-batch and committed retries must not duplicate facts"
    );
    let mut fact_conflict = batch[1].clone();
    fact_conflict["data"]["outcome"] = json!("failed");
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &run.user_id,
                &run.session_id,
                &run.run_id,
                run.run_generation,
                &["running"],
                &[fact_conflict],
            )
            .await
            .is_err()
    );
    let mut conflict = event;
    conflict["data"]["selected_offering_id"] = json!("strong");
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &run.user_id,
                &run.session_id,
                &run.run_id,
                run.run_generation,
                &["running"],
                &[conflict],
            )
            .await
            .is_err()
    );
    let after = store
        .load_run(&run.user_id, &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.model_offering_id, committed.model_offering_id);
    assert_eq!(after.events, committed.events);
}

#[tokio::test]
async fn auto_model_routing_memory_commit_is_atomic_and_fenced() {
    let store = astra_services::runs::InMemoryRunStateStore::new();
    let mut run = durable_run_record(
        "routing-memory".into(),
        "routing-user".into(),
        "routing-session".into(),
    );
    run.model_offering_id = Some("strong".into());
    run.resolved_model_name = Some("strong-model".into());
    store.insert_run(run.clone()).await.unwrap();
    assert_auto_model_routing_commit(&store, &run).await;
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn auto_model_routing_database_commit_is_atomic_and_fenced() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let (pool, store) = setup().await;
    let id = uuid::Uuid::new_v4().to_string();
    let mut run = durable_run_record(
        format!("routing-{id}"),
        format!("routing-user-{id}"),
        format!("routing-session-{id}"),
    );
    run.model_offering_id = Some("strong".into());
    run.resolved_model_name = Some("strong-model".into());
    insert_run_fixture(&pool, store.as_ref(), run.clone()).await;
    assert_auto_model_routing_commit(store.as_ref(), &run).await;
}
