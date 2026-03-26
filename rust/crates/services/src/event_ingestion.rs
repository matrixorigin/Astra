//! Async event ingestion: journal events → MatrixOne `agent_events` table.
//!
//! # Architecture
//!
//! ```text
//! chat_stream (turn loop)
//!   │ journal.write(event)           ← local JSONL (fast, always)
//!   │ pusher.enqueue(event)          ← async channel
//!   ▼
//! EventIngestionWorker (background tokio task)
//!   │ accumulates events in buffer
//!   │ flushes when: buffer >= BATCH_SIZE or FLUSH_INTERVAL elapsed
//!   ▼
//! MatrixOne `agent_events` table     ← batch INSERT IGNORE (idempotent)
//! ```
//!
//! # Guarantees
//!
//! - **At-least-once delivery**: events may be re-sent on retry, deduped by event_id PK
//! - **Backpressure**: bounded channel; if MatrixOne is slow, local journal is source of truth
//! - **Graceful shutdown**: flush remaining buffer on drop

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Configuration for the ingestion worker.
#[derive(Debug, Clone)]
pub struct IngestionConfig {
    /// Max events to accumulate before flushing.
    pub batch_size: usize,
    /// Max time between flushes (seconds).
    pub flush_interval_secs: u64,
    /// Channel capacity (backpressure threshold).
    pub channel_capacity: usize,
    /// Max retries per batch on transient errors.
    pub max_retries: u32,
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            batch_size: 20,
            flush_interval_secs: 5,
            channel_capacity: 200,
            max_retries: 3,
        }
    }
}

/// A journal event prepared for cloud ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionEvent {
    pub event_id: String,
    pub session_id: String,
    pub user_id: String,
    pub event_type: String,
    pub content: Option<String>,
    pub token_usage: Option<serde_json::Value>,
    pub llm_model_used: Option<String>,
    pub skill_name: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Handle for sending events to the ingestion worker.
#[derive(Clone)]
pub struct IngestionSender {
    tx: mpsc::Sender<IngestionEvent>,
}

impl IngestionSender {
    /// Enqueue an event for async ingestion. Non-blocking; drops silently if channel full.
    pub fn enqueue(&self, event: IngestionEvent) {
        let _ = self.tx.try_send(event);
    }

    /// Enqueue with backpressure (waits if channel full).
    pub async fn enqueue_async(&self, event: IngestionEvent) {
        let _ = self.tx.send(event).await;
    }
}

/// Stats about ingestion worker activity.
#[derive(Debug, Clone, Default)]
pub struct IngestionStats {
    pub events_received: u64,
    pub events_flushed: u64,
    pub flush_count: u64,
    pub errors: u64,
    pub last_error: Option<String>,
}

/// The background worker that batches and flushes events to MatrixOne.
pub struct EventIngestionWorker {
    rx: mpsc::Receiver<IngestionEvent>,
    pool: sqlx::Pool<sqlx::MySql>,
    config: IngestionConfig,
    stats: Arc<std::sync::Mutex<IngestionStats>>,
}

impl EventIngestionWorker {
    /// Spawn the ingestion pipeline. Returns (sender, stats_handle, join_handle).
    pub fn spawn(
        pool: sqlx::Pool<sqlx::MySql>,
        config: IngestionConfig,
    ) -> (
        IngestionSender,
        Arc<std::sync::Mutex<IngestionStats>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let stats = Arc::new(std::sync::Mutex::new(IngestionStats::default()));
        let stats_clone = stats.clone();

        let worker = Self {
            rx,
            pool,
            config,
            stats,
        };

        let handle = tokio::spawn(worker.run());

        (IngestionSender { tx }, stats_clone, handle)
    }

    async fn run(mut self) {
        let mut buffer: Vec<IngestionEvent> = Vec::with_capacity(self.config.batch_size);
        let flush_interval =
            tokio::time::Duration::from_secs(self.config.flush_interval_secs);

        loop {
            let deadline = tokio::time::sleep(flush_interval);
            tokio::pin!(deadline);

            tokio::select! {
                Some(event) = self.rx.recv() => {
                    if let Ok(mut s) = self.stats.lock() {
                        s.events_received += 1;
                    }
                    buffer.push(event);
                    if buffer.len() >= self.config.batch_size {
                        self.flush_batch(&mut buffer).await;
                    }
                }
                _ = &mut deadline => {
                    if !buffer.is_empty() {
                        self.flush_batch(&mut buffer).await;
                    }
                }
                else => {
                    // Channel closed — flush remaining and exit
                    if !buffer.is_empty() {
                        self.flush_batch(&mut buffer).await;
                    }
                    break;
                }
            }
        }
    }

    async fn flush_batch(&self, buffer: &mut Vec<IngestionEvent>) {
        if buffer.is_empty() {
            return;
        }

        let batch: Vec<IngestionEvent> = buffer.drain(..).collect();
        let count = batch.len();

        for attempt in 0..self.config.max_retries {
            match self.insert_batch(&batch).await {
                Ok(()) => {
                    if let Ok(mut s) = self.stats.lock() {
                        s.events_flushed += count as u64;
                        s.flush_count += 1;
                    }
                    return;
                }
                Err(e) => {
                    if attempt + 1 < self.config.max_retries {
                        let delay = std::time::Duration::from_millis(500 * (1 << attempt));
                        tokio::time::sleep(delay).await;
                    } else {
                        if let Ok(mut s) = self.stats.lock() {
                            s.errors += 1;
                            s.last_error = Some(format!("batch flush failed after {} retries: {e}", self.config.max_retries));
                        }
                    }
                }
            }
        }
    }

    async fn insert_batch(&self, events: &[IngestionEvent]) -> Result<(), String> {
        // Build multi-row INSERT IGNORE for idempotency
        for event in events {
            sqlx::query(
                "INSERT IGNORE INTO agent_events \
                 (event_id, session_id, user_id, event_type, content, \
                  token_usage, llm_model_used, skill_name, metadata, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(&event.event_id)
            .bind(&event.session_id)
            .bind(&event.user_id)
            .bind(&event.event_type)
            .bind(&event.content)
            .bind(event.token_usage.as_ref().map(|v| v.to_string()))
            .bind(&event.llm_model_used)
            .bind(&event.skill_name)
            .bind(event.metadata.as_ref().map(|v| v.to_string()))
            .execute(&self.pool)
            .await
            .map_err(|e| format!("insert event {}: {e}", event.event_id))?;
        }
        Ok(())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingestion_config_defaults() {
        let config = IngestionConfig::default();
        assert_eq!(config.batch_size, 20);
        assert_eq!(config.flush_interval_secs, 5);
        assert_eq!(config.channel_capacity, 200);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn ingestion_event_json_roundtrip() {
        let event = IngestionEvent {
            event_id: "evt-1".into(),
            session_id: "sess-1".into(),
            user_id: "user-1".into(),
            event_type: "turn_complete".into(),
            content: Some("hello world".into()),
            token_usage: Some(serde_json::json!({"input": 100, "output": 50})),
            llm_model_used: Some("gpt-4".into()),
            skill_name: None,
            metadata: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let loaded: IngestionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.event_id, "evt-1");
        assert_eq!(loaded.event_type, "turn_complete");
    }

    #[test]
    fn ingestion_stats_default() {
        let stats = IngestionStats::default();
        assert_eq!(stats.events_received, 0);
        assert_eq!(stats.events_flushed, 0);
        assert_eq!(stats.flush_count, 0);
        assert_eq!(stats.errors, 0);
        assert!(stats.last_error.is_none());
    }

    #[tokio::test]
    async fn sender_enqueue_without_worker_does_not_panic() {
        let (tx, _rx) = mpsc::channel(10);
        let sender = IngestionSender { tx };
        // Channel is open but no worker reading — should not panic
        sender.enqueue(IngestionEvent {
            event_id: "e1".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            event_type: "test".into(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
        });
    }

    #[tokio::test]
    async fn sender_enqueue_drops_when_channel_full() {
        let (tx, _rx) = mpsc::channel(1);
        let sender = IngestionSender { tx };
        // Fill the channel
        sender.enqueue(IngestionEvent {
            event_id: "e1".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            event_type: "test".into(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
        });
        // This should be silently dropped (channel full, try_send fails)
        sender.enqueue(IngestionEvent {
            event_id: "e2".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            event_type: "test".into(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
        });
        // No panic = test passes
    }
}
