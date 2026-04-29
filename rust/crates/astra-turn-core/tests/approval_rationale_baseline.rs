//! Guard-rail test for the *distribution* of bash approval rationales.
//!
//! Motivation: the existing unit tests in `cloud_approval_policy::tests`
//! pin point-wise behaviour for hand-written samples. They cannot catch a
//! classifier regression that *shifts the distribution* — e.g. a refactor
//! that accidentally classifies every `cargo check` invocation as
//! mutating. Such a regression would silently flood users with approval
//! prompts in production.
//!
//! This test loads a representative corpus sampled from typical dev
//! workflows (see `tests/fixtures/bash_command_corpus.txt`) and asserts
//! the *counts* of each [`BashApprovalReason`] variant fall within ranges
//! the team has consciously signed off on. The ranges are intentionally
//! loose: adding a few more samples to one category is fine, but a whole-
//! category collapse (e.g. all benign commands suddenly mutating) will
//! break the test and force a review.
//!
//! Fixture format: plain text, one command per line. Lines starting with
//! `#` are comments and skipped. Blank / whitespace-only lines ARE
//! counted (they exercise the `Empty` rationale path).

use astra_turn_core::cloud_approval_policy::{
    BashApprovalReason, bash_command_approval_reason,
};

const CORPUS: &str = include_str!("fixtures/bash_command_corpus.txt");

#[derive(Default, Debug)]
struct Buckets {
    total: usize,
    read_only: usize,
    empty: usize,
    shell_injection: usize,
    write_indicator: usize,
    unknown_prefix: usize,
}

fn classify_corpus() -> Buckets {
    let mut b = Buckets::default();
    for line in CORPUS.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        b.total += 1;
        match bash_command_approval_reason(line) {
            None => b.read_only += 1,
            Some(BashApprovalReason::Empty) => b.empty += 1,
            Some(BashApprovalReason::ShellInjection) => b.shell_injection += 1,
            Some(BashApprovalReason::WriteIndicator(_)) => b.write_indicator += 1,
            Some(BashApprovalReason::UnknownPrefix(_)) => b.unknown_prefix += 1,
        }
    }
    b
}

/// The corpus must be substantial enough to be representative.
#[test]
fn corpus_is_representative() {
    let b = classify_corpus();
    assert!(
        b.total >= 80,
        "corpus shrank unexpectedly (got {} samples, expected >= 80); \
         did someone truncate tests/fixtures/bash_command_corpus.txt?",
        b.total
    );
}

/// Read-only commands must dominate the corpus. If a classifier change
/// causes this ratio to collapse, users will see a flood of approval
/// prompts for commands they previously ran frictionlessly.
#[test]
fn read_only_majority_is_preserved() {
    let b = classify_corpus();
    let ratio = b.read_only as f64 / b.total as f64;
    assert!(
        ratio >= 0.45,
        "read-only ratio collapsed: {}/{} = {:.2} (expected >= 0.45). \
         Buckets: {:?}. Investigate `strip_benign_fd_redirects` or \
         `matches_read_only_prefix` for over-conservative changes.",
        b.read_only, b.total, ratio, b
    );
}

/// Symmetric lower-bound: write-indicator commands must STILL be
/// recognised. If this drops, mutating commands are silently executed
/// without approval — a safety regression.
#[test]
fn write_indicators_are_still_flagged() {
    let b = classify_corpus();
    assert!(
        b.write_indicator >= 15,
        "write-indicator count collapsed to {} (expected >= 15). \
         Mutating commands no longer flagged — SAFETY regression. \
         Buckets: {:?}",
        b.write_indicator, b
    );
}

#[test]
fn shell_injection_is_still_detected() {
    let b = classify_corpus();
    assert!(
        b.shell_injection >= 2,
        "shell-injection count dropped to {} (expected >= 2). Buckets: {:?}",
        b.shell_injection, b
    );
}

#[test]
fn unknown_prefix_still_fires() {
    let b = classify_corpus();
    assert!(
        b.unknown_prefix >= 2,
        "unknown-prefix count dropped to {} (expected >= 2). \
         Allowlist may have grown too permissive. Buckets: {:?}",
        b.unknown_prefix, b
    );
}

/// Every Some(reason) reached by the corpus must produce a non-empty
/// `display()` string. Guards against a future variant being added
/// without updating the display match arm.
#[test]
fn every_reason_has_non_empty_display() {
    for line in CORPUS.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(reason) = bash_command_approval_reason(line) {
            let d = reason.display();
            assert!(
                !d.trim().is_empty(),
                "display() returned empty for input {line:?} -> {reason:?}"
            );
        }
    }
}
