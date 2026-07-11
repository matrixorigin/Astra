//! Phase-R10 adversarial contract pins for skill sub-run scaffolding.
//!
//! Locks in:
//!   * Server-side `SUBRUN_MAX_TURNS` (30) and
//!     `SUBRUN_MAX_CUMULATIVE_TOKENS` (500_000) — asymmetric-by-design
//!     vs CLI ceilings, so drift on either side is surfaced.
//!   * `MAX_AGENT_RECURSION_DEPTH` (3) — capping nested delegations,
//!     skill forks, and spawned agents.
//!   * `SubRunResult` carries a typed terminal outcome independently from
//!     partial text output.
//!
//! The CLI-side `SUBRUN_MAX_TURNS` / `SUBRUN_MAX_CUMULATIVE_TOKENS` are
//! pinned inside `astra_cli::cli::skill_subrun`'s own `#[cfg(test)]` mod
//! (the CLI is a binary crate, so its private consts are not visible
//! from an integration test).

use astra_runtime::server::server_skill_subrun::{
    SUBRUN_MAX_CUMULATIVE_TOKENS as SERVER_SUBRUN_MAX_TOKENS,
    SUBRUN_MAX_TURNS as SERVER_SUBRUN_MAX_TURNS,
};
use astra_runtime::skills::executor::isolated::{SubRunOutcome, SubRunResult};
use astra_runtime::turn::agentic_recursion_guard::MAX_AGENT_RECURSION_DEPTH;

#[test]
fn server_subrun_max_turns_is_exactly_30() {
    assert_eq!(SERVER_SUBRUN_MAX_TURNS, 30usize);
}

#[test]
fn server_subrun_max_tokens_is_exactly_500_000() {
    assert_eq!(SERVER_SUBRUN_MAX_TOKENS, 500_000u64);
}

#[test]
fn server_subrun_tokens_strictly_exceeds_cli_subrun_tokens() {
    // CLI ceiling is 120_000 (pinned in astra-cli's skill_subrun tests).
    // Server ceiling intentionally larger; pin the inequality so a
    // refactor unifying the two constants trips this guard at compile
    // time (const block) rather than test time.
    const _: () = assert!(SERVER_SUBRUN_MAX_TOKENS > 120_000);
}

#[test]
fn agent_recursion_depth_cap_is_exactly_3() {
    assert_eq!(MAX_AGENT_RECURSION_DEPTH, 3u8);
}

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
