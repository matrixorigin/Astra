//! Integration tests for the cognitive pipeline: Evaluate + Reflect stages.
//!
//! Tests the full loop: Plan → Execute → Evaluate → Reflect → Plan (retry)
//! ensuring goal-gradient budget and structured self-correction work end-to-end.

use mo_agent_runtime::pipeline::{
    engine::{ExecutionEngine, PipelineStage, StageAction, StageRegistry},
    event::{EventKind, EventLog},
    stages::{evaluate::EvaluateStage, reflect::ReflectStage},
    state::{AgentPhase, TurnOutcome, TurnState, TurnStatus},
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};

// ─── Mock Stages ─────────────────────────────────────────────────────────────

/// Mock Perceive stage — just transitions to Plan.
struct MockPerceive;
#[async_trait::async_trait]
impl PipelineStage for MockPerceive {
    fn name(&self) -> &str {
        "perceive"
    }
    async fn execute(
        &self,
        _state: &mut TurnState,
        _event_log: &mut EventLog,
    ) -> Result<StageAction, String> {
        Ok(StageAction::Transition(AgentPhase::Plan))
    }
}

/// Mock Plan stage — sets up tool calls then transitions to Execute.
/// After reflection, checks widen_selection and adjusts behavior.
struct MockPlan {
    call_count: AtomicU32,
}
#[async_trait::async_trait]
impl PipelineStage for MockPlan {
    fn name(&self) -> &str {
        "plan"
    }
    async fn execute(
        &self,
        state: &mut TurnState,
        _event_log: &mut EventLog,
    ) -> Result<StageAction, String> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed);

        // After reflection, check if we should try different tools
        let widened = state
            .reflections
            .last()
            .map(|r| r.strategy_delta.widen_selection)
            .unwrap_or(false);

        if widened && count >= 2 {
            // After widening, produce a final answer
            state
                .final_text
                .push_str("Here is the answer after reflection.");
            state.outcome = Some(TurnOutcome {
                status: TurnStatus::Success,
                content: "done".into(),
                failure_reason: None,
                failed_tools: vec![],
            });
            return Ok(StageAction::Transition(AgentPhase::Complete));
        }

        // Normal: set up a tool call and go to execute
        state
            .pending_tool_calls
            .push(serde_json::json!({"tool": "bash"}));
        Ok(StageAction::Transition(AgentPhase::Execute))
    }
}

/// Mock Execute stage — simulates tool execution.
/// On first calls: produces no useful output (stall scenario).
/// After reflection: produces useful output.
struct MockExecute {
    call_count: AtomicU32,
}
#[async_trait::async_trait]
impl PipelineStage for MockExecute {
    fn name(&self) -> &str {
        "execute"
    }
    async fn execute(
        &self,
        state: &mut TurnState,
        _event_log: &mut EventLog,
    ) -> Result<StageAction, String> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed);
        state.total_tool_calls += 1;

        // Record same tools each round (will trigger stall after 3 rounds)
        let tools: HashSet<String> = ["bash".to_string()].into();
        state.record_round_tools(tools);

        if count < 3 {
            // First 3 rounds: no text output (stalling)
        } else {
            // After reflection: produce some text
            state.final_text.push_str("Useful output after reflection.");
        }

        state.pending_tool_calls.clear();
        Ok(StageAction::Transition(AgentPhase::Evaluate))
    }
}

// ─── Integration Tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn full_loop_stall_triggers_reflect_then_recovers() {
    let mut registry = StageRegistry::new();
    registry.register(AgentPhase::Perceive, Box::new(MockPerceive));
    registry.register(
        AgentPhase::Plan,
        Box::new(MockPlan {
            call_count: AtomicU32::new(0),
        }),
    );
    registry.register(
        AgentPhase::Execute,
        Box::new(MockExecute {
            call_count: AtomicU32::new(0),
        }),
    );
    registry.register(AgentPhase::Evaluate, Box::new(EvaluateStage));
    registry.register(AgentPhase::Reflect, Box::new(ReflectStage));

    let engine = ExecutionEngine::new(registry);
    let mut state = TurnState::new("test stall recovery", vec![], 20, 1_000_000, 300_000);
    let mut log = EventLog::new();

    engine.run(&mut state, &mut log).await.unwrap();

    // Agent should have recovered after reflection
    assert!(state.is_done());
    assert!(
        !state.reflections.is_empty(),
        "Should have reflected at least once, got {}",
        state.reflections.len()
    );

    // Verify reflection events were emitted
    let reflect_events: Vec<_> = log
        .events()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReflectionGenerated { .. }))
        .collect();
    assert!(
        !reflect_events.is_empty(),
        "Should have ReflectionGenerated events"
    );

    // Verify progress was tracked
    let progress_events: Vec<_> = log
        .events()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ProgressRecorded { .. }))
        .collect();
    assert!(
        progress_events.len() >= 2,
        "Should have multiple progress recordings"
    );
}

#[tokio::test]
async fn budget_exhaustion_terminates_cleanly() {
    // Very tight budget: 2 rounds only
    struct NoProgressExecute;
    #[async_trait::async_trait]
    impl PipelineStage for NoProgressExecute {
        fn name(&self) -> &str {
            "execute"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            state.total_tool_calls += 1;
            state.pending_tool_calls.clear();
            Ok(StageAction::Transition(AgentPhase::Evaluate))
        }
    }

    struct SimplePlan;
    #[async_trait::async_trait]
    impl PipelineStage for SimplePlan {
        fn name(&self) -> &str {
            "plan"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            state
                .pending_tool_calls
                .push(serde_json::json!({"tool": "x"}));
            Ok(StageAction::Transition(AgentPhase::Execute))
        }
    }

    let mut registry = StageRegistry::new();
    registry.register(AgentPhase::Perceive, Box::new(MockPerceive));
    registry.register(AgentPhase::Plan, Box::new(SimplePlan));
    registry.register(AgentPhase::Execute, Box::new(NoProgressExecute));
    registry.register(AgentPhase::Evaluate, Box::new(EvaluateStage));
    registry.register(AgentPhase::Reflect, Box::new(ReflectStage));

    let engine = ExecutionEngine::new(registry);
    // Only 2 rounds budget
    let mut state = TurnState::new("test budget", vec![], 2, 1_000_000, 300_000);
    let mut log = EventLog::new();

    engine.run(&mut state, &mut log).await.unwrap();

    assert!(state.is_done());
    assert_eq!(state.phase, AgentPhase::Failed);
    assert!(state.outcome.is_some());

    // Should be Exhausted or Failure
    let status = state.outcome.as_ref().unwrap().status;
    assert!(
        status == TurnStatus::Exhausted || status == TurnStatus::Failure,
        "Expected Exhausted or Failure, got {:?}",
        status
    );
}

#[tokio::test]
async fn max_reflections_then_fail() {
    /// A stage that always produces zero progress.
    struct ZeroProgressExecute;
    #[async_trait::async_trait]
    impl PipelineStage for ZeroProgressExecute {
        fn name(&self) -> &str {
            "execute"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            // Do nothing useful
            state.pending_tool_calls.clear();
            Ok(StageAction::Transition(AgentPhase::Evaluate))
        }
    }

    struct AlwaysPlan;
    #[async_trait::async_trait]
    impl PipelineStage for AlwaysPlan {
        fn name(&self) -> &str {
            "plan"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            state
                .pending_tool_calls
                .push(serde_json::json!({"tool": "x"}));
            Ok(StageAction::Transition(AgentPhase::Execute))
        }
    }

    let mut registry = StageRegistry::new();
    registry.register(AgentPhase::Perceive, Box::new(MockPerceive));
    registry.register(AgentPhase::Plan, Box::new(AlwaysPlan));
    registry.register(AgentPhase::Execute, Box::new(ZeroProgressExecute));
    registry.register(AgentPhase::Evaluate, Box::new(EvaluateStage));
    registry.register(AgentPhase::Reflect, Box::new(ReflectStage));

    let engine = ExecutionEngine::new(registry);
    let mut state = TurnState::new("test max reflect", vec![], 50, 1_000_000, 300_000);
    let mut log = EventLog::new();

    engine.run(&mut state, &mut log).await.unwrap();

    assert_eq!(state.phase, AgentPhase::Failed);

    // Should have reflected up to MAX_REFLECTIONS times then failed
    // (Evaluate stage enforces the limit)
    assert!(
        state.reflections.len() <= 3,
        "Should have at most 3 reflections, got {}",
        state.reflections.len()
    );

    // Verify causal event chain exists
    let completion_events: Vec<_> = log
        .events()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::TurnCompleted { .. }))
        .collect();
    assert_eq!(completion_events.len(), 1);
}

#[tokio::test]
async fn reflection_injects_context_into_messages() {
    // This test verifies that self-correction messages appear in state.messages
    struct StallExecute {
        call_count: AtomicU32,
    }
    #[async_trait::async_trait]
    impl PipelineStage for StallExecute {
        fn name(&self) -> &str {
            "execute"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            // Do nothing useful — always go to Evaluate
            state.pending_tool_calls.clear();
            Ok(StageAction::Transition(AgentPhase::Evaluate))
        }
    }

    struct StallPlan {
        call_count: AtomicU32,
    }
    #[async_trait::async_trait]
    impl PipelineStage for StallPlan {
        fn name(&self) -> &str {
            "plan"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            let count = self.call_count.fetch_add(1, Ordering::Relaxed);
            // After reflection, produce a final answer
            if count >= 4 && !state.reflections.is_empty() {
                state.final_text.push_str("Final answer.");
                state.outcome = Some(TurnOutcome {
                    status: TurnStatus::Success,
                    content: "done".into(),
                    failure_reason: None,
                    failed_tools: vec![],
                });
                return Ok(StageAction::Transition(AgentPhase::Complete));
            }
            state
                .pending_tool_calls
                .push(serde_json::json!({"tool": "x"}));
            Ok(StageAction::Transition(AgentPhase::Execute))
        }
    }

    let mut registry = StageRegistry::new();
    registry.register(AgentPhase::Perceive, Box::new(MockPerceive));
    registry.register(
        AgentPhase::Plan,
        Box::new(StallPlan {
            call_count: AtomicU32::new(0),
        }),
    );
    registry.register(
        AgentPhase::Execute,
        Box::new(StallExecute {
            call_count: AtomicU32::new(0),
        }),
    );
    registry.register(AgentPhase::Evaluate, Box::new(EvaluateStage));
    registry.register(AgentPhase::Reflect, Box::new(ReflectStage));

    let engine = ExecutionEngine::new(registry);
    let mut state = TurnState::new("test context injection", vec![], 20, 1_000_000, 300_000);
    let mut log = EventLog::new();

    engine.run(&mut state, &mut log).await.unwrap();

    // Check that self-correction messages were injected
    let correction_msgs: Vec<_> = state
        .messages
        .iter()
        .filter(|m| {
            m["content"]
                .as_str()
                .unwrap_or("")
                .contains("[Self-correction]")
        })
        .collect();

    assert!(
        !correction_msgs.is_empty(),
        "Should have self-correction messages in conversation"
    );
}

#[tokio::test]
async fn event_log_captures_full_lifecycle() {
    // Simple pipeline: Perceive → Plan → Complete (no loops)
    struct CompletePlan;
    #[async_trait::async_trait]
    impl PipelineStage for CompletePlan {
        fn name(&self) -> &str {
            "plan"
        }
        async fn execute(
            &self,
            state: &mut TurnState,
            _event_log: &mut EventLog,
        ) -> Result<StageAction, String> {
            state.final_text.push_str("Direct answer.");
            state.outcome = Some(TurnOutcome {
                status: TurnStatus::Success,
                content: "done".into(),
                failure_reason: None,
                failed_tools: vec![],
            });
            Ok(StageAction::Transition(AgentPhase::Complete))
        }
    }

    let mut registry = StageRegistry::new();
    registry.register(AgentPhase::Perceive, Box::new(MockPerceive));
    registry.register(AgentPhase::Plan, Box::new(CompletePlan));

    let engine = ExecutionEngine::new(registry);
    let mut state = TurnState::new("test events", vec![], 10, 100_000, 300_000);
    let mut log = EventLog::new();

    engine.run(&mut state, &mut log).await.unwrap();

    assert_eq!(state.phase, AgentPhase::Complete);

    // Should have: PhaseTransition(Perceive→Plan), PhaseTransition(Plan→Complete), TurnCompleted
    let event_kinds: Vec<String> = log
        .events()
        .iter()
        .map(|e| format!("{:?}", std::mem::discriminant(&e.kind)))
        .collect();

    assert!(
        log.events().len() >= 3,
        "Expected at least 3 events, got {}: {:?}",
        log.events().len(),
        event_kinds
    );

    // TurnCompleted with success
    let completed = log.events().iter().find(
        |e| matches!(&e.kind, EventKind::TurnCompleted { status, .. } if status == "success"),
    );
    assert!(
        completed.is_some(),
        "Should have a success TurnCompleted event"
    );
}
