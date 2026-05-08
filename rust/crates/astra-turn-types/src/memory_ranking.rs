//! L3: layered memory recall with trust-aware ranking.
//!
//! **Problem**: Memoria retrieval returns a flat list of memories
//! ordered by raw vector-similarity score. An auto-generated,
//! low-trust `episodic` summary with 0.72 score outranks a user-
//! confirmed `semantic` fact with 0.68 score, even though the fact
//! is more durable and much more likely to be useful. Observed in
//! session `c6e18730`: pollution entries like auto-compaction blobs
//! displaced curated learnings in the volatile `## User Memories`
//! block.
//!
//! **Fix**: re-rank retrieved memories by a **composite score** that
//! combines trust tier and raw similarity:
//!
//!     composite = raw_score * tier_weight(trust_tier)
//!
//! Tier weights are chosen so that a big trust gap can overcome a
//! small score gap, but not the reverse:
//!
//! | Tier              | Constant         | Weight |
//! | ----------------- | ---------------- | ------ |
//! | VERIFIED   (T1)   | user confirmed   | 1.00   |
//! | CURATED    (T2)   | session-end      | 0.85   |
//! | INFERRED   (T3)   | auto-compaction  | 0.55   |
//! | UNVERIFIED (T4)   | speculative      | 0.35   |
//! | (no tier)         | legacy           | 0.50   |
//!
//! This is a **pure re-ranker** — it runs after Memoria returns
//! results, doesn't change the retrieval call itself. The backend
//! stays unchanged; we just pick the right top_k from what it
//! returned.
//!
//! Companion concept: memory_type filtering. The caller can
//! pre-partition by `memory_type` (session-scoped `working` versus
//! persistent semantic/episodic/procedural/profile) before ranking,
//! so working memory doesn't compete with long-term facts.

/// Tier weight lookup. Recognized tier constants match
/// `astra_prompts::memory_proto::TIER_*`, but we redeclare here so
/// this crate stays prompt-independent.
///
/// Unknown / missing tier → `0.50` (middle of the scale, no bias
/// either way).
#[must_use]
pub fn tier_weight(trust_tier: Option<&str>) -> f64 {
    match trust_tier {
        Some("T1") => 1.00, // VERIFIED
        Some("T2") => 0.85, // CURATED
        Some("T3") => 0.55, // INFERRED
        Some("T4") => 0.35, // UNVERIFIED
        _ => 0.50,          // legacy / unknown
    }
}

/// Compute the composite ranking score used by [`sort_memories`] and
/// similar.
///
/// Returns 0.0 when `raw_score` is None (absent) or NaN; callers who
/// want to drop those rather than rank them can filter upstream.
#[must_use]
pub fn composite_score(raw_score: Option<f64>, trust_tier: Option<&str>) -> f64 {
    let raw = raw_score.unwrap_or(0.0);
    if raw.is_nan() {
        return 0.0;
    }
    raw * tier_weight(trust_tier)
}

/// A memory record exposing the fields needed for ranking. Callers
/// build this from their Memoria client's result type — we keep the
/// ranking layer free of crate dependencies so it can run in tests
/// and in any future memdir/file-based fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct RankableMemory {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    pub retrieval_score: Option<f64>,
    pub trust_tier: Option<String>,
}

/// Sort `memories` in-place by descending composite score.
///
/// Stable sort on (−composite_score, memory_id) so ties break
/// deterministically — same inputs produce the same output across
/// runs, which matters for prompt-cache prefix stability.
pub fn sort_memories(memories: &mut [RankableMemory]) {
    memories.sort_by(|a, b| {
        let sa = composite_score(a.retrieval_score, a.trust_tier.as_deref());
        let sb = composite_score(b.retrieval_score, b.trust_tier.as_deref());
        // Descending on score; ascending on memory_id as tiebreak.
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
}

/// Persistent memory types — long-lived, cross-session. Entries in
/// these types are candidates for the `## User Memories` volatile-
/// lane section.
pub const PERSISTENT_TYPES: &[&str] = &["semantic", "episodic", "procedural", "profile"];

/// Session-scoped memory type — auto-purged, only valid within the
/// originating session.
pub const SESSION_SCOPED_TYPE: &str = "working";

/// Is this memory_type one of the cross-session persistent kinds?
#[must_use]
pub fn is_persistent_type(memory_type: &str) -> bool {
    PERSISTENT_TYPES.contains(&memory_type)
}

/// Split memories into (persistent, session-scoped) partitions based
/// on `memory_type`. Memories with unknown types are placed in the
/// persistent partition by default (fail-open: don't silently drop
/// retrievals we don't fully understand).
///
/// Used so the caller can rank each bucket independently and then
/// allocate its budget (e.g. 4 persistent + 1 working in the volatile
/// lane) without cross-bucket interference.
#[must_use]
pub fn partition_by_scope(
    memories: Vec<RankableMemory>,
) -> (Vec<RankableMemory>, Vec<RankableMemory>) {
    let mut persistent = Vec::new();
    let mut session_scoped = Vec::new();
    for m in memories {
        if m.memory_type == SESSION_SCOPED_TYPE {
            session_scoped.push(m);
        } else {
            persistent.push(m);
        }
    }
    (persistent, session_scoped)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tier_weight ───────────────────────────────────────────────────

    #[test]
    fn tier_weight_known_tiers() {
        assert_eq!(tier_weight(Some("T1")), 1.00);
        assert_eq!(tier_weight(Some("T2")), 0.85);
        assert_eq!(tier_weight(Some("T3")), 0.55);
        assert_eq!(tier_weight(Some("T4")), 0.35);
    }

    #[test]
    fn tier_weight_unknown_or_missing_is_midpoint() {
        assert_eq!(tier_weight(None), 0.50);
        assert_eq!(tier_weight(Some("T99")), 0.50);
        assert_eq!(tier_weight(Some("")), 0.50);
    }

    // ── composite_score ──────────────────────────────────────────────

    #[test]
    fn composite_multiplies_raw_and_tier() {
        // T1 (1.0) × 0.8 raw = 0.8
        assert!((composite_score(Some(0.8), Some("T1")) - 0.8).abs() < 1e-9);
        // T3 (0.55) × 0.8 raw = 0.44
        assert!((composite_score(Some(0.8), Some("T3")) - 0.44).abs() < 1e-9);
    }

    #[test]
    fn composite_trust_gap_can_overcome_score_gap() {
        // Real scenario from session c6e18730: a T1 VERIFIED user
        // preference at 0.62 score should outrank a T3 INFERRED
        // compaction summary at 0.78 score — because the summary is
        // auto-generated pollution and the preference is durable.
        let verified = composite_score(Some(0.62), Some("T1")); // 0.62
        let inferred = composite_score(Some(0.78), Some("T3")); // 0.429
        assert!(verified > inferred, "T1@0.62 must beat T3@0.78");
    }

    #[test]
    fn composite_small_score_gap_does_not_flip_tiers() {
        // Inverse safety check: two T1 entries, the one with higher
        // raw score must win (tier equality → raw score decides).
        let a = composite_score(Some(0.9), Some("T1"));
        let b = composite_score(Some(0.8), Some("T1"));
        assert!(a > b);
    }

    #[test]
    fn composite_handles_missing_score() {
        // No retrieval_score → 0.0 composite. Matches "nothing to rank
        // on" more usefully than pretending it's relevant.
        assert_eq!(composite_score(None, Some("T1")), 0.0);
    }

    #[test]
    fn composite_handles_nan() {
        assert_eq!(composite_score(Some(f64::NAN), Some("T1")), 0.0);
    }

    // ── sort_memories ────────────────────────────────────────────────

    fn mk(id: &str, score: f64, tier: Option<&str>, mem_type: &str) -> RankableMemory {
        RankableMemory {
            memory_id: id.to_string(),
            content: format!("content for {id}"),
            memory_type: mem_type.to_string(),
            retrieval_score: Some(score),
            trust_tier: tier.map(str::to_string),
        }
    }

    #[test]
    fn sort_puts_higher_composite_first() {
        let mut mems = vec![
            mk("low-tier-high-score", 0.78, Some("T3"), "episodic"), // 0.429
            mk("high-tier-mid-score", 0.62, Some("T1"), "semantic"), // 0.62
            mk("mid-tier-mid-score", 0.70, Some("T2"), "procedural"), // 0.595
        ];
        sort_memories(&mut mems);
        assert_eq!(mems[0].memory_id, "high-tier-mid-score");
        assert_eq!(mems[1].memory_id, "mid-tier-mid-score");
        assert_eq!(mems[2].memory_id, "low-tier-high-score");
    }

    #[test]
    fn sort_is_deterministic_on_ties() {
        // Two memories with identical composite scores — sort_memories
        // must produce the same order across runs because prompt-cache
        // prefix stability depends on it.
        let mut a = vec![
            mk("z-id", 0.8, Some("T1"), "semantic"),
            mk("a-id", 0.8, Some("T1"), "semantic"),
        ];
        let mut b = vec![
            mk("a-id", 0.8, Some("T1"), "semantic"),
            mk("z-id", 0.8, Some("T1"), "semantic"),
        ];
        sort_memories(&mut a);
        sort_memories(&mut b);
        assert_eq!(a[0].memory_id, "a-id");
        assert_eq!(b[0].memory_id, "a-id");
    }

    #[test]
    fn sort_handles_missing_tier_at_midpoint_weight() {
        // Legacy entries (no trust_tier) should land between T2 and T3
        // because 0.50 is the default weight.
        let mut mems = vec![
            mk("t1", 1.0, Some("T1"), "semantic"),
            mk("legacy", 1.0, None, "semantic"),
            mk("t2", 1.0, Some("T2"), "semantic"),
            mk("t3", 1.0, Some("T3"), "semantic"),
            mk("t4", 1.0, Some("T4"), "semantic"),
        ];
        sort_memories(&mut mems);
        assert_eq!(mems[0].memory_id, "t1");
        assert_eq!(mems[1].memory_id, "t2");
        // T3=0.55, legacy=0.50
        assert_eq!(mems[2].memory_id, "t3");
        assert_eq!(mems[3].memory_id, "legacy");
        assert_eq!(mems[4].memory_id, "t4");
    }

    // ── partition_by_scope ────────────────────────────────────────────

    #[test]
    fn partition_splits_working_from_persistent() {
        let mems = vec![
            mk("a", 0.9, Some("T1"), "semantic"),
            mk("b", 0.8, Some("T2"), "working"),
            mk("c", 0.7, Some("T3"), "episodic"),
            mk("d", 0.6, None, "working"),
        ];
        let (persistent, session) = partition_by_scope(mems);
        let p_ids: Vec<&str> = persistent.iter().map(|m| m.memory_id.as_str()).collect();
        let s_ids: Vec<&str> = session.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(p_ids, vec!["a", "c"]);
        assert_eq!(s_ids, vec!["b", "d"]);
    }

    #[test]
    fn partition_treats_unknown_types_as_persistent() {
        // Fail-open: we'd rather surface an unknown-type memory than
        // silently drop it when Memoria adds new types.
        let mems = vec![
            mk("a", 0.9, Some("T1"), "future_type"),
            mk("b", 0.8, Some("T2"), "working"),
        ];
        let (persistent, session) = partition_by_scope(mems);
        assert_eq!(persistent.len(), 1);
        assert_eq!(persistent[0].memory_id, "a");
        assert_eq!(session.len(), 1);
        assert_eq!(session[0].memory_id, "b");
    }

    #[test]
    fn is_persistent_type_matches_constants() {
        for t in PERSISTENT_TYPES {
            assert!(is_persistent_type(t));
        }
        assert!(!is_persistent_type(SESSION_SCOPED_TYPE));
        assert!(!is_persistent_type("tool_result"));
    }

    // ── End-to-end scenario: the c6e18730 pollution ranking bug ──────

    #[test]
    fn user_curated_preference_outranks_compaction_summary() {
        // Concrete reproduction: three memories retrieved, flat
        // ordering would put the compaction summary first, tier-aware
        // ordering puts the user preference first.
        let mut mems = vec![
            // Auto-compaction summary, raw score 0.72, inferred tier.
            RankableMemory {
                memory_id: "compact-abc".into(),
                content: "[@episode/compaction] session=abc older discussion".into(),
                memory_type: "episodic".into(),
                retrieval_score: Some(0.72),
                trust_tier: Some("T3".into()),
            },
            // User-confirmed preference, raw score 0.58, verified tier.
            RankableMemory {
                memory_id: "pref-rust".into(),
                content: "[@pref/active] senior Rust engineer, prefers CLI tools".into(),
                memory_type: "profile".into(),
                retrieval_score: Some(0.58),
                trust_tier: Some("T1".into()),
            },
            // Session-end curated learning, raw score 0.65, curated tier.
            RankableMemory {
                memory_id: "learn-cache".into(),
                content: "[@knowledge/curated] Bedrock cache_reference fields are stripped".into(),
                memory_type: "semantic".into(),
                retrieval_score: Some(0.65),
                trust_tier: Some("T2".into()),
            },
        ];

        sort_memories(&mut mems);

        // Expected composite scores:
        //   pref-rust     = 0.58 * 1.00 = 0.580
        //   learn-cache   = 0.65 * 0.85 = 0.5525
        //   compact-abc   = 0.72 * 0.55 = 0.396
        assert_eq!(mems[0].memory_id, "pref-rust", "T1 preference must win");
        assert_eq!(mems[1].memory_id, "learn-cache", "T2 learning next");
        assert_eq!(
            mems[2].memory_id, "compact-abc",
            "T3 auto-compaction last despite highest raw score"
        );
    }
}
