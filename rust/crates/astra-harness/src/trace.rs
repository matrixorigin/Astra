use crate::{DecisionRecord, HarnessKernel, HookPoint, HookVerdict, RuntimeSnapshot};
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

// ─── Session Trace Schema ───────────────────────────────────────────────────

/// Complete trace of a session's harness lifecycle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTrace {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub started_at_unix_millis: u64,
    pub ended_at_unix_millis: Option<u64>,
    pub total_turns: u32,
    pub outcome: TraceOutcome,
    pub records: VecDeque<DecisionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TraceOutcome {
    InProgress,
    Completed,
    Blocked,
    Error,
}

impl SessionTrace {
    pub fn new(session_id: Option<String>) -> Self {
        Self {
            session_id,
            model: None,
            started_at_unix_millis: 0,
            ended_at_unix_millis: None,
            total_turns: 0,
            outcome: TraceOutcome::InProgress,
            records: VecDeque::new(),
        }
    }

    // ── Query API (Trace Reader) ────────────────────────────────────────

    pub fn records_for_turn(&self, turn: u32) -> Vec<&DecisionRecord> {
        self.records.iter().filter(|r| r.turn == turn).collect()
    }

    pub fn records_at_point(&self, point: HookPoint) -> Vec<&DecisionRecord> {
        self.records.iter().filter(|r| r.point == point).collect()
    }

    pub fn turns_summary(&self) -> Vec<TurnSummary> {
        let mut turns: std::collections::BTreeMap<u32, TurnSummary> =
            std::collections::BTreeMap::new();

        for r in &self.records {
            let entry = turns.entry(r.turn).or_insert_with(|| TurnSummary {
                turn: r.turn,
                hook_count: 0,
                hooks_fired: Vec::new(),
                tokens_at_end: 0,
                tool_calls_at_end: 0,
            });
            entry.hook_count += 1;
            if !entry.hooks_fired.contains(&r.point) {
                entry.hooks_fired.push(r.point);
            }
            entry.tokens_at_end = r.snapshot.tokens_used_session;
            entry.tool_calls_at_end = r.snapshot.tool_calls_this_session;
        }

        turns.into_values().collect()
    }

    pub fn tool_calls_timeline(&self) -> Vec<ToolCallEntry> {
        self.records
            .iter()
            .filter(|r| r.point == HookPoint::PostToolBatch)
            .map(|r| ToolCallEntry {
                turn: r.turn,
                tool_calls: r.snapshot.tool_calls_this_session,
                last_tool: r.snapshot.last_tool_called.clone(),
                consecutive_same: r.snapshot.consecutive_same_tool,
            })
            .collect()
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn duration_millis(&self) -> Option<u64> {
        self.ended_at_unix_millis
            .map(|end| end.saturating_sub(self.started_at_unix_millis))
    }

    // ── Persistence ─────────────────────────────────────────────────────

    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, &json)?;
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }

    pub fn load_from_file(path: &std::path::Path) -> std::io::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn save_jsonl(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let tmp_path = path.with_extension("jsonl.tmp");
        let mut f = std::fs::File::create(&tmp_path)?;
        for record in &self.records {
            let line = serde_json::to_string(record)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(f, "{line}")?;
        }
        f.flush()?;
        drop(f);
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnSummary {
    pub turn: u32,
    pub hook_count: u32,
    pub hooks_fired: Vec<HookPoint>,
    pub tokens_at_end: u64,
    pub tool_calls_at_end: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolCallEntry {
    pub turn: u32,
    pub tool_calls: u32,
    pub last_tool: Option<String>,
    pub consecutive_same: u32,
}

// ─── Privacy Policy ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrivacyPolicy {
    /// Full trace with all data. Requires explicit opt-in.
    Full,
    /// Counters and metadata only — no tool names or model info.
    MetadataOnly,
    /// Redact sensitive fields (tool names → hashed, session_id → truncated).
    #[default]
    Redacted,
}

impl SessionTrace {
    /// Apply a privacy policy, returning a sanitized copy.
    pub fn with_privacy(&self, policy: PrivacyPolicy) -> Self {
        match policy {
            PrivacyPolicy::Full => self.clone(),
            PrivacyPolicy::MetadataOnly => {
                let mut trace = self.clone();
                trace.model = None;
                trace.session_id = trace.session_id.as_ref().map(|s| truncate_id(s));
                for r in &mut trace.records {
                    r.session_id = truncate_id(&r.session_id);
                    r.snapshot.session_id = truncate_id(&r.snapshot.session_id);
                    r.snapshot.model = None;
                    r.snapshot.unique_tools_used.clear();
                    r.snapshot.last_tool_called = None;
                }
                trace
            }
            PrivacyPolicy::Redacted => {
                let mut trace = self.clone();
                trace.session_id = trace.session_id.as_ref().map(|s| truncate_id(s));
                trace.model = trace.model.as_deref().map(|m| hash_name(m, "model"));
                for r in &mut trace.records {
                    r.session_id = truncate_id(&r.session_id);
                    r.snapshot.session_id = truncate_id(&r.snapshot.session_id);
                    r.snapshot.model = r.snapshot.model.as_deref().map(|m| hash_name(m, "model"));
                    r.snapshot.unique_tools_used = r
                        .snapshot
                        .unique_tools_used
                        .iter()
                        .map(|t| hash_name(t, "tool"))
                        .collect();
                    r.snapshot.last_tool_called = r
                        .snapshot
                        .last_tool_called
                        .as_deref()
                        .map(|t| hash_name(t, "tool"));
                }
                trace
            }
        }
    }
}

fn truncate_id(id: &str) -> String {
    let truncated: String = id.chars().take(8).collect();
    if truncated.len() < id.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Stable FNV-1a hash with 64-bit output.
///
/// Uses FNV-1a (not `DefaultHasher` which may change across Rust versions)
/// and emits a full 64-bit hex digest to avoid collisions at scale.
fn hash_name(name: &str, prefix: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    format!("{}_{:016x}", prefix, hash)
}

// ─── Recording Kernel (Trace Writer) ────────────────────────────────────────

const DEFAULT_MAX_TRACE_RECORDS: usize = 10_000;

/// A kernel wrapper that records every DecisionRecord into a SessionTrace
/// while delegating verdicts to an inner kernel.
pub struct RecordingKernel {
    inner: Arc<dyn HarnessKernel>,
    trace: Arc<RwLock<SessionTrace>>,
    max_records: usize,
}

impl RecordingKernel {
    pub fn new(inner: Arc<dyn HarnessKernel>, session_id: Option<String>) -> Self {
        Self {
            inner,
            trace: Arc::new(RwLock::new(SessionTrace::new(session_id))),
            max_records: DEFAULT_MAX_TRACE_RECORDS,
        }
    }

    pub fn with_max_records(mut self, max: usize) -> Self {
        self.max_records = max;
        self
    }

    /// Create with an externally-owned trace (shares the same Arc).
    pub fn with_trace(inner: Arc<dyn HarnessKernel>, trace: Arc<RwLock<SessionTrace>>) -> Self {
        Self {
            inner,
            trace,
            max_records: DEFAULT_MAX_TRACE_RECORDS,
        }
    }

    pub fn trace(&self) -> Arc<RwLock<SessionTrace>> {
        self.trace.clone()
    }

    pub fn into_trace(self) -> SessionTrace {
        match Arc::try_unwrap(self.trace) {
            Ok(lock) => lock
                .into_inner()
                .unwrap_or_else(|poison| poison.into_inner()),
            Err(arc) => arc
                .read()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone(),
        }
    }
}

impl HarnessKernel for RecordingKernel {
    fn snapshot(&self) -> Option<RuntimeSnapshot> {
        self.inner.snapshot()
    }

    fn on_record(&self, record: &DecisionRecord) -> HookVerdict {
        match self.trace.write() {
            Ok(mut trace) => {
                // Adopt session_id from first record (late-binding for resume/new-session flows)
                if trace.session_id.is_none() && !record.session_id.is_empty() {
                    trace.session_id = Some(record.session_id.clone());
                }
                if let Some(bound_session_id) = trace.session_id.as_deref() {
                    if !record.session_id.is_empty() && bound_session_id != record.session_id {
                        tracing::error!(
                            trace_session_id = bound_session_id,
                            record_session_id = %record.session_id,
                            "RecordingKernel rejected mismatched session_id"
                        );
                        trace.outcome = TraceOutcome::Error;
                        return HookVerdict::Continue;
                    }
                }
                if record.point == HookPoint::SessionStart {
                    trace.started_at_unix_millis = record.wall_time_unix_millis;
                    trace.model = record.snapshot.model.clone();
                }
                if record.point == HookPoint::SessionEnd {
                    trace.ended_at_unix_millis = Some(record.wall_time_unix_millis);
                }
                trace.total_turns = trace.total_turns.max(record.turn + 1);
                if trace.records.len() >= self.max_records {
                    trace.records.pop_front();
                }
                trace.records.push_back(record.clone());
            }
            Err(_) => {
                tracing::error!("RecordingKernel trace lock poisoned — record dropped");
            }
        }

        let verdict = self.inner.on_record(record);
        if let HookVerdict::Block { .. } = &verdict {
            match self.trace.write() {
                Ok(mut trace) => trace.outcome = TraceOutcome::Blocked,
                Err(_) => {
                    tracing::error!("RecordingKernel trace lock poisoned — outcome not updated");
                }
            }
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemorySnapshotSink, SnapshotSink, StandardKernel};

    fn make_record(session: &str, turn: u32, point: HookPoint) -> DecisionRecord {
        DecisionRecord {
            session_id: session.into(),
            turn,
            point,
            wall_time_unix_millis: 1_000_000 + turn as u64 * 1000,
            monotonic_millis_since_session: turn as u64 * 1000,
            snapshot: RuntimeSnapshot {
                session_id: session.into(),
                turn_number: turn,
                turns_used: turn,
                tokens_used_session: turn as u64 * 10_000,
                tool_calls_this_session: turn * 2,
                unique_tools_used: vec!["bash".into(), "read_file".into()],
                last_tool_called: Some("bash".into()),
                model: Some("claude-sonnet-4-6".into()),
                ..RuntimeSnapshot::empty()
            },
        }
    }

    fn make_recording_kernel() -> (RecordingKernel, Arc<InMemorySnapshotSink>) {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(
            sink.clone() as Arc<dyn SnapshotSink>,
            vec![],
        ));
        let kernel = RecordingKernel::new(inner, Some("test-session".into()));
        (kernel, sink)
    }

    // ── SessionTrace schema tests ───────────────────────────────────────

    #[test]
    fn trace_new_is_empty() {
        let trace = SessionTrace::new(Some("s1".into()));
        assert_eq!(trace.session_id, Some("s1".into()));
        assert_eq!(trace.outcome, TraceOutcome::InProgress);
        assert!(trace.records.is_empty());
        assert_eq!(trace.total_turns, 0);
    }

    #[test]
    fn trace_serde_roundtrip() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace
            .records
            .push_back(make_record("s1", 0, HookPoint::SessionStart));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostTurn));

        let json = serde_json::to_string(&trace).unwrap();
        let deserialized: SessionTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.records.len(), 2);
        assert_eq!(deserialized.session_id, Some("s1".into()));
    }

    // ── Trace reader (query) tests ──────────────────────────────────────

    #[test]
    fn records_for_turn_filters_correctly() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace
            .records
            .push_back(make_record("s1", 0, HookPoint::SessionStart));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PreLlmRequest));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostLlmResponse));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostTurn));
        trace
            .records
            .push_back(make_record("s1", 2, HookPoint::PostTurn));

        assert_eq!(trace.records_for_turn(0).len(), 1);
        assert_eq!(trace.records_for_turn(1).len(), 3);
        assert_eq!(trace.records_for_turn(2).len(), 1);
        assert_eq!(trace.records_for_turn(99).len(), 0);
    }

    #[test]
    fn records_at_point_filters_correctly() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace
            .records
            .push_back(make_record("s1", 0, HookPoint::SessionStart));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostTurn));
        trace
            .records
            .push_back(make_record("s1", 2, HookPoint::PostTurn));

        assert_eq!(trace.records_at_point(HookPoint::SessionStart).len(), 1);
        assert_eq!(trace.records_at_point(HookPoint::PostTurn).len(), 2);
        assert_eq!(trace.records_at_point(HookPoint::PreToolBatch).len(), 0);
    }

    #[test]
    fn turns_summary_aggregates() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PreLlmRequest));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostLlmResponse));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostToolBatch));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostTurn));
        trace
            .records
            .push_back(make_record("s1", 2, HookPoint::PostTurn));

        let summaries = trace.turns_summary();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].turn, 1);
        assert_eq!(summaries[0].hook_count, 4);
        assert_eq!(summaries[0].hooks_fired.len(), 4);
        assert_eq!(summaries[1].turn, 2);
        assert_eq!(summaries[1].hook_count, 1);
    }

    #[test]
    fn tool_calls_timeline() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostToolBatch));
        trace
            .records
            .push_back(make_record("s1", 1, HookPoint::PostTurn));
        trace
            .records
            .push_back(make_record("s1", 2, HookPoint::PostToolBatch));

        let timeline = trace.tool_calls_timeline();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].turn, 1);
        assert_eq!(timeline[1].turn, 2);
    }

    #[test]
    fn duration_millis() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.started_at_unix_millis = 1000;
        trace.ended_at_unix_millis = Some(5000);
        assert_eq!(trace.duration_millis(), Some(4000));

        trace.ended_at_unix_millis = None;
        assert_eq!(trace.duration_millis(), None);
    }

    // ── Privacy policy tests ────────────────────────────────────────────

    #[test]
    fn privacy_full_is_identity() {
        let mut trace = SessionTrace::new(Some("session-abc-123".into()));
        trace
            .records
            .push_back(make_record("session-abc-123", 1, HookPoint::PostTurn));

        let sanitized = trace.with_privacy(PrivacyPolicy::Full);
        assert_eq!(sanitized.session_id, Some("session-abc-123".into()));
        assert_eq!(sanitized.records[0].snapshot.unique_tools_used.len(), 2);
    }

    #[test]
    fn privacy_metadata_only_strips_tools_and_model() {
        let mut trace = SessionTrace::new(Some("session-abc-123".into()));
        trace.model = Some("claude-sonnet-4-6".into());
        trace
            .records
            .push_back(make_record("session-abc-123", 1, HookPoint::PostTurn));

        let sanitized = trace.with_privacy(PrivacyPolicy::MetadataOnly);
        assert_eq!(sanitized.session_id, Some("session-…".into()));
        assert!(sanitized.model.is_none());
        assert!(sanitized.records[0].snapshot.unique_tools_used.is_empty());
        assert!(sanitized.records[0].snapshot.last_tool_called.is_none());
        assert!(sanitized.records[0].snapshot.model.is_none());
    }

    #[test]
    fn privacy_redacted_hashes_tool_names() {
        let mut trace = SessionTrace::new(Some("session-abc-123".into()));
        trace
            .records
            .push_back(make_record("session-abc-123", 1, HookPoint::PostTurn));

        let sanitized = trace.with_privacy(PrivacyPolicy::Redacted);
        assert_eq!(sanitized.session_id, Some("session-…".into()));
        // Tools should be hashed, not original
        for t in &sanitized.records[0].snapshot.unique_tools_used {
            assert!(t.starts_with("tool_"));
        }
        let last = sanitized.records[0]
            .snapshot
            .last_tool_called
            .as_ref()
            .unwrap();
        assert!(last.starts_with("tool_"));
    }

    // ── Recording kernel tests ──────────────────────────────────────────

    #[test]
    fn recording_kernel_captures_records() {
        let (kernel, _sink) = make_recording_kernel();

        kernel.on_record(&make_record("test-session", 0, HookPoint::SessionStart));
        kernel.on_record(&make_record("test-session", 1, HookPoint::PostLlmResponse));
        kernel.on_record(&make_record("test-session", 1, HookPoint::PostTurn));

        let trace = kernel.trace();
        let trace = trace.read().unwrap();
        assert_eq!(trace.record_count(), 3);
        assert_eq!(trace.total_turns, 2);
    }

    #[test]
    fn recording_kernel_adopts_first_non_empty_session_id() {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(sink as Arc<dyn SnapshotSink>, vec![]));
        let kernel = RecordingKernel::new(inner, None);

        kernel.on_record(&make_record("", 0, HookPoint::SessionStart));
        kernel.on_record(&make_record("late-bound-session", 1, HookPoint::PostTurn));
        kernel.on_record(&make_record("", 2, HookPoint::PostTurn));

        let trace = kernel.into_trace();
        assert_eq!(trace.session_id, Some("late-bound-session".into()));
        assert_eq!(trace.record_count(), 3);
    }

    #[test]
    fn recording_kernel_rejects_mismatched_session_id_after_binding() {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(
            sink.clone() as Arc<dyn SnapshotSink>,
            vec![],
        ));
        let kernel = RecordingKernel::new(inner, None);

        kernel.on_record(&make_record("session-a", 0, HookPoint::SessionStart));
        kernel.on_record(&make_record("session-b", 1, HookPoint::PostTurn));

        let trace = kernel.into_trace();
        assert_eq!(trace.session_id, Some("session-a".into()));
        assert_eq!(trace.outcome, TraceOutcome::Error);
        assert_eq!(trace.record_count(), 1);
        assert_eq!(trace.records[0].session_id, "session-a");
        assert_eq!(sink.latest().unwrap().session_id, "session-a");
    }

    #[test]
    fn recording_kernel_sets_start_and_end_times() {
        let (kernel, _sink) = make_recording_kernel();

        kernel.on_record(&make_record("test-session", 0, HookPoint::SessionStart));
        kernel.on_record(&make_record("test-session", 1, HookPoint::PostTurn));
        kernel.on_record(&make_record("test-session", 1, HookPoint::SessionEnd));

        let trace = kernel.trace();
        let trace = trace.read().unwrap();
        assert_eq!(trace.started_at_unix_millis, 1_000_000);
        assert!(trace.ended_at_unix_millis.is_some());
    }

    #[test]
    fn recording_kernel_sets_model_from_session_start() {
        let (kernel, _sink) = make_recording_kernel();
        kernel.on_record(&make_record("test-session", 0, HookPoint::SessionStart));

        let trace = kernel.trace();
        let trace = trace.read().unwrap();
        assert_eq!(trace.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn recording_kernel_delegates_to_inner() {
        let (kernel, sink) = make_recording_kernel();
        kernel.on_record(&make_record("test-session", 1, HookPoint::PostTurn));

        // Inner kernel should have updated the sink
        assert!(sink.latest().is_some());
    }

    #[test]
    fn recording_kernel_marks_blocked_outcome() {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(
            sink as Arc<dyn SnapshotSink>,
            vec![Box::new(crate::verifiers::BudgetVerifier {
                max_turns: Some(1),
                max_tokens: None,
                max_duration_millis: None,
            })],
        ));
        let kernel = RecordingKernel::new(inner, Some("blocked-test".into()));

        let mut record = make_record("blocked-test", 2, HookPoint::PostTurn);
        record.snapshot.turns_used = 2;
        kernel.on_record(&record);

        let trace = kernel.trace();
        let trace = trace.read().unwrap();
        assert_eq!(trace.outcome, TraceOutcome::Blocked);
    }

    #[test]
    fn into_trace_returns_owned() {
        let (kernel, _sink) = make_recording_kernel();
        kernel.on_record(&make_record("test-session", 0, HookPoint::SessionStart));

        let trace = kernel.into_trace();
        assert_eq!(trace.record_count(), 1);
        assert_eq!(trace.session_id, Some("test-session".into()));
    }

    // ── Persistence tests ───────────────────────────────────────────────

    #[test]
    fn save_and_load_json() {
        let mut trace = SessionTrace::new(Some("persist-test".into()));
        trace
            .records
            .push_back(make_record("persist-test", 0, HookPoint::SessionStart));
        trace
            .records
            .push_back(make_record("persist-test", 1, HookPoint::PostTurn));
        trace.total_turns = 2;
        trace.outcome = TraceOutcome::Completed;

        let dir = std::env::temp_dir().join("astra-harness-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace_test.json");

        trace.save_to_file(&path).unwrap();
        let loaded = SessionTrace::load_from_file(&path).unwrap();

        assert_eq!(loaded.session_id, Some("persist-test".into()));
        assert_eq!(loaded.record_count(), 2);
        assert_eq!(loaded.outcome, TraceOutcome::Completed);
        assert_eq!(loaded.total_turns, 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_json_accepts_legacy_string_session_id() {
        let json = r#"{
            "session_id": "legacy-session",
            "model": null,
            "started_at_unix_millis": 0,
            "ended_at_unix_millis": null,
            "total_turns": 0,
            "outcome": "InProgress",
            "records": []
        }"#;

        let trace: SessionTrace = serde_json::from_str(json).unwrap();
        assert_eq!(trace.session_id, Some("legacy-session".into()));
    }

    #[test]
    fn load_json_accepts_null_and_missing_session_id() {
        let null_json = r#"{
            "session_id": null,
            "model": null,
            "started_at_unix_millis": 0,
            "ended_at_unix_millis": null,
            "total_turns": 0,
            "outcome": "InProgress",
            "records": []
        }"#;
        let missing_json = r#"{
            "model": null,
            "started_at_unix_millis": 0,
            "ended_at_unix_millis": null,
            "total_turns": 0,
            "outcome": "InProgress",
            "records": []
        }"#;

        let null_trace: SessionTrace = serde_json::from_str(null_json).unwrap();
        let missing_trace: SessionTrace = serde_json::from_str(missing_json).unwrap();
        assert_eq!(null_trace.session_id, None);
        assert_eq!(missing_trace.session_id, None);
    }

    #[test]
    fn save_jsonl_writes_one_record_per_line() {
        let mut trace = SessionTrace::new(Some("jsonl-test".into()));
        trace
            .records
            .push_back(make_record("jsonl-test", 0, HookPoint::SessionStart));
        trace
            .records
            .push_back(make_record("jsonl-test", 1, HookPoint::PostLlmResponse));
        trace
            .records
            .push_back(make_record("jsonl-test", 1, HookPoint::PostTurn));

        let dir = std::env::temp_dir().join("astra-harness-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace_test.jsonl");

        trace.save_jsonl(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        // Each line must be valid JSON
        for line in &lines {
            serde_json::from_str::<DecisionRecord>(line).unwrap();
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_from_nonexistent_file_errors() {
        let result = SessionTrace::load_from_file(std::path::Path::new(
            "/tmp/nonexistent_harness_trace_xyz.json",
        ));
        assert!(result.is_err());
    }

    // ── Fix verification tests ──────────────────────────────────────────

    #[test]
    fn truncate_id_handles_utf8() {
        // CJK characters: each is 3 bytes
        let cjk_id = "会话标识符测试数据很长";
        let result = super::truncate_id(cjk_id);
        assert!(result.ends_with('…'));
        // Should have 8 chars + …
        assert_eq!(result.chars().count(), 9);
        // Must not panic on multi-byte boundaries
    }

    #[test]
    fn truncate_id_short_passes_through() {
        assert_eq!(super::truncate_id("abc"), "abc");
        assert_eq!(super::truncate_id("12345678"), "12345678");
    }

    #[test]
    fn truncate_id_ascii_truncates() {
        let result = super::truncate_id("session-abc-123-long");
        assert_eq!(result, "session-…");
    }

    #[test]
    fn privacy_redacted_also_redacts_model() {
        let mut trace = SessionTrace::new(Some("session-abc-123".into()));
        trace.model = Some("claude-sonnet-4-6".into());
        trace
            .records
            .push_back(make_record("session-abc-123", 1, HookPoint::PostTurn));

        let sanitized = trace.with_privacy(super::PrivacyPolicy::Redacted);
        // Model should be hashed, not the original
        assert!(sanitized.model.is_some());
        assert!(sanitized.model.as_ref().unwrap().starts_with("model_"));
        assert!(
            sanitized.records[0]
                .snapshot
                .model
                .as_ref()
                .unwrap()
                .starts_with("model_")
        );
    }

    #[test]
    fn recording_kernel_caps_trace_size() {
        let (_, sink) = make_recording_kernel();
        let inner = Arc::new(StandardKernel::new(sink as Arc<dyn SnapshotSink>, vec![]));
        let kernel = RecordingKernel::new(inner, Some("cap-test".into())).with_max_records(3);

        for i in 0..10 {
            kernel.on_record(&make_record("cap-test", i, HookPoint::PostTurn));
        }

        let trace = kernel.into_trace();
        assert_eq!(trace.record_count(), 3);
        // Oldest evicted — last 3 records kept
        assert_eq!(trace.records[0].turn, 7);
        assert_eq!(trace.records[2].turn, 9);
    }

    #[test]
    fn into_trace_recovers_from_poison() {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(sink as Arc<dyn SnapshotSink>, vec![]));
        let kernel = RecordingKernel::new(inner, Some("poison-test".into()));
        kernel.on_record(&make_record("poison-test", 0, HookPoint::SessionStart));

        // Poison the lock
        let trace_arc = kernel.trace();
        let _ = std::thread::spawn({
            let t = trace_arc.clone();
            move || {
                let _guard = t.write().unwrap();
                panic!("intentional poison");
            }
        })
        .join();
        assert!(trace_arc.read().is_err(), "lock should be poisoned");

        // into_trace should recover, not panic
        let trace = kernel.into_trace();
        assert_eq!(trace.session_id, Some("poison-test".into()));
        assert_eq!(trace.record_count(), 1);
    }

    // ── Privacy full-chain tests ────────────────────────────────────────

    #[test]
    fn privacy_redacted_full_chain_multi_turn() {
        let mut trace = SessionTrace::new(Some("session-xyz-long-id".into()));
        trace.model = Some("claude-opus-4-6".into());
        for i in 0..5 {
            let mut r = make_record("session-xyz-long-id", i, HookPoint::PostTurn);
            r.snapshot.last_tool_called = Some(format!("tool_{i}"));
            r.snapshot.unique_tools_used = vec!["bash".into(), format!("custom_{i}")];
            trace.records.push_back(r);
        }

        let sanitized = trace.with_privacy(super::PrivacyPolicy::Redacted);

        // Session ID truncated
        assert!(sanitized.session_id.as_deref().unwrap().ends_with('…'));
        assert!(sanitized.session_id.as_deref().unwrap().len() < "session-xyz-long-id".len());

        // Model hashed
        assert!(sanitized.model.as_ref().unwrap().starts_with("model_"));

        for r in &sanitized.records {
            // All session_ids truncated
            assert!(r.session_id.ends_with('…'));
            assert!(r.snapshot.session_id.ends_with('…'));
            // Model hashed
            assert!(r.snapshot.model.as_ref().unwrap().starts_with("model_"));
            // Tool names hashed
            for t in &r.snapshot.unique_tools_used {
                assert!(t.starts_with("tool_"), "unhashed tool: {t}");
            }
            if let Some(ref lt) = r.snapshot.last_tool_called {
                assert!(lt.starts_with("tool_"), "unhashed last_tool: {lt}");
            }
        }
    }

    #[test]
    fn privacy_metadata_only_strips_everything_sensitive() {
        let mut trace = SessionTrace::new(Some("session-xyz-long-id".into()));
        trace.model = Some("claude-opus-4-6".into());
        let mut r = make_record("session-xyz-long-id", 1, HookPoint::PostTurn);
        r.snapshot.last_tool_called = Some("bash".into());
        r.snapshot.unique_tools_used = vec!["bash".into(), "read_file".into()];
        trace.records.push_back(r);

        let sanitized = trace.with_privacy(super::PrivacyPolicy::MetadataOnly);
        assert!(sanitized.model.is_none());
        assert!(sanitized.records[0].snapshot.model.is_none());
        assert!(sanitized.records[0].snapshot.unique_tools_used.is_empty());
        assert!(sanitized.records[0].snapshot.last_tool_called.is_none());
    }

    #[test]
    fn privacy_redacted_handles_none_fields() {
        let mut trace = SessionTrace::new(Some("short".into()));
        let mut r = make_record("short", 1, HookPoint::PostTurn);
        r.snapshot.last_tool_called = None;
        r.snapshot.unique_tools_used = vec![];
        r.snapshot.model = None;
        trace.records.push_back(r);

        let sanitized = trace.with_privacy(super::PrivacyPolicy::Redacted);
        assert!(sanitized.records[0].snapshot.last_tool_called.is_none());
        assert!(sanitized.records[0].snapshot.unique_tools_used.is_empty());
        assert!(sanitized.records[0].snapshot.model.is_none());
    }
}
