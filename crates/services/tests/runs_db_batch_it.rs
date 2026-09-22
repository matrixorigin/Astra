//! MatrixOne-backed integration tests for batch event writes.
//!
//! Run with: ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test runs_db_batch_it -- --ignored --test-threads=1

mod common;

use astra_core::SharedPool;
use astra_services::auth::session::{DatabaseSessionService, SessionService};
use astra_services::runs::{
    DatabaseRunStateStore, DurableRunRecord, RunStateStore, ToolOutputBatchItem,
};
use serde_json::json;
use sqlx::{Connection, Row, mysql::MySqlConnection};
use std::{sync::Arc, time::Duration};

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
        retry_count: 0,
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

    assert_eq!(rows.len(), 5);

    let types: Vec<String> = rows.iter().map(|r| r.get("event_type")).collect();
    assert_eq!(
        types,
        vec![
            "run_started",
            "tool_call",
            "tool_result",
            "text_delta",
            "run_finished"
        ]
    );

    let indices: Vec<i64> = rows.iter().map(|r| r.get("event_idx")).collect();
    for (i, idx) in indices.iter().enumerate() {
        assert_eq!(*idx, i as i64, "event_idx should be sequential");
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
    let _ = sqlx::query("DELETE FROM run_display_projections WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
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
    let _ = sqlx::query("DELETE FROM run_display_projections WHERE run_id = ?")
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
            "DELETE FROM run_display_projections WHERE user_id = ? AND run_id = ?",
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

/// Owner isolation: dirty tool output rows for another user/session with the
/// same batch/output identity must not block or contaminate the owner batch.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn tool_output_batches_isolate_identity_by_owner_session() {
    let (_pool, store) = setup().await;
    let owner_user_id = format!("tool-owner-{}", uuid::Uuid::new_v4());
    let foreign_user_id = format!("tool-foreign-{}", uuid::Uuid::new_v4());
    let owner_session_id = format!("tool-owner-session-{}", uuid::Uuid::new_v4());
    let foreign_session_id = format!("tool-foreign-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("tool-run-{}", uuid::Uuid::new_v4());
    let foreign_run_id = format!("tool-foreign-run-{}", uuid::Uuid::new_v4());
    let batch_id = format!("batch-{}", uuid::Uuid::new_v4());
    let output_id = format!("output-{}", uuid::Uuid::new_v4());
    let missing_tool_name = format!("missing-tool-{}", uuid::Uuid::new_v4());

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
        "INSERT INTO session_tool_output_batches
         (batch_id, session_id, run_id, user_id, output_count, payload_bytes, status, created_at)
         VALUES (?, ?, ?, ?, 1, 17, 'committed', NOW(6))",
    )
    .bind(&batch_id)
    .bind(&foreign_session_id)
    .bind(&foreign_run_id)
    .bind(&foreign_user_id)
    .execute(_pool.get())
    .await
    .expect("insert foreign dirty batch");
    sqlx::query(
        "INSERT INTO session_tool_outputs
         (output_id, batch_id, session_id, run_id, user_id, output_idx, parent_output_id,
          tool_call_id, tool_name, output_json, payload_bytes, preview_text, preview_status,
          artifact_ref, content_hash, normalize_version, created_at)
         VALUES (?, ?, ?, ?, ?, 0, NULL, 'foreign-call', 'bash', ?, 17, 'foreign',
                 'template', NULL, 'foreign-hash', 'raw_v1', NOW(6))",
    )
    .bind(&output_id)
    .bind(&batch_id)
    .bind(&foreign_session_id)
    .bind(&foreign_run_id)
    .bind(&foreign_user_id)
    .bind(r#"{"source":"foreign_row"}"#)
    .execute(_pool.get())
    .await
    .expect("insert foreign dirty output");

    store
        .insert_tool_output_batch(
            &batch_id,
            &owner_session_id,
            &run_id,
            &owner_user_id,
            &[ToolOutputBatchItem {
                output_id: output_id.clone(),
                tool_call_id: Some("owner-call".into()),
                tool_name: missing_tool_name.clone(),
                result: astra_turn_types::ToolInvocationResultPayload::new(
                    "owner_row",
                    Default::default(),
                    None,
                )
                .unwrap(),
            }],
        )
        .await
        .expect("owner tool output batch insert");

    let same_batch_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_tool_output_batches WHERE batch_id = ?")
            .bind(&batch_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(
        same_batch_count, 2,
        "batch identity must include owner/session"
    );

    let same_output_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_tool_outputs WHERE batch_id = ? AND output_idx = 0",
    )
    .bind(&batch_id)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert_eq!(
        same_output_count, 2,
        "output batch ordering must include owner/session"
    );

    let owner_payload: String = sqlx::query_scalar(
        "SELECT output_json FROM session_tool_outputs
         WHERE user_id = ? AND session_id = ? AND output_id = ?",
    )
    .bind(&owner_user_id)
    .bind(&owner_session_id)
    .bind(&output_id)
    .fetch_one(_pool.get())
    .await
    .unwrap();
    assert!(
        owner_payload.contains("owner_row"),
        "owner output payload must not be overwritten by foreign row: {owner_payload}"
    );

    let owner_event_count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&owner_session_id)
    .bind(&owner_user_id)
    .fetch_one(_pool.get())
    .await
    .expect("load owner event_count");
    assert_eq!(
        owner_event_count, 0,
        "fallback persistence must not change session event_count"
    );

    let missing_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events
         WHERE session_id = ? AND user_id = ? AND event_type = 'preview_template_missing'
           AND meta_tool_name = ?",
    )
    .bind(&owner_session_id)
    .bind(&owner_user_id)
    .bind(&missing_tool_name)
    .fetch_one(_pool.get())
    .await
    .expect("count owner preview-template diagnostic events");
    assert_eq!(missing_event_count, 0);

    let _ = sqlx::query("DELETE FROM session_tool_outputs WHERE batch_id = ?")
        .bind(&batch_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM session_tool_output_batches WHERE batch_id = ?")
        .bind(&batch_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query(
        "DELETE FROM agent_events
         WHERE (session_id = ? AND user_id = ?)
            OR (session_id = ? AND user_id = ?)",
    )
    .bind(&owner_session_id)
    .bind(&owner_user_id)
    .bind(&foreign_session_id)
    .bind(&foreign_user_id)
    .execute(_pool.get())
    .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .execute(_pool.get())
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
        .bind(&owner_session_id)
        .bind(&owner_user_id)
        .execute(_pool.get())
        .await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn tool_output_batches_fence_all_tools_without_session_diagnostic_mutations() {
    let (pool, store) = setup().await;
    let user_id = format!("preview-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("preview-session-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, event_count, last_event_id, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 7, 'prior-event', '2026-01-01', '2026-01-01', '2026-01-01')",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool.get()).await.unwrap();

    for state in ["active", "deleting", "pending_delete", "deleted", "missing"] {
        if state == "deleting" {
            sqlx::query("UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?")
                .bind(&user_id).bind(&session_id).execute(pool.get()).await.unwrap();
        } else if state == "pending_delete" {
            // An active root must still be rejected by the durable fence.
            sqlx::query(
                "UPDATE agent_sessions SET status = 'active' WHERE user_id = ? AND session_id = ?",
            )
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap();
            sqlx::query("UPDATE agent_session_lifecycle_fences SET delete_requested_at = NOW(6) WHERE user_id = ? AND session_id = ?")
                .bind(&user_id).bind(&session_id).execute(pool.get()).await.unwrap();
        } else if state == "deleted" {
            sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
                .bind(&user_id)
                .bind(&session_id)
                .execute(pool.get())
                .await
                .unwrap();
            sqlx::query("UPDATE agent_session_lifecycle_fences SET database_deleted_at = NOW(6) WHERE user_id = ? AND session_id = ?")
                .bind(&user_id).bind(&session_id).execute(pool.get()).await.unwrap();
        } else if state == "missing" {
            sqlx::query(
                "DELETE FROM agent_session_lifecycle_fences WHERE user_id = ? AND session_id = ?",
            )
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap();
        }

        for name in [
            "read_file",
            "ask_user",
            "unknown_preview_tool",
            "empty_batch",
        ] {
            let batch_id = uuid::Uuid::new_v4().to_string();
            let items = if name == "empty_batch" {
                vec![]
            } else {
                vec![ToolOutputBatchItem {
                    output_id: uuid::Uuid::new_v4().to_string(),
                    tool_call_id: None,
                    tool_name: name.to_string(),
                    result: astra_turn_types::ToolInvocationResultPayload::new(
                        "bounded output",
                        Default::default(),
                        None,
                    )
                    .unwrap(),
                }]
            };
            // Retrying must neither manufacture diagnostic events nor escape admission.
            for _ in 0..2 {
                let result = store
                    .insert_tool_output_batch(
                        &batch_id,
                        &session_id,
                        "preview-run",
                        &user_id,
                        &items,
                    )
                    .await;
                if state == "active" {
                    result.unwrap();
                } else {
                    assert!(
                        matches!(
                            result,
                            Err(astra_services::runs::DatabaseRunStateStoreError::Database {
                                operation: "admit_tool_output_batch",
                                source: sqlx::Error::RowNotFound,
                                ..
                            })
                        ),
                        "{state}/{name}: {result:?}"
                    );
                }
            }
            for table in ["session_tool_output_batches", "session_tool_outputs"] {
                let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ? AND batch_id = ?"))
                    .bind(&user_id).bind(&session_id).bind(&batch_id).fetch_one(pool.get()).await.unwrap();
                let expected = if state == "active"
                    && (table == "session_tool_output_batches" || !items.is_empty())
                {
                    1
                } else {
                    0
                };
                assert_eq!(count, expected, "{state}/{name}/{table}");
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE user_id = ? AND session_id = ? AND batch_id = ?"
                ))
                .bind(&user_id)
                .bind(&session_id)
                .bind(&batch_id)
                .execute(pool.get())
                .await
                .unwrap();
            }
        }
        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(events, 0);
        if state == "active" {
            let unchanged: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?
                 AND event_count = 7 AND last_event_id = 'prior-event'
                 AND updated_at = '2026-01-01' AND last_active_at = '2026-01-01'",
            )
            .bind(&user_id)
            .bind(&session_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
            assert_eq!(
                unchanged, 1,
                "preview persistence cannot mutate session diagnostics or activity"
            );
        }
    }
    let roots: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(roots, 0, "output writes must not lazily recreate a session");
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
        "SELECT COUNT(*) FROM agent_session_execution_slots
         WHERE user_id = ? AND session_id = ?",
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
        "run_display_projections",
        "agent_run_events",
        "agent_session_execution_slots",
        "agent_runs",
        "agent_session_lifecycle_fences",
        "agent_sessions",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id = ? AND session_id = ?");
        sqlx::query(&statement)
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
        (
            "run_display_projections",
            "user_id = ? AND session_id = ? AND run_id = ?",
        ),
    ] {
        let statement = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
        let count: i64 = sqlx::query_scalar(&statement)
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

    for table in [
        "agent_session_execution_slots",
        "agent_session_lifecycle_fences",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id = ? AND session_id = ?");
        sqlx::query(&statement)
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("cleanup {table}: {error}"));
    }
}

/// Large batch: 50 events in a single `append_events_batch` must all be stored
/// with contiguous event_idx.
#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn large_batch_50_events_contiguous() {
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

    let n: usize = 50;
    let batch: Vec<_> = (0..n)
        .map(|i| {
            serde_json::json!({
                "type": "test_event",
                "event_id": format!("ev_{i}"),
                "data": {"n": i},
                "ignored": "timestamp column removed",
            })
        })
        .collect();

    store
        .append_events_batch(&user_id, &session_id, &run_id, &batch)
        .await
        .unwrap();

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
    assert_eq!(count_before, 2);

    // Re-append the same events — SELECT finds all keys → no INSERT.
    store
        .append_events_batch(&user_id, &session_id, &run_id, &events)
        .await
        .expect("re-append same keys");

    let count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(count_after, 2, "no new events — all deduped");

    // last_event_idx unchanged: actually_inserted=0 → early return.
    let loaded = store.load_run(&user_id, &run_id).await.unwrap().unwrap();
    assert_eq!(loaded.last_event_idx, 1);

    // Mixed batch: one exact immutable replay plus one new event advances by one.
    let mixed: Vec<serde_json::Value> = vec![
        events[0].clone(),
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
    assert_eq!(count_final, 3, "1 new + 2 original = 3");

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
async fn explain_root_discovery_is_owner_scoped_root_only_and_decodes_narrow_identity() {
    let (pool, store) = setup().await;
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("explain-discovery-user-{suffix}");
    let session_id = format!("explain-discovery-session-{suffix}");
    let older_root = format!("explain-older-{suffix}");
    let newer_root = format!("explain-newer-{suffix}");
    let child = format!("explain-child-{suffix}");

    for run_id in [&older_root, &newer_root, &child] {
        let mut run = durable_run_record(run_id.clone(), user_id.clone(), session_id.clone());
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
                &user_id,
                &session_id,
                run_id,
                make_event(
                    "run_started",
                    json!({"data": {"explain_analyze_requested": true}}),
                ),
            )
            .await
            .expect("append Explain request marker");
    }
    sqlx::query(
        "UPDATE agent_runs SET updated_at = CASE run_id
             WHEN ? THEN '2026-01-01 00:00:00.000000'
             WHEN ? THEN '2026-01-02 00:00:00.000000'
             ELSE '2026-01-03 00:00:00.000000' END
         WHERE user_id = ? AND run_id IN (?, ?, ?)",
    )
    .bind(&older_root)
    .bind(&newer_root)
    .bind(&user_id)
    .bind(&older_root)
    .bind(&newer_root)
    .bind(&child)
    .execute(pool.get())
    .await
    .expect("set deterministic Explain ordering");

    assert_eq!(
        store
            .find_latest_explain_analyze_root(&user_id, &session_id, None)
            .await
            .expect("discover latest Explain root"),
        Some((newer_root.clone(), 1)),
        "newer root must win even when a child has the newest timestamp"
    );
    assert_eq!(
        store
            .find_latest_explain_analyze_root("not-the-owner", &session_id, None)
            .await
            .expect("wrong owner lookup"),
        None
    );

    assert_eq!(
        store
            .find_latest_explain_analyze_root(&user_id, &session_id, Some(&newer_root))
            .await
            .expect("exclude current root"),
        Some((older_root.clone(), 1)),
    );

    sqlx::query("UPDATE agent_runs SET run_generation = -1 WHERE user_id = ? AND run_id = ?")
        .bind(&user_id)
        .bind(&newer_root)
        .execute(pool.get())
        .await
        .expect("seed invalid stored generation");
    let error = store
        .find_latest_explain_analyze_root(&user_id, &session_id, None)
        .await
        .expect_err("negative generation must fail closed");
    assert!(error.contains("run_generation") || error.contains("negative"));

    sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id IN (?, ?, ?)")
        .bind(&user_id)
        .bind(&older_root)
        .bind(&newer_root)
        .bind(&child)
        .execute(pool.get())
        .await
        .expect("clean Explain events");
    sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id IN (?, ?, ?)")
        .bind(&user_id)
        .bind(&older_root)
        .bind(&newer_root)
        .bind(&child)
        .execute(pool.get())
        .await
        .expect("clean Explain runs");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(&user_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("clean Explain session");
}
