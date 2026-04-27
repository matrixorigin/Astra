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

use crate::SessionArtifactStore;

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
    /// Optional upstream event ids when this event is caused by multiple parents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_event_ids: Option<Vec<String>>,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// File path extracted from full tool arguments at execution time.
    /// More reliable than parsing `args_preview` (which is truncated to ~80 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Explicit flag for surgically removed tool calls. When `true`, this record
    /// is an audit-only placeholder — the parallel tool call was removed from
    /// context because a skill covered the work. Prefer this over checking
    /// `name == SURGICAL_REMOVAL_TOOL_NAME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surgically_removed: Option<bool>,
    /// Original tool name before surgical removal replaced it with the sentinel.
    /// Only set when `surgically_removed == Some(true)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_tool_name: Option<String>,
    // ── Observability fields (Phase 1) ───────────────────────────────────
    /// Offset from turn start when this tool began executing (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_offset_ms: Option<u64>,
    /// Batch ID shared by tools executed in parallel within the same round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// Whether this tool was executed in parallel with others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<bool>,
    /// LLM round index within the turn (0-based). Identifies which LLM→tool
    /// cycle this call belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// Full tool arguments as JSON string (untruncated).
    /// Enables exact tool call reproduction from journal data alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_full: Option<String>,
    /// Full tool result text (untruncated, after per-tool output limit).
    /// Enables debugging tool failures without re-execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_full: Option<String>,
    /// When set, this record represents a short-circuited `skill(name=X)`
    /// re-invocation. The value is the re-entry index (1 = first repeat call,
    /// 2 = second, ...). Surfaces skill-loop inefficiencies in journal digests
    /// without needing to grep `result_preview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_reentry_count: Option<u32>,
    /// When `true`, this short-circuited skill call was blocked by the per-turn
    /// re-entry lockout (reentry_count ≥ 3). The executor refused to even
    /// produce a follow-the-instructions stub and returned a BLOCKED result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_locked_out: Option<bool>,
}

/// Tool call name sentinel used for assistant messages that had parallel tool
/// calls surgically removed from context (see
/// `runtime::turn::agentic_tool_interception`). These are intentional context
/// optimizations — **not** tool failures — and are filtered out of
/// evaluation/analytics by [`ToolCallRecord::is_synthetic_placeholder`].
pub const SURGICAL_REMOVAL_TOOL_NAME: &str = "(surgically_removed)";

impl ToolCallRecord {
    /// Synthetic placeholders are audit-only records emitted when skill routing
    /// suppresses a tool call without actually executing it, **or** when a
    /// parallel tool call was surgically removed from context after a skill
    /// took over its work. Neither case represents real tool execution or
    /// failure, so these records must be filtered out before computing
    /// analytics (tool_error_rate, repeat_tool_call, failed_tool_calls, …).
    pub fn is_synthetic_placeholder(&self) -> bool {
        if self.surgically_removed == Some(true) {
            return true;
        }
        if self.name == SURGICAL_REMOVAL_TOOL_NAME {
            return true;
        }

        let Some(result_preview) = self.result_preview.as_deref() else {
            return false;
        };

        result_preview.starts_with("Skipped:")
            || result_preview.starts_with("Deferred:")
            || (self.name == "skill"
                && result_preview.starts_with("Skill '")
                && result_preview.contains(" was already loaded (turn "))
    }

    /// True when this tool call was rejected by the pipeline before execution
    /// (e.g. restricted_tools policy).  These calls should not count toward
    /// `tools_used` for pattern learning — they never ran, so attributing
    /// turn-level success/failure to them creates a self-reinforcing block loop.
    pub fn was_blocked_by_policy(&self) -> bool {
        !self.ok
            && self
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("blocked_tool:"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalJournalDecision {
    pub request_id: String,
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalJournalRequest {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_kind: Option<String>,
}

fn normalize_optional_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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
    /// Internal agentic step within the outer session turn (0-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentic_step: Option<u32>,
    /// LLM model used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// User input text (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input: Option<String>,
    /// Assistant response text (for turn events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_output: Option<String>,
    /// Number of material tool executions in this turn.
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
    /// Full context assembly trace for deep observability (M1 telemetry).
    /// Stores the serialized ContextAssemblyTrace from runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_assembly_trace: Option<serde_json::Value>,
    /// Selector confidence from the first tool-selection pass (0.0–1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector_confidence: Option<f64>,
    /// Routing domain hint label for this REPL turn (e.g. `github`); omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_domain_hint: Option<String>,
    /// True when the turn succeeded with tool calls but routing had no domain — entity graph learn was skipped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub entity_learn_skipped_no_domain: bool,
    // ── Observability fields (Phase 1) ───────────────────────────────────
    /// LLM round index within a turn (0-based, for llm_round events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// Number of tool_calls returned by LLM in this round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls_returned: Option<u32>,
    /// Offset from turn start in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<u64>,
    /// Total LLM rounds in this turn (set on turn_completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_rounds: Option<u32>,
    /// Total LLM time in this turn excluding tool execution (set on turn_completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_llm_ms: Option<u64>,
    /// Total tool execution time in this turn (set on turn_completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tool_ms: Option<u64>,
    // ── Causal lineage (P5) ──────────────────────────────────────────────
    /// Parent event ID for causal tree construction.
    /// Turn → SessionStart, LlmRound → Turn, DelegationSubRunStarted → DelegationStarted, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
    // ── Git snapshot (P0) ────────────────────────────────────────────────
    /// Git HEAD commit hash at the time of this event (short or full SHA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    /// Git branch name at the time of this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
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
    /// Turn quality evaluation recorded for audit and replay surfaces.
    TurnEvaluation,
    /// Plan execution progress (subtask started, completed, plan done).
    PlanProgress,
    /// Forked from another session — records lineage for audit and sync.
    SessionFork,
    /// Cloud–edge policy ack, agent handoff, or other sync metadata (lightweight).
    SyncMarker,
    /// Delegation group started (sub-run group spawned).
    DelegationStarted,
    /// A single sub-run within a delegation started running.
    DelegationSubRunStarted,
    /// A single sub-run within a delegation completed.
    DelegationSubRunCompleted,
    /// A sub-run was retried, linking the original run to the new retry run.
    DelegationRetry,
    /// Delegation completed (all sub-runs done, results aggregated).
    DelegationCompleted,
    /// Adaptive baseline promoted from a completed experiment winner.
    AdaptiveBaselinePromoted,
    /// A spawned agent terminated (completed, failed, or cancelled).
    AgentTerminated,
    /// Subtask or plan verification completed (acceptance-criteria gate result).
    VerificationCompleted,
    /// A composite snapshot was taken — captures references to session state,
    /// data snapshot, memory snapshot, git commit, etc.
    CompositeSnapshot,
    /// Plan was edited (subtask added/removed/reordered, goal changed).
    PlanEdit,
    /// Plan lifecycle event (created, completed, abandoned, replanned).
    PlanLifecycle,
    /// Effective goal steering changed (manual goal set, active plan goal took over).
    GoalSteered,
    /// An approval prompt was emitted for a tool call.
    ApprovalRequired,
    /// An approval decision was received for a tool call.
    ApprovalDecision,
    /// An approval prompt timed out before a decision arrived.
    ApprovalTimeout,
    /// A rollback-capable execution boundary started tracking side effects.
    ExecutionBoundaryOpened,
    /// A rollback-capable execution boundary finished successfully.
    ExecutionBoundaryCommitted,
    /// A rollback-capable execution boundary aborted and may have rolled back prior work.
    ExecutionBoundaryAborted,
    /// Context assembly trace recorded (observability: prompt building details).
    ContextAssemblyRecorded,
    /// Focus drift detected during a turn (severity, cause, evidence).
    DriftDetected,
    /// Scenario detected and adaptive profile applied for this session.
    AdaptiveScenarioApplied,
    /// Per-turn micro-adaptation adjusted config values.
    AdaptivePerTurnApplied,
    /// Experiment enrollment — session assigned to an experiment variant.
    AdaptiveExperimentEnrolled,
    /// Tuning rule evaluated and triggered a config change.
    AdaptiveTuningRuleTriggered,
    /// A structured interruption was recorded (budget exhaustion, rate limit, cancel, etc.).
    InterruptionRecorded,
    /// Low or very-low selector confidence diagnosed (tier, reasons, fallback action).
    ConfidenceDiagnosisRecorded,
    /// Compaction retry completed — records tier, tokens freed, and per-layer breakdown.
    CompactionRetry,
    /// One LLM→tools round within a turn (observability Phase 1).
    LlmRound,
    /// Full LLM request payload for a single attempt within a round.
    LlmRequestFull,
    /// Full LLM response payload for a single attempt within a round.
    LlmResponseFull,
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
    ///
    /// **Concurrency:** the line + trailing `\n` are written via a single
    /// `write_all` call so concurrent appenders cannot interleave the newline
    /// with another writer's payload. On Linux, writes to a regular file
    /// opened with `O_APPEND` of size <= `PIPE_BUF` (4096 bytes) are atomic;
    /// `writeln!` would issue the `\n` as a separate syscall and lose
    /// atomicity, producing concatenated records like `{a}{b}\n\n` that the
    /// reader cannot parse. See `JournalWriter::append` test
    /// `concurrent_appends_remain_record_separated`.
    pub fn append(&self, event: &JournalEvent) -> std::io::Result<()> {
        use std::io::Write;
        let mut buf = serde_json::to_vec(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        buf.push(b'\n');
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
        if let Err(e) = file.write_all(&buf) {
            if e.kind() == std::io::ErrorKind::Other
                || e.raw_os_error() == Some(28) // ENOSPC
                || e.to_string().contains("No space")
            {
                astra_core::agent_error!("journal", "disk full, journal event lost");
            }
            return Err(e);
        }
        // Ensure durability: flush to disk so a crash doesn't lose the event.
        file.sync_data()?;
        Ok(())
    }

    /// Get the path to this journal file.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Batch-append multiple events in a single write + fsync.
    pub fn append_bulk(&self, events: &[JournalEvent]) -> std::io::Result<()> {
        self.append_bulk_inner(events, true)
    }

    /// Batch-append multiple events without fsync (best-effort, for interrupted turns).
    pub fn append_bulk_no_sync(&self, events: &[JournalEvent]) -> std::io::Result<()> {
        self.append_bulk_inner(events, false)
    }

    fn append_bulk_inner(&self, events: &[JournalEvent], sync: bool) -> std::io::Result<()> {
        use std::io::Write;
        if events.is_empty() {
            return Ok(());
        }
        let mut buf = String::new();
        for event in events {
            let line = serde_json::to_string(event)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        file.write_all(buf.as_bytes())?;
        if sync {
            file.sync_data()?;
        }
        Ok(())
    }
}

// ─── Turn Event Buffer ───────────────────────────────────────────────────────

/// Data for one LLM→tools round within a turn.
pub struct LlmRoundRecord {
    pub ttft_ms: Option<u64>,
    pub duration_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub tool_calls_returned: u32,
    pub tool_call_names: Vec<String>,
    pub finish_reason: Option<String>,
    pub agentic_step: Option<u32>,
    pub source: Option<String>,
    pub run_id: Option<String>,
    pub tool_calls: Option<Vec<ToolCallRecord>>,
}

/// In-memory collector for fine-grained turn events.
///
/// Events are accumulated during a turn and flushed to the journal in a single
/// IO operation when the turn completes. On interruption, `flush_interrupted`
/// writes partial data without fsync.
pub struct TurnEventBuffer {
    events: Vec<JournalEvent>,
    turn_start: std::time::Instant,
    session_id: Option<String>,
    turn: u32,
    round: u32,
    batch_counter: u32,
}

impl TurnEventBuffer {
    /// Start collecting events for a new turn.
    pub fn begin_turn(session_id: Option<&str>, turn: u32) -> Self {
        Self::begin_turn_with_round(session_id, turn, 0)
    }

    /// Start collecting events for a new turn at a specific round offset.
    pub fn begin_turn_with_round(session_id: Option<&str>, turn: u32, round: u32) -> Self {
        Self {
            events: Vec::new(),
            turn_start: std::time::Instant::now(),
            session_id: session_id.map(ToString::to_string),
            turn,
            round,
            batch_counter: 0,
        }
    }

    /// Elapsed milliseconds since turn start.
    pub fn offset_ms(&self) -> u64 {
        self.turn_start.elapsed().as_millis() as u64
    }

    /// The instant when this turn started (for passing to sub-contexts).
    pub fn turn_start_instant(&self) -> std::time::Instant {
        self.turn_start
    }

    /// Current LLM round index (0-based).
    pub fn current_round(&self) -> u32 {
        self.round
    }

    /// Generate a batch ID for a group of parallel tool executions.
    pub fn next_batch_id(&mut self) -> String {
        let id = format!("b-{}-{}", self.round, self.batch_counter);
        self.batch_counter += 1;
        id
    }

    /// Record an LLM round completion (one LLM→tools cycle).
    pub fn record_llm_round(&mut self, r: LlmRoundRecord) {
        let mut evt = JournalEvent::base(JournalEventType::LlmRound, self.session_id.as_deref());
        evt.turn = Some(self.turn);
        evt.agentic_step = r.agentic_step;
        evt.round = Some(self.round);
        evt.offset_ms = Some(self.offset_ms().saturating_sub(r.duration_ms));
        evt.ttft_ms = r.ttft_ms;
        evt.duration_ms = Some(r.duration_ms);
        evt.tokens_in = Some(r.prompt_tokens);
        evt.tokens_out = Some(r.completion_tokens);
        if r.cache_read_tokens > 0 {
            evt.cache_read_tokens = Some(r.cache_read_tokens);
        }
        evt.tool_calls_returned = Some(r.tool_calls_returned);
        if let Some(tool_calls) = r.tool_calls {
            evt = evt.with_tool_calls(tool_calls);
        }
        if !r.tool_call_names.is_empty()
            || r.finish_reason.is_some()
            || r.source.is_some()
            || r.run_id.is_some()
        {
            let mut meta = serde_json::Map::new();
            meta.insert(
                "tool_call_names".into(),
                serde_json::json!(r.tool_call_names),
            );
            meta.insert("finish_reason".into(), serde_json::json!(r.finish_reason));
            if let Some(source) = r.source {
                meta.insert("source".into(), serde_json::json!(source));
            }
            if let Some(run_id) = r.run_id {
                meta.insert("run_id".into(), serde_json::json!(run_id));
            }
            evt.metadata = Some(serde_json::Value::Object(meta));
        }
        self.events.push(evt);
        self.round += 1;
        self.batch_counter = 0;
    }

    /// Record a single event (generic).
    pub fn record(&mut self, event: JournalEvent) {
        self.events.push(event);
    }

    /// Number of events collected so far.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no events have been collected.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Flush all collected events to the journal (one IO, with fsync).
    pub fn flush(&mut self, writer: &JournalWriter) -> std::io::Result<()> {
        if self.events.is_empty() {
            return Ok(());
        }
        writer.append_bulk(&self.events)?;
        self.events.clear();
        Ok(())
    }

    /// Best-effort flush on interruption: no fsync, marks events as partial.
    pub fn flush_interrupted(&mut self, writer: &JournalWriter) -> std::io::Result<()> {
        if self.events.is_empty() {
            return Ok(());
        }
        for event in &mut self.events {
            let meta = event.metadata.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = meta.as_object_mut() {
                obj.insert("partial".into(), serde_json::json!(true));
            }
        }
        writer.append_bulk_no_sync(&self.events)?;
        self.events.clear();
        Ok(())
    }

    /// Drain collected events (for callers that persist elsewhere, e.g. DB).
    pub fn drain(&mut self) -> Vec<JournalEvent> {
        std::mem::take(&mut self.events)
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

fn approval_metadata_str(metadata: &serde_json::Value, field: &str) -> Option<String> {
    metadata
        .get("approval")
        .and_then(|approval| approval.get(field))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
}

pub fn find_latest_approval_decision(
    session_id: &str,
    request_id: &str,
) -> std::io::Result<Option<ApprovalJournalDecision>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let events = read_journal(session_id)?;
    for event in events.into_iter().rev() {
        if event.event_type != JournalEventType::ApprovalDecision {
            continue;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        let Some(found_request_id) = approval_metadata_str(metadata, "request_id") else {
            continue;
        };
        if found_request_id != request_id {
            continue;
        }
        let Some(decision) = approval_metadata_str(metadata, "decision") else {
            continue;
        };
        return Ok(Some(ApprovalJournalDecision {
            request_id: found_request_id,
            decision,
            reason: approval_metadata_str(metadata, "reason"),
            tool_name: approval_metadata_str(metadata, "tool_name"),
            approval_kind: approval_metadata_str(metadata, "approval_kind"),
        }));
    }
    Ok(None)
}

pub fn find_latest_approval_required(
    session_id: &str,
    request_id: &str,
) -> std::io::Result<Option<ApprovalJournalRequest>> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let events = read_journal(session_id)?;
    for event in events.into_iter().rev() {
        if event.event_type != JournalEventType::ApprovalRequired {
            continue;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            continue;
        };
        let Some(found_request_id) = approval_metadata_str(metadata, "request_id") else {
            continue;
        };
        if found_request_id != request_id {
            continue;
        }
        return Ok(Some(ApprovalJournalRequest {
            request_id: found_request_id,
            turn: event.turn,
            tool_name: approval_metadata_str(metadata, "tool_name"),
            approval_kind: approval_metadata_str(metadata, "approval_kind"),
        }));
    }
    Ok(None)
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
    items.sort_by_key(|b| std::cmp::Reverse(b.0)); // newest first by mtime
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
/// Designed for fast session listing via partial journal reads.
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

const RECOVERY_TAIL_LINE_LIMIT: usize = 32;
const RECOVERY_TAIL_CHUNK_BYTES: usize = 4096;
const RECOVERY_TAIL_MAX_BYTES: usize = 64 * 1024;

/// Lightweight terminal state for crash-recovery decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndState {
    /// The last recoverability marker in the latest session segment was `session_end`.
    Completed,
    /// The session stopped with a structured interruption record.
    Interrupted { kind: String, resumable: bool },
    /// The session had activity after the latest `session_start` but never ended cleanly.
    Zombie,
}

impl SessionEndState {
    #[must_use]
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Zombie
                | Self::Interrupted {
                    resumable: true,
                    ..
                }
        )
    }
}

#[derive(Debug, Deserialize)]
struct JournalTailEntry {
    #[serde(rename = "type")]
    event_type: JournalEventType,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

fn read_journal_tail_lines(path: &Path, max_lines: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek};

    if max_lines == 0 {
        return Ok(Vec::new());
    }

    let mut file = std::fs::File::open(path)?;
    let mut pos = file.seek(std::io::SeekFrom::End(0))?;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut bytes_read = 0usize;
    let mut newline_count = 0usize;

    while pos > 0 && newline_count <= max_lines && bytes_read < RECOVERY_TAIL_MAX_BYTES {
        let read_len = usize::min(RECOVERY_TAIL_CHUNK_BYTES, pos as usize);
        pos -= read_len as u64;
        file.seek(std::io::SeekFrom::Start(pos))?;
        let mut chunk = vec![0; read_len];
        file.read_exact(&mut chunk)?;
        newline_count += chunk.iter().filter(|&&b| b == b'\n').count();
        bytes_read += read_len;
        chunks.push(chunk);
    }

    chunks.reverse();
    let mut bytes = Vec::with_capacity(bytes_read);
    for chunk in chunks {
        bytes.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(max_lines)
        .map(ToString::to_string)
        .collect();
    lines.reverse();
    Ok(lines)
}

fn parse_journal_tail_entry(line: &str) -> Option<JournalTailEntry> {
    serde_json::from_str::<JournalTailEntry>(line).ok()
}

fn interruption_kind(entry: &JournalTailEntry) -> String {
    entry
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("interruption"))
        .and_then(|value| value.get("kind"))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn interruption_is_resumable(entry: &JournalTailEntry) -> bool {
    let interruption = entry
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("interruption"));

    if let Some(resumable) = interruption
        .and_then(|value| value.get("resumable"))
        .and_then(|value| value.as_bool())
    {
        return resumable;
    }

    match interruption.and_then(|value| value.get("resume_action")) {
        Some(serde_json::Value::String(action)) => !matches!(
            action.as_str(),
            "start_new_session" | "requires_intervention"
        ),
        Some(serde_json::Value::Object(action)) => {
            action.contains_key("continue_immediately")
                || action.contains_key("wait_and_retry")
                || action.contains_key("compact_and_retry")
        }
        _ => true,
    }
}

fn is_recovery_activity_event(event_type: &JournalEventType) -> bool {
    !matches!(
        event_type,
        JournalEventType::SessionStart
            | JournalEventType::SessionEnd
            | JournalEventType::ConfigChange
            | JournalEventType::SyncMarker
            | JournalEventType::ContextAssemblyRecorded
            | JournalEventType::AdaptiveBaselinePromoted
            | JournalEventType::AdaptiveScenarioApplied
            | JournalEventType::AdaptivePerTurnApplied
            | JournalEventType::AdaptiveExperimentEnrolled
            | JournalEventType::AdaptiveTuningRuleTriggered
            | JournalEventType::ConfidenceDiagnosisRecorded
            | JournalEventType::CompactionRetry
    )
}

/// Classify the latest session segment as completed, interrupted, or zombie.
///
/// Uses a bounded reverse tail read instead of loading the full JSONL file.
pub fn classify_session_end_state(session_id: &str) -> std::io::Result<SessionEndState> {
    validate_session_id(session_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let path = journal_file_path(session_id);
    if !path.exists() {
        return Ok(SessionEndState::Completed);
    }

    let tail_lines = read_journal_tail_lines(&path, RECOVERY_TAIL_LINE_LIMIT)?;
    let mut saw_activity_after_start = false;

    for line in tail_lines.iter().rev() {
        let Some(entry) = parse_journal_tail_entry(line) else {
            continue;
        };
        match entry.event_type {
            JournalEventType::SessionEnd => return Ok(SessionEndState::Completed),
            JournalEventType::InterruptionRecorded => {
                return Ok(SessionEndState::Interrupted {
                    kind: interruption_kind(&entry),
                    resumable: interruption_is_resumable(&entry),
                });
            }
            JournalEventType::SessionStart => {
                return Ok(if saw_activity_after_start {
                    SessionEndState::Zombie
                } else {
                    SessionEndState::Completed
                });
            }
            _ if is_recovery_activity_event(&entry.event_type) => {
                saw_activity_after_start = true;
            }
            _ => {}
        }
    }

    Ok(if saw_activity_after_start {
        SessionEndState::Zombie
    } else {
        SessionEndState::Completed
    })
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

// ── Session listing with metadata ────────────────────────────────────────────

/// Metadata for session listing (mtime, size, staleness).
#[derive(Debug, Clone)]
pub struct SessionListMeta {
    pub session_id: String,
    /// Journal file modification time.
    pub last_modified: std::time::SystemTime,
    /// Journal file size in bytes.
    pub journal_bytes: u64,
    /// Total disk usage: journal + workspace dir (recursive).
    pub total_bytes: u64,
    /// Turn count (fast count).
    pub turns: u32,
}

impl SessionListMeta {
    /// Check if this session is stale (older than `max_age`).
    pub fn is_stale(&self, max_age: std::time::Duration) -> bool {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        self.last_modified < cutoff
    }

    /// Get the age of this session (time since last modified).
    pub fn age(&self) -> std::time::Duration {
        self.last_modified
            .elapsed()
            .unwrap_or(std::time::Duration::ZERO)
    }
}

/// List sessions with metadata, sorted by most recent first.
///
/// `limit` — max number of sessions to return.
/// Returns session IDs with mtime and size info for display purposes.
pub fn list_sessions_with_meta(limit: usize) -> std::io::Result<Vec<SessionListMeta>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    // Min-heap of (mtime, sid) — keeps only the `limit` newest entries
    let mut heap: BinaryHeap<Reverse<(std::time::SystemTime, String, u64)>> = BinaryHeap::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sid) = name.strip_suffix(".jsonl") {
            // Skip test-generated sessions
            if sid.starts_with("test-") || sid.starts_with("new-sess-") {
                continue;
            }
            let meta = entry.metadata().ok();
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let size = meta.map(|m| m.len()).unwrap_or(0);
            if heap.len() < limit {
                heap.push(Reverse((mtime, sid.to_string(), size)));
            } else if let Some(&Reverse((min_time, _, _))) = heap.peek()
                && mtime > min_time
            {
                heap.pop();
                heap.push(Reverse((mtime, sid.to_string(), size)));
            }
        }
    }

    // Extract and sort by newest first
    let mut items: Vec<_> = heap.into_iter().map(|Reverse(item)| item).collect();
    items.sort_by_key(|b| std::cmp::Reverse(b.0));

    // Enrich with total size and turn count (done after limit for efficiency)
    let result: Vec<SessionListMeta> = items
        .into_iter()
        .map(|(mtime, sid, journal_bytes)| {
            let ws_dir = crate::session_workspace::workspace_dir_for(&sid);
            let ws_bytes = dir_size_recursive(&ws_dir);
            let turns = count_turns(&sid);
            SessionListMeta {
                session_id: sid,
                last_modified: mtime,
                journal_bytes,
                total_bytes: journal_bytes + ws_bytes,
                turns,
            }
        })
        .collect();

    Ok(result)
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
///
/// Safeguards:
/// - Maximum depth of 10 levels to prevent deep traversal
/// - Maximum 1000 entries per call to prevent hangs on huge directories
fn dir_size_recursive(path: &std::path::Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    walkdir_bounded(path, 0)
}

/// Max depth for recursive directory traversal (10 levels should cover most workspaces).
const MAX_WALKDIR_DEPTH: u32 = 10;
/// Max entries to process per directory (prevents hangs on huge flat directories).
const MAX_ENTRIES_PER_DIR: usize = 1000;

fn walkdir_bounded(path: &std::path::Path, depth: u32) -> u64 {
    if depth > MAX_WALKDIR_DEPTH {
        return 0; // Stop at max depth
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.take(MAX_ENTRIES_PER_DIR).flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += walkdir_bounded(&entry.path(), depth + 1);
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
    let out_file = encoder.finish()?;
    // Ensure compressed data is durable before deleting the original.
    out_file.sync_all()?;
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
    result.sort_by_key(|b| std::cmp::Reverse(b.1)); // largest first
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
    crate::local_session_artifact_store().sessions_root()
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
            parent_event_id: None,
            git_head: None,
            git_branch: None,
        }
    }

    /// Create a minimal event with just event type and session ID.
    /// Public variant of `base()` for use by external crates.
    pub fn base_public(event_type: JournalEventType, session_id: Option<&str>) -> Self {
        Self::base(event_type, session_id)
    }

    pub fn with_agentic_step(mut self, agentic_step: Option<u32>) -> Self {
        self.agentic_step = agentic_step;
        self
    }

    /// Set the parent event ID for causal lineage.
    pub fn with_parent_event_id(mut self, parent_event_id: Option<String>) -> Self {
        self.parent_event_id = parent_event_id;
        self
    }

    /// Attach git snapshot (HEAD commit + branch) to this event.
    pub fn with_git_snapshot(mut self, head: Option<String>, branch: Option<String>) -> Self {
        self.git_head = head;
        self.git_branch = branch;
        self
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

    pub fn approval_required(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        tool_name: &str,
        approval_kind: &str,
        detail: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ApprovalRequired, session_id);
        evt.turn = turn;
        evt.user_input = Some(truncate(
            &format!("approval_required {tool_name} {request_id}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "approval": {
                "request_id": request_id,
                "tool_name": tool_name,
                "approval_kind": approval_kind,
                "detail": detail.filter(|s| !s.is_empty()),
            }
        }));
        evt
    }

    pub fn approval_decision(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        tool_name: Option<&str>,
        approval_kind: Option<&str>,
        decision: &str,
        reason: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ApprovalDecision, session_id);
        evt.turn = turn;
        let summary_tool = tool_name.filter(|s| !s.is_empty()).unwrap_or("unknown");
        evt.user_input = Some(truncate(
            &format!("approval_decision {summary_tool} {request_id} {decision}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "approval": {
                "request_id": request_id,
                "tool_name": tool_name.filter(|s| !s.is_empty()),
                "approval_kind": approval_kind.filter(|s| !s.is_empty()),
                "decision": decision,
                "reason": reason.filter(|s| !s.is_empty()),
            }
        }));
        evt
    }

    pub fn approval_timeout(
        session_id: Option<&str>,
        turn: Option<u32>,
        request_id: &str,
        tool_name: &str,
        approval_kind: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ApprovalTimeout, session_id);
        evt.turn = turn;
        evt.error = Some(truncate(
            &format!("approval timeout for {tool_name} ({request_id})"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "approval": {
                "request_id": request_id,
                "tool_name": tool_name,
                "approval_kind": approval_kind,
            }
        }));
        evt
    }

    pub fn execution_boundary_opened(
        session_id: Option<&str>,
        turn: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        checkpoints: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ExecutionBoundaryOpened, session_id);
        evt.turn = Some(turn);
        let tx_label = transaction_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-");
        evt.user_input = Some(truncate(
            &format!("execution_boundary_opened {boundary_kind} {tx_label}"),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "execution_boundary": {
                "kind": boundary_kind,
                "transaction_id": normalize_optional_str(transaction_id),
                "rollback_on_failure": true,
                "checkpoints": checkpoints,
            }
        }));
        evt
    }

    pub fn execution_boundary_committed(
        session_id: Option<&str>,
        turn: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        detail: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ExecutionBoundaryCommitted, session_id);
        evt.turn = Some(turn);
        let tx_label = transaction_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-");
        evt.user_input = Some(truncate(
            &format!("execution_boundary_committed {boundary_kind} {tx_label}"),
            200,
        ));
        let mut boundary = serde_json::Map::from_iter([
            (
                "kind".to_string(),
                serde_json::Value::String(boundary_kind.to_string()),
            ),
            (
                "transaction_id".to_string(),
                normalize_optional_str(transaction_id)
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            ),
            (
                "rollback_on_failure".to_string(),
                serde_json::Value::Bool(true),
            ),
        ]);
        if let Some(detail) = detail {
            boundary.insert("detail".to_string(), detail);
        }
        evt.metadata = Some(serde_json::json!({
            "execution_boundary": serde_json::Value::Object(boundary),
        }));
        evt
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execution_boundary_aborted(
        session_id: Option<&str>,
        turn: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        reason: &str,
        trigger_tool_name: Option<&str>,
        trigger_request_id: Option<&str>,
        rollback: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ExecutionBoundaryAborted, session_id);
        evt.turn = Some(turn);
        let tx_label = transaction_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-");
        evt.error = Some(truncate(
            &format!("execution boundary aborted: {boundary_kind} {tx_label}"),
            200,
        ));
        let mut boundary = serde_json::Map::from_iter([
            (
                "kind".to_string(),
                serde_json::Value::String(boundary_kind.to_string()),
            ),
            (
                "transaction_id".to_string(),
                normalize_optional_str(transaction_id)
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            ),
            (
                "rollback_on_failure".to_string(),
                serde_json::Value::Bool(true),
            ),
            (
                "reason".to_string(),
                serde_json::Value::String(truncate(reason, 500)),
            ),
        ]);
        if let Some(trigger_tool_name) = normalize_optional_str(trigger_tool_name) {
            boundary.insert(
                "trigger_tool_name".to_string(),
                serde_json::Value::String(trigger_tool_name),
            );
        }
        if let Some(trigger_request_id) = normalize_optional_str(trigger_request_id) {
            boundary.insert(
                "trigger_request_id".to_string(),
                serde_json::Value::String(trigger_request_id),
            );
        }
        if let Some(rollback) = rollback {
            boundary.insert("rollback".to_string(), rollback);
        }
        evt.metadata = Some(serde_json::json!({
            "execution_boundary": serde_json::Value::Object(boundary),
        }));
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
        if journal_content_redact_enabled() {
            evt.user_input = Some(journal_content_marker(user_input));
            evt.assistant_output = Some(journal_content_marker(assistant_output));
        } else {
            evt.user_input = Some(truncate(user_input, 500));
            evt.assistant_output = Some(truncate(assistant_output, 10000));
        }
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
        if journal_content_redact_enabled() {
            evt.user_input = Some(journal_content_marker(user_input));
        } else {
            evt.user_input = Some(truncate(user_input, 500));
        }
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

    /// Compact event with an optional LLM-generated summary attached in metadata.
    pub fn compact_with_summary(
        session_id: Option<&str>,
        turn: u32,
        turns_compacted: usize,
        facts_stored: usize,
        summary: Option<&str>,
    ) -> Self {
        let mut evt = Self::compact(session_id, turn, turns_compacted, facts_stored);
        if let Some(s) = summary
            && !s.is_empty()
        {
            evt.metadata = Some(serde_json::json!({ "compact_summary": s }));
        }
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
    fn turn_guard_avoid_reason_codes(
        avoid_tools: &[String],
        deprioritized_tools: &[String],
        timeout_dominant_tools: &[String],
        nudge_count: usize,
        non_timeout_errors: usize,
    ) -> Vec<&'static str> {
        let mut codes = Vec::new();
        if !avoid_tools.is_empty() && !deprioritized_tools.is_empty() {
            codes.push("tool_health_deprioritized");
        }
        if non_timeout_errors > 0 {
            codes.push("session_failures");
        }
        if !timeout_dominant_tools.is_empty() {
            codes.push("timeout_dominant");
        }
        if nudge_count > 0 {
            codes.push("stall_recovery");
        }
        codes
    }

    fn turn_guard_avoid_reason_summary(
        deprioritized_tools: &[String],
        timeout_dominant_tools: &[String],
        nudge_count: usize,
        non_timeout_errors: usize,
        total_timeouts: usize,
    ) -> Option<String> {
        let mut parts = Vec::new();
        if !deprioritized_tools.is_empty() {
            parts.push(format!(
                "deprioritized by tool health: {}",
                deprioritized_tools.join(", ")
            ));
        }
        if non_timeout_errors > 0 {
            parts.push(format!(
                "{non_timeout_errors} non-timeout failure(s) recorded"
            ));
        }
        if total_timeouts > 0 {
            if timeout_dominant_tools.is_empty() {
                parts.push(format!("{total_timeouts} timeout failure(s) recorded"));
            } else {
                parts.push(format!(
                    "timeout-dominant tools: {}",
                    timeout_dominant_tools.join(", ")
                ));
            }
        }
        if nudge_count > 0 {
            parts.push(format!("{nudge_count} stall/divergence nudge(s) issued"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn turn_guard_verdict(
        session_id: Option<&str>,
        turn: u32,
        severity: &str,
        injections: &[String],
        avoid_tools: &[String],
        deprioritized_tools: &[String],
        force_stop: bool,
        nudge_count: usize,
        total_errors: usize,
        deprioritized_count: usize,
        total_timeouts: usize,
        timeout_dominant_tools: &[String],
        total_cache_hits: usize,
        flaky_count: usize,
    ) -> Self {
        let non_timeout_errors = total_errors.saturating_sub(total_timeouts);
        let avoid_reason_codes = Self::turn_guard_avoid_reason_codes(
            avoid_tools,
            deprioritized_tools,
            timeout_dominant_tools,
            nudge_count,
            non_timeout_errors,
        );
        let avoid_reason_summary = Self::turn_guard_avoid_reason_summary(
            deprioritized_tools,
            timeout_dominant_tools,
            nudge_count,
            non_timeout_errors,
            total_timeouts,
        );
        let mut evt = Self::base(JournalEventType::TurnGuardVerdict, session_id);
        evt.turn = Some(turn);
        evt.stall_type = Some(severity.to_string());
        evt.metadata = Some(serde_json::json!({
            "severity": severity,
            "injections": injections.len(),
            "injection_preview": injections.first().map(|s| truncate(s, 200)),
            "avoid_tools": avoid_tools,
            "avoid_tools_count": avoid_tools.len(),
            "deprioritized_tool_names": deprioritized_tools,
            "timeout_dominant_tools": timeout_dominant_tools,
            "avoid_reason_codes": avoid_reason_codes,
            "avoid_reason_summary": avoid_reason_summary,
            "force_stop": force_stop,
            "nudge_count": nudge_count,
            "total_errors": total_errors,
            "non_timeout_errors": non_timeout_errors,
            "deprioritized_tools": deprioritized_count,
            "total_timeouts": total_timeouts,
            "total_cache_hits": total_cache_hits,
            "flaky_tools": flaky_count,
        }));
        evt
    }

    #[allow(clippy::too_many_arguments)]
    pub fn turn_evaluation(
        session_id: Option<&str>,
        turn: Option<u32>,
        source: &str,
        live_query: bool,
        success: bool,
        quality: f64,
        confidence: f64,
        budget_pressure: f64,
        stall_count: usize,
        verdict_warning: bool,
        tool_call_count: usize,
        signals: Vec<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::TurnEvaluation, session_id);
        evt.turn = turn;
        evt.metadata = Some(serde_json::json!({
            "source": source,
            "live_query": live_query,
            "success": success,
            "quality": quality,
            "confidence": confidence,
            "budget_pressure": budget_pressure,
            "stall_count": stall_count,
            "verdict_warning": verdict_warning,
            "tool_call_count": tool_call_count,
            "signal_count": signals.len(),
            "signals": signals,
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

    /// Plan edited — subtask added/removed/reordered, goal changed.
    pub fn plan_edit(
        session_id: Option<&str>,
        action: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PlanEdit, session_id);
        evt.metadata = Some(serde_json::json!({
            "action": action,
            "detail": metadata,
        }));
        evt
    }

    /// Plan lifecycle event — created, completed, abandoned, replanned.
    pub fn plan_lifecycle(
        session_id: Option<&str>,
        summary: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::PlanLifecycle, session_id);
        evt.metadata = Some(serde_json::json!({
            "summary": summary,
            "detail": metadata,
        }));
        evt
    }

    /// Goal steering event — manual goal set or plan-goal alignment took over.
    pub fn goal_steered(
        session_id: Option<&str>,
        turn: u32,
        source: &str,
        previous_goal: Option<&str>,
        new_goal: &str,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::GoalSteered, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "source": source,
            "previous_goal": previous_goal,
            "new_goal": new_goal,
            "detail": metadata,
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

    /// Delegation sub-run started event — emitted when a single sub-run enters running state.
    #[allow(clippy::too_many_arguments)]
    pub fn delegation_sub_run_started(
        session_id: Option<&str>,
        delegation_id: &str,
        sub_run_id: &str,
        parent_run_id: &str,
        agent_id: &str,
        status: &str,
        depth: u32,
        retry_of: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationSubRunStarted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "sub_run_id": sub_run_id,
            "parent_run_id": parent_run_id,
            "agent_id": agent_id,
            "status": status,
            "depth": depth,
            "retry_of": retry_of,
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
        output_preview: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationSubRunCompleted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "sub_run_id": sub_run_id,
            "agent_id": agent_id,
            "status": status,
            "error": error.map(|msg| truncate(msg, 500)),
            "output_preview": output_preview.map(|msg| truncate(msg, 500)),
        }));
        evt
    }

    /// Delegation retry event - emitted when a verification-gated sub-run spawns a retry.
    pub fn delegation_retry(
        session_id: Option<&str>,
        delegation_id: &str,
        original_run_id: &str,
        retry_run_id: &str,
        agent_id: &str,
        attempt: u32,
        reason: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationRetry, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "original_run_id": original_run_id,
            "retry_run_id": retry_run_id,
            "agent_id": agent_id,
            "attempt": attempt,
            "reason": reason,
        }));
        evt
    }

    /// Delegation completed event — emitted when all sub-runs finish and results aggregate.
    #[allow(clippy::too_many_arguments)]
    pub fn delegation_completed(
        session_id: Option<&str>,
        delegation_id: &str,
        pattern: &str,
        total_sub_runs: usize,
        succeeded: usize,
        failed: usize,
        aggregated_status: &str,
        aggregated_output_preview: Option<&str>,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DelegationCompleted, session_id);
        evt.metadata = Some(serde_json::json!({
            "delegation_id": delegation_id,
            "pattern": pattern,
            "total_sub_runs": total_sub_runs,
            "succeeded": succeeded,
            "failed": failed,
            "aggregated_status": aggregated_status,
            "aggregated_output_preview": aggregated_output_preview.map(|msg| truncate(msg, 500)),
        }));
        evt
    }

    /// Adaptive baseline promoted event — emitted when a completed experiment winner
    /// is promoted into a durable baseline.
    pub fn adaptive_baseline_promoted(
        session_id: Option<&str>,
        task_type: &str,
        domain: Option<&str>,
        experiment_id: &str,
        variant_id: &str,
        replaced_existing: bool,
        config_keys: &[String],
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptiveBaselinePromoted, session_id);
        evt.metadata = Some(serde_json::json!({
            "task_type": task_type,
            "domain": domain,
            "experiment_id": experiment_id,
            "variant_id": variant_id,
            "replaced_existing": replaced_existing,
            "config_keys": config_keys,
        }));
        evt
    }

    /// Agent terminated event — persists final state of a spawned agent.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_terminated(
        session_id: Option<&str>,
        agent_id: &str,
        run_id: &str,
        agent_type: &str,
        status: &str,
        turns_completed: u32,
        tool_calls: u32,
        prompt_tokens: u64,
        completion_tokens: u64,
        duration_ms: u64,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AgentTerminated, session_id);
        evt.metadata = Some(serde_json::json!({
            "agent_id": agent_id,
            "run_id": run_id,
            "agent_type": agent_type,
            "status": status,
            "turns_completed": turns_completed,
            "tool_calls": tool_calls,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "duration_ms": duration_ms,
        }));
        evt
    }

    /// Context assembly recorded event — deep observability for turn context composition.
    ///
    /// The `trace` should be a serialized `ContextAssemblyTrace` from runtime.
    /// Stores full context breakdown: system prompt, history, memory, tools, token budget.
    pub fn context_assembly_recorded(
        session_id: Option<&str>,
        turn: u32,
        trace: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ContextAssemblyRecorded, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "trace_recorded": true,
            "trace_kind": "context_assembly",
            "turn_id": trace.get("turn_id").and_then(|value| value.as_str()),
            "tool_count": trace
                .get("tools")
                .and_then(|tools| tools.get("tools_selected"))
                .and_then(|selected| selected.as_array())
                .map(Vec::len),
            "total_tokens": trace
                .get("token_budget")
                .and_then(|budget| budget.get("total_used"))
                .and_then(|value| value.as_u64()),
        }));
        evt.context_assembly_trace = Some(trace);
        evt
    }

    /// Full LLM request payload recorded in the session journal.
    pub fn llm_request_full(
        session_id: Option<&str>,
        turn: u32,
        round: u32,
        metadata: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::LlmRequestFull, session_id);
        evt.turn = Some(turn);
        evt.round = Some(round);
        evt.metadata = Some(metadata);
        evt
    }

    /// Full LLM response payload recorded in the session journal.
    pub fn llm_response_full(
        session_id: Option<&str>,
        turn: u32,
        round: u32,
        metadata: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::LlmResponseFull, session_id);
        evt.turn = Some(turn);
        evt.round = Some(round);
        evt.metadata = Some(metadata);
        evt
    }

    /// Builder to attach a context assembly trace to an existing turn event.
    pub fn with_context_assembly_trace(mut self, trace: serde_json::Value) -> Self {
        self.context_assembly_trace = Some(trace);
        self
    }

    /// Focus drift detected — emitted when drift analysis finds significant drift.
    pub fn drift_detected(
        session_id: Option<&str>,
        turn: u32,
        severity: f64,
        cause: astra_core::DriftCause,
        evidence: Vec<astra_core::DriftEvidence>,
        recovery_suggestion: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::DriftDetected, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "severity": severity,
            "cause": cause,
            "evidence_count": evidence.len(),
            "evidence": evidence,
            "recovery_suggestion": recovery_suggestion,
        }));
        evt
    }

    /// Adaptive scenario applied — emitted once per session when the adaptive
    /// profile selects a scenario and applies config adjustments.
    #[allow(clippy::too_many_arguments)]
    pub fn adaptive_scenario_applied(
        session_id: Option<&str>,
        turn: u32,
        scenario: &str,
        confidence: f64,
        config_changes: Vec<(String, String, String)>, // (key, from, to)
        experiment_id: Option<&str>,
        variant_id: Option<&str>,
        baseline_applied: bool,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptiveScenarioApplied, session_id);
        evt.turn = Some(turn);
        let changes: Vec<serde_json::Value> = config_changes
            .iter()
            .map(|(k, from, to)| serde_json::json!({"key": k, "from": from, "to": to}))
            .collect();
        evt.metadata = Some(serde_json::json!({
            "scenario": scenario,
            "confidence": confidence,
            "config_changes": changes,
            "experiment_id": experiment_id,
            "variant_id": variant_id,
            "baseline_applied": baseline_applied,
        }));
        evt
    }

    /// Per-turn micro-adaptation applied — emitted when per-turn adaptation
    /// modifies runtime config based on immediate signals.
    pub fn adaptive_per_turn_applied(
        session_id: Option<&str>,
        turn: u32,
        changes: Vec<(String, String, String)>, // (key, from, to)
        triggers: Vec<String>,                  // reason strings
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptivePerTurnApplied, session_id);
        evt.turn = Some(turn);
        let change_vals: Vec<serde_json::Value> = changes
            .iter()
            .map(|(k, from, to)| serde_json::json!({"key": k, "from": from, "to": to}))
            .collect();
        evt.metadata = Some(serde_json::json!({
            "changes": change_vals,
            "triggers": triggers,
        }));
        evt
    }

    /// Experiment enrollment — session assigned to a variant.
    pub fn adaptive_experiment_enrolled(
        session_id: Option<&str>,
        turn: u32,
        experiment_id: &str,
        variant_id: &str,
        experiment_name: &str,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptiveExperimentEnrolled, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "experiment_id": experiment_id,
            "variant_id": variant_id,
            "experiment_name": experiment_name,
        }));
        evt
    }

    /// Tuning rule triggered — emitted when an evolution rule fires and
    /// modifies runtime config.
    pub fn adaptive_tuning_rule_triggered(
        session_id: Option<&str>,
        turn: u32,
        rule_id: &str,
        rule_name: &str,
        signal_type: &str,
        config_changes: Vec<(String, String, String)>, // (key, from, to)
    ) -> Self {
        let mut evt = Self::base(JournalEventType::AdaptiveTuningRuleTriggered, session_id);
        evt.turn = Some(turn);
        let changes: Vec<serde_json::Value> = config_changes
            .iter()
            .map(|(k, from, to)| serde_json::json!({"key": k, "from": from, "to": to}))
            .collect();
        evt.metadata = Some(serde_json::json!({
            "rule_id": rule_id,
            "rule_name": rule_name,
            "signal_type": signal_type,
            "config_changes": changes,
        }));
        evt
    }

    /// Record a structured interruption (budget exhaustion, rate limit, cancel, etc.).
    pub fn interruption_recorded(
        session_id: Option<&str>,
        turn: u32,
        interruption_json: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::InterruptionRecorded, session_id);
        evt.turn = Some(turn);
        let kind_str = interruption_json
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let resumable = interruption_json
            .get("resumable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        evt.user_input = Some(truncate(
            &format!("interruption: {} (resumable={})", kind_str, resumable,),
            200,
        ));
        evt.metadata = Some(serde_json::json!({
            "interruption": interruption_json,
        }));
        evt
    }

    /// Record a selector confidence diagnosis (emitted only for actionable tiers).
    pub fn confidence_diagnosis_recorded(
        session_id: Option<&str>,
        turn: u32,
        confidence: f64,
        diagnosis_json: serde_json::Value,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::ConfidenceDiagnosisRecorded, session_id);
        evt.turn = Some(turn);
        evt.selector_confidence = Some(confidence);
        evt.metadata = Some(serde_json::json!({
            "confidence_diagnosis": diagnosis_json,
        }));
        evt
    }

    /// Build a compaction retry telemetry event.
    ///
    /// Emitted after a successful compaction retry to capture operational metrics:
    /// tier escalation, tokens freed, budget satisfaction, and per-layer breakdown.
    #[allow(clippy::too_many_arguments)]
    pub fn compaction_retry(
        session_id: Option<&str>,
        turn: u32,
        tier: &str,
        tokens_freed: u64,
        budget_likely_satisfied: bool,
        retry_count: u32,
        layers: Vec<(String, u64)>,
        consecutive_context_window_errors: u32,
    ) -> Self {
        let mut evt = Self::base(JournalEventType::CompactionRetry, session_id);
        evt.turn = Some(turn);
        evt.metadata = Some(serde_json::json!({
            "compaction": {
                "tier": tier,
                "tokens_freed": tokens_freed,
                "budget_likely_satisfied": budget_likely_satisfied,
                "retry_count": retry_count,
                "consecutive_context_window_errors": consecutive_context_window_errors,
                "layers": layers.iter().map(|(name, freed)| {
                    serde_json::json!({ "name": name, "tokens_freed": freed })
                }).collect::<Vec<_>>(),
            }
        }));
        evt
    }
}
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

pub const ASTRA_JOURNAL_CONTENT_REDACT_ENV: &str = "ASTRA_JOURNAL_CONTENT_REDACT";

/// Returns true when [`ASTRA_JOURNAL_CONTENT_REDACT_ENV`]=`1` is set in the
/// environment. When enabled, the on-disk JSONL journal stores a privacy
/// marker (`<redacted: len=N sha=...>`) in place of `user_input` and
/// `assistant_output` fields.
pub fn journal_content_redact_enabled() -> bool {
    std::env::var(ASTRA_JOURNAL_CONTENT_REDACT_ENV).as_deref() == Ok("1")
}

/// Replace raw user content with a deterministic privacy marker.
///
/// Uses a non-cryptographic 64-bit hash for dedup/debugging only — not as
/// a security primitive.
pub fn journal_content_marker(raw: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("<redacted: len={} sha={:016x}>", raw.len(), h.finish())
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

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[test]
    fn find_latest_approval_decision_reads_latest_matching_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();

        writer
            .append(&JournalEvent::approval_decision(
                Some("sess-approval"),
                Some(7),
                "req-1",
                Some("write_file"),
                Some("standard"),
                "allow",
                None,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::approval_decision(
                Some("sess-approval"),
                Some(9),
                "req-2",
                Some("bash"),
                Some("explicit"),
                "deny",
                Some("too dangerous"),
            ))
            .unwrap();

        let found = find_latest_approval_decision("sess-approval", "req-2")
            .unwrap()
            .expect("approval decision");
        assert_eq!(found.request_id, "req-2");
        assert_eq!(found.decision, "deny");
        assert_eq!(found.reason.as_deref(), Some("too dangerous"));
        assert_eq!(found.tool_name.as_deref(), Some("bash"));
        assert_eq!(found.approval_kind.as_deref(), Some("explicit"));
    }

    #[test]
    fn find_latest_approval_decision_ignores_non_matching_events() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();

        writer
            .append(&JournalEvent::approval_required(
                Some("sess-approval"),
                Some(4),
                "req-1",
                "write_file",
                "standard",
                Some("src/lib.rs"),
            ))
            .unwrap();

        let found = find_latest_approval_decision("sess-approval", "req-1").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn find_latest_approval_required_reads_matching_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-approval").unwrap();

        writer
            .append(&JournalEvent::approval_required(
                Some("sess-approval"),
                Some(11),
                "req-11",
                "bash",
                "explicit",
                Some("cargo test"),
            ))
            .unwrap();

        let found = find_latest_approval_required("sess-approval", "req-11")
            .unwrap()
            .expect("approval request");
        assert_eq!(found.request_id, "req-11");
        assert_eq!(found.turn, Some(11));
        assert_eq!(found.tool_name.as_deref(), Some("bash"));
        assert_eq!(found.approval_kind.as_deref(), Some("explicit"));
    }

    #[test]
    fn execution_boundary_events_round_trip() {
        let opened = JournalEvent::execution_boundary_opened(
            Some("sess-boundary"),
            7,
            "tool_batch",
            Some("tx-7"),
            serde_json::json!({
                "file_after_sequence": 3,
                "database_after_sequence": 1,
            }),
        );
        let committed = JournalEvent::execution_boundary_committed(
            Some("sess-boundary"),
            7,
            "tool_batch",
            Some("tx-7"),
            Some(serde_json::json!({
                "completed_request_id": "tr-2",
            })),
        );
        let aborted = JournalEvent::execution_boundary_aborted(
            Some("sess-boundary"),
            7,
            "turn_rollback",
            None,
            "tool failed",
            Some("write_file"),
            Some("tr-3"),
            Some(serde_json::json!({
                "summary": "Rolled back 1 file edit from turn 7",
            })),
        );

        let opened_json = serde_json::to_string(&opened).unwrap();
        let committed_json = serde_json::to_string(&committed).unwrap();
        let aborted_json = serde_json::to_string(&aborted).unwrap();

        let restored_opened: JournalEvent = serde_json::from_str(&opened_json).unwrap();
        let restored_committed: JournalEvent = serde_json::from_str(&committed_json).unwrap();
        let restored_aborted: JournalEvent = serde_json::from_str(&aborted_json).unwrap();

        assert_eq!(
            restored_opened.event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            restored_committed.event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );
        assert_eq!(
            restored_aborted.event_type,
            JournalEventType::ExecutionBoundaryAborted
        );
        assert_eq!(restored_opened.turn, Some(7));
        assert_eq!(
            restored_opened
                .metadata
                .as_ref()
                .and_then(|m| m.get("execution_boundary"))
                .and_then(|m| m.get("transaction_id"))
                .and_then(serde_json::Value::as_str),
            Some("tx-7")
        );
        assert_eq!(
            restored_aborted
                .metadata
                .as_ref()
                .and_then(|m| m.get("execution_boundary"))
                .and_then(|m| m.get("trigger_tool_name"))
                .and_then(serde_json::Value::as_str),
            Some("write_file")
        );
    }

    #[test]
    fn context_assembly_recorded_carries_metadata_summary() {
        let evt = JournalEvent::context_assembly_recorded(
            Some("sess"),
            3,
            serde_json::json!({
                "turn_id": "turn-3",
                "tools": {"tools_selected": [{"tool_name": "read_file"}]},
                "token_budget": {"total_used": 1234}
            }),
        );

        assert!(evt.context_assembly_trace.is_some());
        let metadata = evt.metadata.as_ref().expect("context metadata");
        assert_eq!(metadata["trace_recorded"], true);
        assert_eq!(metadata["turn_id"], "turn-3");
        assert_eq!(metadata["tool_count"], 1);
        assert_eq!(metadata["total_tokens"], 1234);
    }
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
    let out_file = encoder.finish()?;
    // Ensure compressed data is durable before deleting the original.
    out_file.sync_all()?;

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
    use astra_core::{DriftCause, DriftEvidence, EvidenceType};
    use tempfile::tempdir;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../fixtures/real_session_0ac769_min.jsonl");
    const REAL_SESSION_1D21375_FIXTURE: &str =
        include_str!("../fixtures/real_session_1d21375_min.jsonl");

    fn base_tool_record(name: &str, ok: bool, preview: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: preview.map(ToString::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn real_session_fixture_parses_with_expected_rounds_and_repeat_signals() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        std::fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            REAL_SESSION_0AC769_FIXTURE,
        )
        .unwrap();

        let (events, non_empty_lines, malformed_lines) = read_journal_for_digest(sid).unwrap();
        assert_eq!(non_empty_lines, 14);
        assert_eq!(malformed_lines, 0);
        assert_eq!(events.len(), 14);

        let llm_rounds: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::LlmRound)
            .collect();
        assert_eq!(
            llm_rounds.len(),
            7,
            "fixture should preserve the 7-round loop"
        );

        let turn = events
            .iter()
            .find(|event| event.event_type == JournalEventType::Turn)
            .expect("turn event");
        assert_eq!(
            turn.user_input.as_deref(),
            Some("review b273c589a73799070a71f4cfc6d55349b534d8d1")
        );
        assert!(
            turn.assistant_output
                .as_deref()
                .unwrap_or("")
                .contains("not b273c589"),
            "fixture should preserve the wrong-prefetch symptom"
        );

        let eval = events
            .iter()
            .find(|event| event.event_type == JournalEventType::TurnEvaluation)
            .expect("turn_evaluation event");
        let metadata = eval.metadata.as_ref().expect("turn evaluation metadata");
        assert_eq!(metadata["tool_call_count"], 12);
        assert_eq!(metadata["signal_count"], 4);
        assert_eq!(metadata["quality"], 0.5);
        assert_eq!(metadata["confidence"], 0.7);

        let signals = metadata["signals"].as_array().expect("signals array");
        let repeat_tools: std::collections::BTreeSet<_> = signals
            .iter()
            .filter(|signal| signal["kind"].as_str() == Some("repeat_tool_call"))
            .filter_map(|signal| signal["tool"].as_str())
            .collect();
        assert_eq!(
            repeat_tools,
            std::collections::BTreeSet::from(["git_show", "read_file"])
        );
    }

    #[test]
    fn real_session_followup_fixture_preserves_low_information_repair_pathology() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "1d21375d-18f5-4e53-9145-1fa197b564dd";
        std::fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            REAL_SESSION_1D21375_FIXTURE,
        )
        .unwrap();

        let (events, non_empty_lines, malformed_lines) = read_journal_for_digest(sid).unwrap();
        assert_eq!(non_empty_lines, 31);
        assert_eq!(malformed_lines, 0);
        assert_eq!(events.len(), 31);

        let llm_rounds = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::LlmRound)
            .count();
        assert_eq!(
            llm_rounds, 19,
            "fixture should preserve the 19-round session"
        );

        let turn = events
            .iter()
            .rev()
            .find(|event| event.event_type == JournalEventType::Turn)
            .expect("turn event");
        assert_eq!(turn.turn, Some(3));
        assert_eq!(turn.user_input.as_deref(), Some("修复?"));
        assert_eq!(turn.tool_count, Some(16));
        assert_eq!(turn.llm_rounds, Some(17));
        assert_eq!(turn.tokens_in, Some(285_235));
        assert_eq!(turn.tokens_out, Some(4_624));
        assert_eq!(
            turn.tool_calls.as_ref().map(Vec::len),
            Some(16),
            "fixture should preserve the serial repair spiral"
        );

        let context = events
            .iter()
            .find(|event| {
                event.event_type == JournalEventType::ContextAssemblyRecorded
                    && event.turn == Some(3)
            })
            .expect("turn 3 context record");
        let token_budget = &context
            .context_assembly_trace
            .as_ref()
            .expect("context trace")["token_budget"];
        assert_eq!(token_budget["user_message_tokens"], 3);
        assert_eq!(token_budget["tool_schema_tokens"], 4_663);
        assert_eq!(token_budget["system_prompt_tokens"], 3_829);
        assert_eq!(token_budget["compression_triggered"], false);

        let stall_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::StallDetected)
            .count();
        let verdict_count = events
            .iter()
            .filter(|event| event.event_type == JournalEventType::TurnGuardVerdict)
            .count();
        assert_eq!(stall_count, 1);
        assert_eq!(verdict_count, 1);

        let eval = events
            .iter()
            .find(|event| {
                event.event_type == JournalEventType::TurnEvaluation && event.turn == Some(3)
            })
            .expect("turn 3 evaluation");
        let metadata = eval.metadata.as_ref().expect("turn evaluation metadata");
        assert_eq!(metadata["quality"], 0.2);
        assert_eq!(metadata["confidence"], 0.7);
        assert_eq!(metadata["stall_count"], 1);
        assert_eq!(metadata["success"], false);
        assert_eq!(metadata["tool_call_count"], 16);
        assert_eq!(metadata["verdict_warning"], true);
    }

    #[test]
    fn surgical_removal_record_is_synthetic_placeholder() {
        let rec = base_tool_record(
            SURGICAL_REMOVAL_TOOL_NAME,
            true,
            Some("(removed from context — skill covered this work)"),
        );
        assert!(
            rec.is_synthetic_placeholder(),
            "surgically_removed records must be classified as synthetic \
             placeholders so evaluation/analytics skip them"
        );
    }

    #[test]
    fn skipped_deferred_and_skill_records_remain_synthetic_placeholders() {
        assert!(
            base_tool_record("read_file", false, Some("Skipped: skill routed"))
                .is_synthetic_placeholder()
        );
        assert!(
            base_tool_record("read_file", false, Some("Deferred: skill invoked"))
                .is_synthetic_placeholder()
        );
        assert!(
            base_tool_record(
                "skill",
                true,
                Some("Skill 'debug' was already loaded (turn 2). Follow those instructions.")
            )
            .is_synthetic_placeholder()
        );
    }

    #[test]
    fn real_tool_records_are_not_synthetic_placeholders() {
        assert!(!base_tool_record("git_show", true, Some("diff")).is_synthetic_placeholder());
        assert!(
            !base_tool_record("grep", false, Some("error: bad regex")).is_synthetic_placeholder()
        );
        assert!(!base_tool_record("read_file", true, None).is_synthetic_placeholder());
    }

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
    fn journal_event_plan_edit_serializes_and_round_trips() {
        let meta = serde_json::json!({"subtask_count": 2});
        let evt = JournalEvent::plan_edit(Some("sid-plan"), "Plan edited: add step", Some(meta));
        assert_eq!(evt.event_type, JournalEventType::PlanEdit);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"plan_edit\""));
        assert!(json.contains("Plan edited"));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::PlanEdit);
        let m = parsed.metadata.expect("metadata");
        assert_eq!(
            m.get("action").and_then(|v| v.as_str()),
            Some("Plan edited: add step")
        );
    }

    #[test]
    fn journal_event_plan_lifecycle_serializes_and_round_trips() {
        let detail = serde_json::json!({"mode": "auto", "subtask_count": 3});
        let evt =
            JournalEvent::plan_lifecycle(Some("sid-lc"), "Plan execution started", Some(detail));
        assert_eq!(evt.event_type, JournalEventType::PlanLifecycle);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"plan_lifecycle\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::PlanLifecycle);
        let m = parsed.metadata.expect("metadata");
        assert_eq!(
            m.get("summary").and_then(|v| v.as_str()),
            Some("Plan execution started")
        );
    }

    #[test]
    fn journal_event_goal_steered_serializes_and_round_trips() {
        let evt = JournalEvent::goal_steered(
            Some("sid-goal"),
            4,
            "edge_tool:set_goal",
            Some("old goal"),
            "new goal",
            Some(serde_json::json!({"mode": "manual"})),
        );
        assert_eq!(evt.event_type, JournalEventType::GoalSteered);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"goal_steered\""));
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::GoalSteered);
        assert_eq!(parsed.turn, Some(4));
        let metadata = parsed.metadata.expect("metadata");
        assert_eq!(
            metadata.get("source").and_then(|value| value.as_str()),
            Some("edge_tool:set_goal")
        );
        assert_eq!(
            metadata
                .get("previous_goal")
                .and_then(|value| value.as_str()),
            Some("old goal")
        );
        assert_eq!(
            metadata.get("new_goal").and_then(|value| value.as_str()),
            Some("new goal")
        );
    }

    #[test]
    fn journal_event_drift_detected_round_trips_structured_cause_and_evidence() {
        let evt = JournalEvent::drift_detected(
            Some("sid-drift"),
            7,
            0.75,
            DriftCause::MemoryMiss {
                expected_but_not_retrieved: vec!["session history".into(), "repo context".into()],
                query_used: "debug repeated session start".into(),
            },
            vec![DriftEvidence {
                turn: 6,
                evidence_type: EvidenceType::MemoryMismatch,
                description: "Retrieved unrelated CI memories instead of resume context".into(),
                confidence: 0.9.into(),
            }],
            "Re-query with explicit session-resume terms",
        );

        assert_eq!(evt.event_type, JournalEventType::DriftDetected);
        assert_eq!(evt.turn, Some(7));
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        let meta = parsed.metadata.expect("metadata");

        assert_eq!(meta.get("severity").and_then(|v| v.as_f64()), Some(0.75));
        assert_eq!(meta.get("evidence_count").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            meta.get("recovery_suggestion").and_then(|v| v.as_str()),
            Some("Re-query with explicit session-resume terms")
        );
        assert_eq!(
            meta.get("cause")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("MemoryMiss")
        );
        let evidence = meta
            .get("evidence")
            .and_then(|v| v.as_array())
            .expect("evidence array");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].get("turn").and_then(|v| v.as_u64()), Some(6));
        assert_eq!(
            evidence[0].get("evidence_type").and_then(|v| v.as_str()),
            Some("MemoryMismatch")
        );
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
        assert!(evt.metadata.is_none());
    }

    #[test]
    fn journal_event_compact_with_summary() {
        let evt = JournalEvent::compact_with_summary(
            Some("s"),
            5,
            10,
            3,
            Some("User worked on fixing auth bugs"),
        );
        assert_eq!(evt.event_type, JournalEventType::Compact);
        assert_eq!(evt.turns_compacted, Some(10));
        assert_eq!(evt.facts_stored, Some(3));
        let meta = evt.metadata.unwrap();
        assert_eq!(meta["compact_summary"], "User worked on fixing auth bugs");
    }

    #[test]
    fn journal_event_compact_with_empty_summary() {
        let evt = JournalEvent::compact_with_summary(Some("s"), 5, 10, 3, Some(""));
        assert!(evt.metadata.is_none());
        let evt2 = JournalEvent::compact_with_summary(Some("s"), 5, 10, 3, None);
        assert!(evt2.metadata.is_none());
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
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("missing repo"));
        let parsed: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error.as_deref(), Some("missing repo parameter"));
    }

    #[test]
    fn tool_call_record_detects_synthetic_placeholders() {
        let skipped = ToolCallRecord {
            name: "read_file".into(),
            ok: false,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: Some(
                "Skipped: the skill already completed this work. Do NOT call `read_file` again."
                    .into(),
            ),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let deferred = ToolCallRecord {
            name: "bash".into(),
            ok: false,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: Some(
                "Deferred: skill was invoked in this turn. Read the skill instructions above."
                    .into(),
            ),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let dedup = ToolCallRecord {
            name: "skill".into(),
            ok: false,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: Some(
                "Skill 'debug' was already loaded (turn 2). Follow those instructions directly."
                    .into(),
            ),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        let actual_failure = ToolCallRecord {
            name: "skill".into(),
            ok: false,
            ms: 0,
            error: Some("Unknown skill".into()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: Some("Unknown skill 'debug'.".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };

        assert!(skipped.is_synthetic_placeholder());
        assert!(deferred.is_synthetic_placeholder());
        assert!(dedup.is_synthetic_placeholder());
        assert!(!actual_failure.is_synthetic_placeholder());
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
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
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
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
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
            &["bash".to_string()],
            false,
            1,
            2,
            1,
            0, // total_timeouts
            &[],
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
        assert_eq!(meta["avoid_tools_count"], 1);
        assert_eq!(meta["deprioritized_tool_names"][0], "bash");
        assert_eq!(meta["avoid_reason_codes"][0], "tool_health_deprioritized");
        assert_eq!(meta["avoid_reason_codes"][1], "session_failures");
        assert_eq!(meta["avoid_reason_codes"][2], "stall_recovery");
        assert_eq!(
            meta["avoid_reason_summary"],
            "deprioritized by tool health: bash; 2 non-timeout failure(s) recorded; 1 stall/divergence nudge(s) issued"
        );
        assert_eq!(meta["force_stop"], false);
        assert_eq!(meta["nudge_count"], 1);
        assert_eq!(meta["total_errors"], 2);
        assert_eq!(meta["non_timeout_errors"], 2);
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
            &["bash".to_string(), "grep".to_string()],
            true,
            3,
            5,
            2,
            2, // total_timeouts
            &["bash".to_string()],
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
        assert_eq!(meta["non_timeout_errors"], 3);
        assert_eq!(meta["timeout_dominant_tools"][0], "bash");
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
        let evt = JournalEvent::turn_guard_verdict(
            None,
            1,
            "info",
            &[],
            &[],
            &[],
            false,
            0,
            1,
            0,
            0,
            &[],
            0,
            0,
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnGuardVerdict);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["injections"], 0);
        assert!(meta["injection_preview"].is_null());
        assert_eq!(meta["force_stop"], false);
        assert_eq!(meta["non_timeout_errors"], 1);
        assert_eq!(meta["avoid_tools_count"], 0);
    }

    #[test]
    fn turn_evaluation_event_serializes() {
        let evt = JournalEvent::turn_evaluation(
            Some("sess-1"),
            Some(4),
            "cli_repl",
            true,
            true,
            0.91,
            0.72,
            0.18,
            1,
            false,
            2,
            vec![serde_json::json!({
                "kind": "all_tools_healthy",
                "weight": 0.4,
                "message": "All tool calls completed successfully"
            })],
        );
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"turn_evaluation\""));
        assert!(json.contains("\"turn\":4"));

        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnEvaluation);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["source"], "cli_repl");
        assert_eq!(meta["live_query"], true);
        assert_eq!(meta["success"], true);
        assert_eq!(meta["quality"], 0.91);
        assert_eq!(meta["confidence"], 0.72);
        assert_eq!(meta["budget_pressure"], 0.18);
        assert_eq!(meta["stall_count"], 1);
        assert_eq!(meta["verdict_warning"], false);
        assert_eq!(meta["tool_call_count"], 2);
        assert_eq!(meta["signal_count"], 1);
        assert_eq!(meta["signals"][0]["kind"], "all_tools_healthy");
    }

    #[test]
    fn turn_evaluation_event_without_turn_is_allowed() {
        let evt = JournalEvent::turn_evaluation(
            Some("sess-2"),
            None,
            "server_runtime",
            false,
            false,
            0.35,
            0.81,
            0.64,
            2,
            true,
            0,
            vec![],
        );
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, JournalEventType::TurnEvaluation);
        assert_eq!(parsed.turn, None);
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta["source"], "server_runtime");
        assert_eq!(meta["signal_count"], 0);
        assert_eq!(meta["signals"], serde_json::json!([]));
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
            Some("finished the review"),
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationSubRunCompleted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["agent_id"], "agent-a");
        assert_eq!(meta["status"], "completed");
        assert!(meta["error"].is_null());
        assert_eq!(meta["output_preview"], "finished the review");
    }

    #[test]
    fn delegation_sub_run_started_event_builder() {
        let evt = JournalEvent::delegation_sub_run_started(
            Some("s1"),
            "del-1",
            "run-sub-1",
            "run-parent",
            "agent-a",
            "running",
            2,
            Some("run-sub-0"),
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationSubRunStarted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-sub-1");
        assert_eq!(meta["parent_run_id"], "run-parent");
        assert_eq!(meta["agent_id"], "agent-a");
        assert_eq!(meta["status"], "running");
        assert_eq!(meta["depth"], 2);
        assert_eq!(meta["retry_of"], "run-sub-0");
    }

    #[test]
    fn delegation_retry_event_builder() {
        let evt = JournalEvent::delegation_retry(
            Some("s1"),
            "del-1",
            "run-sub-1",
            "run-sub-2",
            "agent-a",
            2,
            "quality too low",
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationRetry);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["original_run_id"], "run-sub-1");
        assert_eq!(meta["retry_run_id"], "run-sub-2");
        assert_eq!(meta["attempt"], 2);
        assert_eq!(meta["reason"], "quality too low");
    }

    #[test]
    fn delegation_completed_event_builder() {
        let evt = JournalEvent::delegation_completed(
            Some("s1"),
            "del-1",
            "fan_out",
            3,
            2,
            1,
            "partial",
            Some("merged result preview"),
        );
        assert_eq!(evt.event_type, JournalEventType::DelegationCompleted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["succeeded"], 2);
        assert_eq!(meta["failed"], 1);
        assert_eq!(meta["aggregated_status"], "partial");
        assert_eq!(meta["aggregated_output_preview"], "merged result preview");
    }

    #[test]
    fn adaptive_baseline_promoted_event_builder() {
        let keys = vec![
            "memory.retrieval_top_k".to_string(),
            "compression.max_history_tokens".to_string(),
        ];
        let evt = JournalEvent::adaptive_baseline_promoted(
            Some("s1"),
            "fetch",
            None,
            "exp-1",
            "winner",
            true,
            &keys,
        );
        assert_eq!(evt.event_type, JournalEventType::AdaptiveBaselinePromoted);
        let meta = evt.metadata.as_ref().unwrap();
        assert_eq!(meta["task_type"], "fetch");
        assert!(meta["domain"].is_null());
        assert_eq!(meta["experiment_id"], "exp-1");
        assert_eq!(meta["variant_id"], "winner");
        assert_eq!(meta["replaced_existing"], true);
        assert_eq!(meta["config_keys"][0], "memory.retrieval_top_k");
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

    #[test]
    fn classify_session_end_state_detects_completed_session() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = format!("test-recovery-complete-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
            .unwrap();
        writer
            .append(&JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "fix auth flow",
                "I checked the login path.",
                0,
                10,
                5,
                10,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::session_end(Some(&sid), 1))
            .unwrap();

        assert_eq!(
            classify_session_end_state(&sid).unwrap(),
            SessionEndState::Completed
        );
    }

    #[test]
    fn classify_session_end_state_uses_resume_action_when_resumable_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = format!("test-recovery-interrupt-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
            .unwrap();
        writer
            .append(&JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "continue the migration",
                "I finished the schema diff.",
                0,
                10,
                5,
                10,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::interruption_recorded(
                Some(&sid),
                1,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resume_action": {"wait_and_retry": {"delay_seconds": 30}},
                    "has_checkpoint": true,
                    "tool_calls_completed": 2,
                    "turns_completed": 1,
                    "remaining_turns": 4,
                }),
            ))
            .unwrap();

        assert_eq!(
            classify_session_end_state(&sid).unwrap(),
            SessionEndState::Interrupted {
                kind: "rate_limited".to_string(),
                resumable: true,
            }
        );
    }

    #[test]
    fn classify_session_end_state_marks_requires_intervention_as_non_resumable() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = format!("test-recovery-auth-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
            .unwrap();
        writer
            .append(&JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "fetch CI logs",
                "Need valid credentials first.",
                0,
                10,
                5,
                10,
            ))
            .unwrap();
        writer
            .append(&JournalEvent::interruption_recorded(
                Some(&sid),
                1,
                serde_json::json!({
                    "kind": "auth_failure",
                    "resume_action": {
                        "requires_intervention": {
                            "description": "refresh credentials"
                        }
                    },
                    "has_checkpoint": true,
                }),
            ))
            .unwrap();

        assert_eq!(
            classify_session_end_state(&sid).unwrap(),
            SessionEndState::Interrupted {
                kind: "auth_failure".to_string(),
                resumable: false,
            }
        );
    }

    #[test]
    fn classify_session_end_state_detects_zombie_session() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = format!("test-recovery-zombie-{}", uuid::Uuid::new_v4());
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&JournalEvent::session_start(Some(&sid), Some("gpt-5")))
            .unwrap();
        writer
            .append(&JournalEvent::plan_progress(
                Some(&sid),
                1,
                "task-1",
                "Implement restart flow",
                "started",
                33,
                3,
                1,
            ))
            .unwrap();

        assert_eq!(
            classify_session_end_state(&sid).unwrap(),
            SessionEndState::Zombie
        );
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

    #[test]
    fn tool_call_record_serde_roundtrip_with_surgical_fields() {
        // New-style record with both surgical fields populated
        let rec = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: Some(0),
            args_preview: None,
            result_preview: Some("(removed)".into()),
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("read_file".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"surgically_removed\":true"));
        assert!(json.contains("\"original_tool_name\":\"read_file\""));
        let deser: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.surgically_removed, Some(true));
        assert_eq!(deser.original_tool_name.as_deref(), Some("read_file"));
        assert!(deser.is_synthetic_placeholder());
    }

    #[test]
    fn tool_call_record_serde_omits_none_surgical_fields() {
        // Normal record: surgical fields should be omitted from JSON
        let rec = base_tool_record("bash", true, Some("ok"));
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains("surgically_removed"),
            "None surgical fields should be skipped in serialization"
        );
        assert!(
            !json.contains("original_tool_name"),
            "None original_tool_name should be skipped in serialization"
        );
    }

    #[test]
    fn tool_call_record_backward_compat_deserialize() {
        // Legacy JSON without the new fields — should deserialize with defaults
        let legacy_json = r#"{"name":"read_file","ok":true,"ms":10}"#;
        let rec: ToolCallRecord = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(rec.surgically_removed, None);
        assert_eq!(rec.original_tool_name, None);
        assert!(!rec.is_synthetic_placeholder());
    }

    #[test]
    fn is_synthetic_placeholder_flag_takes_priority() {
        // Even if name is normal, flag=true marks as synthetic
        let rec = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(100),
            args_preview: None,
            result_preview: Some("content".into()),
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("read_file".to_string()),
            ..Default::default()
        };
        assert!(
            rec.is_synthetic_placeholder(),
            "surgically_removed=true must classify as synthetic regardless of name"
        );
    }

    #[test]
    fn journal_content_marker_is_deterministic() {
        let a = journal_content_marker("hello world");
        let b = journal_content_marker("hello world");
        assert_eq!(a, b);
        assert!(a.starts_with("<redacted: len=11 sha="));
        assert!(a.ends_with('>'));
        assert!(!a.contains("hello"));
    }

    #[test]
    fn journal_content_marker_differs_for_different_input() {
        assert_ne!(
            journal_content_marker("hello"),
            journal_content_marker("world")
        );
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn journal_content_redact_enabled_reads_env_var() {
        // SAFETY: serialized via #[serial] above.
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
        assert!(!journal_content_redact_enabled());
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        assert!(journal_content_redact_enabled());
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "0") };
        assert!(!journal_content_redact_enabled());
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn turn_event_redacts_content_when_env_set() {
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        let evt = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "secret query",
            "secret answer",
            0,
            10,
            5,
            100,
        );
        let user = evt.user_input.as_deref().unwrap_or("");
        let asst = evt.assistant_output.as_deref().unwrap_or("");
        assert!(!user.contains("secret query"), "user_input leaked: {user}");
        assert!(
            !asst.contains("secret answer"),
            "assistant_output leaked: {asst}"
        );
        assert!(user.starts_with("<redacted:"));
        assert!(asst.starts_with("<redacted:"));
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn turn_event_keeps_content_when_env_unset() {
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
        let evt = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "hello",
            "world",
            0,
            10,
            5,
            100,
        );
        assert_eq!(evt.user_input.as_deref(), Some("hello"));
        assert_eq!(evt.assistant_output.as_deref(), Some("world"));
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn turn_error_event_redacts_user_input_when_env_set() {
        unsafe { std::env::set_var("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        let evt =
            JournalEvent::turn_error(Some("s1"), 1, Some("gpt-4"), "secret query", "boom", 50);
        let user = evt.user_input.as_deref().unwrap_or("");
        assert!(!user.contains("secret query"));
        assert!(user.starts_with("<redacted:"));
        // Error message itself is system-generated, kept as-is.
        assert_eq!(evt.error.as_deref(), Some("boom"));
        unsafe { std::env::remove_var("ASTRA_JOURNAL_CONTENT_REDACT") };
    }

    #[test]
    fn was_blocked_by_policy_detects_restricted_tool() {
        let rec = ToolCallRecord {
            name: "read_file".to_string(),
            ok: false,
            error: Some(
                "blocked_tool: Tool 'read_file' is currently restricted and cannot be executed."
                    .into(),
            ),
            ..Default::default()
        };
        assert!(rec.was_blocked_by_policy());
    }

    #[test]
    fn was_blocked_by_policy_ignores_normal_failures() {
        let rec = ToolCallRecord {
            name: "read_file".to_string(),
            ok: false,
            error: Some("Error: file not found".into()),
            ..Default::default()
        };
        assert!(!rec.was_blocked_by_policy());
    }

    #[test]
    fn was_blocked_by_policy_ignores_successful_calls() {
        let rec = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            error: None,
            ..Default::default()
        };
        assert!(!rec.was_blocked_by_policy());
    }
}

#[cfg(test)]
mod turn_event_buffer_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn begin_turn_initializes_round_zero() {
        let buf = TurnEventBuffer::begin_turn(Some("sess-1"), 3);
        assert_eq!(buf.current_round(), 0);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn begin_turn_with_round_uses_provided_round() {
        let buf = TurnEventBuffer::begin_turn_with_round(Some("sess-1"), 3, 4);
        assert_eq!(buf.current_round(), 4);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn record_llm_round_advances_round_counter() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 1);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(100),
            duration_ms: 500,
            prompt_tokens: 1000,
            completion_tokens: 200,
            cache_read_tokens: 0,
            tool_calls_returned: 2,
            tool_call_names: vec!["read_file".into(), "grep".into()],
            finish_reason: Some("tool_calls".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        assert_eq!(buf.current_round(), 1);
        assert_eq!(buf.len(), 1);

        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 300,
            prompt_tokens: 2000,
            completion_tokens: 100,
            cache_read_tokens: 500,
            tool_calls_returned: 1,
            tool_call_names: vec!["write_file".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        assert_eq!(buf.current_round(), 2);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn recorded_llm_round_event_has_correct_fields() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 5);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(42),
            duration_ms: 800,
            prompt_tokens: 3000,
            completion_tokens: 400,
            cache_read_tokens: 1000,
            tool_calls_returned: 3,
            tool_call_names: vec!["a".into(), "b".into(), "c".into()],
            finish_reason: Some("tool_calls".into()),
            agentic_step: Some(4),
            source: Some("agentic_loop".into()),
            run_id: Some("run-42".into()),
            tool_calls: None,
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.event_type, JournalEventType::LlmRound);
        assert_eq!(ev.turn, Some(5));
        assert_eq!(ev.agentic_step, Some(4));
        assert_eq!(ev.round, Some(0));
        assert_eq!(ev.ttft_ms, Some(42));
        assert_eq!(ev.tokens_in, Some(3000));
        assert_eq!(ev.tokens_out, Some(400));
        assert_eq!(ev.cache_read_tokens, Some(1000));
        assert_eq!(ev.tool_calls_returned, Some(3));
        let meta = ev.metadata.as_ref().unwrap();
        assert_eq!(meta["tool_call_names"].as_array().unwrap().len(), 3);
        assert_eq!(meta["source"], "agentic_loop");
        assert_eq!(meta["run_id"], "run-42");
    }

    #[test]
    fn recorded_llm_round_event_can_embed_tool_calls() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-embed"), 2);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(10),
            duration_ms: 200,
            prompt_tokens: 100,
            completion_tokens: 20,
            cache_read_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["git_diff".into()],
            finish_reason: Some("tool_calls".into()),
            agentic_step: Some(1),
            source: Some("agentic_loop".into()),
            run_id: Some("run-embed".into()),
            tool_calls: Some(vec![ToolCallRecord {
                name: "git_diff".into(),
                ok: true,
                ms: 50,
                args_full: Some("{\"stat_only\":true}".into()),
                result_preview: Some("diff --git ...".into()),
                round: Some(0),
                ..Default::default()
            }]),
        });
        let events = buf.drain();
        let ev = &events[0];
        let tool_calls = ev.tool_calls.as_ref().expect("embedded tool calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "git_diff");
        assert_eq!(
            tool_calls[0].args_full.as_deref(),
            Some("{\"stat_only\":true}")
        );
    }

    #[test]
    fn next_batch_id_includes_round() {
        let mut buf = TurnEventBuffer::begin_turn(Some("s"), 0);
        assert_eq!(buf.next_batch_id(), "b-0-0");
        assert_eq!(buf.next_batch_id(), "b-0-1");
        // Advance round
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        assert_eq!(buf.next_batch_id(), "b-1-0");
    }

    /// Regression: llm_round events must carry the session-level turn number,
    /// not the internal agentic loop iteration count.
    #[test]
    fn llm_round_turn_uses_session_turn_number() {
        // Simulate session turn 7 (the 7th user message in the session)
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-turn"), 7);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(100),
            duration_ms: 500,
            prompt_tokens: 5000,
            completion_tokens: 200,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        let events = buf.drain();
        assert_eq!(
            events[0].turn,
            Some(7),
            "llm_round must use session turn number"
        );
    }

    /// Regression: text-only LLM responses (no tool calls) must still record
    /// an llm_round event so llm_rounds count is correct.
    #[test]
    fn text_only_response_records_llm_round() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-text"), 3);
        // Simulate a text-only response (0 tool calls)
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(48521),
            duration_ms: 120000,
            prompt_tokens: 24829,
            completion_tokens: 1281,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        assert_eq!(
            buf.current_round(),
            1,
            "round must advance even for text-only"
        );
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens_in, Some(24829));
        assert_eq!(events[0].tool_calls_returned, Some(0));
    }

    /// Regression: auto-reflection LLM calls must record llm_round events
    /// so turn.tokens_in breakdown is complete.
    #[test]
    fn auto_reflection_round_has_finish_reason() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-refl"), 2);
        // Normal round
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(100),
            duration_ms: 500,
            prompt_tokens: 10000,
            completion_tokens: 200,
            cache_read_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["read_file".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        // Auto-reflection round
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 0,
            prompt_tokens: 54000,
            completion_tokens: 500,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("auto_reflection".into()),
            agentic_step: None,
            source: Some("auto_reflection".into()),
            run_id: Some("run-reflect".into()),
            tool_calls: None,
        });
        assert_eq!(buf.current_round(), 2);
        let events = buf.drain();
        assert_eq!(events.len(), 2);
        // Verify auto-reflection round has the finish_reason in metadata
        let refl = &events[1];
        assert_eq!(refl.round, Some(1));
        assert_eq!(refl.tokens_in, Some(54000));
        let refl_meta = refl.metadata.as_ref().unwrap();
        assert_eq!(refl_meta["source"], "auto_reflection");
        assert_eq!(refl_meta["run_id"], "run-reflect");
    }

    /// Regression: rate-limited early exit must record an llm_round with
    /// finish_reason so the journal reflects the LLM call happened.
    #[test]
    fn rate_limited_round_records_finish_reason() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-rl"), 2);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(50),
            duration_ms: 200,
            prompt_tokens: 8000,
            completion_tokens: 0,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("rate_limited".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        let meta = events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["finish_reason"], "rate_limited");
        assert_eq!(events[0].tool_calls_returned, Some(0));
    }

    /// Regression: token-budget-exceeded early exit must record an llm_round.
    #[test]
    fn token_budget_exceeded_round_records_finish_reason() {
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-tb"), 5);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 100,
            prompt_tokens: 128000,
            completion_tokens: 50,
            cache_read_tokens: 64000,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("token_budget_exceeded".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        let meta = events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["finish_reason"], "token_budget_exceeded");
        assert_eq!(events[0].tokens_in, Some(128000));
        assert_eq!(events[0].cache_read_tokens, Some(64000));
    }

    #[test]
    fn flush_writes_events_to_journal() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-flush").unwrap();

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-flush"), 1);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: None,
            duration_ms: 100,
            prompt_tokens: 500,
            completion_tokens: 50,
            cache_read_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["bash".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        buf.record(JournalEvent::base_public(
            JournalEventType::Turn,
            Some("sess-flush"),
        ));
        assert_eq!(buf.len(), 2);

        buf.flush(&writer).unwrap();
        assert!(buf.is_empty());

        // Verify written to disk
        let content = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let ev0: JournalEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(ev0.event_type, JournalEventType::LlmRound);
        let ev1: JournalEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ev1.event_type, JournalEventType::Turn);
    }

    #[test]
    fn flush_interrupted_marks_events_partial() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-interrupted").unwrap();

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-interrupted"), 1);
        buf.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(50),
            duration_ms: 200,
            prompt_tokens: 1000,
            completion_tokens: 100,
            cache_read_tokens: 0,
            tool_calls_returned: 2,
            tool_call_names: vec!["read_file".into(), "grep".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });

        buf.flush_interrupted(&writer).unwrap();
        assert!(buf.is_empty());

        let content = std::fs::read_to_string(writer.path()).unwrap();
        let ev: JournalEvent = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(ev.event_type, JournalEventType::LlmRound);
        let partial = ev
            .metadata
            .as_ref()
            .and_then(|m| m.get("partial"))
            .and_then(|v| v.as_bool());
        assert_eq!(partial, Some(true));
    }

    #[test]
    fn flush_empty_buffer_is_noop() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-empty").unwrap();

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-empty"), 1);
        buf.flush(&writer).unwrap();
        // File should not exist (no events written)
        assert!(!writer.path().exists());
    }

    #[test]
    fn drain_returns_events_and_clears_buffer() {
        let mut buf = TurnEventBuffer::begin_turn(Some("s"), 0);
        buf.record(JournalEvent::base_public(JournalEventType::Turn, Some("s")));
        buf.record(JournalEvent::base_public(JournalEventType::Turn, Some("s")));
        assert_eq!(buf.len(), 2);
        let drained = buf.drain();
        assert_eq!(drained.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn append_bulk_writes_multiple_events_atomically() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-bulk").unwrap();

        let events = vec![
            JournalEvent::session_start(Some("sess-bulk"), Some("gpt-4")),
            JournalEvent::base_public(JournalEventType::Turn, Some("sess-bulk")),
            JournalEvent::session_end(Some("sess-bulk"), 1),
        ];
        writer.append_bulk(&events).unwrap();

        let content = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3);
    }

    /// Concurrent appends from multiple threads must remain record-separated.
    ///
    /// Regression for cancel-shutdown audit #2 fix: when the in-process
    /// `edge_callback_ledger` mutex was narrowed (so it no longer wrapped the
    /// journal write), two HTTP approval handlers could call
    /// `JournalWriter::append` simultaneously. The old implementation used
    /// `writeln!`, which issues the line and the trailing `\n` as **two**
    /// syscalls. With `O_APPEND`, that lost atomicity: two writers produced
    /// `{a}{b}\n\n` instead of `{a}\n{b}\n`, and the parser saw zero valid
    /// events. The fix is a single `write_all` of `line + "\n"`.
    #[test]
    fn concurrent_appends_remain_record_separated() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let session_id = "sess-concurrent-append";
        let n_threads = 8usize;
        let n_per_thread = 16usize;

        std::thread::scope(|scope| {
            for t in 0..n_threads {
                let dir = dir.clone();
                scope.spawn(move || {
                    let _guard = JournalDirGuard::new(&dir);
                    let writer = JournalWriter::new(session_id).unwrap();
                    for i in 0..n_per_thread {
                        let mut event =
                            JournalEvent::base_public(JournalEventType::Turn, Some(session_id));
                        event.user_input = Some(format!("t{t}-i{i}"));
                        writer.append(&event).unwrap();
                    }
                });
            }
        });

        let _guard = JournalDirGuard::new(&dir);
        let events = read_journal(session_id).unwrap();
        assert_eq!(
            events.len(),
            n_threads * n_per_thread,
            "every concurrent append should produce one parseable record"
        );
    }

    /// E2E regression test using real session data (a33177cc).
    ///
    /// Before the fix, llm_round events used 0-based turn numbers while
    /// turn events used 1-based numbers (state.turn += 1 happens before
    /// the turn event is written, but after stream_chat_sse returns).
    ///
    /// Real data (buggy):
    ///   llm_round turn=0  ← should be 1
    ///   turn      turn=1
    ///   llm_round turn=1  ← should be 2
    ///   llm_round turn=1  ← should be 2
    ///   turn      turn=2
    ///   llm_round turn=2  ← should be 3
    ///   turn      turn=3
    #[test]
    fn e2e_llm_round_turn_matches_turn_event() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::new("sess-e2e-turn").unwrap();

        // Simulate 3 turns with the FIXED numbering (1-based).
        // Turn 1: "hi" — 1 round, text-only
        let mut buf1 = TurnEventBuffer::begin_turn(Some("sess-e2e-turn"), 1);
        buf1.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(988),
            duration_ms: 1831,
            prompt_tokens: 9375,
            completion_tokens: 11,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("stop".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        let obs1 = buf1.drain();
        writer.append_bulk(&obs1).unwrap();
        let turn1 = JournalEvent::turn(
            Some("sess-e2e-turn"),
            1,
            Some("qwen-turbo"),
            "hi",
            "你好！",
            0,
            9375,
            11,
            1831,
        );
        writer.append(&turn1).unwrap();

        // Turn 2: "描述一下这个项目" — 2 rounds, 1 tool call
        let mut buf2 = TurnEventBuffer::begin_turn(Some("sess-e2e-turn"), 2);
        buf2.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(2388),
            duration_ms: 3500,
            prompt_tokens: 10070,
            completion_tokens: 30,
            cache_read_tokens: 0,
            tool_calls_returned: 1,
            tool_call_names: vec!["read_file".into()],
            finish_reason: None,
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        buf2.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(1200),
            duration_ms: 7121,
            prompt_tokens: 19744,
            completion_tokens: 539,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("stop".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        let obs2 = buf2.drain();
        writer.append_bulk(&obs2).unwrap();
        let turn2 = JournalEvent::turn(
            Some("sess-e2e-turn"),
            2,
            Some("qwen-turbo"),
            "描述一下这个项目",
            "这个项目是...",
            1,
            29814,
            569,
            10621,
        );
        writer.append(&turn2).unwrap();

        // Turn 3: "review local changes" — 1 round, text-only (prefetch)
        let mut buf3 = TurnEventBuffer::begin_turn(Some("sess-e2e-turn"), 3);
        buf3.record_llm_round(LlmRoundRecord {
            ttft_ms: Some(21633),
            duration_ms: 85243,
            prompt_tokens: 21454,
            completion_tokens: 1347,
            cache_read_tokens: 0,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: Some("stop".into()),
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
        });
        let obs3 = buf3.drain();
        writer.append_bulk(&obs3).unwrap();
        let turn3 = JournalEvent::turn(
            Some("sess-e2e-turn"),
            3,
            Some("qwen3.6-plus"),
            "review local changes",
            "Code review...",
            0,
            43815,
            3308,
            85243,
        );
        writer.append(&turn3).unwrap();

        // Parse back and verify consistency
        let content = std::fs::read_to_string(writer.path()).unwrap();
        let events: Vec<JournalEvent> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let llm_rounds: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == JournalEventType::LlmRound)
            .collect();
        let turns: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == JournalEventType::Turn)
            .collect();

        assert_eq!(turns.len(), 3);
        assert_eq!(llm_rounds.len(), 4); // 1 + 2 + 1

        // Core invariant: every llm_round's turn must match its parent turn event
        // Turn 1 has 1 llm_round
        assert_eq!(llm_rounds[0].turn, Some(1), "llm_round[0] must be turn 1");
        assert_eq!(turns[0].turn, Some(1));

        // Turn 2 has 2 llm_rounds
        assert_eq!(llm_rounds[1].turn, Some(2), "llm_round[1] must be turn 2");
        assert_eq!(llm_rounds[2].turn, Some(2), "llm_round[2] must be turn 2");
        assert_eq!(turns[1].turn, Some(2));

        // Turn 3 has 1 llm_round
        assert_eq!(llm_rounds[3].turn, Some(3), "llm_round[3] must be turn 3");
        assert_eq!(turns[2].turn, Some(3));

        // Verify round numbers within each turn
        assert_eq!(llm_rounds[0].round, Some(0));
        assert_eq!(llm_rounds[1].round, Some(0));
        assert_eq!(llm_rounds[2].round, Some(1));
        assert_eq!(llm_rounds[3].round, Some(0));
    }

    /// Verify the needs_start_event logic for resumed sessions.
    /// This mirrors the rposition-based check in repl_turn::initialize_journal.
    #[test]
    fn needs_start_event_scenarios() {
        let tmp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());

        // Helper: same logic as repl_turn::initialize_journal
        fn needs_start(events: &[JournalEvent]) -> bool {
            let last_type = events.last().map(|e| &e.event_type);
            match last_type {
                None | Some(JournalEventType::SessionEnd) => true,
                _ => {
                    let last_start = events
                        .iter()
                        .rposition(|e| e.event_type == JournalEventType::SessionStart);
                    let last_end = events
                        .iter()
                        .rposition(|e| e.event_type == JournalEventType::SessionEnd);
                    let has_unmatched_start = match (last_start, last_end) {
                        (Some(s), Some(e)) => s > e,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    !has_unmatched_start
                }
            }
        }

        // Empty journal → needs start
        assert!(needs_start(&[]));

        // Clean end → needs start
        let events = vec![
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
            JournalEvent::session_end(Some("s"), 1),
        ];
        assert!(needs_start(&events));

        // Interrupted (start, turn, no end) → already has open start, skip
        let events = vec![
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
        ];
        assert!(!needs_start(&events));

        // start → end → start → turn (interrupted) → already has open start, skip
        let events = vec![
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::session_end(Some("s"), 1),
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
        ];
        assert!(!needs_start(&events));

        // start → end → turn (orphan turn after clean end) → needs start
        let events = vec![
            JournalEvent::session_start(Some("s"), Some("m")),
            JournalEvent::session_end(Some("s"), 1),
            JournalEvent::base_public(JournalEventType::Turn, Some("s")),
        ];
        assert!(needs_start(&events));
    }
}

#[cfg(test)]
mod observability_serde_tests {
    use super::*;

    #[test]
    fn tool_call_record_new_fields_serialize_only_when_set() {
        let rec = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            ms: 50,
            ..Default::default()
        };
        let json = serde_json::to_string(&rec).unwrap();
        // New fields should be omitted when None.
        assert!(!json.contains("start_offset_ms"));
        assert!(!json.contains("batch_id"));
        assert!(!json.contains("parallel"));
        assert!(!json.contains("\"round\""));
    }

    #[test]
    fn tool_call_record_new_fields_round_trip() {
        let rec = ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            ms: 10,
            start_offset_ms: Some(5000),
            batch_id: Some("b-0-0".into()),
            parallel: Some(true),
            round: Some(2),
            ..Default::default()
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"start_offset_ms\":5000"));
        assert!(json.contains("\"batch_id\":\"b-0-0\""));
        assert!(json.contains("\"parallel\":true"));
        assert!(json.contains("\"round\":2"));

        let deser: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.start_offset_ms, Some(5000));
        assert_eq!(deser.batch_id.as_deref(), Some("b-0-0"));
        assert_eq!(deser.parallel, Some(true));
        assert_eq!(deser.round, Some(2));
    }

    #[test]
    fn tool_call_record_backward_compat_old_json_missing_new_fields() {
        // Old journal entries won't have the new fields — they must deserialize to None.
        let old_json = r#"{"name":"bash","ok":true,"ms":100}"#;
        let rec: ToolCallRecord = serde_json::from_str(old_json).unwrap();
        assert_eq!(rec.name, "bash");
        assert!(rec.ok);
        assert_eq!(rec.ms, 100);
        assert_eq!(rec.start_offset_ms, None);
        assert_eq!(rec.batch_id, None);
        assert_eq!(rec.parallel, None);
        assert_eq!(rec.round, None);
    }

    #[test]
    fn journal_event_new_fields_serialize_only_when_set() {
        let ev = JournalEvent::base_public(JournalEventType::Turn, Some("s1"));
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("\"round\""));
        assert!(!json.contains("tool_calls_returned"));
        assert!(!json.contains("offset_ms"));
        assert!(!json.contains("llm_rounds"));
        assert!(!json.contains("total_llm_ms"));
        assert!(!json.contains("total_tool_ms"));
    }

    #[test]
    fn journal_event_llm_round_type_round_trip() {
        let mut ev = JournalEvent::base_public(JournalEventType::LlmRound, Some("s1"));
        ev.round = Some(3);
        ev.tool_calls_returned = Some(5);
        ev.offset_ms = Some(12000);
        ev.tokens_in = Some(8000);
        ev.tokens_out = Some(400);

        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"llm_round\""));
        assert!(json.contains("\"round\":3"));
        assert!(json.contains("\"tool_calls_returned\":5"));

        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.event_type, JournalEventType::LlmRound);
        assert_eq!(deser.round, Some(3));
        assert_eq!(deser.tool_calls_returned, Some(5));
        assert_eq!(deser.offset_ms, Some(12000));
    }

    #[test]
    fn journal_event_backward_compat_old_turn_missing_new_fields() {
        let old_json = r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"s1","turn":1,"tokens_in":100,"tokens_out":20,"duration_ms":500}"#;
        let ev: JournalEvent = serde_json::from_str(old_json).unwrap();
        assert_eq!(ev.event_type, JournalEventType::Turn);
        assert_eq!(ev.round, None);
        assert_eq!(ev.tool_calls_returned, None);
        assert_eq!(ev.llm_rounds, None);
        assert_eq!(ev.total_llm_ms, None);
        assert_eq!(ev.total_tool_ms, None);
    }

    #[test]
    fn journal_event_turn_with_observability_summary() {
        let mut ev = JournalEvent::turn(
            Some("s1"),
            1,
            Some("gpt-4"),
            "hi",
            "hello",
            3,
            1000,
            200,
            5000,
        );
        ev.llm_rounds = Some(2);
        ev.total_llm_ms = Some(4500);
        ev.total_tool_ms = Some(500);

        let json = serde_json::to_string(&ev).unwrap();
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.llm_rounds, Some(2));
        assert_eq!(deser.total_llm_ms, Some(4500));
        assert_eq!(deser.total_tool_ms, Some(500));
    }

    // ── P5: parent_event_id causal lineage ──────────────────────────────

    #[test]
    fn parent_event_id_round_trips_through_serde() {
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
            .with_parent_event_id(Some("evt-session-start-001".to_string()));
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("parent_event_id"));
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deser.parent_event_id.as_deref(),
            Some("evt-session-start-001")
        );
    }

    #[test]
    fn parent_event_id_none_omitted_from_json() {
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100);
        assert!(ev.parent_event_id.is_none());
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains("parent_event_id"),
            "None parent_event_id must be omitted from JSON"
        );
    }

    #[test]
    fn parent_event_id_backward_compat_old_json_without_field() {
        // Simulate reading a journal line written before parent_event_id existed.
        let old_json = r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","turn":1,"tokens_in":10,"tokens_out":5,"duration_ms":100}"#;
        let ev: JournalEvent = serde_json::from_str(old_json).unwrap();
        assert!(
            ev.parent_event_id.is_none(),
            "old events without parent_event_id must deserialize as None"
        );
    }

    #[test]
    fn parent_event_id_chaining_with_other_builders() {
        let ev = JournalEvent::turn(Some("s"), 2, Some("m"), "q", "a", 1, 50, 10, 200)
            .with_parent_event_id(Some("parent-123".to_string()))
            .with_agentic_step(Some(3));
        assert_eq!(ev.parent_event_id.as_deref(), Some("parent-123"));
        assert_eq!(ev.agentic_step, Some(3));
    }

    #[test]
    fn parent_event_id_persists_through_writer_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "test-parent-id-00000000-0000-0000-0000-000000000001";
        let writer = JournalWriter::new(sid).unwrap();

        let ev = JournalEvent::session_start(Some(sid), Some("m"))
            .with_parent_event_id(Some("root".to_string()));
        writer.append(&ev).unwrap();

        let (events, _, _) = read_journal_for_digest(sid).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].parent_event_id.as_deref(), Some("root"));
    }

    // ── P0: git snapshot on Turn events ─────────────────────────────────

    #[test]
    fn git_snapshot_round_trips_through_serde() {
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
            .with_git_snapshot(
                Some("abc1234".to_string()),
                Some("feat/my-branch".to_string()),
            );
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("git_head"));
        assert!(json.contains("git_branch"));
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.git_head.as_deref(), Some("abc1234"));
        assert_eq!(deser.git_branch.as_deref(), Some("feat/my-branch"));
    }

    #[test]
    fn git_snapshot_none_omitted_from_json() {
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("git_head"), "None git_head must be omitted");
        assert!(
            !json.contains("git_branch"),
            "None git_branch must be omitted"
        );
    }

    #[test]
    fn git_snapshot_backward_compat_old_json_without_fields() {
        let old_json = r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","turn":1}"#;
        let ev: JournalEvent = serde_json::from_str(old_json).unwrap();
        assert!(ev.git_head.is_none());
        assert!(ev.git_branch.is_none());
    }

    #[test]
    fn git_snapshot_partial_only_head_no_branch() {
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
            .with_git_snapshot(Some("deadbeef".to_string()), None);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("git_head"));
        assert!(!json.contains("git_branch"));
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.git_head.as_deref(), Some("deadbeef"));
        assert!(deser.git_branch.is_none());
    }

    #[test]
    fn git_snapshot_detached_head_no_branch() {
        // Detached HEAD: git_head is set but git_branch is None (not on any branch).
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
            .with_git_snapshot(Some("f36ae6b1".to_string()), None);
        assert!(ev.git_branch.is_none());
        assert_eq!(ev.git_head.as_deref(), Some("f36ae6b1"));
    }

    #[test]
    fn git_snapshot_persists_through_writer_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "test-git-snap-00000000-0000-0000-0000-000000000001";
        let writer = JournalWriter::new(sid).unwrap();

        let ev = JournalEvent::turn(Some(sid), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
            .with_git_snapshot(Some("abc1234def5678".to_string()), Some("main".to_string()));
        writer.append(&ev).unwrap();

        let (events, _, _) = read_journal_for_digest(sid).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].git_head.as_deref(), Some("abc1234def5678"));
        assert_eq!(events[0].git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_snapshot_and_parent_event_id_combined() {
        let ev = JournalEvent::turn(Some("s"), 1, Some("m"), "hi", "yo", 0, 10, 5, 100)
            .with_parent_event_id(Some("parent-abc".to_string()))
            .with_git_snapshot(Some("cafe0123".to_string()), Some("dev".to_string()));
        let json = serde_json::to_string(&ev).unwrap();
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.parent_event_id.as_deref(), Some("parent-abc"));
        assert_eq!(deser.git_head.as_deref(), Some("cafe0123"));
        assert_eq!(deser.git_branch.as_deref(), Some("dev"));
    }

    #[test]
    fn git_snapshot_on_non_turn_event_works() {
        // git_snapshot can be attached to any event type (e.g., CompositeSnapshot).
        let ev = JournalEvent::base_public(JournalEventType::CompositeSnapshot, Some("s"))
            .with_git_snapshot(Some("1111aaaa".to_string()), Some("release".to_string()));
        let json = serde_json::to_string(&ev).unwrap();
        let deser: JournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.git_head.as_deref(), Some("1111aaaa"));
    }
}
