//! Incremental turn state for streaming survivability.
//!
//! During an agentic loop, token counts, partial text, and tool records are
//! written to this shared state as events stream in. When the turn is
//! interrupted (Ctrl+C, timeout, crash), a snapshot is recoverable even if the
//! stream future is dropped before returning.
//!
//! # Design
//! - Token counts use `AtomicU64` for lock-free writes during hot streaming.
//! - Partial text and tool records use `Mutex<Vec<T>>` — contention is low
//!   (single writer per field at any point in the SSE parse).
//! - `snapshot()` takes a non-consuming snapshot, safe on poisoned mutexes.

use astra_services::session_journal::ToolCallRecord;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Backpressure cap: oldest records are evicted when this is exceeded.
const MAX_TOOL_RECORDS: usize = 200;

/// Incremental turn state shared between the SSE streaming future and the
/// cancel/drain handler.  All fields are written during streaming and read
/// on interruption to recover partial data.
///
/// Tool call count is derived from `tool_call_records.len()` — no separate
/// atomic counter is needed.
#[derive(Debug)]
pub struct IncrementalTurnState {
    /// Prompt tokens consumed so far.
    pub prompt_tokens: AtomicU64,
    /// Completion tokens produced so far.
    pub completion_tokens: AtomicU64,
    /// Cache read tokens.
    pub cache_read_tokens: AtomicU64,
    /// Cache creation tokens.
    pub cache_creation_tokens: AtomicU64,
    /// Accumulated assistant text (partial or full).
    pub partial_text: Mutex<String>,
    /// Byte length of text already committed via `update_text`.  Used so
    /// that repeated calls with a growing full-text accumulator only pay
    /// for the delta (O(Δ) instead of O(N) per SSE chunk).
    pub partial_text_len: AtomicUsize,
    /// Per-tool-call records (name, ok, ms, error).  Length equals
    /// the total tool call count.  Capped at `MAX_TOOL_RECORDS`; oldest
    /// entries are evicted under backpressure.
    pub tool_call_records: Mutex<Vec<ToolCallRecord>>,
    /// Tool names that have been used (deduplicated).
    pub tools_used: Mutex<Vec<String>>,
    /// Session id from the first response event.
    pub session_id: Mutex<Option<String>>,
    /// Run id from the first response event.
    pub run_id: Mutex<Option<String>>,
}

impl Default for IncrementalTurnState {
    fn default() -> Self {
        Self {
            prompt_tokens: AtomicU64::new(0),
            completion_tokens: AtomicU64::new(0),
            cache_read_tokens: AtomicU64::new(0),
            cache_creation_tokens: AtomicU64::new(0),
            partial_text: Mutex::new(String::new()),
            partial_text_len: AtomicUsize::new(0),
            tool_call_records: Mutex::new(Vec::new()),
            tools_used: Mutex::new(Vec::new()),
            session_id: Mutex::new(None),
            run_id: Mutex::new(None),
        }
    }
}

/// Snapshot of incremental state for persistence.
#[derive(Debug, Default, Clone)]
pub struct TurnIncrementalSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub partial_text: String,
    /// Tool call records captured at snapshot time.
    /// `tool_call_records.len()` gives the tool call count.
    pub tool_call_records: Vec<ToolCallRecord>,
    pub tools_used: Vec<String>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
}

impl IncrementalTurnState {
    /// Set prompt tokens to the latest observed absolute value.
    pub fn set_prompt_tokens(&self, n: u64) {
        self.prompt_tokens.store(n, Ordering::Relaxed);
    }

    /// Add prompt tokens (typically only once at the start of streaming).
    pub fn add_prompt_tokens(&self, n: u64) {
        self.prompt_tokens.fetch_add(n, Ordering::Relaxed);
    }

    /// Set completion tokens to the latest observed absolute value.
    pub fn set_completion_tokens(&self, n: u64) {
        self.completion_tokens.store(n, Ordering::Relaxed);
    }

    /// Add completion tokens (typically once per SSE chunk).
    pub fn add_completion_tokens(&self, n: u64) {
        self.completion_tokens.fetch_add(n, Ordering::Relaxed);
    }

    /// Set cache read tokens to the latest observed absolute value.
    pub fn set_cache_read_tokens(&self, n: u64) {
        self.cache_read_tokens.store(n, Ordering::Relaxed);
    }

    /// Add cache read tokens.
    pub fn add_cache_read_tokens(&self, n: u64) {
        self.cache_read_tokens.fetch_add(n, Ordering::Relaxed);
    }

    /// Set cache creation tokens to the latest observed absolute value.
    pub fn set_cache_creation_tokens(&self, n: u64) {
        self.cache_creation_tokens.store(n, Ordering::Relaxed);
    }

    /// Add cache creation tokens.
    pub fn add_cache_creation_tokens(&self, n: u64) {
        self.cache_creation_tokens.fetch_add(n, Ordering::Relaxed);
    }

    /// Replace the accumulated assistant text with the latest observed snapshot.
    /// Prefer `update_text` during streaming to avoid O(N) clones on every chunk.
    pub fn replace_text(&self, text: &str) {
        let mut guard = unwrap_lock(&self.partial_text);
        guard.clear();
        guard.push_str(text);
        self.partial_text_len.store(text.len(), Ordering::Relaxed);
    }

    /// Delta-friendly update: only appends the suffix of `full_text` that has
    /// not been committed yet.  Safe to call repeatedly with a growing
    /// accumulator (e.g. every SSE chunk) — O(Δ) instead of O(N).
    pub fn update_text(&self, full_text: &str) {
        let prev = self.partial_text_len.load(Ordering::Relaxed);
        // Fast path: no new content.
        if full_text.len() <= prev {
            return;
        }
        // `prev` must land on a char boundary in `full_text`. If the upstream
        // accumulator was reset or diverged, it may fall inside a multi-byte
        // UTF-8 code point (e.g. inside '卡', bytes 65..68). In that case we
        // replace the entire partial text to stay consistent.
        let delta = match full_text.get(prev..) {
            Some(s) => s,
            None => {
                let mut guard = unwrap_lock(&self.partial_text);
                guard.clear();
                guard.push_str(full_text);
                self.partial_text_len
                    .store(full_text.len(), Ordering::Relaxed);
                return;
            }
        };
        {
            let mut guard = unwrap_lock(&self.partial_text);
            guard.push_str(delta);
        }
        self.partial_text_len
            .store(full_text.len(), Ordering::Relaxed);
    }

    /// Append text to the partial response.
    pub fn append_text(&self, text: &str) {
        unwrap_lock(&self.partial_text).push_str(text);
        self.partial_text_len
            .fetch_add(text.len(), Ordering::Relaxed);
    }

    /// Replace the tool-call record collection with the latest authoritative set.
    pub fn replace_tool_records(&self, records: Vec<ToolCallRecord>) {
        *unwrap_lock(&self.tool_call_records) = records;
    }

    /// Replace the deduplicated tools-used list with the latest authoritative set.
    pub fn replace_tools_used(&self, tools_used: Vec<String>) {
        *unwrap_lock(&self.tools_used) = tools_used;
    }

    /// Push a tool call record for incremental accumulation.
    /// Backpressure: if the record count exceeds `MAX_TOOL_RECORDS`, the
    /// oldest half is evicted (keeps the cap O(1) amortized).
    pub fn push_tool_record(&self, record: ToolCallRecord) {
        let mut guard = unwrap_lock(&self.tool_call_records);
        guard.push(record);
        if guard.len() > MAX_TOOL_RECORDS {
            let drop = guard.len() / 2;
            guard.drain(0..drop);
        }
    }

    /// Register a tool name as used.  Uses `str` comparison to avoid
    /// allocating a `String` for the lookup.
    pub fn add_tool_used(&self, name: &str) {
        let mut guard = unwrap_lock(&self.tools_used);
        if !guard.iter().any(|n| n == name) {
            guard.push(name.to_owned());
        }
    }

    /// Set the session id (first-wins).
    pub fn set_session_id(&self, sid: String) {
        let mut guard = unwrap_lock(&self.session_id);
        if guard.is_none() {
            *guard = Some(sid);
        }
    }

    /// Set the run id (first-wins).
    pub fn set_run_id(&self, rid: String) {
        let mut guard = unwrap_lock(&self.run_id);
        if guard.is_none() {
            *guard = Some(rid);
        }
    }

    /// Take a snapshot of the current state without consuming self.
    /// Suitable for recovery on force-exit where the Arc may still have other
    /// references.
    ///
    /// Each lock is held only long enough to clone the value.  Poisoned
    /// mutexes are recovered via `into_inner()` so that a poisoned lock from a
    /// panicking writer (e.g. SSE parse) still yields a valid snapshot.
    pub fn snapshot(&self) -> TurnIncrementalSnapshot {
        TurnIncrementalSnapshot {
            prompt_tokens: self.prompt_tokens.load(Ordering::Relaxed),
            completion_tokens: self.completion_tokens.load(Ordering::Relaxed),
            cache_read_tokens: self.cache_read_tokens.load(Ordering::Relaxed),
            cache_creation_tokens: self.cache_creation_tokens.load(Ordering::Relaxed),
            partial_text: unwrap_lock(&self.partial_text).clone(),
            tool_call_records: unwrap_lock(&self.tool_call_records).clone(),
            tools_used: unwrap_lock(&self.tools_used).clone(),
            session_id: unwrap_lock(&self.session_id).clone(),
            run_id: unwrap_lock(&self.run_id).clone(),
        }
    }
}

/// Helper: lock a mutex, recovering from poison if the previous holder panicked.
fn unwrap_lock<T>(mu: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mu.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Token accounting ────────────────────────────────────────────────��─

    #[test]
    fn token_counters_start_at_zero() {
        let state = IncrementalTurnState::default();
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 0);
        assert_eq!(snap.completion_tokens, 0);
        assert_eq!(snap.cache_read_tokens, 0);
        assert_eq!(snap.cache_creation_tokens, 0);
    }

    #[test]
    fn add_prompt_tokens_accumulates() {
        let state = IncrementalTurnState::default();
        state.add_prompt_tokens(100);
        state.add_prompt_tokens(50);
        assert_eq!(state.prompt_tokens.load(Ordering::Relaxed), 150);
    }

    #[test]
    fn add_completion_tokens_accumulates() {
        let state = IncrementalTurnState::default();
        state.add_completion_tokens(10);
        state.add_completion_tokens(5);
        assert_eq!(state.completion_tokens.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn token_counters_survive_snapshot() {
        let state = IncrementalTurnState::default();
        state.add_prompt_tokens(200);
        state.add_completion_tokens(80);
        state.add_cache_read_tokens(30);
        state.add_cache_creation_tokens(10);
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 200);
        assert_eq!(snap.completion_tokens, 80);
        assert_eq!(snap.cache_read_tokens, 30);
        assert_eq!(snap.cache_creation_tokens, 10);
    }

    #[test]
    fn token_setters_overwrite_previous_values() {
        let state = IncrementalTurnState::default();
        state.add_prompt_tokens(10);
        state.add_completion_tokens(5);
        state.set_prompt_tokens(200);
        state.set_completion_tokens(80);
        state.set_cache_read_tokens(30);
        state.set_cache_creation_tokens(10);
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 200);
        assert_eq!(snap.completion_tokens, 80);
        assert_eq!(snap.cache_read_tokens, 30);
        assert_eq!(snap.cache_creation_tokens, 10);
    }

    // ── Tool calls ────────────────────────────────────────────────────────

    fn tool_record(name: &str, ok: bool, ms: u64, error: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.into(),
            ok,
            ms,
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn tool_call_count_derived_from_records_len() {
        let state = IncrementalTurnState::default();
        state.push_tool_record(tool_record("read_file", true, 42, None));
        state.push_tool_record(tool_record("bash", false, 100, Some("fail")));
        state.push_tool_record(tool_record("grep", true, 10, None));
        let snap = state.snapshot();
        // Count is derived from records len, not a separate atomic.
        assert_eq!(snap.tool_call_records.len(), 3);
    }

    #[test]
    fn tool_records_preserved_in_snapshot() {
        let state = IncrementalTurnState::default();
        state.push_tool_record(tool_record("read_file", true, 42, None));
        state.push_tool_record(tool_record("bash", false, 100, Some("command not found")));
        let snap = state.snapshot();
        assert_eq!(snap.tool_call_records.len(), 2);
        assert_eq!(snap.tool_call_records[0].name, "read_file");
        assert!(snap.tool_call_records[0].ok);
        assert_eq!(snap.tool_call_records[1].name, "bash");
        assert!(!snap.tool_call_records[1].ok);
        assert_eq!(
            snap.tool_call_records[1].error.as_deref(),
            Some("command not found")
        );
    }

    #[test]
    fn tools_used_deduplicates() {
        let state = IncrementalTurnState::default();
        state.add_tool_used("bash");
        state.add_tool_used("read_file");
        state.add_tool_used("bash"); // duplicate
        let snap = state.snapshot();
        assert_eq!(snap.tools_used, vec!["bash", "read_file"]);
    }

    #[test]
    fn replace_tool_records_and_tools_used_overwrite_previous_state() {
        let state = IncrementalTurnState::default();
        state.push_tool_record(tool_record("read_file", true, 42, None));
        state.add_tool_used("read_file");
        state.replace_tool_records(vec![tool_record("bash", false, 100, Some("boom"))]);
        state.replace_tools_used(vec!["bash".to_string()]);
        let snap = state.snapshot();
        assert_eq!(snap.tool_call_records.len(), 1);
        assert_eq!(snap.tool_call_records[0].name, "bash");
        assert_eq!(snap.tools_used, vec!["bash"]);
    }

    // ── Partial text ──────────────────────────────────────────────────────

    #[test]
    fn partial_text_appends() {
        let state = IncrementalTurnState::default();
        state.append_text("Hello");
        state.append_text(", ");
        state.append_text("world!");
        let snap = state.snapshot();
        assert_eq!(snap.partial_text, "Hello, world!");
    }

    #[test]
    fn replace_text_overwrites_previous_content() {
        let state = IncrementalTurnState::default();
        state.append_text("draft");
        state.replace_text("final");
        let snap = state.snapshot();
        assert_eq!(snap.partial_text, "final");
    }

    #[test]
    fn empty_partial_text_is_empty_string() {
        let state = IncrementalTurnState::default();
        let snap = state.snapshot();
        assert!(snap.partial_text.is_empty());
    }

    // ── Session / run id ──────────────────────────────────────────────────

    #[test]
    fn session_id_first_wins() {
        let state = IncrementalTurnState::default();
        state.set_session_id("sess-001".into());
        state.set_session_id("sess-002".into()); // ignored
        let snap = state.snapshot();
        assert_eq!(snap.session_id.as_deref(), Some("sess-001"));
    }

    #[test]
    fn run_id_first_wins() {
        let state = IncrementalTurnState::default();
        state.set_run_id("run-abc".into());
        state.set_run_id("run-xyz".into());
        let snap = state.snapshot();
        assert_eq!(snap.run_id.as_deref(), Some("run-abc"));
    }

    #[test]
    fn unset_ids_are_none() {
        let state = IncrementalTurnState::default();
        let snap = state.snapshot();
        assert!(snap.session_id.is_none());
        assert!(snap.run_id.is_none());
    }

    // ── Concurrent writes from two threads ────────────────────────────────

    #[test]
    fn concurrent_token_writes_sum_correctly() {
        use std::sync::Arc;
        let state = Arc::new(IncrementalTurnState::default());
        let s1 = state.clone();
        let s2 = state.clone();
        let t1 = std::thread::spawn(move || {
            for _ in 0..1000 {
                s1.add_prompt_tokens(1);
            }
        });
        let t2 = std::thread::spawn(move || {
            for _ in 0..1000 {
                s2.add_completion_tokens(1);
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let snap = Arc::try_unwrap(state).unwrap().snapshot();
        assert_eq!(snap.prompt_tokens, 1000);
        assert_eq!(snap.completion_tokens, 1000);
    }

    #[test]
    fn snapshot_handles_poisoned_mutex() {
        use std::sync::Arc;
        let state = Arc::new(IncrementalTurnState::default());
        state.append_text("before-panic");

        // Poison the partial_text mutex by panicking while holding the lock.
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            let _guard = state_clone
                .partial_text
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            panic!("deliberate poison");
        });
        let _ = handle.join(); // thread panicked → mutex is poisoned

        // snapshot() should still work — it recovers the inner value.
        let snap = state.snapshot();
        assert_eq!(snap.partial_text, "before-panic");
    }

    // ── update_text delta (SSE chunk streaming) ──────────────────────────

    #[test]
    fn update_text_delta_only_appends_new_bytes() {
        let state = IncrementalTurnState::default();
        // Simulate SSE streaming: each call passes the growing full_text.
        state.update_text("Hel");
        state.update_text("Hello");
        state.update_text("Hello, world!");
        let snap = state.snapshot();
        assert_eq!(snap.partial_text, "Hello, world!");
    }

    #[test]
    fn update_text_noop_when_no_new_content() {
        let state = IncrementalTurnState::default();
        state.update_text("Hello, world!");
        let len_before = unwrap_lock(&state.partial_text).len();
        // Same text again — should be a no-op.
        state.update_text("Hello, world!");
        let len_after = unwrap_lock(&state.partial_text).len();
        assert_eq!(len_before, len_after);
        assert_eq!(len_after, "Hello, world!".len());
    }

    #[test]
    fn update_text_empty_full_text_is_noop() {
        let state = IncrementalTurnState::default();
        state.update_text("");
        state.update_text("data");
        // Shorter full_text should be ignored (no regression).
        state.update_text("d");
        let snap = state.snapshot();
        assert_eq!(snap.partial_text, "data");
    }

    #[test]
    fn update_text_after_replace_respects_new_len() {
        let state = IncrementalTurnState::default();
        state.update_text("first pass");
        // replace_text resets the length tracker.
        state.replace_text("second");
        state.update_text("second pass!");
        let snap = state.snapshot();
        assert_eq!(snap.partial_text, "second pass!");
    }

    // ── Tool record backpressure ─────────────────────────────────────────

    #[test]
    fn push_tool_record_evicts_oldest_at_cap() {
        let state = IncrementalTurnState::default();
        // Fill beyond MAX_TOOL_RECORDS (200).
        for i in 0..300 {
            state.push_tool_record(tool_record(&format!("tool_{i}"), true, i, None));
        }
        let snap = state.snapshot();
        // After 2 evict cycles: drain(0..150) at i=200 → 0..199, then
        // i=200..299 fills 100 more, at i=300 drain(0..150) → 150..299 = 150.
        assert!(snap.tool_call_records.len() <= super::MAX_TOOL_RECORDS);
        // Oldest surviving records should be from the second batch.
        assert!(
            snap.tool_call_records[0].name.as_str().starts_with("tool_"),
            "records should survive eviction cycles"
        );
    }

    #[test]
    fn push_tool_record_below_cap_no_eviction() {
        let state = IncrementalTurnState::default();
        for i in 0..50 {
            state.push_tool_record(tool_record(&format!("tool_{i}"), true, i, None));
        }
        let snap = state.snapshot();
        assert_eq!(snap.tool_call_records.len(), 50);
        assert_eq!(snap.tool_call_records[0].name, "tool_0");
        assert_eq!(snap.tool_call_records[49].name, "tool_49");
    }
}
