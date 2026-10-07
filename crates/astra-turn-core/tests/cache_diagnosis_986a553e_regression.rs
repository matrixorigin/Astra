//! Regression fixture for **session 986a553e**
//! (`986a553e-b0e5-4570-bcd2-a47a11c41a15`, 2026-05-08).
//!
//! Captured after the rolling-breakpoint and deferred-tool surface fixes.
//! The measured cache read drops from 7680 at t4 r0 to zero at t4 r1..r6.
//! That collapse must remain visible through `cache_read_collapsed`.
//!
//! The request also contains volatile text patterns, preserved by the scrubber.
//! Its CurrentUserOnly capability permits required runtime context, but these
//! captures do not identify required versus optional producers. Text patterns
//! alone cannot prove a delivery violation or authorize suppressing context.
//! Marker and creation-waste rules remain silent because this path has no
//! cache_control markers and reports no cache creation.

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
            let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
            let v: Value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {p:?}: {e}"));
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

/// Keep the measured regression without inferring an unsupported cause.
#[test]
fn session_986a553e_reports_cache_collapse_without_inventing_delivery_violation() {
    let rs = load_fixture_rounds();
    let findings: Vec<CacheFinding> = evaluate_all(&rs);
    let ids: Vec<&str> = findings.iter().map(|f| f.rule_id).collect();
    assert!(
        ids.contains(&"cache_read_collapsed"),
        "measured 7680-to-zero collapse must remain visible: {findings:#?}",
    );
    for rule in [
        "volatile_in_cached_prefix",
        "cc_marker_frozen",
        "tool_marker_not_on_tail",
        "cache_creation_waste",
    ] {
        assert!(
            !ids.contains(&rule),
            "fixture lacks evidence for {rule}: {findings:#?}",
        );
    }
}
