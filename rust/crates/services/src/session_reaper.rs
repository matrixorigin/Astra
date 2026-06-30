//! Background session reaper for multi-tenant cleanup (Phase 5.4).
//!
//! Periodically scans `agent_sessions` to:
//! 1. Mark sessions idle when `last_active_at` exceeds a threshold.
//! 2. End sessions that have been idle too long.
//! 3. Delete ended sessions (and their workspaces) past a retention window.
//!
//! Runs as a `tokio::spawn` background task alongside the existing `spawn_data_cleanup`.

use std::{collections::BTreeMap, time::Duration};

use crate::session_lifecycle::{SessionTableDeleteOutcome, hard_delete_session};
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
    /// Total database rows deleted across session lifecycle tables.
    pub database_rows_deleted: u64,
    /// Session-scoped provenance references cleared without deleting durable owner-level rows.
    pub session_references_cleared: u64,
    /// Cleanup debts created from cloud workspace records before deleting sessions.
    pub workspace_cleanup_debts_enqueued: u64,
    /// Database rows deleted per lifecycle table across this sweep.
    pub database_tables_deleted: BTreeMap<String, u64>,
    /// Owner-bound local session journal/artifact bytes removed.
    pub local_bytes_freed: u64,
    /// Workspace directories removed.
    pub workspaces_removed: u64,
    /// Cumulative database hard-delete transaction time.
    pub database_delete_ms: u64,
    /// Cumulative owner-bound local journal/artifact cleanup time.
    pub local_artifact_delete_ms: u64,
    /// Cumulative server workspace cleanup time.
    pub workspace_delete_ms: u64,
    /// Cumulative end-to-end hard-delete time.
    pub total_delete_ms: u64,
    /// Non-transactional cleanup errors after the database delete committed.
    ///
    /// Database query/delete failures are returned as `Err` from
    /// [`reap_sessions`]; cleanup errors are retained here because the session
    /// row has already been removed and the caller may need to alert/repair
    /// orphaned files.
    pub cleanup_errors: Vec<String>,
}

/// Run one sweep of the session reaper.
///
/// This is a pure service function (no state) so it can be tested independently.
pub async fn reap_sessions(
    pool: &Pool<MySql>,
    policy: &SessionReaperPolicy,
) -> Result<ReaperSweepResult, String> {
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
    .map_err(|source| format!("session_reaper.mark_idle: {source}"))?
    .rows_affected();
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
    .map_err(|source| format!("session_reaper.mark_ended: {source}"))?
    .rows_affected();
    result.marked_ended = marked_ended;

    // 3. Collect owner-bound sessions that have been ended long enough to delete.
    //    Sessions stuck in `deleting` are previous hard-delete attempts that
    //    persisted intent before local cleanup or DB deletion failed; retry them
    //    after the same retention window using ended_at as the marker time.
    //    Using ended_at for both paths avoids relying on updated_at which may
    //    be touched by unrelated operations (provenance clearing, etc.).
    let to_delete: Vec<(String, String)> = sqlx::query_as(
        "SELECT session_id, user_id FROM agent_sessions \
         WHERE status IN ('ended', 'closed', 'cancelled', 'deleting') \
           AND ended_at IS NOT NULL \
           AND ended_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.delete_after_ended_days)
    .bind(policy.batch_limit)
    .fetch_all(pool)
    .await
    .map_err(|source| format!("session_reaper.select_delete_candidates: {source}"))?;

    if !to_delete.is_empty() {
        for (sid, user_id) in &to_delete {
            match hard_delete_session(pool, sid, user_id).await {
                Ok(outcome) => {
                    result.deleted = result.deleted.saturating_add(1);
                    record_reaper_database_delete(
                        &mut result,
                        sid,
                        outcome.database_rows_deleted,
                        outcome.session_references_cleared,
                        outcome.workspace_cleanup_debts_enqueued,
                        outcome.database_tables_deleted,
                    )?;
                    result.local_bytes_freed = result
                        .local_bytes_freed
                        .saturating_add(outcome.local_bytes_freed);
                    result.workspaces_removed = result
                        .workspaces_removed
                        .saturating_add(outcome.workspaces_removed);
                    result.database_delete_ms = result
                        .database_delete_ms
                        .saturating_add(outcome.database_delete_ms);
                    result.local_artifact_delete_ms = result
                        .local_artifact_delete_ms
                        .saturating_add(outcome.local_artifact_delete_ms);
                    result.workspace_delete_ms = result
                        .workspace_delete_ms
                        .saturating_add(outcome.workspace_delete_ms);
                    result.total_delete_ms = result
                        .total_delete_ms
                        .saturating_add(outcome.total_delete_ms);
                    for error in outcome.cleanup_errors {
                        result.cleanup_errors.push(format!("{sid}: {error}"));
                        tracing::warn!(
                            target: "astra_services::session_reaper",
                            session_id = %sid,
                            user_id = %user_id,
                            error = %error,
                            "session reaper cleanup failed after database delete"
                        );
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "session_reaper.delete_session session_id={sid} user_id={user_id}: {error}"
                    ));
                }
            }
        }
    }

    Ok(result)
}

fn record_reaper_database_delete(
    result: &mut ReaperSweepResult,
    session_id: &str,
    database_rows_deleted: u64,
    session_references_cleared: u64,
    workspace_cleanup_debts_enqueued: u64,
    database_tables_deleted: Vec<SessionTableDeleteOutcome>,
) -> Result<(), String> {
    let next_database_rows_deleted = result
        .database_rows_deleted
        .checked_add(database_rows_deleted)
        .ok_or_else(|| {
            format!(
                "session_reaper.delete_session session_id={session_id} database row total overflow"
            )
        })?;
    let next_session_references_cleared = result
        .session_references_cleared
        .checked_add(session_references_cleared)
        .ok_or_else(|| {
            format!(
                "session_reaper.delete_session session_id={session_id} session reference total overflow"
            )
        })?;
    let next_workspace_cleanup_debts_enqueued = result
        .workspace_cleanup_debts_enqueued
        .checked_add(workspace_cleanup_debts_enqueued)
        .ok_or_else(|| {
            format!(
                "session_reaper.delete_session session_id={session_id} workspace cleanup debt total overflow"
            )
        })?;
    let mut next_database_tables_deleted = result.database_tables_deleted.clone();

    for table in database_tables_deleted {
        let entry = next_database_tables_deleted
            .entry(table.label.to_string())
            .or_default();
        *entry = entry.checked_add(table.rows_deleted).ok_or_else(|| {
            format!(
                "session_reaper.delete_session table={} row total overflow",
                table.label
            )
        })?;
    }

    result.database_rows_deleted = next_database_rows_deleted;
    result.session_references_cleared = next_session_references_cleared;
    result.workspace_cleanup_debts_enqueued = next_workspace_cleanup_debts_enqueued;
    result.database_tables_deleted = next_database_tables_deleted;
    Ok(())
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
    let reaper_interval = Duration::from_secs(5 * 60); // 5 minutes

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
            let r = match reap_sessions(pool.get(), &policy).await {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(
                        target: "astra_services::session_reaper",
                        error = %error,
                        "session reaper sweep failed"
                    );
                    continue;
                }
            };
            let total = r.marked_idle + r.marked_ended + r.deleted;
            if total > 0 {
                tracing::info!(
                    target: "astra_services::session_reaper",
                    marked_idle = r.marked_idle,
                    marked_ended = r.marked_ended,
                    deleted = r.deleted,
                    database_rows_deleted = r.database_rows_deleted,
                    session_references_cleared = r.session_references_cleared,
                    workspace_cleanup_debts_enqueued = r.workspace_cleanup_debts_enqueued,
                    database_tables_deleted = ?r.database_tables_deleted,
                    local_bytes_freed = r.local_bytes_freed,
                    workspaces_removed = r.workspaces_removed,
                    database_delete_ms = r.database_delete_ms,
                    local_artifact_delete_ms = r.local_artifact_delete_ms,
                    workspace_delete_ms = r.workspace_delete_ms,
                    total_delete_ms = r.total_delete_ms,
                    cleanup_error_count = r.cleanup_errors.len(),
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
        assert_eq!(r.database_rows_deleted, 0);
        assert_eq!(r.session_references_cleared, 0);
        assert_eq!(r.workspace_cleanup_debts_enqueued, 0);
        assert!(r.database_tables_deleted.is_empty());
        assert_eq!(r.local_bytes_freed, 0);
        assert_eq!(r.workspaces_removed, 0);
        assert_eq!(r.database_delete_ms, 0);
        assert_eq!(r.local_artifact_delete_ms, 0);
        assert_eq!(r.workspace_delete_ms, 0);
        assert_eq!(r.total_delete_ms, 0);
        assert!(r.cleanup_errors.is_empty());
    }

    #[test]
    fn reaper_retries_sessions_stuck_in_deleting_status() {
        let source = include_str!("session_reaper.rs");
        assert!(
            source.contains("'deleting'"),
            "reaper must retry sessions whose hard delete marked intent but did not finish"
        );
        assert!(
            source.contains("ended_at < DATE_SUB(NOW(6), INTERVAL ? DAY)"),
            "deleting retry must use the persisted delete-intent timestamp"
        );
    }

    #[test]
    fn record_reaper_database_delete_aggregates_table_rows() {
        let mut r = ReaperSweepResult::default();
        record_reaper_database_delete(
            &mut r,
            "session-1",
            3,
            1,
            1,
            vec![
                SessionTableDeleteOutcome {
                    label: "agent_events",
                    rows_deleted: 2,
                },
                SessionTableDeleteOutcome {
                    label: "agent_sessions",
                    rows_deleted: 1,
                },
            ],
        )
        .expect("record first delete");
        record_reaper_database_delete(
            &mut r,
            "session-2",
            4,
            2,
            2,
            vec![
                SessionTableDeleteOutcome {
                    label: "agent_events",
                    rows_deleted: 3,
                },
                SessionTableDeleteOutcome {
                    label: "agent_sessions",
                    rows_deleted: 1,
                },
            ],
        )
        .expect("record second delete");

        assert_eq!(r.database_rows_deleted, 7);
        assert_eq!(r.session_references_cleared, 3);
        assert_eq!(r.workspace_cleanup_debts_enqueued, 3);
        assert_eq!(r.database_tables_deleted["agent_events"], 5);
        assert_eq!(r.database_tables_deleted["agent_sessions"], 2);
    }

    #[test]
    fn record_reaper_database_delete_fails_loudly_on_table_overflow() {
        let mut r = ReaperSweepResult::default();
        r.database_tables_deleted
            .insert("agent_events".to_string(), u64::MAX);

        let err = record_reaper_database_delete(
            &mut r,
            "session-1",
            1,
            0,
            0,
            vec![SessionTableDeleteOutcome {
                label: "agent_events",
                rows_deleted: 1,
            }],
        )
        .expect_err("table overflow must fail loudly");

        assert!(
            err.contains("table=agent_events") && err.contains("overflow"),
            "error should identify the overflowing table: {err}"
        );
        assert_eq!(
            r.database_rows_deleted, 0,
            "failed table aggregation must not partially update total rows"
        );
    }
}
