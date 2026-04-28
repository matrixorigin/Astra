//! Phase E — Track #2: approval persistence across session restart.
//!
//! The audit flagged this as a Hard-tier gap: today only three focused tests
//! cover the `FingerprintedOverrides` JSON roundtrip, and none of them exercise
//! the real restart flow — multi-generation restore, malformed checkpoint
//! payloads, fingerprint-type heterogeneity, and deny-rule durability.
//!
//! These tests deliberately use only the public API surface consumed by
//! `permission_manager::merge_restored_overrides` /
//! `export_session_overrides` and the `apply_heavy_checkpoint_fallback` path
//! in `slash_session`, so they stay valid even if the checkpoint wire format
//! evolves (they go through `to_json` / `merge_from_json`).

use astra_turn_core::approval_fingerprint::{ApprovalFingerprint, FingerprintedOverrides};
use serde_json::{Value, json};

/// Helper: simulate one session restart — serialize `current`, merge into a
/// fresh `FingerprintedOverrides` to emulate the process-boundary crossing.
fn restart(current: &FingerprintedOverrides) -> FingerprintedOverrides {
    let mut fresh = FingerprintedOverrides::default();
    if let Some(json) = current.to_json() {
        fresh.merge_from_json(&json);
    }
    fresh
}

// ─── 1. Three-generation restore chain ──────────────────────────────────────
// Session1 grants two approvals, dies. Session2 restores them, grants a third,
// dies. Session3 restores should see all three — restart must not lose rules
// each generation.
#[test]
fn approvals_survive_multi_generation_restore() {
    let mut s1 = FingerprintedOverrides::default();
    s1.insert(
        ApprovalFingerprint::shell("bash", "git commit -m 'x'", false),
        true,
    );
    s1.insert(
        ApprovalFingerprint::file_op("write_file", Some("src/lib.rs")),
        true,
    );

    let mut s2 = restart(&s1);
    assert_eq!(s2.len(), 2, "gen2 must restore both rules");
    s2.insert(ApprovalFingerprint::bare("read_file"), true);

    let s3 = restart(&s2);
    assert_eq!(s3.len(), 3, "gen3 must carry forward all three rules");

    // All three fingerprints still authoritative after two hops.
    let git = ApprovalFingerprint::shell("bash", "git commit --amend", false);
    assert_eq!(s3.check(&git), Some(true));
    let file = ApprovalFingerprint::file_op("write_file", Some("src/lib.rs"));
    assert_eq!(s3.check(&file), Some(true));
    let read = ApprovalFingerprint::shell("read_file", "irrelevant", true);
    assert_eq!(s3.check(&read), Some(true));
}

// ─── 2. Malformed checkpoint payload never panics ───────────────────────────
// A corrupt or schema-skewed `approval_overrides` JSON blob must be a no-op —
// rolling forward a restore with bad data must not crash and must preserve
// existing live rules (checkpoint failure should never regress the session).
#[test]
fn malformed_checkpoint_json_is_noop_preserves_live_rules() {
    let mut live = FingerprintedOverrides::default();
    live.insert(ApprovalFingerprint::bare("bash"), true);
    let live_before = serde_json::to_value(live.iter().collect::<Vec<_>>()).unwrap();

    // Each of these is syntactically valid JSON but semantically invalid for
    // FingerprintedOverrides. None may panic; none may disturb live rules.
    let garbage = [
        Value::Null,
        json!("just a string"),
        json!(42),
        json!([1, 2, 3]),
        json!({"rules": "wrong-shape"}),
        json!({"rules": [{"fingerprint": {}, "allowed": "yes"}]}),
    ];
    for g in &garbage {
        live.merge_from_json(g);
    }

    let live_after = serde_json::to_value(live.iter().collect::<Vec<_>>()).unwrap();
    assert_eq!(
        live_before, live_after,
        "malformed checkpoint JSON must not touch live rules"
    );
    let fp = ApprovalFingerprint::shell("bash", "ls", true);
    assert_eq!(live.check(&fp), Some(true));
}

// ─── 3. Path-pattern matching still works after restart ─────────────────────
// An approval for a deep path normalizes to `src/turn/**`. After a restart,
// a DIFFERENT file under `src/turn/` must remain auto-approved; an unrelated
// directory must still need approval. This is the realistic "approve once,
// edit many" scenario.
#[test]
fn path_pattern_match_survives_restart() {
    let mut s1 = FingerprintedOverrides::default();
    s1.insert(
        ApprovalFingerprint::file_op("write_file", Some("src/turn/interruption.rs")),
        true,
    );

    let s2 = restart(&s1);

    // Different file under same normalized pattern — auto-approved.
    let sibling = ApprovalFingerprint::file_op("write_file", Some("src/turn/new_module.rs"));
    assert_eq!(
        s2.check(&sibling),
        Some(true),
        "sibling under `src/turn/**` must match after restart"
    );

    // Unrelated directory — NOT covered.
    let stranger = ApprovalFingerprint::file_op("write_file", Some("src/other/foo.rs"));
    assert_eq!(
        s2.check(&stranger),
        None,
        "out-of-pattern path must still require approval after restart"
    );
}

// ─── 4. Command-prefix match still works after restart ──────────────────────
// Mirror of test 3 for shell tools. A `git commit` override must still match
// `git commit --amend` post-restart but must not match `rm -rf`.
#[test]
fn command_prefix_match_survives_restart() {
    let mut s1 = FingerprintedOverrides::default();
    s1.insert(
        ApprovalFingerprint::shell("bash", "git commit -m 'first'", false),
        true,
    );

    let s2 = restart(&s1);

    let cousin = ApprovalFingerprint::shell("bash", "git commit --amend --no-edit", false);
    assert_eq!(s2.check(&cousin), Some(true));

    let stranger = ApprovalFingerprint::shell("bash", "rm -rf /", false);
    assert_eq!(s2.check(&stranger), None);
}

// ─── 5. Idempotent merge — restoring twice does not duplicate or flip ───────
// Under pathological resume paths (two checkpoints replayed, or a retry) the
// same payload may be merged multiple times. Must be idempotent: same
// decisions, same count, same matching behavior.
#[test]
fn merge_is_idempotent() {
    let mut original = FingerprintedOverrides::default();
    original.insert(ApprovalFingerprint::shell("bash", "cargo test", true), true);
    original.insert(
        ApprovalFingerprint::file_op("write_file", Some("README.md")),
        false,
    );
    let json = original.to_json().unwrap();

    let mut target = FingerprintedOverrides::default();
    target.merge_from_json(&json);
    let len_after_first = target.len();
    target.merge_from_json(&json);
    target.merge_from_json(&json);

    assert_eq!(
        target.len(),
        len_after_first,
        "repeated merges must not duplicate rules"
    );
    let fp = ApprovalFingerprint::shell("bash", "cargo test --release", true);
    assert_eq!(target.check(&fp), Some(true));
    let readme = ApprovalFingerprint::file_op("write_file", Some("README.md"));
    assert_eq!(target.check(&readme), Some(false));
}

// ─── 6. Deny overrides are durable across restart ───────────────────────────
// A deny rule from a prior session must keep denying after restart — this
// protects against the "forgotten danger" failure mode where a user denies
// a risky tool once and then a resumed session silently allows it.
#[test]
fn deny_overrides_persist_and_still_deny_after_restart() {
    let mut s1 = FingerprintedOverrides::default();
    s1.insert(ApprovalFingerprint::bare("delete_database"), false);
    s1.insert(ApprovalFingerprint::shell("bash", "rm -rf", false), false);

    let s2 = restart(&s1);

    let dd = ApprovalFingerprint::shell("delete_database", "anything", false);
    assert_eq!(
        s2.check(&dd),
        Some(false),
        "denied bare tool must stay denied after restart"
    );
    let rm = ApprovalFingerprint::shell("bash", "rm -rf /var", false);
    assert_eq!(
        s2.check(&rm),
        Some(false),
        "denied command prefix must stay denied after restart"
    );
}

// ─── 7. Session-priority merge preserves live deny over restored allow ──────
// User denied bash in the live session (intent = "stop"), but a stale
// checkpoint allowed it. After merge, live MUST win. This is the correctness
// pivot of `merge_from_json` and the bedrock of the restart protocol.
#[test]
fn live_session_deny_wins_over_restored_allow() {
    // Live session has deny, just saved.
    let mut live = FingerprintedOverrides::default();
    live.insert(ApprovalFingerprint::bare("bash"), false);

    // Stale checkpoint from before the user changed their mind.
    let mut stale = FingerprintedOverrides::default();
    stale.insert(ApprovalFingerprint::bare("bash"), true);
    let stale_json = stale.to_json().unwrap();

    // Merge the stale checkpoint onto live — live must still deny.
    live.merge_from_json(&stale_json);
    let fp = ApprovalFingerprint::shell("bash", "echo hi", true);
    assert_eq!(
        live.check(&fp),
        Some(false),
        "live deny must outlast a restored allow"
    );

    // Now simulate a restart that saves `live` and is re-loaded on next boot.
    // The deny must still be authoritative because that's what was persisted.
    let next_boot = restart(&live);
    assert_eq!(next_boot.check(&fp), Some(false));
}

// ─── 8. Heterogeneous fingerprint types coexist across restart ──────────────
// Bare tool rule + shell prefix rule + file path rule all in one session must
// each survive, and matching must still correctly distinguish them (the rules
// list is order-sensitive: first match wins).
#[test]
fn heterogeneous_fingerprints_coexist_after_restart() {
    let mut s1 = FingerprintedOverrides::default();
    s1.insert(ApprovalFingerprint::bare("read_file"), true);
    s1.insert(
        ApprovalFingerprint::shell("bash", "cargo build", true),
        true,
    );
    s1.insert(
        ApprovalFingerprint::file_op("write_file", Some("docs/guide.md")),
        false,
    );

    let s2 = restart(&s1);
    assert_eq!(s2.len(), 3);

    // Each fingerprint type resolves to the correct decision.
    let read_any = ApprovalFingerprint::shell("read_file", "irrelevant", true);
    assert_eq!(s2.check(&read_any), Some(true));

    let build = ApprovalFingerprint::shell("bash", "cargo build --release", true);
    assert_eq!(s2.check(&build), Some(true));

    // A different shell command still requires approval (distinct prefix).
    let other_shell = ApprovalFingerprint::shell("bash", "cargo fmt", true);
    assert_eq!(s2.check(&other_shell), None);

    let deny = ApprovalFingerprint::file_op("write_file", Some("docs/guide.md"));
    assert_eq!(s2.check(&deny), Some(false));

    // A different file under write_file still requires approval — the deny
    // rule is keyed on the specific path, not the bare tool name.
    let other_file = ApprovalFingerprint::file_op("write_file", Some("README.md"));
    assert_eq!(s2.check(&other_file), None);
}
