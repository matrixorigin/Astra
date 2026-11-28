mod common;

use astra_services::{
    AdminUserRoleManager, AdminUserRoleRequestData, DatabaseAdminUserRoleManager,
};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_admin_paths_reject_corrupt_required_fields() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let role_manager =
        DatabaseAdminUserRoleManager::new(settings.clone()).with_pool(shared_pool.clone());

    let username = format!("admin_corrupt_{}", Uuid::new_v4().simple());
    let email = format!("{username}@example.test");

    let _ = sqlx::query("DELETE FROM auth_users WHERE user_id = ''")
        .execute(&pool)
        .await;

    sqlx::query(
        "INSERT INTO auth_users (user_id, username, email, password_hash) \
         VALUES ('', ?, ?, 'unused')",
    )
    .bind(&username)
    .bind(&email)
    .execute(&pool)
    .await
    .expect("insert corrupt auth user");

    let err = role_manager
        .grant_role(AdminUserRoleRequestData {
            username: username.clone(),
            role_name: "astra_admin".to_owned(),
        })
        .await
        .expect_err("empty persisted auth_users.user_id must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("auth_users.user_id"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM auth_users WHERE username = ?")
        .bind(&username)
        .execute(&pool)
        .await;
}
