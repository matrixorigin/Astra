//! Memory prefetch utilities for LLM prompt augmentation.
//!
//! Provides hybrid retrieval (full message + entity-keyword) from Memoria HTTP API,
//! merging and deduplicating results into a structured section for injection into
//! the system prompt.
//!
//! Ranking: Memoria's server-side `final_score` already tier-weights
//! via confidence decay (half-life T1=365d … T4=30d) — we trust it and
//! only re-sort the merged hybrid results by that score so the output
//! is deterministic. No client-side tier multiplier: doing one here
//! would double-count what the server already did.

use std::time::Instant;

use astra_turn_core::context_sources::MemoryEntry as ContextMemoryEntry;
use astra_turn_types::RankableMemory;

// Keep these prompt-facing labels centralized so future wording changes stay
// synchronized between rendering and tests. The learnings label intentionally
// includes the source session id; the cross-session label stays generic to keep
// repeated prompt decorations short.
const LEARNINGS_SOURCE_LABEL_PREFIX: &str = "[learnings from session ";
const LEARNINGS_SOURCE_LABEL_SUFFIX: &str = "]";
const CROSS_SESSION_SOURCE_LABEL: &str = "[cross-session memory]";

/// Result of a memory prefetch operation.
#[derive(Debug, Default)]
pub struct MemoryPrefetchResult {
    pub section: Option<String>,
    pub entries: Vec<ContextMemoryEntry>,
    pub items: usize,
    pub preview: Vec<String>,
    pub fetch_ms: i64,
}

/// Prefetch memories relevant to the user message via hybrid retrieval.
/// Sends two queries (full message + entity tokens), merges and deduplicates.
/// When `session_id` is provided, memories suppressed via
/// `MemoriaClient::suppress_memory` are filtered out before building entries.
pub async fn prefetch_memories(
    mem_url: &str,
    mem_key: &str,
    user_msg: &str,
    user_id: &str,
    top_k: u32,
    session_id: Option<&str>,
) -> MemoryPrefetchResult {
    if mem_key.is_empty() || user_msg.trim().is_empty() {
        return MemoryPrefetchResult::default();
    }
    let started = Instant::now();
    let entity_query = extract_entity_tokens(user_msg);
    let trimmed_msg = user_msg.trim();

    // Parallel fetch: full message retrieval + entity-keyword retrieval via tokio::join!
    // Each fetch returns structured memories (content + memory_type +
    // retrieval_score + trust_tier) so the merge step can re-rank by
    // composite score instead of flat vector similarity.
    let do_entity = !entity_query.is_empty() && entity_query != trimmed_msg;
    let (full_result, entity_result) = tokio::join!(
        fetch_memories_structured(mem_url, mem_key, trimmed_msg, user_id, top_k),
        async {
            if do_entity {
                fetch_memories_structured(mem_url, mem_key, &entity_query, user_id, top_k).await
            } else {
                Vec::new()
            }
        }
    );
    // Merge by memory_id (dedup across the two parallel queries), then
    // re-sort by server-side retrieval_score (already tier-weighted),
    // then cap at top_k*2 (heuristic: we want enough to pick from
    // downstream but not 100s).
    let mut merged_records = merge_structured_results(full_result, entity_result);
    astra_turn_types::sort_by_retrieval_score(&mut merged_records);
    let ranked_cap = (top_k as usize).saturating_mul(2).max(top_k as usize);
    merged_records.truncate(ranked_cap);

    // Filter out session-suppressed memories before building entries.
    if let Some(sid) = session_id {
        let suppressed = astra_tools::memoria::MemoriaClient::suppressed_snapshot(sid);
        if !suppressed.is_empty() {
            merged_records.retain(|m| !suppressed.contains(&m.memory_id));
        }
    }

    // Slice each record to its compact view (tag + abstract layer)
    // before handing it to the section builders. The volatile system-
    // prompt lane should only ship the one-line abstract; overview
    // and detail layers stay behind Memoria for lazy `memory_expand`.
    // Records that don't parse as tagged entries fall through
    // untouched — those are legacy / unstructured retrievals that
    // will be filtered downstream by `is_memory_worthy`.
    let merged: Vec<String> = merged_records
        .iter()
        .map(|m| {
            let compact = compact_view_of(&m.content);
            match source_label(&m.memory_type, m.session_id.as_deref()) {
                Some(label) => {
                    tracing::debug!(
                        target: "astra::memory::prefetch",
                        memory_id = %m.memory_id,
                        memory_type = %m.memory_type,
                        session_id = m.session_id.as_deref().unwrap_or(""),
                        source_label = %label,
                        "prefetch_memories: applied source label"
                    );
                    format!("{label} {compact}")
                }
                None => compact,
            }
        })
        .collect();
    let fetch_ms = started.elapsed().as_millis() as i64;
    let preview = merged.iter().take(3).map(|l| l.to_string()).collect();
    let items = merged.len();
    let section = build_memory_section(&merged);
    let entries = build_memory_entries(&merged);
    MemoryPrefetchResult {
        section,
        entries,
        items,
        preview,
        fetch_ms,
    }
}

/// Result of a session-start prefetch — gathers `profile` + recent
/// `episodic` memories so the first turn of a session has
/// cross-session continuity baked into the system prompt.
///
/// The three buckets mirror `MemoryOrchestrator::SessionStartMemories`
/// but stay in the prefetch module so we don't drag the orchestrator
/// dependency into `bridge_inprocess`.
#[derive(Debug, Default)]
pub struct SessionStartPrefetchResult {
    /// Rendered `<session_memory>` block. `None` when nothing to inject.
    pub section: Option<String>,
    /// Ground-truth profile memories (`memory_type=profile`).
    pub profile: Vec<RankableMemory>,
    /// Most recent cross-session episodes (`memory_type=episodic`).
    pub recent_episodes: Vec<RankableMemory>,
    /// Reflect-produced scene abstractions (`astra:scene`-tagged
    /// `semantic` memories) — higher-level than a single episode,
    /// lower-level than a profile fact. Surfaces "what themes have
    /// recurred across sessions".
    pub recent_scenes: Vec<RankableMemory>,
    /// How long the prefetch took.
    pub fetch_ms: i64,
}

/// Prefetch session-start memories: profile + recent episodes. Intended
/// to run **only on the first turn** of a session — the caller decides
/// based on `turn_number`. On non-first turns we rely on
/// `prefetch_memories` (hybrid per-turn recall) to surface relevant
/// memory.
///
/// Both fetches run in parallel; any fetch failure is treated as "no
/// memory" so a degraded Memoria never blocks the turn.
pub async fn prefetch_session_start_memories(
    mem_url: &str,
    mem_key: &str,
    user_id: &str,
) -> SessionStartPrefetchResult {
    if mem_key.is_empty() {
        return SessionStartPrefetchResult::default();
    }
    let started = Instant::now();

    // Three queries in parallel:
    //   1. `profile` — broad query to surface user-identity memories.
    //      Filtered client-side on `memory_type == "profile"`.
    //   2. `episodic` — query biased toward recent session summaries.
    //      Filtered on `memory_type == "episodic"`.
    //   3. `scenes` — reflect-produced `astra:scene`-tagged semantic
    //      memories. Content starts with `[scene…]`, filtered client-
    //      side so the bucket stays pure.
    const PROFILE_TOP_K: u32 = 5;
    const EPISODE_TOP_K: u32 = 6;
    const SCENE_TOP_K: u32 = 5;
    let (profile_raw, episode_raw, scene_raw) = tokio::join!(
        fetch_memories_structured(
            mem_url,
            mem_key,
            "user profile preferences role",
            user_id,
            PROFILE_TOP_K,
        ),
        fetch_memories_structured(
            mem_url,
            mem_key,
            "recent session episode summary",
            user_id,
            EPISODE_TOP_K,
        ),
        fetch_memories_structured(
            mem_url,
            mem_key,
            "recurring scene pattern insight",
            user_id,
            SCENE_TOP_K,
        ),
    );

    let mut profile: Vec<RankableMemory> = profile_raw
        .into_iter()
        .filter(|m| m.memory_type == "profile")
        .collect();
    let mut recent_episodes: Vec<RankableMemory> = episode_raw
        .into_iter()
        .filter(|m| m.memory_type == "episodic")
        .collect();
    let mut recent_scenes: Vec<RankableMemory> = scene_raw
        .into_iter()
        .filter(|m| m.memory_type == "semantic" && m.content.starts_with("[scene"))
        .collect();

    // Sort each bucket by server-side retrieval score and cap.
    astra_turn_types::sort_by_retrieval_score(&mut profile);
    astra_turn_types::sort_by_retrieval_score(&mut recent_episodes);
    astra_turn_types::sort_by_retrieval_score(&mut recent_scenes);
    profile.truncate(3);
    recent_episodes.truncate(3);
    recent_scenes.truncate(3);

    let section = build_session_start_block(&profile, &recent_episodes, &recent_scenes);
    SessionStartPrefetchResult {
        section,
        profile,
        recent_episodes,
        recent_scenes,
        fetch_ms: started.elapsed().as_millis() as i64,
    }
}

/// Fetch an ambient `<memory_index>` block — a compact list of all
/// known memories so the LLM knows what it *could* recall. Session-
/// stable: rebuilt once per session-start and kept in the prompt
/// prefix; only flips when the user writes a new memory.
///
/// Backed by the same `/v1/memories/retrieve` endpoint with an empty
/// query (browse mode). Returns `None` when the key is empty or no
/// memories exist for this user. `exclude_ids` omits memory_ids that
/// will appear in `<session_memory>` so the same content isn't shown
/// in two blocks on turn 1.
pub async fn prefetch_memory_index(
    mem_url: &str,
    mem_key: &str,
    user_id: &str,
    exclude_ids: &std::collections::HashSet<String>,
) -> Option<String> {
    if mem_key.is_empty() {
        return None;
    }
    const INDEX_TOP_K: u32 = 80;
    let items = fetch_memories_structured(mem_url, mem_key, "", user_id, INDEX_TOP_K).await;
    if items.is_empty() {
        return None;
    }
    build_memory_index_block(&items, exclude_ids)
}

/// Render a `<memory_index>` block — one line per entry, under 100
/// chars of abstract. Entries are sorted by `memory_id` so the block is
/// byte-stable across turns (Memoria's browse-mode ordering drifts as
/// scores decay). `exclude_ids` removes memory_ids that will already
/// appear in `<session_memory>` to avoid duplication. Returns `None`
/// on empty input (or when every input was excluded).
fn build_memory_index_block(
    items: &[RankableMemory],
    exclude_ids: &std::collections::HashSet<String>,
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut filtered: Vec<&RankableMemory> = items
        .iter()
        .filter(|m| !exclude_ids.contains(&m.memory_id))
        .collect();
    if filtered.is_empty() {
        return None;
    }
    // Deterministic ordering so cached prefix bytes don't flip when
    // Memoria returns the same set in a different order.
    filtered.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));

    let mut s = String::with_capacity(1024);
    s.push_str("<memory_index>\n");
    s.push_str(
        "(Ambient awareness. Call `memory(action=recall, query=X)` for full content \
        of a topic, or `memory(action=expand, memory_id=ID)` to drill into one entry.)\n",
    );
    for m in filtered {
        let tag = if m.memory_type.is_empty() {
            "?".to_string()
        } else {
            m.memory_type.clone()
        };
        let abstract_line = session_start_line(&m.content, 100);
        let suffix = m.freshness_suffix();
        s.push_str(&format!(
            "- [{tag}] {}: {abstract_line}{suffix}\n",
            m.memory_id
        ));
    }
    s.push_str("</memory_index>");
    Some(s)
}

/// Concatenate the session-stable memory blocks (`<memory_index>` +
/// `<session_memory>`) into one text block suitable for insertion in
/// the session-stable prompt lane. Pure; returns `None` when nothing
/// to inject.
///
/// This block is **byte-stable across turns within a session** (assuming
/// no new writes occur). It's separated from the per-turn recall block
/// because per-turn content changes every turn and must sit in the
/// volatile lane instead.
pub fn build_session_stable_memory_block(
    memory_index: Option<&str>,
    session_start: Option<&str>,
) -> Option<String> {
    match (memory_index, session_start) {
        (Some(idx), Some(start)) => Some(format!("{idx}\n\n{start}")),
        (Some(idx), None) => Some(idx.to_string()),
        (None, Some(start)) => Some(start.to_string()),
        (None, None) => None,
    }
}

/// Collect the memory_ids that a `SessionStartPrefetchResult` will
/// surface via `<session_memory>` so the `<memory_index>` builder can
/// dedup against them.
pub fn session_start_exposed_ids(
    result: &SessionStartPrefetchResult,
) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for m in result
        .profile
        .iter()
        .chain(result.recent_episodes.iter())
        .chain(result.recent_scenes.iter())
    {
        if !m.memory_id.is_empty() {
            set.insert(m.memory_id.clone());
        }
    }
    set
}

/// Filter a list of per-turn Memoria context entries, removing ones
/// whose content is already surfaced in the session-stable block so the
/// LLM doesn't see the same memory twice. `seen_contents` holds the
/// compact-view text of every entry shown in `<session_memory>` /
/// `<memory_index>` earlier this session; dedup is on content, not
/// memory_id, because the typed-pipeline entries don't carry ids.
pub fn filter_entries_already_surfaced(
    entries: Vec<ContextMemoryEntry>,
    seen_contents: &std::collections::HashSet<String>,
) -> Vec<ContextMemoryEntry> {
    if seen_contents.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| {
            let key = memory_dedup_key(e.content.trim());
            !seen_contents.contains(&key)
        })
        .collect()
}

/// Extract dedup keys for every entry in a per-turn recall list so a
/// later call to [`filter_entries_already_surfaced`] can prune
/// duplicates. Returns a set of normalized (case-folded, collapsed)
/// content strings.
pub fn collect_seen_contents(entries: &[ContextMemoryEntry]) -> std::collections::HashSet<String> {
    entries
        .iter()
        .map(|e| memory_dedup_key(e.content.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Render the `<session_memory>` block injected into the system prompt
/// on the first turn. Returns `None` when both lists are empty so the
/// caller can skip injection entirely.
fn build_session_start_block(
    profile: &[RankableMemory],
    episodes: &[RankableMemory],
    scenes: &[RankableMemory],
) -> Option<String> {
    if profile.is_empty() && episodes.is_empty() && scenes.is_empty() {
        return None;
    }
    let mut s = String::with_capacity(512);
    s.push_str("<session_memory>\n");
    if !profile.is_empty() {
        s.push_str("### User profile\n");
        for m in profile {
            s.push_str("- ");
            s.push_str(&session_start_line(&m.content, 160));
            s.push_str(&m.freshness_suffix());
            s.push('\n');
        }
    }
    if !episodes.is_empty() {
        s.push_str("### Recent sessions\n");
        for m in episodes {
            s.push_str("- ");
            s.push_str(&session_start_line(&m.content, 200));
            s.push_str(&m.freshness_suffix());
            s.push('\n');
        }
    }
    if !scenes.is_empty() {
        s.push_str("### Recurring scenes\n");
        for m in scenes {
            s.push_str("- ");
            s.push_str(&session_start_line(&m.content, 200));
            s.push_str(&m.freshness_suffix());
            s.push('\n');
        }
    }
    s.push_str("</session_memory>");
    Some(s)
}

/// Compact one memory entry to its first meaningful line, capped.
fn session_start_line(raw: &str, budget: usize) -> String {
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

/// Flatten a retrieved entry to its compact view:
/// `[@ns/status] <abstract>`. Drops any overview/detail layers so the
/// volatile system-prompt lane stays within budget.
///
/// Entries that don't parse as structured protocol memories pass
/// through verbatim — downstream `is_memory_worthy` still filters
/// them, and legacy unstructured hits remain visible. The fallback
/// path is counted at `trace` level so operators can tell from logs
/// whether the v1→v1.1 hard break is actually landing (all hits on
/// the structured path) or whether legacy v1 entries still dominate
/// retrieval (fallback rate stays high).
pub(crate) fn compact_view_of(content: &str) -> String {
    match astra_prompts::memory_proto::MemoryEntry::parse(content) {
        Some(entry) => astra_prompts::memory_proto::MemoryEntry::new(
            &entry.ns,
            &entry.status,
            entry.compact_view(),
        )
        .encode(),
        None => {
            tracing::trace!(
                target: "astra::memory::compact_view",
                bytes = content.len(),
                "compact_view_of: unstructured fallback (v1 legacy or malformed)"
            );
            content.to_string()
        }
    }
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

/// Merge and deduplicate memory results from multiple retrieval queries.
///
/// Retained for the test-suite which exercises the string-flat API.
/// Production code goes through `merge_structured_results` which
/// preserves tier / memory_type so ranking can apply.
#[cfg(test)]
pub(crate) fn merge_memory_results(results: &[&str]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for result in results {
        for line in result.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                merged.push(trimmed.to_string());
            }
        }
    }
    merged
}

/// Build the memory section for the profile block.
/// Returns None if no memories matched.
///
/// Applies the same `is_memory_worthy` filter as `build_memory_entries` so
/// that session-replay lines (`[session:…] Recent conversation: Assistant:
/// Step 15 done. yyyy…`), L1 protocol markers, bare headers, and
/// single-token echoes never leak into the volatile `## User Memories`
/// block. Without this filter the unstructured fallback path
/// (`memory_proto::format_for_llm` → `**Context:** …`) burned 3,000+
/// characters per turn on session-replay noise (observed in session
/// `e61916d6`, turn 4: 3,129c of `**Context:** Assistant: Step N done. yyy`).
pub(crate) fn build_memory_section(merged_lines: &[String]) -> Option<String> {
    if merged_lines.is_empty() {
        return None;
    }
    let filtered: Vec<&String> = merged_lines
        .iter()
        .filter(|s| is_memory_worthy(s.trim()))
        .collect();
    if filtered.is_empty() {
        return None;
    }
    let refs: Vec<&str> = filtered.iter().map(|s| s.as_str()).collect();
    let formatted = astra_prompts::memory_proto::format_for_llm(&refs);
    if !formatted.is_empty() {
        Some(format!("## User Memories\n{formatted}"))
    } else {
        Some(format!("## User Memories\n{}", refs.join("\n")))
    }
}

/// Build typed memory catalog entries from ranked Memoria retrieval lines.
///
/// Applies a quality filter to skip low-signal fragments that Memoria
/// sometimes returns when it indexes L1 session-memory docs per-line:
/// bare markdown headers, single-word echoes, and near-duplicates of
/// higher-ranked entries. Without this filter the `## User Memories`
/// block was observed to bloat to ~1,200 tokens with `**Context:** hi`,
/// `**Context:** # Task Specification` etc. (session `ec35c711`).
pub(crate) fn build_memory_entries(merged_lines: &[String]) -> Vec<ContextMemoryEntry> {
    let total = merged_lines.len();
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    merged_lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            if !is_memory_worthy(trimmed) {
                return None;
            }
            // Dedup key: case-folded, trailing-punctuation stripped.
            // Tagged entries (`[@ns/type] body`) dedup on the full line
            // so two entries with different tags but same body still
            // both appear (they carry different semantics).
            let key = memory_dedup_key(trimmed);
            if !seen_keys.insert(key) {
                return None;
            }
            let formatted = astra_prompts::memory_proto::format_for_llm(&[trimmed]);
            let content = if formatted.is_empty() {
                trimmed.to_string()
            } else {
                formatted
            };
            Some(
                ContextMemoryEntry::scored(content, (total.saturating_sub(idx)) as f64)
                    .with_source("memoria.prefetch"),
            )
        })
        .collect()
}

/// Is this line signal-bearing enough to keep as a memory?
///
/// Rejects patterns observed in real Memoria retrieval noise:
/// - lone markdown headers (`# …`, `## …`, `### …`) — section titles
///   stored per-line from L1 doc fragmentation
/// - L1 protocol markers (`[session-memory:v1]`, `[attention:v1]`)
/// - short generic status fragments (`None`, `Tools used: none`,
///   `Turn 1`, `Turn 1, active`, `🔄 In progress`, `✅ …`, `⚠ …`)
/// - single-word user echoes (`hi`, `ok`, `yes`)
///
/// Tagged entries (`[@namespace/type] body`) always pass even if short —
/// the namespace already asserts the entry carries structured meaning.
fn is_memory_worthy(trimmed: &str) -> bool {
    let semantic_payload = memory_payload_without_source_label(trimmed);
    if astra_prompts::memory_proto::is_session_namespace_memory(semantic_payload) {
        return false;
    }

    // Always keep structured-tagged entries except session-memory
    // protocol rows, which are injected through the dedicated
    // session-memory lane rather than generic cross-session recall.
    if trimmed.starts_with("[@") && trimmed.contains("/") && trimmed.contains("]") {
        return true;
    }

    // Reject lone markdown headers (up to 6 levels of `#`).
    if let Some(rest) = trimmed.strip_prefix('#') {
        let stripped = rest.trim_start_matches('#').trim_start();
        // If the line is just a header with short body (common for fragmented docs), drop.
        if stripped.is_empty() || stripped.chars().count() <= 30 {
            return false;
        }
    }

    // Reject L1 protocol markers and session-replay lines. These last
    // two shapes surface when Memoria indexes session-memory / L1 docs
    // per-line and retrieval matches a fragment; keeping parity with
    // `memoria_insights::is_digest_worthy` so both the CLI-side digest
    // and the bridge-side `## User Memories` filter out the same noise.
    if trimmed.starts_with("[session-memory:")
        || trimmed.starts_with("[attention:")
        || trimmed.starts_with("[session:")
        || trimmed.starts_with("[compaction:")
        || trimmed.starts_with("[session-knowledge:")
    {
        // Legacy pre-L2 auto-write shapes. L2 now rejects these at
        // write, but entries that landed *before* the gate shipped
        // still live in Memoria. Observed dominant pollution source
        // in session 72ef747f: every `## User Memories` entry was
        // `**Context:** [compaction:<sid>] …` from before L2. Decay
        // (T3 half-life 60d) eventually removes them; until then
        // filter at read so they don't reach the prompt.
        return false;
    }

    // Reject well-known low-signal fragments.
    const NOISE_EXACT: &[&str] = &["None", "(none)", "Tools used: none", "🔄 In progress"];
    if NOISE_EXACT.contains(&trimmed) {
        return false;
    }

    // Reject runtime scaffolding that leaked into conversation history
    // and was then indexed by Memoria. These lines are synthesized by the
    // runtime itself (nudges, corrections, verification directives,
    // attention-manifest echoes) and have zero cross-session semantic
    // value — retrieving them as "memories" just replays runtime
    // injections back into the prompt. Observed in session 6676c7b5,
    // turn 4: 78 such `**Context:**` entries filled a 6,397c block.
    //
    // Routed through the single source of truth in
    // `astra_turn_types::scaffolding_body_prefixes_for_filtering` so new runtime
    // injections added there are automatically filtered here.
    for prefix in astra_turn_types::scaffolding_body_prefixes_for_filtering() {
        if trimmed.starts_with(prefix) {
            return false;
        }
    }

    // Reject short status/emoji-prefixed fragments.
    if trimmed.starts_with("Turn ")
        || trimmed.starts_with("✅ ")
        || trimmed.starts_with("🔄 ")
        || trimmed.starts_with("⏳ ")
        || trimmed.starts_with("⚠ ")
        || trimmed.starts_with("⚠️")
    {
        // These are short status ticks. Only allow if body is long enough
        // to contain real information (>50 chars after the prefix).
        if trimmed.chars().count() <= 40 {
            return false;
        }
    }

    // Reject bare single-token user echoes (`hi`, `ok`, `yes`, emoji-only).
    let word_count = trimmed
        .split(|c: char| c.is_whitespace() || "，。！？,.!?".contains(c))
        .filter(|w| !w.is_empty())
        .count();
    if word_count < 3 && trimmed.chars().count() < 20 {
        return false;
    }

    true
}

fn memory_payload_without_source_label(trimmed: &str) -> &str {
    let content = trimmed.trim_start();
    if let Some(rest) = content.strip_prefix(CROSS_SESSION_SOURCE_LABEL) {
        return rest.trim_start();
    }
    if let Some(rest) = content.strip_prefix(LEARNINGS_SOURCE_LABEL_PREFIX)
        && let Some((_, payload)) = rest.split_once(LEARNINGS_SOURCE_LABEL_SUFFIX)
    {
        return payload.trim_start();
    }
    content
}

/// Normalize a memory line for dedup: case-fold + strip trailing
/// punctuation + collapse whitespace. Matches the `memoria_insights`
/// dedup key so behaviour is consistent across the two surfaces.
fn memory_dedup_key(trimmed: &str) -> String {
    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches(['.', '!', '?', ';', ':', ',', ' '])
        .to_lowercase()
}

/// Extract non-CJK, non-punctuation tokens from a message for keyword-based retrieval.
pub(crate) fn extract_entity_tokens(msg: &str) -> String {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in msg.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            current.push(ch);
        } else {
            if current.len() >= 3 {
                tokens.push(current.clone());
            }
            current.clear();
        }
    }
    if current.len() >= 3 {
        tokens.push(current);
    }
    tokens.join(" ")
}

#[cfg(test)]
thread_local! {
    /// Test override: when `Some`, `fetch_memories` builds its HTTP client with
    /// this (connect_timeout, request_timeout) tuple instead of the production
    /// 5s/10s. Used by the black-hole timeout test to avoid burning 10s of real
    /// wall-clock. Production ignores this.
    static TEST_FETCH_MEMORIES_TIMEOUTS: std::cell::RefCell<
        Option<(std::time::Duration, std::time::Duration)>,
    > = const { std::cell::RefCell::new(None) };
}

fn fetch_memories_timeouts() -> (std::time::Duration, std::time::Duration) {
    #[cfg(test)]
    if let Some(t) = TEST_FETCH_MEMORIES_TIMEOUTS.with(|c| *c.borrow()) {
        return t;
    }
    (
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(10),
    )
}

/// Fetch memories from Memoria HTTP API. Returns joined content string.
#[cfg(test)]
async fn fetch_memories(
    base_url: &str,
    api_key: &str,
    query: &str,
    user_id: &str,
    top_k: u32,
) -> String {
    // Legacy string-flat API, kept for tests that exercise only the
    // content path. Production goes through `fetch_memories_structured`.
    fetch_memories_structured(base_url, api_key, query, user_id, top_k)
        .await
        .into_iter()
        .map(|m| m.content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hit the Memoria retrieve endpoint and deserialize responses into
/// `RankableMemory` records that preserve `memory_type`, `trust_tier`,
/// and `retrieval_score`. These fields are needed for composite-score
/// ranking (L3); the legacy string-flat path threw them away.
///
/// Returns an empty vector on any failure (network, HTTP, parse) so
/// the caller can treat recall as purely additive — a failed fetch
/// never breaks the turn.
async fn fetch_memories_structured(
    base_url: &str,
    api_key: &str,
    query: &str,
    user_id: &str,
    top_k: u32,
) -> Vec<RankableMemory> {
    let (connect_timeout, request_timeout) = fetch_memories_timeouts();
    let client = astra_core::net::build_internal_http_client(
        reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout),
        "memory prefetch client",
    );
    let mut payload = serde_json::json!({"query": query, "top_k": top_k});
    if !user_id.is_empty() {
        payload["session_id"] = serde_json::Value::String(user_id.to_string());
        payload["user_id"] = serde_json::Value::String(user_id.to_string());
    }
    let resp = match client
        .post(format!("{base_url}/v1/memories/retrieve"))
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            astra_core::agent_error!("memory", "fetch error: {e:#}");
            return Vec::new();
        }
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let arr = match resp.json::<Vec<serde_json::Value>>().await {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    arr.into_iter().filter_map(parse_rankable).collect()
}

/// Parse one Memoria retrieve-response entry into a `RankableMemory`.
/// Requires `content` and `memory_id`; everything else is optional
/// and filled with reasonable defaults. Returns `None` when the
/// required fields are missing (Memoria server bug, don't crash).
fn parse_rankable(value: serde_json::Value) -> Option<RankableMemory> {
    let content = value.get("content")?.as_str()?.to_string();
    if content.is_empty() {
        return None;
    }
    // memory_id may be absent in some legacy responses; fall back to
    // a hash of the content so dedup still functions.
    let memory_id = value
        .get("memory_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            content.hash(&mut h);
            format!("anon-{:x}", h.finish())
        });
    let memory_type = value
        .get("memory_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let retrieval_score = value
        .get("retrieval_score")
        .and_then(serde_json::Value::as_f64);
    let trust_tier = value
        .get("trust_tier")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let observed_at = value
        .get("observed_at")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let updated_at = value
        .get("updated_at")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let session_id = value
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some(RankableMemory {
        memory_id,
        content,
        memory_type,
        retrieval_score,
        trust_tier,
        observed_at,
        updated_at,
        session_id,
    })
}

/// Returns a human-readable source label for a prefetched memory entry.
/// `"procedural"` memories originated from the agent's own learnings;
/// all other typed memories are cross-session knowledge.
/// Returns `None` when there is no session_id to attribute.
pub(crate) fn source_label(memory_type: &str, session_id: Option<&str>) -> Option<String> {
    let sid = session_id.filter(|s| !s.is_empty())?;
    match memory_type {
        "procedural" => Some(format!(
            "{LEARNINGS_SOURCE_LABEL_PREFIX}{sid}{LEARNINGS_SOURCE_LABEL_SUFFIX}"
        )),
        t if !t.is_empty() => Some(CROSS_SESSION_SOURCE_LABEL.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_memory_response(body: &'static str, requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..requests {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn extract_entity_tokens_empty_string() {
        assert_eq!(extract_entity_tokens(""), "");
    }

    #[test]
    fn extract_entity_tokens_short_words_filtered() {
        assert_eq!(extract_entity_tokens("a bc"), "");
    }

    #[test]
    fn extract_entity_tokens_preserves_long_tokens() {
        assert_eq!(extract_entity_tokens("hello world"), "hello world");
    }

    #[test]
    fn extract_entity_tokens_special_chars_split() {
        assert_eq!(extract_entity_tokens("hello.world!foo"), "hello world foo");
    }

    #[test]
    fn extract_entity_tokens_hyphens_and_underscores_kept() {
        assert_eq!(extract_entity_tokens("my-var_name"), "my-var_name");
    }

    #[test]
    fn extract_entity_tokens_unicode_chars_as_delimiters() {
        assert_eq!(extract_entity_tokens("memoria 最新的ci?"), "memoria");
    }

    #[test]
    fn extract_entity_tokens_only_special_chars() {
        assert_eq!(extract_entity_tokens("!@#$%"), "");
    }

    #[test]
    fn extract_entity_tokens_from_mixed_language() {
        assert_eq!(extract_entity_tokens("memoria 最新的ci?"), "memoria");
        assert_eq!(
            extract_entity_tokens("matrixone latest pr"),
            "matrixone latest"
        );
        assert_eq!(extract_entity_tokens("你好"), "");
        assert_eq!(
            extract_entity_tokens("check astra status"),
            "check astra status"
        );
    }

    #[test]
    fn merge_deduplicates_across_queries() {
        let r1 = "[@fact/semantic] memoria is matrixorigin/memoria\nsome other fact";
        let r2 = "[@fact/semantic] memoria is matrixorigin/memoria\nnew fact";
        let merged = merge_memory_results(&[r1, r2]);
        assert_eq!(
            merged.len(),
            3,
            "duplicate should be removed, got: {merged:?}"
        );
        assert!(merged.contains(&"[@fact/semantic] memoria is matrixorigin/memoria".to_string()));
        assert!(merged.contains(&"some other fact".to_string()));
        assert!(merged.contains(&"new fact".to_string()));
    }

    #[test]
    fn merge_skips_empty_lines() {
        let r1 = "line1\n\n\nline2";
        let r2 = "";
        let merged = merge_memory_results(&[r1, r2]);
        assert_eq!(merged, vec!["line1", "line2"]);
    }

    #[test]
    fn merge_empty_inputs() {
        assert!(merge_memory_results(&["", ""]).is_empty());
        assert!(merge_memory_results(&[]).is_empty());
    }

    // ── L3: parse_rankable preserves ranking metadata ──────────────────

    #[test]
    fn parse_rankable_extracts_full_metadata() {
        let json = serde_json::json!({
            "memory_id": "m-42",
            "content": "[@pref/active] senior Rust engineer",
            "memory_type": "profile",
            "retrieval_score": 0.72,
            "trust_tier": "T1",
        });
        let m = parse_rankable(json).expect("valid entry");
        assert_eq!(m.memory_id, "m-42");
        assert_eq!(m.content, "[@pref/active] senior Rust engineer");
        assert_eq!(m.memory_type, "profile");
        assert_eq!(m.retrieval_score, Some(0.72));
        assert_eq!(m.trust_tier.as_deref(), Some("T1"));
    }

    #[test]
    fn parse_rankable_fills_memory_id_from_content_hash_when_absent() {
        // Some legacy Memoria responses omit memory_id. parse_rankable
        // falls back to a deterministic content-hash id so dedup in
        // merge_structured_results still works.
        let json = serde_json::json!({
            "content": "some legacy memory",
            "memory_type": "semantic",
        });
        let m = parse_rankable(json).expect("must parse with fallback id");
        assert!(
            m.memory_id.starts_with("anon-"),
            "fallback id missing: {}",
            m.memory_id
        );
        // Same content → same fallback id (so dedup works).
        let json2 = serde_json::json!({
            "content": "some legacy memory",
            "memory_type": "working",
        });
        let m2 = parse_rankable(json2).unwrap();
        assert_eq!(m.memory_id, m2.memory_id);
    }

    #[test]
    fn parse_rankable_rejects_empty_content() {
        let json = serde_json::json!({
            "memory_id": "m-1",
            "content": "",
        });
        assert!(parse_rankable(json).is_none());
    }

    #[test]
    fn parse_rankable_rejects_missing_content() {
        let json = serde_json::json!({"memory_id": "m-1"});
        assert!(parse_rankable(json).is_none());
    }

    // ── L3: merge_structured_results dedups and preserves order ────────

    #[test]
    fn merge_structured_dedupes_by_memory_id() {
        use astra_turn_types::RankableMemory;
        let full = vec![
            RankableMemory {
                memory_id: "shared".into(),
                content: "from full query".into(),
                memory_type: "semantic".into(),
                retrieval_score: Some(0.9),
                trust_tier: Some("T1".into()),
                ..Default::default()
            },
            RankableMemory {
                memory_id: "full-only".into(),
                content: "full unique".into(),
                memory_type: "semantic".into(),
                retrieval_score: Some(0.7),
                trust_tier: Some("T2".into()),
                ..Default::default()
            },
        ];
        let entity = vec![
            // Same memory_id as the full query hit — must be dedupped.
            RankableMemory {
                memory_id: "shared".into(),
                content: "from entity query".into(),
                memory_type: "semantic".into(),
                retrieval_score: Some(0.6),
                trust_tier: Some("T1".into()),
                ..Default::default()
            },
            RankableMemory {
                memory_id: "entity-only".into(),
                content: "entity unique".into(),
                memory_type: "episodic".into(),
                retrieval_score: Some(0.8),
                trust_tier: Some("T3".into()),
                ..Default::default()
            },
        ];
        let merged = merge_structured_results(full, entity);
        let ids: Vec<&str> = merged.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["shared", "full-only", "entity-only"],
            "first occurrence wins; full query goes before entity query"
        );
        // The "shared" entry must be the one from the full query
        // (dedup kept the first occurrence).
        assert_eq!(merged[0].content, "from full query");
    }

    // ── Merge + re-sort preserves server ordering across queries ──
    //
    // Tier weighting is Memoria's job (server `final_score` already
    // does it via conf_score half-life). Here we only verify that
    // after merging the two parallel-query results we re-sort by the
    // server-provided score so the volatile section is deterministic.

    // ── Compact-view slicing ──────────────────────────────────────

    #[test]
    fn compact_view_strips_overview_and_detail() {
        let wire = "[@knowledge/curated] Session sess1: 2 corrections, 1 decisions\n\n\
                    First correction: RS256. First decision: axum.\n\
                    <!--layer:detail-->\n\
                    ## User Corrections\n- Use RS256\n";
        let compact = compact_view_of(wire);
        assert_eq!(
            compact,
            "[@knowledge/curated] Session sess1: 2 corrections, 1 decisions"
        );
    }

    #[test]
    fn compact_view_passthrough_for_unstructured() {
        let raw = "legacy unstructured memory line";
        assert_eq!(compact_view_of(raw), raw);
    }

    #[test]
    fn compact_view_preserves_tag_when_abstract_is_single_line() {
        let wire = "[@pref/active] memoria = matrixorigin/Memoria";
        assert_eq!(compact_view_of(wire), wire);
    }

    #[test]
    fn merge_then_sort_orders_by_server_score() {
        use astra_turn_types::RankableMemory;
        let full = vec![RankableMemory {
            memory_id: "lower-score".into(),
            content: "[@pref/active] prefers terse answers".into(),
            memory_type: "profile".into(),
            retrieval_score: Some(0.65),
            trust_tier: Some("T1".into()),
            ..Default::default()
        }];
        let entity = vec![RankableMemory {
            memory_id: "higher-score".into(),
            content: "[@episode/compaction] session=abc auto-summary".into(),
            memory_type: "episodic".into(),
            retrieval_score: Some(0.82),
            trust_tier: Some("T3".into()),
            ..Default::default()
        }];
        let mut merged = merge_structured_results(full, entity);
        astra_turn_types::sort_by_retrieval_score(&mut merged);
        assert_eq!(
            merged[0].memory_id, "higher-score",
            "trust the server score; don't re-rank client-side"
        );
        assert_eq!(merged[1].memory_id, "lower-score");
    }

    #[test]
    fn build_memory_section_returns_none_for_empty() {
        assert!(build_memory_section(&[]).is_none());
    }

    #[test]
    fn build_memory_section_includes_header() {
        let lines = vec!["[@pref/active] memoria = matrixorigin/Memoria".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(section.starts_with("## User Memories"), "got: {section}");
    }

    #[test]
    fn build_memory_section_formats_structured_entries() {
        let lines = vec!["[@pref/active] dark mode preferred".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(
            section.contains("Preferences"),
            "structured entries should be grouped, got: {section}"
        );
    }

    #[test]
    fn build_memory_section_handles_unstructured() {
        let lines = vec!["just a plain memory without tags".to_string()];
        let section = build_memory_section(&lines).unwrap();
        assert!(section.contains("just a plain memory"), "got: {section}");
    }

    // ── Session-replay noise rejection (volatile-lane dominance fix)
    // Without `is_memory_worthy` filtering inside build_memory_section,
    // every line that survived merge_memory_results went through
    // `memory_proto::format_for_llm`, which wraps unstructured text as
    // `**Context:** …`. Memoria's L1 session-memory indexing produces
    // `[session:xyz] Recent conversation: Assistant: Step 15 done. yyyyy…`
    // fragments — 30+ of these at ~100c each burned ~3,000 characters of
    // volatile-lane budget per turn (observed in session `e61916d6`,
    // turn 4: 3,129c `## User Memories` block).

    #[test]
    fn build_memory_section_drops_session_replay_lines() {
        let lines = vec![
            "[session:abc] Recent conversation: Assistant: Step 15 done. yyyyy".to_string(),
            "User prefers Rust for CLI work.".to_string(),
        ];
        let section = build_memory_section(&lines).expect("some survivors");
        assert!(
            !section.contains("[session:"),
            "session-replay lines must be filtered from section, got: {section}"
        );
        assert!(section.contains("User prefers Rust"));
    }

    #[test]
    fn build_memory_section_drops_l1_protocol_markers() {
        let lines = vec![
            "[session-memory:v1] # Session Title".to_string(),
            "[attention:v1] turn budget tight".to_string(),
            "Legitimate memory survives.".to_string(),
        ];
        let section = build_memory_section(&lines).expect("legit survivor");
        assert!(!section.contains("session-memory:v1"), "got: {section}");
        assert!(!section.contains("attention:v1"), "got: {section}");
        assert!(section.contains("Legitimate memory"));
    }

    #[test]
    fn build_memory_section_drops_pre_l2_legacy_writes() {
        // Pre-L2 auto-compaction + session-end-governance writers
        // emitted these shapes. L2 now rejects them at write, but
        // entries that landed before the gate still live in Memoria.
        // Observed dominant pollution in session 72ef747f: 43 of 68
        // User Memories entries were `[compaction:<sid>] …`.
        let lines = vec![
            "[compaction:055932d7-22bd] ### Primary Request: review PR 42".to_string(),
            "[session-knowledge:abc123] ## User Corrections\n- Use RS256".to_string(),
            "[@pref/active] user prefers terse answers over long explanations".to_string(),
        ];
        let section = build_memory_section(&lines).expect("tagged pref survives");
        assert!(
            !section.contains("[compaction:"),
            "pre-L2 compaction leak: {section}"
        );
        assert!(
            !section.contains("[session-knowledge:"),
            "pre-L2 session-knowledge leak: {section}"
        );
        assert!(
            section.contains("prefers terse answers"),
            "tagged entry must survive: {section}"
        );
    }

    #[test]
    fn build_memory_section_returns_none_when_all_filtered() {
        // All lines are noise — section should collapse to None so the
        // caller knows there is nothing worth emitting, and the bridge's
        // "section only if entries empty" path doesn't inject a bare
        // header into the volatile lane.
        let lines = vec![
            "[session:abc] Recent conversation: foo".to_string(),
            "[session-memory:v1] # Session Title hi".to_string(),
            "None".to_string(),
        ];
        assert!(
            build_memory_section(&lines).is_none(),
            "all-noise input must return None, not an empty-body section"
        );
    }

    #[test]
    fn build_memory_section_keeps_structured_tagged_entries() {
        // `[@ns/type] body` entries always pass — they carry structured
        // semantics even when short. Parallel to the behavior in
        // `build_memory_entries`.
        let lines = vec!["[@swap/archived] Turns 1-1 swapped out".to_string()];
        let section = build_memory_section(&lines).expect("tagged entry survives");
        assert!(
            section.contains("Archived Context") || section.contains("[@swap/archived]"),
            "structured-tagged entries must pass through, got: {section}"
        );
    }

    #[test]
    fn build_memory_section_drops_structured_session_namespace_entries() {
        let lines = vec![
            "[@session/active] session_id=other\nrecent work".to_string(),
            "[@session/memory] session_id=legacy\nlegacy body".to_string(),
            "[cross-session memory] [@session/active] stale active task from another session"
                .to_string(),
            "[@pref/active] prefer Rust".to_string(),
        ];
        let section = build_memory_section(&lines).expect("pref entry survives");
        assert!(!section.contains("[@session/active]"), "got: {section}");
        assert!(!section.contains("[@session/memory]"), "got: {section}");
        assert!(
            !section.contains("[cross-session memory]"),
            "source labels must not let session namespace entries bypass filtering: {section}"
        );
        assert!(section.contains("prefer Rust"), "got: {section}");
    }

    #[test]
    fn build_memory_entries_preserves_rank_as_relevance() {
        let lines = vec![
            "[@pref/active] prefer Rust".to_string(),
            "[@fact/semantic] project is astra".to_string(),
        ];
        let entries = build_memory_entries(&lines);

        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].relevance_score > entries[1].relevance_score,
            "retrieval order should become relevance for binder ranking: {entries:?}"
        );
        assert!(entries[0].content.contains("Preferences"));
        assert_eq!(entries[0].source.as_deref(), Some("memoria.prefetch"));
    }

    #[test]
    fn build_memory_entries_drop_structured_session_namespace_entries() {
        let lines = vec![
            "[@session/active] current session memory".to_string(),
            "[cross-session memory] [@session/active] stale active session memory".to_string(),
            "[@pref/active] prefer Rust".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("prefer Rust"));
    }

    // ── Low-signal fragment filter ────────────────────────────────────
    // Memoria sometimes indexes L1 session-memory documents per-line,
    // so retrieval can return markdown headers / empty section labels
    // as individual "memories". These are noise — they bloat the
    // volatile block (observed 4.7K chars / 1,183 tok in session
    // ec35c711) without carrying retrievable signal. Drop them at ingress.

    #[test]
    fn build_memory_entries_drops_lone_markdown_headers() {
        let lines = vec![
            "# Session Title".to_string(),
            "## Task Specification".to_string(),
            "### Current State".to_string(),
            "User prefers Rust for CLI work".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        let contents: Vec<&str> = entries.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(
            entries.len(),
            1,
            "only the real memory should survive: {contents:?}"
        );
        assert!(entries[0].content.contains("prefers Rust"));
    }

    #[test]
    fn build_memory_entries_drops_fragmented_l1_wrappers() {
        // These appear when Memoria chunks an L1 session-memory doc.
        let lines = vec![
            "[session-memory:v1]".to_string(),
            "None".to_string(),
            "🔄 In progress".to_string(),
            "Tools used: none".to_string(),
            "Turn 1".to_string(),
            "Turn 1, active".to_string(),
            "Turn 1, ~0K tokens".to_string(),
            "hi".to_string(), // single-word user echoes
            "ok".to_string(),
            "真实的 memory fragment with actual content we want to keep".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(
            entries.len(),
            1,
            "only the substantive entry should survive"
        );
        assert!(entries[0].content.contains("真实"));
    }

    #[test]
    fn build_memory_entries_keeps_short_but_meaningful_tagged_entries() {
        // Tagged entries with [@ns/type] prefix are structured — keep even if short.
        let lines = vec![
            "[@pref/active] dark mode".to_string(),
            "[@fact/semantic] astra = CLI tool".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn build_memory_entries_drops_near_duplicates_after_normalization() {
        // Different surface but same signal — we already had trailing-
        // punctuation dedup in memoria_insights.rs, apply the same here.
        let lines = vec![
            "OceanBase is a distributed HTAP database".to_string(),
            "OceanBase is a distributed HTAP database.".to_string(), // trailing dot
            "oceanbase IS a DISTRIBUTED HTAP database".to_string(),  // case-varied
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(entries.len(), 1, "near-duplicates should collapse");
    }

    // ── Runtime-scaffolding echo filter (session 6676c7b5 feedback loop)
    // Memoria indexes conversation history, which includes runtime-
    // injected nudges/corrections/directives. When retrieved they are
    // unstructured text (no `[@ns/type]` tag, no `[session:…]` prefix)
    // and slip through every prior filter. Session 6676c7b5 turn 4 saw
    // a 6,397c `## User Memories` block with 78 such entries, crowding
    // out real memories and tripling the volatile lane.

    #[test]
    fn build_memory_entries_drops_runtime_tool_roll_up() {
        let lines = vec![
            "Tools used: read_file, bash, glob".to_string(),
            "Tools used: memory, skill".to_string(),
            "Real memory: prefer RS256 for JWT signing.".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(entries.len(), 1, "only real memory survives");
        assert!(entries[0].content.contains("RS256"));
    }

    #[test]
    fn build_memory_entries_drops_parallel_feedback_nudge() {
        let lines = vec![
            "✓ Previous round: 2 tools executed in parallel — excellent. Keep batching independent operations.".to_string(),
            "User values parallel tool execution for speed.".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]
                .content
                .contains("parallel tool execution for speed")
        );
    }

    #[test]
    fn build_memory_entries_drops_runtime_correction_headers() {
        let lines = vec![
            "## ⤴ Execution Escalation Runtime correction: you have made 10 read-only tool calls"
                .to_string(),
            "## ⤴ Parallel Batching Force Runtime correction: your last 3 rounds each ran"
                .to_string(),
            "## ⤴ Repeated Cached Tool Calls Detected".to_string(),
            "## ⚠ Sequential Tool Calls Detected".to_string(),
            "Project uses Cargo workspaces for build organization.".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(
            entries.len(),
            1,
            "runtime correction headers must be filtered out, keeping only real memory"
        );
        assert!(entries[0].content.contains("Cargo workspaces"));
    }

    #[test]
    fn build_memory_entries_drops_legacy_scaffolding_echo() {
        let lines = vec![
            "[Active task attachment] Resume the active task/thread below unless the user explicitly changes topic.".to_string(),
            "[Self-check — round 12] You have been reading/exploring for 12 consecutive rounds".to_string(),
            "User confirmed the decision to merge branch X into main.".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("confirmed the decision"));
    }

    #[test]
    fn build_memory_entries_drops_verification_and_error_budget_directives() {
        let lines = vec![
            "⚠️ VERIFICATION REQUIRED: Before you finish, run these checks using the bash tool"
                .to_string(),
            "🔄 ERROR BUDGET EXHAUSTED: You've hit Unknown errors 3 turns in a row.".to_string(),
            "♻ Duplicate calls detected: [read_file (3x)]. You've made identical calls".to_string(),
            "Real insight: parallelize independent read_file calls when exploring repo".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(
            entries.len(),
            1,
            "only the legit insight should survive, got {entries:?}"
        );
        assert!(entries[0].content.contains("Real insight"));
    }

    #[test]
    fn build_memory_section_drops_all_scaffolding_returns_none() {
        // A whole session's worth of scaffolding echoes should collapse
        // to no section at all, rather than produce an empty-body
        // `## User Memories\n` header.
        let lines = vec![
            "Tools used: bash, grep, read_file".to_string(),
            "✓ Previous round: 3 tools executed in parallel — excellent.".to_string(),
            "## ⤴ Execution Escalation Runtime correction:".to_string(),
            "[Active task attachment] Resume the active task/thread below".to_string(),
        ];
        assert!(
            build_memory_section(&lines).is_none(),
            "pure-scaffolding input must not produce a `## User Memories` section"
        );
    }

    #[test]
    fn entity_query_differs_from_mixed_language_input() {
        let msg = "memoria 最新的ci?";
        let entity = extract_entity_tokens(msg);
        assert_ne!(
            entity,
            msg.trim(),
            "entity query should differ for mixed-language"
        );
        assert_eq!(entity, "memoria");
    }

    #[test]
    fn entity_query_same_for_pure_ascii() {
        let msg = "memoria latest ci";
        let entity = extract_entity_tokens(msg);
        assert_eq!(
            entity, "memoria latest",
            "pure ASCII: entity ≈ original (minus short words)"
        );
    }

    #[tokio::test]
    async fn prefetch_memories_empty_key_returns_default() {
        let result = prefetch_memories("http://localhost", "", "query", "user1", 5, None).await;
        assert!(result.section.is_none());
        assert_eq!(result.items, 0);
    }

    #[tokio::test]
    async fn prefetch_memories_whitespace_message_returns_default() {
        let result = prefetch_memories("http://localhost", "key", "   ", "user1", 5, None).await;
        assert!(result.section.is_none());
        assert_eq!(result.items, 0);
    }

    #[tokio::test]
    async fn prefetch_memories_filters_session_suppressed_ids() {
        let sid = "prefetch-suppression-test";
        astra_tools::memoria::MemoriaClient::reset_suppressed(sid);
        astra_tools::memoria::MemoriaClient::suppress_memory(sid, "mem-suppressed");
        let body = r#"[
            {
                "memory_id": "mem-suppressed",
                "content": "Real insight: suppressed memory should not be injected",
                "memory_type": "semantic",
                "retrieval_score": 0.99
            },
            {
                "memory_id": "mem-visible",
                "content": "Real insight: visible memory should be injected",
                "memory_type": "semantic",
                "retrieval_score": 0.5
            }
        ]"#;
        let base_url = serve_memory_response(body, 1).await;

        let result =
            prefetch_memories(&base_url, "test-key", "memory topic", "user1", 5, Some(sid)).await;

        astra_tools::memoria::MemoriaClient::reset_suppressed(sid);
        let section = result.section.expect("visible memory should remain");
        assert!(!section.contains("suppressed memory"));
        assert!(section.contains("visible memory"));
        assert_eq!(result.items, 1);
    }

    #[tokio::test]
    async fn prefetch_session_start_empty_key_is_noop() {
        let result = prefetch_session_start_memories("http://localhost", "", "user1").await;
        assert!(result.section.is_none());
        assert!(result.profile.is_empty());
        assert!(result.recent_episodes.is_empty());
    }

    #[test]
    fn session_start_block_none_when_empty() {
        let out = build_session_start_block(&[], &[], &[]);
        assert!(out.is_none());
    }

    #[test]
    fn session_start_block_emits_all_sections_when_populated() {
        fn mem(id: &str, ty: &str, c: &str) -> RankableMemory {
            RankableMemory {
                memory_id: id.into(),
                memory_type: ty.into(),
                content: c.into(),
                retrieval_score: Some(0.8),
                trust_tier: None,
                ..Default::default()
            }
        }
        let profile = vec![mem("p1", "profile", "[user] prefers Rust")];
        let episodes = vec![mem(
            "e1",
            "episodic",
            "[episode] turn=5, finished auth refactor",
        )];
        let scenes = vec![mem(
            "s1",
            "semantic",
            "[scene:auth] recurring scope of OAuth fixes",
        )];
        let out = build_session_start_block(&profile, &episodes, &scenes).expect("non-empty");
        assert!(out.starts_with("<session_memory>"));
        assert!(out.ends_with("</session_memory>"));
        assert!(out.contains("### User profile"));
        assert!(out.contains("### Recent sessions"));
        assert!(out.contains("### Recurring scenes"));
        assert!(out.contains("prefers Rust"));
        assert!(out.contains("auth refactor"));
        assert!(out.contains("OAuth fixes"));
    }

    #[test]
    fn memory_index_block_none_when_empty() {
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(build_memory_index_block(&[], &empty).is_none());
    }

    fn ri(id: &str, ty: &str, content: &str) -> RankableMemory {
        RankableMemory {
            memory_id: id.into(),
            content: content.into(),
            memory_type: ty.into(),
            retrieval_score: Some(0.5),
            ..Default::default()
        }
    }

    #[test]
    fn memory_index_block_sorts_by_memory_id_for_cache_stability() {
        // Same set, returned in different order from Memoria. Rendered
        // block bytes must match — otherwise cross-turn cache misses.
        let items_a = vec![
            ri("m-z", "profile", "later-id"),
            ri("m-a", "feedback", "earliest-id"),
            ri("m-m", "project", "middle-id"),
        ];
        let items_b = vec![
            ri("m-m", "project", "middle-id"),
            ri("m-a", "feedback", "earliest-id"),
            ri("m-z", "profile", "later-id"),
        ];
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(
            build_memory_index_block(&items_a, &empty),
            build_memory_index_block(&items_b, &empty),
            "same set in different order must render identical bytes"
        );
    }

    #[test]
    fn memory_index_block_excludes_ids_in_session_memory() {
        let items = vec![
            ri("m1", "profile", "user prefers Rust"),
            ri("m2", "episodic", "[episode] last session"),
            ri("m3", "project", "milestone M3"),
        ];
        let mut excl = std::collections::HashSet::new();
        excl.insert("m2".to_string());
        let block = build_memory_index_block(&items, &excl).expect("non-empty");
        assert!(
            !block.contains("m2"),
            "excluded id must not appear: {block}"
        );
        assert!(block.contains("m1"));
        assert!(block.contains("m3"));
    }

    #[test]
    fn memory_index_block_none_when_everything_excluded() {
        let items = vec![ri("m1", "profile", "only entry")];
        let mut excl = std::collections::HashSet::new();
        excl.insert("m1".to_string());
        assert!(
            build_memory_index_block(&items, &excl).is_none(),
            "index is None when every id is excluded"
        );
    }

    #[test]
    fn session_stable_block_concatenates_index_then_session_start() {
        let idx = "<memory_index>\n- [profile] m1: x\n</memory_index>";
        let start = "<session_memory>\n### User profile\n- y\n</session_memory>";
        let out = build_session_stable_memory_block(Some(idx), Some(start)).expect("non-empty");
        assert!(out.starts_with("<memory_index>"));
        assert!(out.contains("</memory_index>"));
        assert!(out.contains("<session_memory>"));
    }

    #[test]
    fn session_stable_block_is_none_when_both_empty() {
        assert!(build_session_stable_memory_block(None, None).is_none());
    }

    #[test]
    fn session_stable_block_passes_through_single_source() {
        let idx = "<memory_index>\n- [p] x\n</memory_index>";
        assert_eq!(
            build_session_stable_memory_block(Some(idx), None).unwrap(),
            idx
        );
        let start = "<session_memory>\n### p\n- y\n</session_memory>";
        assert_eq!(
            build_session_stable_memory_block(None, Some(start)).unwrap(),
            start
        );
    }

    #[test]
    fn session_start_exposed_ids_collects_profile_episodes_and_scenes() {
        let result = SessionStartPrefetchResult {
            section: None,
            profile: vec![ri("p1", "profile", "p"), ri("p2", "profile", "q")],
            recent_episodes: vec![ri("e1", "episodic", "e")],
            recent_scenes: vec![ri("s1", "semantic", "[scene] x")],
            fetch_ms: 0,
        };
        let ids = session_start_exposed_ids(&result);
        assert_eq!(ids.len(), 4);
        assert!(ids.contains("p1"));
        assert!(ids.contains("p2"));
        assert!(ids.contains("e1"));
        assert!(ids.contains("s1"));
    }

    #[test]
    fn filter_entries_already_surfaced_drops_seen_contents() {
        let entries = vec![
            ContextMemoryEntry::scored("user prefers Rust", 1.0),
            ContextMemoryEntry::scored("use real DB in tests", 0.9),
            ContextMemoryEntry::scored("milestone M3 ships 2026-06-01", 0.8),
        ];
        let mut seen = std::collections::HashSet::new();
        seen.insert(memory_dedup_key("user prefers Rust"));
        seen.insert(memory_dedup_key("milestone M3 ships 2026-06-01"));
        let filtered = filter_entries_already_surfaced(entries, &seen);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].content.contains("real DB"));
    }

    #[test]
    fn filter_entries_already_surfaced_is_identity_on_empty_seen() {
        let entries = vec![ContextMemoryEntry::scored("something", 0.5)];
        let empty = std::collections::HashSet::new();
        let out = filter_entries_already_surfaced(entries, &empty);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn collect_seen_contents_case_folds_and_normalizes() {
        let entries = vec![
            ContextMemoryEntry::scored("  Hello  World .", 0.5),
            ContextMemoryEntry::scored("hello world", 0.5),
        ];
        let seen = collect_seen_contents(&entries);
        assert_eq!(
            seen.len(),
            1,
            "case-fold + punctuation strip must collapse both into one"
        );
        assert!(seen.contains("hello world"));
    }

    #[test]
    fn memory_index_block_renders_entries() {
        fn mk(id: &str, ty: &str, c: &str) -> RankableMemory {
            RankableMemory {
                memory_id: id.into(),
                content: c.into(),
                memory_type: ty.into(),
                retrieval_score: Some(0.5),
                ..Default::default()
            }
        }
        let items = vec![
            mk("m1", "profile", "user prefers Rust"),
            mk("m2", "feedback", "use real DB in tests"),
            mk("m3", "project", "merge freeze 2026-05-08"),
        ];
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let out = build_memory_index_block(&items, &empty).expect("non-empty");
        assert!(out.starts_with("<memory_index>"));
        assert!(out.ends_with("</memory_index>"));
        assert!(out.contains("- [profile] m1: user prefers Rust"));
        assert!(out.contains("- [feedback] m2: use real DB in tests"));
        assert!(out.contains("- [project] m3: merge freeze 2026-05-08"));
    }

    #[test]
    fn memory_index_block_truncates_long_content() {
        let items = vec![RankableMemory {
            memory_id: "big".into(),
            content: "x".repeat(500),
            memory_type: "semantic".into(),
            retrieval_score: Some(0.5),
            ..Default::default()
        }];
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let out = build_memory_index_block(&items, &empty).expect("non-empty");
        // Entry line must cap at 100-char abstract + a bit of scaffold;
        // total under ~180 chars.
        let entry_line = out
            .lines()
            .find(|l| l.starts_with("- ["))
            .expect("has entry line");
        assert!(
            entry_line.chars().count() <= 200,
            "entry line too long: {} chars",
            entry_line.chars().count()
        );
    }

    #[tokio::test]
    async fn prefetch_memory_index_empty_key_is_noop() {
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            prefetch_memory_index("http://localhost", "", "user1", &empty)
                .await
                .is_none()
        );
    }

    #[test]
    fn session_start_line_respects_budget_with_ellipsis() {
        let long = "a".repeat(500);
        let out = session_start_line(&long, 120);
        assert!(out.chars().count() <= 120);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn memory_prefetch_result_default() {
        let r = MemoryPrefetchResult::default();
        assert!(r.section.is_none());
        assert!(r.entries.is_empty());
        assert_eq!(r.items, 0);
        assert!(r.preview.is_empty());
        assert_eq!(r.fetch_ms, 0);
    }

    /// audit-A2: fetch_memories must time out on an unresponsive Memoria server
    /// instead of blocking the turn pipeline indefinitely.
    #[tokio::test]
    async fn fetch_memories_times_out_on_unresponsive_server() {
        // Shorten the production 5s/10s timeouts to 500ms/200ms so the
        // black-hole behaviour manifests within the per-case test budget.
        TEST_FETCH_MEMORIES_TIMEOUTS.with(|c| {
            *c.borrow_mut() = Some((
                std::time::Duration::from_millis(500),
                std::time::Duration::from_millis(200),
            ));
        });
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_FETCH_MEMORIES_TIMEOUTS.with(|c| *c.borrow_mut() = None);
            }
        }
        let _reset = Reset;

        // Black-hole server: accepts connections, never responds.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    drop(sock);
                });
            }
        });

        let start = std::time::Instant::now();
        let result = fetch_memories(
            &format!("http://{addr}"),
            "test-key",
            "test query",
            "user1",
            5,
        )
        .await;
        let elapsed = start.elapsed();

        // fetch_memories returns empty string on error, not Err
        assert!(result.is_empty(), "should return empty on timeout");
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "should time out well before 30s, took {elapsed:?}"
        );
    }

    // ── Task 4: prefetch source labels ───────────────────────────────────

    #[test]
    fn source_label_procedural_with_session_id_is_learnings() {
        let label = source_label("procedural", Some("abc123"));
        assert_eq!(label, Some("[learnings from session abc123]".to_string()));
    }

    #[test]
    fn source_label_semantic_with_session_id_is_cross_session() {
        let label = source_label("semantic", Some("abc"));
        assert_eq!(label, Some("[cross-session memory]".to_string()));
    }

    #[test]
    fn source_label_no_session_id_returns_none() {
        assert!(source_label("semantic", None).is_none());
    }

    #[test]
    fn source_label_empty_memory_type_returns_none() {
        assert!(source_label("", None).is_none());
    }

    #[test]
    fn source_label_empty_session_id_returns_none() {
        assert!(source_label("procedural", Some("")).is_none());
    }
}
