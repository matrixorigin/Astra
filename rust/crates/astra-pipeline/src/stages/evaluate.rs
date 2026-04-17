//! EvaluateStage: goal-gradient budget control and progress evaluation.
//!
//! Runs after each Execute phase. Measures delta progress, adjusts budget,
//! and decides: continue (Plan), self-correct (Reflect), or terminate.
//!
//! # Goal-gradient budget algorithm
//!
//! ```text
//! rate > 0.5  → expand budget (if under pressure), continue
//! 0.1 < rate ≤ 0.5 → continue without expansion
//! rate ≤ 0.1 (2+ rounds) → stalled → trigger Reflect
//! strictly decreasing + last < 0.15 → regressing → trigger Reflect or Fail
//! budget.is_exhausted() → Failed (highest priority check)
//! ```

use crate::engine::{PipelineStage, StageAction};
use crate::event::{EventKind, EventLog};
use crate::state::{AgentPhase, ProgressTracker, TurnOutcome, TurnState, TurnStatus};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Budget expansion factor when progress is good and budget is under pressure.
const GOOD_PROGRESS_EXPANSION: f64 = 1.25;

/// Maximum reflections before the Evaluate stage stops retrying.
pub const MAX_REFLECTIONS_BEFORE_FAIL: usize = 3;

/// Budget usage ratio at or above which expansion is considered.
const EXPANSION_PRESSURE_THRESHOLD: f64 = 0.6;

// ─── Progress categorization ─────────────────────────────────────────────────

/// Categorized progress state for budget decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressCategory {
    /// rate > 0.5: strong forward motion.
    Good,
    /// 0.1 < rate ≤ 0.5: some progress.
    Moderate,
    /// rate ≤ 0.1 for 2+ rounds: stuck.
    Stalled,
    /// Strictly decreasing scores with last < 0.15: getting worse.
    Regressing,
}

/// Determine the progress category from the tracker.
pub fn categorize_progress(progress: &ProgressTracker) -> ProgressCategory {
    if progress.is_regressing() {
        return ProgressCategory::Regressing;
    }
    if progress.is_stalled() {
        return ProgressCategory::Stalled;
    }
    match progress.rate() {
        Some(rate) if rate > 0.5 => ProgressCategory::Good,
        Some(rate) if rate > 0.1 => ProgressCategory::Moderate,
        Some(_) => {
            // Low rate but not yet flagged as stalled (< 2 rounds of data)
            // Give benefit of doubt on first round
            if progress.round_scores.len() < 2 {
                ProgressCategory::Moderate
            } else {
                ProgressCategory::Stalled
            }
        }
        // No data yet (first round) — benefit of doubt
        None => ProgressCategory::Moderate,
    }
}

// ─── EvaluateStage ───────────────────────────────────────────────────────────

/// Evaluates round progress and makes goal-gradient budget decisions.
pub struct EvaluateStage;

#[async_trait::async_trait]
impl PipelineStage for EvaluateStage {
    fn name(&self) -> &str {
        "evaluate"
    }

    async fn execute(
        &self,
        state: &mut TurnState,
        event_log: &mut EventLog,
    ) -> Result<StageAction, String> {
        // 1. Compute delta progress for this round
        let score = state.compute_round_delta_progress();
        state.progress.record(score);

        // 2. Advance round counter
        state.budget.advance_round();

        // 3. Emit progress event
        let rate = state.progress.rate();
        event_log.emit(EventKind::ProgressRecorded { score, rate }, None);

        // 4. Emit budget update
        event_log.emit(
            EventKind::BudgetUpdate {
                tokens_consumed: state.budget.tokens_consumed,
                rounds_used: state.budget.round,
                elapsed_ms: state.budget.elapsed_ms(),
            },
            None,
        );

        // 5. Check stall via tool signature repetition
        let tool_stall = state.detect_stall();
        if let Some(ref reason) = tool_stall {
            event_log.emit(
                EventKind::StallDetected {
                    round: state.budget.round,
                    reason: reason.clone(),
                },
                None,
            );
        }

        // 6. Budget exhaustion check (highest priority)
        if state.budget.is_exhausted() {
            state.outcome = Some(TurnOutcome {
                status: TurnStatus::Exhausted,
                content: format!("Budget exhausted ({})", state.budget.pressure_dimension()),
                failure_reason: Some(format!(
                    "Exhausted {} budget after {} rounds",
                    state.budget.pressure_dimension(),
                    state.budget.round,
                )),
                failed_tools: state.tool_failures.keys().cloned().collect(),
            });
            return Ok(StageAction::Transition(AgentPhase::Failed));
        }

        // 7. Goal-gradient budget decision
        // Tool signature stall overrides progress-based category
        let category = if tool_stall.is_some() {
            ProgressCategory::Stalled
        } else {
            categorize_progress(&state.progress)
        };

        match category {
            ProgressCategory::Good => {
                // Expand budget if under pressure (>60% used)
                let usage_ratio = state.budget.round as f64 / state.budget.max_rounds.max(1) as f64;
                if usage_ratio >= EXPANSION_PRESSURE_THRESHOLD {
                    let old_max = state.budget.max_rounds;
                    state.budget.expand(GOOD_PROGRESS_EXPANSION);
                    if state.budget.max_rounds > old_max {
                        event_log.emit(
                            EventKind::BudgetExpanded {
                                new_max_rounds: state.budget.max_rounds,
                                factor: GOOD_PROGRESS_EXPANSION,
                            },
                            None,
                        );
                    }
                }
                Ok(StageAction::Transition(AgentPhase::Plan))
            }

            ProgressCategory::Moderate => Ok(StageAction::Transition(AgentPhase::Plan)),

            ProgressCategory::Stalled => {
                if state.reflections.len() >= MAX_REFLECTIONS_BEFORE_FAIL {
                    state.outcome = Some(TurnOutcome {
                        status: TurnStatus::Failure,
                        content: "Agent stalled after maximum reflections".into(),
                        failure_reason: Some(format!(
                            "Stall not recoverable after {} reflections",
                            MAX_REFLECTIONS_BEFORE_FAIL,
                        )),
                        failed_tools: state.tool_failures.keys().cloned().collect(),
                    });
                    Ok(StageAction::Transition(AgentPhase::Failed))
                } else {
                    Ok(StageAction::Transition(AgentPhase::Reflect))
                }
            }

            ProgressCategory::Regressing => {
                // Regression is more serious — one fewer reflection allowed
                let regression_limit = MAX_REFLECTIONS_BEFORE_FAIL.saturating_sub(1);
                if state.reflections.len() >= regression_limit {
                    state.outcome = Some(TurnOutcome {
                        status: TurnStatus::Failure,
                        content: "Agent regressing — approach not working".into(),
                        failure_reason: Some("Performance regression detected".into()),
                        failed_tools: state.tool_failures.keys().cloned().collect(),
                    });
                    Ok(StageAction::Transition(AgentPhase::Failed))
                } else {
                    Ok(StageAction::Transition(AgentPhase::Reflect))
                }
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventLog;
    use crate::state::TurnState;

    /// Create a TurnState with budget of 10 rounds.
    fn make_state() -> TurnState {
        TurnState::new("test query", vec![], 10, 100_000, 300_000)
    }

    /// Simulate a round with some tool calls and text output.
    fn simulate_round(
        state: &mut TurnState,
        text: &str,
        tool_calls: u32,
        failures: &[(&str, &str)],
    ) {
        let round = state.round_tool_signatures.len();
        state.final_text.push_str(text);
        state.total_tool_calls += tool_calls;
        for (tool, err) in failures {
            state.record_tool_failure(tool, err);
        }
        // Unique tool names per round to avoid triggering stall detection
        let tools: std::collections::HashSet<String> = (0..tool_calls)
            .map(|i| format!("tool_r{}_i{}", round, i))
            .collect();
        state.record_round_tools(tools);
    }

    #[tokio::test]
    async fn evaluate_good_progress_continues_to_plan() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Simulate a good round: lots of text + tool calls + no failures
        simulate_round(&mut state, &"x".repeat(200), 3, &[]);
        state.phase = AgentPhase::Evaluate;

        let action = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));

        // Progress was recorded
        assert_eq!(state.progress.round_scores.len(), 1);
        assert!(state.progress.round_scores[0] > 0.5);

        // Round advanced
        assert_eq!(state.budget.round, 1);
    }

    #[tokio::test]
    async fn evaluate_good_progress_expands_budget_under_pressure() {
        let stage = EvaluateStage;
        // Small budget (5 rounds) so we can easily reach >60% usage
        let mut state = TurnState::new("test", vec![], 5, 100_000, 300_000);
        let mut log = EventLog::new();

        // Run 3 good rounds to get to 60%+ usage
        for _ in 0..3 {
            simulate_round(&mut state, &"x".repeat(200), 2, &[]);
            state.phase = AgentPhase::Evaluate;
            let _ = stage.execute(&mut state, &mut log).await.unwrap();
        }

        // Budget should have been expanded
        assert!(state.budget.max_rounds > 5, "Budget should expand from 5");
    }

    #[tokio::test]
    async fn evaluate_budget_expansion_capped_at_2x() {
        let stage = EvaluateStage;
        let mut state = TurnState::new("test", vec![], 5, 100_000, 300_000);
        let mut log = EventLog::new();

        // Run many good rounds to push expansion to max
        for _ in 0..8 {
            simulate_round(&mut state, &"x".repeat(200), 2, &[]);
            state.phase = AgentPhase::Evaluate;
            let _ = stage.execute(&mut state, &mut log).await;
        }

        // Max is 2x base = 10
        assert!(state.budget.max_rounds <= 10);
    }

    #[tokio::test]
    async fn evaluate_stall_triggers_reflect() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Simulate 3 rounds of zero progress (no text, no tools)
        for _ in 0..3 {
            state.phase = AgentPhase::Evaluate;
            let _ = stage.execute(&mut state, &mut log).await.unwrap();
        }

        // After 3 zero-progress rounds, should be stalled
        let category = categorize_progress(&state.progress);
        assert_eq!(category, ProgressCategory::Stalled);
    }

    #[tokio::test]
    async fn evaluate_regression_triggers_reflect_or_fail() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Round 1: good progress
        simulate_round(&mut state, &"x".repeat(300), 5, &[]);
        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        // Round 2: moderate progress
        simulate_round(&mut state, &"x".repeat(50), 2, &[]);
        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        // Round 3: zero progress
        state.phase = AgentPhase::Evaluate;
        let action = stage.execute(&mut state, &mut log).await.unwrap();

        // Should trigger Reflect (regression or stall)
        assert!(
            action == StageAction::Transition(AgentPhase::Reflect)
                || action == StageAction::Transition(AgentPhase::Plan),
            "Expected Reflect or Plan, got {:?}",
            action
        );
    }

    #[tokio::test]
    async fn evaluate_budget_exhausted_fails() {
        let stage = EvaluateStage;
        // Budget of only 1 round
        let mut state = TurnState::new("test", vec![], 1, 100_000, 300_000);
        let mut log = EventLog::new();

        simulate_round(&mut state, "some text", 1, &[]);
        state.phase = AgentPhase::Evaluate;

        let action = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action, StageAction::Transition(AgentPhase::Failed));
        assert!(state.outcome.is_some());
        assert_eq!(
            state.outcome.as_ref().unwrap().status,
            TurnStatus::Exhausted
        );
    }

    #[tokio::test]
    async fn evaluate_max_reflections_then_stall_fails() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Pre-fill 3 reflections (max)
        use crate::state::{Reflection, StrategyDelta};
        for _ in 0..MAX_REFLECTIONS_BEFORE_FAIL {
            state.reflections.push(Reflection {
                what_happened: "stall".into(),
                why: "repeated".into(),
                what_to_try: "different".into(),
                confidence: 0.3,
                strategy_delta: StrategyDelta::default(),
            });
        }

        // Two zero-progress rounds to trigger stall
        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();
        state.phase = AgentPhase::Evaluate;
        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Failed));
        assert_eq!(state.outcome.as_ref().unwrap().status, TurnStatus::Failure);
    }

    #[tokio::test]
    async fn evaluate_moderate_progress_no_expansion() {
        let stage = EvaluateStage;
        let mut state = TurnState::new("test", vec![], 5, 100_000, 300_000);
        let mut log = EventLog::new();

        // Round with small text (moderate progress)
        simulate_round(&mut state, "short", 1, &[]);
        state.phase = AgentPhase::Evaluate;

        let action = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));

        // Budget NOT expanded (not under pressure + only moderate)
        assert_eq!(state.budget.max_rounds, 5);
    }

    #[tokio::test]
    async fn evaluate_records_progress_and_emits_events() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        simulate_round(&mut state, "hello", 2, &[]);
        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        // ProgressRecorded event emitted
        assert!(
            log.events()
                .iter()
                .any(|e| { matches!(e.kind, EventKind::ProgressRecorded { .. }) })
        );
        // BudgetUpdate event emitted
        assert!(
            log.events()
                .iter()
                .any(|e| { matches!(e.kind, EventKind::BudgetUpdate { .. }) })
        );
    }

    #[test]
    fn categorize_first_round_is_moderate() {
        let progress = ProgressTracker::new();
        assert_eq!(categorize_progress(&progress), ProgressCategory::Moderate);
    }

    #[test]
    fn categorize_good_progress() {
        let mut progress = ProgressTracker::new();
        progress.record(0.8);
        progress.record(0.9);
        assert_eq!(categorize_progress(&progress), ProgressCategory::Good);
    }

    #[test]
    fn categorize_stalled() {
        let mut progress = ProgressTracker::new();
        // Not strictly decreasing (avoids triggering Regressing)
        progress.record(0.05);
        progress.record(0.03);
        progress.record(0.05);
        assert_eq!(categorize_progress(&progress), ProgressCategory::Stalled);
    }

    #[test]
    fn categorize_regressing() {
        let mut progress = ProgressTracker::new();
        progress.record(0.5);
        progress.record(0.3);
        progress.record(0.1);
        assert_eq!(categorize_progress(&progress), ProgressCategory::Regressing);
    }
}
