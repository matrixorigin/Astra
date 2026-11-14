pub(crate) use astra_turn_core::evaluation::path_is_external_volatile_scratch;
use astra_turn_core::evaluation::{external_scratch_setup, normalize_absolute_path};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::super::{CompactionEngine, TokenBudget};
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, BudgetWrapupOrigin,
    CompletionActionWindow, RunControlStatus, SkillAutoRouteJudgeContext, TurnIntentJudgeOutcome,
    TurnPhaseKind, TurnPhaseOutcome, complete_turn_phase, try_write_heavy_checkpoint,
};
use crate::orchestration::permission_sync::PermissionResponseMessaging;
use crate::orchestration::{
    AgentToolRecordActionKind, CANCELLATION_ORIGIN_UNVERIFIED, CancellationOrigin,
    project_agent_tool_budget_record, render_agent_tool_budget_unfinished_detail,
    summarize_agent_tool_budget_result,
};
use astra_config::user_profile::{TurnIntent, WorkLifecycleIntent};
use astra_services::SessionArtifactStore;
use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind, CompactionTier};
use astra_turn_core::interruption::{
    InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
};
use astra_turn_core::stall::CLI_AGENTIC_TURN_BUDGET_STALL_ABORT_MSG;

const CHILD_AGENT_CANCEL_TIMEOUT: Duration = Duration::from_secs(3);
const PAUSE_LOOP_LOCAL_CHECK_INTERVAL: Duration = Duration::from_millis(25);
const PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_MAILBOX_CONTEXT_CHARS: usize = 24_000;
const MAX_RETAINED_MAILBOX_MESSAGES: usize = 128;
const MAX_RETAINED_MAILBOX_BYTES: usize = 128 * 1024;

fn retained_mailbox_usage(pending: &[super::host::VolatileInjection]) -> (usize, usize) {
    pending
        .iter()
        .filter(|injection| super::host::is_retained_mailbox_context(injection))
        .fold((0, 0), |(count, bytes), injection| {
            (
                count + 1,
                bytes.saturating_add(injection.payload.to_string().len()),
            )
        })
}

/// One mailbox consumption owner for ordinary turns and waits. A transport
/// wake is not permission to call the model; only injected context is.
pub(crate) async fn drain_mailbox_model_context<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    let mut model_context_changed = false;
    let question_correlations = Arc::clone(&state.messaging.question_correlations);
    const MAX_MAILBOX_DRAIN_PER_TURN: usize = 64;
    let (mut retained_mailbox_messages, mut retained_mailbox_bytes) =
        retained_mailbox_usage(&state.volatile_pending);
    for _ in 0..MAX_MAILBOX_DRAIN_PER_TURN {
        let Some(mailbox) = state.messaging.mailbox.as_mut() else {
            break;
        };
        // A one-envelope lease prevents a later permission or transport await
        // from rolling back messages that were already adopted or answered.
        let lease = mailbox.lease_bounded(1);
        let Some(msg) = lease.messages().first().cloned() else {
            lease.commit();
            break;
        };
        let has_more = lease.has_more();
        let address = &lease.mailbox().address;
        // A root mailbox may outlive one execution. Observe through the
        // current run without changing transport identity or reply matching.
        let mut observer = address.clone();
        if let Some(run_id) = state.current_run_id.as_ref() {
            observer.run_id.clone_from(run_id);
        }
        observer.agent_id.clone_from(&state.self_agent_id);
        let mut parts = Vec::new();
        let self_echo = msg.from == *address
            && matches!(
                msg.to,
                astra_messaging::types::MessageTarget::Broadcast { .. }
            );
        if !self_echo {
            let from_label = &msg.from.agent_id;
            let is_transient_progress = matches!(
                msg.payload,
                astra_messaging::types::MessagePayload::Progress { .. }
            );
            if !is_transient_progress {
                host.on_agent_communication(astra_messaging::agent_communication_event(
                    &observer,
                    astra_messaging::AgentCommunicationDirection::Received,
                    &msg,
                ));
            }

            let mut handled_permission = false;
            if let Some(ref handler) = state.permission_handler
                && let Some((correlation_id, response)) = handler.process_message(&msg).await
            {
                handled_permission = true;
                let response_msg = response.to_message(address, &msg.from, &correlation_id);
                if let Err(error) = lease.mailbox().send(response_msg.clone()).await {
                    astra_core::agent_warn!(
                        "mailbox",
                        "Failed to send permission response: {error}"
                    );
                    if !matches!(
                        error,
                        astra_messaging::types::MailboxError::AgentNotFound(_)
                            | astra_messaging::types::MailboxError::ChannelClosed
                    ) {
                        // An uncertain send may have delivered the answer.
                        // Keep this request and stop rather than retrying it
                        // into a second permission decision.
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                            format!("permission response delivery uncertain: {error}"),
                        ));
                    }
                    // The child is definitively gone: no permission was
                    // granted. Consume the obsolete request so an unrelated
                    // parent task can continue without replaying it forever.
                } else {
                    host.on_agent_communication(astra_messaging::agent_communication_event(
                        &observer,
                        astra_messaging::AgentCommunicationDirection::Sent,
                        &response_msg,
                    ));
                }
            }

            if !handled_permission {
                match &msg.payload {
                    astra_messaging::types::MessagePayload::Text { content, .. } => {
                        parts.push(format!("[{from_label}]: {content}"));
                    }
                    astra_messaging::types::MessagePayload::Progress { .. } => {}
                    astra_messaging::types::MessagePayload::Request { request_type, data } => {
                        let data = (!data.is_null()).then(|| format!(" · {data}"));
                        parts.push(format!(
                            "[{from_label} request id={}]: {request_type:?}{}",
                            msg.id,
                            data.as_deref().unwrap_or("")
                        ));
                    }
                    astra_messaging::types::MessagePayload::Response {
                        request_id,
                        accepted,
                        data,
                    } => {
                        let assessment = question_correlations.classify_response(
                            state.current_run_id.as_deref().unwrap_or_default(),
                            &msg,
                        );
                        let data = data.as_ref().map(|data| format!(" · {data}"));
                        parts.push(format!(
                            "[{from_label} run={} response to {request_id}; correlation={assessment}]: accepted={accepted}{}",
                            msg.from.run_id, data.as_deref().unwrap_or("")
                        ));
                    }
                    astra_messaging::types::MessagePayload::Signal(sig) => {
                        let signal = format!("[{from_label} signal]: {sig:?}");
                        let preview = signal.chars().take(4_000).collect::<String>();
                        parts.push(if preview.len() < signal.len() {
                            format!("{preview}… [signal preview truncated]")
                        } else {
                            preview
                        });
                    }
                }
            }
        }
        let mut defer_ack = false;
        let mut retain_delivery = false;
        let mut retry_after_provider = false;
        let staged = if parts.is_empty() {
            None
        } else {
            let display = format!(
                "📬 Message id={} from another agent:\n{}",
                msg.id,
                parts.join("\n")
            );
            if display.chars().count() > MAX_MAILBOX_CONTEXT_CHARS {
                retain_delivery = true;
                Some((
                    super::host::VolatileKind::ContextPressure,
                    serde_json::Value::String(format!(
                        "📬 Message id={} was received but not injected because it exceeds the {}-character model context limit; ask the sender for a concise follow-up or an artifact reference.",
                        msg.id, MAX_MAILBOX_CONTEXT_CHARS
                    )),
                ))
            } else {
                let semantic = matches!(
                    msg.payload,
                    astra_messaging::types::MessagePayload::Text { .. }
                        | astra_messaging::types::MessagePayload::Request {
                            request_type: astra_messaging::types::RequestType::Shutdown
                                | astra_messaging::types::RequestType::ContextShare
                                | astra_messaging::types::RequestType::Custom(_),
                            ..
                        }
                        | astra_messaging::types::MessagePayload::Response { .. }
                );
                if semantic {
                    let payload_digest = format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::to_vec(&msg.payload)
                                .expect("agent message payload must serialize"),
                        )
                    );
                    let message_kind = match msg.payload {
                        astra_messaging::types::MessagePayload::Text { .. } => "text",
                        astra_messaging::types::MessagePayload::Request { .. } => "request",
                        astra_messaging::types::MessagePayload::Response { .. } => "response",
                        _ => unreachable!(
                            "semantic mailbox payload must be text, request, or response"
                        ),
                    };
                    let payload = serde_json::json!({
                        "schema": super::host::RETAINED_MAILBOX_CONTEXT_SCHEMA,
                        "receiver": address,
                        "sender": msg.from,
                        "message_id": msg.id,
                        "correlation_id": msg.correlation_id,
                        "message_kind": message_kind,
                        "response_request_id": match &msg.payload {
                            astra_messaging::types::MessagePayload::Response { request_id, .. } => Some(request_id),
                            _ => None,
                        },
                        "response_match": question_correlations.classify_response(
                            state.current_run_id.as_deref().unwrap_or_default(), &msg),
                        "payload_digest": payload_digest,
                        "delivery_count": 0,
                        "display": display,
                    });
                    let prior = state.volatile_pending.iter().find(|injection| {
                        injection.kind == super::host::VolatileKind::Mailbox
                            && injection.payload["schema"]
                                == super::host::RETAINED_MAILBOX_CONTEXT_SCHEMA
                            && injection.payload["receiver"] == payload["receiver"]
                            && injection.payload["sender"] == payload["sender"]
                            && injection.payload["message_id"] == payload["message_id"]
                    });
                    if let Some(prior) = prior {
                        if prior.payload["payload_digest"] != payload["payload_digest"]
                            || prior.payload["response_request_id"]
                                != payload["response_request_id"]
                        {
                            return Err(astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::ContractViolation,
                                "mailbox message identity conflicts with retained semantic input",
                            ));
                        }
                        defer_ack = true;
                        None
                    } else {
                        let payload_bytes = payload.to_string().len();
                        while retained_mailbox_messages >= MAX_RETAINED_MAILBOX_MESSAGES
                            || retained_mailbox_bytes.saturating_add(payload_bytes)
                                > MAX_RETAINED_MAILBOX_BYTES
                        {
                            let evict = state
                                .volatile_pending
                                .iter()
                                .position(|injection| {
                                    super::host::is_retained_mailbox_context(injection)
                                        && injection.payload["observed_by_provider"] == true
                                        && injection.payload["message_kind"] != "request"
                                })
                                .or_else(|| {
                                    state.volatile_pending.iter().position(|injection| {
                                        super::host::is_retained_mailbox_context(injection)
                                            && injection.payload["observed_by_provider"] == true
                                    })
                                });
                            let Some(index) = evict else {
                                break;
                            };
                            let removed = state.volatile_pending.remove(index);
                            retained_mailbox_messages = retained_mailbox_messages.saturating_sub(1);
                            retained_mailbox_bytes = retained_mailbox_bytes
                                .saturating_sub(removed.payload.to_string().len());
                        }
                        if retained_mailbox_messages >= MAX_RETAINED_MAILBOX_MESSAGES
                            || retained_mailbox_bytes.saturating_add(payload_bytes)
                                > MAX_RETAINED_MAILBOX_BYTES
                        {
                            // The notice is useful to the current provider
                            // attempt, but it is not adoption of the message
                            // itself. Keep the original envelope in transport
                            // custody and retry after an existing context
                            // entry has been consumed.
                            retain_delivery = true;
                            retry_after_provider = true;
                            Some((
                                super::host::VolatileKind::ContextPressure,
                                serde_json::Value::String(format!(
                                    "📬 Message id={} was received but not retained because the mailbox context budget is full; inspect the communication trace or ask the sender to resend only the essential detail.",
                                    msg.id
                                )),
                            ))
                        } else {
                            retained_mailbox_messages += 1;
                            retained_mailbox_bytes =
                                retained_mailbox_bytes.saturating_add(payload_bytes);
                            defer_ack = true;
                            Some((super::host::VolatileKind::Mailbox, payload))
                        }
                    }
                } else {
                    defer_ack = true;
                    Some((
                        super::host::VolatileKind::Mailbox,
                        serde_json::Value::String(display),
                    ))
                }
            }
        };

        if retain_delivery {
            // Park the exact envelope outside the ready queue. This lets later
            // messages proceed while preserving transport custody; capacity-
            // blocked deliveries are released after a successful provider
            // boundary, while permanently oversized ones stay parked.
            lease.defer(retry_after_provider);
            if let Some((kind, payload)) = staged {
                state.push_volatile_payload(kind, payload);
                model_context_changed = true;
            }
            if !has_more {
                break;
            }
            continue;
        }

        lease.commit();
        // No await separates releasing process-local ownership from staging
        // this exact envelope for the next model request.
        if let Some((kind, payload)) = staged {
            state.push_volatile_payload(kind, payload);
            model_context_changed = true;
        }
        if defer_ack {
            if let Some(mailbox) = state.messaging.mailbox.as_mut() {
                mailbox.defer_acknowledgement(msg);
            }
        } else if let Some(mailbox) = state.messaging.mailbox.as_ref()
            && let Err(error) = mailbox
                .acknowledge_received(std::slice::from_ref(&msg))
                .await
        {
            astra_core::agent_warn!(
                "mailbox",
                "failed to confirm consumed message; transport may redeliver it: {error}"
            );
        }
        if !has_more {
            break;
        }
    }

    Ok(model_context_changed)
}

/// Confirm mailbox deliveries after a successful provider decision consumed
/// their staged context. Before that boundary, `queued` means only
/// transport custody; acknowledging earlier can lose a message during context
/// pressure, provider failure, or process recovery.
pub(crate) async fn acknowledge_adopted_mailbox_messages(state: &mut AgenticLoopState) {
    let pending = {
        let Some(mailbox) = state.messaging.mailbox.as_mut() else {
            return;
        };
        mailbox.adopted_acknowledgements()
    };
    if pending.is_empty() {
        return;
    }
    // Completion obligations are settled by the successful attempt's typed
    // context lease, independently of these process-local transport envelopes.
    let (retry, error) = {
        let Some(mailbox) = state.messaging.mailbox.as_ref() else {
            return;
        };
        mailbox.acknowledge_received_with_failures(&pending).await
    };
    if let Some(error) = error {
        astra_core::agent_warn!(
            "mailbox",
            "failed to confirm adopted messages; transport may redeliver them: {error}"
        );
    }
    if let Some(mailbox) = state.messaging.mailbox.as_mut() {
        for message in pending {
            if !retry.iter().any(|failed| failed.id == message.id) {
                mailbox.forget_acknowledgement(&message.id);
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TurnIterationPrep {
    pub(crate) quiet: bool,
    pub(crate) turn_start_time: Instant,
}

pub(crate) enum PreparedTurnIteration {
    Ready(TurnIterationPrep),
    Finished(AgenticLoopOutcome),
}

/// Wait until a live pause clears or cancellation wins. This is the sole
/// pause-control wait primitive for both turn preparation and provider
/// boundaries: local flags give prompt same-pod wakeups while the bounded
/// durable poll preserves correct cross-pod resume/cancel behavior.
pub(crate) async fn wait_for_pause_clear_or_cancel<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    if !state
        .cancellation
        .pause_flag
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        return Ok(false);
    }
    match host.execution_time_budget_remaining() {
        Some(budget) => tokio::time::timeout(budget.total_remaining, wait_for_pause_control(state))
            .await
            .map_err(|_| {
                astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "execution deadline expired while paused",
                )
            }),
        None => Ok(wait_for_pause_control(state).await),
    }
}

async fn wait_for_pause_control(state: &mut AgenticLoopState) -> bool {
    let mut last_db_poll = tokio::time::Instant::now();
    while state
        .cancellation
        .pause_flag
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        if state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
            || state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        {
            return true;
        }

        if !state.has_durable_control_watcher()
            && last_db_poll.elapsed() >= PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL
        {
            if let Some(run_control) = state.run_control.as_ref() {
                if let (Some(user_id), Some(run_id)) = (
                    state.owner_user_id.as_deref(),
                    state.current_run_id.as_deref(),
                ) {
                    match run_control.control_status(user_id, run_id).await {
                        Ok(Some(RunControlStatus::Cancelled)) => {
                            if let Some(flag) = state.cancellation.flag.as_ref() {
                                flag.store(true, Ordering::SeqCst);
                            }
                            if let Some(token) = state.cancellation.token.as_ref() {
                                token.cancel();
                            }
                            return true;
                        }
                        Ok(Some(RunControlStatus::Paused)) => {}
                        Ok(None) => {
                            if let Some(flag) = state.cancellation.pause_flag.as_ref() {
                                flag.store(false, Ordering::SeqCst);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                run_id,
                                error = %error,
                                "failed to poll run control status while paused"
                            );
                        }
                    }
                }
            }
            last_db_poll = tokio::time::Instant::now();
        }
        tokio::time::sleep(PAUSE_LOOP_LOCAL_CHECK_INTERVAL).await;
    }
    false
}

pub(crate) fn session_turn_number(state: &AgenticLoopState) -> u32 {
    state.current_session_turn_number()
}

pub(crate) fn current_agentic_step(state: &AgenticLoopState) -> u32 {
    if state.llm_rounds_completed > 0 {
        return state.llm_rounds_completed;
    }
    state.max_turns.saturating_sub(state.remaining_turns) as u32
}

pub(crate) fn completed_tool_calls(state: &AgenticLoopState) -> u32 {
    state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed() && !record.outcome_is_unknown())
        .count()
        .min(u32::MAX as usize) as u32
}

/// Keep the final settlement boundary aware of execution outcomes that may be
/// hidden by a compacted or very long tool history. This is evidence only: it
/// does not decide whether the user goal is blocked, and it does not constrain
/// the tools available before settlement.
fn settlement_tool_outcome_evidence(state: &AgenticLoopState) -> Value {
    let executed = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed() && !record.outcome_is_unknown())
        .count();
    let failed = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed() && !record.ok && !record.outcome_is_unknown())
        .count();
    let rejected = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| {
            record.effective_disposition()
                == astra_services::session_journal::ToolCallDisposition::Rejected
        })
        .count();
    let unknown = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.outcome_is_unknown())
        .count();
    let latest_failure = state
        .stall
        .tool_call_records
        .iter()
        .rev()
        .find(|record| {
            (record.was_executed() && !record.ok)
                || record.outcome_is_unknown()
                || record.effective_disposition()
                    == astra_services::session_journal::ToolCallDisposition::Rejected
        })
        .map(|record| {
            serde_json::json!({
                "tool": record.name,
                "error_kind": record.error_kind.map(|kind| format!("{kind:?}")),
                "disposition": format!("{:?}", record.effective_disposition()),
                "outcome_unknown": record.outcome_is_unknown(),
            })
        });

    serde_json::json!({
        "tool_calls_completed": executed,
        "tool_calls_failed": failed,
        "tool_calls_rejected": rejected,
        "tool_calls_unknown": unknown,
        "latest_failure": latest_failure,
    })
}

fn apply_judged_turn_intent_to_observability(
    state: &AgenticLoopState,
    intent: &TurnIntent,
    record_feedback: bool,
    user_message: &str,
) {
    if let Some(session) = &state.telemetry.observability_session {
        let mut session = astra_core::sync_poison::recover_rwlock_write(session);
        session.profile.apply_judged_turn_intent(intent);
        let is_correction = intent.objective_relation
            == astra_turn_types::ObjectiveRelation::Correct
            || intent.feedback.is_some_and(|feedback| {
                feedback.kind == astra_turn_types::UserFeedbackKind::Correction
            });
        if record_feedback && is_correction && session.record_user_correction() {
            session.record_correction_excerpt(user_message);
        }
    }

    let Some(hub) = &state.telemetry.observability_hub else {
        return;
    };
    if !record_feedback {
        return;
    }
    let signal_type = match intent.feedback.map(|feedback| feedback.kind) {
        Some(astra_turn_types::UserFeedbackKind::Approval) => {
            astra_core::feedback::SignalType::Acceptance
        }
        Some(astra_turn_types::UserFeedbackKind::Correction) => {
            astra_core::feedback::SignalType::Correction
        }
        Some(
            astra_turn_types::UserFeedbackKind::Clarification
            | astra_turn_types::UserFeedbackKind::Requirement
            | astra_turn_types::UserFeedbackKind::Preference,
        ) => astra_core::feedback::SignalType::Reanchor,
        None => match intent.objective_relation {
            astra_turn_types::ObjectiveRelation::Acknowledge => {
                astra_core::feedback::SignalType::Acceptance
            }
            astra_turn_types::ObjectiveRelation::Correct => {
                astra_core::feedback::SignalType::Correction
            }
            astra_turn_types::ObjectiveRelation::Unknown
            | astra_turn_types::ObjectiveRelation::Continue
            | astra_turn_types::ObjectiveRelation::Refine
            | astra_turn_types::ObjectiveRelation::Replace => return,
        },
    };

    let mut signal = astra_core::feedback::FeedbackSignal::new(signal_type).with_context(
        "objective_relation",
        serde_json::json!(intent.objective_relation.as_str()),
    );
    if let Some(feedback) = intent.feedback {
        signal = signal
            .with_context("feedback_kind", serde_json::json!(feedback.kind.as_str()))
            .with_context(
                "feedback_target",
                serde_json::json!(feedback.target.as_str()),
            );
    }
    if let Some(run_id) = state.current_run_id.as_deref() {
        signal = signal.with_context("source_run_id", serde_json::json!(run_id));
    }
    if let Some(session_id) = state.current_session_id.as_deref() {
        signal = signal.with_context("session_id", serde_json::json!(session_id));
    }
    hub.record_feedback(signal);
}

/// Apply observations exactly once to their frozen canonical human source.
/// Work admission and capability decisions remain owned by the Server boundary.
pub(crate) fn apply_current_user_turn_semantics(
    state: &mut AgenticLoopState,
    intent: &TurnIntent,
) -> bool {
    if !record_current_user_turn_semantics(state, intent) {
        return false;
    }
    let user_message = state
        .telemetry
        .turn_intent_context
        .as_ref()
        .and_then(|context| context.source.as_ref())
        .expect("recorded semantics require a canonical source")
        .message_text
        .clone();
    apply_judged_turn_intent_to_observability(state, intent, true, &user_message);
    if intent.reanchors_current_objective() {
        apply_structured_user_reanchor(state, intent.objective_relation, &user_message);
    }
    apply_structured_user_feedback(state, intent, &user_message);
    true
}

/// Persist new semantics and return whether objective/feedback effects may be applied.
/// Filling only a missing assessment must not replay previously applied effects.
pub(crate) fn record_current_user_turn_semantics(
    state: &mut AgenticLoopState,
    intent: &TurnIntent,
) -> bool {
    crate::turn::agentic::turn_intent::capture_turn_intent_context(state);
    let Some(source) = state
        .telemetry
        .turn_intent_context
        .as_ref()
        .and_then(|context| context.source.clone())
    else {
        tracing::warn!("turn intent was judged without an exact canonical user-message owner");
        return false;
    };
    let index = source.message_index;
    // A compaction or rewrite while the judge was running invalidates this
    // binding. Never relocate by text and accidentally label another turn.
    if !state.messages.get(index).is_some_and(|message| {
        astra_turn_types::is_human_user_message(message)
            && astra_turn_core::prompt_facing::extract_text_content(message).as_deref()
                == Some(source.message_text.as_str())
    }) {
        tracing::warn!("turn intent source changed after judgment began; omitting observation");
        return false;
    }
    let mut semantics =
        astra_turn_types::UserTurnSemantics::new(intent.objective_relation, intent.feedback);
    semantics.assessment = intent.assessment;
    if let Some(assessment) = semantics.assessment.as_mut()
        && assessment.feedback_relation
            == astra_turn_types::FeedbackResponseRelation::PreviousResponse
    {
        semantics.feedback_response = source.feedback_response;
        if semantics.feedback_response.is_none() {
            assessment.feedback_relation = astra_turn_types::FeedbackResponseRelation::Unknown;
            assessment.satisfaction = astra_turn_types::ResponseSatisfaction::Unknown;
            assessment.satisfaction_confidence = astra_turn_types::AssessmentConfidence::Unknown;
        }
    }

    let mut record_feedback = true;
    match astra_turn_types::user_turn_semantics(&state.messages[index]) {
        Ok(Some(current)) => {
            let advances_unknown = current.objective_relation
                == astra_turn_types::ObjectiveRelation::Unknown
                && current.feedback.is_none()
                && (intent.objective_relation != astra_turn_types::ObjectiveRelation::Unknown
                    || intent.feedback.is_some());
            if !advances_unknown {
                semantics.objective_relation = current.objective_relation;
                semantics.feedback = current.feedback;
                record_feedback = false;
            }
            // Filling an unknown objective must not rewrite the earlier
            // observational judgment or its causal response reference.
            if current.assessment.is_some() {
                semantics.assessment = current.assessment;
                semantics.feedback_response = current.feedback_response.clone();
            }
            if semantics == current {
                return false;
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                error = %error,
                "refusing to overwrite invalid producer-owned user-turn semantics"
            );
            return false;
        }
    }
    let message = &mut state.messages[index];
    let recorded = astra_turn_types::mark_user_turn_semantics(message, semantics);
    if !recorded {
        tracing::warn!("canonical turn-semantics owner was not a user message");
    }
    recorded && record_feedback
}

fn auto_route_tool_call_id(skill_name: &str) -> String {
    let mut normalized = String::with_capacity(skill_name.len());
    let mut last_was_dash = true;
    for ch in skill_name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            normalized.push('-');
            last_was_dash = true;
        }
    }
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "skill-auto-route".to_string()
    } else {
        format!("skill-auto-route-{normalized}")
    }
}

fn skill_auto_route_attempt_key(query: &str, skill_name: &str) -> String {
    let mut normalized_skill = String::with_capacity(skill_name.len());
    let mut last_was_dash = true;
    for ch in skill_name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized_skill.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            normalized_skill.push('-');
            last_was_dash = true;
        }
    }
    let normalized_skill = normalized_skill.trim_matches('-');
    let normalized_skill = if normalized_skill.is_empty() {
        "unknown"
    } else {
        normalized_skill
    };
    let mut hasher = Sha256::new();
    hasher.update(query.trim().as_bytes());
    let query_hash = hasher.finalize();
    format!("{normalized_skill}:{query_hash:x}")
}

#[cfg(test)]
fn pure_user_intent_for_runtime_decision(message: &str) -> String {
    message.trim().to_string()
}

async fn maybe_pre_route_skill<H: AgenticLoopHost>(host: &mut H, state: &mut AgenticLoopState) {
    if state.llm_rounds_completed > 0
        || !state.tool_results.is_empty()
        || !state.skills.execution.invoked.is_empty()
    {
        return;
    }

    // Auto-executing a workflow is a control decision, not a catalog hint.
    // Require producer-owned turn semantics before making it. In particular,
    // an asynchronous Work-admission decision must not race a separate skill
    // judge and let a repository skill seize an unrelated user task. The
    // primary model still sees the catalog and may explicitly invoke a skill.
    let Some(turn_intent) = state.turn_intent.as_ref() else {
        return;
    };
    if turn_intent.work_lifecycle == WorkLifecycleIntent::Required {
        return;
    }

    let query = state.runtime_decision_user_intent();
    if query.is_empty() {
        return;
    }

    let Some(resolver) = state.skills.resolver.clone() else {
        return;
    };

    let full = resolver.available_skills();
    if full.is_empty() {
        return;
    }

    let visible = crate::turn::skill_tool::visible_skills_for_host_turn(
        &full,
        &state.skills.execution.invoked,
    );
    let Some(decision) = host
        .judge_skill_auto_route(
            state,
            SkillAutoRouteJudgeContext {
                query: &query,
                visible_skills: &visible,
            },
        )
        .await
    else {
        tracing::debug!(
            query_length = query.len(),
            visible_skill_count = visible.len(),
            "skill auto-route produced no decision; continuing without pre-route"
        );
        return;
    };
    let skill_name = decision.skill_name.trim().to_string();
    let attempt_key = skill_auto_route_attempt_key(&query, &skill_name);
    if state
        .skills
        .execution
        .auto_route_attempts
        .contains(&attempt_key)
    {
        tracing::debug!(
            skill_name,
            "skill auto-route skipped repeated decision for same user intent"
        );
        return;
    }
    if skill_name.is_empty() || !visible.iter().any(|skill| skill.name == skill_name) {
        state
            .skills
            .execution
            .auto_route_attempts
            .insert(attempt_key);
        tracing::warn!(
            skill_name,
            "skill auto-route judge returned a skill outside the visible catalog"
        );
        return;
    }

    let composition_ctx = crate::skills::composition::CompositionContext::root();
    let skill_ctx = crate::turn::agentic::tool_interception::build_skill_context(state);
    let result = crate::turn::skill_tool::execute_skill_direct(
        resolver.as_ref(),
        state.skills.executor.as_ref(),
        &skill_name,
        &query,
        Some(&composition_ctx),
        &skill_ctx,
    )
    .await;
    let verified = result
        .verification
        .as_ref()
        .map(|outcome| outcome.all_required_passed)
        .unwrap_or(true);
    if !result.success || !verified {
        state
            .skills
            .execution
            .auto_route_attempts
            .insert(attempt_key);
        return;
    }

    let tool_call_id = auto_route_tool_call_id(&skill_name);
    let mut skill_result =
        crate::turn::skill_tool::append_skill_loaded_marker(&result.output, &skill_name);
    if let Some(activation) = result.activation {
        crate::turn::agentic::tool_interception::apply_skill_activation(state, activation);
    }
    if let Some(notice) =
        crate::turn::agentic::tool_interception::runtime_tool_allowlist_notice(state)
    {
        skill_result.push_str("\n\n");
        skill_result.push_str(&notice);
    }
    let skill_result =
        astra_turn_core::tool_result_sanitize::tool_result_content_for_model_unbounded(
            crate::turn::skill_tool::SKILL_TOOL_NAME,
            &skill_result,
        );
    let content_for_model = astra_turn_core::tool_result_sanitize::truncate_tool_result_for_model(
        crate::turn::skill_tool::SKILL_TOOL_NAME,
        &skill_result,
    );

    let synthetic_start = state.messages.len();
    state.messages.push(serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": tool_call_id,
            "type": "function",
            "function": {
                "name": crate::turn::skill_tool::SKILL_TOOL_NAME,
                "arguments": serde_json::json!({
                    "skill_name": skill_name.clone(),
                    "task": query,
                }).to_string(),
            }
        }]
    }));
    host.attest_runtime_messages(state, synthetic_start);
    state.record_appended_prompt_history_from(synthetic_start);
    let (tool_msg, tool_result) =
        astra_turn_core::headless_tool_assembly::openai_tool_roundtrip_values(
            &tool_call_id,
            crate::turn::skill_tool::SKILL_TOOL_NAME,
            &content_for_model,
            astra_turn_core::tool_result_semantics::ToolResultStatus::Completed,
        );
    state.push_prompt_history_message(tool_msg);
    state.tool_results.push(tool_result);
    state.telemetry.all_selected_skills.push(skill_name.clone());
    state
        .skills
        .execution
        .auto_route_attempts
        .insert(attempt_key);
    let execution_topology =
        crate::turn::skill_tool::declared_execution_topology(resolver.as_ref(), &skill_name);
    state.skills.execution.invoked.insert(
        skill_name.clone(),
        crate::turn::skill_tool::InvokedSkill {
            name: skill_name,
            content: skill_result,
            invoked_at_turn: session_turn_number(state),
            reentry_count: 0,
            execution_topology,
        },
    );
}

fn default_budget_exhaustion_completion_text(state: &AgenticLoopState) -> String {
    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    let completed_tool_calls = completed_tool_calls(state);
    let completed_agentic_turns = current_agentic_step(state);
    if completed_tool_calls > 0 {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). {} completed tool call(s) are preserved above.{}]\n",
            completed_agentic_turns, completed_tool_calls, checkpoint_note
        )
    } else {
        format!(
            "[Turn budget exhausted after {} agentic turn(s). Partial progress is preserved.{}]\n",
            completed_agentic_turns, checkpoint_note
        )
    }
}

#[derive(Default)]
struct ParallelAgentSummary {
    label: Option<String>,
    /// Terminality is ownership state, not outcome quality.  A failed or
    /// cancelled child is no longer live and must not block parent synthesis.
    terminal: bool,
    normal_result_available: bool,
    completed_result: Option<String>,
    partial_result: Option<String>,
    incomplete_reason: Option<String>,
    control_errors: Vec<String>,
}

struct CompletedParallelAgent {
    label: String,
    /// The provider-observed terminal result may have been compacted from the
    /// bounded required-context lane after repeated leases. Result observation
    /// remains authoritative even when this presentation copy is gone.
    result: Option<String>,
}

struct UnfinishedParallelAgent {
    agent_id: String,
    label: String,
    incomplete_reason: Option<String>,
    control_errors: Vec<String>,
}

struct TerminalParallelAgentIssue {
    agent_id: String,
    label: String,
    partial_result: Option<String>,
    incomplete_reason: Option<String>,
    control_errors: Vec<String>,
}

struct ParallelAgentBudgetRollup {
    completed: Vec<CompletedParallelAgent>,
    terminal_issues: Vec<TerminalParallelAgentIssue>,
    unfinished: Vec<UnfinishedParallelAgent>,
}

/// Apply one typed observation to the owner's per-agent lifecycle state.
///
/// `agent.spawn` is allowed to return a terminal child result directly.  It
/// must therefore use the same reducer as `agent.get_result`; otherwise a
/// foreground spawn-only completion is recorded as launched forever.  Once a
/// terminal result is observed it is sticky: a later stale running/launched
/// observation must not reopen the child or make the owner look incomplete.
fn apply_parallel_agent_budget_projection(
    entry: &mut ParallelAgentSummary,
    projection: &crate::orchestration::AgentToolBudgetRecordProjection,
) {
    if entry.label.is_none() {
        entry.label = projection.display_name_hint.clone();
    }
    if let Some(summarized) = projection.control_error_summary.clone()
        && !entry
            .control_errors
            .iter()
            .any(|existing| existing == &summarized)
    {
        entry.control_errors.push(summarized);
    }
    // Once a terminal observation releases the child owner, later stale
    // running/terminal callbacks must not reopen or rewrite that outcome.
    if entry.terminal {
        return;
    }
    if projection.terminal {
        entry.terminal = true;
        entry.normal_result_available = projection.normal_result_available;
        entry.completed_result = projection.completed_result.clone();
        entry.partial_result = projection.partial_result.clone();
        entry.incomplete_reason = projection.incomplete_reason.clone();
    } else {
        entry.incomplete_reason = projection.incomplete_reason.clone();
    }
}

fn collect_parallel_agent_budget_rollup(
    state: &AgenticLoopState,
) -> Option<ParallelAgentBudgetRollup> {
    let mut summaries: BTreeMap<String, ParallelAgentSummary> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();

    for record in &state.stall.tool_call_records {
        if record.name != "agent" {
            continue;
        }
        let projection = project_agent_tool_budget_record(record);

        match projection.action {
            AgentToolRecordActionKind::Spawn | AgentToolRecordActionKind::GetResult => {
                let Some(agent_id) = projection.agent_id.clone() else {
                    continue;
                };
                if !order.iter().any(|id| id == &agent_id) {
                    order.push(agent_id.clone());
                }
                let entry = summaries.entry(agent_id).or_default();
                apply_parallel_agent_budget_projection(entry, &projection);
                if !entry.terminal
                    && let Some(observed_child) =
                        super::execution_phase::observed_direct_child_for_nonterminal_receipt(
                            state, record,
                        )
                {
                    entry.terminal = true;
                    let observed_result = observed_child["result"]
                        .as_str()
                        .filter(|result| !result.trim().is_empty())
                        .map(str::to_owned);
                    // The shared proof above is producer-owned terminal
                    // evidence: exact child identity, completed status, and
                    // provider observation. After two provider leases the
                    // host deliberately compacts the presentation result,
                    // so missing `result` is not a child failure.
                    entry.normal_result_available = true;
                    entry.completed_result = observed_result;
                    entry.partial_result = None;
                    entry.incomplete_reason = None;
                }
            }
            AgentToolRecordActionKind::Other => {}
        }
    }

    let completed: Vec<_> = order
        .iter()
        .filter_map(|agent_id| {
            summaries.get(agent_id).and_then(|entry| {
                entry
                    .normal_result_available
                    .then_some(CompletedParallelAgent {
                        label: entry.label.clone().unwrap_or_else(|| agent_id.clone()),
                        result: entry.completed_result.clone(),
                    })
            })
        })
        .collect();
    let terminal_issues: Vec<_> = order
        .iter()
        .filter_map(|agent_id| {
            summaries.get(agent_id).and_then(|entry| {
                if !entry.terminal || entry.normal_result_available {
                    return None;
                }
                Some(TerminalParallelAgentIssue {
                    agent_id: agent_id.clone(),
                    label: entry.label.clone().unwrap_or_else(|| agent_id.clone()),
                    partial_result: entry.partial_result.clone(),
                    incomplete_reason: entry.incomplete_reason.clone(),
                    control_errors: entry.control_errors.clone(),
                })
            })
        })
        .collect();
    let unfinished: Vec<_> = order
        .iter()
        .filter_map(|agent_id| {
            summaries.get(agent_id).and_then(|entry| {
                if entry.terminal {
                    return None;
                }
                Some(UnfinishedParallelAgent {
                    agent_id: agent_id.clone(),
                    label: entry.label.clone().unwrap_or_else(|| agent_id.clone()),
                    incomplete_reason: entry.incomplete_reason.clone(),
                    control_errors: entry.control_errors.clone(),
                })
            })
        })
        .collect();

    if completed.is_empty() && terminal_issues.is_empty() && unfinished.is_empty() {
        return None;
    }

    Some(ParallelAgentBudgetRollup {
        completed,
        terminal_issues,
        unfinished,
    })
}

fn parallel_agent_budget_exhaustion_summary(
    state: &AgenticLoopState,
    cancelled_agents: &HashSet<String>,
) -> Option<String> {
    let rollup = collect_parallel_agent_budget_rollup(state)?;
    if rollup.completed.is_empty()
        && rollup.terminal_issues.is_empty()
        && rollup.unfinished.is_empty()
    {
        return None;
    }

    let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
        " The latest checkpoint was saved, so you can continue in the next message."
    } else {
        " You can continue in the next message."
    };
    let mut lines = vec![format!(
        "[The owner turn reached its execution boundary after {} agentic turn(s). {} sub-agent(s) exited with a result; {} terminated with an execution issue or without an available normal result; {} remain live. Check each result to determine whether its work is complete.{}]",
        current_agentic_step(state),
        rollup.completed.len(),
        rollup.terminal_issues.len(),
        rollup.unfinished.len(),
        checkpoint_note
    )];
    if !rollup.completed.is_empty() {
        lines.push(String::new());
        lines.push("Results from exited sub-agents:".to_string());
        for (idx, entry) in rollup.completed.iter().enumerate() {
            let detail = entry
                .result
                .as_deref()
                .map(summarize_agent_tool_budget_result)
                .unwrap_or_else(|| "exited; result was already observed".to_string());
            lines.push(format!("{}. {} — {}", idx + 1, entry.label, detail));
        }
    }
    if !rollup.terminal_issues.is_empty() {
        lines.push(String::new());
        lines.push("Terminal sub-agent issues:".to_string());
        for (idx, entry) in rollup.terminal_issues.iter().enumerate() {
            let mut detail = render_agent_tool_budget_unfinished_detail(
                entry.incomplete_reason.as_deref(),
                &entry.control_errors,
                cancelled_agents.contains(&entry.agent_id),
            );
            if let Some(partial_result) = entry.partial_result.as_deref() {
                detail.push_str("; partial result: ");
                detail.push_str(&summarize_agent_tool_budget_result(partial_result));
            }
            lines.push(format!("{}. {} — {}", idx + 1, entry.label, detail));
        }
    }
    if !rollup.unfinished.is_empty() {
        lines.push(String::new());
        lines.push("Live sub-agent results:".to_string());
        for (idx, entry) in rollup.unfinished.iter().enumerate() {
            let detail = render_agent_tool_budget_unfinished_detail(
                entry.incomplete_reason.as_deref(),
                &entry.control_errors,
                cancelled_agents.contains(&entry.agent_id),
            );
            lines.push(format!("{}. {} — {}", idx + 1, entry.label, detail));
        }
    }
    Some(lines.join("\n"))
}

pub(crate) fn unfinished_parallel_agent_ids(state: &AgenticLoopState) -> Vec<String> {
    collect_parallel_agent_budget_rollup(state)
        .map(|rollup| {
            rollup
                .unfinished
                .into_iter()
                .map(|entry| entry.agent_id)
                .collect()
        })
        .unwrap_or_default()
}

fn budget_exhaustion_completion_text(
    state: &AgenticLoopState,
    cancelled_agents: &HashSet<String>,
) -> String {
    parallel_agent_budget_exhaustion_summary(state, cancelled_agents)
        .unwrap_or_else(|| default_budget_exhaustion_completion_text(state))
}

async fn cancel_child_agents_with_timeout<H: AgenticLoopHost>(
    host: &mut H,
    runtime_tool_executor: Option<
        std::sync::Arc<crate::server::runtime_tool_executor::RuntimeToolExecutor>,
    >,
    agent_ids: Vec<String>,
    reason: &str,
    origin: CancellationOrigin,
) -> HashSet<String> {
    if agent_ids.is_empty() && runtime_tool_executor.is_none() {
        return HashSet::new();
    }
    let cancellation = async {
        if let Some(executor) = runtime_tool_executor
            && let Some(cancelled) = executor
                .cancel_child_agents(&agent_ids, reason, origin)
                .await
        {
            return cancelled;
        }
        host.cancel_child_agents(&agent_ids, reason, origin).await
    };
    match tokio::time::timeout(CHILD_AGENT_CANCEL_TIMEOUT, cancellation).await {
        Ok(cancelled) => cancelled.into_iter().collect(),
        Err(_) => {
            tracing::warn!(
                target: "astra_runtime::agentic_loop_lifecycle",
                agent_count = agent_ids.len(),
                timeout_ms = CHILD_AGENT_CANCEL_TIMEOUT.as_millis() as u64,
                "timed out while cancelling unfinished child agents"
            );
            HashSet::new()
        }
    }
}

pub(super) async fn cancel_unfinished_child_agents<H: AgenticLoopHost>(
    host: &mut H,
    state: &AgenticLoopState,
    reason: &str,
    origin: CancellationOrigin,
) -> HashSet<String> {
    let execution_lease_lost = state
        .cancellation
        .execution_lease_lost
        .as_ref()
        .is_some_and(|lost| lost.load(Ordering::Acquire));
    if execution_lease_lost && origin != CancellationOrigin::User {
        // A fenced executor no longer owns run-id-scoped descendant control:
        // those identities may already name a newer durable generation. Its
        // inherited token tree still stops local producers, while each child
        // settles through its own generation fence. A canonical user request
        // remains lineage authority and is the sole exception.
        return HashSet::new();
    }
    cancel_child_agents_with_timeout(
        host,
        state.runtime_tool_executor.clone(),
        unfinished_parallel_agent_ids(state),
        reason,
        origin,
    )
    .await
}

pub(crate) async fn resolve_cancellation_origin(
    state: &mut AgenticLoopState,
) -> CancellationOrigin {
    // User is the only run-tree cancellation authority and is monotonic once
    // observed. Runtime/Unverified are provisional execution-local
    // classifications: a durable User marker may linearize after the local
    // token fired but before the terminal CAS, so those values must be
    // reconciled again at settlement.
    if state.cancellation.resolved_origin == Some(CancellationOrigin::User) {
        return CancellationOrigin::User;
    }
    let provisional_origin = state.cancellation.resolved_origin;
    let (Some(run_control), Some(user_id), Some(run_id)) = (
        state.run_control.as_ref(),
        state.owner_user_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        // Without a durable origin provider, a lost lease or other local
        // token-only cancellation is runtime-owned: no user-request fact
        // exists.
        let origin = provisional_origin.unwrap_or(CancellationOrigin::Runtime);
        state.cancellation.resolved_origin = Some(origin);
        return origin;
    };
    let origin = match tokio::time::timeout(
        crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT,
        run_control.cancellation_origin(user_id, run_id),
    )
    .await
    {
        Ok(Ok(origin)) => origin,
        Ok(Err(error)) => {
            tracing::warn!(
                %run_id,
                %error,
                "could not prove agentic-loop cancellation origin"
            );
            provisional_origin.unwrap_or(CancellationOrigin::Unverified)
        }
        Err(_) => {
            tracing::warn!(
                %run_id,
                timeout_ms = crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT
                    .as_millis() as u64,
                "timed out proving agentic-loop cancellation origin"
            );
            provisional_origin.unwrap_or(CancellationOrigin::Unverified)
        }
    };
    state.cancellation.resolved_origin = Some(origin);
    origin
}

async fn finish_cancellation<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    origin: CancellationOrigin,
) -> PreparedTurnIteration {
    let reason = match origin {
        CancellationOrigin::User => "parent turn cancelled by user",
        CancellationOrigin::Runtime => "parent execution cancelled by runtime",
        CancellationOrigin::Unverified => CANCELLATION_ORIGIN_UNVERIFIED,
    };
    let _cancelled = cancel_unfinished_child_agents(host, state, reason, origin).await;
    try_write_heavy_checkpoint(state);
    if origin == CancellationOrigin::User {
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::UserCancelled,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(state, None),
        ));
    }
    PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
}

pub(crate) fn extract_tool_args(args: Option<&str>) -> Option<Value> {
    let args = args?;
    serde_json::from_str::<Value>(args).ok()
}

pub(crate) fn tool_record_is_workspace_mutation(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    // Structured writer tools run inside the executor that owns the bound
    // workspace.  Accept only their exact typed receipt; a model/MCP payload
    // that merely resembles this shape is not evidence.
    if record_has_typed_workspace_tool_receipt(record) {
        return true;
    }
    // `args_preview` is display-only and may be truncated or stale.  It must
    // never become historical proof that an argument-dependent tool mutated
    // the workspace; direct typed writers are handled by their tool identity
    // below, while Bash/Git require the lossless args_full contract (or the
    // typed writer evidence above).
    let args = extract_tool_args(record.authoritative_args_full());
    // Live admission treats missing arguments conservatively because safety
    // must fail closed. Historical completion evidence has the opposite
    // burden: malformed journal arguments are not proof that a mutation
    // actually happened and must not satisfy a requested-change contract.
    if args.is_none()
        && crate::turn::tool_side_effects::tool_classified_from_arguments(&record.name)
    {
        return false;
    }
    crate::turn::tool_side_effects::tool_call_records_workspace_mutation(
        &record.name,
        args.as_ref(),
    )
}

pub(crate) fn record_has_typed_workspace_tool_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record.was_executed()
        && record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE)
        && astra_tools::workspace_observation::is_typed_workspace_tool_receipt(
            record
                .workspace_mutation_receipt
                .as_ref()
                .unwrap_or(&Value::Null),
        )
        && astra_tools::executor::is_workspace_mutation_tool(
            &record.name,
            &extract_tool_args(record.authoritative_args_full()).unwrap_or(Value::Null),
        )
}

fn begin_budget_settlement(state: &mut AgenticLoopState) -> bool {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    begin_budget_settlement_for_work_state(state, active_work_attempt, false, true)
}

fn begin_deadline_budget_settlement(state: &mut AgenticLoopState) -> bool {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    // A round-slice window can outlive its available wall-clock time. Retire
    // its tool authority before projecting the deadline's narrower boundary;
    // otherwise the old window suppresses settlement and the provider is
    // repeatedly rejected for lack of final-answer budget.
    if state
        .hooks
        .completion_settlement
        .completion_action_window
        .take()
        .is_some()
    {
        state.clear_volatile(super::host::VolatileKind::FinalAnswerSettlement);
    }
    // At the final-answer deadline, there is no room for an additional
    // completion tool plus its receipt and a following model response.
    let settled = begin_budget_settlement_for_work_state(state, active_work_attempt, true, false);
    if settled {
        state.hooks.completion_settlement.wrapup_origin =
            Some(BudgetWrapupOrigin::ExecutionDeadline);
    }
    settled
}

pub(super) async fn enter_deadline_budget_settlement<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> bool {
    if state.hooks.completion_settlement.text_only
        || state.hooks.completion_settlement.work_settlement_only
        || state.budget_wrapup_injected
        || !begin_deadline_budget_settlement(state)
    {
        return false;
    }
    let _cancelled = cancel_unfinished_child_agents(
        host,
        state,
        "parent execution deadline is reserved for final settlement",
        CancellationOrigin::Runtime,
    )
    .await;
    state.final_text_model_item_id = None;
    state.final_text.clear();
    state.interruption = None;
    true
}

fn begin_budget_settlement_for_work_state(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
    allow_without_tool_calls: bool,
    allow_completion_action: bool,
) -> bool {
    let pending = match super::execution_phase::pending_completion_action(state) {
        Ok(action) => action,
        Err(error) => {
            super::execution_phase::finish_unavailable_verification(state, error);
            return false;
        }
    };
    if state.hooks.completion_settlement.text_only
        || state.hooks.completion_settlement.work_settlement_only
        || state
            .hooks
            .completion_settlement
            .completion_action_window
            .is_some()
        || state.budget_wrapup_injected
        || state.interruption.is_some()
        || (!allow_without_tool_calls && completed_tool_calls(state) == 0)
    {
        return false;
    }

    if let Some(action) = pending.as_ref()
        && allow_completion_action
        && !super::execution_phase::completion_action_window_is_batchable(state, action)
    {
        // Do not advertise a one-call window for a dependency chain that
        // cannot be completed atomically. The terminal branch records a
        // truthful incomplete result instead of inviting a misleading retry.
        return false;
    }
    state.hooks.completion_settlement.work_settlement_only = active_work_attempt;
    state.hooks.completion_settlement.wrapup_origin = Some(BudgetWrapupOrigin::RoundSlice);

    if let Some(action) = pending
        && allow_completion_action
        && super::execution_phase::completion_action_window_is_batchable(state, &action)
    {
        let tool_outcome_evidence = settlement_tool_outcome_evidence(state);
        // Reserve exactly one matching completion action and one closing
        // boundary. For an active Work attempt the second boundary is the
        // canonical settle_work_item operation; otherwise it is final text.
        // This is a typed settlement allowance, not ordinary exploration and
        // not a change to the hard review ceiling.

        state.hooks.completion_settlement.work_settlement_only = false;
        state.hooks.completion_settlement.completion_action_window = Some(CompletionActionWindow {
            action: action.clone(),
            attempts_remaining: 1,
            mismatch_corrections_remaining: 1,
            consumed: false,
            matched: false,
        });
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "typed_completion_action_available",
                "mode": if active_work_attempt { "completion_then_work_settlement" } else { "one_completion_action" },
                "allowed_action": action,
                "attempts_remaining": 1,
                "action_hint": super::execution_phase::completion_action_hint(&action),
                "declarations_may_remain_visible_for_cache": true,
                "execution_authority": "one_matching_action",
                "evidence": tool_outcome_evidence,
                "instruction": if active_work_attempt {
                    "Perform exactly one action matching the declared completion obligation, then settle the currently owned WorkItem truthfully with settle_work_item. Do not resume ordinary exploration or request an unrelated tool."
                } else {
                    "Perform exactly one action matching the declared completion obligation, then produce the final answer. Do not resume ordinary exploration or request an unrelated tool."
                },
                "authority": "typed_turn_intent_and_executed_tool_ledger",
            }),
        );
        return true;
    }

    // Preserve the number of already-consumed rounds while adding one
    // non-agentic settlement boundary. This is accounting headroom, not a
    // renewed tool budget: text_only is enforced by both schema projection and
    // tool admission.

    state.hooks.completion_settlement.text_only = !active_work_attempt;
    state.budget_wrapup_injected = !active_work_attempt;
    let tool_outcome_evidence = settlement_tool_outcome_evidence(state);
    let instruction = if active_work_attempt {
        "Exploration is no longer making sufficient progress. Settle the currently owned WorkItem now with its truthful typed outcome (delivered, blocked, or failed). Account for failed or rejected tool calls when they remain relevant, and state any evidence limitation they create. Do not request any other tool, create tasks, or delegate work."
    } else {
        "The bounded tool-execution slice is complete. Answer the user now from the evidence already gathered, mentioning failed or rejected tool calls when they remain relevant and stating any evidence limitation they create. Do not narrate this runtime boundary, request tools, create tasks, delegate work, or promise a future action (for example, `I will run` or `let me check`). If requested work remains, state it as unfinished rather than describing it as about to happen."
    };
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "completion_settlement.v2",
            "signal": "agentic_execution_slice_complete",
            "mode": if active_work_attempt { "work_settlement_only" } else { "text_only" },
            "allowed_action": if active_work_attempt { serde_json::json!("settle_work_item") } else { serde_json::Value::Null },
            "attempts_remaining": if active_work_attempt { 1 } else { 0 },
            "declarations_may_remain_visible_for_cache": true,
            "execution_authority": if active_work_attempt { "one_matching_action" } else { "none" },
            "evidence": {
                "tool_calls_completed": completed_tool_calls(state),
                "tool_outcomes": tool_outcome_evidence,
                "rounds_completed": current_agentic_step(state),
            },
            "instruction": instruction,
            "authority": "runtime_bounded_settlement",
        }),
    );
    true
}

fn reserve_budget_settlement_boundary(state: &mut AgenticLoopState) {
    let needs_work_settlement = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    let reserve = if needs_work_settlement || !state.hooks.stop_hooks.is_empty() {
        3
    } else {
        1
    };
    if state.remaining_turns <= reserve {
        let _ = begin_budget_settlement(state);
    }
}

/// Renew an internal scheduling slice without changing an explicit lifetime
/// limit or erasing consumed iterations. One owner updates the same counters
/// persisted in the execution checkpoint; renewal does not depend on model
/// claims or inferred progress.
pub(super) fn prepare_iteration_budget(
    state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    if state.max_turns == 0 || state.remaining_turns > state.max_turns {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "invalid execution slice accounting",
        ));
    }
    let consumed = usize::try_from(state.charged_iterations)
        .map_err(|_| "execution iteration accounting exceeds runtime capacity".to_string())?;
    let limit = state
        .agentic_turn_budget
        .hard_turn_limit
        .map(|limit| limit.get());
    let mut boundary = limit.map_or(state.max_turns, |limit| state.max_turns.min(limit));
    let uncharged = boundary
        .checked_sub(consumed)
        .ok_or_else(|| "charged iterations exceed the admitted execution boundary".to_string())?;
    let mut remaining = state.remaining_turns.min(uncharged);
    // Keep enough space for work plus its existing settlement boundary. A
    // soft slice must not activate final-answer or child-cancellation policy.
    if remaining <= 3 {
        let slice = state.agentic_turn_budget.initial_turns.max(4);
        let extension = limit.map_or(slice, |limit| slice.min(limit - boundary));
        boundary = boundary
            .checked_add(extension)
            .ok_or_else(|| "execution iteration boundary overflow".to_string())?;
        remaining = remaining
            .checked_add(extension)
            .ok_or_else(|| "execution iteration accounting overflow".to_string())?;
    }
    state.max_turns = boundary;
    state.remaining_turns = remaining;
    Ok(())
}

/// Unified diagnosis of all stall conditions evaluated at interruption time.
///
/// Computes the signal string, human-readable summary, and restricted tool
/// list in a single pass to avoid re-evaluating the same stall
/// observations across three separate functions.
struct StallDiagnosis {
    signal: Option<String>,
    summary: Option<String>,
    restricted_tools: Vec<String>,
}

fn compute_stall_diagnosis(state: &AgenticLoopState) -> StallDiagnosis {
    // 1. Exploration family streak (read/search/diff dominance)
    if let Some((family, streak)) =
        astra_turn_core::evaluation::exploration_family_round_streak(&state.stall.tool_call_records)
        && streak >= astra_turn_core::evaluation::EXPLORATION_FAMILY_CHURN_THRESHOLD
    {
        let summary_text = match family {
            "read" => format!("{streak} consecutive read-dominant exploratory rounds"),
            "search" => format!("{streak} consecutive search-dominant exploratory rounds"),
            "diff" => format!("{streak} consecutive diff-dominant exploratory rounds"),
            _ => format!("{streak} consecutive {family}-dominant exploratory rounds"),
        };
        let summary = Some(summary_text);
        return StallDiagnosis {
            signal: Some(format!("exploration_family={family};streak={streak}")),
            summary,
            restricted_tools: Vec::new(),
        };
    }

    StallDiagnosis {
        signal: None,
        summary: None,
        restricted_tools: Vec::new(),
    }
}

/// Build an interruption state summary from the current loop state.
pub(crate) fn interruption_state_summary(
    state: &AgenticLoopState,
    error_detail: Option<String>,
) -> InterruptionStateSummary {
    let diag = compute_stall_diagnosis(state);
    InterruptionStateSummary {
        has_checkpoint: state.stall.last_heavy_checkpoint.is_some(),
        tool_calls_completed: completed_tool_calls(state),
        turns_completed: current_agentic_step(state),
        remaining_turns: state.remaining_turns as u32,
        error_detail,
        stall_signal: diag.signal,
        resume_restricted_tools: diag.restricted_tools,
    }
}

pub(crate) fn interruption_diagnosis_summary(state: &AgenticLoopState) -> Option<String> {
    compute_stall_diagnosis(state).summary
}

pub(crate) fn mark_work_settlement_incomplete(state: &mut AgenticLoopState) {
    if state.final_text.is_empty() {
        state.final_text_model_item_id = None;
        state.final_text = super::host::WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT.to_string();
    }
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some("canonical Work still had an unsettled item at the terminal boundary".into()),
        ),
    ));
}

pub(crate) async fn run_loop_preamble(state: &mut AgenticLoopState) {
    // This flag is a turn outcome consumed by the canonical commit after the
    // agentic loop returns. Reset it when a new turn actually starts, not
    // during finalization, so prefix rewrites remain observable to commit.
    state.context_compression_triggered = false;

    if let Some(context) = state.permission_context.as_ref()
        && context.read().await.inherited.read_only_execution
    {
        state.skills.tool_event_hooks.disable_execution();
        state.skills.session_event_hooks.disable_execution();
    }

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
            state.push_volatile_payload(
                super::host::VolatileKind::SessionHookContext,
                serde_json::json!({
                    "event": "session_start",
                    "context": ctx,
                }),
            );
        }
        for (key, value) in hook_output.env_vars {
            astra_core::session_env_overlay::set(&key, &value);
            state.skills.session_event_hooks.note_environment_applied();
        }
    }
}

/// Host-local schema installation is needed for a new executor even when its
/// logical turn has already completed the side-effecting preamble.
pub(crate) fn configure_loop_host<H: AgenticLoopHost>(host: &mut H, state: &AgenticLoopState) {
    for diagnostic in state
        .skills
        .tool_event_hooks
        .take_config_diagnostics()
        .into_iter()
        .chain(state.skills.session_event_hooks.take_config_diagnostics())
    {
        host.emit_hook_config_diagnostic(&diagnostic);
    }

    if let Some(resolver) = &state.skills.resolver {
        // The canonical skill schema is byte-stable — it
        // takes no skill list and advertises `skill_name` as an open
        // string. The catalog is surfaced via `<available_skills>` in the
        // session-cached prompt prefix (see `build_skill_listing_section`),
        // so adding/removing a skill no longer perturbs the tool schema
        // bytes.
        if !resolver.available_skills().is_empty() || resolver.has_dynamic_catalog() {
            host.inject_tool_schema(astra_tools::schemas::skill_tool_schema());
        }
        if !resolver.available_skills().is_empty() {
            host.inject_tool_schema(astra_tools::schemas::discover_skills_tool_schema());
        }
    }

    // NOTE: Cross-Session Project Context used to be injected into
    // `state.messages` here as a system message. It has moved into the
    // context pipeline's `ProjectContext` section (bound from
    // `SessionContext.project_context`) so it sits in `CacheScope::Session`
    // BEFORE the Session→None marker — now it participates in the cached
    // session prefix instead of being re-sent after the marker every turn.
    // See `context_pipeline_adapter::build_session_context` + `bind_project_context`.
}

fn apply_structured_user_reanchor(
    state: &mut AgenticLoopState,
    relation: astra_turn_types::ObjectiveRelation,
    user_message: &str,
) -> bool {
    if user_message.trim().is_empty() {
        return false;
    }

    state.turn_guard.begin_fresh_user_turn();
    // Hard capability/permission restrictions are owned by their boundary and
    // must survive a semantic re-anchor. Behavioral state can reset here, but
    // user intent must not broaden the executable capability surface.

    if let Some(session) = state.pipeline_session.as_mut() {
        match relation {
            astra_turn_types::ObjectiveRelation::Replace => session
                .working_memory_mut()
                .apply_objective_replacement(user_message),
            astra_turn_types::ObjectiveRelation::Correct => session
                .working_memory_mut()
                .apply_user_correction(user_message),
            astra_turn_types::ObjectiveRelation::Unknown
            | astra_turn_types::ObjectiveRelation::Acknowledge
            | astra_turn_types::ObjectiveRelation::Continue
            | astra_turn_types::ObjectiveRelation::Refine => {}
        }
    }
    true
}

fn apply_structured_user_feedback(
    state: &mut AgenticLoopState,
    intent: &TurnIntent,
    user_message: &str,
) -> bool {
    let Some(feedback) = intent.feedback else {
        return false;
    };
    // The re-anchor path already records corrections while resetting stale
    // transient state. Avoid duplicating the same exact feedback entry.
    if intent.reanchors_current_objective()
        && feedback.kind == astra_turn_types::UserFeedbackKind::Correction
    {
        return false;
    }
    let Some(session) = state.pipeline_session.as_mut() else {
        return false;
    };
    session
        .working_memory_mut()
        .apply_user_feedback(feedback, user_message);
    true
}

/// Estimate context pressure from raw message token count.
///
/// Returns `(pressure, estimated_tokens)` where pressure is a 0.0–1.0
/// ratio against `max_turn_input_tokens`. When no limit is configured
/// (`max_turn_input_tokens == 0`) returns `(0.0, 0)`.
#[inline]
#[cfg(test)]
pub(crate) fn estimate_context_pressure(
    messages: &[serde_json::Value],
    pinned_tool_schema_tokens: usize,
    max_turn_input_tokens: u64,
) -> (f64, u64) {
    estimate_context_pressure_with_system_prompt_tokens(
        messages,
        pinned_tool_schema_tokens,
        max_turn_input_tokens,
        None,
    )
}

/// Estimate pressure using the last prompt assembly measured by the shared
/// provider-context manifest when one is available.
///
/// The canonical history does not contain the assembled system prompt, so a
/// pressure estimate needs that prompt overhead exactly once.  Before the
/// first provider request the generic estimator's conservative fallback is
/// unavoidable; after a request, reusing the typed manifest avoids charging
/// the old fixed-size approximation on every later round.  This affects only
/// the local compaction decision — the final provider wire estimate remains
/// authoritative.
#[inline]
pub(crate) fn estimate_context_pressure_for_state(state: &AgenticLoopState) -> (f64, u64) {
    estimate_context_pressure_with_system_prompt_tokens(
        &state.messages,
        state.pinned_tool_schema_tokens as usize,
        state.max_turn_input_tokens,
        crate::prompts::measured_prompt_tokens_from_manifest(
            state.last_llm_context_manifest_trace.as_ref(),
        ),
    )
}

#[inline]
pub(crate) fn estimate_context_pressure_with_system_prompt_tokens(
    messages: &[serde_json::Value],
    pinned_tool_schema_tokens: usize,
    max_turn_input_tokens: u64,
    system_prompt_tokens: Option<usize>,
) -> (f64, u64) {
    if max_turn_input_tokens == 0 {
        return (0.0, 0);
    }
    let tokens = crate::prompts::estimate_tokens(
        messages,
        pinned_tool_schema_tokens,
        system_prompt_tokens.unwrap_or(0),
    ) as u64;
    (tokens as f64 / max_turn_input_tokens as f64, tokens)
}

// AgenticLoopState currently carries the effective input budget, not the full
// model context window. This approximation is used only for skill-listing size
// hints; callers with registry metadata should pass the full context window.
fn approximate_context_window_from_effective_input_budget(
    max_turn_input_tokens: u64,
) -> Option<u32> {
    if max_turn_input_tokens == 0 {
        return None;
    }
    let approx_context_window = max_turn_input_tokens.saturating_mul(10).div_ceil(8);
    Some(approx_context_window.min(u64::from(u32::MAX)) as u32)
}

/// Run the compaction pipeline and record results (event + audit).
///
/// Shared by pre-turn and resume. Exact deduplication runs first; the existing
/// durable artifact owner is used only if pressure remains.
async fn run_proactive_compaction<H: AgenticLoopHost>(
    pressure: f64,
    tokens_measured: u64,
    state: &mut AgenticLoopState,
    host: &mut H,
    kind: CompactionKind,
    audit_label: &str,
) {
    let max_tokens = state.max_turn_input_tokens;
    let budget = TokenBudget {
        max_prompt_tokens: max_tokens,
        last_measured_tokens: tokens_measured,
        current_round_index: Some(state.current_round_index),
        now_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let pipeline = if pressure >= CompactionTier::aggressive_trigger(max_tokens) {
        CompactionEngine::aggressive_pipeline()
    } else {
        CompactionEngine::default_pipeline_for(max_tokens)
    };
    let messages_before = state.messages.len();
    let Some(rewrite_permit) = state
        .begin_canonical_rewrite()
        .filter(|permit| permit.is_valid())
    else {
        return;
    };
    let mut outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
    if outcome.total_tokens_freed > 0 {
        state.finish_canonical_rewrite(Some(rewrite_permit));
    }
    let remaining = budget
        .last_measured_tokens
        .saturating_sub(outcome.total_tokens_freed);
    let still_under_pressure = budget.max_prompt_tokens > 0
        && remaining as f64 / budget.max_prompt_tokens as f64
            >= CompactionTier::pre_turn_trigger(budget.max_prompt_tokens);
    if still_under_pressure
        && let Some((freed, removed)) =
            super::execution_phase::spill_old_messages_to_artifact(host, state).await
    {
        outcome.total_tokens_freed += freed;
        outcome.layer_results.push((
            "history_artifact".into(),
            astra_turn_core::compression_types::CompressionResult {
                estimated_tokens_freed: freed,
                messages_removed: removed,
                ..Default::default()
            },
        ));
    }
    state
        .compaction_effectiveness
        .record_compaction(outcome.total_tokens_freed);
    if outcome.total_tokens_freed > 0 {
        let messages_after = state.messages.len();
        let messages_removed = messages_before.saturating_sub(messages_after);
        let layer_descriptions: Vec<String> = outcome
            .layer_results
            .iter()
            .map(|(name, r)| format!("{}: ~{} tokens", name, r.estimated_tokens_freed))
            .collect();
        state.context_compression_triggered = true;
        state.step_recorder.record_compaction_with_kind(
            &kind.to_string(),
            messages_removed.min(u32::MAX as usize) as u32,
            outcome.total_tokens_freed,
            pressure,
        );
        let event = CompactionEvent::new(
            kind,
            pressure,
            outcome.total_tokens_freed,
            tokens_measured,
            max_tokens,
            messages_removed,
            messages_after,
            layer_descriptions,
        );
        host.on_compaction(event);
        if let Some(ref mut sess) = state.pipeline_session {
            sess.record_compaction_audit(
                audit_label,
                outcome.layer_results.len() as u32,
                outcome.total_tokens_freed.min(u32::MAX as u64) as u32,
            );
            sess.stats.record_compaction(outcome.total_tokens_freed);
        }
    }
}

pub(crate) async fn prepare_turn_iteration<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
) -> Result<PreparedTurnIteration, astra_core::ClassifiedError> {
    let quiet = host.is_quiet();

    // Outer loop: re-evaluate pause/cancel state until we either get
    // cancelled or the run is no longer paused. This replaces the prior
    // recursive `Box::pin(prepare_turn_iteration(...)).await` which
    // would stack-overflow during long pause windows.
    loop {
        if wait_for_pause_clear_or_cancel(host, state).await? {
            let origin = resolve_cancellation_origin(state).await;
            return Ok(finish_cancellation(host, state, origin).await);
        }

        // Fast-path in-memory cancel check (same pod)
        let in_memory_cancelled = state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
            || state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());

        // Server's live watcher owns cross-pod observation. Local flags are
        // already checked above; profiles without a watcher retain polling.
        let durable_control_status = if !in_memory_cancelled && !state.has_durable_control_watcher()
        {
            if let Some(ref rc) = state.run_control {
                if let (Some(user_id), Some(run_id)) = (
                    state.owner_user_id.as_deref(),
                    state.current_run_id.as_deref(),
                ) {
                    match rc.control_status(user_id, run_id).await {
                        Ok(status) => status,
                        Err(error) => {
                            tracing::warn!(
                                run_id,
                                error = %error,
                                "failed to poll run control status"
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if in_memory_cancelled
            || matches!(durable_control_status, Some(RunControlStatus::Cancelled))
        {
            if matches!(durable_control_status, Some(RunControlStatus::Cancelled)) {
                if let Some(ref flag) = state.cancellation.flag {
                    flag.store(true, Ordering::SeqCst);
                }
                if let Some(ref token) = state.cancellation.token {
                    token.cancel();
                }
            }
            let origin = resolve_cancellation_origin(state).await;
            return Ok(finish_cancellation(host, state, origin).await);
        }

        if matches!(durable_control_status, Some(RunControlStatus::Paused)) {
            if let Some(ref flag) = state.cancellation.pause_flag {
                flag.store(true, Ordering::SeqCst);
            }
            // Loop back to top and re-enter the while-pause loop instead of
            // recursing (prevents stack overflow).
            continue;
        }

        // Clean state: no cancel, no pause — proceed to turn preparation.
        break;
    }

    // Cancellation and pause have priority over round control. Capture the
    // permission selection before any mode-dependent preparation begins.
    super::settings_boundary::apply_round_settings(host, state).await?;

    if let Err(error) = super::execution_phase::missing_explicit_verification_hooks(state) {
        super::execution_phase::finish_unavailable_verification(state, error);
        try_write_heavy_checkpoint(state);
        return Ok(PreparedTurnIteration::Finished(
            AgenticLoopOutcome::Completed,
        ));
    }

    if host
        .execution_time_budget_remaining()
        .is_some_and(|remaining| !remaining.has_work())
    {
        let settlement_already_active = state.hooks.completion_settlement.text_only
            || state.hooks.completion_settlement.work_settlement_only
            || state.budget_wrapup_injected;
        if !settlement_already_active {
            if !enter_deadline_budget_settlement(host, state).await {
                let _cancelled = cancel_unfinished_child_agents(
                    host,
                    state,
                    "parent execution deadline expired before obligations could settle",
                    CancellationOrigin::Runtime,
                )
                .await;
                if state.interruption.is_none() {
                    state.final_text_model_item_id = None;
                    state.final_text = "The execution deadline arrived before the remaining verification could be safely completed. Progress is preserved, but the requested result is incomplete.".into();
                    state.final_text_streamed = false;
                    state.interruption = Some(InterruptionRecord::new(
                        InterruptionKind::ExecutionIncomplete,
                        ResumeAction::ContinueImmediately,
                        interruption_state_summary(
                            state,
                            Some("safe settlement could not fit within the remaining execution deadline".into()),
                        ),
                    ));
                }
                try_write_heavy_checkpoint(state);
                return Ok(PreparedTurnIteration::Finished(
                    AgenticLoopOutcome::Completed,
                ));
            }
        }
    }

    // Workspace ownership is separate from an invocation's outcome. An
    // unknown outcome can reach the model when custody permits; unsettled
    // ownership still prevents access to a workspace that may be in use.
    if super::execution_phase::reject_unsettled_execution_completion(state) {
        try_write_heavy_checkpoint(state);
        return Ok(PreparedTurnIteration::Finished(
            AgenticLoopOutcome::Completed,
        ));
    }

    // Corrupt accounting is not ordinary budget exhaustion. Keep the
    // original state intact so recovery cannot hide a wrapped counter.
    state
        .charged_iterations
        .checked_add(1)
        .ok_or_else(|| "execution iteration accounting overflow".to_string())?;
    prepare_iteration_budget(state)?;
    if state.remaining_turns == 0 {
        let cancelled = cancel_child_agents_with_timeout(
            host,
            state.runtime_tool_executor.clone(),
            unfinished_parallel_agent_ids(state),
            "owner execution boundary reached",
            CancellationOrigin::Runtime,
        )
        .await;
        let active_work = state.runtime_tool_executor.as_deref().is_some_and(
            crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
        );
        if active_work {
            mark_work_settlement_incomplete(state);
        } else {
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::BudgetExhausted,
                ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
            if state.final_text.is_empty() {
                let settlement = &state.hooks.completion_settlement;
                if let Some(text) = settlement
                    .latest_provider_text
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                {
                    state.final_text = text.clone();
                    state.final_text_model_item_id =
                        settlement.latest_provider_text_model_item_id.clone();
                } else {
                    state.final_text = budget_exhaustion_completion_text(state, &cancelled);
                    state.final_text_model_item_id = None;
                }
            }
            state.final_text_streamed = false;
        }
        try_write_heavy_checkpoint(state);
        return Ok(PreparedTurnIteration::Finished(
            AgenticLoopOutcome::Completed,
        ));
    }

    reserve_budget_settlement_boundary(state);

    match state.rate_limit_cooldown.check_request() {
        astra_turn_core::rate_limit_cooldown::RateLimitAction::Proceed => {}
        astra_turn_core::rate_limit_cooldown::RateLimitAction::WaitAndRetry { delay_ms } => {
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
        astra_turn_core::rate_limit_cooldown::RateLimitAction::Reject {
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
                state.final_text_model_item_id = None;
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
                        delay_seconds: Some(reset_in_ms / 1000),
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
                    delay_seconds: Some(reset_in_ms / 1000),
                },
                interruption_state_summary(state, Some(format!("Rate limit: {}", reason.as_str()))),
            ));
            return Err(format!(
                "Rate limit cooldown active ({}). Resets in ~{secs}s. Please wait and retry.",
                reason.as_str(),
            )
            .into());
        }
    }

    // Preserve cancellation, pause and no-round exits before touching the WAL.
    // Recovery must still precede semantic admission and resume compaction.
    // Durable hosts make repeated calls a no-op once recovery succeeds.
    host.hydrate_restored_history(state).await?;

    state.charged_iterations = state
        .charged_iterations
        .checked_add(1)
        .ok_or_else(|| "execution iteration accounting overflow".to_string())?;
    state.remaining_turns = state.remaining_turns.saturating_sub(1);
    state
        .step_recorder
        .begin_turn_with_context(session_turn_number(state), turn_index as u32);

    let turn_start_time = Instant::now();
    tracing::debug!(
        target: "astra_timing",
        turn_index = turn_index,
        session_turn = session_turn_number(state),
        "agentic turn started"
    );
    if state.turn_event_buffer.is_none() {
        state.turn_event_buffer = Some(
            astra_services::session_journal::TurnEventBuffer::begin_turn(
                state.current_session_id.as_deref(),
                session_turn_number(state),
            ),
        );
    }

    if let (Some(buffer), Some(sender), Some(owner), Some(session), Some(generation)) = (
        state.turn_event_buffer.as_mut(),
        state.telemetry.trace_ingestion.clone(),
        state.owner_user_id.as_deref(),
        state.current_session_id.as_deref(),
        state.current_run_owner_generation,
    ) {
        if let Err(error) = buffer.bind_trace_ingestion(owner, session, generation, sender) {
            tracing::warn!(error, "turn trace sink binding rejected");
        }
    }

    if let (Some(hub), Some(session)) = (
        &state.telemetry.observability_hub,
        &state.telemetry.observability_session,
    ) {
        let session_id = state.current_session_id.as_deref().unwrap_or("");
        let user_id = {
            let s = astra_core::sync_poison::recover_rwlock_read(session);
            s.user_id.clone()
        };
        crate::observability::on_turn_start(hub, session_id, &user_id, &state.message);
    }
    // TurnIntent describes the submitted user turn, not an individual model
    // round. Re-running an auxiliary judge after every tool round adds
    // unbounded latency and can overwrite the semantics that governed the
    // first round. A resumed non-zero round preserves the restored state.
    if turn_index == 0 {
        let admission_started_at = Instant::now();
        if !host.owns_semantic_admission_timing() {
            host.on_turn_phase_started(
                state,
                TurnPhaseKind::SemanticAdmission,
                0,
                0,
                admission_started_at,
            );
        }
        crate::turn::agentic::turn_intent::capture_turn_intent_context(state);
        let outcome = host.judge_turn_intent(state).await;
        // Asynchronous hosts publish receipts from their actual task owner.
        // A baseline or unavailable dispatch is not another timed task.
        if !host.owns_semantic_admission_timing()
            && let Some(phase_outcome) = outcome.terminal_phase_outcome()
        {
            complete_turn_phase(
                host,
                state,
                admission_started_at,
                TurnPhaseKind::SemanticAdmission,
                0,
                0,
                phase_outcome,
                "turn_intent_admission_0".to_string(),
            );
        }
        match outcome {
            TurnIntentJudgeOutcome::Pending => {
                // The host publishes the eventual decision; pending is not failure.
                state.turn_intent = None;
            }
            TurnIntentJudgeOutcome::FixedDefault => {
                // The baseline profile is a text-independent fixed default. Do not
                // retain or synthesize judge-owned semantics without a typed LLM
                // result.
                state.turn_intent = None;
            }
            TurnIntentJudgeOutcome::Unavailable => {
                tracing::debug!(
                    "turn intent judge unavailable; preserving current runtime profile"
                );
            }
        }
    }
    host.prepare_model_selection(state).await?;
    if let Some(prompt) =
        astra_turn_core::stop_hooks::build_stop_hook_prompt(&state.hooks.stop_hooks)
        && let Some(content) = prompt.get("content").and_then(Value::as_str)
    {
        state.push_volatile(
            super::host::VolatileKind::StopHookEvidence,
            content.to_string(),
        );
    }

    if (state.telemetry.observability_session.is_some() || state.skills.resolver.is_some())
        && state.telemetry.turn_trace_collector.is_none()
    {
        let capture = std::env::var("ASTRA_CAPTURE_TRACES")
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

    let _ = super::execution_phase::runtime_input_boundary(
        host,
        state,
        super::execution_phase::RuntimeInputBoundary::Preparation,
    )
    .await?;
    if let Some(resolver) = &state.skills.resolver {
        // Phase-9: skill listing moves from per-turn volatile to
        // session-stable. The full skill catalog is rendered via
        // CacheScope::Session — the resolved turn input budget sizes the
        // listing so small-context providers do not waste prompt space and
        // large-context providers are not silently capped by model-name
        // fallbacks.
        //
        // We still populate `listing_message` as a rendered `role: system`
        // value for downstream adapters (introspect tooling, tests) so
        // they don't need to know about the cache-scope plumbing.
        let full = resolver.available_skills();
        if state.skills.listing_message.is_none() {
            state.skills.listing_message = if full.is_empty() {
                None
            } else {
                let agent_spawn_available = host
                    .capabilities()
                    .has(astra_turn_core::capability::Capability::AgentSpawner);
                let edge_skills = full
                    .iter()
                    .map(|skill| {
                        serde_json::json!({
                            "name": skill.name,
                            "version": null,
                            "description": skill.description,
                            "when_to_use": skill.when_to_use,
                            "aliases": skill.aliases,
                        })
                    })
                    .collect::<Vec<_>>();
                crate::prompts::build_skill_listing_section_with_context_window_and_caps(
                    &full,
                    approximate_context_window_from_effective_input_budget(
                        state.max_turn_input_tokens,
                    ),
                    agent_spawn_available,
                )
                .map(|section| {
                    serde_json::json!({
                        "role": "system",
                        "content": section.text,
                        "edge_skills": edge_skills,
                    })
                })
            };
        }
    }
    maybe_pre_route_skill(host, state).await;

    if turn_index > 0 || state.messages.len() > 10 {
        // Context pressure estimation + adaptive compaction.
        // When pipeline_session is active, use its pressure model (predictive
        // with reserves) and cascade-aware limits. Otherwise fall back to
        // legacy inline estimation.
        // The last provider measurement includes protocol-owned input which
        // the local text estimator cannot see. Use it until a new request is
        // measured, discounting only the savings from rewrites in this pass.
        let mut measured_floor = state.last_measured_prompt_tokens.unwrap_or(0);
        let pressure_with_measurement = |state: &AgenticLoopState, floor: u64| {
            let (pressure, estimate) = estimate_context_pressure_for_state(state);
            if state.max_turn_input_tokens == 0 || estimate >= floor {
                (pressure, estimate)
            } else {
                (floor as f64 / state.max_turn_input_tokens as f64, floor)
            }
        };
        let (mut pressure, mut pressure_estimate_tokens) =
            pressure_with_measurement(state, measured_floor);

        // Pre-turn LLM compact: if pressure exceeds the model-adaptive
        // trigger, let the host run an optional cache-friendly inline-summary
        // pass before the next LLM call.
        if pressure >= CompactionTier::pre_turn_trigger(state.max_turn_input_tokens)
            && state.messages.len() > 10
        {
            if let Some(event) = host.maybe_pre_turn_compact(state, pressure).await {
                measured_floor = measured_floor.saturating_sub(event.tokens_freed);
                state.context_compression_triggered = true;
                state
                    .compaction_effectiveness
                    .record_compaction(event.tokens_freed);
                state.step_recorder.record_compaction_with_kind(
                    &event.kind.to_string(),
                    event.messages_removed.min(u32::MAX as usize) as u32,
                    event.tokens_freed,
                    event.pressure,
                );
                if let Some(ref mut sess) = state.pipeline_session {
                    let audit_label = event.kind.to_string();
                    sess.record_compaction_audit(
                        &audit_label,
                        event.messages_removed.min(u32::MAX as usize) as u32,
                        event.tokens_freed.min(u64::from(u32::MAX)) as u32,
                    );
                    sess.stats.record_compaction(event.tokens_freed);
                }
                host.on_compaction(event);
                (pressure, pressure_estimate_tokens) =
                    pressure_with_measurement(state, measured_floor);
            }
        }

        // Pre-turn pressure warning: when context is near the model-adaptive
        // warning threshold, emit a non-intrusive advisory so the user knows
        // compaction is imminent.
        if pressure >= CompactionTier::pre_turn_warning(state.max_turn_input_tokens) {
            let warning = CompactionEvent::new(
                CompactionKind::PressureWarning,
                pressure,
                0, // no tokens freed yet
                pressure_estimate_tokens,
                state.max_turn_input_tokens,
                0,                    // no messages removed
                state.messages.len(), // current message count
                vec![],               // no layers applied
            );
            host.on_compaction(warning);
        }

        // Canonical history is shortened by the semantic-summary or durable
        // history-artifact owner. A local tool-result file does not establish
        // that another Server instance can retrieve the omitted evidence.
        // Re-estimate pressure after the semantic-summary attempt.
        // Include selected schema and measured system overhead in this check.
        let (post_mc_pressure, post_mc_tokens) = pressure_with_measurement(state, measured_floor);

        // Proactive compression gate: if pressure is still high,
        // try a recoverable history rewrite *before* calling
        // the LLM, preventing 413 errors instead of reacting to them.
        if post_mc_pressure >= CompactionTier::pre_turn_trigger(state.max_turn_input_tokens) {
            let is_aggressive =
                post_mc_pressure >= CompactionTier::aggressive_trigger(state.max_turn_input_tokens);
            let (kind, label) = if turn_index == 0 {
                (CompactionKind::Resume, "resume")
            } else if is_aggressive {
                (CompactionKind::ProactiveAggressive, "aggressive")
            } else {
                (CompactionKind::ProactiveDefault, "default")
            };
            run_proactive_compaction(post_mc_pressure, post_mc_tokens, state, host, kind, label)
                .await;
        }
    }

    Ok(PreparedTurnIteration::Ready(TurnIterationPrep {
        quiet,
        turn_start_time,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astra_config::user_profile::{Scenario, TurnIntent, WorkspaceMutationIntent};
    use astra_services::session_journal::ToolCallRecord;
    use astra_skills::hooks::SkillHooks;
    use astra_skills::manifest::{ExecutionContext, SkillSourceKind, TrustTier};
    use astra_skills::traits::{ResolvedSkill, SkillResolver, SkillToolInfo};
    use serde_json::json;

    use crate::turn::agentic_loop::host::VolatileKind;
    use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
    use crate::turn::agentic_loop::host::tests::{
        MockHost, make_hub, make_session, make_state, text_result,
    };

    use super::*;

    #[tokio::test]
    async fn exact_duplicates_relieving_pressure_do_not_write_a_history_artifact() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 32_000;
        state.messages = vec![json!({"role":"user","content":"Inspect the same file"})];
        for index in 0..32 {
            let id = format!("call-{index}");
            state
                .messages
                .push(json!({"role":"assistant","tool_calls":[{
                    "id":id,"type":"function","function":{"name":"read_file","arguments":"{}"}
                }]}));
            state.messages.push(
                json!({"role":"tool","tool_call_id":id,"content":"same evidence ".repeat(200)}),
            );
        }
        super::super::host::tests::admit_history_recovery(&mut host, &mut state);
        prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        assert!(state.context_compression_triggered);
        assert!(
            host.history_artifacts.is_empty(),
            "sufficient dedup must not schedule a database write"
        );
        assert!(
            estimate_context_pressure_for_state(&state).0
                < CompactionTier::pre_turn_trigger(state.max_turn_input_tokens)
        );
    }

    #[tokio::test]
    async fn failed_pre_turn_summary_keeps_unsummarized_tool_facts() {
        let mut state = make_state();
        state.messages = vec![
            json!({"role":"user","content":"Continue the long task using the confirmed resource receipts."}),
        ];
        for round in 0..32 {
            state.messages.push(json!({"role":"assistant","content":"", "tool_calls":[{"id":format!("receipt-{round}"),"type":"function","function":{"name":"read_file","arguments":"{}"}}]}));
            state.messages.push(json!({"role":"tool","tool_call_id":format!("receipt-{round}"),"content":format!("confirmed-resource-{round}: {}", "abcdefghij".repeat(12))}));
        }
        state.initialize_canonical_rewrite_proof(
            &state.messages.clone(),
            &astra_turn_types::canonical_conversation_root(&state.messages),
            0,
        );
        let prior = state.messages.clone();
        state.max_turn_input_tokens = 2_000;
        let mut host = MockHost::new(vec![text_result("done", 10, 5, None)]);
        prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        assert!(
            state.messages.iter().any(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("confirmed-resource-3:"))),
            "failed summary deleted an unsummarized confirmed resource: {}",
            serde_json::to_string(&state.messages).unwrap()
        );
        let mut terminal = state.messages.clone();
        terminal
            .push(json!({"role":"assistant","content":"Continue after preserving the evidence"}));
        let (mode, _) = crate::turn::canonical_commit::canonical_commit_delta(
            &prior,
            true,
            &terminal,
            state.canonical_rewrite_proof(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(mode, astra_turn_types::CanonicalDeltaModeV1::Append);
    }

    #[tokio::test]
    async fn hook_config_diagnostics_reach_loop_output_once_without_execution() {
        for (filename, content, field) in [
            (
                "hooks.json",
                r#"{"hooks":[{"event":"unknown-secret","action":{"type":"shell","command":"never execute this"}}]}"#,
                "hooks[0].event",
            ),
            (
                "hooks.json",
                r#"{"hooks":[{"event":"pre_tool_use","matchr":"secret","action":{"type":"set_env","key":"UNUSED","value":"env-secret"}}]}"#,
                "hooks[0]",
            ),
            (
                "hooks.json",
                r#"{"session_hooks":[{"event":"session_start","action":{"type":"set_env","key":"UNUSED","value":"env-secret","vaule":"secret"}}]}"#,
                "session_hooks[0].action",
            ),
            (
                "hooks.json",
                r#"{"session_hooks":[{"event":"turn_end","action":{"type":"set_env","key":"UNUSED","value":"env-secret"}}]}"#,
                "session_hooks[0].event",
            ),
            ("hooks.yaml", "hooks: []\nunknown-secret: secret\n", "($)"),
            (
                "hooks.yaml",
                "session_hooks:\n  - event: session_start\n    action:\n      type: set_env\n      key: UNUSED\n      value: env-secret\n      vaule: secret\n",
                "session_hooks[0].action",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join(".astra")).unwrap();
            std::fs::write(dir.path().join(".astra").join(filename), content).unwrap();
            let mut state = make_state();
            (
                state.skills.tool_event_hooks,
                state.skills.session_event_hooks,
            ) = crate::skills::hooks::load_all_hooks(dir.path());
            state.skills.tool_event_hooks.disable_execution();
            state.skills.session_event_hooks.disable_execution();
            let mut host = MockHost::new(vec![text_result("done", 10, 5, None)]);
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            assert!(outcome.is_ok(), "{outcome:?}");
            let diagnostics = host
                .emitted_lines
                .iter()
                .filter(|line| line.starts_with("Hook configuration"))
                .collect::<Vec<_>>();
            assert_eq!(diagnostics.len(), 1);
            assert!(diagnostics[0].contains(&format!(".astra/{filename}")));
            assert!(diagnostics[0].contains(field), "{}", diagnostics[0]);
            assert!(!diagnostics[0].contains("secret"));
            configure_loop_host(&mut host, &state);
            assert_eq!(
                host.emitted_lines
                    .iter()
                    .filter(|line| line.starts_with("Hook configuration"))
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn restored_verification_error_stops_before_budget_or_provider_admission() {
        for active_work in [false, true] {
            let mut state = make_state();
            state.stall.verification_frontier =
                super::super::verification_frontier::tests::restored_read_only_prefix();
            state.hooks.stop_hooks.push(astra_turn_types::StopHook {
                label: "new contract".into(),
                command: "./new-check".into(),
                working_dir: None,
                depends_on: Vec::new(),
                timeout_secs: None,

                authoritative: true,
            });
            state.max_turns = 50;
            state.remaining_turns = 0;
            state.charged_iterations = 50;
            assert!(!begin_budget_settlement_for_work_state(
                &mut state,
                active_work,
                false,
                true,
            ));
            let mut host = MockHost::new(Vec::new());
            assert!(matches!(
                prepare_turn_iteration(&mut host, &mut state, 50)
                    .await
                    .unwrap(),
                PreparedTurnIteration::Finished(_)
            ));
            assert_eq!(
                (
                    state.max_turns,
                    state.remaining_turns,
                    state.charged_iterations
                ),
                (50, 0, 50)
            );
            assert_eq!(
                state.interruption.as_ref().map(|record| record.kind),
                Some(InterruptionKind::ExecutionIncomplete)
            );
            assert!(
                state
                    .hooks
                    .completion_settlement
                    .completion_action_window
                    .is_none()
            );
            assert_eq!(state.llm_rounds_completed, 0);
        }
    }

    #[tokio::test]
    async fn heavy_checkpoint_preserves_renewed_boundary_and_cumulative_charges() {
        let sessions = tempfile::tempdir().expect("isolated journal");
        let _journal = astra_services::session_journal::JournalDirGuard::new(sessions.path());
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.current_session_id = Some(uuid::Uuid::new_v4().to_string());
        state.owner_user_id = Some("budget-checkpoint-test".into());
        state.current_run_id = Some("same-budget-run".into());
        state.current_run_owner_generation = Some(3);
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget::new(
            24,
            std::num::NonZeroUsize::new(50),
        );
        state.max_turns = 24;
        state.remaining_turns = 24;
        for iteration in 0..25 {
            let prepared = prepare_turn_iteration(&mut host, &mut state, iteration)
                .await
                .expect("iteration preparation");
            assert!(!matches!(prepared, PreparedTurnIteration::Finished(_)));
        }
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.textless_response_retries = 1;
        state.budget_wrapup_injected = true;
        state.budget_wrapup_ignored_rounds = 1;
        let hook = astra_turn_types::StopHook {
            label: "required validation".into(),
            command: "make check".into(),
            working_dir: Some("/workspace".into()),
            depends_on: Vec::new(),
            timeout_secs: Some(30),

            authoritative: true,
        };
        state.hooks.stop_hooks = vec![hook.clone()];
        state.hooks.declarations.stop = vec![hook.clone()];
        let expected_control = astra_pipeline::step_protocol::RunExecutionControl::V4 {
            completion_settlement: state.hooks.completion_settlement.clone(),
            hook_obligations: astra_turn_types::StopHookObligations {
                declarations: state.hooks.declarations.clone(),
                phase: state.hooks.phase,
            },
            question_correlations: state
                .messaging
                .question_correlations
                .snapshot("same-budget-run", 3)
                .unwrap(),
            budget_wrapup_injected: true,
            budget_wrapup_ignored_rounds: 1,
        };
        try_write_heavy_checkpoint(&mut state);
        let astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) = state
            .stall
            .last_heavy_checkpoint
            .as_ref()
            .expect("heavy checkpoint")
        else {
            panic!("expected heavy checkpoint")
        };
        let wire = serde_json::to_value(heavy).expect("checkpoint serialization");
        assert_eq!(wire["run_execution_budget"]["run_id"], "same-budget-run");
        assert_eq!(wire["run_execution_budget"]["charged_iterations"], 25);
        assert_eq!(
            wire["run_execution_budget"]["granted_iteration_boundary"],
            48
        );
        assert_eq!(wire["run_execution_budget"]["remaining_iterations"], 23);
        assert_eq!(
            wire["run_execution_budget"]["effective_hard_turn_limit"],
            50
        );
        assert_eq!(
            heavy.run_execution_control.as_ref(),
            Some(&expected_control)
        );
    }

    #[test]
    fn runtime_decision_intent_does_not_reinterpret_user_text() {
        let message = "review literal <system-reminder> syntax";

        assert_eq!(
            pure_user_intent_for_runtime_decision(message),
            "review literal <system-reminder> syntax"
        );
    }

    #[tokio::test]
    async fn execution_budget_charge_preserves_state_on_cancel_and_overflow() {
        for cancelled in [true, false] {
            let mut host = MockHost::new(Vec::new());
            let mut state = make_state();
            state.charged_iterations = if cancelled { 7 } else { u64::MAX };
            state.cancellation.flag = Some(Arc::new(AtomicBool::new(cancelled)));
            let before = (state.charged_iterations, state.remaining_turns);
            let result = prepare_turn_iteration(&mut host, &mut state, 0).await;
            if cancelled {
                assert!(matches!(
                    result,
                    Ok(PreparedTurnIteration::Finished(
                        AgenticLoopOutcome::Cancelled
                    ))
                ));
            } else {
                assert!(
                    matches!(result, Err(ref error) if error.message == "execution iteration accounting overflow")
                );
            }
            assert_eq!((state.charged_iterations, state.remaining_turns), before);
        }
    }

    #[test]
    fn invalid_slice_accounting_fails_without_changing_the_checkpoint_facts() {
        for (boundary, remaining, charged) in [(24, 1, 25), (usize::MAX, 0, usize::MAX)] {
            let mut state = make_state();
            state.current_run_id = Some("accounting-test".into());
            state.current_run_owner_generation = Some(0);
            state.max_turns = boundary;
            state.remaining_turns = remaining;
            state.charged_iterations = charged as u64;
            let before = state.run_execution_budget_snapshot();

            assert!(prepare_iteration_budget(&mut state).is_err());

            assert_eq!(state.run_execution_budget_snapshot(), before);
            assert_eq!(
                (state.max_turns, state.remaining_turns),
                (boundary, remaining)
            );
        }
    }

    #[tokio::test]
    async fn loop_preamble_starts_fresh_compression_tracking() {
        let mut state = make_state();
        state.context_compression_triggered = true;

        run_loop_preamble(&mut state).await;

        assert!(!state.context_compression_triggered);
    }

    #[tokio::test]
    async fn read_only_child_preamble_disables_project_hook_execution() {
        let mut state = make_state();
        let mut permissions = astra_turn_core::permission::types::InheritedPermissions::default();
        permissions.read_only_execution = true;
        state.permission_context =
            Some(astra_turn_core::permission::types::PermissionSyncContext::shared(permissions));
        state.skills.session_event_hooks =
            crate::skills::hooks::SessionEventHookRegistry::new(vec![
                astra_skills::hooks::SessionEventHook {
                    event: crate::skills::hooks::SessionEvent::SessionStart,
                    action: astra_skills::hooks::HookAction::SetEnv {
                        key: "ASTRA_READ_ONLY_HOOK_TEST".into(),
                        value: "must_not_apply".into(),
                    },
                    timeout_secs: 5,
                    is_async: false,
                    condition: None,
                    once: false,
                    priority: 0,
                },
            ]);
        run_loop_preamble(&mut state).await;
        assert!(state.skills.session_event_hooks.is_empty());
        assert!(state.skills.tool_event_hooks.is_empty());
    }

    fn agent_record(
        action: &str,
        args: serde_json::Value,
        result: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: "agent".into(),
            ok: error.is_none(),
            ms: 0,
            error: error.map(str::to_string),
            args_preview: Some(action.to_string()),
            args_full: Some(args.to_string()),
            result_full: result.map(|mut value| {
                if action == "spawn" && value["status"] == "launched" {
                    let id = value["agent_id"]
                        .as_str()
                        .expect("fixture child identity")
                        .to_string();
                    value["result_family"] = json!("control_receipt");
                    value["action"] = json!("spawn");
                    value["success"] = json!(true);
                    value["parent_run_id"] = json!("fixture-parent");
                    value["run_id"] = json!(format!("run-{id}"));
                } else {
                    value["result_family"] = json!("child_result");
                }
                assert!(
                    astra_turn_core::orchestration::agent_result_wire::decode_agent_tool_result(
                        &value
                    )
                    .is_some(),
                    "invalid fixture receipt: {value}"
                );
                value.to_string()
            }),
            ..Default::default()
        }
    }

    fn read_record(round: u32, start_line: u32, end_line: u32) -> ToolCallRecord {
        read_record_at_path(round, "/workspace/sample.rs", start_line, end_line)
    }

    fn read_record_at_path(
        round: u32,
        path: &str,
        start_line: u32,
        end_line: u32,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            file_path: Some(path.into()),
            args_full: Some(
                json!({
                    "path": path,
                    "start_line": start_line,
                    "end_line": end_line
                })
                .to_string(),
            ),
            result_full: Some(format!("{path}: lines {start_line}-{end_line}")),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_observation_receipt()
                    .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                    .cloned(),
            round: Some(round),
            ..Default::default()
        }
    }

    fn write_record(round: u32, path: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: "write_file".into(),
            ok,
            file_path: Some(path.into()),
            args_full: Some(json!({"path": path, "content": "updated"}).to_string()),
            round: Some(round),
            ..Default::default()
        }
    }

    /// Production-shaped successful structured writer. Tests that exercise
    /// completion, renewal, or validation after an actual commit must use the
    /// executor-owned receipt; `write_record` remains available for risk-only
    /// and negative-evidence fixtures.
    fn committed_write_record(round: u32, path: &str) -> ToolCallRecord {
        let mut record = write_record(round, path, true);
        let receipt = astra_tools::workspace_observation::typed_workspace_tool_receipt();
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Executed);
        record.workspace_mutation_observed = Some(true);
        record.workspace_mutation_scope =
            Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into());
        record.workspace_mutation_receipt = receipt
            .get(astra_tools::workspace_observation::RECEIPT_FIELD)
            .cloned();
        record
    }

    #[tokio::test]
    async fn observed_mutation_does_not_renew_an_explicit_hard_limit() {
        let mut state = make_state();
        let fallback = astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default();
        state.task_profile = fallback;
        state.agentic_turn_budget = fallback.agentic_turn_budget;
        state.agentic_turn_budget.hard_turn_limit = std::num::NonZeroUsize::new(24);
        state.max_turns = fallback.agentic_turn_budget.initial_turns;
        state.remaining_turns = 0;
        state.charged_iterations = 24;
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![committed_write_record(23, "/workspace/src/lib.rs")];
        let mut host = MockHost::new(Vec::new());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 24)
            .await
            .expect("exhausted runs must settle without another inference");
        assert!(matches!(prepared, PreparedTurnIteration::Finished(_)));
        assert_eq!(state.max_turns, 24);
        assert_eq!(state.remaining_turns, 0);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::BudgetExhausted)
        );
    }

    #[tokio::test]
    async fn infrastructure_ceiling_survives_slice_renewal_and_cancels_children() {
        use crate::turn::agentic_loop::host::tests::{edge_tool_result, make_edge_tool_with_args};
        let sessions = tempfile::tempdir().unwrap();
        let _journal = astra_services::session_journal::JournalDirGuard::new(sessions.path());
        let mut state = make_state();
        state.owner_user_id = Some("infrastructure-budget-test".into());
        state.current_session_id = Some(uuid::Uuid::new_v4().to_string());
        state.current_run_id = Some("fixture-parent".into());
        state.current_run_owner_generation = Some(0);
        state.agentic_turn_budget = state.task_profile.agentic_turn_budget;
        state.max_turns = state.agentic_turn_budget.initial_turns;
        state.remaining_turns = state.max_turns;
        state.stall.circuit_breaker =
            astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker::new(
                astra_turn_core::loop_circuit_breaker::BreakerConfig {
                    absolute_max_rounds: 28,
                    ..Default::default()
                },
            );
        state.stall.tool_call_records.push(agent_record(
            "spawn",
            json!({"description": "Review remaining evidence"}),
            Some(json!({"status": "launched", "agent_id": "pending-child"})),
            None,
        ));
        let turns = (0..40)
            .map(|index| {
                let mut tool = make_edge_tool_with_args(
                    "read_file",
                    json!({"path": format!("/workspace/source-{index}.rs")}),
                    &format!("source {index}"),
                );
                tool.request_id = format!("read-{index}");
                edge_tool_result(vec![tool], 10, 2, None)
            })
            .collect();
        let mut host = MockHost::new(turns).with_valid_tools(&["read_file"]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 28);
        assert_eq!(host.cancelled_agent_ids, vec!["pending-child"]);
        let interruption = state.interruption.as_ref().unwrap();
        assert_eq!(interruption.kind, InterruptionKind::BudgetExhausted);
        assert!(interruption.to_json().to_string().contains("28/28 rounds"));
        assert!(state.max_turns > 24);
        assert!(state.stall.last_heavy_checkpoint.is_some());
        assert_eq!(state.llm_rounds_completed, 28);
    }

    fn explicit_verification_hook(
        label: &str,
        command: &str,
    ) -> astra_turn_core::stop_hooks::StopHook {
        astra_turn_core::stop_hooks::StopHook {
            label: label.into(),
            command: command.into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,

            authoritative: true,
        }
    }

    #[test]
    fn bash_preview_never_counts_as_historical_mutation_evidence() {
        let record = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_preview: Some(r#"{"command":"printf x > /workspace/out"}"#.into()),
            ..Default::default()
        };
        assert!(!tool_record_is_workspace_mutation(&record));
    }

    #[test]
    fn settlement_evidence_keeps_failed_and_rejected_calls_visible() {
        let mut state = make_state();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            error_kind: Some(astra_core::ErrorKind::ToolInvalidArgs),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        });

        let evidence = settlement_tool_outcome_evidence(&state);
        assert_eq!(evidence["tool_calls_completed"], 1);
        assert_eq!(evidence["tool_calls_failed"], 1);
        assert_eq!(evidence["tool_calls_rejected"], 1);
        assert_eq!(evidence["latest_failure"]["tool"], "read_file");
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Unknown),
            error_kind: Some(astra_core::ErrorKind::ToolUnavailable),
            ..Default::default()
        });
        let evidence = settlement_tool_outcome_evidence(&state);
        assert_eq!(evidence["tool_calls_completed"], 1);
        assert_eq!(evidence["tool_calls_unknown"], 1);
        assert_eq!(evidence["latest_failure"]["tool"], "write_file");
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            error_kind: Some(astra_core::ErrorKind::ToolOutcomeUnknown),
            ..Default::default()
        });
        let evidence = settlement_tool_outcome_evidence(&state);
        assert_eq!(evidence["tool_calls_completed"], 1);
        assert_eq!(evidence["tool_calls_failed"], 1);
        assert_eq!(evidence["tool_calls_unknown"], 2);
    }

    #[tokio::test]
    async fn hard_agentic_limit_reserves_a_text_only_answer_boundary() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::ReadOnly));
        state.agentic_turn_budget = astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
            initial_turns: 2,
            hard_turn_limit: std::num::NonZeroUsize::new(2),
        };
        state.max_turns = 2;
        state.remaining_turns = 1;
        state.stall.tool_call_records = vec![read_record(0, 1, 80)];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .expect("the final evidence-bearing round must remain answerable");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.max_turns, 2);
        assert_eq!(state.remaining_turns, 0);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.budget_wrapup_injected);
        assert!(state.interruption.is_none());
        let settlement = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "agentic_execution_slice_complete")
            .expect("typed settlement instruction");
        assert_eq!(settlement.payload["evidence"]["tool_calls_completed"], 1);
        assert!(
            !settlement.payload.to_string().contains("hard_turn_limit"),
            "internal counters must not leak into model-facing product copy"
        );
    }

    #[tokio::test]
    async fn short_total_budget_preserves_an_admitted_ordinary_work_slice() {
        let mut host = MockHost::new(Vec::new()).with_execution_time_budget_remaining(
            astra_turn_types::ExecutionTimeRemaining {
                work_remaining: Duration::from_secs(10),
                total_remaining: Duration::from_secs(20),
            },
        );
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::ReadOnly));
        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(!state.budget_wrapup_injected);
    }

    #[tokio::test]
    async fn execution_deadline_forces_settlement_before_round_budget_expires() {
        let mut host = MockHost::new(Vec::new()).with_execution_time_budget_remaining(
            astra_turn_types::ExecutionTimeRemaining {
                work_remaining: Duration::ZERO,
                total_remaining: Duration::from_secs(20),
            },
        );
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::ReadOnly));
        state.max_turns = 8;
        state.remaining_turns = 4;

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("deadline-bound execution should enter safe settlement");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(state.hooks.completion_settlement.text_only);
        assert_eq!(state.max_turns, 8);
        assert_eq!(
            state.remaining_turns, 3,
            "settlement consumes the existing allowance"
        );
        assert_eq!(
            state.hooks.completion_settlement.wrapup_origin,
            Some(BudgetWrapupOrigin::ExecutionDeadline)
        );
        assert!(state.budget_wrapup_injected);
        assert!(!state.volatile_pending.is_empty());
    }

    #[tokio::test]
    async fn execution_deadline_accounts_for_whole_second_provider_budget_rounding() {
        let remaining = astra_turn_types::ExecutionTimeRemaining {
            work_remaining: Duration::from_millis(999),
            total_remaining: Duration::from_secs(30),
        };
        let mut host = MockHost::new(Vec::new()).with_execution_time_budget_remaining(remaining);
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::ReadOnly));
        state.max_turns = 8;
        state.remaining_turns = 4;

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("a sub-second ordinary slice must switch to final settlement");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(state.hooks.completion_settlement.text_only);
        assert_eq!(
            state.hooks.completion_settlement.wrapup_origin,
            Some(BudgetWrapupOrigin::ExecutionDeadline)
        );
    }

    #[test]
    fn hard_boundary_does_not_reopen_execution_for_uncertain_generic_action() {
        let mut state = make_state();
        state.turn_intent =
            Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::MayMutate));
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![read_record(0, 1, 80)];

        assert!(begin_budget_settlement(&mut state));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none(),
            "a generic action is not an exact completion obligation"
        );
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.budget_wrapup_injected);
        let payload = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "agentic_execution_slice_complete")
            .expect("text-only settlement frame");
        assert_eq!(payload.payload["execution_authority"], "none");
    }

    #[test]
    fn hard_boundary_reserves_one_typed_completion_action_then_text() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.max_turns = 2;
        state.remaining_turns = 2;
        state
            .stall
            .tool_call_records
            .push(committed_write_record(0, "/workspace/out.txt"));

        state.hooks.stop_hooks = vec![astra_turn_core::stop_hooks::StopHook {
            label: "required check".into(),
            command: "cargo test".into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            authoritative: true,
        }];
        assert!(begin_budget_settlement(&mut state));
        assert_eq!(state.max_turns, 2);
        assert_eq!(state.remaining_turns, 2);
        assert!(!state.hooks.completion_settlement.text_only);
        assert!(!state.budget_wrapup_injected);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("typed settlement must expose one action");

        assert_eq!(window.attempts_remaining, 1);
        let payload = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "typed_completion_action_available")
            .expect("typed action frame");
        assert_eq!(payload.payload["schema"], "completion_settlement.v2");
        assert_eq!(
            payload.payload["execution_authority"],
            "one_matching_action"
        );
    }

    #[test]
    fn exhausted_budget_does_not_reissue_unreceipted_external_bash() {
        use astra_services::session_journal::ToolCallDisposition;

        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.turn_intent = Some(
            TurnIntent::default()
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
                .with_mutation_completion_scope(
                    astra_config::user_profile::MutationCompletionScope::External,
                ),
        );
        state.max_turns = 2;
        state.remaining_turns = 0;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            tool_call_id: Some("upload-1".into()),
            args_full: Some(r#"{"command":"moi upload"}"#.into()),
            runtime_args_full: Some(r#"{"command":"moi upload"}"#.into()),
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        });

        assert!(begin_budget_settlement(&mut state));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.text_only);
        assert_eq!(state.max_turns, 2);
        assert_eq!(state.remaining_turns, 0);
        assert_eq!(state.stall.tool_call_records.len(), 1);
        let payload = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["signal"] == "agentic_execution_slice_complete")
            .expect("text-only settlement");
        assert_eq!(payload.payload["execution_authority"], "none");
        assert!(
            state
                .volatile_pending
                .iter()
                .all(|entry| entry.payload["signal"] != "typed_completion_action_available")
        );
    }

    #[test]
    fn active_work_without_explicit_validation_uses_existing_settlement_owner() {
        let mut state = make_state();
        state.remaining_turns = 2;
        state
            .stall
            .tool_call_records
            .push(committed_write_record(0, "/workspace/out.txt"));
        let before = (state.max_turns, state.remaining_turns);
        assert!(begin_budget_settlement_for_work_state(
            &mut state, true, false, true
        ));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert_eq!((state.max_turns, state.remaining_turns), before);
    }

    #[test]
    fn deadline_boundary_does_not_invite_an_action_without_followup_time() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(committed_write_record(0, "/workspace/out.txt"));

        assert!(begin_budget_settlement_for_work_state(
            &mut state, true, true, false
        ));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.work_settlement_only);
    }

    #[test]
    fn deadline_retires_an_open_completion_action_window() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(committed_write_record(0, "/workspace/out.txt"));
        state.hooks.stop_hooks = vec![explicit_verification_hook("quality", "cargo test")];
        assert!(begin_budget_settlement_for_work_state(
            &mut state, false, true, true
        ));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );

        assert!(begin_deadline_budget_settlement(&mut state));
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.budget_wrapup_injected);
        assert_eq!(
            state.hooks.completion_settlement.wrapup_origin,
            Some(BudgetWrapupOrigin::ExecutionDeadline)
        );
        let settlement = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == super::super::host::VolatileKind::FinalAnswerSettlement)
            .expect("deadline projects a truthful closing boundary");
        assert_eq!(settlement.payload["execution_authority"], "none");
    }

    #[test]
    fn active_work_dependency_chain_cannot_skip_to_work_settlement() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        let first = explicit_verification_hook("compile", "./compile-gate");
        let mut second = explicit_verification_hook("acceptance", "./acceptance-gate");
        second.depends_on = vec!["compile".into()];
        state.hooks.stop_hooks = vec![first, second];
        state.stall.tool_call_records = vec![
            committed_write_record(0, "/workspace/src/lib.rs"),
            read_record_at_path(1, "/workspace/src/lib.rs", 1, 20),
        ];
        state.max_turns = 50;
        state.remaining_turns = 0;

        let action = super::super::execution_phase::pending_completion_action(&state)
            .unwrap()
            .expect("both dependent verification hooks remain pending");
        assert!(matches!(
            action,
            super::super::host::CompletionAction::ExplicitVerification { .. }
        ));
        assert!(
            !super::super::execution_phase::completion_action_window_is_batchable(&state, &action)
        );
        assert!(!begin_budget_settlement_for_work_state(
            &mut state, true, false, true
        ));
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    struct AutoRouteResolver;

    impl SkillResolver for AutoRouteResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, crate::skills::SkillError> {
            Ok(ResolvedSkill {
                name: name.to_string(),
                instructions: format!("Use the {name} workflow."),
                max_tokens: None,
                allowed_tools: vec!["read_file".into()],
                execution_context: ExecutionContext::Inline,
                hooks: SkillHooks::default(),
                skill_dir: None,
                source: SkillSourceKind::Local,
                success_criteria: Vec::new(),
                composition: None,
                input_schema: None,
                output_schema: None,
                remote_url: None,
                forward_headers: Vec::new(),
                required_headers: Vec::new(),
                aliases: vec!["review changes".into()],
                effort: None,
                agent_type: None,
                trust_tier: TrustTier::Bundled,
            })
        }

        fn available_skills(&self) -> Vec<SkillToolInfo> {
            vec![
                SkillToolInfo {
                    name: "review-changes".into(),
                    description: "Review the current branch diff.".into(),
                    aliases: vec!["review changes".into()],
                    ..Default::default()
                },
                SkillToolInfo {
                    name: "optimize-prompt".into(),
                    description: "Reduce prompt size.".into(),
                    aliases: vec!["prompt optimization".into()],
                    ..Default::default()
                },
            ]
        }
    }

    struct FailingAutoRouteResolver;

    impl SkillResolver for FailingAutoRouteResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, crate::skills::SkillError> {
            Err(crate::skills::SkillError::LoadFailed(format!(
                "cannot load {name}"
            )))
        }

        fn available_skills(&self) -> Vec<SkillToolInfo> {
            vec![SkillToolInfo {
                name: "review-changes".into(),
                description: "Review the current branch diff.".into(),
                aliases: vec!["review changes".into()],
                ..Default::default()
            }]
        }
    }

    #[test]
    fn interruption_state_summary_prefers_exploration_family_stall_signal() {
        let mut state = make_state();
        state.stall.tool_call_records = (0..5)
            .flat_map(|round| [read_record(round, 10, 40), read_record(round, 50, 80)])
            .collect();

        let summary = interruption_state_summary(&state, None);
        assert_eq!(
            summary.stall_signal.as_deref(),
            Some("exploration_family=read;streak=5")
        );
        assert_eq!(
            summary.resume_restricted_tools,
            Vec::<String>::new(),
            "read-heavy stall must preserve guidance without hard-blocking read tools"
        );
        let diagnosis = interruption_diagnosis_summary(&state).expect("diagnosis");
        assert!(
            diagnosis.contains("5 consecutive read-dominant exploratory rounds"),
            "expected read-family diagnosis, got {diagnosis}"
        );
        assert!(
            !diagnosis.contains("redundant"),
            "requested ranges are not delivery evidence"
        );
    }

    #[test]
    fn productive_single_tool_rounds_are_not_a_stall_boundary() {
        let mut state = make_state();
        state.stall.tool_call_records = (0..3)
            .map(|round| ToolCallRecord {
                name: "bash".into(),
                ok: true,
                round: Some(round),
                ..Default::default()
            })
            .collect();
        state.messages = vec![
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": ""}),
        ];

        let summary = interruption_state_summary(&state, None);
        assert_eq!(summary.stall_signal, None);
        assert_eq!(
            summary.resume_restricted_tools,
            Vec::<String>::new(),
            "batching shape must not hard-block the executor needed to finish"
        );
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

    #[test]
    fn current_agentic_step_uses_actual_llm_rounds_when_available() {
        let mut state = make_state();
        state.max_turns = 25;
        state.remaining_turns = 9;
        state.llm_rounds_completed = 14;

        assert_eq!(
            current_agentic_step(&state),
            14,
            "agentic_step must not skip when control-only loop iterations consume remaining_turns"
        );
    }

    #[test]
    fn budget_exhaustion_summary_prefers_partial_parallel_agent_results() {
        let mut state = make_state();
        state.max_turns = 13;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 13;
        state.total_tool_calls = 8;
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"description":"Review architecture"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-a",
                    "description":"Review architecture"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review bugs"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-b",
                    "description":"Review bugs"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review security"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-c",
                    "description":"Review security"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-a"}),
                Some(json!({
                    "status":"completed",
                    "agent_id":"agent-a",
                    "result":"Architecture review finished: layout looks good."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-b"}),
                Some(json!({
                    "status":"completed",
                    "agent_id":"agent-b",
                    "result":"Bug review finished: no blocker found."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                Some(json!({
                    "status":"still_running",
                    "agent_id":"agent-c",
                    "current_status":"Running { activity: \"executing\" }"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                None,
                Some("duplicate_within_turn"),
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                None,
                Some("blocked_tool: Tool 'agent' is currently restricted in this session."),
            ),
        ];

        let text = budget_exhaustion_completion_text(&state, &HashSet::new());

        assert!(
            text.contains("owner turn reached its execution boundary"),
            "{text}"
        );
        assert!(text.contains("Review architecture"), "{text}");
        assert!(text.contains("Architecture review finished"), "{text}");
        assert!(text.contains("Review bugs"), "{text}");
        assert!(text.contains("Bug review finished"), "{text}");
        assert!(text.contains("Review security"), "{text}");
        assert!(
            text.contains("still running") || text.contains("did not finish"),
            "{text}"
        );
        assert!(
            text.contains("duplicate_within_turn") || text.contains("restricted"),
            "{text}"
        );
    }

    #[test]
    fn parallel_budget_rollup_accepts_terminal_spawn_and_keeps_it_sticky() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({
                    "agent_id": "agent-a",
                    "description": "Direct review"
                }),
                Some(json!({
                    "status": "completed",
                    "agent_id": "agent-a",
                    "result": "Direct review finished."
                })),
                None,
            ),
            // A stale callback must not reopen a child after its terminal
            // result was already observed from the foreground spawn.
            agent_record(
                "get_result",
                json!({"agent_id": "agent-a"}),
                Some(json!({
                    "status": "launched",
                    "agent_id": "agent-a"
                })),
                None,
            ),
        ];

        let rollup = collect_parallel_agent_budget_rollup(&state).expect("agent rollup");
        assert_eq!(rollup.unfinished.len(), 0);
        assert_eq!(rollup.completed.len(), 1);
        assert_eq!(rollup.completed[0].label, "Direct review");
        assert_eq!(
            rollup.completed[0].result.as_deref(),
            Some("Direct review finished.")
        );
        assert!(unfinished_parallel_agent_ids(&state).is_empty());
    }

    #[test]
    fn parallel_budget_rollup_adopts_provider_observed_child_completion_after_launch() {
        let mut state = make_state();
        state.current_run_id = Some("fixture-parent".into());
        let mut launch = agent_record(
            "spawn",
            json!({
                "action": "spawn",
                "description": "Observed child"
            }),
            Some(json!({
                "status": "launched",
                "agent_id": "agent-a",
                "description": "Observed child"
            })),
            None,
        );
        launch.disposition = Some(astra_services::session_journal::ToolCallDisposition::Executed);
        state.stall.tool_call_records = vec![launch];
        state.push_volatile_payload(
            VolatileKind::BackgroundTaskNotification,
            json!({
                "schema": super::super::host::DIRECT_CHILD_RESULT_SCHEMA,
                "parent_run_id": "fixture-parent",
                "observed_by_provider": true,
                "children": [{
                    "agent_id": "agent-a",
                    "run_id": "run-agent-a",
                    "status": "completed",
                    "result": "Observed child finished."
                }]
            }),
        );

        let rollup = collect_parallel_agent_budget_rollup(&state).expect("agent rollup");
        assert!(rollup.unfinished.is_empty());
        assert_eq!(rollup.completed.len(), 1);
        assert_eq!(rollup.completed[0].label, "Observed child");
        assert_eq!(
            rollup.completed[0].result.as_deref(),
            Some("Observed child finished.")
        );
        assert!(unfinished_parallel_agent_ids(&state).is_empty());
    }

    #[test]
    fn parallel_budget_rollup_accepts_provider_compacted_child_after_lease_cycle() {
        let mut state = make_state();
        state.current_run_id = Some("fixture-parent".into());
        let mut launch = agent_record(
            "spawn",
            json!({
                "action": "spawn",
                "description": "Compacted child"
            }),
            Some(json!({
                "status": "launched",
                "agent_id": "agent-a",
                "description": "Compacted child"
            })),
            None,
        );
        launch.disposition = Some(astra_services::session_journal::ToolCallDisposition::Executed);
        state.stall.tool_call_records = vec![launch];
        state.push_volatile_payload(
            VolatileKind::BackgroundTaskNotification,
            json!({
                "schema": super::super::host::DIRECT_CHILD_RESULT_SCHEMA,
                "parent_run_id": "fixture-parent",
                "children": [{
                    "agent_id": "agent-a",
                    "run_id": "run-agent-a",
                    "status": "completed",
                    "status_fingerprint": "completed:fixture",
                    "result": "Observed before provider compaction.",
                    "result_bytes": 36
                }]
            }),
        );

        // Exercise the real host lease lifecycle. Its second commit keeps
        // terminal identity/status but intentionally removes the result.
        state
            .lease_volatile_pending()
            .expect("first provider lease");
        state.commit_volatile_attempt_lease();
        state
            .lease_volatile_pending()
            .expect("second provider lease");
        state.commit_volatile_attempt_lease();
        let compacted = state
            .volatile_pending
            .iter()
            .flat_map(|injection| injection.payload["children"].as_array())
            .flatten()
            .find(|child| child["agent_id"] == "agent-a")
            .expect("compacted child evidence");
        assert_eq!(compacted["status"], "completed");
        assert!(compacted.get("result").is_none());
        assert_eq!(compacted["result_bytes"], 36);

        let rollup = collect_parallel_agent_budget_rollup(&state).expect("agent rollup");
        assert_eq!(rollup.completed.len(), 1);
        assert!(rollup.completed[0].result.is_none());
        assert!(rollup.terminal_issues.is_empty());
        assert!(rollup.unfinished.is_empty());
        assert!(unfinished_parallel_agent_ids(&state).is_empty());
        let summary = parallel_agent_budget_exhaustion_summary(&state, &HashSet::new())
            .expect("compacted completion summary");
        assert!(summary.contains("Compacted child"), "{summary}");
        assert!(
            summary.contains("exited; result was already observed"),
            "{summary}"
        );
    }

    #[test]
    fn parallel_budget_rollup_releases_non_success_terminal_child() {
        let mut state = make_state();
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"agent_id": "agent-failed", "description": "Failed review"}),
                Some(json!({
                    "status": "failed",
                    "agent_id": "agent-failed",
                    "error": "child exploded"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"agent_id": "agent-cancelled", "description": "Cancelled review"}),
                Some(json!({
                    "status": "cancelled",
                    "agent_id": "agent-cancelled",
                    "reason": "parent cancelled this sub-agent"
                })),
                None,
            ),
        ];

        let rollup = collect_parallel_agent_budget_rollup(&state).expect("agent rollup");
        assert!(rollup.completed.is_empty());
        assert!(
            unfinished_parallel_agent_ids(&state).is_empty(),
            "terminal failure/cancellation must not be treated as live ownership"
        );
        let details = rollup
            .terminal_issues
            .iter()
            .map(|entry| entry.incomplete_reason.as_deref().unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(details.contains(&"parent cancelled this sub-agent"));
        assert!(details.contains(&"child exploded"));
    }

    #[test]
    fn budget_exhaustion_does_not_claim_child_work_completed() {
        let mut state = make_state();
        state.remaining_turns = 0;
        state.stall.tool_call_records = vec![agent_record(
            "get_result",
            json!({"agent_id":"agent-a"}),
            Some(
                json!({"status":"completed", "agent_id":"agent-a", "result":format!(
                "Findings: {} Review incomplete; final lead unverified.", "x".repeat(400))}),
            ),
            None,
        )];
        let summary = parallel_agent_budget_exhaustion_summary(&state, &HashSet::new()).unwrap();
        assert!(summary.contains("Check each result to determine whether its work is complete"));
        assert!(summary.contains("Review incomplete; final lead unverified"));
        assert!(summary.contains("Summary truncated"));
        assert!(!summary.contains("Completed sub-agent results"));
    }

    #[test]
    fn budget_exhaustion_summary_uses_shared_child_result_projection() {
        let mut state = make_state();
        state.max_turns = 9;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 9;
        state.total_tool_calls = 6;
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"description":"Review architecture"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-a",
                    "description":"Review architecture"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review security"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-b",
                    "description":"Review security"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review infra"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-c",
                    "description":"Review infra"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-a"}),
                Some(json!({
                    "status":"interrupted",
                    "agent_id":"agent-a",
                    "finish_reason":"budget_exhausted",
                    "result":"Partial architecture findings."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-b"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-b"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                Some(json!({
                    "status":"cancelled",
                    "agent_id":"agent-c",
                    "reason":"parent cancelled this sub-agent"
                })),
                None,
            ),
        ];

        let text = budget_exhaustion_completion_text(&state, &HashSet::new());

        assert!(text.contains("Partial architecture findings."), "{text}");
        assert!(
            text.contains("launched and has not produced a child result yet"),
            "{text}"
        );
        assert!(text.contains("parent cancelled this sub-agent"), "{text}");
    }

    #[test]
    fn budget_exhaustion_summary_uses_record_projection_when_args_preview_is_missing() {
        let mut state = make_state();
        state.max_turns = 5;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 5;
        state.total_tool_calls = 4;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "spawn",
                        "agent_id": "agent-a",
                        "description": "Review architecture"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"launched",
                        "result_family":"control_receipt", "action":"spawn", "success":true,
                        "agent_id":"agent-a", "run_id":"run-agent-a", "parent_run_id":"fixture-parent",
                        "description":"Review architecture"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "get_result",
                        "agent_id": "agent-a"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"interrupted",
                        "result_family":"child_result", "agent_id":"agent-a",
                        "result":"Partial architecture findings.",
                        "finish_reason":"budget_exhausted"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "spawn",
                        "agent_id": "agent-b",
                        "description": "Review security"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"launched",
                        "result_family":"control_receipt", "action":"spawn", "success":true,
                        "agent_id":"agent-b", "run_id":"run-agent-b", "parent_run_id":"fixture-parent",
                        "description":"Review security"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
            ToolCallRecord {
                name: "agent".into(),
                ok: true,
                ms: 0,
                args_full: Some(
                    json!({
                        "action": "get_result",
                        "agent_id": "agent-b"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    json!({
                        "status":"launched", "result_family":"child_result", "agent_id":"agent-b"
                    })
                    .to_string(),
                ),
                ..Default::default()
            },
        ];

        let text = budget_exhaustion_completion_text(&state, &HashSet::new());
        assert!(text.contains("Review architecture"), "{text}");
        assert!(text.contains("Partial architecture findings."), "{text}");
        assert!(text.contains("Review security"), "{text}");
    }

    #[tokio::test]
    async fn execution_settlement_cancels_unfinished_parallel_agents_before_synthesis() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        // Exercise actual resource exhaustion, not missing progress receipts.
        state.agentic_turn_budget.hard_turn_limit = std::num::NonZeroUsize::new(13);
        state.max_turns = 13;
        state.remaining_turns = 0;
        state.llm_rounds_completed = 13;
        state.total_tool_calls = 8;
        state.stall.tool_call_records = vec![
            agent_record(
                "spawn",
                json!({"description":"Review architecture"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-a",
                    "description":"Review architecture"
                })),
                None,
            ),
            agent_record(
                "spawn",
                json!({"description":"Review security"}),
                Some(json!({
                    "status":"launched",
                    "agent_id":"agent-c",
                    "description":"Review security"
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-a"}),
                Some(json!({
                    "status":"completed",
                    "agent_id":"agent-a",
                    "result":"Architecture review finished."
                })),
                None,
            ),
            agent_record(
                "get_result",
                json!({"agent_id":"agent-c"}),
                Some(json!({
                    "status":"still_running",
                    "agent_id":"agent-c",
                    "current_status":"Running { activity: \"executing\" }"
                })),
                None,
            ),
        ];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 13)
            .await
            .expect("execution settlement should remain answerable");

        assert!(matches!(prepared, PreparedTurnIteration::Finished(_)));
        assert_eq!(host.cancelled_agent_ids, vec!["agent-c".to_string()]);
        assert_eq!(state.remaining_turns, 0);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none(),
            "running child execution must be cancelled before synthesis, not consume the terminal task action"
        );
        assert!(state.interruption.is_some());
    }

    #[tokio::test]
    async fn stale_runtime_parent_does_not_sweep_reused_child_run_identity() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.execution_lease_lost = Some(Arc::new(AtomicBool::new(true)));
        state.stall.tool_call_records = vec![agent_record(
            "spawn",
            json!({"description":"new generation child"}),
            Some(json!({
                "status":"launched",
                "agent_id":"child-run-reused-by-n-plus-one",
                "description":"new generation child"
            })),
            None,
        )];

        let cancelled = cancel_unfinished_child_agents(
            &mut host,
            &state,
            "stale parent lease lost",
            CancellationOrigin::Runtime,
        )
        .await;

        assert!(cancelled.is_empty());
        assert!(
            host.cancelled_agent_ids.is_empty(),
            "a stale runtime-owned parent must not issue run-id-scoped cancellation against a newer child generation"
        );

        let cancelled = cancel_unfinished_child_agents(
            &mut host,
            &state,
            "canonical ancestor user cancellation",
            CancellationOrigin::User,
        )
        .await;
        assert!(cancelled.contains("child-run-reused-by-n-plus-one"));
        assert_eq!(
            host.cancelled_agent_ids,
            vec!["child-run-reused-by-n-plus-one".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn child_agent_cancel_timeout_does_not_block_budget_wrapup() {
        let mut host = MockHost::new(Vec::new())
            .with_cancel_child_agents_delay(CHILD_AGENT_CANCEL_TIMEOUT + Duration::from_secs(30));

        let cancelled = cancel_child_agents_with_timeout(
            &mut host,
            None,
            vec!["agent-c".to_string()],
            "owner execution boundary reached",
            CancellationOrigin::Runtime,
        )
        .await;

        assert!(
            cancelled.is_empty(),
            "a hung child-agent cancel hook should not claim cancellation"
        );
        assert!(
            host.cancelled_agent_ids.is_empty(),
            "timeout must cancel the host future before it mutates cancellation state"
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_pre_routes_judged_skill_requests() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());
        state.skills.request_constraints.allowed_tools =
            Some(std::collections::HashSet::from(["read_file".to_string()]));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");
        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            state.remaining_turns, 9,
            "pre-routing still consumes the current turn budget"
        );
        assert_eq!(state.tool_results.len(), 1);
        assert!(
            state
                .skills
                .execution
                .invoked
                .contains_key("review-changes")
        );
        assert_eq!(
            state.messages.iter().find_map(|msg| {
                msg.get("tool_calls")?
                    .as_array()?
                    .first()?
                    .get("id")?
                    .as_str()
            }),
            Some("skill-auto-route-review-changes")
        );
        assert_eq!(
            state.messages.iter().find_map(|msg| {
                msg.get("tool_calls")?
                    .as_array()?
                    .first()?
                    .get("function")?
                    .get("name")?
                    .as_str()
            }),
            Some(crate::turn::skill_tool::SKILL_TOOL_NAME)
        );
        assert_eq!(
            state.telemetry.all_selected_skills,
            vec!["review-changes".to_string()]
        );
        assert!(
            state.tool_results[0]["result"]
                .as_str()
                .is_some_and(|content| content.contains("<skill-loaded name=\"review-changes\"/>"))
        );
        assert!(
            state.tool_results[0]["result"]
                .as_str()
                .is_some_and(|content| content.contains("Declared workflow tools: `read_file`")),
            "auto-routed skills must expose their typed workflow guidance at the model boundary"
        );
        assert!(
            state.tool_results[0]["result"]
                .as_str()
                .is_some_and(|content| {
                    content
                        .contains("request policy permits at most these non-skill tools: read_file")
                        && content.contains("current tool surface is authoritative")
                })
        );
        assert!(state.messages.iter().any(|msg| {
            msg.get("role").and_then(|v| v.as_str()) == Some("tool")
                && msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|content| {
                        content.contains("<skill-loaded name=\"review-changes\"/>")
                    })
        }));
    }

    #[tokio::test]
    async fn required_work_skips_optional_skill_pre_route_before_the_task_board_exists() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "verify two independent release requirements".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent =
            Some(TurnIntent::default().with_work_lifecycle(WorkLifecycleIntent::Required));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(host.skill_auto_route_queries.is_empty());
        assert!(state.skills.execution.invoked.is_empty());
        assert!(state.tool_results.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_iteration_pre_routes_structured_user_intent_not_prompt_scaffolding() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "<project-instructions>\nMention prompt optimization here.\n</project-instructions>\n\nreview changes on current branch".into();
        state.user_intent = "review changes on current branch".into();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(
            state
                .skills
                .execution
                .invoked
                .contains_key("review-changes")
        );
        assert_eq!(
            host.skill_auto_route_queries,
            vec!["review changes on current branch".to_string()]
        );

        let arguments = state
            .messages
            .iter()
            .find_map(|msg| {
                msg.get("tool_calls")?
                    .as_array()?
                    .first()?
                    .get("function")?
                    .get("arguments")?
                    .as_str()
            })
            .expect("skill auto-route tool call arguments");
        let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(parsed["task"], "review changes on current branch");
        assert!(
            !parsed["task"]
                .as_str()
                .unwrap()
                .contains("<project-instructions>")
        );
        assert!(
            !parsed["task"]
                .as_str()
                .unwrap()
                .contains("prompt optimization")
        );
    }

    #[tokio::test]
    async fn unavailable_turn_intent_judge_does_not_manufacture_objective_semantics() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.message = "repair the lifecycle".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare with the deterministic runtime profile");

        assert!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("canonical message is valid")
                .is_none(),
            "judge unavailability is runtime state, not producer-owned objective semantics"
        );
    }

    #[test]
    fn repeated_semantic_effects_keep_the_submitted_turn_owner() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Refine);
        let mut state = make_state();
        state.session_turn = 4;
        state.message = "also verify the database path".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        assert!(apply_current_user_turn_semantics(&mut state, &intent));
        let later_intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace);
        state.messages.push(json!({
            "role": "user",
            "content": "guidance accepted while the run is active"
        }));
        assert!(!apply_current_user_turn_semantics(
            &mut state,
            &later_intent
        ));

        assert!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("valid semantics")
                .is_some()
        );
        assert!(
            astra_turn_types::user_turn_semantics(state.messages.last().unwrap())
                .expect("valid semantics")
                .is_none()
        );
        assert_eq!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .unwrap()
                .map(|semantics| semantics.objective_relation),
            Some(astra_turn_types::ObjectiveRelation::Refine),
            "later model rounds must not re-judge or overwrite user-turn semantics"
        );
    }

    #[test]
    fn canonical_effects_persist_assessment_with_prior_response_reference_once() {
        use astra_turn_types::{
            AssessmentConfidence, FeedbackResponseRelation, ResponseSatisfaction, TaskDifficulty,
            TurnAssessment,
        };
        let intent = TurnIntent {
            assessment: Some(TurnAssessment {
                satisfaction: ResponseSatisfaction::Dissatisfied,
                satisfaction_confidence: AssessmentConfidence::High,
                feedback_relation: FeedbackResponseRelation::PreviousResponse,
                difficulty: TaskDifficulty::Easy,
                difficulty_confidence: AssessmentConfidence::Medium,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = make_state();
        state.message = "correct it".into();
        state.user_intent = state.message.clone();
        let prefix = vec![
            json!({"role":"user","content":"first"}),
            json!({"role":"assistant","content":"answer"}),
        ];
        state.messages = prefix.clone();
        state
            .messages
            .push(json!({"role":"user","content":"correct it"}));
        state
            .messages
            .push(json!({"role":"assistant","content":"later response"}));
        assert!(apply_current_user_turn_semantics(&mut state, &intent));
        let recorded = astra_turn_types::user_turn_semantics(&state.messages[2])
            .unwrap()
            .unwrap();
        assert_eq!(recorded.assessment, intent.assessment);
        let target = recorded.feedback_response.as_ref().unwrap();
        assert_eq!(target.message_count, 2);
        assert_eq!(
            target.prefix_root,
            astra_turn_types::canonical_conversation_root(&prefix)
        );
        assert!(!record_current_user_turn_semantics(&mut state, &intent));
        assert_eq!(
            astra_turn_types::user_turn_semantics(&state.messages[2]).unwrap(),
            Some(recorded)
        );
    }

    #[test]
    fn assessment_does_not_invent_missing_or_ambiguous_response_targets() {
        use astra_turn_types::{FeedbackResponseRelation, ResponseSatisfaction, TurnAssessment};
        for relation in [
            FeedbackResponseRelation::PreviousResponse,
            FeedbackResponseRelation::EarlierOrMultiple,
            FeedbackResponseRelation::Unknown,
        ] {
            let mut state = make_state();
            state.message = "feedback".into();
            state.user_intent = state.message.clone();
            state.messages = vec![json!({"role":"user","content":"feedback"})];
            let intent = TurnIntent {
                assessment: Some(TurnAssessment {
                    feedback_relation: relation,
                    satisfaction: ResponseSatisfaction::Dissatisfied,
                    ..Default::default()
                }),
                ..Default::default()
            };
            if relation != FeedbackResponseRelation::PreviousResponse {
                state.messages.insert(
                    0,
                    json!({"role":"assistant","content":"available but not targeted"}),
                );
            }
            assert!(record_current_user_turn_semantics(&mut state, &intent));
            let recorded = astra_turn_types::user_turn_semantics(state.messages.last().unwrap())
                .unwrap()
                .unwrap();
            assert!(recorded.feedback_response.is_none());
            if relation == FeedbackResponseRelation::PreviousResponse {
                assert_eq!(
                    recorded.assessment.unwrap().satisfaction,
                    ResponseSatisfaction::Unknown
                );
            }
        }
    }

    #[test]
    fn assessment_uses_frozen_source_and_normalized_response() {
        use astra_turn_types::{FeedbackResponseRelation, TurnAssessment};
        for response in [
            json!(" response B\n"),
            json!([{"type":"text","text":"response B"}]),
        ] {
            let mut state = make_state();
            state.user_intent = "correct it".into();
            state.message =
                "<project-instructions>context</project-instructions>\n\ncorrect it".into();
            state.messages = vec![
                json!({"role":"user","content":"first"}),
                json!({"role":"assistant","content":"response A"}),
                json!({"role":"user","content":"correct it"}),
                json!({"role":"assistant","content":response}),
                json!({"role":"user","content":state.message}),
            ];
            crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
            let context = crate::turn::agentic::turn_intent::context_for_state(&state);
            assert_eq!(
                context.prior_assistant_message.as_deref(),
                Some("response B")
            );
            assert_eq!(context.source.as_ref().unwrap().message_index, 4);
            let expected = context.source.unwrap().feedback_response.unwrap();
            state
                .messages
                .push(json!({"role":"assistant","content":"future response"}));
            state
                .messages
                .push(json!({"role":"user","content":state.message}));
            let intent = TurnIntent {
                assessment: Some(TurnAssessment {
                    feedback_relation: FeedbackResponseRelation::PreviousResponse,
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(record_current_user_turn_semantics(&mut state, &intent));
            let recorded = astra_turn_types::user_turn_semantics(&state.messages[4])
                .unwrap()
                .unwrap();
            assert_eq!(recorded.feedback_response, Some(expected));
            assert!(
                astra_turn_types::user_turn_semantics(&state.messages[6])
                    .unwrap()
                    .is_none()
            );
            assert!(!record_current_user_turn_semantics(&mut state, &intent));
        }
    }

    #[test]
    fn changed_or_missing_assessment_source_is_never_relocated() {
        let mut state = make_state();
        state.message = "source".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role":"user","content":"source"})];
        crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
        state.messages[0]["content"] = json!("rewritten");
        state
            .messages
            .push(json!({"role":"user","content":"source"}));
        assert!(!record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
        ));
        state.telemetry.turn_intent_context = None;
        state.messages = vec![json!({"role":"assistant","content":"unowned later answer"})];
        crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
        let context = crate::turn::agentic::turn_intent::context_for_state(&state);
        assert!(context.source.is_none());
        assert!(context.prior_assistant_message.is_none());
    }

    #[test]
    fn later_assessment_preserves_recorded_objective_and_feedback() {
        use astra_turn_types::{
            FeedbackResponseReference, FeedbackResponseRelation, ObjectiveRelation,
            ResponseSatisfaction, TaskDifficulty, TurnAssessment, UserFeedback, UserFeedbackKind,
            UserFeedbackTarget,
        };
        let feedback = UserFeedback {
            kind: UserFeedbackKind::Correction,
            target: UserFeedbackTarget::Approach,
        };
        for (objective_relation, feedback) in [
            (ObjectiveRelation::Correct, None),
            (ObjectiveRelation::Unknown, Some(feedback)),
            (ObjectiveRelation::Correct, Some(feedback)),
        ] {
            let mut state = make_state();
            state.message = "repair it".into();
            state.user_intent = state.message.clone();
            let prefix = vec![
                json!({"role":"user","content":"first request"}),
                json!({"role":"assistant","content":"previous answer"}),
            ];
            state.messages = prefix.clone();
            state
                .messages
                .push(json!({"role":"user","content":state.message}));
            assert!(record_current_user_turn_semantics(
                &mut state,
                &TurnIntent {
                    objective_relation,
                    feedback,
                    ..Default::default()
                }
            ));
            let assessment = TurnAssessment {
                feedback_relation: FeedbackResponseRelation::PreviousResponse,
                satisfaction: ResponseSatisfaction::Dissatisfied,
                difficulty: TaskDifficulty::Easy,
                ..Default::default()
            };
            let late_intent = TurnIntent {
                assessment: Some(assessment),
                ..Default::default()
            };
            assert!(
                !record_current_user_turn_semantics(&mut state, &late_intent),
                "an assessment fill must not replay objective/feedback effects"
            );
            let recorded = astra_turn_types::user_turn_semantics(&state.messages[2])
                .unwrap()
                .unwrap();
            assert_eq!(recorded.objective_relation, objective_relation);
            assert_eq!(recorded.feedback, feedback);
            assert_eq!(recorded.assessment, Some(assessment));
            assert_eq!(
                recorded.feedback_response,
                Some(FeedbackResponseReference::from_canonical_prefix(prefix))
            );
            for repeated in [
                late_intent,
                TurnIntent {
                    objective_relation: ObjectiveRelation::Replace,
                    assessment: Some(TurnAssessment::default()),
                    ..Default::default()
                },
            ] {
                assert!(!record_current_user_turn_semantics(&mut state, &repeated));
                assert_eq!(
                    astra_turn_types::user_turn_semantics(&state.messages[2]).unwrap(),
                    Some(recorded.clone())
                );
            }
        }
    }

    #[test]
    fn known_assessment_survives_later_resolution_of_unknown_objective() {
        let mut state = make_state();
        state.message = "repair it".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role":"user","content":"repair it"})];
        let assessment = astra_turn_types::TurnAssessment {
            difficulty: astra_turn_types::TaskDifficulty::Easy,
            ..Default::default()
        };
        assert!(record_current_user_turn_semantics(
            &mut state,
            &TurnIntent {
                assessment: Some(assessment),
                ..Default::default()
            }
        ));
        let correction = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct);
        assert!(record_current_user_turn_semantics(&mut state, &correction));
        let recorded = astra_turn_types::user_turn_semantics(&state.messages[0])
            .unwrap()
            .unwrap();
        assert_eq!(recorded.assessment, Some(assessment));
        assert_eq!(
            recorded.objective_relation,
            astra_turn_types::ObjectiveRelation::Correct
        );
        assert!(!record_current_user_turn_semantics(&mut state, &correction));
    }

    #[test]
    fn turn_semantics_only_advance_from_explicit_unknown_once() {
        let mut state = make_state();
        state.message = "repair it".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": "repair it"})];

        assert!(record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
        ));
        assert!(record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct)
        ));
        assert!(!record_current_user_turn_semantics(
            &mut state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace)
        ));
        assert_eq!(
            astra_turn_types::user_turn_semantics(&state.messages[0])
                .expect("valid semantics")
                .map(|semantics| semantics.objective_relation),
            Some(astra_turn_types::ObjectiveRelation::Correct)
        );
    }

    #[tokio::test]
    async fn prepare_turn_iteration_preserves_profile_when_judge_unavailable() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.message = "continue the implementation".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        let existing_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                true,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Complex,
            );
        state.task_profile = existing_profile;
        state.agentic_turn_budget = existing_profile.agentic_turn_budget;
        state.turn_guard.set_task_profile(existing_profile);
        state.turn_intent = Some(
            TurnIntent::default()
                .with_requested_scenario(Scenario::Refactoring)
                .with_workspace_mutation(WorkspaceMutationIntent::MustMutate),
        );

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            state.task_profile, existing_profile,
            "unavailable judge must not downgrade a current structured runtime profile"
        );
        assert_eq!(
            state.agentic_turn_budget, existing_profile.agentic_turn_budget,
            "budget must remain coupled to the preserved profile"
        );
        assert_eq!(
            state
                .turn_intent
                .as_ref()
                .map(|intent| intent.workspace_mutation),
            Some(WorkspaceMutationIntent::MustMutate)
        );
    }

    #[test]
    fn auto_route_tool_call_id_is_deterministic_and_sanitized() {
        assert_eq!(
            auto_route_tool_call_id("Review Changes"),
            "skill-auto-route-review-changes"
        );
        assert_eq!(auto_route_tool_call_id("  !!!  "), "skill-auto-route");
    }

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_pre_route_without_judge_decision() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert!(host.skill_auto_route_queries.is_empty());
        assert!(state.tool_results.is_empty());
        assert!(state.skills.execution.invoked.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_iteration_rejects_judged_skill_outside_visible_catalog() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("missing-skill");
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(AutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            host.skill_auto_route_queries,
            vec!["review changes on current branch".to_string()]
        );
        assert!(state.tool_results.is_empty());
        assert!(state.skills.execution.invoked.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_iteration_does_not_retry_failed_auto_route_for_same_intent() {
        let mut host = MockHost::new(Vec::new()).with_skill_auto_route_decision("review-changes");
        let mut state = make_state();
        state.message = "review changes on current branch".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        state.skills.resolver = Some(Arc::new(FailingAutoRouteResolver));
        state.turn_intent = Some(TurnIntent::default());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("first turn should prepare despite failed auto-route");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(state.skills.execution.auto_route_attempts.len(), 1);
        assert!(state.tool_results.is_empty());
        assert!(state.skills.execution.invoked.is_empty());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("second turn should prepare without retrying failed auto-route");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            state.skills.execution.auto_route_attempts.len(),
            1,
            "same intent+skill failure should not create repeated auto-route attempts"
        );
        assert!(state.tool_results.is_empty());
        assert!(state.skills.execution.invoked.is_empty());
        assert_eq!(
            host.skill_auto_route_queries,
            vec![
                "review changes on current branch".to_string(),
                "review changes on current branch".to_string()
            ],
            "the semantic judge may be consulted again, but the repeated failed decision must not execute"
        );
    }

    // ── Cancellation logic tests ──────────────────────────────────────────
    //
    // P1: Lifecycle cancellation has three orthogonal dimensions:
    //   (1) cancel signal: in-memory flag vs CancellationToken
    //   (2) pause context: inside pause loop vs between turns
    //   (3) checkpoint: written on cancel path
    //
    // These tests exercise every combination of (1) and (2), plus
    // verify checkpoint and interruption record invariants.

    use crate::turn::run_control::{
        QueuedUserIntent, RunControlStatus, RunStatusProvider, UserIntentPoll, UserIntentProvider,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio_util::sync::CancellationToken;

    struct FailingStatusRunControl;

    struct CountingStatusRunControl {
        status: Option<RunControlStatus>,
        cancellation_origin: Option<CancellationOrigin>,
        subsequent_cancellation_origin: Option<CancellationOrigin>,
        calls: AtomicUsize,
        origin_calls: AtomicUsize,
    }

    impl CountingStatusRunControl {
        fn new(status: Option<RunControlStatus>) -> Self {
            Self {
                status,
                cancellation_origin: Some(CancellationOrigin::Runtime),
                subsequent_cancellation_origin: None,
                calls: AtomicUsize::new(0),
                origin_calls: AtomicUsize::new(0),
            }
        }

        fn with_cancellation_origin(mut self, origin: CancellationOrigin) -> Self {
            self.cancellation_origin = Some(origin);
            self
        }

        fn with_pending_cancellation_origin(mut self) -> Self {
            self.cancellation_origin = None;
            self
        }

        fn with_cancellation_origin_sequence(
            mut self,
            first: CancellationOrigin,
            subsequent: CancellationOrigin,
        ) -> Self {
            self.cancellation_origin = Some(first);
            self.subsequent_cancellation_origin = Some(subsequent);
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Acquire)
        }

        fn origin_calls(&self) -> usize {
            self.origin_calls.load(Ordering::Acquire)
        }
    }

    #[async_trait::async_trait]
    impl RunStatusProvider for CountingStatusRunControl {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<RunControlStatus>, String> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(self.status)
        }

        async fn cancellation_origin(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<CancellationOrigin, String> {
            let call = self.origin_calls.fetch_add(1, Ordering::AcqRel);
            let origin = if call == 0 {
                self.cancellation_origin
            } else {
                self.subsequent_cancellation_origin
                    .or(self.cancellation_origin)
            };
            match origin {
                Some(origin) => Ok(origin),
                None => std::future::pending().await,
            }
        }
    }

    #[async_trait::async_trait]
    impl UserIntentProvider for CountingStatusRunControl {
        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            UserIntentPoll {
                next_cursor: after_event_index,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::<QueuedUserIntent>::new(),
                issues: Vec::new(),
                error: None,
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
            _authority: crate::turn::run_control::UserIntentAdmissionAuthority,
            _invocation_identity: Option<
                crate::turn::run_control::UserIntentInvocationApplication<'_>,
            >,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    #[async_trait::async_trait]
    impl RunStatusProvider for FailingStatusRunControl {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<RunControlStatus>, String> {
            Err("transient db timeout".to_string())
        }
    }

    #[async_trait::async_trait]
    impl UserIntentProvider for FailingStatusRunControl {
        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            UserIntentPoll {
                next_cursor: after_event_index,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::<QueuedUserIntent>::new(),
                issues: Vec::new(),
                error: None,
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
            _authority: crate::turn::run_control::UserIntentAdmissionAuthority,
            _invocation_identity: Option<
                crate::turn::run_control::UserIntentInvocationApplication<'_>,
            >,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    /// A bare in-memory flag has runtime origin unless durable user control
    /// proves otherwise.
    #[tokio::test]
    async fn in_memory_flag_cancellation_returns_cancelled() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));
        state.stall.tool_call_records = vec![agent_record(
            "spawn",
            json!({"description":"Review runtime"}),
            Some(json!({
                "status":"launched",
                "agent_id":"agent-running",
                "description":"Review runtime"
            })),
            None,
        )];

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "flag-based cancellation must return Cancelled outcome"
        );
        assert!(
            state.interruption.is_none(),
            "runtime cancellation must not fabricate user interruption authority"
        );
        assert_eq!(
            host.cancelled_agent_ids,
            vec!["agent-running".to_string()],
            "parent cancellation must propagate to every unfinished dynamic child"
        );
    }

    /// A bare CancellationToken is runtime-owned without durable user proof.
    #[tokio::test]
    async fn in_memory_token_cancellation_returns_cancelled() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let token = CancellationToken::new();
        token.cancel();
        state.cancellation.token = Some(Arc::new(token));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "token-based cancellation must return Cancelled outcome"
        );
        assert!(state.interruption.is_none());
    }

    /// Pause flag set + cancel flag set → cancel wins inside pause loop
    /// (fast path, no DB poll needed).
    #[tokio::test]
    async fn cancel_flag_inside_pause_loop_wins_immediately() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.pause_flag = Some(Arc::new(AtomicBool::new(true)));
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation during pause should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "cancel during pause must return Cancelled immediately"
        );
        assert!(
            state.interruption.is_none(),
            "runtime cancellation must not fabricate user interruption authority"
        );
    }

    /// Pause flag set + token cancelled → cancel wins immediately.
    #[tokio::test]
    async fn cancel_token_inside_pause_loop_wins_immediately() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let token = CancellationToken::new();
        token.cancel();
        state.cancellation.pause_flag = Some(Arc::new(AtomicBool::new(true)));
        state.cancellation.token = Some(Arc::new(token));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("cancellation during pause should complete cleanly");

        assert!(
            matches!(
                prepared,
                PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
            ),
            "token cancel during pause must return Cancelled immediately"
        );
        assert!(
            state.interruption.is_none(),
            "runtime cancellation must not fabricate user interruption authority"
        );
    }

    /// Clean state (no cancel, no pause) → normal turn preparation.
    #[tokio::test]
    async fn clean_state_returns_ready_for_turn_preparation() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("normal turn preparation should succeed");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "clean state without cancel/pause must return Ready"
        );
        assert!(
            state.interruption.is_none(),
            "no interruption should be recorded for clean state"
        );
        assert_eq!(
            state.remaining_turns, 9,
            "first turn should consume one budget tick"
        );
    }

    /// Neither flag nor token set → not cancelled.
    #[tokio::test]
    async fn no_cancel_signal_returns_ready() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        // flag = None, token = None → should not cancel
        state.cancellation.flag = None;
        state.cancellation.token = None;

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("should succeed");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "None flag and None token must not trigger cancellation"
        );
    }

    /// Flag=false and token not cancelled → not cancelled.
    #[tokio::test]
    async fn flag_false_and_token_not_cancelled_returns_ready() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(false)));
        let token = CancellationToken::new();
        // token NOT cancelled
        state.cancellation.token = Some(Arc::new(token));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("should succeed");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "flag=false and token not cancelled must proceed normally"
        );
    }

    #[tokio::test]
    async fn transient_control_status_error_does_not_cancel_run() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.current_run_id = Some("run-control-error".to_string());
        state.run_control = Some(Arc::new(FailingStatusRunControl));

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("control-plane lookup failure should fail open");

        assert!(
            matches!(prepared, PreparedTurnIteration::Ready(_)),
            "transient control status errors must not be treated as cancellation"
        );
        assert!(
            state.interruption.is_none(),
            "no cancellation interruption should be recorded for control-plane errors"
        );
    }

    #[tokio::test]
    async fn between_turn_control_status_poll_is_shared_for_cancel_and_pause_checks() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.owner_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-control-clean".to_string());
        let provider = Arc::new(CountingStatusRunControl::new(None));
        state.run_control = Some(provider.clone());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("clean durable control state should proceed");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));
        assert_eq!(
            provider.calls(),
            1,
            "between-turn cancel and pause checks should share one durable control poll"
        );
    }

    #[tokio::test]
    async fn between_turn_cross_pod_cancel_uses_single_control_poll() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.owner_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-control-cancel".to_string());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        state.cancellation.flag = Some(cancel_flag.clone());
        let provider = Arc::new(CountingStatusRunControl::new(Some(
            RunControlStatus::Cancelled,
        )));
        state.run_control = Some(provider.clone());

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("durable cancel should complete cleanly");

        assert!(matches!(
            prepared,
            PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
        ));
        assert_eq!(
            provider.calls(),
            1,
            "durable cancel should not be followed by a second pause poll"
        );
        assert!(
            cancel_flag.load(Ordering::Acquire),
            "durable cancel should sync the same-pod cancel flag"
        );
    }

    #[tokio::test]
    async fn canonical_user_marker_wins_over_execution_lease_loss() {
        let mut state = make_state();
        state.owner_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-user-cancelled".to_string());
        state.cancellation.execution_lease_lost = Some(Arc::new(AtomicBool::new(true)));
        state.run_control = Some(Arc::new(
            CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                .with_cancellation_origin(CancellationOrigin::User),
        ));

        assert_eq!(
            resolve_cancellation_origin(&mut state).await,
            CancellationOrigin::User,
            "the durable user marker is stronger evidence than the local lease fence it caused"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stalled_cancellation_origin_provider_is_bounded_and_unverified() {
        let mut state = make_state();
        state.owner_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-stalled-origin".to_string());
        state.run_control = Some(Arc::new(
            CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                .with_pending_cancellation_origin(),
        ));

        let resolution = resolve_cancellation_origin(&mut state);
        tokio::pin!(resolution);
        tokio::select! {
            _ = &mut resolution => panic!("stalled origin lookup must not settle before its bound"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT).await;
        assert_eq!(resolution.await, CancellationOrigin::Unverified);
    }

    #[tokio::test]
    async fn durable_user_marker_upgrades_provisional_origin_and_never_downgrades() {
        for provisional in [CancellationOrigin::Runtime, CancellationOrigin::Unverified] {
            let mut state = make_state();
            state.owner_user_id = Some("user-control".to_string());
            state.current_run_id = Some("run-origin-settlement".to_string());
            let provider = Arc::new(
                CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                    .with_cancellation_origin_sequence(provisional, CancellationOrigin::User),
            );
            state.run_control = Some(provider.clone());

            assert_eq!(resolve_cancellation_origin(&mut state).await, provisional);
            assert_eq!(
                resolve_cancellation_origin(&mut state).await,
                CancellationOrigin::User,
                "a durable User marker must upgrade {provisional:?} before terminal settlement"
            );
            assert_eq!(
                state.cancellation.resolved_origin,
                Some(CancellationOrigin::User),
                "terminal and descendant consumers share the upgraded User authority"
            );
            assert_eq!(provider.origin_calls(), 2);
            assert_eq!(
                resolve_cancellation_origin(&mut state).await,
                CancellationOrigin::User
            );
            assert_eq!(
                provider.origin_calls(),
                2,
                "canonical User authority is monotonic and needs no further lookup"
            );
        }

        let mut state = make_state();
        state.owner_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-user-origin-monotonic".to_string());
        let provider = Arc::new(
            CountingStatusRunControl::new(Some(RunControlStatus::Cancelled))
                .with_cancellation_origin_sequence(
                    CancellationOrigin::User,
                    CancellationOrigin::Runtime,
                ),
        );
        state.run_control = Some(provider.clone());
        assert_eq!(
            resolve_cancellation_origin(&mut state).await,
            CancellationOrigin::User
        );
        assert_eq!(
            resolve_cancellation_origin(&mut state).await,
            CancellationOrigin::User
        );
        assert_eq!(provider.origin_calls(), 1, "User must never be downgraded");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn paused_loop_uses_slow_durable_poll_but_keeps_local_cancel_fast() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.owner_user_id = Some("user-control".to_string());
        state.current_run_id = Some("run-control-paused".to_string());
        let pause_flag = Arc::new(AtomicBool::new(true));
        let cancel_flag = Arc::new(AtomicBool::new(false));
        state.cancellation.pause_flag = Some(pause_flag);
        state.cancellation.flag = Some(cancel_flag.clone());
        let provider = Arc::new(CountingStatusRunControl::new(Some(
            RunControlStatus::Paused,
        )));
        state.run_control = Some(provider.clone());

        let mut prepared = Box::pin(prepare_turn_iteration(&mut host, &mut state, 0));

        tokio::select! {
            _ = &mut prepared => panic!("paused run should not complete before control changes"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(provider.calls(), 0);

        tokio::time::advance(PAUSED_RUN_DURABLE_CONTROL_POLL_INTERVAL - Duration::from_millis(1))
            .await;
        tokio::select! {
            _ = &mut prepared => panic!("paused run should not complete before control changes"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(
            provider.calls(),
            0,
            "paused run should not poll durable control before the slow interval"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::time::advance(PAUSE_LOOP_LOCAL_CHECK_INTERVAL).await;
        tokio::select! {
            _ = &mut prepared => panic!("paused run should not complete while durable status remains paused"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(
            provider.calls(),
            1,
            "paused run should poll durable control after the slow interval and one local tick"
        );

        cancel_flag.store(true, Ordering::Release);
        tokio::time::advance(PAUSE_LOOP_LOCAL_CHECK_INTERVAL).await;
        let prepared = prepared
            .await
            .expect("local cancel should exit paused loop");
        assert!(matches!(
            prepared,
            PreparedTurnIteration::Finished(AgenticLoopOutcome::Cancelled)
        ));
        assert_eq!(
            provider.calls(),
            1,
            "local cancel should not wait for another durable control poll"
        );
    }

    /// Runtime cancellation does not write a user interruption record.
    #[tokio::test]
    async fn cancellation_sets_interruption_record() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.cancellation.flag = Some(Arc::new(AtomicBool::new(true)));

        let _ = prepare_turn_iteration(&mut host, &mut state, 0).await;

        assert!(state.interruption.is_none());
    }

    // ── estimate_context_pressure tests ────────────────────────────
    //
    // Session 540c37d1: the old estimate_tokens (without schema tokens)
    // underestimated CJK tokens by ~50%, causing pressure to read 0.61
    // when it was actually 0.89. These tests ensure the unified function
    // returns correct pressure across all scenarios.

    #[test]
    fn estimate_context_pressure_zero_max_tokens() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let (pressure, tokens) = estimate_context_pressure(&messages, 0, 0);
        assert_eq!(pressure, 0.0, "zero max_tokens → zero pressure");
        assert_eq!(tokens, 0, "zero max_tokens → zero token count");
    }

    #[test]
    fn estimate_context_pressure_empty_messages() {
        let messages: Vec<serde_json::Value> = vec![];
        let (pressure, _tokens) = estimate_context_pressure(&messages, 0, 100_000);
        // estimate_tokens has a base overhead even for empty messages,
        // so pressure is non-zero. What matters: it's well below warning.
        assert!(pressure < 0.2, "empty messages pressure stays low");

        // With zero max_tokens: returns (0.0, 0) via guard.
        let (pressure_zero, tokens_zero) = estimate_context_pressure(&messages, 0, 0);
        assert_eq!(pressure_zero, 0.0);
        assert_eq!(tokens_zero, 0);
    }

    #[test]
    fn estimate_context_pressure_normal_ascii() {
        let messages = vec![
            json!({"role": "system", "content": "You are a helpful assistant."}),
            json!({"role": "user", "content": "What is 2+2?"}),
        ];
        let (pressure, tokens) = estimate_context_pressure(&messages, 5_000, 100_000);
        assert!(
            pressure > 0.0,
            "non-empty messages produce non-zero pressure"
        );
        assert!(
            pressure < 0.3,
            "short messages with schema well under budget; got {pressure}"
        );
        assert!(tokens > 0, "tokens must be > 0");
    }

    #[test]
    fn estimate_context_pressure_schema_tokens_raise_pressure() {
        let messages: Vec<serde_json::Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("message {}", i)}))
            .collect();
        let (p_without, _) = estimate_context_pressure(&messages, 0, 100_000);
        let (p_with, _) = estimate_context_pressure(&messages, 50_000, 100_000);
        assert!(
            p_with > p_without,
            "50K schema tokens must raise pressure above baseline"
        );
    }

    #[test]
    fn estimate_context_pressure_cjk_messages_count_tokens() {
        let messages: Vec<serde_json::Value> = (0..50)
            .map(|i| {
                json!({"role": "user", "content": format!("这是第{}条中文消息，包含较多的中文字符以确保token估算准确。", i)})
            })
            .collect();
        let (pressure, tokens) = estimate_context_pressure(&messages, 10_000, 100_000);
        assert!(tokens > 5_000, "50 CJK messages produce substantial tokens");
        assert!(
            pressure > 0.05,
            "50 CJK messages produce measurable pressure"
        );
    }

    #[test]
    fn estimate_context_pressure_scales_with_message_count() {
        let make_messages = |n: usize| -> Vec<serde_json::Value> {
            (0..n)
                .map(|i| json!({"role": "user", "content": format!("message number {}", i)}))
                .collect()
        };
        let (p10, _) = estimate_context_pressure(&make_messages(10), 0, 100_000);
        let (p50, _) = estimate_context_pressure(&make_messages(50), 0, 100_000);
        let (p100, _) = estimate_context_pressure(&make_messages(100), 0, 100_000);
        assert!(p100 > p50, "100 msgs > 50 msgs pressure");
        assert!(p50 > p10, "50 msgs > 10 msgs pressure");
        assert!(p100 > p10, "100 msgs > 10 msgs pressure");
    }

    #[test]
    fn estimate_context_pressure_for_state_reuses_last_assembly_measurement() {
        let mut state = make_state();
        state.max_turn_input_tokens = 100_000;
        state.messages = vec![json!({
            "role": "user",
            "content": "a short retained message",
        })];
        state.pinned_tool_schema_tokens = 2_000;

        let fallback_tokens = estimate_context_pressure(
            &state.messages,
            state.pinned_tool_schema_tokens as usize,
            state.max_turn_input_tokens,
        )
        .1;
        state.last_llm_context_manifest_trace = Some(json!({
            "system_prompt_tokens": 4_000,
            "volatile_preamble_tokens": 100,
            "wire": {
                "budget": {
                    "estimated_system_tokens": 3_900,
                },
            },
        }));

        let (pressure, measured_tokens) = estimate_context_pressure_for_state(&state);
        assert!(
            measured_tokens < fallback_tokens,
            "a measured prompt must replace the stale fallback estimate"
        );
        assert_eq!(
            measured_tokens,
            crate::prompts::estimate_tokens(&state.messages, 2_000, 3_900) as u64
        );
        assert_eq!(
            pressure,
            measured_tokens as f64 / state.max_turn_input_tokens as f64
        );
    }

    #[test]
    fn structured_reanchor_updates_working_memory_before_turn() {
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "No, that's wrong; use the server-side executor.".into();
        state.messages = vec![json!({"role": "user", "content": state.message})];
        crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
        // The last fragment and cumulative instruction are not the frozen
        // canonical human source of this judgment.
        state.message = "uncaptured last fragment".into();
        state.user_intent = "obsolete cumulative instruction".into();
        let hub = make_hub();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));
        state.restricted_tools.insert("bash".into());
        {
            let memory = state
                .pipeline_session
                .as_mut()
                .expect("pipeline session")
                .working_memory_mut();
            memory.push_decision("keep durable project fact");
            memory.push_blocker("stale tool outage");
            memory.set_next_action("retry stale path");
        }

        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct);
        assert!(apply_current_user_turn_semantics(&mut state, &intent));

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("keep durable project fact"));
        assert!(!rendered.contains("stale tool outage"));
        assert!(!rendered.contains("retry stale path"));
        assert!(
            rendered.contains("Latest user correction overrides conflicting prior working memory")
        );
        assert!(rendered.contains("server-side executor"));
        assert!(!rendered.contains("uncaptured last fragment"));
        assert!(!rendered.contains("obsolete cumulative instruction"));
        assert!(state.restricted_tools.contains("bash"));
        let feedback_count = hub.recent_feedback_signals().len();
        assert_eq!(feedback_count, 1);
        assert!(!apply_current_user_turn_semantics(&mut state, &intent));
        assert_eq!(hub.recent_feedback_signals().len(), feedback_count);
        assert_eq!(
            state
                .pipeline_session
                .as_ref()
                .unwrap()
                .working_memory()
                .render_prompt_section(),
            rendered,
        );
    }

    #[tokio::test]
    async fn prepare_turn_without_judged_reanchor_does_not_reanchor_working_memory() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "No, that's wrong; use the server-side executor.".into();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];
        {
            let memory = state
                .pipeline_session
                .as_mut()
                .expect("pipeline session")
                .working_memory_mut();
            memory.push_blocker("current blocker");
            memory.set_next_action("continue current path");
        }

        let prepared = prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .expect("turn should prepare");

        assert!(matches!(prepared, PreparedTurnIteration::Ready(_)));

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("current blocker"));
        assert!(rendered.contains("continue current path"));
    }

    #[test]
    fn user_correction_reanchors_transient_runtime_state_before_turn() {
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "不是修修补补，我要的是第一性原则系统性修复。".into();
        state.user_intent = state.message.clone();

        state.turn_guard.nudge_count = 4;
        state.turn_guard.record_tool_calls(&[
            json!({"function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"}}),
            json!({"function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"}}),
        ]);
        for _ in 0..3 {
            state
                .turn_guard
                .record_tool_result("bash", "Error: command timed out");
            state.turn_guard.health.record_failure("bash");
        }
        assert!(state.turn_guard.health.is_avoidance_advised("bash"));
        assert!(!state.turn_guard.tool_sigs.is_empty());
        assert!(state.turn_guard.errors.recent_error_pressure() > 0);

        state.restricted_tools.insert("bash".into());

        assert!(apply_structured_user_reanchor(
            &mut state,
            astra_turn_types::ObjectiveRelation::Correct,
            "correct the stale approach",
        ));

        assert_eq!(state.turn_guard.nudge_count, 0);
        assert!(state.turn_guard.tool_sigs.is_empty());
        assert_eq!(state.turn_guard.errors.recent_error_pressure(), 0);
        assert!(
            state.turn_guard.health.is_avoidance_advised("bash"),
            "durable tool diagnostics should remain available"
        );
        assert_eq!(
            state.restricted_tools,
            HashSet::from(["bash".to_string()]),
            "semantic re-anchoring must not broaden the hard capability surface"
        );
    }

    #[test]
    fn canonical_effects_fill_assessment_without_replaying_feedback() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct)
            .with_feedback(astra_turn_types::UserFeedback {
                kind: astra_turn_types::UserFeedbackKind::Correction,
                target: astra_turn_types::UserFeedbackTarget::Approach,
            });
        let mut state = make_state();
        let hub = make_hub();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));
        state.telemetry.observability_session = Some(make_session());
        state.message = "repair it".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role":"user","content":state.message})];
        assert!(apply_current_user_turn_semantics(&mut state, &intent));
        let assessment = astra_turn_types::TurnAssessment {
            difficulty: astra_turn_types::TaskDifficulty::Easy,
            ..Default::default()
        };
        let assessed_intent = TurnIntent {
            assessment: Some(assessment),
            ..intent
        };
        state.turn_guard.nudge_count = 2;
        assert!(!apply_current_user_turn_semantics(
            &mut state,
            &assessed_intent
        ));
        assert_eq!(
            state.turn_guard.nudge_count, 2,
            "assessment must not reanchor again"
        );
        assert_eq!(hub.recent_feedback_signals().len(), 1);
        let recorded = astra_turn_types::user_turn_semantics(&state.messages[0])
            .unwrap()
            .unwrap();
        assert_eq!(recorded.assessment, Some(assessment));
    }

    #[test]
    fn canonical_effects_apply_structured_reanchor_once() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct)
            .with_feedback(astra_turn_types::UserFeedback {
                kind: astra_turn_types::UserFeedbackKind::Correction,
                target: astra_turn_types::UserFeedbackTarget::Approach,
            });
        let mut state = make_state();
        state.current_run_id = Some("feedback-source-run".into());
        let hub = make_hub();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));
        state.telemetry.observability_session = Some(make_session());
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "不是修修补补，我要的是第一性原则系统性修复。".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        state.turn_guard.nudge_count = 2;
        state.restricted_tools.insert("bash".into());
        state
            .pipeline_session
            .as_mut()
            .expect("pipeline session")
            .working_memory_mut()
            .set_next_action("stale path");

        assert!(apply_current_user_turn_semantics(&mut state, &intent));
        assert!(!apply_current_user_turn_semantics(&mut state, &intent));

        assert_eq!(state.turn_guard.nudge_count, 0);
        assert_eq!(state.restricted_tools, HashSet::from(["bash".to_string()]));
        let signals = hub.recent_feedback_signals();
        let correction = signals
            .iter()
            .find(|signal| signal.signal_type == astra_core::feedback::SignalType::Correction)
            .expect("typed correction feedback");
        assert_eq!(
            signals
                .iter()
                .filter(|signal| {
                    signal.signal_type == astra_core::feedback::SignalType::Correction
                })
                .count(),
            1,
            "one user turn must emit one correction signal"
        );
        assert_eq!(correction.context["source_run_id"], "feedback-source-run");
        assert_eq!(
            correction.turn_id, None,
            "the source run is not the rated response"
        );
        assert_eq!(correction.context["objective_relation"], "correct");
        assert_eq!(correction.context["feedback_kind"], "correction");
        assert_eq!(correction.context["feedback_target"], "approach");
        let session = state
            .telemetry
            .observability_session
            .as_ref()
            .expect("observability session");
        let session = astra_core::sync_poison::recover_rwlock_read(session);
        assert_eq!(session.user_corrections.len(), 1);
        assert_eq!(
            session
                .recent_correction_excerpts
                .last()
                .map(String::as_str),
            Some("不是修修补补，我要的是第一性原则系统性修复。")
        );
        assert_eq!(session.recent_correction_excerpts.len(), 1);
        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(!rendered.contains("stale path"));
        assert!(
            rendered.contains("Latest user correction overrides conflicting prior working memory")
        );
    }

    #[test]
    fn canonical_effects_retain_typed_requirement_for_next_model_boundary() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Refine)
            .with_feedback(astra_turn_types::UserFeedback {
                kind: astra_turn_types::UserFeedbackKind::Requirement,
                target: astra_turn_types::UserFeedbackTarget::Verification,
            });
        let mut state = make_state();
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        state.message = "Run the online database journey before merging.".into();
        state.user_intent = state.message.clone();
        state.messages = vec![json!({"role": "user", "content": state.message.clone()})];

        assert!(apply_current_user_turn_semantics(&mut state, &intent));

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("Latest user requirement for verification"));
        assert!(rendered.contains("Run the online database journey before merging."));
    }

    #[test]
    fn structured_correction_feedback_does_not_require_profile_session() {
        let intent = TurnIntent::default()
            .with_objective_relation(astra_turn_types::ObjectiveRelation::Correct);
        let mut state = make_state();
        let hub = make_hub();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));
        assert!(state.telemetry.observability_session.is_none());

        apply_judged_turn_intent_to_observability(&state, &intent, true, &state.message);

        assert!(
            hub.recent_feedback_signals().iter().any(|signal| {
                signal.signal_type == astra_core::feedback::SignalType::Correction
            })
        );
    }

    #[test]
    fn acknowledgement_implies_acceptance_but_work_relations_do_not() {
        let hub = make_hub();
        let mut state = make_state();
        state.telemetry.observability_hub = Some(Arc::clone(&hub));

        apply_judged_turn_intent_to_observability(
            &state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Continue),
            true,
            &state.message,
        );
        assert!(hub.recent_feedback_signals().is_empty());
        apply_judged_turn_intent_to_observability(
            &state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Replace),
            true,
            &state.message,
        );
        assert!(hub.recent_feedback_signals().is_empty());

        apply_judged_turn_intent_to_observability(
            &state,
            &TurnIntent::default()
                .with_objective_relation(astra_turn_types::ObjectiveRelation::Acknowledge),
            true,
            &state.message,
        );
        assert!(
            hub.recent_feedback_signals()
                .iter()
                .any(|signal| signal.signal_type == astra_core::feedback::SignalType::Acceptance)
        );
    }

    // ── Full pipeline integration test ─────────────────────────────
    //
    // Exercise pressure, durable recovery and quiet-mode telemetry together.

    fn high_pressure_cjk_state(max_tokens: u64, schema_tokens: usize) -> AgenticLoopState {
        let mut state = make_state();
        state.max_turn_input_tokens = max_tokens;
        state.pinned_tool_schema_tokens = schema_tokens as u64;
        state.messages = vec![json!({"role":"user", "content":"继续检查中文工具证据"})];
        for index in 0..75 {
            let id = format!("cjk-{index}");
            state.messages.extend([
                json!({"role":"assistant", "tool_calls":[{"id":id, "type":"function", "function":{"name":"read_file", "arguments":"{}"}}]}),
                json!({"role":"tool", "tool_call_id":id, "content":format!("证据{index}: {}", "已确认的文件内容与任务进展，不能无依据地丢失。".repeat(10))}),
            ]);
        }
        state
    }

    #[tokio::test]
    async fn prepare_turn_with_cjk_pressure_runs_full_pipeline() {
        // Small context window (32K) + CJK messages + schema tokens
        // pushes pressure well above the 0.70 trigger for ≤32K windows.
        let mut host = MockHost::new(Vec::new());
        let mut state = high_pressure_cjk_state(32_000, 10_000);

        super::super::host::tests::admit_history_recovery(&mut host, &mut state);
        let result = prepare_turn_iteration(&mut host, &mut state, 1).await;
        assert!(
            result.is_ok(),
            "prepare_turn_iteration must not fail under high pressure: {:?}",
            result.err()
        );

        // Durable recovery reduces the live set without dropping evidence.
        assert!(
            state.messages.len() < 150,
            "messages must be compacted under high pressure; got {}",
            state.messages.len()
        );
        // CJK + schema tokens at 32K window must trigger at least
        // CompactHistory-level compaction (significant reduction).
        assert!(
            state.messages.len() <= 100,
            "CJK session must be significantly compacted (≤100 from 150); got {}",
            state.messages.len()
        );

        // Message-count reduction proves this canonical rewrite. Boundary
        // metadata stays in state.messages and is removed only by provider
        // projection. The public compaction_pipeline integration cases check
        // its placement; this case retains the quiet-mode telemetry contract.

        assert!(
            state.context_compression_triggered,
            "quiet/headless rendering must not erase the turn-level compression fact"
        );
        assert!(
            !host.compaction_events.is_empty(),
            "quiet mode must preserve structured compaction callbacks"
        );
        let compaction_event = state
            .step_recorder
            .events()
            .iter()
            .find(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::CompactionFired
            })
            .expect(
                "quiet mode may suppress UI output, but durable compaction audit must be emitted",
            );
        let compaction_kind = compaction_event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("kind"))
            .and_then(serde_json::Value::as_str)
            .expect("production compaction paths must identify their strategy");
        assert!(
            matches!(
                compaction_kind,
                "proactive_default" | "proactive_aggressive"
            ),
            "unexpected compaction strategy: {compaction_kind}"
        );
    }

    #[tokio::test]
    async fn quiet_changes_only_compaction_rendering_not_typed_runtime_facts() {
        #[derive(Debug)]
        struct Capture {
            messages: Vec<serde_json::Value>,
            callbacks: Vec<CompactionEvent>,
            durable_facts: Vec<(
                astra_pipeline::step_protocol::StepEventType,
                Option<serde_json::Value>,
            )>,
            effectiveness: (u64, bool, u64, u32, u32),
            compression_triggered: bool,
            tier: CompactionTier,
            rendered_summaries: Vec<String>,
        }

        let mut captures = Vec::new();
        for quiet in [true, false] {
            let mut host = MockHost::new(Vec::new()).with_quiet(quiet);
            let mut state = high_pressure_cjk_state(50_000, 10_000);
            for message in &mut state.messages {
                if message["role"] == "tool" {
                    message["content"] = json!("完全相同的中文文件证据。".repeat(160));
                }
            }
            super::super::host::tests::admit_history_recovery(&mut host, &mut state);

            let prepared = prepare_turn_iteration(&mut host, &mut state, 1)
                .await
                .expect("the shared compaction input must be accepted");
            let PreparedTurnIteration::Ready(prep) = prepared else {
                panic!("compaction fixture must remain ready for provider execution");
            };
            assert_eq!(
                prep.quiet, quiet,
                "render policy must be the only varied input"
            );

            let durable_facts = state
                .step_recorder
                .events()
                .iter()
                .filter(|event| {
                    event.event_type
                        == astra_pipeline::step_protocol::StepEventType::CompactionFired
                })
                .map(|event| (event.event_type.clone(), event.payload.clone()))
                .collect();
            captures.push(Capture {
                messages: state.messages,
                callbacks: host.compaction_events,
                durable_facts,
                effectiveness: (
                    state.compaction_effectiveness.last_tokens_freed,
                    state.compaction_effectiveness.last_was_insufficient,
                    state.compaction_effectiveness.cumulative_tokens_freed,
                    state.compaction_effectiveness.attempt_count,
                    state.compaction_effectiveness.consecutive_futile_attempts,
                ),
                compression_triggered: state.context_compression_triggered,
                tier: state.compact_tier_applied,
                rendered_summaries: host.rendered_compaction_summaries,
            });
        }

        let quiet = &captures[0];
        let rendered = &captures[1];
        assert!(quiet.compression_triggered, "fixture must actually compact");
        assert_eq!(quiet.callbacks, rendered.callbacks);
        assert_eq!(quiet.durable_facts, rendered.durable_facts);
        assert_eq!(quiet.effectiveness, rendered.effectiveness);
        assert_eq!(quiet.compression_triggered, rendered.compression_triggered);
        assert_eq!(quiet.tier, rendered.tier);
        assert_eq!(
            quiet.messages, rendered.messages,
            "quiet must not alter the compacted conversation"
        );

        assert!(
            quiet.rendered_summaries.is_empty(),
            "quiet suppresses rendering only"
        );
        assert_eq!(
            rendered.rendered_summaries,
            rendered
                .callbacks
                .iter()
                .map(|event| event.summary.clone())
                .collect::<Vec<_>>(),
            "rendered mode projects every typed callback without changing it"
        );
    }

    /// Resume-time compaction: when turn_index==0 and there are >10
    /// messages (e.g. restored from checkpoint), pressure estimation
    /// should trigger proactive compression before the first LLM call.
    #[tokio::test]
    async fn prepare_turn_resume_pressure_preserves_uncovered_user_requests() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 32_000;
        state.pinned_tool_schema_tokens = 10_000;
        state.messages = (0..100)
            .map(|i| {
                json!({"role": "user", "content": format!("CJK压力测试第{}条消息——确保恢复路径也能正常压缩上下文窗口。", i)})
            })
            .collect();

        // turn_index == 0 triggers resume compaction path
        let result = prepare_turn_iteration(&mut host, &mut state, 0).await;
        assert!(
            result.is_ok(),
            "resume compaction must not fail: {:?}",
            result.err()
        );
        assert_eq!(
            state.messages.len(),
            100,
            "pressure cannot drop unique user requests without a covering summary"
        );
    }

    #[tokio::test]
    async fn provider_measurement_triggers_compaction_when_text_estimate_is_low() {
        for turn_index in [0, 1] {
            let mut host = MockHost::new(Vec::new());
            let mut state = make_state();
            state.max_turn_input_tokens = 200_000;
            state.messages = vec![json!({"role":"user", "content":"Complete the task"})];
            state.messages.extend((0..30).map(|index| {
                json!({"role":"assistant", "content":format!("evidence {index}: {}", "detail ".repeat(300))})
            }));
            assert!(estimate_context_pressure_for_state(&state).0 < 0.5);
            state.last_measured_prompt_tokens = Some(195_000);

            super::super::host::tests::admit_history_recovery(&mut host, &mut state);
            prepare_turn_iteration(&mut host, &mut state, turn_index)
                .await
                .unwrap();

            assert!(
                host.compaction_events.iter().any(|event| matches!(
                    event.kind,
                    CompactionKind::Resume | CompactionKind::ProactiveAggressive
                )),
                "provider pressure must reach the compaction owner before the next request"
            );
        }
    }

    #[tokio::test]
    async fn prepare_turn_with_low_pressure_skips_compaction() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 200_000; // large window
        state.messages = (0..5)
            .map(|i| json!({"role": "user", "content": format!("msg {}", i)}))
            .collect();

        let result = prepare_turn_iteration(&mut host, &mut state, 1).await;
        assert!(result.is_ok());

        // Low pressure should leave all 5 messages intact.
        assert_eq!(
            state.messages.len(),
            5,
            "low-pressure messages must not be compacted"
        );
    }
}
