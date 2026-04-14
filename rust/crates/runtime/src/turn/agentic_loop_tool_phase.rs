use std::collections::HashMap;

use super::agentic_adaptive_tuning::{
    apply_per_turn_adaptation, apply_tactical_actions, maybe_run_tuning_cycle,
};
use super::agentic_auto_reflection::maybe_trigger_auto_reflection;
use super::agentic_delegate_interception::{DelegationInterceptionResult, intercept_delegations};
use super::agentic_headless_round::{
    HeadlessRoundTerminal, HeadlessStderrStyle, HeadlessToolRoundCtx,
    run_agentic_headless_tool_round,
};
use super::agentic_loop_execution_phase::TurnExecutionPhase;
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CONSECUTIVE_ERROR_BUDGET,
    MAX_TRACKED_FILE_READS, extract_file_path_from_tool, finalize_turn_trace,
    record_edge_tool_observability,
};
use super::agentic_loop_lifecycle::TurnIterationPrep;
use super::agentic_post_tool_policy::{
    AgenticPostToolIterationControl, AgenticPostToolPolicyRequest, apply_agentic_post_tool_policy,
    map_post_tool_policy_outcome,
};
use super::agentic_tool_interception::{PreparedToolRound, prepare_intercepted_tool_round};
use super::agentic_turn_flow::{
    agentic_round_stall_preflight_with_tool_calls, append_explain_turn_batch,
};
use super::tool_result_semantics::tool_dedup_signature;

pub(crate) enum TurnToolPhaseControl {
    ContinueLoop,
    Return(AgenticLoopOutcome),
}

fn tool_record_result_text(rec: &astra_services::session_journal::ToolCallRecord) -> &str {
    rec.result_preview
        .as_deref()
        .or(rec.error.as_deref())
        .unwrap_or("")
}

fn tool_record_was_rejected(rec: &astra_services::session_journal::ToolCallRecord) -> bool {
    rec.error
        .as_deref()
        .map(|error| error.starts_with("blocked_tool:"))
        .unwrap_or(false)
}

pub(crate) async fn execute_tool_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
    phase: TurnExecutionPhase,
) -> Result<TurnToolPhaseControl, String> {
    let TurnExecutionPhase {
        llm_wall_start,
        turn_result,
    } = phase;

    let tool_calls_for_guard = agentic_round_stall_preflight_with_tool_calls(
        turn_index,
        &turn_result.accum.tool_calls,
        &turn_result.edge_tool_round,
        &mut state.stall.turn_sigs,
        &mut state.stall.turn_tool_names,
        &mut state.stall.events,
        &mut state.turn_guard,
    );

    let DelegationInterceptionResult {
        effective_tool_calls,
        intercepted_any: delegation_intercepted,
    } = intercept_delegations(host, state, &turn_result, prep.quiet).await;

    let PreparedToolRound {
        tool_calls,
        pre_resolved_results,
        edge_tool_round,
    } = prepare_intercepted_tool_round(
        state,
        &turn_result,
        &effective_tool_calls,
        delegation_intercepted,
    )
    .await;
    let all_tool_calls = tool_calls.as_slice();
    let edge_round_for_headless = edge_tool_round.as_slice();

    let errors_before_round = state.turn_guard.errors.total_errors;
    let errors_by_cat_before = state.turn_guard.errors.errors_by_category.clone();

    struct HostTerminalAdapter<'a, H: AgenticLoopHost>(&'a mut H);
    impl<H: AgenticLoopHost> HeadlessRoundTerminal for HostTerminalAdapter<'_, H> {
        fn emit_line(&mut self, style: HeadlessStderrStyle, line: String) {
            self.0.emit_headless_line(style, line);
        }
    }

    let edge_callback_outputs: HashMap<String, String> = turn_result
        .edge_tool_round
        .iter()
        .map(|r| (tool_dedup_signature(&r.tool, &r.args), r.output.clone()))
        .collect();

    let evo_records_before = state.stall.tool_call_records.len();
    {
        let valid_tool_names = host.valid_tool_names().clone();
        let mut term_adapter = HostTerminalAdapter(host);
        let headless_quiet = prep.quiet || state.skill_produced_output;
        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index,
            quiet: headless_quiet,
            api: &state.api,
            token: &state.api_token,
            current_session_id: state.current_session_id.as_ref(),
            tool_calls: all_tool_calls,
            edge_tool_round: edge_round_for_headless,
            reasoning_content: turn_result.accum.reasoning_content.as_str(),
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut state.messages,
            tool_results: &mut state.tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut state.restricted_tools,
            turn_guard: &mut state.turn_guard,
            step_recorder: &mut state.step_recorder,
            idempotency_cache: &mut state.idempotency_cache,
            semantic_dedup: &mut state.semantic_dedup,
            call_counts: &mut state.call_counts,
            max_identical_calls: state.max_identical_tool_calls,
            max_tools_per_turn: state.max_tools_per_turn,
            tool_call_records: &mut state.stall.tool_call_records,
            tool_event_hooks: &state.skills.tool_event_hooks,
            term: &mut term_adapter,
            mailbox: state.messaging.mailbox.as_mut(),
            permission_context: state.permission_context.as_ref(),
            progress_emitter: state.messaging.progress_emitter.as_ref(),
            pre_resolved_results: &pre_resolved_results,
            server_tool_executor: state.server_tool_executor.as_deref(),
        })
        .await;
    }

    if let Some(ref evo) = state.evolution_service {
        let turn_id = state.current_run_id.as_deref().unwrap_or("unknown");
        let active_skill: Option<String> = state
            .skills
            .invoked
            .iter()
            .max_by_key(|(_, v)| v.invoked_at_turn)
            .map(|(name, _)| name.clone());
        let active_skill_ref = active_skill.as_deref();
        for rec in &state.stall.tool_call_records[evo_records_before..] {
            if rec.is_synthetic_placeholder() {
                continue;
            }
            let result_text = tool_record_result_text(rec);
            let classification = crate::turn::action_compensation::classify_execution_outcome(
                result_text,
                !rec.ok,
                rec.ms,
                tool_record_was_rejected(rec),
            );
            let ctx = crate::evolution::types::ToolResultContext {
                tool_name: &rec.name,
                tool_args: rec.args_preview.as_deref().unwrap_or(""),
                result: result_text,
                is_error: !rec.ok,
                failure_category: classification.failure_category,
                duration_ms: rec.ms,
                active_skill: active_skill_ref,
                turn_id,
            };
            evo.on_tool_result(&ctx).await;
        }

        if !state.stall.turn_sigs.is_empty() {
            let sigs = &state.stall.turn_sigs;
            let n = sigs.len();
            if n >= 3 && sigs[n - 1] == sigs[n - 2] && sigs[n - 2] == sigs[n - 3] {
                let chain: Vec<String> = sigs[n - 1].iter().cloned().collect();
                evo.add_signal(crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: chain,
                    stall_count: 3,
                    turn_id: turn_id.to_string(),
                })
                .await;
            }
        }

        let this_turn = &state.stall.tool_call_records[evo_records_before..];
        let mut fail_counts: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::new();
        for rec in this_turn {
            if !rec.ok {
                *fail_counts.entry(rec.name.as_str()).or_default() += 1;
            }
        }
        for (tool, count) in &fail_counts {
            if *count >= 3 {
                evo.add_signal(crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![(*tool).to_string()],
                    stall_count: *count,
                    turn_id: turn_id.to_string(),
                })
                .await;
            }
        }
    }

    if state.step_signal_collector.is_some() || state.tactical_adapter.is_some() {
        let new_records = &state.stall.tool_call_records[evo_records_before..];
        let mut step_actions: Vec<crate::liquid::tactical::TacticalAction> = Vec::new();

        for rec in new_records {
            let outcome = crate::liquid::step_signals::StepOutcome {
                tool_name: rec.name.clone(),
                ok: rec.ok,
                latency_ms: rec.ms,
                tokens_used: (rec.input_bytes.unwrap_or(0) + rec.output_bytes.unwrap_or(0)) as u64,
                error_hint: rec.error.clone(),
            };
            let triggers = if let Some(ref mut collector) = state.step_signal_collector {
                collector.record(outcome)
            } else {
                vec![]
            };
            if !triggers.is_empty()
                && let Some(ref mut adapter) = state.tactical_adapter
            {
                let actions = adapter.evaluate(&triggers);
                for action in actions {
                    if !matches!(action, crate::liquid::tactical::TacticalAction::NoOp) {
                        step_actions.push(action);
                    }
                }
                adapter.advance_step();
            }
        }

        if !step_actions.is_empty() {
            let hint_parts = apply_tactical_actions(state, &step_actions);
            if !hint_parts.is_empty() {
                let hint_text = format!("[Tactical Adaptation]\n{}", hint_parts.join("\n"));
                state.messages.push(serde_json::json!({
                    "role": "system",
                    "content": hint_text
                }));
            }
        }
    }

    if let Some(ref emitter) = state.messaging.progress_emitter {
        for rec in &state.stall.tool_call_records {
            if let Some(ref err) = rec.error
                && err.starts_with("blocked_tool:")
            {
                emitter.permission_denied(
                    &rec.name,
                    err.trim_start_matches("blocked_tool: "),
                    turn_index as u32,
                );
            }
        }
    }

    append_explain_turn_batch(
        &mut state.telemetry.explain_turns,
        turn_result.accum.explain_turns.as_slice(),
    );

    {
        let turn_num = (state.max_turns - state.remaining_turns) as u32;
        for edge_result in &turn_result.edge_tool_round {
            if let Some(path) = extract_file_path_from_tool(&edge_result.tool, &edge_result.args) {
                if let Some(existing) = state.recent_file_reads.iter_mut().find(|(p, _)| p == &path)
                {
                    existing.1 = turn_num;
                } else {
                    state.recent_file_reads.push((path, turn_num));
                }
                if state.recent_file_reads.len() > MAX_TRACKED_FILE_READS {
                    state.recent_file_reads.sort_by_key(|(_, t)| *t);
                    state.recent_file_reads.remove(0);
                }
            }
        }
    }

    record_edge_tool_observability(state, &turn_result.edge_tool_round);

    if let Some(ref registry) = state.skills.registry_for_activation {
        let mut any_newly_activated = false;
        for edge_result in &turn_result.edge_tool_round {
            if let Some(path) = extract_file_path_from_tool(&edge_result.tool, &edge_result.args) {
                let newly = registry.record_file_path(&path);
                if !newly.is_empty() {
                    any_newly_activated = true;
                    if !prep.quiet {
                        for name in &newly {
                            host.emit_headless_line(
                                HeadlessStderrStyle::Dim,
                                format!("  ◆ Skill activated: {name}"),
                            );
                        }
                    }
                }
            }
        }
        if any_newly_activated && let Some(resolver) = &state.skills.resolver {
            let full = resolver.available_skills();
            if !full.is_empty() {
                let (visible, open_skill_name) =
                    crate::turn::skill_tool::visible_skills_for_host_turn(
                        &full,
                        state.message.as_str(),
                        &state.skills.quality_tracker,
                        &state.skills.pinned,
                        &state.skills.discovered,
                        &state.skills.search,
                    );
                host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema(
                    &visible,
                    Some(&state.skills.quality_tracker),
                    Some(&state.skills.pinned),
                    open_skill_name,
                ));
                if open_skill_name {
                    host.inject_tool_schema(crate::turn::skill_tool::discover_skills_tool_schema());
                }
            }
        }
    }

    {
        let turn_errors = state
            .turn_guard
            .errors
            .total_errors
            .saturating_sub(errors_before_round);
        if turn_errors > 0 {
            let dominant = state
                .turn_guard
                .errors
                .errors_by_category
                .iter()
                .filter_map(|(cat, &count)| {
                    let before = errors_by_cat_before.get(cat).copied().unwrap_or(0);
                    let delta = count.saturating_sub(before);
                    if delta > 0 { Some((*cat, delta)) } else { None }
                })
                .max_by_key(|(_, delta)| *delta)
                .map(|(cat, _)| cat);
            if dominant == state.error_recovery.last_error_category {
                state.error_recovery.consecutive_same_error += 1;
            } else {
                state.error_recovery.consecutive_same_error = 1;
                state.error_recovery.last_error_category = dominant;
            }
            if state.error_recovery.consecutive_same_error >= CONSECUTIVE_ERROR_BUDGET {
                let cat_name = state
                    .error_recovery
                    .last_error_category
                    .map(|c| format!("{c:?}"))
                    .unwrap_or_else(|| "Unknown".into());
                state.messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "🔄 ERROR BUDGET EXHAUSTED: You've hit {cat_name} errors \
                         {n} turns in a row. Your current approach is not working. \
                         STOP repeating the same strategy. You MUST try a fundamentally \
                         different approach: different tool, different file, different \
                         method. If you cannot make progress, explain what's blocking you.",
                        n = state.error_recovery.consecutive_same_error,
                    )
                }));
                state.error_recovery.consecutive_same_error = 0;
            }
        } else {
            state.error_recovery.consecutive_same_error = 0;
            state.error_recovery.last_error_category = None;
        }
    }

    if let Some(ref gate) = state.checkpoint_gate {
        let freq = gate.checkpoint_frequency();
        if freq > 0 && (turn_index as u32 + 1).is_multiple_of(freq) {
            let run_id = state.current_run_id.as_deref().unwrap_or("unknown");
            match gate
                .check(run_id, turn_index as u32, state.total_tool_calls)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    observe_gate_cancelled(state, turn_index, prep.turn_start_time, &turn_result);
                    state.step_recorder.end_turn(true);
                    finalize_turn_trace(state);
                    return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Cancelled));
                }
                Err(e) => {
                    eprintln!("[checkpoint-gate] check error: {e}");
                }
            }
        }
    }

    match map_post_tool_policy_outcome(apply_agentic_post_tool_policy(
        AgenticPostToolPolicyRequest {
            turn_index: turn_index as u32,
            message: &state.message,
            tool_calls_for_guard: &tool_calls_for_guard,
            intent_tool_turns: &mut state.stall.intent_tool_turns,
            messages: &mut state.messages,
            stall_events: &mut state.stall.events,
            turn_guard: &mut state.turn_guard,
            verdict_events: &mut state.stall.verdict_events,
            restricted_tools: &mut state.restricted_tools,
            remaining_turns: &mut state.remaining_turns,
            step_recorder: &mut state.step_recorder,
            current_session_id: state.current_session_id.as_ref(),
            max_turns: state.max_turns,
            loop_turn: turn_index,
            recent_tools: &state.recent_tools,
            last_heavy_checkpoint: &mut state.stall.last_heavy_checkpoint,
        },
    )) {
        AgenticPostToolIterationControl::Abort(e) => {
            finalize_turn_trace(state);
            return Err(e);
        }
        AgenticPostToolIterationControl::RetryLlmClearToolResults => {
            state.tool_results.clear();
        }
        AgenticPostToolIterationControl::ProceedEndTurn => {
            if let Some(ref emitter) = state.messaging.progress_emitter {
                let tool_calls_this_turn =
                    state.total_tool_calls.saturating_sub(if turn_index > 0 {
                        state.total_tool_calls
                    } else {
                        0
                    });
                let last_tool = turn_result
                    .edge_tool_round
                    .last()
                    .map(|r| r.tool.clone())
                    .unwrap_or_else(|| "thinking".to_string());
                emitter.turn_completed(turn_index as u32 + 1, tool_calls_this_turn, last_tool);
                emitter.metrics_update(
                    turn_index as u32 + 1,
                    state.max_turns as u32,
                    state.total_prompt,
                    state.total_completion,
                    state.total_tool_calls,
                );
            }

            if let Some(ref mailbox) = state.messaging.mailbox
                && mailbox.has_parent().await
            {
                if let Err(e) = mailbox
                    .send_progress(
                        turn_index as u32,
                        state.total_tool_calls,
                        "turn_complete",
                        None,
                    )
                    .await
                {
                    astra_core::agent_warn!("mailbox", "Failed to send turn progress: {e}");
                }
            }

            if let (Some(hub), Some(session)) = (
                state.telemetry.observability_hub.as_ref(),
                state.telemetry.observability_session.as_ref(),
            ) {
                let total_ms = prep.turn_start_time.elapsed().as_millis() as u64;
                let ctx_asm_ms = (llm_wall_start - prep.turn_start_time).as_millis() as u64;
                let tool_exec_ms: u64 = turn_result
                    .edge_tool_round
                    .iter()
                    .map(|e| e.duration_ms)
                    .sum();
                let timing = crate::observability_integration::TurnTiming {
                    turn: turn_index as u32,
                    context_assembly_ms: ctx_asm_ms,
                    ttft_ms: turn_result.ttft_ms.unwrap_or(0),
                    llm_total_ms: total_ms
                        .saturating_sub(ctx_asm_ms)
                        .saturating_sub(tool_exec_ms),
                    tool_execution_ms: tool_exec_ms,
                    total_ms,
                };
                let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
                crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
            }

            finalize_turn_trace(state);
            state.step_recorder.end_turn(false);
            state.telemetry.completed_turns_for_tuning += 1;
            maybe_run_tuning_cycle(state);
            maybe_trigger_auto_reflection(host, state).await;
            let turn_tokens = state.last_measured_prompt_tokens.unwrap_or(0);
            apply_per_turn_adaptation(state, turn_tokens);
        }
    }

    Ok(TurnToolPhaseControl::ContinueLoop)
}

fn observe_gate_cancelled(
    state: &mut AgenticLoopState,
    turn_index: usize,
    turn_start_time: std::time::Instant,
    turn_result: &super::agentic_loop_host::HostTurnResult,
) {
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability_integration::TurnTiming {
            turn: turn_index as u32,
            context_assembly_ms: 0,
            ttft_ms: turn_result.ttft_ms.unwrap_or(0),
            llm_total_ms: total_ms,
            tool_execution_ms: 0,
            total_ms,
        };
        let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
        crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
    }
}

#[cfg(test)]
mod tests {
    use super::{tool_record_result_text, tool_record_was_rejected};
    use astra_services::session_journal::ToolCallRecord;

    fn tool_record(ok: bool, error: Option<&str>, result_preview: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok,
            ms: 100,
            error: error.map(str::to_string),
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("{\"command\":\"echo hi\"}".into()),
            result_preview: result_preview.map(str::to_string),
        }
    }

    #[test]
    fn blocked_tool_records_fall_back_to_error_text_and_mark_rejected() {
        let rec = tool_record(
            false,
            Some("blocked_tool: Explicit approval required: action scope is unbounded."),
            None,
        );
        assert_eq!(
            tool_record_result_text(&rec),
            "blocked_tool: Explicit approval required: action scope is unbounded."
        );
        assert!(tool_record_was_rejected(&rec));
    }

    #[test]
    fn executed_tool_records_prefer_result_preview() {
        let rec = tool_record(false, Some("Error: command failed"), Some("stderr preview"));
        assert_eq!(tool_record_result_text(&rec), "stderr preview");
        assert!(!tool_record_was_rejected(&rec));
    }
}
