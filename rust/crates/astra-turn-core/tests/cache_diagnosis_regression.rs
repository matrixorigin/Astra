//! End-to-end regression: real captured session → all four rules fire.
//!
//! Session `d0640d3d-3be0-4ce1-a4b7-e52d49601da6` (2026-05-08) exposed
//! every cache-regression `cache_diagnosis` was designed to catch:
//!
//! | round-range | pathology | rule                        |
//! | ----------- | --------- | --------------------------- |
//! | t3 r0       | tool[20] outside cache marker       | `tool_marker_not_on_tail` |
//! | t6 r0–r13   | rolling cc frozen in 14-round loop  | `cc_marker_frozen`        |
//! | t4, t6      | creation/read ratio > 0.3           | `cache_creation_waste`    |
//!
//! Note: **`cache_read_collapsed` does NOT fire on this fixture**, by
//! design. The d0640d3d bedrock pathology is that `cache_read`
//! *stayed pinned* at 11312 across 14 rounds while message_count grew
//! — i.e., the prefix never advanced, not that a prefix was broken and
//! invalidated. `cache_read_collapsed` catches the *other* class of bug
//! (prefix invalidation mid-turn, e.g. volatile content leaking into
//! the cached block), and has its own unit test coverage in
//! `cache_diagnosis::tests::cache_read_collapsed_*`.
//!
//! The fixture is a scrubbed mirror of the production capture files —
//! free-form text replaced with `<sha:len>` digests. Structural data
//! (roles, cache_control markers, usage, tool counts) is preserved
//! byte-for-byte, which is everything the rules actually read.
//!
//! If the rules change shape, this test has to change. The signal we
//! want to preserve is: *this exact set of real-world pathologies keeps
//! producing exactly this set of findings*, so a future refactor that
//! accidentally silences any one of them trips this test immediately.

use std::path::{Path, PathBuf};

use astra_turn_core::introspect::cache_diagnosis::{
    CacheFinding, RoundSnapshot, evaluate_all, snapshot_from_capture_json,
};
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("cache_diagnosis_d0640d3d")
}

/// Load every `t{N}_r{M}.json` file in the fixture dir, sorted by
/// (turn, round). Uses the production parser
/// ([`snapshot_from_capture_json`]) so that fixture drift surfaces
/// here (test) or in prod reads, never silently — they share one
/// code path.
fn load_fixture_rounds() -> Vec<RoundSnapshot> {
    let dir = fixture_dir();
    let mut entries: Vec<(u32, u32, PathBuf)> = std::fs::read_dir(&dir)
        .expect("fixture dir exists")
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?.to_string();
            // Expect names like t3_r0, t6_r13 — strip prefix `t`, split on `_r`.
            let rest = stem.strip_prefix('t')?;
            let (t_s, r_s) = rest.split_once("_r")?;
            let t: u32 = t_s.parse().ok()?;
            let r: u32 = r_s.parse().ok()?;
            Some((t, r, path))
        })
        .collect();
    entries.sort_by_key(|(t, r, _)| (*t, *r));
    entries
        .into_iter()
        .map(|(_, _, p)| {
            let text = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("read {p:?}: {e}"));
            let v: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("parse {p:?}: {e}"));
            snapshot_from_capture_json(&v)
        })
        .collect()
}

/// Sanity: fixture loaded at all.
#[test]
fn fixture_loads_19_captures() {
    let rounds = load_fixture_rounds();
    assert_eq!(
        rounds.len(),
        19,
        "d0640d3d fixture must have 19 captures (t3 r0; t4 r0-r1; t5 r0-r1; t6 r0-r13); \
         add/remove files? got {}",
        rounds.len(),
    );
    // Spot-check the two key samples.
    let t3 = rounds
        .iter()
        .find(|r| r.turn == 3 && r.round == 0)
        .expect("t3_r0");
    assert_eq!(t3.provider, "anthropic");
    assert_eq!(t3.model, "deepseek-v4-pro-anthropic");
    assert_eq!(t3.tool_count, 21);
    assert_eq!(t3.tool_cc_index, Some(19), "t3 has cc on `skill` (idx 19)");

    let t6_r0 = rounds
        .iter()
        .find(|r| r.turn == 6 && r.round == 0)
        .expect("t6_r0");
    assert_eq!(t6_r0.provider, "bedrock");
    assert_eq!(t6_r0.cache_read_tokens, 11312);
}

/// **The regression net.**
///
/// Every pathology that hit us in this one live session must keep
/// firing through the rules. If any of these asserts goes silent
/// without an intentional rule change, a real bug is slipping through.
#[test]
fn d0640d3d_fixture_triggers_session_specific_rules() {
    let rounds = load_fixture_rounds();
    let findings: Vec<CacheFinding> = evaluate_all(&rounds);
    let ids: Vec<&str> = findings.iter().map(|f| f.rule_id).collect();

    // Three rules specifically exposed by this session.
    // `cache_read_collapsed` is covered by unit tests — see module docstring.
    let expected = [
        "cache_creation_waste",
        "cc_marker_frozen",
        "tool_marker_not_on_tail",
    ];
    for rule in expected {
        assert!(
            ids.contains(&rule),
            "expected rule {rule} to fire on d0640d3d fixture, got {ids:?}. \
             Full findings:\n{findings:#?}",
        );
    }

    // And a negative: cache_read_collapsed MUST NOT fire on this fixture.
    // If a future change causes it to fire here, the rule is too eager —
    // d0640d3d's cache never *collapsed*, it just never *amortized*.
    assert!(
        !ids.contains(&"cache_read_collapsed"),
        "cache_read_collapsed must stay silent on d0640d3d — the session \
         pattern is flat cache_read, not a collapse. Rule got over-eager? \
         findings={findings:#?}",
    );

    // Spot-check key narratives so a renaming/refactor that changes the
    // finding content surfaces loudly rather than silently.
    let frozen = findings
        .iter()
        .find(|f| f.rule_id == "cc_marker_frozen")
        .unwrap();
    assert!(
        frozen.triggered_on.len() >= 3,
        "cc_marker_frozen should cite >=3 triggering rounds, got {} — \
         narrative: {}",
        frozen.triggered_on.len(),
        frozen.narrative,
    );
    assert!(
        frozen.triggered_on.iter().all(|(t, _)| *t == 6),
        "cc_marker_frozen triggers live inside turn 6, got {:?}",
        frozen.triggered_on,
    );

    let tool_tail = findings
        .iter()
        .find(|f| f.rule_id == "tool_marker_not_on_tail")
        .unwrap();
    assert!(
        tool_tail.narrative.contains("index 19") && tool_tail.narrative.contains("of 21"),
        "tool_marker_not_on_tail narrative should name the gap (19 of 21): got {}",
        tool_tail.narrative,
    );
}
