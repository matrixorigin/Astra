//! ReflectStage: structured self-correction when the agent is stalled.
//!
//! Unlike simple "nudge" prompts, this stage performs rule-based failure
//! analysis and generates structured corrections:
//!
//! 1. **Diagnose** — categorize the failure (tool failures, stall, no progress)
//! 2. **Prescribe** — generate what_to_try advice and strategy adjustments
//! 3. **Apply** — modify TurnState (block tools, widen selection, inject context)
//! 4. **Store** — push Reflection for learning and cross-turn memory
//!
//! # Non-happy path handling
//!
//! This is the core mechanism for requirement #4 (non-happy path). Instead of
//! generic retry, the agent understands WHY it's stuck and adjusts strategy.

use crate::engine::{PipelineStage, StageAction};
use crate::event::{EventKind, EventLog};
use crate::state::{AgentPhase, Reflection, StrategyDelta, TurnOutcome, TurnState, TurnStatus};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Maximum times the Reflect stage will generate a reflection before giving up.
const MAX_REFLECTIONS: usize = 3;

// ─── Failure Categories ──────────────────────────────────────────────────────

/// Root cause category for structured diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    /// Specific tools keep failing (HTTP errors, permission denied, etc.)
    ToolFailures,
    /// Agent repeats the same tool calls without new information.
    Stall,
    /// Tools succeed but produce no useful output toward the goal.
    NoProgress,
    /// Catch-all: insufficient progress without a clear single cause.
    General,
}

// ─── Diagnosis ───────────────────────────────────────────────────────────────

/// Analyze TurnState to determine failure category and generate diagnosis.
///
/// Returns: (category, what_happened, what_to_try)
pub fn diagnose(state: &TurnState) -> (FailureCategory, String, String) {
    // Priority 1: Check for repeatedly failing tools
    let failing_tools: Vec<&String> = state
        .tool_failures
        .iter()
        .filter(|(_, errs)| errs.len() >= 2)
        .map(|(name, _)| name)
        .collect();

    if !failing_tools.is_empty() {
        let names: Vec<&str> = failing_tools.iter().map(|s| s.as_str()).collect();
        let names_str = names.join(", ");
        return (
            FailureCategory::ToolFailures,
            format!("Tools failing repeatedly: {}", names_str),
            format!(
                "Avoid {} and try alternative tools or a different approach entirely.",
                names_str
            ),
        );
    }

    // Priority 2: Stall detection via tool signature repetition
    if let Some(reason) = state.detect_stall() {
        return (
            FailureCategory::Stall,
            format!("Agent stalled: {}", reason),
            "Try a completely different approach. Consider different tools or decompose the problem into smaller steps.".into(),
        );
    }

    // Priority 3: Tools succeed but no useful text output
    if state.total_tool_calls > 0 && state.final_text.is_empty() {
        return (
            FailureCategory::NoProgress,
            "Tools called but no output produced".into(),
            "Summarize what you've learned so far and try a more direct approach.".into(),
        );
    }

    // Fallback: general insufficient progress
    (
        FailureCategory::General,
        "Insufficient progress toward goal".into(),
        "Step back and reconsider. What is the simplest path to answer the user's query?".into(),
    )
}

/// Compute strategy adjustments based on the failure category.
pub fn compute_strategy_delta(state: &TurnState, category: FailureCategory) -> StrategyDelta {
    let mut delta = StrategyDelta::default();

    match category {
        FailureCategory::ToolFailures => {
            // Block tools with 3+ failures (even if already blocked by circuit breaker,
            // record in delta for observability/learning)
            for (tool, errs) in &state.tool_failures {
                if errs.len() >= 3 {
                    delta.block_tools.push(tool.clone());
                }
            }
            delta.widen_selection = true;
        }
        FailureCategory::Stall => {
            delta.widen_selection = true;
            delta.inject_context = Some(
                "You've been repeating similar actions without progress. \
                 Try a fundamentally different approach or different tools."
                    .into(),
            );
        }
        FailureCategory::NoProgress => {
            delta.inject_context = Some(
                "Previous tool calls didn't produce useful output. \
                 Summarize findings and try a more direct path to the answer."
                    .into(),
            );
        }
        FailureCategory::General => {
            delta.inject_context = Some(
                "Progress is insufficient. Consider: Is this the right approach? \
                 What's the simplest path forward?"
                    .into(),
            );
        }
    }

    delta
}

/// Confidence in the reflection's correction decreases with each attempt.
fn reflection_confidence(reflection_count: usize) -> f64 {
    match reflection_count {
        0 => 0.7,
        1 => 0.4,
        2 => 0.2,
        _ => 0.1,
    }
}

/// Apply a strategy delta to the TurnState.
fn apply_strategy_delta(state: &mut TurnState, delta: &StrategyDelta) {
    for tool in &delta.block_tools {
        state.blocked_tools.insert(tool.clone());
    }
    // inject_context and widen_selection are read by the Plan stage
    // from the latest reflection's strategy_delta
}

// ─── ReflectStage ────────────────────────────────────────────────────────────

/// Structured self-correction stage.
///
/// Runs when the agent is stalled or regressing. Produces a [`Reflection`]
/// with diagnosis, prescription, and strategy adjustments.
pub struct ReflectStage;

#[async_trait::async_trait]
impl PipelineStage for ReflectStage {
    fn name(&self) -> &str {
        "reflect"
    }

    async fn execute(
        &self,
        state: &mut TurnState,
        event_log: &mut EventLog,
    ) -> Result<StageAction, String> {
        // 1. Check reflection limit — too many reflections means unrecoverable
        if state.reflections.len() >= MAX_REFLECTIONS {
            state.outcome = Some(TurnOutcome {
                status: TurnStatus::Failure,
                content: "Exhausted reflection budget — unable to self-correct".into(),
                failure_reason: Some(format!(
                    "Failed after {} reflections",
                    state.reflections.len()
                )),
                failed_tools: state.tool_failures.keys().cloned().collect(),
            });
            return Ok(StageAction::Transition(AgentPhase::Failed));
        }

        // 2. Diagnose what went wrong
        let (category, what_happened, what_to_try) = diagnose(state);

        // 3. Compute strategy adjustments
        let strategy_delta = compute_strategy_delta(state, category);

        // 4. Build reflection
        let confidence = reflection_confidence(state.reflections.len());
        let reflection = Reflection {
            what_happened: what_happened.clone(),
            why: format!("Category: {:?}", category),
            what_to_try: what_to_try.clone(),
            confidence,
            strategy_delta: strategy_delta.clone(),
        };

        // 5. Apply strategy delta to TurnState
        apply_strategy_delta(state, &strategy_delta);

        // 6. Inject reflection context into messages for next LLM call
        if let Some(ref ctx) = strategy_delta.inject_context {
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[Self-correction] {}\n\nSuggestion: {}",
                    what_happened, ctx
                )
            }));
        }

        // 7. Store reflection
        state.reflections.push(reflection);

        // 8. Emit event
        event_log.emit(
            EventKind::ReflectionGenerated {
                what_happened,
                what_to_try,
                confidence,
            },
            None,
        );

        // 9. Return to Plan for retry with adjusted strategy
        Ok(StageAction::Transition(AgentPhase::Plan))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventLog;
    use crate::state::TurnState;
    use std::collections::HashSet;

    fn make_state() -> TurnState {
        TurnState::new("test query", vec![], 10, 100_000, 300_000)
    }

    #[tokio::test]
    async fn reflect_on_tool_failures_blocks_tools() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Simulate 3 failures on the same tool (circuit breaker)
        state.record_tool_failure("github_api", "404 Not Found");
        state.record_tool_failure("github_api", "404 Not Found");
        state.record_tool_failure("github_api", "404 Not Found");
        state.phase = AgentPhase::Reflect;

        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));
        assert_eq!(state.reflections.len(), 1);

        let refl = &state.reflections[0];
        assert!(refl.what_happened.contains("github_api"));
        assert!(!refl.strategy_delta.block_tools.is_empty());
        assert!(refl.strategy_delta.widen_selection);
        assert!(state.blocked_tools.contains("github_api"));
    }

    #[tokio::test]
    async fn reflect_on_stall_widens_selection() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Simulate stall: same tools 3 rounds in a row
        let tools: HashSet<String> = ["bash".to_string()].into();
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools);
        state.phase = AgentPhase::Reflect;

        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));
        let refl = &state.reflections[0];
        assert!(refl.strategy_delta.widen_selection);
        assert!(refl.strategy_delta.inject_context.is_some());
    }

    #[tokio::test]
    async fn reflect_on_no_progress_injects_context() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Tools called but no text output
        state.total_tool_calls = 5;
        state.phase = AgentPhase::Reflect;

        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));
        let refl = &state.reflections[0];
        assert!(refl.strategy_delta.inject_context.is_some());
        assert!(
            refl.strategy_delta
                .inject_context
                .as_ref()
                .unwrap()
                .contains("Summarize")
        );
    }

    #[tokio::test]
    async fn reflect_general_case() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();
        state.phase = AgentPhase::Reflect;

        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Plan));
        let refl = &state.reflections[0];
        assert!(refl.what_happened.contains("Insufficient"));
    }

    #[tokio::test]
    async fn reflect_max_reflections_transitions_to_failed() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Pre-fill max reflections
        for _ in 0..MAX_REFLECTIONS {
            state.reflections.push(Reflection {
                what_happened: "test".into(),
                why: "test".into(),
                what_to_try: "test".into(),
                confidence: 0.3,
                strategy_delta: StrategyDelta::default(),
            });
        }
        state.phase = AgentPhase::Reflect;

        let action = stage.execute(&mut state, &mut log).await.unwrap();

        assert_eq!(action, StageAction::Transition(AgentPhase::Failed));
        assert!(state.outcome.is_some());
        assert_eq!(state.outcome.as_ref().unwrap().status, TurnStatus::Failure);
    }

    #[tokio::test]
    async fn reflect_confidence_decreases_with_attempts() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // First reflection
        state.phase = AgentPhase::Reflect;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();
        let conf_1 = state.reflections[0].confidence;

        // Second reflection
        state.phase = AgentPhase::Reflect;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();
        let conf_2 = state.reflections[1].confidence;

        // Third reflection
        state.phase = AgentPhase::Reflect;
        let _ = stage.execute(&mut state, &mut log).await.unwrap();
        let conf_3 = state.reflections[2].confidence;

        assert!(
            conf_1 > conf_2,
            "Confidence should decrease: {} > {}",
            conf_1,
            conf_2
        );
        assert!(
            conf_2 > conf_3,
            "Confidence should decrease: {} > {}",
            conf_2,
            conf_3
        );
    }

    #[tokio::test]
    async fn reflect_emits_event() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();
        state.phase = AgentPhase::Reflect;

        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        let reflection_events: Vec<_> = log
            .events()
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ReflectionGenerated { .. }))
            .collect();
        assert_eq!(reflection_events.len(), 1);
    }

    #[tokio::test]
    async fn reflect_injects_self_correction_message() {
        let stage = ReflectStage;
        let mut state = make_state();
        let mut log = EventLog::new();

        // Create a stall condition
        let tools: HashSet<String> = ["bash".to_string()].into();
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools);
        state.phase = AgentPhase::Reflect;

        let msg_count_before = state.messages.len();
        let _ = stage.execute(&mut state, &mut log).await.unwrap();

        // Self-correction message injected
        assert!(state.messages.len() > msg_count_before);
        let last_msg = state.messages.last().unwrap();
        assert_eq!(last_msg["role"], "user");
        assert!(
            last_msg["content"]
                .as_str()
                .unwrap()
                .contains("[Self-correction]")
        );
    }

    #[test]
    fn diagnose_tool_failures_first_priority() {
        let mut state = make_state();
        // Add both tool failures AND stall — tool failures should win
        state.record_tool_failure("api", "err");
        state.record_tool_failure("api", "err");
        let tools: HashSet<String> = ["bash".to_string()].into();
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools);

        let (cat, _, _) = diagnose(&state);
        assert_eq!(cat, FailureCategory::ToolFailures);
    }

    #[test]
    fn diagnose_stall_second_priority() {
        let mut state = make_state();
        // Stall but no tool failures
        let tools: HashSet<String> = ["bash".to_string()].into();
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools.clone());
        state.record_round_tools(tools);

        let (cat, _, _) = diagnose(&state);
        assert_eq!(cat, FailureCategory::Stall);
    }

    #[test]
    fn diagnose_no_progress_third_priority() {
        let mut state = make_state();
        state.total_tool_calls = 5;
        // no text, no stall, no tool failures

        let (cat, _, _) = diagnose(&state);
        assert_eq!(cat, FailureCategory::NoProgress);
    }

    #[test]
    fn diagnose_general_fallback() {
        let state = make_state();
        let (cat, _, _) = diagnose(&state);
        assert_eq!(cat, FailureCategory::General);
    }

    #[test]
    fn strategy_delta_blocks_failing_tools() {
        let mut state = make_state();
        state.record_tool_failure("bad_tool", "err");
        state.record_tool_failure("bad_tool", "err");
        state.record_tool_failure("bad_tool", "err");

        let delta = compute_strategy_delta(&state, FailureCategory::ToolFailures);
        assert!(delta.block_tools.contains(&"bad_tool".to_string()));
        assert!(delta.widen_selection);
    }

    #[test]
    fn reflection_confidence_values() {
        assert!((reflection_confidence(0) - 0.7).abs() < f64::EPSILON);
        assert!((reflection_confidence(1) - 0.4).abs() < f64::EPSILON);
        assert!((reflection_confidence(2) - 0.2).abs() < f64::EPSILON);
        assert!((reflection_confidence(3) - 0.1).abs() < f64::EPSILON);
        assert!((reflection_confidence(10) - 0.1).abs() < f64::EPSILON);
    }
}
