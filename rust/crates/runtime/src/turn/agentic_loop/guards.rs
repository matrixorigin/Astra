//! Composable mid-loop guards extracted from `execute_turn_and_ingest_phase`.
//!
//! Each guard is a self-contained fn that checks a condition on
//! `AgenticLoopState`, optionally sets a state flag, and returns a
//! `GuardOutcome`. Guards were previously inlined as ~30-line blocks
//! inside a 1100-line function; extracting them here gives each a
//! documented home, makes them independently testable, and reduces the
//! orchestration function to a readable pipeline.
//!
//! # Adding a new guard
//!
//! 1. Add a `check_<name>(state, cfg) -> GuardOutcome` function below.
//! 2. Register it in `default_guards()`.
//! 3. Write a unit test using a minimal `AgenticLoopState` fixture.
//! 4. Remove the corresponding inline block from
//!    `execute_turn_and_ingest_phase`.

use super::execution_phase::{
    cache_waste_corrective_message, cache_wasteful_tools, parallel_batching_force_message,
    redundant_reads_corrective_message, should_force_parallel_batching,
    should_inject_cache_waste_corrective, should_inject_redundant_reads_corrective,
};
use super::host::{AgenticLoopState, VolatileKind};
use astra_turn_core::evaluation::count_redundant_overlapping_reads;
use std::panic::{AssertUnwindSafe, catch_unwind};

// ── Pipeline types ─────────────────────────────────────────────────────

/// Outcome of a single guard evaluation.
#[must_use]
#[allow(dead_code)]
pub(crate) enum GuardOutcome {
    /// Guard did not fire; continue to next guard.
    Pass,
    /// Guard fired: push this message as a volatile correction.
    Correct {
        message: String,
        kind: VolatileKind,
        mutation: GuardMutation,
    },
    /// Guard triggered a loop abort; stop the turn immediately.
    Abort { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuardMutation {
    ParallelBatching,
    RedundantReadsCorrective,
    CacheWasteCorrective,
}

impl GuardMutation {
    fn apply(self, state: &mut AgenticLoopState) {
        match self {
            Self::ParallelBatching => {
                state.stall.forced_parallel_batching = true;
            }
            Self::RedundantReadsCorrective => {
                state.stall.forced_redundant_reads_corrective = true;
            }
            Self::CacheWasteCorrective => {
                state.stall.forced_cache_waste_corrective = true;
            }
        }
    }
}

/// Shared configuration for all guards in a turn.
#[derive(Clone)]
pub(crate) struct GuardConfig {
    pub suppress_nudges: bool,
    pub parallel_batching_force_streak: usize,
    pub redundant_reads_threshold: usize,
    pub cache_waste_threshold: usize,
}

type GuardFn = fn(&AgenticLoopState, &GuardConfig) -> GuardOutcome;

/// Render a panic payload (`Box<dyn Any>`) into a human-readable string.
///
/// Guards are sandboxed via `catch_unwind`; a panicking guard must not
/// take down the turn loop. The captured payload is `Box<dyn Any>` and
/// may originate from `panic!("msg")` (which boxes a `&'static str` or
/// `String`) or `panic_any(value)` (which boxes arbitrary types). Only
/// `&'static str` and `String` were previously extracted; everything else
/// collapsed to "<non-string panic payload>", losing the actual value for
/// numeric payloads (`panic_any(42)`) that would meaningfully aid
/// debugging.
///
/// This helper walks a small set of common concrete payload types and
/// falls back to a typed marker naming the payload's `type_id`-derived
/// type name so an operator at least knows what type panicked, rather
/// than a generic "non-string" label.
fn panic_payload_to_string(payload: Box<dyn std::any::Any>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    // Numeric payloads are common in assertion-style guards (`panic_any(n)`).
    if let Some(n) = payload.downcast_ref::<i32>() {
        return n.to_string();
    }
    if let Some(n) = payload.downcast_ref::<i64>() {
        return n.to_string();
    }
    if let Some(n) = payload.downcast_ref::<u32>() {
        return n.to_string();
    }
    if let Some(n) = payload.downcast_ref::<u64>() {
        return n.to_string();
    }
    if let Some(n) = payload.downcast_ref::<usize>() {
        return n.to_string();
    }
    if let Some(b) = payload.downcast_ref::<bool>() {
        return b.to_string();
    }
    "<non-string panic payload>".to_string()
}

/// Ordered list of guards to run before each LLM call.
pub(crate) fn default_guards() -> Vec<(&'static str, GuardFn)> {
    vec![
        ("parallel_batching_force", check_parallel_batching_force),
        ("redundant_reads", check_redundant_reads),
        ("cache_waste", check_cache_waste),
    ]
}

/// Run all registered guards. Returns collected corrections or an abort reason.
/// All guards are skipped when `cfg.suppress_nudges` is true.
pub(crate) fn evaluate_guards(
    guards: &[(&str, GuardFn)],
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> Result<Vec<(String, VolatileKind)>, String> {
    if cfg.suppress_nudges {
        return Ok(Vec::new());
    }
    let mut corrections = Vec::new();
    let mut corrective_fired = false;
    for (name, guard_fn) in guards {
        // Guards are pure readers. A panic inside one guard must not take down
        // the whole turn loop, and because mutations are returned as data and
        // applied only after a successful outcome, panic isolation cannot leave
        // AgenticLoopState half-mutated.
        let outcome = match catch_unwind(AssertUnwindSafe(|| guard_fn(state, cfg))) {
            Ok(outcome) => outcome,
            Err(payload) => {
                let panic_msg = panic_payload_to_string(payload);
                tracing::error!(
                    target: "astra::loop_guard",
                    guard = name,
                    panic = %panic_msg,
                    round = state.llm_rounds_completed,
                    "guard panicked — isolating failure and aborting turn"
                );
                GuardOutcome::Abort {
                    reason: format!("guard `{name}` panicked: {panic_msg}"),
                }
            }
        };
        // First-corrective-wins: a second Corrective on the same turn is
        // downgraded to Pass. Stacked corrections overload the model with
        // contradictory directives (0619_job2 review: redundant_reads +
        // cache_waste firing together). Abort still propagates — safety
        // short-circuits are never suppressed by a prior corrective.
        let outcome = if corrective_fired && matches!(outcome, GuardOutcome::Correct { .. }) {
            GuardOutcome::Pass
        } else {
            outcome
        };
        match outcome {
            GuardOutcome::Pass => {}
            GuardOutcome::Correct {
                message,
                kind,
                mutation,
            } => {
                mutation.apply(state);
                state.push_volatile(kind, message.clone());
                tracing::info!(
                    target: "astra::loop_guard",
                    guard = name,
                    round = state.llm_rounds_completed,
                    "guard fired"
                );
                corrections.push((message, kind));
                corrective_fired = true;
            }
            GuardOutcome::Abort { reason } => return Err(reason),
        }
    }
    Ok(corrections)
}

// ═══════════════════════════════════════════════════════════════════════
// Individual guard implementations
// ═══════════════════════════════════════════════════════════════════════

/// Force parallel batching when the model has produced a long streak of
/// single-tool rounds despite prompt-layer guidance. Catches the
/// "exploratory churn" failure mode (sessions 6566d6a8, bbae8641, 6da9cf8f).
fn check_parallel_batching_force(state: &AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.forced_parallel_batching
        || !should_force_parallel_batching(state, cfg.parallel_batching_force_streak)
    {
        return GuardOutcome::Pass;
    }
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    let msg = parallel_batching_force_message(streak, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "parallel_batching_force",
        streak,
        round = state.llm_rounds_completed,
        "loop guard fired"
    );
    GuardOutcome::Correct {
        message: msg,
        kind: VolatileKind::Corrective,
        mutation: GuardMutation::ParallelBatching,
    }
}

/// Detect redundant read-only tool calls (repeated `read_file`/`grep` on
/// the same paths without intervening edits) and inject a corrective nudge.
fn check_redundant_reads(state: &AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.forced_redundant_reads_corrective
        || !should_inject_redundant_reads_corrective(state, cfg.redundant_reads_threshold)
    {
        return GuardOutcome::Pass;
    }
    let count = count_redundant_overlapping_reads(&state.stall.tool_call_records);
    let msg = redundant_reads_corrective_message(count, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "redundant_reads_corrective",
        count = count,
        round = state.llm_rounds_completed,
        "loop guard fired"
    );
    GuardOutcome::Correct {
        message: msg,
        kind: VolatileKind::Corrective,
        mutation: GuardMutation::RedundantReadsCorrective,
    }
}

/// Detect wasteful cache reads (repeated reads hitting the stale-cache
/// guard without follow-up writes) and inject a corrective nudge.
fn check_cache_waste(state: &AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.forced_cache_waste_corrective
        || !should_inject_cache_waste_corrective(state, cfg.cache_waste_threshold)
    {
        return GuardOutcome::Pass;
    }
    let wasteful = cache_wasteful_tools(state, cfg.cache_waste_threshold);
    // Contract: `should_inject_cache_waste_corrective` returning true implies
    // `cache_wasteful_tools` is non-empty. The prior `if wasteful.is_empty()
    // { return Pass }` branch was dead code (upstream predicate already
    // guaranteed non-empty). If the contract ever breaks, the debug
    // assertion surfaces it loudly rather than silently swallowing the guard.
    debug_assert!(
        !wasteful.is_empty(),
        "should_inject_cache_waste_corrective contract violated: returned true but no wasteful tools found"
    );
    let msg = cache_waste_corrective_message(&wasteful, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "cache_waste_corrective",
        round = state.llm_rounds_completed,
        tools = ?wasteful,
        "loop guard fired"
    );
    GuardOutcome::Correct {
        message: msg,
        kind: VolatileKind::Corrective,
        mutation: GuardMutation::CacheWasteCorrective,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop::host::tests::make_state;

    fn cfg(suppress: bool) -> GuardConfig {
        GuardConfig {
            suppress_nudges: suppress,
            parallel_batching_force_streak: usize::MAX,
            redundant_reads_threshold: usize::MAX,
            cache_waste_threshold: usize::MAX,
        }
    }

    // Pass must never inject a correction or abort — the fundamental
    // pipeline invariant: stays inert unless a guard explicitly fires.
    #[test]
    fn pass_only_pipeline_is_inert() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![("noop", |_, _| GuardOutcome::Pass)];
        let corrections =
            evaluate_guards(&guards, &mut state, &cfg(false)).expect("Pass guards must not abort");
        assert!(corrections.is_empty(), "Pass must produce zero corrections");
    }

    // Correct must push a volatile and record the correction so downstream
    // rendering surfaces it — proving the pipeline doesn't swallow it.
    #[test]
    fn correct_outcome_pushes_volatile_and_records_correction() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![("force_correct", |_, _| GuardOutcome::Correct {
            message: "nudge".to_string(),
            kind: VolatileKind::Corrective,
            mutation: GuardMutation::ParallelBatching,
        })];
        let corrections =
            evaluate_guards(&guards, &mut state, &cfg(false)).expect("Correct must not abort");
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].0, "nudge");
        assert!(
            !state.volatile_pending.is_empty(),
            "volatile must be queued for the host"
        );
        assert!(
            state.stall.forced_parallel_batching,
            "guard mutation must be applied with the volatile correction"
        );
    }

    #[test]
    fn guard_functions_are_read_only_until_pipeline_applies_mutation() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![("force_correct", |_, _| GuardOutcome::Correct {
            message: "nudge".to_string(),
            kind: VolatileKind::Corrective,
            mutation: GuardMutation::RedundantReadsCorrective,
        })];

        assert!(!state.stall.forced_redundant_reads_corrective);
        let corrections =
            evaluate_guards(&guards, &mut state, &cfg(false)).expect("Correct must not abort");

        assert_eq!(corrections.len(), 1);
        assert!(state.stall.forced_redundant_reads_corrective);
    }

    // Abort must short-circuit: later guards must not run. The panicking
    // second guard proves the short-circuit because it would crash the
    // test process otherwise.
    #[test]
    fn abort_short_circuits_and_propagates_reason() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![
            ("abort", |_, _| GuardOutcome::Abort {
                reason: "stop now".into(),
            }),
            ("must_not_run", |_, _| {
                panic!("guard after abort must not run")
            }),
        ];
        let err = evaluate_guards(&guards, &mut state, &cfg(false))
            .expect_err("Abort must surface as Err");
        assert_eq!(err, "stop now");
    }

    // suppress_nudges (Auto mode): zero guards evaluated, zero volatiles.
    // The safety contract that prevents "不停的被打断" regressions.
    #[test]
    fn suppress_nudges_skips_all_guards() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![("would_fire", |_, _| {
            panic!("suppressed pipeline must not call guards")
        })];
        let corrections =
            evaluate_guards(&guards, &mut state, &cfg(true)).expect("suppress must not abort");
        assert!(corrections.is_empty());
        assert!(state.volatile_pending.is_empty());
    }

    // Panic isolation: a bug in one guard must not crash the host process.
    // The pipeline catches the unwind and converts it to an Abort with a
    // machine-readable reason rather than a panic backtrace.
    #[test]
    fn panicking_guard_is_isolated_and_converted_to_abort() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> =
            vec![("panic_guard", |_, _| panic!("simulated guard bug"))];
        let err = evaluate_guards(&guards, &mut state, &cfg(false))
            .expect_err("panicking guard must surface as Abort, not propagate");
        assert!(
            err.contains("panic_guard") && err.contains("simulated guard bug"),
            "abort reason must name the guard and its payload: {err}"
        );
    }

    // Non-string panic payload must not break recovery — the pipeline falls
    // back to a placeholder rather than re-panicking during downcast.
    #[test]
    fn panicking_guard_with_non_string_payload_is_handled() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> =
            vec![("panic_struct", |_, _| std::panic::panic_any(42i32))];
        let err = evaluate_guards(&guards, &mut state, &cfg(false))
            .expect_err("non-string panic must still surface as Abort");
        assert!(
            err.contains("panic_struct"),
            "reason must name the guard: {err}"
        );
        // The numeric payload must be rendered, not collapsed to the generic
        // "<non-string panic payload>" placeholder. Losing the value hides
        // the actual cause from operators debugging a guard crash.
        assert!(
            err.contains('4') && err.contains('2'),
            "reason must render the numeric payload value, got: {err}"
        );
        assert!(
            !err.contains("<non-string panic payload>"),
            "i32 payload must not fall through to the generic placeholder, got: {err}"
        );
    }

    // After an isolated panic, a later guard in the SAME pipeline must not
    // execute — the abort short-circuits before reaching it.
    #[test]
    fn panic_aborts_before_subsequent_guards_run() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![
            ("panic_first", |_, _| panic!("boom")),
            ("never_called", |_, _| {
                panic!("second guard ran after panic")
            }),
        ];
        let err = evaluate_guards(&guards, &mut state, &cfg(false)).expect_err("must abort");
        assert!(err.contains("panic_first"));
    }

    // First-corrective-wins: at most one corrective volatile per turn.
    // Multiple corrections stacked in one turn overload the model with
    // contradictory directives (observed: redundant_reads + cache_waste
    // firing together in 0619_job2 review). The pipeline must serialize.
    #[test]
    fn at_most_one_corrective_per_turn() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![
            ("first_corrective", |_, _| GuardOutcome::Correct {
                message: "first".to_string(),
                kind: VolatileKind::Corrective,
                mutation: GuardMutation::ParallelBatching,
            }),
            ("second_corrective", |_, _| GuardOutcome::Correct {
                message: "second".to_string(),
                kind: VolatileKind::Corrective,
                mutation: GuardMutation::CacheWasteCorrective,
            }),
            ("third_corrective", |_, _| GuardOutcome::Correct {
                message: "third".to_string(),
                kind: VolatileKind::Corrective,
                mutation: GuardMutation::RedundantReadsCorrective,
            }),
        ];
        let corrections =
            evaluate_guards(&guards, &mut state, &cfg(false)).expect("Corrective must not abort");
        assert_eq!(
            corrections.len(),
            1,
            "exactly one corrective must fire per turn, got {corrections:?}"
        );
        assert_eq!(
            corrections[0].0, "first",
            "first guard wins: {corrections:?}"
        );
    }

    // First-corrective-wins does not suppress Abort — safety aborts still
    // short-circuit immediately even after a corrective fired.
    #[test]
    fn abort_still_short_circuits_after_corrective() {
        let mut state = make_state();
        let guards: Vec<(&str, GuardFn)> = vec![
            ("corrective", |_, _| GuardOutcome::Correct {
                message: "nudge".to_string(),
                kind: VolatileKind::Corrective,
                mutation: GuardMutation::ParallelBatching,
            }),
            ("abort", |_, _| GuardOutcome::Abort {
                reason: "safety stop".into(),
            }),
        ];
        let err = evaluate_guards(&guards, &mut state, &cfg(false))
            .expect_err("Abort must propagate after Corrective");
        assert_eq!(err, "safety stop");
    }
}
