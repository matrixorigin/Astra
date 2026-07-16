//! Background lifecycle sweepers for `session_todos`.
//!
//! Two cron tasks ship together because they shared infrastructure:
//!
//! 1. **Stale in_progress** (U-16): every 5 minutes scan rows where
//!    `status='in_progress'` AND `updated_at < now - STALE_THRESHOLD`.
//!    Transition to `paused` with `metadata.auto_paused_reason` so
//!    the user / model sees why the slot opened up. Without this, a
//!    model that crashed or forgot to mark the task done leaves a
//!    permanent `in_progress` row violating the "exactly one in_progress
//!    at a time" invariant the schema prose enforces.
//!
//! 2. **Terminal auto-archive** (U-18): every week move a bounded batch
//!    of stale `completed` / `failed` / `cancelled` rows to `archived` after
//!    `COMPLETED_AUTO_ARCHIVE_DAYS`.
//!    This keeps the live board focused on actionable work even when the
//!    model forgets to call `task_board(action='archive')`.
//!
//! 3. **Archived GC** (U-17): every week DELETE a bounded batch of rows
//!    that have been `archived` for more than `ARCHIVE_RETENTION_DAYS`.
//!    Keeps the table size bounded for long-lived users. Conservative
//!    default of 90 days — adjust via
//!    `ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS` when needed.
//!
//! 4. **Stale idempotency** (NEW): every 5 minutes UPDATE rows where
//!    `output IS NULL` AND `updated_at < now - IDEMPOTENCY_STALE_MINUTES`
//!    to set `output` to an error message. Cleans up orphaned idempotency
//!    rows where task creation succeeded but the idempotency completion
//!    update failed. UPDATE (not DELETE) prevents duplicate task creation
//!    on retry — the client receives an explicit error on replay.
//!
//! Both sweepers log a one-line summary per run (rows affected) so
//! operators can spot anomalies (millions of in_progress → maybe
//! something stuck the executor; zero archived deletes for weeks →
//! maybe the archive action isn't being called).

use astra_core::SharedPool;
use astra_tools::task_mgmt::detach_dependency_edges_for_task_ids;
use futures_util::FutureExt;
use serde_json::{Map, Value};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

const STALE_SWEEP_INTERVAL_SECS: u64 = 300; // 5 min
const STALE_THRESHOLD_HOURS: u64 = 24;
const STALE_BATCH_LIMIT: i64 = 500;
/// Consecutive sweep failures before emitting a heightened alert.
/// At 5-min ticks, 3 failures = 15 min of silent DB degradation.
const STALE_SWEEP_ALERT_THRESHOLD: u32 = 3;

const IDEMPOTENCY_STALE_MINUTES: u64 = 5;
const IDEMPOTENCY_BATCH_LIMIT: i64 = 100;

const ARCHIVE_SWEEP_INTERVAL_SECS: u64 = 7 * 24 * 3600; // 1 week
const COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT: i64 = 7;
const ARCHIVE_RETENTION_DAYS_DEFAULT: i64 = 90;
const ARCHIVE_BATCH_LIMIT: i64 = 500;
const ARCHIVE_GC_BATCH_LIMIT: i64 = 500;

fn positive_i64_env_or_default(key: &'static str, default: i64) -> i64 {
    let raw = match std::env::var(key) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return default,
        Err(error) => {
            tracing::warn!(key, %error, default, "failed to read session todo sweeper env");
            return default;
        }
    };
    match raw.parse::<i64>() {
        Ok(value) if value > 0 => value,
        Ok(value) => {
            tracing::warn!(
                key,
                value,
                default,
                "session todo sweeper env must be a positive integer"
            );
            default
        }
        Err(error) => {
            tracing::warn!(
                key,
                raw,
                %error,
                default,
                "failed to parse session todo sweeper env"
            );
            default
        }
    }
}

fn completed_auto_archive_days() -> i64 {
    positive_i64_env_or_default(
        "ASTRA_SESSION_TODO_AUTO_ARCHIVE_DAYS",
        COMPLETED_AUTO_ARCHIVE_DAYS_DEFAULT,
    )
}

fn archive_retention_days() -> i64 {
    positive_i64_env_or_default(
        "ASTRA_SESSION_TODO_ARCHIVE_RETENTION_DAYS",
        ARCHIVE_RETENTION_DAYS_DEFAULT,
    )
}

fn auto_pause_metadata_json(existing: Option<&str>, paused_at: &str) -> String {
    let parsed = existing.and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let needs_repair_marker = match (&parsed, existing) {
        (Some(Value::Object(_)), _) => false,
        (Some(_), _) => true,
        (None, Some(raw)) => !raw.trim().is_empty(),
        (None, None) => false,
    };
    let metadata_before_auto_pause = if needs_repair_marker {
        Some(match (&parsed, existing) {
            (Some(value), _) => value.clone(),
            (None, Some(raw)) => Value::String(raw.to_string()),
            (None, None) => Value::Null,
        })
    } else {
        None
    };
    let mut metadata = match parsed {
        Some(Value::Object(map)) => map,
        Some(_) | None => Map::new(),
    };
    if needs_repair_marker {
        metadata.insert(
            "metadata_before_auto_pause".to_string(),
            metadata_before_auto_pause.unwrap_or(Value::Null),
        );
        metadata.insert(
            "metadata_repair_reason".to_string(),
            Value::String("invalid_or_non_object_metadata_before_auto_pause".to_string()),
        );
    }
    metadata.insert(
        "auto_paused_reason".to_string(),
        Value::String("stale_in_progress > 24h".to_string()),
    );
    metadata.insert(
        "auto_paused_at".to_string(),
        Value::String(paused_at.to_string()),
    );
    Value::Object(metadata).to_string()
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
    //
    // SELECT … FOR UPDATE serialises with mutator transactions (which
    // also lock all rows for a session via FOR UPDATE). Without this,
    // a mutator DELETE+INSERT that restores an in_progress row from its
    // stale in-memory snapshot can silently overwrite a sweeper pause.
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT user_id, session_id, todo_id, metadata FROM session_todos \
         WHERE status = 'in_progress' \
           AND updated_at < DATE_SUB(NOW(6), INTERVAL ? HOUR) \
         ORDER BY updated_at ASC, user_id ASC, session_id ASC, todo_id ASC \
         LIMIT ? \
         FOR UPDATE",
    )
    .bind(STALE_THRESHOLD_HOURS as i64)
    .bind(STALE_BATCH_LIMIT)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| format!("stale-sweep query/decode: {e}"))?;

    let mut affected = 0u64;
    for (user_id, session_id, todo_id, metadata) in &rows {
        // Defense-in-depth: skip plan-derived tasks even if the SQL
        // JSON filter missed them (e.g. due to MatrixOne JSON function
        // differences). The plan orchestrator owns their lifecycle.
        if let Some(meta) = metadata {
            if meta.contains("\"plan_subtask_id\"") {
                tracing::warn!(
                    target: "astra_runtime::session_todo_sweeper",
                    user_id = %user_id,
                    session_id = %session_id,
                    todo_id = %todo_id,
                    "Stale-sweep: plan-derived task passed SQL filter — \
                     skipping to preserve plan consistency"
                );
                continue;
            }
        }
        // CAS on status: re-check inside the UPDATE so a row that
        // raced to `completed` between our SELECT and now isn't
        // clobbered back to `paused`. status must STILL be
        // in_progress at apply time.
        let paused_at = chrono::Utc::now().to_rfc3339();
        let metadata_json = auto_pause_metadata_json(metadata.as_deref(), &paused_at);
        let result = sqlx::query(
            "UPDATE session_todos \
             SET status = 'paused', \
                 metadata = ?, \
                 updated_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND todo_id = ? AND status = 'in_progress' \
               AND updated_at < DATE_SUB(NOW(6), INTERVAL ? HOUR)",
        )
        .bind(metadata_json)
        .bind(user_id)
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

/// Clean up stale idempotency rows where task creation succeeded but
/// `complete_todo_create_idempotency` never ran (e.g. DB connection dropped
/// mid-response).  Instead of deleting rows (which would allow a retry to
/// create a duplicate task), we set `output` to a clear error message so
/// the client receives an explicit failure on replay and the poisoned key
/// cannot create a duplicate.
///
/// This is defense-in-depth.  The primary fix makes idempotency completion
/// non-blocking in the HTTP handler (see session_todo_handlers.rs); this
/// sweeper handles the rare case where the response never reached the
/// client AND the idempotency row was left behind.
pub(crate) async fn run_stale_idempotency_sweep(pool: SharedPool) -> Result<u64, String> {
    const ERROR_MSG: &str = "Error: task creation was interrupted — idempotency record expired without completion. \
         Please retry with a new idempotency key.";

    let mut affected: u64 = 0;
    loop {
        let result = sqlx::query(
            "UPDATE session_todo_idempotency \
             SET output = ?, updated_at = NOW(6) \
             WHERE output IS NULL \
               AND updated_at < NOW(6) - INTERVAL ? MINUTE \
             LIMIT ?",
        )
        .bind(ERROR_MSG)
        .bind(IDEMPOTENCY_STALE_MINUTES as i64)
        .bind(IDEMPOTENCY_BATCH_LIMIT)
        .execute(pool.get())
        .await
        .map_err(|e| format!("stale idempotency sweep: {e}"))?;
        let n = result.rows_affected();
        if n == 0 {
            break;
        }
        affected = affected.saturating_add(n);
    }
    Ok(affected)
}

/// Run a single terminal→archived pass. Returns the number of rows
/// transitioned. Weekly cadence is enough because this is purely a
/// hygiene sweep for stale history, not a user-facing real-time state
/// change.
pub(crate) async fn run_completed_auto_archive_once(pool: SharedPool) -> Result<u64, String> {
    run_completed_auto_archive_batch(pool, ARCHIVE_BATCH_LIMIT).await
}

async fn run_completed_auto_archive_batch(pool: SharedPool, limit: i64) -> Result<u64, String> {
    let days = completed_auto_archive_days();
    let limit = limit.max(1);

    // Process in sub-batches of SUB_BATCH_SIZE to avoid a single large
    // transaction that delays the entire weekly cleanup on transient failure.
    // Each sub-batch commits independently so partial progress survives.
    const SUB_BATCH_SIZE: i64 = 50;

    let mut total_affected = 0u64;
    let mut remaining = limit;

    while remaining > 0 {
        let batch_size = remaining.min(SUB_BATCH_SIZE);
        let mut tx = pool
            .get()
            .begin()
            .await
            .map_err(|e| format!("completed-auto-archive tx begin: {e}"))?;

        // The process-level sweeper lease is the ordinary single-owner path,
        // but candidate claiming must still be correct if a lease handoff,
        // operator-triggered pass, or test overlaps another worker. Lock the
        // deterministic oldest-first batch inside the mutation transaction so
        // two workers cannot both select a row and then report false progress.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT user_id, session_id, todo_id FROM session_todos \
             WHERE status IN ('completed', 'failed', 'cancelled') \
               AND updated_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
             ORDER BY updated_at ASC, user_id ASC, session_id ASC, todo_id ASC \
             LIMIT ? \
             FOR UPDATE",
        )
        .bind(days)
        .bind(batch_size)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| format!("completed-auto-archive select/decode: {e}"))?;

        if rows.is_empty() {
            break;
        }

        let mut affected = 0u64;
        let mut archived_by_owner_session: HashMap<(String, String), HashSet<String>> =
            HashMap::new();
        for (user_id, session_id, todo_id) in &rows {
            let result = sqlx::query(
                "UPDATE session_todos \
                 SET status = 'archived', archived_at = NOW(6), updated_at = NOW(6) \
                 WHERE user_id = ? AND session_id = ? AND todo_id = ? \
                   AND status IN ('completed', 'failed', 'cancelled') \
                   AND updated_at < DATE_SUB(NOW(6), INTERVAL ? DAY)",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(todo_id)
            .bind(days)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("completed-auto-archive update: {e}"))?;
            let rows_affected = result.rows_affected();
            affected = affected.saturating_add(rows_affected);
            if rows_affected > 0 {
                archived_by_owner_session
                    .entry((user_id.clone(), session_id.clone()))
                    .or_default()
                    .insert(todo_id.clone());
            }
        }

        for ((user_id, session_id), archived_ids) in &archived_by_owner_session {
            detach_auto_archived_dependency_edges(&mut tx, user_id, session_id, archived_ids)
                .await?;
        }

        tx.commit()
            .await
            .map_err(|e| format!("completed-auto-archive commit: {e}"))?;

        total_affected = total_affected.saturating_add(affected);
        remaining -= batch_size;

        // If this sub-batch returned fewer rows than requested, no more
        // candidates exist — stop early.
        if (rows.len() as i64) < batch_size {
            break;
        }
    }

    Ok(total_affected)
}

fn decode_edge_ids(
    value: Option<String>,
    column: &'static str,
    todo_id: &str,
) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    serde_json::from_str::<Vec<String>>(&value).map_err(|e| {
        format!("session_todos.{column} for task {todo_id} contains invalid JSON: {e}")
    })
}

fn encode_edge_ids(ids: &[String]) -> Result<Option<String>, String> {
    if ids.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(ids)
        .map(Some)
        .map_err(|e| format!("dependency edge serialization failed: {e}"))
}

async fn detach_auto_archived_dependency_edges(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    archived_ids: &HashSet<String>,
) -> Result<(), String> {
    if archived_ids.is_empty() {
        return Ok(());
    }

    let rows = sqlx::query(
        "SELECT todo_id, blocks, blocked_by FROM session_todos \
         WHERE user_id = ? \
           AND session_id = ? \
           AND (blocks IS NOT NULL OR blocked_by IS NOT NULL) \
         ORDER BY ordinal ASC \
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| format!("completed-auto-archive dependency select: {e}"))?;

    for row in rows {
        let todo_id: String = row
            .try_get("todo_id")
            .map_err(|e| format!("completed-auto-archive dependency todo_id decode: {e}"))?;
        let blocks_raw: Option<String> = row.try_get("blocks").map_err(|e| {
            format!("completed-auto-archive dependency blocks decode for {todo_id}: {e}")
        })?;
        let blocked_by_raw: Option<String> = row.try_get("blocked_by").map_err(|e| {
            format!("completed-auto-archive dependency blocked_by decode for {todo_id}: {e}")
        })?;
        let mut blocks = decode_edge_ids(blocks_raw, "blocks", &todo_id)?;
        let mut blocked_by = decode_edge_ids(blocked_by_raw, "blocked_by", &todo_id)?;
        if !detach_dependency_edges_for_task_ids(
            &todo_id,
            &mut blocks,
            &mut blocked_by,
            archived_ids,
        ) {
            continue;
        }

        let blocks_json = encode_edge_ids(&blocks)?;
        let blocked_by_json = encode_edge_ids(&blocked_by)?;

        sqlx::query(
            "UPDATE session_todos \
             SET blocks = ?, blocked_by = ? \
             WHERE user_id = ? AND session_id = ? AND todo_id = ?",
        )
        .bind(blocks_json)
        .bind(blocked_by_json)
        .bind(user_id)
        .bind(session_id)
        .bind(&todo_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("completed-auto-archive dependency update: {e}"))?;
    }

    Ok(())
}

/// Run a single archived-GC pass. Hard-deletes rows that have been
/// `archived` for longer than the retention window. Returns the
/// affected row count.
pub(crate) async fn run_archive_gc_once(pool: SharedPool) -> Result<u64, String> {
    run_archive_gc_batch(pool, ARCHIVE_GC_BATCH_LIMIT).await
}

async fn run_archive_gc_batch(pool: SharedPool, limit: i64) -> Result<u64, String> {
    let days = archive_retention_days();
    let limit = limit.max(1);

    // Process in sub-batches of SUB_BATCH_SIZE to avoid a single large
    // transaction that delays the entire weekly GC on transient failure.
    // Each sub-batch commits independently so partial progress survives.
    const SUB_BATCH_SIZE: i64 = 50;

    let mut total_affected = 0u64;
    let mut remaining = limit;

    while remaining > 0 {
        let batch_size = remaining.min(SUB_BATCH_SIZE);
        let mut tx = pool
            .get()
            .begin()
            .await
            .map_err(|e| format!("archive-gc tx begin: {e}"))?;

        // GC uses the same transaction-local claim boundary as auto-archive.
        // Stable tie-breakers prevent an equal-timestamp backlog from
        // repeatedly favoring an arbitrary subset.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT user_id, session_id, todo_id FROM session_todos \
             WHERE status = 'archived' \
               AND archived_at IS NOT NULL \
               AND archived_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
             ORDER BY archived_at ASC, user_id ASC, session_id ASC, todo_id ASC \
             LIMIT ? \
             FOR UPDATE",
        )
        .bind(days)
        .bind(batch_size)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| format!("archive-gc select/decode: {e}"))?;

        if rows.is_empty() {
            break;
        }

        let mut affected = 0u64;
        for (user_id, session_id, todo_id) in &rows {
            let result = sqlx::query(
                "DELETE FROM session_todos \
                 WHERE user_id = ? AND session_id = ? AND todo_id = ? AND status = 'archived' \
                   AND archived_at IS NOT NULL \
                   AND archived_at < DATE_SUB(NOW(6), INTERVAL ? DAY)",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(todo_id)
            .bind(days)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("archive-gc delete: {e}"))?;
            affected = affected.saturating_add(result.rows_affected());
        }

        tx.commit()
            .await
            .map_err(|e| format!("archive-gc commit: {e}"))?;

        total_affected = total_affected.saturating_add(affected);
        remaining -= batch_size;

        // If this sub-batch returned fewer rows than requested, no more
        // candidates exist — stop early.
        if (rows.len() as i64) < batch_size {
            break;
        }
    }

    Ok(total_affected)
}

/// Spawn the stale-in_progress sweeper. Runs once on startup and then
/// every 5 minutes; missed ticks are coalesced (`Delay`) so a paused
/// server doesn't cause a ticker burst on resume.
pub(crate) fn spawn_session_todo_stale_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(STALE_SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut consecutive_failures: u32 = 0;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let tick_result = AssertUnwindSafe(async {
                        match lease.check_leader().await {
                            crate::server::sweeper_lease::LeaderStatus::Leader => {}
                            crate::server::sweeper_lease::LeaderStatus::NotLeader => return,
                            crate::server::sweeper_lease::LeaderStatus::Unavailable(e) => {
                                tracing::warn!(
                                    target: "astra_runtime::session_todo_sweeper",
                                    error = %e,
                                    "stale sweep lease check unavailable, skipping"
                                );
                                return;
                            }
                        }
                        match run_stale_in_progress_sweep(pool.clone()).await {
                            Ok(0) => {
                                consecutive_failures = 0;
                            }
                            Ok(n) => {
                                consecutive_failures = 0;
                                tracing::info!(
                                    target: "astra_runtime::session_todo_sweeper",
                                    rows = n,
                                    "auto-paused {n} stale in_progress task(s)"
                                );
                            }
                            Err(e) => {
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                if consecutive_failures >= STALE_SWEEP_ALERT_THRESHOLD {
                                    tracing::error!(
                                        target: "astra_runtime::session_todo_sweeper",
                                        consecutive_failures = consecutive_failures,
                                        error = %e,
                                        "stale-in_progress sweeper has failed {consecutive_failures} \
                                         consecutive times — DB or infrastructure may be degraded"
                                    );
                                } else {
                                    tracing::error!(
                                        target: "astra_runtime::session_todo_sweeper",
                                        error = %e,
                                        "stale-in_progress sweep failed"
                                    );
                                }
                            }
                        }
                        match run_stale_idempotency_sweep(pool.clone()).await {
                            Ok(0) => {}
                            Ok(n) => {
                                tracing::info!(
                                    target: "astra_runtime::session_todo_sweeper",
                                    rows = n,
                                    "completed {n} stale idempotency row(s) with error message"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    target: "astra_runtime::session_todo_sweeper",
                                    error = %e,
                                    "stale-idempotency sweep failed"
                                );
                            }
                        }
                    })
                    .catch_unwind()
                    .await;
                    if let Err(panic_err) = tick_result {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let msg = panic_err
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("unknown panic");
                        if consecutive_failures >= STALE_SWEEP_ALERT_THRESHOLD {
                            tracing::error!(
                                target: "astra_runtime::session_todo_sweeper",
                                consecutive_failures = consecutive_failures,
                                panic = %msg,
                                "stale-in_progress sweeper has panicked {consecutive_failures} \
                                 consecutive times — DB or infrastructure may be degraded"
                            );
                        } else {
                            tracing::error!(
                                target: "astra_runtime::session_todo_sweeper",
                                panic = %msg,
                                "stale-in_progress sweeper panicked; will retry on next tick"
                            );
                        }
                    }
                }
            }
        }
    })
}

/// Spawn the weekly archive hygiene sweepers. First pass auto-archives
/// stale `completed` rows; second pass hard-deletes long-retained
/// `archived` rows. Fires on the first tick so a freshly-started server
/// still performs backlog cleanup immediately.
pub(crate) fn spawn_session_todo_archive_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(ARCHIVE_SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let tick_result = AssertUnwindSafe(async {
                        match lease.check_leader().await {
                            crate::server::sweeper_lease::LeaderStatus::Leader => {}
                            crate::server::sweeper_lease::LeaderStatus::NotLeader => return,
                            crate::server::sweeper_lease::LeaderStatus::Unavailable(e) => {
                                tracing::warn!(
                                    target: "astra_runtime::session_todo_sweeper",
                                    error = %e,
                                    "archive sweep lease check unavailable, skipping"
                                );
                                return;
                            }
                        }
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
                    })
                    .catch_unwind()
                    .await;
                    if let Err(panic_err) = tick_result {
                        let msg = panic_err
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("unknown panic");
                        tracing::error!(
                            target: "astra_runtime::session_todo_sweeper",
                            panic = %msg,
                            "archive sweeper panicked; will retry on next tick"
                        );
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_matrixone_settings() -> astra_core::MatrixOneSettings {
        let mut settings = astra_core::MatrixOneSettings::from_env();
        settings.db_pool_max_connections = settings.db_pool_max_connections.min(4);
        settings.db_pool_min_connections = settings.db_pool_min_connections.min(1);
        settings
    }

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
    fn auto_pause_metadata_preserves_existing_object_keys() {
        let metadata =
            auto_pause_metadata_json(Some(r#"{"owner_note":"keep","count":2}"#), "paused-at");
        let parsed: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(parsed["owner_note"], "keep");
        assert_eq!(parsed["count"], 2);
        assert_eq!(parsed["auto_paused_reason"], "stale_in_progress > 24h");
        assert_eq!(parsed["auto_paused_at"], "paused-at");
        assert!(parsed.get("metadata_repair_reason").is_none(), "{parsed}");
    }

    #[test]
    fn auto_pause_metadata_repairs_invalid_or_non_object_metadata() {
        for (existing, expected_original) in [
            (Some("not-json"), Value::String("not-json".to_string())),
            (Some("[1,2]"), serde_json::json!([1, 2])),
        ] {
            let metadata = auto_pause_metadata_json(existing, "paused-at");
            let parsed: serde_json::Value = serde_json::from_str(&metadata).unwrap();
            assert_eq!(parsed["auto_paused_reason"], "stale_in_progress > 24h");
            assert_eq!(parsed["auto_paused_at"], "paused-at");
            assert_eq!(
                parsed["metadata_before_auto_pause"], expected_original,
                "{parsed}"
            );
            assert_eq!(
                parsed["metadata_repair_reason"],
                "invalid_or_non_object_metadata_before_auto_pause"
            );
        }
    }

    #[test]
    fn decode_edge_ids_rejects_corrupt_dependency_columns() {
        assert_eq!(
            decode_edge_ids(None, "blocks", "task-1").expect("null means no edges"),
            Vec::<String>::new()
        );
        assert_eq!(
            decode_edge_ids(Some(r#"["task-2"]"#.into()), "blocks", "task-1")
                .expect("valid edge array"),
            vec!["task-2".to_string()]
        );

        let bad_json = decode_edge_ids(Some("not-json".into()), "blocked_by", "task-1")
            .expect_err("invalid JSON must not be treated as empty");
        assert!(
            bad_json.contains("session_todos.blocked_by")
                && bad_json.contains("task-1")
                && bad_json.contains("invalid JSON"),
            "unexpected error: {bad_json}"
        );

        let wrong_shape = decode_edge_ids(Some(r#"{"task":"task-2"}"#.into()), "blocks", "task-1")
            .expect_err("wrong dependency shape must not be treated as empty");
        assert!(
            wrong_shape.contains("session_todos.blocks") && wrong_shape.contains("task-1"),
            "unexpected error: {wrong_shape}"
        );
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
    fn automatic_archive_work_is_batch_limited() {
        assert_eq!(ARCHIVE_BATCH_LIMIT, 500);
        assert_eq!(ARCHIVE_GC_BATCH_LIMIT, 500);
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

    async fn cleanup_sweeper_fixture_for_owner(
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) {
        sqlx::query("DELETE FROM session_todo_idempotency WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo sweeper fixture session_todo_idempotency");
        sqlx::query("DELETE FROM session_todos WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo sweeper fixture session_todos");
        sqlx::query("DELETE FROM session_todo_counters WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo sweeper fixture session_todo_counters");
        sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup session todo sweeper fixture agent_sessions");
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn completed_auto_archive_moves_old_terminal_rows() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
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
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, blocks, blocked_by, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'old done', 'completed', '[\"task-3\"]', NULL, NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 8 DAY)), \
                 (?, 'task-2', ?, 1, 'recent done', 'completed', NULL, NULL, NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 2 DAY)), \
                 (?, 'task-3', ?, 2, 'old in progress', 'in_progress', NULL, '[\"task-1\"]', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 8 DAY)), \
                 (?, 'task-4', ?, 3, 'old failed', 'failed', NULL, NULL, NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 8 DAY)), \
                 (?, 'task-5', ?, 4, 'old cancelled', 'cancelled', NULL, NULL, NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 8 DAY))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&user_id)
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
        assert!(
            archived >= 3,
            "global sweeper may also archive unrelated stale rows; got {archived}"
        );

        let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT status, CAST(archived_at AS CHAR) AS archived_at, blocks, blocked_by \
             FROM session_todos WHERE session_id = ? AND user_id = ? ORDER BY ordinal ASC",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_all(&pool)
        .await
        .expect("load session_todos");
        assert_eq!(rows[0].0, "archived");
        assert!(rows[0].1.is_some(), "{rows:?}");
        assert!(rows[0].2.is_none(), "{rows:?}");
        assert!(rows[0].3.is_none(), "{rows:?}");
        assert_eq!(rows[1].0, "completed");
        assert!(rows[1].1.is_none(), "{rows:?}");
        assert_eq!(rows[2].0, "in_progress");
        assert!(rows[2].1.is_none(), "{rows:?}");
        assert!(rows[2].2.is_none(), "{rows:?}");
        assert!(rows[2].3.is_none(), "{rows:?}");
        assert_eq!(rows[3].0, "archived");
        assert!(rows[3].1.is_some(), "{rows:?}");
        assert_eq!(rows[4].0, "archived");
        assert!(rows[4].1.is_some(), "{rows:?}");

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn completed_auto_archive_detaches_dependencies_only_within_owner_session() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");
        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let session_id = format!("todo-auto-archive-owner-{}", uuid::Uuid::new_v4());
        let user_a = format!("user-a-{}", uuid::Uuid::new_v4());
        let user_b = format!("user-b-{}", uuid::Uuid::new_v4());
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_a).await;
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_b).await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, blocked_by, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'old done owner a', 'completed', NULL, NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 1000 DAY)), \
                 (?, 'task-2', ?, 1, 'blocked owner a', 'in_progress', '[\"task-1\"]', NULL, NOW(6), NOW(6)), \
                 (?, 'task-1', ?, 0, 'same id owner b', 'in_progress', NULL, NULL, NOW(6), NOW(6)), \
                 (?, 'task-2', ?, 1, 'blocked owner b', 'in_progress', '[\"task-1\"]', NULL, NOW(6), NOW(6))",
        )
        .bind(&session_id)
        .bind(&user_a)
        .bind(&session_id)
        .bind(&user_a)
        .bind(&session_id)
        .bind(&user_b)
        .bind(&session_id)
        .bind(&user_b)
        .execute(&pool)
        .await
        .expect("seed owner-colliding session_todos");

        let archived = run_completed_auto_archive_batch(shared, 50)
            .await
            .expect("auto archive");
        assert!(
            archived >= 1,
            "global sweeper may also archive unrelated stale rows; got {archived}"
        );

        let owner_a_blocked_by: Option<String> = sqlx::query_scalar(
            "SELECT blocked_by FROM session_todos \
             WHERE user_id = ? AND session_id = ? AND todo_id = 'task-2'",
        )
        .bind(&user_a)
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("load owner a dependent row");
        let owner_b_blocked_by: Option<String> = sqlx::query_scalar(
            "SELECT blocked_by FROM session_todos \
             WHERE user_id = ? AND session_id = ? AND todo_id = 'task-2'",
        )
        .bind(&user_b)
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("load owner b dependent row");

        assert!(
            owner_a_blocked_by.is_none(),
            "owner a dependency edge should detach after task-1 is archived"
        );
        assert_eq!(
            owner_b_blocked_by.as_deref(),
            Some("[\"task-1\"]"),
            "owner b dependency edge must not be detached by owner a archive"
        );

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_a).await;
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_b).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn completed_auto_archive_rejects_corrupt_dependency_edges_in_matrixone() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");
        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let session_id = format!("todo-auto-archive-bad-edges-{}", uuid::Uuid::new_v4());
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, blocks, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'old done with corrupt edges', 'completed', 'not-json', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 10000 DAY))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed corrupt dependency edge row");

        let err = run_completed_auto_archive_batch(shared, 1)
            .await
            .expect_err("corrupt dependency edges must fail the archive transaction");
        assert!(
            err.contains("session_todos.blocks") && err.contains("invalid JSON"),
            "unexpected error: {err}"
        );

        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, CAST(archived_at AS CHAR) AS archived_at, blocks \
             FROM session_todos WHERE session_id = ? AND user_id = ? AND todo_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind("task-1")
        .fetch_one(&pool)
        .await
        .expect("load row after failed archive");
        assert_eq!(
            row.0, "completed",
            "failed dependency detach must rollback the archive update"
        );
        assert!(row.1.is_none(), "{row:?}");
        assert_eq!(row.2.as_deref(), Some("not-json"));

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn completed_auto_archive_batch_limit_bounds_automatic_work() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");
        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let session_id = format!("todo-auto-archive-limit-{}", uuid::Uuid::new_v4());
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'old done 1', 'completed', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 1000 DAY)), \
                 (?, 'task-2', ?, 1, 'old done 2', 'completed', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 1000 DAY))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed session_todos");

        let archived = run_completed_auto_archive_batch(shared, 1)
            .await
            .expect("auto archive batch");
        assert_eq!(archived, 1);

        let counts: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, COUNT(*) AS count FROM session_todos \
             WHERE session_id = ? AND user_id = ? GROUP BY status ORDER BY status",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_all(&pool)
        .await
        .expect("load status counts");
        assert!(
            counts.contains(&("archived".to_string(), 1))
                && counts.contains(&("completed".to_string(), 1)),
            "batch limit should archive exactly one old completed row: {counts:?}"
        );

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn archive_gc_batch_limit_bounds_automatic_deletes() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");
        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let session_id = format!("todo-archive-gc-limit-{}", uuid::Uuid::new_v4());
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'old archived 1', 'archived', DATE_SUB(NOW(6), INTERVAL 91 DAY), NOW(6), DATE_SUB(NOW(6), INTERVAL 91 DAY)), \
                 (?, 'task-2', ?, 1, 'old archived 2', 'archived', DATE_SUB(NOW(6), INTERVAL 91 DAY), NOW(6), DATE_SUB(NOW(6), INTERVAL 91 DAY))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed session_todos");

        let deleted = run_archive_gc_batch(shared, 1)
            .await
            .expect("archive gc batch");
        assert_eq!(deleted, 1);

        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_todos WHERE session_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("remaining count");
        assert_eq!(
            remaining, 1,
            "batch limit should delete exactly one archived row"
        );

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn stale_in_progress_sweep_surfaces_paused_as_first_class_status() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");
        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let session_id = format!("todo-auto-pause-{}", uuid::Uuid::new_v4());
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, agent_id, title, status, metadata)
             VALUES (?, ?, 'session-todo-sweeper-test', 'session todo sweeper test', 'active', '{}')",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert agent_sessions owner root");

        let store: std::sync::Arc<dyn astra_tools::task_mgmt::TaskStore> = std::sync::Arc::new(
            astra_tools::task_mgmt_matrixone::MatrixOneTaskStore::from_shared_for_user(
                &shared, &user_id,
            )
            .unwrap(),
        );
        let manager = astra_tools::task_mgmt::TaskManager::new(session_id.clone(), store);
        let create = manager
            .create(&serde_json::json!({"title": "stale running work"}))
            .await;
        assert!(create.contains("\"success\":true"), "{create}");
        let start = manager
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(start.contains("\"success\":true"), "{start}");
        sqlx::query(
            "UPDATE session_todos \
             SET updated_at = DATE_SUB(NOW(6), INTERVAL 25 HOUR) \
             WHERE session_id = ? AND user_id = ? AND todo_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind("task-1")
        .execute(&pool)
        .await
        .expect("age in_progress task");

        let paused_count = run_stale_in_progress_sweep(shared.clone())
            .await
            .expect("stale sweep");
        assert!(
            paused_count >= 1,
            "global sweeper may also pause unrelated stale rows; got {paused_count}"
        );

        let paused: serde_json::Value = serde_json::from_str(
            &manager
                .list(&serde_json::json!({"status_filter": "paused"}))
                .await,
        )
        .expect("paused list json");
        assert_eq!(paused["count"], 1, "{paused}");
        assert_eq!(paused["tasks"][0]["status"], "paused", "{paused}");
        assert_ne!(paused["tasks"][0]["status"], "other", "{paused}");

        let active = manager
            .list(&serde_json::json!({"status_filter": "active"}))
            .await;
        assert!(
            active.contains("\"count\":1")
                && active.contains("\"status\":\"paused\"")
                && active.contains("stale running work"),
            "active list should treat paused work as first-class open work: {active}"
        );

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(session_todo_sweeper_db)]
    async fn stale_in_progress_sweep_repairs_invalid_metadata_in_matrixone() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = test_matrixone_settings();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        let pool = astra_core::connect_matrixone(&settings)
            .await
            .expect("connect matrixone");
        let shared = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let session_id = format!("todo-auto-pause-badmeta-{}", uuid::Uuid::new_v4());
        let user_id = format!("user-{}", uuid::Uuid::new_v4());
        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;

        sqlx::query(
            "INSERT INTO session_todos (\
                 session_id, todo_id, user_id, ordinal, title, status, metadata, archived_at, created_at, updated_at\
             ) VALUES \
                 (?, 'task-1', ?, 0, 'bad metadata running work', 'in_progress', 'not-json', NULL, NOW(6), DATE_SUB(NOW(6), INTERVAL 25 HOUR))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed invalid metadata row");

        let paused = run_stale_in_progress_sweep(shared)
            .await
            .expect("stale sweep should repair invalid metadata instead of failing batch");
        assert!(
            paused >= 1,
            "global sweeper may also pause unrelated stale rows; got {paused}"
        );

        let row: (String, String) = sqlx::query_as(
            "SELECT status, metadata FROM session_todos \
             WHERE session_id = ? AND user_id = ? AND todo_id = ?",
        )
        .bind(&session_id)
        .bind(&user_id)
        .bind("task-1")
        .fetch_one(&pool)
        .await
        .expect("load repaired row");
        assert_eq!(row.0, "paused");
        let metadata: serde_json::Value = serde_json::from_str(&row.1).expect("metadata json");
        assert_eq!(metadata["auto_paused_reason"], "stale_in_progress > 24h");
        assert_eq!(
            metadata["metadata_repair_reason"],
            "invalid_or_non_object_metadata_before_auto_pause"
        );

        cleanup_sweeper_fixture_for_owner(&pool, &session_id, &user_id).await;
    }
}
