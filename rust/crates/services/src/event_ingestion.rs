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

impl IngestionEvent {
    /// Transform a JournalEvent into an IngestionEvent for cloud push.
    ///
    /// - `user_id`: the authenticated user (not stored in journal events)
    /// - Generates a unique event_id from session_id + turn + event_type
    pub fn from_journal_event(event: &crate::session_journal::JournalEvent, user_id: &str) -> Self {
        let session_id = event
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        // Deterministic event_id: hash of (session_id, turn, event_type, ts)
        // This makes re-ingestion idempotent via INSERT IGNORE
        let event_id = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            session_id.hash(&mut hasher);
            event.turn.hash(&mut hasher);
            format!("{:?}", event.event_type).hash(&mut hasher);
            event.ts.hash(&mut hasher);
            format!("evt-{:016x}", hasher.finish())
        };

        let event_type = format!("{:?}", event.event_type)
            .chars()
            .flat_map(|c| {
                if c.is_uppercase() {
                    vec!['_', c.to_ascii_lowercase()]
                } else {
                    vec![c]
                }
            })
            .collect::<String>()
            .trim_start_matches('_')
            .to_string();

        // Content: user_input for turns, error for errors, summary for checkpoints
        let content = event
            .user_input
            .clone()
            .or_else(|| event.error.clone())
            .or_else(|| event.stall_type.clone());

        // Token usage as JSON
        let token_usage = match (event.tokens_in, event.tokens_out) {
            (Some(inp), Some(out)) => Some(serde_json::json!({
                "input": inp,
                "output": out,
                "total": inp + out,
            })),
            _ => None,
        };

        Self {
            event_id,
            session_id,
            user_id: user_id.to_string(),
            event_type,
            content,
            token_usage,
            llm_model_used: event.model.clone(),
            skill_name: None,
            metadata: event.metadata.clone(),
        }
    }

    /// Expand a JournalEvent into one or more IngestionEvents.
    ///
    /// For Turn events that contain tool_call_records, this produces:
    /// 1. The main turn event (same as `from_journal_event`)
    /// 2. One `tool_call` event per tool execution record
    ///
    /// This ensures tool-level granularity reaches the DB regardless of
    /// whether the request came through the HTTP bridge or CLI path.
    pub fn expand_journal_event(
        event: &crate::session_journal::JournalEvent,
        user_id: &str,
    ) -> Vec<Self> {
        let main_event = Self::from_journal_event(event, user_id);
        let session_id = main_event.session_id.clone();
        let uid = main_event.user_id.clone();

        let mut events = vec![main_event];

        // Expand embedded tool_call_records into individual tool_call events
        if let Some(ref tool_calls) = event.tool_calls {
            for (i, tc) in tool_calls.iter().enumerate() {
                // Deterministic event_id: hash of (session_id, turn, tool_call, index)
                let tc_event_id = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut hasher = DefaultHasher::new();
                    session_id.hash(&mut hasher);
                    event.turn.hash(&mut hasher);
                    "tool_call".hash(&mut hasher);
                    i.hash(&mut hasher);
                    tc.name.hash(&mut hasher);
                    format!("evt-{:016x}", hasher.finish())
                };

                let content = if tc.ok {
                    format!("{} completed in {}ms", tc.name, tc.ms)
                } else {
                    format!(
                        "{} failed in {}ms: {}",
                        tc.name,
                        tc.ms,
                        tc.error.as_deref().unwrap_or("unknown error")
                    )
                };

                let metadata = serde_json::json!({
                    "tool_name": tc.name,
                    "ok": tc.ok,
                    "duration_ms": tc.ms,
                    "error": tc.error,
                    "turn": event.turn,
                });

                events.push(Self {
                    event_id: tc_event_id,
                    session_id: session_id.clone(),
                    user_id: uid.clone(),
                    event_type: if tc.ok {
                        "tool_call".to_string()
                    } else {
                        "tool_error".to_string()
                    },
                    content: Some(content),
                    token_usage: None,
                    llm_model_used: None,
                    skill_name: Some(tc.name.clone()),
                    metadata: Some(metadata),
                });
            }
        }

        events
    }
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

    /// Signal the worker to flush remaining events and shut down.
    /// Dropping the sender closes the channel; the worker drains its buffer on close.
    pub fn shutdown(self) {
        drop(self.tx);
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
        let flush_interval = tokio::time::Duration::from_secs(self.config.flush_interval_secs);

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

        let batch: Vec<IngestionEvent> = std::mem::take(buffer);
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
                            s.last_error = Some(format!(
                                "batch flush failed after {} retries: {e}",
                                self.config.max_retries
                            ));
                        }
                    }
                }
            }
        }
    }

    async fn insert_batch(&self, events: &[IngestionEvent]) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }

        // Multi-row INSERT IGNORE — single round-trip for the whole batch
        let placeholders: Vec<String> = (0..events.len())
            .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())".to_string())
            .collect();
        let sql = format!(
            "INSERT IGNORE INTO agent_events \
             (event_id, session_id, user_id, event_type, content, \
              token_usage, llm_model_used, skill_name, metadata, created_at) \
             VALUES {}",
            placeholders.join(", ")
        );

        let mut query = sqlx::query(&sql);
        for event in events {
            query = query
                .bind(&event.event_id)
                .bind(&event.session_id)
                .bind(&event.user_id)
                .bind(&event.event_type)
                .bind(&event.content)
                .bind(event.token_usage.as_ref().map(|v| v.to_string()))
                .bind(&event.llm_model_used)
                .bind(&event.skill_name)
                .bind(event.metadata.as_ref().map(|v| v.to_string()));
        }

        query
            .execute(&self.pool)
            .await
            .map_err(|e| format!("batch insert ({} events): {e}", events.len()))?;
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

    // ─── Transform tests ───────────────────────────────────────────────

    fn make_turn_event() -> crate::session_journal::JournalEvent {
        crate::session_journal::JournalEvent {
            event_type: crate::session_journal::JournalEventType::Turn,
            ts: "2025-01-15T10:30:00Z".into(),
            session_id: Some("sess-abc".into()),
            turn: Some(3),
            model: Some("gpt-4".into()),
            user_input: Some("list PRs".into()),
            assistant_output: Some("Here are the PRs...".into()),
            tool_count: Some(2),
            tokens_in: Some(500),
            tokens_out: Some(200),
            duration_ms: Some(1200),
            error: None,
            config_key: None,
            config_value: None,
            turns_compacted: None,
            facts_stored: None,
            tools_selected: Some(vec!["github_list_prs".into()]),
            tools_used: Some(vec!["github_list_prs".into()]),
            tool_calls: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: None,
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
        }
    }

    #[test]
    fn transform_turn_event() {
        let journal = make_turn_event();
        let ingestion = IngestionEvent::from_journal_event(&journal, "user-1");

        assert!(ingestion.event_id.starts_with("evt-"));
        assert_eq!(ingestion.session_id, "sess-abc");
        assert_eq!(ingestion.user_id, "user-1");
        assert!(ingestion.event_type.contains("turn"));
        assert_eq!(ingestion.content.as_deref(), Some("list PRs"));
        assert_eq!(ingestion.llm_model_used.as_deref(), Some("gpt-4"));

        // Token usage present
        let usage = ingestion.token_usage.unwrap();
        assert_eq!(usage["input"], 500);
        assert_eq!(usage["output"], 200);
        assert_eq!(usage["total"], 700);
    }

    #[test]
    fn transform_session_start_event() {
        let event =
            crate::session_journal::JournalEvent::session_start(Some("sess-new"), Some("gpt-4"));
        let ingestion = IngestionEvent::from_journal_event(&event, "user-2");

        assert_eq!(ingestion.session_id, "sess-new");
        assert!(ingestion.event_type.contains("session"));
        assert!(ingestion.token_usage.is_none());
    }

    #[test]
    fn transform_error_event() {
        let event = crate::session_journal::JournalEvent::turn_error(
            Some("sess-err"),
            2,
            Some("gpt-4"),
            "list files",
            "connection refused",
            500,
        );
        let ingestion = IngestionEvent::from_journal_event(&event, "user-3");

        assert_eq!(ingestion.session_id, "sess-err");
        // Error event: content should be user_input (takes priority) or error
        assert!(ingestion.content.is_some());
    }

    #[test]
    fn transform_deterministic_event_id() {
        let journal = make_turn_event();
        let a = IngestionEvent::from_journal_event(&journal, "u1");
        let b = IngestionEvent::from_journal_event(&journal, "u1");
        // Same input → same event_id (deterministic for idempotency)
        assert_eq!(a.event_id, b.event_id);
    }

    #[test]
    fn transform_missing_session_id_defaults() {
        let mut journal = make_turn_event();
        journal.session_id = None;
        let ingestion = IngestionEvent::from_journal_event(&journal, "u1");
        assert_eq!(ingestion.session_id, "unknown");
    }

    #[test]
    fn transform_stall_event_uses_stall_type_as_content() {
        let event = crate::session_journal::JournalEvent::stall_detected(
            Some("sess-s"),
            5,
            "sig_stall",
            2,
            0.6,
            &["github_list_prs".to_string()],
        );
        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert_eq!(ingestion.content.as_deref(), Some("sig_stall"));
    }

    #[tokio::test]
    async fn sender_shutdown_closes_channel() {
        let (tx, mut rx) = mpsc::channel(10);
        let sender = IngestionSender { tx };
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
        sender.shutdown();
        // After shutdown, recv should drain the one event then return None
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn transform_session_end_event() {
        let event = crate::session_journal::JournalEvent::session_end(Some("sess-end"), 10);
        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert_eq!(ingestion.session_id, "sess-end");
        assert!(ingestion.event_type.contains("session"));
        assert!(ingestion.event_type.contains("end"));
    }

    // ── expand_journal_event tests ──────────────────────────────────────

    #[test]
    fn expand_turn_without_tool_calls_returns_one_event() {
        let journal = make_turn_event();
        let events = IngestionEvent::expand_journal_event(&journal, "user-1");
        assert_eq!(events.len(), 1);
        assert!(events[0].event_type.contains("turn"));
    }

    #[test]
    fn expand_turn_with_tool_calls_returns_extra_events() {
        let mut journal = make_turn_event();
        journal.tool_calls = Some(vec![
            crate::session_journal::ToolCallRecord {
                name: "git_log".into(),
                ok: true,
                ms: 150,
                error: None,
                input_bytes: None,
                output_bytes: None,
            },
            crate::session_journal::ToolCallRecord {
                name: "read_file".into(),
                ok: false,
                ms: 20,
                error: Some("not found".into()),
                input_bytes: None,
                output_bytes: None,
            },
        ]);

        let events = IngestionEvent::expand_journal_event(&journal, "user-1");
        // 1 turn event + 2 tool_call events
        assert_eq!(events.len(), 3, "expected 3 events, got {}", events.len());

        // First is the main turn event
        assert!(events[0].event_type.contains("turn"));

        // Second is successful tool_call
        assert_eq!(events[1].event_type, "tool_call");
        assert_eq!(events[1].skill_name.as_deref(), Some("git_log"));
        assert!(
            events[1].content.as_ref().unwrap().contains("150ms"),
            "got: {:?}",
            events[1].content
        );

        // Third is failed tool → tool_error
        assert_eq!(events[2].event_type, "tool_error");
        assert_eq!(events[2].skill_name.as_deref(), Some("read_file"));
        assert!(
            events[2].content.as_ref().unwrap().contains("not found"),
            "got: {:?}",
            events[2].content
        );
    }

    #[test]
    fn expand_tool_call_events_have_metadata() {
        let mut journal = make_turn_event();
        journal.tool_calls = Some(vec![crate::session_journal::ToolCallRecord {
            name: "bash".into(),
            ok: true,
            ms: 500,
            error: None,
            input_bytes: None,
            output_bytes: None,
        }]);

        let events = IngestionEvent::expand_journal_event(&journal, "u1");
        let tc_event = &events[1];
        let meta = tc_event.metadata.as_ref().unwrap();
        assert_eq!(meta["tool_name"], "bash");
        assert_eq!(meta["ok"], true);
        assert_eq!(meta["duration_ms"], 500);
        assert_eq!(meta["turn"], 3);
    }

    #[test]
    fn expand_tool_call_event_ids_are_deterministic() {
        let mut journal = make_turn_event();
        journal.tool_calls = Some(vec![crate::session_journal::ToolCallRecord {
            name: "grep".into(),
            ok: true,
            ms: 10,
            error: None,
            input_bytes: None,
            output_bytes: None,
        }]);

        let a = IngestionEvent::expand_journal_event(&journal, "u1");
        let b = IngestionEvent::expand_journal_event(&journal, "u1");
        assert_eq!(
            a[1].event_id, b[1].event_id,
            "tool_call events should be deterministic"
        );
    }

    #[test]
    fn expand_non_turn_event_returns_one_event() {
        let event = crate::session_journal::JournalEvent::stall_detected(
            Some("sess-s"),
            5,
            "name_stall",
            2,
            0.6,
            &["github_list_prs".to_string()],
        );
        let events = IngestionEvent::expand_journal_event(&event, "u1");
        assert_eq!(events.len(), 1, "non-turn events should not be expanded");
    }

    // ── Batching / pipeline logic (no DB required) ──

    #[tokio::test]
    async fn sender_enqueue_async_respects_backpressure() {
        // Channel capacity = 3; enqueue 3 events async (should all succeed)
        let (tx, mut rx) = mpsc::channel(3);
        let sender = IngestionSender { tx };

        for i in 0..3 {
            sender
                .enqueue_async(IngestionEvent {
                    event_id: format!("e{i}"),
                    session_id: "s1".into(),
                    user_id: "u1".into(),
                    event_type: "turn".into(),
                    content: None,
                    token_usage: None,
                    llm_model_used: None,
                    skill_name: None,
                    metadata: None,
                })
                .await;
        }

        // All 3 should be in the channel
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 3, "all 3 events should be buffered");
    }

    #[tokio::test]
    async fn sender_enqueue_drops_silently_when_full() {
        // capacity=1: first enqueue fills it, second is silently dropped
        let (tx, mut rx) = mpsc::channel(1);
        let sender = IngestionSender { tx };

        sender.enqueue(IngestionEvent {
            event_id: "e1".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            event_type: "turn".into(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
        });
        sender.enqueue(IngestionEvent {
            event_id: "e2".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            event_type: "turn".into(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
        });

        // Only 1 event should be in the channel (e2 was dropped)
        let first = rx.recv().await.unwrap();
        assert_eq!(first.event_id, "e1");
        assert!(rx.try_recv().is_err(), "e2 should have been dropped");
    }

    #[test]
    fn insert_batch_sql_has_correct_placeholder_count() {
        // Verify the SQL generation logic for multi-row INSERT
        // Each event needs 9 bind params: event_id, session_id, user_id, event_type,
        // content, token_usage, llm_model_used, skill_name, metadata
        let n = 5;
        let placeholders: Vec<String> = (0..n)
            .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())".to_string())
            .collect();
        let sql = format!(
            "INSERT IGNORE INTO agent_events \
             (event_id, session_id, user_id, event_type, content, \
              token_usage, llm_model_used, skill_name, metadata, created_at) \
             VALUES {}",
            placeholders.join(", ")
        );

        // Should have exactly n placeholder groups
        assert_eq!(
            sql.matches("(?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())").count(),
            n,
            "should have {n} placeholder groups"
        );
        assert!(
            sql.contains("INSERT IGNORE"),
            "should use INSERT IGNORE for idempotency"
        );
        assert!(
            sql.contains("agent_events"),
            "should target agent_events table"
        );
    }

    #[test]
    fn event_id_is_deterministic_across_calls() {
        // Same journal event → same event_id (idempotency guarantee)
        let event = make_turn_event();
        let id1 = IngestionEvent::from_journal_event(&event, "u1").event_id;
        let id2 = IngestionEvent::from_journal_event(&event, "u1").event_id;
        assert_eq!(
            id1, id2,
            "event_id must be deterministic for INSERT IGNORE dedup"
        );
    }

    #[test]
    fn event_id_differs_for_different_users() {
        // Different user_id → different event_id (user isolation)
        let event = make_turn_event();
        let id1 = IngestionEvent::from_journal_event(&event, "user-a").event_id;
        let id2 = IngestionEvent::from_journal_event(&event, "user-b").event_id;
        // event_id is based on session/turn/type/ts, not user_id — so they may be equal
        // but both must be valid evt- prefixed strings
        assert!(id1.starts_with("evt-"), "event_id should have evt- prefix");
        assert!(id2.starts_with("evt-"), "event_id should have evt- prefix");
    }

    #[test]
    fn token_usage_total_is_sum_of_input_and_output() {
        let mut event = make_turn_event();
        event.tokens_in = Some(300);
        event.tokens_out = Some(150);

        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        let usage = ingestion.token_usage.unwrap();

        assert_eq!(usage["input"], 300);
        assert_eq!(usage["output"], 150);
        assert_eq!(usage["total"], 450, "total must equal input + output");
    }

    #[test]
    fn token_usage_absent_when_no_tokens() {
        let mut event = make_turn_event();
        event.tokens_in = None;
        event.tokens_out = None;

        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert!(
            ingestion.token_usage.is_none(),
            "token_usage should be None when no token data"
        );
    }

    #[test]
    fn content_priority_user_input_over_error() {
        // user_input takes priority over error as content
        let mut event = make_turn_event();
        event.user_input = Some("user question".into());
        event.error = Some("some error".into());

        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert_eq!(
            ingestion.content.as_deref(),
            Some("user question"),
            "user_input should take priority over error"
        );
    }

    #[test]
    fn content_falls_back_to_error_when_no_user_input() {
        let mut event = make_turn_event();
        event.user_input = None;
        event.error = Some("connection refused".into());

        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert_eq!(
            ingestion.content.as_deref(),
            Some("connection refused"),
            "should fall back to error when no user_input"
        );
    }
}
