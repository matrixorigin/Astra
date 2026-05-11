//! Memory Orchestrator — runtime-side coordination of the v2 memory surface.
//!
//! The LLM sees `memory(action=...)` as a tool; the runtime has its own
//! independent need to consult / write memory around the session
//! lifecycle (pre-warm context, persist episodes, trigger reflection,
//! nudge attention on topic shifts). Centralising that logic here
//! avoids scattering v1 HTTP calls across server_loop_host, bridge,
//! session_end, and the prefetch path.
//!
//! Design:
//! - **Stateless at construction**; all state is held either on the
//!   underlying [`MemoriaClient`] (focus store) or passed per-call.
//! - **Best-effort**: every method returns a result but failures at the
//!   orchestrator level log and degrade, never panic or abort the turn.
//! - **No LLM calls**: orchestration is deterministic. Summarisation
//!   that needs an LLM lives in the extraction path
//!   ([`crate::session_memory`]), not here.
//!
//! Phases:
//! - **session start** → [`MemoryOrchestrator::on_session_start`]
//!   prefetch profile + optional recent-episode recall for
//!   continuation hints; returns a [`SessionStartMemories`] blob
//!   callers can inject into the prompt cold-warmup lane.
//! - **turn start** → [`MemoryOrchestrator::on_turn_recall`]
//!   the thin wrapper around `retrieve_ext` that is aware of the
//!   active focus hints and applies a consistent top-k.
//! - **topic focus** → [`MemoryOrchestrator::focus_on_topic`]
//!   sets a session-scoped boost. Callers decide when to call this
//!   (e.g. after `session(action=set_goal)` or when the routing
//!   engine sees a persistent topic).
//! - **tool-result feedback** → [`MemoryOrchestrator::feedback`]
//!   records useful/irrelevant/outdated/wrong on a specific
//!   memory_id — callers thread the id through from a prior recall.
//! - **session end** → delegates to [`super::session_end_governance`]
//!   which already handles purge + episode + reflect.

use std::sync::Arc;

use super::memoria_compact::{MemoriaClient, MemoriaMemory};

/// Bundle returned on session start — memories the orchestrator pulled
/// to warm up context. Callers decide how to surface them (system
/// prompt block, first-user-message prefix, etc.).
#[derive(Debug, Clone, Default)]
pub struct SessionStartMemories {
    /// User profile / role / preferences (from `memory_type=profile`).
    pub profile: Vec<MemoriaMemory>,
    /// Recent cross-session episodes that may be relevant (sorted by
    /// retrieval score; top 3 by default).
    pub recent_episodes: Vec<MemoriaMemory>,
    /// Best-effort query-relevant long-term memories for the first user
    /// message, if any was available.
    pub relevant: Vec<MemoriaMemory>,
}

impl SessionStartMemories {
    pub fn is_empty(&self) -> bool {
        self.profile.is_empty() && self.recent_episodes.is_empty() && self.relevant.is_empty()
    }

    /// Render a compact text block suitable for the `<session_memory>`
    /// system-prompt section (`None` when nothing to inject).
    pub fn to_prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut s = String::with_capacity(512);
        s.push_str("<session_memory>\n");
        if !self.profile.is_empty() {
            s.push_str("### User profile\n");
            for m in &self.profile {
                s.push_str("- ");
                s.push_str(compact_line(&m.content, 160).as_str());
                s.push('\n');
            }
        }
        if !self.recent_episodes.is_empty() {
            s.push_str("### Recent sessions\n");
            for m in &self.recent_episodes {
                s.push_str("- ");
                s.push_str(compact_line(&m.content, 200).as_str());
                s.push('\n');
            }
        }
        if !self.relevant.is_empty() {
            s.push_str("### Relevant memory\n");
            for m in &self.relevant {
                s.push_str("- ");
                s.push_str(compact_line(&m.content, 160).as_str());
                s.push('\n');
            }
        }
        s.push_str("</session_memory>");
        Some(s)
    }
}

fn compact_line(raw: &str, budget: usize) -> String {
    // First non-empty line, collapsed whitespace, capped to budget chars.
    let first = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or(raw);
    let normalized: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= budget {
        normalized
    } else {
        let mut out: String = normalized.chars().take(budget.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Outcome observed on a downstream tool / action that used a memory
/// surfaced by a prior recall. Maps to a Memoria feedback signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallObservedOutcome {
    /// The downstream action used the memory and succeeded.
    UsefulSuccess,
    /// The memory was surfaced but the agent visibly ignored it /
    /// reached the answer another way → signal `irrelevant`.
    IgnoredNoEffect,
    /// The agent followed the memory but the result contradicted
    /// it / the memory was out of date → `outdated`.
    Outdated,
    /// The memory gave objectively wrong guidance → `wrong`.
    Wrong,
}

impl RecallObservedOutcome {
    /// Map the observed outcome to a Memoria feedback `signal` string.
    pub fn signal(self) -> &'static str {
        match self {
            Self::UsefulSuccess => "useful",
            Self::IgnoredNoEffect => "irrelevant",
            Self::Outdated => "outdated",
            Self::Wrong => "wrong",
        }
    }
}

/// One entry in the "last recall" store — remembers which memory_ids
/// were most recently surfaced to the LLM so a later tool-outcome
/// observation can route feedback back to them.
#[derive(Debug, Clone)]
struct RecallLedgerEntry {
    memory_ids: Vec<String>,
    turn: u32,
    at: std::time::Instant,
}

/// The orchestrator itself. Cheap to construct — just an Arc'd trait
/// object — so callers instantiate one per session without worrying
/// about shared state. The recall-ledger is the one piece of interior
/// state: it remembers which memory_ids the last recall surfaced so
/// downstream tool outcomes can be routed back as feedback.
pub struct MemoryOrchestrator {
    client: Arc<dyn MemoriaClient>,
    /// Top-k used on session-start prefetch queries.
    prefetch_top_k: usize,
    /// Maximum episodes surfaced on session start.
    max_episodes: usize,
    /// Last recall per session — maps session_id → (memory_ids, turn,
    /// observed_at). Bounded by explicit `record_recall` calls; oldest
    /// entries are evicted on observation so a long-lived orchestrator
    /// doesn't leak memory.
    recall_ledger: std::sync::RwLock<std::collections::HashMap<String, RecallLedgerEntry>>,
}

impl MemoryOrchestrator {
    pub fn new(client: Arc<dyn MemoriaClient>) -> Self {
        Self {
            client,
            prefetch_top_k: 5,
            max_episodes: 3,
            recall_ledger: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_prefetch_top_k(mut self, top_k: usize) -> Self {
        self.prefetch_top_k = top_k.max(1);
        self
    }

    pub fn with_max_episodes(mut self, max: usize) -> Self {
        self.max_episodes = max.max(1);
        self
    }

    /// Pre-warm: on session start, gather profile + recent episodes +
    /// (optionally) query-relevant memories for the first user message.
    ///
    /// `first_user_message` may be `None` if the session was resumed
    /// without a fresh prompt (e.g. background worker task).
    pub async fn on_session_start(
        &self,
        _session_id: &str,
        first_user_message: Option<&str>,
    ) -> SessionStartMemories {
        let mut out = SessionStartMemories::default();

        // Profile — pulls most recent "profile" memories. We use a
        // broad query ("user profile") so the retrieval layer surfaces
        // whatever is tagged profile.
        match self
            .client
            .retrieve("user profile preferences role", None, 5)
            .await
        {
            Ok(mut memories) => {
                memories.retain(|m| m.memory_type == "profile");
                out.profile = memories;
            }
            Err(e) => tracing::debug!("session-start profile fetch failed: {e}"),
        }

        // Recent episodic summaries. Pull ≤ max_episodes across all
        // sessions (not session-scoped).
        match self
            .client
            .retrieve(
                "recent session episode summary",
                None,
                self.max_episodes * 2,
            )
            .await
        {
            Ok(mut memories) => {
                memories.retain(|m| m.memory_type == "episodic");
                memories.truncate(self.max_episodes);
                out.recent_episodes = memories;
            }
            Err(e) => tracing::debug!("session-start episode fetch failed: {e}"),
        }

        // Query-relevant memories for the first user message (cross-session).
        if let Some(msg) = first_user_message {
            let trimmed = msg.trim();
            if !trimmed.is_empty() {
                match self
                    .client
                    .retrieve(trimmed, None, self.prefetch_top_k)
                    .await
                {
                    Ok(memories) => {
                        out.relevant = memories;
                    }
                    Err(e) => tracing::debug!("session-start relevant fetch failed: {e}"),
                }
            }
        }

        out
    }

    /// Per-turn recall. Session-scoped if `strict_session=true`, else
    /// unscoped (prefer-mode default). Active focus hints are applied
    /// at the transport layer (see `HttpMemoriaClient::retrieve_ext`).
    pub async fn on_turn_recall(
        &self,
        query: &str,
        session_id: &str,
        top_k: usize,
        strict_session: bool,
    ) -> Vec<MemoriaMemory> {
        let sid = if session_id.is_empty() {
            None
        } else {
            Some(session_id)
        };
        match self
            .client
            .retrieve_ext(query, sid, top_k, strict_session)
            .await
        {
            Ok(memories) => memories,
            Err(e) => {
                tracing::debug!("turn recall failed for session {session_id}: {e}");
                Vec::new()
            }
        }
    }

    /// Set a focus hint for the current session. TTL defaults to 1h
    /// when omitted; boost defaults to 1.5×.
    pub async fn focus_on_topic(
        &self,
        session_id: &str,
        topic: &str,
        ttl_secs: Option<i64>,
    ) -> Result<(), String> {
        self.client
            .focus(session_id, "topic", topic, None, ttl_secs)
            .await
    }

    /// Record quality feedback on a specific memory. Callers MUST have
    /// the `memory_id` from a prior recall; the orchestrator does not
    /// track recall→feedback mapping itself.
    pub async fn feedback(
        &self,
        memory_id: &str,
        signal: &str,
        context: Option<&str>,
    ) -> Result<(), String> {
        self.client.feedback(memory_id, signal, context).await
    }

    /// Record the memory_ids surfaced to the LLM by a recall at a
    /// given turn. Later, [`observe_recall_outcome`] uses the ledger to
    /// route feedback back to those ids.
    ///
    /// Overwrites any prior entry for the session — only the **latest**
    /// recall can be scored against tool outcomes, because older
    /// recalls' memory_ids have probably already been acted on.
    pub fn record_recall(&self, session_id: &str, turn: u32, memories: &[MemoriaMemory]) {
        if session_id.is_empty() || memories.is_empty() {
            return;
        }
        let entry = RecallLedgerEntry {
            memory_ids: memories.iter().map(|m| m.memory_id.clone()).collect(),
            turn,
            at: std::time::Instant::now(),
        };
        if let Ok(mut g) = self.recall_ledger.write() {
            g.insert(session_id.to_string(), entry);
        }
    }

    /// Observe the outcome of a downstream action that followed a
    /// recall in this session. For each memory_id surfaced by the
    /// previous recall, records the mapped feedback signal to Memoria.
    /// The ledger entry is consumed (evicted) so a single recall is
    /// scored at most once.
    ///
    /// `max_age`: if provided, ignore recalls older than this — a
    /// stale ledger entry would attribute a new tool outcome to a
    /// recall the agent may have already moved past. Default (None)
    /// preserves full history.
    pub async fn observe_recall_outcome(
        &self,
        session_id: &str,
        outcome: RecallObservedOutcome,
        max_age: Option<std::time::Duration>,
    ) -> Vec<Result<(), String>> {
        let entry = {
            let Ok(mut g) = self.recall_ledger.write() else {
                return Vec::new();
            };
            let Some(entry) = g.remove(session_id) else {
                return Vec::new();
            };
            if let Some(max) = max_age {
                if entry.at.elapsed() > max {
                    return Vec::new();
                }
            }
            entry
        };
        let signal = outcome.signal();
        let mut results = Vec::with_capacity(entry.memory_ids.len());
        for id in &entry.memory_ids {
            let ctx = format!("auto: turn {} outcome", entry.turn);
            results.push(self.client.feedback(id, signal, Some(&ctx)).await);
        }
        results
    }

    /// Returns true iff there is an unconsumed recall ledger entry
    /// for this session (for introspection / tests).
    pub fn has_pending_recall(&self, session_id: &str) -> bool {
        self.recall_ledger
            .read()
            .ok()
            .is_some_and(|g| g.contains_key(session_id))
    }

    /// Given a rolling window of recent user messages (oldest first),
    /// decide whether a topic has "stuck" long enough to warrant an
    /// auto-focus. Returns `Some(topic)` when the same token appears
    /// in ≥ `min_streak` consecutive messages, after basic stop-word
    /// filtering. Callers then fire [`focus_on_topic`].
    ///
    /// Pure function — the orchestrator does not hold turn history
    /// state; callers are responsible for maintaining the window.
    pub fn detect_topic_for_auto_focus(
        recent_user_messages: &[String],
        min_streak: usize,
    ) -> Option<String> {
        if recent_user_messages.len() < min_streak || min_streak < 2 {
            return None;
        }
        // Tokens seen in the most recent turn; we then check each
        // earlier-in-window turn for overlap.
        let window_start = recent_user_messages.len().saturating_sub(min_streak);
        let tail = &recent_user_messages[window_start..];
        let mut streak: Option<String> = None;
        for candidate in salient_tokens(&tail[tail.len() - 1]) {
            if tail
                .iter()
                .all(|msg| salient_tokens(msg).iter().any(|t| t == &candidate))
            {
                streak = Some(candidate);
                break;
            }
        }
        streak
    }
}

/// Extract salient tokens from a user message. Lowercase, alphanum
/// plus underscore, minimum 3 chars, stop-word filtered. Deliberately
/// small scope so the heuristic stays predictable (no stemming, no
/// camelCase splitting, no Unicode classes beyond ASCII).
fn salient_tokens(msg: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "this", "that", "have", "has", "are", "was", "can", "could",
        "should", "would", "please", "just", "some", "about", "from", "you", "your", "our", "what",
        "when", "where", "which", "how", "why", "but", "not", "all", "any", "one", "two", "into",
        "out", "over", "under", "also", "again", "already", "now", "then", "there", "they", "them",
        "she", "him", "her", "his", "hers",
    ];
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let lower = msg.to_lowercase();
    for raw in lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let t = raw.trim_matches('_');
        if t.len() < 3 {
            continue;
        }
        if STOP.contains(&t) {
            continue;
        }
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, ty: &str, content: &str) -> MemoriaMemory {
        MemoriaMemory {
            memory_id: id.into(),
            memory_type: ty.into(),
            content: content.into(),
            retrieval_score: Some(0.8),
        }
    }

    #[test]
    fn session_start_empty_renders_none() {
        let m = SessionStartMemories::default();
        assert!(m.is_empty());
        assert!(m.to_prompt_block().is_none());
    }

    #[test]
    fn session_start_block_has_the_three_sections_when_populated() {
        let m = SessionStartMemories {
            profile: vec![memory("p1", "profile", "User prefers Rust.")],
            recent_episodes: vec![memory(
                "e1",
                "episodic",
                "[episode] turn=5, finished auth refactor.",
            )],
            relevant: vec![memory(
                "r1",
                "semantic",
                "[project] milestone M3 ships 2026-06-01.",
            )],
        };
        let block = m.to_prompt_block().expect("non-empty");
        assert!(block.starts_with("<session_memory>"));
        assert!(block.ends_with("</session_memory>"));
        assert!(block.contains("### User profile"));
        assert!(block.contains("### Recent sessions"));
        assert!(block.contains("### Relevant memory"));
        assert!(block.contains("User prefers Rust."));
        assert!(block.contains("auth refactor"));
        assert!(block.contains("milestone M3"));
    }

    #[test]
    fn compact_line_respects_budget() {
        let long = "a".repeat(500);
        let out = compact_line(&long, 120);
        // Budget includes the ellipsis char.
        assert!(out.chars().count() <= 120);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn compact_line_collapses_whitespace() {
        let text = "first line\n  second line\n   third line";
        assert_eq!(compact_line(text, 120), "first line");
    }

    // ── Auto-focus heuristic ───────────────────────────────────────

    #[test]
    fn auto_focus_detects_streak_of_three() {
        let window = vec![
            "let's debug auth middleware".to_string(),
            "the auth flow rejects valid tokens".to_string(),
            "still seeing auth failures on prod".to_string(),
        ];
        assert_eq!(
            MemoryOrchestrator::detect_topic_for_auto_focus(&window, 3),
            Some("auth".to_string())
        );
    }

    #[test]
    fn auto_focus_returns_none_below_streak() {
        let window = vec![
            "check the compaction logic".to_string(),
            "debug auth middleware".to_string(),
        ];
        // 3-streak requested, only 2 messages → None.
        assert_eq!(
            MemoryOrchestrator::detect_topic_for_auto_focus(&window, 3),
            None
        );
    }

    #[test]
    fn auto_focus_none_when_topic_differs_between_turns() {
        let window = vec![
            "debug auth middleware".to_string(),
            "what's the compaction budget".to_string(),
            "grep for tokens in tests".to_string(),
        ];
        assert_eq!(
            MemoryOrchestrator::detect_topic_for_auto_focus(&window, 3),
            None
        );
    }

    #[test]
    fn auto_focus_skips_stop_words() {
        // "the" appears in every turn but is a stop word → no match.
        let window = vec![
            "the system is broken".to_string(),
            "the log shows failure".to_string(),
            "the test hangs".to_string(),
        ];
        assert_eq!(
            MemoryOrchestrator::detect_topic_for_auto_focus(&window, 3),
            None
        );
    }

    #[test]
    fn auto_focus_handles_min_streak_zero_or_one_safely() {
        let window = vec!["anything".to_string()];
        // min_streak < 2 is rejected — we require at least 2 signals.
        assert_eq!(
            MemoryOrchestrator::detect_topic_for_auto_focus(&window, 1),
            None
        );
        assert_eq!(
            MemoryOrchestrator::detect_topic_for_auto_focus(&window, 0),
            None
        );
    }

    // ── Recall ledger + feedback loop ──────────────────────────────

    use std::sync::Mutex;

    struct FeedbackCapturingClient {
        feedback_calls: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl FeedbackCapturingClient {
        fn new() -> Self {
            Self {
                feedback_calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl MemoriaClient for FeedbackCapturingClient {
        async fn retrieve_ext(
            &self,
            _q: &str,
            _sid: Option<&str>,
            _k: usize,
            _fs: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(vec![])
        }
        async fn store(
            &self,
            _c: &str,
            _t: &str,
            _s: Option<&str>,
            _tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("m".into())
        }
        async fn purge_working(&self, _s: &str) -> Result<u64, String> {
            Ok(0)
        }
        async fn feedback(
            &self,
            memory_id: &str,
            signal: &str,
            context: Option<&str>,
        ) -> Result<(), String> {
            self.feedback_calls.lock().unwrap().push((
                memory_id.to_string(),
                signal.to_string(),
                context.map(String::from),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn record_recall_then_observe_fires_feedback_for_each_id() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());
        let memories = vec![
            memory("m1", "semantic", "first"),
            memory("m2", "semantic", "second"),
        ];
        orch.record_recall("sess1", 3, &memories);
        assert!(orch.has_pending_recall("sess1"));
        let results = orch
            .observe_recall_outcome("sess1", RecallObservedOutcome::UsefulSuccess, None)
            .await;
        assert_eq!(results.len(), 2, "one feedback call per memory_id");
        let calls = client.feedback_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "m1");
        assert_eq!(calls[0].1, "useful");
        assert_eq!(calls[1].0, "m2");
        assert!(!orch.has_pending_recall("sess1"), "ledger consumed");
    }

    #[tokio::test]
    async fn observe_without_prior_recall_is_noop() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());
        let results = orch
            .observe_recall_outcome("sess_no_prior", RecallObservedOutcome::Wrong, None)
            .await;
        assert!(results.is_empty());
        assert!(client.feedback_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn outcome_maps_to_correct_signal() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());

        let cases = [
            (RecallObservedOutcome::UsefulSuccess, "useful"),
            (RecallObservedOutcome::IgnoredNoEffect, "irrelevant"),
            (RecallObservedOutcome::Outdated, "outdated"),
            (RecallObservedOutcome::Wrong, "wrong"),
        ];
        for (outcome, expected) in cases {
            orch.record_recall("sess_map", 1, &[memory("m", "semantic", "x")]);
            let _ = orch.observe_recall_outcome("sess_map", outcome, None).await;
            let last = client
                .feedback_calls
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap();
            assert_eq!(
                last.1, expected,
                "outcome={:?} must map to {expected}",
                outcome
            );
        }
    }

    #[tokio::test]
    async fn stale_ledger_is_ignored_when_max_age_elapsed() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());
        orch.record_recall("sess_stale", 1, &[memory("m", "semantic", "x")]);
        // Brief sleep so the entry is older than max_age.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let results = orch
            .observe_recall_outcome(
                "sess_stale",
                RecallObservedOutcome::UsefulSuccess,
                Some(std::time::Duration::from_millis(1)),
            )
            .await;
        assert!(results.is_empty());
        assert!(client.feedback_calls.lock().unwrap().is_empty());
    }
}
