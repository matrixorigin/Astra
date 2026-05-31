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

    // 2. Poll returns the pending dispatch
    let rows = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("poll_pending");
    assert_eq!(rows.len(), 1, "should have 1 pending dispatch");
    assert_eq!(rows[0].dispatch_id, dispatch_id);
    assert_eq!(rows[0].status, "pending");

    // 3. Mark as dispatched
    svc.mark_dispatched(&[dispatch_id])
        .await
        .expect("mark_dispatched");

    // 4. Poll again — now empty (status is 'dispatched', not 'pending')
    let rows = svc
        .poll_pending(&user_id, &agent_id)
        .await
        .expect("poll_pending after mark_dispatched");
    assert!(rows.is_empty(), "should be empty after marking dispatched");

    // 5. Deliver result
    let ok = svc
        .deliver_result(&request_id, r#"{"output":"hello"}"#)
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
        .deliver_result("no-such-request", "{}")
        .await
        .expect("deliver_result");
    assert!(
        !ok,
        "deliver_result for nonexistent request should return false"
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
    svc.deliver_result(&request_id, r#"{"done":true}"#)
        .await
        .expect("deliver_result");

    // Cleanup with 0 seconds — removes all completed rows
    let removed = svc
        .cleanup_stale(std::time::Duration::from_secs(0))
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
