//! Session Journal — local JSONL persistence for observability & auditability.
//!
//! Writes one line per event to `~/.astra/sessions/<session_id>.jsonl`.
//! Events include: turn completions, config changes, errors, compactions.
//!
//! The journal is append-only and survives process exits.
//! It can be replayed, exported, or analyzed by `/session` commands.
//!
//! **Test isolation:** use [`JournalDirGuard`] to redirect all `sessions`-rooted I/O on the
//! current thread (journal JSONL, workspace, step checkpoints) without mutating `HOME`.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

thread_local! {
    static LOCAL_SESSIONS_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Resolved local `sessions` directory (`~/.astra/sessions` or a per-thread override).
///
/// Step checkpoints, workspace metadata, and session journal files all live under this root.
pub fn local_sessions_dir() -> PathBuf {
    LOCAL_SESSIONS_DIR_OVERRIDE.with(|c| {
        if let Some(ref p) = *c.borrow() {
            return p.clone();
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".astra")
            .join("sessions")
    })
}

/// Validate that a session ID is safe for use as a filesystem path component.
/// Rejects path traversal attempts (`..`, `/`, `\`) and empty/whitespace-only IDs.
pub fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty() || session_id.trim().is_empty() {
        return Err("session ID cannot be empty".to_string());
    }
    if session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
        || session_id.contains('\0')
    {
        return Err(format!(
            "invalid session ID '{session_id}': must not contain '/', '\\\\', '..', or null bytes"
        ));
    }
    // Must be a single path component (no directory separators after normalization)
    if Path::new(session_id).components().count() != 1 {
        return Err(format!(
            "invalid session ID '{session_id}': must be a single path component"
        ));
    }
    Ok(())
}

/// Redirect session journal + workspace + step checkpoint paths on **this thread** to `dir`.
///
/// `dir` must be the `sessions` folder (the directory that will contain `<session_id>.jsonl`
/// files and `<session_id>/` subdirectories). Nestable: dropping restores the previous override.
#[must_use = "drop restores the previous sessions-dir override for this thread"]
pub struct JournalDirGuard {
    previous: Option<PathBuf>,
}

impl JournalDirGuard {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let previous = LOCAL_SESSIONS_DIR_OVERRIDE.with(|c| (*c.borrow_mut()).replace(dir));
        Self { previous }
    }
}

impl Drop for JournalDirGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        LOCAL_SESSIONS_DIR_OVERRIDE.with(|c| {
            *c.borrow_mut() = prev;
        });
    }
}

/// Session state change tracking for edge-cloud sync.
/// Records mutations as deltas instead of overwriting full state.
pub mod state_delta {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    /// Change operation type for session state mutations.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum StateChangeOp {
        Create,
        Update,
        Delete,
    }

    /// A single state change entry for session mutations.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StateChange {
        /// Monotonic version number within the session.
        pub version: u64,
        /// Timestamp in milliseconds.
        pub timestamp_ms: u64,
        /// The state key being mutated.
        pub key: String,
        /// The operation type.
        pub op: StateChangeOp,
        /// The value (None for Delete).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub value: Option<serde_json::Value>,
        /// Turn number when the change occurred.
        pub turn: u32,
    }

    /// Accumulates session state changes for delta sync.
    pub struct SessionStateAccumulator {
        version_counter: u64,
        entries: Vec<StateChange>,
        current_state: HashMap<String, serde_json::Value>,
    }

    impl Default for SessionStateAccumulator {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SessionStateAccumulator {
        /// Create a new state accumulator starting at version 1.
        pub fn new() -> Self {
            Self {
                version_counter: 1,
                entries: Vec::new(),
                current_state: HashMap::new(),
            }
        }

        fn next_version(&mut self) -> u64 {
            let v = self.version_counter;
            self.version_counter += 1;
            v
        }

        fn now_ms() -> u64 {
            use std::time::SystemTime;
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        }

        /// Record a state creation.
        pub fn create(
            &mut self,
            key: impl Into<String>,
            value: impl Serialize,
            turn: u32,
        ) -> Result<u64, String> {
            let key = key.into();
            if self.current_state.contains_key(&key) {
                return Err(format!("Key '{}' already exists", key));
            }

            let version = self.next_version();
            let json_value = serde_json::to_value(value).map_err(|e| e.to_string())?;

            self.current_state.insert(key.clone(), json_value.clone());
            self.entries.push(StateChange {
                version,
                timestamp_ms: Self::now_ms(),
                key,
                op: StateChangeOp::Create,
                value: Some(json_value),
                turn,
            });

            Ok(version)
        }

        /// Record a state update.
        pub fn update(
            &mut self,
            key: impl Into<String>,
            value: impl Serialize,
            turn: u32,
        ) -> Result<u64, String> {
            let key = key.into();
            if !self.current_state.contains_key(&key) {
                return Err(format!("Key '{}' not found", key));
            }

            let version = self.next_version();
            let json_value = serde_json::to_value(value).map_err(|e| e.to_string())?;

            self.current_state.insert(key.clone(), json_value.clone());
            self.entries.push(StateChange {
                version,
                timestamp_ms: Self::now_ms(),
                key,
                op: StateChangeOp::Update,
                value: Some(json_value),
                turn,
            });

            Ok(version)
        }

        /// Record a state deletion.
        pub fn delete(&mut self, key: impl Into<String>, turn: u32) -> Result<u64, String> {
            let key = key.into();
            if !self.current_state.contains_key(&key) {
                return Err(format!("Key '{}' not found", key));
            }

            let version = self.next_version();
            self.current_state.remove(&key);
            self.entries.push(StateChange {
                version,
                timestamp_ms: Self::now_ms(),
                key,
                op: StateChangeOp::Delete,
                value: None,
                turn,
            });

            Ok(version)
        }

        /// Get the current value for a key.
        pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
            self.current_state.get(key)
        }

        /// Get all changes since a version (exclusive).
        pub fn changes_since(&self, since_version: u64) -> Vec<&StateChange> {
            self.entries
                .iter()
                .filter(|e| e.version > since_version)
                .collect()
        }

        /// Get all changes.
        pub fn all_changes(&self) -> &[StateChange] {
            &self.entries
        }

        /// Get current state snapshot.
        pub fn snapshot(&self) -> &HashMap<String, serde_json::Value> {
            &self.current_state
        }

        /// Get the latest version number.
        pub fn latest_version(&self) -> u64 {
            self.version_counter.saturating_sub(1)
        }

        /// Get the number of change entries.
        pub fn change_count(&self) -> usize {
            self.entries.len()
        }

        /// Clear all change entries (after sync).
        pub fn clear_changes(&mut self) {
            self.entries.clear();
        }

        /// Compact by keeping only latest change per key.
        pub fn compact(&mut self) {
            let mut latest: HashMap<String, StateChange> = HashMap::new();

            for entry in &self.entries {
                if entry.op == StateChangeOp::Delete {
                    latest.remove(&entry.key);
                } else {
                    latest.insert(entry.key.clone(), entry.clone());
                }
            }

            let mut new_entries: Vec<StateChange> = latest.into_values().collect();
            new_entries.sort_by_key(|e| e.version);
            self.entries = new_entries;
        }

        /// Calculate memory overhead of changes vs full state.
        pub fn overhead_percentage(&self) -> f64 {
            let changes_bytes: usize = self
                .entries
                .iter()
                .map(|e| {
                    let base = e.key.len() + std::mem::size_of::<StateChange>();
                    let val_bytes = e
                        .value
                        .as_ref()
                        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                        .unwrap_or(0);
                    base + val_bytes
                })
                .sum();

            let state_bytes: usize = self
                .current_state
                .iter()
                .map(|(k, v)| k.len() + serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                .sum();

            if state_bytes == 0 {
                0.0
            } else {
                (changes_bytes as f64 / state_bytes as f64) * 100.0
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_create_and_get() {
            let mut acc = SessionStateAccumulator::new();
            let v = acc.create("key1", "value1", 1).unwrap();

            assert_eq!(v, 1);
            assert_eq!(acc.get("key1"), Some(&serde_json::json!("value1")));
            assert_eq!(acc.change_count(), 1);
        }

        #[test]
        fn test_update_existing() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("key1", "value1", 1).unwrap();
            let v = acc.update("key1", "value2", 2).unwrap();

            assert_eq!(v, 2);
            assert_eq!(acc.get("key1"), Some(&serde_json::json!("value2")));
            assert_eq!(acc.change_count(), 2);
        }

        #[test]
        fn test_delete_existing() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("key1", "value1", 1).unwrap();
            let v = acc.delete("key1", 2).unwrap();

            assert_eq!(v, 2);
            assert_eq!(acc.get("key1"), None);
            assert_eq!(acc.change_count(), 2);
        }

        #[test]
        fn test_changes_since_version() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("a", 1, 1).unwrap();
            acc.create("b", 2, 1).unwrap();
            acc.update("a", 3, 2).unwrap();

            let changes = acc.changes_since(1);
            assert_eq!(changes.len(), 2); // b, a-update
        }

        #[test]
        fn test_compact_reduces_entries() {
            let mut acc = SessionStateAccumulator::new();
            acc.create("key", "v1", 1).unwrap();
            acc.update("key", "v2", 1).unwrap();
            acc.update("key", "v3", 1).unwrap();

            assert_eq!(acc.change_count(), 3);
            acc.compact();
            assert_eq!(acc.change_count(), 1);
            assert_eq!(acc.get("key"), Some(&serde_json::json!("v3")));
        }

        #[test]
        fn test_overhead_with_updates() {
            let mut acc = SessionStateAccumulator::new();

            // Create many entries
            for i in 0..100 {
                acc.create(format!("key{}", i), "x".repeat(100), 1).unwrap();
            }

            // Update many times
            for _ in 0..5 {
                for i in 0..50 {
                    acc.update(format!("key{}", i), "y".repeat(100), 1).unwrap();
                }
            }

            let overhead = acc.overhead_percentage();
            // After many updates, overhead will be high until compaction
            // This verifies the measurement works
            assert!(overhead > 0.0, "Should have overhead after updates");

            // After compaction, overhead should be reduced
            acc.compact();
            let after = acc.overhead_percentage();
            assert!(after < overhead, "Compaction should reduce overhead");
        }
    }
}

/// Parent session linkage when forking or branching a session (edge-local audit + cloud sync).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLineage {
    pub parent_session_id: String,
    /// Last turn number included from the parent at fork time (for replay boundaries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_after_turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Correlates this session or event with multi-agent / handoff workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    /// Shared id across forked sessions, sub-agents, or cloud-orchestrated steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Edge permission / cloud policy fingerprint at a point in time (for cloud–edge audit alignment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgePolicySnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_policy_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_fingerprint: Option<String>,
}

/// Tool selection decision trace for post-hoc analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionTrace {
    /// Candidate tools and their TF-IDF/LLM scores (top 10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_scores: Option<Vec<(String, f64)>>,
    /// Boost terms applied from entity graph / pattern library.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boost_terms: Option<Vec<String>>,
    /// Learned context summary injected into selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learned_context_summary: Option<String>,
    /// Final selected tools.
    pub final_tools: Vec<String>,
    /// Selection confidence score.
    pub confidence: f64,
    /// Strategy used (tfidf, llm, fallback, etc.).
    pub strategy: String,
}

/// Per-tool-call audit record, embedded in turn events for granular tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Tool name.
    pub name: String,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Execution time in milliseconds.
    pub ms: u64,
    /// Error message if the call failed (first 500 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Input size in bytes (arguments/parameters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bytes: Option<u32>,
    /// Output size in bytes (result/response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u32>,
    /// Preview of tool arguments (truncated to ~80 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_preview: Option<String>,
    /// Preview of tool result (truncated to 500 chars) for cloud audit trail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

/// A single journal event (one line in the JSONL file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEvent {
    /// Event type discriminator.
    #[serde(rename = "type")]
    pub event_type: JournalEventType,
    /// ISO 8601 timestamp.
    pub ts: String,
    /// Session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Turn number (1-based, for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// LLM model used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// User input text (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input: Option<String>,
    /// Assistant response text (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_output: Option<String>,
    /// Number of tool calls in this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<u32>,
    /// Prompt tokens used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    /// Completion tokens used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// Turn duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Error message (for error events or failed turns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Config key (for config_change events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_key: Option<String>,
    /// Config value (for config_change events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_value: Option<String>,
    /// Number of turns compacted (for compact events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns_compacted: Option<usize>,
    /// Number of facts stored (for compact events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts_stored: Option<usize>,
    /// Tool names selected for the LLM request (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_selected: Option<Vec<String>>,
    /// Skill names selected for the LLM request (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_skills: Option<Vec<String>>,
    /// Tool names actually called by the LLM (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_used: Option<Vec<String>>,
    /// Per-tool-call detail: [{name, ok, ms, error?}] for granular audit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    /// Token budget used by selected dynamic tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_used: Option<u32>,
    /// Token budget pressure (0.0 = normal, 0.3 = trim, 0.6 = compact, 0.9 = aggressive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_pressure: Option<f64>,
    /// Stall type (for stall_detected events): "sig_stall", "name_stall", "divergence".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_type: Option<String>,
    /// Flexible metadata for event-specific data (JSON object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Plan subtask ID — set when this turn was executed as part of plan mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_subtask_id: Option<String>,
    /// Time to first token in milliseconds (streaming latency).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Context assembly time in milliseconds (prompt building).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_ms: Option<u64>,
    /// Tool selection strategy used (e.g. "tfidf", "llm", "tfidf_fast").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_strategy: Option<String>,
    /// Tool selection time in milliseconds (subset of context_ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_ms: Option<u64>,
    /// LLM tokens consumed by tool selector (0 = TF-IDF only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_tokens_out: Option<u64>,
    /// Cache read tokens (prompt cache hits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Cache creation tokens (prompt cache writes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Memoria search time in milliseconds (subset of context_ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memoria_ms: Option<u64>,
    /// Fork / branch lineage (also set on `session_fork` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_lineage: Option<SessionLineage>,
    /// Multi-agent or handoff correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination: Option<CoordinationMeta>,
    /// Edge policy snapshot for cloud–edge audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_policy: Option<EdgePolicySnapshot>,
    /// Tool selection decision trace for post-hoc analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_trace: Option<SelectionTrace>,
    /// Selector confidence from the first tool-selection pass (0.0–1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector_confidence: Option<f64>,
    /// Routing domain hint label for this REPL turn (e.g. `github`); omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_domain_hint: Option<String>,
    /// True when the turn succeeded with tool calls but routing had no domain — entity graph learn was skipped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub entity_learn_skipped_no_domain: bool,
}

/// Event type discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JournalEventType {
    /// Session started.
    SessionStart,
    /// A conversation turn completed.
    Turn,
    /// A turn failed with an error.
    TurnError,
    /// Manual or auto compact.
    Compact,
    /// Configuration changed (model, explain, skill toggle).
    ConfigChange,
    /// An error occurred (non-turn).
    Error,
    /// Session ended.
    SessionEnd,
    /// Stall or divergence detected (non-happy path).
    StallDetected,
    /// Session checkpoint saved.
    Checkpoint,
    /// TurnGuard verdict emitted (unified non-happy-path audit).
    TurnGuardVerdict,
    /// Plan execution progress (subtask started, completed, plan done).
    PlanProgress,
    /// Forked from another session — records lineage for audit and sync.
    SessionFork,
    /// Cloud–edge policy ack, agent handoff, or other sync metadata (lightweight).
    SyncMarker,
    /// Delegation group started (sub-run group spawned).
    DelegationStarted,
    /// A single sub-run within a delegation completed.
    DelegationSubRunCompleted,
    /// Delegation completed (all sub-runs done, results aggregated).
    DelegationCompleted,
    /// Subtask or plan verification completed (acceptance-criteria gate result).
    VerificationCompleted,
    /// A composite snapshot was taken — captures references to session state,
    /// data snapshot, memory snapshot, git commit, etc.
    CompositeSnapshot,
}

/// Writer that appends events to a session journal file.
pub struct JournalWriter {
    path: PathBuf,
}

impl JournalWriter {
    /// Create a writer for the given session ID.
    /// Creates the parent directory if needed.
    pub fn new(session_id: &str) -> std::io::Result<Self> {
        let dir = journal_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        Ok(Self { path })
    }

    /// Append a single event to the journal file.
    pub fn append(&self, event: &JournalEvent) -> std::io::Result<()> {
        use std::io::Write;
        let line = serde_json::to_string(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // Restrict file permissions to owner-only (0o600) to protect sensitive
        // conversation history from other users on shared systems.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        if let Err(e) = writeln!(file, "{line}") {
            if e.kind() == std::io::ErrorKind::Other
                || e.raw_os_error() == Some(28) // ENOSPC
                || e.to_string().contains("No space")
            {
                astra_core::agent_error!("journal", "disk full, journal event lost");
            }
            return Err(e);
        }
        Ok(())
    }

    /// Get the path to this journal file.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

fn parse_journal_text(content: &str) -> (Vec<JournalEvent>, usize, usize) {
    let mut events = Vec::new();
    let mut non_empty_lines = 0usize;
    let mut malformed_lines = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        non_empty_lines += 1;
        match serde_json::from_str::<JournalEvent>(line) {
            Ok(evt) => events.push(evt),
            Err(_) => malformed_lines += 1,
        }
    }
    (events, non_empty_lines, malformed_lines)
}

/// Read all events from a session journal file.
pub fn read_journal(session_id: &str) -> std::io::Result<Vec<JournalEvent>> {
    let path = journal_dir().join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(parse_journal_text(&content).0)
}

/// Read journal for offline analysis tools. Returns an error if the JSONL file is missing.
///
/// Second element: non-empty physical lines; third: lines that failed JSON parse.
pub fn read_journal_for_digest(
    session_id: &str,
) -> std::io::Result<(Vec<JournalEvent>, usize, usize)> {
    let path = journal_dir().join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session journal not found: {}", path.display()),
        ));
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(parse_journal_text(&content))
}

/// List all session IDs that have journal files.
pub fn list_sessions() -> std::io::Result<Vec<String>> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sid) = name.strip_suffix(".jsonl") {
            sessions.push(sid.to_string());
        }
    }
    sessions.sort();
    Ok(sessions)
}

/// Path to the JSONL journal file for a session.
///
/// # Panics
/// Panics if `session_id` contains path traversal characters. Use [`validate_session_id`]
/// to pre-validate untrusted input.
#[must_use]
pub fn journal_file_path(session_id: &str) -> PathBuf {
    assert!(
        validate_session_id(session_id).is_ok(),
        "unsafe session ID passed to journal_file_path: {session_id}"
    );
    journal_dir().join(format!("{session_id}.jsonl"))
}

/// List local session IDs sorted by file modification time (most recent first).
/// Only returns the `limit` most recent sessions to avoid scanning all files.
pub fn list_sessions_by_time(limit: usize) -> std::io::Result<Vec<String>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Min-heap of (mtime, sid) — keeps only the `limit` newest entries
    let mut heap: BinaryHeap<Reverse<(std::time::SystemTime, String)>> = BinaryHeap::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sid) = name.strip_suffix(".jsonl") {
            // Skip test-generated sessions
            if sid.starts_with("test-") || sid.starts_with("new-sess-") {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if heap.len() < limit {
                heap.push(Reverse((mtime, sid.to_string())));
            } else if let Some(&Reverse((min_time, _))) = heap.peek()
                && mtime > min_time
            {
                heap.pop();
                heap.push(Reverse((mtime, sid.to_string())));
            }
        }
    }
    let mut items: Vec<_> = heap.into_iter().map(|Reverse(item)| item).collect();
    items.sort_by(|a, b| b.0.cmp(&a.0)); // newest first by mtime
    Ok(items.into_iter().map(|(_, sid)| sid).collect())
}

/// Count turn events in a journal without fully parsing all events.
pub fn count_turns(session_id: &str) -> u32 {
    use std::io::BufRead;
    let path = journal_dir().join(format!("{session_id}.jsonl"));
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(|l| l.ok())
        .filter(|l| l.contains("\"type\":\"turn\""))
        .count() as u32
}

/// Quick metadata peek from a journal file — reads only the first few lines.
///
/// Returns `(first_user_input, model, timestamp)` without parsing the entire JSONL.
/// Designed for fast session listing (like claudecode's head/tail extraction).
pub fn peek_session_meta(session_id: &str) -> Option<SessionPeek> {
    use std::io::BufRead;
    let path = journal_dir().join(format!("{session_id}.jsonl"));
    let file = std::fs::File::open(&path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut model: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut created_at: Option<String> = None;

    // Read at most 20 lines — enough to find session_start + first turn
    for line in reader.lines().take(20).map_while(|l| l.ok()) {
        if created_at.is_none() {
            // Extract timestamp from first line (any event type)
            if let Some(ts) = extract_json_str(&line, "\"ts\":\"") {
                created_at = Some(ts);
            }
        }
        if model.is_none() && line.contains("\"type\":\"session_start\"") {
            model = extract_json_str(&line, "\"model\":\"");
        }
        if first_prompt.is_none() && line.contains("\"type\":\"turn\"") {
            first_prompt = extract_json_str(&line, "\"user_input\":\"");
        }
        if model.is_some() && first_prompt.is_some() {
            break;
        }
    }

    Some(SessionPeek {
        first_prompt,
        model,
        created_at,
    })
}

/// Lightweight session metadata from journal head.
#[derive(Debug, Clone, Default)]
pub struct SessionPeek {
    /// First user message (truncated, from first Turn event).
    pub first_prompt: Option<String>,
    /// Model from SessionStart event.
    pub model: Option<String>,
    /// Timestamp of first event.
    pub created_at: Option<String>,
}

/// Fast JSON string field extraction without full parse.
/// Looks for `"key":"value"` and returns the value (handles simple escapes).
fn extract_json_str(line: &str, needle: &str) -> Option<String> {
    let start = line.find(needle)? + needle.len();
    let rest = &line[start..];
    // Find closing quote, handling escaped quotes
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\') {
            break;
        }
        end += 1;
    }
    if end == 0 || end >= bytes.len() {
        return None;
    }
    Some(rest[..end].replace("\\\"", "\"").replace("\\n", " "))
}

// ── Session cleanup / lifecycle ──────────────────────────────────────────────

/// Metadata about a session that's a candidate for cleanup.
#[derive(Debug, Clone)]
pub struct StaleSessionInfo {
    pub session_id: String,
    /// File modification time of the journal.
    pub last_modified: std::time::SystemTime,
    /// Journal file size in bytes.
    pub journal_bytes: u64,
    /// Turn count (fast count, not full parse).
    pub turns: u32,
    /// Total disk usage: journal + workspace dir (recursive).
    pub total_bytes: u64,
}

/// Find sessions whose journal file hasn't been modified in `max_age`.
///
/// `exclude_id` — the currently active session (never returned).
pub fn find_stale_sessions(
    max_age: std::time::Duration,
    exclude_id: Option<&str>,
) -> std::io::Result<Vec<StaleSessionInfo>> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let mut stale = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(sid) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if sid.starts_with("test-") || sid.starts_with("new-sess-") {
            continue;
        }
        if exclude_id == Some(sid) {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if mtime >= cutoff {
            continue; // still fresh
        }
        let journal_bytes = meta.len();
        let turns = count_turns(sid);
        let ws_dir = crate::session_workspace::workspace_dir_for(sid);
        let ws_bytes = dir_size_recursive(&ws_dir);
        stale.push(StaleSessionInfo {
            session_id: sid.to_string(),
            last_modified: mtime,
            journal_bytes,
            turns,
            total_bytes: journal_bytes + ws_bytes,
        });
    }
    // Sort oldest first
    stale.sort_by_key(|s| s.last_modified);
    Ok(stale)
}

/// Delete a session's journal file and workspace directory.
///
/// Returns `Ok(bytes_freed)` on success.
pub fn delete_session(session_id: &str) -> std::io::Result<u64> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let journal = journal_file_path(session_id);
    let ws_dir = crate::session_workspace::workspace_dir_for(session_id);
    let mut freed = 0u64;
    if journal.exists() {
        freed += std::fs::metadata(&journal).map(|m| m.len()).unwrap_or(0);
        std::fs::remove_file(&journal)?;
    }
    if ws_dir.exists() {
        freed += dir_size_recursive(&ws_dir);
        std::fs::remove_dir_all(&ws_dir)?;
    }
    Ok(freed)
}

/// Recursively compute total size of a directory (best-effort, ignores errors).
fn dir_size_recursive(path: &std::path::Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    walkdir(path)
}

fn walkdir(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += walkdir(&entry.path());
            }
        }
    }
    total
}

/// Compress a session's `.jsonl` journal to `.jsonl.gz` and remove the original.
///
/// Returns `Ok((original_bytes, compressed_bytes))` on success.
/// Only archives if the session has a `session_end` event (i.e., completed).
pub fn archive_journal(session_id: &str) -> std::io::Result<(u64, u64)> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let src = journal_file_path(session_id);
    if !src.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("journal file not found for {session_id}"),
        ));
    }
    // Check the journal has a session_end (don't archive active sessions)
    let content = std::fs::read(&src)?;
    if !content
        .windows(b"\"session_end\"".len())
        .any(|w| w == b"\"session_end\"")
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session has no session_end event — still active?",
        ));
    }
    let original_bytes = content.len() as u64;
    let dst = journal_dir().join(format!("{session_id}.jsonl.gz"));
    let out_file = std::fs::File::create(&dst)?;
    let mut encoder = GzEncoder::new(out_file, Compression::default());
    encoder.write_all(&content)?;
    encoder.finish()?;
    let compressed_bytes = std::fs::metadata(&dst)?.len();
    std::fs::remove_file(&src)?;
    Ok((original_bytes, compressed_bytes))
}

/// Find completed sessions eligible for archival (have session_end, not yet compressed).
///
/// `exclude_id` — the currently active session.
pub fn find_archivable_sessions(exclude_id: Option<&str>) -> std::io::Result<Vec<(String, u64)>> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(sid) = name.strip_suffix(".jsonl") else {
            continue;
        };
        // Skip already-compressed (.jsonl.gz would not match .jsonl suffix)
        if sid.ends_with(".jsonl") {
            continue; // double extension guard
        }
        if sid.starts_with("test-") || sid.starts_with("new-sess-") {
            continue;
        }
        if exclude_id == Some(sid) {
            continue;
        }
        let meta = entry.metadata()?;
        let bytes = meta.len();
        // Quick check: has session_end?
        let path = entry.path();
        let has_end = std::fs::read_to_string(&path)
            .map(|c| c.contains("\"session_end\""))
            .unwrap_or(false);
        if has_end {
            result.push((sid.to_string(), bytes));
        }
    }
    result.sort_by(|a, b| b.1.cmp(&a.1)); // largest first
    Ok(result)
}

/// Resolve a session id to an exact journal filename stem.
///
/// Accepts:
/// - a full session id
/// - a unique prefix of a session id
/// - an id with a trailing `.jsonl`
pub fn resolve_session_id(query: &str) -> std::io::Result<String> {
    let query = query.trim();
    let query = query.strip_suffix(".jsonl").unwrap_or(query);
    if query.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id cannot be empty",
        ));
    }
    let sessions = list_sessions()?;
    resolve_session_id_from_list(query, &sessions)
}

/// Helper: get the journal directory path (same as [`local_sessions_dir()`]).
fn journal_dir() -> PathBuf {
    local_sessions_dir()
}

fn resolve_session_id_from_list(query: &str, sessions: &[String]) -> std::io::Result<String> {
    if let Some(exact) = sessions.iter().find(|sid| sid.as_str() == query) {
        return Ok(exact.clone());
    }

    let matches: Vec<String> = sessions
        .iter()
        .filter(|sid| sid.starts_with(query))
        .cloned()
        .collect();

    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session journal matches '{query}'"),
        )),
        _ => {
            let preview = matches
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let extra = if matches.len() > 5 {
                format!(" (+{} more)", matches.len() - 5)
            } else {
                String::new()
            };
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session id prefix '{query}' is ambiguous: {preview}{extra}"),
            ))
        }
    }
}

// ── Builder helpers for common events ───────────────────────────────

impl JournalEvent {
    fn base(event_type: JournalEventType, session_id: Option<&str>) -> Self {
        Self {
            event_type,
            ts: chrono::Utc::now().to_rfc3339(),
            session_id: session_id.map(|s| s.to_string()),
            turn: None,
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
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
        }
    }

    /// Session start event.
    pub fn session_start(session_id: Option<&str>, model: Option<&str>) -> Self {
        let mut evt = Self::base(JournalEventType::SessionStart, session_id);
        evt.model = model.map(|s| s.to_string());
        evt
    }

    /// Record that this session was forked from `lineage.parent_session_id`.
    pub fn session_fork(
        session_id: Option<&str>,
        lineage: SessionLineage,
        label_note: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::SessionFork, session_id);
        evt.session_lineage = Some(lineage);
        if let Some(n) = label_note.filter(|s| !s.is_empty()) {
            evt.user_input = Some(truncate(n, 200));
        }
        evt
    }

    /// Cloud–edge sync or multi-agent coordination marker (policy version, correlation id, etc.).
    pub fn sync_marker(
        session_id: Option<&str>,
        policy: Option<EdgePolicySnapshot>,
        coordination: Option<CoordinationMeta>,
        note: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::SyncMarker, session_id);
        evt.edge_policy = policy;
        evt.coordination = coordination;
        if let Some(n) = note.filter(|s| !s.is_empty()) {
            evt.user_input = Some(truncate(n, 200));
        }
        evt
    }

    /// After a successful MatrixOne pull of learning / preferences (startup or post-login audit).
    ///
    /// Structured fields live under `metadata.cloud_pull` for analytics; `user_input` holds a short
    /// human-readable summary for export and grep.
    #[allow(clippy::too_many_arguments)]
    pub fn cloud_pull_sync_marker(
        session_id: Option<&str>,
        profile: &str,
        source: &str,
        learning_version: Option<i64>,
        learning_snapshot_merged: bool,
        tool_health_rows_from_cloud: usize,
        preference_keys_merged: &[String],
        reachable_empty_ack: bool,
    ) -> Self {
        let note = format!(
            "cloud_pull {source} profile={profile} learning_v={:?} prefs={}{}",
            learning_version,
            preference_keys_merged.len(),
            if reachable_empty_ack {
                " empty_ack"
            } else {
                ""
            }
        );
        let mut evt = Self::sync_marker(session_id, None, None, Some(note.as_str()));
        evt.metadata = Some(serde_json::json!({
            "cloud_pull": {
                "profile": profile,
                "source": source,
                "learning_version": learning_version,
                "learning_snapshot_merged": learning_snapshot_merged,
                "tool_health_rows_from_cloud": tool_health_rows_from_cloud,
                "preference_keys_merged": preference_keys_merged,
                "reachable_empty_ack": reachable_empty_ack,
            }
        }));
        evt
    }

    /// Turn completion event.
    #[allow(clippy::too_many_arguments)]
    pub fn turn(
        session_id: Option<&str>,
        turn: u32,
        model: Option<&str>,
        user_input: &str,
        assistant_output: &str,
        tool_count: u32,
        tokens_in: u64,
        tokens_out: u64,
        duration_ms: u64,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::Turn, session_id);
        evt.turn = Some(turn);
        evt.model = model.map(|s| s.to_string());
        evt.user_input = Some(truncate(user_input, 500));
        evt.assistant_output = Some(truncate(assistant_output, 10000));
        evt.tool_count = Some(tool_count);
        evt.tokens_in = Some(tokens_in);
        evt.tokens_out = Some(tokens_out);
        evt.duration_ms = Some(duration_ms);
        evt
    }

    /// Add cache token counts to a turn event (builder pattern).
    pub fn with_cache_tokens(mut self, cache_read: u64, cache_creation: u64) -> Self {
        if cache_read > 0 {
            self.cache_read_tokens = Some(cache_read);
        }
        if cache_creation > 0 {
            self.cache_creation_tokens = Some(cache_creation);
        }
        self
    }

    /// Turn error event.
    pub fn turn_error(
        session_id: Option<&str>,
        turn: u32,
        model: Option<&str>,
        user_input: &str,
        error: &str,
        duration_ms: u64,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::TurnError, session_id);
        evt.turn = Some(turn);
        evt.model = model.map(|s| s.to_string());
        evt.user_input = Some(truncate(user_input, 500));
        evt.error = Some(truncate(error, 500));
        evt.duration_ms = Some(duration_ms);
        evt
    }

    /// Compact event.
    pub fn compact(
        session_id: Option<&str>,
        turn: u32,
        turns_compacted: usize,
        facts_stored: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::Compact, session_id);
        evt.turn = Some(turn);
        evt.turns_compacted = Some(turns_compacted);
        evt.facts_stored = Some(facts_stored);
        evt
    }

    /// Config change event.
    pub fn config_change(session_id: Option<&str>, key: &str, value: &str) -> Self {
        let mut evt = Self::base(JournalEventType::ConfigChange, session_id);
        evt.config_key = Some(key.to_string());
        evt.config_value = Some(value.to_string());
        evt
    }

    /// Error event (non-turn).
    pub fn error(session_id: Option<&str>, error: &str) -> Self {
        let mut evt = Self::base(JournalEventType::Error, session_id);
        evt.error = Some(truncate(error, 500));
        evt
    }

    /// Session end event.
    pub fn session_end(session_id: Option<&str>, total_turns: u32) -> Self {
        let mut evt = Self::base(JournalEventType::SessionEnd, session_id);
        evt.turn = Some(total_turns);
        evt
    }

    /// Attach tool selection data to a turn event.
    pub fn with_tool_selection(
        mut self,
        tools_selected: Vec<String>,
        selected_skills: Vec<String>,
        tools_used: Vec<String>,
        budget_used: u32,
    ) -> Self {
        self.tools_selected = Some(tools_selected);
        if !selected_skills.is_empty() {
            self.selected_skills = Some(selected_skills);
        }
        self.tools_used = Some(tools_used);
        self.budget_used = Some(budget_used);
        self
    }

    /// Attach budget pressure to a turn event (0.0-0.9 from compaction tier).
    pub fn with_budget_pressure(mut self, pressure: f64) -> Self {
        self.budget_pressure = Some(pressure);
        self
    }

    /// Attach per-tool-call audit records to a turn event.
    pub fn with_tool_calls(mut self, records: Vec<ToolCallRecord>) -> Self {
        if !records.is_empty() {
            self.tool_calls = Some(records);
        }
        self
    }

    /// Tag this turn event as belonging to a plan mode subtask.
    pub fn with_plan_subtask(mut self, subtask_id: Option<&str>) -> Self {
        self.plan_subtask_id = subtask_id.map(|s| s.to_string());
        self
    }

    /// Set time to first token (streaming latency).
    pub fn with_ttft(mut self, ttft_ms: Option<u64>) -> Self {
        self.ttft_ms = ttft_ms;
        self
    }

    /// Set context assembly time (prompt building).
    pub fn with_context_time(mut self, context_ms: Option<u64>) -> Self {
        self.context_ms = context_ms;
        self
    }

    /// Set memoria search time.
    pub fn with_memoria_time(mut self, memoria_ms: Option<u64>) -> Self {
        self.memoria_ms = memoria_ms;
        self
    }

    /// Set tool selection time.
    pub fn with_selector_time(mut self, selector_ms: Option<u64>) -> Self {
        self.selector_ms = selector_ms;
        self
    }

    /// Set tool selector LLM token usage.
    pub fn with_selector_tokens(mut self, tokens_in: u64, tokens_out: u64) -> Self {
        if tokens_in > 0 || tokens_out > 0 {
            self.selector_tokens_in = Some(tokens_in);
            self.selector_tokens_out = Some(tokens_out);
        }
        self
    }

    /// Set tool selection strategy.
    pub fn with_selector_strategy(mut self, strategy: Option<String>) -> Self {
        self.selector_strategy = strategy;
        self
    }

    /// Learning / routing telemetry for this REPL turn (journal + analytics).
    pub fn with_selector_learning_telemetry(
        mut self,
        selector_confidence: Option<f64>,
        routing_domain_hint: Option<String>,
        entity_learn_skipped_no_domain: bool,
    ) -> Self {
        self.selector_confidence = selector_confidence;
        self.routing_domain_hint = routing_domain_hint;
        self.entity_learn_skipped_no_domain = entity_learn_skipped_no_domain;
        self
    }

    /// Attach selection trace for post-hoc tool selection analysis.
    pub fn with_selection_trace(mut self, trace: SelectionTrace) -> Self {
        self.selection_trace = Some(trace);
        self
    }

    /// Stall detection event.
    pub fn stall_detected(
        session_id: Option<&str>,
        turn: u32,
        stall_type: &str,
        nudge_count: u32,
        confidence: f64,
        avoid_tools: &[String],
    ) -> Self {
        let mut evt = Self::base(JournalEventType::StallDetected, session_id);
        evt.turn = Some(turn);
        evt.stall_type = Some(stall_type.to_string());
        evt.metadata = Some(serde_json::json!({
            "nudge_count": nudge_count,
            "confidence": confidence,
            "avoid_tools": avoid_tools,
        }));
        evt
    }

    /// Session checkpoint event.
    pub fn checkpoint(
        session_id: Option<&str>,
        turn: u32,
        summary: &str,
        total_tokens: u64,
        tools_used_count: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::Checkpoint, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "summary": truncate(summary, 500),
            "total_tokens": total_tokens,
            "tools_used_count": tools_used_count,
        }));
        evt
    }

    /// TurnGuard verdict event — records unified non-happy-path decisions.
    ///
    /// Only emitted for non-Healthy verdicts (Info, Warning, Critical).
    /// Captures severity, injected messages, avoided tools, and force_stop.
    #[allow(clippy::too_many_arguments)]
    pub fn turn_guard_verdict(
        session_id: Option<&str>,
        turn: u32,
        severity: &str,
        injections: &[String],
        avoid_tools: &[String],
        force_stop: bool,
        nudge_count: usize,
        total_errors: usize,
        deprioritized_count: usize,
        total_timeouts: usize,
        total_cache_hits: usize,
        flaky_count: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::TurnGuardVerdict, session_id);
        evt.turn = Some(turn);
        evt.stall_type = Some(severity.to_string());
        evt.metadata = Some(serde_json::json!({
            "severity": severity,
            "injections": injections.len(),
            "injection_preview": injections.first().map(|s| truncate(s, 200)),
            "avoid_tools": avoid_tools,
            "force_stop": force_stop,
            "nudge_count": nudge_count,
            "total_errors": total_errors,
            "deprioritized_tools": deprioritized_count,
            "total_timeouts": total_timeouts,
            "total_cache_hits": total_cache_hits,
            "flaky_tools": flaky_count,
        }));
        evt
    }

    /// Build a plan progress event — emitted when a subtask starts, completes, or plan finishes.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_progress(
        session_id: Option<&str>,
        turn: u32,
        subtask_id: &str,
        subtask_title: &str,
        action: &str, // "started" | "completed" | "skipped" | "plan_complete" | "plan_paused"
        progress_pct: u32,
        total_subtasks: usize,
        completed_subtasks: usize,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PlanProgress, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "subtask_id": subtask_id,
            "subtask_title": subtask_title,
            "action": action,
            "progress_pct": progress_pct,
            "total_subtasks": total_subtasks,
            "completed_subtasks": completed_subtasks,
        }));
        evt
    }

    /// Verification completed — emitted after subtask or global verification.
    pub fn verification_completed(
        session_id: Option<&str>,
        turn: u32,
        subtask_id: &str,
        scope: &str, // "subtask" | "global"
        passed: bool,
        results: &serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::VerificationCompleted, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "subtask_id": subtask_id,
            "scope": scope,
            "passed": passed,
            "results": results,
        }));
        evt
    }

    /// Composite snapshot taken — records references to state dimensions.
    pub fn composite_snapshot(
        session_id: Option<&str>,
        turn: u32,
        snapshot_id: &str,
        label: Option<&str>,
        components: &[&str],
    ) -> Self {
        let mut evt = Self::base(JournalEventType::CompositeSnapshot, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "snapshot_id": snapshot_id,
            "label": label,
            "components": components,
        }));
        evt
    }

    /// Delegation started event — emitted when a delegation group is spawned.
    pub fn delegation_started(
        session_id: Option<&str>,
        delegation_id: &str,
        parent_run_id: &str,
        pattern: &str,
        agent_ids: &[String],
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationStarted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "parent_run_id": parent_run_id,
            "pattern": pattern,
            "agent_ids": agent_ids,
            "agent_count": agent_ids.len(),
        }));
        evt
    }

    /// Delegation sub-run completed event — emitted when a single sub-run finishes.
    pub fn delegation_sub_run_completed(
        session_id: Option<&str>,
        delegation_id: &str,
        sub_run_id: &str,
        agent_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationSubRunCompleted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "sub_run_id": sub_run_id,
            "agent_id": agent_id,
            "status": status,
            "error": error,
        }));
        evt
    }

    /// Delegation completed event — emitted when all sub-runs finish and results aggregate.
    pub fn delegation_completed(
        session_id: Option<&str>,
        delegation_id: &str,
        pattern: &str,
        total_sub_runs: usize,
        succeeded: usize,
        failed: usize,
        aggregated_status: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationCompleted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "pattern": pattern,
            "total_sub_runs": total_sub_runs,
            "succeeded": succeeded,
            "failed": failed,
            "aggregated_status": aggregated_status,
        }));
        evt
    }
}

/// Truncate a string to max chars (for journal size control).
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

// ═══════════════════════════ Session Lifecycle ════════════════════════════

/// Result of a single session lifecycle maintenance run.
#[derive(Debug, Default)]
pub struct SessionMaintenanceResult {
    /// Number of sessions deleted (TTL expired).
    pub sessions_deleted: usize,
    /// Number of journals compressed (.jsonl → .jsonl.gz).
    pub journals_compressed: usize,
    /// Total disk bytes freed by deletion.
    pub bytes_freed: u64,
    /// Errors encountered (non-fatal, best-effort).
    pub errors: Vec<String>,
}

/// Run session lifecycle maintenance: delete expired sessions and compress old journals.
///
/// - `ttl_days`: sessions older than this are deleted entirely (default: 30).
/// - `compress_after_days`: journals older than this (but younger than ttl) are gzip-compressed (default: 7).
///
/// Both thresholds use the journal file's modification time. This function is
/// idempotent and safe to call at every REPL startup.
pub fn run_session_maintenance(
    ttl_days: u64,
    compress_after_days: u64,
) -> SessionMaintenanceResult {
    let dir = journal_dir();
    if !dir.exists() {
        return SessionMaintenanceResult::default();
    }
    run_session_maintenance_in(dir, ttl_days, compress_after_days)
}

/// Testable version that operates on an explicit directory.
fn run_session_maintenance_in(
    dir: PathBuf,
    ttl_days: u64,
    compress_after_days: u64,
) -> SessionMaintenanceResult {
    use std::time::{Duration, SystemTime};

    let now = SystemTime::now();
    let ttl_threshold = now
        .checked_sub(Duration::from_secs(ttl_days * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let compress_threshold = now
        .checked_sub(Duration::from_secs(compress_after_days * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut result = SessionMaintenanceResult::default();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            result.errors.push(format!("read_dir failed: {e}"));
            return result;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only process .jsonl files (active journals)
        let session_id = match name_str.strip_suffix(".jsonl") {
            Some(sid) => sid.to_string(),
            None => continue,
        };
        // Skip .jsonl.gz — already compressed
        if name_str.ends_with(".jsonl.gz") {
            continue;
        }

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if mtime < ttl_threshold {
            // Session expired: delete journal + session directory
            let freed = delete_session_files(&dir, &session_id);
            result.bytes_freed += freed;
            result.sessions_deleted += 1;
        } else if mtime < compress_threshold {
            // Journal old enough to compress
            match compress_journal(&dir, &session_id) {
                Ok(()) => result.journals_compressed += 1,
                Err(e) => result.errors.push(format!("compress {session_id}: {e}")),
            }
        }
    }

    // Also clean up orphaned .jsonl.gz files past TTL
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(sid) = name_str.strip_suffix(".jsonl.gz") {
                let mtime = match entry.metadata().and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if mtime < ttl_threshold {
                    let freed = delete_session_files(&dir, sid);
                    result.bytes_freed += freed;
                    result.sessions_deleted += 1;
                }
            }
        }
    }

    result
}

/// Delete all files for a session: .jsonl, .jsonl.gz, and the session directory.
/// Returns the total bytes freed.
fn delete_session_files(sessions_dir: &Path, session_id: &str) -> u64 {
    let mut freed: u64 = 0;
    // Journal file (.jsonl)
    let journal = sessions_dir.join(format!("{session_id}.jsonl"));
    if let Ok(meta) = journal.metadata() {
        freed += meta.len();
        let _ = std::fs::remove_file(&journal);
    }
    // Compressed journal (.jsonl.gz)
    let gz = sessions_dir.join(format!("{session_id}.jsonl.gz"));
    if let Ok(meta) = gz.metadata() {
        freed += meta.len();
        let _ = std::fs::remove_file(&gz);
    }
    // Session directory (checkpoints, workspace, tool results, etc.)
    let session_dir = sessions_dir.join(session_id);
    if session_dir.is_dir() {
        if let Ok(size) = dir_size(&session_dir) {
            freed += size;
        }
        let _ = std::fs::remove_dir_all(&session_dir);
    }
    freed
}

/// Compress a .jsonl file to .jsonl.gz using gzip, then remove the original.
fn compress_journal(sessions_dir: &Path, session_id: &str) -> std::io::Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::{BufRead, BufReader, Write};

    let src = sessions_dir.join(format!("{session_id}.jsonl"));
    let dst = sessions_dir.join(format!("{session_id}.jsonl.gz"));

    // Don't re-compress if .gz already exists
    if dst.exists() {
        let _ = std::fs::remove_file(&src);
        return Ok(());
    }

    let reader = BufReader::new(std::fs::File::open(&src)?);
    let file = std::fs::File::create(&dst)?;
    let mut encoder = GzEncoder::new(file, Compression::default());

    for line in reader.lines() {
        let line = line?;
        encoder.write_all(line.as_bytes())?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()?;

    // Remove original after successful compression
    std::fs::remove_file(&src)?;
    Ok(())
}

/// Recursively compute total size of a directory tree.
fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_file() {
                total += entry.metadata()?.len();
            } else if ft.is_dir() {
                total += dir_size(&entry.path())?;
            }
        }
    }
    Ok(total)
}

// ═══════════════════════════════════════════════════════════ Tests ═════
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn journal_dir_guard_overrides_local_sessions_dir_nested() {
        let outer = tempdir().unwrap();
        let inner = tempdir().unwrap();
        let outer_sessions = outer.path().join("sessions");
        let inner_sessions = inner.path().join("sessions");
        std::fs::create_dir_all(&outer_sessions).unwrap();
        std::fs::create_dir_all(&inner_sessions).unwrap();

        let _g1 = JournalDirGuard::new(&outer_sessions);
        assert_eq!(local_sessions_dir(), outer_sessions);
        {
            let _g2 = JournalDirGuard::new(&inner_sessions);
            assert_eq!(local_sessions_dir(), inner_sessions);
        }
        assert_eq!(local_sessions_dir(), outer_sessions);
    }

    #[test]
    fn journal_event_session_start_serializes() {
        let evt = JournalEvent::session_start(Some("sess-1"), Some("gpt-4"));
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"session_start\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"model\":\"gpt-4\""));
        // Shouldn't have null fields
        assert!(!json.contains("\"turn\""));
    }

    #[test]
    fn journal_event_session_fork_round_trip() {
        let lineage = SessionLineage {
            parent_session_id: "parent-uuid".into(),
            forked_after_turn: Some(3),
            label: Some("try plan B".into()),
        };
        let evt = JournalEvent::session_fork(Some("child-uuid"), lineage, Some("note"));
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"session_fork\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::SessionFork);
        assert!(parsed.session_lineage.is_some());
    }

    #[test]
    fn journal_event_cloud_pull_sync_marker_round_trip() {
        let keys = vec!["explain_mode".to_string()];
        let evt = JournalEvent::cloud_pull_sync_marker(
            Some("sid-1"),
            "work",
            "repl_startup",
            Some(42),
            true,
            3,
            &keys,
            false,
        );
        assert_eq!(evt.event_type, JournalEventType::SyncMarker);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"sync_marker\""));
        assert!(json.contains("\"cloud_pull\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::SyncMarker);
        let meta = parsed.metadata.expect("metadata");
        let cp = meta.get("cloud_pull").expect("cloud_pull");
        assert_eq!(cp.get("profile").and_then(|v| v.as_str()), Some("work"));
        assert_eq!(
            cp.get("source").and_then(|v| v.as_str()),
            Some("repl_startup")
        );
        assert_eq!(
            cp.get("learning_version").and_then(|v| v.as_i64()),
            Some(42)
        );
        assert_eq!(
            cp.get("learning_snapshot_merged").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            cp.get("tool_health_rows_from_cloud")
                .and_then(|v| v.as_u64()),
            Some(3)
        );
        let pref = cp
            .get("preference_keys_merged")
            .and_then(|v| v.as_array())
            .expect("prefs array");
        assert_eq!(pref.len(), 1);
        assert_eq!(pref[0].as_str(), Some("explain_mode"));
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn journal_event_cloud_pull_sync_marker_empty_ack_round_trip() {
        let evt = JournalEvent::cloud_pull_sync_marker(
            Some("s-empty"),
            "default",
            "post_login",
            None,
            false,
            0,
            &[],
            true,
        );
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"reachable_empty_ack\":true"));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let cp = parsed
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .unwrap();
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn cloud_pull_sync_marker_append_to_journal_file() {
        let sid = format!("test-cloud-pull-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();
        let evt = JournalEvent::cloud_pull_sync_marker(
            Some(&sid),
            "default",
            "post_login",
            None,
            false,
            0,
            &[],
            true,
        );
        writer.append(&evt).unwrap();
        let events = read_journal(&sid).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, JournalEventType::SyncMarker);
        let cp = events[0]
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(
            cp.get("source").and_then(|v| v.as_str()),
            Some("post_login")
        );
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
        std::fs::remove_file(writer.path()).ok();
    }

    #[test]
    fn journal_event_turn_round_trip() {
        let evt = JournalEvent::turn(
            Some("sess-2"),
            3,
            Some("claude"),
            "hello",
            "world",
            2,
            100,
            50,
            1234,
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::Turn);
        assert_eq!(parsed.turn, Some(3));
        assert_eq!(parsed.tool_count, Some(2));
        assert_eq!(parsed.tokens_in, Some(100));
        assert_eq!(parsed.tokens_out, Some(50));
        assert_eq!(parsed.duration_ms, Some(1234));
    }

    #[test]
    fn journal_event_compact_has_correct_fields() {
        let evt = JournalEvent::compact(Some("s"), 5, 10, 3);
        assert_eq!(evt.event_type, JournalEventType::Compact);
        assert_eq!(evt.turns_compacted, Some(10));
        assert_eq!(evt.facts_stored, Some(3));
    }

    #[test]
    fn journal_event_config_change() {
        let evt = JournalEvent::config_change(Some("s"), "model", "gpt-4o");
        assert_eq!(evt.event_type, JournalEventType::ConfigChange);
        assert_eq!(evt.config_key, Some("model".to_string()));
        assert_eq!(evt.config_value, Some("gpt-4o".to_string()));
    }

    #[test]
    fn journal_event_error() {
        let evt = JournalEvent::error(Some("s"), "connection refused");
        assert_eq!(evt.event_type, JournalEventType::Error);
        assert_eq!(evt.error, Some("connection refused".to_string()));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let s = "a".repeat(600);
        let t = truncate(&s, 500);
        assert!(t.len() <= 504); // 500 chars + "…"
        assert!(t.ends_with('…'));
    }

    #[test]
    fn journal_writer_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".astra").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test-sess.jsonl");
        let writer = JournalWriter { path: path.clone() };
        let evt = JournalEvent::session_start(Some("test-sess"), None);
        writer.append(&evt).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("session_start"));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn journal_writer_appends_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".astra").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("multi.jsonl");
        let writer = JournalWriter { path: path.clone() };
        writer
            .append(&JournalEvent::session_start(Some("m"), None))
            .unwrap();
        writer
            .append(&JournalEvent::config_change(Some("m"), "model", "x"))
            .unwrap();
        writer
            .append(&JournalEvent::session_end(Some("m"), 5))
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn journal_event_skip_none_fields() {
        let evt = JournalEvent::session_start(None, None);
        let json = serde_json::to_string(&evt).unwrap();
        // Should NOT contain null session_id or model
        assert!(!json.contains("\"session_id\""));
        assert!(!json.contains("\"model\""));
        assert!(!json.contains("\"turn\""));
    }

    // ── Tool selection tracking (p5g observability + p6e feedback) ──

    #[test]
    fn turn_event_with_tool_selection_round_trip() {
        let evt = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "最新的pr?",
            "Here are the PRs...",
            2,
            500,
            200,
            1234,
        )
        .with_tool_selection(
            vec!["bash".into(), "github_list_prs".into(), "read_file".into()],
            vec!["tune-performance".into()],
            vec!["github_list_prs".into()],
            45,
        )
        .with_budget_pressure(0.6);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tools_selected.as_ref().unwrap().len(), 3);
        assert_eq!(
            parsed.selected_skills.as_ref().unwrap(),
            &["tune-performance"]
        );
        assert_eq!(parsed.tools_used.as_ref().unwrap(), &["github_list_prs"]);
        assert_eq!(parsed.budget_used, Some(45));
        assert_eq!(parsed.budget_pressure, Some(0.6));
    }

    #[test]
    fn turn_event_without_tool_selection_omits_fields() {
        let evt = JournalEvent::turn(Some("s2"), 1, None, "hello", "world", 0, 10, 5, 100);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(
            !json.contains("tools_selected"),
            "should omit None fields: {json}"
        );
        assert!(
            !json.contains("tools_used"),
            "should omit None fields: {json}"
        );
        assert!(
            !json.contains("budget_used"),
            "should omit None fields: {json}"
        );
        assert!(
            !json.contains("budget_pressure"),
            "should omit None fields: {json}"
        );
    }

    #[test]
    fn journal_write_read_with_selection_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".astra").join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sel-test.jsonl");
        let writer = JournalWriter { path: path.clone() };

        writer
            .append(&JournalEvent::session_start(
                Some("sel-test"),
                Some("gpt-4"),
            ))
            .unwrap();
        writer
            .append(
                &JournalEvent::turn(
                    Some("sel-test"),
                    1,
                    Some("gpt-4"),
                    "pr?",
                    "...",
                    1,
                    100,
                    50,
                    500,
                )
                .with_tool_selection(
                    vec!["bash".into(), "github_list_prs".into()],
                    vec![],
                    vec!["github_list_prs".into()],
                    35,
                )
                .with_budget_pressure(0.3),
            )
            .unwrap();
        writer
            .append(&JournalEvent::session_end(Some("sel-test"), 1))
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<JournalEvent> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(events.len(), 3);

        // Verify the turn event has selection data
        let turn = &events[1];
        assert_eq!(turn.event_type, JournalEventType::Turn);
        assert_eq!(turn.tools_selected.as_ref().unwrap().len(), 2);
        assert!(turn.selected_skills.is_none());
        assert_eq!(turn.tools_used.as_ref().unwrap(), &["github_list_prs"]);
        assert_eq!(turn.budget_used, Some(35));
        assert_eq!(turn.budget_pressure, Some(0.3));
    }

    #[test]
    fn backward_compat_old_events_missing_selection_fields() {
        // Old journal events won't have tools_selected/tools_used/budget_used/budget_pressure.
        // Verify serde handles missing fields gracefully.
        let old_json = r#"{"type":"turn","ts":"2025-01-01T00:00:00Z","session_id":"s","turn":1,"tool_count":0,"tokens_in":10,"tokens_out":5,"duration_ms":100}"#;
        let evt: JournalEvent = serde_json::from_str(old_json).unwrap();
        assert_eq!(evt.event_type, JournalEventType::Turn);
        assert!(evt.tools_selected.is_none());
        assert!(evt.tools_used.is_none());
        assert!(evt.budget_used.is_none());
        assert!(evt.budget_pressure.is_none());
        assert!(evt.tool_calls.is_none());
    }

    // ── Per-tool-call audit records ──

    #[test]
    fn tool_call_record_serialization_round_trip() {
        let record = ToolCallRecord {
            name: "github_list_prs".into(),
            ok: true,
            ms: 761,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("owner/repo".into()),
            result_preview: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(!json.contains("\"error\""), "None error should be omitted");
        let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "github_list_prs");
        assert!(parsed.ok);
        assert_eq!(parsed.ms, 761);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn tool_call_record_with_error() {
        let record = ToolCallRecord {
            name: "github_ci_status".into(),
            ok: false,
            ms: 587,
            error: Some("missing repo parameter".into()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("missing repo"));
        let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error.as_deref(), Some("missing repo parameter"));
    }

    #[test]
    fn turn_event_with_tool_calls_round_trip() {
        let evt = JournalEvent::turn(
            Some("s1"),
            3,
            Some("gpt-4"),
            "pr呢？",
            "Here are PRs...",
            1,
            300,
            150,
            800,
        )
        .with_tool_selection(
            vec!["github_list_prs".into()],
            vec![],
            vec!["github_list_prs".into()],
            20,
        )
        .with_tool_calls(vec![ToolCallRecord {
            name: "github_list_prs".into(),
            ok: true,
            ms: 761,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("owner/repo".into()),
            result_preview: None,
        }]);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "github_list_prs");
        assert!(calls[0].ok);
        assert_eq!(calls[0].ms, 761);
    }

    #[test]
    fn with_tool_calls_empty_omits_field() {
        let evt = JournalEvent::turn(Some("s1"), 1, None, "hi", "hello", 0, 10, 5, 50)
            .with_tool_calls(vec![]);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(
            !json.contains("tool_calls"),
            "empty tool_calls should be omitted: {json}"
        );
    }

    #[test]
    fn backward_compat_old_events_missing_tool_calls() {
        // Old events without tool_calls field should deserialize fine.
        let old_json = r#"{"type":"turn","ts":"2025-01-01T00:00:00Z","turn":1,"tool_count":2,"tokens_in":100,"tokens_out":50,"duration_ms":500,"tools_used":["bash","read_file"]}"#;
        let evt: JournalEvent = serde_json::from_str(old_json).unwrap();
        assert!(evt.tool_calls.is_none());
        assert_eq!(evt.tools_used.as_ref().unwrap().len(), 2);
    }

    // ── ToolCallRecord edge cases ──

    #[test]
    fn tool_call_record_bulk_array() {
        let records: Vec<ToolCallRecord> = (0..100)
            .map(|i| ToolCallRecord {
                name: format!("tool_{i}"),
                ok: i % 2 == 0,
                ms: i as u64 * 100,
                error: if i % 3 == 0 {
                    Some(format!("err_{i}"))
                } else {
                    None
                },
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
            })
            .collect();
        let json = serde_json::to_string(&records).unwrap();
        let parsed: Vec<ToolCallRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 100);
        assert_eq!(parsed[99].name, "tool_99");
        assert_eq!(parsed[0].ms, 0);
    }

    #[test]
    fn tool_call_record_unicode_error() {
        let record = ToolCallRecord {
            name: "github_list_prs".into(),
            ok: false,
            ms: 500,
            error: Some("连接超时: タイムアウト 🚫".into()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error.unwrap(), "连接超时: タイムアウト 🚫");
    }

    #[test]
    fn tool_call_record_max_ms_value() {
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            ms: u64::MAX,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ms, u64::MAX);
    }

    #[test]
    fn resolve_session_id_accepts_exact_match() {
        let sessions = vec![
            "abc12345-0000-0000-0000-000000000000".to_string(),
            "def67890-0000-0000-0000-000000000000".to_string(),
        ];
        let resolved =
            resolve_session_id_from_list("abc12345-0000-0000-0000-000000000000", &sessions)
                .unwrap();
        assert_eq!(resolved, "abc12345-0000-0000-0000-000000000000");
    }

    #[test]
    fn resolve_session_id_accepts_unique_prefix() {
        let sessions = vec![
            "f5d90983-7130-41b6-8947-9827257c34f4".to_string(),
            "0be92d83-fb65-47d0-815a-dc8442930c3a".to_string(),
        ];
        let resolved = resolve_session_id_from_list("f5d90983-713", &sessions).unwrap();
        assert_eq!(resolved, "f5d90983-7130-41b6-8947-9827257c34f4");
    }

    #[test]
    fn resolve_session_id_rejects_ambiguous_prefix() {
        let sessions = vec![
            "f5d90983-7130-41b6-8947-9827257c34f4".to_string(),
            "f5d90983-7131-4b27-8b15-cfdc3375390f".to_string(),
        ];
        let err = resolve_session_id_from_list("f5d90983-713", &sessions).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn resolve_session_id_rejects_unknown_prefix() {
        let sessions = vec!["abc12345-0000-0000-0000-000000000000".to_string()];
        let err = resolve_session_id_from_list("missing", &sessions).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no session journal matches"));
    }

    #[test]
    fn turn_guard_verdict_event_serializes() {
        let evt = JournalEvent::turn_guard_verdict(
            Some("sess-1"),
            3,
            "warning",
            &["Stall detected: repeated bash calls".to_string()],
            &["bash".to_string()],
            false,
            1,
            2,
            1,
            0, // total_timeouts
            0, // total_cache_hits
            0, // flaky_count
        );
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"turn_guard_verdict\""));
        assert!(json.contains("\"turn\":3"));
        // stall_type field reused for severity
        assert!(json.contains("\"stall_type\":\"warning\""));

        // Metadata should contain verdict details
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["severity"], "warning");
        assert_eq!(meta["injections"], 1);
        assert_eq!(meta["avoid_tools"][0], "bash");
        assert_eq!(meta["force_stop"], false);
        assert_eq!(meta["nudge_count"], 1);
        assert_eq!(meta["total_errors"], 2);
        assert_eq!(meta["deprioritized_tools"], 1);
        assert_eq!(meta["total_timeouts"], 0);
        assert_eq!(meta["total_cache_hits"], 0);
        assert_eq!(meta["flaky_tools"], 0);
    }

    #[test]
    fn turn_guard_verdict_critical_force_stop() {
        let evt = JournalEvent::turn_guard_verdict(
            Some("sess-1"),
            5,
            "critical",
            &[
                "CRITICAL: multiple stalls".to_string(),
                "Tool health degraded".to_string(),
            ],
            &["bash".to_string(), "grep".to_string()],
            true,
            3,
            5,
            2,
            2, // total_timeouts
            1, // total_cache_hits
            1, // flaky_count
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["severity"], "critical");
        assert_eq!(meta["force_stop"], true);
        assert_eq!(meta["injections"], 2);
        assert_eq!(meta["nudge_count"], 3);
        assert_eq!(meta["total_timeouts"], 2);
        assert_eq!(meta["total_cache_hits"], 1);
        assert_eq!(meta["flaky_tools"], 1);
        // injection_preview should truncate to first injection
        assert!(
            meta["injection_preview"]
                .as_str()
                .unwrap()
                .contains("CRITICAL")
        );
    }

    #[test]
    fn turn_guard_verdict_info_minimal() {
        let evt =
            JournalEvent::turn_guard_verdict(None, 1, "info", &[], &[], false, 0, 1, 0, 0, 0, 0);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["injections"], 0);
        assert!(meta["injection_preview"].is_null());
        assert_eq!(meta["force_stop"], false);
    }

    // ── stall_detected event ──

    #[test]
    fn stall_detected_event_has_correct_fields() {
        let evt = JournalEvent::stall_detected(
            Some("sess-1"),
            5,
            "repetition_stall",
            2,
            0.7,
            &["bash".to_string(), "grep".to_string()],
        );

        assert_eq!(evt.event_type, JournalEventType::StallDetected);
        assert_eq!(evt.turn, Some(5));
        assert_eq!(evt.stall_type.as_deref(), Some("repetition_stall"));

        let meta = evt.metadata.unwrap();
        assert_eq!(meta["nudge_count"], 2);
        assert_eq!(meta["confidence"], 0.7);
        assert_eq!(meta["avoid_tools"][0], "bash");
        assert_eq!(meta["avoid_tools"][1], "grep");
    }

    #[test]
    fn stall_detected_event_json_roundtrip() {
        let evt = JournalEvent::stall_detected(
            Some("sess-2"),
            3,
            "exploration_stall",
            1,
            0.5,
            &["list_dir".to_string()],
        );
        let json = serde_json::to_string(&evt).unwrap();
        let restored: JournalEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.event_type, JournalEventType::StallDetected);
        assert_eq!(restored.turn, Some(3));
        let meta = restored.metadata.unwrap();
        assert_eq!(meta["nudge_count"], 1);
        assert_eq!(meta["confidence"], 0.5);
    }

    #[test]
    fn stall_detected_confidence_range() {
        // Confidence should be stored as-is (0.0 to 1.0)
        for confidence in [0.0, 0.5, 0.8, 1.0] {
            let evt = JournalEvent::stall_detected(Some("s"), 1, "stall", 0, confidence, &[]);
            let meta = evt.metadata.unwrap();
            let stored = meta["confidence"].as_f64().unwrap();
            assert!(
                (stored - confidence).abs() < 1e-9,
                "confidence {confidence} should be stored exactly, got {stored}"
            );
        }
    }

    // ── checkpoint event ──

    #[test]
    fn checkpoint_event_has_correct_fields() {
        let evt = JournalEvent::checkpoint(
            Some("sess-1"),
            10,
            "Completed token efficiency phase",
            50_000,
            15,
        );

        assert_eq!(evt.event_type, JournalEventType::Checkpoint);
        assert_eq!(evt.turn, Some(10));

        let meta = evt.metadata.unwrap();
        assert_eq!(meta["summary"], "Completed token efficiency phase");
        assert_eq!(meta["total_tokens"], 50_000);
        assert_eq!(meta["tools_used_count"], 15);
    }

    #[test]
    fn checkpoint_event_json_roundtrip() {
        let evt = JournalEvent::checkpoint(Some("sess-1"), 5, "Phase A done", 10_000, 8);
        let json = serde_json::to_string(&evt).unwrap();
        let restored: JournalEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.event_type, JournalEventType::Checkpoint);
        assert_eq!(restored.turn, Some(5));
        let meta = restored.metadata.unwrap();
        assert_eq!(meta["summary"], "Phase A done");
        assert_eq!(meta["total_tokens"], 10_000);
        assert_eq!(meta["tools_used_count"], 8);
    }

    #[test]
    fn checkpoint_summary_truncated_at_500_chars() {
        let long_summary = "x".repeat(600);
        let evt = JournalEvent::checkpoint(Some("s"), 1, &long_summary, 0, 0);
        let meta = evt.metadata.unwrap();
        let stored = meta["summary"].as_str().unwrap();
        // truncate() takes 500 chars then appends '…' (1 char, 3 bytes)
        assert!(
            stored.chars().count() <= 501,
            "summary should be truncated to ~500 chars, got {}",
            stored.chars().count()
        );
        assert!(
            stored.ends_with('…'),
            "truncated summary should end with ellipsis"
        );
    }

    #[test]
    fn stall_and_checkpoint_events_written_to_journal() {
        let sid = format!("test-stall-ckpt-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();

        writer
            .append(&JournalEvent::stall_detected(
                Some(&sid),
                3,
                "repetition_stall",
                1,
                0.7,
                &["bash".to_string()],
            ))
            .unwrap();
        writer
            .append(&JournalEvent::checkpoint(
                Some(&sid),
                5,
                "Midpoint checkpoint",
                20_000,
                10,
            ))
            .unwrap();

        let events = read_journal(&sid).unwrap();
        assert_eq!(events.len(), 2);

        let stall = &events[0];
        assert_eq!(stall.event_type, JournalEventType::StallDetected);
        assert_eq!(stall.stall_type.as_deref(), Some("repetition_stall"));

        let ckpt = &events[1];
        assert_eq!(ckpt.event_type, JournalEventType::Checkpoint);
        let meta = ckpt.metadata.as_ref().unwrap();
        assert_eq!(meta["summary"], "Midpoint checkpoint");
        assert_eq!(meta["total_tokens"], 20_000);
    }

    #[test]
    fn plan_progress_event_builder() {
        let evt = JournalEvent::plan_progress(
            Some("s1"),
            5,
            "add-tests",
            "Add unit tests",
            "started",
            40,
            5,
            2,
        );
        assert_eq!(evt.event_type, JournalEventType::PlanProgress);
        assert_eq!(evt.turn, Some(5));
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["subtask_id"], "add-tests");
        assert_eq!(meta["subtask_title"], "Add unit tests");
        assert_eq!(meta["action"], "started");
        assert_eq!(meta["progress_pct"], 40);
        assert_eq!(meta["total_subtasks"], 5);
        assert_eq!(meta["completed_subtasks"], 2);
    }

    #[test]
    fn plan_progress_serialization_roundtrip() {
        let evt =
            JournalEvent::plan_progress(Some("s1"), 3, "fix-bug", "Fix login", "started", 0, 3, 0);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::PlanProgress);
        assert_eq!(parsed.turn, Some(3));
        let meta = parsed.metadata.as_ref().unwrap();
        assert_eq!(meta["subtask_id"], "fix-bug");
        assert_eq!(meta["action"], "started");

        // Also test completed and plan_complete variants
        let evt2 =
            JournalEvent::plan_progress(Some("s1"), 5, "", "Full plan", "plan_complete", 100, 3, 3);
        let json2 = serde_json::to_string(&evt2).unwrap();
        let parsed2: JournalEvent = serde_json::from_str(&json2).unwrap();
        assert_eq!(
            parsed2.metadata.as_ref().unwrap()["action"],
            "plan_complete"
        );
        assert_eq!(parsed2.metadata.as_ref().unwrap()["progress_pct"], 100);
    }

    // ── count_turns tests ──────────────────────────────────────────────

    #[test]
    fn count_turns_counts_only_turn_events() {
        let dir = tempfile::tempdir().unwrap();
        // Override journal_dir by writing directly
        let sid = format!("count-test-{}", uuid::Uuid::new_v4());
        let path = dir.path().join(format!("{sid}.jsonl"));

        // Write mixed event types
        let lines = [
            r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z","session_id":"s"}"#,
            r#"{"type":"turn","ts":"2026-01-01T00:00:01Z","session_id":"s","turn":1}"#,
            r#"{"type":"checkpoint","ts":"2026-01-01T00:00:02Z","session_id":"s"}"#,
            r#"{"type":"turn","ts":"2026-01-01T00:00:03Z","session_id":"s","turn":2}"#,
            r#"{"type":"session_end","ts":"2026-01-01T00:00:04Z","session_id":"s"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        // count_turns reads from journal_dir(), so test via the actual function
        // by writing to the real journal dir
        let real_path = journal_dir().join(format!("{sid}.jsonl"));
        std::fs::create_dir_all(journal_dir()).ok();
        std::fs::write(&real_path, lines.join("\n")).unwrap();

        let count = count_turns(&sid);
        assert_eq!(count, 2, "should count exactly 2 turn events");

        // Cleanup
        let _ = std::fs::remove_file(&real_path);
    }

    #[test]
    fn count_turns_returns_zero_for_missing_session() {
        assert_eq!(count_turns("nonexistent-session-xyz-999"), 0);
    }

    #[test]
    fn count_turns_ignores_checkpoint_and_other_types() {
        let sid = format!("count-no-turns-{}", uuid::Uuid::new_v4());
        let real_path = journal_dir().join(format!("{sid}.jsonl"));
        std::fs::create_dir_all(journal_dir()).ok();
        std::fs::write(
            &real_path,
            r#"{"type":"session_start","ts":"2026-01-01T00:00:00Z"}
{"type":"checkpoint","ts":"2026-01-01T00:00:01Z"}
{"type":"session_end","ts":"2026-01-01T00:00:02Z"}"#,
        )
        .unwrap();

        assert_eq!(count_turns(&sid), 0);
        let _ = std::fs::remove_file(&real_path);
    }

    // ── list_sessions_by_time tests ────────────────────────────────────

    #[test]
    fn list_sessions_by_time_filters_test_prefixes() {
        std::fs::create_dir_all(journal_dir()).ok();

        // Create test-prefixed and real session files
        let real_sid = format!("real-session-{}", uuid::Uuid::new_v4());
        let test_sid = format!("test-session-{}", uuid::Uuid::new_v4());
        let new_sess = format!("new-sess-{}", uuid::Uuid::new_v4());

        let real_path = journal_dir().join(format!("{real_sid}.jsonl"));
        let test_path = journal_dir().join(format!("{test_sid}.jsonl"));
        let new_path = journal_dir().join(format!("{new_sess}.jsonl"));

        std::fs::write(&real_path, "{}").unwrap();
        std::fs::write(&test_path, "{}").unwrap();
        std::fs::write(&new_path, "{}").unwrap();

        let sessions = list_sessions_by_time(100).unwrap();
        assert!(
            sessions.contains(&real_sid),
            "real session should be listed"
        );
        assert!(
            !sessions.contains(&test_sid),
            "test- prefix should be filtered"
        );
        assert!(
            !sessions.contains(&new_sess),
            "new-sess- prefix should be filtered"
        );

        // Cleanup
        let _ = std::fs::remove_file(&real_path);
        let _ = std::fs::remove_file(&test_path);
        let _ = std::fs::remove_file(&new_path);
    }

    #[test]
    fn list_sessions_by_time_respects_limit() {
        std::fs::create_dir_all(journal_dir()).ok();

        let mut created = Vec::new();
        for i in 0..5 {
            let sid = format!("limit-test-{i}-{}", uuid::Uuid::new_v4());
            let path = journal_dir().join(format!("{sid}.jsonl"));
            std::fs::write(&path, "{}").unwrap();
            // Stagger mtime slightly
            std::thread::sleep(std::time::Duration::from_millis(10));
            created.push((sid, path));
        }

        let sessions = list_sessions_by_time(3).unwrap();
        let our_sessions: Vec<_> = sessions
            .iter()
            .filter(|s| s.starts_with("limit-test-"))
            .collect();
        assert!(
            our_sessions.len() <= 3,
            "should return at most 3 of our sessions, got {}",
            our_sessions.len()
        );

        // Cleanup
        for (_, path) in &created {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn delegation_started_event_builder() {
        let agents = vec!["agent-a".to_string(), "agent-b".to_string()];
        let evt =
            JournalEvent::delegation_started(Some("s1"), "del-1", "run-parent", "fan_out", &agents);
        assert_eq!(evt.event_type, JournalEventType::DelegationStarted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["pattern"], "fan_out");
        assert_eq!(meta["agent_count"], 2);
    }

    #[test]
    fn delegation_sub_run_completed_event_builder() {
        let evt = JournalEvent::delegation_sub_run_completed(
            Some("s1"),
            "del-1",
            "run-sub-1",
            "agent-a",
            "completed",
            None,
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationSubRunCompleted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["agent_id"], "agent-a");
        assert_eq!(meta["status"], "completed");
        assert!(meta["error"].is_null());
    }

    #[test]
    fn delegation_completed_event_builder() {
        let evt =
            JournalEvent::delegation_completed(Some("s1"), "del-1", "fan_out", 3, 2, 1, "partial");
        assert_eq!(evt.event_type, JournalEventType::DelegationCompleted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["succeeded"], 2);
        assert_eq!(meta["failed"], 1);
        assert_eq!(meta["aggregated_status"], "partial");
    }

    #[test]
    fn delegation_events_serialize_roundtrip() {
        let agents = vec!["a1".to_string()];
        let evt = JournalEvent::delegation_started(Some("s1"), "d1", "r1", "sequential", &agents);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::DelegationStarted);
        assert!(json.contains("\"delegation_id\":\"d1\""));
    }

    // ── Session ID Validation Security Tests ──

    #[test]
    fn validate_session_id_rejects_path_traversal() {
        assert!(validate_session_id("../../etc/passwd").is_err());
        assert!(validate_session_id("../sibling").is_err());
        assert!(validate_session_id("a/b/c").is_err());
        assert!(validate_session_id("a\\b").is_err());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("   ").is_err());
        assert!(validate_session_id("a\0b").is_err());
    }

    #[test]
    fn validate_session_id_accepts_safe_ids() {
        assert!(validate_session_id("abc-123").is_ok());
        assert!(validate_session_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_session_id("my_session").is_ok());
        assert!(validate_session_id("session.2024").is_ok());
    }

    #[test]
    #[should_panic(expected = "unsafe session ID")]
    fn journal_file_path_panics_on_traversal() {
        let _ = journal_file_path("../../etc/passwd");
    }

    // ── Session Lifecycle Maintenance Tests ──────────────────────────

    /// Helper: create a journal file with a backdated mtime.
    fn create_aged_journal(dir: &Path, session_id: &str, age_days: u64) {
        let path = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&path, r#"{"type":"session_start"}"#).unwrap();
        let mtime = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - std::time::Duration::from_secs(age_days * 86400 + 3600),
        );
        filetime::set_file_mtime(&path, mtime).unwrap();
    }

    /// Helper: create a session subdirectory with some data.
    fn create_session_dir(dir: &Path, session_id: &str) {
        let session_dir = dir.join(session_id);
        std::fs::create_dir_all(session_dir.join("step_checkpoints")).unwrap();
        std::fs::write(session_dir.join("workspace.yaml"), "session_id: test").unwrap();
    }

    #[test]
    fn maintenance_deletes_expired_sessions() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // Session older than TTL (40 days old, TTL=30)
        create_aged_journal(&dir, "old-session", 40);
        create_session_dir(&dir, "old-session");

        // Recent session (1 day old)
        create_aged_journal(&dir, "new-session", 1);

        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.sessions_deleted, 1);
        assert!(!dir.join("old-session.jsonl").exists());
        assert!(!dir.join("old-session").exists());
        assert!(dir.join("new-session.jsonl").exists());
    }

    #[test]
    fn maintenance_compresses_old_journals() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // Session 10 days old (compress_after=7, ttl=30)
        create_aged_journal(&dir, "mid-session", 10);

        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.journals_compressed, 1);
        assert!(!dir.join("mid-session.jsonl").exists());
        assert!(dir.join("mid-session.jsonl.gz").exists());
    }

    #[test]
    fn maintenance_skips_recent_sessions() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // Very recent session (0 days old)
        std::fs::write(dir.join("fresh.jsonl"), r#"{"type":"session_start"}"#).unwrap();

        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.sessions_deleted, 0);
        assert_eq!(result.journals_compressed, 0);
        assert!(dir.join("fresh.jsonl").exists());
    }

    #[test]
    fn maintenance_deletes_expired_compressed_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // Create an old .jsonl.gz file (40 days old)
        let gz_path = dir.join("archived.jsonl.gz");
        std::fs::write(&gz_path, b"fake-gz-data").unwrap();
        let mtime = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86400 + 3600),
        );
        filetime::set_file_mtime(&gz_path, mtime).unwrap();

        let result = run_session_maintenance_in(dir.clone(), 30, 7);
        assert_eq!(result.sessions_deleted, 1);
        assert!(!gz_path.exists());
    }

    #[test]
    fn maintenance_empty_dir_returns_default() {
        let tmp = tempdir().unwrap();
        let result = run_session_maintenance_in(tmp.path().to_path_buf(), 30, 7);
        assert_eq!(result.sessions_deleted, 0);
        assert_eq!(result.journals_compressed, 0);
        assert_eq!(result.bytes_freed, 0);
    }
}
