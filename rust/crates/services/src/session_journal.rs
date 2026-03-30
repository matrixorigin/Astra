//! Session Journal — local JSONL persistence for observability & auditability.
//!
//! Writes one line per event to `~/.mo-agent/sessions/<session_id>.jsonl`.
//! Events include: turn completions, config changes, errors, compactions.
//!
//! The journal is append-only and survives process exits.
//! It can be replayed, exported, or analyzed by `/session` commands.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

/// Per-tool-call audit record, embedded in turn events for granular tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Tool name.
    pub name: String,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Execution time in milliseconds.
    pub ms: u64,
    /// Error message if the call failed.
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
        if let Err(e) = writeln!(file, "{line}") {
            if e.kind() == std::io::ErrorKind::Other
                || e.raw_os_error() == Some(28) // ENOSPC
                || e.to_string().contains("No space")
            {
                mo_agent_core::agent_error!("journal", "disk full, journal event lost");
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

/// Read all events from a session journal file.
pub fn read_journal(session_id: &str) -> std::io::Result<Vec<JournalEvent>> {
    let path = journal_dir().join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut events = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalEvent>(line) {
            Ok(evt) => events.push(evt),
            Err(_) => continue, // skip malformed lines
        }
    }
    Ok(events)
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

/// Helper: get the journal directory path.
fn journal_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
        .join("sessions")
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
            tools_used: None,
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

    /// Session start event.
    pub fn session_start(session_id: Option<&str>, model: Option<&str>) -> Self {
        let mut evt = Self::base(JournalEventType::SessionStart, session_id);
        evt.model = model.map(|s| s.to_string());
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
        evt.assistant_output = Some(truncate(assistant_output, 1000));
        evt.tool_count = Some(tool_count);
        evt.tokens_in = Some(tokens_in);
        evt.tokens_out = Some(tokens_out);
        evt.duration_ms = Some(duration_ms);
        evt
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
        tools_used: Vec<String>,
        budget_used: u32,
    ) -> Self {
        self.tools_selected = Some(tools_selected);
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

// ═══════════════════════════════════════════════════════════ Tests ═════
#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = tmp.path().join(".mo-agent").join("sessions");
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
        let dir = tmp.path().join(".mo-agent").join("sessions");
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
            vec!["github_list_prs".into()],
            45,
        )
        .with_budget_pressure(0.6);
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tools_selected.as_ref().unwrap().len(), 3);
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
        let dir = tmp.path().join(".mo-agent").join("sessions");
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
}
