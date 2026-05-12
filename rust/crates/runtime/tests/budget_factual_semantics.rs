//! Contract: at high context pressure the runtime must surface FACTS
//! (usage, compaction results), never issue disabling directives. A hard
//! "Do NOT call any more tools. Summarize progress." is the prompt that
//! produces the very behaviour users complain about — the agent stops
//! after one compaction cycle and refuses to continue a task that would
//! still fit in the post-compact window.
//!
//! Scope (one red per concern, all locked into one file so a single
//! red/green cycle covers the full "pressure-handling" refactor):
//!   1. Default budget must follow the model's context window, not a
//!      fixed 200K. Sonnet 4.6 / Opus 4.6 advertise 1M; a 200K clamp
//!      throws away 80% of the usable window.
//!   2. The budget-exhausted volatile injection must be factual. No
//!      "Do NOT call any more tools", no "Summarize your progress".
//!   3. The adaptive 85%-usage auto-reduction of max_turn_input_tokens
//!      must NOT be active by default — it lowers the ceiling precisely
//!      when the agent most needs headroom (see session 0e37eb46).
//!
//! Test 1 is a behaviour check on `RuntimeLimits::effective_max_turn_input_tokens`.
//! Test 2 asserts the text content of the two volatile templates.
//! Test 3 asserts the default value of `ContextWindowConfig::adaptive_budget`.
//!
//! All three target PUBLIC surfaces so this file compiles as a normal
//! integration test.

use astra_config::runtime_config::ContextWindowConfig;
use astra_core::RuntimeLimits;
use astra_runtime::turn::budget_messaging::{BUDGET_REACHED_ADVISORY, COMPACT_RESUME_DIRECTIVE};

// ─── 1. Model-aware default budget ───────────────────────────────────────

#[test]
fn sonnet_4_6_gets_near_full_one_million_window() {
    // Sonnet 4.6 advertises a 1M context window. The effective budget
    // must use that window (minus a reasonable reserve), not the legacy
    // 200K clamp. A 600K minimum guards against regression where someone
    // reintroduces the 200K upper bound in min(...).
    let limits = RuntimeLimits::default();
    let budget = limits.effective_max_turn_input_tokens(Some("claude-sonnet-4-6"));
    assert!(
        budget >= 600_000,
        "Sonnet 4.6 must expose a near-1M budget, got {budget}"
    );
}

#[test]
fn opus_4_6_gets_near_full_one_million_window() {
    let limits = RuntimeLimits::default();
    let budget = limits.effective_max_turn_input_tokens(Some("claude-opus-4-6"));
    assert!(
        budget >= 600_000,
        "Opus 4.6 must expose a near-1M budget, got {budget}"
    );
}

#[test]
fn gpt_4o_stays_within_128k_window() {
    // For a 128K-window model the budget must land well below 128K so
    // there is room for the output. A typical reserve is ~10K error-
    // recovery + ~4K system prompt + ~15K tool schemas = ~29K, so the
    // budget should be in the 95K–115K band.
    let limits = RuntimeLimits::default();
    let budget = limits.effective_max_turn_input_tokens(Some("gpt-4o"));
    assert!(
        (80_000..115_000).contains(&budget),
        "GPT-4o budget must fit inside the 128K window with a sane reserve, \
         got {budget}"
    );
}

#[test]
fn unknown_model_falls_back_to_configured_default() {
    let limits = RuntimeLimits::default();
    let budget = limits.effective_max_turn_input_tokens(Some("some-local-7b-model"));
    // Unknown model → no special knowledge → configured default
    // (which itself stays 200K for backwards compat with explicit
    // env overrides).
    assert_eq!(budget, limits.max_turn_input_tokens);
}

// ─── 2. Factual volatile messages ────────────────────────────────────────

#[test]
fn budget_reached_advisory_does_not_disable_tools() {
    // The failure mode is a directive like "Do NOT call any more tools."
    // followed by "Summarize your progress." That turns the agent into a
    // read-only entity mid-task. The advisory must be a fact statement,
    // not a constraint imposed on the model's action space.
    let msg = BUDGET_REACHED_ADVISORY.to_lowercase();
    assert!(
        !msg.contains("do not call") && !msg.contains("don't call"),
        "budget advisory must not forbid tool use, got: {BUDGET_REACHED_ADVISORY:?}"
    );
    assert!(
        !msg.contains("summarize your progress") && !msg.contains("summarize progress"),
        "budget advisory must not order a summary, got: {BUDGET_REACHED_ADVISORY:?}"
    );
    // Minimum content: should mention context or budget so the model
    // knows what the message is about.
    assert!(
        msg.contains("context") || msg.contains("budget") || msg.contains("tokens"),
        "budget advisory should surface the relevant fact, got: {BUDGET_REACHED_ADVISORY:?}"
    );
}

#[test]
fn compact_resume_directive_stays_factual_not_adversarial() {
    // This one fires right after a successful compact+spill. Its purpose
    // is to tell the model "history was compressed, continue" without
    // forbidding anything. The current version contains an absolute
    // directive ("Do NOT summarize progress") which is fine tactically
    // but becomes adversarial when combined with BUDGET_REACHED_ADVISORY
    // on the same turn. Keep ONE behavioural nudge ("continue"); facts
    // should dominate.
    let msg = COMPACT_RESUME_DIRECTIVE;
    assert!(
        msg.to_lowercase().contains("continue") || msg.to_lowercase().contains("keep working"),
        "compact-resume directive should tell the model to continue, \
         got: {msg:?}"
    );
    // Sanity: still mentions the compaction event itself so the model
    // knows why the history looks shorter.
    assert!(
        msg.to_lowercase().contains("compact")
            || msg.to_lowercase().contains("compress")
            || msg.to_lowercase().contains("freed"),
        "compact-resume directive should reference the compaction event, \
         got: {msg:?}"
    );
}

// ─── 3. Adaptive auto-reduction is off by default ────────────────────────

#[test]
fn adaptive_budget_reduction_is_opt_in_not_default() {
    // The 85% → reduce-budget path in agentic_adaptive_tuning is a
    // self-defeating loop: at high pressure it LOWERS the ceiling the
    // next turn must fit under, which raises the probability of another
    // high-pressure event, which lowers the ceiling again. Default off;
    // keep it reachable via config for deployments that want it.
    let cfg = ContextWindowConfig::default();
    assert!(
        !cfg.adaptive_budget_reduction,
        "adaptive budget reduction must default to OFF to prevent \
         the shrink-spiral pattern seen in session 0e37eb46"
    );
}
