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
use astra_tools::task_mgmt::{SessionTask, SessionTaskStatusKind, TaskManager, TaskStore};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use serde_json::json;

async fn bootstrap_pool() -> sqlx::Pool<sqlx::MySql> {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 to run this ignored test"
    );
    let mut settings = MatrixOneSettings::from_env();
    settings.db_pool_max_connections = settings.db_pool_max_connections.min(4);
    settings.db_pool_min_connections = settings.db_pool_min_connections.min(1);
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
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

async fn create_session(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, agent_id, title, status, metadata)
         VALUES (?, ?, 'session-todos-test', 'session todos test', 'active', '{}')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert agent_sessions owner root");
}

async fn prepare_session(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str, user_id: &str) {
    cleanup(pool, session_id).await;
    create_session(pool, session_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn edge_created_task_visible_on_cloud() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-x-client-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), user_id.clone()).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), user_id.clone()).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    let create = edge
        .create(&json!({"title": "shared task", "active_form": "sharing"}))
        .await;
    assert!(create.contains("\"success\":true"), "{create}");

    let list = cloud.list(&json!({"status_filter": "all"})).await;
    assert!(
        list.contains("shared task"),
        "cloud TaskManager did not see edge-created task: {list}"
    );
    let counter_owner: String =
        sqlx::query_scalar("SELECT user_id FROM session_todo_counters WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("load counter owner");
    assert_eq!(
        counter_owner, user_id,
        "task id counter must carry the same owner as session_todos"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn matrixone_task_store_refuses_mixed_owner_counter_without_overwrite() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-mixed-counter-store-{}", uuid::Uuid::new_v4());
    let owner_user_id = format!("u-owner-{}", uuid::Uuid::new_v4());
    let other_user_id = format!("u-other-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &owner_user_id).await;

    sqlx::query(
        "INSERT INTO session_todo_counters (session_id, user_id, next_id, version) \
         VALUES (?, ?, 7, 3)",
    )
    .bind(&session_id)
    .bind(&other_user_id)
    .execute(&pool)
    .await
    .expect("insert other-owner counter");

    let store = MatrixOneTaskStore::new_for_user(pool.clone(), owner_user_id.clone()).unwrap();
    let set_err = store
        .set_next_task_id(&session_id, 99)
        .await
        .expect_err("set_next_task_id must reject a counter owned by another user");
    assert!(
        set_err.contains("session_todo_counters owner mismatch"),
        "unexpected set_next_task_id error: {set_err}"
    );

    let restore_err = store
        .restore_snapshot_state(&session_id, Vec::new(), 99, 0)
        .await
        .expect_err("restore_snapshot_state must reject a counter owned by another user");
    assert!(
        restore_err.contains("session_todo_counters owner mismatch"),
        "unexpected restore_snapshot_state error: {restore_err}"
    );

    let manager = TaskManager::new(
        session_id.clone(),
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), owner_user_id.clone()).unwrap()),
    );
    let create = manager
        .create(&json!({"title": "must not overwrite"}))
        .await;
    assert!(
        create.starts_with("Error:") && create.contains("session_todo_counters owner mismatch"),
        "create must fail closed on mixed-owner counter: {create}"
    );

    let (actual_owner, next_id, version): (String, i64, i64) = sqlx::query_as(
        "SELECT user_id, next_id, version FROM session_todo_counters WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load counter after rejected writes");
    assert_eq!(actual_owner, other_user_id);
    assert_eq!(next_id, 7);
    assert_eq!(version, 3);

    let owner_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_todos WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner todos");
    assert_eq!(owner_rows, 0, "failed create must not insert task rows");

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn unknown_task_fields_are_rejected_through_matrixone_store() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-unknown-fields-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    let create_typo = mgr
        .create(&json!({"title": "typo create", "titel": "wrong"}))
        .await;
    assert!(
        create_typo.starts_with("Error:")
            && create_typo.contains("unknown field")
            && create_typo.contains("titel"),
        "MatrixOne-backed create must reject typo fields before insert: {create_typo}"
    );
    let empty = mgr.list(&json!({"status_filter": "all"})).await;
    assert!(
        empty.starts_with("No tasks"),
        "rejected create must not insert a MatrixOne row: {empty}"
    );

    let create = mgr.create(&json!({"title": "real task"})).await;
    assert!(create.contains("\"success\":true"), "{create}");

    let update_typo = mgr
        .update(&json!({"task_id": "task-1", "state": "paused"}))
        .await;
    assert!(
        update_typo.starts_with("Error:")
            && update_typo.contains("unknown field")
            && update_typo.contains("state"),
        "MatrixOne-backed update must reject typo fields before mutation: {update_typo}"
    );
    let task: SessionTask =
        serde_json::from_str(&mgr.get(&json!({"task_id": "task-1"})).await).expect("task details");
    assert_eq!(task.status, SessionTaskStatusKind::Pending);

    let archive_typo = mgr
        .archive(&json!({"older_than_days": 30, "dry_run": true}))
        .await;
    assert!(
        archive_typo.starts_with("Error:")
            && archive_typo.contains("unknown field")
            && archive_typo.contains("dry_run"),
        "MatrixOne-backed archive must reject typo fields: {archive_typo}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn matrixone_update_title_refuses_duplicate_open_task() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-dup-rename-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    let first = mgr
        .create(&json!({"title": "Implement OAuth callback"}))
        .await;
    assert!(first.contains("\"success\":true"), "{first}");
    let second = mgr.create(&json!({"title": "Wire webhook"})).await;
    assert!(second.contains("\"success\":true"), "{second}");

    let dup = mgr
        .update(&json!({
            "task_id": "task-2",
            "title": " implement oauth callback. "
        }))
        .await;
    assert!(
        dup.starts_with("Refused: open task #task-1") && dup.contains("\"success\":false"),
        "MatrixOne-backed duplicate rename should be refused: {dup}"
    );

    let task_2: SessionTask =
        serde_json::from_str(&mgr.get(&json!({"task_id": "task-2"})).await).expect("task-2 json");
    assert_eq!(
        task_2.title, "Wire webhook",
        "refused MatrixOne rename must not mutate task-2"
    );
    let list = mgr.list(&json!({"status_filter": "active"})).await;
    assert!(list.contains("\"count\":2"), "{list}");
    assert!(
        list.contains("Implement OAuth callback") && list.contains("Wire webhook"),
        "active list should retain the two distinct open task titles: {list}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn snapshot_restore_roundtrips_through_mo() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-snap-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);
    mgr.create(&json!({"title": "t1"})).await;
    let mut snap = mgr
        .try_snapshot_state()
        .await
        .expect("snapshot in cross-client test");
    mgr.create(&json!({"title": "t2"})).await;
    let pre_restore = mgr.list(&json!({"status_filter": "all"})).await;
    assert!(pre_restore.contains("\"count\":2"), "{pre_restore}");

    mgr.seal_snapshot_for_restore(&mut snap)
        .await
        .expect("seal restore snapshot");
    mgr.restore_snapshot(&snap).await.expect("restore");
    let post = mgr.list(&json!({"status_filter": "all"})).await;
    assert!(
        post.contains("\"count\":1"),
        "restore should drop t2: {post}"
    );
    // Counter is never rewound; restoring t1 still preserves the higher
    // allocator watermark that t2 already consumed.
    let recreate = mgr.create(&json!({"title": "t2-again"})).await;
    assert!(recreate.contains("task-3"), "{recreate}");

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn snapshot_restore_uses_existing_rows_when_matrixone_counter_is_zero() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-zero-counter-snapshot-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), user_id.clone()).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    let create = mgr.create(&json!({"title": "surviving task"})).await;
    assert!(create.contains("task-1"), "{create}");
    sqlx::query(
        "UPDATE session_todo_counters SET next_id = 0 \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("corrupt counter to zero");

    let err = mgr
        .try_snapshot_state()
        .await
        .expect_err("snapshot with corrupt counter must fail closed");
    assert!(
        err.contains("peek_next_task_id failed") && err.contains("out of range"),
        "corrupt MatrixOne counter should be surfaced explicitly: {err}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn matrixone_restore_snapshot_rolls_back_counter_when_task_insert_fails() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-restore-atomic-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store.clone());
    let create = mgr.create(&json!({"title": "surviving task"})).await;
    assert!(create.contains("\"success\":true"), "{create}");
    assert_eq!(
        store
            .peek_next_task_id(&session_id)
            .await
            .expect("counter after create"),
        2
    );

    let bad_snapshot = astra_tools::task_mgmt::TaskManagerSnapshot {
        tasks: vec![SessionTask {
            archived_at: None,
            id: "task-99".to_string(),
            title: "bad restore row".to_string(),
            description: None,
            status: SessionTaskStatusKind::Pending,
            subtasks: Vec::new(),
            created_at: "not-a-valid-datetime".to_string(),
            updated_at: "not-a-valid-datetime".to_string(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }],
        next_task_id: 100,
        version: store
            .get_session_version(&session_id)
            .await
            .expect("version before bad restore"),
        restore_version: None,
    };

    let err = mgr
        .restore_snapshot(&bad_snapshot)
        .await
        .expect_err("invalid DATETIME should make MatrixOne restore fail");
    assert!(
        err.contains("Incorrect datetime") || err.contains("invalid") || err.contains("datetime"),
        "unexpected MatrixOne restore error: {err}"
    );
    assert_eq!(
        store
            .peek_next_task_id(&session_id)
            .await
            .expect("counter after failed restore"),
        2,
        "failed MatrixOne restore must roll back the counter update"
    );
    let list = mgr.list(&json!({"status_filter": "all"})).await;
    assert!(
        list.contains("surviving task") && !list.contains("bad restore row"),
        "failed MatrixOne restore must leave existing task rows intact: {list}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn load_open_sessions_is_bounded_open_work_and_user_scoped() {
    let pool = bootstrap_pool().await;
    let user_id = format!("u-open-sessions-{}", uuid::Uuid::new_v4());
    let other_user = format!("u-open-sessions-other-{}", uuid::Uuid::new_v4());
    let session_a = format!("s-open-sessions-a-{}", uuid::Uuid::new_v4());
    let session_b = format!("s-open-sessions-b-{}", uuid::Uuid::new_v4());
    let session_other = format!("s-open-sessions-other-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_a, &user_id).await;
    prepare_session(&pool, &session_b, &user_id).await;
    prepare_session(&pool, &session_other, &other_user).await;

    let store_a: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let store_b: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let other_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &other_user).unwrap());
    let mgr_a = TaskManager::new(session_a.clone(), store_a);
    let mgr_b = TaskManager::new(session_b.clone(), store_b);
    let other_mgr = TaskManager::new(session_other.clone(), other_store);

    mgr_a.create(&json!({"title": "pending-a"})).await;
    mgr_a.create(&json!({"title": "completed-a"})).await;
    let completed_a_started = mgr_a
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(
        !completed_a_started.starts_with("Error:"),
        "{completed_a_started}"
    );
    let completed_a = mgr_a
        .update(&json!({"task_id": "task-2", "new_status": "completed"}))
        .await;
    assert!(!completed_a.starts_with("Error:"), "{completed_a}");
    mgr_b.create(&json!({"title": "paused-b"})).await;
    mgr_b
        .update(&json!({"task_id": "task-1", "new_status": "paused"}))
        .await;
    mgr_b.create(&json!({"title": "pending-b"})).await;
    other_mgr.create(&json!({"title": "other-user-open"})).await;

    let store = MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap();
    let rows = store
        .load_open_sessions(2)
        .await
        .expect("bounded open sessions");
    let titles: Vec<&str> = rows
        .iter()
        .flat_map(|(_, tasks)| tasks.iter().map(|task| task.title.as_str()))
        .collect();
    assert_eq!(
        titles.len(),
        2,
        "MatrixOne cross-session task board fetch must respect its limit: {rows:?}"
    );
    assert!(
        !titles.contains(&"completed-a"),
        "completed history must not be returned by open-session fetch: {rows:?}"
    );
    assert!(
        !titles.contains(&"other-user-open"),
        "cross-session open fetch must stay scoped to the store user_id: {rows:?}"
    );
    assert!(
        titles
            .iter()
            .all(|title| matches!(*title, "pending-a" | "paused-b" | "pending-b")),
        "only this user's open work should appear: {rows:?}"
    );

    for session_id in [&session_a, &session_b, &session_other] {
        cleanup(&pool, session_id).await;
    }
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn active_list_includes_paused_open_work_in_matrixone() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-active-list-paused-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let manager = TaskManager::new(session_id.clone(), store);

    let pending = manager
        .create(&json!({"title": "pending active work"}))
        .await;
    assert!(!pending.starts_with("Error:"), "{pending}");
    let running = manager
        .create(&json!({"title": "running active work"}))
        .await;
    assert!(!running.starts_with("Error:"), "{running}");
    let running_update = manager
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(!running_update.starts_with("Error:"), "{running_update}");
    let paused = manager
        .create(&json!({"title": "paused active work"}))
        .await;
    assert!(!paused.starts_with("Error:"), "{paused}");
    let paused_update = manager
        .update(&json!({"task_id": "task-3", "new_status": "paused"}))
        .await;
    assert!(!paused_update.starts_with("Error:"), "{paused_update}");
    let completed = manager.create(&json!({"title": "completed history"})).await;
    assert!(!completed.starts_with("Error:"), "{completed}");
    let running_pause = manager
        .update(&json!({"task_id": "task-2", "new_status": "paused"}))
        .await;
    assert!(!running_pause.starts_with("Error:"), "{running_pause}");
    let completed_update = manager
        .update(&json!({"task_id": "task-4", "new_status": "in_progress"}))
        .await;
    assert!(
        !completed_update.starts_with("Error:"),
        "{completed_update}"
    );
    let completed_update = manager
        .update(&json!({"task_id": "task-4", "new_status": "completed"}))
        .await;
    assert!(
        !completed_update.starts_with("Error:"),
        "{completed_update}"
    );
    let running_resume = manager
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(!running_resume.starts_with("Error:"), "{running_resume}");

    let active = manager.list(&json!({"status_filter": "active"})).await;
    let parsed: serde_json::Value =
        serde_json::from_str(&active).expect("active task list should be JSON");
    let titles: Vec<&str> = parsed["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .map(|task| task["title"].as_str().expect("title"))
        .collect();
    assert!(
        titles.contains(&"pending active work")
            && titles.contains(&"running active work")
            && titles.contains(&"paused active work"),
        "MatrixOne active list should include every open-work status: {active}"
    );
    assert!(
        !titles.contains(&"completed history"),
        "MatrixOne active list should exclude terminal history: {active}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn snapshot_restore_after_cross_client_allocations_rejects_stale_snapshot_without_collision()
{
    let pool = bootstrap_pool().await;
    let session_id = format!("s-snap-x-client-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    let t1 = edge.create(&json!({"title": "snapshot survivor"})).await;
    assert!(t1.contains("task-1"), "{t1}");
    let snap = edge
        .try_snapshot_state()
        .await
        .expect("snapshot for edge-cloud test");

    let t2 = cloud.create(&json!({"title": "rolled back t2"})).await;
    let t3 = cloud.create(&json!({"title": "rolled back t3"})).await;
    assert!(t2.contains("task-2"), "{t2}");
    assert!(t3.contains("task-3"), "{t3}");
    let before_restore = cloud.list(&json!({"status_filter": "all"})).await;
    assert!(before_restore.contains("\"count\":3"), "{before_restore}");

    let err = edge
        .restore_snapshot(&snap)
        .await
        .expect_err("stale cross-client snapshot must not clobber later writes");
    assert!(err.contains("version conflict"), "{err}");
    let final_list = edge.list(&json!({"status_filter": "all"})).await;
    assert!(final_list.contains("\"count\":3"), "{final_list}");
    assert!(
        final_list.contains("snapshot survivor")
            && final_list.contains("rolled back t2")
            && final_list.contains("rolled back t3"),
        "stale restore rejection must preserve later cross-client writes: {final_list}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn deleted_and_cancelled_are_distinct_transitions() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-states-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
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
        .update(&json!({"task_id": "task-2", "new_status": "deleted"}))
        .await;
    assert!(delete.contains("\"status\":\"deleted\""), "{delete}");
    let after_delete = mgr.get(&json!({"task_id": "task-2"})).await;
    assert!(
        after_delete.contains("not found"),
        "deleted task should be soft-removed: {after_delete}"
    );

    // After delete, task-1 (cancelled) is still present — cancel ≠ delete.
    let list = mgr.list(&json!({"status_filter": "all"})).await;
    assert!(
        list.contains("to-cancel"),
        "cancelled task still listed: {list}"
    );
    assert!(!list.contains("to-delete"), "deleted task leaked: {list}");

    let cancelled = mgr.list(&json!({"status_filter": "cancelled"})).await;
    assert!(
        cancelled.contains("to-cancel"),
        "cancelled filter should include stopped task: {cancelled}"
    );
    assert!(
        !cancelled.contains("to-delete"),
        "cancelled filter should not include deleted task: {cancelled}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn bulk_archive_is_scoped_to_current_session_even_with_user_store() {
    let pool = bootstrap_pool().await;
    let session_a = format!("s-archive-a-{}", uuid::Uuid::new_v4());
    let session_b = format!("s-archive-b-{}", uuid::Uuid::new_v4());

    let user_id = format!("u-archive-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_a, &user_id).await;
    prepare_session(&pool, &session_b, &user_id).await;
    let store_a: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let store_b: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr_a = TaskManager::new(session_a.clone(), store_a);
    let mgr_b = TaskManager::new(session_b.clone(), store_b);

    mgr_a
        .create(&json!({"title": "old task in current session"}))
        .await;
    mgr_b
        .create(&json!({"title": "old task in another session"}))
        .await;
    let archived_a_started = mgr_a
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(
        !archived_a_started.starts_with("Error:"),
        "{archived_a_started}"
    );
    let archived_b_started = mgr_b
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(
        !archived_b_started.starts_with("Error:"),
        "{archived_b_started}"
    );
    let archived_a_completed = mgr_a
        .update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;
    assert!(
        !archived_a_completed.starts_with("Error:"),
        "{archived_a_completed}"
    );
    let archived_b_completed = mgr_b
        .update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;
    assert!(
        !archived_b_completed.starts_with("Error:"),
        "{archived_b_completed}"
    );

    for session_id in [&session_a, &session_b] {
        sqlx::query(
            "UPDATE session_todos \
             SET updated_at = DATE_SUB(NOW(6), INTERVAL 10 DAY) \
             WHERE session_id = ?",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("age completed task");
    }

    let archived = mgr_a.archive(&json!({"older_than_days": 7})).await;
    assert!(
        archived.contains("\"archived\":1"),
        "bulk archive should only affect the active session: {archived}"
    );

    let archived_a = mgr_a.list(&json!({"status_filter": "archived"})).await;
    assert!(
        archived_a.contains("old task in current session"),
        "current-session task should be archived: {archived_a}"
    );
    let completed_b = mgr_b.list(&json!({"status_filter": "completed"})).await;
    assert!(
        completed_b.contains("old task in another session"),
        "other session's completed task must not be archived by session_a cleanup: {completed_b}"
    );

    cleanup(&pool, &session_a).await;
    cleanup(&pool, &session_b).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn dependency_edges_remain_symmetric_across_matrixone_clients() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-edge-symmetric-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-edge-symmetric-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    edge.create(&json!({"title": "producer"})).await;
    edge.create(&json!({"title": "consumer"})).await;
    let linked = edge
        .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
        .await;
    assert!(!linked.starts_with("Error:"), "{linked}");

    let producer: SessionTask =
        serde_json::from_str(&cloud.get(&json!({"task_id": "task-1"})).await).unwrap();
    let consumer: SessionTask =
        serde_json::from_str(&cloud.get(&json!({"task_id": "task-2"})).await).unwrap();
    assert_eq!(producer.blocks, vec!["task-2"]);
    assert_eq!(consumer.blocked_by, vec!["task-1"]);

    let unlinked = cloud
        .update(&json!({"task_id": "task-2", "remove_blocked_by": ["task-1"]}))
        .await;
    assert!(!unlinked.starts_with("Error:"), "{unlinked}");
    let producer: SessionTask =
        serde_json::from_str(&edge.get(&json!({"task_id": "task-1"})).await).unwrap();
    let consumer: SessionTask =
        serde_json::from_str(&edge.get(&json!({"task_id": "task-2"})).await).unwrap();
    assert!(producer.blocks.is_empty(), "{producer:?}");
    assert!(consumer.blocked_by.is_empty(), "{consumer:?}");

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn blocked_task_cannot_start_until_dependency_completes_in_matrixone() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-blocked-start-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-blocked-start-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    edge.create(&json!({"title": "prepare"})).await;
    edge.create(&json!({"title": "consume"})).await;
    let linked = edge
        .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
        .await;
    assert!(!linked.starts_with("Error:"), "{linked}");

    let blocked = cloud
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(
        blocked.starts_with("Error:")
            && blocked.contains("cannot start")
            && blocked.contains("task-1"),
        "MatrixOne blocked task should not start before blocker completes: {blocked}"
    );

    let blocker_started = edge
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(!blocker_started.starts_with("Error:"), "{blocker_started}");
    let completed = edge
        .update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;
    assert!(!completed.starts_with("Error:"), "{completed}");
    let started = cloud
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(
        started.contains("\"success\":true") && started.contains("\"status\":\"in_progress\""),
        "{started}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn in_progress_task_rejects_new_unresolved_blocker_in_matrixone() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-running-blocker-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-running-blocker-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    edge.create(&json!({"title": "prepare"})).await;
    edge.create(&json!({"title": "already running"})).await;
    let started = edge
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(!started.starts_with("Error:"), "{started}");

    let blocked_while_running = cloud
        .update(&json!({"task_id": "task-2", "add_blocked_by": ["task-1"]}))
        .await;
    assert!(
        blocked_while_running.starts_with("Error:")
            && blocked_while_running.contains("cannot start")
            && blocked_while_running.contains("task-1")
            && blocked_while_running.contains("pending"),
        "MatrixOne must reject adding an unresolved blocker to an in_progress task: {blocked_while_running}"
    );

    let task: SessionTask =
        serde_json::from_str(&edge.get(&json!({"task_id": "task-2"})).await).unwrap();
    assert_eq!(task.status, SessionTaskStatusKind::InProgress);
    assert!(
        task.blocked_by.is_empty(),
        "rejected MatrixOne blocker edge must not persist: {task:?}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn dangling_blocked_by_dependency_blocks_start_in_matrixone() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-dangling-blocked-by-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-dangling-blocked-by-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    let create = mgr.create(&json!({"title": "dangling dependency"})).await;
    assert!(create.contains("\"success\":true"), "{create}");
    sqlx::query(
        "UPDATE session_todos SET blocked_by = ? \
         WHERE session_id = ? AND todo_id = ? AND user_id = ?",
    )
    .bind(r#"["task-missing"]"#)
    .bind(&session_id)
    .bind("task-1")
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed dangling blocked_by");

    let out = mgr
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(
        out.starts_with("Error:") && out.contains("task-missing") && out.contains("missing"),
        "MatrixOne dangling blocked_by should block start with an actionable error: {out}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn corrupt_dependency_json_fails_closed_in_matrixone() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-corrupt-blocked-by-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-corrupt-blocked-by-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    let create = mgr.create(&json!({"title": "corrupt dependency"})).await;
    assert!(create.contains("\"success\":true"), "{create}");
    sqlx::query(
        "UPDATE session_todos SET blocked_by = ? \
         WHERE session_id = ? AND todo_id = ? AND user_id = ?",
    )
    .bind("not-json")
    .bind(&session_id)
    .bind("task-1")
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed corrupt blocked_by");

    let out = mgr
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(
        out.starts_with("Error:")
            && out.contains("session_todos.blocked_by")
            && out.contains("invalid JSON"),
        "MatrixOne corrupt blocked_by must fail closed instead of clearing dependencies: {out}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn matrixone_load_rejects_unknown_persisted_task_status() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-corrupt-status-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-corrupt-status-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    let create = mgr.create(&json!({"title": "corrupt status"})).await;
    assert!(create.contains("\"success\":true"), "{create}");
    sqlx::query(
        "UPDATE session_todos SET status = ? \
         WHERE session_id = ? AND todo_id = ? AND user_id = ?",
    )
    .bind("mystery")
    .bind(&session_id)
    .bind("task-1")
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed corrupt status");

    let err = mgr
        .load_tasks()
        .await
        .expect_err("MatrixOne must reject unknown persisted task statuses");
    assert!(
        err.contains("session_todos.status")
            && err.contains("invalid status")
            && err.contains("mystery"),
        "bad MatrixOne status should be surfaced explicitly: {err}"
    );

    let active = mgr
        .load_active_tasks()
        .await
        .expect("active MatrixOne loads should stay fail-closed and skip unknown statuses");
    assert!(
        active.is_empty(),
        "unknown persisted statuses should not leak into active MatrixOne loads: {active:?}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn subtask_depends_on_blocks_out_of_order_start_in_matrixone() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-subtask-deps-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-subtask-deps-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    let create = edge
        .create(&json!({
            "title": "ordered subtasks",
            "subtasks": [
                { "id": "setup", "title": "setup" },
                { "id": "verify", "title": "verify", "depends_on": ["setup"] }
            ]
        }))
        .await;
    assert!(create.contains("\"success\":true"), "{create}");

    let blocked = cloud
        .update(&json!({
            "task_id": "task-1",
            "subtask_id": "verify",
            "new_status": "in_progress"
        }))
        .await;
    assert!(
        blocked.starts_with("Error:")
            && blocked.contains("depends_on")
            && blocked.contains("setup"),
        "MatrixOne subtask should not start before depends_on completes: {blocked}"
    );

    let setup_done = edge
        .update(&json!({
            "task_id": "task-1",
            "subtask_id": "setup",
            "new_status": "completed"
        }))
        .await;
    assert!(!setup_done.starts_with("Error:"), "{setup_done}");
    let started = cloud
        .update(&json!({
            "task_id": "task-1",
            "subtask_id": "verify",
            "new_status": "in_progress"
        }))
        .await;
    assert!(
        started.contains("\"success\":true") && started.contains("\"status\":\"in_progress\""),
        "{started}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn second_in_progress_task_is_rejected_across_matrixone_clients() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-single-running-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-single-running-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let edge_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let cloud_store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let edge = TaskManager::new(session_id.clone(), edge_store);
    let cloud = TaskManager::new(session_id.clone(), cloud_store);

    edge.create(&json!({"title": "first"})).await;
    edge.create(&json!({"title": "second"})).await;
    let first = edge
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(!first.starts_with("Error:"), "{first}");

    let second = cloud
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(
        second.starts_with("Error:")
            && second.contains("already in_progress")
            && second.contains("task-1"),
        "MatrixOne must reject a second in_progress task across clients: {second}"
    );

    let paused = edge
        .update(&json!({"task_id": "task-1", "new_status": "paused"}))
        .await;
    assert!(!paused.starts_with("Error:"), "{paused}");
    let second = cloud
        .update(&json!({"task_id": "task-2", "new_status": "in_progress"}))
        .await;
    assert!(
        second.contains("\"success\":true") && second.contains("\"status\":\"in_progress\""),
        "{second}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn archive_detaches_dependency_edges_through_matrixone_store() {
    let pool = bootstrap_pool().await;
    let session_id = format!("s-archive-edges-{}", uuid::Uuid::new_v4());
    let user_id = format!("u-archive-edges-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_id, &user_id).await;
    let store: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let mgr = TaskManager::new(session_id.clone(), store);

    mgr.create(&json!({"title": "producer"})).await;
    mgr.create(&json!({"title": "consumer"})).await;
    let linked = mgr
        .update(&json!({"task_id": "task-1", "add_blocks": ["task-2"]}))
        .await;
    assert!(!linked.starts_with("Error:"), "{linked}");
    let started = mgr
        .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
        .await;
    assert!(!started.starts_with("Error:"), "{started}");
    let completed = mgr
        .update(&json!({"task_id": "task-1", "new_status": "completed"}))
        .await;
    assert!(!completed.starts_with("Error:"), "{completed}");

    let archived = mgr.archive(&json!({"task_id": "task-1"})).await;
    assert!(!archived.starts_with("Error:"), "{archived}");

    let producer: SessionTask =
        serde_json::from_str(&mgr.get(&json!({"task_id": "task-1"})).await).unwrap();
    assert_eq!(producer.status, SessionTaskStatusKind::Archived);
    assert!(
        producer.blocks.is_empty() && producer.blocked_by.is_empty(),
        "archived MatrixOne task should be detached: {producer:?}"
    );
    let consumer: SessionTask =
        serde_json::from_str(&mgr.get(&json!({"task_id": "task-2"})).await).unwrap();
    assert!(
        consumer.blocked_by.is_empty(),
        "MatrixOne archive should unblock open dependents: {consumer:?}"
    );

    cleanup(&pool, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live infrastructure: run with ASTRA_TEST_DB_IT=1"]
async fn load_all_sessions_is_scoped_to_user_store() {
    let pool = bootstrap_pool().await;
    let session_a = format!("s-load-all-a-{}", uuid::Uuid::new_v4());
    let session_b = format!("s-load-all-b-{}", uuid::Uuid::new_v4());

    let user_a = format!("u-load-all-a-{}", uuid::Uuid::new_v4());
    let user_b = format!("u-load-all-b-{}", uuid::Uuid::new_v4());
    prepare_session(&pool, &session_a, &user_a).await;
    prepare_session(&pool, &session_b, &user_b).await;
    let store_a: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_a).unwrap());
    let store_b: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_b).unwrap());
    let mgr_a = TaskManager::new(session_a.clone(), store_a.clone());
    let mgr_b = TaskManager::new(session_b.clone(), store_b);

    mgr_a
        .create(&json!({"title": "visible only to user a"}))
        .await;
    mgr_b
        .create(&json!({"title": "visible only to user b"}))
        .await;

    let rows = store_a.load_all_sessions().await.expect("load_all");
    let session_ids: Vec<&str> = rows
        .iter()
        .map(|(session_id, _tasks)| session_id.as_str())
        .collect();
    assert!(
        session_ids.contains(&session_a.as_str()),
        "user A should see their own session: {session_ids:?}"
    );
    assert!(
        !session_ids.contains(&session_b.as_str()),
        "user A must not see user B's session via load_all_sessions: {session_ids:?}"
    );

    cleanup(&pool, &session_a).await;
    cleanup(&pool, &session_b).await;
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
    let user_id = format!("u-{}", session_id);
    prepare_session(&pool, &session_id, &user_id).await;

    // Two independent stores sharing the pool — mirrors prod cross-host.
    let store_a: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());
    let store_b: Arc<dyn TaskStore> =
        Arc::new(MatrixOneTaskStore::new_for_user(pool.clone(), &user_id).unwrap());

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
