//! MatrixOne integration: session reaper idle → ended transitions.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test session_reaper_db_integration -- --ignored
//! ```

use astra_services::session_reaper::{SessionReaperPolicy, reap_sessions};
use sqlx::Row;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "live MatrixOne; ASTRA_TEST_DB_IT=1"]
async fn reaper_marks_stale_active_session_idle_then_ended() {
    common::require_db_it_env();
    let pool = common::setup_pool().await.get().clone();

    let session_id = format!("reaper-it-{}", Uuid::new_v4());
    let user_id = format!("reaper-user-{}", Uuid::new_v4());

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, agent_id, status, title, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'astra-cli', 'active', 'reaper test', \
                 DATE_SUB(NOW(6), INTERVAL 3 HOUR), DATE_SUB(NOW(6), INTERVAL 3 HOUR), \
                 DATE_SUB(NOW(6), INTERVAL 3 HOUR))",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert stale active session");

    // Pass 1: mark stale actives as idle (do not end yet).
    let idle_only = SessionReaperPolicy {
        idle_after_secs: 60,
        end_after_idle_secs: 86_400,
        delete_after_ended_days: 365,
        batch_limit: 100,
    };
    let idle_result = reap_sessions(&pool, &idle_only).await;
    assert!(
        idle_result.marked_idle >= 1,
        "expected idle transition, got {idle_result:?}"
    );

    let status_idle: String =
        sqlx::query_scalar("SELECT status FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("status after idle sweep");
    assert_eq!(status_idle, "idle");

    // Pass 2: end sessions idle longer than the threshold.
    let end_policy = SessionReaperPolicy {
        idle_after_secs: 86_400,
        end_after_idle_secs: 60,
        delete_after_ended_days: 365,
        batch_limit: 100,
    };
    let end_result = reap_sessions(&pool, &end_policy).await;
    assert!(
        end_result.marked_ended >= 1,
        "expected ended transition, got {end_result:?}"
    );

    let row = sqlx::query("SELECT status, ended_at FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("final row");
    let status = row.try_get::<String, _>("status").expect("status");
    assert_eq!(status, "ended", "session should end after long idle");
    let ended_at_set: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND ended_at IS NOT NULL",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("ended_at count");
    assert_eq!(ended_at_set, Some(1), "ended_at should be set");

    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .execute(&pool)
        .await;
}
