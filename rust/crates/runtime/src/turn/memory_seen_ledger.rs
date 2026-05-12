//! Session-scoped "already surfaced to the LLM" ledger for memory content.
//!
//! The prompt cache can hold the `<memory_index>` + `<session_memory>`
//! blocks across every turn of a session — so the same memory content
//! reaches the model on turn 1, turn 2, turn N. Re-injecting those
//! same entries through the per-turn recall block wastes token budget
//! and can produce duplicated bullets on the rendered prompt.
//!
//! This ledger remembers the **dedup key** of every memory content the
//! bridge has already surfaced this session, keyed by `session_id`. The
//! per-turn path filters its recall entries against the ledger before
//! rendering, so a memory shown on turn 1 via `<session_memory>` won't
//! reappear as a `## User Memories` bullet on turn 2.
//!
//! The ledger is process-local state: it's held in a static `RwLock`
//! rather than a bridge field to avoid threading the state through
//! every construction site. Sessions are bounded — entries auto-evict
//! on [`SessionMemorySeenLedger::reset_session`], which the bridge
//! calls at session-end governance.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

/// A per-session set of dedup keys for memory contents that have
/// already been surfaced to the LLM in `<memory_index>` or
/// `<session_memory>` blocks.
#[derive(Debug, Default)]
pub struct SessionMemorySeenLedger {
    inner: RwLock<HashMap<String, HashSet<String>>>,
}

impl SessionMemorySeenLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record every content key as "surfaced for this session". No-op
    /// when `session_id` or `keys` is empty.
    pub fn record(&self, session_id: &str, keys: impl IntoIterator<Item = String>) {
        if session_id.is_empty() {
            return;
        }
        let Ok(mut g) = self.inner.write() else {
            return;
        };
        let bucket = g.entry(session_id.to_string()).or_default();
        for k in keys {
            if !k.is_empty() {
                bucket.insert(k);
            }
        }
    }

    /// Snapshot the current set for this session. Clones; callers can
    /// drop this in O(1) after filtering.
    pub fn snapshot(&self, session_id: &str) -> HashSet<String> {
        if session_id.is_empty() {
            return HashSet::new();
        }
        self.inner
            .read()
            .ok()
            .and_then(|g| g.get(session_id).cloned())
            .unwrap_or_default()
    }

    /// Clear all entries for this session. Call at session-end so the
    /// process-global ledger doesn't grow unbounded over a long-lived
    /// server lifetime.
    pub fn reset_session(&self, session_id: &str) {
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

/// Process-wide singleton. The bridge is constructed per HTTP handler
/// scope but memory surfacing state must outlive a single `forward()`
/// invocation — so one global ledger is the minimum viable wiring.
pub fn global() -> &'static SessionMemorySeenLedger {
    static LEDGER: OnceLock<SessionMemorySeenLedger> = OnceLock::new();
    LEDGER.get_or_init(SessionMemorySeenLedger::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_snapshot_returns_the_keys() {
        let ledger = SessionMemorySeenLedger::new();
        ledger.record("sess1", vec!["key-a".into(), "key-b".into()]);
        let snap = ledger.snapshot("sess1");
        assert!(snap.contains("key-a"));
        assert!(snap.contains("key-b"));
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn snapshot_unknown_session_is_empty() {
        let ledger = SessionMemorySeenLedger::new();
        assert!(ledger.snapshot("nonexistent").is_empty());
    }

    #[test]
    fn reset_session_clears_entries() {
        let ledger = SessionMemorySeenLedger::new();
        ledger.record("sess1", vec!["k".into()]);
        assert_eq!(ledger.session_count(), 1);
        ledger.reset_session("sess1");
        assert!(ledger.snapshot("sess1").is_empty());
        assert_eq!(ledger.session_count(), 0);
    }

    #[test]
    fn empty_session_id_is_noop() {
        let ledger = SessionMemorySeenLedger::new();
        ledger.record("", vec!["k".into()]);
        assert!(ledger.snapshot("").is_empty());
        assert_eq!(ledger.session_count(), 0);
    }

    #[test]
    fn record_empty_key_skipped() {
        let ledger = SessionMemorySeenLedger::new();
        ledger.record("sess1", vec!["".into(), "k".into(), "".into()]);
        let snap = ledger.snapshot("sess1");
        assert_eq!(snap.len(), 1, "empty strings must be skipped");
        assert!(snap.contains("k"));
    }

    #[test]
    fn record_merges_across_calls() {
        let ledger = SessionMemorySeenLedger::new();
        ledger.record("sess1", vec!["a".into()]);
        ledger.record("sess1", vec!["b".into()]);
        let snap = ledger.snapshot("sess1");
        assert_eq!(snap.len(), 2);
        assert!(snap.contains("a") && snap.contains("b"));
    }

    #[test]
    fn sessions_are_isolated() {
        let ledger = SessionMemorySeenLedger::new();
        ledger.record("sess1", vec!["a".into()]);
        ledger.record("sess2", vec!["b".into()]);
        assert!(ledger.snapshot("sess1").contains("a"));
        assert!(!ledger.snapshot("sess1").contains("b"));
        assert!(ledger.snapshot("sess2").contains("b"));
    }
}
