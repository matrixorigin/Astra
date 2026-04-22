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
