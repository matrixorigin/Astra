//! Session-scoped structured feedback store.
//!
//! Stores `StructuredFeedback` rules extracted from user corrections and
//! injects matching rules into subsequent turn system prompts. This closes
//! the feedback loop: detect → extract → store → inject.
//!
//! Rules are isolated per session_id — no cross-session leakage.
//!
//! # Why not persist across sessions?
//!
//! Feedback rules capture raw user corrections that may contain private or
//! project-specific context (filenames, commands, identities). Persisting
//! them would leak signal across unrelated sessions and risk re-injecting
//! obsolete or private guidance into fresh conversations. Cross-session
//! signal is expressed instead through aggregated, anonymised health and
//! learning snapshots.

use std::collections::{HashMap, HashSet, VecDeque};

use tokio::sync::Mutex;

use astra_text_utils::text_tokenize::tokenize;
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
    /// Least-recently-used order, oldest at the front.
    order: VecDeque<String>,
}

struct SessionRules {
    rules: Vec<StructuredFeedback>,
    suppressed_rule_keys: HashSet<String>,
    last_injected_rule_keys: Vec<String>,
}

impl FeedbackStore {
    fn touch_session(inner: &mut StoreInner, session_id: &str) {
        inner.order.retain(|existing| existing != session_id);
        inner.order.push_back(session_id.to_string());
    }

    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StoreInner {
                sessions: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Store a feedback rule for a session. Deduplicates by rule text.
    pub async fn add(&self, session_id: &str, feedback: StructuredFeedback) {
        let mut inner = self.inner.lock().await;

        // LRU eviction of the least recently used session if at capacity.
        if !inner.sessions.contains_key(session_id)
            && inner.order.len() >= MAX_SESSIONS
            && let Some(oldest) = inner.order.pop_front()
        {
            inner.sessions.remove(&oldest);
        }
        Self::touch_session(&mut inner, session_id);

        let entry = inner
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionRules {
                rules: Vec::new(),
                suppressed_rule_keys: HashSet::new(),
                last_injected_rule_keys: Vec::new(),
            });

        if entry
            .rules
            .iter()
            .any(|r| r.rule.eq_ignore_ascii_case(&feedback.rule))
        {
            entry
                .suppressed_rule_keys
                .remove(&Self::rule_key(&feedback));
            return;
        }
        if entry.rules.len() >= MAX_RULES_PER_SESSION {
            let removed = entry.rules.remove(0);
            entry.suppressed_rule_keys.remove(&Self::rule_key(&removed));
        }
        entry.rules.push(feedback);
    }

    /// Number of stored rules for a session.
    pub async fn len(&self, session_id: &str) -> usize {
        let mut inner = self.inner.lock().await;
        if inner.sessions.contains_key(session_id) {
            Self::touch_session(&mut inner, session_id);
        }
        inner
            .sessions
            .get(session_id)
            .map(|s| {
                s.rules
                    .iter()
                    .filter(|rule| !s.suppressed_rule_keys.contains(&Self::rule_key(rule)))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Whether a session has any rules.
    pub async fn is_empty(&self, session_id: &str) -> bool {
        self.len(session_id).await == 0
    }

    /// Build a context injection string for a specific session.
    pub async fn build_injection(&self, session_id: &str) -> String {
        self.build_injection_filtered(session_id, None).await
    }

    /// Build injection text, optionally filtering rules by relevance to the
    /// current user message. When `user_message` is provided, only rules with
    /// concrete lexical evidence in the current task are injected.
    pub async fn build_injection_filtered(
        &self,
        session_id: &str,
        user_message: Option<&str>,
    ) -> String {
        let mut inner = self.inner.lock().await;
        if inner.sessions.contains_key(session_id) {
            Self::touch_session(&mut inner, session_id);
        }
        let Some(entry) = inner.sessions.get_mut(session_id) else {
            return String::new();
        };
        if entry.rules.is_empty() {
            return String::new();
        }

        let selected_indices: Vec<usize> = match user_message {
            Some(msg) => {
                Self::filter_relevant_indices(&entry.rules, &entry.suppressed_rule_keys, msg)
            }
            None => Self::active_rule_indices(&entry.rules, &entry.suppressed_rule_keys),
        };

        if selected_indices.is_empty() {
            entry.last_injected_rule_keys.clear();
            return String::new();
        }

        entry.last_injected_rule_keys = selected_indices
            .iter()
            .map(|&idx| Self::rule_key(&entry.rules[idx]))
            .collect();

        let mut lines = vec!["[Learned Feedback Rules]".to_string()];
        for &idx in &selected_indices {
            let fb = &entry.rules[idx];
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

    /// Suppress rules from the most recent injection by their injected-list
    /// indices. The caller owns semantic interpretation of user/model feedback.
    ///
    /// Returns the number of newly suppressed rules.
    pub async fn suppress_last_injected_rule_indices(
        &self,
        session_id: &str,
        indices: &[usize],
    ) -> usize {
        if indices.is_empty() {
            return 0;
        }
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.sessions.get_mut(session_id) else {
            return 0;
        };
        let requested: HashSet<usize> = indices.iter().copied().collect();
        let mut changed = 0usize;
        for (idx, key) in entry.last_injected_rule_keys.iter().enumerate() {
            if requested.contains(&idx) && entry.suppressed_rule_keys.insert(key.clone()) {
                changed += 1;
            }
        }
        if changed > 0 {
            entry.last_injected_rule_keys = entry
                .last_injected_rule_keys
                .iter()
                .enumerate()
                .filter_map(|(idx, key)| (!requested.contains(&idx)).then_some(key.clone()))
                .collect();
        }
        changed
    }

    /// Select only rules with concrete evidence in the current user message.
    fn filter_relevant_indices(
        rules: &[StructuredFeedback],
        suppressed_rule_keys: &HashSet<String>,
        user_message: &str,
    ) -> Vec<usize> {
        let query_terms = Self::meaningful_terms(user_message);
        if query_terms.is_empty() {
            return Vec::new();
        }

        let mut scored = Vec::new();
        for (idx, fb) in rules.iter().enumerate() {
            if suppressed_rule_keys.contains(&Self::rule_key(fb)) {
                continue;
            }
            let rule_text = Self::rule_relevance_text(fb);
            let rule_terms = Self::meaningful_terms(&rule_text);
            let overlap_score = Self::overlap_score(&query_terms, &rule_terms);
            if overlap_score > 0 {
                scored.push((idx, overlap_score));
            }
        }

        scored.sort_by(|(left_idx, left_score), (right_idx, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_idx.cmp(right_idx))
        });
        scored.into_iter().map(|(idx, _)| idx).collect()
    }

    fn active_rule_indices(
        rules: &[StructuredFeedback],
        suppressed_rule_keys: &HashSet<String>,
    ) -> Vec<usize> {
        rules
            .iter()
            .enumerate()
            .filter_map(|(idx, rule)| {
                (!suppressed_rule_keys.contains(&Self::rule_key(rule))).then_some(idx)
            })
            .collect()
    }

    fn rule_relevance_text(fb: &StructuredFeedback) -> String {
        let mut text = format!("{} {}", fb.rule, fb.apply_when);
        if fb.reason != "Not stated" {
            text.push(' ');
            text.push_str(&fb.reason);
        }
        text
    }

    fn meaningful_terms(text: &str) -> HashSet<String> {
        tokenize(text)
            .into_iter()
            .filter(|term| Self::is_meaningful_term(term))
            .collect()
    }

    fn overlap_score(query_terms: &HashSet<String>, rule_terms: &HashSet<String>) -> usize {
        query_terms
            .intersection(rule_terms)
            .filter(|term| Self::is_strong_overlap_term(term))
            .map(|term| if term.is_ascii() { 2 } else { 1 })
            .sum()
    }

    fn is_strong_overlap_term(term: &str) -> bool {
        if term.is_ascii() {
            return term.chars().count() >= 3;
        }
        term.chars().count() >= 2
    }

    fn is_meaningful_term(term: &str) -> bool {
        if term.trim().is_empty() {
            return false;
        }
        if matches!(
            term,
            "the"
                | "and"
                | "for"
                | "with"
                | "that"
                | "this"
                | "from"
                | "into"
                | "when"
                | "rule"
                | "rules"
                | "general"
                | "always"
                | "never"
                | "should"
                | "would"
                | "could"
                | "don't"
                | "dont"
                | "doesn't"
                | "doesnt"
                | "do"
                | "not"
                | "use"
                | "using"
                | "used"
                | "user"
                | "task"
                | "please"
                | "help"
                | "need"
                | "want"
                | "about"
                | "because"
                | "instead"
                | "prefer"
                | "run"
        ) {
            return false;
        }
        if matches!(
            term,
            "的" | "了"
                | "是"
                | "在"
                | "和"
                | "与"
                | "或"
                | "这"
                | "那"
                | "用"
                | "要"
                | "不"
                | "做"
                | "说"
                | "把"
                | "给"
                | "对"
                | "错"
        ) {
            return false;
        }
        true
    }

    fn rule_key(fb: &StructuredFeedback) -> String {
        format!(
            "{}\n{}\n{}",
            fb.rule.to_lowercase(),
            fb.reason.to_lowercase(),
            fb.apply_when.to_lowercase()
        )
    }

    /// Get a snapshot of rules for a session.
    pub async fn rules(&self, session_id: &str) -> Vec<StructuredFeedback> {
        let mut inner = self.inner.lock().await;
        if inner.sessions.contains_key(session_id) {
            Self::touch_session(&mut inner, session_id);
        }
        inner
            .sessions
            .get(session_id)
            .map(|s| {
                s.rules
                    .iter()
                    .filter(|rule| !s.suppressed_rule_keys.contains(&Self::rule_key(rule)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Number of tracked sessions.
    pub async fn session_count(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// Remove all rules for a session. Call on session close for explicit cleanup.
    pub async fn clear_session(&self, session_id: &str) {
        let mut inner = self.inner.lock().await;
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

    #[tokio::test]
    async fn sessions_are_isolated() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A")).await;
        store.add("s2", make_fb("rule B")).await;

        assert_eq!(store.len("s1").await, 1);
        assert_eq!(store.len("s2").await, 1);
        assert!(store.build_injection("s1").await.contains("rule A"));
        assert!(!store.build_injection("s1").await.contains("rule B"));
        assert!(store.build_injection("s2").await.contains("rule B"));
        assert!(!store.build_injection("s2").await.contains("rule A"));
    }

    #[tokio::test]
    async fn unknown_session_returns_empty() {
        let store = FeedbackStore::new();
        assert!(store.is_empty("nonexistent").await);
        assert!(store.build_injection("nonexistent").await.is_empty());
        assert!(store.rules("nonexistent").await.is_empty());
    }

    // ── Dedup ──

    #[tokio::test]
    async fn deduplicates_within_session() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A")).await;
        store.add("s1", make_fb("rule A")).await;
        assert_eq!(store.len("s1").await, 1);
    }

    #[tokio::test]
    async fn deduplicates_case_insensitive() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("Don't use mocks")).await;
        store.add("s1", make_fb("don't use mocks")).await;
        assert_eq!(
            store.len("s1").await,
            1,
            "case-insensitive dedup should prevent duplicate"
        );
    }

    #[tokio::test]
    async fn same_rule_different_sessions_not_deduped() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A")).await;
        store.add("s2", make_fb("rule A")).await;
        assert_eq!(store.len("s1").await, 1);
        assert_eq!(store.len("s2").await, 1);
    }

    // ── Capacity ──

    #[tokio::test]
    async fn evicts_oldest_rule_at_capacity() {
        let store = FeedbackStore::new();
        for i in 0..MAX_RULES_PER_SESSION + 3 {
            store.add("s1", make_fb(&format!("rule {i}"))).await;
        }
        assert_eq!(store.len("s1").await, MAX_RULES_PER_SESSION);
        let rules = store.rules("s1").await;
        assert_eq!(rules[0].rule, "rule 3");
    }

    #[tokio::test]
    async fn evicts_oldest_session_at_capacity() {
        let store = FeedbackStore::new();
        for i in 0..MAX_SESSIONS + 5 {
            store.add(&format!("s{i}"), make_fb("rule")).await;
        }
        assert!(store.session_count().await <= MAX_SESSIONS);
        // Oldest sessions should be evicted
        assert!(store.is_empty("s0").await);
        assert!(!store.is_empty(&format!("s{}", MAX_SESSIONS + 4)).await);
    }

    #[tokio::test]
    async fn active_session_is_retained_by_lru_eviction() {
        let store = FeedbackStore::new();
        for i in 0..MAX_SESSIONS {
            store.add(&format!("s{i}"), make_fb("rule")).await;
        }

        assert!(store.build_injection("s0").await.contains("rule"));
        store.add("new-session", make_fb("new rule")).await;

        assert!(
            !store.is_empty("s0").await,
            "reading a session must refresh its LRU position"
        );
        assert!(
            store.is_empty("s1").await,
            "the least recently used session should be evicted"
        );
        assert!(!store.is_empty("new-session").await);
    }

    #[tokio::test]
    async fn every_session_read_refreshes_lru_without_creating_missing_sessions() {
        for read in ["len", "is_empty", "rules"] {
            let store = FeedbackStore::new();
            for i in 0..MAX_SESSIONS {
                store.add(&format!("s{i}"), make_fb("rule")).await;
            }

            match read {
                "len" => assert_eq!(store.len("s0").await, 1),
                "is_empty" => assert!(!store.is_empty("s0").await),
                "rules" => assert_eq!(store.rules("s0").await.len(), 1),
                _ => unreachable!(),
            }
            store.add("new-session", make_fb("new rule")).await;

            assert!(!store.is_empty("s0").await, "{read} must refresh LRU");
            assert!(store.is_empty("s1").await, "oldest session must be evicted");

            let before = store.session_count().await;
            assert_eq!(store.len("missing").await, 0);
            assert!(store.is_empty("missing").await);
            assert!(store.rules("missing").await.is_empty());
            assert_eq!(store.session_count().await, before);
        }
    }

    // ── Injection format ──

    #[tokio::test]
    async fn empty_session_empty_injection() {
        let store = FeedbackStore::new();
        assert!(store.build_injection("s1").await.is_empty());
    }

    #[tokio::test]
    async fn injection_includes_reason_when_stated() {
        let store = FeedbackStore::new();
        store
            .add(
                "s1",
                make_fb_full("use real DB", "mocks diverged", "General"),
            )
            .await;
        let inj = store.build_injection("s1").await;
        assert!(inj.contains("Why: mocks diverged"));
    }

    #[tokio::test]
    async fn injection_omits_reason_when_not_stated() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("use real DB")).await;
        let inj = store.build_injection("s1").await;
        assert!(!inj.contains("Why:"));
    }

    #[tokio::test]
    async fn injection_includes_apply_when_when_specific() {
        let store = FeedbackStore::new();
        store
            .add(
                "s1",
                make_fb_full("use real DB", "Not stated", "integration tests"),
            )
            .await;
        let inj = store.build_injection("s1").await;
        assert!(inj.contains("When: integration tests"));
    }

    #[tokio::test]
    async fn injection_omits_apply_when_when_general() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("use real DB")).await;
        let inj = store.build_injection("s1").await;
        assert!(!inj.contains("When:"));
    }

    // ── Thread safety ──

    #[tokio::test]
    async fn clear_session_removes_rules_and_order() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A")).await;
        store.add("s2", make_fb("rule B")).await;
        assert_eq!(store.session_count().await, 2);

        store.clear_session("s1").await;
        assert!(store.is_empty("s1").await);
        assert_eq!(store.session_count().await, 1);
        assert!(!store.is_empty("s2").await);
    }

    #[tokio::test]
    async fn clear_nonexistent_session_is_noop() {
        let store = FeedbackStore::new();
        store.add("s1", make_fb("rule A")).await;
        store.clear_session("nonexistent").await;
        assert_eq!(store.session_count().await, 1);
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

    #[tokio::test]
    async fn filtered_injection_includes_relevant_rules_first() {
        let store = FeedbackStore::new();
        store
            .add("s1", make_fb_with_apply("don't use mocks", "testing"))
            .await;
        store
            .add("s1", make_fb_with_apply("use JSON output", "API responses"))
            .await;
        store
            .add("s1", make_fb_with_apply("prefer async", "database queries"))
            .await;

        let injection = store
            .build_injection_filtered("s1", Some("write tests without mocks"))
            .await;
        assert!(
            injection.contains("don't use mocks"),
            "relevant rule should be included"
        );
    }

    #[tokio::test]
    async fn filtered_injection_drops_irrelevant_rules() {
        let store = FeedbackStore::new();
        for i in 0..6 {
            store
                .add(
                    "s1",
                    make_fb_with_apply(&format!("rule about topic{i}"), "General"),
                )
                .await;
        }
        let injection = store
            .build_injection_filtered("s1", Some("deploy to production"))
            .await;
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(
            rule_count, 0,
            "irrelevant rules must not be injected, got {rule_count}"
        );
    }

    #[tokio::test]
    async fn filtered_injection_does_not_fill_with_unrelated_browser_rule() {
        let store = FeedbackStore::new();
        store
            .add(
                "s1",
                make_fb_with_apply(
                    "Do not treat curl/server/process checks as browser verification",
                    "HTML/browser verification",
                ),
            )
            .await;

        let injection = store
            .build_injection_filtered("s1", Some("review the Rust executor code"))
            .await;

        assert!(
            injection.is_empty(),
            "unrelated learned feedback must stay out of the prompt"
        );
    }

    #[tokio::test]
    async fn structured_feedback_suppresses_last_injected_rules() {
        let store = FeedbackStore::new();
        store
            .add("s1", make_fb_with_apply("don't use mocks", "testing"))
            .await;

        let first = store
            .build_injection_filtered("s1", Some("write tests with mocks"))
            .await;
        assert!(first.contains("don't use mocks"));

        let suppressed = store.suppress_last_injected_rule_indices("s1", &[0]).await;
        assert_eq!(suppressed, 1);
        assert_eq!(store.len("s1").await, 0);

        let second = store
            .build_injection_filtered("s1", Some("write tests with mocks"))
            .await;
        assert!(
            second.is_empty(),
            "dismissed rule should not be re-injected"
        );
    }

    #[tokio::test]
    async fn filtered_injection_all_relevant_bypass_cap() {
        let store = FeedbackStore::new();
        store
            .add(
                "s1",
                make_fb_with_apply("always deploy with --dry-run", "General"),
            )
            .await;
        store
            .add(
                "s1",
                make_fb_with_apply("deploy to staging first", "General"),
            )
            .await;
        store
            .add(
                "s1",
                make_fb_with_apply("check deploy logs after", "General"),
            )
            .await;
        store
            .add("s1", make_fb_with_apply("deploy needs approval", "General"))
            .await;
        store
            .add(
                "s1",
                make_fb_with_apply("run deploy in background", "General"),
            )
            .await;
        store
            .add(
                "s1",
                make_fb_with_apply("deploy only from main branch", "General"),
            )
            .await;
        let injection = store
            .build_injection_filtered("s1", Some("deploy the service"))
            .await;
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(
            rule_count, 6,
            "all relevant rules should be included, got {rule_count}"
        );
    }

    #[tokio::test]
    async fn filtered_injection_without_message_returns_all() {
        let store = FeedbackStore::new();
        for i in 0..6 {
            store
                .add(
                    "s1",
                    make_fb_with_apply(&format!("rule about topic{i}"), "General"),
                )
                .await;
        }
        let injection = store.build_injection_filtered("s1", None).await;
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(rule_count, 6, "no filter should return all rules");
    }

    #[tokio::test]
    async fn unfiltered_build_injection_still_returns_all() {
        let store = FeedbackStore::new();
        for i in 0..6 {
            store
                .add(
                    "s1",
                    make_fb_with_apply(&format!("rule about topic{i}"), "General"),
                )
                .await;
        }
        let injection = store.build_injection("s1").await;
        let rule_count = injection.matches("- Rule:").count();
        assert_eq!(rule_count, 6, "unfiltered should return all rules");
    }

    #[tokio::test]
    async fn filtered_injection_case_insensitive() {
        let store = FeedbackStore::new();
        store
            .add("s1", make_fb_with_apply("Don't use Mocks", "Testing"))
            .await;
        let injection = store
            .build_injection_filtered("s1", Some("write tests with mocks"))
            .await;
        assert!(
            injection.contains("Don't use Mocks"),
            "case-insensitive match should find 'Mocks' via 'mocks'"
        );
    }

    #[tokio::test]
    async fn filtered_injection_chinese_keyword_match() {
        let store = FeedbackStore::new();
        store
            .add("s1", make_fb_with_apply("不要用bash执行git命令", "General"))
            .await;
        store
            .add("s1", make_fb_with_apply("always run clippy", "General"))
            .await;
        let injection = store
            .build_injection_filtered("s1", Some("用bash运行测试"))
            .await;
        // "bash" overlaps — should match
        assert!(
            injection.contains("bash"),
            "Chinese+ASCII mixed keyword should match"
        );
        assert!(
            !injection.contains("clippy"),
            "unrelated rule should not be included as filler"
        );
    }

    #[tokio::test]
    async fn concurrent_access() {
        use std::sync::Arc;
        let store = Arc::new(FeedbackStore::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let s = store.clone();
                tokio::spawn(async move {
                    let sid = format!("s{}", i % 3);
                    s.add(&sid, make_fb(&format!("rule {i}"))).await;
                    s.len(&sid).await
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        // 10 rules added across 3 sessions — each Add should succeed,
        // so total lengths should sum to 10.
        assert_eq!(
            store.len("s0").await + store.len("s1").await + store.len("s2").await,
            10
        );
    }
}
