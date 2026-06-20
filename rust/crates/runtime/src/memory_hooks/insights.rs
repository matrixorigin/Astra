//! Memoria insights digest.
//!
//! Compact prompt fragment rendered from Memoria recall hits so the agent
//! sees "what it previously learned" in its own system prompt.
//!
//! Kept as a pure function to be unit-testable and reused by both CLI
//! `agentic_loop_turn.rs` (producer) and runtime `bridge_inprocess.rs`
//! (consumer) without circular deps.

const MAX_BULLETS: usize = 4;
const MAX_CHARS_PER_BULLET: usize = 180;
const MIN_CONTENT_LEN: usize = 8;
const SECTION_HEADER: &str = "## Memoria Recall";

/// Render a bulleted digest from raw memory hit contents.
///
/// Returns `None` when there is nothing substantive to surface (empty
/// input, all hits too short, or every bullet got squashed to whitespace).
/// The caller is expected to inject the returned text verbatim into the
/// system prompt (usually prefixed by a blank line).
pub fn render_digest(contents: &[String]) -> Option<String> {
    let mut bullets: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw in contents {
        if bullets.len() >= MAX_BULLETS {
            break;
        }
        let cleaned = compact_one_line(raw);
        if cleaned.len() < MIN_CONTENT_LEN {
            continue;
        }
        // Drop L1-protocol and session-replay fragments that Memoria
        // sometimes surfaces from indexed session-memory docs. Mirrors the
        // filter in `turn::memory_prefetch::is_memory_worthy` so the CLI-
        // generated `## Memoria Recall` and the bridge-generated
        // `## User Memories` both reject the same noise.
        if !is_digest_worthy(&cleaned) {
            continue;
        }
        // Decode business type prefix for categorized display.
        let (cat, body) = astra_prompts::memory_types::decode(&cleaned);
        // Dedup on the decoded body (category-agnostic, case-insensitive,
        // trailing-punctuation-insensitive). Memoria often surfaces the
        // same underlying memory twice — once via the full-message query,
        // once via the entity-keyword query — sometimes with a different
        // category prefix or punctuation drift. Hashing on `cleaned`
        // alone (the previous behaviour) let those slip through.
        let key = dedup_key(body);
        if !seen.insert(key) {
            continue;
        }
        let label = match cat {
            Some(c) => format!(
                "[{}] ",
                c.content_prefix().trim_matches(|ch| ch == '[' || ch == ']')
            ),
            None => String::new(),
        };
        bullets.push(format!(
            "{label}{}",
            truncate_with_ellipsis(body, MAX_CHARS_PER_BULLET)
        ));
    }
    if bullets.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(SECTION_HEADER.len() + bullets.len() * 64);
    out.push_str(SECTION_HEADER);
    out.push('\n');
    for b in bullets {
        out.push_str("- ");
        out.push_str(&b);
        out.push('\n');
    }
    Some(out.trim_end().to_string())
}

/// Reject low-signal fragments before adding them to the Memoria Recall
/// digest. Same intent as `turn::memory_prefetch::is_memory_worthy`; kept as a
/// second copy because the two surfaces live in different crates and the
/// filter is intentionally lenient (Recall has a hard `MAX_BULLETS=4` cap, so
/// the cost of letting noise through is an entire useful bullet).
fn is_digest_worthy(line: &str) -> bool {
    // Session-namespace entries are not generic cross-session memories:
    // they represent active/current session state and must flow only
    // through the dedicated session-memory lane.
    if astra_prompts::memory_proto::is_session_namespace_memory(line) {
        return false;
    }

    // Structured tagged entries (`[@ns/type] body`) always pass — the
    // namespace already asserts meaning.
    if line.starts_with("[@") && line.contains('/') && line.contains(']') {
        return true;
    }
    // L1 session-memory / attention protocol markers: these are replayed
    // verbatim from `[session-memory:v1] # Session Title hi # Task …`
    // indexed per-line and burn a whole bullet on scaffolding.
    if line.starts_with("[session-memory:") || line.starts_with("[attention:") {
        return false;
    }
    // Session-replay lines like `[session:abc] Recent conversation: …`
    // carry inline transcript dumps that bloat the bullet and repeat the
    // conversation history the model already has.
    if line.starts_with("[session:") {
        return false;
    }
    // Legacy pre-L2 auto-write shapes. L2 now rejects these at write,
    // but entries that landed *before* the gate shipped still live in
    // Memoria. Observed dominant pollution in session 72ef747f: 43 of
    // 68 User Memories entries were `[compaction:<sid>] …` from before
    // L2. Parity with `memory_prefetch::is_memory_worthy`.
    if line.starts_with("[compaction:") || line.starts_with("[session-knowledge:") {
        return false;
    }
    // Runtime scaffolding that leaked into conversation history and was
    // then indexed by Memoria. Routed through the single source of truth
    // in `astra_turn_types::scaffolding_body_prefixes_for_filtering` so new runtime
    // injections added there are automatically filtered here without a
    // per-site update. Pinned after observing the feedback loop in
    // session 6676c7b5 where 78 runtime-correction echoes filled a
    // 6,397c `## User Memories` block.
    for prefix in astra_turn_types::scaffolding_body_prefixes_for_filtering() {
        if line.starts_with(prefix) {
            return false;
        }
    }
    true
}

fn compact_one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

/// Build the dedup key for a digest bullet.
///
/// Normalizes:
/// - Unicode case (to_lowercase) so "RS256" == "rs256".
/// - Trailing punctuation commonly drifted across imports (`.`, `!`, `?`,
///   `;`, `:`, `,`).
/// - Surrounding whitespace (the body has already been whitespace-
///   collapsed by `compact_one_line`, but `decode` may leave a leading
///   space after stripping the prefix).
fn dedup_key(body: &str) -> String {
    body.trim()
        .trim_end_matches(['.', '!', '?', ';', ':', ','])
        .to_lowercase()
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_empty_and_too_short() {
        assert!(render_digest(&[]).is_none(), "empty");
        assert!(render_digest(&["hi".to_string()]).is_none(), "too short");
        assert!(
            render_digest(&["hi".to_string(), "ok".to_string()]).is_none(),
            "all too short"
        );
    }

    #[test]
    fn renders_header_and_bullets() {
        let hits = vec![
            "User prefers Rust for CLI work.".to_string(),
            "Astra should always run tests before committing.".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert!(out.starts_with("## Memoria Recall"));
        assert!(out.contains("- User prefers Rust"));
        assert!(out.contains("- Astra should always run tests"));
    }

    #[test]
    fn deduplicates_identical_content() {
        let hits = vec![
            "Always run `cargo test` before commit.".to_string(),
            "Always run `cargo test` before commit.".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert_eq!(out.matches("Always run").count(), 1);
    }

    #[test]
    fn truncates_long_hit_with_ellipsis() {
        let long = "x".repeat(500);
        let out = render_digest(&[long]).expect("digest");
        assert!(out.contains('…'));
        let bullet_len = out
            .lines()
            .find(|l| l.starts_with("- "))
            .map(|l| l.chars().count())
            .unwrap_or(0);
        // "- " + MAX_CHARS_PER_BULLET chars max
        assert!(bullet_len <= 2 + MAX_CHARS_PER_BULLET);
    }

    #[test]
    fn caps_bullet_count() {
        let hits: Vec<String> = (0..10)
            .map(|i| format!("Insight number {i} with enough text to pass filter."))
            .collect();
        let out = render_digest(&hits).expect("digest");
        let n = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(n, MAX_BULLETS);
    }

    #[test]
    fn collapses_whitespace() {
        let hits = vec!["multi\n  line\n\t\tcontent with gaps".to_string()];
        let out = render_digest(&hits).expect("digest");
        assert!(out.contains("- multi line content with gaps"));
    }

    #[test]
    fn categorized_rendering_with_business_prefix() {
        let hits = vec![
            "[feedback] always use RS256 for JWT signing".to_string(),
            "[user] senior Rust engineer, prefers CLI tools".to_string(),
            "plain legacy memory without prefix".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert!(
            out.contains("[feedback] always use RS256"),
            "should show category label for typed memories"
        );
        assert!(
            out.contains("[user] senior Rust"),
            "should show category label for user memories"
        );
        assert!(
            out.contains("- plain legacy memory"),
            "legacy memories should render without label"
        );
    }

    #[test]
    fn categorized_rendering_strips_prefix_from_body() {
        let hits = vec!["[project] merge freeze starts May 8th for mobile release".to_string()];
        let out = render_digest(&hits).expect("digest");
        assert!(
            !out.contains("[project] [project]"),
            "should not double the prefix"
        );
        assert!(out.contains("[project] merge freeze"));
    }

    // ── Semantic dedup: same body surfaced under different shapes should
    //    collapse to a single bullet. Memoria's retrieval can return the
    //    same underlying memory twice when hybrid queries (full message +
    //    entity keywords) both hit it with different stored category
    //    prefixes or punctuation.

    #[test]
    fn dedup_same_body_different_category_prefix() {
        // Same body body stored once as `[feedback]`, once as `[user]`.
        let hits = vec![
            "[feedback] OceanBase is a distributed HTAP database".to_string(),
            "[user] OceanBase is a distributed HTAP database".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count, 1,
            "expected same body under different prefixes to dedupe, got:\n{out}"
        );
    }

    #[test]
    fn dedup_same_body_prefixed_vs_legacy() {
        // Same body once with a typed prefix, once unprefixed (legacy import).
        let hits = vec![
            "[lesson] Always run `cargo test` before commit".to_string(),
            "Always run `cargo test` before commit".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count, 1,
            "expected prefixed vs legacy with same body to dedupe, got:\n{out}"
        );
    }

    #[test]
    fn dedup_same_body_trailing_punctuation() {
        // Same body with and without trailing period.
        let hits = vec![
            "OceanBase is a distributed HTAP database.".to_string(),
            "OceanBase is a distributed HTAP database".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count, 1,
            "expected trailing-punctuation variants to dedupe, got:\n{out}"
        );
    }

    #[test]
    fn dedup_case_insensitive_after_prefix_strip() {
        // Same body with case drift — already handled pre-fix because of
        // to_lowercase(), but pin the behaviour so a future refactor
        // doesn't regress it.
        let hits = vec![
            "[feedback] Use RS256 for JWT".to_string(),
            "[feedback] use rs256 for jwt".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count, 1,
            "case drift should still dedupe, got:\n{out}"
        );
    }

    #[test]
    fn dedup_preserves_first_seen_prefix() {
        // When two variants collapse, keep the first (higher-ranked) one.
        let hits = vec![
            "[feedback] OceanBase is a distributed HTAP database".to_string(),
            "[user] OceanBase is a distributed HTAP database".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert!(
            out.contains("[feedback]"),
            "first-seen category label should win, got:\n{out}"
        );
        assert!(
            !out.contains("[user]"),
            "losing category label should not appear, got:\n{out}"
        );
    }

    #[test]
    fn distinct_bodies_not_collapsed() {
        // Guardrail against over-collapsing: different content should stay
        // separate even if they share a prefix.
        let hits = vec![
            "[feedback] Use RS256 for JWT".to_string(),
            "[feedback] Use HS512 for JWT".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        let bullet_count = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count, 2,
            "distinct bodies must not collapse, got:\n{out}"
        );
    }

    // ── Noise rejection: L1 protocol markers and session-replay fragments

    #[test]
    fn test_rejects_noise_markers() {
        let noise_cases: &[(&str, &str)] = &[
            (
                "[session-memory:v1] # Session Title hi",
                "session-memory:v1",
            ),
            ("[attention:v1] turn budget 2k", "attention:v1"),
            (
                "[session:abc123] Recent conversation: Assistant: Step 15 done.",
                "[session:",
            ),
            (
                "[compaction:055932d7-22bd] ### Primary Request: review PR 42",
                "[compaction:",
            ),
            (
                "[session-knowledge:abc] ## User Corrections - Use RS256",
                "[session-knowledge:",
            ),
        ];
        let real = "Real fact that should survive.".to_string();
        for (noise, needle) in noise_cases {
            let hits = vec![noise.to_string(), real.clone()];
            let out = render_digest(&hits).expect("digest");
            assert!(
                !out.contains(needle),
                "noise marker '{needle}' leaked: {out}"
            );
            assert!(
                out.contains("Real fact"),
                "real fact missing for '{needle}'"
            );
        }
    }

    #[test]
    fn test_keeps_valid_entries() {
        // [@ns/type] structured tags must pass.
        assert!(render_digest(&["[@swap/archived] Turns 1-1 swapped".to_string()]).is_some());
        // [@session] and [@pref] filtering: session namespace rejected, others kept.
        let hits = vec![
            "[@session/active] Session abc: Turn 4".to_string(),
            "[@session/memory] session_id=xyz # Session Memory".to_string(),
            "[@pref/active] prefer Rust for CLI work".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert!(
            !out.contains("[@session/active]"),
            "leaked session/active: {out}"
        );
        assert!(
            !out.contains("[@session/memory]"),
            "leaked session/memory: {out}"
        );
        assert!(
            out.contains("prefer Rust for CLI work"),
            "missing pref: {out}"
        );
    }
}
