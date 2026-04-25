use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::agentic_adaptive_tuning::apply_adaptive_execution_profile;
use super::agentic_headless_round::HeadlessStderrStyle;
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, delegate_tool_schema,
    try_write_heavy_checkpoint,
};
use super::interruption::{
    InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
};
use super::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;
use crate::orchestration::permission_sync::PermissionResponseMessaging;

#[derive(Clone, Copy)]
pub(crate) struct TurnIterationPrep {
    pub(crate) quiet: bool,
    pub(crate) turn_start_time: Instant,
}

pub(crate) enum PreparedTurnIteration {
    Ready(TurnIterationPrep),
    Finished(AgenticLoopOutcome),
}

fn should_complete_budget_exhaustion_gracefully(state: &AgenticLoopState) -> bool {
    state.total_tool_calls > 0
        || state.total_prompt > 0
        || state.total_completion > 0
        || state.stall.last_heavy_checkpoint.is_some()
}

pub(crate) fn session_turn_number(state: &AgenticLoopState) -> u32 {
    if state.session_turn > 0 {
        state.session_turn
    } else {
        state.max_turns.saturating_sub(state.remaining_turns).max(1) as u32
    }
}

pub(crate) fn current_agentic_step(state: &AgenticLoopState) -> u32 {
    state.max_turns.saturating_sub(state.remaining_turns) as u32
}

pub(crate) fn completed_tool_calls(state: &AgenticLoopState) -> u32 {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .count()
        .min(u32::MAX as usize) as u32
}

fn budget_exhaustion_completion_text(state: &AgenticLoopState) -> String {
    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    let completed_tool_calls = completed_tool_calls(state);
    if completed_tool_calls > 0 {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). {} completed tool call(s) are preserved above.{}]\n",
            state.max_turns, completed_tool_calls, checkpoint_note
        )
    } else {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). Partial progress is preserved.{}]\n",
            state.max_turns, checkpoint_note
        )
    }
}

fn used_budget_extensions(state: &AgenticLoopState) -> u32 {
    let budget = state.agentic_turn_budget;
    if budget.extension_turns == 0 || state.max_turns <= budget.initial_turns {
        return 0;
    }
    state
        .max_turns
        .saturating_sub(budget.initial_turns)
        .div_ceil(budget.extension_turns)
        .min(u32::MAX as usize) as u32
}

fn bash_command_looks_mutating(command: &str) -> bool {
    crate::bash_intent::bash_command_looks_mutating(command)
}

fn extract_bash_command(args: Option<&str>) -> Option<String> {
    let args = args?;
    let value = serde_json::from_str::<Value>(args).ok()?;
    let command = value.get("command").and_then(Value::as_str)?;
    Some(command.to_string())
}

pub(crate) fn tool_record_is_workspace_mutation(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    match record.name.as_str() {
        "write_file" | "edit_file" | "apply_patch" | "create_file" | "delete_file"
        | "rename_file" | "move_file" | "replace_text" | "insert_text" | "append_file"
        | "str_replace" => true,
        "bash" => extract_bash_command(record.args_full.as_deref())
            .or_else(|| extract_bash_command(record.args_preview.as_deref()))
            .is_some_and(|command| bash_command_looks_mutating(&command)),
        _ => false,
    }
}

fn recent_turns_are_repetitive(state: &AgenticLoopState) -> bool {
    let Some(last) = state.stall.turn_sigs.last() else {
        return false;
    };
    if last.is_empty() {
        return false;
    }
    state
        .stall
        .turn_sigs
        .iter()
        .rev()
        .nth(1)
        .is_some_and(|previous| previous == last)
}

fn recent_progress_is_real(state: &AgenticLoopState) -> bool {
    // Note: `tool_record_is_workspace_mutation` no longer treats every `bash`
    // call as mutating — only commands that actually modify state qualify.
    // This is intentional: a loop that only runs `grep`/`cat`/`ls` should not
    // earn budget extensions, since "spinning on read-only inspection without
    // committing changes" is exactly the failure mode we are trying to break.
    let recent_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .rev()
        .filter(|record| !record.is_synthetic_placeholder())
        .take(8)
        .collect();
    if recent_records.is_empty() {
        return false;
    }

    let successful_recent = recent_records.iter().any(|record| record.ok);
    if !successful_recent {
        return false;
    }

    let mutating_progress = recent_records
        .iter()
        .any(|record| record.ok && tool_record_is_workspace_mutation(record));
    let distinct_recent_turns = state
        .stall
        .turn_sigs
        .iter()
        .rev()
        .take(3)
        .filter(|sig| !sig.is_empty())
        .fold(
            Vec::<&std::collections::BTreeSet<String>>::new(),
            |mut acc, sig| {
                if !acc.contains(&sig) {
                    acc.push(sig);
                }
                acc
            },
        )
        .len();

    if recent_turns_are_repetitive(state) {
        return false;
    }

    if state.task_profile.mutates_workspace {
        return mutating_progress;
    }

    if state.task_profile.exploratory_task
        || state.task_profile.complexity
            == astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex
    {
        return mutating_progress || distinct_recent_turns >= 2;
    }

    false
}

fn maybe_extend_turn_budget(state: &mut AgenticLoopState) -> Option<String> {
    let budget = state.agentic_turn_budget;
    if budget.extension_turns == 0
        || budget.max_extensions == 0
        || state.max_turns >= budget.hard_turn_limit
        || used_budget_extensions(state) >= budget.max_extensions
        || crate::server::run_lifecycle::has_turn_verdict_warning(&state.stall.verdict_events)
        || !recent_progress_is_real(state)
    {
        return None;
    }

    let additional_turns = budget
        .extension_turns
        .min(budget.hard_turn_limit.saturating_sub(state.max_turns));
    if additional_turns == 0 {
        return None;
    }

    state.max_turns += additional_turns;
    state.remaining_turns += additional_turns;
    let review_message = format!(
        "[Budget review] Recent progress looks real for this {}task, so continuing with {} extra turn(s). Hard limit: {} total turns.",
        if state.task_profile.exploratory_task {
            "exploratory "
        } else if state.task_profile.mutates_workspace {
            "implementation "
        } else {
            ""
        },
        additional_turns,
        budget.hard_turn_limit,
    );
    state.messages.push(serde_json::json!({
        "role": "system",
        "content": review_message,
    }));
    Some(review_message)
}

/// Build an interruption state summary from the current loop state.
pub(crate) fn interruption_state_summary(
    state: &AgenticLoopState,
    error_detail: Option<String>,
) -> InterruptionStateSummary {
    InterruptionStateSummary {
        has_checkpoint: state.stall.last_heavy_checkpoint.is_some(),
        tool_calls_completed: completed_tool_calls(state),
        turns_completed: current_agentic_step(state),
        remaining_turns: state.remaining_turns as u32,
        error_detail,
    }
}

pub(crate) async fn run_loop_preamble<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    if state
        .skills
        .session_event_hooks
        .has_event(crate::skills::hooks::SessionEvent::SessionStart)
    {
        let session_id = state.current_session_id.as_deref().unwrap_or("");
        let user_msg = state.message.as_str();
        let hook_output = crate::skills::hooks::evaluate_session_hooks(
            &state.skills.session_event_hooks,
            crate::skills::hooks::SessionEvent::SessionStart,
            session_id,
            Some(user_msg),
        )
        .await;
        if let Some(ctx) = hook_output.context {
            state.messages.insert(
                0,
                serde_json::json!({
                    "role": "system",
                    "content": format!("[Session hooks]\n{ctx}"),
                }),
            );
        }
        for (key, value) in hook_output.env_vars {
            astra_core::session_env_overlay::set(&key, &value);
        }
    }

    if state.delegation_engine.is_some() {
        host.inject_tool_schema(delegate_tool_schema());
    }

    if state.messaging.mailbox.is_some() {
        host.inject_tool_schema(crate::messaging::send_tool::send_message_tool_schema());
    }

    if let Some(resolver) = &state.skills.resolver {
        let full = resolver.available_skills();
        if !full.is_empty() {
            let (visible, open_skill_name) = crate::turn::skill_tool::visible_skills_for_host_turn(
                &full,
                state.message.as_str(),
                &state.skills.quality_tracker,
                &state.skills.pinned,
                &state.skills.discovered,
                &state.skills.invoked,
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

    if let Some(ref ctx) = state.project_context {
        state.messages.push(serde_json::json!({
            "role": "system",
            "content": format!(
                "## Cross-Session Project Context\n\
                 Below are summaries of recent sessions in this project. \
                 Use them for continuity — avoid re-asking questions already answered.\n\n{ctx}"
            )
        }));
    }

    if let Some(ref evo) = state.evolution_service {
        let turn_id = state.current_run_id.as_deref().unwrap_or("unknown");
        let prior_assistant = state
            .messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .and_then(|m| m.get("content").and_then(|c| c.as_str()))
            .map(String::from);
        let active_skill: Option<String> = state
            .skills
            .invoked
            .iter()
            .max_by_key(|(_, v)| v.invoked_at_turn)
            .map(|(name, _)| name.clone());
        evo.on_user_message(
            &state.message,
            prior_assistant.as_deref(),
            active_skill.as_deref(),
            turn_id,
        )
        .await;
    }
}

pub(crate) async fn prepare_turn_iteration<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
) -> Result<PreparedTurnIteration, String> {
    let quiet = host.is_quiet();

    while state
        .cancellation
        .pause_flag
        .as_ref()
        .is_some_and(|f| f.load(Ordering::Acquire))
    {
        if state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
            || state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled())
        {
            try_write_heavy_checkpoint(state);
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::UserCancelled,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
            return Ok(PreparedTurnIteration::Finished(
                AgenticLoopOutcome::Cancelled,
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    if state
        .cancellation
        .flag
        .as_ref()
        .is_some_and(|f| f.load(Ordering::Acquire))
        || state
            .cancellation
            .token
            .as_ref()
            .is_some_and(|t| t.is_cancelled())
    {
        try_write_heavy_checkpoint(state);
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::UserCancelled,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(state, None),
        ));
        return Ok(PreparedTurnIteration::Finished(
            AgenticLoopOutcome::Cancelled,
        ));
    }

    if state.remaining_turns == 0 {
        if maybe_extend_turn_budget(state).is_some() {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "↻ Budget review — extended to {}/{} turns.",
                        state.max_turns, state.agentic_turn_budget.hard_turn_limit
                    ),
                );
            }
            state.final_text.clear();
            state.interruption = None;
        } else if should_complete_budget_exhaustion_gracefully(state) {
            try_write_heavy_checkpoint(state);
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::BudgetExhausted,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
            if state.final_text.trim().is_empty() {
                state.final_text = budget_exhaustion_completion_text(state);
                state.final_text_streamed = false;
            }
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "⚠ Turn budget exhausted — preserving progress and ending the turn.".into(),
                );
            }
            return Ok(PreparedTurnIteration::Finished(
                AgenticLoopOutcome::Completed,
            ));
        } else {
            return Err(format!(
                "{} (budget: {} turns)",
                CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG, state.max_turns
            ));
        }
    }

    match state.rate_limit_cooldown.check_request(false) {
        crate::bridge::RateLimitAction::Proceed => {}
        crate::bridge::RateLimitAction::WaitAndRetry { delay_ms } => {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "⏳ Rate limit cooldown — waiting {:.1}s before next turn…",
                        delay_ms as f64 / 1000.0,
                    ),
                );
            }
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        crate::bridge::RateLimitAction::UseFallback { .. } => {
            if !quiet {
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    "⏳ Rate limit cooldown — waiting 5s (no fallback model)…".into(),
                );
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        crate::bridge::RateLimitAction::Reject {
            reason,
            reset_in_ms,
        } => {
            let secs = reset_in_ms / 1000;
            if state.total_tool_calls > 0 {
                if !quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "⚠ Rate limit cooldown active ({}) — preserving {} tool call(s). Resets in {secs}s.",
                            reason.as_str(),
                            state.total_tool_calls,
                        ),
                    );
                }
                state.final_text = format!(
                    "[Rate limit cooldown active ({}). \
                     {} completed tool call(s) preserved. \
                     Cooldown resets in ~{secs}s — you can continue then.]\n",
                    reason.as_str(),
                    state.total_tool_calls,
                );
                state.final_text_streamed = false;
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::CooldownRejected,
                    ResumeAction::WaitAndRetry {
                        delay_seconds: reset_in_ms / 1000,
                    },
                    interruption_state_summary(
                        state,
                        Some(format!("Rate limit: {}", reason.as_str())),
                    ),
                ));
                return Ok(PreparedTurnIteration::Finished(
                    AgenticLoopOutcome::Completed,
                ));
            }
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::CooldownRejected,
                ResumeAction::WaitAndRetry {
                    delay_seconds: reset_in_ms / 1000,
                },
                interruption_state_summary(state, Some(format!("Rate limit: {}", reason.as_str()))),
            ));
            return Err(format!(
                "Rate limit cooldown active ({}). Resets in ~{secs}s. Please wait and retry.",
                reason.as_str(),
            ));
        }
    }

    state.remaining_turns = state.remaining_turns.saturating_sub(1);
    state.step_recorder.begin_turn(turn_index as u32);

    if let Some(ref mut adapter) = state.tactical_adapter {
        adapter.reset_turn();
    }
    if let Some(ref mut collector) = state.step_signal_collector {
        collector.reset(state.max_turn_input_tokens);
    }

    let turn_start_time = Instant::now();

    // Initialize turn event buffer for fine-grained observability (once per turn).
    if state.turn_event_buffer.is_none() {
        state.turn_event_buffer = Some(
            astra_services::session_journal::TurnEventBuffer::begin_turn(
                state.current_session_id.as_deref(),
                session_turn_number(state),
            ),
        );
    }

    if let (Some(hub), Some(session)) = (
        &state.telemetry.observability_hub,
        &state.telemetry.observability_session,
    ) {
        let session_id = state.current_session_id.as_deref().unwrap_or("");
        let user_id = {
            let s = session.read().unwrap_or_else(|e| e.into_inner());
            s.user_id.clone()
        };
        crate::observability_integration::on_turn_start(hub, session_id, &user_id, &state.message);
    }
    apply_adaptive_execution_profile(state);

    if state.telemetry.observability_session.is_some()
        && state.telemetry.turn_trace_collector.is_none()
    {
        let capture = std::env::var("MO_CAPTURE_TRACES")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);
        if capture {
            let turn_id = format!("turn-{}", turn_index);
            let session_id = state.current_session_id.clone().unwrap_or_default();
            state.telemetry.turn_trace_collector = Some(
                crate::turn::turn_trace_collector::TurnTraceCollector::new(turn_id, session_id),
            );
        }
    }

    if state.permission_handler.is_none()
        && let Some(ctx) = state.permission_context.clone()
    {
        state.permission_handler = Some(crate::orchestration::PermissionRequestHandler::new(ctx));
    }

    const MAX_MAILBOX_DRAIN_PER_TURN: usize = 64;
    if let Some(ref mut mailbox) = state.messaging.mailbox {
        let (pending, has_more) = mailbox.drain_bounded(MAX_MAILBOX_DRAIN_PER_TURN);
        if !pending.is_empty() {
            let mut parts = Vec::with_capacity(pending.len());
            for msg in &pending {
                let from_label = &msg.from.agent_id;

                match &msg.payload {
                    crate::messaging::types::MessagePayload::Ack { message_id } => {
                        if let Some(ref tracker) = state.messaging.ack_tracker {
                            tracker.acknowledge(message_id).await;
                        }
                        if let Some(ref metrics) = state.messaging.metrics {
                            metrics.acks_received.fetch_add(1, Ordering::Relaxed);
                        }
                        parts.push(format!(
                            "[{from_label} ack]: message {message_id} acknowledged"
                        ));
                        continue;
                    }
                    crate::messaging::types::MessagePayload::Nack { message_id, reason } => {
                        if let Some(ref tracker) = state.messaging.ack_tracker
                            && let Some(crate::messaging::ack_tracker::AckOutcome::Rejected {
                                message,
                                ..
                            }) = tracker.reject(message_id, reason.clone()).await
                        {
                            eprintln!(
                                "  ⚠ messaging: nack for message {}: {}",
                                message_id,
                                reason.as_deref().unwrap_or("no reason")
                            );
                            if let Some(ref dlq) = state.messaging.dead_letter_queue {
                                dlq.store(
                                    Arc::clone(&message),
                                    crate::messaging::dead_letter::DeadLetterReason::Rejected {
                                        reason: reason.clone(),
                                    },
                                    1,
                                )
                                .await;
                            }
                            if let Some(ref metrics) = state.messaging.metrics {
                                metrics.dead_letters.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        if let Some(ref metrics) = state.messaging.metrics {
                            metrics.nacks_received.fetch_add(1, Ordering::Relaxed);
                        }
                        let r = reason.as_deref().unwrap_or("no reason");
                        parts.push(format!(
                            "[{from_label} nack]: message {message_id} rejected — {r}"
                        ));
                        continue;
                    }
                    _ => {}
                }

                if let Some(ref metrics) = state.messaging.metrics {
                    metrics.messages_received.fetch_add(1, Ordering::Relaxed);
                }

                if msg.requires_ack {
                    let ack_reply = msg.make_ack(mailbox.address.clone());
                    if let Err(e) = mailbox.send(ack_reply).await {
                        astra_core::agent_warn!("mailbox", "Failed to send ack: {e}");
                    }
                    if let Some(ref metrics) = state.messaging.metrics {
                        metrics.acks_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if let Some(ref handler) = state.permission_handler
                    && let Some((correlation_id, response)) = handler.process_message(msg).await
                {
                    let response_msg =
                        response.to_message(&mailbox.address, &msg.from, &correlation_id);
                    if let Err(e) = mailbox.send(response_msg).await {
                        astra_core::agent_warn!(
                            "mailbox",
                            "Failed to send permission response: {e}"
                        );
                    }
                    continue;
                }

                match &msg.payload {
                    crate::messaging::types::MessagePayload::Text { content, .. } => {
                        parts.push(format!("[{from_label}]: {content}"));
                    }
                    crate::messaging::types::MessagePayload::Progress {
                        status, detail, ..
                    } => {
                        let extra = detail.as_deref().unwrap_or("");
                        parts.push(format!("[{from_label} progress]: {status} {extra}"));
                    }
                    crate::messaging::types::MessagePayload::Request { request_type, .. } => {
                        parts.push(format!("[{from_label} request]: {request_type:?}"));
                    }
                    crate::messaging::types::MessagePayload::Response { accepted, .. } => {
                        parts.push(format!("[{from_label} response]: accepted={accepted}"));
                    }
                    crate::messaging::types::MessagePayload::Signal(sig) => {
                        parts.push(format!("[{from_label} signal]: {sig:?}"));
                    }
                    crate::messaging::types::MessagePayload::Ack { .. } => {}
                    crate::messaging::types::MessagePayload::Nack { .. } => {}
                }
            }
            if !parts.is_empty() {
                let mailbox_text = format!(
                    "📬 Messages from other agents ({}{}):\n{}",
                    pending.len(),
                    if has_more { "+, more queued" } else { "" },
                    parts.join("\n")
                );
                state.messages.push(serde_json::json!({
                    "role": "system",
                    "content": mailbox_text,
                }));
            }
        }
    }

    if let Some(resolver) = &state.skills.resolver {
        let full = resolver.available_skills();
        state.skills.listing_message = if full.is_empty() {
            None
        } else {
            let (visible, open_skill_name) = crate::turn::skill_tool::visible_skills_for_host_turn(
                &full,
                state.message.as_str(),
                &state.skills.quality_tracker,
                &state.skills.pinned,
                &state.skills.discovered,
                &state.skills.invoked,
                &state.skills.search,
            );
            Some(crate::turn::skill_tool::skill_listing_system_message(
                &visible,
                Some(&state.skills.quality_tracker),
                Some(&state.skills.pinned),
                open_skill_name,
            ))
        };
    }

    if turn_index > 0 {
        const INVENTORY_HEADER: &str = "## Already Fetched (do NOT re-read/re-grep these)\n";
        state.messages.retain(|m| {
            m.get("role").and_then(Value::as_str) != Some("system")
                || !m
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.starts_with(INVENTORY_HEADER))
        });
        let inventory = state.semantic_dedup.context_inventory();
        if !inventory.is_empty() {
            state.messages.push(serde_json::json!({
                "role": "system",
                "content": format!("{INVENTORY_HEADER}{inventory}"),
            }));
        }
    }

    if turn_index > 0 {
        // ── Stall correction: inject a nudge if stall was detected ────
        // Stall events are recorded during the tool phase of the *previous*
        // turn.  If any new events appeared, build a reflection and inject it
        // so the LLM can self-correct before the next tool round.
        //
        // Limit: at most 3 nudges per loop to avoid nudge-spam which itself
        // wastes context.
        const MAX_NUDGES: u32 = 3;
        if !state.stall.events.is_empty() && state.stall.nudge_count < MAX_NUDGES {
            let recent_events: Vec<_> = state
                .stall
                .events
                .iter()
                .filter(|(_, t)| *t as usize >= turn_index.saturating_sub(1))
                .collect();
            if !recent_events.is_empty() {
                let error_tools: Vec<&str> = state.turn_guard.health.deprioritized_tools();
                let reflection = super::stall::build_stall_reflection(
                    &state.stall.turn_sigs,
                    &error_tools,
                    state.stall.nudge_count as usize,
                );
                let nudge = reflection.to_nudge_message();
                state.messages.push(serde_json::json!({
                    "role": "system",
                    "content": nudge,
                }));
                state.stall.nudge_count += 1;
                if !quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "  ⚠ Stall correction injected (nudge #{}) — {}",
                            state.stall.nudge_count, reflection.what_happened,
                        ),
                    );
                }
            }
        }

        // Compute context pressure from last measured prompt tokens.
        let pressure = if state.max_turn_input_tokens > 0 {
            state
                .last_measured_prompt_tokens
                .map(|p| p as f64 / state.max_turn_input_tokens as f64)
                .unwrap_or(0.0)
        } else {
            0.0
        };

        // Adaptive microcompact: scale aggressiveness with context pressure.
        // Use state-aware variant when SessionFacts has active files (pin list).
        let mc = if !state.session_facts.active_files.is_empty() {
            super::microcompact::compact_tool_results_state_aware(
                &mut state.messages,
                pressure,
                &state.session_facts,
                5,
            )
        } else {
            super::microcompact::compact_tool_results_adaptive(&mut state.messages, pressure)
        };
        if mc.results_compacted > 0 && !quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Dim,
                format!(
                    "  ♻ Compacted {} old tool result(s), ~{} tokens saved (pressure {:.0}%)",
                    mc.results_compacted,
                    mc.tokens_saved,
                    pressure * 100.0,
                ),
            );
        }

        // Proactive compression gate: if pressure is still high after
        // microcompact, run the full compression pipeline *before* calling
        // the LLM, preventing 413 errors instead of reacting to them.
        if pressure >= 0.75 {
            let budget = super::context_compression::TokenBudget {
                max_prompt_tokens: state.max_turn_input_tokens,
                last_measured_tokens: state
                    .last_measured_prompt_tokens
                    .unwrap_or(0)
                    .saturating_sub(mc.tokens_saved as u64 * 4),
                chars_per_token: 4.0,
                current_round_index: Some(state.current_round_index),
            };
            let pipeline = if pressure >= 0.90 {
                super::context_compression::CompressionPipeline::aggressive_pipeline()
            } else {
                super::context_compression::CompressionPipeline::default_pipeline()
            };
            let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
            if outcome.total_tokens_freed > 0 && !quiet {
                let tier = if pressure >= 0.90 {
                    "aggressive"
                } else {
                    "default"
                };
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "  ⚡ Proactive {} compression: freed ~{} tokens at {:.0}% pressure",
                        tier,
                        outcome.total_tokens_freed,
                        pressure * 100.0,
                    ),
                );
            }
        }
    }

    // ── Compaction-on-resume: if turn 0 has many messages (restored from
    // checkpoint), estimate context pressure from raw content size and
    // proactively compress before the first LLM call.  This prevents an
    // immediate 413 when resuming from a CompactAndRetry interruption.
    if turn_index == 0 && state.messages.len() > 10 && state.max_turn_input_tokens > 0 {
        let total_chars: usize = state
            .messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .map(|s| s.len())
            .sum();
        let estimated_tokens = total_chars as f64 / 4.0;
        let estimated_pressure = estimated_tokens / state.max_turn_input_tokens as f64;
        if estimated_pressure >= 0.75 {
            let budget = super::context_compression::TokenBudget {
                max_prompt_tokens: state.max_turn_input_tokens,
                last_measured_tokens: estimated_tokens as u64,
                chars_per_token: 4.0,
                current_round_index: Some(state.current_round_index),
            };
            let pipeline = if estimated_pressure >= 0.90 {
                super::context_compression::CompressionPipeline::aggressive_pipeline()
            } else {
                super::context_compression::CompressionPipeline::default_pipeline()
            };
            let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
            if outcome.total_tokens_freed > 0 && !quiet {
                let tier = if estimated_pressure >= 0.90 {
                    "aggressive"
                } else {
                    "default"
                };
                host.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "  ⚡ Resume {} compression: freed ~{} tokens at ~{:.0}% est. pressure",
                        tier,
                        outcome.total_tokens_freed,
                        estimated_pressure * 100.0,
                    ),
                );
            }
        }
    }

    Ok(PreparedTurnIteration::Ready(TurnIterationPrep {
        quiet,
        turn_start_time,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::turn::agentic_loop_host::run_agentic_loop_with_host;
    use crate::turn::agentic_loop_host::tests::{
        MockHost, make_state, make_test_delegation_engine, text_result,
    };

    use super::*;

    #[tokio::test]
    async fn auto_inject_delegate_schema_when_engine_present() {
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        state.delegation_engine = Some(make_test_delegation_engine());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert_eq!(host.injected_schemas.len(), 1);
        let injected = &host.injected_schemas[0];
        let name = injected["function"]["name"].as_str().unwrap();
        assert_eq!(name, "delegate");
        assert!(host.valid_tools.contains("delegate"));
    }

    #[tokio::test]
    async fn no_inject_when_delegation_engine_absent() {
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(host.injected_schemas.is_empty());
        assert!(!host.valid_tools.contains("delegate"));
    }

    #[tokio::test]
    async fn injected_schema_matches_delegate_tool_schema() {
        let mut host = MockHost::new(vec![text_result("done", 50, 20, Some(10))]);
        let mut state = make_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));
        state.delegation_engine = Some(make_test_delegation_engine());

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;

        let expected = delegate_tool_schema();
        assert_eq!(host.injected_schemas[0], expected);
    }

    #[test]
    fn session_turn_number_prefers_explicit_outer_turn_over_agentic_step() {
        let mut state = make_state();
        state.session_turn = 1;
        state.max_turns = 50;
        state.remaining_turns = 0;

        assert_eq!(current_agentic_step(&state), 50);
        assert_eq!(session_turn_number(&state), 1);
    }

    /// P1-D: Production code must not use unsafe set_var.
    /// Hook env vars must go through session_env_overlay instead.
    #[test]
    fn no_unsafe_set_var_in_production() {
        let source = include_str!("agentic_loop_lifecycle.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            !prod_code.contains("std::env::set_var"),
            "production code must not use std::env::set_var (UB in multi-threaded context); \
             use astra_core::session_env_overlay::set instead"
        );
        assert!(
            prod_code.contains("session_env_overlay::set"),
            "hook env vars must be set via session_env_overlay"
        );
    }
}
