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
            if inner.order.len() >= MAX_SESSIONS
                && let Some(oldest) = inner.order.pop_front()
            {
                inner.sessions.remove(&oldest);
            }
            inner.order.push_back(session_id.to_string());
        }

        let entry = inner
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionRules { rules: Vec::new() });

        if entry
            .rules
            .iter()
            .any(|r| r.rule.eq_ignore_ascii_case(&feedback.rule))
        {
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
        self.build_injection_filtered(session_id, None)
    }

    /// Build injection text, optionally filtering rules by relevance to the
    /// current user message. When `user_message` is provided, rules whose
    /// keywords overlap with the message are injected first ("relevant"),
    /// followed by up to `MAX_IRRELEVANT_RULES` others so the model still
    /// has background context without unbounded token growth.
    pub fn build_injection_filtered(&self, session_id: &str, user_message: Option<&str>) -> String {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = inner.sessions.get(session_id) else {
            return String::new();
        };
        if entry.rules.is_empty() {
            return String::new();
        }

        let rules: Vec<&StructuredFeedback> = match user_message {
            Some(msg) => Self::filter_relevant(&entry.rules, msg),
            None => entry.rules.iter().collect(),
        };

        if rules.is_empty() {
            return String::new();
        }

        let mut lines = vec!["[Learned Feedback Rules]".to_string()];
        for fb in &rules {
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

    /// Maximum number of non-matching rules to include as background context.
    const MAX_IRRELEVANT_RULES: usize = 3;

    /// Select rules relevant to the current user message. Rules with keyword
    /// overlap are always included; up to `MAX_IRRELEVANT_RULES` others are
    /// appended so the model retains some background awareness.
    fn filter_relevant<'a>(
        rules: &'a [StructuredFeedback],
        user_message: &str,
    ) -> Vec<&'a StructuredFeedback> {
        let msg_lower = user_message.to_lowercase();
        let msg_words: std::collections::HashSet<&str> = msg_lower
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|w| w.chars().count() >= 2)
            .collect();

        let mut relevant = Vec::new();
        let mut irrelevant = Vec::new();

        for fb in rules {
            let rule_text = format!("{} {}", fb.rule, fb.apply_when).to_lowercase();
            let has_overlap = rule_text
                .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                .filter(|w| w.chars().count() >= 2)
                .any(|w| msg_words.contains(w));

            if has_overlap {
                relevant.push(fb);
            } else {
                irrelevant.push(fb);
            }
        }

        // Always include all relevant rules + a few irrelevant for background
        relevant.extend(irrelevant.into_iter().take(Self::MAX_IRRELEVANT_RULES));
        relevant
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
    fn deduplicates_case_insensitive() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("Don't use mocks"));
        store.add("s1", make_fb("don't use mocks"));
        assert_eq!(
            store.len("s1"),
            1,
            "case-insensitive dedup should prevent duplicate"
        );
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
        store.add(
            "s1",
            make_fb_full("use real DB", "mocks diverged", "General"),
        );
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
        store.add(
            "s1",
            make_fb_full("use real DB", "Not stated", "integration tests"),
        );
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

    // ── Relevance filtering ──

    fn make_fb_with_apply(rule: &str, apply_when: &str) -> StructuredFeedback {
        StructuredFeedback {
            rule: rule.to_string(),
            reason: "Not stated".to_string(),
            apply_when: apply_when.to_string(),
            source_signal: "correction".to_string(),
            confidence: 0.8,
        }
    }

    #[test]
    fn filtered_injection_includes_relevant_rules_first() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb_with_apply("don't use mocks", "testing"));
        store.add("s1", make_fb_with_apply("use JSON output", "API responses"));
        store.add("s1", make_fb_with_apply("prefer async", "database queries"));

        let injection = store.build_injection_filtered("s1", Some("write tests without mocks"));
        assert!(
            injection.contains("don't use mocks"),
            "relevant rule should be included"
        );
    }

    #[test]
    fn filtered_injection_caps_irrelevant_rules() {
        let store = FeedbackStore::new();
        for i in 0..6 {
            store.add(
                "s1",
                make_fb_with_apply(&format!("rule about topic{i}"), "General"),
            );
        }
        let injection = store.build_injection_filtered("s1", Some("deploy to production"));
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(
            rule_count, 3,
            "should cap irrelevant rules at 3, got {rule_count}"
        );
    }

    #[test]
    fn filtered_injection_all_relevant_bypass_cap() {
        let store = FeedbackStore::new();
        store.add(
            "s1",
            make_fb_with_apply("always deploy with --dry-run", "General"),
        );
        store.add(
            "s1",
            make_fb_with_apply("deploy to staging first", "General"),
        );
        store.add(
            "s1",
            make_fb_with_apply("check deploy logs after", "General"),
        );
        store.add("s1", make_fb_with_apply("deploy needs approval", "General"));
        store.add(
            "s1",
            make_fb_with_apply("run deploy in background", "General"),
        );
        store.add(
            "s1",
            make_fb_with_apply("deploy only from main branch", "General"),
        );
        let injection = store.build_injection_filtered("s1", Some("deploy the service"));
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(
            rule_count, 6,
            "all relevant rules should be included, got {rule_count}"
        );
    }

    #[test]
    fn filtered_injection_without_message_returns_all() {
        let store = FeedbackStore::new();
        for i in 0..6 {
            store.add(
                "s1",
                make_fb_with_apply(&format!("rule about topic{i}"), "General"),
            );
        }
        let injection = store.build_injection_filtered("s1", None);
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(rule_count, 6, "no filter should return all rules");
    }

    #[test]
    fn unfiltered_build_injection_still_returns_all() {
        let store = FeedbackStore::new();
        for i in 0..6 {
            store.add(
                "s1",
                make_fb_with_apply(&format!("rule about topic{i}"), "General"),
            );
        }
        let injection = store.build_injection("s1");
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(rule_count, 6, "unfiltered should return all rules");
    }

    #[test]
    fn filtered_injection_case_insensitive() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb_with_apply("Don't use Mocks", "Testing"));
        let injection = store.build_injection_filtered("s1", Some("write tests with mocks"));
        assert!(
            injection.contains("Don't use Mocks"),
            "case-insensitive match should find 'Mocks' via 'mocks'"
        );
    }

    #[test]
    fn filtered_injection_chinese_keyword_match() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb_with_apply("不要用bash执行git命令", "General"));
        store.add("s1", make_fb_with_apply("always run clippy", "General"));
        let injection = store.build_injection_filtered("s1", Some("用bash运行测试"));
        // "bash" overlaps — should match
        assert!(
            injection.contains("bash"),
            "Chinese+ASCII mixed keyword should match"
        );
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
