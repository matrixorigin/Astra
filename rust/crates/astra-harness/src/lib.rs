pub mod adjust;
pub mod debug;
pub mod forensics;
mod kernel;
pub mod query;
pub mod rollback;
pub mod scenario;
mod snapshot_diff;
pub mod trace;
pub mod verifiers;

pub use adjust::{AdjustCommand, AdjustSender, adjust_channel};
pub use debug::{Breakpoint, DebugKernel};
pub use kernel::{HarnessLimits, StandardKernel};
pub use query::{HarnessQueryReceiver, HarnessQuerySender, query_channel};
pub use rollback::RollbackAssessment;
pub use snapshot_diff::SnapshotDiff;
pub use trace::{PrivacyPolicy, RecordingKernel, SessionTrace, TraceOutcome};

use std::sync::Arc;

// ─── Runtime Snapshot ───────────────────────────────────────────────────────

/// Read-only counters extracted from agentic loop state at each hook point.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSnapshot {
    // ── Identity ──
    pub session_id: String,
    pub turn_number: u32,
    pub model: Option<String>,

    // ── Context window ──
    pub context_total_tokens: Option<u32>,
    pub context_budget_tokens: Option<u32>,
    pub context_message_count: u32,
    pub context_system_prompt_tokens: Option<u32>,
    pub context_utilization: Option<f32>,

    // ── Budget (inner agentic loop iterations) ──
    pub turns_used: u32,
    pub turns_limit: Option<u32>,
    /// Outer session/REPL turn number (1-based). Distinct from inner loop rounds.
    #[serde(default)]
    pub session_turn: u32,
    pub tokens_used_session: u64,
    #[serde(default)]
    pub tokens_prompt: u64,
    #[serde(default)]
    pub tokens_completion: u64,
    #[serde(default)]
    pub tokens_cache_read: u64,
    #[serde(default)]
    pub tokens_cache_creation: u64,
    pub elapsed_millis: u64,

    // ── Tools ──
    pub tool_calls_this_session: u32,
    pub unique_tools_used: Vec<String>,
    pub last_tool_called: Option<String>,

    // ── Behavior signals ──
    pub consecutive_same_tool: u32,

    // ── Delegation (Phase 4) ──
    #[serde(default)]
    pub delegations_this_turn: u32,
    #[serde(default)]
    pub recursion_depth: u8,

    // ── Error tracking (Phase 4) ──
    #[serde(default)]
    pub consecutive_errors: u32,

    // ── Timestamps ──
    pub captured_at_unix_millis: u64,
    pub session_start_unix_millis: u64,

    // ── Audit ──
    #[serde(default)]
    pub causal_chain_id: Option<String>,

    // ── Schema ──
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

fn default_schema_version() -> u32 {
    2
}

impl RuntimeSnapshot {
    pub fn empty() -> Self {
        Self {
            session_id: String::new(),
            turn_number: 0,
            model: None,
            context_total_tokens: None,
            context_budget_tokens: None,
            context_message_count: 0,
            context_system_prompt_tokens: None,
            context_utilization: None,
            turns_used: 0,
            turns_limit: None,
            session_turn: 0,
            tokens_used_session: 0,
            tokens_prompt: 0,
            tokens_completion: 0,
            tokens_cache_read: 0,
            tokens_cache_creation: 0,
            elapsed_millis: 0,
            tool_calls_this_session: 0,
            unique_tools_used: Vec::new(),
            last_tool_called: None,
            consecutive_same_tool: 0,
            delegations_this_turn: 0,
            recursion_depth: 0,
            consecutive_errors: 0,
            captured_at_unix_millis: 0,
            session_start_unix_millis: 0,
            causal_chain_id: None,
            schema_version: 2,
        }
    }
}

// ─── Snapshot Access Policy ─────────────────────────────────────────────────

/// Controls what `/inspect` exposes. Default is metadata-only (production safe).
#[derive(Debug, Clone, Copy, Default)]
pub enum SnapshotAccessPolicy {
    #[default]
    MetadataOnly,
    DebugPreview {
        max_chars: usize,
    },
}

// ─── Hook Points ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HookPoint {
    SessionStart,
    PreLlmRequest,
    PostLlmResponse,
    PreToolBatch,
    PostToolBatch,
    PostTurn,
    SessionEnd,
}

// ─── Decision Record ────────────────────────────────────────────────────────

/// Context passed to verifiers at each hook invocation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecisionRecord {
    pub session_id: String,
    pub turn: u32,
    pub point: HookPoint,
    pub wall_time_unix_millis: u64,
    pub monotonic_millis_since_session: u64,
    pub snapshot: RuntimeSnapshot,
}

// ─── Violation ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct Violation {
    pub severity: Severity,
    pub verifier: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Severity {
    Warning,
    Error,
    Fatal,
}

// ─── Hook Verdict ───────────────────────────────────────────────────────────

pub enum HookVerdict {
    Continue,
    Block { reason: String },
    Pause { reason: String },
}

// ─── Verifier Trait ─────────────────────────────────────────────────────────

pub trait Verifier: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn trigger_points(&self) -> &'static [HookPoint];
    fn check(&self, record: &DecisionRecord) -> Vec<Violation>;
    /// If true, a panic in this verifier blocks the session (fail-closed).
    /// If false (default), a panic is logged and skipped (fail-open).
    fn is_critical(&self) -> bool {
        false
    }
}

// ─── Snapshot Sink ──────────────────────────────────────────────────────────

/// Storage backend for snapshots. CLI uses in-memory; server uses DB-backed.
pub trait SnapshotSink: Send + Sync + 'static {
    fn update(&self, record: &DecisionRecord);
    fn latest(&self) -> Option<RuntimeSnapshot>;
    /// Return the most recent `n` snapshots (newest first).
    fn history(&self, n: usize) -> Vec<RuntimeSnapshot> {
        self.latest()
            .into_iter()
            .collect::<Vec<_>>()
            .into_iter()
            .take(n)
            .collect()
    }
}

const DEFAULT_HISTORY_CAPACITY: usize = 64;

/// In-memory sink with bounded history ring for CLI single-process use.
pub struct InMemorySnapshotSink {
    inner: std::sync::RwLock<SnapshotRing>,
}

struct SnapshotRing {
    buf: std::collections::VecDeque<RuntimeSnapshot>,
    capacity: usize,
}

impl SnapshotRing {
    fn new(capacity: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, snap: RuntimeSnapshot) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(snap);
    }

    fn latest(&self) -> Option<&RuntimeSnapshot> {
        self.buf.back()
    }

    fn history(&self, n: usize) -> Vec<RuntimeSnapshot> {
        self.buf.iter().rev().take(n).cloned().collect()
    }
}

impl InMemorySnapshotSink {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_HISTORY_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: std::sync::RwLock::new(SnapshotRing::new(capacity)),
        }
    }

    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl Default for InMemorySnapshotSink {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotSink for InMemorySnapshotSink {
    fn update(&self, record: &DecisionRecord) {
        match self.inner.write() {
            Ok(mut guard) => guard.push(record.snapshot.clone()),
            Err(poison) => {
                tracing::error!("InMemorySnapshotSink write lock poisoned — recovering");
                poison.into_inner().push(record.snapshot.clone());
            }
        }
    }

    fn latest(&self) -> Option<RuntimeSnapshot> {
        match self.inner.read() {
            Ok(g) => g.latest().cloned(),
            Err(poison) => {
                tracing::error!("InMemorySnapshotSink lock poisoned — recovering");
                poison.into_inner().latest().cloned()
            }
        }
    }

    fn history(&self, n: usize) -> Vec<RuntimeSnapshot> {
        match self.inner.read() {
            Ok(g) => g.history(n),
            Err(poison) => {
                tracing::error!("InMemorySnapshotSink lock poisoned — recovering");
                poison.into_inner().history(n)
            }
        }
    }
}

// ─── Harness Kernel Trait ───────────────────────────────────────────────────

pub trait HarnessKernel: Send + Sync + 'static {
    fn snapshot(&self) -> Option<RuntimeSnapshot>;
    fn on_record(&self, record: &DecisionRecord) -> HookVerdict;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_snapshot_serde_roundtrip() {
        let snap = RuntimeSnapshot {
            session_id: "ses-123".into(),
            turn_number: 7,
            model: Some("claude-sonnet-4-6".into()),
            context_total_tokens: Some(50_000),
            context_budget_tokens: Some(200_000),
            context_message_count: 42,
            context_system_prompt_tokens: Some(3_000),
            context_utilization: Some(0.25),
            turns_used: 7,
            turns_limit: Some(20),
            session_turn: 3,
            tokens_used_session: 150_000,
            tokens_prompt: 90_000,
            tokens_completion: 30_000,
            tokens_cache_read: 20_000,
            tokens_cache_creation: 10_000,
            elapsed_millis: 45_000,
            tool_calls_this_session: 12,
            unique_tools_used: vec!["bash".into(), "read_file".into()],
            last_tool_called: Some("bash".into()),
            consecutive_same_tool: 2,
            delegations_this_turn: 0,
            recursion_depth: 0,
            consecutive_errors: 0,
            captured_at_unix_millis: 1_700_000_000_000,
            session_start_unix_millis: 1_700_000_000_000 - 45_000,
            schema_version: 2,
            causal_chain_id: None,
        };

        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: RuntimeSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.session_id, "ses-123");
        assert_eq!(deserialized.turn_number, 7);
        assert_eq!(deserialized.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(deserialized.context_total_tokens, Some(50_000));
        assert_eq!(deserialized.context_utilization, Some(0.25));
        assert_eq!(deserialized.unique_tools_used.len(), 2);
        assert_eq!(deserialized.consecutive_same_tool, 2);
    }

    #[test]
    fn runtime_snapshot_empty_all_zeroes() {
        let snap = RuntimeSnapshot::empty();
        assert!(snap.session_id.is_empty());
        assert_eq!(snap.turn_number, 0);
        assert!(snap.model.is_none());
        assert!(snap.context_total_tokens.is_none());
        assert!(snap.unique_tools_used.is_empty());
        assert!(snap.last_tool_called.is_none());
    }

    #[test]
    fn snapshot_access_policy_default_is_metadata_only() {
        let policy = SnapshotAccessPolicy::default();
        assert!(matches!(policy, SnapshotAccessPolicy::MetadataOnly));
    }

    #[test]
    fn in_memory_sink_update_and_latest() {
        let sink = InMemorySnapshotSink::new();

        assert!(sink.latest().is_none());

        let record = DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                turns_used: 3,
                ..RuntimeSnapshot::empty()
            },
        };
        sink.update(&record);

        let snap = sink.latest().unwrap();
        assert_eq!(snap.turns_used, 3);
    }

    #[test]
    fn in_memory_sink_history_returns_newest_first() {
        let sink = InMemorySnapshotSink::new();

        for i in 1..=5 {
            let record = DecisionRecord {
                session_id: "test".into(),
                turn: i,
                point: HookPoint::PostTurn,
                wall_time_unix_millis: 0,
                monotonic_millis_since_session: 0,
                snapshot: RuntimeSnapshot {
                    turns_used: i,
                    ..RuntimeSnapshot::empty()
                },
            };
            sink.update(&record);
        }

        let history = sink.history(3);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].turns_used, 5);
        assert_eq!(history[1].turns_used, 4);
        assert_eq!(history[2].turns_used, 3);
    }

    #[test]
    fn in_memory_sink_history_bounded_capacity() {
        let sink = InMemorySnapshotSink::with_capacity(3);

        for i in 1..=5 {
            let record = DecisionRecord {
                session_id: "test".into(),
                turn: i,
                point: HookPoint::PostTurn,
                wall_time_unix_millis: 0,
                monotonic_millis_since_session: 0,
                snapshot: RuntimeSnapshot {
                    turns_used: i,
                    ..RuntimeSnapshot::empty()
                },
            };
            sink.update(&record);
        }

        // Only last 3 should survive
        let history = sink.history(10);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].turns_used, 5);
        assert_eq!(history[2].turns_used, 3);
    }

    #[test]
    fn hook_point_serde() {
        let json = serde_json::to_string(&HookPoint::PreToolBatch).unwrap();
        assert_eq!(json, "\"PreToolBatch\"");
    }

    #[test]
    fn decision_record_serializes() {
        let record = DecisionRecord {
            session_id: "s1".into(),
            turn: 0,
            point: HookPoint::SessionStart,
            wall_time_unix_millis: 1_000,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot::empty(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("SessionStart"));
        assert!(json.contains("s1"));
    }
}
