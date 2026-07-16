//! Edge dispatch relay: cross-pod tool dispatch via DB-backed queue.
//!
//! When the in-memory edge ledger times out or a tool targets an agent connected
//! to a different pod, the dispatch relay persists the request and the
//! owning pod's edge WS handler polls it for delivery.  Results flow back
//! through `deliver_result`.
//!
//! Split from the monolithic `multi_agent.rs`.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use sqlx::{MySql, Row};

use super::metrics::{SharedMultiAgentMetrics, saturating_decrement};
use crate::db_row::RowExt as EdgeDispatchDbRow;
use crate::interaction_contract::{InteractionStatus, edge_dispatch_status};

#[derive(Debug)]
pub struct EdgeDispatchRow {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub edge_agent_id: String,
    pub request_id: String,
    pub payload_json: String,
    pub result_json: Option<String>,
    pub status: String,
    pub pending_wait_us: u64,
}

impl EdgeDispatchRow {
    pub fn identity(&self) -> EdgeDispatchIdentity {
        EdgeDispatchIdentity::new(
            self.user_id.clone(),
            self.session_id.clone(),
            self.run_id.clone(),
            self.turn_chain_id.clone(),
            self.request_id.clone(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeDispatchIdentity {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub request_id: String,
}

impl EdgeDispatchIdentity {
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        turn_chain_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            turn_chain_id: turn_chain_id.into(),
            request_id: request_id.into(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.user_id.trim().is_empty()
            && !self.session_id.trim().is_empty()
            && !self.run_id.trim().is_empty()
            && !self.turn_chain_id.trim().is_empty()
            && !self.request_id.trim().is_empty()
    }

    pub fn for_request_id(&self, request_id: impl Into<String>) -> Self {
        Self {
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_chain_id: self.turn_chain_id.clone(),
            request_id: request_id.into(),
        }
    }

    fn request_id_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "user_id": self.user_id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "turn_chain_id": self.turn_chain_id,
            "request_id": self.request_id,
        })
    }
}

async fn rollback_edge_dispatch_tx(tx: sqlx::Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "edge_dispatch rollback failed");
    }
}

#[async_trait]
pub trait EdgeDispatchService: Send + Sync {
    /// Insert a new pending dispatch idempotently inside the full turn boundary.
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<(), String>;

    /// Poll for pending dispatches targeting the given (user, agent) pairs.
    /// Returns dispatches that are still 'pending' and not yet dispatched.
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String>;

    /// Deliver a tool result (from HTTP callback or WS) — updates status to 'completed'.
    /// The full dispatch identity and `edge_agent_id` must match the dispatch
    /// record to prevent cross-owner, cross-run, or cross-agent injection.
    async fn deliver_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String>;

    /// Move an in-flight dispatch to a failed terminal state.
    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        reason: &str,
    ) -> Result<bool, String>;

    /// Poll for a specific request's result. Returns Some(result_json) when completed.
    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String>;

    /// Clean up stale dispatches older than `older_than`.
    async fn cleanup_stale(&self, older_than: std::time::Duration) -> Result<u64, String>;
}

pub struct DatabaseEdgeDispatchService {
    pool: sqlx::Pool<sqlx::MySql>,
    metrics: Option<SharedMultiAgentMetrics>,
}

impl DatabaseEdgeDispatchService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self {
            pool,
            metrics: None,
        }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: SharedMultiAgentMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

fn edge_dispatch_decode_error(context: &str, column: &'static str, error: sqlx::Error) -> String {
    format!("edge_dispatch {context} decode `{column}`: {error}")
}

fn decode_claimed_dispatch_row(row: &impl EdgeDispatchDbRow) -> Result<EdgeDispatchRow, String> {
    Ok(EdgeDispatchRow {
        user_id: row
            .string_column("user_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "user_id", e))?,
        session_id: row
            .string_column("session_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "session_id", e))?,
        run_id: row
            .string_column("run_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "run_id", e))?,
        turn_chain_id: row
            .string_column("turn_chain_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "turn_chain_id", e))?,
        edge_agent_id: row
            .string_column("edge_agent_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "edge_agent_id", e))?,
        request_id: row
            .string_column("request_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "request_id", e))?,
        payload_json: row
            .string_column("payload_json")
            .map_err(|e| edge_dispatch_decode_error("poll row", "payload_json", e))?,
        result_json: row
            .optional_string_column("result_json")
            .map_err(|e| edge_dispatch_decode_error("poll row", "result_json", e))?,
        status: "dispatched".to_string(),
        pending_wait_us: non_negative_i64_column(row, "pending_wait_us", "poll row")? as u64,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeDispatchBacklogRow {
    pending_rows: u64,
    dispatched_rows: u64,
    oldest_pending_age_ms: u64,
    oldest_dispatched_age_ms: u64,
}

fn edge_dispatch_failed_result_json(request_id: serde_json::Value, output: String) -> String {
    serde_json::json!({
        "request_id": request_id,
        "status": "error",
        "output": output,
        "duration_ms": 0,
    })
    .to_string()
}

impl EdgeDispatchBacklogRow {
    fn empty() -> Self {
        Self {
            pending_rows: 0,
            dispatched_rows: 0,
            oldest_pending_age_ms: 0,
            oldest_dispatched_age_ms: 0,
        }
    }
}

const EDGE_DISPATCH_BACKLOG_SQL: &str = "SELECT \
    COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS pending_rows, \
    COALESCE(SUM(CASE WHEN status = 'dispatched' THEN 1 ELSE 0 END), 0) AS dispatched_rows, \
    COALESCE(TIMESTAMPDIFF(MICROSECOND, MIN(CASE WHEN status = 'pending' THEN created_at ELSE NULL END), NOW(6)), 0) AS oldest_pending_age_us, \
    COALESCE(TIMESTAMPDIFF(MICROSECOND, MIN(CASE WHEN status = 'dispatched' THEN created_at ELSE NULL END), NOW(6)), 0) AS oldest_dispatched_age_us \
    FROM edge_pending_dispatch \
    WHERE status IN ('pending', 'dispatched')";

fn non_negative_i64_column(
    row: &impl EdgeDispatchDbRow,
    column: &'static str,
    context: &str,
) -> Result<i64, String> {
    let value = row
        .i64_column(column)
        .map_err(|e| edge_dispatch_decode_error(context, column, e))?;
    if value < 0 {
        return Err(format!(
            "edge_dispatch {context} decode `{column}`: negative value {value}"
        ));
    }
    Ok(value)
}

fn micros_to_millis(us: u64) -> u64 {
    us.saturating_add(999) / 1000
}

fn decode_backlog_row(row: &impl EdgeDispatchDbRow) -> Result<EdgeDispatchBacklogRow, String> {
    Ok(EdgeDispatchBacklogRow {
        pending_rows: non_negative_i64_column(row, "pending_rows", "backlog row")? as u64,
        dispatched_rows: non_negative_i64_column(row, "dispatched_rows", "backlog row")? as u64,
        oldest_pending_age_ms: micros_to_millis(non_negative_i64_column(
            row,
            "oldest_pending_age_us",
            "backlog row",
        )? as u64),
        oldest_dispatched_age_ms: micros_to_millis(non_negative_i64_column(
            row,
            "oldest_dispatched_age_us",
            "backlog row",
        )? as u64),
    })
}

fn apply_backlog_metrics(metrics: &SharedMultiAgentMetrics, backlog: EdgeDispatchBacklogRow) {
    metrics
        .dispatch_pending_rows
        .store(backlog.pending_rows, Ordering::Relaxed);
    metrics
        .dispatch_dispatched_rows
        .store(backlog.dispatched_rows, Ordering::Relaxed);
    metrics
        .dispatch_oldest_pending_age_ms
        .store(backlog.oldest_pending_age_ms, Ordering::Relaxed);
    metrics
        .dispatch_oldest_dispatched_age_ms
        .store(backlog.oldest_dispatched_age_ms, Ordering::Relaxed);
}

/// Refresh DB-authoritative edge dispatch backlog gauges.
///
/// Hot-path counters are process-local approximations. This scrape-time query
/// gives operators the cross-pod truth for queue depth and oldest pending age.
pub async fn refresh_edge_dispatch_backlog_metrics(
    shared: &astra_core::SharedPool,
    metrics: &SharedMultiAgentMetrics,
) -> Result<(), String> {
    let row = sqlx::query(EDGE_DISPATCH_BACKLOG_SQL)
        .fetch_optional(shared.get())
        .await
        .map_err(|e| format!("edge_dispatch backlog metrics SELECT: {e}"))?;
    let backlog = match row {
        Some(row) => decode_backlog_row(&row)?,
        None => EdgeDispatchBacklogRow::empty(),
    };
    apply_backlog_metrics(metrics, backlog);
    Ok(())
}

fn decode_terminal_result_json(row: &impl EdgeDispatchDbRow) -> Result<String, String> {
    row.optional_string_column("result_json")
        .map_err(|e| edge_dispatch_decode_error("wait_result terminal row", "result_json", e))?
        .ok_or_else(|| "edge_dispatch wait_result terminal row missing `result_json`".to_string())
}

fn validate_claimed_dispatch_update_count(expected: usize, actual: u64) -> Result<(), String> {
    if actual == expected as u64 {
        return Ok(());
    }
    Err(format!(
        "edge_dispatch poll UPDATE claimed {expected} rows but updated {actual}"
    ))
}

#[async_trait]
impl EdgeDispatchService for DatabaseEdgeDispatchService {
    #[tracing::instrument(skip(self, identity, payload_json), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, edge_agent_id = %edge_agent_id, request_id = %identity.request_id))]
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<(), String> {
        if !identity.is_complete() {
            return Err("edge_dispatch insert: incomplete dispatch identity".to_string());
        }
        // Idempotent insert inside a full turn boundary: duplicate calls for
        // the same dispatched tool are harmless retries, while same request_id
        // values in other runs/chains remain isolated.
        match sqlx::query(
            "INSERT IGNORE INTO edge_pending_dispatch \
             (user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, payload_json, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(edge_agent_id)
        .bind(&identity.request_id)
        .bind(payload_json)
        .execute(&self.pool)
        .await
        {
            Ok(r) => {
                if r.rows_affected() > 0
                    && let Some(ref m) = self.metrics
                {
                    m.dispatch_queue_depth.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            Err(e) => Err(format!("edge_dispatch insert: {e}")),
        }
    }
    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        // Fast path: non-locking COUNT before opening any transaction.
        // MatrixOne can acquire a table-level lock on SELECT FOR UPDATE even
        // when the result set is empty, adding significant latency (seconds)
        // per 2-second poll cycle when there is nothing to claim. Skipping
        // the transaction when the count is zero avoids that entirely. The
        // tiny TOCTOU window (a new row arriving between COUNT and FOR UPDATE)
        // is benign: we will pick it up in the next poll interval.
        let fast_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending'",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch poll COUNT: {e}"))?;

        if fast_count == 0 {
            return Ok(vec![]);
        }

        // Atomically claim pending dispatches using SELECT FOR UPDATE
        // within a transaction. This eliminates the race window between
        // poll and mark — two pods polling simultaneously cannot both
        // claim the same rows.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("edge_dispatch poll begin tx: {e}"))?;

        let rows = match sqlx::query(
            "SELECT user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, \
             CAST(payload_json AS CHAR) AS payload_json, \
             CAST(result_json AS CHAR) AS result_json, \
             status, \
             COALESCE(TIMESTAMPDIFF(MICROSECOND, created_at, NOW(6)), 0) AS pending_wait_us \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending' \
             ORDER BY created_at ASC, request_id ASC LIMIT 50 \
             FOR UPDATE",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "poll select").await;
                return Err(format!("edge_dispatch poll SELECT: {e}"));
            }
        };

        if rows.is_empty() {
            tx.commit()
                .await
                .map_err(|e| format!("edge_dispatch poll commit (no rows): {e}"))?;
            return Ok(vec![]);
        }

        let claimed_rows: Vec<EdgeDispatchRow> = match rows
            .iter()
            .map(decode_claimed_dispatch_row)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(rows) => rows,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "decode claimed rows").await;
                return Err(e);
            }
        };

        // Mark claimed rows as dispatched within the same transaction.
        // MatrixOne rejects row-value `IN ((?, ?), ...)`, so spell the identity
        // set as disjunctions over the full turn-scoped primary key.
        let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "UPDATE edge_pending_dispatch \
             SET status = 'dispatched', dispatched_at = NOW(6) \
             WHERE status = 'pending' AND (",
        );
        let mut first_identity = true;
        for row in &claimed_rows {
            if !first_identity {
                update.push(" OR ");
            }
            first_identity = false;
            update
                .push("(user_id = ")
                .push_bind(&row.user_id)
                .push(" AND session_id = ")
                .push_bind(&row.session_id)
                .push(" AND run_id = ")
                .push_bind(&row.run_id)
                .push(" AND turn_chain_id = ")
                .push_bind(&row.turn_chain_id)
                .push(" AND request_id = ")
                .push_bind(&row.request_id)
                .push(")");
        }
        update.push(")");
        let update_result = match update.build().execute(&mut *tx).await {
            Ok(result) => result,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "poll update").await;
                return Err(format!("edge_dispatch poll UPDATE: {e}"));
            }
        };
        if let Err(e) = validate_claimed_dispatch_update_count(
            claimed_rows.len(),
            update_result.rows_affected(),
        ) {
            rollback_edge_dispatch_tx(tx, "validate claimed dispatch count").await;
            return Err(e);
        }

        tx.commit()
            .await
            .map_err(|e| format!("edge_dispatch poll commit: {e}"))?;

        if let Some(ref m) = self.metrics {
            m.dispatch_claimed_total
                .fetch_add(claimed_rows.len() as u64, Ordering::Relaxed);
            for row in &claimed_rows {
                m.dispatch_claim_wait_latency
                    .record(std::time::Duration::from_micros(row.pending_wait_us));
            }
        }

        Ok(claimed_rows)
    }

    #[tracing::instrument(skip(self, identity, result_json), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, edge_agent_id = %edge_agent_id))]
    async fn deliver_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let start = std::time::Instant::now();
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .bind(edge_agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch deliver_result: {e}"))?;
        let affected = n.rows_affected() > 0;
        if let Some(ref m) = self.metrics {
            m.dispatch_deliver_update_latency.record(start.elapsed());
            if affected {
                saturating_decrement(&m.dispatch_queue_depth);
                m.dispatch_deliver_hits_total
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                m.dispatch_deliver_misses_total
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(affected)
    }

    #[tracing::instrument(skip(self, identity), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, reason = %reason))]
    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        reason: &str,
    ) -> Result<bool, String> {
        let output = format!("edge dispatch {reason}");
        let result_json =
            edge_dispatch_failed_result_json(identity.request_id_json_value(), output);
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch fail_dispatch: {e}"))?;
        let affected = n.rows_affected() > 0;
        if affected && let Some(ref m) = self.metrics {
            saturating_decrement(&m.dispatch_queue_depth);
            m.dispatch_failed_total.fetch_add(1, Ordering::Relaxed);
        }
        Ok(affected)
    }

    #[tracing::instrument(skip(self, identity), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, timeout_ms = timeout.as_millis()))]
    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut iterations: u32 = 0;
        loop {
            let row = sqlx::query(
                "SELECT CAST(result_json AS CHAR) AS result_json, status FROM edge_pending_dispatch \
                 WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND request_id = ?",
            )
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(&identity.request_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch wait_result: {e}"))?;

            match row {
                Some(r) => {
                    let status: String = r.try_get("status").map_err(|e| e.to_string())?;
                    let result_json = r
                        .try_get::<Option<String>, _>("result_json")
                        .map_err(|e| e.to_string())?;
                    match edge_dispatch_status(&status, result_json.as_deref()) {
                        InteractionStatus::Pending => {} // still pending or dispatched
                        InteractionStatus::Resolved
                        | InteractionStatus::Expired
                        | InteractionStatus::Cancelled => {
                            return Ok(Some(decode_terminal_result_json(&r)?));
                        }
                    }
                }
                None => return Ok(None), // request not found
            }

            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    iterations,
                    "edge_dispatch: wait_result timed out after {} iterations",
                    iterations
                );
                if let Some(ref m) = self.metrics {
                    m.dispatch_wait_result_timeouts_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                return Ok(None); // timeout
            }
            // Exponential backoff with jitter: 100ms → 200ms → 400ms → 800ms (capped).
            let backoff_ms = (100u64 * 2u64.saturating_pow(iterations.min(3))).min(800);
            let jitter = fastrand::u64(0..backoff_ms / 2);
            let sleep_ms = backoff_ms / 2 + jitter; // 50%..100% of backoff_ms
            iterations += 1;
            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        }
    }

    async fn cleanup_stale(&self, older_than: std::time::Duration) -> Result<u64, String> {
        let secs = older_than.as_secs() as i64;
        let expired_result_json = edge_dispatch_failed_result_json(
            serde_json::Value::Null,
            "edge dispatch expired".to_string(),
        );
        let expired = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE status IN ('pending', 'dispatched') \
               AND created_at <= DATE_SUB(NOW(6), INTERVAL ? SECOND)",
        )
        .bind(expired_result_json)
        .bind(secs)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch expire stale: {e}"))?
        .rows_affected();
        if expired > 0
            && let Some(ref m) = self.metrics
        {
            for _ in 0..expired {
                saturating_decrement(&m.dispatch_queue_depth);
            }
            m.dispatch_cleanup_expired_total
                .fetch_add(expired, Ordering::Relaxed);
        }

        let deleted = sqlx::query(
            "DELETE FROM edge_pending_dispatch \
             WHERE status IN ('completed', 'failed') \
               AND COALESCE(completed_at, created_at) <= DATE_SUB(NOW(6), INTERVAL ? SECOND)",
        )
        .bind(secs)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch cleanup: {e}"))?;
        if let Some(ref m) = self.metrics {
            m.dispatch_cleanup_deleted_total
                .fetch_add(deleted.rows_affected(), Ordering::Relaxed);
        }
        Ok(expired + deleted.rows_affected())
    }
}

pub struct UnconfiguredEdgeDispatchService;

#[async_trait]
impl EdgeDispatchService for UnconfiguredEdgeDispatchService {
    async fn insert_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _payload_json: &str,
    ) -> Result<(), String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn deliver_result(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn fail_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _reason: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn wait_result(
        &self,
        _identity: &EdgeDispatchIdentity,
        _timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
        Err("edge dispatch service not configured".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    static EDGE_DISPATCH_DB: tokio::sync::OnceCell<astra_core::SharedPool> =
        tokio::sync::OnceCell::const_new();

    async fn setup_edge_dispatch_db_it() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        EDGE_DISPATCH_DB
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                astra_core::SharedPool::new(&settings)
                    .await
                    .expect("SharedPool::new")
            })
            .await
            .clone()
    }

    async fn cleanup_edge_dispatch_fixture(
        pool: &astra_core::SharedPool,
        identity: &EdgeDispatchIdentity,
    ) {
        sqlx::query(
            "DELETE FROM edge_pending_dispatch \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND request_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .execute(pool.get())
        .await
        .expect("cleanup edge dispatch fixture");
    }

    struct FakeEdgeDispatchRow {
        failed_column: Option<&'static str>,
        result_json: Option<String>,
    }

    impl FakeEdgeDispatchRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                result_json: Some(r#"{"status":"completed"}"#.to_string()),
            }
        }

        fn pending_without_result() -> Self {
            Self {
                failed_column: None,
                result_json: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }
    }

    impl EdgeDispatchDbRow for FakeEdgeDispatchRow {
        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "pending_wait_us" => Ok(1_234),
                "pending_rows" => Ok(3),
                "dispatched_rows" => Ok(2),
                "oldest_pending_age_us" => Ok(1_500),
                "oldest_dispatched_age_us" => Ok(2_001),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            Ok(match column {
                "user_id" => "user-1",
                "session_id" => "session-1",
                "run_id" => "run-1",
                "turn_chain_id" => "turn-chain-1",
                "edge_agent_id" => "edge-1",
                "request_id" => "request-1",
                "payload_json" => r#"{"tool":"agent_fanout"}"#,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "result_json" => Ok(self.result_json.clone()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn claimed_dispatch_row_decode_preserves_values() {
        let row = decode_claimed_dispatch_row(&FakeEdgeDispatchRow::complete()).unwrap();

        assert_eq!(row.user_id, "user-1");
        assert_eq!(row.session_id, "session-1");
        assert_eq!(row.run_id, "run-1");
        assert_eq!(row.turn_chain_id, "turn-chain-1");
        assert_eq!(row.edge_agent_id, "edge-1");
        assert_eq!(row.request_id, "request-1");
        assert_eq!(row.payload_json, r#"{"tool":"agent_fanout"}"#);
        assert_eq!(
            row.result_json.as_deref(),
            Some(r#"{"status":"completed"}"#)
        );
        assert_eq!(row.status, "dispatched");
        assert_eq!(row.pending_wait_us, 1_234);
    }

    #[test]
    fn claimed_dispatch_row_decode_preserves_null_result_for_pending_rows() {
        let row = decode_claimed_dispatch_row(&FakeEdgeDispatchRow::pending_without_result())
            .expect("pending row with null result_json is valid");

        assert_eq!(row.result_json, None);
        assert_eq!(row.status, "dispatched");
    }

    #[test]
    fn claimed_dispatch_row_decode_fails_loudly_on_any_column_error() {
        for column in [
            "user_id",
            "session_id",
            "run_id",
            "turn_chain_id",
            "edge_agent_id",
            "request_id",
            "payload_json",
            "result_json",
            "pending_wait_us",
        ] {
            let error =
                decode_claimed_dispatch_row(&FakeEdgeDispatchRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("edge_dispatch poll row decode") && error.contains(column),
                "decode error should identify poll row column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn backlog_row_decode_preserves_counts_and_rounds_age_up_to_ms() {
        let row = decode_backlog_row(&FakeEdgeDispatchRow::complete()).unwrap();

        assert_eq!(
            row,
            EdgeDispatchBacklogRow {
                pending_rows: 3,
                dispatched_rows: 2,
                oldest_pending_age_ms: 2,
                oldest_dispatched_age_ms: 3,
            }
        );
    }

    #[test]
    fn backlog_row_decode_fails_loudly_on_any_column_error() {
        for column in [
            "pending_rows",
            "dispatched_rows",
            "oldest_pending_age_us",
            "oldest_dispatched_age_us",
        ] {
            let error = decode_backlog_row(&FakeEdgeDispatchRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("edge_dispatch backlog row decode") && error.contains(column),
                "backlog decode error should identify `{column}`: {error}"
            );
        }
    }

    #[test]
    fn apply_backlog_metrics_updates_cross_pod_backlog_gauges() {
        let metrics = crate::multi_agent::metrics::shared_metrics();

        apply_backlog_metrics(
            &metrics,
            EdgeDispatchBacklogRow {
                pending_rows: 7,
                dispatched_rows: 5,
                oldest_pending_age_ms: 11_000,
                oldest_dispatched_age_ms: 13_000,
            },
        );

        assert_eq!(metrics.dispatch_pending_rows.load(Ordering::Relaxed), 7);
        assert_eq!(metrics.dispatch_dispatched_rows.load(Ordering::Relaxed), 5);
        assert_eq!(
            metrics
                .dispatch_oldest_pending_age_ms
                .load(Ordering::Relaxed),
            11_000
        );
        assert_eq!(
            metrics
                .dispatch_oldest_dispatched_age_ms
                .load(Ordering::Relaxed),
            13_000
        );
    }

    #[test]
    fn backlog_metrics_sql_is_bounded_to_nonterminal_dispatch_rows() {
        assert!(EDGE_DISPATCH_BACKLOG_SQL.contains("status IN ('pending', 'dispatched')"));
        assert!(EDGE_DISPATCH_BACKLOG_SQL.contains("oldest_pending_age_us"));
        assert!(EDGE_DISPATCH_BACKLOG_SQL.contains("oldest_dispatched_age_us"));
    }

    #[test]
    fn failed_dispatch_result_json_uses_tool_error_status() {
        let result = edge_dispatch_failed_result_json(
            serde_json::json!("request-1"),
            "edge dispatch expired".to_string(),
        );
        let value: serde_json::Value =
            serde_json::from_str(&result).expect("failed result should be JSON");

        assert_eq!(value["request_id"], "request-1");
        assert_eq!(value["status"], "error");
        assert_eq!(value["output"], "edge dispatch expired");
        assert_eq!(value["duration_ms"], 0);
    }

    #[test]
    fn terminal_result_json_fails_loudly_when_missing_or_undecodable() {
        let missing = decode_terminal_result_json(&FakeEdgeDispatchRow::pending_without_result())
            .expect_err("terminal row must carry result_json");
        assert!(
            missing.contains("terminal row missing `result_json`"),
            "missing terminal result should be explicit: {missing}"
        );

        let decode_error =
            decode_terminal_result_json(&FakeEdgeDispatchRow::fail_on("result_json"))
                .expect_err("terminal result decode errors must not become None");
        assert!(
            decode_error.contains("edge_dispatch wait_result terminal row decode `result_json`"),
            "decode error should identify terminal result_json: {decode_error}"
        );
    }

    #[test]
    fn terminal_result_json_preserves_payload() {
        let result = decode_terminal_result_json(&FakeEdgeDispatchRow::complete()).unwrap();

        assert_eq!(result, r#"{"status":"completed"}"#);
    }

    #[test]
    fn claimed_dispatch_update_count_must_match_selected_rows() {
        validate_claimed_dispatch_update_count(2, 2).expect("matching counts are valid");

        let error = validate_claimed_dispatch_update_count(2, 1)
            .expect_err("claim/update mismatch must fail loudly");
        assert!(
            error.contains("claimed 2 rows but updated 1"),
            "error should identify claim/update mismatch: {error}"
        );
    }

    #[test]
    fn poll_claim_update_uses_matrixone_compatible_identity_predicates() {
        let source = include_str!("edge_dispatch.rs");
        let poll_body = source
            .split("impl EdgeDispatchService for DatabaseEdgeDispatchService")
            .nth(1)
            .and_then(|rest| {
                rest.split("#[tracing::instrument(skip(self, result_json)")
                    .next()
            })
            .expect("poll_pending body");
        assert!(
            !poll_body.contains("(user_id, request_id) IN")
                && !poll_body
                    .contains("(user_id, session_id, run_id, turn_chain_id, request_id) IN"),
            "MatrixOne rejects row-value IN predicates with bound tuple parameters"
        );
        assert!(
            poll_body.contains("WHERE status = 'pending' AND (")
                && poll_body.contains("(user_id = ")
                && poll_body.contains(" AND session_id = ")
                && poll_body.contains(" AND run_id = ")
                && poll_body.contains(" AND turn_chain_id = ")
                && poll_body.contains(" AND request_id = ")
                && poll_body.contains(" OR "),
            "poll claim update must use explicit turn-scoped identity predicates"
        );
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_dispatch_round_trip_survives_cross_pod_delivery() {
        let pool = setup_edge_dispatch_db_it().await;
        let pod_a = DatabaseEdgeDispatchService::from_shared(&pool);
        let pod_b = DatabaseEdgeDispatchService::from_shared(&pool);
        let pod_c = DatabaseEdgeDispatchService::from_shared(&pool);

        let user_id = format!("edge-user-{}", Uuid::new_v4());
        let other_user_id = format!("edge-other-{}", Uuid::new_v4());
        let edge_agent_id = format!("edge-agent-{}", Uuid::new_v4());
        let other_edge_agent_id = format!("edge-other-agent-{}", Uuid::new_v4());
        let request_id = format!("edge-req-{}", Uuid::new_v4());
        let identity = EdgeDispatchIdentity::new(
            &user_id,
            format!("session-{request_id}"),
            format!("run-{request_id}"),
            format!("chain-{request_id}"),
            &request_id,
        );
        let other_identity = EdgeDispatchIdentity::new(
            &other_user_id,
            identity.session_id.clone(),
            identity.run_id.clone(),
            identity.turn_chain_id.clone(),
            &request_id,
        );
        cleanup_edge_dispatch_fixture(&pool, &identity).await;
        cleanup_edge_dispatch_fixture(&pool, &other_identity).await;

        let payload = json!({
            "request_id": request_id,
            "tool": "bash",
            "args": {"cmd": "printf ok"}
        })
        .to_string();
        pod_a
            .insert_dispatch(&identity, &edge_agent_id, &payload)
            .await
            .expect("insert pending dispatch");
        pod_a
            .insert_dispatch(&identity, &edge_agent_id, &payload)
            .await
            .expect("duplicate insert should be idempotent");

        let wrong_agent_rows = pod_b
            .poll_pending(&user_id, &other_edge_agent_id)
            .await
            .expect("wrong edge agent poll");
        assert!(wrong_agent_rows.is_empty());

        assert!(
            !pod_c
                .deliver_result(
                    &other_identity,
                    &edge_agent_id,
                    r#"{"status":"completed","output":"wrong-user"}"#,
                )
                .await
                .expect("wrong owner deliver should not error")
        );
        assert!(
            !pod_c
                .deliver_result(
                    &identity,
                    &other_edge_agent_id,
                    r#"{"status":"completed","output":"wrong-agent"}"#,
                )
                .await
                .expect("wrong agent deliver should not error")
        );

        let claimed = pod_b
            .poll_pending(&user_id, &edge_agent_id)
            .await
            .expect("correct edge agent poll");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].user_id, user_id);
        assert_eq!(claimed[0].request_id, request_id);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&claimed[0].payload_json)
                .expect("claimed payload should be valid JSON"),
            serde_json::from_str::<serde_json::Value>(&payload).expect("payload should be JSON")
        );
        assert_eq!(claimed[0].status, "dispatched");
        assert!(
            pod_b
                .poll_pending(&user_id, &edge_agent_id)
                .await
                .expect("already claimed poll")
                .is_empty(),
            "claimed dispatch must not be re-claimed by another pod"
        );

        let wait_identity = identity.clone();
        let wait = tokio::spawn(async move {
            pod_a
                .wait_result(&wait_identity, std::time::Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result_json = json!({
            "request_id": request_id,
            "status": "completed",
            "output": "ok",
            "duration_ms": 12
        })
        .to_string();
        assert!(
            pod_c
                .deliver_result(&identity, &edge_agent_id, &result_json)
                .await
                .expect("cross-pod deliver result")
        );
        let waited = wait
            .await
            .expect("wait task should join")
            .expect("wait_result should not fail")
            .expect("wait_result should observe completed result");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&waited)
                .expect("waited result should be JSON"),
            serde_json::from_str::<serde_json::Value>(&result_json).expect("result should be JSON")
        );
        assert!(
            !pod_c
                .deliver_result(
                    &identity,
                    &edge_agent_id,
                    r#"{"status":"completed","output":"duplicate"}"#,
                )
                .await
                .expect("duplicate terminal deliver should not error"),
            "terminal result must not be overwritten"
        );

        let row = sqlx::query(
            "SELECT status, CAST(result_json AS CHAR) AS result_json
             FROM edge_pending_dispatch
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND request_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .fetch_one(pool.get())
        .await
        .expect("load terminal dispatch row");
        let status: String = row.try_get("status").expect("status");
        let stored_result: Option<String> = row.try_get("result_json").expect("result_json");
        assert_eq!(status, "completed");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                stored_result.as_deref().expect("stored result_json")
            )
            .expect("stored result_json should be JSON"),
            serde_json::from_str::<serde_json::Value>(&result_json).expect("result should be JSON")
        );

        cleanup_edge_dispatch_fixture(&pool, &identity).await;
        cleanup_edge_dispatch_fixture(&pool, &other_identity).await;
    }
}
