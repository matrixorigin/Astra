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

use crate::step_checkpoint::FileBackedEventStore;
use crate::step_protocol::*;
use std::collections::HashMap;

/// Records chat_stream execution as Step lifecycle events.
/// Wraps the implicit state machine with explicit StepAction tracking.
pub struct StepRecorder {
    session_id: String,
    task_id: String,
    events: Vec<StepEvent>,
    current_step: Option<Step>,
    turn_number: u32,
    round_index: u32,
    step_sequence: u32,
    current_step_sequence: Option<u32>,
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
            round_index: 0,
            step_sequence: 0,
            current_step_sequence: None,
            slot_counter: 0,
            tool_timings: HashMap::new(),
            phase_log: Vec::new(),
            checkpoint_count: 0,
            file_store: None,
        }
    }

    /// Create with file-backed persistence (events written to JSONL on disk).
    ///
    /// Scans existing checkpoints so `checkpoint_count` starts after the
    /// highest existing file number, preventing cross-turn overwrites.
    pub fn with_persistence(session_id: &str, task_id: &str) -> Self {
        let file_store = FileBackedEventStore::new(session_id);
        let events = file_store.all_events().to_vec();
        let step_sequence = next_step_sequence(&events);
        let existing_max = crate::step_checkpoint::list_checkpoints(session_id)
            .unwrap_or_default()
            .iter()
            .map(|(n, _)| *n)
            .max()
            .unwrap_or(0);
        Self {
            file_store: Some(file_store),
            events,
            step_sequence,
            checkpoint_count: existing_max.saturating_add(1),
            ..Self::new(session_id, task_id)
        }
    }

    /// Attach file-backed persistence after the authoritative session id becomes known.
    ///
    /// Existing in-memory events are rebound to the adopted session id before being
    /// flushed to disk so first-turn forensic artifacts land under the real session.
    pub fn attach_persistence(&mut self, session_id: &str) {
        if self.file_store.is_some() && self.session_id == session_id {
            return;
        }

        self.rebind_session_id(session_id);

        let existing_max = crate::step_checkpoint::list_checkpoints(session_id)
            .unwrap_or_default()
            .iter()
            .map(|(n, _)| *n)
            .max()
            .unwrap_or(0);
        self.checkpoint_count = self.checkpoint_count.max(existing_max.saturating_add(1));

        let mut file_store = FileBackedEventStore::new(session_id);
        for event in &self.events {
            file_store.append(event.clone());
        }
        self.file_store = Some(file_store);
    }

    /// Begin a new turn. Creates a PERCEIVE step.
    pub fn begin_turn(&mut self, turn: u32) {
        self.begin_turn_with_context(turn, turn);
    }

    /// Begin a new agentic round for a visible user turn.
    pub fn begin_turn_with_context(&mut self, visible_turn: u32, round_index: u32) {
        self.turn_number = visible_turn;
        self.round_index = round_index;
        self.slot_counter = 0;
        let step_sequence = self.step_sequence;
        self.step_sequence = self.step_sequence.saturating_add(1);
        self.current_step_sequence = Some(step_sequence);

        let step = Step::new(
            format!(
                "{}-turn-{}-step-{}",
                self.session_id, visible_turn, step_sequence
            ),
            self.task_id.clone(),
            format!("turn-{}", visible_turn),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: String::new(), // filled later
                memory_context: vec![],
            },
        );

        self.emit(step.step_id(), StepEventType::StepCreated);
        self.phase_log
            .push((visible_turn, StepAction::Perceive, epoch_ms()));
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
        self.begin_tool_with_key_and_args_preview(tool_name, call_id, idempotency_key, None);
    }

    /// Record start of a tool execution with idempotency key and argument preview.
    pub fn begin_tool_with_key_and_args_preview(
        &mut self,
        tool_name: &str,
        call_id: &str,
        idempotency_key: Option<&str>,
        args_preview: Option<&str>,
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
            slot.args_preview = args_preview.map(|a| a.to_string());
        }

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "slot_index": slot_idx,
            "call_id": call_id,
            "idempotency_key": idempotency_key,
        });
        if let Some(args_preview) = args_preview {
            payload["args_preview"] = serde_json::json!(args_preview);
        }
        self.emit_with_payload(StepEventType::ToolCallStarted, payload);
    }

    /// Record a cache hit on the current slot (sets cached_result + Skipped state).
    /// Call this instead of complete_tool() when the idempotency cache provides the result.
    pub fn record_cache_hit(&mut self, tool_name: &str, cached: CachedToolResult) {
        self.record_cache_hit_with_reason(tool_name, cached, "idempotency_cache_hit");
    }

    /// Record a cache hit with an explicit trace reason.
    ///
    /// Use a scoped reason (for example `cached_cross_turn`) when the cache
    /// source matters for loop diagnostics and trace replay.
    pub fn record_cache_hit_with_reason(
        &mut self,
        tool_name: &str,
        cached: CachedToolResult,
        reason: &str,
    ) {
        let slot_idx = self.slot_counter.saturating_sub(1);

        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize)
        {
            slot.cached_result = Some(cached);
        }

        self.skip_tool_with_reason(tool_name, reason, true, None);
    }

    /// Record a short-circuit skip for the current tool slot.
    ///
    /// Use this for duplicate blocks, permission/restriction blocks, semantic dedup,
    /// and other paths where the model requested a tool but runtime intentionally
    /// did not execute it.
    pub fn skip_tool_with_reason(
        &mut self,
        tool_name: &str,
        reason: &str,
        was_cached: bool,
        output: Option<&str>,
    ) {
        let slot_idx = self.slot_counter.saturating_sub(1);
        let slot_meta = self.current_step.as_ref().and_then(|step| {
            step.execution.cursor.slots.get(slot_idx as usize).map(|s| {
                (
                    s.call_id.clone(),
                    s.idempotency_key.clone(),
                    s.args_preview.clone(),
                )
            })
        });

        if let Some(ref mut step) = self.current_step {
            if let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize) {
                slot.state = SlotState::Skipped;
            }
            if let Some(StepResult::Act {
                ref mut tool_results_count,
                ..
            }) = step.execution.result
            {
                *tool_results_count += 1;
            }
        }

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "reason": reason,
            "cached": was_cached,
        });
        if let Some((call_id, idem_key, args_preview)) = slot_meta {
            payload["call_id"] = serde_json::json!(call_id);
            if let Some(key) = idem_key {
                payload["idempotency_key"] = serde_json::json!(key);
            }
            if let Some(args_preview) = args_preview {
                payload["args_preview"] = serde_json::json!(args_preview);
            }
        }
        if let Some(output) = output {
            payload["output"] = serde_json::json!(output);
        }
        self.emit_with_payload(StepEventType::ToolCallSkipped, payload);
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
        self.complete_tool_inner(
            tool_name, is_error, elapsed_ms, was_cached, None, None, None,
        );
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
        self.complete_tool_inner(
            tool_name,
            is_error,
            elapsed_ms,
            was_cached,
            Some(output),
            None,
            None,
        );
    }

    /// Record tool execution result with explicit trace metadata. Use this for
    /// runtime paths that already know the call id and arguments so payloads stay
    /// actionable even if slot metadata is incomplete.
    pub fn complete_tool_with_result_and_metadata(
        &mut self,
        tool_name: &str,
        call_id: &str,
        args_preview: Option<&str>,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: &str,
    ) {
        self.complete_tool_inner(
            tool_name,
            is_error,
            elapsed_ms,
            was_cached,
            Some(output),
            Some(call_id),
            args_preview,
        );
    }

    fn complete_tool_inner(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: Option<&str>,
        fallback_call_id: Option<&str>,
        fallback_args_preview: Option<&str>,
    ) {
        let slot_idx = self.slot_counter.saturating_sub(1);

        // Extract trace metadata from slot before mutation.
        let slot_meta = self.current_step.as_ref().and_then(|step| {
            step.execution.cursor.slots.get(slot_idx as usize).map(|s| {
                (
                    s.call_id.clone(),
                    s.idempotency_key.clone(),
                    s.args_preview.clone(),
                )
            })
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
            "is_error": is_error,
        });
        if let Some((call_id, idem_key, args_preview)) = slot_meta {
            let call_id = if call_id.is_empty() {
                fallback_call_id.unwrap_or("")
            } else {
                call_id.as_str()
            };
            if !call_id.is_empty() {
                payload["call_id"] = serde_json::json!(call_id);
            }
            if let Some(key) = idem_key {
                payload["idempotency_key"] = serde_json::json!(key);
            }
            if let Some(args_preview) = args_preview.as_deref().or(fallback_args_preview) {
                payload["args_preview"] = serde_json::json!(args_preview);
            }
        } else if let Some(call_id) = fallback_call_id.filter(|value| !value.is_empty()) {
            payload["call_id"] = serde_json::json!(call_id);
            if let Some(args_preview) = fallback_args_preview {
                payload["args_preview"] = serde_json::json!(args_preview);
            }
        }
        if let Some(out) = output {
            payload["output"] = serde_json::json!(out);
            if is_error {
                payload["error"] = serde_json::json!(out);
            }
        } else if is_error {
            payload["error"] = serde_json::json!("tool failed without captured error");
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
                StepEventType::StepEvaluated
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
    ///
    /// **Idempotent guard**: if `completed_at` is already set, this is a no-op.
    /// This prevents duplicate terminal events when multiple code paths could
    /// reach `end_turn` (e.g., rate-limit early exit + tool phase fallback).
    pub fn end_turn(&mut self, completed: bool) {
        if let Some(ref mut step) = self.current_step {
            if step.execution.completed_at.is_some() {
                return; // already finalized — idempotent guard
            }
            if completed {
                step.execution.status = StepStatus::Completed;
            }
            step.execution.completed_at = Some(epoch_ms());
        }

        let event_type = if completed {
            StepEventType::StepCompleted
        } else {
            StepEventType::StepIncomplete
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
        let total_tool_time_ms: u64 = self.tool_timings.values().flatten().sum();

        let mut slowest_tools: Vec<(String, u64)> = self
            .tool_timings
            .iter()
            .map(|(name, times)| {
                let avg = times.iter().sum::<u64>() / times.len().max(1) as u64;
                (name.clone(), avg)
            })
            .collect();
        slowest_tools.sort_by_key(|b| std::cmp::Reverse(b.1));
        slowest_tools.truncate(5);

        RecorderSummary {
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            iterations: if self.events.is_empty() {
                0
            } else {
                self.turn_number + 1
            },
            total_events: self.events.len(),
            total_tools,
            total_tool_time_ms,
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
        self.build_heavy_checkpoint_with_interruption(
            messages,
            budget_remaining_tokens,
            budget_remaining_rounds,
            blocked_tools,
            recent_tools,
            None,
            None,
            0,
        )
    }

    /// Build a heavy checkpoint, optionally including a structured interruption record
    /// and approval overrides for session continuity.
    pub fn build_heavy_checkpoint_with_interruption(
        &self,
        messages: &[serde_json::Value],
        budget_remaining_tokens: u64,
        budget_remaining_rounds: u32,
        blocked_tools: &[String],
        recent_tools: &[String],
        interruption: Option<serde_json::Value>,
        approval_overrides: Option<serde_json::Value>,
        consecutive_context_window_errors: u32,
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
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption,
            approval_overrides,
            consecutive_context_window_errors,
            compaction_state: None, // Set by caller after construction
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
                vec![
                    self.events
                        .last()
                        .expect("events non-empty")
                        .event_id
                        .clone(),
                ]
            },
            payload: Some(self.trace_context_payload()),
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
            vec![
                self.events
                    .last()
                    .expect("events non-empty")
                    .event_id
                    .clone(),
            ]
        };
        let event = StepEvent {
            event_id: format!("evt-{}-{}", self.events.len(), epoch_ms()),
            step_id,
            event_type,
            agent_id: None,
            caused_by,
            payload: Some(self.with_trace_context(payload)),
            created_at: epoch_ms(),
        };
        if let Some(ref mut fs) = self.file_store {
            fs.append(event.clone());
        }
        self.events.push(event);
    }

    fn trace_context_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "trace_context": {
                "visible_turn": self.turn_number,
                "round_index": self.round_index,
                "step_sequence": self.current_step_sequence,
            }
        })
    }

    fn with_trace_context(&self, mut payload: serde_json::Value) -> serde_json::Value {
        let trace_context = self.trace_context_payload()["trace_context"].clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("trace_context".to_string(), trace_context);
            payload
        } else {
            serde_json::json!({
                "value": payload,
                "trace_context": trace_context,
            })
        }
    }

    fn rebind_session_id(&mut self, session_id: &str) {
        let previous_session_id = self.session_id.clone();
        if previous_session_id == session_id {
            return;
        }

        self.session_id = session_id.to_string();

        if let Some(step) = self.current_step.as_mut() {
            rebind_step(step, &previous_session_id, session_id);
        }
        for event in &mut self.events {
            rebind_step_id(&mut event.step_id, &previous_session_id, session_id);
        }
    }
}

fn rebind_step(step: &mut Step, previous_session_id: &str, session_id: &str) {
    rebind_step_id(
        &mut step.descriptor.step_id,
        previous_session_id,
        session_id,
    );
    if let Some(parent_step_id) = step.descriptor.parent_step_id.as_mut() {
        rebind_step_id(parent_step_id, previous_session_id, session_id);
    }
    if let Some(checkpoint) = step.checkpoint.as_mut() {
        match checkpoint {
            StepCheckpoint::Light(light) => {
                rebind_step_id(&mut light.step_id, previous_session_id, session_id);
            }
            StepCheckpoint::Heavy(heavy) => {
                rebind_step_id(&mut heavy.light.step_id, previous_session_id, session_id);
            }
        }
    }
}

fn rebind_step_id(step_id: &mut String, previous_session_id: &str, session_id: &str) {
    let previous_prefix = format!("{previous_session_id}-turn-");
    if let Some(suffix) = step_id.strip_prefix(&previous_prefix) {
        *step_id = format!("{session_id}-turn-{suffix}");
    }
}

fn next_step_sequence(events: &[StepEvent]) -> u32 {
    events
        .iter()
        .filter_map(|event| {
            event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("trace_context"))
                .and_then(|ctx| ctx.get("step_sequence"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|seq| u32::try_from(seq).ok())
                .or_else(|| {
                    event
                        .step_id
                        .rsplit("-step-")
                        .next()
                        .and_then(|seq| seq.parse::<u32>().ok())
                })
        })
        .max()
        .map_or(0, |seq| seq.saturating_add(1))
}

/// Summary of a recorded session for debugging/audit.
#[derive(Debug, Clone)]
pub struct RecorderSummary {
    pub session_id: String,
    pub task_id: String,
    pub iterations: u32,
    pub total_events: usize,
    pub total_tools: usize,
    pub total_tool_time_ms: u64,
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
    fn skip_tool_with_reason_marks_slot_and_records_payload() {
        let mut rec = StepRecorder::new("sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool_with_key("grep", "call-1", Some("sem:grep"));

        rec.skip_tool_with_reason(
            "grep",
            "duplicate_within_turn",
            false,
            Some("blocked duplicate output"),
        );

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Skipped);
        let last = rec.events().last().unwrap();
        assert_eq!(last.event_type, StepEventType::ToolCallSkipped);
        let payload = last.payload.as_ref().unwrap();
        assert_eq!(
            payload.get("reason").and_then(serde_json::Value::as_str),
            Some("duplicate_within_turn")
        );
        assert_eq!(
            payload
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str),
            Some("sem:grep")
        );
        assert_eq!(
            payload.get("output").and_then(serde_json::Value::as_str),
            Some("blocked duplicate output")
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
    fn regression_incomplete_turn_is_not_recorded_as_retry() {
        let mut rec = StepRecorder::new("sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("read_file", "call-1");
        rec.complete_tool("read_file", false, 12, false);

        rec.end_turn(false);

        let last = rec.events().last().unwrap();
        assert_eq!(
            last.event_type,
            StepEventType::StepIncomplete,
            "normal incomplete turn progression must not be mislabeled as retry"
        );
        assert!(
            !rec.events()
                .iter()
                .any(|event| event.event_type == StepEventType::StepRetried),
            "StepRetried should be reserved for actual retry scheduling"
        );
    }

    #[test]
    fn regression_incomplete_turn_has_single_terminal_event() {
        let mut rec = StepRecorder::new("sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("read_file", "call-1");
        rec.complete_tool("read_file", false, 12, false);
        rec.record_verdict("Healthy", false, false, false, 0);

        rec.end_turn(false);

        let terminal_events: Vec<_> = rec
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    StepEventType::StepCompleted
                        | StepEventType::StepIncomplete
                        | StepEventType::StepFailed
                        | StepEventType::StepRetried
                )
            })
            .collect();
        assert_eq!(
            terminal_events.len(),
            1,
            "a step must not record both StepCompleted and StepIncomplete: {terminal_events:?}"
        );
        assert_eq!(terminal_events[0].event_type, StepEventType::StepIncomplete);
    }

    #[test]
    fn regression_failed_tool_event_carries_actionable_payload() {
        let mut rec = StepRecorder::new("sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool_with_key_and_args_preview(
            "str_replace",
            "call-1",
            Some("sem:str_replace:file.rs"),
            Some("path=file.rs old_str=fn_old"),
        );

        rec.complete_tool_with_result(
            "str_replace",
            true,
            7,
            false,
            "Error: old_str not found in file",
        );

        let failed = rec
            .events()
            .iter()
            .find(|event| event.event_type == StepEventType::ToolCallFailed)
            .expect("expected failed tool event");
        let payload = failed.payload.as_ref().expect("failed event payload");
        assert_eq!(
            payload.get("tool_name").and_then(serde_json::Value::as_str),
            Some("str_replace")
        );
        assert_eq!(
            payload.get("call_id").and_then(serde_json::Value::as_str),
            Some("call-1")
        );
        assert_eq!(
            payload
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str),
            Some("sem:str_replace:file.rs")
        );
        assert_eq!(
            payload
                .get("args_preview")
                .and_then(serde_json::Value::as_str),
            Some("path=file.rs old_str=fn_old")
        );
        assert!(
            payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|error| error.contains("old_str not found")),
            "failed event should carry actionable error, got: {payload:?}"
        );
    }

    #[test]
    fn complete_tool_with_metadata_backfills_actionable_payload() {
        let mut rec = StepRecorder::new("sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);

        rec.complete_tool_with_result_and_metadata(
            "read_file",
            "call-read-1",
            Some("src/main.rs"),
            false,
            12,
            false,
            "file contents",
        );

        let last = rec.events().last().unwrap();
        assert_eq!(last.event_type, StepEventType::ToolCallCompleted);
        let payload = last.payload.as_ref().unwrap();
        assert_eq!(
            payload.get("call_id").and_then(serde_json::Value::as_str),
            Some("call-read-1")
        );
        assert_eq!(
            payload
                .get("args_preview")
                .and_then(serde_json::Value::as_str),
            Some("src/main.rs")
        );
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
        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.total_tools, 3);
        assert_eq!(summary.total_tool_time_ms, 210);
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

    #[test]
    fn with_persistence_starts_after_existing_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let sid = "test-cp-resume";

        // Create checkpoint files directly (simulating previous turns)
        let cp_dir = tmp.path().join(sid).join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("000003-light.json"), "{}").unwrap();
        std::fs::write(cp_dir.join("000005-heavy.json"), "{}").unwrap();

        let rec = StepRecorder::with_persistence(sid, "task-1");
        // checkpoint_count should be max(5,3) + 1 = 6
        assert_eq!(
            rec.summary().checkpoints,
            6,
            "checkpoint_count must start after existing max"
        );
        // tmp is dropped here, cleaning up automatically
    }

    #[test]
    fn attach_persistence_rebinds_existing_events_to_adopted_session() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());

        let mut rec = StepRecorder::new("ephemeral", "task-1");
        rec.begin_turn(0);
        rec.record_plan(&["bash".into()], 0.9, 0.0, 4000);
        rec.attach_persistence("sess-adopted");
        rec.end_turn(true);

        assert_eq!(rec.summary().session_id, "sess-adopted");
        assert_eq!(
            rec.current_step().unwrap().step_id(),
            "sess-adopted-turn-0-step-0"
        );
        assert!(
            rec.events()
                .iter()
                .all(|event| event.step_id == "sess-adopted-turn-0-step-0")
        );

        let adopted_path = tmp.path().join("sess-adopted").join("step_events.jsonl");
        let persisted = std::fs::read_to_string(adopted_path).unwrap();
        assert!(persisted.contains("\"step_id\":\"sess-adopted-turn-0-step-0\""));
        assert!(
            !tmp.path()
                .join("ephemeral")
                .join("step_events.jsonl")
                .exists()
        );
    }

    #[test]
    fn with_persistence_continues_step_sequence_and_causal_chain_across_recorders() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let sid = "sess-continued";

        let mut first = StepRecorder::with_persistence(sid, "task-1");
        first.begin_turn_with_context(0, 0);
        first.end_turn(false);
        let previous_tail = first.events().last().unwrap().event_id.clone();
        drop(first);

        let mut second = StepRecorder::with_persistence(sid, "task-2");
        second.begin_turn_with_context(1, 0);

        assert_eq!(
            second.current_step().unwrap().step_id(),
            "sess-continued-turn-1-step-1"
        );
        let created = second.events().last().unwrap();
        assert_eq!(created.event_type, StepEventType::StepCreated);
        assert_eq!(created.caused_by, vec![previous_tail]);
        let trace_context = created
            .payload
            .as_ref()
            .and_then(|payload| payload.get("trace_context"))
            .unwrap();
        assert_eq!(
            trace_context
                .get("visible_turn")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            trace_context
                .get("round_index")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            trace_context
                .get("step_sequence")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn regression_recreated_visible_turns_have_unique_step_ids_and_context() {
        let mut rec = StepRecorder::new("sess-regression", "task-1");
        rec.begin_turn(6);
        rec.end_turn(false);
        rec.begin_turn(6);
        rec.end_turn(false);

        let created: Vec<&StepEvent> = rec
            .events()
            .iter()
            .filter(|event| event.event_type == StepEventType::StepCreated)
            .collect();
        assert_eq!(created.len(), 2);
        assert_ne!(
            created[0].step_id, created[1].step_id,
            "re-created visible turns need unique step ids for trace correlation"
        );
        for (idx, event) in created.iter().enumerate() {
            let trace_context = event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("trace_context"))
                .expect("step events should carry trace context");
            assert_eq!(
                trace_context
                    .get("visible_turn")
                    .and_then(serde_json::Value::as_u64),
                Some(6)
            );
            assert_eq!(
                trace_context
                    .get("step_sequence")
                    .and_then(serde_json::Value::as_u64),
                Some(idx as u64)
            );
        }
    }

    #[test]
    fn regression_step_events_jsonl_satisfies_trace_invariants() {
        let mut rec = StepRecorder::new("sess-regression", "task-1");
        rec.begin_turn(3);
        rec.record_plan(&["read_file".into()], 0.8, 0.2, 4000);
        rec.begin_act(1);
        rec.begin_tool_with_key_and_args_preview(
            "read_file",
            "call-read-1",
            Some("sem:read:file.rs"),
            Some("path=file.rs start=1 end=20"),
        );
        rec.record_cache_hit_with_reason(
            "read_file",
            CachedToolResult {
                tool_name: "read_file".to_string(),
                output: "cached output".to_string(),
                is_error: false,
                cached_at: 42,
            },
            "cached_cross_turn",
        );
        rec.end_turn(false);

        let jsonl = rec
            .events()
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        let parsed = jsonl
            .lines()
            .map(serde_json::from_str::<StepEvent>)
            .collect::<Result<Vec<_>, _>>()
            .expect("events should parse as JSONL");

        let mut event_ids = std::collections::HashSet::new();
        let mut created_step_ids = std::collections::HashSet::new();
        for (idx, event) in parsed.iter().enumerate() {
            assert!(
                event_ids.insert(event.event_id.clone()),
                "event_id must be unique: {}",
                event.event_id
            );
            if idx == 0 {
                assert!(event.caused_by.is_empty());
            } else {
                assert!(
                    event
                        .caused_by
                        .iter()
                        .all(|parent| event_ids.contains(parent)),
                    "all causal parents must refer to earlier events: {:?}",
                    event.caused_by
                );
            }
            let trace_context = event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("trace_context"))
                .expect("every trace event should carry trace_context");
            assert_eq!(
                trace_context
                    .get("visible_turn")
                    .and_then(serde_json::Value::as_u64),
                Some(3)
            );
            if event.event_type == StepEventType::StepCreated {
                assert!(
                    created_step_ids.insert(event.step_id.clone()),
                    "StepCreated step_id must be unique: {}",
                    event.step_id
                );
            }
            if event.event_type == StepEventType::ToolCallSkipped {
                let payload = event.payload.as_ref().unwrap();
                assert_eq!(
                    payload.get("reason").and_then(serde_json::Value::as_str),
                    Some("cached_cross_turn")
                );
                assert_eq!(
                    payload.get("cached").and_then(serde_json::Value::as_bool),
                    Some(true)
                );
            }
            assert_ne!(
                event.event_type,
                StepEventType::StepRetried,
                "StepRetried must not represent normal cross-round progression"
            );
        }
    }
}
