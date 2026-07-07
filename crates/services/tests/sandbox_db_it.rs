mod common;

use astra_services::{DatabaseSandboxService, SandboxCreateRequestData, SandboxService};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_sandbox_invalid_status_fails_loud() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseSandboxService::new(settings).with_pool(shared_pool);
    let user_id = Uuid::new_v4().to_string();
    let sandbox_name = format!("sb_{}", Uuid::new_v4().simple());

    let created = service
        .create_sandbox(
            user_id.clone(),
            SandboxCreateRequestData {
                name: sandbox_name.clone(),
                description: "sandbox db unhappy path".to_string(),
            },
        )
        .await
        .expect("create sandbox");

    assert_eq!(created.sandbox_name, sandbox_name);
    assert_eq!(created.status, "active");
    assert_eq!(created.user_id, user_id);

    sqlx::query("UPDATE infra_sandbox_metadata SET status = ? WHERE sandbox_name = ?")
        .bind("paused")
        .bind(&sandbox_name)
        .execute(&pool)
        .await
        .expect("corrupt sandbox status");

    let err = service
        .get_sandbox(sandbox_name.clone(), user_id.clone())
        .await
        .expect_err("invalid persisted status must fail loudly");

    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("infra_sandbox_metadata.status"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM infra_sandbox_metadata WHERE sandbox_name = ?")
        .bind(&sandbox_name)
        .execute(&pool)
        .await;
}
