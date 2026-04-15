use std::time::Instant;

use super::agentic_headless_round::HeadlessStderrStyle;
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, HostTurnResult, finalize_and_render,
    finalize_turn_trace, try_write_heavy_checkpoint,
};
use super::agentic_loop_lifecycle::TurnIterationPrep;
use super::agentic_loop_lifecycle::interruption_state_summary;
use super::agentic_turn_ingest::{
    AgenticIngestIterationControl, AgenticTurnIngestMut,
    agentic_turn_stream_snapshot_from_sse_accum, ingest_agentic_turn_stream,
    map_ingest_outcome_to_iteration_control,
};
use super::interruption::{InterruptionKind, InterruptionRecord, ResumeAction};

pub(crate) struct TurnExecutionPhase {
    pub(crate) llm_wall_start: Instant,
    pub(crate) turn_result: HostTurnResult,
}

pub(crate) enum TurnExecutionControl {
    Proceed(Box<TurnExecutionPhase>),
    ContinueLoop,
    Return(AgenticLoopOutcome),
}

pub(crate) async fn execute_turn_and_ingest_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
) -> Result<TurnExecutionControl, String> {
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_started(turn_index as u32);
    }
    let llm_wall_start = Instant::now();
    let turn_result = host.execute_turn(state).await?;
    state.rate_limit_cooldown.record_success();
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_completed(
            turn_index as u32,
            turn_result.ttft_ms,
            llm_wall_start.elapsed().as_millis() as u64,
        );
    }

    let snap = agentic_turn_stream_snapshot_from_sse_accum(&turn_result.accum, turn_result.ttft_ms);
    update_turn_trace_collector(state, &turn_result);

    let edge_len = turn_result.edge_tool_round.len();
    match map_ingest_outcome_to_iteration_control(ingest_agentic_turn_stream(
        &snap,
        edge_len,
        |i| turn_result.edge_tool_round[i].tool.clone(),
        &state.message,
        &state.recent_tools,
        prep.quiet,
        AgenticTurnIngestMut {
            task_profile: state.task_profile,
            first_ttft_ms: &mut state.telemetry.first_ttft_ms,
            current_session_id: &mut state.current_session_id,
            current_run_id: &mut state.current_run_id,
            final_text: &mut state.final_text,
            total_prompt: &mut state.total_prompt,
            total_completion: &mut state.total_completion,
            total_cache_read: &mut state.total_cache_read,
            total_cache_creation: &mut state.total_cache_creation,
            total_tool_calls: &mut state.total_tool_calls,
            total_evidence_tool_calls: &mut state.total_evidence_tool_calls,
            step_recorder: &mut state.step_recorder,
            all_tools_used: &mut state.telemetry.all_tools_used,
            has_any_usage: &mut state.has_any_usage,
            forced_factual_retry: &mut state.stall.forced_factual_retry,
            messages: &mut state.messages,
            last_measured_prompt_tokens: &mut state.last_measured_prompt_tokens,
            consecutive_context_window_errors: &mut state.consecutive_context_window_errors,
            turn_policy: state.last_turn_policy.clone(),
        },
    )) {
        AgenticIngestIterationControl::Fatal(e) => {
            let lower = e.to_lowercase();
            let is_rate_limit = lower.contains("rate")
                || lower.contains("429")
                || lower.contains("too many requests")
                || lower.contains("tpm")
                || lower.contains("rpm");

            if is_rate_limit {
                let is_overload =
                    lower.contains("529") || lower.contains("503") || lower.contains("overload");
                if is_overload {
                    state.rate_limit_cooldown.record_529(None, false);
                } else {
                    state.rate_limit_cooldown.record_429(None, false);
                }
            }

            if is_rate_limit && state.total_tool_calls > 0 {
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "⚠ Rate limit hit after {} tool calls — preserving work.",
                            state.total_tool_calls,
                        ),
                    );
                }
                state.final_text = format!(
                    "[Rate limit reached after {} tool call(s). \
                     All completed tool results are preserved above. \
                     You can continue from where I left off in the next message.]\n\n\
                     Error: {}",
                    state.total_tool_calls, e,
                );
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::RateLimited,
                    ResumeAction::WaitAndRetry { delay_seconds: 30 },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit during streaming: {}", e)),
                    ),
                ));
                observe_turn_end_without_tools(
                    state,
                    turn_index,
                    prep.turn_start_time,
                    turn_result.ttft_ms,
                );
                finalize_and_render(host, state);
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            if is_rate_limit {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::RateLimited,
                    ResumeAction::WaitAndRetry { delay_seconds: 30 },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit during streaming: {}", e)),
                    ),
                ));
            }

            // ── Context-window overflow: compact and retry ────────────
            let is_context_overflow = e
                .contains(crate::turn::llm_client::CONTEXT_WINDOW_ERROR_PREFIX)
                || crate::turn::llm_client::is_context_window_error(&lower);
            if is_context_overflow
                && state.consecutive_context_window_errors
                    <= super::compaction_replay::MAX_COMPACT_RETRIES
            {
                if let Some(result) = super::compaction_replay::try_compact_for_retry(
                    &mut state.messages,
                    state.last_measured_prompt_tokens,
                    state.max_turn_input_tokens,
                ) {
                    let summary = super::compaction_replay::compaction_summary(&result);
                    if !prep.quiet {
                        host.emit_headless_line(
                            HeadlessStderrStyle::Yellow,
                            format!("♻ Context overflow — {}; retrying turn…", summary),
                        );
                    }
                    try_write_heavy_checkpoint(state);
                    return Ok(TurnExecutionControl::ContinueLoop);
                }
            }
            // If we reach here with a context overflow that couldn't be
            // compacted (or retries exhausted), record a structured
            // interruption so the session can resume from checkpoint.
            if is_context_overflow {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::ContextOverflow,
                    ResumeAction::CompactAndRetry,
                    interruption_state_summary(
                        state,
                        Some(format!("Context overflow after compaction: {}", e)),
                    ),
                ));
            }

            finalize_turn_trace(state);
            try_write_heavy_checkpoint(state);
            return Err(e);
        }
        AgenticIngestIterationControl::BreakLoop => {
            if state.hooks.stop_hook_runs == 0
                && let Some(prompt) =
                    crate::turn::stop_hooks::build_stop_hook_prompt(&state.hooks.stop_hooks)
            {
                state.hooks.stop_hook_runs = 1;
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        "⚠ Verification required, continuing…".to_string(),
                    );
                }
                state.messages.push(prompt);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            observe_turn_end_without_tools(
                state,
                turn_index,
                prep.turn_start_time,
                turn_result.ttft_ms,
            );
            finalize_and_render(host, state);
            return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
        }
        AgenticIngestIterationControl::ContinueIterating => {
            try_write_heavy_checkpoint(state);
            return Ok(TurnExecutionControl::ContinueLoop);
        }
        AgenticIngestIterationControl::ProceedWithToolCalls => {}
    }

    emit_subrun_text_preview(host, state, prep.quiet);
    if let Some(control) = handle_token_budget(host, state, turn_index, prep, &turn_result) {
        return Ok(control);
    }
    if should_wrap_up_for_cumulative_budget(host, state, prep.quiet) {
        return Ok(TurnExecutionControl::ContinueLoop);
    }

    record_tool_selection(state, &turn_result, turn_index);

    Ok(TurnExecutionControl::Proceed(Box::new(
        TurnExecutionPhase {
            llm_wall_start,
            turn_result,
        },
    )))
}

fn update_turn_trace_collector(state: &mut AgenticLoopState, turn_result: &HostTurnResult) {
    if let Some(ref collector) = state.telemetry.turn_trace_collector {
        if let Some(spt) = turn_result.accum.system_prompt_tokens {
            collector.set_system_prompt_tokens(spt);
        }
        if let Some(ref breakdown_json) = turn_result.accum.system_prompt_breakdown
            && let Ok(breakdown) = serde_json::from_value::<
                crate::turn::context_assembly_trace::SystemPromptBreakdown,
            >(breakdown_json.clone())
        {
            collector.record_system_prompt(breakdown);
        }
    }
}

fn observe_turn_end_without_tools(
    state: &mut AgenticLoopState,
    turn_index: usize,
    turn_start_time: Instant,
    ttft_ms: Option<u64>,
) {
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability_integration::TurnTiming {
            turn: turn_index as u32,
            context_assembly_ms: 0,
            ttft_ms: ttft_ms.unwrap_or(0),
            llm_total_ms: total_ms,
            tool_execution_ms: 0,
            total_ms,
        };
        let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
        crate::observability_integration::on_turn_end(hub, &mut session_guard, timing);
    }
}

fn emit_subrun_text_preview<H: AgenticLoopHost>(
    host: &mut H,
    state: &AgenticLoopState,
    quiet: bool,
) {
    if !quiet && !state.final_text.is_empty() {
        let preview: String = state.final_text.chars().take(120).collect();
        let line = if state.final_text.len() > 120 {
            format!("{preview}…")
        } else {
            preview
        };
        host.emit_headless_line(HeadlessStderrStyle::Dim, line);
    }
}

fn handle_token_budget<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
    turn_result: &HostTurnResult,
) -> Option<TurnExecutionControl> {
    if state.max_turn_input_tokens == 0 {
        return None;
    }
    let measured = state.last_measured_prompt_tokens?;
    if measured <= state.max_turn_input_tokens {
        return None;
    }

    if state.budget_wrapup_injected {
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                "⚠ Token budget exceeded — completing turn.".to_string(),
            );
        }
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::TokenBudgetExceeded,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some(format!(
                    "Token budget: {}/{} tokens",
                    measured, state.max_turn_input_tokens,
                )),
            ),
        ));
        observe_turn_end_without_tools(
            state,
            turn_index,
            prep.turn_start_time,
            turn_result.ttft_ms,
        );
        finalize_and_render(host, state);
        return Some(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
    }

    state.budget_wrapup_injected = true;
    if !prep.quiet {
        host.emit_headless_line(
            HeadlessStderrStyle::Yellow,
            format!(
                "⚠ Token budget reached ({measured}/{} tokens) — wrapping up.",
                state.max_turn_input_tokens,
            ),
        );
    }
    state.messages.push(serde_json::json!({
        "role": "system",
        "content": "You have reached the token budget limit for this turn. \
            Do NOT call any more tools. Summarize your progress so far and \
            present your results to the user. If you have partial work, \
            explain what remains to be done."
    }));
    try_write_heavy_checkpoint(state);
    Some(TurnExecutionControl::ContinueLoop)
}

fn should_wrap_up_for_cumulative_budget<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    quiet: bool,
) -> bool {
    if state.max_cumulative_tokens == 0 {
        return false;
    }
    let cumulative = state.total_prompt + state.total_completion;
    if cumulative <= state.max_cumulative_tokens || state.budget_wrapup_injected {
        return false;
    }

    state.budget_wrapup_injected = true;
    if !quiet {
        host.emit_headless_line(
            HeadlessStderrStyle::Yellow,
            format!(
                "⚠ Cumulative token budget reached ({cumulative}/{} tokens) — wrapping up.",
                state.max_cumulative_tokens,
            ),
        );
    }
    state.messages.push(serde_json::json!({
        "role": "system",
        "content": "You have reached the cumulative token budget. \
            Do NOT call any more tools. Summarize your progress so far and \
            present your results to the user."
    }));
    try_write_heavy_checkpoint(state);
    true
}

fn record_tool_selection(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_index: usize,
) {
    // Feed confidence trend tracker with latest selector confidence.
    if let Some(conf) = state.telemetry.first_selector_confidence {
        let floor_loop = state.confidence_trend.record(conf);

        // Compute diagnosis from available signals.
        let query_tokens = state.message.split_whitespace().count();
        let dynamic_tools = turn_result.edge_tool_round.len();
        let diagnosis = crate::turn::confidence_contract::ConfidenceDiagnosis::diagnose(
            conf,
            dynamic_tools,     // signal_count proxy
            dynamic_tools > 0, // task_type_known proxy
            0,                 // memory_hint_count: not available here
            0,                 // file_context_count: not available here
            false,             // disambiguation: not available here
            query_tokens,
        );

        if floor_loop {
            tracing::warn!(
                streak = state.confidence_trend.floor_streak(),
                avg = %format!("{:.2}", state.confidence_trend.average_confidence()),
                "confidence-floor loop detected"
            );
        } else if diagnosis.is_actionable() {
            tracing::info!(
                tier = diagnosis.tier.label(),
                confidence = %format!("{:.2}", conf),
                "low-confidence tool selection"
            );
        }

        state.last_confidence_diagnosis = Some(diagnosis);
    }

    if let Some(session) = &state.telemetry.observability_session {
        let selected_tools: Vec<String> = turn_result
            .edge_tool_round
            .iter()
            .map(|r| r.tool.clone())
            .collect();
        if !selected_tools.is_empty() {
            let explanation = crate::turn::decision_explainer::DecisionExplanation {
                id: format!(
                    "tool-sel-{}-{}",
                    state.current_session_id.as_deref().unwrap_or("?"),
                    turn_index
                ),
                timestamp: std::time::SystemTime::now(),
                decision_type: crate::turn::decision_explainer::DecisionType::ToolSelection {
                    selected_tools: selected_tools.clone(),
                    total_available: state.telemetry.all_tools_used.len() as u32,
                },
                inputs: vec![crate::turn::decision_explainer::ExplainableInput {
                    name: "user_query".to_string(),
                    value: state.message.clone(),
                    influence: 1.0,
                    explanation: Some("Primary input driving tool selection".to_string()),
                }],
                reasoning: format!(
                    "LLM selected {} tool(s) for this turn",
                    selected_tools.len()
                ),
                alternatives: vec![],
                confidence: 0.8,
            };
            let mut session_guard = session.write().unwrap_or_else(|e| e.into_inner());
            crate::observability_integration::on_tool_selection(&mut session_guard, explanation);
        }
    }

    if let Some(ref collector) = state.telemetry.turn_trace_collector
        && !collector.has_tool_trace()
    {
        let selected_tools: Vec<String> = turn_result
            .edge_tool_round
            .iter()
            .map(|r| r.tool.clone())
            .collect();
        collector.record_tool_selection(
            &selected_tools,
            state
                .telemetry
                .first_selector_strategy
                .as_deref()
                .unwrap_or("unknown"),
            state.telemetry.first_selector_confidence.unwrap_or(0.0),
            &[],
            state.telemetry.all_tools_used.len() as u32,
            state.telemetry.first_selector_ms.unwrap_or(0),
        );
    }
}
