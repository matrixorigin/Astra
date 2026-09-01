//! Memory prefetch utilities for LLM prompt augmentation.
//!
//! Provides one semantic retrieval of the complete user message from Memoria,
//! admitting only typed results into the system prompt.
//!
//! Ranking: Memoria's server-side `final_score` already tier-weights
//! via confidence decay (half-life T1=365d … T4=30d) — we trust it and
//! only re-sort the returned results by that score so the output
//! is deterministic. No client-side tier multiplier: doing one here
//! would double-count what the server already did.

use std::time::{Duration, Instant};

use astra_turn_core::context_sources::MemoryEntry as ContextMemoryEntry;
use astra_turn_types::RankableMemory;

use super::cloud::memoria_compact::{MemoriaMemory, MemoriaPort};

/// Prompt recall is optional context, never an admission dependency for a
/// model turn.  A single unhealthy memory replica must not consume the
/// interactive first-response budget (the transport-level Memoria timeout is
/// deliberately much longer because persistence and maintenance operations
/// have different latency semantics).
pub(crate) const INTERACTIVE_MEMORY_READ_DEADLINE: Duration = Duration::from_millis(750);

/// Result of a memory prefetch operation.
#[derive(Debug, Default)]
pub struct MemoryPrefetchResult {
    pub entries: Vec<ContextMemoryEntry>,
    pub items: usize,
    pub preview: Vec<String>,
    pub fetch_ms: i64,
    pub outcome: astra_turn_types::MemoryRetrievalOutcome,
}

#[derive(Default)]
struct RankableRetrieval {
    memories: Vec<RankableMemory>,
    outcome: astra_turn_types::MemoryRetrievalOutcome,
}

/// Prefetch memories relevant to the complete user message.
///
/// Memoria owns semantic retrieval. Deriving a second query by splitting ASCII
/// words here both doubled interactive load and behaved inconsistently across
/// languages; it was not a durable entity boundary.
/// Provider-neutral prompt recall. All deployment forms route through this
/// function; only the `MemoriaPort` implementation differs.
pub async fn prefetch_memories_with_client(
    client: &dyn MemoriaPort,
    user_msg: &str,
    user_id: &str,
    session_id: &str,
    top_k: u32,
) -> MemoryPrefetchResult {
    if user_msg.trim().is_empty() {
        return MemoryPrefetchResult::default();
    }
    let started = Instant::now();
    let trimmed_msg = user_msg.trim();
    let result = retrieve_rankable(client, trimmed_msg, user_id, session_id, top_k).await;
    let outcome = result.outcome;
    let mut merged_records = merge_structured_results(result.memories, Vec::new());
    astra_turn_types::sort_by_retrieval_score(&mut merged_records);
    let ranked_cap = (top_k as usize).saturating_mul(2).max(top_k as usize);
    merged_records.truncate(ranked_cap);

    // Only the typed protocol crosses into prompt assembly. Malformed or
    // unstructured rows remain observable in Memoria but cannot become
    // prompt context through a heuristic text fallback. Session snapshots
    // use their dedicated current-session lane.
    let entries: Vec<ContextMemoryEntry> = merged_records
        .iter()
        .filter_map(context_entry_from_rankable)
        .collect();
    let fetch_ms = started.elapsed().as_millis() as i64;
    let preview = entries
        .iter()
        .take(3)
        .map(|entry| entry.content.clone())
        .collect();
    let items = entries.len();
    MemoryPrefetchResult {
        entries,
        items,
        preview,
        fetch_ms,
        outcome,
    }
}

async fn retrieve_rankable(
    client: &dyn MemoriaPort,
    query: &str,
    user_id: &str,
    session_id: &str,
    top_k: u32,
) -> RankableRetrieval {
    let started = Instant::now();
    let retrieval = tokio::time::timeout(
        INTERACTIVE_MEMORY_READ_DEADLINE,
        client.retrieve_for_prompt(query, user_id, session_id, top_k as usize),
    )
    .await;
    match retrieval {
        Ok(Ok(memories)) => RankableRetrieval {
            memories: memories.into_iter().map(rankable_from_memoria).collect(),
            outcome: astra_turn_types::MemoryRetrievalOutcome::Complete,
        },
        Ok(Err(error)) => {
            tracing::warn!(
                target: "astra_runtime::memory_prefetch",
                error = %error,
                "prompt memory recall degraded; continuing without this retrieval result"
            );
            RankableRetrieval {
                memories: Vec::new(),
                outcome: astra_turn_types::MemoryRetrievalOutcome::Unavailable,
            }
        }
        Err(_) => {
            tracing::warn!(
                target: "astra_runtime::memory_prefetch",
                deadline_ms = INTERACTIVE_MEMORY_READ_DEADLINE.as_millis(),
                elapsed_ms = started.elapsed().as_millis(),
                "prompt memory recall exceeded its interactive budget; continuing without this retrieval result"
            );
            RankableRetrieval {
                memories: Vec::new(),
                outcome: astra_turn_types::MemoryRetrievalOutcome::Unavailable,
            }
        }
    }
}

fn rankable_from_memoria(memory: MemoriaMemory) -> RankableMemory {
    RankableMemory {
        memory_id: memory.memory_id,
        content: memory.content,
        memory_type: memory.memory_type,
        retrieval_score: memory.retrieval_score,
        trust_tier: memory.trust_tier,
        observed_at: memory.observed_at,
        updated_at: memory.updated_at,
        session_id: memory.session_id,
    }
}

/// Typed first-turn recall used to establish cross-session continuity.
/// It uses the same dynamic Memory lane as per-turn recall; no rendered
/// memory block enters the cacheable prompt prefix.
#[derive(Debug, Default)]
pub struct SessionStartPrefetchResult {
    pub entries: Vec<ContextMemoryEntry>,
    pub fetch_ms: i64,
    pub outcome: astra_turn_types::MemoryRetrievalOutcome,
}

/// Prefetch session-start memories: profile + recent episodes. Intended
/// to run **only on the first turn** of a session — the caller decides
/// based on `turn_number`. On non-first turns we rely on
/// [`prefetch_memories_with_client`] (per-turn semantic recall) to surface relevant
/// memory.
///
/// Both fetches run in parallel; any fetch failure is treated as "no
/// memory" so a degraded Memoria never blocks the turn.
pub async fn prefetch_session_start_memories_with_client(
    client: &dyn MemoriaPort,
    user_id: &str,
    session_id: &str,
) -> SessionStartPrefetchResult {
    let started = Instant::now();

    // Two structured queries in parallel:
    //   1. `profile` — broad query to surface user-identity memories.
    //      Kept only when the typed protocol agrees with `profile`.
    //   2. `episodic` — query biased toward recent session summaries.
    //      Kept only when the typed protocol agrees with `episodic`.
    // Recurring semantic scenes are left to query-relevant per-turn recall;
    // a content-prefix classifier is not a durable type boundary.
    const PROFILE_TOP_K: u32 = 5;
    const EPISODE_TOP_K: u32 = 6;
    let (profile_raw, episode_raw) = tokio::join!(
        retrieve_rankable(
            client,
            "user profile preferences role",
            user_id,
            session_id,
            PROFILE_TOP_K,
        ),
        retrieve_rankable(
            client,
            "recent session episode summary",
            user_id,
            session_id,
            EPISODE_TOP_K,
        ),
    );

    let outcome = profile_raw.outcome.combine(episode_raw.outcome);
    let mut profile: Vec<RankableMemory> = profile_raw
        .memories
        .into_iter()
        .filter(|m| m.memory_type == "profile")
        .collect();
    let mut recent_episodes: Vec<RankableMemory> = episode_raw
        .memories
        .into_iter()
        .filter(|m| m.memory_type == "episodic")
        .collect();
    // Sort each typed bucket by server-side retrieval score and cap.
    astra_turn_types::sort_by_retrieval_score(&mut profile);
    astra_turn_types::sort_by_retrieval_score(&mut recent_episodes);
    profile.truncate(3);
    recent_episodes.truncate(3);
    let entries = merge_structured_results(profile, recent_episodes)
        .iter()
        .filter_map(context_entry_from_rankable)
        .collect();
    SessionStartPrefetchResult {
        entries,
        fetch_ms: started.elapsed().as_millis() as i64,
        outcome,
    }
}

/// Deduplicate one retrieval payload and record surfaced identities for later
/// quality feedback. The ledger is observability, not an admission gate:
/// another LLM consumer (a later tool round or subrun) must receive the same
/// relevant evidence even when a sibling consumer already saw it.
pub fn admit_prompt_memory_entries(
    session_id: &str,
    entries: Vec<ContextMemoryEntry>,
) -> Vec<ContextMemoryEntry> {
    if entries.is_empty() {
        return entries;
    }
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_contents = std::collections::HashSet::new();
    let admitted = entries
        .into_iter()
        .filter(|entry| {
            let id_unique = entry
                .memory_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .is_none_or(|id| seen_ids.insert(id.to_string()));
            let content_unique = seen_contents.insert(memory_dedup_key(entry.content.trim()));
            id_unique && content_unique
        })
        .collect::<Vec<_>>();
    let new_keys = collect_seen_contents(&admitted);
    if !new_keys.is_empty() {
        astra_tools::memoria::MemoriaToolGateway::record_seen(session_id, new_keys);
    }
    admitted
}

/// Extract backend identities and normalized compact-content keys for the
/// shared surfaced-memory ledger.
pub fn collect_seen_contents(entries: &[ContextMemoryEntry]) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for entry in entries {
        if let Some(memory_id) = entry
            .memory_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            keys.insert(memory_id.to_string());
        }
        let content_key = memory_dedup_key(entry.content.trim());
        if !content_key.is_empty() {
            keys.insert(content_key);
        }
    }
    keys
}

/// Merge two structured-retrieval results, deduplicating by
/// `memory_id`. The first occurrence of a given id wins — the full
/// message query is passed first so its hits take priority on ties.
pub(crate) fn merge_structured_results(
    full: Vec<RankableMemory>,
    entity: Vec<RankableMemory>,
) -> Vec<RankableMemory> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(full.len() + entity.len());
    for m in full.into_iter().chain(entity) {
        if seen.insert(m.memory_id.clone()) {
            out.push(m);
        }
    }
    out
}

fn context_entry_from_rankable(memory: &RankableMemory) -> Option<ContextMemoryEntry> {
    let memory_id = memory.memory_id.trim();
    let memory_type = memory.memory_type.trim();
    let relevance_score = memory.retrieval_score.filter(|score| score.is_finite())?;
    if memory_id.is_empty() || memory_type.is_empty() {
        return None;
    }
    let entry = astra_prompts::memory_proto::MemoryEntry::parse(&memory.content)?;
    if entry.ns == astra_prompts::memory_proto::NS_SESSION {
        return None;
    }
    if !astra_prompts::memory_proto::is_prompt_recallable_status(&entry.status) {
        return None;
    }
    if astra_prompts::memory_proto::ns_to_memory_type(&entry.ns) != memory_type {
        return None;
    }
    // Recency changes recall confidence, not the stored fact itself. Keep the
    // durable row immutable and attach the backend-derived freshness caveat to
    // the compact runtime view only. This lets the model verify stale evidence
    // without rewriting history or promoting old state to a hard instruction.
    let compact_view = format!("{}{}", entry.compact_view(), memory.freshness_suffix());
    let compact =
        astra_prompts::memory_proto::MemoryEntry::new(&entry.ns, &entry.status, &compact_view)
            .encode();
    Some(
        ContextMemoryEntry::scored(compact, relevance_score)
            .with_memory_identity(memory_id, memory_type)
            .with_source("memoria.prefetch"),
    )
}

/// Normalize a memory line for dedup: case-fold + strip trailing
/// punctuation + collapse whitespace for semantic deduplication.
fn memory_dedup_key(trimmed: &str) -> String {
    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches(['.', '!', '?', ';', ':', ',', ' '])
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    fn typed_memory(
        memory_id: &str,
        namespace: &str,
        memory_type: &str,
        abstract_text: &str,
        score: f64,
    ) -> MemoriaMemory {
        MemoriaMemory {
            memory_id: memory_id.to_string(),
            content: astra_prompts::memory_proto::MemoryEntry::new(
                namespace,
                astra_prompts::memory_proto::ST_ACTIVE,
                abstract_text,
            )
            .encode(),
            memory_type: memory_type.to_string(),
            retrieval_score: Some(score),
            ..Default::default()
        }
    }

    #[derive(Default)]
    struct ScriptedClient {
        calls: Mutex<Vec<(String, String, String, usize)>>,
        responses: Mutex<VecDeque<Result<Vec<MemoriaMemory>, String>>>,
    }

    #[derive(Default)]
    struct NeverRespondingClient {
        calls: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl MemoriaPort for NeverRespondingClient {
        async fn retrieve_for_prompt(
            &self,
            _query: &str,
            _user_id: &str,
            _session_id: &str,
            _top_k: usize,
        ) -> Result<Vec<MemoriaMemory>, String> {
            *self.calls.lock().expect("calls") += 1;
            std::future::pending().await
        }

        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            panic!("prompt recall must use retrieve_for_prompt")
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok(String::new())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    #[async_trait::async_trait]
    impl MemoriaPort for ScriptedClient {
        async fn retrieve_for_prompt(
            &self,
            query: &str,
            user_id: &str,
            session_id: &str,
            top_k: usize,
        ) -> Result<Vec<MemoriaMemory>, String> {
            self.calls.lock().expect("calls").push((
                query.to_string(),
                user_id.to_string(),
                session_id.to_string(),
                top_k,
            ));
            self.responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        }

        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            panic!("prompt recall must use retrieve_for_prompt")
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok(String::new())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    #[test]
    fn context_entry_requires_identity_type_and_layered_protocol() {
        let valid = RankableMemory {
            memory_id: "m1".into(),
            content: typed_memory(
                "m1",
                astra_prompts::memory_proto::NS_KNOWLEDGE,
                "semantic",
                "Typed memory remains structured across runtime boundaries",
                0.9,
            )
            .content,
            memory_type: "semantic".into(),
            retrieval_score: Some(0.9),
            ..Default::default()
        };
        let entry = context_entry_from_rankable(&valid).expect("typed entry");
        assert_eq!(entry.memory_id.as_deref(), Some("m1"));
        assert_eq!(entry.memory_type.as_deref(), Some("semantic"));
        assert_eq!(entry.source.as_deref(), Some("memoria.prefetch"));

        let mut malformed = valid.clone();
        malformed.memory_id.clear();
        assert!(context_entry_from_rankable(&malformed).is_none());

        let mut mismatched = valid.clone();
        mismatched.memory_type = "episodic".into();
        assert!(context_entry_from_rankable(&mismatched).is_none());

        let mut unranked = valid.clone();
        unranked.retrieval_score = None;
        assert!(context_entry_from_rankable(&unranked).is_none());

        let mut session = valid;
        session.content = astra_prompts::memory_proto::MemoryEntry::new_layered(
            astra_prompts::memory_proto::NS_SESSION,
            astra_prompts::memory_proto::ST_ACTIVE,
            "Current session snapshots use their dedicated runtime lane",
            None,
            Some("{}"),
        )
        .encode();
        session.memory_type = "working".into();
        assert!(context_entry_from_rankable(&session).is_none());
    }

    #[test]
    fn prompt_recall_excludes_non_current_lifecycle_states_and_marks_stale_evidence() {
        let mut stale = RankableMemory {
            memory_id: "old-preference".into(),
            content: astra_prompts::memory_proto::MemoryEntry::new(
                astra_prompts::memory_proto::NS_PREF,
                astra_prompts::memory_proto::ST_ACTIVE,
                "The user previously preferred an older deployment topology",
            )
            .encode(),
            memory_type: "profile".into(),
            retrieval_score: Some(0.9),
            trust_tier: Some("T3".into()),
            observed_at: Some((chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339()),
            ..Default::default()
        };

        let entry = context_entry_from_rankable(&stale).expect("active typed memory");
        assert!(entry.content.contains("stale — verify first"));

        for status in [
            astra_prompts::memory_proto::ST_SUPERSEDED,
            astra_prompts::memory_proto::ST_DISPUTED,
            astra_prompts::memory_proto::ST_EXPIRED,
            astra_prompts::memory_proto::ST_ARCHIVED,
            astra_prompts::memory_proto::ST_DONE,
        ] {
            stale.content = astra_prompts::memory_proto::MemoryEntry::new(
                astra_prompts::memory_proto::NS_PREF,
                status,
                "Historical evidence remains durable but is not prompt current",
            )
            .encode();
            assert!(
                context_entry_from_rankable(&stale).is_none(),
                "status {status} must stay out of prompt recall"
            );
        }
    }

    #[tokio::test]
    async fn provider_neutral_recall_preserves_scope_and_returns_typed_entries() {
        let client = ScriptedClient {
            responses: Mutex::new(VecDeque::from([Ok(vec![
                typed_memory(
                    "shared",
                    astra_prompts::memory_proto::NS_KNOWLEDGE,
                    "semantic",
                    "Runtime recall preserves typed evidence across deployment modes",
                    0.8,
                ),
                typed_memory(
                    "high",
                    astra_prompts::memory_proto::NS_PREF,
                    "profile",
                    "The user prefers behavior tests over source text matching",
                    0.95,
                ),
            ])])),
            ..Default::default()
        };

        let result =
            prefetch_memories_with_client(&client, "复核 astra 记忆 #42", "user-7", "session-9", 5)
                .await;

        assert_eq!(result.items, 2);
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::Complete
        );
        assert_eq!(result.entries[0].memory_id.as_deref(), Some("high"));
        let calls = client.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "复核 astra 记忆 #42");
        assert!(calls.iter().all(|(_, user, session, top_k)| {
            user == "user-7" && session == "session-9" && *top_k == 5
        }));
    }

    #[tokio::test]
    async fn first_turn_prefetch_uses_only_typed_profile_and_episode_rows() {
        let client = ScriptedClient {
            responses: Mutex::new(VecDeque::from([
                Ok(vec![
                    typed_memory(
                        "profile",
                        astra_prompts::memory_proto::NS_PREF,
                        "profile",
                        "The user consistently prefers concise technical explanations",
                        0.9,
                    ),
                    typed_memory(
                        "wrong-profile",
                        astra_prompts::memory_proto::NS_KNOWLEDGE,
                        "semantic",
                        "Semantic rows do not enter the profile prewarm bucket",
                        0.99,
                    ),
                ]),
                Ok(vec![typed_memory(
                    "episode",
                    astra_prompts::memory_proto::NS_EPISODE,
                    "episodic",
                    "The prior session completed the runtime lane migration",
                    0.8,
                )]),
            ])),
            ..Default::default()
        };

        let result =
            prefetch_session_start_memories_with_client(&client, "user-7", "session-9").await;
        let ids = result
            .entries
            .iter()
            .filter_map(|entry| entry.memory_id.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["profile", "episode"]);
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::Complete
        );
        assert_eq!(client.calls.lock().expect("calls").len(), 2);
    }

    #[tokio::test]
    async fn recall_failure_is_soft_evidence_absence() {
        let client = ScriptedClient {
            responses: Mutex::new(VecDeque::from([Err("backend unavailable".into())])),
            ..Default::default()
        };
        let result =
            prefetch_memories_with_client(&client, "review memory #42", "user", "session", 5).await;
        assert!(result.entries.is_empty());
        assert_eq!(result.items, 0);
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::Unavailable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_recall_deadline_makes_a_blackholed_backend_soft_absence() {
        let client = NeverRespondingClient::default();
        let result = prefetch_memories_with_client(
            &client,
            "retrieve the durable architecture notes",
            "user",
            "session",
            5,
        )
        .await;

        // The paused clock reaches the retrieval budget without wall-clock
        // waiting. Without the timeout above this test would never complete.
        assert!(result.entries.is_empty());
        assert_eq!(result.items, 0);
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::Unavailable
        );
        assert_eq!(*client.calls.lock().expect("calls"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn session_start_recall_uses_the_same_interactive_deadline() {
        let client = NeverRespondingClient::default();
        let result = prefetch_session_start_memories_with_client(&client, "user", "session").await;

        assert!(result.entries.is_empty());
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::Unavailable
        );
        assert_eq!(
            *client.calls.lock().expect("calls"),
            2,
            "profile and episodic recall are independent but both must be bounded"
        );
    }

    #[test]
    fn admission_deduplicates_one_payload_but_does_not_hide_from_later_consumers() {
        let entry = ContextMemoryEntry::scored("typed compact evidence", 0.8)
            .with_memory_identity("m1", "semantic");
        let first =
            admit_prompt_memory_entries("session-admission", vec![entry.clone(), entry.clone()]);
        let second = admit_prompt_memory_entries("session-admission", vec![entry]);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
    }

    #[test]
    fn surfaced_keys_preserve_backend_identity_and_normalized_content() {
        let entries = vec![
            ContextMemoryEntry::scored("  Hello  World .", 0.5)
                .with_memory_identity("m1", "semantic"),
        ];
        let seen = collect_seen_contents(&entries);
        assert!(seen.contains("m1"));
        assert!(seen.contains("hello world"));
    }

    #[tokio::test]
    async fn empty_inputs_do_not_contact_a_provider() {
        let client = ScriptedClient::default();
        let result = prefetch_memories_with_client(&client, "   ", "user", "session", 5).await;
        assert!(result.entries.is_empty());
        assert!(client.calls.lock().expect("calls").is_empty());
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::NotAttempted
        );
    }

    #[test]
    fn memory_prefetch_result_default_is_empty() {
        let result = MemoryPrefetchResult::default();
        assert!(result.entries.is_empty());
        assert_eq!(result.items, 0);
        assert!(result.preview.is_empty());
        assert_eq!(result.fetch_ms, 0);
        assert_eq!(
            result.outcome,
            astra_turn_types::MemoryRetrievalOutcome::NotAttempted
        );
    }
}
