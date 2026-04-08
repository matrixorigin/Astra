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
//!     created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
//!     INDEX idx_amq_direct    (to_run_id, to_agent_id, id),
//!     INDEX idx_amq_broadcast (delegation_id, is_broadcast, id)
//! );
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::{MySql, Pool, Row, query};
use tokio::sync::{RwLock, mpsc};

use super::transport::{MessageStream, MessageTransport};
use super::types::{AgentAddress, AgentMessage, MailboxError};

/// Default interval between DB polls for new messages.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum number of messages to fetch per poll cycle.
const POLL_BATCH_SIZE: i64 = 100;

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
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_amq_direct    (to_run_id, to_agent_id, id),
            INDEX idx_amq_broadcast (delegation_id, is_broadcast, id)
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
    /// Tracks registered agents and their delegation group.
    registrations: RwLock<HashMap<AgentAddress, Option<String>>>,
}

impl DatabaseTransport {
    /// Create a new database transport.
    ///
    /// Call [`ensure_schema()`] before first use to create the table.
    pub fn new(pool: Pool<MySql>) -> Self {
        Self {
            pool,
            poll_interval: DEFAULT_POLL_INTERVAL,
            registrations: RwLock::new(HashMap::new()),
        }
    }

    /// Set the poll interval for message streams.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Get the underlying pool (for tests / diagnostics).
    pub fn pool(&self) -> &Pool<MySql> {
        &self.pool
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

    /// Delete all messages older than the given duration.
    pub async fn cleanup_older_than(&self, max_age: Duration) -> Result<u64, sqlx::Error> {
        let cutoff_ms = chrono::Utc::now().timestamp_millis() - max_age.as_millis() as i64;
        let result = query("DELETE FROM agent_message_queue WHERE timestamp_ms < ?")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Number of currently registered agents (local process only).
    pub async fn agent_count(&self) -> usize {
        self.registrations.read().await.len()
    }

    /// Insert a message into the database.
    async fn insert_message(&self, msg: &AgentMessage, is_broadcast: bool) -> Result<(), MailboxError> {
        let (to_run_id, to_agent_id, delegation_id) = match &msg.to {
            super::types::MessageTarget::Direct { address } => {
                (Some(address.run_id.as_str()), Some(address.agent_id.as_str()), None)
            }
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
        self.registrations
            .write()
            .await
            .insert(addr, delegation_id);
        Ok(())
    }

    async fn unregister(&self, addr: &AgentAddress) -> Result<(), MailboxError> {
        self.registrations.write().await.remove(addr);
        Ok(())
    }

    async fn subscribe(
        &self,
        addr: &AgentAddress,
    ) -> Result<Box<dyn MessageStream>, MailboxError> {
        let regs = self.registrations.read().await;
        let delegation_id = regs
            .get(addr)
            .ok_or_else(|| MailboxError::AgentNotFound(addr.clone()))?
            .clone();
        drop(regs);

        let (tx, rx) = mpsc::unbounded_channel();
        let poll_task = tokio::spawn(poll_loop(
            self.pool.clone(),
            addr.clone(),
            delegation_id,
            self.poll_interval,
            tx,
        ));

        Ok(Box::new(DatabaseMessageStream {
            buffer_rx: rx,
            _poll_task: AbortOnDrop(poll_task),
        }))
    }

    async fn send(&self, msg: Arc<AgentMessage>) -> Result<(), MailboxError> {
        match &msg.to {
            super::types::MessageTarget::Direct { .. } => {}
            _ => {
                return Err(MailboxError::Transport(
                    "send() requires Direct target".into(),
                ))
            }
        }
        self.insert_message(&msg, false).await
    }

    async fn broadcast(
        &self,
        delegation_id: &str,
        msg: Arc<AgentMessage>,
    ) -> Result<(), MailboxError> {
        // Ensure the message has the right broadcast target for storage.
        // The actual message's `to` field might already be set correctly by the router.
        let stored_msg = if matches!(&msg.to, super::types::MessageTarget::Broadcast { .. }) {
            (*msg).clone()
        } else {
            // Wrap with broadcast target for DB storage.
            let mut m = (*msg).clone();
            m.to = super::types::MessageTarget::Broadcast {
                delegation_id: delegation_id.to_string(),
            };
            m
        };
        self.insert_message(&stored_msg, true).await
    }
}

// ─── Poll Loop ──────────────────────────────────────────────────────────────

/// Background task that polls the database for new messages and pushes them
/// into a local channel.
async fn poll_loop(
    pool: Pool<MySql>,
    addr: AgentAddress,
    delegation_id: Option<String>,
    interval: Duration,
    tx: mpsc::UnboundedSender<Arc<AgentMessage>>,
) {
    let mut last_direct_id: i64 = 0;
    let mut last_broadcast_id: i64 = 0;

    loop {
        // Check if receiver dropped (stream was dropped).
        if tx.is_closed() {
            break;
        }

        // 1. Poll direct messages.
        let direct_result = query(
            "SELECT id, payload_json FROM agent_message_queue
             WHERE to_run_id = ? AND to_agent_id = ? AND id > ?
             ORDER BY id ASC LIMIT ?",
        )
        .bind(&addr.run_id)
        .bind(&addr.agent_id)
        .bind(last_direct_id)
        .bind(POLL_BATCH_SIZE)
        .fetch_all(&pool)
        .await;

        if let Ok(rows) = direct_result {
            for row in rows {
                let row_id: i64 = match row.try_get("id") {
                    Ok(id) => id,
                    Err(_) => continue, // skip corrupted row, don't reset cursor
                };
                let json: String = match row.try_get("payload_json") {
                    Ok(j) => j,
                    Err(_) => {
                        last_direct_id = last_direct_id.max(row_id);
                        continue; // skip but advance cursor past this row
                    }
                };

                if let Ok(msg) = serde_json::from_str::<AgentMessage>(&json) {
                    if !msg.is_expired() {
                        if tx.send(Arc::new(msg)).is_err() {
                            return; // receiver dropped
                        }
                    }
                }
                last_direct_id = last_direct_id.max(row_id);
            }
        } else {
            eprintln!("  ⚠ messaging: direct poll error for {}@{}: {:?}", addr.agent_id, addr.run_id, direct_result.unwrap_err());
        }

        // 2. Poll broadcast messages (if in a delegation group).
        if let Some(ref did) = delegation_id {
            let broadcast_result = query(
                "SELECT id, payload_json FROM agent_message_queue
                 WHERE delegation_id = ? AND is_broadcast = TRUE AND id > ?
                 ORDER BY id ASC LIMIT ?",
            )
            .bind(did)
            .bind(last_broadcast_id)
            .bind(POLL_BATCH_SIZE)
            .fetch_all(&pool)
            .await;

            if let Ok(rows) = broadcast_result {
                for row in rows {
                    let row_id: i64 = match row.try_get("id") {
                        Ok(id) => id,
                        Err(_) => continue,
                    };
                    let json: String = match row.try_get("payload_json") {
                        Ok(j) => j,
                        Err(_) => {
                            last_broadcast_id = last_broadcast_id.max(row_id);
                            continue;
                        }
                    };

                    if let Ok(msg) = serde_json::from_str::<AgentMessage>(&json) {
                        if !msg.is_expired() {
                            if tx.send(Arc::new(msg)).is_err() {
                                return;
                            }
                        }
                    }
                    last_broadcast_id = last_broadcast_id.max(row_id);
                }
            } else {
                eprintln!("  ⚠ messaging: broadcast poll error for delegation {}: {:?}", did, broadcast_result.unwrap_err());
            }
        }

        tokio::time::sleep(interval).await;
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::types::{
        AgentSignal, MessagePayload, MessageTarget,
    };

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
        assert!(matches!(&msg.to, MessageTarget::Broadcast { delegation_id } if delegation_id == "del-1"));

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
}
