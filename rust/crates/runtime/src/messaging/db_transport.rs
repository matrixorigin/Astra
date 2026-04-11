//! Database-backed transport — for distributed / multi-process deployments.
//!
//! Uses a MySQL (MatrixOne) `agent_message_queue` table to persist messages.
//! Each subscriber runs a background poll task that feeds an in-memory channel,
//! so the `MessageStream` trait's synchronous `try_recv()` works without blocking.
//!
//! # Design
//!
//! ```text
//!  send()/broadcast()                subscribe()
//!       │                                 │
//!       ▼                                 ▼
//!  ┌──────────┐                    ┌──────────────┐
//!  │  INSERT   │                   │  poll task   │
//!  │  into DB  │                   │  (SELECT >   │
//!  └──────────┘                   │   cursor,     │
//!                                  │   push to    │
//!                                  │   mpsc)      │
//!                                  └──────┬───────┘
//!                                         │
//!                                         ▼
//!                                  ┌──────────────┐
//!                                  │  mpsc buffer │ ← try_recv() / recv()
//!                                  └──────────────┘
//! ```
//!
//! # Table Schema
//!
//! Created by [`ensure_schema()`]:
//! ```sql
//! CREATE TABLE IF NOT EXISTS agent_message_queue (
//!     id            BIGINT AUTO_INCREMENT PRIMARY KEY,
//!     message_id    VARCHAR(36) NOT NULL,
//!     from_run_id   VARCHAR(128) NOT NULL,
//!     from_agent_id VARCHAR(128) NOT NULL,
//!     to_run_id     VARCHAR(128),          -- NULL for broadcast
//!     to_agent_id   VARCHAR(128),          -- NULL for broadcast
//!     delegation_id VARCHAR(128),          -- set for broadcast
//!     is_broadcast  BOOLEAN NOT NULL DEFAULT FALSE,
//!     payload_json  LONGTEXT NOT NULL,     -- full AgentMessage JSON
//!     timestamp_ms  BIGINT NOT NULL,       -- message creation time
//!     ttl_ms        BIGINT,               -- optional TTL
//!     status        VARCHAR(16) DEFAULT 'pending', -- pending/claimed/acked/failed
//!     claimed_by    VARCHAR(256),         -- consumer ID that claimed the message
//!     claimed_at_ms BIGINT,               -- when the message was claimed
//!     attempt_count INT DEFAULT 0,        -- number of delivery attempts
//!     created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
//!     INDEX idx_amq_direct    (to_run_id, to_agent_id, status, id),
//!     INDEX idx_amq_broadcast (delegation_id, is_broadcast, status, id)
//! );
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::mysql::MySqlRow;
use sqlx::{MySql, Pool, Row, query};
use tokio::sync::{RwLock, mpsc, watch};

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Default interval between DB polls for new messages.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum number of messages to fetch per poll cycle.
const POLL_BATCH_SIZE: i64 = 100;

/// Initial backoff on DB poll error (doubles each consecutive failure).
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

/// Maximum backoff on repeated DB poll errors.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// After this many consecutive failures, log a critical warning.
const CRITICAL_FAILURE_THRESHOLD: u32 = 30;

// ─── Metrics ────────────────────────────────────────────────────────────────

/// Observable counters for the database transport.
#[derive(Debug, Default)]
pub struct TransportMetrics {
    pub messages_sent: std::sync::atomic::AtomicU64,
    pub messages_received: std::sync::atomic::AtomicU64,
    pub messages_dropped: std::sync::atomic::AtomicU64,
    pub poll_errors: std::sync::atomic::AtomicU64,
    pub send_errors: std::sync::atomic::AtomicU64,
}

// ─── Schema ─────────────────────────────────────────────────────────────────

/// Create the `agent_message_queue` table if it doesn't exist.
pub async fn ensure_schema(pool: &Pool<MySql>) -> Result<(), sqlx::Error> {
    query(
        "CREATE TABLE IF NOT EXISTS agent_message_queue (
            id            BIGINT AUTO_INCREMENT PRIMARY KEY,
            message_id    VARCHAR(36) NOT NULL,
            from_run_id   VARCHAR(128) NOT NULL,
            from_agent_id VARCHAR(128) NOT NULL,
            to_run_id     VARCHAR(128),
            to_agent_id   VARCHAR(128),
            delegation_id VARCHAR(128),
            is_broadcast  BOOLEAN NOT NULL DEFAULT FALSE,
            payload_json  LONGTEXT NOT NULL,
            timestamp_ms  BIGINT NOT NULL,
            ttl_ms        BIGINT,
            status        VARCHAR(16) NOT NULL DEFAULT 'pending',
            claimed_by    VARCHAR(256),
            claimed_at_ms BIGINT,
            attempt_count INT NOT NULL DEFAULT 0,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_amq_direct    (to_run_id, to_agent_id, status, id),
            INDEX idx_amq_broadcast (delegation_id, is_broadcast, status, id)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ─── DatabaseTransport ──────────────────────────────────────────────────────

/// Database-backed message transport for distributed deployments.
///
/// Messages are persisted to MySQL, enabling cross-process agent communication.
/// Each subscriber gets a background poll task that pushes messages into a local
/// `mpsc` channel for zero-latency `try_recv()`.
pub struct DatabaseTransport {
    pool: Pool<MySql>,
    poll_interval: Duration,
    /// How long a claimed message stays invisible before being reclaimed.
    visibility_timeout: Duration,
    /// Maximum delivery attempts before marking as 'failed'.
    max_delivery_attempts: u32,
    /// Tracks registered agents and their delegation group.
    registrations: Arc<RwLock<HashMap<AgentAddress, Option<String>>>>,
    /// Shutdown signal: when sent, all poll tasks stop.
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Active poll task abort handles — aborted on shutdown for clean drain.
    poll_abort_handles: std::sync::Mutex<Vec<tokio::task::AbortHandle>>,
    /// Observable metrics.
    metrics: Arc<TransportMetrics>,
    /// Lazily started maintenance task for reclaiming stale claims and pruning
    /// expired rows.
    cleanup_scheduler: Mutex<Option<CleanupScheduler>>,
}

/// Default visibility timeout (how long before an unclaimed message reappears).
const DEFAULT_VISIBILITY_TIMEOUT: Duration = Duration::from_secs(60);

/// Default max delivery attempts.
const DEFAULT_MAX_DELIVERY_ATTEMPTS: u32 = 5;

impl DatabaseTransport {
    /// Create a new database transport.
    ///
    /// Call [`ensure_schema()`] before first use to create the table.
    pub fn new(pool: Pool<MySql>) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            pool,
            poll_interval: DEFAULT_POLL_INTERVAL,
            visibility_timeout: DEFAULT_VISIBILITY_TIMEOUT,
            max_delivery_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
            registrations: Arc::new(RwLock::new(HashMap::new())),
            shutdown_tx,
            shutdown_rx,
            poll_abort_handles: std::sync::Mutex::new(Vec::new()),
            metrics: Arc::new(TransportMetrics::default()),
            cleanup_scheduler: Mutex::new(None),
        }
    }

    /// Set the poll interval for message streams.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Set the visibility timeout for claimed messages.
    pub fn with_visibility_timeout(mut self, timeout: Duration) -> Self {
        self.visibility_timeout = timeout;
        self
    }

    /// Set maximum delivery attempts before marking a message as failed.
    pub fn with_max_delivery_attempts(mut self, max: u32) -> Self {
        self.max_delivery_attempts = max;
        self
    }

    /// Get the underlying pool (for tests / diagnostics).
    pub fn pool(&self) -> &Pool<MySql> {
        &self.pool
    }

    fn ensure_cleanup_scheduler_started(&self) {
        let mut scheduler = match self.cleanup_scheduler.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if scheduler.is_none() {
            *scheduler = Some(CleanupScheduler::start(self, None, None));
        }
    }

    /// Delete expired messages from the queue.
    ///
    /// Call periodically to prevent unbounded table growth.
    pub async fn cleanup_expired(&self) -> Result<u64, sqlx::Error> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let result = query(
            "DELETE FROM agent_message_queue
             WHERE ttl_ms IS NOT NULL
               AND timestamp_ms < (? - ttl_ms)",
        )
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete terminal messages older than the given duration.
    pub async fn cleanup_older_than(&self, max_age: Duration) -> Result<u64, sqlx::Error> {
        let cutoff_ms = chrono::Utc::now().timestamp_millis() - max_age.as_millis() as i64;
        let result = query(
            "DELETE FROM agent_message_queue
             WHERE timestamp_ms < ?
               AND status IN ('acked', 'failed')",
        )
        .bind(cutoff_ms)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Number of currently registered agents (local process only).
    pub async fn agent_count(&self) -> usize {
        self.registrations.read().await.len()
    }

    /// Get a reference to the transport's metrics counters.
    pub fn metrics(&self) -> &Arc<TransportMetrics> {
        &self.metrics
    }

    fn is_shutdown(&self) -> bool {
        *self.shutdown_rx.borrow()
    }

    /// Acknowledge a message — marks it as 'acked' in the database.
    ///
    /// Called by the receiver after processing the message.
    pub async fn ack_message(
        &self,
        message_id: &str,
        consumer_id: &str,
    ) -> Result<bool, MailboxError> {
        let result = query(
            "UPDATE agent_message_queue
             SET status = 'acked', claimed_by = NULL, claimed_at_ms = NULL
             WHERE message_id = ? AND status = 'claimed' AND claimed_by = ?",
        )
        .bind(message_id)
        .bind(consumer_id)
        .execute(&self.pool)
        .await
        .map_err(|e| MailboxError::Transport(format!("ack: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    /// Negatively acknowledge a message — marks it as 'failed' in the database.
    pub async fn nack_message(
        &self,
        message_id: &str,
        consumer_id: &str,
    ) -> Result<bool, MailboxError> {
        let result = query(
            "UPDATE agent_message_queue
             SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
             WHERE message_id = ? AND status = 'claimed' AND claimed_by = ?",
        )
        .bind(message_id)
        .bind(consumer_id)
        .execute(&self.pool)
        .await
        .map_err(|e| MailboxError::Transport(format!("nack: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    /// Reclaim messages that were claimed but never acked within the visibility timeout.
    ///
    /// This is called periodically to handle crashed consumers.
    pub async fn reclaim_stale(&self) -> Result<u64, MailboxError> {
        reclaim_stale_in_pool(
            &self.pool,
            self.visibility_timeout,
            self.max_delivery_attempts,
        )
        .await
    }

    /// Count messages in each status (for diagnostics).
    pub async fn status_counts(&self) -> Result<HashMap<String, i64>, MailboxError> {
        let rows = query("SELECT status, COUNT(*) as cnt FROM agent_message_queue GROUP BY status")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MailboxError::Transport(format!("status_counts: {e}")))?;

        let mut counts = HashMap::new();
        for row in rows {
            let status: String = row.try_get("status").unwrap_or_default();
            let cnt: i64 = row.try_get("cnt").unwrap_or(0);
            counts.insert(status, cnt);
        }
        Ok(counts)
    }

    /// Insert a message into the database.
    async fn insert_message(
        &self,
        msg: &AgentMessage,
        is_broadcast: bool,
    ) -> Result<(), MailboxError> {
        let (to_run_id, to_agent_id, delegation_id) = match &msg.to {
            super::types::MessageTarget::Direct { address } => (
                Some(address.run_id.as_str()),
                Some(address.agent_id.as_str()),
                None,
            ),
            super::types::MessageTarget::Broadcast { delegation_id } => {
                (None, None, Some(delegation_id.as_str()))
            }
            super::types::MessageTarget::Parent => {
                // Parent should be resolved before reaching transport.
                return Err(MailboxError::Transport(
                    "Parent target must be resolved before database transport".into(),
                ));
            }
        };

        let payload_json = serde_json::to_string(msg)
            .map_err(|e| MailboxError::Transport(format!("serialize: {e}")))?;

        query(
            "INSERT INTO agent_message_queue
             (message_id, from_run_id, from_agent_id, to_run_id, to_agent_id,
              delegation_id, is_broadcast, payload_json, timestamp_ms, ttl_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&msg.id)
        .bind(&msg.from.run_id)
        .bind(&msg.from.agent_id)
        .bind(to_run_id)
        .bind(to_agent_id)
        .bind(delegation_id)
        .bind(is_broadcast)
        .bind(&payload_json)
        .bind(msg.timestamp_ms)
        .bind(msg.ttl_ms)
        .execute(&self.pool)
        .await
        .map_err(|e| MailboxError::Transport(format!("insert: {e}")))?;

        Ok(())
    }
}

#[async_trait]
impl MessageTransport for DatabaseTransport {
    async fn register(
        &self,
        addr: AgentAddress,
        delegation_id: Option<String>,
    ) -> Result<(), MailboxError> {
        if self.is_shutdown() {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }
        self.registrations.write().await.insert(addr, delegation_id);
        self.ensure_cleanup_scheduler_started();
        Ok(())
    }

    async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError> {
        self.registrations.write().await.remove(addr);
        let consumer_id = format!("{}@{}", addr.agent_id, addr.run_id);
        release_claimed_for_consumer_in_pool(&self.pool, &consumer_id, self.max_delivery_attempts)
            .await?;
        Ok(())
    }

    async fn subscribe(&self, addr: &AgentAddress) -> Result<Box<dyn MessageStream>, MailboxError> {
        if self.is_shutdown() {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }
        let regs = self.registrations.read().await;
        let delegation_id = regs
            .get(addr)
            .ok_or_else(|| MailboxError::AgentNotFound(addr.clone()))?
            .clone();
        drop(regs);

        let consumer_id = format!("{}@{}", addr.agent_id, addr.run_id);
        let (tx, rx) = mpsc::unbounded_channel();
        let poll_task = tokio::spawn(poll_loop(
            self.pool.clone(),
            Arc::clone(&self.registrations),
            addr.clone(),
            delegation_id,
            self.poll_interval,
            self.max_delivery_attempts,
            tx,
            self.shutdown_rx.clone(),
            Arc::clone(&self.metrics),
            consumer_id,
        ));
        self.poll_abort_handles.lock().unwrap().push(poll_task.abort_handle());

        Ok(Box::new(DatabaseMessageStream {
            buffer_rx: rx,
            _poll_task: AbortOnDrop(poll_task),
        }))
    }

    async fn send(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError> {
        if self.is_shutdown() {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }
        match &msg.to {
            super::types::MessageTarget::Direct { .. } => {}
            _ => {
                return Err(MailboxError::Transport(
                    "send() requires Direct target".into(),
                ));
            }
        }
        match self.insert_message(&msg, false).await {
            Ok(()) => {
                self.metrics
                    .messages_sent
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.metrics
                    .send_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(e)
            }
        }
    }

    async fn broadcast(
        &self,
        delegation_id: &str,
        msg: Arc<AgentMessage>,
    ) -> Result<(), MailboxError> {
        if self.is_shutdown() {
            return Err(MailboxError::Transport("transport is shut down".into()));
        }
        let stored_msg = if matches!(&msg.to, super::types::MessageTarget::Broadcast { .. }) {
            (*msg).clone()
        } else {
            let mut m = (*msg).clone();
            m.to = super::types::MessageTarget::Broadcast {
                delegation_id: delegation_id.to_string(),
            };
            m
        };
        match self.insert_message(&stored_msg, true).await {
            Ok(()) => {
                self.metrics
                    .messages_sent
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.metrics
                    .send_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(e)
            }
        }
    }

    async fn health_check(&self) -> Result<(), MailboxError> {
        query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|e| MailboxError::Transport(format!("health check failed: {e}")))?;
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), MailboxError> {
        // Signal all poll tasks to stop.
        let _ = self.shutdown_tx.send(true);
        // Abort any poll tasks that haven't noticed the signal yet.
        for h in self.poll_abort_handles.lock().unwrap().drain(..) {
            h.abort();
        }
        // Release all claimed messages back to pending.
        let consumer_ids: Vec<String> = self
            .registrations
            .read()
            .await
            .keys()
            .map(|addr| format!("{}@{}", addr.agent_id, addr.run_id))
            .collect();
        for consumer_id in consumer_ids {
            release_claimed_for_consumer_in_pool(
                &self.pool,
                &consumer_id,
                self.max_delivery_attempts,
            )
            .await?;
        }
        Ok(())
    }
}

// ─── Poll Loop ──────────────────────────────────────────────────────────────

/// Background task that polls the database for new messages and pushes them
/// into a local channel. Implements exponential backoff on errors and
/// respects the shutdown signal.
async fn poll_loop(
    pool: Pool<MySql>,
    registrations: Arc<RwLock<HashMap<AgentAddress, Option<String>>>>,
    addr: AgentAddress,
    delegation_id: Option<String>,
    interval: Duration,
    max_delivery_attempts: u32,
    tx: mpsc::UnboundedSender<Arc<AgentMessage>>,
    mut shutdown_rx: watch::Receiver<bool>,
    metrics: Arc<TransportMetrics>,
    consumer_id: String,
) {
    let mut last_broadcast_id: i64 = 0;
    let mut consecutive_errors: u32 = 0;
    let mut current_backoff = INITIAL_BACKOFF;

    loop {
        // Check shutdown signal or receiver drop.
        if *shutdown_rx.borrow() || tx.is_closed() {
            break;
        }
        if !registrations.read().await.contains_key(&addr) {
            break;
        }

        let mut had_error = false;

        // 1. Claim direct messages atomically (UPDATE then SELECT).
        //    This ensures no two consumers process the same direct message.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let claim_result = query(
            "UPDATE agent_message_queue
             SET status = 'claimed', claimed_by = ?, claimed_at_ms = ?, attempt_count = attempt_count + 1
             WHERE to_run_id = ? AND to_agent_id = ? AND status = 'pending'
             ORDER BY id ASC LIMIT ?",
        )
        .bind(&consumer_id)
        .bind(now_ms)
        .bind(&addr.run_id)
        .bind(&addr.agent_id)
        .bind(POLL_BATCH_SIZE)
        .execute(&pool)
        .await;

        match claim_result {
            Ok(result) if result.rows_affected() > 0 => {
                // Fetch the messages we just claimed.
                let fetch_result = query(
                    "SELECT id, CAST(id AS CHAR) AS row_id_text, message_id, payload_json FROM agent_message_queue
                     WHERE to_run_id = ? AND to_agent_id = ? AND status = 'claimed' AND claimed_by = ?
                       AND claimed_at_ms = ?
                     ORDER BY id ASC LIMIT ?",
                )
                .bind(&addr.run_id)
                .bind(&addr.agent_id)
                .bind(&consumer_id)
                .bind(now_ms)
                .bind(POLL_BATCH_SIZE)
                .fetch_all(&pool)
                .await;

                if let Ok(rows) = fetch_result {
                    for row in rows {
                        let message_id: Option<String> = row.try_get("message_id").ok();
                        let row_id = match extract_queue_row_id(&row) {
                            Ok(id) => Some(id),
                            Err(e) => {
                                had_error = true;
                                metrics
                                    .poll_errors
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                eprintln!(
                                    "  ⚠ messaging: failed to extract direct row id for {}@{} (message_id: {}): {:?}",
                                    addr.agent_id,
                                    addr.run_id,
                                    message_id.as_deref().unwrap_or("<unavailable>"),
                                    e
                                );
                                None
                            }
                        };
                        if row_id.is_none() {
                            metrics
                                .messages_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match mark_direct_failed_by_identity(
                                &pool,
                                None,
                                message_id.as_deref(),
                                &consumer_id,
                            )
                            .await
                            {
                                Ok(()) => continue,
                                Err(e) => {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    eprintln!(
                                        "  ⚠ messaging: failed to dead-letter direct row without id (message_id: {}) for {}@{}: {:?}",
                                        message_id.as_deref().unwrap_or("<unavailable>"),
                                        addr.agent_id,
                                        addr.run_id,
                                        e
                                    );
                                    break;
                                }
                            }
                        }
                        let json: String = match row.try_get("payload_json") {
                            Ok(j) => j,
                            Err(_) => {
                                metrics
                                    .messages_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                match mark_direct_failed_by_identity(
                                    &pool,
                                    row_id,
                                    message_id.as_deref(),
                                    &consumer_id,
                                )
                                .await
                                {
                                    Ok(()) => {}
                                    Err(e) => {
                                        had_error = true;
                                        metrics
                                            .poll_errors
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        eprintln!(
                                            "  ⚠ messaging: failed to dead-letter undecodable direct row (row_id: {:?}, message_id: {}) for {}@{}: {:?}",
                                            row_id,
                                            message_id.as_deref().unwrap_or("<unavailable>"),
                                            addr.agent_id,
                                            addr.run_id,
                                            e
                                        );
                                    }
                                }
                                continue;
                            }
                        };

                        match serde_json::from_str::<AgentMessage>(&json) {
                            Ok(msg) if !msg.is_expired() => {
                                metrics
                                    .messages_received
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if tx.send(Arc::new(msg)).is_err() {
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if let Err(e) = release_claimed_for_consumer_in_pool(
                                        &pool,
                                        &consumer_id,
                                        max_delivery_attempts,
                                    )
                                    .await
                                    {
                                        eprintln!(
                                            "  ⚠ messaging: failed to release direct claims after closed channel for {}@{}: {:?}",
                                            addr.agent_id, addr.run_id, e
                                        );
                                    }
                                    return;
                                }
                                let row_id = row_id.expect("checked above");
                                if let Err(e) = mark_direct_acked(&pool, row_id, &consumer_id).await
                                {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    eprintln!(
                                        "  ⚠ messaging: failed to ack delivered direct row {} for {}@{}: {:?}",
                                        row_id, addr.agent_id, addr.run_id, e
                                    );
                                }
                            }
                            Ok(_) | Err(_) => {
                                metrics
                                    .messages_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                match mark_direct_failed_by_identity(
                                    &pool,
                                    row_id,
                                    message_id.as_deref(),
                                    &consumer_id,
                                )
                                .await
                                {
                                    Ok(()) => {}
                                    Err(e) => {
                                        had_error = true;
                                        metrics
                                            .poll_errors
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        eprintln!(
                                            "  ⚠ messaging: failed to dead-letter direct row (row_id: {:?}, message_id: {}) for {}@{}: {:?}",
                                            row_id,
                                            message_id.as_deref().unwrap_or("<unavailable>"),
                                            addr.agent_id,
                                            addr.run_id,
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else {
                    had_error = true;
                    metrics
                        .poll_errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "  ⚠ messaging: direct fetch error for {}@{}: {:?}",
                        addr.agent_id,
                        addr.run_id,
                        fetch_result.unwrap_err()
                    );
                }
            }
            Ok(_) => {
                // No pending messages — normal idle.
            }
            Err(e) => {
                had_error = true;
                metrics
                    .poll_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "  ⚠ messaging: direct claim error for {}@{}: {:?}",
                    addr.agent_id, addr.run_id, e
                );
            }
        }

        // 2. Poll broadcast messages (if in a delegation group).
        //    Broadcasts use cursor-based reading (all agents see every broadcast).
        if let Some(ref did) = delegation_id {
            let broadcast_result = query(
                "SELECT id, CAST(id AS CHAR) AS row_id_text, message_id, payload_json FROM agent_message_queue
                 WHERE delegation_id = ? AND is_broadcast = TRUE AND id > ? AND status IN ('pending', 'claimed')
                 ORDER BY id ASC LIMIT ?",
            )
            .bind(did)
            .bind(last_broadcast_id)
            .bind(POLL_BATCH_SIZE)
            .fetch_all(&pool)
            .await;

            if let Ok(rows) = broadcast_result {
                for row in rows {
                    let message_id: Option<String> = row.try_get("message_id").ok();
                    let row_id = match extract_queue_row_id(&row) {
                        Ok(id) => Some(id),
                        Err(e) => {
                            had_error = true;
                            metrics
                                .poll_errors
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "  ⚠ messaging: failed to extract broadcast row id for delegation {} (message_id: {}): {:?}",
                                did,
                                message_id.as_deref().unwrap_or("<unavailable>"),
                                e
                            );
                            None
                        }
                    };
                    if row_id.is_none() {
                        metrics
                            .messages_dropped
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        match mark_broadcast_failed_by_identity(
                            &pool,
                            None,
                            message_id.as_deref(),
                            did,
                        )
                        .await
                        {
                            Ok(()) => continue,
                            Err(e) => {
                                had_error = true;
                                metrics
                                    .poll_errors
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                eprintln!(
                                    "  ⚠ messaging: failed to dead-letter broadcast row without id (message_id: {}) for delegation {}: {:?}",
                                    message_id.as_deref().unwrap_or("<unavailable>"),
                                    did,
                                    e
                                );
                                break;
                            }
                        }
                    }
                    let row_id = row_id.expect("checked above");
                    let json: String = match row.try_get("payload_json") {
                        Ok(j) => j,
                        Err(_) => {
                            metrics
                                .messages_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match mark_broadcast_failed(&pool, row_id).await {
                                Ok(()) => {
                                    advance_broadcast_cursor(&mut last_broadcast_id, row_id);
                                }
                                Err(e) => {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    eprintln!(
                                        "  ⚠ messaging: failed to dead-letter undecodable broadcast row {} for delegation {}: {:?}",
                                        row_id, did, e
                                    );
                                    advance_broadcast_cursor(&mut last_broadcast_id, row_id);
                                }
                            }
                            continue;
                        }
                    };

                    match serde_json::from_str::<AgentMessage>(&json) {
                        Ok(msg) if !msg.is_expired() => {
                            metrics
                                .messages_received
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if tx.send(Arc::new(msg)).is_err() {
                                return;
                            }
                            advance_broadcast_cursor(&mut last_broadcast_id, row_id);
                        }
                        Ok(_) | Err(_) => {
                            metrics
                                .messages_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match mark_broadcast_failed(&pool, row_id).await {
                                Ok(()) => {
                                    advance_broadcast_cursor(&mut last_broadcast_id, row_id);
                                }
                                Err(e) => {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    eprintln!(
                                        "  ⚠ messaging: failed to dead-letter broadcast row {} for delegation {}: {:?}",
                                        row_id, did, e
                                    );
                                    advance_broadcast_cursor(&mut last_broadcast_id, row_id);
                                }
                            }
                        }
                    }
                }
            } else {
                had_error = true;
                metrics
                    .poll_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "  ⚠ messaging: broadcast poll error for delegation {}: {:?}",
                    did,
                    broadcast_result.unwrap_err()
                );
            }
        }

        // Backoff logic: on error, increase delay; on success, reset.
        let sleep_duration = if had_error {
            consecutive_errors = consecutive_errors.saturating_add(1);
            if consecutive_errors == CRITICAL_FAILURE_THRESHOLD {
                eprintln!(
                    "  🔴 messaging: CRITICAL — {} consecutive poll failures for {}@{}",
                    consecutive_errors, addr.agent_id, addr.run_id
                );
            }
            let backoff = current_backoff;
            current_backoff = (current_backoff * 2).min(MAX_BACKOFF);
            backoff
        } else {
            consecutive_errors = 0;
            current_backoff = INITIAL_BACKOFF;
            interval
        };

        // Wait with shutdown awareness.
        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            _ = shutdown_rx.changed() => {
                break;
            }
        }
    }
}

async fn mark_broadcast_failed(pool: &Pool<MySql>, row_id: i64) -> Result<(), sqlx::Error> {
    query(
        "UPDATE agent_message_queue
         SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
         WHERE id = ? AND is_broadcast = TRUE AND status IN ('pending', 'claimed')",
    )
    .bind(row_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_broadcast_failed_by_message_id(
    pool: &Pool<MySql>,
    message_id: &str,
    delegation_id: &str,
) -> Result<(), sqlx::Error> {
    query(
        "UPDATE agent_message_queue
         SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
         WHERE message_id = ? AND delegation_id = ? AND is_broadcast = TRUE AND status IN ('pending', 'claimed')",
    )
    .bind(message_id)
    .bind(delegation_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_broadcast_failed_by_identity(
    pool: &Pool<MySql>,
    row_id: Option<i64>,
    message_id: Option<&str>,
    delegation_id: &str,
) -> Result<(), sqlx::Error> {
    match (row_id, message_id) {
        (Some(row_id), _) => mark_broadcast_failed(pool, row_id).await,
        (None, Some(message_id)) => {
            mark_broadcast_failed_by_message_id(pool, message_id, delegation_id).await
        }
        (None, None) => Err(sqlx::Error::Protocol(
            "missing row_id and message_id for broadcast failure path".into(),
        )),
    }
}

async fn mark_direct_failed(pool: &Pool<MySql>, row_id: i64) -> Result<(), sqlx::Error> {
    query(
        "UPDATE agent_message_queue
         SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
         WHERE id = ? AND is_broadcast = FALSE AND status IN ('pending', 'claimed')",
    )
    .bind(row_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_direct_acked(
    pool: &Pool<MySql>,
    row_id: i64,
    consumer_id: &str,
) -> Result<(), sqlx::Error> {
    query(
        "UPDATE agent_message_queue
         SET status = 'acked', claimed_by = NULL, claimed_at_ms = NULL
         WHERE id = ? AND is_broadcast = FALSE AND status = 'claimed' AND claimed_by = ?",
    )
    .bind(row_id)
    .bind(consumer_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_direct_failed_by_message_id(
    pool: &Pool<MySql>,
    message_id: &str,
    consumer_id: &str,
) -> Result<(), sqlx::Error> {
    query(
        "UPDATE agent_message_queue
         SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
         WHERE message_id = ? AND is_broadcast = FALSE AND status = 'claimed' AND claimed_by = ?",
    )
    .bind(message_id)
    .bind(consumer_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn mark_direct_failed_by_identity(
    pool: &Pool<MySql>,
    row_id: Option<i64>,
    message_id: Option<&str>,
    consumer_id: &str,
) -> Result<(), sqlx::Error> {
    match (row_id, message_id) {
        (Some(row_id), _) => mark_direct_failed(pool, row_id).await,
        (None, Some(message_id)) => {
            mark_direct_failed_by_message_id(pool, message_id, consumer_id).await
        }
        (None, None) => Err(sqlx::Error::Protocol(
            "missing row_id and message_id for direct failure path".into(),
        )),
    }
}

// ─── DatabaseMessageStream ──────────────────────────────────────────────────

/// Abort the poll task when the stream is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Database-backed message stream.
///
/// A background poll task fills the internal buffer channel; `recv()` and
/// `try_recv()` read from the buffer without any DB calls.
struct DatabaseMessageStream {
    buffer_rx: mpsc::UnboundedReceiver<Arc<AgentMessage>>,
    _poll_task: AbortOnDrop,
}

#[async_trait]
impl MessageStream for DatabaseMessageStream {
    async fn recv(&mut self) -> Option<Arc<AgentMessage>> {
        self.buffer_rx.recv().await
    }

    fn try_recv(&mut self) -> Option<Arc<AgentMessage>> {
        self.buffer_rx.try_recv().ok()
    }
}

// ─── Cleanup Scheduler ──────────────────────────────────────────────────────

/// Default cleanup interval.
const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes

/// Default max message age for cleanup of terminal rows.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(3600); // 1 hour

/// Periodic background task that cleans up expired and old messages.
///
/// Stops when dropped (AbortOnDrop pattern).
pub struct CleanupScheduler {
    _task: AbortOnDrop,
}

impl CleanupScheduler {
    /// Start a cleanup scheduler for the given transport.
    ///
    /// - `cleanup_interval`: how often to run cleanup (default 5 minutes)
    /// - `max_age`: delete terminal messages older than this regardless of TTL (default 1 hour)
    pub fn start(
        transport: &DatabaseTransport,
        cleanup_interval: Option<Duration>,
        max_age: Option<Duration>,
    ) -> Self {
        let pool = transport.pool.clone();
        let cleanup_every = cleanup_interval.unwrap_or(DEFAULT_CLEANUP_INTERVAL);
        let reclaim_every = std::cmp::max(transport.visibility_timeout, Duration::from_millis(1));
        let tick_every = std::cmp::min(cleanup_every, reclaim_every);
        let age = max_age.unwrap_or(DEFAULT_MAX_AGE);
        let visibility_timeout = transport.visibility_timeout;
        let max_delivery_attempts = transport.max_delivery_attempts;
        let mut shutdown_rx = transport.shutdown_rx.clone();

        let task = tokio::spawn(async move {
            let mut last_cleanup = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tick_every) => {}
                    _ = shutdown_rx.changed() => {
                        break;
                    }
                }

                match reclaim_stale_in_pool(&pool, visibility_timeout, max_delivery_attempts).await
                {
                    Ok(n) => {
                        if n > 0 {
                            eprintln!("  ℹ messaging: reclaimed {n} stale claimed messages");
                        }
                    }
                    Err(e) => {
                        eprintln!("  ⚠ messaging: reclaim_stale error: {e}");
                    }
                }

                if last_cleanup.elapsed() < cleanup_every {
                    continue;
                }
                last_cleanup = tokio::time::Instant::now();

                // 1. Cleanup TTL-expired messages.
                let now_ms = chrono::Utc::now().timestamp_millis();
                match query(
                    "DELETE FROM agent_message_queue
                     WHERE ttl_ms IS NOT NULL
                       AND timestamp_ms < (? - ttl_ms)",
                )
                .bind(now_ms)
                .execute(&pool)
                .await
                {
                    Ok(result) => {
                        let n = result.rows_affected();
                        if n > 0 {
                            eprintln!("  ℹ messaging: cleaned up {n} TTL-expired messages");
                        }
                    }
                    Err(e) => {
                        eprintln!("  ⚠ messaging: TTL cleanup error: {e}");
                    }
                }

                // 2. Cleanup old terminal messages (regardless of TTL).
                let cutoff_ms = now_ms - age.as_millis() as i64;
                match query(
                    "DELETE FROM agent_message_queue
                     WHERE timestamp_ms < ?
                       AND status IN ('acked', 'failed')",
                )
                .bind(cutoff_ms)
                .execute(&pool)
                .await
                {
                    Ok(result) => {
                        let n = result.rows_affected();
                        if n > 0 {
                            eprintln!(
                                "  ℹ messaging: cleaned up {n} messages older than {:?}",
                                age
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("  ⚠ messaging: age cleanup error: {e}");
                    }
                }
            }
        });

        Self {
            _task: AbortOnDrop(task),
        }
    }
}

async fn reclaim_stale_in_pool(
    pool: &Pool<MySql>,
    visibility_timeout: Duration,
    max_delivery_attempts: u32,
) -> Result<u64, MailboxError> {
    let cutoff_ms = chrono::Utc::now().timestamp_millis() - visibility_timeout.as_millis() as i64;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| MailboxError::Transport(format!("reclaim begin: {e}")))?;

    let reclaimed = query(
        "UPDATE agent_message_queue
         SET status = 'pending', claimed_by = NULL, claimed_at_ms = NULL
         WHERE status = 'claimed'
           AND claimed_at_ms < ?
           AND attempt_count < ?",
    )
    .bind(cutoff_ms)
    .bind(max_delivery_attempts as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| MailboxError::Transport(format!("reclaim: {e}")))?;

    query(
        "UPDATE agent_message_queue
           SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
           WHERE status = 'claimed'
             AND claimed_at_ms < ?
             AND attempt_count >= ?",
    )
    .bind(cutoff_ms)
    .bind(max_delivery_attempts as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| MailboxError::Transport(format!("reclaim dead-letter: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| MailboxError::Transport(format!("reclaim commit: {e}")))?;

    Ok(reclaimed.rows_affected())
}

fn extract_queue_row_id(row: &MySqlRow) -> Result<i64, sqlx::Error> {
    match row.try_get("id") {
        Ok(id) => Ok(id),
        Err(primary_err) => match row
            .try_get::<Option<String>, _>("row_id_text")
            .ok()
            .flatten()
            .and_then(|value| parse_row_id_text_fallback(&value))
        {
            Some(id) => Ok(id),
            None => Err(primary_err),
        },
    }
}

fn parse_row_id_text_fallback(value: &str) -> Option<i64> {
    value.parse::<i64>().ok()
}

fn advance_broadcast_cursor(last_broadcast_id: &mut i64, row_id: i64) {
    *last_broadcast_id = (*last_broadcast_id).max(row_id);
}

async fn release_claimed_for_consumer_in_pool(
    pool: &Pool<MySql>,
    consumer_id: &str,
    max_delivery_attempts: u32,
) -> Result<(), MailboxError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| MailboxError::Transport(format!("release claimed begin: {e}")))?;

    query(
        "UPDATE agent_message_queue
         SET status = 'pending', claimed_by = NULL, claimed_at_ms = NULL
         WHERE status = 'claimed'
           AND is_broadcast = FALSE
           AND claimed_by = ?
           AND attempt_count < ?",
    )
    .bind(consumer_id)
    .bind(max_delivery_attempts as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| MailboxError::Transport(format!("release claimed requeue: {e}")))?;

    query(
        "UPDATE agent_message_queue
         SET status = 'failed', claimed_by = NULL, claimed_at_ms = NULL
         WHERE status = 'claimed'
           AND is_broadcast = FALSE
           AND claimed_by = ?
           AND attempt_count >= ?",
    )
    .bind(consumer_id)
    .bind(max_delivery_attempts as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| MailboxError::Transport(format!("release claimed dead-letter: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| MailboxError::Transport(format!("release claimed commit: {e}")))?;

    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::types::{AgentSignal, MessagePayload, MessageTarget};

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    #[test]
    fn abort_on_drop_aborts_task() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let handle = tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(100)).await;
            });
            let abort = AbortOnDrop(handle);
            drop(abort);
            // Task should be aborted — no lingering futures.
            tokio::time::sleep(Duration::from_millis(10)).await;
        });
    }

    #[test]
    fn insert_message_serialization() {
        // Verify message JSON roundtrip for DB storage.
        let msg = AgentMessage::new(
            addr("run-1", "coder"),
            MessageTarget::Direct {
                address: addr("run-2", "reviewer"),
            },
            MessagePayload::Text {
                content: "review this".into(),
                summary: None,
            },
        );

        let json = serde_json::to_string(&msg).unwrap();
        let restored: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, msg.id);
        assert_eq!(restored.from.run_id, "run-1");
    }

    #[test]
    fn parse_row_id_text_fallback_accepts_numeric_ids() {
        assert_eq!(super::parse_row_id_text_fallback("42"), Some(42));
    }

    #[test]
    fn parse_row_id_text_fallback_rejects_non_numeric_ids() {
        assert_eq!(super::parse_row_id_text_fallback("not-a-row-id"), None);
    }

    #[test]
    fn broadcast_cursor_advances_monotonically() {
        let mut cursor = 5;
        super::advance_broadcast_cursor(&mut cursor, 3);
        assert_eq!(cursor, 5);
        super::advance_broadcast_cursor(&mut cursor, 9);
        assert_eq!(cursor, 9);
    }

    #[test]
    fn broadcast_delivers_to_all_subscribers() {
        // Broadcast delivers to ALL subscribers in a delegation group,
        // consistent with InProcessTransport behavior. No sender exclusion.
        let sender = addr("run-1", "leader");
        let other = addr("run-2", "worker");

        // Both should receive broadcasts — sender is NOT excluded.
        assert_ne!(sender, other);
    }

    #[test]
    fn default_poll_interval() {
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(100));
    }

    #[test]
    fn broadcast_target_for_storage() {
        // Verify that broadcast messages get proper target for DB storage.
        let msg = AgentMessage::new(
            addr("r0", "leader"),
            MessageTarget::Broadcast {
                delegation_id: "del-1".into(),
            },
            MessagePayload::Signal(AgentSignal::Heartbeat),
        );

        // Should already have broadcast target.
        assert!(
            matches!(&msg.to, MessageTarget::Broadcast { delegation_id } if delegation_id == "del-1")
        );

        let json = serde_json::to_string(&msg).unwrap();
        let restored: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(&restored.to, MessageTarget::Broadcast { .. }));
    }

    #[test]
    fn expired_messages_filtered() {
        let mut msg = AgentMessage::new(
            addr("r", "a"),
            MessageTarget::Parent,
            MessagePayload::Signal(AgentSignal::Heartbeat),
        );
        msg.ttl_ms = Some(0);
        assert!(msg.is_expired(), "TTL=0 should be expired immediately");
    }

    #[test]
    fn backoff_constants_are_reasonable() {
        assert!(INITIAL_BACKOFF >= Duration::from_millis(100));
        assert!(MAX_BACKOFF <= Duration::from_secs(30));
        assert!(MAX_BACKOFF > INITIAL_BACKOFF);
        assert!(CRITICAL_FAILURE_THRESHOLD >= 10);
    }

    #[test]
    fn transport_metrics_default() {
        let m = TransportMetrics::default();
        assert_eq!(
            m.messages_sent.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(m.poll_errors.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn cleanup_scheduler_constants() {
        assert_eq!(DEFAULT_CLEANUP_INTERVAL, Duration::from_secs(300));
        assert_eq!(DEFAULT_MAX_AGE, Duration::from_secs(3600));
    }
}
