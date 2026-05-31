//! Edge dispatch relay: cross-pod tool dispatch via DB-backed queue.
//!
//! When the in-memory edge ledger times out or a tool targets an agent connected
//! to a different pod, the dispatch relay persists the request and the
//! owning pod's edge WS handler polls it for delivery.  Results flow back
//! through `deliver_result`.
//!
//! Split from the monolithic `multi_agent.rs`.

use async_trait::async_trait;
use sqlx::Row;

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
    async fn deliver_result(&self, request_id: &str, result_json: &str) -> Result<bool, String>;

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
}

impl DatabaseEdgeDispatchService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
        }
    }
}

#[async_trait]
impl EdgeDispatchService for DatabaseEdgeDispatchService {
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
            Ok(r) => Ok(r.last_insert_id() as i64),
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
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        let rows = sqlx::query(
            "SELECT dispatch_id, user_id, edge_agent_id, request_id, \
             CAST(payload_json AS CHAR) AS payload_json, \
             CAST(result_json AS CHAR) AS result_json, \
             status \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending' \
             ORDER BY dispatch_id ASC LIMIT 50",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch poll: {e}"))?;

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
                    status: r
                        .try_get("status")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                })
            })
            .collect()
    }

    async fn mark_dispatched(&self, dispatch_ids: &[i64]) -> Result<(), String> {
        if dispatch_ids.is_empty() {
            return Ok(());
        }
        // Build IN clause
        let placeholders: Vec<String> = dispatch_ids.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "UPDATE edge_pending_dispatch SET status = 'dispatched', dispatched_at = NOW(6) \
             WHERE dispatch_id IN ({})",
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

    async fn deliver_result(&self, request_id: &str, result_json: &str) -> Result<bool, String> {
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE request_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(request_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch deliver_result: {e}"))?;
        Ok(n.rows_affected() > 0)
    }

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
                        "failed" => return Ok(None),
                        _ => {} // still pending or dispatched
                    }
                }
                None => return Ok(None), // request not found
            }

            if tokio::time::Instant::now() >= deadline {
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
        let n = sqlx::query(
            "DELETE FROM edge_pending_dispatch \
             WHERE created_at <= DATE_SUB(NOW(6), INTERVAL ? SECOND) \
             AND status IN ('completed', 'failed')",
        )
        .bind(older_than.as_secs() as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch cleanup: {e}"))?;
        Ok(n.rows_affected())
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
    async fn deliver_result(&self, _request_id: &str, _result_json: &str) -> Result<bool, String> {
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
