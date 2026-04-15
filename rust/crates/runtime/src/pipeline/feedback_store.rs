//! Session-scoped structured feedback store.
//!
//! Stores `StructuredFeedback` rules extracted from user corrections and
//! injects matching rules into subsequent turn system prompts. This closes
//! the feedback loop: detect → extract → store → inject.

use std::sync::Mutex;

use astra_turn_types::StructuredFeedback;

/// Maximum stored feedback rules per session (prevents unbounded growth).
const MAX_RULES: usize = 50;

/// Session-scoped store for structured feedback rules.
///
/// Thread-safe via internal `Mutex`. Designed to be shared as `Arc<FeedbackStore>`
/// across turns within a single session.
pub struct FeedbackStore {
    rules: Mutex<Vec<StructuredFeedback>>,
}

impl FeedbackStore {
    pub fn new() -> Self {
        Self {
            rules: Mutex::new(Vec::new()),
        }
    }

    /// Store a feedback rule. Deduplicates by rule text.
    /// Evicts oldest rule when at capacity.
    pub fn add(&self, feedback: StructuredFeedback) {
        let mut rules = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        // Deduplicate by rule text
        if rules.iter().any(|r| r.rule == feedback.rule) {
            return;
        }
        if rules.len() >= MAX_RULES {
            rules.remove(0);
        }
        rules.push(feedback);
    }

    /// Number of stored rules.
    pub fn len(&self) -> usize {
        self.rules.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build a context injection string from all stored rules.
    /// Returns empty string if no rules are stored.
    pub fn build_injection(&self) -> String {
        let rules = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        if rules.is_empty() {
            return String::new();
        }
        let mut lines = vec!["[Learned Feedback Rules]".to_string()];
        for fb in rules.iter() {
            let mut entry = format!("- Rule: {}", fb.rule);
            if fb.reason != "Not stated" {
                entry.push_str(&format!(" | Why: {}", fb.reason));
            }
            if fb.apply_when != "General" {
                entry.push_str(&format!(" | When: {}", fb.apply_when));
            }
            lines.push(entry);
        }
        lines.join("\n")
    }

    /// Get a snapshot of all stored rules.
    pub fn rules(&self) -> Vec<StructuredFeedback> {
        self.rules.lock().unwrap_or_else(|e| e.into_inner()).clone()
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

    fn make_feedback(rule: &str, reason: &str, apply_when: &str) -> StructuredFeedback {
        StructuredFeedback {
            rule: rule.into(),
            reason: reason.into(),
            apply_when: apply_when.into(),
            source_signal: "correction".into(),
            confidence: 0.9,
        }
    }

    #[test]
    fn add_and_retrieve() {
        let store = FeedbackStore::new();
        store.add(make_feedback("don't use mocks", "prod divergence", "integration tests"));
        assert_eq!(store.len(), 1);
        let rules = store.rules();
        assert_eq!(rules[0].rule, "don't use mocks");
    }

    #[test]
    fn deduplicates_by_rule_text() {
        let store = FeedbackStore::new();
        store.add(make_feedback("don't use mocks", "reason 1", "General"));
        store.add(make_feedback("don't use mocks", "reason 2", "Specific"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let store = FeedbackStore::new();
        for i in 0..MAX_RULES + 5 {
            store.add(make_feedback(&format!("rule {i}"), "Not stated", "General"));
        }
        assert_eq!(store.len(), MAX_RULES);
        let rules = store.rules();
        // Oldest 5 should be evicted
        assert_eq!(rules[0].rule, "rule 5");
        assert_eq!(rules[MAX_RULES - 1].rule, format!("rule {}", MAX_RULES + 4));
    }

    #[test]
    fn empty_store_returns_empty_injection() {
        let store = FeedbackStore::new();
        assert!(store.build_injection().is_empty());
        assert!(store.is_empty());
    }

    #[test]
    fn injection_includes_rule() {
        let store = FeedbackStore::new();
        store.add(make_feedback("don't use mocks", "Not stated", "General"));
        let injection = store.build_injection();
        assert!(injection.contains("[Learned Feedback Rules]"));
        assert!(injection.contains("don't use mocks"));
    }

    #[test]
    fn injection_includes_reason_when_stated() {
        let store = FeedbackStore::new();
        store.add(make_feedback("use real DB", "mocks diverged from prod", "General"));
        let injection = store.build_injection();
        assert!(injection.contains("Why: mocks diverged from prod"));
    }

    #[test]
    fn injection_omits_reason_when_not_stated() {
        let store = FeedbackStore::new();
        store.add(make_feedback("use real DB", "Not stated", "General"));
        let injection = store.build_injection();
        assert!(!injection.contains("Why:"));
    }

    #[test]
    fn injection_includes_apply_when_when_specific() {
        let store = FeedbackStore::new();
        store.add(make_feedback("use real DB", "Not stated", "integration tests"));
        let injection = store.build_injection();
        assert!(injection.contains("When: integration tests"));
    }

    #[test]
    fn injection_omits_apply_when_when_general() {
        let store = FeedbackStore::new();
        store.add(make_feedback("use real DB", "Not stated", "General"));
        let injection = store.build_injection();
        assert!(!injection.contains("When:"));
    }

    #[test]
    fn injection_multiple_rules() {
        let store = FeedbackStore::new();
        store.add(make_feedback("rule A", "Not stated", "General"));
        store.add(make_feedback("rule B", "reason B", "context B"));
        let injection = store.build_injection();
        assert!(injection.contains("rule A"));
        assert!(injection.contains("rule B"));
        assert!(injection.contains("Why: reason B"));
        assert!(injection.contains("When: context B"));
    }

    #[test]
    fn thread_safe_concurrent_access() {
        use std::sync::Arc;
        let store = Arc::new(FeedbackStore::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let s = store.clone();
                std::thread::spawn(move || {
                    s.add(make_feedback(&format!("rule {i}"), "Not stated", "General"));
                    s.len()
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(store.len(), 10);
    }
}
