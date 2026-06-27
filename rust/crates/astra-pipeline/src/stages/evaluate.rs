//! EvaluateStage: policy-driven progress evaluation and budget control.
//!
//! Runs after each Execute phase. The stage itself only performs two
//! framework-primitive checks (budget exhaustion, tool-signature stall).
//! All other decisions — whether to expand budget, transition to reflection,
//! or continue — are delegated to a user-configurable [`BudgetPolicy`].
//!
//! # Design
//!
//! ```text
//! EvaluateStage::execute()
//!   ├── compute delta facts (text_growth, new_tool_calls, failures)
//!   ├── record round outcome → round_outcomes
//!   ├── advance round
//!   ├── emit progress / budget events
//!   ├── detect tool stall (framework primitive)
//!   ├── budget exhaustion check (framework primitive)
//!   └── BudgetPolicy::decide(facts) → actions
//!         ├── ExpandBudget(factor, ceiling) → budget.expand_with_ceiling()
//!         ├── TransitionPhase("reflect")   → AgentPhase::Reflect
//!         └── Continue                     → AgentPhase::Plan
//! ```
//!
//! No hardcoded heuristics. The policy gets pure facts; the framework
//! executes the policy's decisions.

use crate::engine::{PipelineStage, StageAction};
use crate::event::{EventKind, EventLog};
use crate::state::{AgentPhase, TurnOutcome, TurnState, TurnStatus};
use astra_core::observation_journal::{FrameworkAction, JournalFacts};

use serde_json;

// ─── EvaluateStage ───────────────────────────────────────────────────────────

/// Evaluates round progress and delegates to the policy for action decisions.
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
        // 1. Compute delta facts for this round
        let text_growth = state.final_text.len().saturating_sub(state.prev_text_len);
        let new_tool_calls = state.total_tool_calls.saturating_sub(state.prev_tool_calls);
        let current_failures: usize = state.tool_failures.values().map(|v| v.len()).sum();
        let new_failures = current_failures.saturating_sub(state.prev_failure_count);

        // Update snapshots for next round
        state.prev_text_len = state.final_text.len();
        state.prev_tool_calls = state.total_tool_calls;
        state.prev_failure_count = current_failures;

        // Record outcome fact: this round had an observable outcome if it
        // produced text growth or executed tools without failures.
        let had_outcome = text_growth > 0 || (new_tool_calls > 0 && new_failures == 0);
        state.round_outcomes.push(had_outcome);

        // 2. Advance round counter
        state.budget.advance_round();

        // 3. Emit events (observability, not decision-making)
        event_log.emit(
            EventKind::ProgressRecorded {
                score: if had_outcome { 1.0 } else { 0.0 },
                rate: None,
            },
            None,
        );
        event_log.emit(
            EventKind::BudgetUpdate {
                tokens_consumed: state.budget.tokens_consumed,
                rounds_used: state.budget.round,
                elapsed_ms: state.budget.elapsed_ms(),
            },
            None,
        );

        // 4. Tool-stall detection (framework primitive — pure pattern matching)
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

        // 5. Budget exhaustion check (framework primitive — highest priority)
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

        // 6. Compute outcome streaks from round_outcomes
        let consecutive_with_outcome = state
            .round_outcomes
            .iter()
            .rev()
            .take_while(|&&o| o)
            .count() as u32;
        let consecutive_without_outcome = state
            .round_outcomes
            .iter()
            .rev()
            .take_while(|&&o| !o)
            .count() as u32;

        // 7. Build pure facts — no scores, no judgments
        let facts = JournalFacts {
            rounds_completed: state.budget.round,
            consecutive_rounds_with_outcome: consecutive_with_outcome,
            consecutive_rounds_without_outcome: consecutive_without_outcome,
            budget_remaining: state.budget.max_rounds.saturating_sub(state.budget.round),
            budget_max: state.budget.max_rounds,
            stall_reason: tool_stall,
            text_growth,
            new_tool_calls,
            new_failures,
            ..Default::default()
        };

        // 8. Policy decides; framework executes
        let actions = state.budget_policy.decide(&facts);

        for action in actions {
            match action {
                FrameworkAction::ExpandBudget {
                    factor,
                    max_ceiling,
                } => {
                    let old_max = state.budget.max_rounds;
                    state.budget.expand_with_ceiling(factor, max_ceiling);
                    if state.budget.max_rounds > old_max {
                        event_log.emit(
                            EventKind::BudgetExpanded {
                                new_max_rounds: state.budget.max_rounds,
                                factor,
                            },
                            None,
                        );
                    }
                }

                FrameworkAction::TransitionPhase { ref target } => {
                    return Self::handle_phase_transition(state, target);
                }

                FrameworkAction::InjectSignal { ref message } => {
                    state
                        .pending_signals
                        .push(astra_core::observation_journal::PendingSignal {
                            message: message.clone(),
                            injected_at_round: state.budget.round,
                        });
                    // Continue after injecting — don't change phase
                }

                FrameworkAction::Continue => {
                    Self::drain_pending_signals(state);
                    return Ok(StageAction::Transition(AgentPhase::Plan));
                }
            }
        }

        // Fallthrough: if policy returned nothing → continue
        Self::drain_pending_signals(state);
        Ok(StageAction::Transition(AgentPhase::Plan))
    }
}

impl EvaluateStage {
    /// Translate a policy-named phase to an AgentPhase + reflection limit check.
    fn handle_phase_transition(state: &mut TurnState, target: &str) -> Result<StageAction, String> {
        match target {
            "reflect" => {
                let max = state.budget_policy.max_reflections;
                if state.reflections.len() >= max as usize {
                    state.outcome = Some(TurnOutcome {
                        status: TurnStatus::Failure,
                        content: "Agent stalled after maximum reflections".into(),
                        failure_reason: Some(format!(
                            "Stall not recoverable after {} reflections",
                            max,
                        )),
                        failed_tools: state.tool_failures.keys().cloned().collect(),
                    });
                    Ok(StageAction::Transition(AgentPhase::Failed))
                } else {
                    Ok(StageAction::Transition(AgentPhase::Reflect))
                }
            }
            "plan" => {
                Self::drain_pending_signals(state);
                Ok(StageAction::Transition(AgentPhase::Plan))
            }
            _ => {
                Self::drain_pending_signals(state);
                Ok(StageAction::Transition(AgentPhase::Plan))
            }
        }
    }

    /// Drain all pending signals (from policy [`FrameworkAction::InjectSignal`])
    /// and inject them as user messages so the LLM sees them in the next
    /// Plan-phase call.
    fn drain_pending_signals(state: &mut TurnState) {
        for signal in state.pending_signals.drain(..) {
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[Framework signal (round {})] {}",
                    signal.injected_at_round, signal.message
                )
            }));
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventLog;
    use crate::state::TurnState;
    use astra_core::observation_journal::PendingSignal;
    use std::collections::HashSet;

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
        let tools: HashSet<String> = (0..tool_calls)
            .map(|i| format!("tool_r{}_i{}", round, i))
            .collect();
        state.record_round_tools(tools);
    }

    #[tokio::test]
    async fn evaluate_round_with_outcome_continues() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        simulate_round(&mut state, &"x".repeat(200), 3, &[]);
        state.phase = AgentPhase::Evaluate;

        let action = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));

        assert_eq!(state.round_outcomes.len(), 1);
        assert!(state.round_outcomes[0]);
        assert_eq!(state.budget.round, 1);
    }

    #[tokio::test]
    async fn evaluate_multiple_outcome_rounds_expands_budget() {
        let stage = EvaluateStage;
        let mut state = TurnState::new("test", vec![], 5, 100_000, 300_000);
        let mut log = EventLog::new();

        for _ in 0..3 {
            simulate_round(&mut state, &"x".repeat(200), 2, &[]);
            state.phase = AgentPhase::Evaluate;
            let _ = stage.execute(&mut state, &mut log).await.unwrap();
        }

        assert!(state.budget.max_rounds > 5, "Budget should expand from 5");
    }

    #[tokio::test]
    async fn evaluate_budget_expansion_capped_by_ceiling() {
        let stage = EvaluateStage;
        let mut state = TurnState::new("test", vec![], 5, 100_000, 300_000);
        state.budget_policy.max_ceiling = 20;
        let mut log = EventLog::new();

        for _ in 0..15 {
            simulate_round(&mut state, &"x".repeat(200), 2, &[]);
            state.phase = AgentPhase::Evaluate;
            let _ = stage.execute(&mut state, &mut log).await;
        }

        assert!(
            state.budget.max_rounds <= 20,
            "Budget should not exceed ceiling 20, got {}",
            state.budget.max_rounds
        );
    }

    #[tokio::test]
    async fn evaluate_zero_outcome_triggers_reflect() {
        let stage = EvaluateStage;
        let mut state = make_state();
        state.budget_policy.reflect_after_consecutive_zero = 2;
        let mut log = EventLog::new();

        state.phase = AgentPhase::Evaluate;
        let action1 = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action1, StageAction::Transition(AgentPhase::Plan));

        state.phase = AgentPhase::Evaluate;
        let action2 = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action2, StageAction::Transition(AgentPhase::Reflect));
    }

    #[tokio::test]
    async fn evaluate_budget_exhausted_fails() {
        let stage = EvaluateStage;
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
    async fn evaluate_max_reflections_then_fails() {
        let stage = EvaluateStage;
        let mut state = make_state();
        state.budget_policy.max_reflections = 3;
        state.budget_policy.reflect_after_consecutive_zero = 1;
        let mut log = EventLog::new();

        use crate::state::{Reflection, StrategyDelta};
        for _ in 0..3 {
            state.reflections.push(Reflection {
                what_happened: "stall".into(),
                why: "repeated".into(),
                what_to_try: "different".into(),
                confidence: 0.3,
                strategy_delta: StrategyDelta::default(),
            });
        }

        state.phase = AgentPhase::Evaluate;
        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Failed));
        assert_eq!(state.outcome.as_ref().unwrap().status, TurnStatus::Failure);
    }

    #[tokio::test]
    async fn evaluate_tool_stall_triggers_reflect() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        let tools: HashSet<String> = ["bash".to_string()].into();
        for round_idx in 0..3 {
            state.final_text.push_str(&format!("round{round_idx}"));
            state.total_tool_calls += 1;
            state.record_round_tools(tools.clone());
            state.phase = AgentPhase::Evaluate;

            if round_idx < 2 {
                stage.execute(&mut state, &mut log).await.unwrap();
            }
        }

        let action = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action, StageAction::Transition(AgentPhase::Reflect));
    }

    #[tokio::test]
    async fn evaluate_records_progress_and_emits_events() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        simulate_round(&mut state, "hello", 2, &[]);
        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        assert!(log
            .events()
            .iter()
            .any(|e| { matches!(e.kind, EventKind::ProgressRecorded { .. }) }));
        assert!(log
            .events()
            .iter()
            .any(|e| { matches!(e.kind, EventKind::BudgetUpdate { .. }) }));
    }

    #[tokio::test]
    async fn evaluate_with_aggressive_policy() {
        let stage = EvaluateStage;
        let mut state = make_state();
        state.budget_policy.reflect_after_consecutive_zero = 1;
        state.budget_policy.expand_after_consecutive_outcomes = 999;
        let mut log = EventLog::new();

        // First round with no outcome → immediately triggers reflect
        state.phase = AgentPhase::Evaluate;
        let action = stage.execute(&mut state, &mut log).await.unwrap();
        assert_eq!(action, StageAction::Transition(AgentPhase::Reflect));
    }

    // ── pending_signals drain ──────────────────────────────────────────────

    #[tokio::test]
    async fn evaluate_drains_pending_signals_into_messages() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Pre-populate two pending signals (simulating prior InjectSignal actions)
        state.pending_signals.push(PendingSignal {
            message: "signal one".into(),
            injected_at_round: 1,
        });
        state.pending_signals.push(PendingSignal {
            message: "signal two".into(),
            injected_at_round: 2,
        });
        let msg_count_before = state.messages.len();

        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        // Signals must be drained
        assert!(
            state.pending_signals.is_empty(),
            "pending_signals should be drained"
        );
        // Two new messages injected
        assert_eq!(state.messages.len(), msg_count_before + 2);
        // Content includes signal messages
        let all_content: String = state
            .messages
            .iter()
            .filter_map(|v| v["content"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all_content.contains("signal one"));
        assert!(all_content.contains("signal two"));
        assert!(all_content.contains("Framework signal"));
    }

    #[tokio::test]
    async fn evaluate_drains_empty_pending_signals_is_noop() {
        let stage = EvaluateStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        assert!(state.pending_signals.is_empty());
        let msg_count_before = state.messages.len();

        state.phase = AgentPhase::Evaluate;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        // No crash, no spurious messages
        assert!(state.pending_signals.is_empty());
        assert_eq!(state.messages.len(), msg_count_before);
    }
}
