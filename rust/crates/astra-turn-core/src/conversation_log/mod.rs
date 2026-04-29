//! Conversation State Log (CSL) — append-only log for session conversation state.
//!
//! A session's conversation state is a sequence of [`CslEntry`] values:
//! a [`Snapshot`](CslEntry::Snapshot) followed by zero or more
//! [`TurnDelta`](CslEntry::TurnDelta) entries.
//!
//! [`materialize`] replays the log to reconstruct the current
//! `messages: Vec<Value>` + [`SessionStateCompact`] that the LLM sees.

pub mod db_store;
pub mod file_store;
pub mod manager;

use astra_pipeline::step_protocol::DelegationSubRunSummary;
use astra_turn_types::continuity::ContinuityState;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Entry types ────────────────────────────────────────────────────────────

/// A single entry in the conversation state log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum CslEntry {
    /// Full snapshot — session start, fork origin, or compaction fold.
    #[serde(rename = "snapshot")]
    Snapshot {
        seq: u64,
        turn: u32,
        messages: Vec<Value>,
        session_state: SessionStateCompact,
    },

    /// Incremental delta — messages appended during a single turn.
    #[serde(rename = "turn_delta")]
    TurnDelta {
        seq: u64,
        turn: u32,
        appended: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_patch: Option<SessionStatePatch>,
    },
}

impl CslEntry {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Snapshot { seq, .. } | Self::TurnDelta { seq, .. } => *seq,
        }
    }

    pub fn turn(&self) -> u32 {
        match self {
            Self::Snapshot { turn, .. } | Self::TurnDelta { turn, .. } => *turn,
        }
    }

    pub fn is_snapshot(&self) -> bool {
        matches!(self, Self::Snapshot { .. })
    }
}

// ─── Session state (non-message fields) ─────────────────────────────────────

/// Compact representation of session state beyond the message array.
/// Replaces the scattered fields in the old `HeavyCheckpoint`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionStateCompact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuity: Option<ContinuityState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_overrides: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_tracker: Option<Value>,
    #[serde(default)]
    pub budget_remaining_tokens: u64,
    #[serde(default)]
    pub budget_remaining_rounds: u32,
    #[serde(default)]
    pub consecutive_ctx_errors: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationCompact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<Value>,
}

/// Delegation recovery state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DelegationCompact {
    pub id: String,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_sub_runs: Vec<DelegationSubRunSummary>,
}

/// Incremental patch — only changed fields.
/// `None` = field unchanged from the previous state.
///
/// For nullable fields (`approval_overrides`, `interruption`, `compaction_tracker`),
/// `Option<Option<Value>>` is used:
/// - `None` = unchanged (field omitted in JSON)
/// - `Some(None)` = explicitly cleared (serialized as JSON `null`)
/// - `Some(Some(v))` = set to value `v`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionStatePatch {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nullable")]
    pub continuity: Option<Option<ContinuityState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nullable")]
    pub approval_overrides: Option<Option<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nullable")]
    pub interruption: Option<Option<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_remaining_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_remaining_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_ctx_errors: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nullable")]
    pub delegation: Option<Option<DelegationCompact>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "nullable")]
    pub compaction_tracker: Option<Option<Value>>,
}

/// Generic serde helper for `Option<Option<T>>` that preserves JSON `null` as `Some(None)`.
///
/// Without this, serde_json maps JSON `null` to `None` for `Option<T>`, losing the
/// distinction between "field absent" and "field explicitly set to null".
///
/// Semantics:
/// - `None` = field absent (unchanged in a patch) — skipped by `skip_serializing_if`
/// - `Some(None)` = explicitly cleared (serialized as JSON `null`)
/// - `Some(Some(v))` = set to value `v`
mod nullable {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T: Serialize>(
        val: &Option<Option<T>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match val {
            Some(Some(v)) => v.serialize(serializer),
            // Some(None) → JSON null; None is dead code due to skip_serializing_if
            Some(None) | None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D, T: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        // When called, the field IS present in JSON.
        // Option::deserialize naturally maps JSON null → None, value → Some(v).
        // Wrap in outer Some to distinguish from "field absent" (outer None).
        Ok(Some(Option::<T>::deserialize(deserializer)?))
    }
}

impl SessionStateCompact {
    /// Apply an incremental patch, updating only the fields present in `patch`.
    pub fn apply_patch(&mut self, patch: &SessionStatePatch) {
        if let Some(c) = &patch.continuity {
            self.continuity = c.clone();
        }
        if let Some(bt) = &patch.blocked_tools {
            self.blocked_tools = bt.clone();
        }
        if let Some(rt) = &patch.recent_tools {
            self.recent_tools = rt.clone();
        }
        if let Some(ao) = &patch.approval_overrides {
            self.approval_overrides = ao.clone();
        }
        if let Some(intr) = &patch.interruption {
            self.interruption = intr.clone();
        }
        if let Some(t) = patch.budget_remaining_tokens {
            self.budget_remaining_tokens = t;
        }
        if let Some(r) = patch.budget_remaining_rounds {
            self.budget_remaining_rounds = r;
        }
        if let Some(e) = patch.consecutive_ctx_errors {
            self.consecutive_ctx_errors = e;
        }
        if let Some(d) = &patch.delegation {
            self.delegation = d.clone();
        }
        if let Some(ct) = &patch.compaction_tracker {
            self.compaction_tracker = ct.clone();
        }
    }
}

// ─── Materialization ────────────────────────────────────────────────────────

/// Reconstructed conversation state from a CSL.
#[derive(Debug, Clone)]
pub struct MaterializedState {
    pub messages: Vec<Value>,
    pub session_state: SessionStateCompact,
    pub last_seq: u64,
    pub last_turn: u32,
}

/// Error from [`materialize`].
#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error("conversation log is empty")]
    EmptyLog,
    #[error("conversation log must begin with a Snapshot, found TurnDelta at seq {0}")]
    MissingSnapshot(u64),
}

/// Replay a CSL to reconstruct the current conversation state.
///
/// Finds the latest [`Snapshot`](CslEntry::Snapshot) in `entries`, then applies
/// all subsequent [`TurnDelta`](CslEntry::TurnDelta) entries in order.
///
/// Returns [`MaterializeError::EmptyLog`] for an empty slice and
/// [`MaterializeError::MissingSnapshot`] if no `Snapshot` is found.
pub fn materialize(entries: &[CslEntry]) -> Result<MaterializedState, MaterializeError> {
    if entries.is_empty() {
        return Err(MaterializeError::EmptyLog);
    }

    // Find last snapshot (scan backwards).
    let snapshot_idx = entries
        .iter()
        .rposition(CslEntry::is_snapshot)
        .ok_or_else(|| MaterializeError::MissingSnapshot(entries[0].seq()))?;

    let (messages, mut state, mut last_seq, mut last_turn) = match &entries[snapshot_idx] {
        CslEntry::Snapshot {
            seq,
            turn,
            messages,
            session_state,
        } => (messages.clone(), session_state.clone(), *seq, *turn),
        _ => unreachable!(),
    };

    let mut messages = messages;

    // Apply deltas after the snapshot.
    for entry in &entries[snapshot_idx + 1..] {
        match entry {
            CslEntry::TurnDelta {
                seq,
                turn,
                appended,
                state_patch,
            } => {
                messages.extend(appended.iter().cloned());
                if let Some(patch) = state_patch {
                    state.apply_patch(patch);
                }
                last_seq = *seq;
                last_turn = *turn;
            }
            CslEntry::Snapshot {
                seq,
                turn,
                messages: snap_messages,
                session_state,
            } => {
                // A later snapshot resets everything.
                messages = snap_messages.clone();
                state = session_state.clone();
                last_seq = *seq;
                last_turn = *turn;
            }
        }
    }

    Ok(MaterializedState {
        messages,
        session_state: state,
        last_seq,
        last_turn,
    })
}

// ─── Append metadata ───────────────────────────────────────────────────────

/// Metadata carried alongside each [`CslStore::append`] call.
/// `FileCslStore` ignores these (JSONL has no columns); `DbCslStore` writes
/// them to dedicated DB columns for audit/query.
#[derive(Debug, Clone, Default)]
pub struct AppendMeta {
    pub trace_id: Option<String>,
    pub message_count: Option<u32>,
}

// ─── Store trait ────────────────────────────────────────────────────────────

/// Persistence backend for the CSL. Implementations exist for local files
/// ([`file_store::FileCslStore`]) and database ([`db_store::DbCslStore`]).
#[async_trait]
pub trait CslStore: Send + Sync {
    /// Append an entry to the log for `session_id`.
    async fn append(
        &self,
        session_id: &str,
        entry: &CslEntry,
        meta: &AppendMeta,
    ) -> Result<(), CslStoreError>;

    /// Load entries from the latest [`Snapshot`](CslEntry::Snapshot) onward.
    /// Returns entries in seq order, starting with the Snapshot.
    /// Returns empty vec if no log exists or no Snapshot is found.
    async fn load_from_latest_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Vec<CslEntry>, CslStoreError>;

    /// Load entries with seq > `after_seq`, in order.
    async fn load_after(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<CslEntry>, CslStoreError>;

    /// Remove entries with seq < `before_seq`. Returns count of removed entries.
    async fn truncate_before(
        &self,
        session_id: &str,
        before_seq: u64,
    ) -> Result<u64, CslStoreError>;

    /// Fork: copy parent's entries (up to `fork_after_turn`) into a new session.
    /// The new session's log starts with a Snapshot containing the materialized
    /// state at `fork_after_turn`.
    async fn fork(
        &self,
        parent_session_id: &str,
        new_session_id: &str,
        fork_after_turn: u32,
    ) -> Result<u64, CslStoreError>;

    /// Return seq numbers of all Snapshot entries, in ascending order.
    /// Used by GC to find which snapshots to retain without deserializing payloads.
    ///
    /// The default implementation falls back to `load_after(0)` + filter, which
    /// deserializes every entry. Store backends with indexed metadata (e.g.
    /// [`DbCslStore`](db_store::DbCslStore) with `entry_type` column) should
    /// override this to avoid the deserialization cost.
    async fn snapshot_seqs(&self, session_id: &str) -> Result<Vec<u64>, CslStoreError> {
        validate_session_id(session_id)?;
        let entries = self.load_after(session_id, 0).await?;
        Ok(entries
            .iter()
            .filter(|e| e.is_snapshot())
            .map(|e| e.seq())
            .collect())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CslStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("materialize error: {0}")]
    Materialize(#[from] MaterializeError),
    #[error("invalid session_id: {0:?}")]
    InvalidSessionId(String),
    #[error("{0}")]
    Other(String),
}

pub(crate) fn validate_session_id(session_id: &str) -> Result<(), CslStoreError> {
    if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
        || session_id.bytes().any(|b| b.is_ascii_control())
    {
        return Err(CslStoreError::InvalidSessionId(session_id.to_string()));
    }
    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(seq: u64, turn: u32, messages: Vec<Value>) -> CslEntry {
        CslEntry::Snapshot {
            seq,
            turn,
            messages,
            session_state: SessionStateCompact::default(),
        }
    }

    fn snapshot_with_state(
        seq: u64,
        turn: u32,
        messages: Vec<Value>,
        state: SessionStateCompact,
    ) -> CslEntry {
        CslEntry::Snapshot {
            seq,
            turn,
            messages,
            session_state: state,
        }
    }

    fn delta(seq: u64, turn: u32, appended: Vec<Value>) -> CslEntry {
        CslEntry::TurnDelta {
            seq,
            turn,
            appended,
            state_patch: None,
        }
    }

    fn delta_with_patch(
        seq: u64,
        turn: u32,
        appended: Vec<Value>,
        patch: SessionStatePatch,
    ) -> CslEntry {
        CslEntry::TurnDelta {
            seq,
            turn,
            appended,
            state_patch: Some(patch),
        }
    }

    fn user_msg(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }

    fn assistant_msg(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }

    fn tool_call_msg(id: &str, name: &str) -> Value {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": id, "function": {"name": name, "arguments": "{}"}}]
        })
    }

    fn tool_result_msg(id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    // ── Core materialize tests ──────────────────────────────────────────────

    #[test]
    fn empty_log_returns_error() {
        let err = materialize(&[]).unwrap_err();
        assert!(matches!(err, MaterializeError::EmptyLog));
    }

    #[test]
    fn delta_without_snapshot_errors() {
        let entries = vec![delta(0, 1, vec![user_msg("hi")])];
        let err = materialize(&entries).unwrap_err();
        assert!(matches!(err, MaterializeError::MissingSnapshot(0)));
    }

    #[test]
    fn snapshot_only() {
        let msgs = vec![user_msg("hello"), assistant_msg("hi there")];
        let entries = vec![snapshot(0, 1, msgs.clone())];
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages, msgs);
        assert_eq!(state.last_seq, 0);
        assert_eq!(state.last_turn, 1);
    }

    #[test]
    fn snapshot_then_deltas() {
        let entries = vec![
            snapshot(0, 1, vec![user_msg("turn1"), assistant_msg("resp1")]),
            delta(1, 2, vec![user_msg("turn2"), assistant_msg("resp2")]),
            delta(2, 3, vec![user_msg("turn3"), assistant_msg("resp3")]),
        ];
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 6);
        assert_eq!(state.messages[0]["content"], "turn1");
        assert_eq!(state.messages[5]["content"], "resp3");
        assert_eq!(state.last_seq, 2);
        assert_eq!(state.last_turn, 3);
    }

    #[test]
    fn multiple_snapshots_uses_latest() {
        let entries = vec![
            snapshot(0, 1, vec![user_msg("old")]),
            delta(1, 2, vec![user_msg("delta_after_old")]),
            // Second snapshot resets state (this is a compaction fold).
            snapshot(2, 3, vec![user_msg("compacted_summary")]),
            delta(3, 4, vec![user_msg("new_turn")]),
        ];
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0]["content"], "compacted_summary");
        assert_eq!(state.messages[1]["content"], "new_turn");
        assert_eq!(state.last_seq, 3);
        assert_eq!(state.last_turn, 4);
    }

    #[test]
    fn tool_calls_and_results_preserved_in_delta() {
        let entries = vec![
            snapshot(0, 1, vec![user_msg("read the file")]),
            delta(
                1,
                1,
                vec![
                    tool_call_msg("c1", "read_file"),
                    tool_result_msg("c1", "fn main() {}"),
                    assistant_msg("The file contains a main function."),
                ],
            ),
        ];
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 4);
        assert!(state.messages[1].get("tool_calls").is_some());
        assert_eq!(state.messages[2]["role"], "tool");
        assert_eq!(state.messages[2]["content"], "fn main() {}");
    }

    // ── State patch tests ───────────────────────────────────────────────────

    #[test]
    fn state_patch_applied_incrementally() {
        let initial_state = SessionStateCompact {
            blocked_tools: vec!["bash".into()],
            budget_remaining_tokens: 100_000,
            ..Default::default()
        };
        let entries = vec![
            snapshot_with_state(0, 1, vec![user_msg("hi")], initial_state),
            delta_with_patch(
                1,
                2,
                vec![user_msg("turn2")],
                SessionStatePatch {
                    blocked_tools: Some(vec!["bash".into(), "write_file".into()]),
                    ..Default::default()
                },
            ),
            delta_with_patch(
                2,
                3,
                vec![user_msg("turn3")],
                SessionStatePatch {
                    recent_tools: Some(vec!["read_file".into()]),
                    ..Default::default()
                },
            ),
        ];
        let state = materialize(&entries).unwrap();
        assert_eq!(
            state.session_state.blocked_tools,
            vec!["bash", "write_file"]
        );
        assert_eq!(state.session_state.recent_tools, vec!["read_file"]);
        // budget_remaining_tokens unchanged from snapshot (no patch touched it)
        assert_eq!(state.session_state.budget_remaining_tokens, 100_000);
    }

    #[test]
    fn patch_on_default_state() {
        let mut state = SessionStateCompact::default();
        let patch = SessionStatePatch {
            blocked_tools: Some(vec!["x".into()]),
            approval_overrides: Some(Some(json!({"tool": "bash", "approved": true}))),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert_eq!(state.blocked_tools, vec!["x"]);
        assert_eq!(
            state.approval_overrides,
            Some(json!({"tool": "bash", "approved": true}))
        );
        assert!(state.continuity.is_none());
    }

    // ── Serde round-trip tests ──────────────────────────────────────────────

    #[test]
    fn serde_snapshot_roundtrip() {
        let entry = snapshot(42, 5, vec![user_msg("test")]);
        let json = serde_json::to_string(&entry).unwrap();
        let deser: CslEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, deser);
        assert!(json.contains(r#""type":"snapshot""#));
    }

    #[test]
    fn serde_turn_delta_roundtrip() {
        let entry = delta(1, 2, vec![assistant_msg("ok")]);
        let json = serde_json::to_string(&entry).unwrap();
        let deser: CslEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, deser);
        assert!(json.contains(r#""type":"turn_delta""#));
    }

    #[test]
    fn serde_delta_with_patch_omits_none_fields() {
        let entry = delta_with_patch(
            1,
            2,
            vec![],
            SessionStatePatch {
                blocked_tools: Some(vec!["x".into()]),
                ..Default::default()
            },
        );
        let json = serde_json::to_string(&entry).unwrap();
        // continuity, recent_tools, approval_overrides, interruption should be absent
        assert!(!json.contains("continuity"));
        assert!(!json.contains("recent_tools"));
        assert!(!json.contains("approval_overrides"));
        assert!(!json.contains("interruption"));
        // blocked_tools should be present
        assert!(json.contains("blocked_tools"));
    }

    #[test]
    fn serde_session_state_compact_defaults() {
        // An empty JSON object should deserialize to defaults
        let state: SessionStateCompact = serde_json::from_str("{}").unwrap();
        assert_eq!(state, SessionStateCompact::default());
    }

    // ── Entry accessor tests ────────────────────────────────────────────────

    #[test]
    fn entry_accessors() {
        let s = snapshot(10, 3, vec![]);
        assert_eq!(s.seq(), 10);
        assert_eq!(s.turn(), 3);
        assert!(s.is_snapshot());

        let d = delta(11, 4, vec![]);
        assert_eq!(d.seq(), 11);
        assert_eq!(d.turn(), 4);
        assert!(!d.is_snapshot());
    }

    // ── Edge cases ──────────────────────────────────────────────────────────

    #[test]
    fn snapshot_with_empty_messages() {
        let entries = vec![snapshot(0, 0, vec![])];
        let state = materialize(&entries).unwrap();
        assert!(state.messages.is_empty());
    }

    #[test]
    fn delta_with_empty_appended() {
        // A turn that only ran compaction and produced no new messages.
        let entries = vec![snapshot(0, 1, vec![user_msg("hi")]), delta(1, 2, vec![])];
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.last_seq, 1);
        assert_eq!(state.last_turn, 2);
    }

    #[test]
    fn many_deltas_between_snapshots() {
        let mut entries = vec![snapshot(0, 0, vec![])];
        for i in 1..=20u64 {
            entries.push(delta(
                i,
                i as u32,
                vec![
                    user_msg(&format!("turn{i}")),
                    assistant_msg(&format!("resp{i}")),
                ],
            ));
        }
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 40);
        assert_eq!(state.last_seq, 20);
        assert_eq!(state.last_turn, 20);
    }

    // ── apply_patch coverage for all fields ────────────────────────────────

    #[test]
    fn patch_applies_continuity_and_interruption() {
        let mut state = SessionStateCompact::default();
        let continuity = astra_turn_types::continuity::ContinuityState {
            goal: Default::default(),
            todos: Default::default(),
            facts: Default::default(),
            user_corrections: vec![],
            verification: Default::default(),
        };
        let patch = SessionStatePatch {
            continuity: Some(Some(continuity.clone())),
            interruption: Some(Some(
                json!({"kind": "budget_exhausted", "resume_action": "continue"}),
            )),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert_eq!(state.continuity, Some(continuity));
        assert_eq!(
            state.interruption,
            Some(json!({"kind": "budget_exhausted", "resume_action": "continue"}))
        );
        assert!(state.blocked_tools.is_empty());
        assert!(state.recent_tools.is_empty());
    }

    #[test]
    fn patch_overwrites_all_fields() {
        let mut state = SessionStateCompact {
            blocked_tools: vec!["old".into()],
            recent_tools: vec!["old_tool".into()],
            approval_overrides: Some(json!({"old": true})),
            interruption: Some(json!({"old": "reason"})),
            ..Default::default()
        };
        let patch = SessionStatePatch {
            continuity: None,
            blocked_tools: Some(vec!["new".into()]),
            recent_tools: Some(vec!["new_tool".into()]),
            approval_overrides: Some(Some(json!({"new": true}))),
            interruption: Some(Some(json!({"new": "reason"}))),
            budget_remaining_tokens: Some(42_000),
            budget_remaining_rounds: Some(3),
            consecutive_ctx_errors: Some(5),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert_eq!(state.blocked_tools, vec!["new"]);
        assert_eq!(state.recent_tools, vec!["new_tool"]);
        assert_eq!(state.approval_overrides, Some(json!({"new": true})));
        assert_eq!(state.interruption, Some(json!({"new": "reason"})));
        assert_eq!(state.budget_remaining_tokens, 42_000);
        assert_eq!(state.budget_remaining_rounds, 3);
        assert_eq!(state.consecutive_ctx_errors, 5);
        assert!(state.continuity.is_none());
    }

    // ── State patch round-trip via materialize ─────────────────────────────

    #[test]
    fn state_patch_roundtrip_through_materialize() {
        let entries = vec![
            snapshot(0, 1, vec![user_msg("hi")]),
            delta_with_patch(
                1,
                2,
                vec![user_msg("turn2")],
                SessionStatePatch {
                    blocked_tools: Some(vec!["bash".into(), "write".into()]),
                    recent_tools: Some(vec!["read_file".into()]),
                    approval_overrides: Some(Some(json!({"tool": "bash", "approved": true}))),
                    interruption: Some(Some(json!({"kind": "paused"}))),
                    ..Default::default()
                },
            ),
        ];
        let state = materialize(&entries).unwrap();
        assert_eq!(state.session_state.blocked_tools, vec!["bash", "write"]);
        assert_eq!(state.session_state.recent_tools, vec!["read_file"]);
        assert_eq!(
            state.session_state.approval_overrides,
            Some(json!({"tool": "bash", "approved": true}))
        );
        assert_eq!(
            state.session_state.interruption,
            Some(json!({"kind": "paused"}))
        );
    }

    // ── Serde round-trip with populated SessionStateCompact ────────────────

    #[test]
    fn serde_snapshot_with_full_state_roundtrip() {
        let state = SessionStateCompact {
            continuity: None,
            blocked_tools: vec!["bash".into()],
            recent_tools: vec!["read_file".into()],
            approval_overrides: Some(json!({"tool": "bash"})),
            compaction_tracker: Some(json!({"version": 1})),
            budget_remaining_tokens: 50_000,
            budget_remaining_rounds: 8,
            consecutive_ctx_errors: 2,
            delegation: Some(DelegationCompact {
                id: "del-1".into(),
                pattern: "review".into(),
                completed_sub_runs: vec![],
            }),
            interruption: Some(json!({"kind": "paused"})),
        };
        let entry = snapshot_with_state(0, 1, vec![user_msg("test")], state.clone());
        let json_str = serde_json::to_string(&entry).unwrap();
        let deser: CslEntry = serde_json::from_str(&json_str).unwrap();
        assert_eq!(entry, deser);
        if let CslEntry::Snapshot { session_state, .. } = &deser {
            assert_eq!(session_state, &state);
        } else {
            panic!("expected Snapshot");
        }
    }

    // ── Budget/error fields in patch ───────────────────────────────────────

    #[test]
    fn patch_applies_budget_and_error_fields() {
        let mut state = SessionStateCompact {
            budget_remaining_tokens: 100_000,
            budget_remaining_rounds: 10,
            consecutive_ctx_errors: 0,
            ..Default::default()
        };
        let patch = SessionStatePatch {
            budget_remaining_tokens: Some(50_000),
            budget_remaining_rounds: Some(7),
            consecutive_ctx_errors: Some(2),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert_eq!(state.budget_remaining_tokens, 50_000);
        assert_eq!(state.budget_remaining_rounds, 7);
        assert_eq!(state.consecutive_ctx_errors, 2);
    }

    #[test]
    fn patch_none_budget_fields_unchanged() {
        let mut state = SessionStateCompact {
            budget_remaining_tokens: 100_000,
            budget_remaining_rounds: 10,
            consecutive_ctx_errors: 3,
            ..Default::default()
        };
        let patch = SessionStatePatch {
            blocked_tools: Some(vec!["bash".into()]),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert_eq!(state.budget_remaining_tokens, 100_000);
        assert_eq!(state.budget_remaining_rounds, 10);
        assert_eq!(state.consecutive_ctx_errors, 3);
    }

    // ── CSL lifecycle simulation (mirrors runtime persist → load → restore) ─

    #[test]
    fn multi_turn_lifecycle_with_state_patches() {
        let entries = vec![
            // Turn 1: initial snapshot
            CslEntry::Snapshot {
                seq: 0,
                turn: 1,
                messages: vec![user_msg("turn1"), assistant_msg("resp1")],
                session_state: SessionStateCompact {
                    budget_remaining_tokens: 100_000,
                    budget_remaining_rounds: 10,
                    ..Default::default()
                },
            },
            // Turn 2: delta with state changes
            CslEntry::TurnDelta {
                seq: 1,
                turn: 2,
                appended: vec![user_msg("turn2"), assistant_msg("resp2")],
                state_patch: Some(SessionStatePatch {
                    blocked_tools: Some(vec!["bash".into()]),
                    budget_remaining_tokens: Some(80_000),
                    budget_remaining_rounds: Some(9),
                    ..Default::default()
                }),
            },
            // Turn 3: delta with more changes
            CslEntry::TurnDelta {
                seq: 2,
                turn: 3,
                appended: vec![user_msg("turn3"), assistant_msg("resp3")],
                state_patch: Some(SessionStatePatch {
                    recent_tools: Some(vec!["read_file".into(), "bash".into()]),
                    budget_remaining_tokens: Some(60_000),
                    budget_remaining_rounds: Some(8),
                    consecutive_ctx_errors: Some(1),
                    ..Default::default()
                }),
            },
        ];

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 6);
        assert_eq!(state.last_seq, 2);
        assert_eq!(state.last_turn, 3);
        // Turn 2 patch set blocked_tools
        assert_eq!(state.session_state.blocked_tools, vec!["bash"]);
        // Turn 3 patch set recent_tools
        assert_eq!(state.session_state.recent_tools, vec!["read_file", "bash"]);
        // Budget decremented across turns
        assert_eq!(state.session_state.budget_remaining_tokens, 60_000);
        assert_eq!(state.session_state.budget_remaining_rounds, 8);
        assert_eq!(state.session_state.consecutive_ctx_errors, 1);
    }

    #[test]
    fn snapshot_resets_state_accumulated_by_patches() {
        let entries = vec![
            CslEntry::Snapshot {
                seq: 0,
                turn: 1,
                messages: vec![user_msg("t1")],
                session_state: SessionStateCompact::default(),
            },
            CslEntry::TurnDelta {
                seq: 1,
                turn: 2,
                appended: vec![user_msg("t2")],
                state_patch: Some(SessionStatePatch {
                    blocked_tools: Some(vec!["bash".into()]),
                    budget_remaining_tokens: Some(50_000),
                    ..Default::default()
                }),
            },
            // Compaction snapshot — resets everything
            CslEntry::Snapshot {
                seq: 2,
                turn: 3,
                messages: vec![user_msg("compacted")],
                session_state: SessionStateCompact {
                    budget_remaining_tokens: 90_000,
                    budget_remaining_rounds: 7,
                    ..Default::default()
                },
            },
            CslEntry::TurnDelta {
                seq: 3,
                turn: 4,
                appended: vec![user_msg("t4")],
                state_patch: Some(SessionStatePatch {
                    consecutive_ctx_errors: Some(1),
                    ..Default::default()
                }),
            },
        ];

        let state = materialize(&entries).unwrap();
        // Post-compaction snapshot reset — blocked_tools from turn 2 gone
        assert!(state.session_state.blocked_tools.is_empty());
        // Budget from compaction snapshot, not from turn 2 patch
        assert_eq!(state.session_state.budget_remaining_tokens, 90_000);
        assert_eq!(state.session_state.budget_remaining_rounds, 7);
        // Turn 4 delta applied on top
        assert_eq!(state.session_state.consecutive_ctx_errors, 1);
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn serde_patch_with_budget_fields_roundtrip() {
        let entry = delta_with_patch(
            1,
            2,
            vec![user_msg("hello")],
            SessionStatePatch {
                budget_remaining_tokens: Some(42_000),
                budget_remaining_rounds: Some(5),
                consecutive_ctx_errors: Some(3),
                ..Default::default()
            },
        );
        let json_str = serde_json::to_string(&entry).unwrap();
        let deser: CslEntry = serde_json::from_str(&json_str).unwrap();
        assert_eq!(entry, deser);
        assert!(json_str.contains("budget_remaining_tokens"));
        assert!(json_str.contains("budget_remaining_rounds"));
        assert!(json_str.contains("consecutive_ctx_errors"));
        // Fields not set should be absent
        assert!(!json_str.contains("continuity"));
    }

    // ── Bug fix: apply_patch must be able to clear continuity/delegation ──

    #[test]
    fn patch_clears_continuity_via_explicit_none() {
        let continuity = astra_turn_types::continuity::ContinuityState {
            goal: Default::default(),
            todos: Default::default(),
            facts: Default::default(),
            user_corrections: vec![],
            verification: Default::default(),
        };
        let mut state = SessionStateCompact {
            continuity: Some(continuity),
            ..Default::default()
        };

        // A patch that explicitly says "clear continuity" must result in None.
        let patch = SessionStatePatch {
            continuity: Some(None),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert!(
            state.continuity.is_none(),
            "apply_patch should clear continuity when patch has Some(None)"
        );
    }

    #[test]
    fn patch_clears_delegation_via_explicit_none() {
        let mut state = SessionStateCompact {
            delegation: Some(DelegationCompact {
                id: "d1".into(),
                pattern: "review".into(),
                completed_sub_runs: vec![],
            }),
            ..Default::default()
        };

        let patch = SessionStatePatch {
            delegation: Some(None),
            ..Default::default()
        };
        state.apply_patch(&patch);
        assert!(
            state.delegation.is_none(),
            "apply_patch should clear delegation when patch has Some(None)"
        );
    }

    #[test]
    fn patch_none_continuity_leaves_existing_untouched() {
        let continuity = astra_turn_types::continuity::ContinuityState {
            goal: Default::default(),
            todos: Default::default(),
            facts: Default::default(),
            user_corrections: vec![],
            verification: Default::default(),
        };
        let mut state = SessionStateCompact {
            continuity: Some(continuity.clone()),
            ..Default::default()
        };

        // patch.continuity = None means "unchanged".
        let patch = SessionStatePatch::default();
        state.apply_patch(&patch);
        assert_eq!(state.continuity, Some(continuity));
    }

    #[test]
    fn serde_patch_continuity_clear_roundtrip() {
        let patch = SessionStatePatch {
            continuity: Some(None),
            ..Default::default()
        };
        let json_str = serde_json::to_string(&patch).unwrap();
        assert!(
            json_str.contains("continuity"),
            "Some(None) must serialize as null, not be omitted"
        );
        let deser: SessionStatePatch = serde_json::from_str(&json_str).unwrap();
        assert_eq!(
            deser.continuity,
            Some(None),
            "JSON null must deserialize to Some(None)"
        );
    }

    #[test]
    fn serde_patch_delegation_clear_roundtrip() {
        let patch = SessionStatePatch {
            delegation: Some(None),
            ..Default::default()
        };
        let json_str = serde_json::to_string(&patch).unwrap();
        assert!(
            json_str.contains("delegation"),
            "Some(None) must serialize as null, not be omitted"
        );
        let deser: SessionStatePatch = serde_json::from_str(&json_str).unwrap();
        assert_eq!(
            deser.delegation,
            Some(None),
            "JSON null must deserialize to Some(None)"
        );
    }

    #[test]
    fn validate_session_id_rejects_invalid() {
        for bad in [
            "",
            "../etc/passwd",
            "foo/bar",
            "a\\b",
            "..",
            "has\0nul",
            "has\nnewline",
            "has\ttab",
            "has\x7Fdel",
        ] {
            match validate_session_id(bad) {
                Err(CslStoreError::InvalidSessionId(_)) => {}
                other => panic!("{bad:?}: expected InvalidSessionId, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_session_id_accepts_valid() {
        for good in [
            "abc123",
            "550e8400-e29b-41d4-a716-446655440000",
            "session_with-dashes.and.dots",
        ] {
            assert!(
                validate_session_id(good).is_ok(),
                "{good:?} should be accepted"
            );
        }
    }
}
