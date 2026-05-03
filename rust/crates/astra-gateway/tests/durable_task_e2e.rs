//! E2E tests for MysqlDurableTaskStore.
//!
//! Requires: MatrixOne/MySQL at 127.0.0.1:6001
//! Run: cargo test -p astra-gateway --test durable_task_e2e -- --ignored

use astra_core::durable_task_store::*;
use astra_gateway::durable_task_store::MysqlDurableTaskStore;

async fn setup() -> MysqlDurableTaskStore {
    let url = "mysql://root:111@127.0.0.1:6001/astra_gateway";
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("DB connection failed — is MatrixOne running?");
    astra_gateway::storage::ensure_schema(&pool)
        .await
        .expect("schema setup failed");
    MysqlDurableTaskStore::new(pool)
}

fn test_spec(name: &str) -> TaskSpec {
    TaskSpec {
        name: name.to_string(),
        description: Some("e2e test".into()),
        owner_id: format!("test:{}", uuid::Uuid::new_v4()),
        initial_state: None,
    }
}

#[tokio::test]
#[ignore]
async fn full_lifecycle() {
    let store = setup().await;
    let spec = test_spec("lifecycle test");
    let owner = spec.owner_id.clone();

    // Create
    let id = store.create(&spec).await.unwrap();
    assert!(!id.0.is_empty());

    // Get
    let task = store.get(&id).await.unwrap().unwrap();
    assert_eq!(task.name, "lifecycle test");
    assert_eq!(task.status, DurableTaskStatus::Created);
    assert_eq!(task.progress_pct, 0);

    // Checkpoint
    let state = serde_json::json!({"completed": ["alice", "bob"], "pending": 18});
    store.checkpoint(&id, &state, Some(25), Some("2/20 users done")).await.unwrap();

    let task = store.get(&id).await.unwrap().unwrap();
    assert_eq!(task.status, DurableTaskStatus::Running);
    assert_eq!(task.progress_pct, 25);
    assert_eq!(task.step_description.as_deref(), Some("2/20 users done"));
    assert_eq!(task.checkpoint.unwrap()["pending"], 18);

    // Resume (get checkpoint back)
    let cp = store.resume(&id).await.unwrap().unwrap();
    assert_eq!(cp["completed"][0], "alice");

    // Complete
    store.update_status(&id, DurableTaskStatus::Completed, None).await.unwrap();
    let task = store.get(&id).await.unwrap().unwrap();
    assert_eq!(task.status, DurableTaskStatus::Completed);

    // List by owner
    let tasks = store.list(TaskFilter { owner_id: Some(owner), ..Default::default() }).await.unwrap();
    assert!(tasks.iter().any(|t| t.id == id));

    // Delete
    assert!(store.delete(&id).await.unwrap());
    assert!(store.get(&id).await.unwrap().is_none());
}

#[tokio::test]
#[ignore]
async fn checkpoint_non_existent() {
    let store = setup().await;
    let fake = TaskId("nonexistent-id".into());
    let state = serde_json::json!({"x": 1});
    let result = store.checkpoint(&fake, &state, None, None).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}

#[tokio::test]
#[ignore]
async fn resume_non_existent() {
    let store = setup().await;
    let fake = TaskId("nonexistent-id".into());
    let result = store.resume(&fake).await;
    assert!(result.is_err());
}

#[tokio::test]
#[ignore]
async fn resume_no_checkpoint() {
    let store = setup().await;
    let spec = test_spec("no checkpoint");
    let id = store.create(&spec).await.unwrap();
    let cp = store.resume(&id).await.unwrap();
    assert!(cp.is_none());
    store.delete(&id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn delete_non_existent() {
    let store = setup().await;
    let fake = TaskId("nonexistent-id".into());
    assert!(!store.delete(&fake).await.unwrap());
}

#[tokio::test]
#[ignore]
async fn get_non_existent() {
    let store = setup().await;
    let fake = TaskId("nonexistent-id".into());
    assert!(store.get(&fake).await.unwrap().is_none());
}

#[tokio::test]
#[ignore]
async fn create_empty_name_rejected() {
    let store = setup().await;
    let spec = TaskSpec {
        name: "  ".into(),
        description: None,
        owner_id: "test".into(),
        initial_state: None,
    };
    assert!(store.create(&spec).await.is_err());
}

#[tokio::test]
#[ignore]
async fn list_filters_by_status() {
    let store = setup().await;
    let owner = format!("test:{}", uuid::Uuid::new_v4());
    let spec = TaskSpec {
        name: "filter test".into(),
        description: None,
        owner_id: owner.clone(),
        initial_state: None,
    };

    let id1 = store.create(&spec).await.unwrap();
    let id2 = store.create(&spec).await.unwrap();
    store.update_status(&id1, DurableTaskStatus::Completed, None).await.unwrap();

    let active = store.list(TaskFilter {
        owner_id: Some(owner.clone()),
        status: Some(DurableTaskStatus::Created),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, id2);

    let completed = store.list(TaskFilter {
        owner_id: Some(owner),
        status: Some(DurableTaskStatus::Completed),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].id, id1);

    store.delete(&id1).await.unwrap();
    store.delete(&id2).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn checkpoint_terminal_task_rejected() {
    let store = setup().await;
    let spec = test_spec("terminal checkpoint");
    let id = store.create(&spec).await.unwrap();
    store.update_status(&id, DurableTaskStatus::Completed, None).await.unwrap();

    let state = serde_json::json!({"x": 1});
    let result = store.checkpoint(&id, &state, None, None).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("terminal"));

    store.delete(&id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn create_with_initial_state() {
    let store = setup().await;
    let spec = TaskSpec {
        name: "with state".into(),
        description: None,
        owner_id: format!("test:{}", uuid::Uuid::new_v4()),
        initial_state: Some(serde_json::json!({"repos": ["a", "b"]})),
    };
    let id = store.create(&spec).await.unwrap();
    let cp = store.resume(&id).await.unwrap().unwrap();
    assert_eq!(cp["repos"][0], "a");
    store.delete(&id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn sweep_stale_running_tasks() {
    let store = setup().await;
    let owner = format!("test:{}", uuid::Uuid::new_v4());

    // Create two tasks and checkpoint them to running
    let s = |n: &str| TaskSpec { name: n.into(), description: None, owner_id: owner.clone(), initial_state: None };
    let id1 = store.create(&s("stale-1")).await.unwrap();
    let id2 = store.create(&s("stale-2")).await.unwrap();
    store.checkpoint(&id1, &serde_json::json!({"step":1}), Some(10), None).await.unwrap();
    store.checkpoint(&id2, &serde_json::json!({"step":2}), Some(50), None).await.unwrap();

    // Both should be running
    assert_eq!(store.get(&id1).await.unwrap().unwrap().status, DurableTaskStatus::Running);
    assert_eq!(store.get(&id2).await.unwrap().unwrap().status, DurableTaskStatus::Running);

    // Sweep
    let count = store.suspend_stale_running_tasks("gateway restarted").await.unwrap();
    assert!(count >= 2, "should suspend at least 2 tasks, got {count}");

    // Both should be suspended with reason
    let t1 = store.get(&id1).await.unwrap().unwrap();
    assert_eq!(t1.status, DurableTaskStatus::Suspended);
    assert_eq!(t1.error_message.as_deref(), Some("gateway restarted"));
    let t2 = store.get(&id2).await.unwrap().unwrap();
    assert_eq!(t2.status, DurableTaskStatus::Suspended);

    // Checkpoint should be preserved
    assert!(t1.checkpoint.is_some());
    assert!(t2.checkpoint.is_some());

    store.delete(&id1).await.unwrap();
    store.delete(&id2).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn suspend_running_tasks_for_owner() {
    let store = setup().await;
    let owner_a = format!("test:{}", uuid::Uuid::new_v4());
    let owner_b = format!("test:{}", uuid::Uuid::new_v4());

    let spec_a = TaskSpec { name: "a-task".into(), description: None, owner_id: owner_a.clone(), initial_state: None };
    let spec_b = TaskSpec { name: "b-task".into(), description: None, owner_id: owner_b.clone(), initial_state: None };
    let id_a = store.create(&spec_a).await.unwrap();
    let id_b = store.create(&spec_b).await.unwrap();
    store.checkpoint(&id_a, &serde_json::json!({}), None, None).await.unwrap();
    store.checkpoint(&id_b, &serde_json::json!({}), None, None).await.unwrap();

    // Suspend only owner_a's tasks
    let count = store.suspend_running_tasks_for_owner(&owner_a, "CLI crashed").await.unwrap();
    assert_eq!(count, 1);

    // owner_a suspended, owner_b still running
    assert_eq!(store.get(&id_a).await.unwrap().unwrap().status, DurableTaskStatus::Suspended);
    assert_eq!(store.get(&id_b).await.unwrap().unwrap().status, DurableTaskStatus::Running);

    store.delete(&id_a).await.unwrap();
    store.delete(&id_b).await.unwrap();
}

// ─── Context token persistence tests ──────────────────────────────────

#[tokio::test]
#[ignore]
async fn context_token_persist_and_restore() {
    let url = "mysql://root:111@127.0.0.1:6001/astra_gateway";
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .unwrap();
    astra_gateway::storage::ensure_schema(&pool).await.unwrap();

    // Save context tokens
    let tokens = serde_json::json!({
        "user_a": "token_aaa",
        "user_b": "token_bbb",
    });
    astra_gateway::storage::save_credential(
        &pool, "weixin", "default", "context_tokens", &tokens, None,
    ).await.unwrap();

    // Restore
    let cred = astra_gateway::storage::get_credential(
        &pool, "weixin", "default", "context_tokens",
    ).await.unwrap().unwrap();

    assert_eq!(cred.credentials["user_a"], "token_aaa");
    assert_eq!(cred.credentials["user_b"], "token_bbb");

    // Update (overwrite)
    let tokens2 = serde_json::json!({
        "user_a": "token_aaa_updated",
        "user_c": "token_ccc",
    });
    astra_gateway::storage::save_credential(
        &pool, "weixin", "default", "context_tokens", &tokens2, None,
    ).await.unwrap();

    let cred2 = astra_gateway::storage::get_credential(
        &pool, "weixin", "default", "context_tokens",
    ).await.unwrap().unwrap();
    assert_eq!(cred2.credentials["user_a"], "token_aaa_updated");
    assert_eq!(cred2.credentials["user_c"], "token_ccc");
    // user_b gone (full replacement)
    assert!(cred2.credentials.get("user_b").is_none());
}
