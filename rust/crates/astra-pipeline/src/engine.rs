//! ExecutionEngine: the 18-line core that never changes.
//!
//! Drives the agent through phases using a state machine. Phases are
//! implemented as trait objects, making the engine extensible without
//! modification (Open-Closed Principle).

use super::event::{EventKind, EventLog};
use super::state::{AgentPhase, TurnState};

// ─── Stage trait ─────────────────────────────────────────────────────────────

/// What a stage wants the engine to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum StageAction {
    /// Transition to the specified phase.
    Transition(AgentPhase),
    /// Yield streaming text to the caller (and continue in same phase).
    YieldText(String),
}

/// A single pluggable stage in the cognitive pipeline.
///
/// Stages read/mutate [`TurnState`] and emit [`TurnEvent`]s. The engine
/// calls stages in the order defined by the state machine transitions.
#[async_trait::async_trait]
pub trait PipelineStage: Send + Sync {
    /// Human-readable name for logging/metrics.
    fn name(&self) -> &str;

    /// Execute this stage. Returns the desired next action.
    ///
    /// Stages may:
    /// - Read/write `state` fields
    /// - Emit events via `event_log`
    /// - Return `Transition(next_phase)` to move forward
    /// - Return `YieldText(text)` for streaming output
    async fn execute(
        &self,
        state: &mut TurnState,
        event_log: &mut EventLog,
    ) -> Result<StageAction, String>;
}

// ─── Stage registry ──────────────────────────────────────────────────────────

/// Maps each non-terminal phase to its stage implementation.
pub struct StageRegistry {
    stages: std::collections::HashMap<AgentPhase, Box<dyn PipelineStage>>,
}

impl StageRegistry {
    pub fn new() -> Self {
        Self {
            stages: std::collections::HashMap::new(),
        }
    }

    /// Register a stage for a phase. Overwrites any existing stage.
    pub fn register(&mut self, phase: AgentPhase, stage: Box<dyn PipelineStage>) {
        self.stages.insert(phase, stage);
    }

    /// Get the stage for a phase, if registered.
    pub fn get(&self, phase: &AgentPhase) -> Option<&dyn PipelineStage> {
        self.stages.get(phase).map(|s| s.as_ref())
    }
}

impl Default for StageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Execution engine ────────────────────────────────────────────────────────

/// The cognitive agent execution engine.
///
/// Drives TurnState through phases using registered stages. The engine itself
/// is ~20 lines — all intelligence lives in the stages.
///
/// ```text
/// while !state.is_done() {
///     let stage = registry.get(state.phase);
///     let action = stage.execute(&mut state, &mut event_log).await;
///     match action {
///         Transition(next) => state.transition(next),
///         YieldText(text) => yield text to caller,
///     }
/// }
/// ```
pub struct ExecutionEngine {
    registry: StageRegistry,
}

impl ExecutionEngine {
    pub fn new(registry: StageRegistry) -> Self {
        Self { registry }
    }

    /// Run the engine to completion.
    ///
    /// Returns the final TurnState and the event log. Streaming text is
    /// collected in `state.final_text`; a real implementation would yield
    /// chunks via a channel.
    pub async fn run(&self, state: &mut TurnState, event_log: &mut EventLog) -> Result<(), String> {
        // Safety: prevent infinite loops
        let max_iterations = 200;
        let mut iterations = 0;

        while !state.is_done() {
            iterations += 1;
            if iterations > max_iterations {
                state.phase = AgentPhase::Failed;
                event_log.emit(
                    EventKind::TurnCompleted {
                        status: "failed".into(),
                        total_rounds: state.budget.round,
                        total_tokens: state.budget.tokens_consumed,
                    },
                    None,
                );
                return Err("Engine exceeded maximum iterations".into());
            }

            let stage = self
                .registry
                .get(&state.phase)
                .ok_or_else(|| format!("No stage registered for phase {:?}", state.phase))?;

            let prev_phase = state.phase;
            let action = stage.execute(state, event_log).await?;

            match action {
                StageAction::Transition(next) => {
                    event_log.emit(
                        EventKind::PhaseTransition {
                            from: prev_phase,
                            to: next,
                        },
                        None,
                    );
                    state.transition(next);
                }
                StageAction::YieldText(text) => {
                    state.final_text.push_str(&text);
                }
            }
        }

        // Emit completion event
        let status = match state.outcome.as_ref().map(|o| o.status) {
            Some(super::state::TurnStatus::Success) => "success",
            Some(super::state::TurnStatus::Failure) => "failure",
            Some(super::state::TurnStatus::Exhausted) => "exhausted",
            None => "unknown",
        };
        event_log.emit(
            EventKind::TurnCompleted {
                status: status.into(),
                total_rounds: state.budget.round,
                total_tokens: state.budget.tokens_consumed,
            },
            None,
        );

        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock stage that immediately transitions to the next phase.
    struct MockTransitionStage {
        name: String,
        target: AgentPhase,
    }

    #[async_trait::async_trait]
    impl PipelineStage for MockTransitionStage {
        fn name(&self) -> &str {
            &self.name
        }
        async fn execute(
            &self,
            _state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            Ok(StageAction::Transition(self.target))
        }
    }

    /// A stage that yields text then transitions.
    struct MockYieldStage {
        text: String,
        target: AgentPhase,
        call_count: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl PipelineStage for MockYieldStage {
        fn name(&self) -> &str {
            "yield"
        }
        async fn execute(
            &self,
            _state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count == 0 {
                Ok(StageAction::YieldText(self.text.clone()))
            } else {
                Ok(StageAction::Transition(self.target))
            }
        }
    }

    /// A mock plan stage that sets up tool calls and transitions to Execute.
    struct MockPlanStage;

    #[async_trait::async_trait]
    impl PipelineStage for MockPlanStage {
        fn name(&self) -> &str {
            "plan"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            if state.pending_tool_calls.is_empty() {
                // No tools → complete
                state.outcome = Some(crate::state::TurnOutcome {
                    status: crate::state::TurnStatus::Success,
                    content: "done".into(),
                    failure_reason: None,
                    failed_tools: vec![],
                });
                Ok(StageAction::Transition(AgentPhase::Complete))
            } else {
                Ok(StageAction::Transition(AgentPhase::Execute))
            }
        }
    }

    #[tokio::test]
    async fn engine_runs_simple_pipeline() {
        let mut registry = StageRegistry::new();
        registry.register(
            AgentPhase::Perceive,
            Box::new(MockTransitionStage {
                name: "perceive".into(),
                target: AgentPhase::Plan,
            }),
        );
        registry.register(AgentPhase::Plan, Box::new(MockPlanStage));

        let engine = ExecutionEngine::new(registry);
        let mut state = TurnState::new("hello", vec![], 10, 100_000, 30_000);
        let mut log = EventLog::new();

        engine.run(&mut state, &mut log).await.unwrap();

        assert!(state.is_done());
        assert_eq!(state.phase, AgentPhase::Complete);
        assert!(!log.is_empty());
    }

    #[tokio::test]
    async fn engine_collects_yielded_text() {
        let mut registry = StageRegistry::new();
        registry.register(
            AgentPhase::Perceive,
            Box::new(MockYieldStage {
                text: "Hello world".into(),
                target: AgentPhase::Plan,
                call_count: std::sync::atomic::AtomicU32::new(0),
            }),
        );
        registry.register(AgentPhase::Plan, Box::new(MockPlanStage));

        let engine = ExecutionEngine::new(registry);
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        let mut log = EventLog::new();

        engine.run(&mut state, &mut log).await.unwrap();

        assert!(state.final_text.contains("Hello world"));
    }

    #[tokio::test]
    async fn engine_errors_on_missing_stage() {
        let registry = StageRegistry::new(); // empty — no stages registered
        let engine = ExecutionEngine::new(registry);
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        let mut log = EventLog::new();

        let result = engine.run(&mut state, &mut log).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No stage registered"));
    }

    #[tokio::test]
    async fn engine_prevents_infinite_loop() {
        // A stage that always transitions back to itself (via Evaluate → Plan)
        struct LoopStage;
        #[async_trait::async_trait]
        impl PipelineStage for LoopStage {
            fn name(&self) -> &str {
                "loop"
            }
            async fn execute(
                &self,
                state: &mut TurnState,
                _event_log: &mut EventLog,
            ) -> Result<StageAction, String> {
                state.budget.advance_round();
                Ok(StageAction::Transition(AgentPhase::Plan))
            }
        }

        let mut registry = StageRegistry::new();
        registry.register(
            AgentPhase::Perceive,
            Box::new(MockTransitionStage {
                name: "perceive".into(),
                target: AgentPhase::Plan,
            }),
        );
        registry.register(
            AgentPhase::Plan,
            Box::new(MockTransitionStage {
                name: "plan".into(),
                target: AgentPhase::Execute,
            }),
        );
        registry.register(
            AgentPhase::Execute,
            Box::new(MockTransitionStage {
                name: "exec".into(),
                target: AgentPhase::Evaluate,
            }),
        );
        registry.register(AgentPhase::Evaluate, Box::new(LoopStage));

        let engine = ExecutionEngine::new(registry);
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        let mut log = EventLog::new();

        let result = engine.run(&mut state, &mut log).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("maximum iterations"));
        assert_eq!(state.phase, AgentPhase::Failed);
    }

    #[tokio::test]
    async fn engine_emits_phase_transition_events() {
        let mut registry = StageRegistry::new();
        registry.register(
            AgentPhase::Perceive,
            Box::new(MockTransitionStage {
                name: "perceive".into(),
                target: AgentPhase::Plan,
            }),
        );
        registry.register(AgentPhase::Plan, Box::new(MockPlanStage));

        let engine = ExecutionEngine::new(registry);
        let mut state = TurnState::new("test", vec![], 10, 100_000, 30_000);
        let mut log = EventLog::new();

        engine.run(&mut state, &mut log).await.unwrap();

        // Should have: Perceive→Plan, Plan→Complete, and TurnCompleted
        let transitions: Vec<_> = log
            .events()
            .iter()
            .filter(|e| matches!(e.kind, EventKind::PhaseTransition { .. }))
            .collect();
        assert_eq!(transitions.len(), 2);
    }

    #[test]
    fn stage_registry_get_returns_none_for_missing() {
        let registry = StageRegistry::new();
        assert!(registry.get(&AgentPhase::Perceive).is_none());
    }
}
