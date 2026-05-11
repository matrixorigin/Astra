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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RankableMemory {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    /// Server-side `final_score` from Memoria. Already tier-weighted.
    pub retrieval_score: Option<f64>,
    pub trust_tier: Option<String>,
    /// RFC3339 timestamp of first observation — used to render a
    /// freshness suffix so the LLM knows if the memory is stale.
    pub observed_at: Option<String>,
    /// RFC3339 timestamp of the most recent update, if any.
    pub updated_at: Option<String>,
    /// Session id this memory was scoped to at write time (only
    /// present for working / episodic memories).
    pub session_id: Option<String>,
}

impl RankableMemory {
    /// Days elapsed since `observed_at` (or `updated_at` as fallback).
    /// `None` when neither timestamp is present or parseable.
    pub fn age_days(&self) -> Option<i64> {
        let ts = self.observed_at.as_deref().or(self.updated_at.as_deref())?;
        // Minimal RFC3339 parse without a date crate dep: take the first
        // 10 chars (YYYY-MM-DD) and rely on callers for richer parsing.
        // In practice Memoria always emits RFC3339 so we defer to the
        // runtime-side `MemoriaMemory::age_days` which owns chrono. This
        // shim uses a lightweight parse so tests can still exercise the
        // age-suffix formatting without pulling chrono into turn-types.
        parse_rfc3339_days_ago(ts)
    }

    /// Compact freshness suffix. Empty for fresh memories (≤ 1 day);
    /// otherwise ` (N days ago)` or, past the tier half-life,
    /// ` (N days ago — verify first)`.
    pub fn freshness_suffix(&self) -> String {
        let Some(days) = self.age_days() else {
            return String::new();
        };
        if days <= 1 {
            return String::new();
        }
        let half_life = match self.trust_tier.as_deref() {
            Some("T1") => 365,
            Some("T2") => 180,
            Some("T4") => 30,
            _ => 60, // T3 default when tier unknown
        };
        if days >= half_life {
            format!(" ({days} days ago — verify first)")
        } else {
            format!(" ({days} days ago)")
        }
    }
}

/// Very small RFC3339-ish parser: returns "days since" for a
/// timestamp of the form `YYYY-MM-DDTHH:MM:SSZ` (or any prefix with
/// a valid `YYYY-MM-DD`). Returns `None` on malformed input or when
/// the date is in the future (clock skew).
fn parse_rfc3339_days_ago(ts: &str) -> Option<i64> {
    // Parse YYYY-MM-DD.
    let date_part = ts.get(..10)?;
    let mut parts = date_part.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    // Convert to days since epoch via the proleptic Gregorian formula.
    let date_days = days_from_civil(y, m, d)?;
    let now_days = days_from_civil_now()?;
    let diff = now_days - date_days;
    if diff < 0 { None } else { Some(diff) }
}

/// Howard Hinnant's days-from-civil algorithm.
fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || d == 0 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe as i64 * 365 + (yoe / 4) as i64 - (yoe / 100) as i64 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn days_from_civil_now() -> Option<i64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(secs / 86_400)
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
            observed_at: None,
            updated_at: None,
            session_id: None,
        }
    }

    // ── Freshness suffix ────────────────────────────────────────────

    fn mem_with_observed(id: &str, observed: &str, tier: Option<&str>) -> RankableMemory {
        RankableMemory {
            memory_id: id.into(),
            content: "x".into(),
            memory_type: "semantic".into(),
            retrieval_score: Some(0.5),
            trust_tier: tier.map(str::to_string),
            observed_at: Some(observed.into()),
            updated_at: None,
            session_id: None,
        }
    }

    #[test]
    fn freshness_suffix_empty_without_timestamp() {
        let m = RankableMemory {
            memory_id: "m".into(),
            content: "x".into(),
            memory_type: "semantic".into(),
            retrieval_score: None,
            trust_tier: None,
            observed_at: None,
            updated_at: None,
            session_id: None,
        };
        assert!(m.freshness_suffix().is_empty());
    }

    #[test]
    fn freshness_suffix_empty_within_one_day() {
        let today = chrono_like_today();
        let m = mem_with_observed("m", &today, Some("T3"));
        assert_eq!(m.freshness_suffix(), "");
    }

    #[test]
    fn freshness_suffix_emits_n_days_ago() {
        // 10 days ago — under the T3 half-life (60), no verify hint.
        let ts = chrono_like_days_ago(10);
        let m = mem_with_observed("m", &ts, Some("T3"));
        let s = m.freshness_suffix();
        assert!(s.contains("10 days ago"), "got: {s:?}");
        assert!(!s.contains("verify first"));
    }

    #[test]
    fn freshness_suffix_adds_verify_hint_past_half_life() {
        // 120 days ago — past T3's 60-day half-life.
        let ts = chrono_like_days_ago(120);
        let m = mem_with_observed("m", &ts, Some("T3"));
        let s = m.freshness_suffix();
        assert!(s.contains("120 days ago"));
        assert!(s.contains("verify first"), "got: {s:?}");
    }

    #[test]
    fn freshness_suffix_respects_trust_tier_half_life() {
        // 100 days ago. T1 (365d half-life) → no verify hint; T4 (30d) → verify.
        let ts = chrono_like_days_ago(100);
        let t1 = mem_with_observed("m", &ts, Some("T1"));
        let t4 = mem_with_observed("m", &ts, Some("T4"));
        assert!(!t1.freshness_suffix().contains("verify"));
        assert!(t4.freshness_suffix().contains("verify"));
    }

    fn chrono_like_today() -> String {
        format_iso_days_ago(0)
    }

    fn chrono_like_days_ago(days: i64) -> String {
        format_iso_days_ago(days)
    }

    fn format_iso_days_ago(days: i64) -> String {
        // Produce a YYYY-MM-DD timestamp `days` days before today in UTC.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let total_days = secs / 86_400 - days;
        let (y, m, d) = civil_from_days(total_days);
        format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
    }

    /// Inverse of `days_from_civil` — needed only by tests.
    fn civil_from_days(days: i64) -> (i32, u32, u32) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        let y = if m <= 2 { y + 1 } else { y };
        (y as i32, m, d)
    }

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
                observed_at: None,
                updated_at: None,
                session_id: None,
            },
            mk("ok", 0.5, Some("T1"), "semantic"),
            RankableMemory {
                memory_id: "none".into(),
                content: "c".into(),
                memory_type: "semantic".into(),
                retrieval_score: None,
                trust_tier: None,
                observed_at: None,
                updated_at: None,
                session_id: None,
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
