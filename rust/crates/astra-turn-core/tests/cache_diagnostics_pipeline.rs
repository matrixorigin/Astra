//! Cache-break detection pipeline contracts.
//!
//! `cache_diagnostics::CacheBreakDetector` is extensively unit-tested in its
//! own module, but those tests live in the same file and can't prove the
//! module is **usable as a library** from downstream crates. These tests
//! exercise the full `capture → record_turn → classify` loop via the public
//! API only, pinning the contract we want to keep stable for the runtime's
//! context pipeline.

use astra_turn_core::cache_diagnostics::{
    CacheBreakDetector, CacheBreakReason, PromptStateSnapshot,
};
use serde_json::{Value, json};

fn snap(system: &str, tools: &[Value], model: &str) -> PromptStateSnapshot {
    PromptStateSnapshot::capture(system, tools, model, 1000)
}

fn tool(name: &str, desc: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": desc,
            "parameters": { "type": "object", "properties": {} }
        }
    })
}

fn matches_system(r: &CacheBreakReason) -> bool {
    matches!(r, CacheBreakReason::SystemPromptChanged)
        || matches!(r, CacheBreakReason::Multiple(v) if v.iter().any(matches_system))
}
fn matches_tools(r: &CacheBreakReason) -> bool {
    matches!(r, CacheBreakReason::ToolSchemasChanged { .. })
        || matches!(r, CacheBreakReason::Multiple(v) if v.iter().any(matches_tools))
}
fn matches_model(r: &CacheBreakReason) -> bool {
    matches!(r, CacheBreakReason::ModelChanged { .. })
        || matches!(r, CacheBreakReason::Multiple(v) if v.iter().any(matches_model))
}

// ── pc-stable-no-break ─────────────────────────────────────────────────────
#[test]
fn identical_turns_produce_no_break() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "run shell")];
    let s = snap("SYSTEM", &tools, "claude-sonnet-4");
    assert!(det.record_turn(s.clone(), Some(900)).is_none());
    let e = det.record_turn(s, Some(950));
    assert!(e.is_none(), "stable turn must not break (got {e:?})");
}

// ── pc-system-prompt-changed ───────────────────────────────────────────────
#[test]
fn system_prompt_change_classifies_as_system_prompt_changed() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "run shell")];
    det.record_turn(snap("SYSTEM v1", &tools, "m"), None);
    let e = det
        .record_turn(snap("SYSTEM v2", &tools, "m"), Some(0))
        .expect("must detect break");
    assert!(matches_system(&e.reason), "got {:?}", e.reason);
    assert!(e.estimated_token_impact > 0);
}

// ── pc-schema-churn-break ──────────────────────────────────────────────────
#[test]
fn schema_change_classifies_as_tool_schemas_changed() {
    let mut det = CacheBreakDetector::new();
    det.record_turn(snap("SYS", &[tool("bash", "A")], "m"), None);
    let e = det
        .record_turn(snap("SYS", &[tool("bash", "B-changed")], "m"), Some(0))
        .expect("must detect break");
    assert!(matches_tools(&e.reason), "got {:?}", e.reason);
}

#[test]
fn tool_addition_is_detected_as_schemas_changed() {
    let mut det = CacheBreakDetector::new();
    det.record_turn(snap("SYS", &[tool("bash", "A")], "m"), None);
    let e = det
        .record_turn(
            snap("SYS", &[tool("bash", "A"), tool("grep", "G")], "m"),
            Some(0),
        )
        .expect("must detect break");
    match e.reason {
        CacheBreakReason::ToolSchemasChanged {
            ref added,
            ref removed,
            ref changed,
        } => {
            assert_eq!(added, &vec!["grep".to_string()]);
            assert!(removed.is_empty());
            assert!(changed.is_empty(), "name-add must not count as changed");
        }
        other => panic!("expected ToolSchemasChanged, got {other:?}"),
    }
}

// ── pc-model-change-break ──────────────────────────────────────────────────
#[test]
fn model_change_classifies_as_model_changed() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap("SYS", &tools, "claude-sonnet-4"), None);
    let e = det
        .record_turn(snap("SYS", &tools, "claude-opus-4"), Some(0))
        .expect("must detect break");
    assert!(matches_model(&e.reason), "got {:?}", e.reason);
    if let CacheBreakReason::ModelChanged { from, to } = e.reason {
        assert_eq!(from, "claude-sonnet-4");
        assert_eq!(to, "claude-opus-4");
    }
}

// ── pc-concurrent-breaks ───────────────────────────────────────────────────
#[test]
fn simultaneous_system_tool_and_model_changes_classify_multiple() {
    let mut det = CacheBreakDetector::new();
    det.record_turn(snap("SYS v1", &[tool("bash", "A")], "m1"), None);
    let e = det
        .record_turn(
            snap("SYS v2", &[tool("bash", "A"), tool("grep", "G")], "m2"),
            Some(0),
        )
        .expect("must detect break");
    match &e.reason {
        CacheBreakReason::Multiple(reasons) => {
            assert!(reasons.iter().any(matches_system), "reasons: {reasons:?}");
            assert!(reasons.iter().any(matches_tools), "reasons: {reasons:?}");
            assert!(reasons.iter().any(matches_model), "reasons: {reasons:?}");
        }
        other => panic!("expected Multiple, got {other:?}"),
    }
}

// ── pc-stats-accumulate ────────────────────────────────────────────────────
#[test]
fn stats_accumulate_hits_and_misses_correctly() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap("SYS", &tools, "m"), None);
    det.record_turn(snap("SYS", &tools, "m"), Some(800)); // hit
    det.record_turn(snap("SYS", &tools, "m"), Some(800)); // hit
    det.record_turn(snap("SYS v2", &tools, "m"), Some(0)); // miss
    assert_eq!(det.stats.total_turns, 4);
    assert!(det.stats.cache_misses >= 2, "first + break = 2 misses");
    assert!(det.stats.hit_rate_percent() > 0.0);
    assert!(
        !det.stats.recent_breaks.is_empty(),
        "break must appear in recent_breaks history"
    );
}

// ── pc-ttl-expiry ─────────────────────────────────────────────────────────

/// Build a snapshot at a specific wall-clock offset. `timestamp_secs` is pub,
/// so we can construct deterministic gaps without sleeping.
fn snap_at(system: &str, tools: &[Value], model: &str, timestamp_secs: u64) -> PromptStateSnapshot {
    let mut s = PromptStateSnapshot::capture(system, tools, model, 10_000);
    s.timestamp_secs = timestamp_secs;
    s
}

#[test]
fn ttl_expiry_classified_when_hashes_match_gap_long_and_cache_read_zero() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap_at("SYS", &tools, "m", 1_000), None);

    // 1 hour + 1 s later, same system / tools / model, but API reports ~0
    // cache-read tokens. Should classify as TtlExpired.
    let evt = det
        .record_turn(snap_at("SYS", &tools, "m", 1_000 + 3_601), Some(0))
        .expect("TTL-expiry scenario must produce a break event");

    let gap = match evt.reason {
        CacheBreakReason::TtlExpired { gap_seconds } => gap_seconds,
        CacheBreakReason::Multiple(ref v) => v
            .iter()
            .find_map(|r| match r {
                CacheBreakReason::TtlExpired { gap_seconds } => Some(*gap_seconds),
                _ => None,
            })
            .expect("Multiple must contain TtlExpired"),
        other => panic!("expected TtlExpired, got {other:?}"),
    };
    assert!(gap > 300, "gap must exceed 5-min threshold, got {gap}");
    assert!(
        evt.suggestion.is_some(),
        "TtlExpired break must carry a remediation suggestion"
    );
}

#[test]
fn no_ttl_break_when_cache_read_tokens_are_healthy() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap_at("SYS", &tools, "m", 0), None);

    // Huge wall-clock gap, but API says cache_read > MIN_CACHE_MISS_TOKENS:
    // the cache is *actually* still alive — must NOT classify TtlExpired.
    assert!(
        det.record_turn(snap_at("SYS", &tools, "m", 100_000), Some(5_000))
            .is_none(),
        "healthy cache_read_tokens must suppress TTL classification"
    );
}

#[test]
fn no_ttl_break_when_gap_is_below_five_minutes() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap_at("SYS", &tools, "m", 1_000), None);
    // 4-minute gap is below the 5-min TTL inference threshold.
    assert!(
        det.record_turn(snap_at("SYS", &tools, "m", 1_000 + 240), Some(0))
            .is_none(),
        "short gap must not be classified as TTL expiry"
    );
}

#[test]
fn explicit_structural_break_wins_over_ttl_inference() {
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap_at("SYS A", &tools, "m", 0), None);

    // Long gap AND system prompt changed. The structural reason must be
    // reported; TTL inference is only a fallback when no other reason fires.
    let evt = det
        .record_turn(snap_at("SYS B", &tools, "m", 10_000), Some(0))
        .expect("system change must produce an event");

    let has_ttl = matches!(evt.reason, CacheBreakReason::TtlExpired { .. })
        || matches!(&evt.reason, CacheBreakReason::Multiple(v) if v.iter().any(|r| matches!(r, CacheBreakReason::TtlExpired { .. })));
    assert!(
        !has_ttl,
        "TTL inference must not fire when an explicit reason already exists, got {:?}",
        evt.reason
    );
    assert!(matches_system(&evt.reason));
}

#[test]
fn ttl_expiry_requires_cache_read_signal_from_api() {
    // Without the API-provided cache_read_tokens we can't distinguish
    // "cache still warm" from "cache expired" — the detector must stay
    // silent rather than guess.
    let mut det = CacheBreakDetector::new();
    let tools = vec![tool("bash", "A")];
    det.record_turn(snap_at("SYS", &tools, "m", 0), None);
    assert!(
        det.record_turn(snap_at("SYS", &tools, "m", 10_000), None)
            .is_none(),
        "missing cache_read signal must suppress TTL classification"
    );
}
