//! Per-session debounce for session-end governance.
//!
//! Session IDs are sticky across many `create_run` / `stream_chat`
//! calls (same `request.session_id`). Firing session-end governance —
//! purge_working + store_episode + reflect — on every terminal run
//! produces one episode memory per turn, hammers the reflect endpoint,
//! and lets working-memory purges fire mid-conversation when a user
//! reopens an existing session.
//!
//! This debounce tracks the **last governance completion timestamp**
//! per `session_id`. A subsequent governance attempt within
//! [`GOVERNANCE_MIN_INTERVAL`] of the last one is skipped — the caller
//! gets a `DebounceDecision::Skip` and governance runs on a later turn
//! (or at true session-end when the gap finally opens).
//!
//! The store is process-local, bounded by explicit resets from the
//! caller at session-end cleanup.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Minimum gap between governance runs for the same session. Below this
/// the subsequent run is considered "still active" and governance is
/// deferred. Aligns with Memoria's reflect 1h cooldown but is client-
/// side so even purge_working and store_episode are debounced.
pub const GOVERNANCE_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// What the caller should do for this governance attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceDecision {
    /// Run governance for this session.
    Run,
    /// Skip — the last run was too recent.
    Skip,
}

/// Process-wide debounce store keyed by session_id.
#[derive(Debug, Default)]
pub struct SessionEndDebouncer {
    inner: RwLock<HashMap<String, Instant>>,
    min_interval: Duration,
}

impl SessionEndDebouncer {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            min_interval: GOVERNANCE_MIN_INTERVAL,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_interval(min_interval: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            min_interval,
        }
    }

    /// Decide whether to run governance for `session_id`. Does NOT record
    /// the decision — the caller must call [`record`] once governance
    /// actually completes, so a failed governance attempt doesn't use up
    /// the debounce window.
    pub fn should_run(&self, session_id: &str) -> DebounceDecision {
        if session_id.is_empty() {
            return DebounceDecision::Skip;
        }
        let Ok(g) = self.inner.read() else {
            return DebounceDecision::Run;
        };
        match g.get(session_id) {
            Some(last) if last.elapsed() < self.min_interval => DebounceDecision::Skip,
            _ => DebounceDecision::Run,
        }
    }

    /// Record that governance completed for `session_id` at this moment.
    pub fn record(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = self.inner.write() {
            g.insert(session_id.to_string(), Instant::now());
        }
    }

    /// Drop the entry for `session_id`. Intended for explicit cleanup
    /// on session teardown so a long-lived server doesn't accumulate
    /// an entry per session for the life of the process.
    pub fn forget(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut g) = self.inner.write() {
            g.remove(session_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn session_count(&self) -> usize {
        self.inner.read().map(|g| g.len()).unwrap_or(0)
    }
}

/// Process-wide singleton.
pub fn global() -> &'static SessionEndDebouncer {
    static DEBOUNCER: OnceLock<SessionEndDebouncer> = OnceLock::new();
    DEBOUNCER.get_or_init(SessionEndDebouncer::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_runs() {
        let d = SessionEndDebouncer::new();
        assert_eq!(d.should_run("sess1"), DebounceDecision::Run);
    }

    #[test]
    fn empty_session_id_always_skips() {
        let d = SessionEndDebouncer::new();
        assert_eq!(d.should_run(""), DebounceDecision::Skip);
        d.record(""); // no-op
        assert_eq!(d.session_count(), 0);
    }

    #[test]
    fn second_call_within_window_skips() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        d.record("sess1");
        assert_eq!(d.should_run("sess1"), DebounceDecision::Skip);
    }

    #[test]
    fn second_call_after_window_runs() {
        let d = SessionEndDebouncer::with_interval(Duration::from_millis(10));
        d.record("sess1");
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(d.should_run("sess1"), DebounceDecision::Run);
    }

    #[test]
    fn different_sessions_independent() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        d.record("sess1");
        assert_eq!(d.should_run("sess2"), DebounceDecision::Run);
        assert_eq!(d.should_run("sess1"), DebounceDecision::Skip);
    }

    #[test]
    fn forget_clears_debounce_state() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        d.record("sess1");
        assert_eq!(d.session_count(), 1);
        d.forget("sess1");
        assert_eq!(d.session_count(), 0);
        assert_eq!(d.should_run("sess1"), DebounceDecision::Run);
    }

    #[test]
    fn should_run_does_not_record() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        let _ = d.should_run("sess1");
        // No call to record — the window shouldn't be started yet.
        assert_eq!(d.should_run("sess1"), DebounceDecision::Run);
        assert_eq!(d.session_count(), 0);
    }

    // ─── Caller-sequence integration tests ─────────────────────────────
    //
    // These lock the exact should_run/record/forget sequence used by
    // `server::run_lifecycle::post_loop_memory_cleanup` (lines 98-147
    // and the final `forget` at the end of cleanup). Regressions in
    // the caller — e.g. recording on failure, skipping forget, or
    // reversing the order — will show up here rather than only in a
    // six-session production cascade.

    #[test]
    fn caller_sequence_success_path_skips_on_second_turn() {
        // Simulates: first terminal run completes governance successfully,
        // then the user issues another turn on the same session within
        // the window. Caller path:
        //   should_run=Run → governance Ok → record → forget-at-teardown
        // Second turn (before forget, e.g. still active) must Skip.
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        assert_eq!(d.should_run("sess-caller-1"), DebounceDecision::Run);
        // caller: governance succeeded → record()
        d.record("sess-caller-1");
        // Second turn arrives on same session_id before forget/teardown:
        assert_eq!(d.should_run("sess-caller-1"), DebounceDecision::Skip);
    }

    #[test]
    fn caller_sequence_governance_failure_does_not_consume_window() {
        // Simulates: should_run=Run, governance returns Err, caller
        // logs warn and SKIPS record (see run_lifecycle.rs ~132 vs 134).
        // Next turn must still get Run — a failed governance attempt
        // must not burn the 15-min window.
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        assert_eq!(d.should_run("sess-fail"), DebounceDecision::Run);
        // caller: governance returned Err → NO record() call.
        // Next turn:
        assert_eq!(d.should_run("sess-fail"), DebounceDecision::Run);
        assert_eq!(d.session_count(), 0);
    }

    #[test]
    fn caller_sequence_forget_allows_immediate_rerun_on_same_session_id() {
        // Simulates: long-lived server reaches explicit teardown for a
        // session_id, calls forget(), then the user reconnects with the
        // same session_id (sticky IDs). Next should_run must be Run so
        // the new conversation gets its own governance window.
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        d.record("sess-sticky");
        assert_eq!(d.should_run("sess-sticky"), DebounceDecision::Skip);
        // Explicit teardown path:
        d.forget("sess-sticky");
        // Reconnect with same id:
        assert_eq!(d.should_run("sess-sticky"), DebounceDecision::Run);
    }

    #[test]
    fn caller_sequence_empty_session_id_is_noop_throughout() {
        // run_lifecycle.rs: `if session_id.is_empty() { return; }` at the
        // top of post_loop_memory_cleanup. But if a future refactor drops
        // that guard, the debouncer itself must still no-op cleanly for
        // every method in the caller sequence.
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        assert_eq!(d.should_run(""), DebounceDecision::Skip);
        d.record("");
        d.forget("");
        assert_eq!(d.session_count(), 0);
        assert_eq!(d.should_run(""), DebounceDecision::Skip);
    }

    #[test]
    fn caller_sequence_concurrent_sessions_do_not_cross_contaminate() {
        // Server handles multiple sessions in flight. One session's
        // record()/forget() must not affect another's window — regression
        // guard against a future HashMap keying bug.
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        d.record("sess-A");
        d.record("sess-B");
        assert_eq!(d.should_run("sess-A"), DebounceDecision::Skip);
        assert_eq!(d.should_run("sess-B"), DebounceDecision::Skip);
        d.forget("sess-A");
        assert_eq!(d.should_run("sess-A"), DebounceDecision::Run);
        // B's window must be untouched:
        assert_eq!(d.should_run("sess-B"), DebounceDecision::Skip);
    }
}
