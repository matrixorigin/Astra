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
