//! Wiring test: verifies per-model workflow-guard policies reach the loop
//! state. Guards against the regression where `ToolSelectionConfig::resolve_for_model`
//! exists but isn't consulted by the state builder — which silently gives every
//! model the same global defaults regardless of `ModelPolicyProfile`.
//!
//! Exercised via `make_test_loop_state_for_model`, which mirrors production's
//! `run_lifecycle.rs:1949` state construction.

#![cfg(feature = "bridge-e2e-hooks")]

use astra_runtime::turn::agentic_loop_host::make_test_loop_state_for_model;

#[test]
fn opus_model_picks_up_builtin_profile_thresholds() {
    // Built-in profile for "opus" is 4 / 20 (see
    // `ToolSelectionConfig::builtin_model_profiles`). If this test fails,
    // the state builder is no longer threading model_id through
    // `resolve_for_model` — the per-model policy is silently dead code.
    let state = make_test_loop_state_for_model(Some("us.anthropic.claude-opus-4-7"));
    assert_eq!(state.max_identical_tool_calls, 4);
    assert_eq!(state.max_tools_per_turn, 20);
}

#[test]
fn haiku_model_stays_conservative() {
    // Built-in "haiku" profile is 2 / 12.
    let state = make_test_loop_state_for_model(Some("claude-haiku-4-5-20251001"));
    assert_eq!(state.max_identical_tool_calls, 2);
    assert_eq!(state.max_tools_per_turn, 12);
}

#[test]
fn none_model_falls_back_to_global_defaults() {
    // With no builtin/user profile match, should fall back to the global
    // defaults (3 / 15 — see `effective_max_identical_calls` / `_tools_per_turn`).
    let state = make_test_loop_state_for_model(None);
    assert_eq!(state.max_identical_tool_calls, 3);
    assert_eq!(state.max_tools_per_turn, 15);
}

#[test]
fn unknown_model_falls_back_to_global_defaults() {
    let state = make_test_loop_state_for_model(Some("some-obscure-model"));
    assert_eq!(state.max_identical_tool_calls, 3);
    assert_eq!(state.max_tools_per_turn, 15);
}

#[test]
fn opus_state_carries_cache_suppression_and_empty_name_from_profile() {
    // These fields replaced the hardcoded
    // `REPEATED_CACHE_HIT_SUPPRESSION_THRESHOLD` / `MAX_CONSECUTIVE_EMPTY_NAME`
    // constants that used to live in `headless_tool_pipeline`. Opus profile
    // loosens cache suppression to 4 and keeps empty-name cap at 3.
    let state = make_test_loop_state_for_model(Some("claude-opus-4-7"));
    assert_eq!(state.repeated_cache_hit_suppression, 4);
    assert_eq!(state.max_consecutive_empty_name, 3);
}

#[test]
fn haiku_state_tightens_both_new_guards() {
    let state = make_test_loop_state_for_model(Some("claude-haiku-4-5"));
    assert_eq!(state.repeated_cache_hit_suppression, 2);
    assert_eq!(state.max_consecutive_empty_name, 2);
}

#[test]
fn no_model_state_uses_global_default_new_guards() {
    let state = make_test_loop_state_for_model(None);
    assert_eq!(state.repeated_cache_hit_suppression, 3);
    assert_eq!(state.max_consecutive_empty_name, 3);
}
