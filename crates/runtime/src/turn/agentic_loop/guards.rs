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
    should_emit_cache_waste_advisory, should_emit_parallel_batching_advisory,
};
use super::host::{AgenticLoopState, VolatileKind};
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
        ("work_evidence_sufficiency", check_work_evidence_sufficiency),
        (
            "parallel_batching_advisory",
            check_parallel_batching_advisory,
        ),
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

/// Number of successful, non-mutating tool executions inside one owned
/// WorkItem after which the model should explicitly reassess whether its typed
/// expected result is already supported. The threshold is deliberately above
/// the ordinary small investigation path and never removes tool authority.
const WORK_EVIDENCE_REASSESS_CALLS: usize = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD;

/// Count the current WorkItem's evidence path using only typed execution
/// records. The reverse scan is strictly bounded, stops at canonical Work
/// lifecycle boundaries, and declines to classify a path that has mutated the
/// workspace. That keeps this hot-loop check O(1) per model boundary and avoids
/// prompt-text or scenario matching.
fn bounded_read_only_work_evidence_calls(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<usize> {
    let mut successful = 0_usize;
    for record in records
        .iter()
        .rev()
        .take(WORK_EVIDENCE_REASSESS_CALLS.saturating_mul(2))
    {
        if matches!(
            record.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item"
        ) {
            break;
        }
        if !record.was_executed() {
            continue;
        }
        if super::lifecycle::tool_record_is_workspace_mutation(record) {
            return None;
        }
        if record.ok {
            successful = successful.saturating_add(1);
            if successful >= WORK_EVIDENCE_REASSESS_CALLS {
                return Some(successful);
            }
        }
    }
    Some(successful)
}

fn check_work_evidence_sufficiency(
    state: &mut AgenticLoopState,
    _cfg: &GuardConfig,
) -> GuardOutcome {
    if state.stall.any_behavior_advisory_emitted()
        || state
            .runtime_tool_executor
            .as_deref()
            .is_none_or(|executor| !executor.has_active_primary_work_attempt())
    {
        return GuardOutcome::Pass;
    }
    let Some(calls) = bounded_read_only_work_evidence_calls(&state.stall.tool_call_records) else {
        return GuardOutcome::Pass;
    };
    if calls < WORK_EVIDENCE_REASSESS_CALLS {
        return GuardOutcome::Pass;
    }

    state.stall.work_evidence_advisory_emitted = true;
    tracing::info!(
        target: "astra::loop_guard",
        calls,
        round = state.llm_rounds_completed,
        "owned WorkItem evidence-sufficiency advisory observed"
    );
    GuardOutcome::Advisory {
        // Keep decision feedback compact: CurrentUserOnly providers place it
        // on the uncached tail for one request. The count remains in tracing;
        // the model needs only the decision boundary on wire.
        message: "Owned WorkItem: settle_work_item if expected_result is supported; otherwise pursue one specific missing fact.".to_string(),
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::ToolCallRecord;

    fn successful(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ..Default::default()
        }
    }

    #[test]
    fn work_evidence_counter_is_bounded_to_current_lifecycle_item() {
        let mut records = (0..20).map(|_| successful("read_file")).collect::<Vec<_>>();
        records.push(successful("settle_work_item"));
        records.extend((0..3).map(|_| successful("grep")));

        assert_eq!(bounded_read_only_work_evidence_calls(&records), Some(3));
    }

    #[test]
    fn work_evidence_counter_reaches_threshold_without_text_classification() {
        let records = (0..WORK_EVIDENCE_REASSESS_CALLS)
            .map(|index| successful(if index % 2 == 0 { "grep" } else { "read_file" }))
            .collect::<Vec<_>>();

        assert_eq!(
            bounded_read_only_work_evidence_calls(&records),
            Some(WORK_EVIDENCE_REASSESS_CALLS)
        );
    }

    #[test]
    fn work_evidence_counter_defers_to_mutating_execution_paths() {
        let mut records = (0..WORK_EVIDENCE_REASSESS_CALLS)
            .map(|_| successful("read_file"))
            .collect::<Vec<_>>();
        records.push(ToolCallRecord {
            name: "apply_patch".to_string(),
            ok: true,
            args_full: Some(r#"{"patch":"*** Begin Patch"}"#.to_string()),
            ..Default::default()
        });

        assert_eq!(bounded_read_only_work_evidence_calls(&records), None);
    }
}

/// Detect wasteful cache reads (repeated reads hitting the stale-cache
/// guard without follow-up writes) and surface advisory evidence.
///
/// Defers to redundant_reads when both would fire on the same round, and
/// to the same stronger interventions as `check_redundant_reads`.
fn check_cache_waste(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.cache_waste_advisory_emitted
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
