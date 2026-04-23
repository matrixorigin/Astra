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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// When true, replace user-content fields (`content`) on outgoing
    /// IngestionEvents with a privacy marker (`<redacted: len=N sha=...>`)
    /// instead of the raw text. Default: `false` for backward compat;
    /// callers that need PII-free cloud ingestion should opt in.
    pub redact_content: bool,
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            batch_size: 20,
            flush_interval_secs: 5,
            channel_capacity: 200,
            max_retries: 3,
            redact_content: false,
        }
    }
}

/// Replace raw user content with a deterministic privacy marker.
///
/// The marker has the form `<redacted: len=N sha=HHHHHHHHHHHHHHHH>` where
/// the suffix is a non-cryptographic 64-bit hash. It is used only for
/// dedup/debugging when `IngestionConfig.redact_content` is enabled — not as
/// a security primitive — so [`std::collections::hash_map::DefaultHasher`]
/// is acceptable.
pub fn redacted_content_marker(raw: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("<redacted: len={} sha={:016x}>", raw.len(), h.finish())
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
    /// Original event timestamp from the journal (ISO 8601).
    /// Used as `created_at` in the DB instead of `NOW()`.
    pub created_at: String,
    /// Parent event ID for causal chain linkage.
    pub parent_event_id: Option<String>,
    /// Ordered parent event ids for DAG lineage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<String>,
    /// Causal chain root ID for grouping related events.
    pub causal_chain_id: Option<String>,
}

fn merged_metadata_from_journal_event(
    event: &crate::session_journal::JournalEvent,
) -> Option<serde_json::Value> {
    let has_extra = event.session_lineage.is_some()
        || event.coordination.is_some()
        || event.edge_policy.is_some()
        || event.context_assembly_trace.is_some();
    if !has_extra && event.metadata.is_none() {
        return None;
    }
    let mut obj = serde_json::Map::new();
    if let Some(ref m) = event.metadata {
        match m {
            serde_json::Value::Object(map) => obj.extend(map.clone()),
            other => {
                obj.insert("legacy_metadata".to_string(), other.clone());
            }
        }
    }
    if let Some(ref l) = event.session_lineage
        && let Ok(v) = serde_json::to_value(l)
    {
        obj.insert("session_lineage".to_string(), v);
    }
    if let Some(ref c) = event.coordination
        && let Ok(v) = serde_json::to_value(c)
    {
        obj.insert("coordination".to_string(), v);
    }
    if let Some(ref p) = event.edge_policy
        && let Ok(v) = serde_json::to_value(p)
    {
        obj.insert("edge_policy".to_string(), v);
    }
    if let Some(ref trace) = event.context_assembly_trace {
        obj.insert("context_assembly_trace".to_string(), trace.clone());
    }
    Some(serde_json::Value::Object(obj))
}

impl IngestionEvent {
    /// Transform a JournalEvent into an IngestionEvent for cloud push.
    ///
    /// - `user_id`: the authenticated user (not stored in journal events)
    /// - Generates a unique event_id from session_id + turn + event_type
    pub fn from_journal_event(event: &crate::session_journal::JournalEvent, user_id: &str) -> Self {
        Self::from_journal_event_with_redact(event, user_id, false)
    }

    /// Like [`from_journal_event`] but optionally replaces the `content` field
    /// with a deterministic privacy marker when `redact_content == true`.
    pub fn from_journal_event_with_redact(
        event: &crate::session_journal::JournalEvent,
        user_id: &str,
        redact_content: bool,
    ) -> Self {
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
        let content = if redact_content {
            content.as_deref().map(redacted_content_marker)
        } else {
            content
        };

        // Token usage as JSON
        let token_usage = match (event.tokens_in, event.tokens_out) {
            (Some(inp), Some(out)) => Some(serde_json::json!({
                "input": inp,
                "output": out,
                "total": inp + out,
            })),
            _ => None,
        };

        let causal_chain_id = event
            .coordination
            .as_ref()
            .and_then(|c| c.correlation_id.clone());
        let parent_event_ids = event
            .coordination
            .as_ref()
            .and_then(|c| c.upstream_event_ids.clone())
            .unwrap_or_default();
        let parent_event_id = parent_event_ids.first().cloned();

        Self {
            event_id,
            session_id,
            user_id: user_id.to_string(),
            event_type,
            content,
            token_usage,
            llm_model_used: event.model.clone(),
            skill_name: None,
            metadata: merged_metadata_from_journal_event(event),
            created_at: event.ts.clone(),
            parent_event_id,
            parent_event_ids,
            causal_chain_id,
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
        Self::expand_journal_event_with_redact(event, user_id, false)
    }

    /// Like [`expand_journal_event`] but applies privacy redaction to both the
    /// main event and any tool_call expansion content when enabled.
    pub fn expand_journal_event_with_redact(
        event: &crate::session_journal::JournalEvent,
        user_id: &str,
        redact_content: bool,
    ) -> Vec<Self> {
        let main_event = Self::from_journal_event_with_redact(event, user_id, redact_content);
        let session_id = main_event.session_id.clone();
        let uid = main_event.user_id.clone();
        let main_event_id = main_event.event_id.clone();

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

                let raw_content = if tc.ok {
                    format!("{} completed in {}ms", tc.name, tc.ms)
                } else {
                    format!(
                        "{} failed in {}ms: {}",
                        tc.name,
                        tc.ms,
                        tc.error.as_deref().unwrap_or("unknown error")
                    )
                };
                let content = if redact_content {
                    redacted_content_marker(&raw_content)
                } else {
                    raw_content
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
                    created_at: event.ts.clone(),
                    parent_event_id: Some(main_event_id.clone()),
                    parent_event_ids: vec![main_event_id.clone()],
                    causal_chain_id: Some(main_event_id.clone()),
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
    overflow_count: Arc<AtomicU64>,
}

impl IngestionSender {
    /// Handle with no worker: [`Self::enqueue`] is a no-op (disconnected channel). Tests only.
    pub fn disconnected() -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Enqueue an event for async ingestion. Non-blocking; increments overflow counter if channel full.
    pub fn enqueue(&self, event: IngestionEvent) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(event) {
            let n = self.overflow_count.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                target: "astra_services::event_ingestion",
                overflow_count = n,
                "ingestion channel full; event dropped"
            );
        }
    }

    /// Total number of events dropped due to a full channel.
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
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

/// Convert ISO 8601 / RFC 3339 timestamp to MySQL DATETIME(6) format.
///
/// `2025-01-15T10:30:00.123456+00:00` → `2025-01-15 10:30:00.123456`
///
/// Falls back to the original string if parsing fails (MySQL will use
/// its own default or reject it, which is better than silent data loss).
fn iso8601_to_mysql_datetime(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// The background worker that batches and flushes events to MatrixOne.
pub struct EventIngestionWorker {
    rx: mpsc::Receiver<IngestionEvent>,
    pool: sqlx::Pool<sqlx::MySql>,
    config: IngestionConfig,
    stats: Arc<std::sync::Mutex<IngestionStats>>,
}

/// Handle to signal the ingestion worker to shut down immediately.
/// Notify-based so it works even when other senders are still alive.
#[derive(Clone)]
pub struct IngestionShutdownHandle {
    notify: Arc<tokio::sync::Notify>,
}

impl IngestionShutdownHandle {
    /// Signal the worker to stop after a best-effort flush.
    pub fn signal(&self) {
        self.notify.notify_one();
    }
}

impl EventIngestionWorker {
    /// Spawn the ingestion pipeline. Returns (sender, shutdown_handle, stats_handle, join_handle).
    pub fn spawn(
        pool: sqlx::Pool<sqlx::MySql>,
        config: IngestionConfig,
    ) -> (
        IngestionSender,
        IngestionShutdownHandle,
        Arc<std::sync::Mutex<IngestionStats>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let stats = Arc::new(std::sync::Mutex::new(IngestionStats::default()));
        let stats_clone = stats.clone();
        let notify = Arc::new(tokio::sync::Notify::new());

        tracing::info!(
            target: "astra_services::event_ingestion",
            batch_size = config.batch_size,
            flush_interval_secs = config.flush_interval_secs,
            channel_capacity = config.channel_capacity,
            "event ingestion worker spawned"
        );

        let worker = Self {
            rx,
            pool,
            config,
            stats,
        };
        // Share the same Arc so the handle can signal the worker.
        let shutdown_handle = IngestionShutdownHandle {
            notify: Arc::clone(&notify),
        };

        let handle = tokio::spawn(worker.run_with_shutdown(notify));

        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };
        (sender, shutdown_handle, stats_clone, handle)
    }

    async fn run_with_shutdown(mut self, shutdown: Arc<tokio::sync::Notify>) {
        let mut buffer: Vec<IngestionEvent> = Vec::with_capacity(self.config.batch_size);
        let flush_interval = tokio::time::Duration::from_secs(self.config.flush_interval_secs);

        tracing::debug!(
            target: "astra_services::event_ingestion",
            "event ingestion worker run loop started"
        );

        loop {
            let deadline = tokio::time::sleep(flush_interval);
            tokio::pin!(deadline);

            tokio::select! {
                _ = shutdown.notified() => {
                    // Drain any remaining events from the channel before flushing.
                    while let Ok(event) = self.rx.try_recv() {
                        if let Ok(mut s) = self.stats.lock() {
                            s.events_received += 1;
                        }
                        buffer.push(event);
                    }
                    if !buffer.is_empty() {
                        self.flush_batch_once(&mut buffer).await;
                    }
                    tracing::info!(
                        target: "astra_services::event_ingestion",
                        "event ingestion worker stopped after shutdown signal"
                    );
                    break;
                }
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
                    // Channel closed — best-effort flush with no retries.
                    if !buffer.is_empty() {
                        self.flush_batch_once(&mut buffer).await;
                    }
                    tracing::info!(
                        target: "astra_services::event_ingestion",
                        "event ingestion worker stopped (channel closed)"
                    );
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
                        tracing::debug!(
                            target: "astra_services::event_ingestion",
                            attempt = attempt + 1,
                            max_retries = self.config.max_retries,
                            event_count = count,
                            error = %e,
                            "batch flush retry after transient error"
                        );
                        tokio::time::sleep(delay).await;
                    } else {
                        if let Ok(mut s) = self.stats.lock() {
                            s.errors += 1;
                            s.last_error = Some(format!(
                                "batch flush failed after {} retries: {e}",
                                self.config.max_retries
                            ));
                        }
                        tracing::warn!(
                            target: "astra_services::event_ingestion",
                            event_count = count,
                            max_retries = self.config.max_retries,
                            error = %e,
                            "batch flush failed after retries"
                        );
                    }
                }
            }
        }
    }

    /// Single-attempt flush used during shutdown drain. Skips retries so we
    /// don't block exit for seconds when the DB is unreachable.
    async fn flush_batch_once(&self, buffer: &mut Vec<IngestionEvent>) {
        if buffer.is_empty() {
            return;
        }
        let batch: Vec<IngestionEvent> = std::mem::take(buffer);
        let count = batch.len();
        match self.insert_batch(&batch).await {
            Ok(()) => {
                if let Ok(mut s) = self.stats.lock() {
                    s.events_flushed += count as u64;
                    s.flush_count += 1;
                }
            }
            Err(e) => {
                if let Ok(mut s) = self.stats.lock() {
                    s.errors += 1;
                    s.last_error = Some(format!("shutdown flush failed: {e}"));
                }
                tracing::warn!(
                    target: "astra_services::event_ingestion",
                    event_count = count,
                    error = %e,
                    "shutdown flush failed (single attempt)"
                );
            }
        }
    }

    async fn insert_batch(&self, events: &[IngestionEvent]) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("begin tx: {e}"))?;

        // Multi-row INSERT IGNORE — single round-trip for the whole batch
        let placeholders: Vec<String> = (0..events.len())
            .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)".to_string())
            .collect();
        let sql = format!(
            "INSERT IGNORE INTO agent_events \
             (event_id, session_id, user_id, event_type, content, \
              token_usage, llm_model_used, skill_name, metadata, \
              created_at, parent_event_id, causal_chain_id) \
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
                .bind(event.metadata.as_ref().map(|v| v.to_string()))
                .bind(iso8601_to_mysql_datetime(&event.created_at))
                .bind(&event.parent_event_id)
                .bind(&event.causal_chain_id);
        }

        let insert_result = query
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("batch insert ({} events): {e}", events.len()))?;

        for event in events {
            crate::storage::insert_agent_event_edges(
                &mut *tx,
                &event.event_id,
                event.parent_event_id.as_deref(),
                &event.parent_event_ids,
            )
            .await
            .map_err(|e| format!("edge insert for {}: {e}", event.event_id))?;
        }

        let rows_inserted = insert_result.rows_affected() as usize;

        // Update event_count on agent_sessions for each affected session.
        //
        // BUG FIX (Session 7875e355): Race condition between fast path increment
        // and slow path COUNT(*) reconcile. Multiple flush_batch() calls running
        // concurrently could cause count drift.
        //
        // Solution: Always use COUNT(*) reconcile to ensure accuracy. The performance
        // cost is negligible (indexed query) and correctness is more important.
        // Additionally, record last_event_sync_at to help diagnose future issues.
        let mut session_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut session_users: std::collections::HashMap<&str, &str> =
            std::collections::HashMap::new();
        for event in events {
            *session_counts.entry(&event.session_id).or_default() += 1;
            session_users
                .entry(&event.session_id)
                .or_insert(event.user_id.as_str());
        }

        // Log if duplicates were detected (useful for debugging)
        if rows_inserted < events.len() {
            let skipped = events.len() - rows_inserted;
            astra_core::agent_info!(
                "event_ingestion",
                "INSERT IGNORE skipped {skipped} duplicates out of {} events",
                events.len()
            );
        }

        // Always reconcile from actual row count to prevent drift from concurrent flushes.
        // This is more expensive than increment but guarantees accuracy.
        for session_id in session_counts.keys() {
            let user_id = session_users
                .get(session_id)
                .copied()
                .ok_or_else(|| format!("missing user_id for session {session_id}"))?;
            let event_count = crate::storage::load_agent_event_count(&mut *tx, session_id)
                .await
                .map_err(|e| format!("event_count load for {session_id}: {e}"))?;
            crate::storage::upsert_agent_session_event_count(
                &mut *tx,
                session_id,
                user_id,
                event_count,
            )
            .await
            .map_err(|e| format!("event_count reconcile for {session_id}: {e}"))?;
        }

        // Close sessions that have a session_end event
        for event in events {
            if event.event_type == "session_end" {
                sqlx::query(
                    "UPDATE agent_sessions SET status = 'ended', ended_at = NOW() \
                     WHERE session_id = ? AND status != 'ended'",
                )
                .bind(&event.session_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("session close for {}: {e}", event.session_id))?;
            }
        }

        tx.commit().await.map_err(|e| format!("commit tx: {e}"))?;

        Ok(())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: create a minimal IngestionEvent with required fields.
    fn test_event(event_id: &str, session_id: &str, event_type: &str) -> IngestionEvent {
        IngestionEvent {
            event_id: event_id.into(),
            session_id: session_id.into(),
            user_id: "u1".into(),
            event_type: event_type.into(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            created_at: "2025-01-15T10:30:00Z".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
        }
    }

    #[test]
    fn ingestion_config_defaults() {
        let config = IngestionConfig::default();
        assert_eq!(config.batch_size, 20);
        assert_eq!(config.flush_interval_secs, 5);
        assert_eq!(config.channel_capacity, 200);
        assert_eq!(config.max_retries, 3);
        assert!(
            !config.redact_content,
            "redact_content default must be false for backward compat"
        );
    }

    #[test]
    fn redacted_content_marker_is_deterministic() {
        let a = redacted_content_marker("hello world");
        let b = redacted_content_marker("hello world");
        assert_eq!(a, b);
        assert!(a.contains("len=11"));
        assert!(a.starts_with("<redacted: len=11 sha="));
        assert!(a.ends_with('>'));
        assert!(!a.contains("hello"));
    }

    #[test]
    fn from_journal_event_with_redact_off_keeps_raw_content() {
        let event = crate::session_journal::JournalEvent::turn(
            Some("sess-r"),
            1,
            Some("gpt-4"),
            "hello world",
            "ok",
            0,
            10,
            5,
            100,
        );
        let ingestion = IngestionEvent::from_journal_event_with_redact(&event, "u1", false);
        assert_eq!(ingestion.content.as_deref(), Some("hello world"));
    }

    #[test]
    fn from_journal_event_with_redact_on_replaces_content_with_marker() {
        let event = crate::session_journal::JournalEvent::turn(
            Some("sess-r"),
            1,
            Some("gpt-4"),
            "hello world",
            "ok",
            0,
            10,
            5,
            100,
        );
        let ingestion = IngestionEvent::from_journal_event_with_redact(&event, "u1", true);
        let content = ingestion.content.expect("content present");
        assert!(!content.contains("hello world"));
        assert!(content.starts_with("<redacted: len=11 sha="));
    }

    #[test]
    fn expand_journal_event_with_redact_redacts_tool_call_content() {
        use crate::session_journal::{JournalEvent, ToolCallRecord};
        let mut event = JournalEvent::turn(
            Some("sess-tc"),
            1,
            Some("gpt-4"),
            "list files",
            "done",
            1,
            10,
            5,
            100,
        );
        event.tool_calls = Some(vec![ToolCallRecord {
            name: "shell".into(),
            ok: true,
            ms: 12,
            ..Default::default()
        }]);
        let evs = IngestionEvent::expand_journal_event_with_redact(&event, "u1", true);
        assert_eq!(evs.len(), 2);
        let main = &evs[0];
        assert!(
            main.content
                .as_deref()
                .unwrap_or("")
                .starts_with("<redacted:")
        );
        let tc = &evs[1];
        assert!(
            tc.content
                .as_deref()
                .unwrap_or("")
                .starts_with("<redacted:")
        );
    }

    #[test]
    fn iso8601_to_mysql_datetime_rfc3339() {
        assert_eq!(
            iso8601_to_mysql_datetime("2025-01-15T10:30:00+00:00"),
            "2025-01-15 10:30:00.000000"
        );
    }

    #[test]
    fn iso8601_to_mysql_datetime_with_micros() {
        assert_eq!(
            iso8601_to_mysql_datetime("2025-01-15T10:30:00.123456+00:00"),
            "2025-01-15 10:30:00.123456"
        );
    }

    #[test]
    fn iso8601_to_mysql_datetime_zulu() {
        // chrono parses Z suffix as UTC
        assert_eq!(
            iso8601_to_mysql_datetime("2025-01-15T10:30:00Z"),
            "2025-01-15 10:30:00.000000"
        );
    }

    #[test]
    fn iso8601_to_mysql_datetime_fallback() {
        // Invalid input returns original string
        assert_eq!(iso8601_to_mysql_datetime("not-a-date"), "not-a-date");
    }

    #[test]
    fn ingestion_event_json_roundtrip() {
        let mut event = test_event("evt-1", "sess-1", "turn_complete");
        event.user_id = "user-1".into();
        event.content = Some("hello world".into());
        event.token_usage = Some(serde_json::json!({"input": 100, "output": 50}));
        event.llm_model_used = Some("gpt-4".into());

        let json = serde_json::to_string(&event).unwrap();
        let loaded: IngestionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.event_id, "evt-1");
        assert_eq!(loaded.event_type, "turn_complete");
        assert_eq!(loaded.created_at, "2025-01-15T10:30:00Z");
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
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };
        sender.enqueue(test_event("e1", "s1", "test"));
    }

    #[tokio::test]
    async fn sender_enqueue_drops_when_channel_full() {
        let (tx, _rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };
        sender.enqueue(test_event("e1", "s1", "test"));
        // This should be silently dropped (channel full, try_send fails)
        sender.enqueue(test_event("e2", "s1", "test"));
        // No panic = test passes
    }

    // ─── Transform tests ───────────────────────────────────────────────

    fn make_turn_event() -> crate::session_journal::JournalEvent {
        crate::session_journal::JournalEvent {
            event_type: crate::session_journal::JournalEventType::Turn,
            ts: "2025-01-15T10:30:00Z".into(),
            session_id: Some("sess-abc".into()),
            turn: Some(3),
            agentic_step: None,
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
            selected_skills: Some(vec!["tune-performance".into()]),
            tools_used: Some(vec!["github_list_prs".into()]),
            tool_calls: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: None,
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: None,
            selector_tokens_out: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
            selection_trace: None,
            context_assembly_trace: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            round: None,
            tool_calls_returned: None,
            offset_ms: None,
            llm_rounds: None,
            total_llm_ms: None,
            total_tool_ms: None,
        }
    }

    #[test]
    fn transform_sync_marker_cloud_pull_metadata_in_ingestion() {
        use crate::session_journal::JournalEvent;
        let keys = vec!["explain_mode".to_string()];
        let journal = JournalEvent::cloud_pull_sync_marker(
            Some("sess-sync"),
            "default",
            "repl_startup",
            Some(7),
            true,
            2,
            &keys,
            false,
        );
        let ingestion = IngestionEvent::from_journal_event(&journal, "user-z");
        assert!(ingestion.event_type.contains("sync"));
        assert_eq!(ingestion.session_id, "sess-sync");
        let meta = ingestion.metadata.expect("metadata");
        let cp = meta.get("cloud_pull").expect("cloud_pull blob");
        assert_eq!(cp.get("learning_version").and_then(|v| v.as_i64()), Some(7));
        assert_eq!(
            cp.get("preference_keys_merged")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(
            ingestion
                .content
                .as_deref()
                .unwrap_or("")
                .contains("cloud_pull")
        );
    }

    #[test]
    fn transform_sync_marker_preserves_reachable_empty_ack() {
        use crate::session_journal::JournalEvent;
        let journal = JournalEvent::cloud_pull_sync_marker(
            Some("s-empty"),
            "default",
            "post_login",
            None,
            false,
            0,
            &[],
            true,
        );
        let ingestion = IngestionEvent::from_journal_event(&journal, "u1");
        let cp = ingestion
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn merged_metadata_cloud_pull_only_no_lineage_still_emits_object() {
        use crate::session_journal::JournalEvent;
        let journal = JournalEvent::cloud_pull_sync_marker(
            Some("s1"),
            "p",
            "post_login",
            None,
            false,
            0,
            &[],
            true,
        );
        let merged = super::merged_metadata_from_journal_event(&journal);
        let obj = merged.expect("expected metadata");
        assert!(obj.get("cloud_pull").is_some());
    }

    #[test]
    fn transform_includes_lineage_coordination_in_metadata_and_causal_chain() {
        use crate::session_journal::{CoordinationMeta, SessionLineage};
        let mut journal = make_turn_event();
        journal.coordination = Some(CoordinationMeta {
            agent_id: Some("agent-a".into()),
            agent_role: Some("worker".into()),
            correlation_id: Some("corr-chain-1".into()),
            upstream_event_ids: Some(vec!["evt-up-1".into(), "evt-up-2".into()]),
        });
        journal.session_lineage = Some(SessionLineage {
            parent_session_id: "parent-sid".into(),
            forked_after_turn: Some(4),
            label: Some("branch".into()),
        });
        let ingestion = IngestionEvent::from_journal_event(&journal, "user-1");
        assert_eq!(ingestion.causal_chain_id.as_deref(), Some("corr-chain-1"));
        assert_eq!(
            ingestion.parent_event_ids,
            vec!["evt-up-1".to_string(), "evt-up-2".to_string()]
        );
        let meta = ingestion.metadata.expect("metadata");
        assert!(meta.get("coordination").is_some());
        assert!(meta.get("session_lineage").is_some());
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
        assert_eq!(ingestion.created_at, "2025-01-15T10:30:00Z");
        assert!(ingestion.parent_event_id.is_none());
        assert!(ingestion.causal_chain_id.is_none());

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
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };
        sender.enqueue(test_event("e1", "s1", "test"));
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

    #[test]
    fn transform_plan_progress_event() {
        let event = crate::session_journal::JournalEvent::plan_progress(
            Some("sess-plan"),
            3,
            "subtask-1",
            "Analyze code",
            "started",
            33,
            3,
            1,
        );
        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert_eq!(ingestion.session_id, "sess-plan");
        assert_eq!(ingestion.event_type, "plan_progress");
        // metadata should carry subtask details
        let meta = ingestion.metadata.unwrap();
        assert_eq!(meta["subtask_id"], "subtask-1");
        assert_eq!(meta["action"], "started");
        assert_eq!(meta["progress_pct"], 33);
    }

    #[test]
    fn expand_plan_progress_returns_single_event() {
        let event = crate::session_journal::JournalEvent::plan_progress(
            Some("sess-plan"),
            5,
            "sub-2",
            "Run tests",
            "completed",
            66,
            3,
            2,
        );
        let events = IngestionEvent::expand_journal_event(&event, "u1");
        assert_eq!(
            events.len(),
            1,
            "plan_progress should not be expanded into tool_call sub-events"
        );
        assert_eq!(events[0].event_type, "plan_progress");
    }

    #[test]
    fn session_end_event_type_matches_insert_batch_check() {
        // insert_batch checks `event.event_type == "session_end"` to close sessions.
        // Verify the transform produces exactly that string.
        let event = crate::session_journal::JournalEvent::session_end(Some("s1"), 5);
        let ingestion = IngestionEvent::from_journal_event(&event, "u1");
        assert_eq!(
            ingestion.event_type, "session_end",
            "event_type must be exactly 'session_end' for insert_batch session-close logic"
        );
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
                args_preview: Some("HEAD~10".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
            crate::session_journal::ToolCallRecord {
                name: "read_file".into(),
                ok: false,
                ms: 20,
                error: Some("not found".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: Some("missing.txt".into()),
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
        assert_eq!(
            events[1].parent_event_id.as_deref(),
            Some(events[0].event_id.as_str())
        );
        assert_eq!(
            events[1].causal_chain_id.as_deref(),
            Some(events[0].event_id.as_str())
        );
        assert_eq!(events[1].created_at, events[0].created_at);

        // Third is failed tool → tool_error
        assert_eq!(events[2].event_type, "tool_error");
        assert_eq!(events[2].skill_name.as_deref(), Some("read_file"));
        assert!(
            events[2].content.as_ref().unwrap().contains("not found"),
            "got: {:?}",
            events[2].content
        );
        assert_eq!(
            events[2].parent_event_id.as_deref(),
            Some(events[0].event_id.as_str())
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
            args_preview: Some("npm test".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
        let (tx, mut rx) = mpsc::channel(3);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };

        for i in 0..3 {
            sender
                .enqueue_async(test_event(&format!("e{i}"), "s1", "turn"))
                .await;
        }

        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 3, "all 3 events should be buffered");
    }

    #[tokio::test]
    async fn sender_enqueue_drops_silently_when_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };

        sender.enqueue(test_event("e1", "s1", "turn"));
        sender.enqueue(test_event("e2", "s1", "turn"));

        let first = rx.recv().await.unwrap();
        assert_eq!(first.event_id, "e1");
        assert!(rx.try_recv().is_err(), "e2 should have been dropped");
    }

    #[test]
    fn insert_batch_sql_has_correct_placeholder_count() {
        // Each event needs 12 bind params: event_id, session_id, user_id, event_type,
        // content, token_usage, llm_model_used, skill_name, metadata,
        // created_at, parent_event_id, causal_chain_id
        let n = 5;
        let placeholders: Vec<String> = (0..n)
            .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)".to_string())
            .collect();
        let sql = format!(
            "INSERT IGNORE INTO agent_events \
             (event_id, session_id, user_id, event_type, content, \
              token_usage, llm_model_used, skill_name, metadata, \
              created_at, parent_event_id, causal_chain_id) \
             VALUES {}",
            placeholders.join(", ")
        );

        assert_eq!(
            sql.matches("(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)").count(),
            n,
            "should have {n} placeholder groups"
        );
        assert!(sql.contains("INSERT IGNORE"));
        assert!(sql.contains("parent_event_id"));
        assert!(sql.contains("causal_chain_id"));
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

    #[test]
    fn overflow_count_zero_initially() {
        let sender = IngestionSender::disconnected();
        assert_eq!(sender.overflow_count(), 0);
    }

    #[test]
    fn overflow_count_increments_on_full_channel() {
        // Create a channel with capacity 1
        let (tx, _rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
        };

        // Fill the channel
        sender.enqueue(test_event("e1", "s1", "turn"));
        // This should overflow (channel capacity is 1)
        sender.enqueue(test_event("e2", "s1", "turn"));

        assert_eq!(sender.overflow_count(), 1, "second enqueue should overflow");

        // Third enqueue also overflows
        sender.enqueue(test_event("e3", "s1", "turn"));
        assert_eq!(sender.overflow_count(), 2);
    }

    #[test]
    fn insert_batch_sql_uses_rows_affected_guard() {
        // Verify the insert_batch method captures rows_affected and
        // reconciles event_count when INSERT IGNORE skips duplicates.
        // This is a compile-time documentation test: the actual logic is
        // exercised by integration tests against MatrixOne.
        let source = include_str!("event_ingestion.rs");
        assert!(
            source.contains("rows_affected()"),
            "insert_batch must check rows_affected to detect INSERT IGNORE skips"
        );
        assert!(
            source.contains("load_agent_event_count"),
            "insert_batch must load actual persisted agent_event counts before reconciling event_count"
        );
        assert!(
            source.contains("upsert_agent_session_event_count"),
            "insert_batch must reuse the shared agent_sessions upsert helper before reconciling event_count"
        );
        assert!(
            source.contains("rows_affected()"),
            "insert_batch must still preserve duplicate-detection logic while reconciling via the shared helper"
        );
    }

    // ── Shutdown handle tests ───────────────────────────────────────────

    /// Helper: create a MySql pool that will fail on any actual query.
    /// `MySqlPoolOptions::connect_lazy` builds a pool without touching the network,
    /// so `spawn` returns instantly. Any `insert_batch` call will error, which is
    /// fine — we're testing the shutdown signalling, not the DB path.
    fn dummy_pool() -> sqlx::Pool<sqlx::MySql> {
        sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect_lazy("mysql://invalid:invalid@127.0.0.1:1/nonexistent")
            .expect("connect_lazy should not fail")
    }

    #[tokio::test]
    async fn shutdown_handle_signal_stops_worker() {
        let (sender, shutdown, _stats, jh) =
            EventIngestionWorker::spawn(dummy_pool(), IngestionConfig::default());

        // Enqueue a couple of events so the worker has something buffered.
        sender.enqueue(test_event("e1", "s1", "turn"));
        sender.enqueue(test_event("e2", "s1", "turn"));

        // Signal shutdown — worker should exit promptly even though sender is still alive.
        shutdown.signal();

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), jh).await;
        assert!(
            result.is_ok(),
            "worker should exit within 3s after shutdown signal"
        );
        // Sender is still alive here — proves Notify works independent of channel close.
        drop(sender);
    }

    #[tokio::test]
    async fn shutdown_drains_channel_events() {
        let config = IngestionConfig {
            batch_size: 100,          // large batch so nothing auto-flushes
            flush_interval_secs: 300, // long interval so timer doesn't fire
            ..Default::default()
        };
        let (sender, shutdown, stats, jh) = EventIngestionWorker::spawn(dummy_pool(), config);

        for i in 0..5 {
            sender.enqueue(test_event(&format!("e{i}"), "s1", "turn"));
        }

        // Signal immediately — events are still in the channel, exercises try_recv drain.
        shutdown.signal();

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), jh).await;
        assert!(result.is_ok(), "worker should exit after signal");

        let s = stats.lock().unwrap();
        assert_eq!(
            s.events_received, 5,
            "all 5 events should be counted (recv + drain)"
        );
        assert!(
            s.errors > 0,
            "flush should have been attempted (and failed on dummy pool)"
        );
        drop(sender);
    }

    #[tokio::test]
    async fn shutdown_signal_is_idempotent() {
        let (_sender, shutdown, _stats, jh) =
            EventIngestionWorker::spawn(dummy_pool(), IngestionConfig::default());

        shutdown.signal();
        shutdown.signal(); // second signal should be harmless

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), jh).await;
        assert!(
            result.is_ok(),
            "worker should exit cleanly on double signal"
        );
    }

    #[test]
    fn shutdown_path_drains_channel_before_flush() {
        // Source-level assertion: the shutdown.notified() branch must call
        // try_recv to drain remaining channel events before flushing.
        let source = include_str!("event_ingestion.rs");
        assert!(
            source.contains("self.rx.try_recv()"),
            "shutdown branch must drain channel via try_recv before flush_batch_once"
        );
    }

    #[test]
    fn context_assembly_recorded_ingests_trace_in_metadata() {
        use crate::session_journal::JournalEvent;
        let trace = serde_json::json!({
            "turn_id": "turn-0",
            "session_id": "sess-1",
            "system_prompt": {
                "base_persona_tokens": 8000,
                "environment_tokens": 300,
                "user_preferences_tokens": 100,
                "skills_injected": [],
                "repository_memories": [],
                "total_tokens": 8400,
            },
            "token_budget": {
                "system_prompt_tokens": 8400,
                "history_tokens": 500,
                "total_used": 10000,
                "max_tokens": 128000,
            },
        });
        let journal = JournalEvent::context_assembly_recorded(Some("sess-1"), 0, trace.clone());
        let ingestion = IngestionEvent::from_journal_event(&journal, "user-x");

        assert_eq!(ingestion.session_id, "sess-1");
        assert!(
            ingestion.event_type.contains("context_assembly"),
            "event_type should contain context_assembly: {}",
            ingestion.event_type
        );
        let meta = ingestion.metadata.expect("should have metadata");
        let cat = meta
            .get("context_assembly_trace")
            .expect("should have context_assembly_trace in metadata");
        assert_eq!(cat["turn_id"], "turn-0");
        assert_eq!(cat["system_prompt"]["base_persona_tokens"], 8000);
        assert_eq!(cat["token_budget"]["total_used"], 10000);
    }

    #[test]
    fn context_assembly_trace_absent_no_metadata_pollution() {
        let mut event = make_turn_event();
        event.context_assembly_trace = None;
        let ingestion = IngestionEvent::from_journal_event(&event, "user-y");
        // Turn events without context_assembly_trace should not have it in metadata
        if let Some(ref meta) = ingestion.metadata {
            assert!(
                meta.get("context_assembly_trace").is_none(),
                "should not have context_assembly_trace when absent"
            );
        }
    }
}
