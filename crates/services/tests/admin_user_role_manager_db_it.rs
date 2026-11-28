//! Live MatrixOne checks for admin user-role queries.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test admin_user_role_manager_db_it -- --ignored
//! ```

use astra_services::{AdminUserRoleManager, DatabaseAdminUserRoleManager};
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn fixed_roles_are_independent_and_idempotent() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = format!("role-user-{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO auth_users (user_id, username, email, password_hash) VALUES (?, ?, ?, 'unused')",
    )
    .bind(&user_id)
    .bind(&user_id)
    .bind(format!("{user_id}@example.test"))
    .execute(&pool)
    .await
    .unwrap();
    let manager = DatabaseAdminUserRoleManager::new(settings).with_pool(shared);
    let role = |role_name: &str| astra_services::AdminUserRoleRequestData {
        username: user_id.clone(),
        role_name: role_name.to_string(),
    };
    assert!(!manager.has_role_members("missing-role").await.unwrap());
    assert!(manager.grant_role(role("missing-role")).await.is_err());
    assert_eq!(
        manager
            .grant_role(role("astra_user"))
            .await
            .unwrap()
            .message,
        "Role granted successfully"
    );
    let (left, right) = tokio::join!(
        manager.grant_role(role("astra_admin")),
        manager.grant_role(role("astra_admin"))
    );
    let mut messages = [left.unwrap().message, right.unwrap().message];
    messages.sort();
    assert_eq!(
        messages,
        ["Role granted successfully", "User already has this role"]
    );
    assert!(manager.has_role_members("astra_admin").await.unwrap());
    assert_eq!(
        manager
            .revoke_role(role("astra_admin"))
            .await
            .unwrap()
            .message,
        "Role revoked successfully"
    );
    assert_eq!(
        manager
            .revoke_role(role("astra_admin"))
            .await
            .unwrap()
            .message,
        "User does not have this role"
    );
    let roles: (i16, i16) =
        sqlx::query_as("SELECT is_admin, has_user_role FROM auth_users WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(roles, (0, 1));
    sqlx::query("DELETE FROM auth_users WHERE user_id = ?")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
}
