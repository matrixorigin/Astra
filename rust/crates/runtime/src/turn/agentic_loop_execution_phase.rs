use std::time::Instant;

use super::agentic_headless_round::HeadlessStderrStyle;
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, HostTurnResult, finalize_and_render,
    finalize_turn_trace, try_write_heavy_checkpoint,
};
use super::agentic_loop_lifecycle::{
    TurnIterationPrep, current_agentic_step, interruption_state_summary, session_turn_number,
    tool_record_is_workspace_mutation,
};
use super::agentic_turn_ingest::{
    AgenticIngestIterationControl, AgenticTurnIngestMut, agentic_turn_stream_snapshot_with_kind,
    ingest_agentic_turn_stream, map_ingest_outcome_to_iteration_control,
};
use super::interruption::{InterruptionKind, InterruptionRecord, ResumeAction};

/// Record an `llm_round` event for an early-exit path (no tool calls).
fn record_early_exit_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
    finish_reason: Option<&str>,
) {
    let agentic_step = current_agentic_step(state);
    let run_id = state.current_run_id.clone();
    if let Some(ref mut buf) = state.turn_event_buffer {
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            ttft_ms: turn_result.ttft_ms,
            duration_ms: turn_start.elapsed().as_millis() as u64,
            prompt_tokens: turn_result.accum.prompt_tokens,
            completion_tokens: turn_result.accum.completion_tokens,
            cache_read_tokens: turn_result.accum.cache_read_tokens,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            finish_reason: finish_reason.map(Into::into),
            agentic_step: Some(agentic_step),
            source: Some("agentic_loop".into()),
            run_id,
            tool_calls: None,
        });
    }
}

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
) -> Result<TurnExecutionControl, astra_core::ClassifiedError> {
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_started(turn_index as u32);
    }

    // Inject round budget guidance so the model knows to batch or synthesize.
    // Use llm_rounds_completed (actual LLM call count) not turn_index (step
    // counter inflated by progressive penalty).
    // Skip when the host already injects guidance (e.g. server path injects
    // it into the system prompt in its own execute_turn).
    //
    // The guidance is *ephemeral*: we strip any prior guidance message before
    // injecting a fresh one so the message vec (and any downstream REPL-history
    // replay that keys off it) does not accumulate one guidance block per
    // LLM round. Detection uses the stable headings produced by
    // `round_budget_directive`.
    fn is_ephemeral_round_budget_msg(m: &serde_json::Value) -> bool {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            return false;
        }
        m.get("content")
            .and_then(|c| c.as_str())
            .is_some_and(|s| s.contains("## ⚡ Round Budget") || s.contains("## ⚠ Round Budget"))
    }

    if !host.injects_round_guidance() {
        // Drop any stale guidance message(s) from prior rounds before this call.
        state.messages.retain(|m| !is_ephemeral_round_budget_msg(m));
        let guidance =
            crate::prompts::tool_round_guidance(&state.messages, state.llm_rounds_completed);
        if !guidance.is_empty() {
            astra_turn_core::chat_history_openai::append_openai_user_content_messages(
                &mut state.messages,
                &[guidance],
            );
        }
    }

    // Mid-loop execution escalation: if the model has been burning tool calls
    // on read-only inspection of a mutating task without committing a single
    // edit, force a high-signal correction BEFORE the next LLM call. This
    // catches the failure mode where the loop runs out of budget in an
    // inspection spiral (see session 4178c6a7). One-shot per turn; stripped
    // by `finalize_and_render`.
    if should_escalate_execution(state) {
        let read_only_calls = state
            .stall
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder() && r.ok)
            .count();
        state.stall.forced_execution_escalation = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": execution_escalation_message(&state.message, read_only_calls),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "execution_escalation",
            read_only_calls,
            round = state.llm_rounds_completed,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ Mutating task accumulated {read_only_calls} read-only tool calls with zero edits; forcing escalation…"
                ),
            );
        }
    }

    // Load runtime config once per round for all mid-loop guards below.
    let tool_cfg = &crate::runtime_config::RuntimeConfig::load().tool_selection;
    let parallel_batching_force_threshold =
        tool_cfg.effective_parallel_batching_force_streak() as usize;
    let round_budget_hard_limit = tool_cfg.effective_round_budget_limit();
    let redundant_reads_threshold = tool_cfg.effective_redundant_reads_midloop_threshold() as usize;
    let cache_waste_threshold = tool_cfg.effective_cache_waste_midloop_threshold() as usize;
    let exploration_family_threshold =
        tool_cfg.effective_exploration_family_churn_midloop_threshold() as usize;

    // Third-tier guard: parallel-batching force. Independent of the mutating-
    // task escalation above — fires whenever the model has produced a long
    // streak of trailing single-tool rounds despite the prompt-layer nudge,
    // regardless of task type. Catches the "exploratory churn" failure mode
    // (sessions 6566d6a8, bbae8641, 6da9cf8f). One-shot per turn.
    if should_force_parallel_batching(state, parallel_batching_force_threshold) {
        let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
        state.stall.forced_parallel_batching = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": parallel_batching_force_message(streak, &state.message),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "parallel_batching_force",
            streak,
            round = state.llm_rounds_completed,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ {streak} consecutive single-tool rounds; forcing parallel-batching corrective…"
                ),
            );
        }
    }

    // Round-budget convergence guard — phase 1. When the loop has completed
    // at or above the effective hard limit but the model is still calling
    // tools, inject a final-finalize corrective with explicit anti-
    // hallucination wording. Phase 2 (after this round) escalates to a hard
    // abort if the model ignores phase 1.
    if should_inject_round_budget_phase1(state, round_budget_hard_limit) {
        state.stall.forced_round_budget_phase1 = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": round_budget_phase1_message(state.llm_rounds_completed, &state.message),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "round_budget_phase1",
            round = state.llm_rounds_completed,
            hard_limit = round_budget_hard_limit,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ Round budget exhausted at round {}; forcing text-only finalization…",
                    state.llm_rounds_completed
                ),
            );
        }
    }
    if !state.stall.forced_round_budget_phase1
        && let Some((family, blocked_tools)) = exploration_family_phase2_candidate(state)
    {
        state.stall.forced_exploration_family_phase2 = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": exploration_family_phase2_message(&family, &blocked_tools, &state.message),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "exploration_family_phase2",
            round = state.llm_rounds_completed,
            family = family,
            blocked_tools = ?blocked_tools,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ blocked-only retry on restricted {family} tools [{}]; forcing convergence corrective…",
                    blocked_tools.join(", ")
                ),
            );
        }
    }

    // Redundant-reads mid-loop corrective. Detects the model re-reading
    // overlapping line ranges of the same file with no intervening edit;
    // injects a one-shot corrective telling it to use existing context
    // rather than re-reading. Lives below round-budget phase-1 because
    // phase-1 is the harder finalization push — if both would fire on the
    // same round we prefer phase-1's narrower "stop calling tools" message.
    if !state.stall.forced_round_budget_phase1
        && !state.stall.forced_exploration_family_phase2
        && should_inject_redundant_reads_corrective(state, redundant_reads_threshold)
    {
        let count = astra_turn_core::evaluation::count_redundant_overlapping_reads(
            &state.stall.tool_call_records,
        );
        state.stall.forced_redundant_reads_corrective = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": redundant_reads_corrective_message(count, &state.message),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "redundant_reads_corrective",
            round = state.llm_rounds_completed,
            count = count,
            threshold = redundant_reads_threshold,
            "loop guard fired"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ {count} redundant overlapping reads; nudging model to use existing context…"
                ),
            );
        }
    }
    if !state.stall.forced_round_budget_phase1
        && !state.stall.forced_exploration_family_phase2
        && !state.stall.forced_redundant_reads_corrective
        && should_inject_cache_waste_corrective(state, cache_waste_threshold)
    {
        let wasteful = cache_wasteful_tools(state, cache_waste_threshold);
        state.stall.forced_cache_waste_corrective = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": cache_waste_corrective_message(&wasteful, &state.message),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "cache_waste_corrective",
            round = state.llm_rounds_completed,
            tools = ?wasteful,
            threshold = cache_waste_threshold,
            "loop guard fired"
        );
        if !prep.quiet {
            let tool_list = wasteful
                .iter()
                .map(|(tool, count)| format!("{tool} ({count}x)"))
                .collect::<Vec<_>>()
                .join(", ");
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!("↻ repeated cached tool calls on [{tool_list}]; forcing reuse corrective…"),
            );
        }
    }
    if !state.stall.forced_round_budget_phase1
        && !state.stall.forced_exploration_family_phase2
        && !state.stall.forced_redundant_reads_corrective
        && !state.stall.forced_cache_waste_corrective
        && let Some((family, streak)) =
            exploration_family_corrective_candidate(state, exploration_family_threshold)
    {
        let restricted = apply_exploration_family_restrictions(state, &family);
        state.stall.forced_exploration_family_corrective = true;
        state.messages.push(serde_json::json!({
            "role": "user",
            "content": exploration_family_corrective_message(&family, streak, &restricted, &state.message),
        }));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "exploration_family_corrective",
            round = state.llm_rounds_completed,
            family = family,
            streak = streak,
            restricted = ?restricted,
            "loop guard fired"
        );
        if !prep.quiet {
            let restricted_display = restricted.join(", ");
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ {streak} consecutive low-yield {family} rounds; restricting [{restricted_display}] for the next round…"
                ),
            );
        }
    }

    let llm_wall_start = Instant::now();
    // Increment the LLM-round counter regardless of outcome so retry/error
    // paths don't see a stale count (the counter tracks *attempted* LLM
    // calls for guidance-threshold purposes, not just successful ones).
    let turn_result = host.execute_turn(state).await;
    state.llm_rounds_completed += 1;
    let turn_result = turn_result?;
    state.rate_limit_cooldown.record_success();
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_completed(
            turn_index as u32,
            turn_result.ttft_ms,
            llm_wall_start.elapsed().as_millis() as u64,
        );
    }

    let snap = agentic_turn_stream_snapshot_with_kind(
        &turn_result.accum,
        turn_result.ttft_ms,
        turn_result.error_kind,
    );
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
            use astra_core::ErrorKind;

            let is_rate_limit = matches!(e.kind, ErrorKind::RateLimit);

            if is_rate_limit {
                state.rate_limit_cooldown.record_429(None, false);
            }
            if matches!(e.kind, ErrorKind::ServerError) {
                state.rate_limit_cooldown.record_529(None, false);
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
                    state.total_tool_calls, e.message,
                );
                state.final_text_streamed = false;
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::RateLimited,
                    ResumeAction::WaitAndRetry { delay_seconds: 30 },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit during streaming: {}", e.message)),
                    ),
                ));
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("rate_limited"),
                );
                observe_turn_end_without_tools(
                    state,
                    turn_index,
                    prep.turn_start_time,
                    turn_result.ttft_ms,
                );
                state.step_recorder.end_turn(false);
                finalize_and_render(host, state).await;
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            if is_rate_limit {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::RateLimited,
                    ResumeAction::WaitAndRetry { delay_seconds: 30 },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit during streaming: {}", e.message)),
                    ),
                ));
            }

            // ── Context-window overflow: compact and retry ────────────
            let is_context_overflow = e.kind == ErrorKind::ContextWindow;
            if is_context_overflow {
                // If a prior compaction ran but we still got a 413, mark it insufficient.
                if state.compaction_effectiveness.last_tokens_freed > 0
                    && !state.compaction_effectiveness.last_was_insufficient
                {
                    state.compaction_effectiveness.mark_insufficient();
                }
            }
            if is_context_overflow
                && state.consecutive_context_window_errors
                    <= super::compaction_replay::MAX_COMPACT_RETRIES
            {
                if let Some(result) = super::compaction_replay::try_compact_for_retry_tiered(
                    &mut state.messages,
                    state.last_measured_prompt_tokens,
                    state.max_turn_input_tokens,
                    state.consecutive_context_window_errors,
                ) {
                    let tier_label = result.tier.label();
                    state
                        .compaction_effectiveness
                        .record_compaction(result.tokens_freed);
                    let summary = super::compaction_replay::compaction_summary(&result);
                    if !prep.quiet {
                        host.emit_headless_line(
                            HeadlessStderrStyle::Yellow,
                            format!(
                                "♻ Context overflow — {} pipeline: {}; retrying turn…",
                                tier_label, summary,
                            ),
                        );
                    }

                    // Emit structured compaction telemetry for observability.
                    if let Some(sid) = state.current_session_id.as_deref() {
                        let tokens_freed = result.pipeline_outcome.total_tokens_freed;
                        let budget_likely_satisfied = result.budget_likely_satisfied;
                        let layers: Vec<(String, u64)> = result
                            .pipeline_outcome
                            .layer_results
                            .iter()
                            .map(|(name, cr)| (name.clone(), cr.estimated_tokens_freed))
                            .collect();
                        let evt = astra_services::session_journal::JournalEvent::compaction_retry(
                            Some(sid),
                            session_turn_number(state),
                            tier_label,
                            tokens_freed,
                            budget_likely_satisfied,
                            state.consecutive_context_window_errors,
                            layers,
                            state.consecutive_context_window_errors,
                        )
                        .with_agentic_step(Some(current_agentic_step(state)));
                        if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid)
                        {
                            let _ = writer.append(&evt);
                        }
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
                        Some(format!("Context overflow after compaction: {}", e.message)),
                    ),
                ));
            }

            // Catch-all: map ErrorKind to InterruptionRecord so the checkpoint
            // always carries resume guidance. Existing specific records (rate
            // limit, context overflow) take priority — only fill when still empty.
            if state.interruption.is_none() {
                if let Some((kind, action)) =
                    super::interruption::interruption_from_error_kind(e.kind)
                {
                    state.interruption = Some(InterruptionRecord::new(
                        kind,
                        action,
                        interruption_state_summary(state, Some(e.message.clone())),
                    ));
                }
            }

            finalize_turn_trace(state).await;
            try_write_heavy_checkpoint(state);
            return Err(e);
        }
        AgenticIngestIterationControl::BreakLoop => {
            if let Some(retry_reason) = execution_retry_reason(state) {
                state.stall.forced_execution_retry = true;
                state.final_text.clear();
                // The corrective user message is pushed onto `state.messages`
                // for this loop iteration. The one-shot
                // `forced_execution_retry` flag prevents a second injection,
                // and `finalize_and_render` strips the marker before the next
                // user turn so it does not pollute future conversations.
                state.messages.push(serde_json::json!({
                    "role": "user",
                    "content": execution_retry_message(&state.message, retry_reason),
                }));
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "execution_retry",
                    round = state.llm_rounds_completed,
                    "loop guard fired"
                );
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        execution_retry_notice(retry_reason),
                    );
                }
                // Intentionally skip record_verdict: no evaluation happened, only
                // StepIncomplete is emitted as the terminal event.
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("execution_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

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
                // Intentionally skip record_verdict: no evaluation happened, only
                // StepIncomplete is emitted as the terminal event.
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("stop_hook"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            // Record the LLM round even for text-only responses (no tool calls).
            // Without this, simple Q&A turns have llm_rounds=0 in the
            // journal despite the LLM being called.
            record_early_exit_llm_round(state, &turn_result, prep.turn_start_time, Some("stop"));
            state.step_recorder.end_turn(true);

            observe_turn_end_without_tools(
                state,
                turn_index,
                prep.turn_start_time,
                turn_result.ttft_ms,
            );
            finalize_and_render(host, state).await;
            return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
        }
        AgenticIngestIterationControl::ContinueIterating => {
            // Intentionally skip record_verdict: no evaluation happened, only
            // StepIncomplete is emitted as the terminal event.
            record_early_exit_llm_round(
                state,
                &turn_result,
                prep.turn_start_time,
                Some("continue"),
            );
            state.step_recorder.end_turn(false);
            try_write_heavy_checkpoint(state);
            return Ok(TurnExecutionControl::ContinueLoop);
        }
        AgenticIngestIterationControl::ProceedWithToolCalls => {}
    }

    // Round-budget convergence guard — phase 2. Phase 1 already injected the
    // text-only corrective on this iteration's pre-LLM block. If the model
    // STILL emitted tool calls (i.e. we reached `ProceedWithToolCalls` with
    // the phase-1 flag set), we escalate to a hard abort: refuse to execute
    // the new tool calls and end the loop with a partial-progress notice.
    // This mirrors Claude Code's `error_max_turns` exit but reaches it only
    // after a corrective grace round, avoiding overkill on weaker models.
    // Phase-2 only fires inside `ProceedWithToolCalls`: the model emitted tool
    // calls after the phase-1 corrective, so this flag is inherently true here.
    // Using a named variable rather than a literal makes the contract self-
    // documenting and prevents accidental copy-paste to a broader scope.
    if should_abort_for_round_budget_phase2(state, round_budget_hard_limit) {
        state.stall.forced_round_budget_phase2 = true;
        let abort_msg = format!(
            "[Round budget hard-limit reached at round {}. The runtime injected a \
             finalization corrective on the previous round but the model continued \
             to call tools, so the turn was aborted before executing them. Any \
             progress and tool results from earlier rounds are preserved above.]",
            state.llm_rounds_completed,
        );
        if state.final_text.trim().is_empty() {
            state.final_text = abort_msg.clone();
        } else {
            state.final_text.push_str("\n\n");
            state.final_text.push_str(&abort_msg);
        }
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(state, Some(abort_msg.clone())),
        ));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "round_budget_phase2",
            round = state.llm_rounds_completed,
            "loop guard fired (hard abort)"
        );
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "⛔ Phase-1 corrective ignored at round {}; aborting turn.",
                    state.llm_rounds_completed
                ),
            );
        }
        state.step_recorder.end_turn(false);
        finalize_and_render(host, state).await;
        return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
    }

    emit_subrun_text_preview(host, state, prep.quiet);
    if let Some(control) = handle_token_budget(host, state, turn_index, prep, &turn_result).await {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionRetryReason {
    MissingMutation,
    MissingBrowserVerification,
}

#[cfg(test)]
fn should_force_execution_retry(state: &AgenticLoopState) -> bool {
    execution_retry_reason(state).is_some()
}

fn execution_retry_reason(state: &AgenticLoopState) -> Option<ExecutionRetryReason> {
    if state.stall.forced_execution_retry {
        return None;
    }
    // If mid-loop escalation already fired this turn, the model has already
    // received a stronger corrective message telling it to apply an edit.
    // Adding a second retry injection would duplicate correction, waste
    // tokens, and risk contradicting guidance. One corrective injection per
    // turn is the invariant.
    if state.stall.forced_execution_escalation {
        return None;
    }
    if state.stall.forced_parallel_batching {
        return None;
    }
    if state.stall.forced_round_budget_phase1
        || state.stall.forced_redundant_reads_corrective
        || state.stall.forced_cache_waste_corrective
        || state.stall.forced_exploration_family_corrective
    {
        return None;
    }
    if missing_browser_verification_evidence(state) {
        return Some(ExecutionRetryReason::MissingBrowserVerification);
    }
    if has_concrete_workspace_mutation(state) {
        return None;
    }
    if state.final_text.trim().is_empty() {
        return None;
    }
    let attempted_work_without_mutation = state.total_tool_calls > 0;
    let defers = final_text_defers_execution(&state.final_text);
    if state.task_profile.mutates_workspace {
        if !defers && final_text_concludes_no_change_needed(&state.final_text) {
            return None;
        }
        // Only retry when the model engaged with the task (made tool calls but
        // committed nothing) or explicitly deferred. A bare "Done." or "no fix
        // needed" reply with zero tool calls is treated as a legitimate no-op
        // — retrying would burn a turn for nothing.
        return (attempted_work_without_mutation || defers)
            .then_some(ExecutionRetryReason::MissingMutation);
    }
    (user_confirmed_execution_from_recent_context(state)
        && (attempted_work_without_mutation || defers))
        .then_some(ExecutionRetryReason::MissingMutation)
}

fn missing_browser_verification_evidence(state: &AgenticLoopState) -> bool {
    if state.final_text.trim().is_empty() {
        return false;
    }
    if !message_requires_browser_verification(&state.message) {
        return false;
    }
    if final_text_admits_browser_not_verified(&state.final_text) {
        return false;
    }
    if !final_text_claims_browser_success(&state.final_text) {
        return false;
    }
    !has_browser_verification_evidence(state)
}

fn message_requires_browser_verification(message: &str) -> bool {
    let lower = message.to_lowercase();
    let mentions_browser = [
        "browser",
        "in browser",
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromium",
        "chrome",
        "firefox",
        "webkit",
        "浏览器",
        "ui",
        "页面",
        "canvas",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let mentions_verification = [
        "test",
        "verify",
        "validation",
        "validate",
        "check",
        "open",
        "run",
        "qa",
        "smoke",
        "测试",
        "验证",
        "检查",
        "打开",
        "试玩",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    mentions_browser && mentions_verification
}

fn final_text_claims_browser_success(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "tested",
        "verified",
        "works",
        "working",
        "fully functional",
        "looks good",
        "all good",
        "passes",
        "passed",
        "successfully",
        "已经验证",
        "已验证",
        "测试通过",
        "功能正常",
        "可以正常",
        "运行正常",
        "一切正常",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn final_text_admits_browser_not_verified(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "could not verify in a browser",
        "could not verify in browser",
        "can't verify in a browser",
        "can't verify in browser",
        "not verified in browser",
        "unable to open a browser",
        "unable to open the browser",
        "no browser-capable tool",
        "无法在浏览器中验证",
        "没法在浏览器里验证",
        "不能在浏览器中验证",
        "没有浏览器工具",
        "未在浏览器验证",
        "无法打开浏览器",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_browser_verification_evidence(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.ok && !record.is_synthetic_placeholder())
        .any(tool_record_has_browser_verification_evidence)
}

fn tool_record_has_browser_verification_evidence(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    let lower_name = record.name.to_lowercase();
    if [
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromedriver",
        "geckodriver",
        "webdriver",
    ]
    .iter()
    .any(|needle| lower_name.contains(needle))
    {
        return true;
    }
    if record.name == "bash" {
        let command =
            crate::turn::agentic_loop_lifecycle::extract_bash_command(record.args_full.as_deref())
                .or_else(|| {
                    crate::turn::agentic_loop_lifecycle::extract_bash_command(
                        record.args_preview.as_deref(),
                    )
                });
        if command
            .as_deref()
            .is_some_and(text_has_browser_verification_evidence)
        {
            return true;
        }
    }
    [
        record.args_full.as_deref(),
        record.args_preview.as_deref(),
        record.result_full.as_deref(),
        record.result_preview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(text_has_browser_verification_evidence)
}

fn text_has_browser_verification_evidence(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromium",
        "google-chrome",
        "chrome --headless",
        "chrome-headless",
        "firefox --headless",
        "webkit",
        "chromedriver",
        "geckodriver",
        "--screenshot",
        "--dump-dom",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_concrete_workspace_mutation(state: &AgenticLoopState) -> bool {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.ok && !record.is_synthetic_placeholder())
        .any(tool_record_is_workspace_mutation)
}

fn user_confirmed_execution_from_recent_context(state: &AgenticLoopState) -> bool {
    if !looks_like_execution_confirmation(&state.message) {
        return false;
    }

    state
        .messages
        .iter()
        .rev()
        .take(8)
        .filter(|message| message.get("role").and_then(|role| role.as_str()) == Some("assistant"))
        .filter_map(|message| message.get("content").and_then(|content| content.as_str()))
        .any(assistant_text_offered_execution)
}

fn looks_like_execution_confirmation(message: &str) -> bool {
    let normalized = message
        .trim()
        .trim_matches(|c: char| {
            c.is_ascii_punctuation()
                || c.is_whitespace()
                || matches!(c, '。' | '，' | '！' | '？' | '；' | '：')
        })
        .to_lowercase();
    if normalized.is_empty() || normalized.chars().count() > 24 {
        return false;
    }

    matches!(
        normalized.as_str(),
        "yes"
            | "y"
            | "ok"
            | "okay"
            | "go ahead"
            | "do it"
            | "proceed"
            | "continue"
            | "sure"
            | "当然"
            | "当然了"
            | "好"
            | "好的"
            | "可以"
            | "没问题"
            | "继续"
            | "继续吧"
            | "执行"
            | "直接执行"
            | "开始"
            | "做吧"
    ) || normalized.contains("继续")
        || normalized.contains("执行")
}

fn assistant_text_offered_execution(text: &str) -> bool {
    let lower = text.to_lowercase();
    let offered = [
        "需要我",
        "我可以",
        "要继续吗",
        "即可执行",
        "shall i",
        "should i",
        "want me to",
        "i can",
        "go ahead",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let action = [
        "执行",
        "修改",
        "修复",
        "apply",
        "patch",
        "edit",
        "change",
        "implement",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    offered && action
}

fn final_text_defers_execution(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "需要我直接执行",
        "要继续吗",
        "即可执行",
        "等待确认",
        "shall i",
        "should i",
        "want me to",
        "ready to apply",
        "can apply",
        "can execute",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn final_text_concludes_no_change_needed(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "bug does not exist",
        "bug doesn't exist",
        "issue does not exist",
        "issue doesn't exist",
        "no change needed",
        "no changes needed",
        "nothing to change",
        "already correct",
        "already fixed",
        "not reproducible",
        "cannot reproduce",
        "can't reproduce",
        "无需修改",
        "不需要修改",
        "没有需要修改",
        "问题不存在",
        "没有这个问题",
        "无法复现",
        "未复现",
        "已经正确",
        "已经修复",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Stable marker prefix embedded in the corrective user message so that
/// `finalize_and_render` can strip it after the turn completes. Keeps the
/// conversation history clean across user turns without depending on the
/// downstream compactor's heuristics.
pub(crate) const EXECUTION_RETRY_MARKER: &str = "## ⤴ Execution Retry Correction";

pub(crate) fn is_execution_retry_correction(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXECUTION_RETRY_MARKER))
}

fn execution_retry_notice(reason: ExecutionRetryReason) -> String {
    match reason {
        ExecutionRetryReason::MissingMutation => {
            "↻ Execution requested but no edits were applied; forcing corrective retry…".to_string()
        }
        ExecutionRetryReason::MissingBrowserVerification => {
            "↻ Browser verification was claimed without browser-capable evidence; forcing corrective retry…".to_string()
        }
    }
}

fn execution_retry_message(original_query: &str, reason: ExecutionRetryReason) -> String {
    let correction = match reason {
        ExecutionRetryReason::MissingMutation => {
            "Runtime correction: the user requested or confirmed code execution, \
             but your previous response ended without applying any concrete workspace mutation. \
             Do not ask for permission again and do not only restate a plan. \
             Use the available file-editing tools to make the change, then run the appropriate existing verification."
        }
        ExecutionRetryReason::MissingBrowserVerification => {
            "Runtime correction: this task explicitly required browser/UI verification, \
             but your previous response claimed success without recording any browser-capable verification evidence. \
             Do not treat curl/server/process checks as browser verification. \
             Use a real browser-capable tool or workflow (for example Playwright, Selenium, Puppeteer, Cypress, \
             a headless browser screenshot, or a browser DOM dump after page execution), \
             or say plainly that you could not verify it in a browser."
        }
    };
    format!("{EXECUTION_RETRY_MARKER}\n{correction}\n\nOriginal user query: {original_query}")
}

/// Mid-loop escalation: kicks in while the model is still calling tools but
/// has spent the first several rounds only on read-only inspection (`cat`,
/// `grep`, `ls`, `git diff`, etc.) on a task whose profile says it should be
/// mutating the workspace. Without this guard the loop runs out of budget
/// before a single edit is applied.
pub(crate) const EXECUTION_ESCALATION_MARKER: &str = "## ⤴ Execution Escalation";

/// Minimum successful non-synthetic tool calls accumulated on a mutating task
/// before we start forcing an execution escalation. Chosen to allow a normal
/// "read a couple of files, then edit" workflow to proceed uninterrupted
/// (typical fix workflows commit an edit within 3-5 tool calls), while still
/// catching runaway read loops well before budget exhaustion.
pub(crate) const EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD: usize = 8;

pub(crate) fn is_execution_escalation(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXECUTION_ESCALATION_MARKER))
}

pub(crate) fn is_execution_corrective_message(m: &serde_json::Value) -> bool {
    is_execution_retry_correction(m)
        || is_execution_escalation(m)
        || is_parallel_batching_force(m)
        || is_round_budget_phase1(m)
        || is_redundant_reads_corrective(m)
        || is_cache_waste_corrective(m)
        || is_exploration_family_corrective(m)
        || is_exploration_family_phase2(m)
}

/// Third-tier guard for the parallel-batching layer. The prompt-side soft
/// nudge fires when the trailing single-tool round streak hits
/// `PARALLEL_BATCHING_NUDGE_THRESHOLD` (=4). If the model ignores the nudge
/// and produces yet another single-tool round, the streak crosses
/// `PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD` (=5) and we inject a hard
/// corrective `user` message — the same pattern as `EXECUTION_ESCALATION`,
/// scoped to a different failure mode (sequential read churn rather than
/// read-only spin on a mutating task).
pub(crate) const PARALLEL_BATCHING_FORCE_MARKER: &str = "## ⤴ Parallel Batching Force";

/// Trailing single-tool-round streak length at which the soft prompt nudge
/// (=4) escalates into a forced corrective injection. One above the nudge
/// threshold so the model gets exactly ONE chance to self-correct before we
/// intervene with a higher-priority `user` message.
/// Default for the early-streak threshold; the actual value used at runtime
/// flows through `ToolSelectionConfig::effective_parallel_batching_force_streak`.
/// Must match `effective_parallel_batching_force_streak`'s zero-default.
#[cfg(test)]
pub(crate) const PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD: usize = 5;

/// Tightened streak threshold once the turn has entered the round-budget
/// warning zone (`round_index >= ROUND_BUDGET_THRESHOLD`). At that point any
/// additional sequential single-tool round is materially closer to running
/// out of budget without a final answer, so we intervene more aggressively.
/// Empirical real-session data: turns near the round-budget warning that
/// added a 3rd consecutive single-tool round virtually never converged
/// without external correction.
#[cfg(test)]
pub(crate) const PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD_LATE: usize = 3;

pub(crate) fn is_parallel_batching_force(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(PARALLEL_BATCHING_FORCE_MARKER))
}

pub(crate) fn should_force_parallel_batching(
    state: &AgenticLoopState,
    early_threshold: usize,
) -> bool {
    if state.stall.forced_parallel_batching {
        return false;
    }
    // One corrective injection per turn: if mid-loop retry or escalation
    // already fired, a parallel-batching force would duplicate correction.
    if state.stall.forced_execution_retry || state.stall.forced_execution_escalation {
        return false;
    }
    // Same invariant for the new cascade guards.
    if state.stall.forced_round_budget_phase1
        || state.stall.forced_redundant_reads_corrective
        || state.stall.forced_cache_waste_corrective
        || state.stall.forced_exploration_family_corrective
        || state.stall.forced_exploration_family_phase2
    {
        return false;
    }
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    // Late-budget threshold stays derived (compile-time floor of 2): once the
    // model is close to the round-budget warning, even a short streak should
    // trigger correction. Derived as `max(2, early - 2)` to preserve the
    // original (5, 3) gap without requiring a separate config knob.
    let late_threshold = early_threshold.saturating_sub(2).max(2);
    let threshold = if state.llm_rounds_completed >= crate::prompts::ROUND_BUDGET_THRESHOLD {
        late_threshold
    } else {
        early_threshold
    };
    streak >= threshold
}

pub(crate) fn parallel_batching_force_message(streak: usize, original_query: &str) -> String {
    format!(
        "{PARALLEL_BATCHING_FORCE_MARKER}\n\
         Runtime correction: your last {streak} rounds each ran exactly ONE tool, \
         despite the prompt-layer nudge to batch independent calls. This wastes \
         a round of latency, tokens, and budget for each call. \
         Your NEXT response MUST do exactly one of the following:\n\
         - Produce your final answer now if you already have enough information, OR\n\
         - Call ≥2 independent tools in a single parallel batch (different files, \
           different greps, different reads — anything that does not strictly \
           depend on the previous tool's output).\n\
         Do not produce another single-tool round.\n\n\
         Original user query: {original_query}"
    )
}

// ─── Round-budget convergence guard (two-phase) ─────────────────────────
//
// Phase 1 fires when the loop has completed >= effective round-budget hard
// limit but the model is still calling tools. The runtime injects a hard
// corrective `user` message AND restricts all tools for the upcoming round,
// so the model is forced into a text-only finalization. The corrective
// wording is explicitly anti-hallucination: it tells the model to enumerate
// what was verified and what was NOT verified instead of fabricating.
//
// Phase 2 is the safety net: if the model still produces tool calls after
// phase 1 (i.e. ignores both the corrective AND attempts tools that were
// runtime-restricted), `should_abort_for_round_budget_phase2` returns true
// and the caller aborts the loop — equivalent to Claude Code's
// `error_max_turns` but reached only after one extra grace round, which
// avoids the overkill of an immediate hard cap on weaker models.

pub(crate) const ROUND_BUDGET_PHASE1_MARKER: &str = "## ⤴ Round Budget Reached";

pub(crate) fn is_round_budget_phase1(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(ROUND_BUDGET_PHASE1_MARKER))
}

/// Whether to inject phase-1 corrective on the upcoming round. Caller passes
/// the effective hard limit so test/runtime/ToolSelectionConfig overrides
/// flow through.
pub(crate) fn should_inject_round_budget_phase1(state: &AgenticLoopState, hard_limit: u32) -> bool {
    if state.stall.forced_round_budget_phase1 {
        return false;
    }
    // One corrective injection per turn: if another guard already fired
    // this round, skip phase-1 to avoid double-injecting. In practice
    // phase-1 fires at round >= hard_limit (typically 15) while other
    // guards fire earlier, so overlap is rare — this is defensive.
    if state.stall.forced_execution_escalation
        || state.stall.forced_parallel_batching
        || state.stall.forced_redundant_reads_corrective
        || state.stall.forced_cache_waste_corrective
        || state.stall.forced_exploration_family_corrective
    {
        return false;
    }
    state.llm_rounds_completed >= hard_limit
}

/// Whether to abort the loop with phase-2 hard stop. Triggers when phase-1
/// already fired AND the most-recent assistant turn still attempted at least
/// one tool call AND `llm_rounds_completed >= hard_limit` (sanity guard
/// prevents mis-set flags from aborting at a low round count). The caller
/// passes `last_round_had_tool_calls` to distinguish model compliance
/// (text-only) from model ignoring the corrective (tool calls).
pub(crate) fn should_abort_for_round_budget_phase2(
    state: &AgenticLoopState,
    hard_limit: u32,
) -> bool {
    if state.stall.forced_round_budget_phase2 {
        return false;
    }
    // Phase-2 fires when phase-1 corrective was injected but the model
    // continued to call tools (ignored the corrective).
    state.stall.forced_round_budget_phase1 && state.llm_rounds_completed >= hard_limit
}

pub(crate) fn round_budget_phase1_message(round_index: u32, original_query: &str) -> String {
    format!(
        "{ROUND_BUDGET_PHASE1_MARKER}\n\
         Runtime correction: this turn has used {round_index} tool rounds and \
         is past the configured hard limit. The runtime has now disabled all \
         tools for the next round — your next response MUST be a final \
         text-only answer.\n\n\
         IMPORTANT (anti-hallucination):\n\
         - Synthesize what you DID verify with the tool calls already made.\n\
         - Explicitly list anything you could NOT verify or finish.\n\
         - Do NOT fabricate, infer, or invent results you did not actually observe.\n\
         - A partial-but-honest answer is strictly better than a confident-but-fabricated one.\n\n\
         Original user query: {original_query}"
    )
}

// Redundant-reads mid-loop corrective.
//
// Detects the pattern where the model re-reads overlapping line ranges of the
// same file with no intervening workspace mutation. The detection algorithm
// lives in `astra-turn-core::evaluation::count_redundant_overlapping_reads`
// (post-mortem use) and is reused here for a one-shot mid-loop corrective.
//
// Threshold note: post-mortem flags at count ≥ 3; mid-loop fires at ≥
// `REDUNDANT_READS_MIDLOOP_THRESHOLD` to err slightly on the side of
// underkill, since this is a behavioral intervention rather than a passive
// signal. Calibrated against the same 14k-session survey: confirmed-waste
// fixtures all reach 7+ within their turn, so a threshold of 4 still catches
// every problem turn well before the count balloons.

pub(crate) const REDUNDANT_READS_MARKER: &str = "## ⤴ Redundant Reads Detected";
pub(crate) const CACHE_WASTE_MARKER: &str = "## ⤴ Repeated Cached Tool Calls Detected";
pub(crate) const EXPLORATION_FAMILY_MARKER: &str = "## ⤴ Exploration Family Churn Detected";
pub(crate) const EXPLORATION_FAMILY_PHASE2_MARKER: &str =
    "## ⤴ Exploration Family Convergence Required";
/// Default cache-waste midloop threshold. Used in tests; production code
/// reads from `ToolSelectionConfig::effective_cache_waste_midloop_threshold()`.
#[cfg(test)]
pub(crate) const CACHE_WASTE_MIDLOOP_THRESHOLD: usize = 3;

/// Mid-loop corrective threshold (intentionally one above the post-mortem
/// signal threshold). One redundant overlap is normal noise; two can be
/// healthy double-checking; three matches the post-mortem signal but at
/// mid-loop we wait one more event to avoid premature intervention on
/// borderline turns.
/// Default for the redundant-reads mid-loop threshold; the actual value used
/// at runtime flows through
/// `ToolSelectionConfig::effective_redundant_reads_midloop_threshold`. Must
/// match that accessor's zero-default.
#[cfg(test)]
pub(crate) const REDUNDANT_READS_MIDLOOP_THRESHOLD: usize = 4;

pub(crate) fn is_redundant_reads_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(REDUNDANT_READS_MARKER))
}

fn cache_wasteful_tools(state: &AgenticLoopState, threshold: usize) -> Vec<(String, usize)> {
    let mut tools: Vec<(String, usize)> = state
        .turn_guard
        .health
        .cache_wasteful_tools(threshold)
        .into_iter()
        .map(|(tool, count)| (tool.to_string(), count))
        .collect();
    tools.sort_by(|left, right| left.0.cmp(&right.0));
    tools
}

pub(crate) fn is_cache_waste_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(CACHE_WASTE_MARKER))
}

pub(crate) fn is_exploration_family_corrective(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXPLORATION_FAMILY_MARKER))
}

pub(crate) fn is_exploration_family_phase2(m: &serde_json::Value) -> bool {
    if m.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    m.get("content")
        .and_then(|c| c.as_str())
        .is_some_and(|s| s.starts_with(EXPLORATION_FAMILY_PHASE2_MARKER))
}

fn restricted_tools_for_exploration_family(family: &str) -> &'static [&'static str] {
    match family {
        "diff" => &["git_diff"],
        "search" => &["glob", "grep", "rg"],
        "read" => &["read_file", "view"],
        _ => &[],
    }
}

fn exploration_family_label(family: &str) -> &'static str {
    match family {
        "diff" => "diff-review",
        "search" => "search",
        "read" => "read",
        _ => "exploration",
    }
}

pub(crate) fn exploration_family_corrective_candidate(
    state: &AgenticLoopState,
    threshold: usize,
) -> Option<(String, usize)> {
    if state.stall.forced_exploration_family_corrective {
        return None;
    }
    let (family, streak) = astra_turn_core::evaluation::exploration_family_round_streak(
        &state.stall.tool_call_records,
    )?;
    (streak >= threshold).then(|| (family.to_string(), streak))
}

fn apply_exploration_family_restrictions(
    state: &mut AgenticLoopState,
    family: &str,
) -> Vec<String> {
    let mut restricted = restricted_tools_for_exploration_family(family)
        .iter()
        .map(|tool| (*tool).to_string())
        .collect::<Vec<_>>();
    restricted.sort();
    for tool in &restricted {
        state.restricted_tools.insert(tool.clone());
    }
    state.stall.exploration_family_corrective_family = Some(family.to_string());
    restricted
}

fn latest_non_synthetic_round_records(
    state: &AgenticLoopState,
) -> Option<(u32, Vec<&astra_services::session_journal::ToolCallRecord>)> {
    let last_round = state
        .stall
        .tool_call_records
        .iter()
        .filter(|rec| !rec.is_synthetic_placeholder())
        .filter_map(|rec| rec.round)
        .max()?;
    let records = state
        .stall
        .tool_call_records
        .iter()
        .filter(|rec| !rec.is_synthetic_placeholder())
        .filter(|rec| rec.round == Some(last_round))
        .collect::<Vec<_>>();
    Some((last_round, records))
}

pub(crate) fn exploration_family_phase2_candidate(
    state: &AgenticLoopState,
) -> Option<(String, Vec<String>)> {
    if !state.stall.forced_exploration_family_corrective
        || state.stall.forced_exploration_family_phase2
    {
        return None;
    }
    let family = state
        .stall
        .exploration_family_corrective_family
        .as_deref()?;
    let restricted = restricted_tools_for_exploration_family(family);
    let (_, latest_round_records) = latest_non_synthetic_round_records(state)?;
    if latest_round_records.is_empty() {
        return None;
    }

    let mut blocked_tools = latest_round_records
        .iter()
        .filter(|rec| rec.was_blocked_by_policy() && restricted.contains(&rec.name.as_str()))
        .map(|rec| rec.name.clone())
        .collect::<Vec<_>>();
    if blocked_tools.is_empty() || blocked_tools.len() != latest_round_records.len() {
        return None;
    }
    blocked_tools.sort();
    blocked_tools.dedup();
    Some((family.to_string(), blocked_tools))
}

/// Whether to inject the redundant-reads mid-loop corrective on the upcoming
/// round. One-shot per turn (the flag is set when corrective fires).
pub(crate) fn should_inject_redundant_reads_corrective(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.forced_redundant_reads_corrective {
        return false;
    }
    let count = astra_turn_core::evaluation::count_redundant_overlapping_reads(
        &state.stall.tool_call_records,
    );
    count >= threshold
}

pub(crate) fn should_inject_cache_waste_corrective(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.forced_cache_waste_corrective {
        return false;
    }
    !cache_wasteful_tools(state, threshold).is_empty()
}

pub(crate) fn cache_waste_corrective_message(
    tools: &[(impl AsRef<str>, usize)],
    original_query: &str,
) -> String {
    let tool_list = tools
        .iter()
        .map(|(tool, count)| format!("{} ({count}x)", tool.as_ref()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{CACHE_WASTE_MARKER}\n\
         Runtime correction: you have repeated cached tool calls this turn [{tool_list}]. \
         Those results are already in context — calling the same tool again wastes tokens and does not add evidence.\n\n\
         REQUIRED next-step behavior:\n\
         - Reuse the cached result you already have; do NOT repeat the same tool call again.\n\
         - Only call another tool if it fetches genuinely new evidence (different file, different diff target, different query, or changed worktree).\n\
         - If you already have enough evidence, write the final answer now.\n\
         - If you still need more evidence, explain the ONE specific missing fact and use a different tool or different arguments to get it.\n\n\
         Anti-hallucination: do NOT pretend a repeated cached call produced new information.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn exploration_family_corrective_message(
    family: &str,
    streak: usize,
    restricted_tools: &[String],
    original_query: &str,
) -> String {
    let tool_list = restricted_tools.join(", ");
    let label = exploration_family_label(family);
    format!(
        "{EXPLORATION_FAMILY_MARKER}\n\
         Runtime correction: the last {streak} consecutive multi-call rounds stayed inside the same {label} family. \
         That is now classified as low-yield exploration churn, so the runtime has restricted [{tool_list}] for the next round.\n\n\
         REQUIRED next-step behavior:\n\
         - First synthesize the evidence already gathered from prior tool calls.\n\
         - If one fact is still missing, switch to a different tool family that can add genuinely new evidence.\n\
         - Do NOT reopen the same {family} path unless the worktree or target changed.\n\
         - If you already have enough evidence, write the answer now.\n\n\
         Anti-hallucination: do NOT claim that repeated {family} exploration produced new evidence when it did not.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn exploration_family_phase2_message(
    family: &str,
    blocked_tools: &[String],
    original_query: &str,
) -> String {
    let blocked_list = blocked_tools.join(", ");
    format!(
        "{EXPLORATION_FAMILY_PHASE2_MARKER}\n\
         Runtime correction: after the earlier {family}-family restriction, your most recent tool round still attempted ONLY restricted tools [{blocked_list}]. \
         That produced zero new evidence, so this turn must now converge instead of retrying the same path.\n\n\
         REQUIRED next-step behavior:\n\
         - Either write the answer now from the evidence already gathered, OR\n\
         - State the ONE missing fact and use ONE tool from a different family to fetch it.\n\
         - Do NOT attempt [{blocked_list}] again this turn unless the worktree or target actually changed.\n\
         - If you still cannot finish, explicitly summarize verified facts and remaining gaps instead of continuing exploratory retries.\n\n\
         Anti-hallucination: a blocked restricted-tool retry does NOT count as new evidence.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn redundant_reads_corrective_message(count: usize, original_query: &str) -> String {
    format!(
        "{REDUNDANT_READS_MARKER}\n\
         Runtime correction: you have re-read overlapping line ranges of the \
         same file {count} times this turn without any intervening edit. The \
         content has not changed — re-reading wastes tokens and stalls progress.\n\n\
         REQUIRED next-step behavior:\n\
         - Use the file content already in your context; do NOT issue another \
           read for any range you have already loaded.\n\
         - If you genuinely need a new section, use the `view` tool with \
           explicit `view_range` (NOT `bash sed`/`bash cat`) and only for ranges \
           you have not already seen.\n\
         - If you have enough information to answer, produce the final answer now.\n\
         - If you do not, state precisely what is still unknown and which ONE \
           specific new piece of evidence you need — do not loop on the same files.\n\n\
         Anti-hallucination: do NOT fabricate file contents you have not actually \
         observed. A partial-but-honest answer beats a confident-but-fabricated one.\n\n\
         Original user query: {original_query}"
    )
}

pub(crate) fn should_escalate_execution(state: &AgenticLoopState) -> bool {
    if state.stall.forced_execution_escalation {
        return false;
    }
    // One corrective injection per turn: if parallel-batching force already
    // fired, skip escalation to avoid double-injecting corrective messages.
    // NOTE: execution order in execute_turn_and_ingest_phase is
    //   escalation → parallel-batching, so in practice escalation runs first.
    //   This guard is defensive against future reordering.
    if state.stall.forced_parallel_batching {
        return false;
    }
    if !state.task_profile.mutates_workspace {
        return false;
    }
    if has_concrete_workspace_mutation(state) {
        return false;
    }

    let successful_real_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .filter(|record| record.ok)
        .collect();

    if successful_real_records.len() < EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD {
        return false;
    }

    // Every successful call was read-only (none mutating) and none committed
    // a workspace change — the model is spinning on inspection.
    successful_real_records
        .iter()
        .all(|record| !tool_record_is_workspace_mutation(record))
}

pub(crate) fn execution_escalation_message(original_query: &str, read_only_calls: usize) -> String {
    format!(
        "{EXECUTION_ESCALATION_MARKER}\n\
         Runtime correction: you have made {read_only_calls} read-only tool calls on a task that \
         clearly requires changing the workspace, and have not applied a single edit yet. \
         Stop reading more files. Your NEXT response must invoke an editing tool \
         (`apply_patch`, `edit_file`, `str_replace`, `write_file`, or a `bash` command that \
         actually modifies files such as `sed -i`, a redirect `>`/`>>`, or `apply_patch`). \
         Do not produce another round of `cat`/`grep`/`ls`/`git diff`/`find`. Do not ask the \
         user for permission. Apply the change, then run the appropriate existing verification.\n\n\
         Original user query: {original_query}"
    )
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

pub(crate) fn observe_turn_end_without_tools(
    state: &mut AgenticLoopState,
    _turn_index: usize,
    turn_start_time: Instant,
    ttft_ms: Option<u64>,
) {
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability_integration::TurnTiming {
            turn: session_turn_number(state),
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

async fn handle_token_budget<H: AgenticLoopHost>(
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
        record_early_exit_llm_round(
            state,
            turn_result,
            prep.turn_start_time,
            Some("token_budget_exceeded"),
        );
        observe_turn_end_without_tools(
            state,
            turn_index,
            prep.turn_start_time,
            turn_result.ttft_ms,
        );
        state.step_recorder.end_turn(false);
        finalize_and_render(host, state).await;
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
    // Record structured interruption for cumulative budget exhaustion.
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::CumulativeBudgetExceeded,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some(format!(
                "Cumulative token budget: {cumulative}/{} tokens",
                state.max_cumulative_tokens,
            )),
        ),
    ));
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

        // Emit journal event for actionable (low/very-low) confidence diagnoses.
        if let Some(ref diag) = state.last_confidence_diagnosis {
            if diag.is_actionable() {
                if let Some(ref sid) = state.current_session_id {
                    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
                        let evt = astra_services::session_journal::JournalEvent::confidence_diagnosis_recorded(
                            Some(sid),
                            turn_index as u32,
                            conf,
                            diag.to_json(),
                        );
                        let _ = writer.append(&evt);
                    }
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use astra_services::session_journal::ToolCallRecord;

    use super::*;
    use crate::observability_integration::ObservabilityHub;
    use crate::turn::agentic_loop_host::tests::make_state;

    #[test]
    fn observe_turn_end_without_tools_records_outer_session_turn() {
        let mut state = make_state();
        state.session_turn = 6;
        state.max_turns = 20;
        state.remaining_turns = 4;
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_hub = Some(Arc::new(hub));
        state.telemetry.observability_session = Some(session.clone());

        let turn_start_time = Instant::now() - Duration::from_millis(25);
        observe_turn_end_without_tools(&mut state, 16, turn_start_time, Some(7));

        let guard = session.read().unwrap();
        assert_eq!(guard.turn_timings.len(), 1);
        assert_eq!(guard.turn_timings[0].turn, 6);
    }

    #[test]
    fn execution_retry_correction_is_detectable_and_stripped() {
        let msg = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("fix the bug", ExecutionRetryReason::MissingMutation),
        });
        assert!(is_execution_retry_correction(&msg));

        let unrelated = serde_json::json!({
            "role": "user",
            "content": "fix the bug",
        });
        assert!(!is_execution_retry_correction(&unrelated));

        let assistant_with_marker = serde_json::json!({
            "role": "assistant",
            "content": EXECUTION_RETRY_MARKER,
        });
        assert!(!is_execution_retry_correction(&assistant_with_marker));
    }

    #[test]
    fn execution_retry_blocks_plan_only_finish_for_mutating_task() {
        let mut state = make_state();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("修复这个问题");
        state.message = "修复这个问题".into();
        state.final_text = "需要我直接执行这些修改吗？".into();
        state.total_tool_calls = 2;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"grep -rn \"foo\" rust/"}"#.into()),
            ..Default::default()
        });

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_skips_done_without_tools_for_mutating_task() {
        // A mutating-profile task where the model produces a bare conclusion
        // with zero tool calls is treated as a legitimate no-op completion
        // (e.g. "I reviewed the code and the bug doesn't exist"). Forcing a
        // retry here would just waste a turn.
        let mut state = make_state();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "I reviewed the code and the bug does not exist.".into();

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_fires_when_mutating_task_only_planned() {
        // Mutating profile + tool calls were made (exploration) but nothing
        // committed → still retry to push for execution.
        let mut state = make_state();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "Here is the plan: change foo to bar.".into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cat src/foo.rs"}"#.into()),
            ..Default::default()
        });

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_skips_reviewed_no_bug_conclusion_after_read_only_inspection() {
        let mut state = make_state();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "I reviewed the code path and the bug does not exist.".into();
        state.total_tool_calls = 2;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"rg -n \"buggy_path\" rust/"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "view".into(),
            ok: true,
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn confirmation_detector_ignores_keyi_in_descriptive_sentence() {
        // "可以看到这里有问题" is description, not a confirmation.
        assert!(!looks_like_execution_confirmation("可以看到这里有问题"));
    }

    #[test]
    fn bash_mutation_detects_compound_and_sudo_commands() {
        use crate::turn::agentic_loop_lifecycle::tool_record_is_workspace_mutation;
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cd /tmp && mv a b"}"#.into()),
            ..Default::default()
        };
        assert!(tool_record_is_workspace_mutation(&record));

        let sudo = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sudo rm -rf /tmp/cache"}"#.into()),
            ..Default::default()
        };
        assert!(tool_record_is_workspace_mutation(&sudo));
    }

    #[test]
    fn bash_mutation_returns_false_for_malformed_args() {
        use crate::turn::agentic_loop_lifecycle::tool_record_is_workspace_mutation;
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some("rm -rf /".into()),
            ..Default::default()
        };
        // Non-JSON args are treated as missing rather than the raw string,
        // avoiding false positives from corrupted journal entries.
        assert!(!tool_record_is_workspace_mutation(&record));
    }

    #[test]
    fn execution_retry_recognizes_affirmative_followup_context() {
        let mut state = make_state();
        state.message = "当然了".into();
        state.final_text = "我可以继续执行，确认后开始。".into();
        state.total_tool_calls = 1;
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "需要我直接执行这些修改吗？"
        }));
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"cat rust/crates/runtime/src/lib.rs"}"#.into()),
            ..Default::default()
        });

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_recognizes_english_affirmative_followup_context() {
        let mut state = make_state();
        state.message = "go ahead".into();
        state.final_text = "I can apply the patch now.".into();
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "Should I apply this patch?"
        }));

        assert!(should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_does_not_treat_bare_affirmative_as_execution() {
        let mut state = make_state();
        state.message = "当然了".into();
        state.final_text = "好的。".into();
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "这个解释有帮助吗？"
        }));

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_does_not_fire_for_read_only_review() {
        let mut state = make_state();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("review local changes");
        state.message = "review local changes".into();
        state.final_text = "I found one issue.".into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"git diff --stat"}"#.into()),
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_does_not_fire_after_concrete_edit() {
        let mut state = make_state();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.message = "fix the bug".into();
        state.final_text = "Done.".into();
        state.total_tool_calls = 2;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn browser_verification_retry_fires_for_curl_only_success_claim() {
        let mut state = make_state();
        state.message = "Test the game in browser and tell me if it works.".into();
        state.final_text = "I tested it and it's fully functional.".into();
        state.total_tool_calls = 3;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m http.server 8000"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"curl --noproxy '*' http://127.0.0.1:8000"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"ps -ef | grep http.server"}"#.into()),
            ..Default::default()
        });

        assert_eq!(
            execution_retry_reason(&state),
            Some(ExecutionRetryReason::MissingBrowserVerification)
        );
    }

    #[test]
    fn browser_verification_retry_overrides_concrete_edit_short_circuit() {
        let mut state = make_state();
        state.task_profile = crate::turn::chat_turn_heuristics::infer_task_execution_profile(
            "fix the game bug and verify it in browser",
        );
        state.message = "fix the game bug and verify it in browser".into();
        state.final_text = "I fixed the bug and it's fully functional now.".into();
        state.total_tool_calls = 3;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m http.server 8000"}"#.into()),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"curl http://127.0.0.1:8000"}"#.into()),
            ..Default::default()
        });

        assert_eq!(
            execution_retry_reason(&state),
            Some(ExecutionRetryReason::MissingBrowserVerification)
        );
    }

    #[test]
    fn browser_verification_retry_skips_when_playwright_evidence_exists() {
        let mut state = make_state();
        state.message = "Test the game in browser and tell me if it works.".into();
        state.final_text = "I tested it and it's fully functional.".into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"npx playwright test tests/game.spec.ts"}"#.into()),
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn browser_verification_retry_skips_when_model_admits_not_verified() {
        let mut state = make_state();
        state.message = "Test the game in browser and tell me if it works.".into();
        state.final_text =
            "I could not verify this in a browser because no browser-capable tool is available."
                .into();
        state.total_tool_calls = 1;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m http.server 8000"}"#.into()),
            ..Default::default()
        });

        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn execution_retry_suppressed_when_round_budget_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_round_budget_phase1 = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_redundant_reads_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_redundant_reads_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_exploration_family_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_exploration_family_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn execution_retry_suppressed_when_cache_waste_corrective_already_fired() {
        let mut state = make_state();
        state.message = "implement the feature".into();
        state.final_text = "I'll implement that for you.".into();
        state.total_tool_calls = 0;
        state.task_profile.mutates_workspace = true;
        state.stall.forced_cache_waste_corrective = true;
        assert_eq!(execution_retry_reason(&state), None);
    }

    #[test]
    fn bash_read_only_is_not_workspace_mutation_but_sed_i_is() {
        let read_only = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sed -n '1,20p' src/lib.rs"}"#.into()),
            ..Default::default()
        };
        let mutating = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sed -i 's/old/new/' src/lib.rs"}"#.into()),
            ..Default::default()
        };

        assert!(!tool_record_is_workspace_mutation(&read_only));
        assert!(tool_record_is_workspace_mutation(&mutating));
    }

    // ─── Mid-loop execution escalation tests ──────────────────────────────

    fn make_mutating_state_with_reads(n: usize) -> AgenticLoopState {
        let mut state = make_state();
        state.message = "fix the bug in foo".into();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug in foo");
        assert!(
            state.task_profile.mutates_workspace,
            "test precondition: profile must be mutating"
        );
        for i in 0..n {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "bash".into(),
                ok: true,
                args_full: Some(format!(r#"{{"command":"cat src/file{i}.rs"}}"#, i = i)),
                ..Default::default()
            });
        }
        state
    }

    #[test]
    fn escalation_fires_after_threshold_of_read_only_calls_on_mutating_task() {
        let state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        assert!(should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_just_below_threshold() {
        let state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD - 1);
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_on_non_mutating_task() {
        let mut state =
            make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD + 2);
        // Flip profile to read-only exploration — escalation must not engage.
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("review the diff");
        assert!(!state.task_profile.mutates_workspace);
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_when_any_mutation_present() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        // One actual edit in the middle of many reads must suppress the guard.
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "edit_file".into(),
            ok: true,
            ..Default::default()
        });
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_does_not_fire_when_bash_mutation_mixed_in() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"sed -i 's/a/b/' foo.rs"}"#.into()),
            ..Default::default()
        });
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_is_one_shot_per_turn() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        state.stall.forced_execution_escalation = true;
        assert!(
            !should_escalate_execution(&state),
            "flag must prevent a second injection"
        );
    }

    #[test]
    fn escalation_suppressed_when_parallel_batching_already_fired() {
        let mut state = make_mutating_state_with_reads(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD);
        // Precondition: without the flag, escalation would fire.
        assert!(should_escalate_execution(&state));
        // Once parallel-batching force has fired, escalation must yield to
        // honor the one-corrective-per-turn invariant.
        state.stall.forced_parallel_batching = true;
        assert!(
            !should_escalate_execution(&state),
            "escalation must not fire when parallel-batching force already active"
        );
    }

    #[test]
    fn escalation_ignores_failed_tool_calls_for_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        // 20 failed reads — don't count toward threshold (they weren't real
        // progress; retrying reads is already flagged elsewhere).
        for _ in 0..20 {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "bash".into(),
                ok: false,
                args_full: Some(r#"{"command":"cat missing.rs"}"#.into()),
                ..Default::default()
            });
        }
        assert!(!should_escalate_execution(&state));
    }

    #[test]
    fn escalation_ignores_synthetic_placeholders() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        for _ in 0..(EXECUTION_ESCALATION_TOOL_CALL_THRESHOLD + 2) {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "bash".into(),
                ok: true,
                args_preview: Some("<synthetic placeholder>".into()),
                ..Default::default()
            });
        }
        // If all records are synthetic placeholders they should be filtered
        // out and the threshold should not be met.
        let all_synthetic = state
            .stall
            .tool_call_records
            .iter()
            .all(|r| r.is_synthetic_placeholder());
        if all_synthetic {
            assert!(!should_escalate_execution(&state));
        }
    }

    #[test]
    fn retry_guard_yields_to_prior_escalation_in_same_turn() {
        // If escalation already fired mid-loop, retry must NOT also fire at
        // BreakLoop — one corrective injection per turn.
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.final_text = "I will proceed with the edits now.".into();
        state.total_tool_calls = 10;

        // Sanity: without the escalation flag this state would trigger retry.
        state.stall.forced_execution_escalation = false;
        assert!(should_force_execution_retry(&state));

        // With the escalation flag set, retry must yield.
        state.stall.forced_execution_escalation = true;
        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn parallel_batching_force_blocks_subsequent_retry_in_same_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        state.final_text = "I'll continue investigating.".into();
        state.total_tool_calls = 10;
        // Without the parallel-batching flag, this state would trigger retry.
        assert!(should_force_execution_retry(&state));
        // Once the parallel-batching force fired, retry must yield to honor
        // the one-corrective-per-turn invariant.
        state.stall.forced_parallel_batching = true;
        assert!(!should_force_execution_retry(&state));
    }

    #[test]
    fn parallel_batching_suppressed_when_escalation_already_fired() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        // Precondition: without escalation flag, parallel-batching would fire.
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // Once escalation has fired, parallel-batching must yield.
        state.stall.forced_execution_escalation = true;
        assert!(
            !should_force_parallel_batching(&state, PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD),
            "parallel-batching must not fire when escalation already active"
        );
    }

    #[test]
    fn parallel_batching_suppressed_when_retry_already_fired() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        state.stall.forced_execution_retry = true;
        assert!(
            !should_force_parallel_batching(&state, PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD),
            "parallel-batching must not fire when retry already active"
        );
    }

    #[test]
    fn parallel_batching_suppressed_when_cascade_guard_already_fired() {
        let flags: Vec<Box<dyn Fn(&mut AgenticLoopState)>> = vec![
            Box::new(|s| s.stall.forced_round_budget_phase1 = true),
            Box::new(|s| s.stall.forced_redundant_reads_corrective = true),
            Box::new(|s| s.stall.forced_cache_waste_corrective = true),
            Box::new(|s| s.stall.forced_exploration_family_corrective = true),
            Box::new(|s| s.stall.forced_exploration_family_phase2 = true),
        ];
        for set_flag in &flags {
            let mut state = make_state();
            state.message = "explore the codebase".into();
            for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
                push_single_tool_round(&mut state);
            }
            // Precondition: would fire without the flag.
            assert!(should_force_parallel_batching(
                &state,
                PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
            ));
            set_flag(&mut state);
            assert!(
                !should_force_parallel_batching(&state, PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD),
                "parallel-batching must not fire when a cascade guard already active"
            );
        }
    }

    #[test]
    fn escalation_marker_detected_and_stripped_by_corrective_filter() {
        let msg = serde_json::json!({
            "role": "user",
            "content": execution_escalation_message("fix the bug", 9),
        });
        assert!(is_execution_escalation(&msg));
        assert!(is_execution_corrective_message(&msg));

        let retry = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("fix the bug", ExecutionRetryReason::MissingMutation),
        });
        assert!(is_execution_corrective_message(&retry));

        let unrelated = serde_json::json!({"role":"user","content":"fix the bug"});
        assert!(!is_execution_corrective_message(&unrelated));
    }

    // ─── Parallel-batching force (third-tier guard) ─────────────────────

    fn push_single_tool_round(state: &mut AgenticLoopState) {
        state
            .messages
            .push(serde_json::json!({"role": "assistant", "tool_calls": []}));
        state
            .messages
            .push(serde_json::json!({"role": "tool", "content": "..."}));
    }

    #[test]
    fn parallel_batching_force_fires_at_streak_threshold() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_below_threshold() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD - 1) {
            push_single_tool_round(&mut state);
        }
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_when_last_round_batched() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        // Long single-tool history that crossed threshold...
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 2) {
            push_single_tool_round(&mut state);
        }
        // ...but the most-recent round used 3 parallel tools — the model
        // already self-corrected, no force needed.
        state
            .messages
            .push(serde_json::json!({"role": "assistant", "tool_calls": []}));
        for _ in 0..3 {
            state
                .messages
                .push(serde_json::json!({"role": "tool", "content": "..."}));
        }
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 3) {
            push_single_tool_round(&mut state);
        }
        // First time would fire...
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // ...but once the flag is set, a second attempt is suppressed even
        // if the model produces yet another single-tool round.
        state.stall.forced_parallel_batching = true;
        push_single_tool_round(&mut state);
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_marker_recognized_by_corrective_filter() {
        let msg = serde_json::json!({
            "role": "user",
            "content": parallel_batching_force_message(7, "do something"),
        });
        assert!(is_parallel_batching_force(&msg));
        assert!(is_execution_corrective_message(&msg));
        // Other corrective markers must not be misclassified as this one.
        let retry = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("do something", ExecutionRetryReason::MissingMutation),
        });
        assert!(!is_parallel_batching_force(&retry));
    }

    #[test]
    fn parallel_batching_force_uses_tighter_threshold_in_round_budget_warning_zone() {
        // Streak of 3 — below the early-zone threshold of 5...
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD_LATE {
            push_single_tool_round(&mut state);
        }
        // ...so before the warning zone, this must NOT fire.
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_THRESHOLD - 1;
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));

        // Once round_index crosses ROUND_BUDGET_THRESHOLD, the same streak of
        // 3 must fire — this is the coupling we want.
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_THRESHOLD;
        assert!(should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_late_threshold_silent_at_streak_two() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD_LATE - 1) {
            push_single_tool_round(&mut state);
        }
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_THRESHOLD + 2;
        // Even deep into the warning zone, a streak below the late threshold
        // (=3) must not fire — we don't punish a single isolated single-tool
        // round just because the turn is long.
        assert!(!should_force_parallel_batching(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    // ─── Round-budget convergence guard (two-phase) ──────────────────────

    #[test]
    fn round_budget_phase1_fires_at_or_above_hard_limit() {
        let mut state = make_state();
        state.message = "investigate complex bug".into();

        // Just below the limit: silent.
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_HARD_LIMIT - 1;
        assert!(!should_inject_round_budget_phase1(
            &state,
            crate::prompts::ROUND_BUDGET_HARD_LIMIT
        ));

        // At the hard limit: phase-1 fires.
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_HARD_LIMIT;
        assert!(should_inject_round_budget_phase1(
            &state,
            crate::prompts::ROUND_BUDGET_HARD_LIMIT
        ));

        // Above the limit: still fires (until one-shot flag prevents it).
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_HARD_LIMIT + 5;
        assert!(should_inject_round_budget_phase1(
            &state,
            crate::prompts::ROUND_BUDGET_HARD_LIMIT
        ));
    }

    #[test]
    fn round_budget_phase1_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "investigate".into();
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_HARD_LIMIT + 2;
        assert!(should_inject_round_budget_phase1(
            &state,
            crate::prompts::ROUND_BUDGET_HARD_LIMIT
        ));
        // Once flag is set, a second injection is suppressed even if the
        // round count has grown further.
        state.stall.forced_round_budget_phase1 = true;
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_HARD_LIMIT + 10;
        assert!(!should_inject_round_budget_phase1(
            &state,
            crate::prompts::ROUND_BUDGET_HARD_LIMIT
        ));
    }

    #[test]
    fn round_budget_phase1_suppressed_when_other_corrective_already_fired() {
        let hard_limit = crate::prompts::ROUND_BUDGET_HARD_LIMIT;

        // Escalation suppresses phase-1.
        let mut state = make_state();
        state.llm_rounds_completed = hard_limit;
        state.stall.forced_execution_escalation = true;
        assert!(
            !should_inject_round_budget_phase1(&state, hard_limit),
            "phase-1 must not fire when escalation already active"
        );

        // Parallel-batching suppresses phase-1.
        let mut state = make_state();
        state.llm_rounds_completed = hard_limit;
        state.stall.forced_parallel_batching = true;
        assert!(
            !should_inject_round_budget_phase1(&state, hard_limit),
            "phase-1 must not fire when parallel-batching already active"
        );

        // Redundant-reads suppresses phase-1.
        let mut state = make_state();
        state.llm_rounds_completed = hard_limit;
        state.stall.forced_redundant_reads_corrective = true;
        assert!(
            !should_inject_round_budget_phase1(&state, hard_limit),
            "phase-1 must not fire when redundant-reads already active"
        );
    }

    #[test]
    fn round_budget_phase1_marker_recognized_by_corrective_filter() {
        let msg = serde_json::json!({
            "role": "user",
            "content": round_budget_phase1_message(15, "investigate"),
        });
        assert!(is_round_budget_phase1(&msg));
        assert!(is_execution_corrective_message(&msg));
        // Anti-hallucination wording must be present — this is the whole
        // reason phase-1 exists, not just round budgeting.
        let body = msg.get("content").and_then(|c| c.as_str()).unwrap();
        assert!(
            body.contains("Do NOT fabricate"),
            "phase-1 corrective must include explicit anti-hallucination directive; got: {body}"
        );
        assert!(
            body.contains("could NOT verify") || body.contains("not verify"),
            "phase-1 corrective must instruct enumerating gaps"
        );
    }

    #[test]
    fn round_budget_phase2_fires_only_after_phase1_with_subsequent_tool_calls() {
        let mut state = make_state();
        let hard_limit = crate::prompts::ROUND_BUDGET_HARD_LIMIT;
        // Set rounds completed to meet the hard_limit so the sanity guard
        // passes — phase-2 only makes sense when the budget is genuinely
        // exhausted.
        state.llm_rounds_completed = hard_limit;

        // Without phase-1 set, phase-2 must not abort even if last round had
        // tool calls — the model never received the corrective.
        state.stall.forced_round_budget_phase1 = false;
        assert!(!should_abort_for_round_budget_phase2(&state, hard_limit));

        // With phase-1 set: phase-2 fires because the model continued past
        // the hard limit after receiving the corrective.
        state.stall.forced_round_budget_phase1 = true;
        assert!(should_abort_for_round_budget_phase2(&state, hard_limit));

        // Sanity guard: even with phase-1 set, if llm_rounds_completed is
        // below the hard limit, phase-2 must NOT fire.
        state.llm_rounds_completed = hard_limit - 1;
        assert!(!should_abort_for_round_budget_phase2(&state, hard_limit));
    }

    #[test]
    fn round_budget_phase2_is_one_shot_per_turn() {
        let mut state = make_state();
        let hard_limit = crate::prompts::ROUND_BUDGET_HARD_LIMIT;
        state.llm_rounds_completed = hard_limit;
        state.stall.forced_round_budget_phase1 = true;
        state.stall.forced_round_budget_phase2 = true;
        // Even with phase-1 set, once phase-2 has already fired we must not
        // re-trigger.
        assert!(!should_abort_for_round_budget_phase2(&state, hard_limit));
    }

    fn push_redundant_sed_read(state: &mut AgenticLoopState, round: u32) {
        // Same file, same range, no intervening mutation — counts as one
        // overlap each call after the first.
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: Some("sed -n '159,200p' f.rs".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: None,
            parallel: Some(false),
            round: Some(round),
            args_full: Some("sed -n '159,200p' f.rs".into()),
            ..Default::default()
        });
    }

    #[test]
    fn redundant_reads_corrective_fires_at_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        // First read seeds the file's history; subsequent overlapping reads
        // each contribute one redundant event.
        for r in 0..(REDUNDANT_READS_MIDLOOP_THRESHOLD + 1) {
            push_redundant_sed_read(&mut state, r as u32);
        }
        assert!(should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn redundant_reads_corrective_silent_below_threshold() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        // Threshold-many reads = (threshold-1) overlap events: stays silent.
        for r in 0..REDUNDANT_READS_MIDLOOP_THRESHOLD {
            push_redundant_sed_read(&mut state, r as u32);
        }
        assert!(!should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn redundant_reads_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "fix the bug".into();
        for r in 0..(REDUNDANT_READS_MIDLOOP_THRESHOLD + 5) {
            push_redundant_sed_read(&mut state, r as u32);
        }
        // First check fires...
        assert!(should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
        // ...then the one-shot flag gates the next attempt.
        state.stall.forced_redundant_reads_corrective = true;
        push_redundant_sed_read(&mut state, 99);
        assert!(!should_inject_redundant_reads_corrective(
            &state,
            REDUNDANT_READS_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn redundant_reads_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": redundant_reads_corrective_message(5, "fix the bug"),
        });
        assert!(is_redundant_reads_corrective(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_redundant_reads_corrective(&unrelated));
    }

    #[test]
    fn cache_waste_corrective_fires_at_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for _ in 0..CACHE_WASTE_MIDLOOP_THRESHOLD {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_corrective_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD - 1) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(!should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD + 2) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
        state.stall.forced_cache_waste_corrective = true;
        state.turn_guard.record_cache_hit("git_diff");
        assert!(!should_inject_cache_waste_corrective(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": cache_waste_corrective_message(&[("git_diff", 3)], "review local changes"),
        });
        assert!(is_cache_waste_corrective(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_cache_waste_corrective(&unrelated));
    }

    fn push_diff_round(state: &mut AgenticLoopState, round: u32) {
        for idx in 0..2 {
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "git_diff".into(),
                ok: true,
                round: Some(round),
                args_full: Some(format!(r#"{{"path":"src/file_{round}_{idx}.rs"}}"#)),
                ..Default::default()
            });
        }
    }

    fn push_search_round(state: &mut AgenticLoopState, round: u32) {
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "rg".into(),
            ok: true,
            round: Some(round),
            args_full: Some(format!(r#"{{"pattern":"needle_{round}","path":"rust/"}}"#)),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "glob".into(),
            ok: true,
            round: Some(round),
            args_full: Some(format!(r#"{{"pattern":"src/**/*_{round}.rs"}}"#)),
            ..Default::default()
        });
    }

    fn push_blocked_restricted_round(state: &mut AgenticLoopState, tool: &str, round: u32) {
        state.stall.tool_call_records.push(ToolCallRecord {
            name: tool.into(),
            ok: false,
            round: Some(round),
            error: Some(format!(
                "blocked_tool: Tool '{tool}' is currently restricted."
            )),
            result_preview: Some(format!(
                "Tool '{tool}' is currently restricted and cannot be executed."
            )),
            ..Default::default()
        });
    }

    #[test]
    fn exploration_family_corrective_fires_at_threshold_and_restricts_explicit_tools() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for round in 0..astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD {
            push_diff_round(&mut state, round as u32);
        }

        let Some((family, streak)) = exploration_family_corrective_candidate(
            &state,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
        ) else {
            panic!("expected exploration-family corrective candidate");
        };

        assert_eq!(family, "diff");
        assert_eq!(
            streak,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
        );

        let restricted = apply_exploration_family_restrictions(&mut state, &family);
        assert_eq!(restricted, vec!["git_diff".to_string()]);
        assert!(state.restricted_tools.contains("git_diff"));
        assert!(
            !state.restricted_tools.contains("bash"),
            "exploration-family corrective must not globally block bash"
        );
    }

    #[test]
    fn exploration_family_corrective_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for round in 0..(astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD - 1) {
            push_diff_round(&mut state, round as u32);
        }

        assert!(
            exploration_family_corrective_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_none()
        );
    }

    #[test]
    fn exploration_family_corrective_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        for round in 0..(astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD + 2) {
            push_diff_round(&mut state, round as u32);
        }

        assert!(
            exploration_family_corrective_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_some()
        );

        state.stall.forced_exploration_family_corrective = true;
        assert!(
            exploration_family_corrective_candidate(
                &state,
                astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
            )
            .is_none()
        );
    }

    #[test]
    fn exploration_family_corrective_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": exploration_family_corrective_message(
                "diff",
                3,
                &["git_diff".to_string()],
                "review local changes",
            ),
        });
        assert!(is_exploration_family_corrective(&msg));
        assert!(is_execution_corrective_message(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_exploration_family_corrective(&unrelated));
    }

    #[test]
    fn exploration_family_phase2_fires_after_blocked_only_retry_round() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.stall.forced_exploration_family_corrective = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        push_blocked_restricted_round(&mut state, "git_diff", 7);

        let candidate = exploration_family_phase2_candidate(&state);
        assert_eq!(
            candidate,
            Some(("diff".to_string(), vec!["git_diff".to_string()])),
        );
    }

    #[test]
    fn exploration_family_phase2_stays_silent_on_mixed_progress_round() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.stall.forced_exploration_family_corrective = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        push_blocked_restricted_round(&mut state, "git_diff", 7);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            round: Some(7),
            args_full: Some(r#"{"command":"cargo test -p astra-runtime"}"#.into()),
            ..Default::default()
        });

        assert!(exploration_family_phase2_candidate(&state).is_none());
    }

    #[test]
    fn exploration_family_phase2_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.stall.forced_exploration_family_corrective = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        push_blocked_restricted_round(&mut state, "git_diff", 7);

        assert!(exploration_family_phase2_candidate(&state).is_some());
        state.stall.forced_exploration_family_phase2 = true;
        assert!(exploration_family_phase2_candidate(&state).is_none());
    }

    #[test]
    fn exploration_family_phase2_marker_recognized() {
        let msg = serde_json::json!({
            "role": "user",
            "content": exploration_family_phase2_message(
                "diff",
                &["git_diff".to_string()],
                "review local changes",
            ),
        });
        assert!(is_exploration_family_phase2(&msg));
        assert!(is_execution_corrective_message(&msg));
        let unrelated = serde_json::json!({"role": "user", "content": "hello"});
        assert!(!is_exploration_family_phase2(&unrelated));
    }

    #[test]
    fn exploration_family_corrective_restricts_search_tools_without_bash() {
        let mut state = make_state();
        state.message = "investigate auth flow".into();
        for round in 0..astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD {
            push_search_round(&mut state, round as u32);
        }

        let Some((family, streak)) = exploration_family_corrective_candidate(
            &state,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD,
        ) else {
            panic!("expected exploration-family search corrective candidate");
        };

        assert_eq!(family, "search");
        assert_eq!(
            streak,
            astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
        );

        let restricted = apply_exploration_family_restrictions(&mut state, &family);
        assert_eq!(
            restricted,
            vec!["glob".to_string(), "grep".to_string(), "rg".to_string()]
        );
        assert!(state.restricted_tools.contains("glob"));
        assert!(state.restricted_tools.contains("grep"));
        assert!(state.restricted_tools.contains("rg"));
        assert!(
            !state.restricted_tools.contains("bash"),
            "search-family corrective must not globally block bash"
        );
    }
}
