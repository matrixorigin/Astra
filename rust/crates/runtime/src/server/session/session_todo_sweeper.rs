//! Background lifecycle sweepers for `session_todos`.
//!
//! Two cron tasks ship together because they share infrastructure:
//!
//! 1. **Stale in_progress** (U-16): every 5 minutes scan rows where
//!    `status='in_progress'` AND `updated_at < now - STALE_THRESHOLD`.
//!    Transition to `paused` with `metadata.auto_paused_reason` so
//!    the user / model sees why the slot opened up. Without this,
//!    a model that crashed or forgot to mark the task done leaves
//!    a permanent `in_progress` row violating the "exactly one
//!    in_progress at a time" invariant the schema prose enforces.
//!
//! 2. **Completed auto-archive** (U-18): every week move stale
//!    `completed` rows to `archived` after `COMPLETED_AUTO_ARCHIVE_DAYS`.
//!    This keeps the live board focused on actionable work even when the
//!    model forgets to call `task(action='archive')`.
//!
//! 3. **Archived GC** (U-17): every week DELETE rows that have been
//!    `archived` for more than `ARCHIVE_RETENTION_DAYS`. Keeps the
//!    table size bounded for long-lived users. Conservative default of
//!    90 days — adjust via `ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS`
//!    when needed.
//!
//! Both sweepers log a one-line summary per run (rows affected) so
//! operators can spot anomalies (millions of in_progress → maybe
//! something stuck the executor; zero archived deletes for weeks →
//! maybe the archive action isn't being called).

use astra_core::SharedPool;
use sqlx::Row;

const STALE_SWEEP_INTERVAL_SECS: u64 = 300; // 5 min
const STALE_THRESHOLD_HOURS: u64 = 24;
const STALE_BATCH_LIMIT: i64 = 500;

const ARCHIVE_SWEEP_INTERVAL_SECS: u64 = 7 * 24 * 3600; // 1 week
const COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT: i64 = 7;
const ARCHIVE_RETENTION_DAYS_DEFAULT: i64 = 90;

fn completed_auto_archive_days() -> i64 {
    std::env::var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|days: &i64| *days > 0)
        .unwrap_or(COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT)
}

fn archive_retention_days() -> i64 {
    std::env::var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|days: &i64| *days > 0)
        .unwrap_or(ARCHIVE_RETENTION_DAYS_DEFAULT)
}

/// Run a single stale-in_progress sweep. Returns the number of rows
/// transitioned. Idempotent: running twice in quick succession on
/// the same row only flips it once because the WHERE clause checks
/// status AND age.
///
/// Public for runtime tests; production callers go through the
/// spawned interval task.
pub(crate) async fn run_stale_in_progress_sweep(pool: SharedPool) -> Result<u64, String> {
    let mut tx = pool
        .get()
        .begin()
        .await
        .map_err(|e| format!("stale-sweep tx begin: {e}"))?;

    // Pull the candidate rows so we can write per-row metadata
    // (auto_paused_reason). A bulk UPDATE without metadata would be
    // faster but the audit context matters more here than the SQL
    // round-trips — sweeps are 5min apart and N is small.
    let rows: Vec<(String, String)> = sqlx::query(
        "SELECT session_id, todo_id FROM session_todos \
         WHERE status = 'in_progress' \
           AND updated_at < DATE_SUB(NOW(6), INTERVAL ? HOUR) \
         LIMIT ?",
    )
    .bind(STALE_THRESHOLD_HOURS as i64)
    .bind(STALE_BATCH_LIMIT)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| format!("stale-sweep query: {e}"))?
    .into_iter()
    .filter_map(|r| {
        let sid: Option<String> = r.try_get("session_id").ok();
        let tid: Option<String> = r.try_get("todo_id").ok();
        match (sid, tid) {
            (Some(sid), Some(tid)) => Some((sid, tid)),
            _ => None,
        }
    })
    .collect();

    let mut affected = 0u64;
    for (session_id, todo_id) in &rows {
        // CAS on status: re-check inside the UPDATE so a row that
        // raced to `completed` between our SELECT and now isn't
        // clobbered back to `paused`. status must STILL be
        // in_progress at apply time.
        let result = sqlx::query(
            "UPDATE session_todos \
             SET status = 'paused', \
                 metadata = JSON_SET(\
                     COALESCE(metadata, JSON_OBJECT()), \
                     '$.auto_paused_reason', 'stale_in_progress > 24h', \
                     '$.auto_paused_at', DATE_FORMAT(NOW(6), '%Y-%m-%dT%H:%i:%s.%fZ')\
                 ), \
                 updated_at = NOW(6) \
             WHERE session_id = ? AND todo_id = ? AND status = 'in_progress' \
               AND updated_at < DATE_SUB(NOW(6), INTERVAL ? HOUR)",
        )
        .bind(session_id)
        .bind(todo_id)
        .bind(STALE_THRESHOLD_HOURS as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("stale-sweep update: {e}"))?;
        affected = affected.saturating_add(result.rows_affected());
    }

    tx.commit()
        .await
        .map_err(|e| format!("stale-sweep commit: {e}"))?;
    Ok(affected)
}

/// Run a single completed→archived pass. Returns the number of rows
/// transitioned. Weekly cadence is enough because this is purely a
/// hygiene sweep for stale history, not a user-facing real-time state
/// change.
pub(crate) async fn run_completed_auto_archive_once(pool: SharedPool) -> Result<u64, String> {
    let days = completed_auto_archive_days();
    let result = sqlx::query(
        "UPDATE session_todos \
         SET status = 'archived', archived_at = NOW(6), updated_at = NOW(6) \
         WHERE status = 'completed' \
           AND updated_at < DATE_SUB(NOW(6), INTERVAL ? DAY)",
    )
    .bind(days)
    .execute(pool.get())
    .await
    .map_err(|e| format!("completed-auto-archive: {e}"))?;
    Ok(result.rows_affected())
}

/// Run a single archived-GC pass. Hard-deletes rows that have been
/// `archived` for longer than the retention window. Returns the
/// affected row count.
pub(crate) async fn run_archive_gc_once(pool: SharedPool) -> Result<u64, String> {
    let days = archive_retention_days();
    let result = sqlx::query(
        "DELETE FROM session_todos \
         WHERE status = 'archived' \
           AND archived_at IS NOT NULL \
           AND archived_at < DATE_SUB(NOW(6), INTERVAL ? DAY)",
    )
    .bind(days)
    .execute(pool.get())
    .await
    .map_err(|e| format!("archive-gc: {e}"))?;
    Ok(result.rows_affected())
}

/// Spawn the stale-in_progress sweeper. Tick every 5 minutes;
/// missed ticks are coalesced (`Delay`) so a paused server doesn't
/// cause a ticker burst on resume.
pub(crate) fn spawn_session_todo_stale_sweeper(pool: SharedPool) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(STALE_SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick — give the server a chance
        // to finish startup before the first sweep.
        interval.tick().await;
        loop {
            interval.tick().await;
            match run_stale_in_progress_sweep(pool.clone()).await {
                Ok(0) => {} // quiet success: nothing to do this tick
                Ok(n) => tracing::info!(
                    target: "astra_runtime::session_todo_sweeper",
                    rows = n,
                    "auto-paused {n} stale in_progress task(s)"
                ),
                Err(e) => tracing::warn!(
                    target: "astra_runtime::session_todo_sweeper",
                    error = %e,
                    "stale-in_progress sweep failed"
                ),
            }
        }
    });
}

/// Spawn the weekly archive hygiene sweepers. First pass auto-archives
/// stale `completed` rows; second pass hard-deletes long-retained
/// `archived` rows. Fires on the first tick so a freshly-started server
/// still performs backlog cleanup immediately.
pub(crate) fn spawn_session_todo_archive_sweeper(pool: SharedPool) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(ARCHIVE_SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let auto_archive_days = completed_auto_archive_days();
            match run_completed_auto_archive_once(pool.clone()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    target: "astra_runtime::session_todo_sweeper",
                    rows = n,
                    older_than_days = auto_archive_days,
                    "auto-archived {n} completed task(s) older than {auto_archive_days} days"
                ),
                Err(e) => tracing::warn!(
                    target: "astra_runtime::session_todo_sweeper",
                    error = %e,
                    "completed-auto-archive sweep failed"
                ),
            }
            match run_archive_gc_once(pool.clone()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    target: "astra_runtime::session_todo_sweeper",
                    rows = n,
                    "garbage-collected {n} archived task row(s)"
                ),
                Err(e) => tracing::warn!(
                    target: "astra_runtime::session_todo_sweeper",
                    error = %e,
                    "archive-gc failed"
                ),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the constants to the documented values so a future
    /// "let's tighten the threshold" PR can't silently halve the
    /// 24h grace period that prose-driven LLM behaviour relies on.
    #[test]
    fn stale_threshold_is_24h() {
        assert_eq!(STALE_THRESHOLD_HOURS, 24);
    }

    #[test]
    fn stale_sweep_interval_is_5min() {
        assert_eq!(STALE_SWEEP_INTERVAL_SECS, 300);
    }

    #[test]
    #[serial_test::serial(env_auto_archive_days)]
    fn completed_auto_archive_default_is_7_days() {
        unsafe {
            std::env::remove_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS");
        }
        assert_eq!(
            completed_auto_archive_days(),
            COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT
        );
        assert_eq!(COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT, 7);
    }

    #[test]
    #[serial_test::serial(env_auto_archive_days)]
    fn completed_auto_archive_env_override() {
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS", "14");
        }
        assert_eq!(completed_auto_archive_days(), 14);
        unsafe {
            std::env::remove_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS");
        }
    }

    #[test]
    #[serial_test::serial(env_auto_archive_days)]
    fn completed_auto_archive_env_invalid_falls_back_to_default() {
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS", "0");
        }
        assert_eq!(
            completed_auto_archive_days(),
            COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT
        );
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS", "-3");
        }
        assert_eq!(
            completed_auto_archive_days(),
            COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT
        );
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS", "bad");
        }
        assert_eq!(
            completed_auto_archive_days(),
            COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT
        );
        unsafe {
            std::env::remove_var("ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS");
        }
    }

    #[test]
    #[serial_test::serial(env_archive_retention)]
    fn archive_retention_default_is_90_days() {
        // Without env override, the default kicks in.
        unsafe {
            std::env::remove_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS");
        }
        assert_eq!(archive_retention_days(), ARCHIVE_RETENTION_DAYS_DEFAULT);
        assert_eq!(ARCHIVE_RETENTION_DAYS_DEFAULT, 90);
    }

    #[test]
    #[serial_test::serial(env_archive_retention)]
    fn archive_retention_env_override() {
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS", "30");
        }
        assert_eq!(archive_retention_days(), 30);
        unsafe {
            std::env::remove_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS");
        }
    }

    /// Negative / zero retention overrides are ignored — defends
    /// against an operator typo from accidentally deleting all
    /// archived rows.
    #[test]
    #[serial_test::serial(env_archive_retention)]
    fn archive_retention_env_invalid_falls_back_to_default() {
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS", "0");
        }
        assert_eq!(archive_retention_days(), ARCHIVE_RETENTION_DAYS_DEFAULT);
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS", "-5");
        }
        assert_eq!(archive_retention_days(), ARCHIVE_RETENTION_DAYS_DEFAULT);
        unsafe {
            std::env::set_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS", "not-a-number");
        }
        assert_eq!(archive_retention_days(), ARCHIVE_RETENTION_DAYS_DEFAULT);
        unsafe {
            std::env::remove_var("ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn completed_auto_archive_only_moves_old_completed_rows() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");

        let session_id = format!("todo-auto-archive-{}", uuid::Uuid::new_v4());
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        let cleanup = || async {
            let _ = sqlx::query("DELETE FROM session_todos WHERE session_id = ?")
                .bind(&session_id)
                .execute(&pool)
                .await;
        };
        cleanup().await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'old done', 'completed', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 8 DAY)), \
                 (?, 'task-2', ?, 1, 'recent done', 'completed', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 2 DAY)), \
                 (?, 'task-3', ?, 2, 'old in progress', 'in_progress', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 8 DAY))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed session_todos");

        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");
        let archived = run_completed_auto_archive_once(shared)
            .await
            .expect("auto archive");
        assert_eq!(archived, 1);

        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT status, CAST(archived_at AS CHAR) AS archived_at \
             FROM session_todos WHERE session_id = ? ORDER BY ordinal ASC",
        )
        .bind(&session_id)
        .fetch_all(&pool)
        .await
        .expect("load session_todos");
        assert_eq!(rows[0].0, "archived");
        assert!(rows[0].1.is_some(), "{rows:?}");
        assert_eq!(rows[1].0, "completed");
        assert!(rows[1].1.is_none(), "{rows:?}");
        assert_eq!(rows[2].0, "in_progress");
        assert!(rows[2].1.is_none(), "{rows:?}");

        cleanup().await;
    }
}
