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
//!     message_id    VARCHAR(36) PRIMARY KEY,
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
//!     INDEX idx_amq_direct    (to_run_id, to_agent_id, status, created_at, message_id),
//!     INDEX idx_amq_broadcast (delegation_id, is_broadcast, status, created_at, message_id)
//! );
//!
//! CREATE TABLE IF NOT EXISTS agent_message_broadcast_delivery (
//!     message_id    VARCHAR(36) NOT NULL,
//!     consumer_id   VARCHAR(256) NOT NULL,
//!     delegation_id VARCHAR(128) NOT NULL,
//!     delivered_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
//!     PRIMARY KEY (message_id, consumer_id),
//!     INDEX idx_ambd_consumer (consumer_id, delegation_id, delivered_at)
//! );
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::{MySql, Pool, Row, query};
use tokio::sync::{RwLock, mpsc, watch};

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Default interval between DB polls for new messages.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum number of messages to fetch per poll cycle.
const POLL_BATCH_SIZE: i64 = 100;

/// Maximum queue rows pruned by a single maintenance statement.
const QUEUE_CLEANUP_BATCH_LIMIT: i64 = 1000;

/// Maximum broadcast delivery rows pruned by a single maintenance statement.
const BROADCAST_DELIVERY_CLEANUP_BATCH_LIMIT: i64 = 1000;

/// Initial backoff on DB poll error (doubles each consecutive failure).
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

/// Maximum backoff on repeated DB poll errors.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// After this many consecutive failures, log a critical warning.
const CRITICAL_FAILURE_THRESHOLD: u32 = 30;

const CLEANUP_EXPIRED_MESSAGES_SQL: &str = "DELETE FROM agent_message_queue
             WHERE ttl_ms IS NOT NULL
               AND timestamp_ms < (? - ttl_ms)
             ORDER BY created_at ASC, message_id ASC
             LIMIT ?";

const CLEANUP_TERMINAL_MESSAGES_SQL: &str = "DELETE FROM agent_message_queue
             WHERE timestamp_ms < ?
               AND status IN ('acked', 'failed')
             ORDER BY created_at ASC, message_id ASC
             LIMIT ?";

const CLEANUP_ORPHAN_BROADCAST_DELIVERY_SQL: &str = "DELETE FROM agent_message_broadcast_delivery
         WHERE message_id NOT IN (
             SELECT message_id FROM agent_message_queue
         )
         ORDER BY message_id ASC, consumer_id ASC
         LIMIT ?";

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

/// Create the message transport tables if they don't exist.
pub async fn ensure_schema(pool: &Pool<MySql>) -> Result<(), sqlx::Error> {
    recreate_legacy_agent_message_queue_if_needed(pool).await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_message_queue (
            message_id    VARCHAR(36) PRIMARY KEY,
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
            INDEX idx_amq_direct    (to_run_id, to_agent_id, status, created_at, message_id),
            INDEX idx_amq_broadcast (delegation_id, is_broadcast, status, created_at, message_id)
        )",
    )
    .execute(pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_message_broadcast_delivery (
            message_id    VARCHAR(36) NOT NULL,
            consumer_id   VARCHAR(256) NOT NULL,
            delegation_id VARCHAR(128) NOT NULL,
            delivered_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (message_id, consumer_id),
            INDEX idx_ambd_consumer (consumer_id, delegation_id, delivered_at)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn recreate_legacy_agent_message_queue_if_needed(
    pool: &Pool<MySql>,
) -> Result<(), sqlx::Error> {
    let row = query(
        "SELECT COUNT(*) AS cnt
         FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = DATABASE()
           AND TABLE_NAME = 'agent_message_queue'
           AND COLUMN_NAME = 'id'",
    )
    .fetch_one(pool)
    .await?;
    let legacy_id_columns: i64 = row.try_get("cnt")?;
    if legacy_id_columns == 0 {
        return Ok(());
    }

    tracing::warn!(
        target: "astra_runtime::messaging::db_transport",
        "recreating legacy agent_message_queue schema with global AUTO_INCREMENT id; drain pending messages before rollout if queue preservation matters"
    );
    query("DROP TABLE IF EXISTS agent_message_broadcast_delivery")
        .execute(pool)
        .await?;
    query("DROP TABLE IF EXISTS agent_message_queue")
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

    /// Delete one bounded batch of expired messages from the queue.
    ///
    /// Call periodically until it returns 0 to prevent unbounded table growth.
    pub async fn cleanup_expired(&self) -> Result<u64, sqlx::Error> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let deleted = cleanup_expired_in_pool(&self.pool, now_ms).await?;
        cleanup_broadcast_delivery_orphans(&self.pool).await?;
        Ok(deleted)
    }

    /// Delete one bounded batch of terminal messages older than the given duration.
    pub async fn cleanup_older_than(&self, max_age: Duration) -> Result<u64, sqlx::Error> {
        let cutoff_ms = chrono::Utc::now().timestamp_millis() - max_age.as_millis() as i64;
        let deleted = cleanup_terminal_older_than_in_pool(&self.pool, cutoff_ms).await?;
        cleanup_broadcast_delivery_orphans(&self.pool).await?;
        Ok(deleted)
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
        self.poll_abort_handles
            .lock()
            .unwrap()
            .push(poll_task.abort_handle());

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
        if self.shutdown_tx.send(true).is_err() {
            tracing::warn!(
                target: "astra_runtime::messaging::db_transport",
                "shutdown broadcast: no active subscribers"
            );
        }
        // Abort any poll tasks that haven't noticed the signal yet.
        for h in self
            .poll_abort_handles
            .lock()
            .unwrap_or_else(|poisoned| {
                tracing::warn!(
                    target: "astra_runtime::messaging::db_transport",
                    "poll_abort_handles mutex poisoned; recovering"
                );
                poisoned.into_inner()
            })
            .drain(..)
        {
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
#[allow(clippy::too_many_arguments)]
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
             ORDER BY created_at ASC, message_id ASC LIMIT ?",
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
                    "SELECT message_id, payload_json FROM agent_message_queue
                     WHERE to_run_id = ? AND to_agent_id = ? AND status = 'claimed' AND claimed_by = ?
                       AND claimed_at_ms = ?
                     ORDER BY created_at ASC, message_id ASC LIMIT ?",
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
                        if message_id.is_none() {
                            metrics
                                .messages_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match mark_direct_failed_by_identity(
                                &pool,
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
                                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                        "  ⚠ messaging: failed to dead-letter direct row without message_id for {}@{}: {:?}",
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
                                        tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                            "  ⚠ messaging: failed to dead-letter undecodable direct row (message_id: {}) for {}@{}: {:?}",
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
                                        tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                            "  ⚠ messaging: failed to release direct claims after closed channel for {}@{}: {:?}",
                                            addr.agent_id, addr.run_id, e
                                        );
                                    }
                                    return;
                                }
                                let message_id = message_id.as_deref().expect("checked above");
                                if let Err(e) =
                                    mark_direct_acked(&pool, message_id, &consumer_id).await
                                {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                        "  ⚠ messaging: failed to ack delivered direct message {} for {}@{}: {:?}",
                                        message_id, addr.agent_id, addr.run_id, e
                                    );
                                }
                            }
                            Ok(_) | Err(_) => {
                                metrics
                                    .messages_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                match mark_direct_failed_by_identity(
                                    &pool,
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
                                        tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                            "  ⚠ messaging: failed to dead-letter direct row (message_id: {}) for {}@{}: {:?}",
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
                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
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
                tracing::warn!(target: "astra_runtime::messaging::db_transport",
                    "  ⚠ messaging: direct claim error for {}@{}: {:?}",
                    addr.agent_id, addr.run_id, e
                );
            }
        }

        // 2. Poll broadcast messages (if in a delegation group).
        //    Broadcast delivery is tracked per consumer so correctness does not
        //    depend on a global append id or wall-clock-monotonic cursor.
        if let Some(ref did) = delegation_id {
            let broadcast_result = query(
                "SELECT q.message_id, q.payload_json
                 FROM agent_message_queue q
                 LEFT JOIN agent_message_broadcast_delivery d
                   ON d.message_id = q.message_id AND d.consumer_id = ?
                 WHERE q.delegation_id = ? AND q.is_broadcast = TRUE
                   AND q.status IN ('pending', 'claimed')
                   AND d.message_id IS NULL
                 ORDER BY q.created_at ASC, q.message_id ASC LIMIT ?",
            )
            .bind(&consumer_id)
            .bind(did)
            .bind(POLL_BATCH_SIZE)
            .fetch_all(&pool)
            .await;

            if let Ok(rows) = broadcast_result {
                for row in rows {
                    let message_id: Option<String> = row.try_get("message_id").ok();
                    if message_id.is_none() {
                        had_error = true;
                        metrics
                            .poll_errors
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(target: "astra_runtime::messaging::db_transport",
                            "  ⚠ messaging: broadcast row missing message_id for delegation {}",
                            did
                        );
                        continue;
                    }
                    let message_id = message_id.expect("checked above");
                    let json: String = match row.try_get("payload_json") {
                        Ok(j) => j,
                        Err(_) => {
                            metrics
                                .messages_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match mark_broadcast_failed_by_identity(&pool, &message_id, did).await {
                                Ok(()) => {}
                                Err(e) => {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                        "  ⚠ messaging: failed to dead-letter undecodable broadcast message {} for delegation {}: {:?}",
                                        message_id, did, e
                                    );
                                }
                            }
                            continue;
                        }
                    };

                    match serde_json::from_str::<AgentMessage>(&json) {
                        Ok(msg) if !msg.is_expired() => {
                            match reserve_broadcast_delivery(&pool, &message_id, &consumer_id, did)
                                .await
                            {
                                Ok(true) => {}
                                Ok(false) => continue,
                                Err(e) => {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                        "  ⚠ messaging: failed to reserve broadcast delivery {} for {} in delegation {}: {:?}",
                                        message_id, consumer_id, did, e
                                    );
                                    continue;
                                }
                            }
                            metrics
                                .messages_received
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if tx.send(Arc::new(msg)).is_err() {
                                if let Err(e) =
                                    release_broadcast_delivery(&pool, &message_id, &consumer_id)
                                        .await
                                {
                                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                        "  ⚠ messaging: failed to release reserved broadcast delivery {} for {} after closed channel: {:?}",
                                        message_id, consumer_id, e
                                    );
                                }
                                return;
                            }
                        }
                        Ok(_) | Err(_) => {
                            metrics
                                .messages_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match mark_broadcast_failed_by_identity(&pool, &message_id, did).await {
                                Ok(()) => {}
                                Err(e) => {
                                    had_error = true;
                                    metrics
                                        .poll_errors
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    tracing::warn!(target: "astra_runtime::messaging::db_transport",
                                        "  ⚠ messaging: failed to dead-letter broadcast message {} for delegation {}: {:?}",
                                        message_id, did, e
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
                tracing::warn!(target: "astra_runtime::messaging::db_transport",
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
                tracing::warn!(target: "astra_runtime::messaging::db_transport",
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

async fn reserve_broadcast_delivery(
    pool: &Pool<MySql>,
    message_id: &str,
    consumer_id: &str,
    delegation_id: &str,
) -> Result<bool, sqlx::Error> {
    let result = query(
        "INSERT IGNORE INTO agent_message_broadcast_delivery
         (message_id, consumer_id, delegation_id)
         VALUES (?, ?, ?)",
    )
    .bind(message_id)
    .bind(consumer_id)
    .bind(delegation_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

async fn release_broadcast_delivery(
    pool: &Pool<MySql>,
    message_id: &str,
    consumer_id: &str,
) -> Result<(), sqlx::Error> {
    query(
        "DELETE FROM agent_message_broadcast_delivery
         WHERE message_id = ? AND consumer_id = ?",
    )
    .bind(message_id)
    .bind(consumer_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_broadcast_failed_by_identity(
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

async fn mark_direct_acked(
    pool: &Pool<MySql>,
    message_id: &str,
    consumer_id: &str,
) -> Result<(), sqlx::Error> {
    query(
        "UPDATE agent_message_queue
         SET status = 'acked', claimed_by = NULL, claimed_at_ms = NULL
         WHERE message_id = ? AND is_broadcast = FALSE AND status = 'claimed' AND claimed_by = ?",
    )
    .bind(message_id)
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

pub async fn mark_direct_failed_by_identity(
    pool: &Pool<MySql>,
    message_id: Option<&str>,
    consumer_id: &str,
) -> Result<(), sqlx::Error> {
    match message_id {
        Some(message_id) => mark_direct_failed_by_message_id(pool, message_id, consumer_id).await,
        None => Err(sqlx::Error::Protocol(
            "missing message_id for direct failure path".into(),
        )),
    }
}

async fn cleanup_expired_in_pool(pool: &Pool<MySql>, now_ms: i64) -> Result<u64, sqlx::Error> {
    let result = query(CLEANUP_EXPIRED_MESSAGES_SQL)
        .bind(now_ms)
        .bind(QUEUE_CLEANUP_BATCH_LIMIT)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

async fn cleanup_terminal_older_than_in_pool(
    pool: &Pool<MySql>,
    cutoff_ms: i64,
) -> Result<u64, sqlx::Error> {
    let result = query(CLEANUP_TERMINAL_MESSAGES_SQL)
        .bind(cutoff_ms)
        .bind(QUEUE_CLEANUP_BATCH_LIMIT)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

async fn cleanup_broadcast_delivery_orphans(pool: &Pool<MySql>) -> Result<u64, sqlx::Error> {
    let result = query(CLEANUP_ORPHAN_BROADCAST_DELIVERY_SQL)
        .bind(BROADCAST_DELIVERY_CLEANUP_BATCH_LIMIT)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
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
                            tracing::info!(target: "astra_runtime::messaging::db_transport", "reclaimed {n} stale claimed messages");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "astra_runtime::messaging::db_transport", "  ⚠ messaging: reclaim_stale error: {e}");
                    }
                }

                if last_cleanup.elapsed() < cleanup_every {
                    continue;
                }
                last_cleanup = tokio::time::Instant::now();

                // 1. Cleanup TTL-expired messages.
                let now_ms = chrono::Utc::now().timestamp_millis();
                match cleanup_expired_in_pool(&pool, now_ms).await {
                    Ok(result) => {
                        let n = result;
                        if n > 0 {
                            tracing::info!(target: "astra_runtime::messaging::db_transport", "cleaned up {n} TTL-expired messages");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "astra_runtime::messaging::db_transport", "  ⚠ messaging: TTL cleanup error: {e}");
                    }
                }

                // 2. Cleanup old terminal messages (regardless of TTL).
                let cutoff_ms = now_ms - age.as_millis() as i64;
                match cleanup_terminal_older_than_in_pool(&pool, cutoff_ms).await {
                    Ok(result) => {
                        let n = result;
                        if n > 0 {
                            tracing::info!(
                                target: "astra_runtime::messaging::db_transport",
                                "cleaned up {n} messages older than {:?}",
                                age
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "astra_runtime::messaging::db_transport", "  ⚠ messaging: age cleanup error: {e}");
                    }
                }

                // 3. Cleanup broadcast delivery rows whose queue message was pruned.
                match cleanup_broadcast_delivery_orphans(&pool).await {
                    Ok(n) => {
                        if n > 0 {
                            tracing::info!(
                                target: "astra_runtime::messaging::db_transport",
                                "cleaned up {n} orphaned broadcast delivery rows"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "astra_runtime::messaging::db_transport", "  ⚠ messaging: broadcast delivery cleanup error: {e}");
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
    use sqlx::{
        QueryBuilder,
        mysql::{MySqlConnectOptions, MySqlPoolOptions},
    };

    static LIVE_DB_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    fn constants_are_reasonable() {
        // Poll interval
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(100));
        assert_eq!(POLL_BATCH_SIZE, 100);
        assert!(QUEUE_CLEANUP_BATCH_LIMIT >= POLL_BATCH_SIZE);
        assert!(QUEUE_CLEANUP_BATCH_LIMIT <= 10_000);
        assert!(BROADCAST_DELIVERY_CLEANUP_BATCH_LIMIT <= 10_000);
        // Cleanup
        assert_eq!(DEFAULT_CLEANUP_INTERVAL, Duration::from_secs(300));
        assert_eq!(DEFAULT_MAX_AGE, Duration::from_secs(3600));
        // Backoff
        let threshold: u32 = CRITICAL_FAILURE_THRESHOLD;
        assert!(threshold >= 10);
        assert!(INITIAL_BACKOFF >= Duration::from_millis(100));
        assert!(MAX_BACKOFF <= Duration::from_secs(30));
        assert!(MAX_BACKOFF > INITIAL_BACKOFF);
    }

    #[test]
    fn queue_cleanup_sql_uses_ordered_bounded_batches() {
        for (name, sql) in [
            ("expired", CLEANUP_EXPIRED_MESSAGES_SQL),
            ("terminal", CLEANUP_TERMINAL_MESSAGES_SQL),
        ] {
            assert!(
                sql.contains("DELETE FROM agent_message_queue"),
                "{name} cleanup must target the queue table"
            );
            assert!(
                sql.contains("ORDER BY created_at ASC, message_id ASC"),
                "{name} cleanup must prune deterministically"
            );
            assert!(
                sql.trim_end().ends_with("LIMIT ?"),
                "{name} cleanup must stay batch bounded"
            );
        }
        assert!(
            CLEANUP_ORPHAN_BROADCAST_DELIVERY_SQL
                .contains("DELETE FROM agent_message_broadcast_delivery"),
            "orphan cleanup must target broadcast delivery rows"
        );
        assert!(
            CLEANUP_ORPHAN_BROADCAST_DELIVERY_SQL
                .contains("ORDER BY message_id ASC, consumer_id ASC"),
            "orphan delivery cleanup must prune deterministically"
        );
        assert!(
            CLEANUP_ORPHAN_BROADCAST_DELIVERY_SQL
                .trim_end()
                .ends_with("LIMIT ?"),
            "orphan delivery cleanup must stay batch bounded"
        );
    }

    #[test]
    fn cleanup_scheduler_reuses_bounded_cleanup_helpers() {
        let source = include_str!("db_transport.rs");
        let scheduler_body = source
            .split("impl CleanupScheduler")
            .nth(1)
            .and_then(|rest| rest.split("async fn reclaim_stale_in_pool").next())
            .expect("cleanup scheduler body");
        for helper in [
            "cleanup_expired_in_pool(&pool, now_ms)",
            "cleanup_terminal_older_than_in_pool(&pool, cutoff_ms)",
            "cleanup_broadcast_delivery_orphans(&pool)",
        ] {
            assert!(
                scheduler_body.contains(helper),
                "cleanup scheduler must call bounded helper {helper}"
            );
        }
        assert!(
            !scheduler_body.contains("DELETE FROM agent_message_queue"),
            "cleanup scheduler must not inline unbounded queue DELETE statements"
        );
        assert!(
            !scheduler_body.contains("DELETE FROM agent_message_broadcast_delivery"),
            "cleanup scheduler must not inline unbounded broadcast delivery DELETE statements"
        );
    }

    #[tokio::test]
    #[ignore = "requires live MatrixOne; set ASTRA_TEST_DB_IT=1"]
    async fn db_cleanup_prunes_expired_terminal_and_orphan_delivery_batches() {
        let _guard = LIVE_DB_TEST_LOCK.lock().await;
        let pool = live_test_pool().await;
        ensure_schema(&pool).await.expect("ensure messaging schema");
        clear_message_tables(&pool).await;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let delegation_id = format!("delegation-{}", uuid::Uuid::new_v4());
        let consumer_id = format!("consumer-{}", uuid::Uuid::new_v4());
        let expired_message_id = uuid::Uuid::new_v4().to_string();
        let terminal_message_id = uuid::Uuid::new_v4().to_string();
        let live_message_id = uuid::Uuid::new_v4().to_string();
        let orphan_message_id = uuid::Uuid::new_v4().to_string();

        insert_queue_row(
            &pool,
            &expired_message_id,
            &delegation_id,
            now_ms - 10_000,
            Some(1),
            "pending",
            true,
        )
        .await;
        insert_queue_row(
            &pool,
            &terminal_message_id,
            &delegation_id,
            now_ms - 7_200_000,
            None,
            "acked",
            true,
        )
        .await;
        insert_queue_row(
            &pool,
            &live_message_id,
            &delegation_id,
            now_ms,
            Some(86_400_000),
            "pending",
            true,
        )
        .await;
        insert_delivery_row(&pool, &expired_message_id, &consumer_id, &delegation_id).await;
        insert_delivery_row(&pool, &terminal_message_id, &consumer_id, &delegation_id).await;
        insert_delivery_row(&pool, &live_message_id, &consumer_id, &delegation_id).await;
        insert_delivery_row(&pool, &orphan_message_id, &consumer_id, &delegation_id).await;

        let transport = DatabaseTransport::new(pool.clone());
        assert_eq!(
            transport.cleanup_expired().await.expect("ttl cleanup"),
            1,
            "expired cleanup should prune exactly the expired queue row"
        );
        assert_eq!(
            transport
                .cleanup_older_than(Duration::from_secs(3600))
                .await
                .expect("terminal cleanup"),
            1,
            "terminal cleanup should prune exactly the old acked queue row"
        );

        assert_eq!(count_queue_message(&pool, &expired_message_id).await, 0);
        assert_eq!(count_queue_message(&pool, &terminal_message_id).await, 0);
        assert_eq!(count_queue_message(&pool, &live_message_id).await, 1);
        assert_eq!(count_delivery_message(&pool, &expired_message_id).await, 0);
        assert_eq!(count_delivery_message(&pool, &terminal_message_id).await, 0);
        assert_eq!(count_delivery_message(&pool, &orphan_message_id).await, 0);
        assert_eq!(count_delivery_message(&pool, &live_message_id).await, 1);
    }

    #[tokio::test]
    #[ignore = "requires live MatrixOne; set ASTRA_TEST_DB_IT=1"]
    async fn db_cleanup_expired_prunes_more_than_one_queue_batch() {
        let _guard = LIVE_DB_TEST_LOCK.lock().await;
        let pool = live_test_pool().await;
        ensure_schema(&pool).await.expect("ensure messaging schema");
        clear_message_tables(&pool).await;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let delegation_id = format!("delegation-batch-{}", uuid::Uuid::new_v4());
        let consumer_id = format!("consumer-batch-{}", uuid::Uuid::new_v4());
        let row_count = QUEUE_CLEANUP_BATCH_LIMIT + 5;
        let message_ids = (0..row_count)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<Vec<_>>();

        insert_expired_queue_rows(&pool, &message_ids, &delegation_id, now_ms).await;
        insert_delivery_rows(&pool, &message_ids, &consumer_id, &delegation_id).await;
        wait_for_queue_count(&pool, row_count).await;
        wait_for_delivery_count(&pool, row_count).await;

        let transport = DatabaseTransport::new(pool.clone());
        let first = transport.cleanup_expired().await.expect("first cleanup");
        assert_eq!(
            first, QUEUE_CLEANUP_BATCH_LIMIT as u64,
            "first cleanup must stop at one bounded queue batch"
        );
        wait_for_queue_count(&pool, row_count - QUEUE_CLEANUP_BATCH_LIMIT).await;

        let second = transport.cleanup_expired().await.expect("second cleanup");
        assert_eq!(
            second,
            (row_count - QUEUE_CLEANUP_BATCH_LIMIT) as u64,
            "second cleanup must prune the remaining expired rows"
        );
        wait_for_queue_count(&pool, 0).await;
        wait_for_delivery_count(&pool, 0).await;

        let third = transport.cleanup_expired().await.expect("third cleanup");
        assert_eq!(
            third, 0,
            "cleanup should converge after expired rows are gone"
        );
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

    async fn live_test_pool() -> Pool<MySql> {
        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored MatrixOne integration tests"
        );

        let host = std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(6001);
        let user = std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".to_string());
        let password = std::env::var("MATRIXONE_PASSWORD").unwrap_or_else(|_| "111".to_string());
        let database = resolved_test_database_name();
        let bootstrap_catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());

        assert_test_database_name(&database);
        assert_test_database_name(&bootstrap_catalog);

        let admin_options = mysql_options(&host, port, &user, &password, &bootstrap_catalog);
        let admin_pool = MySqlPoolOptions::new()
            .max_connections(2)
            .connect_with(admin_options)
            .await
            .expect("connect MatrixOne bootstrap catalog");
        sqlx::query(&format!(
            "CREATE DATABASE IF NOT EXISTS {}",
            quote_mysql_identifier(&database)
        ))
        .execute(&admin_pool)
        .await
        .expect("create MatrixOne test database");
        admin_pool.close().await;

        MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(mysql_options(&host, port, &user, &password, &database))
            .await
            .expect("connect MatrixOne test database")
    }

    fn mysql_options(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> MySqlConnectOptions {
        MySqlConnectOptions::new()
            .host(host)
            .port(port)
            .username(user)
            .password(password)
            .database(database)
    }

    fn resolved_test_database_name() -> String {
        let base = std::env::var("ASTRA_DATABASE").unwrap_or_else(|_| "astra_runtime".to_string());
        match std::env::var("ASTRA_DATABASE_PREFIX") {
            Ok(prefix) if !prefix.is_empty() => format!("{prefix}{base}"),
            _ => base,
        }
    }

    fn assert_test_database_name(name: &str) {
        assert!(
            name.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
            "MatrixOne test database name must be a simple identifier: {name}"
        );
        if name == "mysql" {
            return;
        }
        assert!(
            name.contains("test") || name.contains("smoke"),
            "refusing to run destructive MatrixOne integration test against non-test database: {name}"
        );
    }

    fn quote_mysql_identifier(name: &str) -> String {
        assert_test_database_name(name);
        format!("`{name}`")
    }

    async fn insert_queue_row(
        pool: &Pool<MySql>,
        message_id: &str,
        delegation_id: &str,
        timestamp_ms: i64,
        ttl_ms: Option<i64>,
        status: &str,
        is_broadcast: bool,
    ) {
        query(
            "INSERT INTO agent_message_queue
             (message_id, from_run_id, from_agent_id, delegation_id, is_broadcast,
              payload_json, timestamp_ms, ttl_ms, status)
             VALUES (?, ?, ?, ?, ?, '{}', ?, ?, ?)",
        )
        .bind(message_id)
        .bind(format!("from-run-{message_id}"))
        .bind("from-agent")
        .bind(delegation_id)
        .bind(is_broadcast)
        .bind(timestamp_ms)
        .bind(ttl_ms)
        .bind(status)
        .execute(pool)
        .await
        .expect("insert queue row");
    }

    async fn insert_delivery_row(
        pool: &Pool<MySql>,
        message_id: &str,
        consumer_id: &str,
        delegation_id: &str,
    ) {
        query(
            "INSERT INTO agent_message_broadcast_delivery
             (message_id, consumer_id, delegation_id)
             VALUES (?, ?, ?)",
        )
        .bind(message_id)
        .bind(consumer_id)
        .bind(delegation_id)
        .execute(pool)
        .await
        .expect("insert broadcast delivery row");
    }

    async fn count_queue_message(pool: &Pool<MySql>, message_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_message_queue WHERE message_id = ?")
            .bind(message_id)
            .fetch_one(pool)
            .await
            .expect("count queue message")
    }

    async fn count_delivery_message(pool: &Pool<MySql>, message_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_message_broadcast_delivery WHERE message_id = ?",
        )
        .bind(message_id)
        .fetch_one(pool)
        .await
        .expect("count delivery message")
    }

    async fn insert_expired_queue_rows(
        pool: &Pool<MySql>,
        message_ids: &[String],
        delegation_id: &str,
        now_ms: i64,
    ) {
        let mut builder = QueryBuilder::<MySql>::new(
            "INSERT INTO agent_message_queue
             (message_id, from_run_id, from_agent_id, delegation_id, is_broadcast,
              payload_json, timestamp_ms, ttl_ms, status) ",
        );
        builder.push_values(message_ids, |mut row, message_id| {
            row.push_bind(message_id)
                .push_bind(format!("from-run-{message_id}"))
                .push_bind("from-agent")
                .push_bind(delegation_id)
                .push_bind(true)
                .push_bind("{}")
                .push_bind(now_ms - 10_000)
                .push_bind(1_i64)
                .push_bind("pending");
        });
        builder
            .build()
            .execute(pool)
            .await
            .expect("insert expired queue rows");
    }

    async fn clear_message_tables(pool: &Pool<MySql>) {
        query("DELETE FROM agent_message_broadcast_delivery")
            .execute(pool)
            .await
            .expect("clear broadcast delivery table");
        query("DELETE FROM agent_message_queue")
            .execute(pool)
            .await
            .expect("clear message queue table");
    }

    async fn insert_delivery_rows(
        pool: &Pool<MySql>,
        message_ids: &[String],
        consumer_id: &str,
        delegation_id: &str,
    ) {
        let mut builder = QueryBuilder::<MySql>::new(
            "INSERT INTO agent_message_broadcast_delivery
             (message_id, consumer_id, delegation_id) ",
        );
        builder.push_values(message_ids, |mut row, message_id| {
            row.push_bind(message_id)
                .push_bind(consumer_id)
                .push_bind(delegation_id);
        });
        builder
            .build()
            .execute(pool)
            .await
            .expect("insert broadcast delivery rows");
    }

    async fn count_queue_rows(pool: &Pool<MySql>) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_message_queue")
            .fetch_one(pool)
            .await
            .expect("count queue rows")
    }

    async fn count_delivery_rows(pool: &Pool<MySql>) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_message_broadcast_delivery")
            .fetch_one(pool)
            .await
            .expect("count delivery rows")
    }

    async fn wait_for_queue_count(pool: &Pool<MySql>, expected: i64) {
        wait_for_count("queue", expected, || async { count_queue_rows(pool).await }).await;
    }

    async fn wait_for_delivery_count(pool: &Pool<MySql>, expected: i64) {
        wait_for_count("broadcast delivery", expected, || async {
            count_delivery_rows(pool).await
        })
        .await;
    }

    async fn wait_for_count<F, Fut>(label: &str, expected: i64, mut current: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = i64>,
    {
        let mut last = current().await;
        for _ in 0..20 {
            if last == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            last = current().await;
        }
        assert_eq!(
            last, expected,
            "{label} row count did not reach expected value"
        );
    }
}
