//! Pending acknowledgment tracker for reliable message delivery.
//!
//! Tracks messages sent with `requires_ack = true` and automatically retries
//! delivery if no ack is received within a configurable timeout. After
//! `max_retries` attempts, the message is moved to "failed" and the sender
//! is notified (via a callback or error).
//!
//! # Design
//!
//! ```text
//!   send(msg, requires_ack=true)
//!       │
//!       ▼
//!  ┌────────────────┐
//!  │ PendingAckMap   │  msg_id → PendingEntry { msg, attempts, sent_at }
//!  └───────┬────────┘
//!          │
//!  ┌───────┴────────────────────┐
//!  │  Sweep task (periodic)     │
//!  │  • Check for expired acks  │
//!  │  • Retry via router.send() │
//!  │  • Mark failed after max   │
//!  └────────────────────────────┘
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use super::types::AgentMessage;

/// Default timeout before a message is considered unacknowledged.
const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Default maximum retry attempts before giving up.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default sweep interval for checking pending acks.
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Configuration for the ack tracker.
#[derive(Clone, Debug)]
pub struct AckConfig {
    /// How long to wait for an ack before retrying.
    pub ack_timeout: Duration,
    /// Maximum number of delivery attempts (including the initial send).
    pub max_retries: u32,
    /// How often the sweep task checks for stale pending entries.
    pub sweep_interval: Duration,
}

impl Default for AckConfig {
    fn default() -> Self {
        Self {
            ack_timeout: DEFAULT_ACK_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
        }
    }
}

/// A message awaiting acknowledgment.
#[derive(Debug)]
struct PendingEntry {
    /// The original message (for retry).
    message: Arc<AgentMessage>,
    /// Number of delivery attempts so far.
    attempts: u32,
    /// When the latest attempt was sent.
    last_sent_at: Instant,
}

/// Outcome of a sweep for a single message.
#[derive(Debug, Clone)]
pub enum AckOutcome {
    /// Message was acknowledged successfully.
    Acknowledged { message_id: String },
    /// Message needs retry (will be re-sent).
    Retry { message_id: String, attempt: u32 },
    /// Message failed after max retries.
    Failed {
        message_id: String,
        attempts: u32,
        message: Arc<AgentMessage>,
    },
    /// Message was explicitly rejected (Nack).
    Rejected {
        message_id: String,
        reason: Option<String>,
        message: Arc<AgentMessage>,
    },
}

/// Tracks messages awaiting acknowledgment and manages retry logic.
///
/// Thread-safe: uses `RwLock` for concurrent access from send/ack/sweep paths.
pub struct PendingAckTracker {
    config: AckConfig,
    /// Pending messages keyed by message ID.
    pending: RwLock<HashMap<String, PendingEntry>>,
    /// Failed messages (for diagnostics / dead-letter inspection).
    failed: RwLock<Vec<AckOutcome>>,
}

impl Default for PendingAckTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingAckTracker {
    /// Create a new tracker with default configuration.
    pub fn new() -> Self {
        Self::with_config(AckConfig::default())
    }

    /// Create a new tracker with custom configuration.
    pub fn with_config(config: AckConfig) -> Self {
        Self {
            config,
            pending: RwLock::new(HashMap::new()),
            failed: RwLock::new(Vec::new()),
        }
    }

    /// Track a sent message that requires acknowledgment.
    pub async fn track(&self, msg: Arc<AgentMessage>) {
        if !msg.requires_ack {
            return;
        }
        let entry = PendingEntry {
            message: msg.clone(),
            attempts: 1,
            last_sent_at: Instant::now(),
        };
        self.pending.write().await.insert(msg.id.clone(), entry);
    }

    /// Process an incoming ack — removes the message from pending.
    ///
    /// Returns `true` if the message was found and acknowledged, `false` if
    /// it was already acked or not tracked.
    pub async fn acknowledge(&self, message_id: &str) -> bool {
        self.pending.write().await.remove(message_id).is_some()
    }

    /// Process an incoming nack — removes from pending and records failure.
    pub async fn reject(&self, message_id: &str, reason: Option<String>) {
        if let Some(entry) = self.pending.write().await.remove(message_id) {
            self.failed.write().await.push(AckOutcome::Rejected {
                message_id: message_id.to_string(),
                reason,
                message: entry.message,
            });
        }
    }

    /// Sweep for timed-out messages. Returns messages that need retry and
    /// messages that have permanently failed.
    ///
    /// Call this periodically (the sweep task does it automatically).
    pub async fn sweep(&self) -> Vec<AckOutcome> {
        let now = Instant::now();
        let mut outcomes = Vec::new();
        let mut to_remove = Vec::new();

        {
            let mut pending = self.pending.write().await;
            for (msg_id, entry) in pending.iter_mut() {
                if now.duration_since(entry.last_sent_at) >= self.config.ack_timeout {
                    if entry.attempts >= self.config.max_retries {
                        // Permanently failed — collect message for DLQ.
                        to_remove.push((msg_id.clone(), entry.attempts, Arc::clone(&entry.message)));
                    } else {
                        // Needs retry.
                        entry.attempts += 1;
                        entry.last_sent_at = now;
                        outcomes.push(AckOutcome::Retry {
                            message_id: msg_id.clone(),
                            attempt: entry.attempts,
                        });
                    }
                }
            }
            for (id, attempts, message) in &to_remove {
                pending.remove(id);
                outcomes.push(AckOutcome::Failed {
                    message_id: id.clone(),
                    attempts: *attempts,
                    message: Arc::clone(message),
                });
            }
        }

        // Record failures for diagnostics.
        if !to_remove.is_empty() {
            let mut failed = self.failed.write().await;
            for outcome in &outcomes {
                if matches!(outcome, AckOutcome::Failed { .. }) {
                    failed.push(outcome.clone());
                }
            }
        }

        outcomes
    }

    /// Get the messages that need to be retried from the last sweep.
    ///
    /// Returns `(message_id, Arc<AgentMessage>)` pairs for messages marked
    /// as `Retry` in the last sweep.
    pub async fn get_retry_messages(&self, outcomes: &[AckOutcome]) -> Vec<Arc<AgentMessage>> {
        let pending = self.pending.read().await;
        outcomes
            .iter()
            .filter_map(|o| match o {
                AckOutcome::Retry { message_id, .. } => {
                    pending.get(message_id).map(|e| Arc::clone(&e.message))
                }
                _ => None,
            })
            .collect()
    }

    /// Number of messages currently awaiting acknowledgment.
    pub async fn pending_count(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Number of permanently failed messages.
    pub async fn failed_count(&self) -> usize {
        self.failed.read().await.len()
    }

    /// Get all failed outcomes (for diagnostics / dead-letter inspection).
    pub async fn failed_outcomes(&self) -> Vec<AckOutcome> {
        self.failed.read().await.clone()
    }

    /// Clear the failed outcomes list.
    pub async fn clear_failed(&self) {
        self.failed.write().await.clear();
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::types::{AgentAddress, MessagePayload, MessageTarget};

    fn addr(run: &str, agent: &str) -> AgentAddress {
        AgentAddress::new(run, agent)
    }

    fn text_msg(from: &str, to: &str) -> AgentMessage {
        AgentMessage::new(
            addr("r1", from),
            MessageTarget::Direct {
                address: addr("r2", to),
            },
            MessagePayload::Text {
                content: "test".into(),
                summary: None,
            },
        )
        .with_ack_required()
    }

    #[tokio::test]
    async fn track_and_acknowledge() {
        let tracker = PendingAckTracker::new();
        let msg = Arc::new(text_msg("a", "b"));
        let msg_id = msg.id.clone();

        tracker.track(msg).await;
        assert_eq!(tracker.pending_count().await, 1);

        let acked = tracker.acknowledge(&msg_id).await;
        assert!(acked);
        assert_eq!(tracker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn ack_unknown_returns_false() {
        let tracker = PendingAckTracker::new();
        assert!(!tracker.acknowledge("nonexistent").await);
    }

    #[tokio::test]
    async fn sweep_retries_on_timeout() {
        let config = AckConfig {
            ack_timeout: Duration::from_millis(10),
            max_retries: 3,
            sweep_interval: Duration::from_millis(5),
        };
        let tracker = PendingAckTracker::with_config(config);
        let msg = Arc::new(text_msg("a", "b"));

        tracker.track(msg).await;
        // Wait for timeout.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let outcomes = tracker.sweep().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], AckOutcome::Retry { attempt: 2, .. }));
        assert_eq!(tracker.pending_count().await, 1);
    }

    #[tokio::test]
    async fn sweep_fails_after_max_retries() {
        let config = AckConfig {
            ack_timeout: Duration::from_millis(5),
            max_retries: 1,
            sweep_interval: Duration::from_millis(5),
        };
        let tracker = PendingAckTracker::with_config(config);
        let msg = Arc::new(text_msg("a", "b"));

        tracker.track(msg).await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let outcomes = tracker.sweep().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], AckOutcome::Failed { attempts: 1, .. }));
        assert_eq!(tracker.pending_count().await, 0);
        assert_eq!(tracker.failed_count().await, 1);
    }

    #[tokio::test]
    async fn reject_records_nack() {
        let tracker = PendingAckTracker::new();
        let msg = Arc::new(text_msg("a", "b"));
        let msg_id = msg.id.clone();

        tracker.track(msg).await;
        tracker
            .reject(&msg_id, Some("bad format".into()))
            .await;

        assert_eq!(tracker.pending_count().await, 0);
        assert_eq!(tracker.failed_count().await, 1);

        let failures = tracker.failed_outcomes().await;
        match &failures[0] {
            AckOutcome::Rejected { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("bad format"));
            }
            _ => panic!("expected Rejected"),
        }
    }

    #[tokio::test]
    async fn skip_tracking_when_requires_ack_false() {
        let tracker = PendingAckTracker::new();
        let msg = Arc::new(AgentMessage::new(
            addr("r1", "a"),
            MessageTarget::Parent,
            MessagePayload::Text {
                content: "no ack needed".into(),
                summary: None,
            },
        ));
        // requires_ack defaults to false.
        assert!(!msg.requires_ack);

        tracker.track(msg).await;
        assert_eq!(tracker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn get_retry_messages_returns_matching() {
        let config = AckConfig {
            ack_timeout: Duration::from_millis(5),
            max_retries: 5,
            sweep_interval: Duration::from_millis(5),
        };
        let tracker = PendingAckTracker::with_config(config);

        let msg1 = Arc::new(text_msg("a", "b"));
        let msg2 = Arc::new(text_msg("c", "d"));

        tracker.track(msg1).await;
        tracker.track(msg2).await;

        tokio::time::sleep(Duration::from_millis(10)).await;
        let outcomes = tracker.sweep().await;
        assert_eq!(outcomes.len(), 2);

        let retries = tracker.get_retry_messages(&outcomes).await;
        assert_eq!(retries.len(), 2);
    }
}
