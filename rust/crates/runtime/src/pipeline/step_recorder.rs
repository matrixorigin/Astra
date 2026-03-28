//! Step Recorder: observation layer that records chat_stream execution as Step events.
//!
//! This module wraps the existing chat_stream.rs execution loop with Step recording,
//! mapping implicit phases to explicit StepAction transitions. It is purely additive —
//! existing control flow is unchanged.
//!
//! # Usage in chat_stream.rs
//!
//! ```ignore
//! let mut recorder = StepRecorder::new("session-123", "task-1");
//!
//! // Before main loop:
//! recorder.begin_turn(turn_number);
//!
//! // After tool selection (PLAN phase):
//! recorder.record_plan(selected_tools, confidence, budget_pressure);
//!
//! // Before each tool execution:
//! recorder.begin_tool(tool_name, &args);
//!
//! // After each tool result:
//! recorder.complete_tool(tool_name, is_error, elapsed_ms);
//!
//! // After turn_guard.evaluate():
//! recorder.record_verdict(severity, injections, force_stop);
//!
//! // After main loop:
//! let summary = recorder.finalize();
//! ```

use crate::pipeline::step_checkpoint::FileBackedEventStore;
use crate::pipeline::step_protocol::*;
use std::collections::HashMap;

/// Records chat_stream execution as Step lifecycle events.
/// Wraps the implicit state machine with explicit StepAction tracking.
pub struct StepRecorder {
    session_id: String,
    task_id: String,
    events: Vec<StepEvent>,
    current_step: Option<Step>,
    turn_number: u32,
    slot_counter: u32,
    /// Per-tool timing for lightweight profiling
    tool_timings: HashMap<String, Vec<u64>>,
    /// Phase transitions recorded for debugging
    phase_log: Vec<(u32, StepAction, u64)>,
    /// Light checkpoint after each tool, heavy after each turn
    checkpoint_count: u32,
    /// Optional file-backed persistence (JSONL) for events
    file_store: Option<FileBackedEventStore>,
}

impl StepRecorder {
    pub fn new(session_id: &str, task_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
            events: Vec::new(),
            current_step: None,
            turn_number: 0,
            slot_counter: 0,
            tool_timings: HashMap::new(),
            phase_log: Vec::new(),
            checkpoint_count: 0,
            file_store: None,
        }
    }

    /// Create with file-backed persistence (events written to JSONL on disk).
    pub fn with_persistence(session_id: &str, task_id: &str) -> Self {
        let file_store = FileBackedEventStore::new(session_id);
        Self {
            file_store: Some(file_store),
            ..Self::new(session_id, task_id)
        }
    }

    /// Begin a new turn. Creates a PERCEIVE step.
    pub fn begin_turn(&mut self, turn: u32) {
        self.turn_number = turn;
        self.slot_counter = 0;

        let step = Step::new(
            format!("{}-turn-{}", self.session_id, turn),
            self.task_id.clone(),
            format!("turn-{}", turn),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: String::new(), // filled later
                memory_context: vec![],
            },
        );

        self.emit(step.step_id(), StepEventType::StepCreated);
        self.phase_log
            .push((turn, StepAction::Perceive, epoch_ms()));
        self.current_step = Some(step);
    }

    /// Record that memory context was loaded (PERCEIVE phase completion).
    pub fn record_perceive(
        &mut self,
        query: &str,
        memory_ids: &[String],
        domain_hints: &[String],
        boost_terms: &[String],
    ) {
        if let Some(ref mut step) = self.current_step {
            step.execution.memory_context = Some(MemoryContext {
                retrieved_memory_ids: memory_ids.to_vec(),
                domain_hints: domain_hints.to_vec(),
                boost_terms: boost_terms.to_vec(),
                provenance: memory_ids.to_vec(),
                governance_actions: memory_ids
                    .iter()
                    .map(|id| MemoryGovernanceAction::Retrieved {
                        memory_id: id.clone(),
                    })
                    .collect(),
                cluster_insights: vec![],
                snapshot_id: None,
            });
            step.execution.payload = StepPayload::Perceive {
                user_query: query.to_string(),
                memory_context: memory_ids.to_vec(),
            };
        }
    }

    /// Record tool selection completion (PLAN phase).
    pub fn record_plan(
        &mut self,
        selected_tools: &[String],
        confidence: f64,
        budget_pressure: f64,
        budget_tokens: u64,
    ) {
        self.transition_phase(StepAction::Plan);
        self.emit_with_payload(
            StepEventType::StepStarted,
            serde_json::json!({
                "selected_tools": selected_tools,
                "confidence": confidence,
                "budget_pressure": budget_pressure,
            }),
        );

        if let Some(ref mut step) = self.current_step {
            step.execution.payload = StepPayload::Plan {
                intent_signals: vec![],
                intent_confidence: confidence,
                available_tool_count: selected_tools.len(),
                budget_tokens,
                restricted_tools: vec![],
            };
            step.execution.result = Some(StepResult::Plan {
                selected_tools: selected_tools.to_vec(),
                confidence,
            });
        }
    }

    /// Transition to ACT phase (before LLM call).
    pub fn begin_act(&mut self, tool_count: usize) {
        self.transition_phase(StepAction::Act);
        if let Some(ref mut step) = self.current_step {
            step.execution.cursor = ExecutionCursor::for_act(tool_count);
            // Initialize Act result so record_tokens() can populate it
            step.execution.result = Some(StepResult::Act {
                tool_results_count: 0,
                assistant_text: None,
                tokens_in: 0,
                tokens_out: 0,
            });
        }
    }

    /// Record start of a tool execution (within ACT phase).
    /// Optionally accepts an idempotency key for cache correlation.
    pub fn begin_tool(&mut self, tool_name: &str, call_id: &str) {
        self.begin_tool_with_key(tool_name, call_id, None);
    }

    /// Record start of a tool execution with idempotency key for cache tracking.
    pub fn begin_tool_with_key(
        &mut self,
        tool_name: &str,
        call_id: &str,
        idempotency_key: Option<&str>,
    ) {
        let slot_idx = self.slot_counter;
        self.slot_counter += 1;

        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize)
        {
            slot.tool_name = tool_name.to_string();
            slot.call_id = call_id.to_string();
            slot.state = SlotState::Running;
            slot.idempotency_key = idempotency_key.map(|k| k.to_string());
        }

        self.emit_with_payload(
            StepEventType::ToolCallStarted,
            serde_json::json!({
                "tool_name": tool_name,
                "slot_index": slot_idx,
                "idempotency_key": idempotency_key,
            }),
        );
    }

    /// Record a cache hit on the current slot (sets cached_result + Skipped state).
    /// Call this instead of complete_tool() when the idempotency cache provides the result.
    pub fn record_cache_hit(&mut self, tool_name: &str, cached: CachedToolResult) {
        let slot_idx = self.slot_counter.saturating_sub(1);

        if let Some(ref mut step) = self.current_step {
            if let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize) {
                slot.cached_result = Some(cached);
                slot.state = SlotState::Skipped;
            }
            // Track in Act result
            if let Some(StepResult::Act {
                ref mut tool_results_count,
                ..
            }) = step.execution.result
            {
                *tool_results_count += 1;
            }
        }

        self.emit_with_payload(
            StepEventType::ToolCallSkipped,
            serde_json::json!({
                "tool_name": tool_name,
                "reason": "idempotency_cache_hit",
            }),
        );

        self.checkpoint_count += 1;
    }

    /// Attach a cached result to the most recently completed slot.
    /// Called after `complete_tool()` when the result is stored in the idempotency cache.
    pub fn attach_cached_result(&mut self, cached: CachedToolResult) {
        let slot_idx = self.slot_counter.saturating_sub(1);
        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize)
        {
            slot.cached_result = Some(cached);
        }
    }

    /// Record tool execution result.
    pub fn complete_tool(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
    ) {
        self.complete_tool_inner(tool_name, is_error, elapsed_ms, was_cached, None);
    }

    /// Record tool execution result with output for crash recovery cache warming.
    /// The output is included in the event payload so that `warm_cache_from_events()`
    /// can reconstruct the idempotency cache on session restore.
    pub fn complete_tool_with_result(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: &str,
    ) {
        self.complete_tool_inner(tool_name, is_error, elapsed_ms, was_cached, Some(output));
    }

    fn complete_tool_inner(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: Option<&str>,
    ) {
        let slot_idx = self.slot_counter.saturating_sub(1);

        // Extract idempotency key from slot before mutation
        let idem_key = self.current_step.as_ref().and_then(|step| {
            step.execution
                .cursor
                .slots
                .get(slot_idx as usize)
                .and_then(|s| s.idempotency_key.clone())
        });

        if let Some(ref mut step) = self.current_step {
            let state = if was_cached {
                SlotState::Skipped
            } else if is_error {
                SlotState::Failed
            } else {
                SlotState::Completed
            };
            step.execution.cursor.advance_slot(slot_idx as usize, state);

            // Track completed tool count in Act result
            if let Some(StepResult::Act {
                ref mut tool_results_count,
                ..
            }) = step.execution.result
            {
                *tool_results_count += 1;
            }
        }

        let event_type = if was_cached {
            StepEventType::ToolCallSkipped
        } else if is_error {
            StepEventType::ToolCallFailed
        } else {
            StepEventType::ToolCallCompleted
        };

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "elapsed_ms": elapsed_ms,
            "cached": was_cached,
        });
        if let Some(key) = &idem_key {
            payload["idempotency_key"] = serde_json::json!(key);
        }
        if let Some(out) = output {
            payload["output"] = serde_json::json!(out);
            payload["is_error"] = serde_json::json!(is_error);
        }

        self.emit_with_payload(event_type, payload);

        self.tool_timings
            .entry(tool_name.to_string())
            .or_default()
            .push(elapsed_ms);

        self.checkpoint_count += 1;
    }

    /// Record tool-level retry.
    pub fn record_retry(&mut self, tool_name: &str, attempt: u32, succeeded: bool) {
        self.emit_with_payload(
            StepEventType::RetryScheduled,
            serde_json::json!({
                "tool_name": tool_name,
                "attempt": attempt,
                "succeeded": succeeded,
            }),
        );

        if succeeded {
            let slot_idx = self.slot_counter.saturating_sub(1);
            if let Some(ref mut step) = self.current_step
                && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize)
            {
                slot.retry_count = attempt;
                slot.state = SlotState::Completed;
            }
        }
    }

    /// Record turn_guard verdict (EVALUATE phase).
    pub fn record_verdict(
        &mut self,
        severity: &str,
        is_stall: bool,
        is_diverging: bool,
        force_stop: bool,
        injections_count: usize,
    ) {
        self.transition_phase(StepAction::Evaluate);

        let verdict = if force_stop {
            StepVerdict::Failed
        } else if is_stall {
            StepVerdict::Stalled
        } else if is_diverging {
            StepVerdict::Diverging
        } else {
            StepVerdict::Continue
        };

        if let Some(ref mut step) = self.current_step {
            let progress = step
                .execution
                .cursor
                .slots
                .iter()
                .filter(|s| s.state == SlotState::Completed)
                .count() as f64
                / step.execution.cursor.slots.len().max(1) as f64;

            step.execution.result = Some(StepResult::Evaluate {
                verdict,
                progress,
                should_continue: !force_stop,
                next_action: if force_stop {
                    StepAction::Fail
                } else {
                    StepAction::Act
                },
            });
        }

        self.emit_with_payload(
            if is_stall {
                StepEventType::StallDetected
            } else if is_diverging {
                StepEventType::DivergenceDetected
            } else {
                StepEventType::StepCompleted
            },
            serde_json::json!({
                "severity": severity,
                "force_stop": force_stop,
                "injections": injections_count,
            }),
        );

        self.checkpoint_count += 1;
    }

    /// Record LLM token usage for the turn.
    pub fn record_tokens(&mut self, prompt_tokens: u64, completion_tokens: u64) {
        if let Some(ref mut step) = self.current_step
            && let Some(StepResult::Act {
                ref mut tokens_in,
                ref mut tokens_out,
                ..
            }) = step.execution.result
        {
            *tokens_in = prompt_tokens;
            *tokens_out = completion_tokens;
        }
    }

    /// Finalize the current turn's step.
    pub fn end_turn(&mut self, completed: bool) {
        if let Some(ref mut step) = self.current_step {
            if completed {
                step.execution.status = StepStatus::Completed;
            }
            step.execution.completed_at = Some(epoch_ms());
        }

        let event_type = if completed {
            StepEventType::StepCompleted
        } else {
            StepEventType::StepRetried
        };
        let step_id = self
            .current_step
            .as_ref()
            .map_or("unknown".to_string(), |s| s.step_id().to_string());
        self.emit(&step_id, event_type);
    }

    /// Get the execution summary after all turns complete.
    pub fn summary(&self) -> RecorderSummary {
        let total_tools: usize = self.tool_timings.values().map(|v| v.len()).sum();
        let total_time_ms: u64 = self.tool_timings.values().flatten().sum();

        let mut slowest_tools: Vec<(String, u64)> = self
            .tool_timings
            .iter()
            .map(|(name, times)| {
                let avg = times.iter().sum::<u64>() / times.len().max(1) as u64;
                (name.clone(), avg)
            })
            .collect();
        slowest_tools.sort_by(|a, b| b.1.cmp(&a.1));
        slowest_tools.truncate(5);

        RecorderSummary {
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            turns: self.turn_number + 1,
            total_events: self.events.len(),
            total_tools,
            total_time_ms,
            slowest_tools,
            checkpoints: self.checkpoint_count,
            phase_log: self.phase_log.clone(),
        }
    }

    /// Get all recorded events (for persistence/audit).
    pub fn events(&self) -> &[StepEvent] {
        &self.events
    }

    /// Get current step reference.
    pub fn current_step(&self) -> Option<&Step> {
        self.current_step.as_ref()
    }

    /// Access the scheduling contract for the current step.
    /// Returns default if no step is active.
    pub fn scheduling(&self) -> SchedulingContract {
        self.current_step
            .as_ref()
            .map(|s| s.descriptor.scheduling.clone())
            .unwrap_or_default()
    }

    /// Build a light checkpoint from current recorder state.
    /// Light checkpoints capture cursor position only — fast, small, frequent.
    pub fn build_light_checkpoint(&self) -> Option<LightCheckpoint> {
        let step = self.current_step.as_ref()?;
        Some(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: step.execution.cursor.clone(),
            step_id: step.step_id().to_string(),
            task_id: self.task_id.clone(),
            agent_id: self.session_id.clone(),
            progress: step
                .execution
                .cursor
                .slots
                .iter()
                .filter(|s| s.state == SlotState::Completed)
                .count() as f64
                / step.execution.cursor.slots.len().max(1) as f64,
            total_tokens: 0, // caller fills in
            created_at: epoch_ms(),
        })
    }

    /// Build a heavy checkpoint with full conversation state for crash recovery.
    pub fn build_heavy_checkpoint(
        &self,
        messages: &[serde_json::Value],
        budget_remaining_tokens: u64,
        budget_remaining_rounds: u32,
        blocked_tools: &[String],
        recent_tools: &[String],
    ) -> Option<HeavyCheckpoint> {
        let light = self.build_light_checkpoint()?;
        Some(HeavyCheckpoint {
            light,
            messages: messages.to_vec(),
            budget_remaining_tokens,
            budget_remaining_rounds,
            blocked_tools: blocked_tools.to_vec(),
            recent_tools: recent_tools.to_vec(),
            learning_snapshot_id: None,
            memory_context: self
                .current_step
                .as_ref()
                .and_then(|s| s.execution.memory_context.clone()),
        })
    }

    // ── Internal helpers ──

    fn transition_phase(&mut self, action: StepAction) {
        self.phase_log.push((self.turn_number, action, epoch_ms()));
        if let Some(ref mut step) = self.current_step {
            step.execution.cursor.phase = action;
        }
    }

    fn emit(&mut self, step_id: &str, event_type: StepEventType) {
        let event = StepEvent {
            event_id: format!("evt-{}-{}", self.events.len(), epoch_ms()),
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: if self.events.is_empty() {
                vec![]
            } else {
                vec![self.events.last().unwrap().event_id.clone()]
            },
            payload: None,
            created_at: epoch_ms(),
        };
        if let Some(ref mut fs) = self.file_store {
            fs.append(event.clone());
        }
        self.events.push(event);
    }

    fn emit_with_payload(&mut self, event_type: StepEventType, payload: serde_json::Value) {
        let step_id = self
            .current_step
            .as_ref()
            .map_or("unknown".to_string(), |s| s.step_id().to_string());
        let caused_by = if self.events.is_empty() {
            vec![]
        } else {
            vec![self.events.last().unwrap().event_id.clone()]
        };
        let event = StepEvent {
            event_id: format!("evt-{}-{}", self.events.len(), epoch_ms()),
            step_id,
            event_type,
            agent_id: None,
            caused_by,
            payload: Some(payload),
            created_at: epoch_ms(),
        };
        if let Some(ref mut fs) = self.file_store {
            fs.append(event.clone());
        }
        self.events.push(event);
    }
}

/// Summary of a recorded session for debugging/audit.
#[derive(Debug, Clone)]
pub struct RecorderSummary {
    pub session_id: String,
    pub task_id: String,
    pub turns: u32,
    pub total_events: usize,
    pub total_tools: usize,
    pub total_time_ms: u64,
    pub slowest_tools: Vec<(String, u64)>,
    pub checkpoints: u32,
    pub phase_log: Vec<(u32, StepAction, u64)>,
}

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

    #[test]
    fn recorder_basic_lifecycle() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);

        assert!(rec.current_step().is_some());
        assert_eq!(rec.current_step().unwrap().action(), StepAction::Perceive);
        assert_eq!(rec.events().len(), 1); // StepCreated
    }

    #[test]
    fn recorder_perceive_records_memory() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.record_perceive(
            "show me PRs",
            &["mem-1".into(), "mem-2".into()],
            &["github".into()],
            &["pr".into(), "pull".into()],
        );

        let step = rec.current_step().unwrap();
        let mc = step.execution.memory_context.as_ref().unwrap();
        assert_eq!(mc.retrieved_memory_ids.len(), 2);
        assert_eq!(mc.domain_hints, vec!["github"]);
        assert_eq!(mc.governance_actions.len(), 2); // 2 Retrieved
    }

    #[test]
    fn recorder_plan_phase_transition() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.record_plan(&["github_list_prs".into(), "grep".into()], 0.85, 0.3, 4000);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Plan);
        assert!(rec.events().len() >= 2); // Created + Started
    }

    #[test]
    fn recorder_act_with_tools() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(2);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Act);
        assert_eq!(step.execution.cursor.slots.len(), 2);

        rec.begin_tool("grep", "call-1");
        rec.complete_tool("grep", false, 50, false);

        rec.begin_tool("read_file", "call-2");
        rec.complete_tool("read_file", false, 10, false);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);
        assert_eq!(step.execution.cursor.slots[1].state, SlotState::Completed);
        assert!(step.execution.cursor.all_slots_done());
    }

    #[test]
    fn recorder_cached_tool_skipped() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("grep", "call-1");
        rec.complete_tool("grep", false, 0, true); // cached

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Skipped);
    }

    #[test]
    fn recorder_tool_failure_and_retry() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("bash", "call-1");
        rec.complete_tool("bash", true, 100, false); // fails

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Failed);

        rec.record_retry("bash", 1, true); // retry succeeds
        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);
        assert_eq!(step.execution.cursor.slots[0].retry_count, 1);
    }

    #[test]
    fn recorder_verdict_stall() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("grep", "call-1");
        rec.complete_tool("grep", false, 50, false);

        rec.record_verdict("Warning", true, false, false, 1);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Evaluate);
        if let Some(StepResult::Evaluate { verdict, .. }) = &step.execution.result {
            assert_eq!(*verdict, StepVerdict::Stalled);
        } else {
            panic!("expected Evaluate result");
        }

        // Should have StallDetected event
        assert!(
            rec.events()
                .iter()
                .any(|e| e.event_type == StepEventType::StallDetected)
        );
    }

    #[test]
    fn recorder_verdict_force_stop() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(0);
        rec.record_verdict("Critical", false, false, true, 2);

        let step = rec.current_step().unwrap();
        if let Some(StepResult::Evaluate {
            should_continue,
            next_action,
            ..
        }) = &step.execution.result
        {
            assert!(!should_continue);
            assert_eq!(*next_action, StepAction::Fail);
        }
    }

    #[test]
    fn recorder_end_turn_completed() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.end_turn(true);

        let step = rec.current_step().unwrap();
        assert_eq!(step.status(), StepStatus::Completed);
        assert!(step.execution.completed_at.is_some());
    }

    #[test]
    fn recorder_summary() {
        let mut rec = StepRecorder::new("sess-1", "task-1");

        // Turn 0: 2 tools
        rec.begin_turn(0);
        rec.begin_act(2);
        rec.begin_tool("grep", "c1");
        rec.complete_tool("grep", false, 100, false);
        rec.begin_tool("read_file", "c2");
        rec.complete_tool("read_file", false, 30, false);
        rec.end_turn(false);

        // Turn 1: 1 tool
        rec.begin_turn(1);
        rec.begin_act(1);
        rec.begin_tool("grep", "c3");
        rec.complete_tool("grep", false, 80, false);
        rec.end_turn(true);

        let summary = rec.summary();
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.total_tools, 3);
        assert_eq!(summary.total_time_ms, 210);
        assert!(!summary.slowest_tools.is_empty());
        assert_eq!(summary.slowest_tools[0].0, "grep"); // grep is slowest (avg 90ms)
    }

    #[test]
    fn recorder_events_form_causal_chain() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("grep", "c1");
        rec.complete_tool("grep", false, 50, false);

        // Every event after the first should reference the previous
        for i in 1..rec.events().len() {
            assert!(
                !rec.events()[i].caused_by.is_empty(),
                "Event {} should have a causal parent",
                i
            );
            assert_eq!(rec.events()[i].caused_by[0], rec.events()[i - 1].event_id);
        }
    }

    #[test]
    fn recorder_multi_turn_phase_log() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.record_plan(&["grep".into()], 0.9, 0.0, 4000);
        rec.begin_act(1);
        rec.record_verdict("Healthy", false, false, false, 0);
        rec.end_turn(false);

        rec.begin_turn(1);
        rec.begin_act(1);
        rec.end_turn(true);

        // Phase log should capture all transitions
        let phases: Vec<StepAction> = rec.summary().phase_log.iter().map(|(_, a, _)| *a).collect();
        // Turn 0: Perceive, Plan, Act, Evaluate
        // Turn 1: Perceive, Act
        assert!(phases.contains(&StepAction::Perceive));
        assert!(phases.contains(&StepAction::Plan));
        assert!(phases.contains(&StepAction::Act));
        assert!(phases.contains(&StepAction::Evaluate));
    }
}
