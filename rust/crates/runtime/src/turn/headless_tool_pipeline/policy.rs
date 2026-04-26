use astra_core::agent_warn;

use super::super::agentic_headless_round::HeadlessStderrStyle;
use super::super::headless_tool_assembly::{
    READ_ONLY_TOOLS, headless_idempotency_hit_openai_pair,
    headless_openai_duplicate_within_turn_pair, headless_unknown_local_tool_openai_pair,
    openai_tool_roundtrip_values, unknown_local_tool_error_message,
};
use super::super::headless_tool_body_preview::emit_headless_tool_body_preview;
use super::super::headless_tool_journal::{
    journal_record_blocked_tool, journal_record_cross_turn_cache_hit,
    journal_record_duplicate_within_turn, journal_record_unknown_tool,
};
use super::super::headless_tool_stderr_lines::{
    headless_stderr_cache_hit_line, headless_stderr_unknown_tool_detail,
    headless_stderr_unknown_tool_header,
};
use super::super::permission_gate::{
    PermissionCheckResult, check_tool_permission, permission_denied_error_result,
};
use super::*;
use crate::turn::edge_prompt_context::make_args_preview;
use crate::turn::tool_result_semantics::tool_dedup_signature;

const OUTCOME_MEMORY_FAILURE_BLOCK_WINDOW: usize = 2;
const OUTCOME_MEMORY_FAILURE_BLOCK_MAX_AGE_SECS: u64 = 60 * 60;

fn emit_blocked_tool_result(
    blocked: HeadlessBlockedTool<'_>,
    step_recorder: &mut crate::pipeline::step_recorder::StepRecorder,
    quiet: bool,
    term: &mut dyn HeadlessRoundTerminal,
    messages: &mut Vec<Value>,
    tool_results: &mut Vec<Value>,
    tool_call_records: &mut Vec<ToolCallRecord>,
) {
    step_recorder.begin_tool_with_key(blocked.name, blocked.id, None);
    step_recorder.skip_tool_with_reason(
        blocked.name,
        blocked.reason_code,
        false,
        Some(&blocked.err_msg),
    );
    if !quiet && let Some(status_line) = blocked.status_line {
        term.emit_line(HeadlessStderrStyle::Yellow, status_line);
    }
    let (tool_msg, err_tr) =
        openai_tool_roundtrip_values(blocked.id, blocked.name, &blocked.err_msg);
    messages.push(tool_msg);
    tool_results.push(err_tr);
    tool_call_records.push(journal_record_blocked_tool(
        blocked.name.to_string(),
        blocked.journal_reason,
        make_args_preview(blocked.name, blocked.args),
        blocked.early_exit_ms,
    ));
}

fn trace_short_circuit_tool_skip(
    step_recorder: &mut crate::pipeline::step_recorder::StepRecorder,
    tool_id: &str,
    tool_name: &str,
    reason: &str,
    idempotency_key: Option<&str>,
    output: Option<&str>,
    was_cached: bool,
) {
    step_recorder.begin_tool_with_key(tool_name, tool_id, idempotency_key);
    step_recorder.skip_tool_with_reason(tool_name, reason, was_cached, output);
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(super) fn emit_turn_budget_stub(&mut self, slot: &HeadlessResolvedToolSlot) {
        let body = format!(
            "⛔ Per-turn tool budget exhausted ({max_tools_per_turn} tools). \
             Skipping this call. Prioritize the most important remaining \
             tools in your next response — do not repeat all skipped calls.",
            max_tools_per_turn = self.ctx.max_tools_per_turn,
        );
        trace_short_circuit_tool_skip(
            self.ctx.step_recorder,
            &slot.id,
            &slot.name,
            "turn_budget_exhausted",
            None,
            Some(&body),
            false,
        );
        let (tool_msg, tr) = headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &body);
        self.ctx.messages.push(tool_msg);
        self.ctx.tool_results.push(tr);
    }

    pub(super) fn handle_empty_tool_name(
        &mut self,
        item: HeadlessRoundToolIdx,
        slot: &HeadlessResolvedToolSlot,
    ) -> HeadlessToolSlotControl {
        self.consecutive_empty_name = self.consecutive_empty_name.saturating_add(1);
        let raw_tc = match item {
            HeadlessRoundToolIdx::ServerToolCall(i) => {
                self.ctx.tool_calls.get(i).map(|v| v.to_string())
            }
            _ => None,
        };
        agent_warn!(
            "step",
            "Empty tool name in slot {item:?} (id={}), raw tool_call: {}",
            slot.id,
            raw_tc.as_deref().unwrap_or("(synthetic edge)")
        );
        let err_msg = unknown_local_tool_error_message(&slot.name, self.ctx.valid_tool_names);
        if !self.ctx.quiet {
            self.ctx.term.emit_line(
                HeadlessStderrStyle::Red,
                headless_stderr_unknown_tool_header(&slot.name),
            );
            self.ctx.term.emit_line(
                HeadlessStderrStyle::Dim,
                headless_stderr_unknown_tool_detail(&err_msg),
            );
        }
        let (tool_msg, err_tr) = headless_unknown_local_tool_openai_pair(
            &slot.id,
            &slot.name,
            self.ctx.valid_tool_names,
        );
        trace_short_circuit_tool_skip(
            self.ctx.step_recorder,
            &slot.id,
            &slot.name,
            "unknown_tool",
            None,
            Some(&err_msg),
            false,
        );
        self.ctx.messages.push(tool_msg);
        self.ctx.tool_results.push(err_tr);
        self.ctx
            .tool_call_records
            .push(journal_record_unknown_tool(slot.name.clone(), 0));
        // Track unknown tool as a failure so ToolHealthTracker can deprioritize
        // after CONSECUTIVE_FAILURE_THRESHOLD hits (prevents infinite retry loops).
        self.ctx.turn_guard.health.record_failure(&slot.name);
        if self.consecutive_empty_name >= Self::MAX_CONSECUTIVE_EMPTY_NAME {
            agent_warn!(
                "step",
                "Aborting headless tool round after {} consecutive empty-name tool calls",
                self.consecutive_empty_name
            );
            HeadlessToolSlotControl::AbortRound
        } else {
            HeadlessToolSlotControl::Continue
        }
    }

    pub(super) fn validate_slot(
        &mut self,
        item: HeadlessRoundToolIdx,
    ) -> HeadlessPipelineStage<ValidatedExecution> {
        if self.executed_this_turn >= self.ctx.max_tools_per_turn {
            let slot = self.resolve_slot(item);
            self.emit_turn_budget_stub(&slot);
            return HeadlessPipelineStage::ShortCircuit;
        }

        let slot = self.resolve_slot(item);

        if self.ctx.pre_resolved_ids.contains(slot.id.as_str()) {
            return HeadlessPipelineStage::ShortCircuit;
        }

        if slot.name.is_empty() {
            return match self.handle_empty_tool_name(item, &slot) {
                HeadlessToolSlotControl::Continue => HeadlessPipelineStage::ShortCircuit,
                HeadlessToolSlotControl::AbortRound => HeadlessPipelineStage::AbortRound,
            };
        }
        self.consecutive_empty_name = 0;

        let call_sig = tool_dedup_signature(&slot.name, &slot.args);
        let count = self.ctx.call_counts.entry(call_sig.clone()).or_insert(0);
        *count += 1;
        if *count > self.ctx.max_identical_calls {
            let idem_key = IdempotencyKey::semantic(&slot.name, &slot.args);
            if let Some(_cached) = self.ctx.idempotency_cache.check(&idem_key) {
                let body = format!(
                    "⛔ Cached repeat (call #{} for identical args, limit: {}). \
                     The result is already in this conversation from an earlier call. \
                     Do NOT call this tool again with the same arguments.",
                    *count, self.ctx.max_identical_calls
                );
                let (tool_msg, tr) =
                    headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &body);
                self.ctx.messages.push(tool_msg);
                self.ctx.tool_results.push(tr);
            } else {
                let (tool_msg, tr) =
                    headless_openai_duplicate_within_turn_pair(&slot.id, &slot.name);
                self.ctx.messages.push(tool_msg);
                self.ctx.tool_results.push(tr);
            }
            trace_short_circuit_tool_skip(
                self.ctx.step_recorder,
                &slot.id,
                &slot.name,
                "duplicate_within_turn",
                Some(&idem_key.cache_key()),
                None,
                false,
            );
            self.ctx
                .tool_call_records
                .push(journal_record_duplicate_within_turn(
                    slot.name.clone(),
                    make_args_preview(&slot.name, &slot.args),
                ));
            self.ctx
                .turn_guard
                .record_cache_hit_for_signature(&slot.name, &call_sig);
            agent_warn!(
                "dedup",
                "Hard cap: tool '{}' (id={}) call #{} (limit: {})",
                slot.name,
                slot.id,
                *count,
                self.ctx.max_identical_calls
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        let idem_key = IdempotencyKey::semantic(&slot.name, &slot.args);
        if READ_ONLY_TOOLS.contains(&slot.name.as_str())
            && let Some(cached) = self.ctx.idempotency_cache.check(&idem_key)
        {
            if !self.ctx.quiet {
                self.ctx.term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_cache_hit_line(&slot.name),
                );
                emit_headless_tool_body_preview(
                    self.ctx.term,
                    self.ctx.quiet,
                    &slot.name,
                    &cached.output,
                    false,
                );
            }
            let (mut tool_msg, tr) =
                headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &cached.output);
            // Add folding metadata so fold_old_read_only_results can decay
            // cache-hit results the same way it decays fresh tool results.
            if let Some(obj) = tool_msg.as_object_mut() {
                obj.insert(
                    "_round_index".to_string(),
                    serde_json::Value::Number(self.ctx.llm_round.into()),
                );
                obj.insert(
                    "_tool_name".to_string(),
                    serde_json::Value::String(slot.name.clone()),
                );
            }
            self.ctx.messages.push(tool_msg);
            self.ctx.tool_results.push(tr);
            let cache_key = idem_key.cache_key();
            self.ctx
                .step_recorder
                .begin_tool_with_key(&slot.name, &slot.id, Some(&cache_key));
            self.ctx
                .step_recorder
                .record_cache_hit(&slot.name, cached.clone());
            self.ctx
                .turn_guard
                .record_cache_hit_for_signature(&slot.name, &call_sig);
            self.ctx
                .tool_call_records
                .push(journal_record_cross_turn_cache_hit(
                    slot.name.clone(),
                    cached.output.len() as u32,
                    make_args_preview(&slot.name, &slot.args),
                ));
            return HeadlessPipelineStage::ShortCircuit;
        }

        if READ_ONLY_TOOLS.contains(&slot.name.as_str())
            && let Some((prev_turn, cached_output)) =
                self.ctx
                    .semantic_dedup
                    .pre_check_block(&slot.name, &slot.args, self.ctx.turn_index)
        {
            let body = format!(
                "{cached_output}\n\n⛔ BLOCKED DUPLICATE: This {} call is semantically \
                 identical to turn {} — same tool with equivalent arguments. \
                 Execution was skipped. Use the result above instead of calling again.",
                slot.name,
                prev_turn + 1,
            );
            let (mut tool_msg, tr) =
                headless_idempotency_hit_openai_pair(&slot.id, &slot.name, &body);
            if let Some(obj) = tool_msg.as_object_mut() {
                obj.insert(
                    "_round_index".to_string(),
                    serde_json::Value::Number(self.ctx.llm_round.into()),
                );
                obj.insert(
                    "_tool_name".to_string(),
                    serde_json::Value::String(slot.name.clone()),
                );
            }
            self.ctx.messages.push(tool_msg);
            self.ctx.tool_results.push(tr);
            trace_short_circuit_tool_skip(
                self.ctx.step_recorder,
                &slot.id,
                &slot.name,
                "semantic_dedup_pre_check",
                Some(&idem_key.cache_key()),
                Some(&body),
                false,
            );
            self.ctx
                .turn_guard
                .record_cache_hit_for_signature(&slot.name, &call_sig);
            self.ctx
                .tool_call_records
                .push(journal_record_cross_turn_cache_hit(
                    slot.name.clone(),
                    cached_output.len() as u32,
                    make_args_preview(&slot.name, &slot.args),
                ));
            agent_warn!(
                "dedup",
                "Semantic block: tool '{}' (id={}) matches turn {} via param-aware dedup",
                slot.name,
                slot.id,
                prev_turn + 1,
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        if let Some(failure_count) =
            should_block_from_outcome_memory(&self.ctx.turn_guard.health, &call_sig)
        {
            let err_msg = format!(
                "blocked_tool: Outcome memory blocked '{}' with identical arguments: \
                 this canonical call failed {} recent time(s) with no intervening success. \
                 Change the arguments, use a different tool, or explain why a retry is necessary.",
                slot.name, failure_count
            );
            emit_blocked_tool_result(
                HeadlessBlockedTool {
                    id: &slot.id,
                    name: &slot.name,
                    args: &slot.args,
                    reason_code: "outcome_memory_blocked",
                    err_msg: err_msg.clone(),
                    journal_reason: err_msg.clone(),
                    early_exit_ms: 0,
                    status_line: Some(format!("  ⚠ Outcome-memory block: {}", slot.name)),
                },
                self.ctx.step_recorder,
                self.ctx.quiet,
                self.ctx.term,
                self.ctx.messages,
                self.ctx.tool_results,
                self.ctx.tool_call_records,
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        let execution = resolve_headless_tool_execution(
            slot,
            self.ctx.edge_tool_round,
            &mut self.consumed_edge,
            self.ctx.by_sig,
        );

        if !self.ctx.valid_tool_names.contains(&execution.name) {
            let err_msg =
                unknown_local_tool_error_message(&execution.name, self.ctx.valid_tool_names);
            if !self.ctx.quiet {
                self.ctx.term.emit_line(
                    HeadlessStderrStyle::Red,
                    headless_stderr_unknown_tool_header(&execution.name),
                );
                self.ctx.term.emit_line(
                    HeadlessStderrStyle::Dim,
                    headless_stderr_unknown_tool_detail(&err_msg),
                );
            }
            let (tool_msg, err_tr) = headless_unknown_local_tool_openai_pair(
                &execution.id,
                &execution.name,
                self.ctx.valid_tool_names,
            );
            trace_short_circuit_tool_skip(
                self.ctx.step_recorder,
                &execution.id,
                &execution.name,
                "unknown_tool",
                None,
                Some(&err_msg),
                false,
            );
            self.ctx.messages.push(tool_msg);
            self.ctx.tool_results.push(err_tr);
            self.ctx.tool_call_records.push(journal_record_unknown_tool(
                execution.name.clone(),
                execution.early_exit_ms,
            ));
            // Track unknown tool as a failure so ToolHealthTracker can deprioritize
            // after CONSECUTIVE_FAILURE_THRESHOLD hits (prevents infinite retry loops).
            self.ctx.turn_guard.health.record_failure(&execution.name);
            return HeadlessPipelineStage::ShortCircuit;
        }

        HeadlessPipelineStage::Continue(ValidatedExecution {
            execution,
            idem_key,
        })
    }

    pub(super) async fn permit_execution(
        &mut self,
        validated: ValidatedExecution,
    ) -> HeadlessPipelineStage<PermittedExecution> {
        let ValidatedExecution {
            mut execution,
            idem_key,
        } = validated;
        if self.ctx.restricted_tools.contains(&execution.name) {
            let err_msg = format!(
                "Tool '{}' is currently restricted and cannot be executed. \
                 Use only the tools whose schemas were provided.",
                execution.name
            );
            emit_blocked_tool_result(
                HeadlessBlockedTool {
                    id: &execution.id,
                    name: &execution.name,
                    args: &execution.args,
                    reason_code: "restricted_tool",
                    journal_reason: err_msg.clone(),
                    err_msg,
                    early_exit_ms: execution.early_exit_ms,
                    status_line: Some(format!("  ⚠ Blocked restricted tool: {}", execution.name)),
                },
                self.ctx.step_recorder,
                self.ctx.quiet,
                self.ctx.term,
                self.ctx.messages,
                self.ctx.tool_results,
                self.ctx.tool_call_records,
            );
            return HeadlessPipelineStage::ShortCircuit;
        }

        let args_str = serde_json::to_string(&execution.args).ok();
        let permission_context = self.ctx.permission_context;
        let effective_permission_timeout = self.ctx.effective_permission_timeout;
        let mailbox = self.ctx.mailbox.as_deref_mut();
        match check_tool_permission(
            &execution.name,
            args_str.as_deref(),
            permission_context,
            mailbox,
            effective_permission_timeout,
        )
        .await
        {
            PermissionCheckResult::Allowed => {}
            PermissionCheckResult::AllowedViaRequest { .. } => {
                if !self.ctx.quiet {
                    self.ctx.term.emit_line(
                        HeadlessStderrStyle::Yellow,
                        format!("  🔓 Permission granted by parent: {}", execution.name),
                    );
                }
            }
            PermissionCheckResult::Denied { reason } => {
                let err_msg = permission_denied_error_result(&execution.name, &reason);
                emit_blocked_tool_result(
                    HeadlessBlockedTool {
                        id: &execution.id,
                        name: &execution.name,
                        args: &execution.args,
                        reason_code: "permission_denied",
                        err_msg,
                        journal_reason: reason,
                        early_exit_ms: execution.early_exit_ms,
                        status_line: Some(format!("  🔒 Permission denied: {}", execution.name)),
                    },
                    self.ctx.step_recorder,
                    self.ctx.quiet,
                    self.ctx.term,
                    self.ctx.messages,
                    self.ctx.tool_results,
                    self.ctx.tool_call_records,
                );
                return HeadlessPipelineStage::ShortCircuit;
            }
        }

        if !self.ctx.tool_event_hooks.is_empty() {
            let decision = crate::skills::hooks::evaluate_pre_tool_hooks(
                self.ctx.tool_event_hooks,
                &execution.name,
                &execution.args,
            )
            .await;
            match decision {
                crate::skills::hooks::PreToolDecision::Block(reason) => {
                    let err_msg = format!(
                        "Tool '{}' blocked by PreToolUse hook: {}",
                        execution.name, reason
                    );
                    emit_blocked_tool_result(
                        HeadlessBlockedTool {
                            id: &execution.id,
                            name: &execution.name,
                            args: &execution.args,
                            reason_code: "pre_tool_hook_blocked",
                            journal_reason: err_msg.clone(),
                            err_msg,
                            early_exit_ms: execution.early_exit_ms,
                            status_line: Some(format!(
                                "  ⚠ Hook blocked: {} — {}",
                                execution.name, reason
                            )),
                        },
                        self.ctx.step_recorder,
                        self.ctx.quiet,
                        self.ctx.term,
                        self.ctx.messages,
                        self.ctx.tool_results,
                        self.ctx.tool_call_records,
                    );
                    return HeadlessPipelineStage::ShortCircuit;
                }
                crate::skills::hooks::PreToolDecision::AllowWithContext(ctx) => {
                    execution.result_str =
                        format!("{}\n\n[Hook context]: {ctx}", execution.result_str);
                }
                crate::skills::hooks::PreToolDecision::Allow => {}
            }
        }

        HeadlessPipelineStage::Continue(PermittedExecution {
            execution,
            idem_key,
        })
    }
}

fn should_block_from_outcome_memory(
    health: &crate::turn::tool_health::ToolHealthTracker,
    call_sig: &str,
) -> Option<usize> {
    let history = health.outcome_history(call_sig)?;
    let recent: Vec<_> = history
        .iter()
        .rev()
        .take(OUTCOME_MEMORY_FAILURE_BLOCK_WINDOW)
        .collect();
    if recent.len() < OUTCOME_MEMORY_FAILURE_BLOCK_WINDOW
        || recent.iter().any(|outcome| outcome.success)
    {
        return None;
    }
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let newest = recent.first()?;
    if newest.at_epoch == 0
        || now_epoch.saturating_sub(newest.at_epoch) > OUTCOME_MEMORY_FAILURE_BLOCK_MAX_AGE_SECS
    {
        return None;
    }
    Some(recent.len())
}
