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
//! - **Retry after acceptance**: flushed batches are retained across transient
//!   MatrixOne failures and retried; duplicate inserts are deduped by
//!   `(user_id, event_id)` PK
//! - **Backpressure**: bounded channel; async callers await capacity, sync
//!   callers defer when running inside Tokio
//! - **Graceful shutdown**: flush remaining buffer on drop

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::mpsc;

use astra_core::canonical_names::normalize_optional_name;

const SESSION_END_EVENT_TYPE: &str = "session_end";
pub const MIN_INGESTION_BATCH_SIZE: usize = 1;
pub const MAX_INGESTION_BATCH_SIZE: usize = 200;
pub const MIN_INGESTION_FLUSH_INTERVAL_SECS: u64 = 1;
pub const MAX_INGESTION_FLUSH_INTERVAL_SECS: u64 = 300;
pub const MIN_INGESTION_CHANNEL_CAPACITY: usize = 1;
pub const MAX_INGESTION_CHANNEL_CAPACITY: usize = 10_000;
pub const MIN_INGESTION_RETRIES: u32 = 1;
pub const MAX_INGESTION_RETRIES: u32 = 8;
pub const DEFAULT_INGESTION_BATCH_SIZE: usize = 100;
pub const DEFAULT_INGESTION_FLUSH_INTERVAL_SECS: u64 = 1;
pub const DEFAULT_INGESTION_CHANNEL_CAPACITY: usize = 5_000;
pub const DEFAULT_INGESTION_RETRIES: u32 = 3;
const MAX_SHUTDOWN_DRAIN_PENDING_YIELDS: usize = 64;
const DISCONNECTED_PENDING_DEFERRAL_LIMIT: usize = 1;

/// Configuration for the ingestion worker.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// instead of the raw text. Default: `true` so cloud ingestion is
    /// privacy-safe unless a caller explicitly opts out.
    pub redact_content: bool,
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_INGESTION_BATCH_SIZE,
            flush_interval_secs: DEFAULT_INGESTION_FLUSH_INTERVAL_SECS,
            channel_capacity: DEFAULT_INGESTION_CHANNEL_CAPACITY,
            max_retries: DEFAULT_INGESTION_RETRIES,
            redact_content: true,
        }
    }
}

impl IngestionConfig {
    fn normalized(mut self) -> Self {
        self.batch_size = self
            .batch_size
            .clamp(MIN_INGESTION_BATCH_SIZE, MAX_INGESTION_BATCH_SIZE);
        self.flush_interval_secs = self.flush_interval_secs.clamp(
            MIN_INGESTION_FLUSH_INTERVAL_SECS,
            MAX_INGESTION_FLUSH_INTERVAL_SECS,
        );
        self.channel_capacity = self.channel_capacity.clamp(
            MIN_INGESTION_CHANNEL_CAPACITY,
            MAX_INGESTION_CHANNEL_CAPACITY,
        );
        self.max_retries = self
            .max_retries
            .clamp(MIN_INGESTION_RETRIES, MAX_INGESTION_RETRIES);
        self
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
                obj.insert("event_metadata".to_string(), other.clone());
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
    pub fn from_journal_event(
        event: &crate::session_journal::JournalEvent,
        user_id: &str,
    ) -> Result<Self, String> {
        Self::from_journal_event_with_redact(event, user_id, false)
    }

    /// Build an `IngestionEvent` that carries a saved config version
    /// to the cloud. The worker classifier uses the event_type tag
    /// (see `config_version_cloud::CONFIG_VERSION_SAVED_EVENT_TYPE`)
    /// to dual-write: agent_events row AND config_versions row.
    ///
    /// Event id == version id so INSERT IGNORE on the PK also
    /// handles agent_events dedup — pushing the same config twice
    /// records "the fact of pushing it" exactly once on both tables.
    pub fn for_config_version(
        row: &crate::config_version_cloud::ConfigVersionPayload,
    ) -> Result<Self, String> {
        let session_id = row
            .first_seen_session
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .ok_or_else(|| {
                format!(
                    "config version push requires first_seen_session for version_id={}",
                    row.version_id
                )
            })?;
        Ok(Self {
            event_id: row.version_id.clone(),
            session_id: session_id.to_string(),
            user_id: row.user_id.clone(),
            event_type: crate::config_version_cloud::CONFIG_VERSION_SAVED_EVENT_TYPE.to_string(),
            content: Some(row.toml_body.clone()),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            // Cloud side uses the server's CURRENT_TIMESTAMP default
            // for `config_versions.created_at`; this field is carried
            // for `agent_events.created_at` to stay consistent with
            // other rows in the same batch.
            created_at: chrono::Utc::now().to_rfc3339(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
        })
    }

    /// Like [`from_journal_event`] but optionally replaces the `content` field
    /// with a deterministic privacy marker when `redact_content == true`.
    pub fn from_journal_event_with_redact(
        event: &crate::session_journal::JournalEvent,
        user_id: &str,
        redact_content: bool,
    ) -> Result<Self, String> {
        let session_id = event
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .ok_or_else(|| {
                format!(
                    "journal event {:?} at {} is missing session_id",
                    event.event_type, event.ts
                )
            })?
            .to_string();

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

        let token_usage = canonical_token_usage_json_from_journal_event(event);

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

        Ok(Self {
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
        })
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
    ) -> Result<Vec<Self>, String> {
        Self::expand_journal_event_with_redact(event, user_id, false)
    }

    /// Like [`expand_journal_event`] but applies privacy redaction to both the
    /// main event and any tool_call expansion content when enabled.
    pub fn expand_journal_event_with_redact(
        event: &crate::session_journal::JournalEvent,
        user_id: &str,
        redact_content: bool,
    ) -> Result<Vec<Self>, String> {
        let main_event = Self::from_journal_event_with_redact(event, user_id, redact_content)?;
        let session_id = main_event.session_id.clone();
        let uid = main_event.user_id.clone();
        let main_event_id = main_event.event_id.clone();

        let mut events = vec![main_event];

        // Expand embedded tool_call_records into individual tool_call events
        if let Some(ref tool_calls) = event.tool_calls {
            for (i, tc) in tool_calls.iter().enumerate() {
                let Some(tool_name) = normalize_optional_name(Some(tc.name.clone())) else {
                    continue;
                };
                // Deterministic event_id: hash of (session_id, turn, tool_call, index)
                let tc_event_id = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut hasher = DefaultHasher::new();
                    session_id.hash(&mut hasher);
                    event.turn.hash(&mut hasher);
                    "tool_call".hash(&mut hasher);
                    i.hash(&mut hasher);
                    tool_name.hash(&mut hasher);
                    format!("evt-{:016x}", hasher.finish())
                };

                let raw_content = if tc.ok {
                    format!("{} completed in {}ms", tool_name, tc.ms)
                } else {
                    format!(
                        "{} failed in {}ms: {}",
                        tool_name,
                        tc.ms,
                        tc.error.as_deref().unwrap_or("unknown error")
                    )
                };
                let content = if redact_content {
                    redacted_content_marker(&raw_content)
                } else {
                    raw_content
                };

                let mut metadata = serde_json::Map::new();
                metadata.insert("tool_name".into(), Value::String(tool_name.clone()));
                metadata.insert("ok".into(), Value::Bool(tc.ok));
                metadata.insert("duration_ms".into(), Value::from(tc.ms));
                metadata.insert(
                    "error".into(),
                    tc.error
                        .as_ref()
                        .map(|error| Value::String(error.clone()))
                        .unwrap_or(Value::Null),
                );
                metadata.insert(
                    "turn".into(),
                    event.turn.map(Value::from).unwrap_or(Value::Null),
                );
                if let Some(ask_user) = tc.ask_user.clone() {
                    metadata.insert("ask_user".into(), ask_user);
                }

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
                    skill_name: Some(tool_name),
                    metadata: Some(Value::Object(metadata)),
                    created_at: event.ts.clone(),
                    parent_event_id: Some(main_event_id.clone()),
                    parent_event_ids: vec![main_event_id.clone()],
                    causal_chain_id: Some(main_event_id.clone()),
                });
            }
        }

        Ok(events)
    }
}

/// Handle for sending events to the ingestion worker.
#[derive(Clone)]
pub struct IngestionSender {
    tx: mpsc::Sender<IngestionEvent>,
    overflow_count: Arc<AtomicU64>,
    dropped_before_acceptance_count: Arc<AtomicU64>,
    pending_deferrals: Arc<AtomicUsize>,
    max_pending_deferrals: usize,
}

impl IngestionSender {
    /// Handle with no worker: [`Self::enqueue`] is a no-op (disconnected channel). Tests only.
    pub fn disconnected() -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: DISCONNECTED_PENDING_DEFERRAL_LIMIT,
        }
    }

    /// Build a sender whose events can be drained from the returned
    /// receiver. Tests wire this up when they need to assert on enqueued
    /// events instead of just ignoring them. Not intended for production
    /// code paths — use [`IngestionSender`] obtained from
    /// [`EventIngestionWorker::spawn`] there.
    pub fn for_tests(capacity: usize) -> (Self, mpsc::Receiver<IngestionEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                overflow_count: Arc::new(AtomicU64::new(0)),
                dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
                pending_deferrals: Arc::new(AtomicUsize::new(0)),
                max_pending_deferrals: capacity.max(1),
            },
            rx,
        )
    }

    /// Enqueue an event for async ingestion.
    ///
    /// Fast path is non-blocking. If the bounded channel is full and a Tokio
    /// runtime is available, detach a small async send task so the event waits
    /// for capacity instead of being dropped. `overflow_count` tracks these
    /// backpressure deferrals and hard closed-channel drops.
    pub fn enqueue(&self, event: IngestionEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                let n = self.overflow_count.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    target: "astra_services::event_ingestion",
                    overflow_count = n,
                    "ingestion channel full; deferring event until capacity is available"
                );
                let tx = self.tx.clone();
                let overflow_count = self.overflow_count.clone();
                let dropped_before_acceptance_count = self.dropped_before_acceptance_count.clone();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        let pending_deferrals = self.pending_deferrals.clone();
                        if pending_deferrals
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                                (pending < self.max_pending_deferrals).then_some(pending + 1)
                            })
                            .is_err()
                        {
                            tracing::warn!(
                                target: "astra_services::event_ingestion",
                                overflow_count = n,
                                max_pending_deferrals = self.max_pending_deferrals,
                                "ingestion deferred-send queue full; event dropped"
                            );
                            self.dropped_before_acceptance_count
                                .fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        drop(handle.spawn(async move {
                            if tx.send(event).await.is_err() {
                                let n = overflow_count.fetch_add(1, Ordering::Relaxed) + 1;
                                dropped_before_acceptance_count
                                    .fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    target: "astra_services::event_ingestion",
                                    overflow_count = n,
                                    "ingestion channel closed while deferred event was waiting; event dropped"
                                );
                            }
                            pending_deferrals.fetch_sub(1, Ordering::Relaxed);
                        }));
                    }
                    Err(_) => {
                        tracing::warn!(
                            target: "astra_services::event_ingestion",
                            overflow_count = n,
                            "ingestion channel full outside a Tokio runtime; event dropped"
                        );
                        self.dropped_before_acceptance_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let n = self.overflow_count.fetch_add(1, Ordering::Relaxed) + 1;
                self.dropped_before_acceptance_count
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: "astra_services::event_ingestion",
                    overflow_count = n,
                    "ingestion channel closed; event dropped"
                );
            }
        }
    }

    /// Total number of immediate enqueue overflows and closed-channel drops.
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
    }

    /// Events dropped before the worker accepted them into the ingestion
    /// channel. This excludes successfully deferred sends and permanently
    /// invalid rows dropped later by the worker.
    pub fn dropped_before_acceptance_count(&self) -> u64 {
        self.dropped_before_acceptance_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn pending_deferral_count(&self) -> usize {
        self.pending_deferrals.load(Ordering::Relaxed)
    }

    /// Enqueue with backpressure (waits if channel full).
    pub async fn enqueue_async(&self, event: IngestionEvent) {
        if self.tx.send(event).await.is_err() {
            self.overflow_count.fetch_add(1, Ordering::Relaxed);
            self.dropped_before_acceptance_count
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "astra_services::event_ingestion",
                "ingestion channel closed; event dropped"
            );
        }
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
    pub events_dropped_permanent: u64,
    pub flush_count: u64,
    pub errors: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct IngestionBatchOutcome {
    events_dropped_permanent: usize,
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

fn ingestion_event_has_parent_edges(event: &IngestionEvent) -> bool {
    event
        .parent_event_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
        || event
            .parent_event_ids
            .iter()
            .any(|id| !id.trim().is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalTokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

impl CanonicalTokenUsage {
    fn from_journal_event(
        event: &crate::session_journal::JournalEvent,
    ) -> Result<Option<Self>, String> {
        let cached_input_tokens = event.cache_read_tokens.unwrap_or(0);
        let cache_creation_tokens = event.cache_creation_tokens.unwrap_or(0);
        let has_any_token_field = event.tokens_in.is_some()
            || event.tokens_out.is_some()
            || cached_input_tokens > 0
            || cache_creation_tokens > 0;
        if !has_any_token_field {
            return Ok(None);
        }

        let input_tokens = event
            .tokens_in
            .ok_or_else(|| "token_usage canonicalization: missing tokens_in".to_string())?;
        let output_tokens = event
            .tokens_out
            .ok_or_else(|| "token_usage canonicalization: missing tokens_out".to_string())?;
        let billable_input = input_tokens
            .checked_add(cached_input_tokens)
            .and_then(|value| value.checked_add(cache_creation_tokens))
            .ok_or_else(|| "token_usage canonicalization: input token overflow".to_string())?;
        let total_tokens = billable_input
            .checked_add(output_tokens)
            .ok_or_else(|| "token_usage canonicalization: total token overflow".to_string())?;
        Ok(Some(Self {
            input_tokens,
            cached_input_tokens,
            cache_creation_tokens,
            output_tokens,
            total_tokens,
        }))
    }

    fn from_json(value: &Value) -> Result<Option<Self>, String> {
        let Some(obj) = value.as_object() else {
            return Err(format!(
                "token_usage must be a canonical JSON object, got {value}"
            ));
        };
        let read_required = |key: &str| -> Result<u64, String> {
            let value = obj
                .get(key)
                .ok_or_else(|| format!("token_usage missing canonical field `{key}`"))?;
            if let Some(value) = value.as_u64() {
                return Ok(value);
            }
            if let Some(value) = value.as_i64() {
                return u64::try_from(value).map_err(|_| {
                    format!("token_usage field `{key}` must be non-negative, got {value}")
                });
            }
            Err(format!(
                "token_usage field `{key}` must be an integer, got {value}"
            ))
        };
        let usage = Self {
            input_tokens: read_required("input_tokens")?,
            cached_input_tokens: read_required("cached_input_tokens")?,
            cache_creation_tokens: read_required("cache_creation_tokens")?,
            output_tokens: read_required("output_tokens")?,
            total_tokens: read_required("total_tokens")?,
        };
        let expected_total = usage
            .input_tokens
            .checked_add(usage.cached_input_tokens)
            .and_then(|value| value.checked_add(usage.cache_creation_tokens))
            .and_then(|value| value.checked_add(usage.output_tokens))
            .ok_or_else(|| "token_usage total overflow".to_string())?;
        if usage.total_tokens != expected_total {
            return Err(format!(
                "token_usage total_tokens mismatch: expected {expected_total}, got {}",
                usage.total_tokens
            ));
        }
        Ok(Some(usage))
    }

    fn to_json(self) -> Value {
        let billable_input = self
            .input_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_creation_tokens);
        serde_json::json!({
            "input_tokens": self.input_tokens,
            "cached_input_tokens": self.cached_input_tokens,
            "cache_creation_tokens": self.cache_creation_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
            "prompt": billable_input,
            "completion": self.output_tokens,
            "cache_read": self.cached_input_tokens,
            "cache_write": self.cache_creation_tokens,
            "total": self.total_tokens,
        })
    }

    fn input_column(self) -> Result<i64, String> {
        let billable_input = self
            .input_tokens
            .checked_add(self.cached_input_tokens)
            .and_then(|value| value.checked_add(self.cache_creation_tokens))
            .ok_or_else(|| "token_usage input column overflow".to_string())?;
        i64::try_from(billable_input)
            .map_err(|_| format!("token_usage input column exceeds i64::MAX: {billable_input}"))
    }

    fn output_column(self) -> Result<i64, String> {
        i64::try_from(self.output_tokens).map_err(|_| {
            format!(
                "token_usage output column exceeds i64::MAX: {}",
                self.output_tokens
            )
        })
    }

    fn total_column(self) -> Result<i64, String> {
        i64::try_from(self.total_tokens).map_err(|_| {
            format!(
                "token_usage total column exceeds i64::MAX: {}",
                self.total_tokens
            )
        })
    }
}

fn canonical_token_usage_json_from_journal_event(
    event: &crate::session_journal::JournalEvent,
) -> Option<Value> {
    CanonicalTokenUsage::from_journal_event(event)
        .ok()
        .flatten()
        .map(CanonicalTokenUsage::to_json)
}

#[derive(Debug)]
struct IngestionEventInsertValues<'a> {
    event: &'a IngestionEvent,
    token_usage_json: Option<String>,
    metadata_json: Option<String>,
    skill_name: Option<String>,
    token_input: Option<i64>,
    token_output: Option<i64>,
    token_total: Option<i64>,
    created_at: String,
}

#[derive(Debug, Default)]
struct TokenUsageDbFields {
    token_usage_json: Option<String>,
    token_input: Option<i64>,
    token_output: Option<i64>,
    token_total: Option<i64>,
}

impl<'a> IngestionEventInsertValues<'a> {
    fn from_event(event: &'a IngestionEvent) -> Result<Self, String> {
        let usage = match event.token_usage.as_ref() {
            Some(value) => match CanonicalTokenUsage::from_json(value) {
                Ok(usage) => usage,
                Err(error) => {
                    tracing::warn!(
                        target: "astra_services::event_ingestion",
                        user_id = %event.user_id,
                        session_id = %event.session_id,
                        event_id = %event.event_id,
                        event_type = %event.event_type,
                        error = %error,
                        "invalid canonical token_usage; storing event without token counters"
                    );
                    None
                }
            },
            None => None,
        };
        let token_fields = match canonical_token_usage_db_fields_or_null(usage) {
            Ok(fields) => fields,
            Err(error) => {
                tracing::warn!(
                    target: "astra_services::event_ingestion",
                    user_id = %event.user_id,
                    session_id = %event.session_id,
                    event_id = %event.event_id,
                    event_type = %event.event_type,
                    error = %error,
                    "canonical token_usage exceeds database token columns; storing event without token counters"
                );
                TokenUsageDbFields::default()
            }
        };
        Ok(Self {
            event,
            token_usage_json: token_fields.token_usage_json,
            metadata_json: event.metadata.as_ref().map(Value::to_string),
            skill_name: normalize_optional_name(event.skill_name.clone()),
            token_input: token_fields.token_input,
            token_output: token_fields.token_output,
            token_total: token_fields.token_total,
            created_at: iso8601_to_mysql_datetime(&event.created_at),
        })
    }
}

fn canonical_token_usage_db_fields_or_null(
    usage: Option<CanonicalTokenUsage>,
) -> Result<TokenUsageDbFields, String> {
    let Some(usage) = usage else {
        return Ok(TokenUsageDbFields::default());
    };
    let input = usage.input_column()?;
    let output = usage.output_column()?;
    let total = usage.total_column()?;
    Ok(TokenUsageDbFields {
        token_usage_json: Some(usage.to_json().to_string()),
        token_input: Some(input),
        token_output: Some(output),
        token_total: Some(total),
    })
}

fn add_inserted_rows(total: &mut i64, rows_affected: u64, context: &str) -> Result<(), String> {
    let inserted =
        crate::storage::rows_affected_to_i64(rows_affected, context).map_err(|e| e.to_string())?;
    *total = total
        .checked_add(inserted)
        .ok_or_else(|| format!("{context}: inserted row total overflow"))?;
    Ok(())
}

fn bind_ingestion_event<'q>(
    mut query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    values: &'q IngestionEventInsertValues<'q>,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    let event = values.event;
    query = query
        .bind(&event.event_id)
        .bind(&event.session_id)
        .bind(&event.user_id)
        .bind(&event.event_type)
        .bind(&event.content)
        .bind(&values.token_usage_json)
        .bind(&event.llm_model_used)
        .bind(&values.skill_name)
        .bind(&values.metadata_json)
        .bind(&values.created_at)
        .bind(&event.parent_event_id)
        .bind(&event.causal_chain_id)
        .bind(values.token_input)
        .bind(values.token_output)
        .bind(values.token_total);
    query
}

/// The background worker that batches and flushes events to MatrixOne.
pub struct EventIngestionWorker {
    rx: mpsc::Receiver<IngestionEvent>,
    pool: sqlx::Pool<sqlx::MySql>,
    config: IngestionConfig,
    stats: Arc<std::sync::Mutex<IngestionStats>>,
    pending_deferrals: Arc<AtomicUsize>,
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
    /// Spawn the ingestion pipeline.
    pub fn spawn(
        pool: sqlx::Pool<sqlx::MySql>,
        config: IngestionConfig,
    ) -> (
        IngestionSender,
        IngestionShutdownHandle,
        Arc<std::sync::Mutex<IngestionStats>>,
        tokio::task::JoinHandle<()>,
    ) {
        let raw_config = config;
        let config = raw_config.clone().normalized();
        if config != raw_config {
            tracing::warn!(
                target: "astra_services::event_ingestion",
                requested_batch_size = raw_config.batch_size,
                batch_size = config.batch_size,
                requested_flush_interval_secs = raw_config.flush_interval_secs,
                flush_interval_secs = config.flush_interval_secs,
                requested_channel_capacity = raw_config.channel_capacity,
                channel_capacity = config.channel_capacity,
                requested_max_retries = raw_config.max_retries,
                max_retries = config.max_retries,
                "event ingestion config was clamped to supported bounds"
            );
        }
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let stats = Arc::new(std::sync::Mutex::new(IngestionStats::default()));
        let stats_clone = stats.clone();
        let notify = Arc::new(tokio::sync::Notify::new());
        let pending_deferrals = Arc::new(AtomicUsize::new(0));
        let max_pending_deferrals = config.channel_capacity;

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
            pending_deferrals: Arc::clone(&pending_deferrals),
        };
        // Share the same Arc so the handle can signal the worker.
        let shutdown_handle = IngestionShutdownHandle {
            notify: Arc::clone(&notify),
        };

        let handle = tokio::spawn(worker.run_with_shutdown(notify));

        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals,
            max_pending_deferrals,
        };
        (sender, shutdown_handle, stats_clone, handle)
    }

    fn record_event_received(&self) {
        if let Ok(mut s) = self.stats.lock() {
            s.events_received += 1;
        }
    }

    fn drain_available_channel_events(&mut self, buffer: &mut Vec<IngestionEvent>) -> usize {
        let mut drained = 0;
        while let Ok(event) = self.rx.try_recv() {
            self.record_event_received();
            buffer.push(event);
            drained += 1;
        }
        drained
    }

    async fn drain_channel_for_shutdown(&mut self, buffer: &mut Vec<IngestionEvent>) {
        let mut pending_yields = 0;
        let mut observed_empty_after_pending_zero = false;
        loop {
            let drained = self.drain_available_channel_events(buffer);
            if drained > 0 {
                pending_yields = 0;
                observed_empty_after_pending_zero = false;
                continue;
            }

            let pending = self.pending_deferrals.load(Ordering::Relaxed);
            if pending == 0 {
                if observed_empty_after_pending_zero {
                    break;
                }
                observed_empty_after_pending_zero = true;
                tokio::task::yield_now().await;
                continue;
            }
            observed_empty_after_pending_zero = false;
            if pending_yields >= MAX_SHUTDOWN_DRAIN_PENDING_YIELDS {
                tracing::warn!(
                    target: "astra_services::event_ingestion",
                    pending_deferrals = pending,
                    "event ingestion shutdown drain stopped with deferred sends still pending"
                );
                break;
            }

            pending_yields += 1;
            tokio::task::yield_now().await;
        }
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
                // audit-#7: bias toward shutdown so a busy `rx` cannot starve the drain branch.
                biased;
                _ = shutdown.notified() => {
                    // Drain remaining events, including deferred `enqueue` sends
                    // that become ready only after the first drain frees capacity.
                    self.drain_channel_for_shutdown(&mut buffer).await;
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
                    self.record_event_received();
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
                Ok(outcome) => {
                    if let Ok(mut s) = self.stats.lock() {
                        s.events_flushed += count as u64;
                        s.flush_count += 1;
                        if outcome.events_dropped_permanent > 0 {
                            s.events_dropped_permanent += outcome.events_dropped_permanent as u64;
                            s.errors += 1;
                            s.last_error = Some(format!(
                                "dropped {} permanently invalid ingestion events",
                                outcome.events_dropped_permanent
                            ));
                        }
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
                            "batch flush failed after retries; retaining batch for retry"
                        );
                        buffer.extend(batch);
                        return;
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
            Ok(outcome) => {
                if let Ok(mut s) = self.stats.lock() {
                    s.events_flushed += count as u64;
                    s.flush_count += 1;
                    if outcome.events_dropped_permanent > 0 {
                        s.events_dropped_permanent += outcome.events_dropped_permanent as u64;
                        s.errors += 1;
                        s.last_error = Some(format!(
                            "dropped {} permanently invalid ingestion events",
                            outcome.events_dropped_permanent
                        ));
                    }
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

    async fn insert_batch(
        &self,
        events: &[IngestionEvent],
    ) -> Result<IngestionBatchOutcome, String> {
        if events.is_empty() {
            return Ok(IngestionBatchOutcome::default());
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("begin tx: {e}"))?;

        let mut grouped_events =
            std::collections::BTreeMap::<(&str, &str), Vec<&IngestionEvent>>::new();
        for event in events {
            grouped_events
                .entry((event.user_id.as_str(), event.session_id.as_str()))
                .or_default()
                .push(event);
        }

        let mut rows_inserted = 0_i64;
        let mut outcome = IngestionBatchOutcome::default();
        let mut inserted_session_end_sessions =
            std::collections::BTreeSet::<(String, String)>::new();
        for ((user_id, session_id), session_events) in grouped_events {
            let session_event_count = session_events.len();
            let foreign_owner: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id <> ? LIMIT 1",
            )
            .bind(session_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("session owner check for {user_id}/{session_id}: {e}"))?;
            if foreign_owner.is_some() {
                outcome.events_dropped_permanent = outcome
                    .events_dropped_permanent
                    .checked_add(session_event_count)
                    .ok_or_else(|| {
                        "event_ingestion.events_dropped_permanent: dropped event total overflow"
                            .to_string()
                    })?;
                tracing::warn!(
                    target: "astra_services::event_ingestion",
                    user_id = %user_id,
                    session_id = %session_id,
                    event_count = session_event_count,
                    "dropping ingestion events for a session_id owned by another user"
                );
                continue;
            }

            let mut plain_events = Vec::new();
            let mut plain_session_end_events = Vec::new();
            let mut parented_events = Vec::new();
            for event in session_events {
                if ingestion_event_has_parent_edges(event) {
                    parented_events.push(event);
                } else if event.event_type == SESSION_END_EVENT_TYPE {
                    plain_session_end_events.push(event);
                } else {
                    plain_events.push(event);
                }
            }

            let mut session_rows_inserted = 0_i64;
            if !plain_events.is_empty() {
                let plain_event_rows = plain_events
                    .iter()
                    .map(|event| IngestionEventInsertValues::from_event(event))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
                    "INSERT IGNORE INTO agent_events \
                     (event_id, session_id, user_id, event_type, content, \
                      token_usage, llm_model_used, skill_name, metadata, \
                      created_at, parent_event_id, causal_chain_id, \
                      token_input, token_output, token_total) ",
                );
                builder.push_values(plain_event_rows.iter(), |mut row, values| {
                    let event = values.event;
                    row.push_bind(&event.event_id)
                        .push_bind(&event.session_id)
                        .push_bind(&event.user_id)
                        .push_bind(&event.event_type)
                        .push_bind(&event.content)
                        .push_bind(&values.token_usage_json)
                        .push_bind(&event.llm_model_used)
                        .push_bind(&values.skill_name)
                        .push_bind(&values.metadata_json)
                        .push_bind(&values.created_at)
                        .push_bind(&event.parent_event_id)
                        .push_bind(&event.causal_chain_id)
                        .push_bind(values.token_input)
                        .push_bind(values.token_output)
                        .push_bind(values.token_total);
                });

                let insert_result = builder
                    .build()
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| format!("batch insert ({user_id}/{session_id}): {e}"))?;
                add_inserted_rows(
                    &mut session_rows_inserted,
                    insert_result.rows_affected(),
                    "event_ingestion.batch_insert",
                )?;
            }

            for event in plain_session_end_events.into_iter().chain(parented_events) {
                let values = IngestionEventInsertValues::from_event(event)?;
                let insert_result = bind_ingestion_event(
                    sqlx::query(
                        "INSERT IGNORE INTO agent_events \
                         (event_id, session_id, user_id, event_type, content, \
                          token_usage, llm_model_used, skill_name, metadata, \
                          created_at, parent_event_id, causal_chain_id, \
                          token_input, token_output, token_total) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    ),
                    &values,
                )
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("event insert ({user_id}/{session_id}): {e}"))?;
                if insert_result.rows_affected() == 0 {
                    continue;
                }
                add_inserted_rows(
                    &mut session_rows_inserted,
                    insert_result.rows_affected(),
                    "event_ingestion.single_insert",
                )?;
                if event.event_type == SESSION_END_EVENT_TYPE {
                    inserted_session_end_sessions
                        .insert((event.user_id.clone(), event.session_id.clone()));
                }
                if ingestion_event_has_parent_edges(event) {
                    crate::storage::insert_agent_event_edges(
                        &mut *tx,
                        &event.user_id,
                        &event.session_id,
                        &event.event_id,
                        event.parent_event_id.as_deref(),
                        &event.parent_event_ids,
                    )
                    .await
                    .map_err(|e| format!("edge insert for {}: {e}", event.event_id))?;
                }
            }

            if session_rows_inserted == 0 {
                continue;
            }
            crate::storage::add_agent_session_event_count_or_create(
                &mut *tx,
                session_id,
                user_id,
                session_rows_inserted,
                None,
            )
            .await
            .map_err(|e| format!("event_count delta for {session_id}: {e}"))?;
            rows_inserted = rows_inserted
                .checked_add(session_rows_inserted)
                .ok_or_else(|| {
                    "event_ingestion.rows_inserted: inserted row total overflow".to_string()
                })?;
        }

        // Log if duplicates were detected (useful for debugging)
        let requested_event_count = u64::try_from(events.len())
            .map_err(|_| "event_ingestion.requested_events: len exceeds u64::MAX".to_string())?;
        let requested_events = crate::storage::rows_affected_to_i64(
            requested_event_count,
            "event_ingestion.requested_events",
        )
        .map_err(|e| e.to_string())?;
        if rows_inserted < requested_events {
            let skipped = requested_events - rows_inserted;
            astra_core::agent_info!(
                "event_ingestion",
                "INSERT IGNORE skipped {skipped} duplicates out of {} events",
                events.len()
            );
        }

        for (user_id, session_id) in inserted_session_end_sessions {
            sqlx::query(
                "UPDATE agent_sessions SET status = 'ended', ended_at = NOW() \
                 WHERE session_id = ? AND user_id = ? AND status != 'ended'",
            )
            .bind(&session_id)
            .bind(&user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("session close for {session_id}: {e}"))?;
        }

        // Step 4b: dual-write config-version events into the
        // `config_versions` table. Any event the classifier
        // recognises gets an INSERT IGNORE with the content-
        // addressed PK so a duplicate push from another machine
        // is a zero-row no-op. We reuse the same transaction so
        // the agent_events row and the config_versions row land
        // together.
        for event in events {
            let Some(payload) = crate::config_version_cloud::extract_config_version_payload(event)?
            else {
                continue;
            };
            sqlx::query(crate::config_version_cloud::CONFIG_VERSIONS_INSERT_SQL)
                .bind(&payload.version_id)
                .bind(&payload.user_id)
                .bind(&payload.toml_body)
                .bind(payload.first_seen_session.as_deref())
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("config_versions insert for {}: {e}", payload.version_id))?;
        }

        tx.commit().await.map_err(|e| format!("commit tx: {e}"))?;

        Ok(outcome)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event(event_id: &str, session_id: &str, event_type: &str) -> IngestionEvent {
        IngestionEvent {
            event_id: event_id.to_string(),
            session_id: session_id.to_string(),
            user_id: "test-user".to_string(),
            event_type: event_type.to_string(),
            content: None,
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            created_at: "2025-01-15T10:30:00Z".to_string(),
            parent_event_id: None,
            parent_event_ids: vec![],
            causal_chain_id: None,
        }
    }

    #[test]
    fn test_is_duplicate_key_error() {
        // MySQL error 1062 is handled at the SQL layer via INSERT IGNORE.
        // Delegate to astra_core which has the real implementation and
        // its own thorough test suite (see crates/core/src/lib.rs).
        let dup = sqlx::Error::Protocol("1062: Duplicate entry 'test' for key 'PRIMARY'".into());
        assert!(astra_core::is_duplicate_key_error(&dup));
        let unrelated = sqlx::Error::Protocol("connection reset by peer".into());
        assert!(!astra_core::is_duplicate_key_error(&unrelated));
    }

    #[test]
    fn ingestion_config_defaults() {
        let config = IngestionConfig::default();
        assert_eq!(config.batch_size, DEFAULT_INGESTION_BATCH_SIZE);
        assert_eq!(
            config.flush_interval_secs,
            DEFAULT_INGESTION_FLUSH_INTERVAL_SECS
        );
        assert_eq!(config.channel_capacity, DEFAULT_INGESTION_CHANNEL_CAPACITY);
        assert_eq!(config.max_retries, DEFAULT_INGESTION_RETRIES);
        assert!(
            config.redact_content,
            "redact_content default must be true for privacy-safe cloud ingestion"
        );
    }

    #[test]
    fn ingestion_config_normalized_keeps_defaults() {
        assert_eq!(
            IngestionConfig::default().normalized(),
            IngestionConfig::default()
        );
    }

    #[test]
    fn ingestion_config_normalized_clamps_zero_and_unbounded_values() {
        let zero = IngestionConfig {
            batch_size: 0,
            flush_interval_secs: 0,
            channel_capacity: 0,
            max_retries: 0,
            ..Default::default()
        }
        .normalized();
        assert_eq!(zero.batch_size, MIN_INGESTION_BATCH_SIZE);
        assert_eq!(zero.flush_interval_secs, MIN_INGESTION_FLUSH_INTERVAL_SECS);
        assert_eq!(zero.channel_capacity, MIN_INGESTION_CHANNEL_CAPACITY);
        assert_eq!(zero.max_retries, MIN_INGESTION_RETRIES);

        let huge = IngestionConfig {
            batch_size: usize::MAX,
            flush_interval_secs: u64::MAX,
            channel_capacity: usize::MAX,
            max_retries: u32::MAX,
            ..Default::default()
        }
        .normalized();
        assert_eq!(huge.batch_size, MAX_INGESTION_BATCH_SIZE);
        assert_eq!(huge.flush_interval_secs, MAX_INGESTION_FLUSH_INTERVAL_SECS);
        assert_eq!(huge.channel_capacity, MAX_INGESTION_CHANNEL_CAPACITY);
        assert_eq!(huge.max_retries, MAX_INGESTION_RETRIES);
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
    fn from_journal_event_redact_behavior() {
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
        // Redact off
        let off = IngestionEvent::from_journal_event_with_redact(&event, "u1", false)
            .expect("valid journal event");
        assert_eq!(off.content.as_deref(), Some("hello world"));

        // Redact on
        let on = IngestionEvent::from_journal_event_with_redact(&event, "u1", true)
            .expect("valid journal event");
        let content = on.content.expect("content present");
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
        let evs = IngestionEvent::expand_journal_event_with_redact(&event, "u1", true)
            .expect("valid journal event");
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
    fn test_iso8601_to_mysql_datetime() {
        let cases = vec![
            ("2025-01-15T10:30:00+00:00", "2025-01-15 10:30:00.000000"),
            (
                "2025-01-15T10:30:00.123456+00:00",
                "2025-01-15 10:30:00.123456",
            ),
            ("2025-01-15T10:30:00Z", "2025-01-15 10:30:00.000000"),
            ("not-a-date", "not-a-date"),
        ];
        for (input, expect) in cases {
            assert_eq!(iso8601_to_mysql_datetime(input), expect);
        }
    }

    #[test]
    fn add_inserted_rows_accumulates_without_saturating() {
        let mut total = 40;
        add_inserted_rows(&mut total, 2, "event_ingestion.test").unwrap();
        assert_eq!(total, 42);
    }

    #[test]
    fn add_inserted_rows_fails_loudly_on_conversion_overflow() {
        let mut total = 0;
        let err = add_inserted_rows(&mut total, i64::MAX as u64 + 1, "event_ingestion.test")
            .expect_err("rows_affected overflow must fail");
        assert!(
            err.contains("event_ingestion.test") && err.contains("exceeds i64::MAX"),
            "error should identify conversion overflow: {err}"
        );
        assert_eq!(total, 0, "failed conversion must not mutate total");
    }

    #[test]
    fn add_inserted_rows_fails_loudly_on_total_overflow() {
        let mut total = i64::MAX;
        let err = add_inserted_rows(&mut total, 1, "event_ingestion.test")
            .expect_err("total overflow must fail");
        assert!(
            err.contains("event_ingestion.test") && err.contains("total overflow"),
            "error should identify total overflow: {err}"
        );
        assert_eq!(total, i64::MAX, "failed add must not mutate total");
    }

    #[test]
    fn ingestion_event_json_roundtrip() {
        let mut event = test_event("evt-1", "sess-1", "turn_complete");
        event.user_id = "user-1".into();
        event.content = Some("hello world".into());
        event.token_usage = Some(serde_json::json!({
            "input_tokens": 100,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 50,
            "total_tokens": 150,
        }));
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
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 10,
        };
        sender.enqueue(test_event("e1", "s1", "test"));
    }

    #[tokio::test]
    async fn sender_enqueue_defers_when_channel_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 1,
        };
        sender.enqueue(test_event("e1", "s1", "test"));
        sender.enqueue(test_event("e2", "s1", "test"));

        let first = rx.recv().await.expect("first event");
        assert_eq!(first.event_id, "e1");
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("deferred send should complete after capacity is freed")
            .expect("second event");
        assert_eq!(second.event_id, "e2");
        assert_eq!(sender.overflow_count(), 1);
        assert_eq!(sender.dropped_before_acceptance_count(), 0);
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
            visible_tools: Some(vec!["github".into()]),
            selected_skills: Some(vec!["tune-performance".into()]),
            tools_used: Some(vec!["github".into()]),
            tool_calls: None,
            budget_used: None,
            budget_pressure: None,
            stall_type: None,
            metadata: None,
            plan_subtask_id: None,
            ttft_ms: None,
            context_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            memoria_ms: None,
            session_lineage: None,
            coordination: None,
            edge_policy: None,
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

    #[test]
    fn transform_sync_marker_cloud_pull_metadata_in_ingestion() {
        use crate::session_journal::JournalEvent;
        let keys = vec!["explain_mode".to_string()];
        let journal = JournalEvent::cloud_pull_sync_marker(
            Some("sess-sync"),
            "default",
            "repl_startup",
            &keys,
            false,
        );
        let ingestion =
            IngestionEvent::from_journal_event(&journal, "user-z").expect("valid journal event");
        assert!(ingestion.event_type.contains("sync"));
        assert_eq!(ingestion.session_id, "sess-sync");
        let meta = ingestion.metadata.expect("metadata");
        let cp = meta.get("cloud_pull").expect("cloud_pull blob");
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
            &[],
            true,
        );
        let ingestion =
            IngestionEvent::from_journal_event(&journal, "u1").expect("valid journal event");
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
        let journal =
            JournalEvent::cloud_pull_sync_marker(Some("s1"), "p", "post_login", &[], true);
        let merged = super::merged_metadata_from_journal_event(&journal);
        let obj = merged.expect("expected metadata");
        assert!(obj.get("cloud_pull").is_some());
    }

    #[test]
    fn merged_metadata_wraps_non_object_metadata_under_canonical_key() {
        let mut journal = make_turn_event();
        journal.metadata = Some(serde_json::json!("raw scalar metadata"));

        let merged = super::merged_metadata_from_journal_event(&journal).expect("metadata");

        assert_eq!(
            merged.get("event_metadata"),
            Some(&serde_json::json!("raw scalar metadata"))
        );
        assert!(
            merged.get("legacy_metadata").is_none(),
            "new journal metadata must not emit compatibility-only keys"
        );
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
        let ingestion =
            IngestionEvent::from_journal_event(&journal, "user-1").expect("valid journal event");
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
        let ingestion =
            IngestionEvent::from_journal_event(&journal, "user-1").expect("valid journal event");

        assert!(ingestion.event_id.starts_with("evt-"));
        assert_eq!(ingestion.session_id, "sess-abc");
        assert_eq!(ingestion.user_id, "user-1");
        assert!(ingestion.event_type.contains("turn"));
        assert_eq!(ingestion.content.as_deref(), Some("list PRs"));
        assert_eq!(ingestion.llm_model_used.as_deref(), Some("gpt-4"));
        assert_eq!(ingestion.created_at, "2025-01-15T10:30:00Z");
        assert!(ingestion.parent_event_id.is_none());
        assert!(ingestion.causal_chain_id.is_none());

        let usage = ingestion.token_usage.as_ref().unwrap();
        assert_eq!(usage["input_tokens"], 500);
        assert_eq!(usage["cached_input_tokens"], 0);
        assert_eq!(usage["cache_creation_tokens"], 0);
        assert_eq!(usage["output_tokens"], 200);
        assert_eq!(usage["total_tokens"], 700);
    }

    #[test]
    fn transform_session_start_event() {
        let event =
            crate::session_journal::JournalEvent::session_start(Some("sess-new"), Some("gpt-4"));
        let ingestion =
            IngestionEvent::from_journal_event(&event, "user-2").expect("valid journal event");

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
        let ingestion =
            IngestionEvent::from_journal_event(&event, "user-3").expect("valid journal event");

        assert_eq!(ingestion.session_id, "sess-err");
        // Error event: content should be user_input (takes priority) or error
        assert!(ingestion.content.is_some());
    }

    #[test]
    fn transform_deterministic_event_id() {
        let journal = make_turn_event();
        let a = IngestionEvent::from_journal_event(&journal, "u1").expect("valid journal event");
        let b = IngestionEvent::from_journal_event(&journal, "u1").expect("valid journal event");
        // Same input → same event_id (deterministic for idempotency)
        assert_eq!(a.event_id, b.event_id);
    }

    #[test]
    fn transform_missing_session_id_fails_loudly() {
        let mut journal = make_turn_event();
        journal.session_id = None;
        let err = IngestionEvent::from_journal_event(&journal, "u1")
            .expect_err("missing session_id must not be ingested as a fake session");
        assert!(err.contains("missing session_id"), "error was: {err}");
        assert!(
            err.contains("Turn"),
            "error should identify event type: {err}"
        );
        assert!(
            err.contains("2025-01-15T10:30:00Z"),
            "error should identify event timestamp: {err}"
        );
    }

    #[test]
    fn transform_blank_session_id_fails_loudly() {
        let mut journal = make_turn_event();
        journal.session_id = Some("   ".into());
        let err = IngestionEvent::from_journal_event(&journal, "u1")
            .expect_err("blank session_id must not be ingested as a fake session");
        assert!(err.contains("missing session_id"), "error was: {err}");
    }

    #[test]
    fn expand_missing_session_id_propagates_error() {
        let mut journal = make_turn_event();
        journal.session_id = None;
        journal.tool_calls = Some(vec![crate::session_journal::ToolCallRecord {
            name: "bash".into(),
            ok: true,
            ms: 1,
            ..Default::default()
        }]);

        let err = IngestionEvent::expand_journal_event(&journal, "u1")
            .expect_err("expansion must reject the whole event when the parent session is missing");
        assert!(err.contains("missing session_id"), "error was: {err}");
    }

    #[test]
    fn transform_stall_event_uses_stall_type_as_content() {
        let event = crate::session_journal::JournalEvent::stall_detected(
            Some("sess-s"),
            5,
            "sig_stall",
            2,
            0.6,
            &["github".to_string()],
        );
        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
        assert_eq!(ingestion.content.as_deref(), Some("sig_stall"));
    }

    #[tokio::test]
    async fn sender_shutdown_closes_channel() {
        let (tx, mut rx) = mpsc::channel(10);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 10,
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
        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
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
        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
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
        let events =
            IngestionEvent::expand_journal_event(&event, "u1").expect("valid journal event");
        assert_eq!(
            events.len(),
            1,
            "plan_progress should not be expanded into tool_call sub-events"
        );
        assert_eq!(events[0].event_type, "plan_progress");
    }

    #[test]
    fn session_end_event_type_matches_insert_batch_check() {
        // insert_batch checks this exact event type to close sessions.
        // Verify the transform produces exactly that string.
        let event = crate::session_journal::JournalEvent::session_end(Some("s1"), 5);
        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
        assert_eq!(
            ingestion.event_type, SESSION_END_EVENT_TYPE,
            "event_type must be exactly 'session_end' for insert_batch session-close logic"
        );
    }

    // ── expand_journal_event tests ──────────────────────────────────────

    #[test]
    fn expand_turn_without_tool_calls_returns_one_event() {
        let journal = make_turn_event();
        let events =
            IngestionEvent::expand_journal_event(&journal, "user-1").expect("valid journal event");
        assert_eq!(events.len(), 1);
        assert!(events[0].event_type.contains("turn"));
    }

    #[test]
    fn expand_turn_with_tool_calls_returns_extra_events() {
        let mut journal = make_turn_event();
        journal.tool_calls = Some(vec![
            crate::session_journal::ToolCallRecord {
                name: " ".into(),
                ok: true,
                ms: 1,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
            crate::session_journal::ToolCallRecord {
                name: " git ".into(),
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

        let events =
            IngestionEvent::expand_journal_event(&journal, "user-1").expect("valid journal event");
        // 1 turn event + 2 nonblank tool_call events
        assert_eq!(events.len(), 3, "expected 3 events, got {}", events.len());

        // First is the main turn event
        assert!(events[0].event_type.contains("turn"));

        // Second is successful tool_call
        assert_eq!(events[1].event_type, "tool_call");
        assert_eq!(events[1].skill_name.as_deref(), Some("git"));
        assert_eq!(
            events[1].metadata.as_ref().unwrap()["tool_name"],
            serde_json::json!("git")
        );
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

        let events =
            IngestionEvent::expand_journal_event(&journal, "u1").expect("valid journal event");
        let tc_event = &events[1];
        let meta = tc_event.metadata.as_ref().unwrap();
        assert_eq!(meta["tool_name"], "bash");
        assert_eq!(meta["ok"], true);
        assert_eq!(meta["duration_ms"], 500);
        assert_eq!(meta["turn"], 3);
    }

    #[test]
    fn expand_tool_call_events_include_ask_user_metadata() {
        let mut journal = make_turn_event();
        journal.tool_calls = Some(vec![crate::session_journal::ToolCallRecord {
            name: "ask_user".into(),
            ok: false,
            ms: 125,
            error: Some("Error: ask_user was cancelled by the user".into()),
            ask_user: Some(serde_json::json!({
                "prompt": {"question_count": 2, "headers": ["Scope", "Notes"]},
                "response": {"outcome": "cancelled", "answered_question_count": 0},
            })),
            ..Default::default()
        }]);

        let events =
            IngestionEvent::expand_journal_event(&journal, "u1").expect("valid journal event");
        let meta = events[1].metadata.as_ref().expect("metadata");
        assert_eq!(meta["tool_name"], "ask_user");
        assert_eq!(meta["ask_user"]["response"]["outcome"], "cancelled");
        assert_eq!(meta["ask_user"]["prompt"]["question_count"], 2);
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

        let a = IngestionEvent::expand_journal_event(&journal, "u1").expect("valid journal event");
        let b = IngestionEvent::expand_journal_event(&journal, "u1").expect("valid journal event");
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
            &["github".to_string()],
        );
        let events =
            IngestionEvent::expand_journal_event(&event, "u1").expect("valid journal event");
        assert_eq!(events.len(), 1, "non-turn events should not be expanded");
    }

    // ── Batching / pipeline logic (no DB required) ──

    #[tokio::test]
    async fn sender_enqueue_async_respects_backpressure() {
        let (tx, mut rx) = mpsc::channel(3);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 3,
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
    async fn sender_enqueue_defers_until_capacity_is_available() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 1,
        };

        sender.enqueue(test_event("e1", "s1", "turn"));
        sender.enqueue(test_event("e2", "s1", "turn"));

        let first = rx.recv().await.unwrap();
        assert_eq!(first.event_id, "e1");
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("deferred send should complete")
            .expect("second event");
        assert_eq!(second.event_id, "e2");
    }

    #[test]
    fn event_id_is_deterministic_across_calls() {
        // Same journal event → same event_id (idempotency guarantee)
        let event = make_turn_event();
        let id1 = IngestionEvent::from_journal_event(&event, "u1")
            .expect("valid journal event")
            .event_id;
        let id2 = IngestionEvent::from_journal_event(&event, "u1")
            .expect("valid journal event")
            .event_id;
        assert_eq!(
            id1, id2,
            "event_id must be deterministic for INSERT IGNORE dedup"
        );
    }

    #[test]
    fn event_id_differs_for_different_users() {
        // Different user_id → different event_id (user isolation)
        let event = make_turn_event();
        let id1 = IngestionEvent::from_journal_event(&event, "user-a")
            .expect("valid journal event")
            .event_id;
        let id2 = IngestionEvent::from_journal_event(&event, "user-b")
            .expect("valid journal event")
            .event_id;
        // event_id is based on session/turn/type/ts, not user_id — so they may be equal
        // but both must be valid evt- prefixed strings
        assert!(id1.starts_with("evt-"), "event_id should have evt- prefix");
        assert!(id2.starts_with("evt-"), "event_id should have evt- prefix");
    }

    #[test]
    fn token_usage_uses_disjoint_canonical_shape_and_derived_columns() {
        let mut event = make_turn_event();
        event.tokens_in = Some(300);
        event.tokens_out = Some(150);
        event.cache_read_tokens = Some(20);
        event.cache_creation_tokens = Some(10);

        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
        let usage = ingestion.token_usage.as_ref().unwrap();

        assert_eq!(usage["input_tokens"], 300);
        assert_eq!(usage["cached_input_tokens"], 20);
        assert_eq!(usage["cache_creation_tokens"], 10);
        assert_eq!(usage["output_tokens"], 150);
        assert_eq!(usage["total_tokens"], 480);
        assert_eq!(usage["prompt"], 330);
        assert_eq!(usage["completion"], 150);
        assert_eq!(usage["cache_read"], 20);
        assert_eq!(usage["cache_write"], 10);
        assert_eq!(usage["total"], 480);

        let values = IngestionEventInsertValues::from_event(&ingestion).expect("canonical usage");
        assert_eq!(values.token_input, Some(330));
        assert_eq!(values.token_output, Some(150));
        assert_eq!(values.token_total, Some(480));
    }

    #[test]
    fn invalid_token_usage_does_not_poison_event_insert_values() {
        let mut ingestion = test_event("evt-bad-usage", "sess-1", "llm_response");
        ingestion.token_usage = Some(serde_json::json!({
            "input_tokens": 10,
            "cached_input_tokens": 1,
            "cache_creation_tokens": 0,
            "output_tokens": 5,
            "total_tokens": 999,
        }));

        let values = IngestionEventInsertValues::from_event(&ingestion)
            .expect("malformed token_usage should not block event persistence");

        assert_eq!(values.token_usage_json, None);
        assert_eq!(values.token_input, None);
        assert_eq!(values.token_output, None);
        assert_eq!(values.token_total, None);
    }

    #[test]
    fn token_usage_column_overflow_does_not_poison_event_insert_values() {
        let input_tokens = i64::MAX as u64 + 1;
        let mut ingestion = test_event("evt-huge-usage", "sess-1", "llm_response");
        ingestion.token_usage = Some(serde_json::json!({
            "input_tokens": input_tokens,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 0,
            "total_tokens": input_tokens,
        }));

        let values = IngestionEventInsertValues::from_event(&ingestion)
            .expect("overflowing token_usage columns should not block event persistence");

        assert_eq!(values.token_usage_json, None);
        assert_eq!(values.token_input, None);
        assert_eq!(values.token_output, None);
        assert_eq!(values.token_total, None);
    }

    #[test]
    fn llm_round_high_cache_preserves_usage_when_cache_exceeds_fresh_input() {
        let mut event = make_turn_event();
        event.event_type = crate::session_journal::JournalEventType::LlmRound;
        event.tokens_in = Some(10);
        event.tokens_out = Some(5);
        event.cache_read_tokens = Some(1_000);
        event.cache_creation_tokens = Some(50);

        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
        let usage = ingestion.token_usage.as_ref().unwrap();

        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["cached_input_tokens"], 1_000);
        assert_eq!(usage["cache_creation_tokens"], 50);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["prompt"], 1_060);
        assert_eq!(usage["total_tokens"], 1_065);

        let values = IngestionEventInsertValues::from_event(&ingestion).expect("canonical usage");
        assert_eq!(values.token_input, Some(1_060));
        assert_eq!(values.token_output, Some(5));
        assert_eq!(values.token_total, Some(1_065));
    }

    #[test]
    fn token_usage_absent_when_no_tokens() {
        let mut event = make_turn_event();
        event.tokens_in = None;
        event.tokens_out = None;

        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
        assert!(
            ingestion.token_usage.is_none(),
            "token_usage should be None when no token data"
        );
    }

    #[test]
    fn token_usage_degrades_to_absent_on_partial_or_overflowing_journal_tokens() {
        let mut partial = make_turn_event();
        partial.tokens_out = None;
        let ingestion = IngestionEvent::from_journal_event(&partial, "u1")
            .expect("partial token data must not drop the event");
        assert!(
            ingestion.token_usage.is_none(),
            "partial token data should only drop token_usage"
        );

        let mut overflowing = make_turn_event();
        overflowing.tokens_in = Some(u64::MAX);
        overflowing.tokens_out = Some(1);
        overflowing.cache_read_tokens = Some(1);
        let ingestion = IngestionEvent::from_journal_event(&overflowing, "u1")
            .expect("overflowing token data must not drop the event");
        assert!(
            ingestion.token_usage.is_none(),
            "overflowing token buckets should only drop token_usage"
        );
    }

    #[test]
    fn insert_values_drops_invalid_token_usage_without_dropping_event() {
        let mut event = test_event("evt-noncanonical", "sess-1", "turn_complete");
        event.token_usage = Some(serde_json::json!({
            "prompt": 10,
            "completion": 5,
        }));

        let values = IngestionEventInsertValues::from_event(&event)
            .expect("noncanonical token_usage must not drop the event");
        assert_eq!(values.token_usage_json, None);
        assert_eq!(values.token_input, None);
        assert_eq!(values.token_output, None);
        assert_eq!(values.token_total, None);
    }

    #[test]
    fn insert_values_preserve_zero_canonical_token_usage_columns() {
        let mut event = test_event("evt-zero-usage", "sess-1", "turn_complete");
        event.token_usage = Some(serde_json::json!({
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0,
        }));

        let values = IngestionEventInsertValues::from_event(&event)
            .expect("zero canonical token usage is valid");
        assert_eq!(values.token_input, Some(0));
        assert_eq!(values.token_output, Some(0));
        assert_eq!(values.token_total, Some(0));
    }

    #[test]
    fn insert_values_canonicalize_skill_name() {
        let mut event = test_event("evt-skill-name", "sess-1", "tool_call");
        event.skill_name = Some(" skill ".to_string());
        let values = IngestionEventInsertValues::from_event(&event).expect("valid event");
        assert_eq!(values.skill_name.as_deref(), Some("skill"));

        event.skill_name = Some(" ".to_string());
        let values = IngestionEventInsertValues::from_event(&event).expect("valid event");
        assert_eq!(values.skill_name, None);
    }

    #[test]
    fn content_priority_user_input_over_error() {
        // user_input takes priority over error as content
        let mut event = make_turn_event();
        event.user_input = Some("user question".into());
        event.error = Some("some error".into());

        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
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

        let ingestion =
            IngestionEvent::from_journal_event(&event, "u1").expect("valid journal event");
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
        assert_eq!(sender.dropped_before_acceptance_count(), 0);
    }

    #[tokio::test]
    async fn overflow_count_increments_on_full_channel() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 2,
        };

        sender.enqueue(test_event("e1", "s1", "turn"));
        sender.enqueue(test_event("e2", "s1", "turn"));

        assert_eq!(sender.overflow_count(), 1, "second enqueue should overflow");

        sender.enqueue(test_event("e3", "s1", "turn"));
        assert_eq!(sender.overflow_count(), 2);

        let first = rx.recv().await.expect("first event");
        assert_eq!(first.event_id, "e1");
        let mut deferred = Vec::new();
        for _ in 0..2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("deferred send should complete")
                .expect("deferred event");
            deferred.push(event.event_id);
        }
        deferred.sort();
        assert_eq!(deferred, vec!["e2".to_string(), "e3".to_string()]);
        assert_eq!(sender.dropped_before_acceptance_count(), 0);
    }

    #[tokio::test]
    async fn sender_enqueue_caps_pending_deferrals() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
            max_pending_deferrals: 1,
        };

        sender.enqueue(test_event("e1", "s1", "turn"));
        sender.enqueue(test_event("e2", "s1", "turn"));
        sender.enqueue(test_event("e3", "s1", "turn"));

        assert_eq!(sender.overflow_count(), 2);
        assert_eq!(sender.dropped_before_acceptance_count(), 1);
        assert_eq!(
            sender.pending_deferral_count(),
            1,
            "only one full-channel event should be allowed to wait for capacity"
        );
        assert_eq!(rx.recv().await.expect("first event").event_id, "e1");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("reserved deferred send should complete")
                .expect("deferred event")
                .event_id,
            "e2"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "event beyond the deferred-send cap must be dropped instead of spawning another waiter"
        );
        assert_eq!(sender.pending_deferral_count(), 0);
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
    async fn shutdown_drains_deferred_full_channel_sends() {
        let (tx, rx) = mpsc::channel(1);
        let stats = Arc::new(std::sync::Mutex::new(IngestionStats::default()));
        let pending_deferrals = Arc::new(AtomicUsize::new(0));
        let worker = EventIngestionWorker {
            rx,
            pool: dummy_pool(),
            config: IngestionConfig {
                batch_size: 100,
                max_retries: 1,
                ..IngestionConfig::default()
            }
            .normalized(),
            stats: Arc::clone(&stats),
            pending_deferrals: Arc::clone(&pending_deferrals),
        };
        let sender = IngestionSender {
            tx,
            overflow_count: Arc::new(AtomicU64::new(0)),
            dropped_before_acceptance_count: Arc::new(AtomicU64::new(0)),
            pending_deferrals,
            max_pending_deferrals: 1,
        };

        sender.enqueue(test_event("e1", "s1", "turn"));
        sender.enqueue(test_event("e2", "s1", "turn"));
        assert_eq!(sender.overflow_count(), 1);

        let shutdown = Arc::new(tokio::sync::Notify::new());
        let handle = tokio::spawn(worker.run_with_shutdown(Arc::clone(&shutdown)));
        shutdown.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), handle)
            .await
            .expect("worker should stop")
            .expect("worker task should not panic");

        let s = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(
            s.events_received, 2,
            "shutdown drain must include deferred sends that become ready after capacity is freed"
        );
        assert_eq!(sender.pending_deferral_count(), 0);
    }

    #[tokio::test]
    async fn spawn_normalizes_zero_config_before_starting_worker() {
        let config = IngestionConfig {
            batch_size: 0,
            flush_interval_secs: 0,
            channel_capacity: 0,
            max_retries: 0,
            ..Default::default()
        };
        let (_sender, shutdown, _stats, jh) = EventIngestionWorker::spawn(dummy_pool(), config);

        shutdown.signal();

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), jh).await;
        assert!(
            result.is_ok(),
            "worker should start and stop with normalized zero config"
        );
    }

    #[tokio::test]
    async fn failed_flush_retains_batch_for_next_retry_cycle() {
        let (_tx, rx) = mpsc::channel(1);
        let stats = Arc::new(std::sync::Mutex::new(IngestionStats::default()));
        let worker = EventIngestionWorker {
            rx,
            pool: dummy_pool(),
            config: IngestionConfig {
                max_retries: 1,
                ..IngestionConfig::default()
            }
            .normalized(),
            stats: Arc::clone(&stats),
            pending_deferrals: Arc::new(AtomicUsize::new(0)),
        };
        let mut buffer = vec![
            test_event("failed-e1", "s1", "turn"),
            test_event("failed-e2", "s1", "turn"),
        ];

        worker.flush_batch(&mut buffer).await;

        assert_eq!(
            buffer.len(),
            2,
            "normal flush failures must keep the batch in memory for a later retry"
        );
        let s = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(s.errors, 1);
        assert!(
            s.last_error
                .as_deref()
                .unwrap_or_default()
                .contains("batch flush failed after 1 retries")
        );
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

        let s = astra_core::sync_poison::recover_mutex_lock(&stats);
        assert_eq!(
            s.events_received, 5,
            "all 5 events should be counted (recv + drain)"
        );
        assert_eq!(
            s.errors, 1,
            "shutdown uses a single flush attempt after draining"
        );
        assert_eq!(
            s.flush_count, 0,
            "dummy pool must fail, so no successful flush should be counted"
        );
        assert!(
            s.last_error
                .as_deref()
                .unwrap_or_default()
                .contains("shutdown flush failed"),
            "shutdown drain should report the single-attempt flush failure: {:?}",
            s.last_error
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
        let ingestion =
            IngestionEvent::from_journal_event(&journal, "user-x").expect("valid journal event");

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
        let ingestion =
            IngestionEvent::from_journal_event(&event, "user-y").expect("valid journal event");
        // Turn events without context_assembly_trace should not have it in metadata
        if let Some(ref meta) = ingestion.metadata {
            assert!(
                meta.get("context_assembly_trace").is_none(),
                "should not have context_assembly_trace when absent"
            );
        }
    }
}
