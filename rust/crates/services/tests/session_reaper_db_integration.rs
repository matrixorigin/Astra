//! MatrixOne integration: session reaper idle → ended transitions.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test session_reaper_db_integration -- --ignored
//! ```

use astra_services::session_reaper::{SessionReaperPolicy, reap_sessions};
use sqlx::Pool;
use sqlx::Row;
use sqlx::mysql::MySql;
use uuid::Uuid;

mod common;

async fn session_status(pool: &Pool<MySql>, session_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .expect("session status")
}

/// Shared CI DB may contain many stale rows; `reap_sessions` uses `LIMIT batch_limit`
/// without `ORDER BY`, so loop until *this* session reaches the expected status.
async fn reap_until(
    pool: &Pool<MySql>,
    policy: &SessionReaperPolicy,
    session_id: &str,
    want: &str,
    max_rounds: u32,
) {
    for _ in 0..max_rounds {
        if session_status(pool, session_id).await == want {
            return;
        }
        let result = reap_sessions(pool, policy).await;
        if result.marked_idle + result.marked_ended + result.deleted == 0 {
            break;
        }
    }
    assert_eq!(
        session_status(pool, session_id).await,
        want,
        "session {session_id} did not reach '{want}' within {max_rounds} reap rounds"
    );
}

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

    let stale_secs: i64 = sqlx::query_scalar(
        "SELECT TIMESTAMPDIFF(SECOND, last_active_at, NOW(6)) FROM agent_sessions WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("stale seconds");
    assert!(
        stale_secs >= 7200,
        "backdate must leave session at least 2h stale, got {stale_secs}s"
    );
    assert_eq!(
        session_status(&pool, &session_id).await,
        "active",
        "seed session must start active"
    );

    // Pass 1: mark stale actives as idle (do not end yet).
    let idle_only = SessionReaperPolicy {
        idle_after_secs: 60,
        end_after_idle_secs: 86_400,
        delete_after_ended_days: 365,
        batch_limit: 500,
    };
    reap_until(&pool, &idle_only, &session_id, "idle", 50).await;

    // Pass 2: end sessions idle longer than the threshold.
    let end_policy = SessionReaperPolicy {
        idle_after_secs: 86_400,
        end_after_idle_secs: 60,
        delete_after_ended_days: 365,
        batch_limit: 500,
    };
    reap_until(&pool, &end_policy, &session_id, "ended", 50).await;

    let row = sqlx::query("SELECT status, ended_at FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("final row");
    let status = row.try_get::<String, _>("status").expect("status");
    assert_eq!(status, "ended");
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
