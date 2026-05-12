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
}
