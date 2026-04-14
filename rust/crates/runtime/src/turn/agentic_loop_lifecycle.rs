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
use super::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;

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

fn budget_exhaustion_completion_text(state: &AgenticLoopState) -> String {
    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    if state.total_tool_calls > 0 {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). {} completed tool call(s) are preserved above.{}]\n",
            state.max_turns, state.total_tool_calls, checkpoint_note
        )
    } else {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). Partial progress is preserved.{}]\n",
            state.max_turns, checkpoint_note
        )
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
            unsafe { std::env::set_var(&key, &value) };
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
        .is_some_and(|f| f.load(Ordering::Relaxed))
    {
        if state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
            || state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled())
        {
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
        .is_some_and(|f| f.load(Ordering::Relaxed))
        || state
            .cancellation
            .token
            .as_ref()
            .is_some_and(|t| t.is_cancelled())
    {
        return Ok(PreparedTurnIteration::Finished(
            AgenticLoopOutcome::Cancelled,
        ));
    }

    if state.remaining_turns == 0 {
        if should_complete_budget_exhaustion_gracefully(state) {
            try_write_heavy_checkpoint(state);
            if state.final_text.trim().is_empty() {
                state.final_text = budget_exhaustion_completion_text(state);
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
        }
        return Err(format!(
            "{} (budget: {} turns)",
            CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG, state.max_turns
        ));
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
                return Ok(PreparedTurnIteration::Finished(
                    AgenticLoopOutcome::Completed,
                ));
            }
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
        let mc = super::microcompact::compact_tool_results(&mut state.messages, None);
        if mc.results_compacted > 0 && !quiet {
            host.emit_headless_line(
                HeadlessStderrStyle::Dim,
                format!(
                    "  ♻ Compacted {} old tool result(s), ~{} tokens saved",
                    mc.results_compacted, mc.tokens_saved,
                ),
            );
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
}
