//! Adversarial contract pin for skill sub-run terminal projection.
//!
//! `SubRunResult` carries a typed terminal outcome independently from partial
//! text output. Execution bounds come from the shared adaptive runtime policy,
//! not surface-specific constants.

use astra_runtime::skills::executor::isolated::{SubRunOutcome, SubRunResult};

#[test]
fn subrun_result_preserves_typed_terminal_outcome() {
    let r = SubRunResult {
        output: String::from("hello world"),
        tokens_used: 1234u32,
        turns: 7u32,
        outcome: SubRunOutcome::Interrupted {
            finish_reason: "budget_exhausted".to_string(),
        },
    };

    assert!(!r.outcome.is_completed());
    assert_eq!(r.outcome.label(), "interrupted");
    assert_eq!(r.outcome.detail(), Some("budget_exhausted"));
    assert_eq!(r.output, "hello world", "partial output remains available");
}
