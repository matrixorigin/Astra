//! Session-scoped structured feedback store.
//!
//! Stores `StructuredFeedback` rules extracted from user corrections and
//! injects matching rules into subsequent turn system prompts. This closes
//! the feedback loop: detect → extract → store → inject.
//!
//! Rules are isolated per session_id — no cross-session leakage.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use astra_turn_types::StructuredFeedback;

/// Maximum stored feedback rules per session.
const MAX_RULES_PER_SESSION: usize = 20;

/// Maximum tracked sessions before oldest is evicted.
const MAX_SESSIONS: usize = 200;

/// Session-scoped store for structured feedback rules.
///
/// Thread-safe via internal `Mutex`. Designed to be shared as `Arc<FeedbackStore>`
/// across the bridge singleton — rules are keyed by session_id.
pub struct FeedbackStore {
    inner: Mutex<StoreInner>,
}

struct StoreInner {
    sessions: HashMap<String, SessionRules>,
    /// Insertion-order tracking for LRU eviction of sessions.
    order: VecDeque<String>,
}

struct SessionRules {
    rules: Vec<StructuredFeedback>,
}

impl FeedbackStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StoreInner {
                sessions: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Store a feedback rule for a session. Deduplicates by rule text.
    pub fn add(&self, session_id: &str, feedback: StructuredFeedback) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // LRU eviction of oldest session if at capacity
        if !inner.sessions.contains_key(session_id) {
            if inner.order.len() >= MAX_SESSIONS {
                if let Some(oldest) = inner.order.pop_front() {
                    inner.sessions.remove(&oldest);
                }
            }
            inner.order.push_back(session_id.to_string());
        }

        let entry = inner
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionRules { rules: Vec::new() });

        if entry.rules.iter().any(|r| r.rule == feedback.rule) {
            return;
        }
        if entry.rules.len() >= MAX_RULES_PER_SESSION {
            entry.rules.remove(0);
        }
        entry.rules.push(feedback);
    }

    /// Number of stored rules for a session.
    pub fn len(&self, session_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(session_id)
            .map(|s| s.rules.len())
            .unwrap_or(0)
    }

    /// Whether a session has any rules.
    pub fn is_empty(&self, session_id: &str) -> bool {
        self.len(session_id) == 0
    }

    /// Build a context injection string for a specific session.
    pub fn build_injection(&self, session_id: &str) -> String {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = inner.sessions.get(session_id) else {
            return String::new();
        };
        if entry.rules.is_empty() {
            return String::new();
        }
        let mut lines = vec!["[Learned Feedback Rules]".to_string()];
        for fb in &entry.rules {
            let mut line = format!("- Rule: {}", fb.rule);
            if fb.reason != "Not stated" {
                line.push_str(&format!(" | Why: {}", fb.reason));
            }
            if fb.apply_when != "General" {
                line.push_str(&format!(" | When: {}", fb.apply_when));
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    /// Get a snapshot of rules for a session.
    pub fn rules(&self, session_id: &str) -> Vec<StructuredFeedback> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(session_id)
            .map(|s| s.rules.clone())
            .unwrap_or_default()
    }

    /// Number of tracked sessions.
    pub fn session_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .len()
    }

    /// Remove all rules for a session. Call on session close for explicit cleanup.
    pub fn clear_session(&self, session_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.sessions.remove(session_id);
        inner.order.retain(|s| s != session_id);
    }
}

impl Default for FeedbackStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fb(rule: &str) -> StructuredFeedback {
        StructuredFeedback {
            rule: rule.into(),
            reason: "Not stated".into(),
            apply_when: "General".into(),
            source_signal: "correction".into(),
            confidence: 0.9,
        }
    }

    fn make_fb_full(rule: &str, reason: &str, apply_when: &str) -> StructuredFeedback {
        StructuredFeedback {
            rule: rule.into(),
            reason: reason.into(),
            apply_when: apply_when.into(),
            source_signal: "correction".into(),
            confidence: 0.9,
        }
    }

    // ── Session isolation ──

    #[test]
    fn sessions_are_isolated() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A"));
        store.add("s2", make_fb("rule B"));

        assert_eq!(store.len("s1"), 1);
        assert_eq!(store.len("s2"), 1);
        assert!(store.build_injection("s1").contains("rule A"));
        assert!(!store.build_injection("s1").contains("rule B"));
        assert!(store.build_injection("s2").contains("rule B"));
        assert!(!store.build_injection("s2").contains("rule A"));
    }

    #[test]
    fn unknown_session_returns_empty() {
        let store = FeedbackStore::new();
        assert!(store.is_empty("nonexistent"));
        assert!(store.build_injection("nonexistent").is_empty());
        assert!(store.rules("nonexistent").is_empty());
    }

    // ── Dedup ──

    #[test]
    fn deduplicates_within_session() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A"));
        store.add("s1", make_fb("rule A"));
        assert_eq!(store.len("s1"), 1);
    }

    #[test]
    fn same_rule_different_sessions_not_deduped() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A"));
        store.add("s2", make_fb("rule A"));
        assert_eq!(store.len("s1"), 1);
        assert_eq!(store.len("s2"), 1);
    }

    // ── Capacity ──

    #[test]
    fn evicts_oldest_rule_at_capacity() {
        let store = FeedbackStore::new();
        for i in 0..MAX_RULES_PER_SESSION + 3 {
            store.add("s1", make_fb(&format!("rule {i}")));
        }
        assert_eq!(store.len("s1"), MAX_RULES_PER_SESSION);
        let rules = store.rules("s1");
        assert_eq!(rules[0].rule, "rule 3");
    }

    #[test]
    fn evicts_oldest_session_at_capacity() {
        let store = FeedbackStore::new();
        for i in 0..MAX_SESSIONS + 5 {
            store.add(&format!("s{i}"), make_fb("rule"));
        }
        assert!(store.session_count() <= MAX_SESSIONS);
        // Oldest sessions should be evicted
        assert!(store.is_empty("s0"));
        assert!(!store.is_empty(&format!("s{}", MAX_SESSIONS + 4)));
    }

    // ── Injection format ──

    #[test]
    fn empty_session_empty_injection() {
        let store = FeedbackStore::new();
        assert!(store.build_injection("s1").is_empty());
    }

    #[test]
    fn injection_includes_reason_when_stated() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb_full("use real DB", "mocks diverged", "General"));
        let inj = store.build_injection("s1");
        assert!(inj.contains("Why: mocks diverged"));
    }

    #[test]
    fn injection_omits_reason_when_not_stated() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("use real DB"));
        let inj = store.build_injection("s1");
        assert!(!inj.contains("Why:"));
    }

    #[test]
    fn injection_includes_apply_when_when_specific() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb_full("use real DB", "Not stated", "integration tests"));
        let inj = store.build_injection("s1");
        assert!(inj.contains("When: integration tests"));
    }

    #[test]
    fn injection_omits_apply_when_when_general() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("use real DB"));
        let inj = store.build_injection("s1");
        assert!(!inj.contains("When:"));
    }

    // ── Thread safety ──

    #[test]
    fn clear_session_removes_rules_and_order() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A"));
        store.add("s2", make_fb("rule B"));
        assert_eq!(store.session_count(), 2);

        store.clear_session("s1");
        assert!(store.is_empty("s1"));
        assert_eq!(store.session_count(), 1);
        assert!(!store.is_empty("s2"));
    }

    #[test]
    fn clear_nonexistent_session_is_noop() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A"));
        store.clear_session("nonexistent");
        assert_eq!(store.session_count(), 1);
    }

    #[test]
    fn concurrent_access() {
        use std::sync::Arc;
        let store = Arc::new(FeedbackStore::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let s = store.clone();
                std::thread::spawn(move || {
                    let sid = format!("s{}", i % 3);
                    s.add(&sid, make_fb(&format!("rule {i}")));
                    s.len(&sid)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // 10 rules across 3 sessions
        let total: usize = (0..3).map(|i| store.len(&format!("s{i}"))).sum();
        assert_eq!(total, 10);
    }
}
