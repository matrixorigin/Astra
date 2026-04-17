//! Scheduling Contract: defines who creates, advances, checkpoints, retries, and observes Steps.
//!
//! # Roles
//!
//! ```text
//! ┌─────────────┐       ┌─────────────┐       ┌──────────────────┐
//! │  Scheduler   │──────▶│  StepRunner  │──────▶│ CheckpointWriter │
//! │  (creates &  │       │  (executes & │       │ (persists state) │
//! │   assigns)   │       │   advances)  │       └──────────────────┘
//! └──────┬──────┘       └──────┬──────┘                │
//!        │                     │                       │
//!        │              ┌──────▼──────┐                │
//!        └─────────────▶│  Observer   │◀───────────────┘
//!                       │  (events)   │
//!                       └─────────────┘
//! ```
//!
//! # Ownership rules
//!
//! - **StepDescriptor** is owned by Scheduler (immutable after creation).
//! - **StepExecution** is owned by StepRunner (mutable during execution).
//! - **StepCheckpoint** is owned by CheckpointWriter (persistence concern).
//! - **StepEvents** flow to Observer (read-only, for audit/UI/debug).
//! - **Retry decisions**: tool-level by StepRunner, step-level by Scheduler.
//!
//! # Current state
//!
//! chat_stream.rs implicitly plays all 4 roles. These traits formalize
//! the boundaries so they can be split across components (or distributed).

use super::step_protocol::*;

// ─── Scheduler: creates and assigns steps ────────────────────────────────────

/// Creates Steps and decides assignment (which agent runs them).
/// In the current model: the outer loop in chat_stream.rs.
/// In the distributed model: a central orchestrator with task queue.
pub trait StepScheduler {
    type Error: std::fmt::Debug;

    /// Create a new step for a task. Returns the step with Pending status.
    /// The scheduler owns the StepDescriptor and sets retry policy.
    fn create_step(
        &mut self,
        task_id: &str,
        action: StepAction,
        payload: StepPayload,
    ) -> Result<Step, Self::Error>;

    /// Assign a step to an agent. Transitions Pending → Assigned.
    /// In single-agent mode, this is a no-op (self-assign).
    fn assign_step(&mut self, step_id: &str, agent_id: &str) -> Result<(), Self::Error>;

    /// Decide whether a failed step should be retried at the step level.
    /// Returns the retry delay in ms, or None to give up.
    fn should_retry_step(&self, step: &Step, error: &ErrorCategory) -> Option<u64>;

    /// Get the next step to execute (for pull-based scheduling).
    /// Returns None if no steps are ready.
    fn next_step(&mut self) -> Option<Step>;
}

// ─── StepRunner: executes a single step ──────────────────────────────────────

/// Executes a step: advances the cursor through slots, manages tool dispatch.
/// In the current model: the inner tool-execution loop in chat_stream.rs.
/// The runner owns StepExecution (mutable) and reads StepDescriptor (immutable).
pub trait StepRunner {
    type Error: std::fmt::Debug;

    /// Begin execution of a step. Transitions Assigned → Running.
    /// Sets started_at, initializes cursor for the step's action.
    fn begin(&mut self, step: &mut Step) -> Result<(), Self::Error>;

    /// Advance one slot: dispatch tool, collect result, update slot state.
    /// Returns the slot index that was advanced, or None if all done.
    fn advance_slot(&mut self, step: &mut Step) -> Result<Option<u32>, Self::Error>;

    /// Complete the step with a result. Transitions Running → Completed.
    fn complete(&mut self, step: &mut Step, result: StepResult) -> Result<(), Self::Error>;

    /// Fail the step with an error. Transitions Running → Failed.
    fn fail(&mut self, step: &mut Step, error: &str) -> Result<(), Self::Error>;

    /// Handle tool-level retry (within a single step, per-slot).
    /// Returns true if retried, false if exhausted.
    fn retry_slot(&mut self, step: &mut Step, slot_index: u32) -> Result<bool, Self::Error>;
}

// ─── CheckpointWriter: persists state ────────────────────────────────────────

/// Writes checkpoints to durable storage.
/// In the current model: local file + (future) MatrixOne.
/// The writer decides tier (Light/Heavy) based on CheckpointTrigger.
pub trait CheckpointWriter {
    type Error: std::fmt::Debug;

    /// Write a checkpoint. Tier is determined by the trigger.
    fn write_checkpoint(
        &mut self,
        step: &Step,
        trigger: CheckpointTrigger,
        messages: Option<&[serde_json::Value]>,
    ) -> Result<(), Self::Error>;

    /// Read the latest checkpoint for a step (for recovery).
    fn read_checkpoint(&self, step_id: &str) -> Result<Option<StepCheckpoint>, Self::Error>;

    /// Delete checkpoints for a completed step (cleanup).
    fn delete_checkpoints(&mut self, step_id: &str) -> Result<(), Self::Error>;
}

// ─── Observer: receives events ───────────────────────────────────────────────

/// Receives step events for audit, debug, and UI.
/// Observers are read-only — they cannot affect execution.
/// Multiple observers can be composed (journal, UI, metrics).
pub trait StepObserver {
    /// Called when a step event occurs. Must not block execution.
    fn on_event(&mut self, event: &StepEvent);

    /// Called when a checkpoint is written (for sync/audit).
    fn on_checkpoint(&mut self, step_id: &str, trigger: CheckpointTrigger, tier: CheckpointTier);
}

// ─── Lifecycle: composes roles into execution sequence ───────────────────────

/// The step execution lifecycle, composing all four roles.
/// This is the contract that binds Scheduler ↔ Runner ↔ Writer ↔ Observer.
///
/// ```text
/// Scheduler.create_step()
///   → Scheduler.assign_step()
///     → Runner.begin()
///       → [for each slot] Runner.advance_slot()
///         → Writer.write_checkpoint(SlotCompleted)
///         → Observer.on_event(ToolCallCompleted)
///       → [if slot failed] Runner.retry_slot()
///       → [phase transition] Writer.write_checkpoint(PhaseTransition)
///     → Runner.complete() / Runner.fail()
///       → Writer.write_checkpoint(Explicit)
///       → Observer.on_event(StepCompleted/StepFailed)
///   → [if failed] Scheduler.should_retry_step()
///     → Scheduler.create_step() (new attempt)
/// ```
pub struct StepLifecycle<S, R, W, O> {
    pub scheduler: S,
    pub runner: R,
    pub writer: W,
    pub observers: Vec<O>,
}

impl<S, R, W, O> StepLifecycle<S, R, W, O>
where
    S: StepScheduler,
    R: StepRunner,
    W: CheckpointWriter,
    O: StepObserver,
{
    pub fn new(scheduler: S, runner: R, writer: W) -> Self {
        Self {
            scheduler,
            runner,
            writer,
            observers: Vec::new(),
        }
    }

    pub fn add_observer(&mut self, observer: O) {
        self.observers.push(observer);
    }

    /// Execute one step through its full lifecycle.
    /// Returns Ok(StepResult) on completion, or the Step on failure (for retry).
    pub fn execute_step(&mut self, mut step: Step) -> Result<Step, LifecycleError> {
        // 1. Begin execution
        self.runner
            .begin(&mut step)
            .map_err(|e| LifecycleError::RunnerError(format!("{:?}", e)))?;
        self.emit_event(&step, StepEventType::StepStarted);

        // 2. Advance slots
        loop {
            let advanced = self
                .runner
                .advance_slot(&mut step)
                .map_err(|e| LifecycleError::RunnerError(format!("{:?}", e)))?;

            match advanced {
                Some(slot_idx) => {
                    let slot = &step.execution.cursor.slots[slot_idx as usize];
                    let event_type = match slot.state {
                        SlotState::Completed => StepEventType::ToolCallCompleted,
                        SlotState::Failed => StepEventType::ToolCallFailed,
                        SlotState::Skipped => StepEventType::ToolCallSkipped,
                        _ => StepEventType::ToolCallStarted,
                    };
                    self.emit_event(&step, event_type);

                    // Light checkpoint after each slot
                    let _ =
                        self.writer
                            .write_checkpoint(&step, CheckpointTrigger::SlotCompleted, None);
                    self.emit_checkpoint(step.step_id(), CheckpointTrigger::SlotCompleted);

                    // Tool-level retry on failure
                    if slot.state == SlotState::Failed {
                        let retried = self
                            .runner
                            .retry_slot(&mut step, slot_idx)
                            .map_err(|e| LifecycleError::RunnerError(format!("{:?}", e)))?;
                        if retried {
                            self.emit_event(&step, StepEventType::RetryScheduled);
                            continue;
                        }
                    }
                }
                None => break, // all slots done
            }
        }

        // 3. Heavy checkpoint at end
        let _ = self
            .writer
            .write_checkpoint(&step, CheckpointTrigger::Explicit, None);
        self.emit_checkpoint(step.step_id(), CheckpointTrigger::Explicit);

        // 4. Mark completion
        if step.execution.cursor.all_slots_done()
            && step
                .execution
                .cursor
                .slots
                .iter()
                .all(|s| s.state != SlotState::Failed)
        {
            self.emit_event(&step, StepEventType::StepCompleted);
        } else {
            self.emit_event(&step, StepEventType::StepFailed);
        }

        Ok(step)
    }

    fn emit_event(&mut self, step: &Step, event_type: StepEventType) {
        let event = StepEvent {
            event_id: format!("{}-{:?}-{}", step.step_id(), event_type, epoch_ms()),
            step_id: step.step_id().to_string(),
            event_type,
            agent_id: step.descriptor.agent_id.clone(),
            caused_by: vec![],
            payload: None,
            created_at: epoch_ms(),
        };
        for obs in &mut self.observers {
            obs.on_event(&event);
        }
    }

    fn emit_checkpoint(&mut self, step_id: &str, trigger: CheckpointTrigger) {
        let tier = trigger.checkpoint_tier();
        for obs in &mut self.observers {
            obs.on_checkpoint(step_id, trigger, tier);
        }
    }
}

/// Errors from the step lifecycle.
#[derive(Debug, Clone)]
pub enum LifecycleError {
    SchedulerError(String),
    RunnerError(String),
    WriterError(String),
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchedulerError(msg) => write!(f, "Scheduler error: {msg}"),
            Self::RunnerError(msg) => write!(f, "Runner error: {msg}"),
            Self::WriterError(msg) => write!(f, "Writer error: {msg}"),
        }
    }
}

impl std::error::Error for LifecycleError {}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mock implementations ──

    struct MockScheduler {
        next_step_id: u32,
    }

    impl MockScheduler {
        fn new() -> Self {
            Self { next_step_id: 0 }
        }
    }

    impl StepScheduler for MockScheduler {
        type Error = String;

        fn create_step(
            &mut self,
            task_id: &str,
            action: StepAction,
            payload: StepPayload,
        ) -> Result<Step, String> {
            self.next_step_id += 1;
            Ok(Step::new(
                format!("step-{}", self.next_step_id),
                task_id.to_string(),
                format!("node-{}", self.next_step_id),
                action,
                payload,
            ))
        }

        fn assign_step(&mut self, _step_id: &str, _agent_id: &str) -> Result<(), String> {
            Ok(())
        }

        fn should_retry_step(&self, step: &Step, error: &ErrorCategory) -> Option<u64> {
            if step.is_retriable() && matches!(error, ErrorCategory::Transient) {
                Some(500)
            } else {
                None
            }
        }

        fn next_step(&mut self) -> Option<Step> {
            None
        }
    }

    struct MockRunner {
        fail_slot: Option<u32>,
        retry_succeeds: bool,
    }

    impl MockRunner {
        fn new() -> Self {
            Self {
                fail_slot: None,
                retry_succeeds: false,
            }
        }

        fn with_failing_slot(slot: u32, retry_succeeds: bool) -> Self {
            Self {
                fail_slot: Some(slot),
                retry_succeeds,
            }
        }
    }

    impl StepRunner for MockRunner {
        type Error = String;

        fn begin(&mut self, step: &mut Step) -> Result<(), String> {
            step.mark_started("mock-agent");
            Ok(())
        }

        fn advance_slot(&mut self, step: &mut Step) -> Result<Option<u32>, String> {
            if let Some(idx) = step.execution.cursor.next_pending_slot() {
                let state = if self.fail_slot == Some(idx as u32) {
                    SlotState::Failed
                } else {
                    SlotState::Completed
                };
                step.execution.cursor.advance_slot(idx, state);
                Ok(Some(idx as u32))
            } else {
                Ok(None)
            }
        }

        fn complete(&mut self, step: &mut Step, result: StepResult) -> Result<(), String> {
            step.mark_completed(result);
            Ok(())
        }

        fn fail(&mut self, step: &mut Step, error: &str) -> Result<(), String> {
            step.mark_failed(error);
            Ok(())
        }

        fn retry_slot(&mut self, step: &mut Step, slot_index: u32) -> Result<bool, String> {
            if self.retry_succeeds {
                step.execution.cursor.slots[slot_index as usize].state = SlotState::Pending;
                step.execution.cursor.slots[slot_index as usize].retry_count += 1;
                self.fail_slot = None; // next attempt succeeds
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    struct MockWriter {
        checkpoints_written: Vec<(String, CheckpointTrigger)>,
    }

    impl MockWriter {
        fn new() -> Self {
            Self {
                checkpoints_written: Vec::new(),
            }
        }
    }

    impl CheckpointWriter for MockWriter {
        type Error = String;

        fn write_checkpoint(
            &mut self,
            step: &Step,
            trigger: CheckpointTrigger,
            _messages: Option<&[serde_json::Value]>,
        ) -> Result<(), String> {
            self.checkpoints_written
                .push((step.step_id().to_string(), trigger));
            Ok(())
        }

        fn read_checkpoint(&self, _step_id: &str) -> Result<Option<StepCheckpoint>, String> {
            Ok(None)
        }

        fn delete_checkpoints(&mut self, _step_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockObserver {
        events: Vec<StepEventType>,
        checkpoints: Vec<(CheckpointTrigger, CheckpointTier)>,
    }

    impl StepObserver for MockObserver {
        fn on_event(&mut self, event: &StepEvent) {
            self.events.push(event.event_type.clone());
        }

        fn on_checkpoint(
            &mut self,
            _step_id: &str,
            trigger: CheckpointTrigger,
            tier: CheckpointTier,
        ) {
            self.checkpoints.push((trigger, tier));
        }
    }

    // ── Contract Tests ──

    #[test]
    fn scheduler_creates_step_with_pending_status() {
        let mut sched = MockScheduler::new();
        let step = sched
            .create_step(
                "task-1",
                StepAction::Act,
                StepPayload::Act {
                    selected_tools: vec!["grep".into()],
                    tool_calls: vec![],
                },
            )
            .unwrap();
        assert_eq!(step.status(), StepStatus::Pending);
        assert_eq!(step.task_id(), "task-1");
        assert_eq!(step.action(), StepAction::Act);
    }

    #[test]
    fn runner_begin_transitions_to_running() {
        let mut sched = MockScheduler::new();
        let mut step = sched
            .create_step(
                "task-1",
                StepAction::Perceive,
                StepPayload::Perceive {
                    user_query: "test".into(),
                    memory_context: vec![],
                },
            )
            .unwrap();

        let mut runner = MockRunner::new();
        runner.begin(&mut step).unwrap();
        assert_eq!(step.status(), StepStatus::Running);
        assert!(step.execution.started_at.is_some());
    }

    #[test]
    fn runner_advance_slots_completes_all() {
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into(), "read_file".into()],
                tool_calls: vec![],
            },
        );
        step.execution.cursor = ExecutionCursor::for_act(2);
        step.mark_started("agent-1");

        let mut runner = MockRunner::new();
        let slot0 = runner.advance_slot(&mut step).unwrap();
        assert_eq!(slot0, Some(0));
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);

        let slot1 = runner.advance_slot(&mut step).unwrap();
        assert_eq!(slot1, Some(1));

        let none = runner.advance_slot(&mut step).unwrap();
        assert_eq!(none, None);
        assert!(step.execution.cursor.all_slots_done());
    }

    #[test]
    fn runner_retry_slot_on_failure() {
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["bash".into()],
                tool_calls: vec![],
            },
        );
        step.execution.cursor = ExecutionCursor::for_act(1);
        step.mark_started("agent-1");

        let mut runner = MockRunner::with_failing_slot(0, true);
        let slot0 = runner.advance_slot(&mut step).unwrap();
        assert_eq!(slot0, Some(0));
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Failed);

        // Retry succeeds → slot reset to Pending
        let retried = runner.retry_slot(&mut step, 0).unwrap();
        assert!(retried);
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Pending);
        assert_eq!(step.execution.cursor.slots[0].retry_count, 1);

        // Next advance succeeds (fail_slot cleared)
        let slot0_retry = runner.advance_slot(&mut step).unwrap();
        assert_eq!(slot0_retry, Some(0));
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);
    }

    #[test]
    fn scheduler_retry_decision_transient_only() {
        let sched = MockScheduler::new();
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec![],
                tool_calls: vec![],
            },
        );
        step.mark_started("a1");
        step.mark_failed("timeout");

        // Transient → retry
        assert!(
            sched
                .should_retry_step(&step, &ErrorCategory::Transient)
                .is_some()
        );
        // InvalidInput → no retry
        assert!(
            sched
                .should_retry_step(&step, &ErrorCategory::InvalidInput)
                .is_none()
        );
    }

    #[test]
    fn writer_records_checkpoints_per_trigger() {
        let mut writer = MockWriter::new();
        let step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec![],
                tool_calls: vec![],
            },
        );

        writer
            .write_checkpoint(&step, CheckpointTrigger::SlotCompleted, None)
            .unwrap();
        writer
            .write_checkpoint(&step, CheckpointTrigger::PhaseTransition, None)
            .unwrap();
        writer
            .write_checkpoint(&step, CheckpointTrigger::Explicit, None)
            .unwrap();

        assert_eq!(writer.checkpoints_written.len(), 3);
        assert_eq!(
            writer.checkpoints_written[0].1,
            CheckpointTrigger::SlotCompleted
        );
        assert_eq!(
            writer.checkpoints_written[1].1,
            CheckpointTrigger::PhaseTransition
        );
        assert_eq!(writer.checkpoints_written[2].1, CheckpointTrigger::Explicit);
    }

    #[test]
    fn observer_receives_events_and_checkpoints() {
        let mut obs = MockObserver::default();
        let event = StepEvent {
            event_id: "e1".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepStarted,
            agent_id: Some("a1".into()),
            caused_by: vec![],
            payload: None,
            created_at: 0,
        };
        obs.on_event(&event);
        obs.on_checkpoint(
            "s1",
            CheckpointTrigger::SlotCompleted,
            CheckpointTier::Light,
        );
        obs.on_checkpoint(
            "s1",
            CheckpointTrigger::PhaseTransition,
            CheckpointTier::Heavy,
        );

        assert_eq!(obs.events.len(), 1);
        assert_eq!(obs.events[0], StepEventType::StepStarted);
        assert_eq!(obs.checkpoints.len(), 2);
        assert_eq!(obs.checkpoints[0].1, CheckpointTier::Light);
        assert_eq!(obs.checkpoints[1].1, CheckpointTier::Heavy);
    }

    #[test]
    fn lifecycle_happy_path_all_slots_complete() {
        let sched = MockScheduler::new();
        let runner = MockRunner::new();
        let writer = MockWriter::new();
        let obs = MockObserver::default();

        let mut lifecycle = StepLifecycle::new(sched, runner, writer);
        lifecycle.add_observer(obs);

        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into(), "read_file".into()],
                tool_calls: vec![],
            },
        );
        step.execution.cursor = ExecutionCursor::for_act(2);

        let result = lifecycle.execute_step(step).unwrap();
        assert_eq!(result.status(), StepStatus::Running); // execute_step doesn't call complete()
        assert!(result.execution.cursor.all_slots_done());

        // Verify checkpoints: 2 SlotCompleted + 1 Explicit = 3
        assert_eq!(lifecycle.writer.checkpoints_written.len(), 3);

        // Verify observer events: Started + 2 Completed + StepCompleted = 4
        assert_eq!(lifecycle.observers[0].events.len(), 4);
        assert_eq!(lifecycle.observers[0].events[0], StepEventType::StepStarted);
        assert_eq!(
            lifecycle.observers[0].events[3],
            StepEventType::StepCompleted
        );
    }

    #[test]
    fn lifecycle_slot_failure_no_retry() {
        let sched = MockScheduler::new();
        let runner = MockRunner::with_failing_slot(1, false); // slot 1 fails, no retry
        let writer = MockWriter::new();
        let obs = MockObserver::default();

        let mut lifecycle = StepLifecycle::new(sched, runner, writer);
        lifecycle.add_observer(obs);

        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into(), "bash".into()],
                tool_calls: vec![],
            },
        );
        step.execution.cursor = ExecutionCursor::for_act(2);

        let result = lifecycle.execute_step(step).unwrap();
        // Slot 0 completed, slot 1 failed
        assert_eq!(result.execution.cursor.slots[0].state, SlotState::Completed);
        assert_eq!(result.execution.cursor.slots[1].state, SlotState::Failed);

        // Observer sees: Started + ToolCompleted + ToolFailed + StepFailed = 4
        assert_eq!(lifecycle.observers[0].events.len(), 4);
        assert_eq!(lifecycle.observers[0].events[3], StepEventType::StepFailed);
    }

    #[test]
    fn lifecycle_slot_failure_with_retry_succeeds() {
        let sched = MockScheduler::new();
        let runner = MockRunner::with_failing_slot(0, true); // slot 0 fails but retry succeeds
        let writer = MockWriter::new();
        let obs = MockObserver::default();

        let mut lifecycle = StepLifecycle::new(sched, runner, writer);
        lifecycle.add_observer(obs);

        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["bash".into()],
                tool_calls: vec![],
            },
        );
        step.execution.cursor = ExecutionCursor::for_act(1);

        let result = lifecycle.execute_step(step).unwrap();
        // Slot completed after retry
        assert_eq!(result.execution.cursor.slots[0].state, SlotState::Completed);
        assert_eq!(result.execution.cursor.slots[0].retry_count, 1);

        // Observer: Started + ToolFailed + RetryScheduled + ToolCompleted + StepCompleted = 5
        assert_eq!(lifecycle.observers[0].events.len(), 5);
        assert!(
            lifecycle.observers[0]
                .events
                .contains(&StepEventType::RetryScheduled)
        );
        assert_eq!(
            *lifecycle.observers[0].events.last().unwrap(),
            StepEventType::StepCompleted
        );
    }

    #[test]
    fn lifecycle_checkpoint_tiers_correct() {
        let sched = MockScheduler::new();
        let runner = MockRunner::new();
        let writer = MockWriter::new();
        let obs = MockObserver::default();

        let mut lifecycle = StepLifecycle::new(sched, runner, writer);
        lifecycle.add_observer(obs);

        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into()],
                tool_calls: vec![],
            },
        );
        step.execution.cursor = ExecutionCursor::for_act(1);

        lifecycle.execute_step(step).unwrap();

        // Observer checkpoints: SlotCompleted(Light) + Explicit(Heavy)
        assert_eq!(lifecycle.observers[0].checkpoints.len(), 2);
        assert_eq!(
            lifecycle.observers[0].checkpoints[0].1,
            CheckpointTier::Light
        );
        assert_eq!(
            lifecycle.observers[0].checkpoints[1].1,
            CheckpointTier::Heavy
        );
    }

    // ── Contract Property Tests ──

    #[test]
    fn contract_step_never_skips_running_state() {
        // A step must go through Running before completing
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: "hello".into(),
                memory_context: vec![],
            },
        );
        assert_eq!(step.status(), StepStatus::Pending);

        let mut runner = MockRunner::new();
        runner.begin(&mut step).unwrap();
        assert_eq!(step.status(), StepStatus::Running);
    }

    #[test]
    fn contract_checkpoint_tier_matches_trigger() {
        // Every trigger produces the documented tier
        let pairs = vec![
            (CheckpointTrigger::SlotCompleted, CheckpointTier::Light),
            (CheckpointTrigger::BeforeExpensiveOp, CheckpointTier::Light),
            (CheckpointTrigger::PhaseTransition, CheckpointTier::Heavy),
            (CheckpointTrigger::Explicit, CheckpointTier::Heavy),
        ];
        for (trigger, expected_tier) in pairs {
            assert_eq!(
                trigger.checkpoint_tier(),
                expected_tier,
                "Trigger {:?} should produce {:?}",
                trigger,
                expected_tier
            );
        }
    }

    #[test]
    fn contract_scheduler_owns_retry_decision() {
        let sched = MockScheduler::new();
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec![],
                tool_calls: vec![],
            },
        );
        step.mark_started("a1");
        step.mark_failed("transient error");

        // Scheduler decides retry based on error category AND step.is_retriable()
        let delay = sched.should_retry_step(&step, &ErrorCategory::Transient);
        assert!(delay.is_some());

        // After max attempts exhausted, no retry
        step.execution.attempt = step.execution.max_attempts;
        let delay = sched.should_retry_step(&step, &ErrorCategory::Transient);
        assert!(delay.is_none());
    }
}
