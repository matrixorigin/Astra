//! Memory prefetch utilities for LLM prompt augmentation.
//!
//! Provides hybrid retrieval (full message + entity-keyword) from Memoria HTTP API,
//! merging and deduplicating results into a structured section for injection into
//! the system prompt.

use std::time::Instant;

use astra_turn_core::context_sources::MemoryEntry as ContextMemoryEntry;

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
pub async fn prefetch_memories(
    mem_url: &str,
    mem_key: &str,
    user_msg: &str,
    user_id: &str,
    top_k: u32,
) -> MemoryPrefetchResult {
    if mem_key.is_empty() || user_msg.trim().is_empty() {
        return MemoryPrefetchResult::default();
    }
    let started = Instant::now();
    let entity_query = extract_entity_tokens(user_msg);
    let trimmed_msg = user_msg.trim();

    // Parallel fetch: full message retrieval + entity-keyword retrieval via tokio::join!
    let do_entity = !entity_query.is_empty() && entity_query != trimmed_msg;
    let (full_result, entity_result) = tokio::join!(
        fetch_memories(mem_url, mem_key, trimmed_msg, user_id, top_k),
        async {
            if do_entity {
                fetch_memories(mem_url, mem_key, &entity_query, user_id, top_k).await
            } else {
                String::new()
            }
        }
    );
    let merged = merge_memory_results(&[&full_result, &entity_result]);
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

/// Merge and deduplicate memory results from multiple retrieval queries.
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
    // Always keep structured-tagged entries.
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
    {
        return false;
    }

    // Reject well-known low-signal fragments.
    const NOISE_EXACT: &[&str] = &[
        "None",
        "(none)",
        "Tools used: none",
        "🔄 In progress",
    ];
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
    const SCAFFOLDING_PREFIXES: &[&str] = &[
        "Tools used:",                     // per-turn tool-call roll-up
        "[Active task attachment]",        // attention manifest
        "[Self-check",                     // runtime self-check directive
        "✓ Previous round:",              // parallel-feedback nudge
        "♻ Duplicate calls detected",      // dedup nudge
        "⚠️ VERIFICATION REQUIRED",        // runtime verification directive
        "## ⤴",                           // runtime correction headers (escalation, batching force, repeated-cache)
        "## ⚠",                           // runtime warning headers (sequential, cascade)
        "🔄 ERROR BUDGET",                // error-budget exhaustion directive
        "Runtime correction:",             // inline correction prefix
    ];
    for prefix in SCAFFOLDING_PREFIXES {
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

/// Normalize a memory line for dedup: case-fold + strip trailing
/// punctuation + collapse whitespace. Matches the `memoria_insights`
/// dedup key so behaviour is consistent across the two surfaces.
fn memory_dedup_key(trimmed: &str) -> String {
    let collapsed: String = trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed
        .trim_end_matches(['.', '!', '?', ';', ':', ','])
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
async fn fetch_memories(
    base_url: &str,
    api_key: &str,
    query: &str,
    user_id: &str,
    top_k: u32,
) -> String {
    let (connect_timeout, request_timeout) = fetch_memories_timeouts();
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
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
            return String::new();
        }
    };
    if !resp.status().is_success() {
        return String::new();
    }
    let arr = match resp.json::<Vec<serde_json::Value>>().await {
        Ok(a) => a,
        Err(_) => return String::new(),
    };
    arr.iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(section.contains("Archived Context") || section.contains("[@swap/archived]"),
            "structured-tagged entries must pass through, got: {section}");
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
        assert_eq!(entries.len(), 1, "only the real memory should survive: {contents:?}");
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
        assert_eq!(entries.len(), 1, "only the substantive entry should survive");
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
            "oceanbase IS a DISTRIBUTED HTAP database".to_string(), // case-varied
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
        assert!(entries[0].content.contains("parallel tool execution for speed"));
    }

    #[test]
    fn build_memory_entries_drops_runtime_correction_headers() {
        let lines = vec![
            "## ⤴ Execution Escalation Runtime correction: you have made 10 read-only tool calls".to_string(),
            "## ⤴ Parallel Batching Force Runtime correction: your last 3 rounds each ran".to_string(),
            "## ⤴ Repeated Cached Tool Calls Detected".to_string(),
            "## ⚠ Sequential Tool Calls Detected".to_string(),
            "Project uses Cargo workspaces for build organization.".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(
            entries.len(), 1,
            "runtime correction headers must be filtered out, keeping only real memory"
        );
        assert!(entries[0].content.contains("Cargo workspaces"));
    }

    #[test]
    fn build_memory_entries_drops_attention_manifest_echo() {
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
            "⚠️ VERIFICATION REQUIRED: Before you finish, run these checks using the bash tool".to_string(),
            "🔄 ERROR BUDGET EXHAUSTED: You've hit Unknown errors 3 turns in a row.".to_string(),
            "♻ Duplicate calls detected: [read_file (3x)]. You've made identical calls".to_string(),
            "Real insight: parallelize independent read_file calls when exploring repo".to_string(),
        ];
        let entries = build_memory_entries(&lines);
        assert_eq!(
            entries.len(), 1,
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
        let result = prefetch_memories("http://localhost", "", "query", "user1", 5).await;
        assert!(result.section.is_none());
        assert_eq!(result.items, 0);
    }

    #[tokio::test]
    async fn prefetch_memories_whitespace_message_returns_default() {
        let result = prefetch_memories("http://localhost", "key", "   ", "user1", 5).await;
        assert!(result.section.is_none());
        assert_eq!(result.items, 0);
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
}
