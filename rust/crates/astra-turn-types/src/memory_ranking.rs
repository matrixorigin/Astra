//! L3: layered memory recall — shaping Memoria results for injection.
//!
//! **Memoria already ranks by tier.** The server's `final_score` is
//! `0.3·vec + 0.2·kw + 0.2·time + 0.3·conf_score` where `conf_score`
//! is the tier-aware confidence-decay signal (half-life T1=365d,
//! T2=180d, T3=60d, T4=30d). A client-side tier multiplier on top of
//! that would double-count — exactly the bug we shipped and reverted.
//!
//! So this module is intentionally *minimal*:
//!
//! 1. [`RankableMemory`] — a transport-agnostic record (no Memoria
//!    HTTP client types leak into the ranking layer; the tests and
//!    any future memdir backend can synthesize it).
//! 2. [`partition_by_scope`] — split retrieved memories into
//!    persistent (cross-session) vs. session-scoped (`working`) so
//!    the caller can allocate budget per lane (e.g. 4 persistent + 1
//!    working in the volatile system-prompt lane).
//! 3. [`sort_by_retrieval_score`] — a *stable* sort on the server's
//!    `retrieval_score` for cases where the caller has merged results
//!    from multiple queries (full-message + entity tokens) and needs a
//!    deterministic ordering. Ties break on `memory_id` so the volatile
//!    prompt-cache prefix stays stable across runs.
//!
//! # Why not re-rank client-side at all?
//!
//! Memoria's server-side ranking is the only place that sees global
//! statistics (corpus confidence distribution, per-user decay, graph
//! neighborhood signals). Re-ranking client-side would need all of
//! that as input to avoid the double-count trap — and if we had it,
//! we wouldn't need to re-rank. Trust the server; shape what comes
//! back.

/// Persistent memory types — long-lived, cross-session. Entries in
/// these types are candidates for the `## User Memories` volatile-
/// lane section.
pub const PERSISTENT_TYPES: &[&str] = &["semantic", "episodic", "procedural", "profile"];

/// Session-scoped memory type — auto-purged, only valid within the
/// originating session.
pub const SESSION_SCOPED_TYPE: &str = "working";

/// A memory record exposing the fields needed for shaping. Callers
/// build this from their Memoria client's result type — we keep the
/// shaping layer free of crate dependencies so it can run in tests
/// and in any future memdir/file-based fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct RankableMemory {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    /// Server-side `final_score` from Memoria. Already tier-weighted.
    pub retrieval_score: Option<f64>,
    pub trust_tier: Option<String>,
}

/// Sort `memories` in-place by descending `retrieval_score` (the
/// server's already-tier-weighted `final_score`).
///
/// Use this *only* when the caller has merged results from multiple
/// queries and needs one ordered list — a single-query result from
/// Memoria is already ordered.
///
/// Stable: ties break ascending on `memory_id` so the volatile
/// prompt-cache prefix stays identical across runs. `None` /
/// `NaN` scores sort to the end.
pub fn sort_by_retrieval_score(memories: &mut [RankableMemory]) {
    memories.sort_by(|a, b| {
        let sa = finite_or_neg_inf(a.retrieval_score);
        let sb = finite_or_neg_inf(b.retrieval_score);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
}

fn finite_or_neg_inf(score: Option<f64>) -> f64 {
    match score {
        Some(s) if s.is_finite() => s,
        _ => f64::NEG_INFINITY,
    }
}

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
/// Used so the caller can allocate the volatile-lane budget per
/// bucket (e.g. 4 persistent + 1 working) without cross-bucket
/// interference.
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

    fn mk(id: &str, score: f64, tier: Option<&str>, mem_type: &str) -> RankableMemory {
        RankableMemory {
            memory_id: id.to_string(),
            content: format!("content for {id}"),
            memory_type: mem_type.to_string(),
            retrieval_score: Some(score),
            trust_tier: tier.map(str::to_string),
        }
    }

    // ── sort_by_retrieval_score ────────────────────────────────────

    #[test]
    fn sort_descends_by_server_score() {
        // Server already tier-weighted; we just order deterministically.
        let mut mems = vec![
            mk("lo", 0.40, Some("T1"), "semantic"),
            mk("hi", 0.82, Some("T3"), "episodic"),
            mk("mid", 0.65, Some("T2"), "procedural"),
        ];
        sort_by_retrieval_score(&mut mems);
        assert_eq!(mems[0].memory_id, "hi");
        assert_eq!(mems[1].memory_id, "mid");
        assert_eq!(mems[2].memory_id, "lo");
    }

    #[test]
    fn sort_is_deterministic_on_ties() {
        // Identical scores → ascending memory_id. Prompt-cache prefix
        // stability depends on this being identical across runs.
        let mut a = vec![
            mk("z-id", 0.8, Some("T1"), "semantic"),
            mk("a-id", 0.8, Some("T1"), "semantic"),
        ];
        let mut b = vec![
            mk("a-id", 0.8, Some("T1"), "semantic"),
            mk("z-id", 0.8, Some("T1"), "semantic"),
        ];
        sort_by_retrieval_score(&mut a);
        sort_by_retrieval_score(&mut b);
        assert_eq!(a[0].memory_id, "a-id");
        assert_eq!(b[0].memory_id, "a-id");
    }

    #[test]
    fn sort_pushes_missing_and_nan_scores_to_end() {
        let mut mems = vec![
            RankableMemory {
                memory_id: "nan".into(),
                content: "c".into(),
                memory_type: "semantic".into(),
                retrieval_score: Some(f64::NAN),
                trust_tier: None,
            },
            mk("ok", 0.5, Some("T1"), "semantic"),
            RankableMemory {
                memory_id: "none".into(),
                content: "c".into(),
                memory_type: "semantic".into(),
                retrieval_score: None,
                trust_tier: None,
            },
        ];
        sort_by_retrieval_score(&mut mems);
        assert_eq!(mems[0].memory_id, "ok");
        // The two sink values tie at -inf; memory_id breaks the tie.
        assert_eq!(mems[1].memory_id, "nan");
        assert_eq!(mems[2].memory_id, "none");
    }

    // ── partition_by_scope ────────────────────────────────────────

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
}
