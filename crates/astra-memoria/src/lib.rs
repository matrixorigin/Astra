//! Canonical Memoria boundary shared by CLI, edge, and server runtimes.
//!
//! Transports implement [`MemoriaPort`]. Prompt-facing tool gateways adapt
//! cognitive verbs to a port or HTTP protocol, but do not own a second memory
//! model. Ephemeral attention, surfaced-memory deduplication, and recall
//! attribution live in one session-keyed runtime state so every deployment
//! shape observes the same semantics.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical owner + session boundary for session-scoped memory operations.
///
/// `user_id` and `session_id` are distinct identities. A transport may derive
/// the owner from authentication, but it must never replace the durable
/// session id with that owner id. Keeping the pair typed makes that invariant
/// explicit at CLI, edge, and server boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryScope {
    pub user_id: String,
    pub session_id: String,
}

impl MemoryScope {
    pub fn new(user_id: impl Into<String>, session_id: impl Into<String>) -> Result<Self, String> {
        let user_id = exact_scope_component("user_id", user_id.into())?;
        let session_id = exact_scope_component("session_id", session_id.into())?;
        Ok(Self {
            user_id,
            session_id,
        })
    }
}

fn exact_scope_component(field: &str, value: String) -> Result<String, String> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(format!(
            "memory scope {field} must be a non-empty exact string without surrounding whitespace or control characters"
        ));
    }
    Ok(value)
}

/// Fail-closed validation for a strict session recall response.
///
/// Memoria may return either a top-level array or `{ "memories": [...] }`.
/// Every item must echo both owner and session identity. A missing identity is
/// treated as a boundary violation rather than as a legacy response: callers
/// cannot safely surface content whose ownership was not proven.
pub fn validate_strict_recall_payload(
    payload: &Value,
    expected: &MemoryScope,
) -> Result<(), String> {
    let entries = strict_recall_entries(payload)?;

    for (index, entry) in entries.iter().enumerate() {
        validate_recall_entry_identity(entry, expected, index)?;
    }
    Ok(())
}

/// Validate the session half of a strict recall at an edge that does not own
/// the authenticated user id. The server must still call
/// [`validate_strict_recall_payload`] with the full owner/session pair.
pub fn validate_strict_recall_session_payload(
    payload: &Value,
    expected_session_id: &str,
) -> Result<(), String> {
    let expected_session_id = exact_scope_component("session_id", expected_session_id.to_string())?;
    for (index, entry) in strict_recall_entries(payload)?.iter().enumerate() {
        let object = entry.as_object().ok_or_else(|| {
            format!("memory_scope_violation: strict recall item {index} is not an object")
        })?;
        if object.get("session_id").and_then(Value::as_str) != Some(expected_session_id.as_str()) {
            let memory_id = object
                .get("memory_id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            return Err(format!(
                "memory_scope_violation: strict recall item {index} ({memory_id}) has invalid session_id"
            ));
        }
    }
    Ok(())
}

fn strict_recall_entries(payload: &Value) -> Result<&[Value], String> {
    payload
        .as_array()
        .or_else(|| payload.get("memories").and_then(Value::as_array))
        .map(Vec::as_slice)
        .ok_or_else(|| {
            "memory_scope_violation: strict recall response is not an array or memories envelope"
                .to_string()
        })
}

fn validate_recall_entry_identity(
    entry: &Value,
    expected: &MemoryScope,
    index: usize,
) -> Result<(), String> {
    let object = entry.as_object().ok_or_else(|| {
        format!("memory_scope_violation: strict recall item {index} is not an object")
    })?;
    for (field, expected_value) in [
        ("user_id", expected.user_id.as_str()),
        ("session_id", expected.session_id.as_str()),
    ] {
        let actual = object.get(field).and_then(Value::as_str);
        if actual != Some(expected_value) {
            let memory_id = object
                .get("memory_id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            return Err(format!(
                "memory_scope_violation: strict recall item {index} ({memory_id}) has invalid {field}"
            ));
        }
    }
    Ok(())
}

/// Typed equivalent of [`validate_strict_recall_payload`] for runtime ports.
pub fn validate_strict_memories(
    memories: &[MemoriaMemory],
    expected: &MemoryScope,
) -> Result<(), String> {
    for (index, memory) in memories.iter().enumerate() {
        if memory.user_id.as_deref() != Some(expected.user_id.as_str()) {
            return Err(format!(
                "memory_scope_violation: strict recall item {index} ({}) has invalid user_id",
                memory.memory_id
            ));
        }
        if memory.session_id.as_deref() != Some(expected.session_id.as_str()) {
            return Err(format!(
                "memory_scope_violation: strict recall item {index} ({}) has invalid session_id",
                memory.memory_id
            ));
        }
    }
    Ok(())
}

/// Provider-neutral Memoria operations used by runtime orchestration.
#[async_trait::async_trait]
pub trait MemoriaPort: Send + Sync {
    /// Return a transport bound to an authenticated owner.
    ///
    /// Server runtimes are multi-tenant and must scope the port before placing
    /// it in request state.  Implementations that cannot establish a new owner
    /// boundary fail closed instead of silently reusing another tenant's
    /// credentials.
    fn bind_owner(&self, _user_id: &str) -> Result<Arc<dyn MemoriaPort>, String> {
        Err("Memoria transport does not support authenticated owner rebinding".to_string())
    }

    async fn retrieve_for_prompt(
        &self,
        query: &str,
        _user_id: &str,
        session_id: &str,
        top_k: usize,
    ) -> Result<Vec<MemoriaMemory>, String> {
        self.retrieve(
            query,
            (!session_id.trim().is_empty()).then_some(session_id),
            top_k,
        )
        .await
    }

    async fn retrieve(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<MemoriaMemory>, String> {
        self.retrieve_ext(query, session_id, top_k, false).await
    }

    async fn retrieve_ext(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
        filter_session: bool,
    ) -> Result<Vec<MemoriaMemory>, String>;

    async fn retrieve_scoped_typed(
        &self,
        query: &str,
        session_id: &str,
        top_k: usize,
        memory_types: &[&str],
    ) -> Result<Vec<MemoriaMemory>, String> {
        let memories = self
            .retrieve_ext(query, Some(session_id), top_k, true)
            .await?;
        for (index, memory) in memories.iter().enumerate() {
            if memory.session_id.as_deref() != Some(session_id) {
                return Err(format!(
                    "memory_scope_violation: strict recall item {index} ({}) has invalid session_id",
                    memory.memory_id
                ));
            }
        }
        if memory_types.is_empty() {
            return Ok(memories);
        }
        Ok(memories
            .into_iter()
            .filter(|memory| memory_types.contains(&memory.memory_type.as_str()))
            .collect())
    }

    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
    ) -> Result<String, String>;

    async fn purge_working(&self, session_id: &str) -> Result<u64, String>;

    async fn purge_memory_types(
        &self,
        _session_id: &str,
        _memory_types: &[&str],
    ) -> Result<u64, String> {
        Ok(0)
    }

    async fn delete(&self, _memory_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn store_episode(&self, _session_id: &str, _overview: &str) -> Result<String, String> {
        Ok(String::new())
    }

    async fn store_scene(
        &self,
        _session_id: &str,
        _signal: &str,
        _summary: &str,
    ) -> Result<String, String> {
        Ok(String::new())
    }

    async fn reflect_session(
        &self,
        _session_id: &str,
        _force: bool,
    ) -> Result<ReflectSummary, String> {
        Ok(ReflectSummary::default())
    }

    async fn focus(
        &self,
        session_id: &str,
        focus_type: &str,
        value: &str,
        boost: Option<f64>,
        ttl_secs: Option<i64>,
    ) -> Result<(), String> {
        memoria_runtime_state().set_focus(
            session_id,
            focus_type,
            value,
            boost.unwrap_or(1.5),
            ttl_secs.unwrap_or(3600).max(1) as u64,
        )
    }

    async fn feedback(
        &self,
        _memory_id: &str,
        _signal: &str,
        _context: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReflectSummary {
    pub synthesized: bool,
    pub candidates: u64,
    pub candidate_payloads: Vec<ReflectCandidate>,
    pub diagnostics: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReflectCandidate {
    pub signal: String,
    pub importance: f64,
    pub summary: String,
}

pub fn parse_reflect_candidates(data: &Value) -> Vec<ReflectCandidate> {
    let Some(candidates) = data.get("candidates").and_then(Value::as_array) else {
        return Vec::new();
    };
    candidates
        .iter()
        .filter_map(|entry| {
            let signal = entry
                .get("signal")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let importance = entry
                .get("importance")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let summary = entry
                .get("memories")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|memory| memory.get("content").and_then(Value::as_str))
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .map(|content| format!("- {content}"))
                .collect::<Vec<_>>()
                .join("\n");
            (!summary.is_empty() || !signal.is_empty()).then_some(ReflectCandidate {
                signal,
                importance,
                summary,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoriaMemory {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    #[serde(default)]
    pub retrieval_score: Option<f64>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub trust_tier: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

impl MemoriaMemory {
    pub fn age_days(&self) -> Option<i64> {
        let timestamp = self.observed_at.as_deref().or(self.updated_at.as_deref())?;
        let observed = chrono::DateTime::parse_from_rfc3339(timestamp).ok()?;
        let age = chrono::Utc::now() - observed.with_timezone(&chrono::Utc);
        Some(age.num_days().max(0))
    }

    pub fn age_label(&self) -> Option<String> {
        match self.age_days()? {
            0 => Some("today".into()),
            1 => Some("yesterday".into()),
            days => Some(format!("{days} days ago")),
        }
    }

    pub fn freshness_suffix(&self) -> String {
        self.age_days()
            .map(|days| astra_turn_types::freshness_suffix_for(days, self.trust_tier.as_deref()))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct FocusHint {
    pub focus_type: String,
    pub value: String,
    pub boost: f64,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct RecallSnapshot {
    pub session_id: String,
    pub memory_ids: Vec<String>,
    pub turn: u32,
    pub at: Instant,
}

pub const MAX_RECALL_LEDGER_PER_SESSION: usize = 16;
pub const MAX_RECALL_IDS_PER_SNAPSHOT: usize = 64;
pub const MAX_FOCUS_HINTS_PER_SESSION: usize = 16;
pub const MAX_SEEN_IDS_PER_SESSION: usize = 512;
pub const MAX_RUNTIME_SESSIONS: usize = 1024;

const MAX_FOCUS_VALUE_CHARS: usize = 512;
const RUNTIME_SESSION_IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
struct SessionRuntimeState {
    focus: Vec<FocusHint>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    recalls: VecDeque<RecallSnapshot>,
    last_touched: Instant,
}

impl SessionRuntimeState {
    fn new(now: Instant) -> Self {
        Self {
            focus: Vec::new(),
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            recalls: VecDeque::new(),
            last_touched: now,
        }
    }

    fn is_empty(&self) -> bool {
        self.focus.is_empty() && self.seen.is_empty() && self.recalls.is_empty()
    }
}

/// Ephemeral, process-local memory state keyed by durable session identity.
///
/// It is intentionally not prompt history or durable memory. Explicit
/// [`Self::reset_session`] calls release state eagerly, while idle eviction and
/// hard per-session/global bounds keep correctness independent of every caller
/// observing a particular shutdown path.
#[derive(Debug, Default)]
pub struct MemoriaRuntimeState {
    sessions: RwLock<HashMap<String, SessionRuntimeState>>,
}

impl MemoriaRuntimeState {
    fn session_key(session_id: &str) -> Option<&str> {
        let session_id = session_id.trim();
        (!session_id.is_empty()).then_some(session_id)
    }

    fn prune_idle_sessions(sessions: &mut HashMap<String, SessionRuntimeState>, now: Instant) {
        sessions.retain(|_, state| {
            now.saturating_duration_since(state.last_touched) <= RUNTIME_SESSION_IDLE_TTL
        });
    }

    fn session_mut<'a>(
        sessions: &'a mut HashMap<String, SessionRuntimeState>,
        session_id: &str,
        now: Instant,
    ) -> &'a mut SessionRuntimeState {
        Self::prune_idle_sessions(sessions, now);
        if !sessions.contains_key(session_id) && sessions.len() >= MAX_RUNTIME_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, state)| state.last_touched)
                .map(|(session_id, _)| session_id.clone());
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        let state = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionRuntimeState::new(now));
        state.last_touched = now;
        state
    }

    pub fn set_focus(
        &self,
        session_id: &str,
        focus_type: &str,
        value: &str,
        boost: f64,
        ttl_secs: u64,
    ) -> Result<(), String> {
        let session_id = Self::session_key(session_id)
            .ok_or_else(|| "focus requires a non-empty session_id".to_string())?;
        if !matches!(focus_type, "topic" | "tag" | "memory_id" | "session") {
            return Err(format!(
                "invalid focus_type {focus_type:?}; expected topic/tag/memory_id/session"
            ));
        }
        let value = value.trim();
        if value.is_empty() {
            return Err("focus value must not be empty".to_string());
        }
        if value.chars().count() > MAX_FOCUS_VALUE_CHARS {
            return Err(format!(
                "focus value must not exceed {MAX_FOCUS_VALUE_CHARS} characters"
            ));
        }
        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| "focus state is unavailable".to_string())?;
        let state = Self::session_mut(&mut sessions, session_id, Instant::now());
        state
            .focus
            .retain(|hint| !(hint.focus_type == focus_type && hint.value == value));
        if state.focus.len() >= MAX_FOCUS_HINTS_PER_SESSION {
            state.focus.remove(0);
        }
        state.focus.push(FocusHint {
            focus_type: focus_type.to_string(),
            value: value.to_string(),
            boost,
            expires_at: Instant::now() + Duration::from_secs(ttl_secs.max(1)),
        });
        Ok(())
    }

    pub fn active_focus(&self, session_id: &str) -> Vec<FocusHint> {
        let Some(session_id) = Self::session_key(session_id) else {
            return Vec::new();
        };
        let Ok(mut sessions) = self.sessions.write() else {
            return Vec::new();
        };
        let now = Instant::now();
        Self::prune_idle_sessions(&mut sessions, now);
        let Some(state) = sessions.get_mut(session_id) else {
            return Vec::new();
        };
        state.last_touched = now;
        state.focus.retain(|hint| hint.expires_at > now);
        let focus = state.focus.clone();
        if state.is_empty() {
            sessions.remove(session_id);
        }
        focus
    }

    pub fn record_seen(&self, session_id: &str, ids: impl IntoIterator<Item = String>) {
        let Some(session_id) = Self::session_key(session_id) else {
            return;
        };
        let ids = ids
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        let Ok(mut sessions) = self.sessions.write() else {
            return;
        };
        let state = Self::session_mut(&mut sessions, session_id, Instant::now());
        for id in ids {
            if state.seen.insert(id.clone()) {
                state.seen_order.push_back(id);
            }
            while state.seen_order.len() > MAX_SEEN_IDS_PER_SESSION {
                if let Some(expired) = state.seen_order.pop_front() {
                    state.seen.remove(&expired);
                }
            }
        }
    }

    pub fn seen_snapshot(&self, session_id: &str) -> HashSet<String> {
        let Some(session_id) = Self::session_key(session_id) else {
            return HashSet::new();
        };
        let Ok(mut sessions) = self.sessions.write() else {
            return HashSet::new();
        };
        let now = Instant::now();
        Self::prune_idle_sessions(&mut sessions, now);
        let Some(state) = sessions.get_mut(session_id) else {
            return HashSet::new();
        };
        state.last_touched = now;
        state.seen.clone()
    }

    pub fn record_recall(&self, session_id: &str, turn: u32, memory_ids: Vec<String>) {
        let Some(session_id) = Self::session_key(session_id) else {
            return;
        };
        let mut unique = HashSet::new();
        let memory_ids = memory_ids
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .filter(|id| unique.insert(id.clone()))
            .take(MAX_RECALL_IDS_PER_SNAPSHOT)
            .collect::<Vec<_>>();
        if memory_ids.is_empty() {
            return;
        }
        let Ok(mut sessions) = self.sessions.write() else {
            return;
        };
        let state = Self::session_mut(&mut sessions, session_id, Instant::now());
        if state.recalls.len() >= MAX_RECALL_LEDGER_PER_SESSION {
            state.recalls.pop_front();
        }
        state.recalls.push_back(RecallSnapshot {
            session_id: session_id.to_string(),
            memory_ids,
            turn,
            at: Instant::now(),
        });
    }

    pub fn drain_recalls(
        &self,
        session_id: &str,
        max_age: Option<Duration>,
    ) -> Vec<RecallSnapshot> {
        let Some(session_id) = Self::session_key(session_id) else {
            return Vec::new();
        };
        let Ok(mut sessions) = self.sessions.write() else {
            return Vec::new();
        };
        let now = Instant::now();
        Self::prune_idle_sessions(&mut sessions, now);
        let drained = sessions
            .get_mut(session_id)
            .map(|state| {
                state.last_touched = now;
                std::mem::take(&mut state.recalls)
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|snapshot| max_age.is_none_or(|max| snapshot.at.elapsed() <= max))
            .collect();
        if sessions
            .get(session_id)
            .is_some_and(SessionRuntimeState::is_empty)
        {
            sessions.remove(session_id);
        }
        drained
    }

    pub fn pending_recall_count(&self, session_id: &str) -> usize {
        let Some(session_id) = Self::session_key(session_id) else {
            return 0;
        };
        let Ok(mut sessions) = self.sessions.write() else {
            return 0;
        };
        let now = Instant::now();
        Self::prune_idle_sessions(&mut sessions, now);
        let Some(state) = sessions.get_mut(session_id) else {
            return 0;
        };
        state.last_touched = now;
        state.recalls.len()
    }

    pub fn reset_focus(&self, session_id: &str) {
        let Some(session_id) = Self::session_key(session_id) else {
            return;
        };
        if let Ok(mut sessions) = self.sessions.write()
            && let Some(state) = sessions.get_mut(session_id)
        {
            state.focus.clear();
            if state.is_empty() {
                sessions.remove(session_id);
            }
        }
    }

    pub fn reset_seen(&self, session_id: &str) {
        let Some(session_id) = Self::session_key(session_id) else {
            return;
        };
        if let Ok(mut sessions) = self.sessions.write()
            && let Some(state) = sessions.get_mut(session_id)
        {
            state.seen.clear();
            state.seen_order.clear();
            if state.is_empty() {
                sessions.remove(session_id);
            }
        }
    }

    pub fn reset_recalls(&self, session_id: &str) {
        let Some(session_id) = Self::session_key(session_id) else {
            return;
        };
        if let Ok(mut sessions) = self.sessions.write()
            && let Some(state) = sessions.get_mut(session_id)
        {
            state.recalls.clear();
            if state.is_empty() {
                sessions.remove(session_id);
            }
        }
    }

    pub fn reset_session(&self, session_id: &str) {
        let Some(session_id) = Self::session_key(session_id) else {
            return;
        };
        if let Ok(mut sessions) = self.sessions.write() {
            sessions.remove(session_id);
        }
    }
}

pub fn memoria_runtime_state() -> &'static MemoriaRuntimeState {
    static STATE: OnceLock<MemoriaRuntimeState> = OnceLock::new();
    STATE.get_or_init(MemoriaRuntimeState::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_scope_keeps_owner_and_session_as_distinct_exact_identities() {
        let scope = MemoryScope::new("user-1", "session-1").unwrap();
        assert_eq!(scope.user_id, "user-1");
        assert_eq!(scope.session_id, "session-1");
        assert!(MemoryScope::new("user-1", "user-1").is_ok());
        assert!(MemoryScope::new(" user-1", "session-1").is_err());
        assert!(MemoryScope::new("user-1", "").is_err());
    }

    #[test]
    fn strict_recall_payload_rejects_foreign_or_unproven_identity() {
        let scope = MemoryScope::new("user-1", "session-1").unwrap();
        validate_strict_recall_payload(
            &json!([{
                "memory_id": "m-ok",
                "content": "safe",
                "user_id": "user-1",
                "session_id": "session-1"
            }]),
            &scope,
        )
        .unwrap();

        for payload in [
            json!([{
                "memory_id": "m-foreign-session",
                "user_id": "user-1",
                "session_id": "session-2"
            }]),
            json!([{
                "memory_id": "m-foreign-user",
                "user_id": "user-2",
                "session_id": "session-1"
            }]),
            json!([{
                "memory_id": "m-missing-owner",
                "session_id": "session-1"
            }]),
        ] {
            let error = validate_strict_recall_payload(&payload, &scope)
                .expect_err("strict recall must fail closed");
            assert!(error.starts_with("memory_scope_violation:"), "{error}");
            assert!(!error.contains("safe"));
        }
    }

    #[test]
    fn strict_recall_payload_accepts_memories_envelope_and_empty_results() {
        let scope = MemoryScope::new("user-1", "session-1").unwrap();
        validate_strict_recall_payload(&json!([]), &scope).unwrap();
        validate_strict_recall_payload(&json!({"memories": []}), &scope).unwrap();
        assert!(validate_strict_recall_payload(&json!({"items": []}), &scope).is_err());
    }

    #[test]
    fn edge_session_validation_rejects_foreign_session_without_needing_owner_identity() {
        validate_strict_recall_session_payload(
            &json!([{"memory_id": "m1", "session_id": "session-1"}]),
            "session-1",
        )
        .unwrap();
        assert!(
            validate_strict_recall_session_payload(
                &json!([{"memory_id": "m1", "session_id": "session-2"}]),
                "session-1",
            )
            .is_err()
        );
    }

    #[test]
    fn session_state_closes_focus_seen_and_recall_lifecycle() {
        let state = MemoriaRuntimeState::default();
        state.set_focus("s", "topic", "rust", 2.0, 60).unwrap();
        state.record_seen("s", ["m1".to_string()]);
        state.record_recall("s", 3, vec!["m1".to_string()]);

        assert_eq!(state.active_focus("s").len(), 1);
        assert!(state.seen_snapshot("s").contains("m1"));
        assert_eq!(state.pending_recall_count("s"), 1);

        state.reset_session("s");
        assert!(state.active_focus("s").is_empty());
        assert!(state.seen_snapshot("s").is_empty());
        assert_eq!(state.pending_recall_count("s"), 0);
    }

    #[test]
    fn recall_ledger_is_bounded_and_fifo() {
        let state = MemoriaRuntimeState::default();
        for turn in 0..20 {
            state.record_recall("s", turn, vec![format!("m{turn}")]);
        }
        let drained = state.drain_recalls("s", None);
        assert_eq!(drained.len(), MAX_RECALL_LEDGER_PER_SESSION);
        assert_eq!(drained.first().map(|entry| entry.turn), Some(4));
        assert_eq!(drained.last().map(|entry| entry.turn), Some(19));
    }

    #[test]
    fn empty_session_identity_never_creates_process_global_focus() {
        let state = MemoriaRuntimeState::default();
        assert!(state.set_focus("", "topic", "private", 2.0, 60).is_err());
        assert!(state.set_focus("   ", "topic", "private", 2.0, 60).is_err());
        assert!(state.active_focus("").is_empty());
        assert!(state.sessions.read().unwrap().is_empty());
    }

    #[test]
    fn ephemeral_state_is_bounded_without_lifecycle_cleanup() {
        let state = MemoriaRuntimeState::default();
        state.record_seen(
            "bounded",
            (0..MAX_SEEN_IDS_PER_SESSION + 8).map(|index| format!("m{index}")),
        );
        let seen = state.seen_snapshot("bounded");
        assert_eq!(seen.len(), MAX_SEEN_IDS_PER_SESSION);
        assert!(!seen.contains("m0"));
        assert!(seen.contains(&format!("m{}", MAX_SEEN_IDS_PER_SESSION + 7)));

        state.record_recall(
            "bounded",
            1,
            (0..MAX_RECALL_IDS_PER_SNAPSHOT + 8)
                .map(|index| format!("r{index}"))
                .collect(),
        );
        let recalls = state.drain_recalls("bounded", None);
        assert_eq!(recalls[0].memory_ids.len(), MAX_RECALL_IDS_PER_SNAPSHOT);

        for index in 0..MAX_RUNTIME_SESSIONS + 8 {
            state.record_seen(&format!("session-{index}"), ["m".to_string()]);
        }
        assert!(state.sessions.read().unwrap().len() <= MAX_RUNTIME_SESSIONS);
        assert!(
            state
                .seen_snapshot(&format!("session-{}", MAX_RUNTIME_SESSIONS + 7))
                .contains("m")
        );
    }
}
