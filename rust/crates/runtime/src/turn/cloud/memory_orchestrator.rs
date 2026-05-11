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

/// The orchestrator itself. Cheap to construct — just an Arc'd trait
/// object — so callers instantiate one per session without worrying
/// about shared state.
pub struct MemoryOrchestrator {
    client: Arc<dyn MemoriaClient>,
    /// Top-k used on session-start prefetch queries.
    prefetch_top_k: usize,
    /// Maximum episodes surfaced on session start.
    max_episodes: usize,
}

impl MemoryOrchestrator {
    pub fn new(client: Arc<dyn MemoriaClient>) -> Self {
        Self {
            client,
            prefetch_top_k: 5,
            max_episodes: 3,
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
}
