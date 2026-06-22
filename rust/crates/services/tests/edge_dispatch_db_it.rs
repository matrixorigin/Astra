//! MatrixOne-backed integration tests for edge dispatch and sweeper leases.
//!
//! Run with:
//!   ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!     --test edge_dispatch_db_it -- --ignored --test-threads=1

mod common;

use astra_services::multi_agent::{
    DatabaseEdgeDispatchService, DatabaseEdgeRegistryService, EdgeDispatchService,
    EdgeRegistryService,
};
use sqlx::Row;
use uuid::Uuid;

// ── Helpers ───────────────────────────────────────────────────────────

fn unique_suffix() -> String {
    Uuid::new_v4().to_string()
}

/// All online tests require the env var gate.
fn require_env() {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// DatabaseEdgeDispatchService
// ═══════════════════════════════════════════════════════════════════════

/// Insert → poll returns pending → mark_dispatched → poll returns empty → deliver_result → completed.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_full_lifecycle() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-usr-{}", unique_suffix());
    let agent_id = format!("ed-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();

    // 1. Insert dispatch
    let payload = r#"{"tool":"bash","args":{"command":"echo hi"}}"#;
    let dispatch_id = svc
        .insert_dispatch(&user_id, &agent_id, &request_id, payload)
        .await
        .expect("insert_dispatch");
    assert!(dispatch_id > 0, "dispatch_id should be positive");

    // 2. Poll atomically claims and marks as dispatched within a transaction
    let rows = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("poll_pending");
    assert_eq!(rows.len(), 1, "should have 1 pending dispatch");
    assert_eq!(rows[0].dispatch_id, dispatch_id);

    // 3. Poll again — now empty (row was claimed in step 2)
    let rows = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("poll_pending after claim");
    assert!(rows.is_empty(), "should be empty after claiming");

    // 4. Deliver result (with edge_agent_id for auth)
    let ok = svc
        .deliver_result(&request_id, &agent_id, r#"{"output":"hello"}"#)
        .await
        .expect("deliver_result");
    assert!(ok, "deliver_result should return true for existing request");

    // 6. wait_result returns the result
    let result = svc
        .wait_result(&request_id, std::time::Duration::from_secs(5))
        .await
        .expect("wait_result");
    assert!(
        result.is_some(),
        "wait_result should return Some with result json"
    );
    assert!(
        result.unwrap().contains("hello"),
        "result should contain hello"
    );
}

/// deliver_result for unknown request_id returns false.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_deliver_result_nonexistent_returns_false() {
    require_env();
    let pool = common::setup_pool().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let ok = svc
        .deliver_result("no-such-request", "any-agent", "{}")
        .await
        .expect("deliver_result");
    assert!(
        !ok,
        "deliver_result for nonexistent request should return false"
    );
}

/// deliver_result with wrong edge_agent_id must return false (security boundary).
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_deliver_result_wrong_agent_rejected() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-usr-{}", unique_suffix());
    let agent_id = format!("ed-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();

    // Insert dispatch with agent_id
    svc.insert_dispatch(&user_id, &agent_id, &request_id, r#"{"test":true}"#)
        .await
        .expect("insert_dispatch");

    // Try to deliver with a DIFFERENT agent — must be rejected
    let ok = svc
        .deliver_result(&request_id, "wrong-agent-id", r#"{"output":"stolen"}"#)
        .await
        .expect("deliver_result");
    assert!(
        !ok,
        "deliver_result with wrong agent_id MUST return false — cross-agent injection"
    );

    // Verify the original agent can still deliver
    let ok = svc
        .deliver_result(&request_id, &agent_id, r#"{"output":"legit"}"#)
        .await
        .expect("deliver_result");
    assert!(
        ok,
        "correct agent_id should succeed after wrong agent rejection"
    );
}

/// wait_result times out if request never completes.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_wait_result_timeout() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-to-usr-{}", unique_suffix());
    let agent_id = format!("ed-to-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();

    svc.insert_dispatch(&user_id, &agent_id, &request_id, "{}")
        .await
        .expect("insert_dispatch");

    // 200ms timeout — should not be enough to deliver result
    let result = svc
        .wait_result(&request_id, std::time::Duration::from_millis(200))
        .await
        .expect("wait_result");
    assert!(result.is_none(), "wait_result should time out (Some=None)");
}

/// poll_pending is scoped to (user_id, edge_agent_id) — different user sees nothing.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_poll_isolation() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_a = format!("ed-iso-usr-a-{}", unique_suffix());
    let user_b = format!("ed-iso-usr-b-{}", unique_suffix());
    let agent = format!("ed-iso-agent-{}", unique_suffix());

    svc.insert_dispatch(&user_a, &agent, &Uuid::new_v4().to_string(), "{}")
        .await
        .expect("insert for user A");

    let rows = svc
        .poll_pending(&user_b, &agent)
        .await
        .expect("poll for user B");
    assert!(rows.is_empty(), "user B should not see user A's dispatch");
}

/// cleanup_stale removes completed dispatches older than N seconds.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_cleanup_stale_removes_completed() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;
    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    let user_id = format!("ed-cln-usr-{}", unique_suffix());
    let agent_id = format!("ed-cln-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();

    svc.insert_dispatch(&user_id, &agent_id, &request_id, "{}")
        .await
        .expect("insert_dispatch");

    // Complete it
    svc.deliver_result(&request_id, &agent_id, r#"{"done":true}"#)
        .await
        .expect("deliver_result");

    // Make only this test row stale. Using cleanup_stale(0) races under
    // nextest/online parallelism because it can expire unrelated pending
    // dispatches created by sibling tests.
    sqlx::query(
        "UPDATE edge_pending_dispatch \
         SET completed_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) \
         WHERE request_id = ?",
    )
    .bind(&request_id)
    .execute(pool.get())
    .await
    .expect("backdate completed dispatch");

    // Cleanup rows older than one day. The backdated row is removed without
    // touching newly-created pending rows from concurrently-running tests.
    let removed = svc
        .cleanup_stale(std::time::Duration::from_secs(24 * 60 * 60))
        .await
        .expect("cleanup_stale");
    assert!(removed >= 1, "should have removed at least 1 completed row");

    // wait_result now returns None (row was deleted)
    let result = svc
        .wait_result(&request_id, std::time::Duration::from_millis(100))
        .await
        .expect("wait_result after cleanup");
    assert!(result.is_none(), "request should be gone after cleanup");
}

// ═══════════════════════════════════════════════════════════════════════
// Sweeper leases
// ═══════════════════════════════════════════════════════════════════════

/// Create a lease row, then verify it exists.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn sweeper_lease_create_and_query() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let sweeper_name = format!("test-sweeper-{}", unique_suffix());
    let pod_id = format!("pod-{}", unique_suffix());

    // Insert a lease
    sqlx::query(
        "INSERT INTO sweeper_leases (sweeper_name, owner_pod_id, expires_at) \
         VALUES (?, ?, DATE_ADD(NOW(6), INTERVAL 60 SECOND))",
    )
    .bind(&sweeper_name)
    .bind(&pod_id)
    .execute(pool.get())
    .await
    .expect("insert lease");

    // Verify it exists
    let row = sqlx::query(
        "SELECT sweeper_name, owner_pod_id, expires_at \
         FROM sweeper_leases WHERE sweeper_name = ?",
    )
    .bind(&sweeper_name)
    .fetch_one(pool.get())
    .await
    .expect("fetch lease");
    assert_eq!(row.get::<String, _>("sweeper_name"), sweeper_name);
    assert_eq!(row.get::<String, _>("owner_pod_id"), pod_id);
}

/// Acquire lease: INSERT ... ON DUPLICATE KEY ... WHERE expired.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn sweeper_lease_acquire_when_expired() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let sweeper_name = format!("acq-sweeper-{}", unique_suffix());
    let pod_a = format!("pod-a-{}", unique_suffix());
    let pod_b = format!("pod-b-{}", unique_suffix());

    // Insert an expired lease (past)
    sqlx::query(
        "INSERT INTO sweeper_leases (sweeper_name, owner_pod_id, expires_at) \
         VALUES (?, ?, DATE_SUB(NOW(6), INTERVAL 10 SECOND))",
    )
    .bind(&sweeper_name)
    .bind(&pod_a)
    .execute(pool.get())
    .await
    .expect("insert expired lease");

    // Acquire: update expired lease to new pod (any pod can acquire expired leases)
    let result = sqlx::query(
        "UPDATE sweeper_leases \
         SET owner_pod_id = ?, expires_at = DATE_ADD(NOW(6), INTERVAL 120 SECOND) \
         WHERE sweeper_name = ? AND expires_at < NOW(6)",
    )
    .bind(&pod_b)
    .bind(&sweeper_name)
    .execute(pool.get())
    .await
    .expect("acquire expired lease");
    assert_eq!(result.rows_affected(), 1, "should acquire expired lease");

    // Verify the lease is still owned by pod_b
    let row = sqlx::query(
        "SELECT owner_pod_id, expires_at \
         FROM sweeper_leases WHERE sweeper_name = ?",
    )
    .bind(&sweeper_name)
    .fetch_one(pool.get())
    .await
    .expect("fetch after renew");
    assert_eq!(row.get::<String, _>("owner_pod_id"), pod_b);
}

// ── Edge Registry Tests ──────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_register_list_unregister() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id_header = format!("edge_{}", unique_suffix());

    // Register
    let record = svc
        .register_or_update(&user_id, &edge_agent_id, &edge_id_header, None, None, None)
        .await
        .expect("register");
    assert_eq!(record.user_id, user_id);
    assert_eq!(record.edge_agent_id, edge_agent_id);
    assert_eq!(record.edge_id, edge_id_header);

    // List returns the agent
    let list = svc.list_by_user(&user_id).await.expect("list_by_user");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].edge_agent_id, edge_agent_id);

    // Unregister
    svc.unregister(&user_id, &edge_agent_id)
        .await
        .expect("unregister");

    // List returns empty
    let list = svc.list_by_user(&user_id).await.expect("list_by_user");
    assert_eq!(list.len(), 0);
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_heartbeat_unregistered() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id_header = format!("edge_{}", unique_suffix());

    let result = svc
        .heartbeat(&user_id, &edge_agent_id, &edge_id_header)
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "edge agent not registered");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_concurrent_register() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc1 = DatabaseEdgeRegistryService::new(pool.clone());
    let svc2 = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id1 = format!("edge_{}", unique_suffix());
    let edge_id2 = format!("edge_{}", unique_suffix());

    // Concurrent registration with same key
    let (r1, r2) = tokio::join!(
        svc1.register_or_update(&user_id, &edge_agent_id, &edge_id1, None, None, None),
        svc2.register_or_update(&user_id, &edge_agent_id, &edge_id2, None, None, None)
    );

    let rec1 = r1.expect("register1");
    let rec2 = r2.expect("register2");

    // Both succeed, one is the final winner
    assert_eq!(rec1.user_id, user_id);
    assert_eq!(rec2.user_id, user_id);

    // List returns exactly one record
    let list = svc1.list_by_user(&user_id).await.expect("list");
    assert_eq!(list.len(), 1);
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn edge_registry_register_twice_updates() {
    require_env();
    let pool = common::setup_pool().await.get().clone();
    let svc = DatabaseEdgeRegistryService::new(pool.clone());

    let user_id = format!("user_{}", unique_suffix());
    let edge_agent_id = format!("agent_{}", unique_suffix());
    let edge_id1 = format!("edge_{}", unique_suffix());
    let edge_id2 = format!("edge_{}", unique_suffix());
    let hostname1 = Some("host1");
    let hostname2 = Some("host2");

    // First register
    let rec1 = svc
        .register_or_update(&user_id, &edge_agent_id, &edge_id1, hostname1, None, None)
        .await
        .expect("register1");
    assert_eq!(rec1.edge_id, edge_id1);
    assert_eq!(rec1.hostname, Some("host1".to_string()));

    // Second register updates
    let rec2 = svc
        .register_or_update(&user_id, &edge_agent_id, &edge_id2, hostname2, None, None)
        .await
        .expect("register2");
    assert_eq!(rec2.edge_id, edge_id2);
    assert_eq!(rec2.hostname, Some("host2".to_string()));

    // List returns one record with updated fields
    let list = svc.list_by_user(&user_id).await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].edge_id, edge_id2);
    assert_eq!(list[0].hostname, Some("host2".to_string()));
}

// ── Concurrent poll_pending tests ────────────────────────────────────

/// Two concurrent poll_pending calls must NOT claim the same row.
/// The first transaction commits (SET dispatched), the second sees
/// status='dispatched' and skips it. Assert exactly one winner
/// and the dispatch status is correct.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_concurrent_poll_single_winner() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let user_id = format!("ed-cc-usr-{}", unique_suffix());
    let agent_id = format!("ed-cc-agent-{}", unique_suffix());
    let request_id = Uuid::new_v4().to_string();

    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());
    svc.insert_dispatch(&user_id, &agent_id, &request_id, r#"{"tool":"bash"}"#)
        .await
        .expect("insert_dispatch");

    use tokio::sync::Barrier;
    let barrier = std::sync::Arc::new(Barrier::new(2));
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let pool1 = pool.get().clone();
    let pool2 = pool.get().clone();
    let user1 = user_id.clone();
    let user2 = user_id.clone();
    let ag1 = agent_id.clone();
    let ag2 = agent_id.clone();

    let (r1, r2) = tokio::join!(
        async move {
            b1.wait().await;
            let svc = DatabaseEdgeDispatchService::new(pool1);
            svc.poll_pending(&user1, &ag1).await
        },
        async move {
            b2.wait().await;
            let svc = DatabaseEdgeDispatchService::new(pool2);
            svc.poll_pending(&user2, &ag2).await
        }
    );

    let rows1 = r1.expect("poll 1");
    let rows2 = r2.expect("poll 2");

    let total = rows1.len() + rows2.len();
    assert_eq!(total, 1, "exactly one poll call should claim the row");
    assert!(
        rows1.is_empty() ^ rows2.is_empty(),
        "one poll should be empty, the other gets the row"
    );

    // Verify the claimed row has status='dispatched'
    let claimed_rows = if !rows1.is_empty() { &rows1 } else { &rows2 };
    assert_eq!(claimed_rows[0].request_id, request_id);
    assert_eq!(
        claimed_rows[0].status, "dispatched",
        "claimed row status must be 'dispatched'"
    );

    // poll_pending again — must be empty
    let rows3 = svc.poll_pending(&user_id, &agent_id).await.expect("poll 3");
    assert!(rows3.is_empty(), "no more pending after concurrent claim");
}

/// With N pending rows and M concurrent pollers, verify exactly N rows
/// are claimed total and status='dispatched' on each.
#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_dispatch_concurrent_poll_multi_row_multi_poller() {
    require_env();
    let (pool, _settings) = common::setup_pool_and_settings().await;

    let user_id = format!("ed-cm-usr-{}", unique_suffix());
    let agent_id = format!("ed-cm-agent-{}", unique_suffix());

    let svc = DatabaseEdgeDispatchService::new(pool.get().clone());

    // Insert 3 pending rows
    let mut request_ids = Vec::new();
    for i in 0..3 {
        let rid = format!("{}-{}", Uuid::new_v4(), i);
        svc.insert_dispatch(&user_id, &agent_id, &rid, r#"{"test":true}"#)
            .await
            .expect("insert_dispatch");
        request_ids.push(rid);
    }

    // 4 concurrent pollers for 3 rows — one poller must get nothing
    use tokio::sync::Barrier;
    let n = 4;
    let barrier = std::sync::Arc::new(Barrier::new(n));
    let mut handles = Vec::new();

    for _ in 0..n {
        let pool = pool.get().clone();
        let user = user_id.clone();
        let agent = agent_id.clone();
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            let svc = DatabaseEdgeDispatchService::new(pool);
            svc.poll_pending(&user, &agent).await
        }));
    }

    let mut total_claimed = 0usize;
    for h in handles {
        let rows = h.await.expect("join").expect("poll");
        for row in &rows {
            assert_eq!(
                row.status, "dispatched",
                "every claimed row must have status='dispatched'"
            );
        }
        total_claimed += rows.len();
    }

    assert_eq!(total_claimed, 3, "exactly 3 rows should be claimed total");
    // No more pending
    let remaining = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("final poll");
    assert!(remaining.is_empty(), "no rows should remain after claim");
}
