mod common;

use astra_services::{DatabaseMarketplaceService, MarketplaceService};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_marketplace_installed_list_rejects_corrupt_required_fields() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseMarketplaceService::new(settings).with_pool(shared_pool);
    let user_id = Uuid::new_v4().to_string();
    let installation_id = Uuid::new_v4().to_string();
    let skill_name = format!("skill-{}", Uuid::new_v4().simple());

    sqlx::query(
        "INSERT INTO skill_installations \
         (installation_id, user_id, skill_name, skill_version, status, installed_at, updated_at) \
         VALUES (?, ?, ?, '1.0.0', 'installed', NOW(6), NOW(6))",
    )
    .bind(&installation_id)
    .bind(&user_id)
    .bind(&skill_name)
    .execute(&pool)
    .await
    .expect("insert skill installation");

    let listed = service
        .list_installed(user_id.clone(), 10, None)
        .await
        .expect("list valid installation");
    assert_eq!(listed.installations.len(), 1);
    assert_eq!(listed.installations[0].installation_id, installation_id);

    sqlx::query("UPDATE skill_installations SET skill_version = '' WHERE installation_id = ?")
        .bind(&installation_id)
        .execute(&pool)
        .await
        .expect("corrupt skill_version");

    let err = service
        .list_installed(user_id.clone(), 10, None)
        .await
        .expect_err("empty persisted skill_version must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("skill_installations.skill_version"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM skill_installations WHERE installation_id = ?")
        .bind(&installation_id)
        .execute(&pool)
        .await;
}
