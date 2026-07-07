mod common;

use astra_services::{DataVersioningService, DatabaseDataVersioningService};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_data_versioning_rejects_corrupt_required_fields() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseDataVersioningService::new(settings).with_pool(shared_pool);
    let user_id = Uuid::new_v4().to_string();
    let checkpoint_id = Uuid::new_v4().to_string();
    let checkpoint_name = format!("checkpoint_{}", Uuid::new_v4().simple());
    let event_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO data_versioning_checkpoints \
         (checkpoint_id, checkpoint_name, user_id, description, created_at) \
         VALUES (?, ?, ?, 'integration checkpoint', '2026-01-01 00:00:01.000000')",
    )
    .bind(&checkpoint_id)
    .bind(&checkpoint_name)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert data versioning checkpoint");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, created_at) \
         VALUES (?, ?, ?, 'assistant_message', 'hello', '2026-01-01 00:00:00.000000')",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert agent event");

    let checkpoints = service
        .list_checkpoints(user_id.clone())
        .await
        .expect("list valid checkpoints");
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_name, checkpoint_name);

    let events = service
        .get_events_at_checkpoint(user_id.clone(), checkpoint_name.clone())
        .await
        .expect("list valid checkpoint events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, event_id);

    sqlx::query("UPDATE agent_events SET event_type = '' WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await
        .expect("corrupt event_type");

    let err = service
        .get_events_at_checkpoint(user_id.clone(), checkpoint_name.clone())
        .await
        .expect_err("empty persisted event_type must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("agent_events.event_type"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query(
        "UPDATE data_versioning_checkpoints SET checkpoint_name = '' WHERE checkpoint_id = ?",
    )
    .bind(&checkpoint_id)
    .execute(&pool)
    .await
    .expect("corrupt checkpoint_name");

    let err = service
        .list_checkpoints(user_id.clone())
        .await
        .expect_err("empty persisted checkpoint_name must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1
            .detail
            .contains("data_versioning_checkpoints.checkpoint_name"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM data_versioning_checkpoints WHERE checkpoint_id = ?")
        .bind(&checkpoint_id)
        .execute(&pool)
        .await;
}
