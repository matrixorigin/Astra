//! Background session reaper for multi-tenant cleanup (Phase 5.4).
//!
//! Periodically scans `agent_sessions` to:
//! 1. Mark sessions idle when `last_active_at` exceeds a threshold.
//! 2. End sessions that have been idle too long.
//! 3. Delete ended sessions (and their workspaces) past a retention window.
//!
//! Runs as a `tokio::spawn` background task alongside the existing `spawn_data_cleanup`.

use std::time::Duration;

use sqlx::Pool;
use sqlx::mysql::MySql;

/// Configuration knobs for the session reaper.
#[derive(Debug, Clone)]
pub struct SessionReaperPolicy {
    /// Seconds of inactivity before marking a session 'idle' (default: 30 min).
    pub idle_after_secs: u64,
    /// Seconds of idleness before ending a session (default: 2 h).
    pub end_after_idle_secs: u64,
    /// Days after `ended_at` before deleting the row (default: 1 day).
    pub delete_after_ended_days: u32,
    /// Maximum rows to mutate per sweep (prevents lock contention).
    pub batch_limit: u32,
}

impl Default for SessionReaperPolicy {
    fn default() -> Self {
        Self {
            idle_after_secs: 30 * 60,      // 30 minutes
            end_after_idle_secs: 2 * 3600, // 2 hours
            delete_after_ended_days: 1,    // 1 day
            batch_limit: 500,
        }
    }
}

/// Summary of a single reaper sweep.
#[derive(Debug, Default)]
pub struct ReaperSweepResult {
    /// Sessions transitioned from active → idle.
    pub marked_idle: u64,
    /// Sessions transitioned from idle → ended.
    pub marked_ended: u64,
    /// Ended session rows deleted.
    pub deleted: u64,
    /// Workspace directories removed.
    pub workspaces_removed: u64,
}

/// Run one sweep of the session reaper.
///
/// This is a pure service function (no state) so it can be tested independently.
pub async fn reap_sessions(pool: &Pool<MySql>, policy: &SessionReaperPolicy) -> ReaperSweepResult {
    let mut result = ReaperSweepResult::default();

    // 1. Mark active sessions as idle when last_active_at exceeds idle threshold.
    let marked_idle = sqlx::query(
        "UPDATE agent_sessions \
         SET status = 'idle', updated_at = NOW(6) \
         WHERE status = 'active' \
           AND last_active_at < DATE_SUB(NOW(6), INTERVAL ? SECOND) \
         LIMIT ?",
    )
    .bind(policy.idle_after_secs as i64)
    .bind(policy.batch_limit)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    result.marked_idle = marked_idle;

    // 2. End sessions that have been idle longer than the end threshold.
    //    We use last_active_at (not updated_at) so that the "end" timer
    //    is measured from the last real user activity, not from when we
    //    flipped the status to idle.
    let marked_ended = sqlx::query(
        "UPDATE agent_sessions \
         SET status = 'ended', ended_at = NOW(6), updated_at = NOW(6) \
         WHERE status = 'idle' \
           AND last_active_at < DATE_SUB(NOW(6), INTERVAL ? SECOND) \
         LIMIT ?",
    )
    .bind(policy.end_after_idle_secs as i64)
    .bind(policy.batch_limit)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    result.marked_ended = marked_ended;

    // 3. Collect session_ids of sessions that have been ended long enough to delete.
    let to_delete: Vec<(String,)> = sqlx::query_as(
        "SELECT session_id FROM agent_sessions \
         WHERE status IN ('ended', 'closed', 'cancelled') \
           AND ended_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.delete_after_ended_days)
    .bind(policy.batch_limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    if !to_delete.is_empty() {
        // Clean up workspace directories.
        let workspace_base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));

        for (sid,) in &to_delete {
            let safe_id: String = sid
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if safe_id.is_empty() {
                continue;
            }
            let ws = workspace_base.join(&safe_id);
            if ws.exists() && std::fs::remove_dir_all(&ws).is_ok() {
                result.workspaces_removed += 1;
            }
        }

        // Build comma-separated placeholders for IN clause.
        let placeholders: Vec<&str> = to_delete.iter().map(|_| "?").collect();
        let in_clause = placeholders.join(",");

        // Delete related events first (FK-safe ordering).
        let query = format!("DELETE FROM agent_events WHERE session_id IN ({in_clause}) LIMIT ?",);
        let mut q = sqlx::query(&query);
        for (sid,) in &to_delete {
            q = q.bind(sid);
        }
        q = q.bind(policy.batch_limit * 10); // events are many-per-session
        let _ = q.execute(pool).await;

        // Delete the session rows.
        let query = format!("DELETE FROM agent_sessions WHERE session_id IN ({in_clause})",);
        let mut q = sqlx::query(&query);
        for (sid,) in &to_delete {
            q = q.bind(sid);
        }
        result.deleted = q
            .execute(pool)
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0);
    }

    result
}

/// Spawn the session reaper as a long-lived background task.
///
/// Call once from `server/mod.rs` during startup, alongside `spawn_data_cleanup`.
/// The returned `JoinHandle` lets the caller drain the task during graceful
/// shutdown by triggering `cancel` and awaiting the handle.
pub fn spawn_session_reaper(
    pool: astra_core::SharedPool,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let reaper_interval = Duration::from_secs(
        std::env::var("MO_REAPER_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5 * 60), // default: 5 minutes
    );

    tokio::spawn(async move {
        let policy = SessionReaperPolicy::default();
        let mut interval = tokio::time::interval(reaper_interval);
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: "astra_services::session_reaper",
                        "session reaper received cancellation; exiting"
                    );
                    break;
                }
                _ = interval.tick() => {}
            }
            let r = reap_sessions(pool.get(), &policy).await;
            let total = r.marked_idle + r.marked_ended + r.deleted;
            if total > 0 {
                tracing::info!(
                    target: "astra_services::session_reaper",
                    marked_idle = r.marked_idle,
                    marked_ended = r.marked_ended,
                    deleted = r.deleted,
                    workspaces_removed = r.workspaces_removed,
                    "session reaper sweep"
                );
            }
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_values() {
        let p = SessionReaperPolicy::default();
        assert_eq!(p.idle_after_secs, 30 * 60);
        assert_eq!(p.end_after_idle_secs, 2 * 3600);
        assert_eq!(p.delete_after_ended_days, 1);
        assert_eq!(p.batch_limit, 500);
    }

    #[test]
    fn sweep_result_default_is_zero() {
        let r = ReaperSweepResult::default();
        assert_eq!(r.marked_idle, 0);
        assert_eq!(r.marked_ended, 0);
        assert_eq!(r.deleted, 0);
        assert_eq!(r.workspaces_removed, 0);
    }
}
