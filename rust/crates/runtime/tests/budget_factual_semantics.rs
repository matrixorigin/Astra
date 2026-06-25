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

// Pure-fact contract (first principle): a runtime event notification
// states what happened and the authoritative user request. It must NOT
// introduce a behavioral fork ("Continue only if X, otherwise Y") because
// weak models under budget pressure read the fork as license to stop early,
// and must NOT issue imperatives ("Avoid ...", "Do NOT ..."). The model's
// action space stays unconstrained; the latest user request remains
// authoritative.

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
fn budget_reached_advisory_has_no_behavioral_fork() {
    // "Continue only if X; otherwise answer concisely" is a fork that weak
    // models resolve toward early termination under budget pressure. A
    // runtime fact notification must not branch the agent's behavior.
    let msg = BUDGET_REACHED_ADVISORY.to_lowercase();
    assert!(
        !msg.contains("otherwise"),
        "budget advisory must not introduce a behavioral fork, got: {BUDGET_REACHED_ADVISORY:?}"
    );
    assert!(
        !msg.contains("concisely") && !msg.contains("concise"),
        "budget advisory must not suggest early termination via brevity, got: {BUDGET_REACHED_ADVISORY:?}"
    );
    assert!(
        !msg.contains("continue only"),
        "budget advisory must not gate continuation, got: {BUDGET_REACHED_ADVISORY:?}"
    );
}

#[test]
fn budget_reached_advisory_has_no_imperatives() {
    // Imperatives ("Avoid re-reading ...", "Do NOT ...") are directives,
    // not facts. The runtime reports state; the latest user request
    // decides behavior.
    let msg = BUDGET_REACHED_ADVISORY.to_lowercase();
    assert!(
        !msg.contains("avoid "),
        "budget advisory must not contain imperatives, got: {BUDGET_REACHED_ADVISORY:?}"
    );
    assert!(
        !msg.contains("do not") && !msg.contains("don't"),
        "budget advisory must not contain imperatives, got: {BUDGET_REACHED_ADVISORY:?}"
    );
}

#[test]
fn compact_resume_directive_is_pure_fact() {
    // The compact-resume directive fires right after a successful
    // compact+spill. Per the design docstring it must state what happened
    // and NOT prescribe whether to continue or summarize. No behavioral
    // fork, no imperatives, no gating of continuation.
    let msg = COMPACT_RESUME_DIRECTIVE.to_lowercase();
    // Must reference the compaction event so the model knows why the
    // history looks shorter.
    assert!(
        msg.contains("compact") || msg.contains("compress") || msg.contains("freed"),
        "compact-resume directive should reference the compaction event, got: {COMPACT_RESUME_DIRECTIVE:?}"
    );
    // No behavioral fork.
    assert!(
        !msg.contains("otherwise"),
        "compact-resume directive must not introduce a behavioral fork, got: {COMPACT_RESUME_DIRECTIVE:?}"
    );
    assert!(
        !msg.contains("continue only"),
        "compact-resume directive must not gate continuation, got: {COMPACT_RESUME_DIRECTIVE:?}"
    );
    // No imperatives.
    assert!(
        !msg.contains("do not") && !msg.contains("don't") && !msg.contains("avoid "),
        "compact-resume directive must not contain imperatives, got: {COMPACT_RESUME_DIRECTIVE:?}"
    );
}

// ─── 3. Adaptive auto-reduction is off by default ────────────────────────

#[test]
fn adaptive_budget_reduction_is_opt_in_not_default() {
    // The 85% -> reduce-budget path in agentic::adaptive_runtime is a
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
