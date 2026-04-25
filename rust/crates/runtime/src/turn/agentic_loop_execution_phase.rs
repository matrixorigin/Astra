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
        if !prep.quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Yellow,
                format!(
                    "↻ Mutating task accumulated {read_only_calls} read-only tool calls with zero edits; forcing escalation…"
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
            if should_force_execution_retry(state) {
                state.stall.forced_execution_retry = true;
                state.final_text.clear();
                // The corrective user message is pushed onto `state.messages`
                // for this loop iteration. The one-shot
                // `forced_execution_retry` flag prevents a second injection,
                // and `finalize_and_render` strips the marker before the next
                // user turn so it does not pollute future conversations.
                state.messages.push(serde_json::json!({
                    "role": "user",
                    "content": execution_retry_message(&state.message),
                }));
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        "↻ Execution requested but no edits were applied; forcing corrective retry…"
                            .to_string(),
                    );
                }
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
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            // Record the LLM round even for text-only responses (no tool calls).
            // Without this, simple Q&A turns have llm_rounds=0 in the
            // journal despite the LLM being called.
            record_early_exit_llm_round(state, &turn_result, prep.turn_start_time, Some("stop"));

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
            try_write_heavy_checkpoint(state);
            return Ok(TurnExecutionControl::ContinueLoop);
        }
        AgenticIngestIterationControl::ProceedWithToolCalls => {}
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

fn should_force_execution_retry(state: &AgenticLoopState) -> bool {
    if state.stall.forced_execution_retry {
        return false;
    }
    // If mid-loop escalation already fired this turn, the model has already
    // received a stronger corrective message telling it to apply an edit.
    // Adding a second retry injection would duplicate correction, waste
    // tokens, and risk contradicting guidance. One corrective injection per
    // turn is the invariant.
    if state.stall.forced_execution_escalation {
        return false;
    }
    if has_concrete_workspace_mutation(state) {
        return false;
    }
    if state.final_text.trim().is_empty() {
        return false;
    }
    let attempted_work_without_mutation = state.total_tool_calls > 0;
    let defers = final_text_defers_execution(&state.final_text);
    if state.task_profile.mutates_workspace {
        // Only retry when the model engaged with the task (made tool calls but
        // committed nothing) or explicitly deferred. A bare "Done." or "no fix
        // needed" reply with zero tool calls is treated as a legitimate no-op
        // — retrying would burn a turn for nothing.
        return attempted_work_without_mutation || defers;
    }
    user_confirmed_execution_from_recent_context(state)
        && (attempted_work_without_mutation || defers)
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

fn execution_retry_message(original_query: &str) -> String {
    format!(
        "{EXECUTION_RETRY_MARKER}\n\
         Runtime correction: the user requested or confirmed code execution, \
         but your previous response ended without applying any concrete workspace mutation. \
         Do not ask for permission again and do not only restate a plan. \
         Use the available file-editing tools to make the change, then run the appropriate existing verification.\n\n\
         Original user query: {original_query}"
    )
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
    is_execution_retry_correction(m) || is_execution_escalation(m)
}

pub(crate) fn should_escalate_execution(state: &AgenticLoopState) -> bool {
    if state.stall.forced_execution_escalation {
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
            "content": execution_retry_message("fix the bug"),
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
    fn escalation_marker_detected_and_stripped_by_corrective_filter() {
        let msg = serde_json::json!({
            "role": "user",
            "content": execution_escalation_message("fix the bug", 9),
        });
        assert!(is_execution_escalation(&msg));
        assert!(is_execution_corrective_message(&msg));

        let retry = serde_json::json!({
            "role": "user",
            "content": execution_retry_message("fix the bug"),
        });
        assert!(is_execution_corrective_message(&retry));

        let unrelated = serde_json::json!({"role":"user","content":"fix the bug"});
        assert!(!is_execution_corrective_message(&unrelated));
    }
}
