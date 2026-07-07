//! Live MatrixOne checks for admin user-role queries.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test admin_user_role_manager_db_it -- --ignored
//! ```

use astra_services::{AdminUserRoleManager, DatabaseAdminUserRoleManager};
use uuid::Uuid;

mod common;

async fn cleanup_fixture(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    role_id: &str,
    username: &str,
    role_name: &str,
) {
    let _ = sqlx::query("DELETE FROM auth_user_roles WHERE user_id = ? OR role_id = ?")
        .bind(user_id)
        .bind(role_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth_users WHERE user_id = ? OR username = ?")
        .bind(user_id)
        .bind(username)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth_roles WHERE role_id = ? OR role_name = ?")
        .bind(role_id)
        .bind(role_name)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn has_role_members_tracks_assignments_by_role_name() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let suffix = Uuid::new_v4().to_string();
    let user_id = format!("it-user-{suffix}");
    let username = format!("it-user-{}", &suffix[..12]);
    let email = format!("{username}@example.com");
    let role_id = format!("it-role-{suffix}");
    let role_name = format!("it-role-{}", &suffix[..12]);
    let missing_role_name = format!("missing-{}", &suffix[..12]);

    cleanup_fixture(&pool, &user_id, &role_id, &username, &role_name).await;

    sqlx::query("INSERT INTO auth_roles (role_id, role_name, description) VALUES (?, ?, ?)")
        .bind(&role_id)
        .bind(&role_name)
        .bind("integration test role")
        .execute(&pool)
        .await
        .expect("insert auth_roles fixture");

    sqlx::query(
        "INSERT INTO auth_users \
         (user_id, username, email, password_hash, display_name, is_active) \
         VALUES (?, ?, ?, ?, NULL, 1)",
    )
    .bind(&user_id)
    .bind(&username)
    .bind(&email)
    .bind("not-a-real-password-hash")
    .execute(&pool)
    .await
    .expect("insert auth_users fixture");

    let manager = DatabaseAdminUserRoleManager::new(settings).with_pool(shared);

    assert!(
        !manager.has_role_members(&role_name).await.unwrap(),
        "role exists but has no assigned users"
    );
    assert!(
        !manager.has_role_members(&missing_role_name).await.unwrap(),
        "missing role should not report members"
    );

    sqlx::query("INSERT INTO auth_user_roles (user_id, role_id) VALUES (?, ?)")
        .bind(&user_id)
        .bind(&role_id)
        .execute(&pool)
        .await
        .expect("insert auth_user_roles fixture");

    assert!(
        manager.has_role_members(&role_name).await.unwrap(),
        "assigned role should report members"
    );
    assert!(
        !manager.has_role_members(&missing_role_name).await.unwrap(),
        "other roles should remain isolated"
    );

    cleanup_fixture(&pool, &user_id, &role_id, &username, &role_name).await;
}
