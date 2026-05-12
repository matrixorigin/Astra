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
                s.push_str(&m.freshness_suffix());
                s.push('\n');
            }
        }
        if !self.recent_episodes.is_empty() {
            s.push_str("### Recent sessions\n");
            for m in &self.recent_episodes {
                s.push_str("- ");
                s.push_str(compact_line(&m.content, 200).as_str());
                s.push_str(&m.freshness_suffix());
                s.push('\n');
            }
        }
        if !self.relevant.is_empty() {
            s.push_str("### Relevant memory\n");
            for m in &self.relevant {
                s.push_str("- ");
                s.push_str(compact_line(&m.content, 160).as_str());
                s.push_str(&m.freshness_suffix());
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

/// One entry in the recall ledger — remembers which memory_ids
/// were surfaced to the LLM at a given turn so a later tool-outcome
/// observation can route feedback back to them.
#[derive(Debug, Clone)]
struct RecallLedgerEntry {
    memory_ids: Vec<String>,
    turn: u32,
    at: std::time::Instant,
}

/// Soft cap on recall-ledger depth per session. When an LLM probes
/// repeatedly without outcomes closing the loop we discard the oldest
/// entry so the ledger doesn't grow unbounded. Tuned conservatively —
/// a typical turn produces at most 1–2 recalls, so 16 entries covers
/// ~10+ turns of accumulation.
const MAX_RECALL_LEDGER_PER_SESSION: usize = 16;

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
    /// Per-session FIFO queue of recalls awaiting outcome attribution.
    /// A single turn can legitimately fire multiple recalls (the LLM
    /// may probe for different topics before acting); each sits in the
    /// queue until [`observe_recall_outcome`] consumes it. Bounded
    /// softly by [`MAX_RECALL_LEDGER_PER_SESSION`] — when the cap is
    /// reached the oldest entry is dropped so we don't leak memory on
    /// sessions whose LLMs never let us close the loop.
    recall_ledger: std::sync::RwLock<
        std::collections::HashMap<String, std::collections::VecDeque<RecallLedgerEntry>>,
    >,
    /// "Already surfaced to the LLM this session" — dedup set of
    /// memory_ids that have appeared in a `<session_memory>` block
    /// or a recall result. `filter_already_seen` applies this to
    /// trim repeat hits from per-turn recall so the LLM doesn't see
    /// the same abstract five turns in a row.
    seen_memory_ids:
        std::sync::RwLock<std::collections::HashMap<String, std::collections::HashSet<String>>>,
}

impl MemoryOrchestrator {
    pub fn new(client: Arc<dyn MemoriaClient>) -> Self {
        Self {
            client,
            prefetch_top_k: 5,
            max_episodes: 3,
            recall_ledger: std::sync::RwLock::new(std::collections::HashMap::new()),
            seen_memory_ids: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Mark a set of memory_ids as already surfaced to the LLM in this
    /// session so future recalls can filter them out. Callers invoke
    /// this after injecting a memory block into the prompt.
    pub fn mark_surfaced(&self, session_id: &str, memory_ids: &[String]) {
        if memory_ids.is_empty() {
            return;
        }
        let Ok(mut g) = self.seen_memory_ids.write() else {
            return;
        };
        let bucket = g.entry(session_id.to_string()).or_default();
        for id in memory_ids {
            if !id.is_empty() {
                bucket.insert(id.clone());
            }
        }
    }

    /// Filter a candidate memory list down to ones not yet surfaced
    /// this session. Returns a new Vec in the same order, minus
    /// already-seen memory_ids. Read-only; does not mark anything.
    pub fn filter_already_surfaced(
        &self,
        session_id: &str,
        memories: Vec<MemoriaMemory>,
    ) -> Vec<MemoriaMemory> {
        let Ok(g) = self.seen_memory_ids.read() else {
            return memories;
        };
        let Some(seen) = g.get(session_id) else {
            return memories;
        };
        memories
            .into_iter()
            .filter(|m| !seen.contains(&m.memory_id))
            .collect()
    }

    /// Clear the "already surfaced" set for a session. Intended for
    /// session-end cleanup so long-lived orchestrator processes don't
    /// keep per-session state indefinitely.
    pub fn reset_session_surface(&self, session_id: &str) {
        if let Ok(mut g) = self.seen_memory_ids.write() {
            g.remove(session_id);
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

    /// Build a compact `<memory_index>` block — one line per known
    /// memory (`- [type] memory_id: one-line abstract`). Intended for
    /// always-injection into the session-stable prompt lane so the LLM
    /// has ambient awareness of what it *could* recall, without the
    /// token cost of the full content.
    ///
    /// `limit` caps the index size (default 80 entries ≈ ~1200 tokens).
    /// Returns `None` when nothing to index.
    pub async fn build_memory_index(&self, user_id: Option<&str>, limit: usize) -> Option<String> {
        // We reuse `retrieve` with a broad query — Memoria's ranker
        // surfaces the "most alive" memories first (recency +
        // importance + access count). We then strip to the compact
        // abstract + id form.
        //
        // Bailing on empty query is fine: `retrieve` treats it as
        // "browse mode" and returns the most-recently-written items.
        let all = self.client.retrieve("", user_id, limit.max(1)).await.ok()?;
        if all.is_empty() {
            return None;
        }
        let mut s = String::with_capacity(512);
        s.push_str("<memory_index>\n");
        s.push_str(
            "(Ambient awareness — compact list of what you could recall. \
            Call `memory(action=recall, query=X)` to pull the full content of a specific \
            topic, or `memory(action=expand, memory_id=ID)` to drill into one entry.)\n",
        );
        for m in all.iter().take(limit) {
            let abstract_line = compact_line(&m.content, 100);
            let tag = if m.memory_type.is_empty() {
                "?".to_string()
            } else {
                m.memory_type.clone()
            };
            let suffix = m.freshness_suffix();
            s.push_str(&format!(
                "- [{tag}] {}: {abstract_line}{suffix}\n",
                m.memory_id
            ));
        }
        s.push_str("</memory_index>");
        Some(s)
    }

    /// Detect potentially conflicting memories before persisting a new
    /// one. Returns the set of similar existing memories (by
    /// retrieval-score rank) whose content overlaps `new_content`
    /// enough that the caller should probably `update` rather than
    /// `remember`.
    ///
    /// Cheap: one top-k recall. The caller decides policy — either
    /// ask the LLM ("update or create?") or hand the ids to a
    /// downstream `update` call.
    pub async fn detect_conflicts(
        &self,
        new_content: &str,
        session_id: Option<&str>,
        similarity_floor: f64,
    ) -> Vec<MemoriaMemory> {
        if new_content.trim().is_empty() {
            return Vec::new();
        }
        let candidates = match self.client.retrieve(new_content, session_id, 5).await {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        // Keep entries above the similarity floor (server-side `final_score`
        // is already hybrid + tier-weighted; we trust it).
        candidates
            .into_iter()
            .filter(|m| m.retrieval_score.unwrap_or(0.0) >= similarity_floor)
            .collect()
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
    /// given turn. Appends to the session's FIFO queue;
    /// [`observe_recall_outcome`] drains the queue oldest-first so the
    /// same memory_id is scored at most once per recall.
    ///
    /// Multiple recalls in the same turn are each retained — the prior
    /// "latest only" policy lost attribution when the LLM probed twice
    /// before acting. When the per-session queue exceeds
    /// [`MAX_RECALL_LEDGER_PER_SESSION`] the oldest entry is dropped.
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
            let q = g.entry(session_id.to_string()).or_default();
            if q.len() >= MAX_RECALL_LEDGER_PER_SESSION {
                q.pop_front();
            }
            q.push_back(entry);
        }
    }

    /// Observe the outcome of a downstream action that followed recalls
    /// in this session. Drains the session's recall queue (FIFO) and
    /// routes the mapped feedback signal to Memoria for every
    /// memory_id in every drained entry. Entries are evicted so each
    /// recall is scored at most once.
    ///
    /// `max_age`: if provided, entries older than this are dropped
    /// *without* scoring (we can no longer attribute reliably). Entries
    /// within the window are scored in order.
    pub async fn observe_recall_outcome(
        &self,
        session_id: &str,
        outcome: RecallObservedOutcome,
        max_age: Option<std::time::Duration>,
    ) -> Vec<Result<(), String>> {
        let entries: Vec<RecallLedgerEntry> = {
            let Ok(mut g) = self.recall_ledger.write() else {
                return Vec::new();
            };
            let Some(q) = g.remove(session_id) else {
                return Vec::new();
            };
            q.into_iter()
                .filter(|e| match max_age {
                    Some(max) => e.at.elapsed() <= max,
                    None => true,
                })
                .collect()
        };
        let signal = outcome.signal();
        let mut results = Vec::new();
        for entry in entries {
            for id in &entry.memory_ids {
                let ctx = format!("auto: turn {} outcome", entry.turn);
                results.push(self.client.feedback(id, signal, Some(&ctx)).await);
            }
        }
        results
    }

    /// Returns true iff there is an unconsumed recall ledger entry
    /// for this session (for introspection / tests).
    pub fn has_pending_recall(&self, session_id: &str) -> bool {
        self.recall_ledger
            .read()
            .ok()
            .is_some_and(|g| g.get(session_id).is_some_and(|q| !q.is_empty()))
    }

    /// Count of unconsumed recall ledger entries for this session.
    /// Exposed so tests can assert FIFO depth without reaching into
    /// private fields.
    pub fn pending_recall_count(&self, session_id: &str) -> usize {
        self.recall_ledger
            .read()
            .ok()
            .and_then(|g| g.get(session_id).map(|q| q.len()))
            .unwrap_or(0)
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
            ..Default::default()
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
        /// Optional canned retrieve response used by index / conflict tests.
        retrieve_response: Mutex<Vec<MemoriaMemory>>,
    }

    impl FeedbackCapturingClient {
        fn new() -> Self {
            Self {
                feedback_calls: Mutex::new(Vec::new()),
                retrieve_response: Mutex::new(Vec::new()),
            }
        }

        fn with_retrieve_response(self, memories: Vec<MemoriaMemory>) -> Self {
            *self.retrieve_response.lock().unwrap() = memories;
            self
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
            Ok(self.retrieve_response.lock().unwrap().clone())
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

    // ── R4: multi-entry recall ledger (FIFO) ───────────────────────

    #[tokio::test]
    async fn record_recall_keeps_multiple_entries_in_same_turn() {
        // The LLM probes twice before acting. Both recalls must be
        // preserved — the prior single-entry ledger would drop the
        // first one silently.
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());
        orch.record_recall("sess_multi", 1, &[memory("m1", "semantic", "a")]);
        orch.record_recall("sess_multi", 1, &[memory("m2", "semantic", "b")]);
        assert_eq!(orch.pending_recall_count("sess_multi"), 2);

        let results = orch
            .observe_recall_outcome("sess_multi", RecallObservedOutcome::UsefulSuccess, None)
            .await;
        assert_eq!(results.len(), 2, "both recalls must be scored");

        let calls = client.feedback_calls.lock().unwrap().clone();
        let ids: Vec<&str> = calls.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2"], "FIFO — first recall scored first");
    }

    #[tokio::test]
    async fn record_recall_caps_queue_at_soft_limit() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        // Blast past the cap; oldest must evict.
        for i in 0..(MAX_RECALL_LEDGER_PER_SESSION + 5) {
            let id = format!("m{i}");
            orch.record_recall("sess_cap", i as u32, &[memory(&id, "semantic", "x")]);
        }
        assert_eq!(
            orch.pending_recall_count("sess_cap"),
            MAX_RECALL_LEDGER_PER_SESSION,
            "queue depth must cap at MAX_RECALL_LEDGER_PER_SESSION"
        );
    }

    #[tokio::test]
    async fn observe_drains_all_entries_fifo_order() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());
        orch.record_recall("sess_fifo", 1, &[memory("first", "semantic", "a")]);
        orch.record_recall("sess_fifo", 2, &[memory("second", "semantic", "b")]);
        orch.record_recall("sess_fifo", 3, &[memory("third", "semantic", "c")]);

        let _ = orch
            .observe_recall_outcome("sess_fifo", RecallObservedOutcome::UsefulSuccess, None)
            .await;

        let calls = client.feedback_calls.lock().unwrap().clone();
        let ids: Vec<&str> = calls.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"], "strict FIFO");
        assert_eq!(orch.pending_recall_count("sess_fifo"), 0);
    }

    #[tokio::test]
    async fn observe_with_max_age_drops_stale_entries_but_keeps_fresh() {
        // Mixed queue: two old entries + one fresh. max_age filters
        // only the old ones; fresh one still scores.
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client.clone());
        orch.record_recall("sess_mix", 1, &[memory("old1", "semantic", "a")]);
        orch.record_recall("sess_mix", 2, &[memory("old2", "semantic", "b")]);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        orch.record_recall("sess_mix", 3, &[memory("fresh", "semantic", "c")]);
        // old1 + old2 are now ≥30ms; fresh < 30ms.
        let _ = orch
            .observe_recall_outcome(
                "sess_mix",
                RecallObservedOutcome::UsefulSuccess,
                Some(std::time::Duration::from_millis(10)),
            )
            .await;
        let calls = client.feedback_calls.lock().unwrap().clone();
        let ids: Vec<&str> = calls.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["fresh"], "only the fresh entry scores");
    }

    // ── "Already surfaced" dedup ───────────────────────────────────

    #[test]
    fn mark_surfaced_then_filter_removes_those_ids() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        orch.mark_surfaced("sess", &["a".into(), "b".into()]);
        let candidates = vec![
            memory("a", "semantic", "alpha"),
            memory("b", "semantic", "beta"),
            memory("c", "semantic", "gamma"),
        ];
        let filtered = orch.filter_already_surfaced("sess", candidates);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].memory_id, "c");
    }

    #[test]
    fn filter_passes_everything_when_nothing_seen() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        let candidates = vec![memory("a", "semantic", "x")];
        assert_eq!(orch.filter_already_surfaced("sess", candidates).len(), 1);
    }

    #[test]
    fn reset_session_surface_clears_dedup_set() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        orch.mark_surfaced("sess", &["a".into()]);
        orch.reset_session_surface("sess");
        let filtered = orch.filter_already_surfaced("sess", vec![memory("a", "semantic", "x")]);
        assert_eq!(filtered.len(), 1, "after reset, ids flow through again");
    }

    #[test]
    fn mark_surfaced_is_per_session() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        orch.mark_surfaced("sess1", &["a".into()]);
        let kept = orch.filter_already_surfaced("sess2", vec![memory("a", "semantic", "x")]);
        assert_eq!(kept.len(), 1, "session boundaries isolate the dedup set");
    }

    // ── Prompt block carries freshness suffix for old memories ─────

    #[test]
    fn session_start_block_renders_bucketed_freshness_for_recent_memory() {
        let mut m = memory("m1", "profile", "user is a data scientist");
        // 10 days ago, T1 tier (365-day half-life) → "within the year" bucket.
        let ts = chrono::Utc::now() - chrono::Duration::days(10);
        m.observed_at = Some(ts.to_rfc3339());
        m.trust_tier = Some("T1".into());
        let bundle = SessionStartMemories {
            profile: vec![m],
            ..Default::default()
        };
        let block = bundle.to_prompt_block().expect("non-empty");
        assert!(
            block.contains("within the year"),
            "bucket label missing: {block}"
        );
        assert!(!block.contains("stale"));
    }

    #[test]
    fn session_start_block_marks_stale_past_half_life() {
        let mut m = memory("m1", "episodic", "[episode] last session fix");
        // 120 days ago, default T3 tier (60-day half-life) → stale.
        let ts = chrono::Utc::now() - chrono::Duration::days(120);
        m.observed_at = Some(ts.to_rfc3339());
        // No trust_tier → defaults to T3 treatment in the formatter.
        let bundle = SessionStartMemories {
            recent_episodes: vec![m],
            ..Default::default()
        };
        let block = bundle.to_prompt_block().expect("non-empty");
        assert!(
            block.contains("stale — verify first"),
            "stale bucket missing past half-life: {block}"
        );
    }

    // ── Memory index ───────────────────────────────────────────────

    #[tokio::test]
    async fn memory_index_returns_none_on_empty_store() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        assert!(orch.build_memory_index(Some("u1"), 50).await.is_none());
    }

    #[tokio::test]
    async fn memory_index_renders_one_line_per_entry() {
        let mems = vec![
            memory("m1", "profile", "user prefers Rust"),
            memory("m2", "feedback", "use real DB in tests"),
            memory("m3", "project", "merge freeze 2026-05-08"),
        ];
        let client =
            std::sync::Arc::new(FeedbackCapturingClient::new().with_retrieve_response(mems));
        let orch = MemoryOrchestrator::new(client);
        let idx = orch
            .build_memory_index(Some("u1"), 50)
            .await
            .expect("non-empty");
        assert!(idx.starts_with("<memory_index>"));
        assert!(idx.ends_with("</memory_index>"));
        assert!(idx.contains("- [profile] m1: user prefers Rust"));
        assert!(idx.contains("- [feedback] m2: use real DB in tests"));
        assert!(idx.contains("- [project] m3: merge freeze 2026-05-08"));
    }

    #[tokio::test]
    async fn memory_index_respects_limit() {
        let mems = (0..10)
            .map(|i| memory(&format!("m{i}"), "semantic", "x"))
            .collect();
        let client =
            std::sync::Arc::new(FeedbackCapturingClient::new().with_retrieve_response(mems));
        let orch = MemoryOrchestrator::new(client);
        let idx = orch
            .build_memory_index(Some("u1"), 3)
            .await
            .expect("non-empty");
        let lines = idx.lines().filter(|l| l.starts_with("- [")).count();
        assert_eq!(lines, 3);
    }

    // ── Conflict detection ─────────────────────────────────────────

    #[tokio::test]
    async fn detect_conflicts_returns_high_similarity_matches() {
        let candidate_a = {
            let mut m = memory("a", "feedback", "use real DB in tests");
            m.retrieval_score = Some(0.92);
            m
        };
        let candidate_b = {
            let mut m = memory("b", "feedback", "prefer pnpm over npm");
            m.retrieval_score = Some(0.41);
            m
        };
        let client = std::sync::Arc::new(
            FeedbackCapturingClient::new().with_retrieve_response(vec![candidate_a, candidate_b]),
        );
        let orch = MemoryOrchestrator::new(client);
        let hits = orch
            .detect_conflicts("always use a real database in tests", None, 0.85)
            .await;
        assert_eq!(hits.len(), 1, "only the high-similarity hit should surface");
        assert_eq!(hits[0].memory_id, "a");
    }

    #[tokio::test]
    async fn detect_conflicts_empty_new_content_returns_nothing() {
        let client = std::sync::Arc::new(FeedbackCapturingClient::new());
        let orch = MemoryOrchestrator::new(client);
        assert!(orch.detect_conflicts("   ", None, 0.8).await.is_empty());
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
