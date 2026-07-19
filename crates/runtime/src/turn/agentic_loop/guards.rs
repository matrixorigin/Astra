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
    cache_waste_advisory_message, cache_wasteful_tools, parallel_batching_advisory_message,
    redundant_reads_advisory_message, should_emit_cache_waste_advisory,
    should_emit_parallel_batching_advisory,
};
use super::host::{AgenticLoopState, VolatileKind};
use astra_turn_core::evaluation::{
    OnlineProgressDecision, OnlineProgressPolicy, OnlineProgressSignals,
    count_redundant_overlapping_reads, decide_online_progress,
};
use astra_turn_core::headless::body_preview::HeadlessStderrStyle;

// ── Pipeline types ─────────────────────────────────────────────────────

/// Outcome of a single guard evaluation.
#[must_use]
pub(crate) enum GuardOutcome {
    /// Guard did not fire; continue to next guard.
    Pass,
    /// Guard fired: push this signal as volatile advisory evidence, optionally
    /// emit `hint` as a yellow stderr line.
    Advisory {
        message: String,
        kind: VolatileKind,
        hint: Option<String>,
    },
}

/// Shared configuration for all guards in a turn.
#[derive(Clone)]
pub(crate) struct GuardConfig {
    pub parallel_batching_force_streak: usize,
    pub redundant_reads_threshold: usize,
    pub cache_waste_threshold: usize,
}

type GuardFn = fn(&mut AgenticLoopState, &GuardConfig) -> GuardOutcome;

/// Ordered list of guards to run before each LLM call.
///
/// Ordering matters: earlier guards set state flags that later guards
/// check to defer when a stronger intervention is already active
/// (e.g. redundant_reads defers to round_budget_phase1).
pub(crate) fn default_guards() -> Vec<(&'static str, GuardFn)> {
    vec![
        (
            "parallel_batching_advisory",
            check_parallel_batching_advisory,
        ),
        ("redundant_reads", check_redundant_reads),
        ("cache_waste", check_cache_waste),
    ]
}

/// Run all registered guards. Model-facing advisory evidence is independent
/// from presentation mode; the caller decides whether returned status hints
/// should be shown to the user.
pub(crate) fn evaluate_guards(
    guards: &[(&str, GuardFn)],
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> Vec<(HeadlessStderrStyle, String)> {
    let mut hints = Vec::new();
    for (name, guard_fn) in guards {
        match guard_fn(state, cfg) {
            GuardOutcome::Pass => {}
            GuardOutcome::Advisory {
                message,
                kind,
                hint,
            } => {
                state.push_volatile(kind, message.clone());
                tracing::info!(
                    target: "astra::loop_guard",
                    guard = name,
                    round = state.llm_rounds_completed,
                    "guard fired"
                );
                if let Some(hint_text) = hint {
                    hints.push((HeadlessStderrStyle::Yellow, hint_text));
                }
            }
        }
    }
    hints
}

// ═══════════════════════════════════════════════════════════════════════
// Individual guard implementations
// ═══════════════════════════════════════════════════════════════════════

/// Surface batching evidence when the model has produced a long streak of
/// single-tool rounds despite prompt-layer guidance. Catches the
/// "exploratory churn" failure mode (sessions 6566d6a8, bbae8641, 6da9cf8f).
fn check_parallel_batching_advisory(
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> GuardOutcome {
    if state.stall.parallel_batching_advisory_emitted
        || !should_emit_parallel_batching_advisory(state, cfg.parallel_batching_force_streak)
    {
        return GuardOutcome::Pass;
    }
    state.stall.parallel_batching_advisory_emitted = true;
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    let msg = parallel_batching_advisory_message(streak, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "parallel_batching_advisory",
        streak,
        round = state.llm_rounds_completed,
        "behavior advisory observed"
    );
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

/// Detect redundant read-only tool calls (repeated `read_file`/`grep` on
/// the same paths without intervening edits) and surface advisory evidence.
///
/// Defers when a stronger intervention is already active for this round
/// (budget phase-1, completion soft-stop, exploration-family phase-2) so we
/// don't stack two advisory messages on top of each other.
fn check_redundant_reads(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    let count = count_redundant_overlapping_reads(&state.stall.tool_call_records);
    let decision = decide_online_progress(
        OnlineProgressSignals {
            tool_calls: state.stall.tool_call_records.len(),
            redundant_overlapping_reads: count,
            stronger_advisory_emitted: state.stall.stronger_advisory_emitted(),
            advisory_already_emitted: state.stall.redundant_reads_advisory_emitted,
        },
        OnlineProgressPolicy {
            redundant_overlapping_reads_threshold: cfg.redundant_reads_threshold,
            ..OnlineProgressPolicy::default()
        },
    );
    let OnlineProgressDecision::ReuseKnownContext {
        redundant_overlapping_reads: count,
    } = decision
    else {
        return GuardOutcome::Pass;
    };

    state.stall.redundant_reads_advisory_emitted = true;
    let msg = redundant_reads_advisory_message(count, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "redundant_reads_advisory",
        count = count,
        threshold = cfg.redundant_reads_threshold,
        round = state.llm_rounds_completed,
        "behavior advisory observed"
    );
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: Some(format!(
            "↻ {count} redundant overlapping reads; nudging model to use existing context…"
        )),
    }
}

/// Detect wasteful cache reads (repeated reads hitting the stale-cache
/// guard without follow-up writes) and surface advisory evidence.
///
/// Defers to redundant_reads when both would fire on the same round, and
/// to the same stronger interventions as `check_redundant_reads`.
fn check_cache_waste(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.stronger_advisory_emitted()
        || state.stall.redundant_reads_advisory_emitted
        || state.stall.cache_waste_advisory_emitted
        || !should_emit_cache_waste_advisory(state, cfg.cache_waste_threshold)
    {
        return GuardOutcome::Pass;
    }
    let wasteful = cache_wasteful_tools(state, cfg.cache_waste_threshold);
    if wasteful.is_empty() {
        return GuardOutcome::Pass;
    }
    state.stall.cache_waste_advisory_emitted = true;
    let msg = cache_waste_advisory_message(&wasteful, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "cache_waste_advisory",
        round = state.llm_rounds_completed,
        tools = ?wasteful,
        threshold = cfg.cache_waste_threshold,
        "behavior advisory observed"
    );
    let tool_list = wasteful
        .iter()
        .map(|(tool, count)| format!("{tool} ({count}x)"))
        .collect::<Vec<_>>()
        .join(", ");
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: Some(format!(
            "↻ repeated cached tool calls on [{tool_list}]; adding reuse advisory…"
        )),
    }
}
