//! Contract test for Plan §2.1 — `session_todos` is authoritative across
//! edge and cloud for a given `session_id`.
//!
//! Scenario:
//!   1. Two `MatrixOneTaskStore` instances (one for each "node") share the
//!      same MatrixOne pool — this is the cross-node setup in production.
//!   2. An edge-side `TaskManager` creates a task via `task_create`.
//!   3. A cloud-side `TaskManager` with the **same** `session_id` lists
//!      tasks and must see it.
//!   4. Snapshot → mutate → restore round-trips through MO (turn rollback
//!      path).
//!   5. `status=deleted` soft-removes; `status=cancelled` is distinct.
//!
//! Gated by `ASTRA_TEST_DB_IT=1` (ignored by default). Safe to run in
//! parallel with other suite tests: every test generates a unique
//! `session_id` so row scoping is clean.

#![cfg(test)]

use std::sync::Arc;

use astra_core::MatrixOneSettings;
use astra_services::storage::ensure_core_schema;
use astra_tools::task_mgmt::{TaskManager, TaskStore};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use serde_json::json;

async fn bootstrap_pool() -> sqlx::Pool<sqlx::MySql> {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 to run this ignored test"
    );
    let settings = MatrixOneSettings::from_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema");
    astra_core::connect_matrixone(&settings)
        .await
        .expect("connect matrixone")
}

async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str) {
    let _ = sqlx::query("DELETE FROM session_todos WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM session_todo_counters WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn edge_created_task_visible_on_cloud() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-x-client-{}", uuid::Uuid::new_v4());
    cleanup(&pool, &session_id).await;

    let edge_store: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let cloud_store: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    let create = edge
        .create(&json!({"title": "shared task", "active_form": "sharing"}))
        .await;
    assert!(create.contains("\"success\":true"), "{create}");

    let list = cloud.list(&json!({"status": "all"})).await;
    assert!(
        list.contains("shared task"),
        "cloud TaskManager did not see edge-created task: {list}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn snapshot_restore_roundtrips_through_mo() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-snap-{}", uuid::Uuid::new_v4());
    cleanup(&pool, &session_id).await;

    let store: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let mgr = TaskManager::new(session_id.clone(), store);
    mgr.create(&json!({"title": "t1"})).await;
    let snap = mgr.snapshot_state().await;
    mgr.create(&json!({"title": "t2"})).await;
    let pre_restore = mgr.list(&json!({"status": "all"})).await;
    assert!(pre_restore.contains("\"count\":2"), "{pre_restore}");

    mgr.restore_snapshot(&snap).await.expect("restore");
    let post = mgr.list(&json!({"status": "all"})).await;
    assert!(
        post.contains("\"count\":1"),
        "restore should drop t2: {post}"
    );
    // Next id must be reset so t2 reuses the same ordinal (task-2).
    let recreate = mgr.create(&json!({"title": "t2-again"})).await;
    assert!(recreate.contains("task-2"), "{recreate}");

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn snapshot_restore_after_cross_client_allocations_reuses_ids_without_collision() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-snap-x-client-{}", uuid::Uuid::new_v4());
    cleanup(&pool, &session_id).await;

    let edge_store: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let cloud_store: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    let t1 = edge.create(&json!({"title": "snapshot survivor"})).await;
    assert!(t1.contains("task-1"), "{t1}");
    let snap = edge.snapshot_state().await;

    let t2 = cloud.create(&json!({"title": "rolled back t2"})).await;
    let t3 = cloud.create(&json!({"title": "rolled back t3"})).await;
    assert!(t2.contains("task-2"), "{t2}");
    assert!(t3.contains("task-3"), "{t3}");
    let before_restore = cloud.list(&json!({"status": "all"})).await;
    assert!(before_restore.contains("\"count\":3"), "{before_restore}");

    edge.restore_snapshot(&snap).await.expect("restore");
    let post_restore = cloud.list(&json!({"status": "all"})).await;
    assert!(
        post_restore.contains("\"count\":1"),
        "restore should roll the shared session back to the snapshot: {post_restore}"
    );
    assert!(
        !post_restore.contains("rolled back t2") && !post_restore.contains("rolled back t3"),
        "post-snapshot rows must not survive the rollback: {post_restore}"
    );

    let recreate = cloud
        .create(&json!({"title": "new t2 after restore"}))
        .await;
    assert!(recreate.contains("task-2"), "{recreate}");
    let final_list = edge.list(&json!({"status": "all"})).await;
    assert!(final_list.contains("\"count\":2"), "{final_list}");
    assert!(
        final_list.matches("task-2").count() == 1,
        "task-2 must be reused exactly once after restore, without duplicate ids: {final_list}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn deleted_and_cancelled_are_distinct_transitions() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-states-{}", uuid::Uuid::new_v4());
    cleanup(&pool, &session_id).await;

    let store: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let mgr = TaskManager::new(session_id.clone(), store);
    mgr.create(&json!({"title": "to-cancel"})).await;
    mgr.create(&json!({"title": "to-delete"})).await;

    let cancel = mgr
        .stop(&json!({"task_id": "task-1", "reason": "no longer needed"}))
        .await;
    assert!(cancel.contains("\"success\":true"), "{cancel}");
    let after_cancel = mgr.get(&json!({"task_id": "task-1"})).await;
    assert!(
        after_cancel.contains("\"status\": \"cancelled\""),
        "expected cancelled status: {after_cancel}"
    );

    let delete = mgr
        .update(&json!({"task_id": "task-2", "status": "deleted"}))
        .await;
    assert!(delete.contains("\"status\":\"deleted\""), "{delete}");
    let after_delete = mgr.get(&json!({"task_id": "task-2"})).await;
    assert!(
        after_delete.contains("not found"),
        "deleted task should be soft-removed: {after_delete}"
    );

    // After delete, task-1 (cancelled) is still present — cancel ≠ delete.
    let list = mgr.list(&json!({"status": "all"})).await;
    assert!(
        list.contains("to-cancel"),
        "cancelled task still listed: {list}"
    );
    assert!(!list.contains("to-delete"), "deleted task leaked: {list}");

    cleanup(&pool, &session_id).await;
}

/// Regression: `next_task_id` must work under concurrent callers sharing
/// the same `session_id` on MatrixOne. MatrixOne rejects the MySQL
/// `LAST_INSERT_ID(expr)` counter idiom, so the production implementation
/// uses `SELECT … FOR UPDATE` in an explicit transaction. This test catches
/// both MatrixOne-incompatible SQL and obvious duplicate/gap regressions
/// under contention.
///
/// This test spawns N concurrent allocations against the SAME session
/// (the worst case — edge + cloud racing) and asserts all returned ids
/// are unique and densely packed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn concurrent_next_task_id_is_unique() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-race-{}", uuid::Uuid::new_v4());
    cleanup(&pool, &session_id).await;

    // Two independent stores sharing the pool — mirrors prod cross-host.
    let store_a: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));
    let store_b: Arc<dyn TaskStore> = Arc::new(MatrixOneTaskStore::new(pool.clone()));

    const CONCURRENCY: u32 = 16;
    let mut handles = Vec::with_capacity(CONCURRENCY as usize);
    for i in 0..CONCURRENCY {
        let store = if i % 2 == 0 {
            store_a.clone()
        } else {
            store_b.clone()
        };
        let sid = session_id.clone();
        handles.push(tokio::spawn(async move {
            store
                .next_task_id(&sid)
                .await
                .expect("next_task_id must succeed under contention")
        }));
    }

    let mut ids = Vec::with_capacity(CONCURRENCY as usize);
    for h in handles {
        ids.push(h.await.expect("join"));
    }
    ids.sort_unstable();

    // Dense 1..=CONCURRENCY (counter starts at 0, +1 on each allocation).
    let expected: Vec<u32> = (1..=CONCURRENCY).collect();
    assert_eq!(
        ids, expected,
        "concurrent next_task_id returned duplicates or gaps: {ids:?}"
    );

    cleanup(&pool, &session_id).await;
}
