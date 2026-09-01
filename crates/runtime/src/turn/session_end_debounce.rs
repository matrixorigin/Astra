//! Per-session debounce for session-end governance.
//!
//! Session IDs are sticky across many `create_run` / `stream_chat`
//! calls (same `request.session_id`). Firing session-end governance —
//! purge_working + store_episode + reflect — on every terminal run
//! produces one episode memory per turn, hammers the reflect endpoint,
//! and lets working-memory purges fire mid-conversation when a user
//! reopens an existing session.
//!
//! This coordinator atomically grants one governance permit per session.
//! Concurrent terminal runs cannot both purge/store/reflect, and dropping a
//! permit before completion releases ownership so cancellation and failures do
//! not strand the session. Successful completion starts a cooldown.
//!
//! The store is process-local, lazily prunes expired completion records, and
//! caps retained completed sessions without evicting active owners.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Minimum gap between governance runs for the same session. Below this
/// the subsequent run is considered "still active" and governance is
/// deferred. Aligns with Memoria's reflect 1h cooldown but is client-
/// side so even purge_working and store_episode are debounced.
pub const GOVERNANCE_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_COMPLETED_SESSIONS: usize = 16 * 1024;

#[derive(Debug, Clone, Copy)]
enum SessionGovernanceState {
    InFlight { generation: u64 },
    Completed { at: Instant },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionEndKey {
    owner_id: String,
    session_id: String,
}

#[derive(Debug, Default)]
struct SessionEndDebouncerInner {
    sessions: HashMap<SessionEndKey, SessionGovernanceState>,
    next_generation: u64,
}

/// Process-wide single-owner/cooldown store keyed by session id.
#[derive(Debug, Default)]
pub struct SessionEndDebouncer {
    inner: Mutex<SessionEndDebouncerInner>,
    min_interval: Duration,
    max_completed_sessions: usize,
}

/// Exclusive right to run session-end governance for one session.
///
/// A permit that is dropped without [`complete`](Self::complete) releases the
/// in-flight state. This makes task cancellation, panic, and ordinary failures
/// retryable without a separate error-path cleanup call.
#[derive(Debug)]
pub struct SessionEndPermit<'a> {
    owner: &'a SessionEndDebouncer,
    key: SessionEndKey,
    generation: u64,
    completed: bool,
}

impl SessionEndDebouncer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SessionEndDebouncerInner::default()),
            min_interval: GOVERNANCE_MIN_INTERVAL,
            max_completed_sessions: MAX_COMPLETED_SESSIONS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_interval(min_interval: Duration) -> Self {
        Self {
            inner: Mutex::new(SessionEndDebouncerInner::default()),
            min_interval,
            max_completed_sessions: MAX_COMPLETED_SESSIONS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(min_interval: Duration, max_completed_sessions: usize) -> Self {
        Self {
            inner: Mutex::new(SessionEndDebouncerInner::default()),
            min_interval,
            max_completed_sessions,
        }
    }

    /// Atomically acquire governance ownership for `session_id`.
    ///
    /// Returns `None` while another owner is active or a successful run is
    /// still inside the cooldown. Poisoned coordination state fails closed:
    /// duplicate destructive cleanup is riskier than deferring best-effort
    /// memory maintenance to a later process lifetime.
    pub fn try_begin(&self, owner_id: &str, session_id: &str) -> Option<SessionEndPermit<'_>> {
        if owner_id.is_empty() || session_id.is_empty() {
            return None;
        }
        let key = SessionEndKey {
            owner_id: owner_id.to_string(),
            session_id: session_id.to_string(),
        };
        let now = Instant::now();
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        inner.sessions.retain(|_, state| match state {
            SessionGovernanceState::InFlight { .. } => true,
            SessionGovernanceState::Completed { at } => now.duration_since(*at) < self.min_interval,
        });
        if inner.sessions.contains_key(&key) {
            return None;
        }
        inner.next_generation = inner.next_generation.wrapping_add(1).max(1);
        let generation = inner.next_generation;
        inner
            .sessions
            .insert(key.clone(), SessionGovernanceState::InFlight { generation });
        Some(SessionEndPermit {
            owner: self,
            key,
            generation,
            completed: false,
        })
    }

    /// Drop the entry for `session_id`. Intended for explicit cleanup
    /// on definitive session deletion or process-local reset.
    pub fn forget(&self, owner_id: &str, session_id: &str) {
        if owner_id.is_empty() || session_id.is_empty() {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.sessions.remove(&SessionEndKey {
                owner_id: owner_id.to_string(),
                session_id: session_id.to_string(),
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn session_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.sessions.len())
            .unwrap_or(0)
    }
}

impl SessionEndPermit<'_> {
    /// Commit a successful governance run and start the session cooldown.
    pub fn complete(mut self) {
        if let Ok(mut inner) = self.owner.inner.lock()
            && matches!(
                inner.sessions.get(&self.key),
                Some(SessionGovernanceState::InFlight { generation })
                    if *generation == self.generation
            )
        {
            inner.sessions.insert(
                self.key.clone(),
                SessionGovernanceState::Completed { at: Instant::now() },
            );
            while inner
                .sessions
                .values()
                .filter(|state| matches!(state, SessionGovernanceState::Completed { .. }))
                .count()
                > self.owner.max_completed_sessions
            {
                let oldest = inner
                    .sessions
                    .iter()
                    .filter_map(|(key, state)| match state {
                        SessionGovernanceState::Completed { at } => Some((key.clone(), *at)),
                        SessionGovernanceState::InFlight { .. } => None,
                    })
                    .min_by_key(|(_, at)| *at)
                    .map(|(key, _)| key);
                let Some(oldest) = oldest else {
                    break;
                };
                inner.sessions.remove(&oldest);
            }
        }
        self.completed = true;
    }
}

impl Drop for SessionEndPermit<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Ok(mut inner) = self.owner.inner.lock()
            && matches!(
                inner.sessions.get(&self.key),
                Some(SessionGovernanceState::InFlight { generation })
                    if *generation == self.generation
            )
        {
            inner.sessions.remove(&self.key);
        }
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
    fn first_caller_gets_exclusive_permit() {
        let d = SessionEndDebouncer::new();
        let first = d
            .try_begin("owner", "sess1")
            .expect("first caller owns cleanup");
        assert!(d.try_begin("owner", "sess1").is_none());
        drop(first);
    }

    #[test]
    fn empty_session_id_always_skips() {
        let d = SessionEndDebouncer::new();
        assert!(d.try_begin("owner", "").is_none());
        assert!(d.try_begin("", "session").is_none());
        d.forget("owner", "");
        assert_eq!(d.session_count(), 0);
    }

    #[test]
    fn successful_completion_starts_cooldown() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        d.try_begin("owner", "sess1").unwrap().complete();
        assert!(d.try_begin("owner", "sess1").is_none());
        assert_eq!(d.session_count(), 1);
    }

    #[test]
    fn expired_completion_is_pruned_and_can_run_again() {
        let d = SessionEndDebouncer::with_interval(Duration::from_millis(10));
        d.try_begin("owner", "sess1").unwrap().complete();
        std::thread::sleep(Duration::from_millis(25));
        assert!(d.try_begin("owner", "sess1").is_some());
    }

    #[test]
    fn dropped_or_failed_owner_releases_session_immediately() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        let permit = d.try_begin("owner", "sess-fail").unwrap();
        assert!(d.try_begin("owner", "sess-fail").is_none());
        drop(permit);
        assert!(d.try_begin("owner", "sess-fail").is_some());
    }

    #[test]
    fn different_sessions_have_independent_owners() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        let first = d.try_begin("owner", "sess1").unwrap();
        let second = d.try_begin("owner", "sess2").unwrap();
        assert_eq!(d.session_count(), 2);
        drop((first, second));
    }

    #[test]
    fn forget_invalidates_old_permit_without_clearing_new_owner() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        let stale = d.try_begin("owner", "sess-sticky").unwrap();
        d.forget("owner", "sess-sticky");
        let current = d.try_begin("owner", "sess-sticky").unwrap();
        drop(stale);
        assert!(
            d.try_begin("owner", "sess-sticky").is_none(),
            "an old guard must not release a newer generation"
        );
        current.complete();
    }

    #[test]
    fn successful_sessions_are_lazily_reclaimed_under_continued_traffic() {
        let d = SessionEndDebouncer::with_interval(Duration::from_millis(5));
        d.try_begin("owner", "expired").unwrap().complete();
        std::thread::sleep(Duration::from_millis(10));
        let live = d.try_begin("owner", "live").unwrap();
        assert_eq!(d.session_count(), 1);
        drop(live);
    }

    #[test]
    fn identical_session_ids_are_isolated_by_authenticated_owner() {
        let d = SessionEndDebouncer::with_interval(Duration::from_secs(60));
        let owner_a = d.try_begin("owner-a", "shared-session").unwrap();
        let owner_b = d.try_begin("owner-b", "shared-session").unwrap();
        assert_eq!(d.session_count(), 2);
        drop((owner_a, owner_b));
    }

    #[test]
    fn completed_session_cache_is_hard_bounded() {
        let d = SessionEndDebouncer::with_limits(Duration::from_secs(60), 2);
        d.try_begin("owner", "session-1").unwrap().complete();
        std::thread::sleep(Duration::from_millis(1));
        d.try_begin("owner", "session-2").unwrap().complete();
        std::thread::sleep(Duration::from_millis(1));
        d.try_begin("owner", "session-3").unwrap().complete();

        assert_eq!(d.session_count(), 2);
        assert!(
            d.try_begin("owner", "session-1").is_some(),
            "the oldest completed cooldown should be evicted first"
        );
        assert!(d.try_begin("owner", "session-2").is_none());
        assert!(d.try_begin("owner", "session-3").is_none());
    }
}
