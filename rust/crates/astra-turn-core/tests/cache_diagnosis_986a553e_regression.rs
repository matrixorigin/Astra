//! Regression fixture for **session 986a553e**
//! (`986a553e-b0e5-4570-bcd2-a47a11c41a15`, 2026-05-08).
//!
//! Captured AFTER the d0640d3d rolling-breakpoint + pinned-tool fixes
//! landed. Exposes the *next* cache regression: MiniMax's OpenAI-
//! compatible prompt cache is strict-history (any mid-history byte
//! change invalidates), and astra was injecting volatile content
//! (`## Self-Awareness` + live turn/token counters) into a synthetic
//! user-role preamble that re-rendered every round.
//!
//! Observable on this fixture:
//!   t4 r0 cache_read = 7680     ← healthy first round
//!   t4 r1..r6  cache_read = 0   ← collapsed: 6 consecutive rounds wasted
//!
//! ## What must fire
//!
//! Two rules should surface:
//!   - `cache_read_collapsed`        — 7680 → 0 drop is >50%
//!   - `volatile_in_cached_prefix`   — MiniMax tool-loop round >0 with
//!                                     Self-Awareness in history
//!
//! ## What must stay silent
//!
//! OpenAI-compat providers have neither cache_control markers nor
//! tool-schema cache boundaries, so these rules cannot fire here:
//!   - `cc_marker_frozen`
//!   - `tool_marker_not_on_tail`
//!   - `cache_creation_waste`        (cache_creation is always 0 here)
//!
//! ## Fixture integrity
//!
//! The scrub preserves the first ~200 chars past each volatile marker
//! so the runtime's `contains_volatile_pattern` still triggers. If a
//! future change to the pattern list or the scrubber stops preserving
//! the marker, this test will fire loudly (the rule goes silent) and
//! tell the operator to regenerate the fixture.

use std::path::{Path, PathBuf};

use astra_turn_core::introspect::cache_diagnosis::{
    CacheFinding, RoundSnapshot, evaluate_all, snapshot_from_capture_json,
};
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("cache_diagnosis_986a553e")
}

fn load_fixture_rounds() -> Vec<RoundSnapshot> {
    let dir = fixture_dir();
    let mut entries: Vec<(u32, u32, PathBuf)> = std::fs::read_dir(&dir)
        .expect("fixture dir exists")
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_str()?.to_string();
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

#[test]
fn fixture_loads_9_captures() {
    let rs = load_fixture_rounds();
    assert_eq!(
        rs.len(),
        9,
        "986a553e fixture must have 9 captures (t2_r0, t3_r0, t4_r0..r6); got {}",
        rs.len(),
    );
    let t4_r1 = rs
        .iter()
        .find(|r| r.turn == 4 && r.round == 1)
        .expect("t4_r1 present");
    assert_eq!(t4_r1.provider, "openai");
    assert_eq!(t4_r1.model, "MiniMax-M2.7");
    assert_eq!(
        t4_r1.cache_read_tokens, 0,
        "t4_r1 is the collapsed round — should report 0 cache_read",
    );
    assert!(
        !t4_r1.volatile_msg_indices.is_empty(),
        "parser must detect volatile content in msg[7] of t4_r1 — \
         scrubber integrity check. volatile_msg_indices={:?}",
        t4_r1.volatile_msg_indices,
    );
}

/// **Regression net: volatile-in-prefix on MiniMax tool loops.**
///
/// These pathologies must keep triggering. If any goes silent on this
/// fixture, a real bug is slipping through.
#[test]
fn session_986a553e_triggers_minimax_tool_loop_rules() {
    let rs = load_fixture_rounds();
    let findings: Vec<CacheFinding> = evaluate_all(&rs);
    let ids: Vec<&str> = findings.iter().map(|f| f.rule_id).collect();

    // Must fire.
    for rule in ["cache_read_collapsed", "volatile_in_cached_prefix"] {
        assert!(
            ids.contains(&rule),
            "{rule} must fire on 986a553e fixture, got {ids:?}. \
             full findings:\n{findings:#?}",
        );
    }

    // Must NOT fire (no cc markers / no tool markers on MiniMax path).
    for rule in [
        "cc_marker_frozen",
        "tool_marker_not_on_tail",
        "cache_creation_waste",
    ] {
        assert!(
            !ids.contains(&rule),
            "{rule} must stay silent on 986a553e (MiniMax OpenAI-compat has no markers), \
             got {ids:?}. full findings:\n{findings:#?}",
        );
    }

    // Narrative spot-check: the volatile finding must identify MiniMax
    // specifically so a future provider-mapping regression (sending
    // MiniMax back to OpenAI TailSuffix) doesn't silently change the
    // finding's wording.
    let volatile = findings
        .iter()
        .find(|f| f.rule_id == "volatile_in_cached_prefix")
        .unwrap();
    assert!(
        volatile.narrative.to_lowercase().contains("minimax")
            || volatile.narrative.contains("strict-history"),
        "volatile finding must name MiniMax or strict-history: got {}",
        volatile.narrative,
    );
}
