//! Real MatrixOne contracts for canonical event identity and replay.
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires MatrixOne; creates and removes one randomly named isolated test database"]
async fn schema_rejects_missing_capture_columns_in_existing_table() {
    use sqlx::Connection;
    let mut settings = common::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let mut admin_settings = settings.clone();
    admin_settings.database = catalog.clone();
    let mut admin = sqlx::MySqlConnection::connect(&admin_settings.database_url_with_password())
        .await
        .expect("connect bootstrap catalog");
    settings.database = format!("astra_test_probe_capture_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE `{}`",
        settings.database
    )))
    .execute(&mut admin)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE `{}`.agent_events (user_id VARCHAR(128) NOT NULL, event_id VARCHAR(128) NOT NULL, PRIMARY KEY(user_id, event_id))",
        settings.database,
    ))).execute(&mut admin).await.unwrap();
    let bootstrap = astra_services::storage::ensure_core_schema(&settings, &catalog).await;
    // The only destructive target is the unique database this test created.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE `{}`",
        settings.database
    )))
    .execute(&mut admin)
    .await
    .expect("remove isolated schema probe");
    let error = bootstrap.expect_err("missing capture columns must prevent startup");
    assert!(
        error.to_string().contains("payload_hash"),
        "unexpected schema rejection: {error}"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1 and a fresh ASTRA_DATABASE"]
async fn event_api_replays_exact_request_and_rejects_changed_agent_or_lineage() {
    use astra_services::events::{
        DatabaseEventService, EventCreateRequestData, EventIngestionSource, EventService,
    };
    let (pool, settings) = common::setup_pool_and_settings().await;
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status, event_count) VALUES (?, ?, 'active', 0)")
        .bind(&session).bind(&owner).execute(pool.get()).await.unwrap();
    let service = DatabaseEventService::new(settings).with_pool(pool.clone());
    let request = EventCreateRequestData {
        ingestion_source: EventIngestionSource::Client,
        event_id: Some(Uuid::new_v4().to_string()),
        session_id: session.clone(),
        event_type: "capture-test".into(),
        content: "immutable content".into(),
        agent_id: Some("original-agent".into()),
        agent_version: None,
        parent_event_id: None,
        parent_event_ids: None,
        causal_chain_id: None,
        metadata: None,
    };
    let created = service
        .create_event(owner.clone(), request.clone())
        .await
        .unwrap();
    assert!(!created.idempotent_replay);
    let replay = service
        .create_event(owner.clone(), request.clone())
        .await
        .unwrap();
    assert!(replay.idempotent_replay);
    assert_eq!(created.record, replay.record);
    let mut changed_agent = request.clone();
    changed_agent.agent_id = Some("different-agent".into());
    assert_eq!(
        service
            .create_event(owner.clone(), changed_agent)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::CONFLICT
    );
    let mut changed_parent = request.clone();
    changed_parent.parent_event_id = Some("different-parent".into());
    assert_eq!(
        service
            .create_event(owner.clone(), changed_parent)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(count, 1);
    let content: String =
        sqlx::query_scalar("SELECT content FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(&owner)
            .bind(&request.event_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(content, "immutable content");
}
