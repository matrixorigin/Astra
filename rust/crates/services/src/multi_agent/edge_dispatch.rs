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
use sqlx::Row;

use super::metrics::{SharedMultiAgentMetrics, saturating_decrement};

pub struct EdgeDispatchRow {
    pub dispatch_id: i64,
    pub user_id: String,
    pub edge_agent_id: String,
    pub request_id: String,
    pub payload_json: String,
    pub result_json: Option<String>,
    pub status: String,
}

#[async_trait]
pub trait EdgeDispatchService: Send + Sync {
    /// Insert a new pending dispatch. Returns the dispatch_id.
    async fn insert_dispatch(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        request_id: &str,
        payload_json: &str,
    ) -> Result<i64, String>;

    /// Poll for pending dispatches targeting the given (user, agent) pairs.
    /// Returns dispatches that are still 'pending' and not yet dispatched.
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String>;

    /// Mark dispatches as 'dispatched' (the edge WS has been sent).
    async fn mark_dispatched(&self, dispatch_ids: &[i64]) -> Result<(), String>;

    /// Deliver a tool result (from HTTP callback or WS) — updates status to 'completed'.
    /// `edge_agent_id` must match the dispatch record to prevent cross-user injection.
    async fn deliver_result(
        &self,
        request_id: &str,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String>;

    /// Move an in-flight dispatch to a failed terminal state.
    async fn fail_dispatch(&self, request_id: &str, reason: &str) -> Result<bool, String>;

    /// Poll for a specific request's result. Returns Some(result_json) when completed.
    async fn wait_result(
        &self,
        request_id: &str,
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

#[async_trait]
impl EdgeDispatchService for DatabaseEdgeDispatchService {
    #[tracing::instrument(skip(self, payload_json), fields(user_id = %user_id, edge_agent_id = %edge_agent_id, request_id = %request_id))]
    async fn insert_dispatch(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        request_id: &str,
        payload_json: &str,
    ) -> Result<i64, String> {
        // Idempotent insert: request_id has a UNIQUE constraint, so
        // duplicate calls return the existing dispatch_id instead of
        // erroring.
        match sqlx::query(
            "INSERT INTO edge_pending_dispatch (user_id, edge_agent_id, request_id, payload_json, status)              VALUES (?, ?, ?, ?, 'pending')",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .bind(request_id)
        .bind(payload_json)
        .execute(&self.pool)
        .await
        {
            Ok(r) => {
                if let Some(ref m) = self.metrics {
                    m.dispatch_queue_depth.fetch_add(1, Ordering::Relaxed);
                }
                Ok(r.last_insert_id() as i64)
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("1062") || msg.contains("Duplicate entry") {
                    // Race or retry: fetch the existing row.
                    let (existing_id,): (i64,) = sqlx::query_as(
                        "SELECT dispatch_id FROM edge_pending_dispatch WHERE request_id = ?",
                    )
                    .bind(request_id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| format!("edge_dispatch insert (fetch existing): {e}"))?;
                    Ok(existing_id)
                } else {
                    Err(format!("edge_dispatch insert: {e}"))
                }
            }
        }
    }
    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
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
            "SELECT dispatch_id, user_id, edge_agent_id, request_id, \
             CAST(payload_json AS CHAR) AS payload_json, \
             CAST(result_json AS CHAR) AS result_json, \
             status \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending' \
             ORDER BY dispatch_id ASC LIMIT 50 \
             FOR UPDATE",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(format!("edge_dispatch poll SELECT: {e}"));
            }
        };

        if rows.is_empty() {
            tx.commit()
                .await
                .map_err(|e| format!("edge_dispatch poll commit (no rows): {e}"))?;
            return Ok(vec![]);
        }

        // Mark claimed rows as dispatched within the same transaction
        let ids: Vec<i64> = rows
            .iter()
            .map(|r| {
                r.try_get::<i64, _>("dispatch_id")
                    .map_err(|e: sqlx::Error| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;

        let placeholders: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
        let update_sql = format!(
            "UPDATE edge_pending_dispatch \
             SET status = 'dispatched', dispatched_at = NOW(6) \
             WHERE dispatch_id IN ({}) AND status = 'pending'",
            placeholders.join(",")
        );
        let mut uq = sqlx::query(&update_sql);
        for id in &ids {
            uq = uq.bind(id);
        }
        if let Err(e) = uq.execute(&mut *tx).await {
            let _ = tx.rollback().await;
            return Err(format!("edge_dispatch poll UPDATE: {e}"));
        }

        tx.commit()
            .await
            .map_err(|e| format!("edge_dispatch poll commit: {e}"))?;

        rows.iter()
            .map(|r| {
                Ok(EdgeDispatchRow {
                    dispatch_id: r
                        .try_get("dispatch_id")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    user_id: r
                        .try_get("user_id")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    edge_agent_id: r
                        .try_get("edge_agent_id")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    request_id: r
                        .try_get("request_id")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    payload_json: r
                        .try_get("payload_json")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    result_json: r.try_get("result_json").ok(),
                    status: "dispatched".to_string(),
                })
            })
            .collect()
    }

    async fn mark_dispatched(&self, dispatch_ids: &[i64]) -> Result<(), String> {
        if dispatch_ids.is_empty() {
            return Ok(());
        }
        // Safety belt: only mark rows still in 'pending' state.
        // The primary claiming happens atomically inside poll_pending(),
        // but this method remains for callers that bypass poll.
        let placeholders: Vec<String> = dispatch_ids.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "UPDATE edge_pending_dispatch SET status = 'dispatched', dispatched_at = NOW(6) \
             WHERE dispatch_id IN ({}) AND status = 'pending'",
            placeholders.join(",")
        );
        let mut q = sqlx::query(&sql);
        for id in dispatch_ids {
            q = q.bind(id);
        }
        q.execute(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch mark_dispatched: {e}"))?;
        Ok(())
    }

    #[tracing::instrument(skip(self, result_json), fields(request_id = %request_id, edge_agent_id = %edge_agent_id))]
    async fn deliver_result(
        &self,
        request_id: &str,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let start = std::time::Instant::now();
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE request_id = ? AND edge_agent_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(request_id)
        .bind(edge_agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch deliver_result: {e}"))?;
        let affected = n.rows_affected() > 0;
        if affected && let Some(ref m) = self.metrics {
            saturating_decrement(&m.dispatch_queue_depth);
            m.dispatch_latency.record(start.elapsed());
        }
        Ok(affected)
    }

    #[tracing::instrument(skip(self), fields(request_id = %request_id, reason = %reason))]
    async fn fail_dispatch(&self, request_id: &str, reason: &str) -> Result<bool, String> {
        let output = format!("edge dispatch {reason}");
        let result_json = serde_json::json!({
            "request_id": request_id,
            "status": "failed",
            "output": output,
            "duration_ms": 0,
        })
        .to_string();
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE request_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(request_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch fail_dispatch: {e}"))?;
        let affected = n.rows_affected() > 0;
        if affected && let Some(ref m) = self.metrics {
            saturating_decrement(&m.dispatch_queue_depth);
        }
        Ok(affected)
    }

    #[tracing::instrument(skip(self), fields(request_id = %request_id, timeout_ms = timeout.as_millis()))]
    async fn wait_result(
        &self,
        request_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut iterations: u32 = 0;
        loop {
            let row = sqlx::query(
                "SELECT CAST(result_json AS CHAR) AS result_json, status FROM edge_pending_dispatch WHERE request_id = ?",
            )
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch wait_result: {e}"))?;

            match row {
                Some(r) => {
                    let status: String = r.try_get("status").map_err(|e| e.to_string())?;
                    match status.as_str() {
                        "completed" => {
                            let result: Option<String> = r.try_get("result_json").ok();
                            return Ok(result);
                        }
                        "failed" => {
                            let result: Option<String> = r.try_get("result_json").ok();
                            return Ok(result);
                        }
                        _ => {} // still pending or dispatched
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
        let expired_result_json = serde_json::json!({
            "request_id": null,
            "status": "failed",
            "output": "edge dispatch expired",
            "duration_ms": 0,
        })
        .to_string();
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
        Ok(expired + deleted.rows_affected())
    }
}

pub struct UnconfiguredEdgeDispatchService;

#[async_trait]
impl EdgeDispatchService for UnconfiguredEdgeDispatchService {
    async fn insert_dispatch(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _request_id: &str,
        _payload_json: &str,
    ) -> Result<i64, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn mark_dispatched(&self, _dispatch_ids: &[i64]) -> Result<(), String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn deliver_result(
        &self,
        _request_id: &str,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn fail_dispatch(&self, _request_id: &str, _reason: &str) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn wait_result(
        &self,
        _request_id: &str,
        _timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
        Err("edge dispatch service not configured".to_string())
    }
}
