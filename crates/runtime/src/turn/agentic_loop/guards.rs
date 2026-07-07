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
use astra_turn_core::headless::body_preview::HeadlessStderrStyle;

// ── Pipeline types ─────────────────────────────────────────────────────

/// Outcome of a single guard evaluation.
#[must_use]
pub(crate) enum GuardOutcome {
    /// Guard did not fire; continue to next guard.
    Pass,
    /// Guard fired: push this message as a volatile correction, optionally
    /// emit `hint` as a yellow stderr line.
    Correct {
        message: String,
        kind: VolatileKind,
        hint: Option<String>,
    },
    /// Guard triggered a loop abort; stop the turn immediately.
    /// Used when a guard detects an unrecoverable loop pattern (e.g.
    /// corrective streak exceeded the hard abort threshold). The pipeline
    /// converts this into an `InterruptionKind::GuardAbort` so resume UIs
    /// surface the reason instead of an opaque empty completion.
    #[allow(dead_code)]
    Abort { reason: String },
}

/// Shared configuration for all guards in a turn.
#[derive(Clone)]
pub(crate) struct GuardConfig {
    pub suppress_nudges: bool,
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
        ("parallel_batching_force", check_parallel_batching_force),
        ("redundant_reads", check_redundant_reads),
        ("cache_waste", check_cache_waste),
    ]
}

/// Run all registered guards. Returns collected corrections (each carrying
/// an optional stderr hint) or an abort reason.
/// All guards are skipped when `cfg.suppress_nudges` is true.
pub(crate) fn evaluate_guards(
    guards: &[(&str, GuardFn)],
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> Result<Vec<(HeadlessStderrStyle, String)>, String> {
    if cfg.suppress_nudges {
        return Ok(Vec::new());
    }
    let mut corrections = Vec::new();
    for (name, guard_fn) in guards {
        match guard_fn(state, cfg) {
            GuardOutcome::Pass => {}
            GuardOutcome::Correct {
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
                    corrections.push((HeadlessStderrStyle::Yellow, hint_text));
                }
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
fn check_parallel_batching_force(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.forced_parallel_batching
        || !should_force_parallel_batching(state, cfg.parallel_batching_force_streak)
    {
        return GuardOutcome::Pass;
    }
    state.stall.forced_parallel_batching = true;
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
        hint: None,
    }
}

/// Detect redundant read-only tool calls (repeated `read_file`/`grep` on
/// the same paths without intervening edits) and inject a corrective nudge.
///
/// Defers when a stronger intervention is already active for this round
/// (budget phase-1, completion soft-stop, exploration-family phase-2) so we
/// don't stack two correction messages on top of each other.
fn check_redundant_reads(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.hard_intervention_active()
        || state.stall.forced_redundant_reads_corrective
        || !should_inject_redundant_reads_corrective(state, cfg.redundant_reads_threshold)
    {
        return GuardOutcome::Pass;
    }
    let count = count_redundant_overlapping_reads(&state.stall.tool_call_records);
    state.stall.forced_redundant_reads_corrective = true;
    let msg = redundant_reads_corrective_message(count, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "redundant_reads_corrective",
        count = count,
        threshold = cfg.redundant_reads_threshold,
        round = state.llm_rounds_completed,
        "loop guard fired"
    );
    GuardOutcome::Correct {
        message: msg,
        kind: VolatileKind::Corrective,
        hint: Some(format!(
            "↻ {count} redundant overlapping reads; nudging model to use existing context…"
        )),
    }
}

/// Detect wasteful cache reads (repeated reads hitting the stale-cache
/// guard without follow-up writes) and inject a corrective nudge.
///
/// Defers to redundant_reads when both would fire on the same round, and
/// to the same stronger interventions as `check_redundant_reads`.
fn check_cache_waste(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.hard_intervention_active()
        || state.stall.forced_redundant_reads_corrective
        || state.stall.forced_cache_waste_corrective
        || !should_inject_cache_waste_corrective(state, cfg.cache_waste_threshold)
    {
        return GuardOutcome::Pass;
    }
    let wasteful = cache_wasteful_tools(state, cfg.cache_waste_threshold);
    if wasteful.is_empty() {
        return GuardOutcome::Pass;
    }
    state.stall.forced_cache_waste_corrective = true;
    let msg = cache_waste_corrective_message(&wasteful, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "cache_waste_corrective",
        round = state.llm_rounds_completed,
        tools = ?wasteful,
        threshold = cfg.cache_waste_threshold,
        "loop guard fired"
    );
    let tool_list = wasteful
        .iter()
        .map(|(tool, count)| format!("{tool} ({count}x)"))
        .collect::<Vec<_>>()
        .join(", ");
    GuardOutcome::Correct {
        message: msg,
        kind: VolatileKind::Corrective,
        hint: Some(format!(
            "↻ repeated cached tool calls on [{tool_list}]; forcing reuse corrective…"
        )),
    }
}
