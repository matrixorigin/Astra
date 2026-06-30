mod common;

use astra_services::{
    AdminFeedbackStatsFilter, AdminFeedbackStatsReader, AdminUserRoleManager,
    AdminUserRoleRequestData, DatabaseAdminFeedbackStatsReader, DatabaseAdminUserRoleManager,
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
    let feedback_reader = DatabaseAdminFeedbackStatsReader::new(settings).with_pool(shared_pool);

    let username = format!("admin_corrupt_{}", Uuid::new_v4().simple());
    let email = format!("{username}@example.test");
    let role_id = Uuid::new_v4().to_string();
    let role_name = format!("role_{}", Uuid::new_v4().simple());
    let feedback_id = Uuid::new_v4().to_string();
    let feedback_user_id = Uuid::new_v4().to_string();

    let _ = sqlx::query("DELETE FROM auth_user_roles WHERE user_id = ''")
        .execute(&pool)
        .await;
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
    sqlx::query("INSERT INTO auth_roles (role_id, role_name) VALUES (?, ?)")
        .bind(&role_id)
        .bind(&role_name)
        .execute(&pool)
        .await
        .expect("insert auth role");

    let err = role_manager
        .grant_role(AdminUserRoleRequestData {
            username: username.clone(),
            role_name: role_name.clone(),
        })
        .await
        .expect_err("empty persisted auth_users.user_id must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("auth_users.user_id"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query(
        "INSERT INTO eval_user_feedback \
         (feedback_id, user_id, agent_id, feedback_type, rating) \
         VALUES (?, ?, 'agent-admin-it', '', 5)",
    )
    .bind(&feedback_id)
    .bind(&feedback_user_id)
    .execute(&pool)
    .await
    .expect("insert corrupt feedback row");

    let err = feedback_reader
        .read_feedback_stats(AdminFeedbackStatsFilter {
            agent_id: Some("agent-admin-it".to_string()),
            since: None,
        })
        .await
        .expect_err("empty persisted eval_user_feedback.feedback_type must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("eval_user_feedback.feedback_type"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM eval_user_feedback WHERE feedback_id = ?")
        .bind(&feedback_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth_user_roles WHERE role_id = ?")
        .bind(&role_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth_roles WHERE role_id = ?")
        .bind(&role_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth_users WHERE username = ?")
        .bind(&username)
        .execute(&pool)
        .await;
}
