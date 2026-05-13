//! Memoria (memory service) HTTP client for tool execution.
//!
//! Provides HTTP client for storing, retrieving, and managing memories
//! via the Memoria API, with circuit breaker for resilience.
//!
//! This module is shared between CLI and server — both use HTTP proxy
//! calls to the Memoria service.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// HTTP method for Memoria API calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Put,
    Post,
}

/// A single memory hit from search, carrying both ID and content.
#[derive(Debug, Clone)]
pub struct BoostSearchHit {
    pub memory_id: Option<String>,
    pub content: String,
}

/// Parse content strings from a Memoria search/retrieve response.
///
/// Handles common Memoria response shapes:
/// - `{ "memories": [ { "content": "..." }, ... ] }`
/// - `[ { "content": "..." }, ... ]`
/// - `{ "results": [ { "content": "..." }, ... ] }`
pub fn parse_memory_search_contents(raw: &str) -> Vec<String> {
    parse_memory_search_hits(raw)
        .into_iter()
        .map(|h| h.content)
        .collect()
}

/// Parse memory hits (ID + content) from a Memoria search/retrieve response.
pub fn parse_memory_search_hits(raw: &str) -> Vec<BoostSearchHit> {
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    if val.get("error").is_some() {
        return vec![];
    }
    let items = val
        .get("memories")
        .or_else(|| val.get("results"))
        .and_then(Value::as_array)
        .or_else(|| val.as_array());

    let Some(arr) = items else {
        if let Some(c) = val.get("content").and_then(Value::as_str) {
            let mid = val
                .get("memory_id")
                .or_else(|| val.get("id"))
                .and_then(Value::as_str)
                .map(String::from);
            return vec![BoostSearchHit {
                memory_id: mid,
                content: c.to_string(),
            }];
        }
        return vec![];
    };

    arr.iter()
        .filter_map(|item| {
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)?;
            if content.is_empty() {
                return None;
            }
            let memory_id = item
                .get("memory_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .map(String::from);
            Some(BoostSearchHit {
                memory_id,
                content: content.to_string(),
            })
        })
        .collect()
}

/// Return true when text appears to contain credentials or similarly
/// sensitive material that must never become durable memory.
pub fn contains_sensitive_memory_content(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    const NEEDLES: &[&str] = &[
        "password:",
        "password=",
        "passwd:",
        "passwd=",
        "api_key:",
        "api_key=",
        "apikey:",
        "apikey=",
        "access_token:",
        "access_token=",
        "refresh_token:",
        "refresh_token=",
        "secret_key:",
        "secret_key=",
        "client_secret:",
        "client_secret=",
        "authorization:",
        "beginprivatekey",
        "beginrsaprivatekey",
        "beginopensshprivatekey",
    ];
    if NEEDLES.iter().any(|needle| compact.contains(needle)) {
        return true;
    }
    if lower.contains("bearer ") {
        return true;
    }
    let raw = text.trim();
    raw.contains("ghp_")
        || raw.contains("github_pat_")
        || raw.contains("sk-live-")
        || raw.contains("sk_test_")
        || raw.contains("xoxb-")
        || raw.contains("xoxp-")
        || raw.contains("AKIA")
}

/// A single `focus` hint: a session-scoped attention boost with TTL.
///
/// Stored in-process by [`MemoriaClient`]. On each `recall` call the
/// client consults the hints whose `expires_at` is still in the future
/// and forwards them to the backend as `boost_topics` / `boost_tags`
/// hints. The hint is evicted on first access past its TTL.
#[derive(Debug, Clone)]
struct FocusHint {
    /// `"topic" | "tag" | "memory_id" | "session"` (matches v2 FocusRequest).
    focus_type: String,
    value: String,
    boost: f64,
    expires_at: Instant,
}

/// Memoria HTTP client with circuit breaker.
///
/// Used by both CLI (via ToolExecutor) and server (via ServerToolExecutor)
/// to proxy memory operations to the Memoria service.
///
/// **Cognitive verbs**: the LLM-facing surface exposes v2 cognitive verbs
/// (`remember`, `recall`, `forget`, `update`, `expand`, `focus`, `reflect`,
/// `profile`, `feedback`). Those are translated to v1 HTTP endpoints by
/// [`Self::build_direct_request`]. `focus` is handled in-process via the
/// [`FocusHint`] store; subsequent `recall`s read it and forward boost
/// hints to the backend.
pub struct MemoriaClient {
    /// Cloud API base URL for proxied calls.
    pub cloud_base: Option<String>,
    /// Auth token for cloud proxy calls.
    pub cloud_token: Option<String>,
    /// Circuit breaker: skip after consecutive failures.
    fail_count: AtomicU32,
}

/// Process-global focus hints, keyed by session_id.
///
/// Tool executors construct `MemoriaClient` per tool call in several
/// production paths. Keeping focus hints on the client instance made
/// `memory(action=focus)` evaporate before the next `recall`. This store
/// is the session-lifetime state for those hints.
static FOCUS_STORE: OnceLock<RwLock<HashMap<String, Vec<FocusHint>>>> = OnceLock::new();

fn focus_store() -> &'static RwLock<HashMap<String, Vec<FocusHint>>> {
    FOCUS_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Process-global "already surfaced" set for memory entries, keyed by
/// session_id. Holds a union of two kinds of dedup keys:
///
/// - **memory_id** (written by tool-side `decorate_recall_response`
///   and by the runtime `MemoryOrchestrator::mark_surfaced`)
/// - **normalized content dedup key** (written by the bridge when it
///   injects `<session_memory>` + the per-turn recall block)
///
/// Both paths share one canonical store so a memory shown via
/// `<session_memory>` won't re-appear in per-turn recall, and a memory
/// returned from an LLM-driven `memory(action=recall)` won't re-appear
/// in the next recall. Cleared at session-end by
/// `post_loop_memory_cleanup`. `MemoriaClient` is constructed per-
/// tool-call in production (see `server_tool_executor.rs`,
/// `edge_tools/memoria.rs`), so a per-client store would reset every
/// call — process-global is the minimum viable scope.
static SEEN_STORE: std::sync::OnceLock<RwLock<HashMap<String, std::collections::HashSet<String>>>> =
    std::sync::OnceLock::new();

fn seen_store() -> &'static RwLock<HashMap<String, std::collections::HashSet<String>>> {
    SEEN_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Process-global store of memory IDs suppressed within a session.
/// Agent calls `session(action="suppress_memory")` to add IDs;
/// `prefetch_memories` checks this before injection.
static SUPPRESS_STORE: std::sync::OnceLock<
    RwLock<HashMap<String, std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

fn suppress_store() -> &'static RwLock<HashMap<String, std::collections::HashSet<String>>> {
    SUPPRESS_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Process-global store of tool_call_ids marked for context release.
/// Agent calls `session(action="release_context")` to mark IDs;
/// compaction stubs their content on the next pass.
static RELEASED_STORE: std::sync::OnceLock<
    RwLock<HashMap<String, std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

fn released_store() -> &'static RwLock<HashMap<String, std::collections::HashSet<String>>> {
    RELEASED_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// One recall awaiting outcome attribution. Populated by the `recall`
/// verb post-processor; drained by the runtime's feedback observer.
#[derive(Debug, Clone)]
pub struct RecallSnapshot {
    pub session_id: String,
    pub memory_ids: Vec<String>,
    pub turn: u32,
    pub at: Instant,
}

/// Result of draining recall snapshots into Memoria feedback calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedbackDrainReport {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

fn memoria_output_is_error(output: &str) -> bool {
    if output.starts_with("Error") {
        return true;
    }
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|value| value.get("error").cloned())
        .is_some()
}

/// Process-global FIFO queue of recall snapshots per session. Mirror of
/// `MemoryOrchestrator::recall_ledger`, but accessible from the tool
/// layer (which doesn't depend on runtime) so `decorate_recall_response`
/// can push without a cross-crate trait. The runtime observes and
/// drains.
static RECALL_LEDGER: std::sync::OnceLock<
    RwLock<HashMap<String, std::collections::VecDeque<RecallSnapshot>>>,
> = std::sync::OnceLock::new();

const MAX_RECALL_LEDGER_PER_SESSION: usize = 16;

fn recall_ledger() -> &'static RwLock<HashMap<String, std::collections::VecDeque<RecallSnapshot>>> {
    RECALL_LEDGER.get_or_init(|| RwLock::new(HashMap::new()))
}

const MAX_FAILS: u32 = 2;

impl MemoriaClient {
    pub fn new(cloud_base: Option<String>, cloud_token: Option<String>) -> Self {
        Self {
            cloud_base,
            cloud_token,
            fail_count: AtomicU32::new(0),
        }
    }

    /// Record a `focus` hint for the given session. Returns the synthetic
    /// response the LLM sees (mirrors the v2 FocusResponse shape).
    pub fn focus_set(&self, session_id: &str, args: &Value) -> String {
        let focus_type = match args
            .get("focus_type")
            .or_else(|| args.get("type"))
            .and_then(Value::as_str)
        {
            Some(t @ ("topic" | "tag" | "memory_id" | "session")) => t.to_string(),
            _ => {
                return json!({"error":
                    "memory(action=focus) requires `focus_type` ∈ {topic,tag,memory_id,session}"})
                .to_string();
            }
        };
        let value = match args
            .get("focus_value")
            .or_else(|| args.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(v) => v.to_string(),
            None => {
                return json!({"error": "memory(action=focus) requires non-empty `focus_value`"})
                    .to_string();
            }
        };
        let boost = args.get("boost").and_then(Value::as_f64).unwrap_or(1.5);
        let ttl_secs = args
            .get("ttl_secs")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .max(1) as u64;
        let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
        let hint = FocusHint {
            focus_type: focus_type.clone(),
            value: value.clone(),
            boost,
            expires_at,
        };
        if let Ok(mut store) = focus_store().write() {
            let sid_key = if session_id.is_empty() {
                "_global".to_string()
            } else {
                session_id.to_string()
            };
            let bucket = store.entry(sid_key).or_default();
            // Evict any existing hint with the same (type, value) so the
            // newest boost/ttl wins.
            bucket.retain(|h| !(h.focus_type == focus_type && h.value == value));
            bucket.push(hint);
        }
        json!({
            "status": "ok",
            "focus_type": focus_type,
            "value": value,
            "boost": boost,
            "active_for_secs": ttl_secs,
        })
        .to_string()
    }

    /// Return active focus hints for a session. Expired entries are
    /// evicted as a side effect.
    fn focus_active(&self, session_id: &str) -> Vec<FocusHint> {
        let now = Instant::now();
        let sid_key = if session_id.is_empty() {
            "_global".to_string()
        } else {
            session_id.to_string()
        };
        if let Ok(mut store) = focus_store().write()
            && let Some(bucket) = store.get_mut(&sid_key)
        {
            bucket.retain(|h| h.expires_at > now);
            return bucket.clone();
        }
        Vec::new()
    }

    /// Record memory_ids surfaced to the LLM in a given session
    /// (process-global store).
    ///
    /// Public: this is the single canonical "already surfaced" store
    /// for the process. The runtime's `MemoryOrchestrator` is a
    /// delegating facade that calls this — no parallel store exists.
    pub fn record_seen(session_id: &str, ids: impl IntoIterator<Item = String>) {
        if session_id.is_empty() {
            return;
        }
        let Ok(mut store) = seen_store().write() else {
            return;
        };
        let bucket = store.entry(session_id.to_string()).or_default();
        for id in ids {
            if !id.is_empty() {
                bucket.insert(id);
            }
        }
    }

    /// Snapshot surfaced ids for a session (process-global store);
    /// caller drops the clone after use.
    ///
    /// Public: see [`record_seen`].
    pub fn seen_snapshot(session_id: &str) -> std::collections::HashSet<String> {
        if session_id.is_empty() {
            return std::collections::HashSet::new();
        }
        seen_store()
            .read()
            .ok()
            .and_then(|g| g.get(session_id).cloned())
            .unwrap_or_default()
    }

    /// Clear the "already surfaced" set for a session. Intended for
    /// session-end cleanup. Public so the runtime's session-end path
    /// can keep tool-side state in lock-step with its own seen ledger.
    pub fn reset_seen(session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = seen_store().write() {
            g.remove(session_id);
        }
    }

    /// Suppress a memory by ID for the rest of this session.
    /// Suppressed memories are filtered out during prefetch before injection.
    pub fn suppress_memory(session_id: &str, memory_id: &str) {
        if session_id.is_empty() || memory_id.is_empty() {
            return;
        }
        let Ok(mut store) = suppress_store().write() else {
            return;
        };
        store
            .entry(session_id.to_string())
            .or_default()
            .insert(memory_id.to_string());
    }

    /// Remove a memory from the session suppress list.
    pub fn unsuppress_memory(session_id: &str, memory_id: &str) {
        if session_id.is_empty() || memory_id.is_empty() {
            return;
        }
        let Ok(mut store) = suppress_store().write() else {
            return;
        };
        if let Some(bucket) = store.get_mut(session_id) {
            bucket.remove(memory_id);
        }
    }

    /// Snapshot suppressed memory IDs for a session.
    pub fn suppressed_snapshot(session_id: &str) -> std::collections::HashSet<String> {
        if session_id.is_empty() {
            return std::collections::HashSet::new();
        }
        suppress_store()
            .read()
            .ok()
            .and_then(|g| g.get(session_id).cloned())
            .unwrap_or_default()
    }

    /// Clear suppressed memories for a session (session-end cleanup).
    pub fn reset_suppressed(session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = suppress_store().write() {
            g.remove(session_id);
        }
    }

    /// Mark a tool_call_id for context release (early eviction from context).
    pub fn release_context(session_id: &str, tool_call_id: &str) {
        if session_id.is_empty() || tool_call_id.is_empty() {
            return;
        }
        let Ok(mut store) = released_store().write() else {
            return;
        };
        store
            .entry(session_id.to_string())
            .or_default()
            .insert(tool_call_id.to_string());
    }

    /// Snapshot released tool_call_ids for a session.
    pub fn released_snapshot(session_id: &str) -> std::collections::HashSet<String> {
        if session_id.is_empty() {
            return std::collections::HashSet::new();
        }
        released_store()
            .read()
            .ok()
            .and_then(|g| g.get(session_id).cloned())
            .unwrap_or_default()
    }

    /// Clear released context for a session (session-end cleanup).
    pub fn reset_released(session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = released_store().write() {
            g.remove(session_id);
        }
    }

    /// Clear focus hints for a session. Called at session-end cleanup so
    /// long-lived processes do not carry stale attention boosts forever.
    pub fn reset_focus(session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = focus_store().write() {
            g.remove(session_id);
        }
    }

    /// Record a recall snapshot for later outcome attribution.
    /// Pushed by `decorate_recall_response` when the LLM calls
    /// `memory(action=recall)` and receives memory_ids; drained by the
    /// runtime's feedback observer at tool-result boundaries.
    ///
    /// Per-session queue is FIFO, soft-capped at 16 entries; oldest
    /// evicted beyond the cap so an LLM that never closes the loop
    /// doesn't leak memory.
    pub fn record_recall(session_id: &str, turn: u32, memory_ids: Vec<String>) {
        if session_id.is_empty() || memory_ids.is_empty() {
            return;
        }
        let snap = RecallSnapshot {
            session_id: session_id.to_string(),
            memory_ids,
            turn,
            at: Instant::now(),
        };
        if let Ok(mut g) = recall_ledger().write() {
            let q = g.entry(session_id.to_string()).or_default();
            if q.len() >= MAX_RECALL_LEDGER_PER_SESSION {
                q.pop_front();
            }
            q.push_back(snap);
        }
    }

    /// Drain and return all recall snapshots for a session older than
    /// `max_age`. Entries within the window are returned; stale ones
    /// are dropped (can no longer reliably attribute). Invoked by the
    /// runtime's feedback observer.
    pub fn drain_recalls(session_id: &str, max_age: Option<Duration>) -> Vec<RecallSnapshot> {
        if session_id.is_empty() {
            return Vec::new();
        }
        let Ok(mut g) = recall_ledger().write() else {
            return Vec::new();
        };
        let Some(q) = g.remove(session_id) else {
            return Vec::new();
        };
        q.into_iter()
            .filter(|s| match max_age {
                Some(max) => s.at.elapsed() <= max,
                None => true,
            })
            .collect()
    }

    /// Number of unconsumed recall snapshots for a session (tests +
    /// observability).
    pub fn pending_recall_count(session_id: &str) -> usize {
        recall_ledger()
            .read()
            .ok()
            .and_then(|g| g.get(session_id).map(|q| q.len()))
            .unwrap_or(0)
    }

    /// Clear the recall ledger for a session. Called at session-end
    /// cleanup alongside `reset_seen`.
    pub fn reset_recall_ledger(session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = recall_ledger().write() {
            g.remove(session_id);
        }
    }

    /// Clear all process-global memory state for a session. Long-lived CLI
    /// and server processes call this at the session boundary so surfaced
    /// memory ids, focus hints, and pending recall feedback cannot bleed into
    /// the next session.
    pub fn reset_session_process_state(session_id: &str) {
        Self::reset_seen(session_id);
        Self::reset_focus(session_id);
        Self::reset_recall_ledger(session_id);
        Self::reset_suppressed(session_id);
        Self::reset_released(session_id);
    }

    /// Drain pending recalls and push one feedback signal for every
    /// surfaced memory id. Intended for tool-result lifecycle hooks:
    /// when a non-memory tool succeeds after a recall, the recall gets
    /// prompt feedback immediately instead of waiting for session-end.
    ///
    /// Best-effort: the recall ledger is consumed exactly once, and failed
    /// feedback attempts are counted + logged so loss is observable.
    pub async fn feedback_pending_recalls(
        &self,
        session_id: &str,
        signal: &str,
        context_prefix: &str,
    ) -> FeedbackDrainReport {
        if session_id.is_empty() || signal.is_empty() {
            return FeedbackDrainReport::default();
        }
        let snapshots = Self::drain_recalls(session_id, None);
        let mut report = FeedbackDrainReport::default();
        for snap in snapshots {
            for id in snap.memory_ids {
                if id.is_empty() {
                    continue;
                }
                report.attempted += 1;
                let context = if context_prefix.is_empty() {
                    format!("auto: turn {} outcome", snap.turn)
                } else {
                    format!("{context_prefix}: turn {} outcome", snap.turn)
                };
                let output = self
                    .call_with_timeout(
                        "feedback",
                        &json!({
                            "memory_id": id,
                            "signal": signal,
                            "context": context,
                        }),
                        Duration::from_secs(3),
                    )
                    .await;
                if memoria_output_is_error(&output) {
                    report.failed += 1;
                    tracing::warn!(
                        target: "memoria",
                        session_id = %snap.session_id,
                        memory_id = %id,
                        signal = %signal,
                        context = %context,
                        error = %output,
                        "failed to close memory recall feedback"
                    );
                } else {
                    report.succeeded += 1;
                }
            }
        }
        report
    }

    /// Inject focus hints into a `recall` payload. Called by the
    /// `call_with_timeout` path right before the HTTP send when `op ==
    /// "recall"`.
    fn apply_focus_hints(&self, session_id: &str, payload: &mut Value) {
        let hints = self.focus_active(session_id);
        if hints.is_empty() {
            return;
        }
        let Some(obj) = payload.as_object_mut() else {
            return;
        };
        let mut topics: Vec<Value> = Vec::new();
        let mut tags: Vec<Value> = Vec::new();
        let mut memory_ids: Vec<Value> = Vec::new();
        for h in hints {
            let entry = json!({ "value": h.value, "boost": h.boost });
            match h.focus_type.as_str() {
                "topic" => topics.push(entry),
                "tag" => tags.push(entry),
                "memory_id" => memory_ids.push(entry),
                _ => {}
            }
        }
        if !topics.is_empty() {
            obj.insert("boost_topics".into(), Value::Array(topics));
        }
        if !tags.is_empty() {
            obj.insert("boost_tags".into(), Value::Array(tags));
        }
        if !memory_ids.is_empty() {
            obj.insert("boost_memory_ids".into(), Value::Array(memory_ids));
        }
    }

    /// Post-process a `recall` response so the LLM gets the same two
    /// signals the prefetch path gives it:
    ///
    /// 1. **Freshness suffix** appended to each memory's `content`
    ///    (e.g. ` (this week)`, ` (stale — verify first)`) — derived
    ///    from `observed_at`/`updated_at` and the memory's `trust_tier`.
    /// 2. **Surface-once dedup**: memories whose `memory_id` already
    ///    appeared in an earlier recall this session are dropped.
    ///
    /// Input `raw_text` is the HTTP body from Memoria's retrieve
    /// endpoint — expected to be a top-level JSON array of memory
    /// entries. Non-array bodies (error envelopes, etc.) pass through
    /// unchanged so the LLM still sees the original error.
    ///
    /// Pure so the wiring + the decoration logic stay testable in
    /// isolation. `seen` is the callers' snapshot of memory_ids
    /// previously surfaced; `newly_surfaced` receives the ids in the
    /// final (post-filter) output so the caller can record them.
    pub fn decorate_recall_response(
        raw_text: &str,
        seen: &std::collections::HashSet<String>,
        newly_surfaced: &mut Vec<String>,
    ) -> String {
        let Ok(mut parsed) = serde_json::from_str::<Value>(raw_text) else {
            return raw_text.to_string();
        };
        let Some(arr) = parsed.as_array_mut() else {
            return raw_text.to_string();
        };
        arr.retain(|item| {
            let id = item
                .get("memory_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            id.is_empty() || !seen.contains(id)
        });
        for item in arr.iter_mut() {
            let id = item
                .get("memory_id")
                .and_then(Value::as_str)
                .map(String::from);
            let days = item
                .get("observed_at")
                .or_else(|| item.get("updated_at"))
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_days_ago);
            let trust_tier = item
                .get("trust_tier")
                .and_then(Value::as_str)
                .map(String::from);
            if let Some(days) = days {
                let suffix = astra_turn_types::freshness_suffix_for(days, trust_tier.as_deref());
                if !suffix.is_empty()
                    && let Some(content) = item.get_mut("content")
                    && let Some(c) = content.as_str()
                {
                    *content = Value::String(format!("{c}{suffix}"));
                }
            }
            if let Some(id) = id
                && !id.is_empty()
            {
                newly_surfaced.push(id);
            }
        }
        serde_json::to_string(&parsed).unwrap_or_else(|_| raw_text.to_string())
    }

    /// Builds a tool result that confirms purge success to the agent.
    /// Use this instead of returning the raw Memoria `{}` response.
    pub fn purge_result_to_agent_response(raw: &Value, filter: &str) -> Value {
        let deleted = raw
            .get("deleted_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        json!({
            "status": "ok",
            "deleted_count": deleted,
            "message": Self::purge_success_message(deleted, filter),
        })
    }

    /// Returns a human-readable success message for a purge response.
    pub fn purge_success_message(deleted_count: u64, filter: &str) -> String {
        if deleted_count == 0 {
            format!("memory_purge: no entries matched filter [{filter}] (0 deleted)")
        } else {
            format!("memory_purge: deleted {deleted_count} entries matching [{filter}]")
        }
    }

    /// Check if the circuit breaker is open (too many consecutive failures).
    pub fn is_circuit_open(&self) -> bool {
        self.fail_count.load(Ordering::Relaxed) >= MAX_FAILS
    }

    /// Whitelist of agent_type values the client will forward to the
    /// Memoria backend. Kept in sync with the `agent_type` enum in the
    /// `memory` tool schema (astra-cli edge_tools). Unknown values are
    /// dropped client-side so that untrusted tool-call arguments from
    /// an LLM cannot smuggle arbitrary strings (`"../admin"`,
    /// `"' OR 1=1--"`, etc.) into request bodies even if the backend
    /// filter is misconfigured.
    const AGENT_TYPE_ALLOWLIST: &'static [&'static str] =
        &["explore", "code-review", "task", "general-purpose"];

    /// Pull an `agent_type` value out of tool args and apply the
    /// client-side allowlist. Returns `None` if the value is missing,
    /// empty, or not in [`AGENT_TYPE_ALLOWLIST`].
    fn sanitized_agent_type(args: &Value) -> Option<&str> {
        let v = args.get("agent_type").and_then(Value::as_str)?;
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return None;
        }
        if Self::AGENT_TYPE_ALLOWLIST.contains(&trimmed) {
            Some(trimmed)
        } else {
            None
        }
    }

    /// Minimum similarity score (Memoria `final_score`) that flags a
    /// new `remember` call as a likely duplicate. Tuned empirically:
    /// vector+keyword hybrid scores above ~0.85 on a short phrase
    /// match are near-synonyms in practice.
    pub const CONFLICT_SIMILARITY_FLOOR: f64 = 0.85;

    /// Client-side conflict pre-check for `remember`. Issues a cheap
    /// top-3 recall with a 2-second timeout; if any existing memory
    /// crosses [`CONFLICT_SIMILARITY_FLOOR`], returns a structured
    /// JSON string the LLM parses as a tool result — a "redirect" that
    /// nudges the model toward `update(memory_id=...)` instead of
    /// writing a duplicate. Returns `None` on no conflict / fetch
    /// failure (degrade to the normal write path).
    async fn detect_remember_conflict(
        &self,
        new_content: &str,
        session_id: Option<&str>,
    ) -> Option<String> {
        // A narrow conflict query: use the new content as the query.
        // We don't want to spend 5s on a full retrieval here — the
        // write path must stay fast when there are no conflicts.
        let mem = astra_core::MemoriaSettings::from_env();
        let key = mem.master_key?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .ok()?;
        let mut body = json!({"query": new_content, "top_k": 3});
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
        }
        let resp = client
            .post(format!("{}/v1/memories/retrieve", mem.base_url))
            .header("Authorization", format!("Bearer {key}"))
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let text = resp.text().await.ok()?;
        let parsed: Value = serde_json::from_str(&text).ok()?;
        let arr = parsed
            .get("memories")
            .and_then(Value::as_array)
            .or_else(|| parsed.as_array())?;

        format_remember_conflict(arr, Self::CONFLICT_SIMILARITY_FLOOR)
    }

    /// Execute a memoria operation (store, retrieve, search, purge, correct, profile).
    pub async fn call(&self, op: &str, args: &Value) -> String {
        self.call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    /// Execute a memoria operation with custom timeout.
    pub async fn call_with_timeout(&self, op: &str, args: &Value, timeout: Duration) -> String {
        // `focus` is handled entirely in-process; no HTTP call.
        if op == "focus" {
            let sid = args.get("session_id").and_then(Value::as_str).unwrap_or("");
            return self.focus_set(sid, args);
        }

        if self.is_circuit_open() {
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

        if let Some(validation_error) = Self::validate_before_side_effects(op, args) {
            return validation_error.to_string();
        }

        // `remember`: run a client-side conflict pre-check so the LLM
        // can't silently create duplicates of near-identical memories.
        // Opt-out via `skip_conflict_check: true` — the background
        // extractor already manifests existing memories in its prompt
        // and sets this flag to bypass the double-check.
        if op == "remember"
            && !args
                .get("skip_conflict_check")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && let Some(content) = args.get("content").and_then(Value::as_str)
            && !content.trim().is_empty()
        {
            let session_id = args.get("session_id").and_then(Value::as_str);
            if let Some(conflict) = self.detect_remember_conflict(content, session_id).await {
                return conflict;
            }
        }

        // The v2→v1 translation — including business-category expansion
        // into (content-prefix + trust_tier + tag) — now happens inside
        // `build_direct_request` for the `remember` branch. No
        // pre-normalization needed here.

        let (endpoint, mut payload, auth_header, method) = if let (Some(cloud_base), Some(token)) =
            (&self.cloud_base, &self.cloud_token)
        {
            // Cloud proxy: route v2 verbs via a dedicated `/memory/v2/:op`
            // namespace so the server-side executor can translate to v1
            // using its own credentials. If the cloud doesn't implement
            // v2 yet, the fall-through direct path still works via the
            // agent's own MEMORIA_MASTER_KEY.
            (
                format!("{cloud_base}/memory/v2/{op}"),
                args.clone(),
                format!("Bearer {token}"),
                HttpMethod::Post,
            )
        } else {
            let mem = astra_core::MemoriaSettings::from_env();
            let key = match mem.master_key {
                Some(k) => k,
                None => {
                    return json!({
                            "error": "Memory unavailable: not connected to cloud and MEMORIA_MASTER_KEY not set",
                            "hint": "Login with /login to enable cloud-backed memory with user isolation"
                        })
                        .to_string();
                }
            };
            let (ep, pl, m) = Self::build_direct_request(&mem.base_url, op, args);
            if ep.is_empty() {
                // Validation errors (and anything else the mapper decides
                // to short-circuit) return the payload verbatim. Must
                // happen BEFORE the auto-snapshot below so rejected
                // calls don't produce orphan `pre_<op>_*` snapshots.
                return pl.to_string();
            }
            (ep, pl, format!("Bearer {key}"), m)
        };

        // Auto-snapshot before destructive ops so `memory_rollback` is a
        // real recovery path. Happens AFTER validation (endpoint is
        // non-empty, required args were verified) so rejected
        // `forget`/`update` calls don't litter the snapshot store with
        // orphan `pre_*` entries. Best-effort: a snapshot failure
        // (cloud down, Memoria misconfigured) logs and continues — we'd
        // rather the op proceed than block on an unreachable snapshot
        // service. Name is deterministic so callers can find the
        // snapshot without listing: `pre_<op>_<ts_ms>`.
        if matches!(op, "forget" | "update") {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let name = pre_op_snapshot_name(op, ts);
            if let Err(e) = memoria_snapshot_create(&name).await {
                tracing::warn!(
                    target: "astra::memory::auto_snapshot",
                    op = %op,
                    error = %e,
                    "pre-op auto-snapshot failed; continuing without safety net"
                );
            }
        }

        // For `recall`, layer in session-scoped focus boosts. The backend
        // is free to ignore fields it doesn't understand; they become
        // active once Memoria v2 lands.
        if op == "recall" {
            let sid = args.get("session_id").and_then(Value::as_str).unwrap_or("");
            self.apply_focus_hints(sid, &mut payload);
        }

        let raw_text = match reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
        {
            Ok(client) => {
                let req = match method {
                    HttpMethod::Get => client.get(&endpoint),
                    HttpMethod::Put => client.put(&endpoint),
                    HttpMethod::Post => client.post(&endpoint),
                };
                match req
                    .header("Authorization", &auth_header)
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(resp) => match resp.text().await {
                        Ok(text) => {
                            self.fail_count.store(0, Ordering::Relaxed);
                            text
                        }
                        Err(e) => {
                            self.fail_count.fetch_add(1, Ordering::Relaxed);
                            json!({"error": format!("read response: {e}")}).to_string()
                        }
                    },
                    Err(e) => {
                        self.fail_count.fetch_add(1, Ordering::Relaxed);
                        json!({"error": format!("memoria request failed: {e}")}).to_string()
                    }
                }
            }
            Err(e) => json!({"error": format!("build client: {e}")}).to_string(),
        };

        // Post-process recall responses with freshness suffixes + surface-
        // once dedup so LLM-driven recalls carry the same signals as the
        // bridge-side prefetch path. Other verbs pass through verbatim.
        if op == "recall" {
            let session_id = args.get("session_id").and_then(Value::as_str).unwrap_or("");
            let turn = args.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
            let seen = Self::seen_snapshot(session_id);
            let mut newly_surfaced = Vec::new();
            let decorated = Self::decorate_recall_response(&raw_text, &seen, &mut newly_surfaced);
            if !newly_surfaced.is_empty() {
                // (a) dedup store: don't re-show same id this session
                Self::record_seen(session_id, newly_surfaced.clone());
                // (b) recall ledger: ids await outcome attribution so
                //     the next tool-result can route useful/irrelevant
                //     feedback back to them. Closes the recall→feedback
                //     loop the prompt promises.
                Self::record_recall(session_id, turn, newly_surfaced);
            }
            return decorated;
        }
        raw_text
    }

    /// Boost search: best-effort memory lookup on the critical path.
    pub async fn boost_search(&self, query: &str, top_k: u64) -> Vec<BoostSearchHit> {
        if query.trim().is_empty() || self.is_circuit_open() {
            return vec![];
        }
        let mem = astra_core::MemoriaSettings::from_env();
        let token = match mem.bearer_token() {
            Some(t) => t,
            None => return vec![],
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(800))
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        match client
            .post(format!("{}/v1/memories/retrieve", mem.base_url))
            .header("Authorization", token)
            .json(&json!({
                "query": query,
                "top_k": top_k,
                "min_confidence": 0.3
            }))
            .send()
            .await
        {
            Ok(resp) => {
                self.fail_count.store(0, Ordering::Relaxed);
                let text = resp.text().await.unwrap_or_default();
                parse_memory_search_hits(&text)
            }
            Err(_) => {
                self.fail_count.fetch_add(1, Ordering::Relaxed);
                vec![]
            }
        }
    }
    /// Map an LLM-facing v2 cognitive verb (`op`) to a concrete Memoria
    /// v1 HTTP request.
    ///
    /// The LLM only ever sees v2 verbs: `remember`, `recall`, `expand`,
    /// `forget`, `update`, `focus`, `reflect`, `profile`, `feedback`.
    /// Runtime translates each to the v1 endpoint with the appropriate
    /// body shape. Some v2-only semantics (`focus`, `expand` detail
    /// levels, `reflect` candidate synthesis) are synthesized client-side
    /// on top of what v1 exposes — see per-verb comments.
    ///
    /// Returns `(endpoint, payload, method)`. An empty `endpoint` signals
    /// "client-side only, return `payload` verbatim as the tool output"
    /// (used for validation errors and `focus`/synthetic responses).
    pub fn build_direct_request(base: &str, op: &str, args: &Value) -> (String, Value, HttpMethod) {
        let inject_identity = |pl: &mut Value| {
            if let Some(obj) = pl.as_object_mut() {
                if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                    obj.insert("session_id".to_string(), json!(sid));
                }
                if let Some(uid) = args.get("user_id").and_then(Value::as_str) {
                    obj.insert("user_id".to_string(), json!(uid));
                }
            }
        };
        match op {
            // ── remember → v1 store (`POST /v1/memories`) ────────────────
            //
            // v2 exposes an open-ended `memory_type` (any Memoria primitive
            // OR an astra business category). The mapping layer lives in
            // [`astra_prompts::memory_types`]:
            //   business `user`       → v1 `profile`     + trust_tier=T1
            //   business `feedback|project|lesson` → v1 `semantic` + T2/T3
            //   business `ref`        → v1 `procedural`  + T2
            //   business `episode`    → v1 `episodic`    + T3
            // The content is prefix-encoded (`[user] …`, `[feedback] …`)
            // so the category survives a v1 store→retrieve round-trip.
            // When v2 stabilises the prefix moves into the `tags` array
            // (`astra:user`, etc.) — the tag is *already* emitted today,
            // v1 just ignores it.
            "remember" => {
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                if content.trim().is_empty() {
                    return (
                        String::new(),
                        json!({"error": "memory(action=remember) requires `content`"}),
                        HttpMethod::Post,
                    );
                }
                if contains_sensitive_memory_content(content) {
                    return (
                        String::new(),
                        json!({"error":
                            "memory(action=remember) refused sensitive-looking content; do not store secrets, credentials, or tokens"}),
                        HttpMethod::Post,
                    );
                }
                let raw_type = args
                    .get("memory_type")
                    .and_then(Value::as_str)
                    .unwrap_or("semantic");

                // Protocol translation between v2 taxonomy (business
                // categories: user/feedback/project/ref/lesson/episode) and
                // v1 primitives (semantic/profile/procedural/episodic/
                // working/tool_result).
                use astra_prompts::memory_types::{MemoryCategory, encode as encode_category};
                let (resolved_content, resolved_type, resolved_tier, category_tag) = match raw_type
                {
                    "user" => (
                        encode_category(MemoryCategory::User, content),
                        "profile",
                        Some(MemoryCategory::User.trust_tier()),
                        Some(MemoryCategory::User.v2_tag()),
                    ),
                    "feedback" => (
                        encode_category(MemoryCategory::Feedback, content),
                        "semantic",
                        Some(MemoryCategory::Feedback.trust_tier()),
                        Some(MemoryCategory::Feedback.v2_tag()),
                    ),
                    "project" => (
                        encode_category(MemoryCategory::Project, content),
                        "semantic",
                        Some(MemoryCategory::Project.trust_tier()),
                        Some(MemoryCategory::Project.v2_tag()),
                    ),
                    "ref" | "reference" => (
                        encode_category(MemoryCategory::Reference, content),
                        "procedural",
                        Some(MemoryCategory::Reference.trust_tier()),
                        Some(MemoryCategory::Reference.v2_tag()),
                    ),
                    "lesson" => (
                        encode_category(MemoryCategory::Lesson, content),
                        "semantic",
                        Some(MemoryCategory::Lesson.trust_tier()),
                        Some(MemoryCategory::Lesson.v2_tag()),
                    ),
                    "episode" => (
                        encode_category(MemoryCategory::Episode, content),
                        "episodic",
                        Some(MemoryCategory::Episode.trust_tier()),
                        Some(MemoryCategory::Episode.v2_tag()),
                    ),
                    // Already a v1 primitive — pass through with no
                    // prefix encoding and no implicit trust tier.
                    other => (
                        content.to_string(),
                        astra_prompts::memory_types::normalize_memoria_type(other),
                        None,
                        None,
                    ),
                };

                let mut payload =
                    json!({"content": resolved_content, "memory_type": resolved_type});

                // Explicit `trust_tier` from the caller wins over the
                // category default — agents occasionally need to downgrade
                // confidence (e.g. speculative project memory).
                if let Some(tier) = args.get("trust_tier").and_then(Value::as_str) {
                    payload["trust_tier"] = json!(tier);
                } else if let Some(tier) = resolved_tier {
                    payload["trust_tier"] = json!(tier);
                }

                if let Some(imp) = args.get("importance").and_then(Value::as_f64) {
                    payload["initial_confidence"] = json!(imp.clamp(0.0, 1.0));
                }

                // Tags: explicit caller tags + the astra v2 category tag
                // (so v2 migration doesn't require re-writing history) +
                // the team-scope tag when visibility=team.
                let mut tags: Vec<Value> = args
                    .get("tags")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(tag) = category_tag
                    && !tags.iter().any(|t| t.as_str() == Some(tag))
                {
                    tags.push(json!(tag));
                }

                // Visibility: `team` requires `team_id` and encodes as an
                // `astra:team:<id>` tag. `private` (or absent) writes a
                // user-scoped memory with no team tag. `visibility=team`
                // without a `team_id` short-circuits to a clear error —
                // silently falling back to private would leak a fact the
                // agent believed was shared.
                let visibility = args
                    .get("visibility")
                    .and_then(Value::as_str)
                    .unwrap_or("private");
                match visibility {
                    "team" => {
                        let Some(team_id) = args
                            .get("team_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        else {
                            return (
                                String::new(),
                                json!({"error":
                                    "memory(action=remember, visibility=\"team\") requires a \
                                     non-empty `team_id`; falling back to private would silently \
                                     narrow the audience."}),
                                HttpMethod::Post,
                            );
                        };
                        let team_tag = format!("astra:team:{team_id}");
                        if !tags.iter().any(|t| t.as_str() == Some(team_tag.as_str())) {
                            tags.push(json!(team_tag));
                        }
                    }
                    "private" | "" => {
                        // Default — no team tag.
                    }
                    other => {
                        return (
                            String::new(),
                            json!({"error": format!(
                                "memory(action=remember): invalid visibility {other:?}; \
                                 expected \"private\" or \"team\""
                            )}),
                            HttpMethod::Post,
                        );
                    }
                }

                if !tags.is_empty() {
                    payload["tags"] = Value::Array(tags);
                }

                if let Some(at) = Self::sanitized_agent_type(args) {
                    payload["agent_type"] = json!(at);
                }
                inject_identity(&mut payload);

                // Session-scoped memory types MUST carry a session_id so
                // Memoria's governance can archive / isolate them. Without
                // it the row becomes orphaned and never cleans up.
                let has_sid = payload
                    .get("session_id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if matches!(resolved_type, "working" | "episodic") && !has_sid {
                    return (
                        String::new(),
                        json!({"error": format!(
                            "memory(action=remember, memory_type=\"{resolved_type}\") requires \
                             `session_id` for session-scoped isolation; the dispatcher must inject it"
                        )}),
                        HttpMethod::Post,
                    );
                }

                (format!("{base}/v1/memories"), payload, HttpMethod::Post)
            }
            // ── recall → v1 retrieve (`POST /v1/memories/retrieve`) ──────
            //
            // v2 collapses `retrieve` + `search` into a single `recall`.
            // Both v1 endpoints share the same request/response shape,
            // so we always hit `/v1/memories/retrieve` (the hybrid path).
            "recall" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                let mut pl = json!({"query": query, "top_k": top_k});
                if let Some(mc) = args.get("min_confidence").and_then(Value::as_f64) {
                    pl["min_confidence"] = json!(mc);
                }
                inject_identity(&mut pl);
                // v2 `scope` → v1 `session_scope`. Memoria v1 understands
                // "prefer" (rank session matches higher, still surface
                // cross-session) and "only" (strict session isolation).
                // v2 only exposes a binary `scope`:
                //   - "session" → "only" (strict)
                //   - "all" (default) → no scope header (v1 default is "prefer"
                //     when session_id is present, otherwise unscoped).
                // `session_scope` + `session_id` is a hard pair in v1; we
                // forward scope only when session_id is already present.
                let has_sid = pl
                    .get("session_id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if let Some(scope) = args.get("scope").and_then(Value::as_str) {
                    match scope {
                        "session" if has_sid => {
                            pl["session_scope"] = json!("only");
                        }
                        "session" => {
                            // Can't enforce strict session-scope without a
                            // session id; fail loud so callers catch it
                            // instead of silently downgrading to unscoped.
                            return (
                                String::new(),
                                json!({"error":
                                    "memory(action=recall, scope=\"session\") requires an active session_id"}),
                                HttpMethod::Post,
                            );
                        }
                        _ => {}
                    }
                }
                if let Some(at) = Self::sanitized_agent_type(args) {
                    pl["agent_type"] = json!(at);
                }
                // `view` is v2-only (compact/overview/full). v1 ignores it
                // silently; keeping it in the payload lets the eventual
                // v2 backend see the hint without code change.
                if let Some(view) = args.get("view").and_then(Value::as_str) {
                    pl["view"] = json!(view);
                }
                // Visibility: when `visibility=team` is requested, the
                // client-side post-filter (see `call_with_timeout`) unions
                // team-tagged hits into the result. Here we forward
                // `include_tags` to the backend — v1 ignores unknown
                // fields; v2 will honor them for server-side filtering.
                // The team_id(s) to union come from either `team_id`
                // (single) or `team_ids` (array); the dispatcher injects
                // the caller's default team when absent.
                let visibility = args.get("visibility").and_then(Value::as_str);
                if matches!(visibility, Some("team")) {
                    let mut team_tags = Vec::new();
                    if let Some(tid) = args.get("team_id").and_then(Value::as_str) {
                        team_tags.push(format!("astra:team:{}", tid.trim()));
                    }
                    if let Some(arr) = args.get("team_ids").and_then(Value::as_array) {
                        for v in arr {
                            if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                                team_tags.push(format!("astra:team:{s}"));
                            }
                        }
                    }
                    if !team_tags.is_empty() {
                        pl["include_tags"] =
                            Value::Array(team_tags.into_iter().map(Value::String).collect());
                    }
                }
                (format!("{base}/v1/memories/retrieve"), pl, HttpMethod::Post)
            }
            // ── expand → v1 GET memory by id (`GET /v1/memories/:id`) ────
            //
            // v2 has abstract / overview / detail / linked levels; v1
            // stores flat content. For now we fetch the full row; the
            // dispatcher downgrades according to `level`.
            "expand" => match args.get("memory_id").and_then(Value::as_str) {
                Some(mid) if !mid.is_empty() => (
                    format!("{base}/v1/memories/{mid}"),
                    json!({}),
                    HttpMethod::Get,
                ),
                _ => (
                    String::new(),
                    json!({"error": "memory(action=expand) requires `memory_id`"}),
                    HttpMethod::Post,
                ),
            },
            // ── forget → v1 purge (`POST /v1/memories/purge`) ────────────
            //
            // `reason` is REQUIRED at the runtime boundary. Destructive
            // ops without a stated reason rot the audit trail — the LLM
            // must declare *why* it's purging so a future inspector can
            // understand how the state evolved. (The schema also marks
            // it required for this action; the runtime is the second
            // line of defense.)
            "forget" => {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(reason) = reason else {
                    return (
                        String::new(),
                        json!({"error":
                            "memory(action=forget) requires a non-empty `reason` (audit trail)"}),
                        HttpMethod::Post,
                    );
                };
                let mut pl = json!({"reason": reason});
                if let Some(ids) = args.get("memory_ids").or_else(|| args.get("memory_id")) {
                    pl["memory_ids"] = if ids.is_array() {
                        ids.clone()
                    } else if let Some(s) = ids.as_str() {
                        json!(s.split(',').map(str::trim).collect::<Vec<_>>())
                    } else {
                        json!([ids.to_string()])
                    };
                } else if let Some(topic) = args.get("topic").and_then(Value::as_str) {
                    pl["topic"] = json!(topic);
                }
                let has_filter = pl
                    .as_object()
                    .is_some_and(|m| m.contains_key("memory_ids") || m.contains_key("topic"));
                if has_filter {
                    (format!("{base}/v1/memories/purge"), pl, HttpMethod::Post)
                } else {
                    (
                        String::new(),
                        json!({"error": "memory(action=forget) requires `memory_id` or `topic`"}),
                        HttpMethod::Post,
                    )
                }
            }
            // ── update → v1 correct (`PUT /v1/memories/:id/correct`) ─────
            //
            // v2's richer update (tags_add / tags_remove / importance) is
            // flattened into v1's single `new_content` + `reason` shape;
            // tag and importance fields are dropped until v1 grows support.
            //
            // `reason` is REQUIRED at the runtime boundary so corrections
            // carry their motivation for the audit trail.
            "update" => {
                let new_content = args
                    .get("content")
                    .or_else(|| args.get("new_content"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(reason) = reason else {
                    return (
                        String::new(),
                        json!({"error":
                            "memory(action=update) requires a non-empty `reason` (audit trail)"}),
                        HttpMethod::Post,
                    );
                };
                if let Some(mid) = args
                    .get("memory_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    let mut pl = json!({"new_content": new_content, "reason": reason});
                    inject_identity(&mut pl);
                    (
                        format!("{base}/v1/memories/{mid}/correct"),
                        pl,
                        HttpMethod::Put,
                    )
                } else if let Some(query) = args
                    .get("query")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    let mut pl =
                        json!({"query": query, "new_content": new_content, "reason": reason});
                    inject_identity(&mut pl);
                    (format!("{base}/v1/memories/correct"), pl, HttpMethod::Post)
                } else {
                    (
                        String::new(),
                        json!({"error": "memory(action=update) requires `memory_id` or `query`"}),
                        HttpMethod::Post,
                    )
                }
            }
            // ── feedback → v1 feedback (`POST /v1/memories/:id/feedback`) ─
            "feedback" => {
                let mid = args.get("memory_id").and_then(Value::as_str).unwrap_or("");
                let signal = args.get("signal").and_then(Value::as_str).unwrap_or("");
                if mid.is_empty() || signal.is_empty() {
                    return (
                        String::new(),
                        json!({"error":
                            "memory(action=feedback) requires `memory_id` and `signal` (useful|irrelevant|outdated|wrong)"}),
                        HttpMethod::Post,
                    );
                }
                let mut pl = json!({"signal": signal});
                if let Some(ctx) = args.get("context").and_then(Value::as_str) {
                    pl["context"] = json!(ctx);
                }
                (
                    format!("{base}/v1/memories/{mid}/feedback"),
                    pl,
                    HttpMethod::Post,
                )
            }
            // ── reflect → v1 reflect (`POST /v1/reflect`) ────────────────
            "reflect" => {
                let mut pl = json!({});
                if let Some(force) = args.get("force").and_then(Value::as_bool) {
                    pl["force"] = json!(force);
                }
                if let Some(mode) = args.get("mode").and_then(Value::as_str) {
                    pl["mode"] = json!(mode);
                }
                if let Some(limit) = args.get("limit").and_then(Value::as_i64) {
                    pl["limit"] = json!(limit);
                }
                inject_identity(&mut pl);
                (format!("{base}/v1/reflect"), pl, HttpMethod::Post)
            }
            // ── profile → v1 profile (`GET /v1/profiles/me`) ─────────────
            "profile" => {
                let mut pl = json!({});
                inject_identity(&mut pl);
                (format!("{base}/v1/profiles/me"), pl, HttpMethod::Get)
            }
            // ── focus → client-side synthetic (no v1 endpoint) ───────────
            //
            // v1 doesn't expose an attention-boost primitive, so the
            // dispatcher handles `focus` in-process: it stores a session-
            // scoped boost hint that subsequent `recall` calls consult.
            // Returning an empty endpoint tells the caller to short-circuit
            // before the HTTP client runs.
            "focus" => (
                String::new(),
                json!({"error": "memory(action=focus) is handled in-process; see dispatcher"}),
                HttpMethod::Post,
            ),
            _ => (
                String::new(),
                json!({"error": format!("Unknown memory action: {op}")}),
                HttpMethod::Post,
            ),
        }
    }

    fn validate_before_side_effects(op: &str, args: &Value) -> Option<Value> {
        match op {
            "remember" => {
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                if content.trim().is_empty() {
                    return Some(json!({"error": "memory(action=remember) requires `content`"}));
                }
                if contains_sensitive_memory_content(content) {
                    return Some(json!({"error":
                        "memory(action=remember) refused sensitive-looking content; do not store secrets, credentials, or tokens"}));
                }
                None
            }
            "forget" => {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if reason.is_none() {
                    return Some(json!({"error":
                        "memory(action=forget) requires a non-empty `reason` (audit trail)"}));
                }
                if args
                    .get("memory_ids")
                    .or_else(|| args.get("memory_id"))
                    .is_none()
                    && args.get("topic").and_then(Value::as_str).is_none()
                {
                    return Some(
                        json!({"error": "memory(action=forget) requires `memory_id` or `topic`"}),
                    );
                }
                None
            }
            "update" => {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if reason.is_none() {
                    return Some(json!({"error":
                        "memory(action=update) requires a non-empty `reason` (audit trail)"}));
                }
                let has_id = args
                    .get("memory_id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                let has_query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if !has_id && !has_query {
                    return Some(
                        json!({"error": "memory(action=update) requires `memory_id` or `query`"}),
                    );
                }
                if let Some(content) = args
                    .get("content")
                    .or_else(|| args.get("new_content"))
                    .and_then(Value::as_str)
                    && contains_sensitive_memory_content(content)
                {
                    return Some(json!({"error":
                        "memory(action=update) refused sensitive-looking content; do not store secrets, credentials, or tokens"}));
                }
                None
            }
            _ => None,
        }
    }
}

/// Classification of a memoria write candidate against an existing
/// corpus: either the content is truly new (→ `Store`) or it duplicates
/// an existing memory whose id is known (→ `Update`). Used by the
/// session-end extraction path so refinements route to `update` rather
/// than creating duplicate rows.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteDecision {
    /// No near-duplicate found. POST /v1/memories to create a new row.
    Store,
    /// A near-duplicate exists at this memory_id; caller should
    /// POST /v1/memories/{id}/correct to refine in place.
    Update { memory_id: String, score: f64 },
    /// Conflict check failed, so the safe choice is to skip rather than
    /// fail-open into a duplicate write.
    Skip { reason: String },
}

/// Given the `memories` array from a Memoria retrieve response, decide
/// whether the write should become a new row (`Store`) or an in-place
/// correction (`Update`) of the top hit.
///
/// Pure. Uses [`MemoriaClient::CONFLICT_SIMILARITY_FLOOR`] as the
/// threshold; entries missing `retrieval_score` or `memory_id` are
/// ignored. Highest-score hit wins.
pub fn classify_write(candidates: &[Value]) -> WriteDecision {
    let mut best: Option<(f64, String)> = None;
    for entry in candidates {
        let Some(score) = entry.get("retrieval_score").and_then(Value::as_f64) else {
            continue;
        };
        if score < MemoriaClient::CONFLICT_SIMILARITY_FLOOR {
            continue;
        }
        let Some(id) = entry.get("memory_id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        match best {
            None => best = Some((score, id.to_string())),
            Some((s, _)) if score > s => best = Some((score, id.to_string())),
            _ => {}
        }
    }
    match best {
        Some((score, memory_id)) => WriteDecision::Update { memory_id, score },
        None => WriteDecision::Store,
    }
}

/// Very small RFC3339-ish parser: returns "days since" for a timestamp
/// of the form `YYYY-MM-DDTHH:MM:SSZ` (or any prefix with a valid
/// `YYYY-MM-DD`). Returns `None` on malformed input or future dates.
fn parse_rfc3339_days_ago(ts: &str) -> Option<i64> {
    let date_part = ts.get(..10)?;
    let mut parts = date_part.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    let date_days = days_from_civil(y, m, d)?;
    let now_days = days_from_civil_now()?;
    let diff = now_days - date_days;
    if diff < 0 { None } else { Some(diff) }
}

fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || d == 0 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe as i64 * 365 + (yoe / 4) as i64 - (yoe / 100) as i64 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn days_from_civil_now() -> Option<i64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(secs / 86_400)
}

/// Given the `memories` array from a Memoria retrieve response, return
/// a conflict-redirect JSON blob if any entry crosses `floor`.
/// Factored out so the LLM-facing shape is unit-testable without
/// spinning up an HTTP server.
fn format_remember_conflict(arr: &[Value], floor: f64) -> Option<String> {
    let mut hits: Vec<(String, f64, String)> = Vec::new();
    for entry in arr {
        let Some(score) = entry.get("retrieval_score").and_then(Value::as_f64) else {
            continue;
        };
        if score < floor {
            continue;
        }
        let Some(id) = entry
            .get("memory_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
        else {
            continue;
        };
        let abstract_text = entry
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(140)
            .collect::<String>();
        hits.push((id, score, abstract_text));
    }
    if hits.is_empty() {
        return None;
    }
    Some(
        json!({
            "status": "conflict",
            "action_required": "update",
            "reason": "A similar memory already exists; update it instead of writing a duplicate.",
            "candidates": hits.iter().map(|(id, score, abs_text)| json!({
                "memory_id": id,
                "similarity": score,
                "abstract": abs_text,
            })).collect::<Vec<_>>(),
            "retry_hint": "Call memory(action=update, memory_id=<chosen_id>, content=<new_content>) \
                           to supersede, OR retry remember with skip_conflict_check=true if the \
                           new memory is intentionally distinct.",
        })
        .to_string(),
    )
}

/// Build a one-shot Memoria HTTP client + auth header.
pub fn memoria_oneshot_client(timeout_secs: u64) -> Option<(reqwest::Client, String, String)> {
    let mem = astra_core::MemoriaSettings::from_env();
    let key = mem.master_key?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .no_proxy()
        .build()
        .ok()?;
    Some((client, mem.base_url, key))
}

/// Fire-and-forget: trigger Memoria governance.
pub async fn memoria_governance_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(10) else {
        return;
    };
    let _ = client
        .post(format!("{base}/v1/governance"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}

/// Fire-and-forget: trigger Memoria graph consolidation.
pub async fn memoria_consolidate_fire_and_forget() {
    let Some((client, base, key)) = memoria_oneshot_client(15) else {
        return;
    };
    let _ = client
        .post(format!("{base}/v1/consolidate"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"force": false}))
        .send()
        .await;
}

// ── Cloud memory helpers (shared between CLI and server) ────────────────

/// Generic Memoria API request for cloud management operations.
pub async fn memoria_cloud_request(
    method: HttpMethod,
    path: &str,
    timeout_secs: u64,
    body: Option<serde_json::Value>,
) -> Result<String, String> {
    let (client, base, key) =
        memoria_oneshot_client(timeout_secs).ok_or("Memoria not configured")?;
    let url = format!("{base}{path}");
    let req = match method {
        HttpMethod::Get => client.get(&url),
        HttpMethod::Put => client.put(&url),
        HttpMethod::Post => client.post(&url),
    };
    let req = req.header("Authorization", format!("Bearer {key}"));
    let req = if let Some(b) = body {
        req.json(&b)
    } else {
        req
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(resp_body)
    } else {
        Err(format!("({status}) {resp_body}"))
    }
}

/// Build the deterministic snapshot name used by the auto-snapshot
/// that brackets `forget` / `update`. Pure; returns `pre_<op>_<ms>`.
pub fn pre_op_snapshot_name(op: &str, ts_ms: u128) -> String {
    format!("pre_{op}_{ts_ms}")
}

pub async fn memoria_snapshot_create(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        "/v1/snapshots",
        5,
        Some(json!({"name": name})),
    )
    .await
}
pub async fn memoria_snapshot_rollback(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        &format!("/v1/snapshots/{name}/rollback"),
        10,
        None,
    )
    .await
}
pub async fn memoria_snapshot_diff(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Get,
        &format!("/v1/snapshots/{name}/diff"),
        5,
        None,
    )
    .await
}
pub async fn memoria_snapshots_list() -> Result<String, String> {
    memoria_cloud_request(HttpMethod::Get, "/v1/snapshots", 5, None).await
}
pub async fn memoria_branch_create(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        "/v1/branches",
        5,
        Some(json!({"name": name})),
    )
    .await
}
pub async fn memoria_branch_checkout(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        &format!("/v1/branches/{name}/checkout"),
        5,
        None,
    )
    .await
}
pub async fn memoria_branch_merge(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        &format!("/v1/branches/{name}/merge"),
        5,
        None,
    )
    .await
}
pub async fn memoria_branch_diff(name: &str) -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Get,
        &format!("/v1/branches/{name}/diff"),
        5,
        None,
    )
    .await
}
pub async fn memoria_branches_list() -> Result<String, String> {
    memoria_cloud_request(HttpMethod::Get, "/v1/branches", 5, None).await
}
pub async fn memoria_reflect() -> Result<String, String> {
    memoria_cloud_request(
        HttpMethod::Post,
        "/v1/reflect",
        15,
        Some(json!({"mode": "auto"})),
    )
    .await
}
pub async fn memoria_health() -> Result<String, String> {
    memoria_cloud_request(HttpMethod::Get, "/v1/health/analyze", 5, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conflict_returns_none_below_floor() {
        let arr = vec![
            json!({"memory_id": "a", "retrieval_score": 0.70, "content": "x"}),
            json!({"memory_id": "b", "retrieval_score": 0.50, "content": "y"}),
        ];
        assert!(format_remember_conflict(&arr, 0.85).is_none());
    }

    #[test]
    fn conflict_returns_none_on_empty() {
        assert!(format_remember_conflict(&[], 0.85).is_none());
    }

    #[test]
    fn conflict_surfaces_high_similarity_hit_as_update_redirect() {
        let arr = vec![
            json!({
                "memory_id": "m-42",
                "retrieval_score": 0.93,
                "content": "Integration tests must hit a real database\nWhy: prior incident",
            }),
            json!({"memory_id": "low", "retrieval_score": 0.41, "content": "noise"}),
        ];
        let out = format_remember_conflict(&arr, 0.85).expect("conflict");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["status"], "conflict");
        assert_eq!(parsed["action_required"], "update");
        let candidates = parsed["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["memory_id"], "m-42");
        assert_eq!(candidates[0]["similarity"].as_f64().unwrap(), 0.93);
        // First-line abstract only.
        assert_eq!(
            candidates[0]["abstract"],
            "Integration tests must hit a real database"
        );
    }

    #[test]
    fn conflict_skips_entries_missing_score_or_id() {
        let arr = vec![
            json!({"memory_id": "ok", "retrieval_score": 0.90, "content": "hit"}),
            json!({"retrieval_score": 0.95, "content": "missing id"}),
            json!({"memory_id": "no_score", "content": "missing score"}),
        ];
        let out = format_remember_conflict(&arr, 0.85).expect("conflict");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["candidates"][0]["memory_id"], "ok");
    }

    #[test]
    fn conflict_truncates_long_abstract() {
        let long = "x".repeat(500);
        let arr = vec![json!({
            "memory_id": "m",
            "retrieval_score": 0.95,
            "content": long,
        })];
        let out = format_remember_conflict(&arr, 0.85).expect("conflict");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let abstract_text = parsed["candidates"][0]["abstract"].as_str().unwrap();
        assert!(abstract_text.chars().count() <= 140);
    }

    #[test]
    fn map_business_types_to_memoria_primitives() {
        use astra_prompts::memory_types::normalize_memoria_type;
        assert_eq!(normalize_memoria_type("user"), "profile");
        assert_eq!(normalize_memoria_type("feedback"), "semantic");
        assert_eq!(normalize_memoria_type("project"), "semantic");
        assert_eq!(normalize_memoria_type("lesson"), "semantic");
        assert_eq!(normalize_memoria_type("ref"), "procedural");
        assert_eq!(normalize_memoria_type("reference"), "procedural");
        assert_eq!(normalize_memoria_type("episode"), "episodic");
        // V1 primitives pass through unchanged
        assert_eq!(normalize_memoria_type("semantic"), "semantic");
        assert_eq!(normalize_memoria_type("profile"), "profile");
        assert_eq!(normalize_memoria_type("working"), "working");
    }

    #[test]
    fn store_maps_business_type_before_sending() {
        let args = json!({"content": "test", "memory_type": "feedback"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "remember", &args);
        assert_eq!(
            pl["memory_type"], "semantic",
            "business type 'feedback' must be mapped to 'semantic' for Memoria V1"
        );
    }

    #[test]
    fn build_direct_request_propagates_session_and_user_id() {
        let args = json!({
            "query": "rust patterns",
            "top_k": 3,
            "session_id": "user-42",
            "user_id": "user-42"
        });

        // retrieve
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");
        assert_eq!(pl["query"], "rust patterns");
        assert!(
            pl.get("min_confidence").is_none(),
            "min_confidence should only be sent when explicitly provided"
        );

        // search
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // store
        let store_args = json!({
            "content": "hello",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "remember", &store_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // purge — Memoria requires ONLY ONE of memory_ids, topic, session_id.
        // inject_identity is NOT called (would add session_id alongside topic → 422).
        // `reason` is REQUIRED at the runtime boundary (P8).
        let purge_args = json!({
            "topic": "old",
            "reason": "user asked to forget",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &purge_args);
        assert_eq!(pl["topic"], "old", "purge should use topic as the filter");
        assert!(
            pl.get("session_id").is_none() || pl.get("topic").is_some(),
            "purge must not send both topic AND session_id"
        );

        // correct — `reason` is REQUIRED at the runtime boundary (P8).
        let correct_args = json!({
            "memory_id": "m1",
            "new_content": "fixed",
            "reason": "refined after user clarification",
            "session_id": "user-42",
            "user_id": "user-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "update", &correct_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");

        // profile
        let profile_args = json!({"session_id": "user-42", "user_id": "user-42"});
        let (_, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "profile", &profile_args);
        assert_eq!(pl["session_id"], "user-42");
        assert_eq!(pl["user_id"], "user-42");
    }

    #[test]
    fn build_direct_request_omits_identity_when_absent() {
        let args = json!({"query": "test"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("user_id").is_none());
        assert!(
            pl.get("min_confidence").is_none(),
            "min_confidence omitted when not provided"
        );
    }

    #[test]
    fn build_direct_request_retrieve_respects_explicit_min_confidence() {
        let args = json!({"query": "q", "min_confidence": 0.7});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(pl["min_confidence"], json!(0.7));
    }

    // ── Session isolation via v2 `scope` → v1 `session_scope` ──

    #[test]
    fn recall_scope_session_requires_session_id() {
        let args = json!({"query": "test", "top_k": 5, "scope": "session"});
        let (endpoint, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(
            endpoint.is_empty(),
            "scope=session without session_id must short-circuit to an error"
        );
        assert!(
            pl.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("session_id"),
            "error must mention missing session_id"
        );
    }

    #[test]
    fn recall_scope_session_sets_session_scope_only() {
        let args = json!({
            "query": "test",
            "top_k": 5,
            "session_id": "sess-abc",
            "scope": "session",
        });
        let (endpoint, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
        assert_eq!(pl["session_id"], "sess-abc");
        assert_eq!(pl["session_scope"], "only");
    }

    #[test]
    fn recall_scope_all_omits_session_scope() {
        let args = json!({
            "query": "test",
            "top_k": 10,
            "session_id": "sess-abc",
            "scope": "all",
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_scope").is_none());
    }

    #[test]
    fn recall_omits_session_fields_when_absent() {
        let args = json!({"query": "test", "top_k": 10});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("session_scope").is_none());
    }

    #[test]
    fn recall_routes_to_v1_retrieve_endpoint() {
        let args = json!({"query": "test", "top_k": 10});
        let (endpoint, _, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert_eq!(endpoint, "http://mem/v1/memories/retrieve");
    }

    // ── Visibility (private / team) ─────────────────────────────────

    #[test]
    fn remember_defaults_to_private_no_team_tag() {
        let args = json!({"content": "private fact", "memory_type": "feedback"});
        let (endpoint, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "remember", &args);
        assert_eq!(endpoint, "http://mem/v1/memories");
        // Category tag is present but no team tag.
        let tags: Vec<&str> = pl["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(tags.contains(&"astra:feedback"));
        assert!(
            !tags.iter().any(|t| t.starts_with("astra:team:")),
            "private memory must not carry a team tag"
        );
    }

    #[test]
    fn remember_team_visibility_adds_team_tag() {
        let args = json!({
            "content": "team-wide convention",
            "memory_type": "feedback",
            "visibility": "team",
            "team_id": "core-infra",
        });
        let (endpoint, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "remember", &args);
        assert_eq!(endpoint, "http://mem/v1/memories");
        let tags: Vec<&str> = pl["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(tags.contains(&"astra:team:core-infra"));
        assert!(tags.contains(&"astra:feedback"));
    }

    #[test]
    fn remember_team_visibility_without_team_id_fails_loudly() {
        let args = json!({
            "content": "whatever",
            "memory_type": "feedback",
            "visibility": "team",
        });
        let (endpoint, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "remember", &args);
        assert!(endpoint.is_empty(), "must short-circuit without team_id");
        assert!(
            pl["error"].as_str().unwrap_or("").contains("team_id"),
            "error must mention team_id"
        );
    }

    #[test]
    fn remember_rejects_unknown_visibility() {
        let args = json!({
            "content": "x",
            "memory_type": "feedback",
            "visibility": "world",
        });
        let (endpoint, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "remember", &args);
        assert!(endpoint.is_empty());
        assert!(
            pl["error"]
                .as_str()
                .unwrap_or("")
                .contains("invalid visibility"),
            "error must call out invalid visibility: {pl:?}"
        );
    }

    #[test]
    fn remember_preserves_caller_tags_alongside_team_tag() {
        let args = json!({
            "content": "x",
            "memory_type": "feedback",
            "visibility": "team",
            "team_id": "t1",
            "tags": ["custom:tag", "astra:feedback"],  // include one duplicate
        });
        let (_endpoint, pl, _) =
            MemoriaClient::build_direct_request("http://mem", "remember", &args);
        let tags: Vec<&str> = pl["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // No duplicates of astra:feedback or astra:team:t1
        assert_eq!(
            tags.iter().filter(|t| **t == "astra:feedback").count(),
            1,
            "category tag must not duplicate: {tags:?}"
        );
        assert_eq!(tags.iter().filter(|t| **t == "astra:team:t1").count(), 1);
        assert!(tags.contains(&"custom:tag"));
    }

    #[test]
    fn recall_team_visibility_forwards_include_tags() {
        let args = json!({
            "query": "testing conventions",
            "top_k": 5,
            "visibility": "team",
            "team_id": "core-infra",
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        let include_tags: Vec<&str> = pl["include_tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(include_tags, vec!["astra:team:core-infra"]);
    }

    #[test]
    fn recall_team_visibility_supports_multiple_team_ids() {
        let args = json!({
            "query": "q",
            "top_k": 5,
            "visibility": "team",
            "team_ids": ["t1", "t2"],
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        let include_tags: Vec<&str> = pl["include_tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(include_tags, vec!["astra:team:t1", "astra:team:t2"]);
    }

    #[test]
    fn recall_private_visibility_omits_include_tags() {
        let args = json!({"query": "q", "top_k": 5});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "recall", &args);
        assert!(pl.get("include_tags").is_none());
    }

    // ── purge exclusivity (Memoria requires ONE of memory_ids/topic/session_id) ──

    #[test]
    fn purge_with_topic_does_not_include_session_id() {
        let args = json!({
            "topic": "NEPTUNE",
            "reason": "obsolete project",
            "session_id": "sess-42"
        });
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert_eq!(pl["topic"], "NEPTUNE");
        assert!(
            pl.get("session_id").is_none(),
            "purge by topic must not include session_id"
        );
    }

    #[test]
    fn purge_with_memory_ids() {
        let args = json!({"memory_ids": ["id1", "id2"], "reason": "user cleanup"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(pl["memory_ids"].is_array());
        assert!(pl.get("topic").is_none());
    }

    #[test]
    fn purge_with_memory_id_string_becomes_array() {
        let args = json!({"memory_id": "id1,id2", "reason": "batch cleanup"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        let ids = pl["memory_ids"].as_array().expect("should be array");
        assert_eq!(ids.len(), 2);
    }

    // ── P9: auto-snapshot naming helper (pure) ────────────────────────

    #[test]
    fn pre_op_snapshot_name_format() {
        use super::*;
        assert_eq!(
            pre_op_snapshot_name("forget", 1_700_000_000_000),
            "pre_forget_1700000000000"
        );
        assert_eq!(pre_op_snapshot_name("update", 42), "pre_update_42");
    }

    /// R2: auto-snapshot must happen AFTER `build_direct_request`
    /// validates args. A rejected `forget` (missing reason, missing
    /// memory_id/topic, etc.) must not produce an orphan `pre_forget_*`
    /// snapshot. Verified by source ordering: in `call_with_timeout`
    /// the `memoria_snapshot_create` call must appear after the early-
    /// return `ep.is_empty()` guard.
    #[test]
    fn auto_snapshot_is_ordered_after_validation() {
        let src = include_str!("memoria.rs");
        let fn_start = src
            .find("pub async fn call_with_timeout")
            .expect("call_with_timeout must exist");
        // Find the first Err / return-style terminator so we bound the body.
        let fn_end = src[fn_start..]
            .find("\n    /// Boost search")
            .map(|i| fn_start + i)
            .expect("fn body end sentinel not found");
        let body = &src[fn_start..fn_end];

        let snapshot_at = body
            .find("memoria_snapshot_create")
            .expect("auto-snapshot call must exist in call_with_timeout");
        let validation_short_circuit_at = body
            .find("if ep.is_empty()")
            .expect("validation short-circuit must exist");
        assert!(
            snapshot_at > validation_short_circuit_at,
            "auto-snapshot must happen AFTER the `if ep.is_empty()` short-\
             circuit so rejected destructive calls don't create orphan \
             `pre_<op>_*` snapshots"
        );
    }

    // ── P8: reason required on forget / update ────────────────────────

    #[test]
    fn forget_without_reason_rejected_loudly() {
        let args = json!({"memory_id": "m1"});
        let (ep, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(ep.is_empty(), "no endpoint → runtime short-circuits");
        let err = pl["error"].as_str().unwrap();
        assert!(err.contains("reason"), "error mentions reason: {err}");
        assert!(err.contains("audit"), "error mentions audit: {err}");
    }

    #[test]
    fn forget_with_empty_reason_rejected() {
        let args = json!({"memory_id": "m1", "reason": "   "});
        let (ep, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(ep.is_empty(), "whitespace-only reason is empty");
        assert!(pl["error"].as_str().unwrap().contains("reason"));
    }

    #[test]
    fn update_without_reason_rejected_loudly() {
        let args = json!({"memory_id": "m1", "new_content": "fixed"});
        let (ep, pl, _) = MemoriaClient::build_direct_request("http://mem", "update", &args);
        assert!(ep.is_empty());
        assert!(pl["error"].as_str().unwrap().contains("reason"));
    }

    #[test]
    fn update_with_empty_reason_rejected() {
        let args = json!({"memory_id": "m1", "new_content": "fixed", "reason": ""});
        let (ep, pl, _) = MemoriaClient::build_direct_request("http://mem", "update", &args);
        assert!(ep.is_empty());
        assert!(pl["error"].as_str().unwrap().contains("reason"));
    }

    #[test]
    fn update_with_valid_reason_builds_correct_endpoint() {
        let args = json!({
            "memory_id": "m1",
            "new_content": "fixed",
            "reason": "user said the tool name changed last week"
        });
        let (ep, pl, _) = MemoriaClient::build_direct_request("http://mem", "update", &args);
        assert!(ep.ends_with("/v1/memories/m1/correct"));
        assert_eq!(
            pl["reason"].as_str(),
            Some("user said the tool name changed last week")
        );
    }

    #[test]
    fn forget_with_valid_reason_builds_purge_endpoint() {
        let args = json!({
            "memory_id": "m1",
            "reason": "memory is stale; user confirmed"
        });
        let (ep, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(ep.ends_with("/v1/memories/purge"));
        assert_eq!(
            pl["reason"].as_str(),
            Some("memory is stale; user confirmed")
        );
    }
}

#[cfg(test)]
mod memoria_http_client_tests {
    use super::*;

    #[test]
    fn purge_session_id_not_supported() {
        // Memoria PurgeRequest only accepts memory_ids and topic.
        // session_id is NOT a valid filter — it would cause 422.
        let args = json!({"session_id": "sess-42"});
        let (ep, _, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(
            ep.is_empty(),
            "purge with only session_id must fail (not supported by Memoria)"
        );
    }

    #[test]
    fn purge_empty_filter_returns_error() {
        let args = json!({});
        let (name, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert_eq!(name, "");
        assert!(pl.get("error").is_some());
        assert!(
            pl["error"]
                .as_str()
                .unwrap()
                .contains("memory(action=forget)")
        );
    }

    #[test]
    fn purge_topic_returns_topic_filter() {
        let args = json!({"topic": "NEPTUNE", "reason": "obsolete project"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert_eq!(pl["topic"], "NEPTUNE");
        assert!(pl.get("session_id").is_none());
        assert!(pl.get("memory_ids").is_none());
    }

    #[test]
    fn purge_responses_are_not_empty() {
        let args = json!({"topic": "NEPTUNE", "reason": "project wound down"});
        let (_, pl, _) = MemoriaClient::build_direct_request("http://mem", "forget", &args);
        assert!(
            pl.is_object() && !pl.as_object().unwrap().contains_key("error"),
            "purge with valid filter must produce non-error payload, got: {pl}"
        );
    }

    #[test]
    fn purge_result_to_agent_response_delivers_message() {
        use super::*;
        let raw = json!({"deleted_count": 3});
        let enriched = MemoriaClient::purge_result_to_agent_response(&raw, "topic:NEPTUNE");
        assert_eq!(enriched["status"], "ok");
        assert_eq!(enriched["deleted_count"], 3);
        assert!(enriched["message"].as_str().unwrap().contains("3"));
    }

    #[test]
    fn purge_result_to_agent_response_zero_deleted() {
        use super::*;
        let raw = json!({"deleted_count": 0});
        let enriched = MemoriaClient::purge_result_to_agent_response(&raw, "session:abc");
        assert_eq!(enriched["deleted_count"], 0);
        assert!(enriched["message"].as_str().unwrap().contains("0 deleted"));
    }

    // ── P6: decorate_recall_response (freshness + surface-once) ────────

    fn days_ago_ts(days: i64) -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let total_days = secs / 86_400 - days;
        // Inverse Howard-Hinnant
        let z = total_days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
    }

    #[test]
    fn decorate_recall_appends_freshness_suffix_per_memory() {
        use super::*;
        let raw = serde_json::json!([
            {
                "memory_id": "m1",
                "content": "fresh memory",
                "observed_at": days_ago_ts(0),
                "trust_tier": "T1",
            },
            {
                "memory_id": "m2",
                "content": "mid memory",
                "observed_at": days_ago_ts(10),
                "trust_tier": "T3",
            },
            {
                "memory_id": "m3",
                "content": "stale memory",
                "observed_at": days_ago_ts(200),
                "trust_tier": "T3",
            },
        ])
        .to_string();
        let seen = std::collections::HashSet::new();
        let mut newly = Vec::new();
        let out = MemoriaClient::decorate_recall_response(&raw, &seen, &mut newly);
        let arr: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(
            arr[0]["content"].as_str().unwrap(),
            "fresh memory",
            "≤1 day → no suffix"
        );
        let c2 = arr[1]["content"].as_str().unwrap();
        assert!(
            c2.ends_with(" (within the two months)"),
            "T3 10d bucket mismatch: {c2}"
        );
        let c3 = arr[2]["content"].as_str().unwrap();
        assert!(
            c3.ends_with(" (stale — verify first)"),
            "T3 200d → stale, got {c3}"
        );
        assert_eq!(newly, vec!["m1", "m2", "m3"]);
    }

    #[test]
    fn decorate_recall_filters_already_surfaced_ids() {
        use super::*;
        let raw = serde_json::json!([
            {"memory_id": "m-seen", "content": "old one"},
            {"memory_id": "m-new", "content": "new one"},
        ])
        .to_string();
        let mut seen = std::collections::HashSet::new();
        seen.insert("m-seen".to_string());
        let mut newly = Vec::new();
        let out = MemoriaClient::decorate_recall_response(&raw, &seen, &mut newly);
        let arr: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(arr.len(), 1, "seen id must be filtered");
        assert_eq!(arr[0]["memory_id"].as_str(), Some("m-new"));
        assert_eq!(newly, vec!["m-new"], "only surviving ids recorded");
    }

    #[test]
    fn decorate_recall_passes_through_non_array_bodies() {
        use super::*;
        let err = r#"{"error": "server down"}"#;
        let seen = std::collections::HashSet::new();
        let mut newly = Vec::new();
        let out = MemoriaClient::decorate_recall_response(err, &seen, &mut newly);
        assert_eq!(out, err);
        assert!(newly.is_empty());
    }

    #[test]
    fn decorate_recall_passes_through_invalid_json() {
        use super::*;
        let bad = "not json at all";
        let seen = std::collections::HashSet::new();
        let mut newly = Vec::new();
        let out = MemoriaClient::decorate_recall_response(bad, &seen, &mut newly);
        assert_eq!(out, bad);
    }

    #[test]
    fn decorate_recall_empty_array_is_noop() {
        use super::*;
        let seen = std::collections::HashSet::new();
        let mut newly = Vec::new();
        let out = MemoriaClient::decorate_recall_response("[]", &seen, &mut newly);
        let arr: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert!(arr.is_empty());
        assert!(newly.is_empty());
    }

    // ── P4: classify_write (extraction conflict gate) ──────────────────

    #[test]
    fn classify_write_returns_store_when_no_hits_above_floor() {
        use super::*;
        let candidates = vec![
            serde_json::json!({"memory_id": "m1", "retrieval_score": 0.70}),
            serde_json::json!({"memory_id": "m2", "retrieval_score": 0.40}),
        ];
        assert_eq!(classify_write(&candidates), WriteDecision::Store);
    }

    #[test]
    fn classify_write_returns_update_on_duplicate() {
        use super::*;
        let candidates = vec![
            serde_json::json!({"memory_id": "m-dup", "retrieval_score": 0.92}),
            serde_json::json!({"memory_id": "m-low", "retrieval_score": 0.50}),
        ];
        match classify_write(&candidates) {
            WriteDecision::Update { memory_id, score } => {
                assert_eq!(memory_id, "m-dup");
                assert!((score - 0.92).abs() < 1e-6);
            }
            d => panic!("expected Update, got {d:?}"),
        }
    }

    #[test]
    fn classify_write_picks_highest_scoring_duplicate() {
        use super::*;
        let candidates = vec![
            serde_json::json!({"memory_id": "m-a", "retrieval_score": 0.86}),
            serde_json::json!({"memory_id": "m-b", "retrieval_score": 0.95}),
            serde_json::json!({"memory_id": "m-c", "retrieval_score": 0.88}),
        ];
        match classify_write(&candidates) {
            WriteDecision::Update { memory_id, .. } => assert_eq!(memory_id, "m-b"),
            d => panic!("expected Update, got {d:?}"),
        }
    }

    #[test]
    fn classify_write_skips_entries_missing_id_or_score() {
        use super::*;
        let candidates = vec![
            serde_json::json!({"retrieval_score": 0.99}), // no id
            serde_json::json!({"memory_id": "m", "retrieval_score": 0.92}),
            serde_json::json!({"memory_id": ""}), // no score + empty id
        ];
        match classify_write(&candidates) {
            WriteDecision::Update { memory_id, .. } => assert_eq!(memory_id, "m"),
            d => panic!("expected Update, got {d:?}"),
        }
    }

    #[test]
    fn classify_write_floor_is_exactly_0_85() {
        use super::*;
        // just below floor → Store
        let below = vec![serde_json::json!({"memory_id": "m", "retrieval_score": 0.84})];
        assert_eq!(classify_write(&below), WriteDecision::Store);
        // exactly at floor → Update (tie-goes-to-dup)
        let at = vec![serde_json::json!({"memory_id": "m", "retrieval_score": 0.85})];
        assert!(matches!(classify_write(&at), WriteDecision::Update { .. }));
    }

    #[test]
    fn seen_store_is_isolated_across_sessions() {
        use super::*;
        // Use unique session names so tests running concurrently don't
        // poison each other via the process-global store.
        MemoriaClient::record_seen("p6-seen-isolation-a", ["m1".into()]);
        assert!(MemoriaClient::seen_snapshot("p6-seen-isolation-a").contains("m1"));
        assert!(MemoriaClient::seen_snapshot("p6-seen-isolation-b").is_empty());
        MemoriaClient::reset_seen("p6-seen-isolation-a");
    }

    #[test]
    fn reset_seen_clears_session_state() {
        use super::*;
        MemoriaClient::record_seen("p6-reset-sess", ["m1".into(), "m2".into()]);
        MemoriaClient::reset_seen("p6-reset-sess");
        assert!(MemoriaClient::seen_snapshot("p6-reset-sess").is_empty());
    }

    #[test]
    fn focus_hints_survive_new_client_instances() {
        use super::*;
        let session_id = "p6-focus-global";
        MemoriaClient::reset_focus(session_id);
        let c1 = MemoriaClient::new(None, None);
        let c2 = MemoriaClient::new(None, None);
        let response = c1.focus_set(
            session_id,
            &json!({
                "focus_type": "topic",
                "focus_value": "memory-runtime",
                "boost": 2.0,
            }),
        );
        assert!(response.contains("\"status\":\"ok\""));

        let mut payload = json!({"query": "review", "top_k": 5});
        c2.apply_focus_hints(session_id, &mut payload);
        assert_eq!(payload["boost_topics"][0]["value"], "memory-runtime");
        assert_eq!(payload["boost_topics"][0]["boost"], 2.0);
        MemoriaClient::reset_focus(session_id);
    }

    #[test]
    fn remember_rejects_sensitive_secret_content() {
        use super::*;
        let args = json!({
            "content": "production api_key: sk-live-1234567890abcdef",
            "memory_type": "semantic",
        });
        let (endpoint, payload, _) =
            MemoriaClient::build_direct_request("http://mem", "remember", &args);
        assert!(
            endpoint.is_empty(),
            "secret-bearing memory must not be sent"
        );
        assert!(
            payload["error"]
                .as_str()
                .unwrap_or_default()
                .contains("sensitive")
        );
    }

    #[tokio::test]
    async fn cloud_path_validates_destructive_args_before_network() {
        use super::*;
        let client = MemoriaClient::new(Some("http://127.0.0.1:9".into()), Some("token".into()));
        let out = client
            .call_with_timeout(
                "forget",
                &json!({"memory_id": "m1"}),
                Duration::from_millis(1),
            )
            .await;
        assert!(out.contains("requires a non-empty `reason`"));
    }

    // ── R5: recall ledger (process-global, for feedback loop) ─────────

    #[test]
    fn record_recall_pushes_ids_onto_session_queue() {
        use super::*;
        MemoriaClient::reset_recall_ledger("r5-single");
        MemoriaClient::record_recall("r5-single", 3, vec!["m1".into(), "m2".into()]);
        assert_eq!(MemoriaClient::pending_recall_count("r5-single"), 1);
        let drained = MemoriaClient::drain_recalls("r5-single", None);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].memory_ids, vec!["m1", "m2"]);
        assert_eq!(drained[0].turn, 3);
        assert_eq!(MemoriaClient::pending_recall_count("r5-single"), 0);
    }

    #[test]
    fn record_recall_caps_queue_depth() {
        use super::*;
        MemoriaClient::reset_recall_ledger("r5-cap");
        for i in 0..20 {
            MemoriaClient::record_recall("r5-cap", i, vec![format!("m{i}")]);
        }
        assert!(MemoriaClient::pending_recall_count("r5-cap") <= 16);
        MemoriaClient::reset_recall_ledger("r5-cap");
    }

    #[test]
    fn drain_recalls_respects_max_age() {
        use super::*;
        MemoriaClient::reset_recall_ledger("r5-age");
        MemoriaClient::record_recall("r5-age", 1, vec!["stale".into()]);
        std::thread::sleep(Duration::from_millis(15));
        MemoriaClient::record_recall("r5-age", 2, vec!["fresh".into()]);
        let drained = MemoriaClient::drain_recalls("r5-age", Some(Duration::from_millis(5)));
        // Stale entry filtered out; fresh one survives.
        let ids: Vec<&str> = drained
            .iter()
            .flat_map(|s| s.memory_ids.iter().map(String::as_str))
            .collect();
        assert_eq!(ids, vec!["fresh"]);
    }

    #[test]
    fn record_recall_empty_ids_is_noop() {
        use super::*;
        MemoriaClient::reset_recall_ledger("r5-empty");
        MemoriaClient::record_recall("r5-empty", 1, vec![]);
        assert_eq!(MemoriaClient::pending_recall_count("r5-empty"), 0);
    }

    #[test]
    fn record_recall_empty_session_is_noop() {
        use super::*;
        MemoriaClient::record_recall("", 1, vec!["m1".into()]);
        assert!(MemoriaClient::drain_recalls("", None).is_empty());
    }

    #[test]
    fn drain_recalls_fifo_order_preserved() {
        use super::*;
        MemoriaClient::reset_recall_ledger("r5-fifo");
        MemoriaClient::record_recall("r5-fifo", 1, vec!["first".into()]);
        MemoriaClient::record_recall("r5-fifo", 2, vec!["second".into()]);
        MemoriaClient::record_recall("r5-fifo", 3, vec!["third".into()]);
        let drained = MemoriaClient::drain_recalls("r5-fifo", None);
        let turns: Vec<u32> = drained.iter().map(|s| s.turn).collect();
        assert_eq!(turns, vec![1, 2, 3]);
    }

    #[test]
    fn reset_recall_ledger_empties_session_state() {
        use super::*;
        MemoriaClient::record_recall("r5-reset", 1, vec!["m1".into()]);
        MemoriaClient::reset_recall_ledger("r5-reset");
        assert_eq!(MemoriaClient::pending_recall_count("r5-reset"), 0);
    }

    #[test]
    fn reset_session_process_state_clears_all_memory_globals() {
        use super::*;
        let session_id = "r5-reset-all";
        let client = MemoriaClient::new(None, None);
        MemoriaClient::record_seen(session_id, ["seen-1".into()]);
        client.focus_set(
            session_id,
            &json!({
                "focus_type": "topic",
                "focus_value": "cleanup",
            }),
        );
        MemoriaClient::record_recall(session_id, 4, vec!["recall-1".into()]);

        assert!(!MemoriaClient::seen_snapshot(session_id).is_empty());
        assert_eq!(MemoriaClient::pending_recall_count(session_id), 1);
        let mut recall_payload = json!({"query": "cleanup"});
        client.apply_focus_hints(session_id, &mut recall_payload);
        assert!(recall_payload.get("boost_topics").is_some());

        MemoriaClient::reset_session_process_state(session_id);

        assert!(MemoriaClient::seen_snapshot(session_id).is_empty());
        assert_eq!(MemoriaClient::pending_recall_count(session_id), 0);
        let mut after_reset_payload = json!({"query": "cleanup"});
        client.apply_focus_hints(session_id, &mut after_reset_payload);
        assert!(after_reset_payload.get("boost_topics").is_none());
    }

    #[tokio::test]
    async fn feedback_pending_recalls_drains_queue_once() {
        use super::*;
        let session_id = "r5-feedback-drain";
        MemoriaClient::reset_recall_ledger(session_id);
        MemoriaClient::record_recall(session_id, 7, vec!["m1".into(), "m2".into()]);
        let client = MemoriaClient::new(Some("http://127.0.0.1:9".into()), Some("token".into()));
        let attempted = client
            .feedback_pending_recalls(session_id, "useful", "unit-test")
            .await;
        assert_eq!(attempted.attempted, 2);
        assert_eq!(attempted.failed, 2);
        assert_eq!(attempted.succeeded, 0);
        assert_eq!(MemoriaClient::pending_recall_count(session_id), 0);
        let attempted_again = client
            .feedback_pending_recalls(session_id, "useful", "unit-test")
            .await;
        assert_eq!(attempted_again.attempted, 0);
    }

    // ── Tests for suppress_memory / unsuppress_memory ────────────────────

    #[test]
    fn suppress_memory_adds_to_session_store() {
        use super::*;
        let sid = "t-suppress-add";
        MemoriaClient::reset_suppressed(sid);

        MemoriaClient::suppress_memory(sid, "mem-001");
        let snap = MemoriaClient::suppressed_snapshot(sid);
        assert!(snap.contains("mem-001"));
        assert_eq!(snap.len(), 1);

        MemoriaClient::reset_suppressed(sid);
    }

    #[test]
    fn suppress_memory_multiple_ids() {
        use super::*;
        let sid = "t-suppress-multi";
        MemoriaClient::reset_suppressed(sid);

        MemoriaClient::suppress_memory(sid, "m1");
        MemoriaClient::suppress_memory(sid, "m2");
        MemoriaClient::suppress_memory(sid, "m3");
        let snap = MemoriaClient::suppressed_snapshot(sid);
        assert_eq!(snap.len(), 3);
        assert!(snap.contains("m1"));
        assert!(snap.contains("m2"));
        assert!(snap.contains("m3"));

        MemoriaClient::reset_suppressed(sid);
    }

    #[test]
    fn unsuppress_memory_removes_from_store() {
        use super::*;
        let sid = "t-unsuppress";
        MemoriaClient::reset_suppressed(sid);

        MemoriaClient::suppress_memory(sid, "m1");
        MemoriaClient::suppress_memory(sid, "m2");
        MemoriaClient::unsuppress_memory(sid, "m1");
        let snap = MemoriaClient::suppressed_snapshot(sid);
        assert_eq!(snap.len(), 1);
        assert!(!snap.contains("m1"));
        assert!(snap.contains("m2"));

        MemoriaClient::reset_suppressed(sid);
    }

    #[test]
    fn suppress_store_isolated_across_sessions() {
        use super::*;
        let sid_a = "t-suppress-iso-a";
        let sid_b = "t-suppress-iso-b";
        MemoriaClient::reset_suppressed(sid_a);
        MemoriaClient::reset_suppressed(sid_b);

        MemoriaClient::suppress_memory(sid_a, "m1");
        assert!(MemoriaClient::suppressed_snapshot(sid_a).contains("m1"));
        assert!(MemoriaClient::suppressed_snapshot(sid_b).is_empty());

        MemoriaClient::reset_suppressed(sid_a);
    }

    #[test]
    fn suppress_empty_session_id_is_noop() {
        use super::*;
        MemoriaClient::suppress_memory("", "m1");
        assert!(MemoriaClient::suppressed_snapshot("").is_empty());
    }

    #[test]
    fn suppress_empty_memory_id_is_noop() {
        use super::*;
        let sid = "t-suppress-empty-mid";
        MemoriaClient::reset_suppressed(sid);
        MemoriaClient::suppress_memory(sid, "");
        assert!(MemoriaClient::suppressed_snapshot(sid).is_empty());
        MemoriaClient::reset_suppressed(sid);
    }

    #[test]
    fn reset_suppressed_clears_all() {
        use super::*;
        let sid = "t-suppress-reset";
        MemoriaClient::suppress_memory(sid, "m1");
        MemoriaClient::suppress_memory(sid, "m2");
        MemoriaClient::reset_suppressed(sid);
        assert!(MemoriaClient::suppressed_snapshot(sid).is_empty());
    }

    // ── Tests for release_context / released_snapshot ────────────────────

    #[test]
    fn release_context_adds_tool_call_id() {
        use super::*;
        let sid = "t-release-add";
        MemoriaClient::reset_released(sid);

        MemoriaClient::release_context(sid, "call_001");
        let snap = MemoriaClient::released_snapshot(sid);
        assert!(snap.contains("call_001"));
        assert_eq!(snap.len(), 1);

        MemoriaClient::reset_released(sid);
    }

    #[test]
    fn release_context_multiple_ids() {
        use super::*;
        let sid = "t-release-multi";
        MemoriaClient::reset_released(sid);

        MemoriaClient::release_context(sid, "c1");
        MemoriaClient::release_context(sid, "c2");
        MemoriaClient::release_context(sid, "c3");
        let snap = MemoriaClient::released_snapshot(sid);
        assert_eq!(snap.len(), 3);

        MemoriaClient::reset_released(sid);
    }

    #[test]
    fn released_store_isolated_across_sessions() {
        use super::*;
        let sid_a = "t-release-iso-a";
        let sid_b = "t-release-iso-b";
        MemoriaClient::reset_released(sid_a);
        MemoriaClient::reset_released(sid_b);

        MemoriaClient::release_context(sid_a, "c1");
        assert!(MemoriaClient::released_snapshot(sid_a).contains("c1"));
        assert!(MemoriaClient::released_snapshot(sid_b).is_empty());

        MemoriaClient::reset_released(sid_a);
    }

    #[test]
    fn release_empty_session_or_id_is_noop() {
        use super::*;
        MemoriaClient::release_context("", "c1");
        assert!(MemoriaClient::released_snapshot("").is_empty());

        let sid = "t-release-empty-tcid";
        MemoriaClient::reset_released(sid);
        MemoriaClient::release_context(sid, "");
        assert!(MemoriaClient::released_snapshot(sid).is_empty());
        MemoriaClient::reset_released(sid);
    }

    #[test]
    fn reset_released_clears_all() {
        use super::*;
        let sid = "t-release-reset";
        MemoriaClient::release_context(sid, "c1");
        MemoriaClient::release_context(sid, "c2");
        MemoriaClient::reset_released(sid);
        assert!(MemoriaClient::released_snapshot(sid).is_empty());
    }

    #[test]
    fn reset_session_process_state_clears_suppressed_and_released() {
        use super::*;
        let sid = "t-session-process-reset";
        MemoriaClient::suppress_memory(sid, "m1");
        MemoriaClient::release_context(sid, "c1");

        MemoriaClient::reset_session_process_state(sid);

        assert!(MemoriaClient::suppressed_snapshot(sid).is_empty());
        assert!(MemoriaClient::released_snapshot(sid).is_empty());
    }
}
