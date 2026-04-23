//! Phase-R9 adversarial contract pins for approval-side behavior.
//!
//! Locks in:
//!   * `approval_callback_key` format (no session_id component)
//!   * `persist_denied_tool_result` tool_call_id + shape
//!   * `DenialTracker` consecutive-denial ceiling
//!
//! The `persist_denied_tool_result` pin lives alongside the function in
//! `cloud_tool_delivery::tests` (private fn, shared-crate visibility).
//! This file covers the public-surface pins accessible from an
//! integration test.

use astra_turn_core::approval_fingerprint::{
    ApprovalFingerprint, DenialAction, DenialLimits, DenialTracker,
};
use astra_turn_core::edge_ledger::{approval_callback_key, tool_callback_key};

/// Pin: `approval_callback_key` format is EXACTLY `"{user}:approval:{req}"`
/// — it intentionally does NOT carry a session_id component. A future
/// reviewer wanting per-session isolation will hit this test and see the
/// gap documented here.
#[test]
fn approval_callback_key_has_no_session_id_exact_format_pin() {
    assert_eq!(
        approval_callback_key("user-42", "req-abc"),
        "user-42:approval:req-abc"
    );
    assert_eq!(approval_callback_key("", ""), ":approval:");
    // Crossed with tool_callback_key to double-check namespace separator.
    assert_eq!(
        tool_callback_key("user-42", "req-abc"),
        "user-42:tool:req-abc"
    );
    assert_ne!(
        approval_callback_key("user-42", "req-abc"),
        tool_callback_key("user-42", "req-abc"),
        "approval and tool ledger namespaces must never collide"
    );
}

/// Pin: `DenialTracker::default()` uses `max_consecutive == 3` (NOT 5 as
/// one sibling audit suggested). `should_prompt` returns
/// `DenialAction::SkipTool` once the consecutive-denial count reaches
/// the limit. This is a security ceiling — bumping it requires changing
/// this test deliberately.
#[test]
fn denial_tracker_default_ceiling_is_three_consecutive() {
    let defaults = DenialLimits::default();
    assert_eq!(defaults.max_consecutive, 3);
    assert_eq!(defaults.max_total, 20);

    let fp = ApprovalFingerprint::bare("bash");
    let mut tracker = DenialTracker::default();
    assert_eq!(tracker.should_prompt(&fp), DenialAction::Continue);

    // Two denials: still continue.
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    assert_eq!(tracker.should_prompt(&fp), DenialAction::Continue);

    // Third consecutive denial trips the ceiling.
    assert_eq!(tracker.record(&fp, false), DenialAction::SkipTool);
    assert_eq!(tracker.should_prompt(&fp), DenialAction::SkipTool);
}

/// Pin: approving a previously-denied fingerprint resets the consecutive
/// counter — so the ceiling is truly "consecutive", not "cumulative".
#[test]
fn denial_tracker_approval_resets_consecutive_count() {
    let fp = ApprovalFingerprint::bare("bash");
    let mut tracker = DenialTracker::default();
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    // Approve → resets.
    assert_eq!(tracker.record(&fp, true), DenialAction::Continue);
    // Two more denials should NOT trip the limit (since counter reset).
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    assert_eq!(tracker.should_prompt(&fp), DenialAction::Continue);
}

/// Pin: per-fingerprint isolation — denials of one fingerprint don't
/// advance the consecutive counter of another fingerprint.
#[test]
fn denial_tracker_consecutive_is_per_fingerprint() {
    let a = ApprovalFingerprint::bare("bash");
    let b = ApprovalFingerprint::bare("write_file");
    let mut tracker = DenialTracker::with_limits(DenialLimits {
        max_consecutive: 3,
        max_total: 100, // avoid total-ceiling interference
    });
    tracker.record(&a, false);
    tracker.record(&a, false);
    // b's counter is independent.
    assert_eq!(tracker.should_prompt(&b), DenialAction::Continue);
    assert_eq!(tracker.record(&b, false), DenialAction::Continue);
    // a still one away from its ceiling.
    assert_eq!(tracker.record(&a, false), DenialAction::SkipTool);
}
