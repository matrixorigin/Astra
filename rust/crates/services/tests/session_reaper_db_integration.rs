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
         (session_id, user_id, agent_id, status, title, event_count) \
         VALUES (?, ?, 'astra-cli', 'active', 'reaper test', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert active session");

    // Backdate activity in UPDATE (reliable on MatrixOne; DATE_SUB in INSERT VALUES is not).
    // 2 hours ago: stale for idle (60s) but not for end (86_400s) within a single reap sweep.
    sqlx::query(
        "UPDATE agent_sessions \
         SET last_active_at = DATE_SUB(NOW(6), INTERVAL 2 HOUR), \
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 HOUR), \
             created_at = DATE_SUB(NOW(6), INTERVAL 2 HOUR) \
         WHERE session_id = ?",
    )
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("backdate session activity");

    let status_before: String =
        sqlx::query_scalar("SELECT status FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("status before reap");
    assert_eq!(status_before, "active", "seed session must start active");

    // Pass 1: mark stale actives as idle (do not end yet).
    let idle_only = SessionReaperPolicy {
        idle_after_secs: 60,
        end_after_idle_secs: 86_400,
        delete_after_ended_days: 365,
        batch_limit: 100,
    };
    let idle_result = reap_sessions(&pool, &idle_only).await;
    let status_idle: String =
        sqlx::query_scalar("SELECT status FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("status after idle sweep");
    assert_eq!(
        status_idle, "idle",
        "seed session should become idle (reap marked_idle={})",
        idle_result.marked_idle
    );

    // Pass 2: end sessions idle longer than the threshold.
    let end_policy = SessionReaperPolicy {
        idle_after_secs: 86_400,
        end_after_idle_secs: 60,
        delete_after_ended_days: 365,
        batch_limit: 100,
    };
    let end_result = reap_sessions(&pool, &end_policy).await;
    let row = sqlx::query("SELECT status, ended_at FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("final row");
    let status = row.try_get::<String, _>("status").expect("status");
    assert_eq!(
        status, "ended",
        "seed session should end after idle threshold (reap marked_ended={})",
        end_result.marked_ended
    );
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
