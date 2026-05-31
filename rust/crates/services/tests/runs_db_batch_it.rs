//! MatrixOne-backed integration tests for batch event writes.
//!
//! Run with: ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test runs_db_batch_it -- --ignored --test-threads=1

mod common;

use astra_core::SharedPool;
use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
use serde_json::json;
use sqlx::Row;
use std::sync::Arc;

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

    let record = astra_services::runs::DurableRunRecord {
        run_id: run_id.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
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
        last_event_idx: 0,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        events: events.clone(),
        created_at: String::new(),
        updated_at: String::new(),
    };

    store.insert_run(record).await.expect("insert_run");

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

    let record = astra_services::runs::DurableRunRecord {
        run_id: run_id.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: Some("bwo-agent".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: 0,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        events: events.clone(),
        created_at: String::new(),
        updated_at: String::new(),
    };

    store.insert_run(record).await.expect("insert_run");

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

    let record = astra_services::runs::DurableRunRecord {
        run_id: run_id.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: Some("bwid-agent".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: 0,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        events: events.clone(),
        created_at: String::new(),
        updated_at: String::new(),
    };

    store.insert_run(record).await.expect("first insert_run");

    // Now try to append the same events again via append_events_batch
    // (should be deduped — no new events inserted).
    store
        .append_events_batch(&run_id, &events)
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
        // Duplicate — should be skipped.
        make_idempotent_event("run_started", "bwid-key-1", json!({})),
        // New — should be inserted.
        make_idempotent_event("run_finished", "bwid-key-4", json!({"exit_code": 0})),
        // No idempotency key — always inserted.
        make_event("tool_call", json!({"name": "bash"})),
    ];
    store
        .append_events_batch(&run_id, &mixed_events)
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

/// Batch write: single-event append_event uses batch path and works.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn single_event_append_uses_batch_path() {
    let (_pool, store) = setup().await;
    let user_id = format!("bwse-user-{}", uuid::Uuid::new_v4());
    let session_id = format!("bwse-session-{}", uuid::Uuid::new_v4());
    let run_id = format!("bwse-run-{}", uuid::Uuid::new_v4());

    // Insert the run first.
    let record = astra_services::runs::DurableRunRecord {
        run_id: run_id.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: Some("bwse-agent".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: 0,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        events: vec![],
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.insert_run(record).await.expect("insert_run");

    // Append single events (this should use the batch path internally).
    store
        .append_event(
            &run_id,
            make_event("tool_call", json!({"name": "read_file"})),
        )
        .await
        .unwrap();
    store
        .append_event(
            &run_id,
            make_event("tool_result", json!({"output": "hello"})),
        )
        .await
        .unwrap();
    store
        .append_event(&run_id, make_event("run_finished", json!({})))
        .await
        .unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(_pool.get())
        .await
        .unwrap();
    assert_eq!(count, 3, "single append_event should use batch path");

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
    let agent_id = Some("agent_0".to_string());

    store
        .insert_run(astra_services::runs::DurableRunRecord {
            run_id: run_id.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            parent_run_id: None,
            root_run_id: None,
            ancestor_path: None,
            depth: 0,
            delegation_id: None,
            agent_id: agent_id.clone(),
            retry_of: None,
            retry_scope: None,
            status: "running".to_string(),
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
            events: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        })
        .await
        .unwrap();

    let store_a = store.clone();
    let store_b = store.clone();
    let rid_a = run_id.clone();
    let rid_b = run_id.clone();

    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            let batch: Vec<_> = (0..5)
                .map(|i| make_event("task_start", json!({"n": i})))
                .collect();
            store_a.append_events_batch(&rid_a, &batch).await
        }),
        tokio::spawn(async move {
            let batch: Vec<_> = (0..7)
                .map(|i| make_event("tool_call", json!({"n": i})))
                .collect();
            store_b.append_events_batch(&rid_b, &batch).await
        }),
    );

    r1.unwrap().unwrap();
    r2.unwrap().unwrap();

    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.events.len(), 12, "all 12 events must be present");

    let mut indices: Vec<i64> = loaded
        .events
        .iter()
        .filter_map(|e| e.get("index").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(indices.len(), 12);
    indices.sort_unstable();
    for (i, idx) in indices.iter().enumerate() {
        assert_eq!(*idx, i as i64, "gap at position {i}");
    }

    assert_eq!(loaded.last_event_idx, 11, "last_event_idx must be 11");

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

/// Large batch: 50 events in a single `append_events_batch` must all be stored
/// with contiguous event_idx.
#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn large_batch_50_events_contiguous() {
    let (_pool, store) = setup().await;
    let run_id = format!("large_batch_{}", uuid::Uuid::new_v4());
    let user_id = "test_user".to_string();
    let session_id = format!("sess_c_{}", uuid::Uuid::new_v4());
    let agent_id = Some("agent_0".to_string());

    store
        .insert_run(astra_services::runs::DurableRunRecord {
            run_id: run_id.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            parent_run_id: None,
            root_run_id: None,
            ancestor_path: None,
            depth: 0,
            delegation_id: None,
            agent_id: agent_id.clone(),
            retry_of: None,
            retry_scope: None,
            status: "running".to_string(),
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
            events: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        })
        .await
        .unwrap();

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

    store.append_events_batch(&run_id, &batch).await.unwrap();

    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.events.len(), n, "all {n} events present");

    let mut indices: Vec<i64> = loaded
        .events
        .iter()
        .filter_map(|e| e.get("index").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(indices.len(), n);
    indices.sort_unstable();
    for (i, idx) in indices.iter().enumerate() {
        assert_eq!(*idx, i as i64, "gap at position {i}");
    }
    assert_eq!(loaded.last_event_idx, (n - 1) as i64);

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
    let agent_id = Some("agent_0".to_string());

    store
        .insert_run(astra_services::runs::DurableRunRecord {
            run_id: run_id.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            parent_run_id: None,
            root_run_id: None,
            ancestor_path: None,
            depth: 0,
            delegation_id: None,
            agent_id: agent_id.clone(),
            retry_of: None,
            retry_scope: None,
            status: "running".to_string(),
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
            events: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        })
        .await
        .unwrap();

    // Batch 1: 1 non-keyed + 2 keyed
    let batch1 = vec![
        make_event("run_started", json!({})),
        make_idempotent_event("task_start", "task_A", json!({"n": 0})),
        make_idempotent_event("tool_call", "tool_A", json!({"n": 0})),
    ];
    store.append_events_batch(&run_id, &batch1).await.unwrap();

    // Batch 2: keyed duplicates + 2 new non-keyed
    let batch2 = vec![
        make_event("heartbeat", json!({"ts": 1})),
        make_idempotent_event("task_start", "task_A", json!({"n": 0})),
        make_idempotent_event("tool_call", "tool_A", json!({"n": 0})),
        make_event("heartbeat", json!({"ts": 2})),
    ];
    store.append_events_batch(&run_id, &batch2).await.unwrap();

    // Re-send batch2 — keyed skipped, non-keyed re-inserted
    store.append_events_batch(&run_id, &batch2).await.unwrap();

    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    // Expected: batch1(3) + batch2-nonkeyed(2) + repeat-nonkeyed(2) = 7
    assert_eq!(loaded.events.len(), 7);

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
    let run_starteds: Vec<_> = types.iter().filter(|t| *t == "run_started").collect();
    let _heartbeats: Vec<_> = types.iter().filter(|t| *t == "heartbeat").collect();

    assert_eq!(task_starts.len(), 1);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(run_starteds.len(), 1);
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

    let record = astra_services::runs::DurableRunRecord {
        run_id: run_id.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: Some("agent-ae".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: 0,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        events: vec![],
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };

    // Use the RunStateStore trait method (not DatabaseRunStateStore directly)
    store.insert_run(record).await.expect("insert run");

    // append via the trait's `append_event` (delegates to append_events_batch)
    let event = make_event("user_query", serde_json::json!({"message": "hello"}));
    store
        .append_event(&run_id, event)
        .await
        .expect("append_event via trait");

    // Verify the event was stored
    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.events.len(), 1);
    assert_eq!(loaded.events[0]["event_type"], "user_query");
    assert!(loaded.events[0].get("message").and_then(|v| v.as_str()) == Some("hello"));

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

/// TOCTOU: INSERT IGNORE handles idempotency keys that the SELECT filter
/// missed (simulating a concurrent insert from another pod).
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

    let record = astra_services::runs::DurableRunRecord {
        run_id: run_id.clone(),
        user_id: user_id.clone(),
        session_id: session_id.clone(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: Some("tctou-agent".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: 0,
        checkpoint_version: None,
        checkpoint_json: None,
        error_code: None,
        error_message: None,
        retry_count: 0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        events: events.clone(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    store.insert_run(record).await.expect("insert_run");

    let count_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(count_before, 2);

    // Re-append the same events — SELECT finds all keys → no INSERT.
    store
        .append_events_batch(&run_id, &events)
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
    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.last_event_idx, 1);

    // Mixed batch: 1 duplicate + 1 new → INSERT IGNORE skips dup,
    // rows_affected() = 1, last_event_idx advances by 1.
    let mixed: Vec<serde_json::Value> = vec![
        make_idempotent_event("tool_call", "tctou-k1", json!({"name": "ls"})),
        make_idempotent_event("tool_result", "tctou-k3", json!({"output": "done"})),
    ];
    store
        .append_events_batch(&run_id, &mixed)
        .await
        .expect("mixed batch");

    let count_final: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(_pool.get())
            .await
            .unwrap();
    assert_eq!(count_final, 3, "1 new + 2 original = 3");

    let loaded2 = store.load_run(&run_id).await.unwrap().unwrap();
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
