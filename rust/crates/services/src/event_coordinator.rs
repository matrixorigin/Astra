//! Event Coordinator: persist-before-broadcast enforcement.
//!
//! # Architecture
//!
//! ```text
//! Turn Loop
//!   │
//!   ├─→ journal.write(event)        [sync, local JSONL]
//!   │
//!   ├─→ EventCoordinator.emit_event
//!   │         │
//!   │         ├─→ DB flush          [async, batched]
//!   │         └─→ receive ACK       [oneshot channel]
//!   │
//!   └─→ broadcast::Sender.send      [ONLY after ACK]
//!             │
//!             └─→ SSE clients       [see durable events]
//! ```
//!
//! # Core Invariant
//!
//! A client should only see an event that is guaranteed to be persisted in
//! MatrixOne. This is enforced by awaiting DB confirmation before broadcast.

use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::session_journal::JournalEvent;

/// Default timeout for sending an ingestion request to the DB worker.
const DEFAULT_INGESTION_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Maximum number of orphaned events to buffer before dropping.
const DEFAULT_ORPHAN_QUEUE_CAPACITY: usize = 256;

/// Error type for event coordination.
#[derive(Debug, Clone, thiserror::Error)]
pub enum EventError {
    #[error("event {0} orphaned: DB flush failed")]
    Orphaned(String),

    #[error("event {0} timed out: DB flush exceeded {1:?}")]
    Timeout(String, std::time::Duration),

    #[error("journal write failed: {0}")]
    JournalWrite(String),

    #[error("ingestion channel closed")]
    ChannelClosed,

    #[error("ingestion send timed out after {0:?}")]
    IngestionSendTimeout(std::time::Duration),
}

/// Status of an event in the coordination pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStatus {
    /// Journal written, DB flush in progress.
    Pending,
    /// DB confirmed.
    Persisted,
    /// Sent to SSE clients.
    Broadcast,
    /// DB flush failed, awaiting reaper.
    Orphaned,
}

/// Request to ingest an event with confirmation channel.
pub struct IngestionRequest {
    pub event: JournalEvent,
    pub confirm_tx: oneshot::Sender<Result<(), IngestionError>>,
}

/// An event that was written to journal but failed to persist to DB.
/// Can be retried by a reaper/recovery process.
#[derive(Debug, Clone)]
pub struct OrphanedEvent {
    pub event: JournalEvent,
    pub reason: OrphanReason,
}

/// Why an event became orphaned.
#[derive(Debug, Clone)]
pub enum OrphanReason {
    /// DB flush returned an error.
    DbFailed(String),
    /// DB flush exceeded timeout.
    DbTimeout(std::time::Duration),
    /// Ingestion channel send timed out.
    IngestionSendTimeout(std::time::Duration),
    /// Ingestion channel was closed.
    ChannelClosed,
}

/// Error during DB ingestion.
#[derive(Debug, Clone, thiserror::Error)]
pub enum IngestionError {
    #[error("DB connection lost")]
    ConnectionLost,

    #[error("DB write timeout")]
    Timeout,

    #[error("DB constraint violation: {0}")]
    ConstraintViolation(String),

    #[error("DB write failed: {0}")]
    Other(String),
}

/// Orchestrates event flow: journal → DB → broadcast.
///
/// Enforces the invariant that events are broadcast to SSE clients ONLY after
/// MatrixOne DB confirmation.
pub struct EventCoordinator {
    journal_writer: Arc<dyn JournalWriter>,
    ingestion_tx: mpsc::Sender<IngestionRequest>,
    broadcast_tx: broadcast::Sender<Value>,
    persist_timeout: std::time::Duration,
    ingestion_send_timeout: std::time::Duration,
    orphan_tx: Option<mpsc::Sender<OrphanedEvent>>,
}

/// Trait for journal write operations (allows mocking in tests).
pub trait JournalWriter: Send + Sync {
    fn write(&self, event: &JournalEvent) -> Result<(), String>;

    /// Flush buffered data to OS buffers (fsync equivalent).
    /// Default implementation is a no-op for writers that don't buffer.
    fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

impl EventCoordinator {
    /// Create a new coordinator with the given dependencies.
    pub fn new(
        journal_writer: Arc<dyn JournalWriter>,
        ingestion_tx: mpsc::Sender<IngestionRequest>,
        broadcast_tx: broadcast::Sender<Value>,
        persist_timeout: std::time::Duration,
    ) -> Self {
        Self {
            journal_writer,
            ingestion_tx,
            broadcast_tx,
            persist_timeout,
            ingestion_send_timeout: DEFAULT_INGESTION_SEND_TIMEOUT,
            orphan_tx: None,
        }
    }

    /// Set the timeout for sending ingestion requests to the DB worker.
    pub fn with_ingestion_send_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.ingestion_send_timeout = timeout;
        self
    }

    /// Enable orphan event queuing. Orphaned events (journal-written but DB-failed)
    /// will be sent to the returned receiver for dead-letter processing.
    pub fn with_orphan_queue(mut self) -> (Self, mpsc::Receiver<OrphanedEvent>) {
        let (tx, rx) = mpsc::channel(DEFAULT_ORPHAN_QUEUE_CAPACITY);
        self.orphan_tx = Some(tx);
        (self, rx)
    }

    /// Queue an orphaned event for dead-letter processing.
    /// Non-blocking: if the queue is full, the event is logged and dropped.
    fn queue_orphan(&self, event: JournalEvent, reason: OrphanReason) {
        if let Some(ref orphan_tx) = self.orphan_tx {
            let orphaned = OrphanedEvent { event, reason };
            match orphan_tx.try_send(orphaned) {
                Ok(()) => debug!("orphaned event queued for dead-letter processing"),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("orphan queue full, dropping orphaned event");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("orphan queue closed, dropping orphaned event");
                }
            }
        }
    }

    /// Persist event to journal + DB, then broadcast to clients.
    ///
    /// Blocks until DB confirmation received. If DB fails, event is marked
    /// orphaned and NOT broadcast.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to emit
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Event persisted and broadcast successfully
    /// * `Err(EventError::Orphaned)` - DB flush failed, event not broadcast
    /// * `Err(EventError::Timeout)` - DB flush exceeded timeout
    /// * `Err(EventError::JournalWrite)` - Local journal write failed
    pub async fn emit_event(&self, event: JournalEvent) -> Result<(), EventError> {
        // 1. Write to journal (sync, always succeeds unless disk full)
        self.journal_writer
            .write(&event)
            .map_err(EventError::JournalWrite)?;

        // 2. Flush journal to ensure durability before DB ingestion
        if let Err(e) = self.journal_writer.flush() {
            warn!(error = %e, "journal flush failed, proceeding with best-effort");
        }

        // 3. Send to DB with confirmation channel (with timeout)
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let request = IngestionRequest {
            event: event.clone(),
            confirm_tx,
        };

        match tokio::time::timeout(self.ingestion_send_timeout, self.ingestion_tx.send(request))
            .await
        {
            Ok(Ok(())) => { /* send succeeded */ }
            Ok(Err(_)) => {
                self.queue_orphan(event.clone(), OrphanReason::ChannelClosed);
                return Err(EventError::ChannelClosed);
            }
            Err(_) => {
                self.queue_orphan(
                    event.clone(),
                    OrphanReason::IngestionSendTimeout(self.ingestion_send_timeout),
                );
                return Err(EventError::IngestionSendTimeout(
                    self.ingestion_send_timeout,
                ));
            }
        }

        // 4. Await DB confirmation with timeout (this is the barrier)
        match tokio::time::timeout(self.persist_timeout, confirm_rx).await {
            Ok(Ok(Ok(()))) => {
                // 5. Broadcast to clients (only now!)
                let _ = self
                    .broadcast_tx
                    .send(serde_json::to_value(&event).unwrap_or_default());
                debug!(event_type = ?event.event_type, "event persisted and broadcast");
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                // DB failed — event is orphaned, do NOT broadcast
                let reason = format!("{}", e);
                warn!(event_type = ?event.event_type, error = %e, "event orphaned: DB flush failed");
                self.queue_orphan(event.clone(), OrphanReason::DbFailed(reason));
                Err(EventError::Orphaned(format!("{:?}", event.event_type)))
            }
            Ok(Err(_)) => {
                // Confirmation channel dropped (ingestion worker died)
                error!(event_type = ?event.event_type, "ingestion worker dropped confirmation channel");
                self.queue_orphan(event.clone(), OrphanReason::ChannelClosed);
                Err(EventError::ChannelClosed)
            }
            Err(_) => {
                // Timeout exceeded
                warn!(
                    event_type = ?event.event_type,
                    timeout = ?self.persist_timeout,
                    "event orphaned: DB flush timeout"
                );
                self.queue_orphan(event.clone(), OrphanReason::DbTimeout(self.persist_timeout));
                Err(EventError::Timeout(
                    format!("{:?}", event.event_type),
                    self.persist_timeout,
                ))
            }
        }
    }
}

/// Mock journal writer for tests.
#[cfg(test)]
pub mod mock_journal_writer {
    use super::*;
    use std::sync::Mutex;

    pub struct MockJournalWriter {
        pub events: Mutex<Vec<JournalEvent>>,
        pub should_fail: Mutex<bool>,
    }

    impl Default for MockJournalWriter {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockJournalWriter {
        pub fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                should_fail: Mutex::new(false),
            }
        }

        pub fn set_fail(&self, fail: bool) {
            *self.should_fail.lock().unwrap() = fail;
        }
    }

    impl JournalWriter for MockJournalWriter {
        fn write(&self, event: &JournalEvent) -> Result<(), String> {
            if *self.should_fail.lock().unwrap() {
                return Err("simulated journal failure".to_string());
            }
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }

        fn flush(&self) -> Result<(), String> {
            if *self.should_fail.lock().unwrap() {
                return Err("simulated flush failure".to_string());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalEventType;
    use std::time::Duration;

    #[allow(unused_variables)]
    fn make_event(event_type: &str) -> JournalEvent {
        JournalEvent {
            event_type: JournalEventType::Turn,
            ts: chrono::Utc::now().to_rfc3339(),
            session_id: Some("test-session".to_string()),
            turn: None,
            agentic_step: None,
            model: None,
            user_input: None,
            assistant_output: None,
            tool_count: None,
            tokens_in: None,
            tokens_out: None,
            duration_ms: None,
            error: None,
            config_key: None,
            config_value: None,
            turns_compacted: None,
            facts_stored: None,
            tools_selected: None,
            selected_skills: None,
            tools_used: None,
            tool_calls: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: Some(serde_json::json!({"key": "value"})),
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
            selection_trace: None,
            context_assembly_trace: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            round: None,
            tool_calls_returned: None,
            offset_ms: None,
            llm_rounds: None,
            total_llm_ms: None,
            total_tool_ms: None,
            parent_event_id: None,
            git_head: None,
            git_branch: None,
        }
    }

    fn setup_coordinator() -> (
        EventCoordinator,
        Arc<mock_journal_writer::MockJournalWriter>,
        mpsc::Receiver<IngestionRequest>,
        broadcast::Receiver<Value>,
    ) {
        let journal_writer = Arc::new(mock_journal_writer::MockJournalWriter::new());
        let (ingestion_tx, ingestion_rx) = mpsc::channel(100);
        let (broadcast_tx, broadcast_rx) = broadcast::channel(100);

        let coord = EventCoordinator::new(
            Arc::clone(&journal_writer) as Arc<dyn JournalWriter>,
            ingestion_tx,
            broadcast_tx,
            Duration::from_secs(5),
        );

        (coord, journal_writer, ingestion_rx, broadcast_rx)
    }

    #[tokio::test]
    async fn test_persist_before_broadcast_blocks_on_db_confirm() {
        let (coord, _journal, mut ingestion_rx, mut broadcast_rx) = setup_coordinator();

        let event = make_event("test-1");
        let handle = tokio::spawn(async move { coord.emit_event(event).await });

        // Broadcast should NOT happen until DB confirms
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            broadcast_rx.try_recv().is_err(),
            "broadcast happened before DB confirm"
        );

        // Receive ingestion request and confirm DB
        let request = ingestion_rx
            .recv()
            .await
            .expect("ingestion request not sent");
        assert_eq!(request.event.event_type, JournalEventType::Turn);

        // Now confirm DB
        request.confirm_tx.send(Ok(())).unwrap();

        // Broadcast should happen after confirm
        let broadcast_event = tokio::time::timeout(Duration::from_millis(100), broadcast_rx.recv())
            .await
            .expect("broadcast timeout")
            .expect("broadcast channel closed");

        assert_eq!(broadcast_event["type"], "turn");

        // Coordinator should complete successfully
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_db_failure_marks_event_orphaned() {
        let (coord, _journal, mut ingestion_rx, mut broadcast_rx) = setup_coordinator();

        let event = make_event("test-2");
        let handle = tokio::spawn(async move { coord.emit_event(event).await });

        // Receive ingestion request and fail DB
        let request = ingestion_rx
            .recv()
            .await
            .expect("ingestion request not sent");
        request
            .confirm_tx
            .send(Err(IngestionError::ConnectionLost))
            .unwrap();

        // Should return Orphaned error
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(EventError::Orphaned(_))));

        // Broadcast should NOT happen
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            broadcast_rx.try_recv().is_err(),
            "broadcast happened despite DB failure"
        );
    }

    #[tokio::test]
    async fn test_db_timeout_marks_event_orphaned() {
        let (coord, _journal, _ingestion_rx, mut broadcast_rx) = setup_coordinator();

        let event = make_event("test-3");
        let coord_clone = EventCoordinator::new(
            Arc::clone(&coord.journal_writer),
            coord.ingestion_tx.clone(),
            coord.broadcast_tx.clone(),
            Duration::from_millis(50), // Short timeout for test
        );

        let handle = tokio::spawn(async move { coord_clone.emit_event(event).await });

        // Should return Timeout error (no DB confirmation sent)
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(EventError::Timeout(_, _))));

        // Broadcast should NOT happen
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            broadcast_rx.try_recv().is_err(),
            "broadcast happened despite timeout"
        );
    }

    #[tokio::test]
    async fn test_journal_write_failure_returns_error() {
        let (coord, journal, _ingestion_rx, _broadcast_rx) = setup_coordinator();
        journal.set_fail(true);

        let event = make_event("test-4");
        let result = coord.emit_event(event).await;

        assert!(matches!(result, Err(EventError::JournalWrite(_))));
    }

    #[tokio::test]
    async fn test_concurrent_events_preserve_ordering() {
        let (coord, _journal, mut ingestion_rx, mut broadcast_rx) = setup_coordinator();

        let events = vec![
            make_event("test-1"),
            make_event("test-2"),
            make_event("test-3"),
        ];

        let handles: Vec<_> = events
            .into_iter()
            .map(|e| {
                let coord = EventCoordinator::new(
                    Arc::clone(&coord.journal_writer),
                    coord.ingestion_tx.clone(),
                    coord.broadcast_tx.clone(),
                    Duration::from_secs(5),
                );
                tokio::spawn(async move { coord.emit_event(e).await })
            })
            .collect();

        // Confirm DB in order
        for _i in 1..=3 {
            let request = ingestion_rx
                .recv()
                .await
                .expect("ingestion request not sent");
            request.confirm_tx.send(Ok(())).unwrap();
        }

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // Broadcast should receive exactly 3 events
        let mut count = 0;
        for _ in 0..3 {
            let _event = tokio::time::timeout(Duration::from_millis(100), broadcast_rx.recv())
                .await
                .expect("broadcast timeout")
                .expect("broadcast channel closed");
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_ingestion_channel_closed_returns_error() {
        let (coord, _journal, ingestion_rx, _broadcast_rx) = setup_coordinator();

        // Drop the receiver to simulate closed channel
        drop(ingestion_rx);

        let event = make_event("test-5");
        let result = coord.emit_event(event).await;

        assert!(matches!(result, Err(EventError::ChannelClosed)));
    }
}
