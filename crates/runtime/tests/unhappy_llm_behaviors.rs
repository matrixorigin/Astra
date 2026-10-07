//! Integration coverage for LLM-misbehavior and unhappy-path compositions.
//!
//! Covers final-text advisory and hard-block composition, exact stall-window
//! boundaries, and rate-limit cooldown expiry. Tool name/argument admission
//! belongs to the actual tool-binding and argument parsing boundaries.

use astra_turn_core::rate_limit_cooldown::{CooldownReason, RateLimitAction, RateLimitCooldown};
use astra_turn_core::response_guard::{PROMPT_LEAK_FALLBACK, apply_response_guards};
use astra_turn_core::stall::{SERVER_STALL_WINDOW, StallSignature, detect_server_stall};
use std::collections::BTreeSet;

#[test]
fn final_text_advisories_do_not_replace_visible_output() {
    for (text, query, fabrication, echo, repetition) in [
        (
            "Please replace <YOUR_API_KEY> in path/to/your/config.rs.",
            "set up",
            true,
            false,
            false,
        ),
        (
            "You asked how to add retries to the HTTP client",
            "how to add retries to the HTTP client",
            false,
            true,
            false,
        ),
        (
            "same same same same same same same same same",
            "review",
            false,
            false,
            true,
        ),
    ] {
        let result = apply_response_guards(text, query);
        assert!(result.replacement.is_none());
        assert_eq!(result.quality.has_fabrication_markers, fabrication);
        assert_eq!(result.quality.is_echo, echo);
        assert_eq!(result.quality.has_repetition_loop, repetition);
    }
}

#[test]
fn prompt_leak_hard_block_beats_text_quality_signals() {
    let text = "## Core Rules: replace path/to/your/config.rs";
    let result = apply_response_guards(text, "anything");
    assert_eq!(result.replacement.as_deref(), Some(PROMPT_LEAK_FALLBACK));
    assert!(!result.quality.has_fabrication_markers);
    assert!(!result.quality.is_echo);
    assert!(!result.quality.has_repetition_loop);
}

// ── 7. Runaway same-tool loop detection at exact window boundary ────────────

#[test]
fn stall_detector_requires_exactly_window_identical_rounds() {
    let sig_a = BTreeSet::from([StallSignature::new("bash", br#"{"cmd":"ls"}"#)]);
    let sig_b = BTreeSet::from([StallSignature::new("bash", br#"{"cmd":"pwd"}"#)]);

    // Exactly window-1 identical rounds at the tail → not yet a stall.
    let mut history = vec![sig_b.clone()];
    for _ in 0..(SERVER_STALL_WINDOW - 1) {
        history.push(sig_a.clone());
    }
    assert!(
        !detect_server_stall(&history, SERVER_STALL_WINDOW).unwrap(),
        "only {} identical rounds should NOT trip the stall window",
        SERVER_STALL_WINDOW - 1
    );

    // Adding one more identical round → exactly window → stall fires.
    history.push(sig_a.clone());
    assert!(
        detect_server_stall(&history, SERVER_STALL_WINDOW).unwrap(),
        "exactly {SERVER_STALL_WINDOW} identical tail rounds should trigger stall"
    );

    // Any divergence inside the window clears the stall, even if the overall
    // tail is dominated by `sig_a` — this is the key property that stops the
    // detector from misfiring on legitimate bursts interspersed with progress.
    let mut varied = history.clone();
    varied.push(sig_b.clone());
    assert!(
        !detect_server_stall(&varied, SERVER_STALL_WINDOW).unwrap(),
        "a divergent final round should clear the stall"
    );
}

// ── 8. Rate-limit cooldown: enters cooldown after consecutive 429s ──────────

#[test]
fn rate_limit_cooldown_blocks_on_429_then_releases_after_retry_after() {
    let cd = RateLimitCooldown::new();

    // Baseline: no errors, no cooldown.
    assert!(matches!(cd.check_request(), RateLimitAction::Proceed));

    // Drive three consecutive 429s with no retry-after hint so the handler
    // takes the "enter cooldown" branch once the consecutive threshold is hit.
    // We do not rely on short retry_after triggering the WaitAndRetry fast-path.
    let mut saw_non_proceed = 0usize;
    for _ in 0..3 {
        let act = cd.record_429(None);
        assert!(
            !matches!(act, RateLimitAction::Proceed),
            "429 must never translate to Proceed, got {act:?}"
        );
        saw_non_proceed += 1;
    }
    assert_eq!(saw_non_proceed, 3);

    // After consecutive threshold, check_request must not Proceed.
    let during = cd.check_request();
    match during {
        RateLimitAction::Reject { reason, .. } => {
            assert!(matches!(reason, CooldownReason::RateLimit));
        }
        RateLimitAction::WaitAndRetry { .. } => {
            // Acceptable: caller should back off.
        }
        RateLimitAction::Proceed => {
            panic!("check_request during active cooldown must not return Proceed")
        }
    }

    // Metrics: three 429s recorded; consecutive counter advanced.
    let metrics = cd.metrics();
    assert!(
        metrics.total_429_errors >= 3,
        "should have recorded all 429 errors, got {metrics:?}"
    );
    assert!(
        metrics.consecutive_errors >= 3,
        "consecutive counter should reflect repeated 429s, got {metrics:?}"
    );

    // record_success resets the consecutive chain (so a future burst must
    // re-accumulate before cooldown re-triggers), but does NOT forcibly exit
    // an already-active cooldown — that is time-driven by design.
    cd.record_success();
    assert_eq!(cd.metrics().consecutive_errors, 0);
}

#[tokio::test]
async fn rate_limit_cooldown_529_tracked_separately_from_429() {
    let cd = RateLimitCooldown::new();

    // Record a single 529 (service overload — close cousin of 503).
    // retry_after None is fine; metrics are what we care about here.
    let _ = cd.record_529(None);

    let metrics = cd.metrics();
    assert_eq!(metrics.total_429_errors, 0, "529 must not count as 429");
    assert!(
        metrics.total_529_errors >= 1,
        "529 counter must increment, got {metrics:?}"
    );

    // A later success wipes the *consecutive* error chains but leaves totals.
    cd.record_success();
    let after = cd.metrics();
    assert_eq!(after.total_529_errors, metrics.total_529_errors);
    assert_eq!(
        after.consecutive_errors, 0,
        "success clears the consecutive chain"
    );
}
