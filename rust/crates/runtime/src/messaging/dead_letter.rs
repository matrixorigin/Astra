//! Dead Letter Queue (DLQ) for messages that failed delivery.
//!
//! Messages end up here when:
//! - Ack timeout exhausted (max retries exceeded)
//! - Explicit Nack from receiver
//! - Reclaim failure in DB transport
//!
//! The DLQ is bounded — oldest entries are evicted when full (ring buffer).
//! Operators can inspect, retry, or purge dead letters.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use super::types::AgentMessage;

/// Default maximum dead letter entries before eviction.
const DEFAULT_MAX_SIZE: usize = 1024;

/// Reason a message was dead-lettered.
#[derive(Debug, Clone)]
pub enum DeadLetterReason {
    /// Ack timeout — receiver never acknowledged within the deadline.
    AckTimeout { attempts: u32 },
    /// Receiver explicitly rejected the message.
    Rejected { reason: Option<String> },
    /// Transport-level delivery failure (e.g., channel closed).
    TransportFailure { error: String },
    /// Message expired (TTL exceeded before delivery).
    Expired,
}

impl std::fmt::Display for DeadLetterReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AckTimeout { attempts } => write!(f, "ack timeout after {attempts} attempts"),
            Self::Rejected { reason } => {
                let r = reason.as_deref().unwrap_or("no reason");
                write!(f, "rejected: {r}")
            }
            Self::TransportFailure { error } => write!(f, "transport failure: {error}"),
            Self::Expired => write!(f, "message expired"),
        }
    }
}

/// A dead-lettered message with metadata.
#[derive(Debug, Clone)]
pub struct DeadLetter {
    /// The original message that failed delivery.
    pub message: Arc<AgentMessage>,
    /// Why this message was dead-lettered.
    pub reason: DeadLetterReason,
    /// When this message was dead-lettered (monotonic clock).
    pub failed_at: Instant,
    /// Number of delivery attempts made.
    pub attempts: u32,
}

/// Bounded dead letter queue — stores failed messages for inspection and retry.
///
/// Thread-safe via `RwLock`. When full, oldest entries are evicted (FIFO).
pub struct DeadLetterQueue {
    entries: RwLock<VecDeque<DeadLetter>>,
    max_size: usize,
}

impl DeadLetterQueue {
    /// Create a new DLQ with default capacity (1024).
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(VecDeque::new()),
            max_size: DEFAULT_MAX_SIZE,
        }
    }

    /// Create a new DLQ with custom capacity.
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            entries: RwLock::new(VecDeque::with_capacity(max_size.min(4096))),
            max_size: max_size.max(1), // at least 1
        }
    }

    /// Store a dead letter. Evicts oldest if at capacity.
    pub async fn store(&self, message: Arc<AgentMessage>, reason: DeadLetterReason, attempts: u32) {
        let mut entries = self.entries.write().await;
        if entries.len() >= self.max_size {
            entries.pop_front(); // evict oldest
        }
        entries.push_back(DeadLetter {
            message,
            reason,
            failed_at: Instant::now(),
            attempts,
        });
    }

    /// Number of dead letters currently stored.
    pub async fn count(&self) -> usize {
        self.entries.read().await.len()
    }

    /// List all dead letters (newest last).
    pub async fn list(&self) -> Vec<DeadLetter> {
        self.entries.read().await.iter().cloned().collect()
    }

    /// List dead letters with pagination.
    pub async fn list_page(&self, offset: usize, limit: usize) -> Vec<DeadLetter> {
        let entries = self.entries.read().await;
        entries.iter().skip(offset).take(limit).cloned().collect()
    }

    /// Remove and return a dead letter by message ID for retry.
    ///
    /// Returns the original message if found, or None.
    pub async fn take_for_retry(&self, message_id: &str) -> Option<Arc<AgentMessage>> {
        let mut entries = self.entries.write().await;
        if let Some(pos) = entries.iter().position(|dl| dl.message.id == message_id) {
            entries.remove(pos).map(|dl| dl.message)
        } else {
            None
        }
    }

    /// Remove all dead letters and return them (for bulk retry or export).
    pub async fn drain_all(&self) -> Vec<DeadLetter> {
        let mut entries = self.entries.write().await;
        entries.drain(..).collect()
    }

    /// Purge dead letters older than the given duration.
    pub async fn purge_older_than(&self, max_age: std::time::Duration) -> usize {
        let cutoff = Instant::now() - max_age;
        let mut entries = self.entries.write().await;
        let before = entries.len();
        entries.retain(|dl| dl.failed_at >= cutoff);
        before - entries.len()
    }

    /// Purge all dead letters.
    pub async fn purge_all(&self) -> usize {
        let mut entries = self.entries.write().await;
        let count = entries.len();
        entries.clear();
        count
    }

    /// Get a summary of dead letter reasons (for diagnostics).
    pub async fn reason_summary(&self) -> DeadLetterSummary {
        let entries = self.entries.read().await;
        let mut summary = DeadLetterSummary::default();
        for dl in entries.iter() {
            match &dl.reason {
                DeadLetterReason::AckTimeout { .. } => summary.ack_timeouts += 1,
                DeadLetterReason::Rejected { .. } => summary.rejections += 1,
                DeadLetterReason::TransportFailure { .. } => summary.transport_failures += 1,
                DeadLetterReason::Expired => summary.expired += 1,
            }
        }
        summary.total = entries.len();
        summary
    }
}

impl Default for DeadLetterQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of dead letter reasons.
#[derive(Debug, Default, Clone)]
pub struct DeadLetterSummary {
    pub total: usize,
    pub ack_timeouts: usize,
    pub rejections: usize,
    pub transport_failures: usize,
    pub expired: usize,
}

impl std::fmt::Display for DeadLetterSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DLQ: {} total (ack_timeout={}, rejected={}, transport={}, expired={})",
            self.total, self.ack_timeouts, self.rejections, self.transport_failures, self.expired
        )
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::types::{AgentAddress, MessagePayload, MessageTarget};

    fn test_msg(id: &str) -> Arc<AgentMessage> {
        let mut msg = AgentMessage::new(
            AgentAddress::new("r1", "sender"),
            MessageTarget::Direct {
                address: AgentAddress::new("r1", "receiver"),
            },
            MessagePayload::Text {
                content: format!("msg-{id}"),
                summary: None,
            },
        );
        msg.id = id.to_string();
        Arc::new(msg)
    }

    #[tokio::test]
    async fn store_and_count() {
        let dlq = DeadLetterQueue::new();
        assert_eq!(dlq.count().await, 0);

        dlq.store(test_msg("1"), DeadLetterReason::Expired, 0).await;
        dlq.store(
            test_msg("2"),
            DeadLetterReason::AckTimeout { attempts: 3 },
            3,
        )
        .await;
        assert_eq!(dlq.count().await, 2);
    }

    #[tokio::test]
    async fn evicts_oldest_when_full() {
        let dlq = DeadLetterQueue::with_capacity(3);

        for i in 0..5 {
            dlq.store(test_msg(&i.to_string()), DeadLetterReason::Expired, 0)
                .await;
        }

        assert_eq!(dlq.count().await, 3);
        let entries = dlq.list().await;
        // Should have messages 2, 3, 4 (oldest 0, 1 evicted).
        assert_eq!(entries[0].message.id, "2");
        assert_eq!(entries[1].message.id, "3");
        assert_eq!(entries[2].message.id, "4");
    }

    #[tokio::test]
    async fn take_for_retry_removes_entry() {
        let dlq = DeadLetterQueue::new();
        dlq.store(test_msg("a"), DeadLetterReason::Expired, 0).await;
        dlq.store(test_msg("b"), DeadLetterReason::Expired, 0).await;

        let msg = dlq.take_for_retry("a").await;
        assert!(msg.is_some());
        assert_eq!(msg.unwrap().id, "a");
        assert_eq!(dlq.count().await, 1);

        // Taking again returns None.
        assert!(dlq.take_for_retry("a").await.is_none());
    }

    #[tokio::test]
    async fn purge_all_clears_queue() {
        let dlq = DeadLetterQueue::new();
        for i in 0..10 {
            dlq.store(test_msg(&i.to_string()), DeadLetterReason::Expired, 0)
                .await;
        }
        let purged = dlq.purge_all().await;
        assert_eq!(purged, 10);
        assert_eq!(dlq.count().await, 0);
    }

    #[tokio::test]
    async fn reason_summary_categorizes() {
        let dlq = DeadLetterQueue::new();
        dlq.store(
            test_msg("1"),
            DeadLetterReason::AckTimeout { attempts: 3 },
            3,
        )
        .await;
        dlq.store(
            test_msg("2"),
            DeadLetterReason::AckTimeout { attempts: 5 },
            5,
        )
        .await;
        dlq.store(
            test_msg("3"),
            DeadLetterReason::Rejected {
                reason: Some("bad".into()),
            },
            1,
        )
        .await;
        dlq.store(
            test_msg("4"),
            DeadLetterReason::TransportFailure {
                error: "closed".into(),
            },
            1,
        )
        .await;
        dlq.store(test_msg("5"), DeadLetterReason::Expired, 0).await;

        let summary = dlq.reason_summary().await;
        assert_eq!(summary.total, 5);
        assert_eq!(summary.ack_timeouts, 2);
        assert_eq!(summary.rejections, 1);
        assert_eq!(summary.transport_failures, 1);
        assert_eq!(summary.expired, 1);
    }

    #[tokio::test]
    async fn list_page_pagination() {
        let dlq = DeadLetterQueue::new();
        for i in 0..10 {
            dlq.store(test_msg(&i.to_string()), DeadLetterReason::Expired, 0)
                .await;
        }

        let page1 = dlq.list_page(0, 3).await;
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].message.id, "0");

        let page2 = dlq.list_page(3, 3).await;
        assert_eq!(page2.len(), 3);
        assert_eq!(page2[0].message.id, "3");

        let page4 = dlq.list_page(9, 3).await;
        assert_eq!(page4.len(), 1);
    }

    #[tokio::test]
    async fn drain_all_empties_queue() {
        let dlq = DeadLetterQueue::new();
        for i in 0..5 {
            dlq.store(test_msg(&i.to_string()), DeadLetterReason::Expired, 0)
                .await;
        }
        let drained = dlq.drain_all().await;
        assert_eq!(drained.len(), 5);
        assert_eq!(dlq.count().await, 0);
    }

    #[tokio::test]
    async fn dead_letter_reason_display() {
        let r1 = DeadLetterReason::AckTimeout { attempts: 3 };
        assert!(r1.to_string().contains("3 attempts"));

        let r2 = DeadLetterReason::Rejected {
            reason: Some("bad data".into()),
        };
        assert!(r2.to_string().contains("bad data"));

        let r3 = DeadLetterReason::TransportFailure {
            error: "channel closed".into(),
        };
        assert!(r3.to_string().contains("channel closed"));

        let r4 = DeadLetterReason::Expired;
        assert!(r4.to_string().contains("expired"));
    }
}
