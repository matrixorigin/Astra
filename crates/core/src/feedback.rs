//! Feedback signals retained for observation and self-model inputs.
//!
//! This module intentionally does not tune runtime policy or mutate config.
//! Durable tuning jobs should consume these signals through the observation
//! plane instead of running an implicit in-process rule engine.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const DEFAULT_MAX_SIGNALS: usize = 1000;

/// A feedback signal from user behavior, explicit rating, or runtime outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackSignal {
    /// Signal type.
    pub signal_type: SignalType,
    /// When the signal was recorded.
    pub timestamp: SystemTime,
    /// Associated turn ID.
    pub turn_id: Option<String>,
    /// Additional context.
    pub context: HashMap<String, serde_json::Value>,
}

impl FeedbackSignal {
    /// Create a new feedback signal of the given type, timestamped to now.
    pub fn new(signal_type: SignalType) -> Self {
        Self {
            signal_type,
            timestamp: SystemTime::now(),
            turn_id: None,
            context: HashMap::new(),
        }
    }

    /// Associate this signal with a specific turn.
    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    /// Attach arbitrary context metadata to this signal.
    pub fn with_context(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.context.insert(key.into(), value);
        self
    }
}

/// Types of feedback signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalType {
    // Implicit signals.
    /// User retried the same query.
    Retry { count: u32 },
    /// User corrected agent output.
    Correction,
    /// User clarified or re-anchored the task without saying the previous
    /// output was wrong.
    Reanchor,
    /// User interrupted the agent.
    Interruption,
    /// User accepted output without changes.
    Acceptance,
    /// Fast follow-up, usually meaning continued engagement.
    QuickFollowUp { delay_ms: u64 },
    /// Long pause before next query.
    LongPause { delay_ms: u64 },

    // Explicit signals.
    /// Thumbs up/down rating.
    ThumbsRating { positive: bool },
    /// Numeric rating.
    StarRating { stars: u8 },
    /// Text feedback.
    TextFeedback { sentiment: Sentiment },

    // Behavioral signals.
    /// High token usage for the task.
    HighTokenUsage { tokens: u64, threshold: u64 },
    /// Many tool calls without progress.
    ToolChurn { calls: u32, unique_tools: u32 },
    /// Agent lost focus.
    FocusDrift,
    /// Task completed successfully.
    TaskSuccess,
    /// Task failed.
    TaskFailure { reason: String },

    // Tool health signals.
    /// A tool triggered health avoidance due to repeated failures.
    ToolHealthAvoidance { tool_name: String },
    /// A tool was rehabilitated after health avoidance.
    ToolRehabilitated { tool_name: String },
}

impl SignalType {
    /// Canonical string name for this signal type.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Retry { .. } => "retry",
            Self::Correction => "correction",
            Self::Reanchor => "reanchor",
            Self::Interruption => "interruption",
            Self::Acceptance => "acceptance",
            Self::QuickFollowUp { .. } => "quick_follow_up",
            Self::LongPause { .. } => "long_pause",
            Self::ThumbsRating { .. } => "thumbs_rating",
            Self::StarRating { .. } => "star_rating",
            Self::TextFeedback { .. } => "text_feedback",
            Self::HighTokenUsage { .. } => "high_token_usage",
            Self::ToolChurn { .. } => "tool_churn",
            Self::FocusDrift => "focus_drift",
            Self::TaskSuccess => "task_success",
            Self::TaskFailure { .. } => "task_failure",
            Self::ToolHealthAvoidance { .. } => "tool_health_avoidance",
            Self::ToolRehabilitated { .. } => "tool_rehabilitated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sentiment {
    Positive,
    Neutral,
    Negative,
}

/// A hint derived from feedback signal analysis for downstream adaptation.
///
/// These are pure observations — the store reports facts, it does not
/// decide what the runtime should do about them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptationHint {
    /// Hint category: "high_retry_rate", "elevated_failure_rate", etc.
    pub kind: String,
    /// Human-readable detail for logging or prompt injection.
    pub detail: String,
    /// Severity: "info", "warning", "critical".
    pub severity: String,
}

/// Aggregate statistics for streaming speculative tool execution.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamingSpeculationStats {
    /// Total speculations spawned across all reports.
    pub started: u64,
    /// Total hits merged back into real batches.
    pub hit: u64,
    /// Total discards.
    pub discarded: u64,
    /// Sum of per-hit overlap durations in ms.
    pub total_saved_ms: u64,
    /// Number of report batches merged into this aggregate.
    pub reports: u64,
}

impl StreamingSpeculationStats {
    /// Hit rate in [0.0, 1.0]. Returns 0.0 if no speculations started.
    pub fn hit_rate(&self) -> f64 {
        if self.started == 0 {
            0.0
        } else {
            self.hit as f64 / self.started as f64
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FeedbackBuffer {
    signals: VecDeque<FeedbackSignal>,
}

/// Thread-safe in-memory signal buffer with deferred persistence.
///
/// # Batch persistence
///
/// To reduce filesystem pressure, `record()` buffers signals in memory
/// and only persists every [`BATCH_PERSIST_INTERVAL`] signals (default 10).
/// Call [`flush`] to force an immediate write (e.g., on shutdown or
/// before reading signals for adaptation).
pub struct FeedbackSignalStore {
    buffer: RwLock<FeedbackBuffer>,
    max_signals: usize,
    streaming_spec: RwLock<StreamingSpeculationStats>,
    storage_path: Option<PathBuf>,
    /// Number of signals recorded since last persist.
    dirty_count: RwLock<usize>,
}

/// Persist to disk every N signals to avoid excessive fs renames.
/// Tune based on expected signal volume.
pub const BATCH_PERSIST_INTERVAL: usize = 10;

impl Default for FeedbackSignalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedbackSignalStore {
    /// Create an empty feedback signal store.
    pub fn new() -> Self {
        Self {
            buffer: RwLock::new(FeedbackBuffer::default()),
            max_signals: DEFAULT_MAX_SIGNALS,
            streaming_spec: RwLock::new(StreamingSpeculationStats::default()),
            storage_path: None,
            dirty_count: RwLock::new(0),
        }
    }

    /// Create a feedback signal store backed by a JSON file.
    ///
    /// Corrupt or unreadable storage starts empty; the next successful record
    /// rewrites the file with a valid bounded snapshot.
    pub fn with_storage(path: PathBuf) -> Self {
        let mut buffer = if path.exists() {
            match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
                Err(_) => FeedbackBuffer::default(),
            }
        } else {
            FeedbackBuffer::default()
        };
        trim_buffer(&mut buffer, DEFAULT_MAX_SIGNALS);
        Self {
            buffer: RwLock::new(buffer),
            max_signals: DEFAULT_MAX_SIGNALS,
            streaming_spec: RwLock::new(StreamingSpeculationStats::default()),
            storage_path: Some(path),
            dirty_count: RwLock::new(0),
        }
    }

    /// Record a new feedback signal.
    ///
    /// Persists to disk only when `BATCH_PERSIST_INTERVAL` unpersisted
    /// signals have accumulated. Call [`flush`] for immediate persistence.
    pub fn record(&self, signal: FeedbackSignal) {
        let should_persist = {
            let mut buffer = self.buffer.write().unwrap_or_else(|e| e.into_inner());
            buffer.signals.push_back(signal);
            trim_buffer(&mut buffer, self.max_signals);
            let mut dirty = self.dirty_count.write().unwrap_or_else(|e| e.into_inner());
            *dirty += 1;
            *dirty >= BATCH_PERSIST_INTERVAL
        };
        if should_persist && let Err(err) = self.persist() {
            eprintln!("[feedback] persist failed: {err}");
        }
    }

    /// Return retained signals from oldest to newest.
    pub fn recent_signals(&self) -> Vec<FeedbackSignal> {
        let buffer = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        buffer.signals.iter().cloned().collect()
    }

    /// Record one batch of streaming-speculation metrics.
    pub fn record_streaming_speculation(
        &self,
        started: u64,
        hit: u64,
        discarded: u64,
        total_saved_ms: u64,
    ) {
        let mut stats = self
            .streaming_spec
            .write()
            .unwrap_or_else(|e| e.into_inner());
        stats.started = stats.started.saturating_add(started);
        stats.hit = stats.hit.saturating_add(hit);
        stats.discarded = stats.discarded.saturating_add(discarded);
        stats.total_saved_ms = stats.total_saved_ms.saturating_add(total_saved_ms);
        stats.reports = stats.reports.saturating_add(1);
    }

    /// Read aggregated streaming-speculation stats.
    pub fn streaming_speculation_stats(&self) -> StreamingSpeculationStats {
        self.streaming_spec
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Analyze recent feedback signals and produce adaptation hints.
    ///
    /// Returns a summary of the most frequent signal types and their
    /// sentiment distribution. Callers (e.g., RuntimePolicy or SelfModel)
    /// can use these hints to adjust behavior — the store only reports
    /// facts, it does not decide what to do with them.
    pub fn adaptation_hints(&self) -> Vec<AdaptationHint> {
        let signals = self.recent_signals();
        if signals.is_empty() {
            return Vec::new();
        }
        let mut hints = Vec::new();
        let total = signals.len() as f64;

        // Count by signal type
        let mut type_counts: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::new();
        for s in &signals {
            *type_counts.entry(s.signal_type.type_name()).or_default() += 1;
        }

        // Repeated retries suggest the current approach is failing
        let retry_count = type_counts.get("Retry").copied().unwrap_or(0);
        if retry_count as f64 / total > 0.3 {
            hints.push(AdaptationHint {
                kind: "high_retry_rate".to_string(),
                detail: format!(
                    "{} retry signals in {} feedback events — consider changing approach",
                    retry_count,
                    signals.len()
                ),
                severity: if retry_count > 5 {
                    "critical"
                } else {
                    "warning"
                }
                .to_string(),
            });
        }

        // High error rate signals
        let error_count = type_counts.get("TaskFailure").copied().unwrap_or(0);
        if error_count > 0 && error_count as f64 / total > 0.2 {
            hints.push(AdaptationHint {
                kind: "elevated_failure_rate".to_string(),
                detail: format!(
                    "{} task failures in {} feedback events",
                    error_count,
                    signals.len()
                ),
                severity: "warning".to_string(),
            });
        }

        hints
    }

    /// Count of unpersisted signals since last flush/persist.
    pub fn dirty_count(&self) -> usize {
        *self.dirty_count.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Force immediate persistence of all buffered signals.
    ///
    /// Resets the dirty counter. Call this before shutdown, before reading
    /// signals for adaptation analysis, or after a burst of high-value signals.
    pub fn flush(&self) -> std::io::Result<()> {
        self.persist()?;
        let mut dirty = self.dirty_count.write().unwrap_or_else(|e| e.into_inner());
        *dirty = 0;
        Ok(())
    }

    /// Persist retained feedback signals using an atomic rename.
    ///
    /// Returns `Ok(())` on success or the first I/O error encountered.
    /// Errors are not silently swallowed; callers decide whether to log or propagate.
    pub fn persist(&self) -> std::io::Result<()> {
        let Some(path) = &self.storage_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = {
            let buffer = self.buffer.read().unwrap_or_else(|e| e.into_inner());
            serde_json::to_vec_pretty(&*buffer).map_err(std::io::Error::other)?
        };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn trim_buffer(buffer: &mut FeedbackBuffer, max_signals: usize) {
    while buffer.signals.len() > max_signals {
        buffer.signals.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_recent_signals_in_order() {
        let store = FeedbackSignalStore::new();

        store.record(FeedbackSignal::new(SignalType::TaskSuccess).with_turn("t1"));
        store.record(FeedbackSignal::new(SignalType::Correction).with_turn("t2"));

        let signals = store.recent_signals();
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].turn_id.as_deref(), Some("t1"));
        assert_eq!(signals[1].turn_id.as_deref(), Some("t2"));
        assert_eq!(signals[1].signal_type.type_name(), "correction");
    }

    #[test]
    fn persists_and_reloads_feedback_signals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("feedback-signals.json");

        let store = FeedbackSignalStore::with_storage(path.clone());
        store.record(FeedbackSignal::new(SignalType::TaskSuccess).with_turn("t1"));
        store.record(FeedbackSignal::new(SignalType::Correction).with_turn("t2"));
        // Batch persist defers writes; flush before reloading.
        store.flush().unwrap();

        let reloaded = FeedbackSignalStore::with_storage(path);
        let signals = reloaded.recent_signals();
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].turn_id.as_deref(), Some("t1"));
        assert_eq!(signals[1].turn_id.as_deref(), Some("t2"));
        assert_eq!(signals[1].signal_type, SignalType::Correction);
    }

    #[test]
    fn storage_recovers_from_corrupt_file_on_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("feedback-signals.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not-json").unwrap();

        let store = FeedbackSignalStore::with_storage(path.clone());
        assert!(store.recent_signals().is_empty());

        store.record(FeedbackSignal::new(SignalType::TaskFailure {
            reason: "bad output".to_string(),
        }));
        // Batch persist defers writes; flush before reloading.
        store.flush().unwrap();

        let reloaded = FeedbackSignalStore::with_storage(path);
        let signals = reloaded.recent_signals();
        assert_eq!(signals.len(), 1);
        assert!(matches!(
            signals[0].signal_type,
            SignalType::TaskFailure { ref reason } if reason == "bad output"
        ));
    }

    #[test]
    fn streaming_speculation_metrics_are_observational_only() {
        let store = FeedbackSignalStore::new();

        store.record_streaming_speculation(10, 5, 1, 120);
        store.record_streaming_speculation(4, 2, 0, 30);

        let stats = store.streaming_speculation_stats();
        assert_eq!(stats.started, 14);
        assert_eq!(stats.hit, 7);
        assert_eq!(stats.discarded, 1);
        assert_eq!(stats.total_saved_ms, 150);
        assert_eq!(stats.reports, 2);
        assert!((stats.hit_rate() - 0.5).abs() < f64::EPSILON);
    }
}
