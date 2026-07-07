//! Phase-R10 adversarial contract pins for skill sub-run scaffolding.
//!
//! Locks in:
//!   * Server-side `SUBRUN_MAX_TURNS` (30) and
//!     `SUBRUN_MAX_CUMULATIVE_TOKENS` (500_000) — asymmetric-by-design
//!     vs CLI ceilings, so drift on either side is surfaced.
//!   * `MAX_AGENT_RECURSION_DEPTH` (3) — capping nested delegations,
//!     skill forks, and spawned agents.
//!   * `SubRunResult` carries EXACTLY three fields
//!     (`output: String`, `tokens_used: u32`, `turns: u32`) — no richer
//!     channel. Constructing via struct-literal guarantees this.
//!
//! The CLI-side `SUBRUN_MAX_TURNS` / `SUBRUN_MAX_CUMULATIVE_TOKENS` are
//! pinned inside `astra_cli::cli::skill_subrun`'s own `#[cfg(test)]` mod
//! (the CLI is a binary crate, so its private consts are not visible
//! from an integration test).

use astra_runtime::server::server_skill_subrun::{
    SUBRUN_MAX_CUMULATIVE_TOKENS as SERVER_SUBRUN_MAX_TOKENS,
    SUBRUN_MAX_TURNS as SERVER_SUBRUN_MAX_TURNS,
};
use astra_runtime::skills::executor::isolated::SubRunResult;
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

/// Pin: `SubRunResult` has EXACTLY three fields. Struct-literal
/// construction with the exact type-annotated fields — if a field is
/// added, removed, or retyped, this test stops compiling.
#[test]
fn subrun_result_has_exactly_three_typed_fields() {
    let r = SubRunResult {
        output: String::from("hello world"),
        tokens_used: 1234u32,
        turns: 7u32,
    };
    // Access each field by name to lock the struct surface.
    let output_ref: &String = &r.output;
    let tokens_ref: &u32 = &r.tokens_used;
    let turns_ref: &u32 = &r.turns;
    assert_eq!(output_ref, "hello world");
    assert_eq!(*tokens_ref, 1234);
    assert_eq!(*turns_ref, 7);

    // Field type pins: if these types ever shift (e.g. u32→u64), the
    // compiler rejects the explicit type annotations above. Runtime
    // assertion on size is a coarse secondary guard.
    assert_eq!(std::mem::size_of_val(&r.tokens_used), 4);
    assert_eq!(std::mem::size_of_val(&r.turns), 4);
}
