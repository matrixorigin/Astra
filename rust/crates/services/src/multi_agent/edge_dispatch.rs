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

#[derive(Debug)]
pub struct EdgeDispatchRow {
    pub dispatch_id: i64,
    pub user_id: String,
    pub edge_agent_id: String,
    pub request_id: String,
    pub payload_json: String,
    pub result_json: Option<String>,
    pub status: String,
}

async fn rollback_edge_dispatch_tx(tx: sqlx::Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "edge_dispatch rollback failed");
    }
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

    /// Deliver a tool result (from HTTP callback or WS) — updates status to 'completed'.
    /// `user_id` and `edge_agent_id` must match the dispatch record to prevent
    /// cross-owner or cross-agent injection.
    async fn deliver_result(
        &self,
        user_id: &str,
        request_id: &str,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String>;

    /// Move an in-flight dispatch to a failed terminal state.
    async fn fail_dispatch(
        &self,
        user_id: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<bool, String>;

    /// Poll for a specific request's result. Returns Some(result_json) when completed.
    async fn wait_result(
        &self,
        user_id: &str,
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

fn edge_dispatch_decode_error(context: &str, column: &'static str, error: sqlx::Error) -> String {
    format!("edge_dispatch {context} decode `{column}`: {error}")
}

fn decode_claimed_dispatch_row(row: &impl EdgeDispatchDbRow) -> Result<EdgeDispatchRow, String> {
    Ok(EdgeDispatchRow {
        dispatch_id: row
            .i64_column("dispatch_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "dispatch_id", e))?,
        user_id: row
            .string_column("user_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "user_id", e))?,
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
    })
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
    #[tracing::instrument(skip(self, payload_json), fields(user_id = %user_id, edge_agent_id = %edge_agent_id, request_id = %request_id))]
    async fn insert_dispatch(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        request_id: &str,
        payload_json: &str,
    ) -> Result<i64, String> {
        // Idempotent insert inside an owner boundary: the same request_id is
        // reusable by another user, but duplicate calls for the same user
        // return the existing dispatch_id instead of erroring.
        match sqlx::query(
            "INSERT INTO edge_pending_dispatch \
             (user_id, edge_agent_id, request_id, payload_json, status) \
             VALUES (?, ?, ?, ?, 'pending')",
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
                        "SELECT dispatch_id FROM edge_pending_dispatch WHERE user_id = ? AND request_id = ?",
                    )
                    .bind(user_id)
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
        let ids: Vec<i64> = claimed_rows.iter().map(|row| row.dispatch_id).collect();

        let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "UPDATE edge_pending_dispatch \
             SET status = 'dispatched', dispatched_at = NOW(6) \
             WHERE dispatch_id IN (",
        );
        let mut separated = update.separated(", ");
        for id in &ids {
            separated.push_bind(*id);
        }
        separated.push_unseparated(") AND status = 'pending'");
        let update_result = match update.build().execute(&mut *tx).await {
            Ok(result) => result,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "poll update").await;
                return Err(format!("edge_dispatch poll UPDATE: {e}"));
            }
        };
        if let Err(e) =
            validate_claimed_dispatch_update_count(ids.len(), update_result.rows_affected())
        {
            rollback_edge_dispatch_tx(tx, "validate claimed dispatch count").await;
            return Err(e);
        }

        tx.commit()
            .await
            .map_err(|e| format!("edge_dispatch poll commit: {e}"))?;

        Ok(claimed_rows)
    }

    #[tracing::instrument(skip(self, result_json), fields(request_id = %request_id, edge_agent_id = %edge_agent_id))]
    async fn deliver_result(
        &self,
        user_id: &str,
        request_id: &str,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let start = std::time::Instant::now();
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND request_id = ? AND edge_agent_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(user_id)
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
    async fn fail_dispatch(
        &self,
        user_id: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
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
             WHERE user_id = ? AND request_id = ? AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json)
        .bind(user_id)
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
        user_id: &str,
        request_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut iterations: u32 = 0;
        loop {
            let row = sqlx::query(
                "SELECT CAST(result_json AS CHAR) AS result_json, status FROM edge_pending_dispatch WHERE user_id = ? AND request_id = ?",
            )
            .bind(user_id)
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch wait_result: {e}"))?;

            match row {
                Some(r) => {
                    let status: String = r.try_get("status").map_err(|e| e.to_string())?;
                    match status.as_str() {
                        "completed" => {
                            return Ok(Some(decode_terminal_result_json(&r)?));
                        }
                        "failed" => {
                            return Ok(Some(decode_terminal_result_json(&r)?));
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
    async fn deliver_result(
        &self,
        _user_id: &str,
        _request_id: &str,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn fail_dispatch(
        &self,
        _user_id: &str,
        _request_id: &str,
        _reason: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn wait_result(
        &self,
        _user_id: &str,
        _request_id: &str,
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
        user_id: &str,
        request_id: &str,
    ) {
        sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ? AND request_id = ?")
            .bind(user_id)
            .bind(request_id)
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
                "dispatch_id" => Ok(42),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            Ok(match column {
                "user_id" => "user-1",
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

        assert_eq!(row.dispatch_id, 42);
        assert_eq!(row.user_id, "user-1");
        assert_eq!(row.edge_agent_id, "edge-1");
        assert_eq!(row.request_id, "request-1");
        assert_eq!(row.payload_json, r#"{"tool":"agent_fanout"}"#);
        assert_eq!(
            row.result_json.as_deref(),
            Some(r#"{"status":"completed"}"#)
        );
        assert_eq!(row.status, "dispatched");
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
            "dispatch_id",
            "user_id",
            "edge_agent_id",
            "request_id",
            "payload_json",
            "result_json",
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
        cleanup_edge_dispatch_fixture(&pool, &user_id, &request_id).await;
        cleanup_edge_dispatch_fixture(&pool, &other_user_id, &request_id).await;

        let payload = json!({
            "request_id": request_id,
            "tool": "bash",
            "args": {"cmd": "printf ok"}
        })
        .to_string();
        let dispatch_id = pod_a
            .insert_dispatch(&user_id, &edge_agent_id, &request_id, &payload)
            .await
            .expect("insert pending dispatch");
        let duplicate_dispatch_id = pod_a
            .insert_dispatch(&user_id, &edge_agent_id, &request_id, &payload)
            .await
            .expect("duplicate insert should be idempotent");
        assert_eq!(duplicate_dispatch_id, dispatch_id);

        let wrong_agent_rows = pod_b
            .poll_pending(&user_id, &other_edge_agent_id)
            .await
            .expect("wrong edge agent poll");
        assert!(wrong_agent_rows.is_empty());

        assert!(
            !pod_c
                .deliver_result(
                    &other_user_id,
                    &request_id,
                    &edge_agent_id,
                    r#"{"status":"completed","output":"wrong-user"}"#,
                )
                .await
                .expect("wrong owner deliver should not error")
        );
        assert!(
            !pod_c
                .deliver_result(
                    &user_id,
                    &request_id,
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
        assert_eq!(claimed[0].dispatch_id, dispatch_id);
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

        let wait_user_id = user_id.clone();
        let wait_request_id = request_id.clone();
        let wait = tokio::spawn(async move {
            pod_a
                .wait_result(
                    &wait_user_id,
                    &wait_request_id,
                    std::time::Duration::from_secs(5),
                )
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
                .deliver_result(&user_id, &request_id, &edge_agent_id, &result_json)
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
                    &user_id,
                    &request_id,
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
             WHERE user_id = ? AND request_id = ?",
        )
        .bind(&user_id)
        .bind(&request_id)
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

        cleanup_edge_dispatch_fixture(&pool, &user_id, &request_id).await;
        cleanup_edge_dispatch_fixture(&pool, &other_user_id, &request_id).await;
    }
}
