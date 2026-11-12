#[cfg(test)]
use crate::turn::cloud::compaction::adjust_spill_boundary_for_tool_pairs;
use crate::turn::cloud::compaction::build_spill_summary;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::agentic::headless_round::HeadlessStderrStyle;
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CompletionAction,
    DIRECT_CHILD_RESULT_SCHEMA, HostTurnResult, RejectedToolCall, RunControlProvider,
    ToolCallAdmission, TurnPhaseKind, TurnPhaseOutcome, UserIntentState,
    WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT, complete_turn_phase, finalize_turn_trace,
    try_write_heavy_checkpoint,
};
use super::lifecycle::{
    TurnIterationPrep, current_agentic_step, interruption_diagnosis_summary,
    interruption_state_summary, mark_work_settlement_incomplete, session_turn_number,
    tool_record_is_workspace_mutation, wait_for_pause_clear_or_cancel,
};
use super::verification_frontier::VerificationRecoveryError;
use crate::turn::run_control::{ProviderBoundaryAuthorization, UserIntentAdmissionAuthority};
use astra_config::user_profile::WorkspaceMutationIntent;
use astra_services::SessionArtifactStore;
use astra_turn_core::agentic_turn_ingest::{
    AgenticIngestIterationControl, AgenticTurnIngestMut, AgenticTurnIngestOutcome,
    AgenticTurnStreamSnapshot, agentic_turn_stream_snapshot_with_kind, ingest_agentic_turn_stream,
    map_ingest_outcome_to_iteration_control,
};
use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind, CompactionTier};
use astra_turn_core::interaction_types::TurnInteractionMode;
use astra_turn_core::interruption::{InterruptionKind, InterruptionRecord, ResumeAction};
use astra_turn_types::NormalizedPromptCacheUsage;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const USER_INTENT_EMPTY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_USER_INTENT_BOUNDARY_PAGES: usize = 16;
const MAX_USER_INTENT_BOUNDARY_FACTS: usize = 4_096;
const MAX_TEXTLESS_RESPONSE_RETRIES: u32 = 1;

/// Use the existing journal/trace sink, including its early-error flush. The
/// digest joins the adopted evidence to the required-context payload without
/// copying potentially private child output into another trace field.
fn record_direct_child_barrier<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    parent_run_id: &str,
    outcome: &str,
    children: &serde_json::Value,
    started: Instant,
    source_tool_call_id: Option<&str>,
) {
    let children = children.as_array().cloned().unwrap_or_default();
    let identities: Vec<_> = children
        .iter()
        .map(|child| {
            serde_json::json!({
                "agent_id": child["agent_id"],
                "run_id": child["run_id"],
                "parent_agent_id": child["parent_agent_id"],
                "status": child["status"],
                "result_sha256": child["result_sha256"],
                "result_truncated": child["result_truncated"],
                "result_range": child["result_range"],
            })
        })
        .collect();
    let replies = state.messaging.question_correlations.pending(parent_run_id);
    let reply_identities: Vec<_> = replies
        .iter()
        .map(|reply| {
            serde_json::json!({
                "request_id": reply.request_id,
                "responder_run_id": reply.expected_responder.run_id,
                "responder_agent_id": reply.expected_responder.agent_id,
            })
        })
        .collect();
    let mut attrs = std::collections::HashMap::from([
        ("parent_run_id".to_string(), parent_run_id.to_string()),
        ("outcome".to_string(), outcome.to_string()),
        (
            "children".to_string(),
            serde_json::json!(identities).to_string(),
        ),
        (
            "pending_replies".to_string(),
            serde_json::json!(reply_identities).to_string(),
        ),
        (
            "round_index".to_string(),
            state.current_round_index.to_string(),
        ),
    ]);
    if let Some(id) = source_tool_call_id {
        attrs.insert("tool_call_id".into(), id.into());
        attrs.insert("wait_kind".into(), "observation".into());
    }
    tracing::info!(target: "astra::direct_child_barrier", parent_run_id, outcome,
        children = %serde_json::json!(identities), pending_replies = %serde_json::json!(reply_identities),
        duration_ms = started.elapsed().as_millis() as u64, "agent dependency boundary");
    host.on_direct_child_completion_boundary(state, outcome, children.len(), started);
    let end_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    if let Some(buffer) = state.turn_event_buffer.as_mut() {
        let event = astra_services::session_journal::TraceSpanBuilder::default()
            .session_id(state.current_session_id.as_deref())
            .turn(Some(state.session_turn))
            .span_id(format!("direct_child_barrier_{}", Uuid::new_v4()))
            .name("agent_dependency_boundary".into())
            .trace_id(Some(parent_run_id.to_string()))
            .start_us(end_us.saturating_sub(started.elapsed().as_micros() as u64))
            .end_us(end_us)
            .attrs(Some(&attrs))
            .build()
            .with_producer_scope(Some(parent_run_id));
        buffer.record(event);
    }
}

/// Record only result bytes present in the final request of a successful
/// provider turn. Collection, artifact storage and a completed child alone do
/// not prove delivery. Reuse the dependency boundary, not a second read ledger.
pub(crate) fn record_observed_child_reads<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    messages: &[serde_json::Value],
    started: Instant,
) {
    let Some(parent) = state.current_run_id.clone() else {
        return;
    };
    let children = observed_child_read_windows(messages, &parent);
    if !children.is_empty() {
        record_direct_child_barrier(
            host,
            state,
            &parent,
            "results_adopted",
            &serde_json::Value::Array(children),
            started,
            None,
        );
    }
}

fn observed_child_read_windows(
    messages: &[serde_json::Value],
    parent: &str,
) -> Vec<serde_json::Value> {
    use astra_turn_core::tool_result_storage::{TOOL_RESULT_TOOL_NAME_FIELD, tool_result_run_id};
    use serde_json::{Value, json};
    let latest_round = messages
        .iter()
        .filter(|message| tool_result_run_id(message) == Some(parent))
        .filter_map(|message| message["_round_index"].as_u64())
        .max();
    let mut children = Vec::new();
    for message in messages {
        if message["role"] != "tool"
            || tool_result_run_id(message) != Some(parent)
            || message["_round_index"].as_u64() != latest_round
        {
            continue;
        }
        let Some(name @ ("agent" | "agent_fanout")) = message[TOOL_RESULT_TOOL_NAME_FIELD].as_str()
        else {
            continue;
        };
        let Some(content) = message["content"].as_str() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(content) else {
            continue;
        };
        let items = if name == "agent_fanout" {
            value["results"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
        } else {
            std::slice::from_ref(&value)
        };
        for item in items {
            let result = if name == "agent_fanout" {
                &item["result"]
            } else {
                item
            };
            let (Some(agent), Some(run), Some(text)) = (
                result["agent_id"].as_str(),
                result["run_id"].as_str(),
                result["result"].as_str(),
            ) else {
                continue;
            };
            if result["status"] != "completed" || agent.is_empty() || run.is_empty() {
                continue;
            }
            let range = if name == "agent_fanout" {
                let (Some(start), Some(end), Some(total)) = (
                    item["result_start_offset"].as_u64(),
                    item["result_end_offset"].as_u64(),
                    item["result_bytes"].as_u64(),
                ) else {
                    continue;
                };
                if item["agent_id"] != agent
                    || item["run_id"] != run
                    || end > total
                    || end.checked_sub(start) != Some(text.len() as u64)
                {
                    continue;
                }
                [start, end, total]
            } else {
                [0, text.len() as u64, text.len() as u64]
            };
            children.push(json!({"agent_id":agent,"run_id":run,"status":"completed",
                "result_sha256":format!("{:x}",Sha256::digest(text.as_bytes())),
                "result_range":range,"result_truncated":range[0] != 0 || range[1] < range[2]}));
        }
    }
    children
}

fn stage_direct_child_results<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    let Some(owner) = host.direct_child_completion_owner(state) else {
        return Ok(false);
    };
    if state.volatile_pending.iter().any(|entry| {
        entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
            && entry.payload["parent_run_id"].as_str() != Some(owner.parent_run_id())
    }) {
        finish_direct_child_barrier_incomplete(
            state,
            "child receipt belongs to a different parent",
        );
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "Child receipt belongs to a different parent; receipt custody is unchanged.",
        ));
    }
    // The required-context lane is checkpointed. After a process restart the
    // producer may reconstruct the same terminal child from its recovery
    // snapshot; do not inject a second copy of an already retained result.
    let retained: HashSet<_> = state
        .volatile_pending
        .iter()
        .filter(|injection| injection.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA)
        .filter(|injection| {
            injection.payload["parent_run_id"].as_str() == Some(owner.parent_run_id())
        })
        .filter_map(|injection| injection.payload["children"].as_array())
        .flatten()
        .filter_map(|child| {
            Some((
                child["agent_id"].as_str()?,
                child["run_id"].as_str()?,
                child["status_fingerprint"].as_str()?,
            ))
        })
        .collect();
    let children: Vec<_> = owner
        .take_completed_direct_children()
        .into_iter()
        .map(|child| {
            child.notification(crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES)
        })
        .filter(|child| {
            !retained.contains(&(
                child["agent_id"].as_str().unwrap_or_default(),
                child["run_id"].as_str().unwrap_or_default(),
                child["status_fingerprint"].as_str().unwrap_or_default(),
            ))
        })
        .collect();
    if children.is_empty() {
        return Ok(false);
    }
    let changed: HashSet<_> = children
        .iter()
        .filter_map(|child| Some((child["agent_id"].as_str()?, child["run_id"].as_str()?)))
        .collect();
    for injection in &mut state.volatile_pending {
        if injection.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
            && injection.payload["parent_run_id"].as_str() == Some(owner.parent_run_id())
            && let Some(previous) = injection.payload["children"].as_array_mut()
        {
            previous.retain(|child| {
                !changed.contains(&(
                    child["agent_id"].as_str().unwrap_or_default(),
                    child["run_id"].as_str().unwrap_or_default(),
                ))
            });
        }
    }
    drop(changed);
    state.volatile_pending.retain(|entry| {
        entry.payload["schema"] != DIRECT_CHILD_RESULT_SCHEMA
            || entry.payload["children"]
                .as_array()
                .is_some_and(|children| !children.is_empty())
    });
    let children = serde_json::json!(children);
    record_direct_child_barrier(
        host,
        state,
        owner.parent_run_id(),
        "results_staged",
        &children,
        Instant::now(),
        None,
    );
    // This accumulative, required lane survives provider-attempt failure and
    // cannot overwrite an unrelated mailbox message or sibling result.
    let mut payload = serde_json::json!({
        "schema": DIRECT_CHILD_RESULT_SCHEMA,
        "parent_run_id": owner.parent_run_id(),
        "children": children,
        "instruction": "These are producer-owned terminal child outcomes. applied_user_intents identifies user changes actually applied to that child; it explains the child's delivery and does not replace the parent's instructions or authorize unrelated work. The completed child's result is authoritative; when passing it through, copy it exactly instead of recomputing or replacing it. result_truncated means this notification contains a preview, not that child execution failed. If the preview is insufficient, call the child's get_result to read its existing result; follow any authorized artifact read instructions returned by that call. Do not rerun or replace completed child work to recover output. Report failures and incomplete work truthfully before finalizing.",
    });
    let budget = crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES;
    let pending_bytes = |state: &AgenticLoopState| -> usize {
        state
            .volatile_pending
            .iter()
            .filter(|entry| entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA)
            .filter(|entry| entry.payload["delivery_count"].as_u64().unwrap_or_default() < 2)
            .map(|entry| {
                serde_json::to_vec(&entry.payload)
                    .expect("child notification must serialize")
                    .len()
            })
            .sum()
    };
    let payload_bytes = serde_json::to_vec(&payload)
        .expect("child notification must serialize")
        .len();
    let mut retained_bytes = pending_bytes(state);
    if retained_bytes.saturating_add(payload_bytes) > budget {
        // Correlation notices are optional; producer-owned child results win
        // even when another completion was staged in an earlier batch.
        for entry in &mut state.volatile_pending {
            if entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                && let Some(object) = entry.payload.as_object_mut()
            {
                object.remove("unobserved_questions");
            }
        }
        retained_bytes = pending_bytes(state);
    }
    if retained_bytes.saturating_add(payload_bytes) > budget {
        // Compact payloads in place: delivery counters, observation markers,
        // and parent identity belong to the existing injection, not this batch.
        for notification in state
            .volatile_pending
            .iter_mut()
            .filter(|entry| entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA)
            .filter(|entry| entry.payload["delivery_count"].as_u64().unwrap_or_default() < 2)
            .map(|entry| &mut entry.payload)
            .chain(std::iter::once(&mut payload))
        {
            for child in notification["children"]
                .as_array_mut()
                .expect("child array")
            {
                let object = child.as_object_mut().expect("child projection");
                if let Some(intents) = object.remove("applied_user_intents") {
                    object.insert(
                        "applied_user_intent_count".into(),
                        serde_json::json!(intents.as_array().map_or(0, Vec::len)),
                    );
                }
                object.insert(
                    "applied_user_intents_inline".into(),
                    serde_json::json!(false),
                );
                object.insert("result".into(), serde_json::json!(""));
                object.insert("result_truncated".into(), serde_json::json!(true));
                object.remove("result_sha256");
            }
        }
        if pending_bytes(state).saturating_add(
            serde_json::to_vec(&payload)
                .expect("child references must serialize")
                .len(),
        ) > budget
        {
            finish_direct_child_barrier_incomplete(
                state,
                "child receipt references exceed the delivery budget",
            );
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ResourceLimit,
                "Child receipt references exceed the delivery budget; complete receipts remain retained.",
            ));
        }
    }
    let questions: Vec<_> = state
        .messaging
        .question_correlations
        .pending(owner.parent_run_id())
        .into_iter()
        .filter(|question| {
            payload["children"].as_array().is_some_and(|children| {
                children.iter().any(|child| {
                    child["run_id"] == question.expected_responder.run_id
                        && child["agent_id"] == question.expected_responder.agent_id
                })
            }) && !state.volatile_pending.iter().any(|entry| {
                entry.payload["response_request_id"] == question.request_id
                    && entry.payload["sender"]["run_id"] == question.expected_responder.run_id
                    && entry.payload["sender"]["agent_id"] == question.expected_responder.agent_id
                    && entry.payload["response_match"] != "not_adopted"
            })
        })
        .collect();
    if !questions.is_empty() {
        payload["unobserved_questions"] = serde_json::json!({
            "questions": questions,
            "instruction": "No matching response was observed before this notification; it may have been sent or still be in transit. The terminal result is not an answer. Continue using the known outcome, or explicitly wait if an answer is needed.",
        });
        // This batch's optional notice may only use otherwise unused bytes.
        if pending_bytes(state).saturating_add(payload.to_string().len()) > budget {
            payload
                .as_object_mut()
                .unwrap()
                .remove("unobserved_questions");
        }
    }
    state.push_volatile_payload(
        super::host::VolatileKind::BackgroundTaskNotification,
        payload,
    );
    Ok(true)
}

fn finish_direct_child_barrier_incomplete(state: &mut AgenticLoopState, reason: &str) {
    // The child obligation controls terminal success, not ownership of text
    // already produced. Finalization retains provider text as partial output;
    // the interruption carries runtime diagnostics separately.
    if state.interruption.is_none() {
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(state, Some(reason.to_string())),
        ));
    }
}

pub(crate) async fn fence_direct_child_finalization<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    let Some(owner) = host.direct_child_completion_owner(state) else {
        return;
    };
    if !owner.has_direct_child_completion_history() {
        return;
    }
    let undelivered = state.volatile_pending.iter().any(|injection| {
        injection.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
            && injection.payload["observed_by_provider"] != true
    });
    let owner_matches_run = state.current_run_id.as_deref() == Some(owner.parent_run_id());
    let foreign_notification = state.volatile_pending.iter().any(|injection| {
        injection.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
            && injection.payload["parent_run_id"].as_str() != Some(owner.parent_run_id())
    });
    let settled = if state.interruption.is_none()
        && owner_matches_run
        && !foreign_notification
        && !undelivered
    {
        owner.finalize_direct_children_if_settled()
    } else {
        !owner.has_pending_direct_children()
    };
    let incomplete = !owner_matches_run || foreign_notification || !settled || undelivered;
    let children = owner.pending_direct_children();
    if incomplete {
        finish_direct_child_barrier_incomplete(
            state,
            "parent finalization reached an unobserved direct-child obligation",
        );
    }
    record_direct_child_barrier(
        host,
        state,
        owner.parent_run_id(),
        if incomplete {
            "finalization_incomplete"
        } else if state.interruption.is_some() {
            "finalization_interrupted"
        } else {
            "finalization_accepted"
        },
        &serde_json::json!(children),
        Instant::now(),
        None,
    );
    if !incomplete {
        // The journal is evaluated after this fence. Snapshot only exact,
        // observed receipt refs even when an unrelated interruption already
        // exists; otherwise the evaluator leaves a completed child launch
        // unresolved and turns an unrelated failure into a false child
        // failure. Removing provider context remains limited to the clean
        // finalization path so resumable interruptions retain their evidence.
        state.stall.terminal_child_evaluation_refs = Some((
            owner.parent_run_id().to_string(),
            state
                .stall
                .tool_call_records
                .iter()
                .filter(|record| nonterminal_child_receipt_superseded(state, record))
                .filter_map(|record| record.execution_completion.clone())
                .collect(),
        ));
        if state.interruption.is_none() {
            state
                .volatile_pending
                .retain(|injection| injection.payload["schema"] != DIRECT_CHILD_RESULT_SCHEMA);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeActivityOutcome {
    InputReady,
    ObservationTimedOut,
    NoPendingWork,
    Incomplete,
    ExecutionHandoff,
}

impl RuntimeActivityOutcome {
    pub(crate) fn should_continue(&self) -> bool {
        matches!(
            self,
            Self::InputReady | Self::ObservationTimedOut | Self::NoPendingWork
        )
    }
}

fn has_retained_runtime_input(state: &AgenticLoopState) -> bool {
    state.volatile_pending.iter().any(|injection| {
        matches!(
            injection.kind,
            super::host::VolatileKind::BackgroundTaskNotification
                | super::host::VolatileKind::Mailbox
        ) && !injection.attempt_leased
            && injection.payload["observed_by_provider"] != true
    })
}

/// Both synchronization edges use the same input/capacity machinery. An
/// observation timeout is not permission to finalize or cancel child work.
pub(crate) async fn await_runtime_activity<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    observation_request: Option<&astra_tools::agent_tool_contract::AgentWaitReceipt>,
) -> Result<RuntimeActivityOutcome, astra_core::ClassifiedError> {
    let owner = host.direct_child_completion_owner(state);
    let run_id = state.current_run_id.clone().unwrap_or_default();
    let retained_runtime_input = has_retained_runtime_input(state);
    if observation_request.is_none()
        && !owner
            .as_ref()
            .is_some_and(|owner| owner.has_pending_direct_children())
        && !retained_runtime_input
    {
        return Ok(RuntimeActivityOutcome::NoPendingWork);
    }
    if observation_request.is_none() {
        state.final_text_model_item_id = None;
        state.final_text.clear();
        state.final_text_streamed = false;
    }
    let started = Instant::now();
    let time_budget = host.execution_time_budget_remaining();
    let remaining = time_budget.map(|budget| budget.total_remaining);
    let admitted_now = tokio::time::Instant::now();
    let authority_deadline = remaining.map(|remaining| admitted_now + remaining);
    let work_deadline = time_budget.map(|budget| admitted_now + budget.work_remaining);
    let paused_authority_live = || {
        observation_request.is_none()
            && authority_deadline.is_none_or(|end| tokio::time::Instant::now() < end)
            && owner
                .as_ref()
                .is_some_and(|owner| owner.paused_direct_child_reason().is_some())
    };
    // Dependency pauses retain execution ownership. Only actual authority
    // expiry, cancellation or handoff may end this parked execution.
    tokio::select! {
        biased;
        _ = direct_child_parent_cancelled(&state.cancellation) => {
            return Err(astra_core::ClassifiedError::new(astra_core::ErrorKind::Cancelled,
                "parent cancelled before waiting for direct children"));
        }
        _ = std::future::ready(()) => {}
    }
    // An input wait can be restored at a slice boundary before the next loop
    // preparation. Resolve the same round authority without charging a round.
    super::lifecycle::prepare_iteration_budget(state)?;
    let slice_exhausted =
        state.remaining_turns == 0 || state.current_round_index as usize + 1 >= state.max_turns;
    let no_synthesis_budget = slice_exhausted
        || state
            .agentic_turn_budget
            .hard_turn_limit
            .is_some_and(|limit| state.llm_rounds_completed as usize >= limit.get());
    let outcome = if no_synthesis_budget {
        "synthesis_budget_exhausted"
    } else if retained_runtime_input {
        // A checkpoint can restore a provider-required child result or mailbox
        // message after its edge notification was consumed. It is already in
        // the canonical volatile custody lane, so do not park on a stale wait
        // receipt and wait for a second wake that will never arrive.
        "runtime_input_ready"
    } else {
        let children = serde_json::json!(
            owner
                .as_ref()
                .map(|owner| owner.pending_direct_children())
                .unwrap_or_default()
        );
        record_direct_child_barrier(
            host,
            state,
            &run_id,
            "wait_started",
            &children,
            started,
            observation_request.map(|request| request.tool_call_id.as_str()),
        );
        host.emit_headless_line(HeadlessStderrStyle::Dim, "Waiting for agent input.".into());
        host.release_execution_capacity_for_wait();
        let requested_wait =
            observation_request.map(|request| Duration::from_millis(request.timeout_ms));
        let observation_deadline = requested_wait.map(|wait| tokio::time::Instant::now() + wait);
        let mut deadline = match (observation_deadline, work_deadline) {
            (Some(requested), Some(authorized)) => Some(requested.min(authorized)),
            (requested, authorized) => requested.or(authorized),
        };
        let executor = state.runtime_tool_executor.clone();
        let cancellation = state.cancellation.clone();
        let handoff = host.execution_handoff_wake();
        let handoff_requested = async {
            match handoff.as_ref() {
                Some(wake) => wake.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(handoff_requested);
        let mut mailbox_open = state.messaging.mailbox.is_some();
        let mut intent_wake = state.user_intents.wake.clone();
        let mut checked_watermark = -1_i64;
        #[derive(Clone, Copy)]
        enum ReadySource {
            Child,
            Intent(Option<i64>),
            Mailbox(bool),
            ObservationTimeout,
        }
        'waiting: loop {
            // Keep remote reconciliation alive across presentation-only
            // messages. Recreate it only after a child fact was observed.
            let child_update = async {
                if let Some(owner) = owner
                    .as_ref()
                    .filter(|owner| owner.has_pending_direct_children())
                {
                    if let Some(executor) = executor.as_ref() {
                        executor.wait_for_direct_child_update(owner).await;
                    } else {
                        owner.wait_for_direct_child_update().await;
                    }
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(child_update);
            loop {
                let source = tokio::select! {
                    biased;
                    _ = direct_child_parent_cancelled(&cancellation) => {
                        record_direct_child_barrier(host, state, &run_id, "cancelled", &children, started, observation_request.map(|request| request.tool_call_id.as_str()));
                        return Err(astra_core::ClassifiedError::new(astra_core::ErrorKind::Cancelled,
                            "parent cancelled while waiting for direct children"));
                    }
                    _ = &mut handoff_requested => {
                        if authority_deadline.is_some_and(|end| tokio::time::Instant::now() >= end) {
                            break 'waiting "deadline";
                        }
                        return Ok(RuntimeActivityOutcome::ExecutionHandoff);
                    },
                    _ = wait_for_runtime_deadline(deadline) => {
                        if authority_deadline.is_some_and(|end| tokio::time::Instant::now() >= end) {
                            break 'waiting "deadline";
                        }
                        if paused_authority_live() {
                            deadline = authority_deadline;
                            continue;
                        }
                        if observation_request.is_none() {
                            break 'waiting "deadline";
                        }
                        ReadySource::ObservationTimeout
                    },
                    _ = &mut child_update => ReadySource::Child,
                    watermark = async {
                        match intent_wake.as_mut() {
                            Some(wake) => {
                                let current = *wake.borrow_and_update();
                                if current > checked_watermark {
                                    Some(current)
                                } else if wake.changed().await.is_ok() {
                                    Some(*wake.borrow_and_update())
                                } else {
                                    None
                                }
                            }
                            None => std::future::pending().await,
                        }
                    }, if intent_wake.is_some() => ReadySource::Intent(watermark),
                    message = async {
                        match state.messaging.mailbox.as_mut().filter(|_| mailbox_open) {
                            Some(mailbox) => mailbox.wait_ready().await,
                            None => std::future::pending().await,
                        }
                    } => ReadySource::Mailbox(message),
                };
                let mut boundary = match source {
                    ReadySource::Child => Some(RuntimeInputBoundary::ChildReady),
                    ReadySource::Intent(Some(index)) => {
                        checked_watermark = checked_watermark.max(index);
                        if index
                            < i64::try_from(state.user_intents.user_intent_cursor())
                                .unwrap_or(i64::MAX)
                        {
                            continue;
                        }
                        Some(RuntimeInputBoundary::IntentReady)
                    }
                    ReadySource::Intent(None) => {
                        intent_wake = None;
                        continue;
                    }
                    ReadySource::Mailbox(true) => Some(RuntimeInputBoundary::MailboxReady),
                    ReadySource::Mailbox(false) => {
                        mailbox_open = false;
                        continue;
                    }
                    ReadySource::ObservationTimeout => None,
                };
                // The observation cutoff limits parking, not ownership of an
                // admitted capacity request or input transaction. Those keep
                // the original execution deadline and cancellation boundary.
                let mut input_deadline = if boundary.is_none() || paused_authority_live() {
                    authority_deadline
                } else if observation_request.is_some() {
                    work_deadline
                } else {
                    deadline
                };
                let readmission = {
                    let capacity = host.reacquire_execution_capacity_after_wait();
                    tokio::pin!(capacity);
                    loop {
                        break tokio::select! {
                            biased;
                            _ = direct_child_parent_cancelled(&cancellation) => {
                                Err(astra_core::ClassifiedError::new(astra_core::ErrorKind::Cancelled,
                                    "parent cancelled while waiting for execution capacity"))
                            }
                            _ = &mut handoff_requested => {
                                if authority_deadline.is_some_and(|end| tokio::time::Instant::now() >= end) {
                                    Ok(None)
                                } else {
                                    Ok(Some(false))
                                }
                            },
                            _ = wait_for_runtime_deadline(input_deadline) => {
                                if paused_authority_live() {
                                    deadline = authority_deadline;
                                    input_deadline = authority_deadline;
                                    continue;
                                }
                                Ok(None)
                            },
                            result = &mut capacity => result.map(|()| Some(true)),
                        };
                    }
                };
                let readmission = match readmission {
                    Ok(None) => break 'waiting "deadline",
                    Ok(Some(false)) => return Ok(RuntimeActivityOutcome::ExecutionHandoff),
                    Err(error) if error.kind == astra_core::ErrorKind::Cancelled => {
                        record_direct_child_barrier(
                            host,
                            state,
                            &run_id,
                            "cancelled",
                            &children,
                            started,
                            observation_request.map(|request| request.tool_call_id.as_str()),
                        );
                        return Err(error);
                    }
                    Ok(Some(true)) => Ok(()),
                    Err(error) => Err(error),
                };
                if let Err(error) = readmission {
                    record_direct_child_barrier(
                        host,
                        state,
                        &run_id,
                        "readmission_failed",
                        &children,
                        started,
                        observation_request.map(|request| request.tool_call_id.as_str()),
                    );
                    return Err(error);
                }
                if observation_deadline.is_some_and(|end| tokio::time::Instant::now() >= end) {
                    // A wake can precede the cutoff but capacity arrive after
                    // it. Leave ready input in its existing owner for the next
                    // iteration; a tool timeout never cancels a child run.
                    boundary = None;
                }
                let Some(boundary) = boundary else {
                    if paused_authority_live() {
                        host.release_execution_capacity_for_wait();
                        deadline = authority_deadline;
                        continue 'waiting;
                    }
                    state.push_volatile_payload(
                        super::host::VolatileKind::RuntimeInputBoundary,
                        serde_json::json!({"agent_wait": "timed_out", "children_cancelled": false}),
                    );
                    break 'waiting "observation_timed_out";
                };
                // The whole input transaction, including DB ACK/poll I/O, is
                // inside the same absolute deadline and hard-cancel boundary.
                let observed = {
                    let input = runtime_input_boundary(host, state, boundary);
                    tokio::pin!(input);
                    loop {
                        break tokio::select! {
                            biased;
                            _ = direct_child_parent_cancelled(&cancellation) => Err(
                                astra_core::ClassifiedError::new(astra_core::ErrorKind::Cancelled,
                                    "parent cancelled while applying runtime input")),
                            _ = wait_for_runtime_deadline(input_deadline) => {
                                if paused_authority_live() {
                                    deadline = authority_deadline;
                                    input_deadline = authority_deadline;
                                    continue;
                                }
                                Ok(None)
                            },
                            result = &mut input => result.map(Some),
                        };
                    }
                };
                let observed = match observed {
                    Ok(Some(observed)) => observed,
                    Ok(None) => {
                        host.release_execution_capacity_for_wait();
                        break 'waiting "deadline";
                    }
                    Err(error) => {
                        host.release_execution_capacity_for_wait();
                        if error.kind == astra_core::ErrorKind::Cancelled {
                            record_direct_child_barrier(
                                host,
                                state,
                                &run_id,
                                "cancelled",
                                &children,
                                started,
                                observation_request.map(|request| request.tool_call_id.as_str()),
                            );
                        }
                        return Err(error);
                    }
                };
                if observed.model_context_changed
                    || matches!(source, ReadySource::Child)
                        && !owner
                            .as_ref()
                            .is_some_and(|owner| owner.has_pending_direct_children())
                {
                    break 'waiting if matches!(source, ReadySource::Child) {
                        "synthesis_ready"
                    } else {
                        "runtime_input_ready"
                    };
                }
                if matches!(source, ReadySource::Child) {
                    host.release_execution_capacity_for_wait();
                    continue 'waiting;
                }
                host.release_execution_capacity_for_wait();
            }
        }
    };
    record_direct_child_barrier(
        host,
        state,
        &run_id,
        outcome,
        &serde_json::json!(
            owner
                .as_ref()
                .map(|owner| owner.pending_direct_children())
                .unwrap_or_default()
        ),
        started,
        observation_request.map(|request| request.tool_call_id.as_str()),
    );
    if outcome == "synthesis_ready" || outcome == "runtime_input_ready" {
        // Continue through normal preparation, accounting, and budget checks.
        // This barrier never extends a slice or a user-owned hard limit.
        return Ok(RuntimeActivityOutcome::InputReady);
    }
    if outcome == "observation_timed_out" {
        return Ok(RuntimeActivityOutcome::ObservationTimedOut);
    }
    host.release_execution_capacity_for_wait();
    let _ = super::lifecycle::cancel_unfinished_child_agents(
        host,
        state,
        "parent direct-child completion boundary stopped",
        crate::orchestration::CancellationOrigin::Runtime,
    )
    .await;
    finish_direct_child_barrier_incomplete(state, outcome);
    Ok(RuntimeActivityOutcome::Incomplete)
}

async fn wait_for_runtime_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn direct_child_parent_cancelled(cancellation: &super::host::CancellationState) {
    let flagged = || {
        cancellation
            .flag
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
            || cancellation
                .execution_lease_lost
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    };
    if flagged() {
        return;
    }
    if let Some(token) = &cancellation.token {
        token.cancelled().await;
    } else if cancellation.flag.is_some() || cancellation.execution_lease_lost.is_some() {
        // Legacy hosts may have only an atomic flag. No per-parent timer is
        // scheduled for the normal token-backed runtime.
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if flagged() {
                return;
            }
        }
    } else {
        std::future::pending::<()>().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderBoundaryGate {
    Authorized,
    Paused,
}

/// Runtime-owned interpretation of a text terminal at the current provider
/// boundary. This deliberately consumes only typed settlement state: provider
/// prose is user-visible evidence, not execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalCompletionDisposition {
    /// An ordinary provider stop that was not forced by a runtime execution
    /// slice. Existing text-completion behavior remains unchanged.
    OrdinaryCompletionCandidate,
    /// A committed Work graph authorized this one final synthesis boundary.
    CommittedWorkSynthesis,
    /// A narrowly declared completion action executed and its typed
    /// obligation is now settled.
    SettledExactCompletionAction,
    /// The bounded execution slice ended, but typed runtime state proves that
    /// no executable obligation remains; deliver the provider's synthesis
    /// through the ordinary outcome/verification guards.
    RoundSliceTextDelivery,
    /// The runtime exhausted an execution slice and reserved only a summary
    /// boundary, but no typed fact established completion of the user goal.
    RoundSliceIncomplete,
}

fn terminal_completion_disposition(
    state: &AgenticLoopState,
    committed_work_synthesis_authorized: bool,
) -> TerminalCompletionDisposition {
    if missing_explicit_verification_hooks(state).is_err() {
        return TerminalCompletionDisposition::RoundSliceIncomplete;
    }
    // Canonical Work authority is independent of how the final review
    // boundary was reached. In particular, a graph may settle before the
    // generic round-slice rail fires. Requiring a budget flag here recreates
    // the Work→Run split-brain by handing terminal authority back to the
    // generic workspace-intent guard.
    if committed_work_synthesis_authorized {
        return if state
            .hooks
            .completion_settlement
            .foreground_fanout_pagination
            .is_none()
            && super::lifecycle::unfinished_parallel_agent_ids(state).is_empty()
        {
            TerminalCompletionDisposition::CommittedWorkSynthesis
        } else {
            // Canonical Work completion never overrides producer/process
            // ownership. Keep the candidate resumable, but make this text
            // boundary explicitly incomplete even outside a budget wrap-up.
            TerminalCompletionDisposition::RoundSliceIncomplete
        };
    }

    if !state.budget_wrapup_injected
        || !matches!(
            state.hooks.completion_settlement.wrapup_origin,
            Some(
                super::host::BudgetWrapupOrigin::RoundSlice
                    | super::host::BudgetWrapupOrigin::ExecutionDeadline
            )
        )
    {
        return TerminalCompletionDisposition::OrdinaryCompletionCandidate;
    }

    let exact_action_settled = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .is_some_and(|window| {
            window.consumed
                && window.matched
                && matches!(pending_completion_action(state), Ok(None))
        });
    if exact_action_settled {
        return TerminalCompletionDisposition::SettledExactCompletionAction;
    }

    // Final reporting uses the same explicit hooks, Work and execution-custody
    // guards as an ordinary terminal. An inferred task profile does not create
    // another completion obligation.
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    // A successful generic task action may spend the one terminal fallback
    // obligation, but it is still progress evidence rather than typed proof
    // that an Unknown/MayMutate user goal completed. Keep the closing text
    // boundary resumably incomplete in that case. Explicit ReadOnly intent
    // and concrete mutation/verification receipts retain their existing
    // completion authority.
    let bounded_synthesis_authorized = !active_work_attempt
        && !workspace_observation_is_quarantined(state)
        && state
            .hooks
            .completion_settlement
            .foreground_fanout_pagination
            .is_none()
        && super::lifecycle::unfinished_parallel_agent_ids(state).is_empty()
        && matches!(pending_completion_action(state), Ok(None));
    if bounded_synthesis_authorized {
        return TerminalCompletionDisposition::RoundSliceTextDelivery;
    }

    TerminalCompletionDisposition::RoundSliceIncomplete
}

fn enforce_terminal_completion_disposition_before_success(
    state: &mut AgenticLoopState,
    disposition: TerminalCompletionDisposition,
) -> bool {
    if let Err(error) = missing_explicit_verification_hooks(state) {
        finish_unavailable_verification(state, error);
        return true;
    }
    if disposition != TerminalCompletionDisposition::RoundSliceIncomplete
        || state.interruption.is_some()
    {
        return false;
    }

    // Keep the provider's useful summary, but do not promote a runtime-forced
    // handoff to successful task completion. The checkpoint is immediately
    // resumable and no additional provider or tool authority is manufactured.
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::BudgetExhausted,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some("bounded execution slice ended without typed completion authority".to_string()),
        ),
    ));
    true
}

pub(crate) async fn authorize_provider_boundary(
    state: &mut AgenticLoopState,
) -> Result<ProviderBoundaryGate, astra_core::ClassifiedError> {
    let (Some(run_control), Some(user_id), Some(run_id)) = (
        state.run_control.as_ref(),
        state.owner_user_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        // Before a run identity exists there is no durable execution row to
        // authorize. Once a run_control identity is attached, every provider
        // boundary below is exact and fail-closed.
        return Ok(ProviderBoundaryGate::Authorized);
    };
    let authority = state
        .current_run_owner_generation
        .map(UserIntentAdmissionAuthority::DurableOwnerGeneration)
        .unwrap_or(UserIntentAdmissionAuthority::ProcessLocal);
    let expected_session_id = state.current_session_id.as_deref().ok_or_else(|| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            format!("run {run_id} is missing its immutable session identity"),
        )
    })?;
    let outcome = run_control
        .authorize_provider_boundary(user_id, expected_session_id, run_id, authority)
        .await
        .map_err(|error| {
            astra_core::ClassifiedError::new(
                match authority {
                    UserIntentAdmissionAuthority::ProcessLocal => {
                        astra_core::ErrorKind::ContractViolation
                    }
                    UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => {
                        astra_core::ErrorKind::Unknown
                    }
                },
                format!("failed to authorize provider boundary for run {run_id}: {error}"),
            )
        })?;
    match outcome {
        ProviderBoundaryAuthorization::Authorized => Ok(ProviderBoundaryGate::Authorized),
        ProviderBoundaryAuthorization::Paused => {
            if let Some(flag) = state.cancellation.pause_flag.as_ref() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(ProviderBoundaryGate::Paused)
        }
        ProviderBoundaryAuthorization::Inactive { status } => {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                format!("run {run_id} became {status} before the next provider boundary"),
            ))
        }
        ProviderBoundaryAuthorization::AuthorityLost { reason } => {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                format!(
                    "run {run_id} lost execution authority before the next provider boundary: {reason}"
                ),
            ))
        }
    }
}

fn reconcile_unsettled_work_status(state: &mut AgenticLoopState) {
    // The turn-local tracker is only a projection. Reconcile every identity
    // against the producer registry immediately before freezing user-visible
    // output so a terminal transition that arrived after the last model
    // boundary cannot be rendered as still running.
    if let Some(registry) = state.stall.active_work_registry.clone() {
        let tracked = state.stall.work_unit_observations.active_work_units();
        for observation in tracked {
            if let Some(canonical) =
                registry.canonical_observation(&observation.id, &observation.kind)
            {
                state.stall.work_unit_observations.observe(&canonical);
            }
        }
        for observation in registry.active_work_observations() {
            state.stall.work_unit_observations.observe(&observation);
        }
    }
}

/// A canonical Work attempt cannot be completed by assistant prose.  This is
/// the same control-plane rule as a stop hook that refuses completion while a
/// task is still owned: give the provider one focused retry, then let the run
/// lifecycle mark the still-owned carrier failed instead of rendering an
/// uncommitted success claim or leaving it paused.
fn enforce_typed_work_settlement_before_text_completion(state: &mut AgenticLoopState) -> bool {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    enforce_typed_work_settlement_before_text_completion_for_work_state(state, active_work_attempt)
}

fn enforce_typed_work_settlement_before_text_completion_for_work_state(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
) -> bool {
    // The executor's canonical ownership is the terminal invariant. The
    // settlement-only flag is a provider-facing projection and may not have
    // been installed yet when a system-owned dispatcher resumed an existing
    // attempt. Never let that projection become completion authority.
    if !state.hooks.completion_settlement.work_settlement_only && !active_work_attempt {
        return false;
    }

    // An exact completion-action window owns this boundary. Its admission
    // guard below will either advance the typed chain or reject prose. Once
    // the window closes, active ownership becomes settlement-only.
    if state
        .hooks
        .completion_settlement
        .completion_action_window
        .is_some()
    {
        return false;
    }

    state.hooks.completion_settlement.work_settlement_only = true;
    state.hooks.completion_settlement.text_only = false;

    state.budget_wrapup_ignored_rounds = state.budget_wrapup_ignored_rounds.saturating_add(1);
    if state.budget_wrapup_ignored_rounds == 1 {
        state.final_text_model_item_id = None;
        state.final_text.clear();

        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "work_settlement_required.v1",
                "signal": "owned_work_attempt_unsettled",
                "instruction": "The previous text did not settle the currently owned WorkItem. Call settle_work_item now with the truthful typed outcome. Do not return prose and do not call any other tool.",
                "authority": "canonical_work_lifecycle",
            }),
        );
        return true;
    }

    // Do not publish a second uncommitted model answer. The outer run
    // lifecycle observes the still-owned attempt and transitions its carrier
    // to Failed, so the task board cannot remain Running or Paused.
    mark_work_settlement_incomplete(state);
    false
}

pub(crate) fn workspace_observation_is_quarantined(state: &AgenticLoopState) -> bool {
    state.stall.workspace_observation_quarantine.is_some()
        || state
            .hooks
            .workspace_root_hint
            .as_deref()
            .and_then(|root| {
                astra_tools::workspace_observation::workspace_ownership_is_unsettled(
                    std::path::Path::new(root),
                )
            })
            .unwrap_or(false)
}

/// Whether the observation state is strong enough to make a user-visible
/// completion claim impossible.
///
/// A foreground process-group receipt is an attribution quarantine: it keeps
/// later fingerprints and budget renewal fail-closed because a descendant
/// could have escaped the group, but it is not proof that this invocation is
/// still running.  Treating that advisory quarantine as an unconditional
/// terminal interruption makes every ordinary foreground writer fail on
/// hosts without delegated cgroups (and makes verifier-successful turns look
/// interrupted).  Only a typed partial commit or an explicitly unsettled
/// ownership scope is a terminal barrier.
pub(crate) fn workspace_observation_requires_terminal_incomplete(state: &AgenticLoopState) -> bool {
    state.stall.workspace_observation_quarantine.as_ref().is_some_and(|q| {
        q.reason != astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::WEAK_PROCESS_OWNERSHIP_REASON
    }) || state
        .hooks
        .workspace_root_hint
        .as_deref()
        .and_then(|root| {
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(
                std::path::Path::new(root),
            )
        })
        .unwrap_or(false)
}

pub(super) fn tool_call_verifies_explicit_hook(
    name: &str,
    args: Option<&serde_json::Value>,
    hook: &astra_turn_core::stop_hooks::StopHook,
    workspace_root: Option<&str>,
) -> bool {
    if name != "bash" || hook.command.trim().is_empty() {
        return false;
    }
    let Some(args) = args else {
        return false;
    };
    let command = args.get("command").and_then(serde_json::Value::as_str);
    let workdir = args.get("workdir").and_then(serde_json::Value::as_str);
    let actual_dir = workdir.filter(|dir| *dir != ".").or(workspace_root);
    let expected_dir = hook
        .working_dir
        .as_deref()
        .filter(|dir| !dir.is_empty() && *dir != ".")
        .or(workspace_root);
    command == Some(hook.command.as_str()) && actual_dir == expected_dir
}

pub(crate) fn record_verifies_explicit_hook(
    record: &astra_services::session_journal::ToolCallRecord,
    hook: &astra_turn_core::stop_hooks::StopHook,
    workspace_root: Option<&str>,
) -> bool {
    if record.workspace_mutation_partial == Some(true)
        || record.result_class.as_deref() != Some("success")
        || !astra_turn_core::evaluation::tool_outcome_is_positive_success(record)
    {
        return false;
    }

    let args = super::lifecycle::extract_tool_args(record.authoritative_args_full());
    tool_call_verifies_explicit_hook(&record.name, args.as_ref(), hook, workspace_root)
}

/// Return the declared checks without a successful outcome after the latest
/// executor-reported workspace change. Auto-detected hooks are advisory
/// and deliberately excluded: without an explicit contract Astra cannot know
/// the hidden verifier's scope.
pub(crate) fn missing_explicit_verification_hooks(
    state: &AgenticLoopState,
) -> Result<Option<Vec<String>>, VerificationRecoveryError> {
    state.stall.verification_frontier.evaluate(
        state.hooks.workspace_root_hint.as_deref(),
        &state.hooks.stop_hooks,
        &state.stall.tool_call_records,
    )
}

/// Enforce only caller-declared checks at terminal settlement. Ordinary tool
/// outcomes do not create a project-validation or workspace-proof contract.
fn enforce_explicit_verification_before_text_completion(state: &mut AgenticLoopState) -> bool {
    let missing = match missing_explicit_verification_hooks(state) {
        Ok(Some(missing)) => missing,
        Ok(None) => return false,
        Err(error) => {
            finish_unavailable_verification(state, error);
            return false;
        }
    };
    if missing.is_empty() {
        return false;
    }

    let settlement = &mut state.hooks.completion_settlement;
    if settlement.verification_retries == 0 {
        settlement.verification_retries = 1;
        settlement.text_only = false;
        settlement.work_settlement_only = false;
        settlement.wrapup_origin = None;
        state.budget_wrapup_injected = false;
        state.final_text_model_item_id = None;
        state.final_text.clear();

        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "explicit_verification_required.v1",
                "signal": "verification_contract_unmet",
                "missing_obligations": missing,
                "checks": state.hooks.stop_hooks.iter().filter(|hook| hook.authoritative && missing.contains(&hook.label)).map(|hook| serde_json::json!({
                    "label": hook.label,
                    "command": hook.command,
                    "workdir": hook.working_dir,
                })).collect::<Vec<_>>(),
                "instruction": "Run the missing declared commands with their declared workdir through the normal tool path. Report their actual results. A successful exit is evidence of that invocation, not a guarantee of global workspace freshness.",
                "authority": "caller_declared_verification_contract",
            }),
        );
        return true;
    }

    state.final_text_model_item_id = None;
    state.final_text = format!(
        "Required checks did not complete successfully: checks still missing ({}) after the bounded verification recovery.",
        missing.join(", ")
    );
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(
            state,
            Some(format!(
                "verification contract unmet: {}",
                missing.join(", ")
            )),
        ),
    ));
    false
}

pub(crate) fn finish_unavailable_verification(
    state: &mut AgenticLoopState,
    error: VerificationRecoveryError,
) {
    state.final_text_model_item_id = None;
    state.final_text = "The saved verification evidence could not be confirmed. The work is preserved, but I cannot claim it is complete.".into();
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::RequiresIntervention { description: "Restore the verification history and its original contract before resuming this execution.".into() },
        interruption_state_summary(state, Some(error.to_string())),
    ));
}

pub(crate) fn finish_unavailable_policy(
    state: &mut AgenticLoopState,
    error: crate::turn::runtime_policy::RuntimePolicyContinuationError,
) {
    state.final_text_model_item_id = None;
    state.final_text = "The execution evidence could not be continued consistently. Completed tool results are preserved, but the task is not complete.".into();
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::RequiresIntervention {
            description: "Restore the original execution evidence before resuming.".into(),
        },
        interruption_state_summary(state, Some(error.to_string())),
    ));
}

/// Close a typed completion-action window after its one attempt.  A matching
/// call is not enough by itself: the post-action ledger must show that the
/// original obligation disappeared. A failed declared check cannot reopen
/// ordinary exploration after the hard slice boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionActionBoundary {
    NoWindow,
    /// The provider returned text without spending the still-unconsumed
    /// completion-action allowance.  One bounded provider retry is allowed so
    /// a compliant model can execute the typed action; the window itself is
    /// deliberately kept open and the correction budget is consumed.
    Retry,
    Settled,
    TerminalIncomplete,
}

pub(crate) fn tool_terminal_requires_followup(state: &AgenticLoopState) -> bool {
    workspace_observation_requires_terminal_incomplete(state)
        // A later tool's success template cannot replace the model's chance to
        // interpret that result after an earlier invocation went unconfirmed.
        || state.stall.tool_call_records.iter().any(|record| {
            record.outcome_is_unknown()
                || (record.was_executed()
                && !record.ok
                && matches!(
                    record.error_kind,
                    Some(
                        astra_core::ErrorKind::Cancelled
                            | astra_core::ErrorKind::ToolTimeout
                            | astra_core::ErrorKind::ToolOutcomeUnknown
                    )
                ))
        })
        || missing_explicit_verification_hooks(state)
            .map(|missing| missing.is_some_and(|labels| !labels.is_empty()))
            .unwrap_or(true)
        || state.runtime_tool_executor.as_deref().is_some_and(
            crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
        )
        || !super::lifecycle::unfinished_parallel_agent_ids(state).is_empty()
        || completion_action_window_requires_followup(state)
}

pub(crate) fn enforce_completion_action_window_before_text_completion(
    state: &mut AgenticLoopState,
) -> CompletionActionBoundary {
    let pending_action = match pending_completion_action(state) {
        Ok(action) => action,
        Err(error) => {
            finish_unavailable_verification(state, error);
            return CompletionActionBoundary::TerminalIncomplete;
        }
    };
    let Some(window) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .clone()
    else {
        return CompletionActionBoundary::NoWindow;
    };

    if !window.consumed || !window.matched {
        if !window.consumed && !window.matched && window.mismatch_corrections_remaining > 0 {
            let action = window.action.clone();
            let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
                crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
            );
            if let Some(window) = state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_mut()
            {
                // The first provider boundary was text-only.  Consume only
                // the correction allowance; the typed action attempt remains
                // available and still has to match at admission time.
                window.mismatch_corrections_remaining =
                    window.mismatch_corrections_remaining.saturating_sub(1);
                window.attempts_remaining = 1;
            }
            state.final_text_model_item_id = None;
            state.final_text.clear();
            state.final_text_streamed = false;
            state.hooks.completion_settlement.latest_provider_text = None;
            state.hooks.completion_settlement.deferred_candidate_text = None;
            state.hooks.completion_settlement.text_only = false;
            state.hooks.completion_settlement.work_settlement_only = false;
            state.budget_wrapup_injected = false;

            state.push_volatile_payload(
                super::host::VolatileKind::FinalAnswerSettlement,
                serde_json::json!({
                    "schema": "completion_settlement.v2",
                    "signal": "completion_action_no_tool_retry",
                    "allowed_action": action.clone(),
                    "attempts_remaining": 1,
                    "mismatch_corrections_remaining": 0,
                    "action_hint": completion_action_hint(&action),
                    "execution_authority": "one_matching_action",
                    "instruction": if active_work_attempt {
                        "The previous response did not execute the required typed completion action. Perform exactly one matching action now, then settle the currently owned WorkItem truthfully with settle_work_item. Do not return prose or request an unrelated tool."
                    } else {
                        "The previous response did not execute the required typed completion action. Perform exactly one matching action now, then produce the final answer. Do not return prose or request an unrelated tool."
                    },
                    "authority": "typed_completion_action_window",
                }),
            );
            return CompletionActionBoundary::Retry;
        }
        state.final_text_model_item_id = None;
        state.final_text =
            "The bounded completion action was not executed or did not match the declared obligation."
                .to_string();
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some("typed completion action was not satisfied".to_string()),
            ),
        ));
        return CompletionActionBoundary::TerminalIncomplete;
    }

    if let Some(pending) = pending_action {
        let pending_label = match pending {
            CompletionAction::ExplicitVerification { missing_labels } => {
                format!("explicit verification ({})", missing_labels.join(", "))
            }
        };
        state.final_text_model_item_id = None;
        state.final_text = format!(
            "The bounded completion action ran, but the requested work remains unverified ({pending_label})."
        );
        state.final_text_streamed = false;
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(
                state,
                Some("typed completion action did not settle the obligation".to_string()),
            ),
        ));
        return CompletionActionBoundary::TerminalIncomplete;
    }

    // The typed obligation is settled. Do not let the stale window affect a
    // resumed turn or a later terminal guard.
    state.hooks.completion_settlement.completion_action_window = None;
    CompletionActionBoundary::Settled
}

/// Whether a provider-declared early-success terminal must yield to the
/// completion-action window.  The provider stop signal is authoritative only
/// after every dependent typed obligation has settled; a successful mutation
/// can still require one bounded observation/verification action.
pub(crate) fn completion_action_window_requires_followup(state: &AgenticLoopState) -> bool {
    if workspace_observation_requires_terminal_incomplete(state) {
        // Keep provider-declared success from bypassing the terminal guard.
        // No new reader is authorized in this state; the next text boundary
        // renders a truthful incomplete result instead.
        return true;
    }
    if !matches!(pending_completion_action(state), Ok(None)) {
        return true;
    }
    state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .is_some_and(|window| !window.consumed || !window.matched)
}

/// An explicit check keeps its existing bounded terminal attempt. Canonical
/// Work owns its own settlement; command-shape guesses never open a repair or
/// revalidation state machine.
pub(crate) fn advance_completion_action_window_after_tool_round(state: &mut AgenticLoopState) {
    let active_work_attempt = state.runtime_tool_executor.as_deref().is_some_and(
        crate::server::runtime_tool_executor::RuntimeToolExecutor::has_active_primary_work_attempt,
    );
    advance_completion_action_window_after_tool_round_for_work_state(state, active_work_attempt);
}

fn advance_completion_action_window_after_tool_round_for_work_state(
    state: &mut AgenticLoopState,
    active_work_attempt: bool,
) {
    let pending_action = match pending_completion_action(state) {
        Ok(action) => action,
        Err(error) => {
            finish_unavailable_verification(state, error);
            return;
        }
    };
    let Some(window) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
    else {
        return;
    };
    if !window.consumed {
        return;
    }
    if active_work_attempt
        && window.matched
        && pending_action.is_none()
        && !workspace_observation_is_quarantined(state)
    {
        state.hooks.completion_settlement.completion_action_window = None;
        state.hooks.completion_settlement.work_settlement_only = true;
        state.hooks.completion_settlement.text_only = false;
        state.budget_wrapup_injected = false;
        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "work_settlement_required.v1",
                "signal": "completion_action_attempted_settle_work",
                "instruction": "The declared checks finished. Settle the currently owned WorkItem with its observed outcome using settle_work_item.",
                "authority": "canonical_work_lifecycle",
            }),
        );
        return;
    }
    state.hooks.completion_settlement.text_only = true;
    project_completion_action_text_settlement(state);
}

/// Project current settlement authority without changing its budget or outcome.
pub(crate) fn project_completion_action_text_settlement(state: &mut AgenticLoopState) {
    // FinalAnswerSettlement is a singleton. A closer that already explained
    // why tools are closed — for example an unverified external write — must
    // keep that instruction when the text-only round is reserved.
    let already_closed = state.volatile_pending.iter().any(|entry| {
        entry.kind == super::host::VolatileKind::FinalAnswerSettlement
            && entry.payload["mode"] == "text_only"
            && entry.payload["execution_authority"] == "none"
    });
    if already_closed {
        return;
    }
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "completion_settlement.v2",
            "signal": "typed_completion_action_settled",
            "mode": "text_only",
            "allowed_action": serde_json::Value::Null,
            "attempts_remaining": 0,
            "declarations_may_remain_visible_for_cache": true,
            "execution_authority": "none",
            "instruction": "The bounded completion action has been attempted. Produce the final answer from the resulting evidence; do not request another tool.",
            "authority": "typed_completion_action_window",
        }),
    );
}

/// Physical execution custody is independent of task-quality or Work claims.
pub(crate) fn reject_unsettled_execution_completion(state: &mut AgenticLoopState) -> bool {
    let partial = state.stall.workspace_observation_quarantine.as_ref().is_some_and(|q| {
        q.reason == astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::PARTIAL_MUTATION_REASON
    });
    let reason = if partial {
        Some((
            "workspace update was only partially applied",
            "Some workspace changes were applied before the tool failed. Review the affected files before continuing.",
        ))
    } else if workspace_observation_requires_terminal_incomplete(state) {
        Some((
            "workspace execution ownership is unsettled",
            "Workspace execution could not be confirmed stopped. The available result is preserved as unverified.",
        ))
    } else if workspace_observation_is_quarantined(state)
        && missing_explicit_verification_hooks(state)
            .map(|missing| missing.is_some_and(|labels| !labels.is_empty()))
            .unwrap_or(true)
    {
        Some((
            "execution quarantine prevents the required verification",
            "The available result is preserved, but required verification cannot safely run until execution ownership is reconciled.",
        ))
    } else {
        None
    };
    if let Some((detail, fallback)) = reason {
        mark_workspace_completion_incomplete(state, detail, fallback);
        let source = state
            .stall
            .workspace_observation_quarantine
            .as_ref()
            .and_then(|quarantine| quarantine.source_tool_call_id.as_deref())
            .and_then(|id| {
                state.stall.tool_call_records.iter().rev().find(|record| {
                    record.tool_call_id.as_deref() == Some(id)
                        && record.was_executed()
                        && !record.ok
                })
            });
        let cause = source
            .map(|record| {
                let name: String = record
                    .name
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
                    .take(80)
                    .collect();
                let tool = if name.is_empty() {
                    "A tool".to_string()
                } else {
                    format!("The {name} tool")
                };
                if partial {
                    return format!("{tool} reported a partial update. ");
                }
                match record.error_kind {
                    Some(astra_core::ErrorKind::ToolTimeout) => format!("{tool} timed out. "),
                    Some(astra_core::ErrorKind::Cancelled) => format!("{tool} was cancelled. "),
                    _ => format!("{tool} did not return a confirmed result. "),
                }
            })
            .unwrap_or_default();
        if let Some(interruption) = state.interruption.as_mut() {
            let checkpoint = if interruption.has_checkpoint {
                "Progress was saved."
            } else {
                "No checkpoint was saved."
            };
            interruption.user_message = if partial {
                format!(
                    "{cause}Some changes were applied before the tool failed. {checkpoint} Review the affected files and available output before continuing."
                )
            } else {
                format!(
                    "{cause}Execution could not be confirmed stopped and may have changed state. {checkpoint} Review available output and inspect the current state before retrying."
                )
            };
        }
        true
    } else {
        false
    }
}

/// Convert an exhausted workspace-delivery recovery window into a typed
/// incomplete terminal. Returning `false` from a completion guard means
/// "do not reopen another provider turn"; it must not mean "the model's last
/// prose is a successful completion". Keeping this transition typed also
/// lets the server/CLI preserve the partial result without accepting an
/// artifact-less reward or a false verified claim.
fn mark_workspace_completion_incomplete(
    state: &mut AgenticLoopState,
    detail: &str,
    fallback_text: &str,
) {
    if state.final_text.trim().is_empty() {
        let settlement = &state.hooks.completion_settlement;
        if let Some(text) = settlement
            .latest_provider_text
            .as_ref()
            .filter(|text| !text.trim().is_empty())
        {
            state.final_text = text.clone();
            state.final_text_model_item_id = settlement.latest_provider_text_model_item_id.clone();
        } else {
            state.final_text_model_item_id = None;
            state.final_text = fallback_text.to_string();
        }
    }
    state.final_text_streamed = false;
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::ExecutionIncomplete,
        ResumeAction::ContinueImmediately,
        interruption_state_summary(state, Some(detail.to_string())),
    ));
}

/// A launch receipt, nonterminal child observation, or queued answer is not a failed child.
/// It stops blocking completion only after the same child has a producer-owned
/// successful terminal result that the parent model actually observed.
///
/// `agent.send_message(message_type="answer")` is intentionally included
/// here.  The answer is a delivery acknowledgement, not the child's terminal
/// result; once the child completion has been staged and adopted, retaining the
/// queued acknowledgement as an unresolved `agent_incomplete` outcome makes a
/// healthy parent turn impossible to finalize.
pub(crate) fn observed_direct_child_for_nonterminal_receipt<'a>(
    state: &'a AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<&'a serde_json::Value> {
    use astra_turn_core::orchestration::agent_result_wire::{
        AgentToolResultStatusKind, DecodedAgentToolResult, decode_agent_tool_result,
    };
    if record.name != "agent"
        || !record.ok
        || record.disposition
            != Some(astra_services::session_journal::ToolCallDisposition::Executed)
    {
        return None;
    }
    let args = record
        .authoritative_args_full()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
    let result = record
        .runtime_model_result_full
        .as_deref()
        .or(record.result_full.as_deref())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
    let action = args["action"].as_str();
    let agent_id = match action {
        Some("spawn") | Some("get_result") => {
            let agent_id = result["agent_id"].as_str()?;
            if action == Some("get_result") && args["agent_id"] != agent_id {
                return None;
            }
            let pending_receipt = match action {
                Some("spawn") => result["status"] == "launched",
                Some("get_result") => matches!(
                    decode_agent_tool_result(&result),
                    Some(DecodedAgentToolResult::ChildResult(
                        AgentToolResultStatusKind::StillRunning
                            | AgentToolResultStatusKind::Waiting
                            | AgentToolResultStatusKind::Paused
                    ))
                ),
                _ => false,
            };
            if !pending_receipt {
                return None;
            }
            agent_id
        }
        Some("send_message") if args["message_type"].as_str() == Some("answer") => {
            // The parent addresses the exact child runtime identity when it
            // answers a question.  Do not accept a display name or a generic
            // target here: the terminal result must be correlated to the
            // same producer-owned child.
            let agent_id = args["to"].as_str().map(str::trim)?;
            if agent_id.is_empty() || result["status"].as_str() != Some("queued") {
                return None;
            }
            agent_id
        }
        _ => return None,
    };
    state
        .volatile_pending
        .iter()
        .filter(|entry| {
            entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                && entry.payload["parent_run_id"].as_str() == state.current_run_id.as_deref()
                && entry.payload["observed_by_provider"] == true
        })
        .find_map(|entry| {
            let delivery_count = entry.payload["delivery_count"].as_u64().unwrap_or_default();
            entry.payload["children"].as_array().and_then(|children| {
                children.iter().find(|child| {
                    if child["agent_id"] != agent_id || child["status"] != "completed" {
                        return false;
                    }
                    if child["result"]
                        .as_str()
                        .is_some_and(|result| !result.trim().is_empty())
                    {
                        return true;
                    }
                    // `AgenticLoopState::commit_volatile_attempt_lease` is
                    // the sole producer of this compacted shape: after two
                    // observed leases it removes the presentation result but
                    // retains the positive byte count and delivery count.
                    delivery_count >= 2
                        && child.get("result").is_none()
                        && child["result_bytes"]
                            .as_u64()
                            .is_some_and(|bytes| bytes > 0)
                })
            })
        })
}

pub(crate) fn nonterminal_child_receipt_superseded(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if record.name == "agent_fanout" && fanout_nonterminal_receipt_superseded(state, record) {
        return true;
    }
    observed_direct_child_for_nonterminal_receipt(state, record).is_some()
}

/// Settle launch receipts and nonterminal observations only from complete delivery
/// of the canonical group: either observed child completions or a successful
/// full-group read. Historical observations remain unchanged in the journal.
pub(crate) fn fanout_nonterminal_receipt_superseded(
    state: &AgenticLoopState,
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    use astra_turn_core::orchestration::agent_result_wire::{
        AgentToolResultStatusKind, DecodedAgentToolResult, agent_tool_structured_result_class,
        decode_agent_tool_result,
    };
    if !record.ok
        || record.disposition
            != Some(astra_services::session_journal::ToolCallDisposition::Executed)
    {
        return false;
    }
    let Some(args) = record
        .authoritative_args_full()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return false;
    };
    let Some(mut start_result) = record
        .runtime_model_result_full
        .as_deref()
        .or(record.result_full.as_deref())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return false;
    };
    match args["action"].as_str() {
        Some("start") => {}
        Some("get_results") => {
            let query = &start_result;
            let Some(group_id) = args["group_id"].as_str().filter(|id| !id.is_empty()) else {
                return false;
            };
            // Pending counts include still-running slots. Only terminal failures
            // disqualify a running observation from later successful settlement.
            if query["group_id"] != group_id
                || state.current_run_id.is_none()
                || query["fanout"]["parent_run_id"].as_str() != state.current_run_id.as_deref()
                || [
                    "failed",
                    "interrupted",
                    "timed_out",
                    "cancelled_by_user",
                    "cancelled_by_runtime",
                    "spawn_rejected",
                ]
                .iter()
                .any(|field| query[*field].as_u64().unwrap_or(0) > 0)
            {
                return false;
            }
            let Some(results) = query["results"].as_array().filter(|rows| !rows.is_empty()) else {
                return false;
            };
            // Child reads precede the refreshed group summary. A child can
            // finish between them, leaving a genuine running observation in
            // a now-terminal aggregate. The later complete-delivery proof
            // settles that observation; it does not rewrite the receipt.
            let mut pending_observation = false;
            for item in results {
                match decode_agent_tool_result(&item["result"]) {
                    Some(DecodedAgentToolResult::ChildResult(
                        AgentToolResultStatusKind::StillRunning
                        | AgentToolResultStatusKind::Waiting
                        | AgentToolResultStatusKind::Paused,
                    )) => pending_observation = true,
                    _ if agent_tool_structured_result_class(&item["result"]) == Some("success") => {
                    }
                    _ => return false,
                }
            }
            if !matches!(
                (query["status"].as_str(), query["active"].as_u64()),
                (Some("incomplete"), Some(1..))
            ) && !(query["status"] == "completed_with_issues"
                && query["active"].as_u64() == Some(0)
                && pending_observation)
            {
                return false;
            }
            let Some(start) = state.stall.tool_call_records.iter().find_map(|candidate| {
                if candidate.name != "agent_fanout"
                    || !candidate.ok
                    || candidate.disposition
                        != Some(astra_services::session_journal::ToolCallDisposition::Executed)
                {
                    return None;
                }
                let args: serde_json::Value =
                    serde_json::from_str(candidate.authoritative_args_full()?).ok()?;
                if args["action"] != "start" {
                    return None;
                }
                let start: serde_json::Value = serde_json::from_str(
                    candidate
                        .runtime_model_result_full
                        .as_deref()
                        .or(candidate.result_full.as_deref())?,
                )
                .ok()?;
                (start["group_id"] == group_id
                    && start["fanout"]["parent_run_id"].as_str() == state.current_run_id.as_deref())
                .then_some(start)
            }) else {
                return false;
            };
            let Some(agents) = start["agents"].as_array() else {
                return false;
            };
            if query["target_count"] != start["target_count"]
                || results.iter().any(|item| {
                    !agents.iter().any(|agent| {
                        item["agent_id"].as_str().is_some_and(|id| !id.is_empty())
                            && item["agent_id"] == agent["agent_id"]
                            && item["slot_index"].is_u64()
                            && item["slot_index"] == agent["slot_index"]
                            && (item["run_id"].is_null() || item["run_id"] == agent["run_id"])
                            && item["result"]["agent_id"] == agent["agent_id"]
                    })
                })
            {
                return false;
            }
            start_result = start;
        }
        _ => return false,
    }
    if start_result["status"].as_str() != Some("started") {
        return false;
    }
    let Some(group_id) = start_result["group_id"].as_str() else {
        return false;
    };
    let Some(agents) = start_result["agents"].as_array().filter(|agents| {
        !agents.is_empty() && start_result["target_count"].as_u64() == Some(agents.len() as u64)
    }) else {
        return false;
    };
    let identities: HashSet<_> = agents
        .iter()
        .filter_map(|agent| Some((agent["agent_id"].as_str()?, agent["run_id"].as_str()?)))
        .collect();
    if identities.len() != agents.len()
        || identities
            .iter()
            .any(|(agent, run)| agent.is_empty() || run.is_empty())
    {
        return false;
    }
    let observed: HashSet<_> = state
        .volatile_pending
        .iter()
        .filter(|entry| {
            entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                && entry.payload["parent_run_id"].as_str() == state.current_run_id.as_deref()
                && entry.payload["observed_by_provider"] == true
        })
        .filter_map(|entry| entry.payload["children"].as_array())
        .flatten()
        .filter(|child| {
            child["status"] == "completed"
                && child["result_bytes"]
                    .as_u64()
                    .is_some_and(|bytes| bytes > 0)
        })
        .filter_map(|child| Some((child["agent_id"].as_str()?, child["run_id"].as_str()?)))
        .collect();
    if identities.is_subset(&observed) {
        return true;
    }

    state.stall.tool_call_records.iter().any(|later| {
        if later.name != "agent_fanout"
            || !later.ok
            || later.disposition
                != Some(astra_services::session_journal::ToolCallDisposition::Executed)
        {
            return false;
        }
        let Some(later_args) = later
            .authoritative_args_full()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        else {
            return false;
        };
        if later_args["action"].as_str() != Some("get_results")
            || later_args["group_id"].as_str() != Some(group_id)
        {
            return false;
        }
        let Some(result) = later
            .runtime_model_result_full
            .as_deref()
            .or(later.result_full.as_deref())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        else {
            return false;
        };
        result["group_id"].as_str() == Some(group_id)
            && astra_turn_core::orchestration::agent_result_wire::agent_fanout_results_delivered(
                &result,
            )
            && result["results"].as_array().is_some_and(|results| {
                let delivered: HashSet<_> = results
                    .iter()
                    .filter_map(|item| Some((item["agent_id"].as_str()?, item["run_id"].as_str()?)))
                    .collect();
                identities == delivered
            })
    })
}

/// A completed provider turn delivered no text and selected no tool. That is
/// not a deliverable answer even when the transport reported success.
fn provider_turn_without_actionable_output(
    state: &AgenticLoopState,
    turn_result: &HostTurnResult,
) -> bool {
    turn_result.accum.tool_calls.is_empty()
        && turn_result.accum.full_text.trim().is_empty()
        && state.final_text.trim().is_empty()
        && state
            .hooks
            .completion_settlement
            .deferred_candidate_text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
}

fn should_retry_textless_response(state: &AgenticLoopState, turn_result: &HostTurnResult) -> bool {
    provider_turn_without_actionable_output(state, turn_result)
        && state.interruption.is_none()
        && state.remaining_turns > 0
        && state.hooks.completion_settlement.textless_response_retries
            < MAX_TEXTLESS_RESPONSE_RETRIES
}

/// A repeated textless completion is the exhausted side of the same bound the
/// recovery request consumed. Record the typed interruption here so the
/// provider cause and its intervention guidance reach run finalization
/// instead of a generic settlement notice invented later.
fn record_exhausted_textless_completion(state: &mut AgenticLoopState) -> bool {
    if state.interruption.is_some()
        || state.hooks.completion_settlement.textless_response_retries
            < MAX_TEXTLESS_RESPONSE_RETRIES
    {
        return false;
    }
    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::EmptyCompletion,
        exhausted_provider_recovery_action(state),
        interruption_state_summary(
            state,
            Some(
                "provider ended without user-visible text after the bounded textless recovery"
                    .into(),
            ),
        ),
    ));
    true
}

fn begin_textless_response_retry(state: &mut AgenticLoopState) {
    let settlement = &mut state.hooks.completion_settlement;
    settlement.textless_response_retries += 1;
    settlement.text_only = true;
    let rounds_completed = state.max_turns.saturating_sub(state.remaining_turns);
    state.push_volatile_payload(
        super::host::VolatileKind::FinalAnswerSettlement,
        serde_json::json!({
            "schema": "final_answer_settlement.v1",
            "signal": "textless_provider_response",
            "evidence": {
                "tool_calls_completed": state.total_tool_calls,
                "rounds_completed": rounds_completed,
                "remaining_turns": state.remaining_turns,
            },
            "instruction": "The previous model response ended without user-visible text. Produce a concise, direct final answer from the preserved evidence now. This recovery boundary is text-only; do not request tools, create tasks, or delegate work.",
            "authority": "runtime_bounded_text_only_retry",
        }),
    );
}

/// Lazily-initialized process-wide alert dispatcher.
///
/// Reads `ASTRA_ALERT_WEBHOOK_URL` (and optional `ASTRA_ALERT_WEBHOOK_MIN_SEVERITY`)
/// once, builds a single `AlertDispatcher` with a reusable reqwest client so that
/// webhook calls share a TCP connection pool and TLS session cache across turns.
///
/// Returns `None` when no webhook URL is configured — the whole alert-dispatch
/// branch is then a single cheap `OnceLock` load.
fn global_alert_dispatcher()
-> Option<&'static std::sync::Arc<astra_turn_core::alert_dispatcher::AlertDispatcher>> {
    use std::sync::OnceLock;
    static DISPATCHER: OnceLock<
        Option<std::sync::Arc<astra_turn_core::alert_dispatcher::AlertDispatcher>>,
    > = OnceLock::new();
    DISPATCHER
        .get_or_init(|| {
            let url = std::env::var("ASTRA_ALERT_WEBHOOK_URL").ok()?;
            let url = url.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let min_severity = std::env::var("ASTRA_ALERT_WEBHOOK_MIN_SEVERITY")
                .ok()
                .and_then(|s| match s.to_ascii_lowercase().as_str() {
                    "info" => Some(astra_turn_core::trace_alert::AlertSeverity::Info),
                    "warning" | "warn" => {
                        Some(astra_turn_core::trace_alert::AlertSeverity::Warning)
                    }
                    "error" => Some(astra_turn_core::trace_alert::AlertSeverity::Error),
                    _ => None,
                })
                .unwrap_or(astra_turn_core::trace_alert::AlertSeverity::Error);
            let client =
                std::sync::Arc::new(astra_turn_core::alert_dispatcher::ReqwestWebhookClient::new());
            let cfg = astra_turn_core::alert_dispatcher::AlertWebhookConfig { url, min_severity };
            Some(std::sync::Arc::new(
                astra_turn_core::alert_dispatcher::AlertDispatcher::new(cfg, client),
            ))
        })
        .as_ref()
}

fn alert_dispatch_session_id(session_id: Option<&str>) -> Option<String> {
    session_id
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .map(ToString::to_string)
}

fn is_llm_provider_admission_error(error: &astra_core::ClassifiedError) -> bool {
    let Some(details_json) = error.details_json.as_deref() else {
        return false;
    };
    let Ok(serde_json::Value::Object(details)) =
        serde_json::from_str::<serde_json::Value>(details_json)
    else {
        return false;
    };
    details.get("source").and_then(serde_json::Value::as_str) == Some("llm_provider_admission")
}

fn exhausted_provider_recovery_action(state: &AgenticLoopState) -> ResumeAction {
    ResumeAction::RequiresIntervention {
        description: format!(
            "{} could not produce a usable response after one recovery. The request remains unfinished. Choose another authorized model, or explicitly retry after the provider recovers; prior conversation and tool evidence remain available.",
            state
                .current_model_identity()
                .unwrap_or("The selected model"),
        ),
    }
}

/// Keep only observed assistant bytes, never reasoning or unexecuted tool calls.
/// The same history owner feeds Root Context and child transcript capture.
fn preserve_failed_provider_text(
    state: &mut AgenticLoopState,
    text: &str,
    model_item_id: Option<String>,
) {
    if text.trim().is_empty() {
        return;
    }
    // Failed streams are newer provider observations too. Keep one latest
    // text/identity owner so terminal materialization cannot revive an older answer.
    state.hooks.completion_settlement.latest_provider_text = Some(text.to_owned());
    state
        .hooks
        .completion_settlement
        .latest_provider_text_model_item_id = model_item_id.clone();
    state.final_text = text.to_owned();
    state.final_text_model_item_id = model_item_id;
    state.final_text_streamed = false;
    if let Some(message) =
        astra_turn_core::agentic_turn_ingest::terminal_assistant_message_to_append(
            &state.messages,
            &state.final_text,
            state.final_text_model_item_id.as_deref(),
        )
    {
        state.push_prompt_history_message(message);
    }
}

fn record_direct_llm_error_state(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) {
    match error.kind {
        astra_core::ErrorKind::RateLimit if !is_llm_provider_admission_error(error) => {
            state.rate_limit_cooldown.record_429(None);
        }
        astra_core::ErrorKind::ServerError => {
            state.rate_limit_cooldown.record_529(None);
        }
        _ => {}
    }

    if state.interruption.is_some() {
        return;
    }
    if let Some((kind, action)) =
        astra_turn_core::interruption::interruption_from_error_kind(error.kind)
    {
        let exhausted_safe_recovery = state.provider_adaptation.safe_recovery_attempted
            && matches!(
                error.kind,
                astra_core::ErrorKind::ProviderDeadline
                    | astra_core::ErrorKind::StreamIdle
                    | astra_core::ErrorKind::StreamTransport
            );
        let action = if exhausted_safe_recovery {
            exhausted_provider_recovery_action(state)
        } else {
            action
        };
        state.interruption = Some(InterruptionRecord::new(
            kind,
            action,
            interruption_state_summary(state, Some(error.message.clone())),
        ));
    }
}

/// A clean provider terminal can still be a runtime non-delivery (for
/// example, reasoning-only output).  The provider request and its usage were
/// real even though the loop will recover rather than settle the response.
/// Fold that evidence exactly once on the error lane, which otherwise has no
/// `HostTurnResult` from which normal ingest can account it.
fn fold_provider_completion_error_usage(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) {
    let Some(details) = error
        .details_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return;
    };
    // Accounting qualification is independent of successful delivery. An
    // interrupted dispatched request may carry only an unknown sample.
    let qualified = details
        .get("qualified_usage")
        .filter(|usage| !usage.is_null())
        .map(|usage| {
            astra_turn_types::CanonicalTokenUsage::from_json(usage).unwrap_or_else(|_| {
                astra_turn_types::CanonicalTokenUsage::new(None, None, None, None)
                    .expect("unknown usage is valid")
            })
        });
    if let Some(usage) = qualified {
        state.last_request_usage = None;
        state.last_measured_prompt_tokens = None;
        state.add_qualified_usage(Some(usage));
    }
    let transport_success = details
        .pointer("/provider_response/transport_success")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if transport_success {
        state.last_measured_prompt_tokens = details
            .get("last_request_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                details
                    .get("last_request_usage")
                    .and_then(|usage| astra_turn_types::CanonicalTokenUsage::from_json(usage).ok())
                    .and_then(|usage| usage.input_column())
                    .and_then(|tokens| u64::try_from(tokens).ok())
            });
        state.last_request_usage = details
            .get("last_request_usage")
            .and_then(|usage| astra_turn_types::CanonicalTokenUsage::from_json(usage).ok())
            .and_then(|usage| {
                astra_turn_types::RequestTokenUsage::try_new(
                    usage.input_tokens()?,
                    usage.cached_input_tokens()?,
                    usage.cache_creation_tokens()?,
                    usage.output_tokens()?,
                )
                .ok()
            });
    }
    // One host-return boundary, one increment. Failed transports still carry
    // actual measured buckets; an unknown bucket never becomes known zero.
    let usage = if transport_success {
        details
            .get("usage")
            .and_then(|usage| astra_turn_types::CanonicalTokenUsage::from_json(usage).ok())
    } else {
        qualified
    };
    let Some(usage) = usage else {
        return;
    };
    state.total_prompt = state
        .total_prompt
        .saturating_add(usage.input_tokens().unwrap_or(0));
    state.total_cache_read = state
        .total_cache_read
        .saturating_add(usage.cached_input_tokens().unwrap_or(0));
    state.total_cache_creation = state
        .total_cache_creation
        .saturating_add(usage.cache_creation_tokens().unwrap_or(0));
    state.total_completion = state
        .total_completion
        .saturating_add(usage.output_tokens().unwrap_or(0));
    let any_known = usage.input_tokens().is_some()
        || usage.cached_input_tokens().is_some()
        || usage.cache_creation_tokens().is_some()
        || usage.output_tokens().is_some();
    state.has_any_usage |= any_known;
    state.step_recorder.record_tokens(
        usage.input_tokens().unwrap_or(0),
        usage.output_tokens().unwrap_or(0),
    );
    if !any_known {
        return;
    }
    // The physical provider round was already counted as a local error before
    // this typed payload reached the loop. Reclassify that same attempt; do
    // not create a second coverage record.
    state.telemetry.local_usage_unavailable =
        state.telemetry.local_usage_unavailable.saturating_sub(1);
    state.telemetry.local_usage_provider_reported = state
        .telemetry
        .local_usage_provider_reported
        .saturating_add(1);
}

/// True only when the provider transport itself completed successfully but
/// the response contained neither visible text nor a selected tool. This is
/// distinct from a provider wall/semantic deadline even though both currently
/// travel through the `ProviderDeadline` error lane.
fn is_transport_success_empty_completion(details: &serde_json::Value) -> bool {
    let scope = details
        .pointer("/deadline/scope")
        .and_then(serde_json::Value::as_str);
    let actionable_output_boundary =
        matches!(scope, Some("provider_completion" | "provider_convergence"))
            && details
                .pointer("/deadline/phase")
                .and_then(serde_json::Value::as_str)
                == Some("actionable_output");
    let transport_succeeded = details
        .pointer("/provider_response/transport_success")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let explicitly_non_actionable_text = details
        .get("partial_full_text")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|text| !crate::turn::llm::client::text_has_actionable_content(text));
    let explicitly_empty_tools = details
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty);

    actionable_output_boundary
        && transport_succeeded
        && explicitly_non_actionable_text
        && explicitly_empty_tools
}

fn record_repeated_transport_success_empty_completion(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) -> bool {
    if state.interruption.is_some()
        || !state.provider_adaptation.safe_recovery_attempted
        || error.kind != astra_core::ErrorKind::ProviderDeadline
    {
        return false;
    }
    let Some(details) = error
        .details_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return false;
    };
    if !is_transport_success_empty_completion(&details) {
        return false;
    }

    state.interruption = Some(InterruptionRecord::new(
        InterruptionKind::EmptyCompletion,
        exhausted_provider_recovery_action(state),
        interruption_state_summary(state, Some(error.message.clone())),
    ));
    true
}

/// Admit one logical recovery round after a provider attempt failed before it
/// produced an executable or user-visible action. This is deliberately
/// outside the physical transport retry loop: the prior attempt is settled
/// first, and the next request gets its own durable inference identity.
///
/// A partial provider delivery is never replayed blindly. Visible text can be
/// an externally observable deliverable, and a selected tool can be a pending
/// side effect, so either one makes a new provider attempt unsafe. The only
/// recoverable shape is provisional reasoning with no text and no tool call.
fn schedule_safe_provider_recovery(
    state: &mut AgenticLoopState,
    error: &astra_core::ClassifiedError,
) -> bool {
    if !matches!(
        error.kind,
        astra_core::ErrorKind::ProviderDeadline
            | astra_core::ErrorKind::StreamTransport
            | astra_core::ErrorKind::StreamIdle
    ) || state.provider_adaptation.safe_recovery_attempted
    {
        return false;
    }
    let Some(details) = error
        .details_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return false;
    };
    let no_visible_text = details
        .get("partial_full_text")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|text| !crate::turn::llm::client::text_has_actionable_content(text));
    let no_selected_tool = details
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_none_or(Vec::is_empty);
    let observed_provisional_reasoning = details
        .get("partial_reasoning")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|reasoning| !reasoning.is_empty());
    let explicit_transport_partial = details
        .get("partial_full_text")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && details
            .get("partial_reasoning")
            .and_then(serde_json::Value::as_str)
            .is_some()
        && details
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .is_some();
    let semantic_progress_deadline = details
        .pointer("/deadline/phase")
        .and_then(serde_json::Value::as_str)
        == Some("semantic_progress");
    let provider_completed_without_delivery = details
        .pointer("/deadline/scope")
        .and_then(serde_json::Value::as_str)
        == Some("provider_completion")
        && details
            .pointer("/deadline/phase")
            .and_then(serde_json::Value::as_str)
            == Some("actionable_output");
    let transport_success_empty_completion = error.kind == astra_core::ErrorKind::ProviderDeadline
        && is_transport_success_empty_completion(&details);
    let terminal_text_only_recovery = state.remaining_turns == 0
        && state.hooks.completion_settlement.text_only
        && transport_success_empty_completion;
    if state.remaining_turns == 0 && !terminal_text_only_recovery {
        return false;
    }
    // A semantic-progress deadline is a known safe convergence boundary even
    // when the provider did not expose reasoning. Transport/idle failures need
    // affirmative partial-attempt evidence: otherwise an error from before
    // delivery must remain fail-closed rather than becoming a retry loop.
    let transport_or_idle = matches!(
        error.kind,
        astra_core::ErrorKind::StreamTransport | astra_core::ErrorKind::StreamIdle
    );
    let recovery_evidence = observed_provisional_reasoning
        || (error.kind == astra_core::ErrorKind::ProviderDeadline
            && (semantic_progress_deadline
                || provider_completed_without_delivery
                || transport_success_empty_completion));
    if !no_visible_text
        || !no_selected_tool
        || !recovery_evidence
        || (transport_or_idle && !explicit_transport_partial)
    {
        return false;
    }

    if terminal_text_only_recovery && state.remaining_turns == 0 {
        return false;
    }

    state.provider_adaptation.safe_recovery_attempted = true;
    state.provider_adaptation.safe_recovery_pending = true;
    let text_only = state.hooks.completion_settlement.text_only;
    let interruption = match error.kind {
        astra_core::ErrorKind::ProviderDeadline => "spent its action window",
        astra_core::ErrorKind::StreamTransport => "lost its provider connection",
        astra_core::ErrorKind::StreamIdle => "stalled before delivery completed",
        _ => unreachable!("safe provider recovery checked the error kind"),
    };
    let instruction = if text_only {
        format!(
            "The previous inference {interruption} without producing a deliverable or selecting a tool. Do not restart or narrate the analysis, and do not call tools. Use the retained conversation and evidence to return the concise final answer now."
        )
    } else {
        format!(
            "The previous inference {interruption} without producing a deliverable or selecting a tool. Do not restart or narrate the analysis. Use the retained conversation and evidence to make the next concrete tool call now, or return a concise final answer if no execution is needed."
        )
    };
    state.push_volatile_payload(
        super::host::VolatileKind::BehaviorAdvisory,
        serde_json::json!({
            "schema": "provider_safe_recovery.v1",
            "signal": "provider_attempt_ended_without_executable_or_visible_delivery",
            "evidence": {
                "error_kind": error.kind.as_str(),
                "previous_attempt_produced_visible_text": false,
                "previous_attempt_selected_tool": false,
                "previous_attempt_produced_provisional_reasoning": observed_provisional_reasoning,
            },
            "instruction": instruction,
            "execution_mode": if text_only { "text_only" } else { "tool_or_final" },
            "attempts_remaining": 1,
            "authority": "advisory_evidence_only"
        }),
    );
    true
}

/// A runtime-owned provider-work deadline may end an ordinary request just
/// before the reserved final-answer window. It is safe to enter that window
/// only when the failed attempt delivered no visible text or selected tool;
/// otherwise the existing delivery-unknown path must remain authoritative.
fn provider_deadline_has_no_actionable_delivery(error: &astra_core::ClassifiedError) -> bool {
    if error.kind != astra_core::ErrorKind::ProviderDeadline {
        return false;
    }
    let Some(raw) = error.details_json.as_deref() else {
        // The send/response deadline can fire before any provider response is
        // received; that shape has no partial response to replay or discard.
        return true;
    };
    let Ok(serde_json::Value::Object(details)) = serde_json::from_str(raw) else {
        return false;
    };
    if details.get("partial_full_text").is_some_and(|value| {
        !value
            .as_str()
            .is_some_and(|text| !crate::turn::llm::client::text_has_actionable_content(text))
    }) || details
        .get("tool_calls")
        .is_some_and(|value| !value.as_array().is_some_and(Vec::is_empty))
    {
        return false;
    }
    true
}

/// A completed local tool execution changes the evidence available to the
/// next inference.  Recovery remains bounded to one attempt for each such
/// progress epoch, but must not remain run-global: otherwise an agent that
/// recovered, executed useful work, and later independently stalls loses the
/// only safe way to converge.
pub(crate) fn executed_tool_advances_provider_recovery_epoch(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> bool {
    records.iter().any(|record| record.was_executed())
}

/// Apply the progress-epoch transition from records appended by exactly one
/// tool phase.  A floor makes the transition insensitive to old receipts and
/// keeps selections that never executed from replenishing recovery.
pub(crate) fn advance_provider_recovery_epoch_from_new_records(
    state: &mut AgenticLoopState,
    record_floor: usize,
) -> bool {
    let advanced = executed_tool_advances_provider_recovery_epoch(
        state
            .stall
            .tool_call_records
            .get(record_floor..)
            .unwrap_or_default(),
    );
    if advanced {
        state.provider_adaptation.safe_recovery_attempted = false;
    }
    advanced
}

#[cfg(test)]
pub(crate) async fn inject_polled_user_intents<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    inject_polled_user_intents_inner(host, state, false)
        .await
        .map(|_| ())
}

/// Poll the durable intent lane at a model-completion boundary even when the
/// normal empty-poll cadence has not elapsed. This is the ownership handoff
/// that prevents a remotely accepted intent from falling between the final
/// model response and terminal run settlement.
pub(crate) async fn inject_polled_user_intents_before_settlement<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    Ok(
        runtime_input_boundary(host, state, RuntimeInputBoundary::Settlement)
            .await?
            .model_context_changed,
    )
}

/// Reconcile guidance after provider inference and before admitting any tool
/// action produced from the now-stale request snapshot. This is a control
/// epoch fence: a newly applied intent must be visible to the model before a
/// response generated without it can cause another side effect.
pub(crate) async fn inject_polled_user_intents_before_action<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    Ok(
        runtime_input_boundary(host, state, RuntimeInputBoundary::Action)
            .await?
            .model_context_changed,
    )
}

/// Reconcile the durable control lane immediately before beginning another
/// provider round. A tool may finish inside the normal empty-poll debounce
/// window; carrying that debounce through request preparation would allow an
/// already-accepted intent to miss the first safe post-tool boundary and
/// spend an entire provider request on stale work.
async fn inject_polled_user_intents_before_provider<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<bool, astra_core::ClassifiedError> {
    Ok(
        runtime_input_boundary(host, state, RuntimeInputBoundary::Provider)
            .await?
            .model_context_changed,
    )
}

/// Every agent execution uses this boundary regardless of root/child role.
/// The boundary selects only the sources that can have changed; each source
/// retains its own authority, acknowledgement and checkpoint owner.
#[derive(Clone, Copy)]
pub(crate) enum RuntimeInputBoundary {
    Preparation,
    Provider,
    Action,
    Settlement,
    MailboxReady,
    IntentReady,
    ChildReady,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct RuntimeInputOutcome {
    pub(crate) model_context_changed: bool,
    pub(crate) durable_guidance_applied: bool,
}

pub(crate) async fn runtime_input_boundary<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    boundary: RuntimeInputBoundary,
) -> Result<RuntimeInputOutcome, astra_core::ClassifiedError> {
    // Run admission and the existing timed watcher own durable control reads.
    // No changed watermark means no new guidance to fetch at another model or
    // tool boundary. The terminal transaction returns late, unapplied inputs;
    // pending local deliveries/acknowledgements retain their existing owner.
    let guidance_ready = matches!(boundary, RuntimeInputBoundary::IntentReady)
        || state.user_intents.input_wake_is_pending().unwrap_or(true)
        || state
            .user_intents
            .should_retry_apply_ack(tokio::time::Instant::now())
        || state
            .run_control
            .as_ref()
            .is_some_and(|control| control.has_pending_inputs());
    let guidance = if guidance_ready
        && matches!(
            boundary,
            RuntimeInputBoundary::Provider
                | RuntimeInputBoundary::Action
                | RuntimeInputBoundary::Settlement
                | RuntimeInputBoundary::IntentReady
        ) {
        inject_polled_user_intents_inner(host, state, true).await?
    } else {
        false
    };
    // The receive batch is bounded; transport-specific ACK I/O remains in
    // the mailbox owner. Progress never changes model context.
    let message = super::lifecycle::drain_mailbox_model_context(host, state).await?;
    let child = if matches!(
        boundary,
        RuntimeInputBoundary::Provider
            | RuntimeInputBoundary::Action
            | RuntimeInputBoundary::Settlement
            | RuntimeInputBoundary::ChildReady
    ) {
        stage_direct_child_results(host, state)?
    } else {
        false
    };
    Ok(RuntimeInputOutcome {
        model_context_changed: guidance || message || child,
        durable_guidance_applied: guidance,
    })
}

async fn inject_polled_user_intents_inner<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    force_poll: bool,
) -> Result<bool, astra_core::ClassifiedError> {
    let poll_started = tokio::time::Instant::now();
    let (run_control, user_id, run_id) = match (
        state.run_control.as_ref(),
        state.owner_user_id.as_ref(),
        state.current_run_id.as_ref(),
    ) {
        (Some(run_control), Some(user_id), Some(run_id)) => {
            (run_control.clone(), user_id.clone(), run_id.clone())
        }
        _ => return Ok(false),
    };
    let expected_session_id = state.current_session_id.clone().ok_or_else(|| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            format!("run {run_id} is missing its immutable session identity"),
        )
    })?;
    if !force_poll
        && !state.user_intents.should_poll_user_intents(poll_started)
        && !run_control.has_pending_inputs()
    {
        return Ok(false);
    }

    // Consume readiness before taking the snapshot. A notification arriving
    // during the read remains changed for the next boundary.
    state.user_intents.note_input_poll_started();
    let mut pages = 0_usize;
    let mut inspected_facts = 0_usize;
    let mut applied_model_guidance = false;
    loop {
        let page_start_cursor = state.user_intents.user_intent_cursor();
        let mut poll = run_control
            .poll_user_intents(&user_id, &run_id, page_start_cursor)
            .await;
        if let Some(error) = &poll.error {
            state.user_intents.retry_input_wake();
            state
                .user_intents
                .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
            state.user_intents.defer_pending_apply_ack(poll_started);
            tracing::warn!(run_id, error = %error, "user intent poll failed");
            if force_poll {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Unknown,
                    format!("authoritative user intent boundary poll failed: {error}"),
                ));
            }
            return Ok(applied_model_guidance);
        }

        pages = pages.saturating_add(1);
        inspected_facts = inspected_facts.saturating_add(poll.snapshot_page_fact_count);
        let page_has_more = poll.snapshot_has_more;
        let page_cursor = poll.next_cursor;
        if page_cursor < page_start_cursor
            || (poll.snapshot_page_fact_count > 0 && page_cursor == page_start_cursor)
            || (page_has_more && poll.snapshot_page_fact_count == 0)
        {
            tracing::error!(
                run_id,
                page_start_cursor,
                page_cursor,
                page_facts = poll.snapshot_page_fact_count,
                page_has_more,
                "authoritative user-intent pagination violated forward progress"
            );
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                format!(
                    "authoritative user-intent pagination made no forward progress for run {run_id}: cursor {page_start_cursor} -> {page_cursor}, facts={}, has_more={page_has_more}",
                    poll.snapshot_page_fact_count
                ),
            ));
        }
        if pages > MAX_USER_INTENT_BOUNDARY_PAGES
            || inspected_facts > MAX_USER_INTENT_BOUNDARY_FACTS
            || (page_has_more && pages == MAX_USER_INTENT_BOUNDARY_PAGES)
        {
            tracing::error!(
                run_id,
                pages,
                inspected_facts,
                page_has_more,
                max_pages = MAX_USER_INTENT_BOUNDARY_PAGES,
                max_facts = MAX_USER_INTENT_BOUNDARY_FACTS,
                "authoritative user-intent snapshot exceeded the boundary drain limit"
            );
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                format!(
                    "authoritative user-intent snapshot exceeds the boundary drain limit for run {run_id}: pages={pages}, facts={inspected_facts}, has_more={page_has_more}"
                ),
            ));
        }

        // A prior executor may have committed the durable apply disposition
        // and crashed before checkpointing model context. Replay that outbox
        // exactly once by stable intent identity.
        let mut durably_applied = Vec::new();
        poll.inputs.retain(|event| {
            if event.status == astra_turn_types::UserIntentStatus::Applied {
                if !state.user_intents.has_applied_user_intent(&event.intent_id) {
                    durably_applied.push(event.clone());
                }
                false
            } else {
                true
            }
        });
        applied_model_guidance |= apply_acknowledged_user_intents(host, state, &durably_applied);

        let mut runtime_notifications = Vec::new();
        poll.inputs.retain(|event| {
            if let Some(content) =
                crate::turn::run_control::runtime_notification_content(&event.input)
            {
                runtime_notifications.push((event.clone(), content));
                false
            } else {
                true
            }
        });
        let observed = state
            .user_intents
            .observe_polled_user_intents(poll, crate::turn::run_control::user_intent_content);
        for issue in &observed.issues {
            tracing::error!(
                run_id,
                event_index = issue.event_index,
                intent_id = issue.intent_id.as_deref().unwrap_or(""),
                kind = ?issue.kind,
                "invalid durable user intent was isolated"
            );
        }
        let mut accepted_for_ack = observed.accepted.clone();
        accepted_for_ack.extend(runtime_notifications.iter().map(|(event, _)| event.clone()));
        let has_new_apply_events = state
            .user_intents
            .stage_pending_disposition_events(
                &accepted_for_ack,
                astra_turn_types::UserIntentStatus::Applied,
            )
            .map_err(|error| {
                astra_core::ClassifiedError::new(astra_core::ErrorKind::ContractViolation, error)
            })?;
        let release_event_indices = state.user_intents.pending_apply_event_indices();

        if release_event_indices.is_empty() {
            state
                .user_intents
                .commit_observed_cursor(observed.next_cursor);
        } else if !has_new_apply_events
            && !state
                .user_intents
                .should_retry_apply_ack(tokio::time::Instant::now())
            && !force_poll
        {
            state
                .user_intents
                .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
            return Ok(applied_model_guidance);
        } else {
            if let Some((ack, model_context_changed)) = apply_pending_user_intents(
                host,
                state,
                run_control.as_ref(),
                &user_id,
                &expected_session_id,
                &run_id,
                &release_event_indices,
                Some(observed.next_cursor),
                force_poll,
                None,
            )
            .await?
            {
                applied_model_guidance |= model_context_changed;
                if ack == crate::turn::run_control::UserIntentApplyAck::RunTerminalReturned {
                    state.user_intents.note_user_intent_poll_finished(
                        poll_started,
                        USER_INTENT_EMPTY_POLL_INTERVAL,
                    );
                    tracing::debug!(
                        run_id = %run_id,
                        ?release_event_indices,
                        "terminal run won user intent application race"
                    );
                    if force_poll {
                        return Err(astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "run terminated while applying authoritative user guidance; stale provider actions were discarded",
                        ));
                    }
                    return Ok(applied_model_guidance);
                }
            } else if force_poll {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    format!(
                        "authoritative user guidance could not be durably acknowledged before the action boundary for run {run_id}"
                    ),
                ));
            } else {
                state
                    .user_intents
                    .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
                return Ok(applied_model_guidance);
            }
        }

        if !force_poll || !page_has_more {
            state
                .user_intents
                .note_user_intent_poll_finished(poll_started, USER_INTENT_EMPTY_POLL_INTERVAL);
            return Ok(applied_model_guidance);
        }
    }
}

pub(crate) fn apply_acknowledged_user_intents<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    acknowledged: &[crate::turn::run_control::QueuedUserIntent],
) -> bool {
    let mut model_guidance = Vec::new();
    let mut runtime_notifications = Vec::new();
    let mut active_work_contexts = Vec::new();
    for event in acknowledged {
        if let Some(content) = crate::turn::run_control::runtime_notification_content(&event.input)
        {
            runtime_notifications.push(content);
            continue;
        }
        let Some(content) = crate::turn::run_control::user_intent_content(&event.input) else {
            continue;
        };
        host.apply_user_intent_context(event);
        if let Some(context) = event.input.get("astra_runtime_context").filter(|context| {
            context.get("schema").and_then(serde_json::Value::as_str)
                == Some("active_work_snapshot.v1")
                && context.get("authority").and_then(serde_json::Value::as_str)
                    == Some("run_control_provider")
        }) {
            active_work_contexts.push(context.clone());
        }
        model_guidance.push(super::host::AppliedUserIntent {
            intent_id: event.intent_id.clone(),
            delivery: event.delivery,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: event.event_index,
            content,
        });
    }

    if !model_guidance.is_empty() {
        // A queued user instruction starts a new semantic turn. Preserve the
        // accumulated exploration/budget shape, but clear the prior turn's
        // effect and completion authority until the bounded classifier owns a
        // new value. Otherwise ReadOnly/MustMutate can leak across user
        // steering and make tool admission contradict the current request.
        state.canonical_turn_chain_id = model_guidance.last().map(|input| input.intent_id.clone());
        state.turn_intent = None;
        state.task_profile.mutates_workspace = false;
        state.task_profile.verification_required = false;
        state.hooks.completion_settlement = Default::default();
        state.turn_guard.set_task_profile(state.task_profile);
        if let Some(executor) = state.runtime_tool_executor.as_deref() {
            executor.set_workspace_mutation_intent(
                astra_config::user_profile::WorkspaceMutationIntent::Unknown,
            );
        }
        let combined = model_guidance
            .iter()
            .map(|input| input.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        state.push_prompt_history_message(serde_json::json!({
            "role": "user",
            "content": combined,
        }));
        // Freeze the exact acknowledged canonical message, including every
        // input in this batch. The prior turn's frozen source and the last
        // individual input cannot own observations of this combined message.
        state.telemetry.turn_intent_context = Some(
            crate::turn::agentic::turn_intent::build_turn_intent_judge_context(
                &state.messages,
                &combined,
                &combined,
                state.current_session_turn_number(),
                &state.recent_tools,
                &state.skills.execution.invoked,
            ),
        );
        state.message = model_guidance
            .last()
            .map(|input| input.content.clone())
            .unwrap_or_default();
        state
            .user_intents
            .record_applied_user_intents(&model_guidance);
    }
    if !runtime_notifications.is_empty() {
        state.push_volatile_payload(
            super::host::VolatileKind::BackgroundTaskNotification,
            serde_json::json!({
                "schema": "background_task_notification.v1",
                "priority": "below_latest_user_intent",
                "updates": runtime_notifications,
                "instruction": "Reconcile these runtime facts with the latest user goal. Do not let a stale completion override newer user steering.",
            }),
        );
    }
    if !active_work_contexts.is_empty() {
        for context in &mut active_work_contexts {
            state.reconcile_active_work_context(context);
        }
        state.push_volatile_payload(
            super::host::VolatileKind::ActiveWorkSnapshot,
            serde_json::json!({
                "schema": "active_work_guidance_context.v1",
                "snapshots": active_work_contexts,
                "instruction": "This is the runtime-owned work state captured when the active-run guidance was accepted. Use canonical group/task IDs from it. Treat fanout groups as one work unit; do not copy child IDs or infer group completion from individual events.",
                "authority": "runtime_required_context",
            }),
        );
    }
    !model_guidance.is_empty()
}

/// Complete the durable acknowledgement for events that have already been
/// staged by the canonical user-intent owner. This function deliberately does
/// not poll for new input: a native provider may have rejected or ambiguously
/// delivered a different event, and that event must not become applied merely
/// because a settlement boundary was reached.
pub(crate) async fn apply_pending_user_intents<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    run_control: &dyn RunControlProvider,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    event_indices: &[usize],
    commit_cursor: Option<usize>,
    strict: bool,
    invocation_identity: Option<&astra_turn_types::ToolInvocationIdentity>,
) -> Result<Option<(crate::turn::run_control::UserIntentApplyAck, bool)>, astra_core::ClassifiedError>
{
    loop {
        let outcome = apply_pending_user_intents_once(
            host,
            state,
            run_control,
            user_id,
            session_id,
            run_id,
            event_indices,
            commit_cursor,
            strict,
            invocation_identity.map(|identity| {
                crate::turn::run_control::UserIntentInvocationApplication {
                    identity,
                    purpose: astra_services::runs::UserIntentApplyPurpose::ModelBoundary,
                }
            }),
        )
        .await?;
        if !matches!(
            outcome,
            Some((crate::turn::run_control::UserIntentApplyAck::Paused, _))
        ) {
            return Ok(outcome);
        }
        if wait_for_pause_clear_or_cancel(host, state).await? {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "run cancelled while accepted guidance was awaiting resume",
            ));
        }
    }
}

/// One persistence attempt without waiting for resume. Provider waiters must
/// keep polling completion and cancellation while an application is paused.
pub(crate) async fn apply_pending_user_intents_once<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    run_control: &dyn RunControlProvider,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    event_indices: &[usize],
    commit_cursor: Option<usize>,
    strict: bool,
    invocation_identity: Option<crate::turn::run_control::UserIntentInvocationApplication<'_>>,
) -> Result<Option<(crate::turn::run_control::UserIntentApplyAck, bool)>, astra_core::ClassifiedError>
{
    let authority = state
        .current_run_owner_generation
        .map(UserIntentAdmissionAuthority::DurableOwnerGeneration)
        .unwrap_or(UserIntentAdmissionAuthority::ProcessLocal);
    let disposition = state
        .user_intents
        .pending_disposition(event_indices)
        .map_err(|error| {
            astra_core::ClassifiedError::new(astra_core::ErrorKind::ContractViolation, error)
        })?;
    let invocation_identity = if disposition == astra_turn_types::UserIntentStatus::Returned {
        Some(
            invocation_identity
                .map(|mut application| {
                    application.purpose = application.purpose.returning();
                    application
                })
                .ok_or_else(|| {
                    astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::ContractViolation,
                        "provider rejection requires its exact invocation authority",
                    )
                })?,
        )
    } else {
        invocation_identity
    };
    match run_control
        .mark_user_intents_applied(
            user_id,
            session_id,
            run_id,
            event_indices,
            authority,
            invocation_identity,
        )
        .await
    {
        Ok(crate::turn::run_control::UserIntentApplyAck::Paused) => {
            let pause_flag = state.cancellation.pause_flag.as_ref().ok_or_else(|| {
                astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    "paused input consumer has no shared execution pause control",
                )
            })?;
            pause_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            state
                .user_intents
                .defer_pending_apply_ack(tokio::time::Instant::now());
            Ok(Some((
                crate::turn::run_control::UserIntentApplyAck::Paused,
                false,
            )))
        }
        Ok(crate::turn::run_control::UserIntentApplyAck::Applied) => {
            let acknowledged = state.user_intents.acknowledge_apply_events(event_indices);
            let model_context_changed = apply_acknowledged_user_intents(host, state, &acknowledged);
            for event in &acknowledged {
                if crate::turn::run_control::runtime_notification_content(&event.input).is_none() {
                    host.on_user_intent_applied(event).await;
                }
            }
            if let Some(cursor) = commit_cursor {
                state.user_intents.commit_observed_cursor(cursor);
            }
            Ok(Some((
                crate::turn::run_control::UserIntentApplyAck::Applied,
                model_context_changed,
            )))
        }
        Ok(
            ack @ (crate::turn::run_control::UserIntentApplyAck::Returned
            | crate::turn::run_control::UserIntentApplyAck::RunTerminalReturned),
        ) => {
            let returned = state
                .user_intents
                .return_pending_apply_events(event_indices);
            for event in &returned {
                if crate::turn::run_control::runtime_notification_content(&event.input).is_none() {
                    host.on_user_intent_returned(event).await;
                }
            }
            if let Some(cursor) = commit_cursor {
                state.user_intents.commit_observed_cursor(cursor);
            }
            Ok(Some((ack, false)))
        }
        Err(error) => {
            state
                .user_intents
                .note_apply_ack_failure(tokio::time::Instant::now());
            tracing::warn!(
                run_id,
                ?event_indices,
                error = %error,
                "failed to durably acknowledge user intent application"
            );
            if strict {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    format!(
                        "authoritative user guidance could not be durably acknowledged before the action boundary: {error}"
                    ),
                ));
            }
            Ok(None)
        }
    }
}

fn runtime_feedback_run_usage(
    state: &AgenticLoopState,
    accum: &astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum,
) -> Option<astra_turn_core::token_accounting::TokenAccounting> {
    accum.has_usage.then(|| {
        if accum.usage_is_run_total {
            astra_turn_core::token_accounting::TokenAccounting::from_fields(
                accum.prompt_tokens,
                accum.cache_read_tokens,
                accum.cache_creation_tokens,
                accum.completion_tokens,
            )
        } else {
            astra_turn_core::token_accounting::TokenAccounting::from_fields(
                state.total_prompt,
                state.total_cache_read,
                state.total_cache_creation,
                state.total_completion,
            )
        }
    })
}

/// Record an `llm_round` event for an early-exit path (no tool calls).
fn record_early_exit_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
    finish_reason: Option<&str>,
) {
    record_observed_llm_round(state, turn_result, turn_start, finish_reason, Vec::new());
}

/// Retain the physical provider facts for a response whose conversational
/// content and tool authority were superseded by a newer durable control
/// epoch. Accounting is evidence, not execution authority: it must survive
/// even though no assistant text or tool call from this response is ingested.
fn record_superseded_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
) {
    if state.telemetry.first_ttft_ms.is_none() {
        state.telemetry.first_ttft_ms = turn_result.ttft_ms;
    }
    state.total_prompt = state
        .total_prompt
        .saturating_add(turn_result.accum.prompt_tokens);
    state.total_completion = state
        .total_completion
        .saturating_add(turn_result.accum.completion_tokens);
    state.total_cache_read = state
        .total_cache_read
        .saturating_add(turn_result.accum.cache_read_tokens);
    state.total_cache_creation = state
        .total_cache_creation
        .saturating_add(turn_result.accum.cache_creation_tokens);
    state.step_recorder.record_tokens(
        turn_result.accum.prompt_tokens,
        turn_result.accum.completion_tokens,
    );
    state.has_any_usage |= turn_result.accum.has_usage;
    state.add_qualified_usage(turn_result.accum.accounted_usage());
    state.last_request_usage = turn_result.accum.current_request_usage;
    state.last_measured_prompt_tokens = turn_result.accum.measured_request_input_tokens();
    let tool_names = turn_result
        .accum
        .tool_calls
        .iter()
        .filter_map(|call| {
            astra_turn_core::tool::args::shape::tool_call_name(call).map(ToString::to_string)
        })
        .collect();
    record_observed_llm_round(
        state,
        turn_result,
        turn_start,
        Some("superseded_by_runtime_input"),
        tool_names,
    );
}

fn record_observed_llm_round(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_start: Instant,
    finish_reason: Option<&str>,
    tool_call_names: Vec<String>,
) {
    let agentic_step = current_agentic_step(state);
    let run_id = state.current_run_id.clone();
    let duration_ms = turn_start.elapsed().as_millis() as u64;
    let start_offset_ms = state
        .turn_event_buffer
        .as_ref()
        .map(|buffer| buffer.offset_ms().saturating_sub(duration_ms))
        .unwrap_or_default();
    state.push_recent_round(super::host::RecentRoundSummary {
        qualified_usage: turn_result.accum.qualified_usage,
        purpose: state.inference_purpose,
        turn: state.session_turn,
        round: state.current_round_index,
        provider: String::new(),
        model: state.current_model_identity().unwrap_or("").to_string(),
        prompt_tokens: turn_result.accum.prompt_tokens,
        cache_read_tokens: turn_result.accum.cache_read_tokens,
        cache_creation_tokens: turn_result.accum.cache_creation_tokens,
        completion_tokens: turn_result.accum.completion_tokens,
        tool_calls_returned: tool_call_names.len() as u32,
        tool_call_names: tool_call_names.clone(),
        start_offset_ms,
        duration_ms,
        finish_reason: finish_reason.map(ToString::to_string),
    });
    let producer_agent_id = (state.inference_purpose
        == astra_turn_types::InferencePurpose::SubAgent)
        .then(|| state.self_agent_id.clone());
    if let Some(ref mut buf) = state.turn_event_buffer {
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            purpose: state.inference_purpose,
            ttft_ms: turn_result.ttft_ms,
            duration_ms,
            prompt_tokens: turn_result.accum.prompt_tokens,
            completion_tokens: turn_result.accum.completion_tokens,
            cache_read_tokens: turn_result.accum.cache_read_tokens,
            cache_creation_tokens: turn_result.accum.cache_creation_tokens,
            tool_calls_returned: tool_call_names.len() as u32,
            tool_call_names,
            finish_reason: finish_reason.map(Into::into),
            agentic_step: Some(agentic_step),
            source: Some("agentic_loop".into()),
            run_id,
            parent_run_id: None,
            tool_calls: None,
            agent_id: producer_agent_id,
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
    WaitForInput,
    Return(AgenticLoopOutcome),
}

fn apply_terminal_control_stream_snapshot(
    state: &mut AgenticLoopState,
    snap: &AgenticTurnStreamSnapshot<'_>,
    control_outcome: crate::turn::terminal_control::TerminalControlOutcome,
) -> AgenticLoopOutcome {
    if state.telemetry.first_ttft_ms.is_none() {
        state.telemetry.first_ttft_ms = snap.ttft_ms;
    }
    if let Some(session_id) = snap.session_id.as_ref() {
        state.current_session_id = Some(session_id.clone());
    }
    if let Some(run_id) = snap.run_id.as_deref() {
        state.current_run_id = Some(run_id.to_string());
        if let Some(session_id) = snap.session_id.as_deref() {
            state
                .step_recorder
                .bind_authoritative_run(session_id, run_id);
        }
    }
    state.total_prompt += snap.prompt_tokens;
    state.total_completion += snap.completion_tokens;
    state.total_cache_read += snap.cache_read_tokens;
    state.total_cache_creation += snap.cache_creation_tokens;

    state.add_qualified_usage(snap.qualified_usage);

    state
        .step_recorder
        .record_tokens(snap.prompt_tokens, snap.completion_tokens);
    state.has_any_usage |= snap.has_usage;
    // Only physical-request evidence can calibrate the next context window.
    state.last_measured_prompt_tokens = snap.measured_request_input_tokens();
    state.consecutive_context_window_errors = 0;

    state.last_request_usage = snap.current_request_usage;

    match control_outcome {
        crate::turn::terminal_control::TerminalControlOutcome::Requested(_) => {
            AgenticLoopOutcome::Delegated
        }
        crate::turn::terminal_control::TerminalControlOutcome::Rejected(rejection) => {
            AgenticLoopOutcome::ControlRejected(rejection)
        }
        crate::turn::terminal_control::TerminalControlOutcome::Passthrough => {
            unreachable!("host must not surface a passthrough terminal-control outcome")
        }
    }
}

fn refresh_context_pressure_advisory(state: &mut AgenticLoopState) {
    use crate::turn::providers::LiveRuntimeProvider;
    let pressure = crate::turn::local_provider::LocalSessionProvider::new(state).token_pressure();
    state.clear_volatile(super::host::VolatileKind::ContextPressure);
    let urgency = if pressure >= 0.90 {
        "aggressive"
    } else if pressure >= 0.70 {
        "normal"
    } else {
        return;
    };
    state.push_volatile_payload(
        super::host::VolatileKind::ContextPressure,
        serde_json::json!({
            "schema": "runtime_policy_advisory.v1",
            "signal": "context_pressure_observed",
            "evidence": {"token_pressure": pressure, "urgency": urgency},
            "authority": "advisory_evidence_only",
        }),
    );
    tracing::info!(
        target: "astra::policy",
        urgency,
        token_pressure = pressure,
        "policy context-pressure evidence recorded"
    );
}

pub(crate) fn record_context_compactions<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    observations: &[astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation],
) {
    for observation in observations {
        let Some(event) = observation.record_projection(
            state.max_turn_input_tokens,
            &mut state.step_recorder,
            state.pipeline_session.as_mut(),
        ) else {
            continue;
        };
        state.context_compression_triggered = true;
        state.compact_tier_applied = state.compact_tier_applied.max(observation.tier);
        state
            .compaction_effectiveness
            .record_compaction(observation.tokens_saved);
        if observation.effectiveness
            == astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Insufficient
        {
            state.compaction_effectiveness.mark_insufficient();
        }
        host.on_compaction(event);
    }
}

pub(crate) async fn execute_turn_and_ingest_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
) -> Result<TurnExecutionControl, astra_core::ClassifiedError> {
    // A provider request is an externally metered action boundary. Polling is
    // deliberately forced here rather than governed by the UI-oriented empty
    // cadence: one indexed control-lane read per provider round is the bounded
    // cost of guaranteeing that post-tool guidance supersedes stale work
    // before more model traffic is admitted.
    loop {
        inject_polled_user_intents_before_provider(host, state).await?;
        match authorize_provider_boundary(state).await? {
            ProviderBoundaryGate::Authorized => break,
            ProviderBoundaryGate::Paused => {
                if state.cancellation.pause_flag.is_none() {
                    return Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::ContractViolation,
                        "paused provider boundary has no retained control flag",
                    ));
                }
                if wait_for_pause_clear_or_cancel(host, state).await? {
                    return Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::Cancelled,
                        "run was cancelled while paused before a provider boundary",
                    ));
                }
            }
        }
    }
    super::guards::refresh_work_evidence_context(state);

    // Project the immutable policy revision selected at the preceding
    // authoritative tool boundary. Request retries and preparation reruns
    // render the same bytes; they never evaluate or advance policy state.
    state.clear_volatile(super::host::VolatileKind::PolicyAdvisory);
    if let Some(payload) =
        crate::turn::runtime_policy::policy_advisory_payload(&state.stall.active_policy_feedback)
    {
        state.push_volatile_payload(super::host::VolatileKind::PolicyAdvisory, payload);
    }

    // Interaction mode controls only whether policy evidence is also rendered
    // as user-facing status text. Provider cache capability controls delivery:
    // RequiredOnly intentionally omits optional advisory evidence from the
    // wire; All delivers it according to the declared placement. Neither
    // placement nor generation proves reuse of accumulated history or model
    // adoption.
    let show_policy_feedback_status = host.turn_interaction_mode().shows_policy_feedback_status();
    // Resolve from this execution's selected policy without round-local I/O.
    let tool_cfg = &state.admitted_tool_policy;
    let resolved_tool_policy = tool_cfg.resolve_for_model(state.policy_model_name.as_deref());
    let parallel_batching_force_threshold =
        resolved_tool_policy.parallel_batching_force_streak as usize;
    let cache_waste_threshold = tool_cfg.effective_cache_waste_midloop_threshold() as usize;

    // ── Composable guard pipeline ────────────────────────────────────────
    // Each guard is defined in the `guards` module. The pipeline evaluates
    // them in order and records advisory evidence. Guards are
    // independently testable — see guards.rs for individual unit tests.
    //
    // Previously each guard was inlined as ~30-line blocks below; the
    // pipeline reduces this section from ~80 lines to ~15.
    //
    // Runs after the circuit breaker so guards can avoid stacking redundant
    // advisory evidence when a stronger signal was already emitted.
    {
        let guard_cfg = super::guards::GuardConfig {
            parallel_batching_force_streak: parallel_batching_force_threshold,
            cache_waste_threshold,
        };
        let guards = super::guards::default_guards();
        for (hint_style, hint_text) in super::guards::evaluate_guards(&guards, state, &guard_cfg) {
            if show_policy_feedback_status && !prep.quiet {
                host.emit_headless_line(hint_style, hint_text);
            }
        }
    }

    refresh_context_pressure_advisory(state);

    // ── Circuit breaker observation ──────────────────────────────────────
    // Feed the previous round's signal to the circuit breaker. The runtime
    // treats loop-pattern detectors as observation, not authority: repeated
    // read/search/tool patterns can be valid work. Only infrastructure hard
    // ceilings may terminate the turn here. Advisory signals are recorded for
    // telemetry and introspection but do not inject extra user messages.
    if state.llm_rounds_completed > 0 {
        let signal = build_circuit_breaker_signal(state);
        let action = state.stall.circuit_breaker.observe(signal);
        match action {
            astra_turn_core::loop_circuit_breaker::BreakerAction::PatternObserved => {
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    serde_json::json!({
                        "signal": "repeated_behavior_pattern",
                        "round": state.llm_rounds_completed,
                        "assessment": "The recent tool pattern may be repetitive. Treat this as evidence when choosing the next action; continue if the repetition is justified by the task."
                    }),
                );
                state
                    .stall
                    .circuit_breaker
                    .acknowledge_pattern_observation();
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_advisory",
                    round = state.llm_rounds_completed,
                    "circuit breaker observation delivered as advisory evidence"
                );
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::AdvisoryThresholdReached => {
                let diagnosis = interruption_diagnosis_summary(state);
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    serde_json::json!({
                        "signal": "repetition_threshold_reached",
                        "round": state.llm_rounds_completed,
                        "diagnosis": diagnosis,
                        "assessment": "A behavior-pattern detector reached its configured threshold. This is advisory evidence, not a budget or safety boundary; decide whether to change approach or continue with the current evidence."
                    }),
                );
                tracing::warn!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_threshold_advisory",
                    round = state.llm_rounds_completed,
                    "circuit breaker threshold recorded as advisory evidence"
                );
                if show_policy_feedback_status && !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!(
                            "↻ Repetition threshold observed at round {}; continuing with advisory evidence.",
                            state.llm_rounds_completed
                        ),
                    );
                }
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::HardRoundLimitReached {
                rounds,
                limit,
            } => {
                let detail = format!("Infrastructure round limit reached: {rounds}/{limit} rounds");
                let _cancelled = super::lifecycle::cancel_unfinished_child_agents(
                    host,
                    state,
                    &detail,
                    astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                )
                .await;
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::BudgetExhausted,
                    ResumeAction::ContinueImmediately,
                    interruption_state_summary(state, Some(detail.clone())),
                ));
                if !prep.quiet {
                    host.emit_headless_line(
                        HeadlessStderrStyle::Yellow,
                        format!("⚠ {detail}; preserving completed work."),
                    );
                }
                try_write_heavy_checkpoint(state);
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Introspect {
                consecutive_read_only,
            } => {
                state.stall.introspection_count = state.stall.introspection_count.saturating_add(1);
                let emission_index = state.stall.introspection_count;
                state.push_volatile_payload(
                    super::host::VolatileKind::CircuitBreaker,
                    serde_json::json!({
                        "signal": "read_only_streak",
                        "consecutive_read_only": consecutive_read_only,
                        "round": state.llm_rounds_completed,
                        "assessment": "Review whether the current read-only investigation is still producing new evidence. This signal does not require stopping or changing tools."
                    }),
                );
                tracing::info!(
                    target: "astra::loop_guard",
                    tier = "circuit_breaker_introspect_advisory",
                    round = state.llm_rounds_completed,
                    consecutive_read_only,
                    emission = emission_index,
                    "circuit breaker introspection signal delivered as advisory evidence"
                );
            }
            astra_turn_core::loop_circuit_breaker::BreakerAction::Continue => {}
            // BreakerAction is #[non_exhaustive] — future soft-intervention
            // variants should default to a no-op so the loop continues.
            _ => {}
        }
    }

    // ── Harness: PreLlmRequest — Block/Pause prevents LLM call ──
    #[cfg(feature = "harness")]
    match super::super::harness_adapter::harness_at!(
        &state.harness,
        astra_harness::HookPoint::PreLlmRequest,
        state
    ) {
        astra_harness::HookVerdict::Block { reason } => {
            tracing::warn!(reason = %reason, "harness blocked LLM call at PreLlmRequest");
            super::host::set_harness_interruption(
                state,
                astra_turn_core::interruption::InterruptionKind::HarnessBlocked,
                &reason,
            );
            return Ok(TurnExecutionControl::Return(
                super::host::AgenticLoopOutcome::Completed,
            ));
        }
        astra_harness::HookVerdict::Pause { reason, .. } => {
            tracing::info!(reason = %reason, "harness paused LLM call at PreLlmRequest");
            super::host::set_harness_interruption(
                state,
                astra_turn_core::interruption::InterruptionKind::HarnessPaused,
                &reason,
            );
            return Ok(TurnExecutionControl::Return(
                super::host::AgenticLoopOutcome::Completed,
            ));
        }
        astra_harness::HookVerdict::Continue => {}
    }
    #[cfg(not(feature = "harness"))]
    super::super::harness_adapter::harness_at!(
        &state.harness,
        astra_harness::HookPoint::PreLlmRequest,
        state
    );

    let request_message_count = state.messages.len();
    let llm_attempt_index = state.llm_rounds_completed;
    state.last_llm_context_manifest_trace = None;
    // Protect the exact request prefix we are about to send even if the LLM
    // call fails; the next retry/compaction pass must not clear tool results
    // that were already part of this attempted request.
    state.last_request_message_count = Some(request_message_count);
    // Increment the LLM-round counter regardless of outcome so retry/error
    // paths don't see a stale count (the counter tracks *attempted* LLM
    // calls for guidance-threshold purposes, not just successful ones).
    let llm_wall_start = Instant::now();
    let t_llm_start = std::time::Instant::now();
    if !host.owns_model_inference_timing() {
        host.on_turn_phase_started(
            state,
            TurnPhaseKind::ModelInference,
            turn_index as u32,
            0,
            llm_wall_start,
        );
    }
    tracing::debug!(
        target: "astra_timing",
        llm_round = llm_attempt_index,
        messages = request_message_count,
        "LLM call started"
    );
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_started(turn_index as u32);
    }
    state.current_model_item_id = Some(Uuid::new_v4().to_string());
    let parent_cancel = state.cancellation.token.clone();
    let previous_inference_token = state.cancellation.inference_token.take();
    let mut guidance_wake = state.user_intents.guidance_wake.clone();
    let guidance_cursor =
        i64::try_from(state.user_intents.user_intent_cursor()).unwrap_or(i64::MAX);
    let supersession = std::sync::Arc::new(
        parent_cancel
            .as_ref()
            .map(|token| token.child_token())
            .unwrap_or_default(),
    );
    if guidance_wake.is_some() {
        state.cancellation.inference_token = Some(supersession.clone());
    } else {
        state.cancellation.inference_token = previous_inference_token.clone();
    }
    let (turn_result, guidance_preempted) = {
        let execution = host.execute_turn(state);
        tokio::pin!(execution);
        if let Some(wake) = guidance_wake.as_mut() {
            loop {
                if *wake.borrow_and_update() > guidance_cursor {
                    // Cancel only this inference, then await its existing
                    // settlement owner; dropping it would lose usage and
                    // provider-delivery evidence.
                    supersession.cancel();
                    break (execution.await, true);
                }
                tokio::select! {
                    biased;
                    result = &mut execution => break (result, false),
                    changed = wake.changed() => {
                        if changed.is_err() {
                            break (execution.await, false);
                        }
                    }
                }
            }
        } else {
            (execution.await, false)
        }
    };
    state.cancellation.inference_token = previous_inference_token;
    if let Some(ref emitter) = state.messaging.progress_emitter {
        emitter.llm_call_completed(
            turn_index as u32,
            turn_result.as_ref().ok().and_then(|result| result.ttft_ms),
            llm_wall_start.elapsed().as_millis() as u64,
        );
    }
    // `text_only` applies to one successful model boundary. Keep it through
    // the admission/tool phase when a provider nevertheless returns tools so
    // a retry cannot execute them. A safely recoverable provider failure keeps
    // the same authority until the bounded recovery decision below; every
    // other error clears it.
    let retain_text_only_for_tool_admission = turn_result
        .as_ref()
        .is_ok_and(|result| result.accum.has_tool_calls || !result.accum.tool_calls.is_empty());
    let provider_recovery_pending = turn_result.as_ref().is_err_and(|error| {
        matches!(
            error.kind,
            astra_core::ErrorKind::ProviderDeadline
                | astra_core::ErrorKind::StreamTransport
                | astra_core::ErrorKind::StreamIdle
        )
    });
    // Capture the current-round typed candidate. It remains live until every
    // terminal guard accepts the prose or an authoritative Work/user-intent
    // fact revokes it; merely receiving non-empty prose is not consumption.
    let committed_work_synthesis_candidate = state
        .hooks
        .completion_settlement
        .preserve_final_synthesis_wire_surface;
    // A compliant non-empty text response is the deliverable produced by the
    // bounded settlement boundary. The runtime limit constrains additional
    // tool execution; it is not itself evidence that the user's requested
    // answer failed. Tool-shaped or empty responses still follow the bounded
    // retry/interruption paths below.
    if !retain_text_only_for_tool_admission
        && !provider_recovery_pending
        && !(guidance_preempted
            && turn_result
                .as_ref()
                .is_err_and(|error| error.kind == astra_core::ErrorKind::Cancelled))
    {
        state.hooks.completion_settlement.text_only = false;
    }
    tracing::debug!(
        target: "astra_timing",
        llm_round = llm_attempt_index,
        elapsed_ms = t_llm_start.elapsed().as_millis(),
        ok = turn_result.is_ok(),
        "LLM call completed"
    );
    if !host.owns_model_inference_timing() {
        complete_turn_phase(
            host,
            state,
            llm_wall_start,
            TurnPhaseKind::ModelInference,
            turn_index as u32,
            0,
            if turn_result.is_ok() {
                TurnPhaseOutcome::Succeeded
            } else {
                TurnPhaseOutcome::Failed
            },
            format!("model_inference_{turn_index}"),
        );
    }
    state.record_local_usage_coverage(
        turn_result
            .as_ref()
            .is_ok_and(|result| result.accum.has_usage),
    );
    state.record_local_llm_round();
    let run_cancelled = parent_cancel
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
        || state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        || state
            .cancellation
            .execution_lease_lost
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire));
    if guidance_preempted
        && !run_cancelled
        && turn_result
            .as_ref()
            .is_err_and(|error| error.kind == astra_core::ErrorKind::Cancelled)
    {
        let error = turn_result
            .err()
            .expect("superseded inference returned cancellation");
        state.restore_volatile_attempt_lease();
        fold_provider_completion_error_usage(state, &error);
        // Provider partials may still be buffered behind completion checks.
        // Keep the prior canonical history; an interrupted response must not
        // become an assistant assertion or an executable tool call.
        state.final_text_model_item_id = None;
        state.final_text.clear();
        state.final_text_streamed = false;
        if !runtime_input_boundary(host, state, RuntimeInputBoundary::IntentReady)
            .await?
            .durable_guidance_applied
        {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "guidance superseded inference without an acknowledged instruction",
            ));
        }
        state.step_recorder.end_turn(false);
        return Ok(TurnExecutionControl::ContinueLoop);
    }
    // Capture finish_reason before the match consumes turn_result.
    // Used by textless-stop retry (loop level) and ensure_terminal_text
    // (finalization level) to distinguish true silence from forced truncation
    // when the API's max_tokens limit cuts off the model's output.
    state.last_finish_reason = turn_result
        .as_ref()
        .ok()
        .and_then(|r| r.accum.finish_reason.clone());
    // Bridge-reported context compactions live in TurnOutput::accum and are
    // available only when the host returned a successful output. Failed calls
    // have no accumulator evidence to record here.
    if let Ok(result) = &turn_result
        && !result.accum.context_compactions.is_empty()
    {
        record_context_compactions(host, state, &result.accum.context_compactions);
    }
    // Persist the per-call manifest only after the host returns: the durable
    // record includes observed token usage and the emitted context-manifest
    // trace, both of which are only available on the completed turn result.
    if let Ok(result) = &turn_result
        && let Some(trace) = result.accum.context_manifest_trace.clone()
    {
        state.last_llm_context_manifest_trace = Some(trace);
    }
    let preparation_context_error = turn_result
        .as_ref()
        .err()
        .filter(|error| error.kind == astra_core::ErrorKind::ContextWindow)
        .cloned();
    let turn_result = match turn_result {
        Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => Ok(HostTurnResult {
            accum: astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum {
                error_message: Some(error.message),
                ..Default::default()
            },
            ttft_ms: None,
            error_kind: Some(astra_core::ErrorKind::ContextWindow),
        }),
        other => other,
    };
    let mut turn_result = match turn_result {
        Ok(turn_result) => {
            let delivered_children: Vec<_> = state
                .volatile_pending
                .iter()
                .filter(|injection| {
                    injection.attempt_leased
                        && injection.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                        && injection.payload["observed_by_provider"] != true
                })
                .map(|injection| injection.payload.clone())
                .collect();
            for payload in &delivered_children {
                let outcome = if turn_result.accum.error_message.is_some() {
                    "result_delivery_deferred"
                } else {
                    "results_adopted"
                };
                record_direct_child_barrier(
                    host,
                    state,
                    payload["parent_run_id"].as_str().unwrap_or_default(),
                    outcome,
                    &payload["children"],
                    llm_wall_start,
                    None,
                );
            }
            // A provider response carrying an embedded error did not produce
            // an assistant decision.
            // Keep the exact leased authorities for the next real attempt.
            if turn_result.accum.error_message.is_some() {
                state.restore_volatile_attempt_lease();
            } else {
                state.commit_volatile_attempt_lease();
                super::lifecycle::acknowledge_adopted_mailbox_messages(state).await;
                if let Some(mailbox) = state.messaging.mailbox.as_mut() {
                    mailbox.retry_deferred();
                }
            }
            turn_result
        }
        Err(error) => {
            state.restore_volatile_attempt_lease();
            fold_provider_completion_error_usage(state, &error);
            let deadline_settlement_due = !state.hooks.completion_settlement.text_only
                && !state.budget_wrapup_injected
                && host
                    .execution_time_budget_remaining()
                    .is_some_and(|remaining| !remaining.has_work());
            let run_deadline_stopped_safe_work = error.kind
                == astra_core::ErrorKind::BudgetExhausted
                || (error.kind == astra_core::ErrorKind::ProviderDeadline
                    && provider_deadline_has_no_actionable_delivery(&error));
            if run_deadline_stopped_safe_work && deadline_settlement_due {
                // Preparation can cross the reserved boundary before dispatch,
                // or an ordinary request can reach its shortened provider
                // deadline. If no text or tool was delivered, re-enter the
                // owner loop so it can use the same execution's final-answer
                // window; never replay an uncertain actionable response.
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::ContinueLoop);
            }
            if crate::turn::llm::durable::is_guidance_admission_fence(&error) {
                // Admission rejected this request before HTTP delivery. Consume
                // the authoritative guidance using the ordinary control lane;
                // never retry the stale request or manufacture a new run.
                if runtime_input_boundary(host, state, RuntimeInputBoundary::Action)
                    .await?
                    .durable_guidance_applied
                {
                    state.final_text_model_item_id = None;
                    state.final_text.clear();
                    state.final_text_streamed = false;
                    state.step_recorder.end_turn(false);
                    return Ok(TurnExecutionControl::ContinueLoop);
                }
                // A fence without applicable guidance is not permission to
                // loop: cancellation/owner loss and inconsistent snapshots
                // must remain fail-closed.
                record_direct_llm_error_state(state, &error);
                return Err(error);
            }
            if schedule_safe_provider_recovery(state, &error) {
                tracing::warn!(
                    target: "astra::provider_recovery",
                    error = %error,
                    "provider attempt ended before executable or visible delivery; scheduling one capability-aware safe recovery round"
                );
                host.on_provider_recovery(state, error.kind);
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::ContinueLoop);
            }
            // The client reserves inference-ledger settlement inside the run
            // work slice. Its deadline may therefore leave a whole second of
            // work while the run's final-answer window is still untouched.
            // After the one safe recovery, only a dispatch proven to be
            // run-work-limited can enter that existing terminal boundary.
            if state.provider_adaptation.safe_recovery_attempted
                && provider_deadline_has_no_actionable_delivery(&error)
                && error
                    .details_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .is_some_and(|details| {
                        details
                            .pointer("/deadline/execution_boundary")
                            .and_then(serde_json::Value::as_str)
                            == Some("run_work")
                    })
                && host
                    .execution_time_budget_remaining()
                    .is_some_and(|remaining| remaining.total_remaining > remaining.work_remaining)
                && super::lifecycle::enter_deadline_budget_settlement(host, state).await
            {
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::ContinueLoop);
            }
            let empty_completion =
                record_repeated_transport_success_empty_completion(state, &error);
            state.hooks.completion_settlement.text_only = false;
            if empty_completion {
                // Affirmative clean non-delivery is an interrupted completion,
                // not a provider failure. Use the ordinary terminal path so
                // every surface preserves the producer's EmptyCompletion action.
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }
            if let Some(details) = error
                .details_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                && let Some(text) = details
                    .get("partial_full_text")
                    .and_then(serde_json::Value::as_str)
            {
                preserve_failed_provider_text(state, text, state.current_model_item_id.clone());
            }
            record_direct_llm_error_state(state, &error);
            return Err(error);
        }
    };
    // Reconcile newly accepted input before granting this response action authority.
    let action_fence = inject_polled_user_intents_before_action(host, state).await;
    if action_fence.as_ref().is_ok_and(|applied| *applied) || action_fence.is_err() {
        // Physical usage and round evidence remain true even though the
        // response no longer has conversational or execution authority.
        record_superseded_llm_round(state, &turn_result, prep.turn_start_time);
        update_turn_trace_collector(state, &turn_result);
    }
    let input_changed = match action_fence {
        Ok(applied) => applied,
        Err(error) => return Err(error),
    };
    if input_changed {
        // Usage/round accounting above remains true provider evidence, but
        // neither text nor tool calls from the older context may enter the
        // canonical conversation or execution lane. New runtime input was
        // retained by its source owner and the next request sees it.
        state.final_text_model_item_id = None;
        state.final_text.clear();
        state.final_text_streamed = false;
        state.push_volatile_payload(
            super::host::VolatileKind::RuntimeInputBoundary,
            serde_json::json!({
                "schema": "runtime_input_action_fence.v1",
                "signal": "provider_response_superseded",
                "execution_authority": "none",
                "instruction": "The preceding provider response was generated before new runtime input was applied. Re-evaluate the current objective using the newly retained input before choosing any action or final response.",
            }),
        );
        state.step_recorder.end_turn(false);
        return Ok(TurnExecutionControl::ContinueLoop);
    }

    let collapsed_observation_calls =
        collapse_batched_observation_fanout(&mut turn_result.accum.tool_calls);
    if collapsed_observation_calls > 0 {
        tracing::info!(
            target: "astra::provenance_guard",
            collapsed_observation_calls,
            "collapsed same-boundary observation facet fanout into composite overview calls"
        );
    }
    if turn_result.accum.error_message.is_none() {
        state.rate_limit_cooldown.record_success();
        if let Some(ref mut session) = state.pipeline_session {
            session.recovery.reset_on_success();
        }
    }
    let snap = agentic_turn_stream_snapshot_with_kind(
        &turn_result.accum,
        turn_result.ttft_ms,
        turn_result.error_kind,
    );

    state.last_request_usage = turn_result.accum.current_request_usage;
    update_turn_trace_collector(state, &turn_result);

    if let Some(control_outcome) = host.take_terminal_control_outcome()
        && turn_result.accum.error_message.is_none()
    {
        return Ok(TurnExecutionControl::Return(
            apply_terminal_control_stream_snapshot(state, &snap, control_outcome),
        ));
    }

    let tool_record_floor = state.stall.tool_call_records.len();
    let transcript_append_start = state.messages.len();

    state.add_qualified_usage(snap.qualified_usage);

    let ingest_state = AgenticTurnIngestMut {
        model_item_id: turn_result
            .accum
            .model_item_id
            .as_deref()
            .or(state.current_model_item_id.as_deref()),
        final_text_model_item_id: &mut state.final_text_model_item_id,
        first_ttft_ms: &mut state.telemetry.first_ttft_ms,
        current_session_id: &mut state.current_session_id,
        current_run_id: &mut state.current_run_id,
        final_text: &mut state.final_text,
        last_finish_reason: &mut state.last_finish_reason,
        total_prompt: &mut state.total_prompt,
        total_completion: &mut state.total_completion,
        total_cache_read: &mut state.total_cache_read,
        total_cache_creation: &mut state.total_cache_creation,
        total_tool_calls: &mut state.total_tool_calls,
        total_observation_tool_calls: &mut state.total_observation_tool_calls,
        step_recorder: &mut state.step_recorder,
        all_tools_used: &mut state.telemetry.all_tools_used,
        has_any_usage: &mut state.has_any_usage,
        messages: &mut state.messages,
        last_measured_prompt_tokens: &mut state.last_measured_prompt_tokens,
        consecutive_context_window_errors: &mut state.consecutive_context_window_errors,
    };
    let mut ingest_outcome = ingest_agentic_turn_stream(&snap, &state.message, ingest_state);
    if let (Some(original), AgenticTurnIngestOutcome::Fatal(error)) =
        (preparation_context_error, &mut ingest_outcome)
    {
        // Share recovery decisions without replacing the preparation owner's
        // structured budget, stage and non-execution diagnostics.
        *error = original;
    }
    // Ingest already materializes successful text-only replies. Attach protocol
    // state before the canonical transcript owner records those new messages.
    if state.final_text_model_item_id.is_some()
        && state.final_text_model_item_id == state.current_model_item_id
        && let Some(continuation) = state.pending_provider_continuation.as_ref()
    {
        for message in &mut state.messages[transcript_append_start..] {
            if astra_turn_types::model_item_id(message) == state.final_text_model_item_id.as_deref()
            {
                message[astra_core::model_wire::responses::CONTINUATION_KEY] = continuation.clone();
            }
        }
    }
    // Apply weak/partial quarantine on the same boundary as newly ingested
    // records, and checkpoint the first transition immediately.
    if let Some(records) = state.stall.tool_call_records.get(tool_record_floor..) {
        let records = records.to_vec();
        apply_workspace_observation_quarantine_transition(state, &records);
    }

    // Preserve settlement text at the observation boundary, before the
    // ingest control is consumed.  This covers both server tool calls and
    // edge-tool rounds, and lets a later compliant text-only retry replace a
    // prior mixed candidate without allowing another violating tool request
    // to overwrite it.
    capture_latest_provider_text(state, &turn_result);
    capture_deferred_candidate_text(state, &turn_result);
    host.attest_model_messages(state, transcript_append_start);
    state.record_appended_prompt_history_from(transcript_append_start);
    if !turn_result.accum.has_tool_calls && turn_result.accum.tool_calls.is_empty() {
        super::finalization::bound_ingested_terminal_message(state, transcript_append_start);
    }
    if let Some(session_id) = state.current_session_id.as_deref() {
        if let Some(buffer) = state.turn_event_buffer.as_mut()
            && let Err(error) = buffer.bind_session_id(session_id)
        {
            tracing::warn!(
                session_id,
                error = %error,
                "could not bind streamed session identity to first-round observability events"
            );
        }
    }

    // PR 5a: post-sampling hook. Fires exactly once after a
    // successful turn has been received AND cleanly ingested
    // (non-Fatal outcome), BEFORE any side effects (tool phase,
    // microcompact prep, memory extraction).
    //
    // Fatal ingest outcomes include SSE-embedded rate limits,
    // context-window overflows, and provider 5xx strings. On those,
    // state is only partially updated — firing the hook would let a
    // downstream capture snapshot record a corrupt prefix. We peek
    // at the variant via `matches!` so the original `ingest_outcome`
    // can still move by-value into the control-flow mapper below.
    let ingest_is_fatal = matches!(ingest_outcome, AgenticTurnIngestOutcome::Fatal(_));
    if !ingest_is_fatal {
        let turn = session_turn_number(state);
        let session_id = state.current_session_id.clone();
        let run_id = state.current_run_id.clone();
        let model_id = state.current_model_identity().map(str::to_string);
        let identity = session_id
            .as_deref()
            .zip(run_id.as_deref())
            .zip(model_id.as_deref())
            .filter(|((session_id, run_id), model_id)| {
                !session_id.trim().is_empty()
                    && !run_id.trim().is_empty()
                    && !model_id.trim().is_empty()
                    && !state.self_agent_id.trim().is_empty()
            })
            .map(|((session_id, run_id), model_id)| {
                astra_turn_core::context_feedback::RuntimeFeedbackIdentity {
                    session_id: session_id.to_string(),
                    run_id: run_id.to_string(),
                    agent_id: state.self_agent_id.clone(),
                    model_id: model_id.to_string(),
                    topology: host.runtime_feedback_topology(),
                    request: state
                        .last_llm_context_manifest_trace
                        .as_ref()
                        .and_then(|trace| trace.get("request_identity"))
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok()),
                }
            });
        // A logical response may contain multiple physical provider
        // attempts (for example one bounded output-cap retry).  The
        // aggregate belongs in run accounting, while context pressure
        // and cache decisions must use only the final physical request.
        // Missing physical evidence stays unknown. A retry/run subtotal
        // cannot measure the current request's context or cache rate.
        let request_usage = turn_result.accum.current_request_usage.map(|usage| {
            astra_turn_core::token_accounting::TokenAccounting::from_fields(
                usage.fresh_input_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.output_tokens,
            )
        });
        let run_usage = runtime_feedback_run_usage(state, &turn_result.accum);
        let mut runtime_feedback = {
            identity.map(|identity| {
                let wire_budget = state
                    .last_llm_context_manifest_trace
                    .as_ref()
                    .and_then(|trace| trace.pointer("/wire/budget"));
                let model_context_window_tokens = state
                    .last_llm_context_manifest_trace
                    .as_ref()
                    .and_then(|trace| trace.get("model_context_window_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .filter(|value| *value > 0);
                let estimated_input_tokens = wire_budget
                    .and_then(|budget| budget.get("estimated_input_tokens"))
                    .and_then(serde_json::Value::as_u64);
                let estimated_cache_eligible_tokens = prompt_cache_eligible_tokens_from_manifest(
                    state.last_llm_context_manifest_trace.as_ref(),
                );
                let effective_input_limit_tokens = wire_budget
                    .and_then(|budget| budget.get("effective_input_limit"))
                    .and_then(serde_json::Value::as_u64)
                    .filter(|value| *value > 0)
                    .or_else(|| {
                        (state.max_turn_input_tokens > 0).then_some(state.max_turn_input_tokens)
                    });
                let token_pressure = estimated_input_tokens
                    .zip(effective_input_limit_tokens)
                    .map(|(estimated, limit)| estimated as f64 / limit as f64);
                let prompt_cache_identity = prompt_cache_identity_from_manifest(
                    state.last_llm_context_manifest_trace.as_ref(),
                );
                let absolute_round_ceiling = state
                    .agentic_turn_budget
                    .hard_turn_limit
                    .map(|limit| u32::try_from(limit.get()).unwrap_or(u32::MAX));
                astra_turn_core::context_feedback::RuntimeFeedbackFrame {
                    schema_version:
                        astra_turn_core::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
                    identity,
                    progress: astra_turn_core::context_feedback::RuntimeFeedbackProgress {
                        session_turn: turn,
                        agentic_round_index: llm_attempt_index,
                        llm_rounds_completed: state.llm_rounds_completed,
                        slice_round_limit: u32::try_from(state.max_turns).unwrap_or(u32::MAX),
                        slice_rounds_remaining: u32::try_from(state.remaining_turns)
                            .unwrap_or(u32::MAX),
                        absolute_round_ceiling,
                    },
                    context: astra_turn_core::context_feedback::RuntimeContextFeedback {
                        prompt_cache_identity,
                        model_context_window_tokens,
                        effective_input_limit_tokens,
                        estimated_input_tokens,
                        estimated_cache_eligible_tokens,
                        token_pressure,
                        compaction_tier: state.compact_tier_applied,
                    },
                    request_usage,
                    run_usage,
                    was_truncated: state.last_finish_reason.as_deref() == Some("length"),
                    cache_break_detected: None,
                    policy_feedback: state.stall.active_policy_feedback.clone(),
                }
            })
        };
        if let (Some(pipeline_sess), Some(frame)) =
            (&mut state.pipeline_session, runtime_feedback.as_mut())
        {
            let accepted = pipeline_sess.record_runtime_feedback("agentic_loop", frame, None);
            if !accepted {
                tracing::warn!(
                    target: "astra_runtime::agentic_loop",
                    session_id = %frame.identity.session_id,
                    run_id = %frame.identity.run_id,
                    session_turn = frame.progress.session_turn,
                    llm_rounds_completed = frame.progress.llm_rounds_completed,
                    "rejecting invalid or out-of-order runtime feedback frame"
                );
            }

            if accepted {
                host.publish_runtime_feedback(frame);
                let feedback = frame.request_usage.map(|tokens| {
                    astra_turn_core::context_feedback::ContextFeedback {
                        tokens,
                        cache_hit_ratio: tokens.cache_hit_ratio(),
                        was_truncated: frame.was_truncated,
                        cache_break_detected: frame.cache_break_detected.clone(),
                    }
                });

                // Emit pipeline journal events for observability and cloud sync
                if let Some(ref mut buf) = state.turn_event_buffer {
                    // Per-turn feedback event
                    let feedback_evt =
                        astra_turn_core::pipeline_journal::PipelineJournalEvent::from_feedback(
                            frame,
                        );
                    if let Ok(payload) = serde_json::to_value(&feedback_evt) {
                        buf.record(
                            astra_services::session_journal::JournalEvent::pipeline_feedback(
                                session_id.as_deref(),
                                turn,
                                payload,
                            )
                            .with_producer_scope(run_id.as_deref()),
                        );
                    }

                    // Drain and emit compaction audit events
                    for audit in pipeline_sess.drain_pending_audits() {
                        if let Ok(payload) = serde_json::to_value(&audit) {
                            buf.record(
                                astra_services::session_journal::JournalEvent::pipeline_compaction_audit(
                                    session_id.as_deref(),
                                    turn,
                                    payload,
                                )
                                .with_producer_scope(run_id.as_deref()),
                            );
                        }
                    }

                    // Evaluate trace alerts and emit them to the journal.
                    let alerts = feedback.as_ref().map_or_else(Vec::new, |feedback| {
                        astra_turn_core::trace_alert::evaluate_alerts(
                            turn,
                            feedback,
                            &pipeline_sess.stats,
                            &pipeline_sess.recovery,
                        )
                    });
                    // Best-effort webhook dispatch: dispatcher is initialized once
                    // per process via a global OnceLock, reusing reqwest::Client's
                    // connection pool + TLS session cache across turns. Dispatch
                    // runs async so it never blocks turn execution.
                    if !alerts.is_empty() {
                        if let Some(session_id_str) =
                            alert_dispatch_session_id(session_id.as_deref())
                        {
                            if let Some(dispatcher) = global_alert_dispatcher() {
                                let alerts_to_send = alerts.clone();
                                let dispatcher = dispatcher.clone();
                                tokio::spawn(async move {
                                    dispatcher.dispatch(&session_id_str, &alerts_to_send).await;
                                });
                            }
                        } else {
                            tracing::warn!(
                                target: "astra_runtime::agentic_loop",
                                turn,
                                "skipping alert webhook dispatch without session_id"
                            );
                        }
                    }

                    for alert in &alerts {
                        let alert_evt =
                            astra_turn_core::pipeline_journal::PipelineJournalEvent::from_alert(
                                alert,
                            );
                        if let Ok(payload) = serde_json::to_value(&alert_evt) {
                            buf.record(
                                astra_services::session_journal::JournalEvent::pipeline_alert(
                                    session_id.as_deref(),
                                    turn,
                                    payload,
                                )
                                .with_producer_scope(run_id.as_deref()),
                            );
                        }
                    }
                }
            }
        } else if state.pipeline_session.is_some() {
            tracing::warn!(
                target: "astra_runtime::agentic_loop",
                turn,
                "skipping model-scoped pipeline feedback without resolved model identity"
            );
        }

        host.on_turn_completed(state);
    }

    let iteration_control = map_ingest_outcome_to_iteration_control(ingest_outcome);
    match iteration_control {
        AgenticIngestIterationControl::Fatal(e) => {
            use astra_core::ErrorKind;

            let is_rate_limit = matches!(e.kind, ErrorKind::RateLimit);

            if is_rate_limit {
                state.rate_limit_cooldown.record_429(None);
            }
            if matches!(e.kind, ErrorKind::ServerError) {
                state.rate_limit_cooldown.record_529(None);
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
                    <= super::super::compaction_replay::MAX_COMPACT_RETRIES
            {
                // Inform the pipeline session about the PTL error so its
                // RecoveryState can escalate tier on subsequent turns and
                // widen reserve estimates. This bridges the legacy compaction
                // retry path with the pipeline's observability/feedback loop.
                if let Some(ref mut sess) = state.pipeline_session {
                    sess.recovery.record_ptl_error();
                }

                let rewrite_permit = state.begin_canonical_rewrite();
                let outcome = super::super::compaction_replay::try_compact_for_retry_checked(
                    &mut state.messages,
                    &mut state.compaction_effectiveness,
                    state.last_measured_prompt_tokens,
                    state.max_turn_input_tokens,
                    state.consecutive_context_window_errors,
                );
                match outcome {
                    super::super::compaction_replay::CompactionReplayOutcome::Compacted(result) => {
                        state.finish_canonical_rewrite(rewrite_permit);
                        let tier_label = result.tier.to_string();
                        // Feed compaction stats into pipeline for reserve estimation.
                        if let Some(ref mut sess) = state.pipeline_session {
                            sess.recovery.record_reactive_compact();
                            sess.stats.record_compaction(result.tokens_freed);
                        }
                        let tokens_freed = result.pipeline_outcome.total_tokens_freed;
                        let messages_after = state.messages.len();
                        // In a retry context we know we overflowed the context
                        // window, so use max_turn_input_tokens as the floor for
                        // tokens_before when measured usage is unavailable.
                        let tokens_before = state
                            .last_measured_prompt_tokens
                            .unwrap_or(state.max_turn_input_tokens);
                        let pressure = if state.max_turn_input_tokens == 0 {
                            0.0
                        } else {
                            (tokens_before as f64 / state.max_turn_input_tokens as f64).min(1.0)
                        };
                        state.context_compression_triggered = true;
                        state.step_recorder.record_compaction_with_kind(
                            &result.tier.to_string(),
                            result.messages_removed.min(u32::MAX as usize) as u32,
                            tokens_freed,
                            pressure,
                        );
                        let event = CompactionEvent::new(
                            result.tier,
                            pressure,
                            tokens_freed,
                            tokens_before,
                            state.max_turn_input_tokens,
                            result.messages_removed,
                            messages_after,
                            result.layer_descriptions.clone(),
                        );
                        host.on_compaction(event);

                        // Emit structured compaction telemetry for observability.
                        if let Some(sid) = state.current_session_id.as_deref() {
                            let budget_likely_satisfied = result.budget_likely_satisfied;
                            let layers: Vec<(String, u64)> = result
                                .pipeline_outcome
                                .layer_results
                                .iter()
                                .map(|(name, cr)| (name.clone(), cr.estimated_tokens_freed))
                                .collect();
                            let evt =
                                astra_services::session_journal::JournalEvent::compaction_retry(
                                    Some(sid),
                                    session_turn_number(state),
                                    &tier_label,
                                    tokens_freed,
                                    budget_likely_satisfied,
                                    state.consecutive_context_window_errors,
                                    layers,
                                    state.consecutive_context_window_errors,
                                )
                                .with_agentic_step(Some(current_agentic_step(state)));
                            // `JournalWriter::append` auto-prepends
                            // `SessionStart` under the same file lock;
                            // see `prepend_session_start_if_needed`.
                            let writer = match state.owner_user_id.as_deref() {
                                Some(user_id) => {
                                    astra_services::session_journal::JournalWriter::for_user(
                                        user_id, sid,
                                    )
                                }
                                None => astra_services::session_journal::JournalWriter::new(sid),
                            };
                            if let Ok(writer) = writer {
                                if let Err(error) = writer.append(&evt) {
                                    tracing::warn!(
                                        session_id = sid,
                                        error = %error,
                                        "failed to append compaction retry event to session journal"
                                    );
                                }
                            }
                        }

                        try_write_heavy_checkpoint(state);
                        return Ok(TurnExecutionControl::ContinueLoop);
                    }
                    super::super::compaction_replay::CompactionReplayOutcome::Futile => {
                        // No candidate reduced the protected request. Preserve
                        // progress and report actionable context pressure;
                        // historical counters cannot disable later recovery.
                    }
                }
            }
            // If we reach here with a context overflow that couldn't be
            // compacted (or retries exhausted), record a structured
            // interruption so the session can resume from checkpoint.
            if is_context_overflow {
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::ContextOverflow,
                    ResumeAction::RequiresIntervention { description: "Automatic context reduction could not fit the protected conversation. Choose a larger-context authorized model or reduce the request before retrying; progress is preserved.".into() },
                    interruption_state_summary(
                        state,
                        Some(format!("Context overflow after compaction: {}", e.message)),
                    ),
                ));
            }

            // Fatal responses are partial observations, never successful
            // completion. Preserve this response's bytes before returning the
            // error; normal finalization does not run on the fatal path.
            preserve_failed_provider_text(
                state,
                &turn_result.accum.full_text,
                turn_result.accum.model_item_id.clone(),
            );

            // Catch-all: map ErrorKind to InterruptionRecord so the checkpoint
            // always carries resume guidance. Existing specific records (rate
            // limit, context overflow) take priority — only fill when still empty.
            if state.interruption.is_none() {
                if let Some((kind, action)) =
                    astra_turn_core::interruption::interruption_from_error_kind(e.kind)
                {
                    state.interruption = Some(InterruptionRecord::new(
                        kind,
                        action,
                        interruption_state_summary(state, Some(e.message.clone())),
                    ));
                }
            }

            // Even error kinds without a resumable interruption mapping may
            // have delivered a partial answer before this checkpoint.
            super::finalization::bound_current_terminal_message(state);
            finalize_turn_trace(state).await;
            try_write_heavy_checkpoint(state);
            return Err(e);
        }
        AgenticIngestIterationControl::BreakLoop => {
            // An authoritative interruption is a terminal boundary for this
            // turn.  Do not let user-intent polling or completion obligations
            // reopen another provider call after the host has already
            // preserved a partial result (for example, an exhausted output
            // cap, transport interruption, or token budget).  Those guards
            // are valuable while the turn is healthy, but once a typed
            // interruption exists the correct user experience is to render
            // the resumable partial result and let the next turn continue.
            if state.interruption.is_some() {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("interrupted"),
                );
                observe_turn_end_without_tools(state, prep.turn_start_time, turn_result.ttft_ms);
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            // Child completion is a producer fact, never assistant prose.
            // Fence it before committing final user-intent/Work settlement.
            if host
                .direct_child_completion_owner(state)
                .is_some_and(|owner| owner.has_pending_direct_children())
            {
                // Close provider timing before waiting. The wait is its own
                // Explain interval, never part of model inference time.
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("direct_child_completion_wait"),
                );
                state.step_recorder.end_turn(false);
                finalize_turn_trace(state).await;
                return Ok(TurnExecutionControl::WaitForInput);
            }

            // Observed guidance still continues through the shared input owner.
            // A later arrival is returned by the atomic Run terminal transaction
            // so accepting it never creates a second closing/reopening protocol.
            if inject_polled_user_intents_before_settlement(host, state).await? {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("runtime_input_arrived_before_settlement"),
                );
                state.step_recorder.end_turn(false);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            // Child results, replies and newly accepted guidance must reach
            // the model before spending its one final-answer recovery. A blank
            // response while a child is running is an input wait, not a reason
            // to hide coordination tools or consume the synthesis retry.
            // A successful provider response with neither tool calls nor text
            // is not a completed user turn while budget remains. Retry once in
            // an explicitly text-only settlement mode. This handles transient
            // empty provider responses inside the runtime instead of exposing
            // an internal `empty_completion` reason and asking the user to
            // manually drive a continuation.
            if should_retry_textless_response(state, &turn_result) {
                begin_textless_response_retry(state);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("textless_response_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            // Canonical Work owns completion authorization independently of
            // Run presentation. Re-read it so a correction or successor cannot
            // borrow process-local presentation as completion authority.
            let committed_work_synthesis_authorized = if committed_work_synthesis_candidate {
                let authorized = match host.committed_work_synthesis_authorized(state).await {
                    Ok(authorized) => authorized,
                    Err(error) => {
                        state.messages.truncate(transcript_append_start);
                        state.final_text_model_item_id = None;
                        state.final_text.clear();
                        state.hooks.completion_settlement.latest_provider_text = None;
                        state.hooks.completion_settlement.deferred_candidate_text = None;
                        state.interruption = Some(InterruptionRecord::new(
                            InterruptionKind::Interrupted,
                            ResumeAction::ContinueImmediately,
                            interruption_state_summary(
                                state,
                                Some(format!(
                                    "canonical Work final-synthesis state was unavailable: {error}"
                                )),
                            ),
                        ));
                        record_early_exit_llm_round(
                            state,
                            &turn_result,
                            prep.turn_start_time,
                            Some("canonical_work_revalidation_unavailable"),
                        );
                        observe_turn_end_without_tools(
                            state,
                            prep.turn_start_time,
                            turn_result.ttft_ms,
                        );
                        state.step_recorder.end_turn(false);
                        try_write_heavy_checkpoint(state);
                        return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
                    }
                };
                // The provider request and Work validation are not the Run
                // terminal commit. Re-authorize immediately after the durable
                // Work read so lease/generation loss cannot race final prose.
                match authorize_provider_boundary(state).await {
                    Ok(ProviderBoundaryGate::Authorized) => {}
                    Ok(ProviderBoundaryGate::Paused) => {
                        // The model response was produced before the durable
                        // pause won. Do not admit stale prose or execute a
                        // follow-up action; wait here for resume without
                        // treating pause as a user cancellation, then reopen
                        // the exact settlement fence before continuing.
                        state.messages.truncate(transcript_append_start);
                        state.final_text_model_item_id = None;
                        state.final_text.clear();
                        state.hooks.completion_settlement.latest_provider_text = None;
                        state.hooks.completion_settlement.deferred_candidate_text = None;
                        record_early_exit_llm_round(
                            state,
                            &turn_result,
                            prep.turn_start_time,
                            Some("paused_after_provider_response"),
                        );
                        state.step_recorder.end_turn(false);
                        try_write_heavy_checkpoint(state);
                        if wait_for_pause_clear_or_cancel(host, state).await? {
                            return Err(astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::Cancelled,
                                "run was cancelled while paused after a provider response",
                            ));
                        }
                        return Ok(TurnExecutionControl::ContinueLoop);
                    }
                    Err(error) => {
                        // The response was produced by an owner that no
                        // longer controls the Run. Keep provider accounting,
                        // but do not admit its prose into the durable/user-
                        // visible turn.
                        state.messages.truncate(transcript_append_start);
                        state.final_text_model_item_id = None;
                        state.final_text.clear();
                        state.hooks.completion_settlement.latest_provider_text = None;
                        state.hooks.completion_settlement.deferred_candidate_text = None;
                        return Err(error);
                    }
                }
                if !authorized {
                    // The canonical cut no longer matches this exact
                    // Run/generation/Work revision. This is an explicit
                    // revocation, not permission to reinterpret stale prose
                    // through generic workspace-completion heuristics.
                    state
                        .hooks
                        .completion_settlement
                        .preserve_final_synthesis_wire_surface = false;
                    state.messages.truncate(transcript_append_start);
                    state.final_text_model_item_id = None;
                    state.final_text.clear();
                    state.hooks.completion_settlement.latest_provider_text = None;
                    state.hooks.completion_settlement.deferred_candidate_text = None;
                    state.interruption = Some(InterruptionRecord::new(
                        InterruptionKind::ExecutionIncomplete,
                        ResumeAction::ContinueImmediately,
                        interruption_state_summary(
                            state,
                            Some(
                                "canonical Work final-synthesis authority was revoked by newer durable state"
                                    .to_string(),
                            ),
                        ),
                    ));
                    record_early_exit_llm_round(
                        state,
                        &turn_result,
                        prep.turn_start_time,
                        Some("canonical_work_synthesis_revoked"),
                    );
                    observe_turn_end_without_tools(
                        state,
                        prep.turn_start_time,
                        turn_result.ttft_ms,
                    );
                    state.step_recorder.end_turn(false);
                    try_write_heavy_checkpoint(state);
                    return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
                }
                authorized
            } else {
                false
            };
            let terminal_completion_disposition =
                terminal_completion_disposition(state, committed_work_synthesis_authorized);

            if enforce_typed_work_settlement_before_text_completion(state) {
                state.messages.truncate(transcript_append_start);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("work_settlement_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            match enforce_completion_action_window_before_text_completion(state) {
                CompletionActionBoundary::Retry => {
                    state.messages.truncate(transcript_append_start);
                    record_early_exit_llm_round(
                        state,
                        &turn_result,
                        prep.turn_start_time,
                        Some("completion_action_no_tool_retry"),
                    );
                    state.step_recorder.end_turn(false);
                    try_write_heavy_checkpoint(state);
                    return Ok(TurnExecutionControl::ContinueLoop);
                }
                CompletionActionBoundary::TerminalIncomplete => {
                    // The typed action window is terminal authority. Do not
                    // fall through to the older workspace/verification
                    // guards: doing so would reopen an unrestricted retry
                    // after the one allowed completion action was consumed.
                    record_early_exit_llm_round(
                        state,
                        &turn_result,
                        prep.turn_start_time,
                        Some("completion_action_incomplete"),
                    );
                    observe_turn_end_without_tools(
                        state,
                        prep.turn_start_time,
                        turn_result.ttft_ms,
                    );
                    state.step_recorder.end_turn(false);
                    try_write_heavy_checkpoint(state);
                    return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
                }
                CompletionActionBoundary::NoWindow | CompletionActionBoundary::Settled => {}
            }

            if reject_unsettled_execution_completion(state) {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("execution_custody_unsettled"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            if state.interruption.is_none()
                && enforce_explicit_verification_before_text_completion(state)
            {
                state.messages.truncate(transcript_append_start);
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some("explicit_verification_retry"),
                );
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::ContinueLoop);
            }

            let round_slice_incomplete = enforce_terminal_completion_disposition_before_success(
                state,
                terminal_completion_disposition,
            );

            // The bounded textless recovery is the only automatic response to
            // a provider turn that delivered neither text nor a tool. Once it
            // has been spent, settle through the same typed interruption path
            // as an exhausted provider recovery.
            if provider_turn_without_actionable_output(state, &turn_result) {
                record_exhausted_textless_completion(state);
            }

            // A completion guard may discover the exhausted side of its
            // bounded recovery window while this response is being settled
            // (workspace mutation, post-mutation observation, or an explicit
            // verification contract). Re-check here before the generic stop
            // path records a successful turn. The earlier check above covers
            // interruptions that arrived from transport/lifecycle; this one
            // covers obligations that became terminal during this boundary.
            if state.interruption.is_some() {
                record_early_exit_llm_round(
                    state,
                    &turn_result,
                    prep.turn_start_time,
                    Some(if round_slice_incomplete {
                        "round_slice_without_typed_completion"
                    } else {
                        "completion_contract_incomplete"
                    }),
                );
                observe_turn_end_without_tools(state, prep.turn_start_time, turn_result.ttft_ms);
                state.step_recorder.end_turn(false);
                try_write_heavy_checkpoint(state);
                return Ok(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
            }

            // A model response cannot settle producer-owned asynchronous
            // work. Reconcile canonical producer truth before accepting the
            // turn, but keep runtime state out of assistant-authored content.
            // UI and continuation decisions consume the typed observation;
            // transcript text remains exactly what the model produced.
            reconcile_unsettled_work_status(state);

            if committed_work_synthesis_candidate {
                // Exactly-once consumption occurs only after every typed
                // completion, verification, outcome, fanout, and process
                // barrier above accepted the response.
                state
                    .hooks
                    .completion_settlement
                    .preserve_final_synthesis_wire_surface = false;
            }

            // Record the LLM round even for text-only responses (no tool calls).
            // Without this, simple Q&A turns have llm_rounds=0 in the
            // journal despite the LLM being called.
            record_early_exit_llm_round(state, &turn_result, prep.turn_start_time, Some("stop"));
            state.step_recorder.end_turn(true);

            observe_turn_end_without_tools(state, prep.turn_start_time, turn_result.ttft_ms);

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

    // Circuit-breaker pattern signals are advisory-only. Continuing to use
    // tools after such a signal is not a protocol violation, so there is no
    // "ignored correction" phase and no synthetic abort here.
    emit_subrun_text_preview(host, state, prep.quiet);
    if let Some(control) = handle_token_budget(host, state, prep, &turn_result).await {
        return Ok(control);
    }
    record_tool_selection(state, &turn_result, turn_index);

    Ok(TurnExecutionControl::Proceed(Box::new(
        TurnExecutionPhase {
            llm_wall_start,
            turn_result,
        },
    )))
}

/// Preserve the latest mixed provider response before any bounded text-only
/// boundary can short-circuit the tool phase. This covers round-slice/token
/// wrap-ups, typed completion actions, and edge tool rounds; finalization
/// labels the text as partial whenever interruption is typed, so it cannot
/// become an unqualified success claim.
/// Keep the newest provider-authored text separately from the bounded
/// settlement candidate. The latter intentionally keeps the first
/// substantive mixed response during a tool-call lockout; that is useful for
/// a bounded retry, but it must not become the stale partial handoff when a
/// later model response is the one that was actually interrupted.
pub(crate) fn capture_latest_provider_text(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
) {
    let text = &turn_result.accum.full_text;
    if !text.trim().is_empty() {
        state.hooks.completion_settlement.latest_provider_text = Some(text.to_string());
        state
            .hooks
            .completion_settlement
            .latest_provider_text_model_item_id = turn_result
            .accum
            .model_item_id
            .clone()
            .or_else(|| state.current_model_item_id.clone());
    }
}

pub(crate) fn capture_deferred_candidate_text(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
) {
    if !(state.budget_wrapup_injected || state.hooks.completion_settlement.text_only)
        || turn_result.accum.full_text.trim().is_empty()
    {
        return;
    }
    let has_tool_work =
        !turn_result.accum.tool_calls.is_empty() || turn_result.accum.has_tool_calls;
    let deferred = &mut state.hooks.completion_settlement.deferred_candidate_text;
    if has_tool_work && deferred.is_some() {
        // The first substantive mixed response is the most complete
        // provider-owned candidate. Later responses that still violate the
        // text-only boundary are not evidence of a better answer and must
        // not replace it.
        return;
    }
    // A text-only retry may replace an earlier mixed candidate.
    *deferred = Some(turn_result.accum.full_text.clone());
    state
        .hooks
        .completion_settlement
        .deferred_candidate_model_item_id = turn_result
        .accum
        .model_item_id
        .clone()
        .or_else(|| state.current_model_item_id.clone());
}

fn collapse_batched_observation_fanout(tool_calls: &mut Vec<serde_json::Value>) -> usize {
    let mut remove = std::collections::HashSet::new();
    for tool_name in ["introspect", "reflect"] {
        let candidates = tool_calls
            .iter()
            .enumerate()
            .filter_map(|(index, call)| {
                let function = call.get("function")?;
                (function.get("name").and_then(serde_json::Value::as_str) == Some(tool_name))
                    .then_some((index, observation_call_args(function)))
            })
            .filter(|(_, args)| {
                tool_name != "introspect"
                    || (!args.contains_key("artifact") && !args.contains_key("explain"))
            })
            .collect::<Vec<_>>();
        if candidates.len() < 2 {
            continue;
        }
        let keep = candidates
            .iter()
            .find(|(_, args)| {
                args.get("facet").and_then(serde_json::Value::as_str) == Some("overview")
            })
            .map_or(candidates[0].0, |(index, _)| *index);
        for (index, _) in &candidates {
            if *index != keep {
                remove.insert(*index);
            }
        }
        canonicalize_composite_observation_args(&mut tool_calls[keep], tool_name);
    }
    let collapsed = remove.len();
    if collapsed > 0 {
        let mut index = 0usize;
        tool_calls.retain(|_| {
            let keep = !remove.contains(&index);
            index += 1;
            keep
        });
    }
    collapsed
}

fn observation_call_args(
    function: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    match function.get("arguments") {
        Some(serde_json::Value::Object(args)) => args.clone(),
        Some(serde_json::Value::String(args)) => serde_json::from_str(args).unwrap_or_default(),
        _ => serde_json::Map::new(),
    }
}

fn canonicalize_composite_observation_args(call: &mut serde_json::Value, tool_name: &str) {
    let Some(function) = call.get_mut("function") else {
        return;
    };
    let mut args = observation_call_args(function);
    args.insert("facet".to_string(), serde_json::json!("overview"));
    args.insert("depth".to_string(), serde_json::json!("diagnostic"));
    if tool_name == "reflect" {
        args.insert("topic".to_string(), serde_json::json!("overview"));
        args.insert("horizon".to_string(), serde_json::json!("session"));
    } else {
        args.insert("horizon".to_string(), serde_json::json!("recent"));
    }
    function["arguments"] = serde_json::Value::String(
        serde_json::to_string(&args).expect("observation arguments must serialize"),
    );
}

fn prompt_cache_identity_from_manifest(
    manifest: Option<&serde_json::Value>,
) -> Option<astra_turn_types::PromptCacheIdentityV1> {
    manifest
        .and_then(|trace| trace.pointer("/wire/fingerprint/prompt_cache_identity"))
        .and_then(|identity| serde_json::from_value(identity.clone()).ok())
}

fn prompt_cache_eligible_tokens_from_manifest(manifest: Option<&serde_json::Value>) -> Option<u64> {
    manifest
        .and_then(|trace| trace.pointer("/wire/cache_estimate/eligible_tokens"))
        .and_then(serde_json::Value::as_u64)
}

/// A bound executor receipt that is still attached to the live invocation.
/// This admits weaker foreground-process-group provenance for the current
/// completion chain without promoting it to durable cache, renewal, or resume
/// authority. Restored records have no `runtime_args_full` by contract.
pub(super) fn record_has_live_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record.runtime_args_full.is_some()
        && record_has_bound_workspace_receipt(record)
            .is_some_and(astra_tools::workspace_observation::is_changed_receipt)
}

fn record_has_bound_workspace_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<&serde_json::Value> {
    (record.name == "bash"
        && record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Executed
        && record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref()
            == Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE))
    .then_some(record.workspace_mutation_receipt.as_ref())
    .flatten()
}

pub(crate) fn record_has_weak_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    record_has_bound_workspace_receipt(record)
        .is_some_and(astra_tools::workspace_observation::is_weak_changed_receipt)
}

pub(super) fn record_has_trusted_partial_workspace_mutation_receipt(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    matches!(record.name.as_str(), "str_replace" | "lsp")
        && record.was_executed()
        && !record.ok
        && record.workspace_mutation_partial == Some(true)
        && record
            .workspace_mutation_partial_paths
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
        && record.workspace_mutation_receipt.as_ref().is_some_and(
            astra_tools::workspace_observation::is_typed_partial_workspace_mutation_receipt,
        )
}

/// Apply quarantine at every tool-record boundary, regardless of whether the
/// record came from an LLM ingest frame or the local/server tool phase.  Weak
/// process ownership and partial multi-file commits are both transport facts,
/// never completion evidence; the first transition is checkpointed immediately
/// so a crash cannot resurrect a clean observation epoch.
pub(crate) fn apply_workspace_observation_quarantine_transition(
    state: &mut AgenticLoopState,
    records: &[astra_services::session_journal::ToolCallRecord],
) -> bool {
    if state.stall.workspace_observation_quarantine.is_some() {
        return false;
    }
    let Some(quarantine) = workspace_observation_quarantine_from_records(records) else {
        return false;
    };
    state.stall.workspace_observation_quarantine = Some(quarantine);
    try_write_heavy_checkpoint(state);
    true
}

/// Derive transport quarantine without borrowing an execution loop.
pub fn workspace_observation_quarantine_from_records(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1> {
    let record = records.iter().find(|record| {
        record_has_weak_workspace_mutation_receipt(record)
            || record_has_trusted_partial_workspace_mutation_receipt(record)
    })?;
    let source_tool_call_id = record.tool_call_id.clone();
    let quarantine = if record_has_trusted_partial_workspace_mutation_receipt(record) {
        astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::partial_workspace_mutation(
            source_tool_call_id,
        )
    } else {
        astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(
            source_tool_call_id,
        )
    };
    Some(quarantine)
}

/// Only explicitly declared checks create a runtime completion obligation.
/// Tool results remain evidence for the model and canonical Work owner; the
/// runtime does not infer a project-validation contract from shell syntax.
pub(crate) fn pending_completion_action(
    state: &AgenticLoopState,
) -> Result<Option<CompletionAction>, VerificationRecoveryError> {
    let missing = missing_explicit_verification_hooks(state)?;
    if workspace_observation_is_quarantined(state) {
        return Ok(None);
    }
    Ok(missing
        .filter(|labels| !labels.is_empty())
        .map(|missing_labels| CompletionAction::ExplicitVerification { missing_labels }))
}

pub(crate) fn completion_action_hint(action: &CompletionAction) -> serde_json::Value {
    let CompletionAction::ExplicitVerification { missing_labels } = action;
    serde_json::json!({
        "hint_version": "completion_action_hint.v1",
        "reason_code": "explicit_verification_missing",
        "latest_known_stable_target": null,
        "accepted_action_family": "declared_verification",
        "accepted_action_shapes": [],
        "missing_labels": missing_labels,
        "authority": "typed_completion_action_window",
    })
}

fn completion_action_mismatch_instruction(correction_available: bool) -> &'static str {
    if correction_available {
        "This request did not match a declared check and was not executed. Correct it with one matching command."
    } else {
        "Only commands matching the remaining declared checks may execute at this boundary."
    }
}

pub(crate) fn completion_action_match_label(
    state: &AgenticLoopState,
    action: &CompletionAction,
    call: &serde_json::Value,
) -> Option<String> {
    let name = astra_turn_core::tool::args::shape::tool_call_name(call)?;
    let args = astra_turn_core::tool::args::shape::parse_tool_call_arguments(call).ok();
    let CompletionAction::ExplicitVerification { missing_labels } = action;
    state
        .hooks
        .stop_hooks
        .iter()
        .find(|hook| {
            hook.authoritative
                && missing_labels.iter().any(|label| label == &hook.label)
                && tool_call_verifies_explicit_hook(
                    name,
                    args.as_ref(),
                    hook,
                    state.hooks.workspace_root_hint.as_deref(),
                )
        })
        .map(|hook| hook.label.clone())
}

/// A single provider boundary may carry all missing independent verifiers,
/// but it must not skip an explicit dependency layer.  Dependent hooks stay
/// on the ordinary bounded verification path, where the first receipt can be
/// observed before the next hook is requested.
pub(crate) fn completion_action_window_is_batchable(
    state: &AgenticLoopState,
    action: &CompletionAction,
) -> bool {
    let CompletionAction::ExplicitVerification { missing_labels } = action;
    state.hooks.stop_hooks.iter().all(|hook| {
        !hook.authoritative
            || !missing_labels.iter().any(|label| label == &hook.label)
            || hook
                .depends_on
                .iter()
                .all(|dependency| !missing_labels.iter().any(|label| label == dependency))
    })
}

/// Apply the single typed completion-action allowance after ordinary tool
/// admission has run.  The helper is deliberately stateful and shared by the
/// server and local/edge paths: any provider tool request consumes the one
/// boundary, while only an actually admitted matching call receives execute
/// authority.  A rejected matching call is therefore evidence of a failed
/// attempt, never a reason to reopen the window.
pub(crate) fn apply_completion_action_admission(
    state: &mut AgenticLoopState,
    mut admission: ToolCallAdmission,
    raw_tool_calls: &[serde_json::Value],
) -> ToolCallAdmission {
    if admission.completion_action_applied {
        return admission;
    }
    admission.completion_action_applied = true;
    let Some(action) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .filter(|window| !window.consumed)
        .map(|window| window.action.clone())
    else {
        return admission;
    };

    if raw_tool_calls.is_empty() {
        return admission;
    }

    // Completion obligations apply to logical operations, not their transport
    // carrier. Include rejected requests so a denied matching action consumes
    // its boundary without acquiring execution authority or a mismatch retry.
    let requested_matching_action = admission
        .admitted
        .iter()
        .chain(
            admission
                .rejected
                .iter()
                .map(|rejected| &rejected.invocation),
        )
        .any(|call| {
            completion_action_match_label(state, &action, call.logical_target_call()).is_some()
        });
    let correction_available = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_ref()
        .is_some_and(|window| window.mismatch_corrections_remaining > 0);
    let mut retained = Vec::with_capacity(admission.admitted.len());
    let mut matched_labels = Vec::new();
    for call in admission.admitted.drain(..) {
        let match_label = completion_action_match_label(state, &action, call.logical_target_call());
        let matches = {
            match_label.as_ref().is_some_and(|label| {
                if matched_labels.iter().any(|seen| seen == label) {
                    false
                } else {
                    matched_labels.push(label.clone());
                    true
                }
            })
        };
        if matches {
            retained.push(call);
            continue;
        }
        admission.rejected.push(RejectedToolCall {
            invocation: call,
            result: serde_json::json!({
                "status": "rejected",
                "error_kind": "completion_action_mismatch",
                "retryable": !requested_matching_action && correction_available,
                "allowed_action": action.clone(),
                "action_hint": completion_action_hint(&action),
                "error": completion_action_mismatch_instruction(!requested_matching_action && correction_available),
            })
            .to_string(),
        });
    }
    let matched = !retained.is_empty();
    admission.admitted = retained;
    if !requested_matching_action && correction_available {
        let action_hint = completion_action_hint(&action);
        let mismatch_instruction = completion_action_mismatch_instruction(true);
        if let Some(window) = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_mut()
        {
            window.mismatch_corrections_remaining =
                window.mismatch_corrections_remaining.saturating_sub(1);
        }
        // Replace only the provider boundary spent on the non-executed
        // mismatch. The original action and closing settlement boundaries
        // remain bounded; a second mismatch is terminal.

        state.push_volatile_payload(
            super::host::VolatileKind::FinalAnswerSettlement,
            serde_json::json!({
                "schema": "completion_settlement.v2",
                "signal": "completion_action_mismatch_retry",
                "allowed_action": action.clone(),
                "attempts_remaining": 1,
                "mismatch_corrections_remaining": 0,
                "action_hint": action_hint,
                "execution_authority": "one_matching_action",
                "instruction": mismatch_instruction,
                "authority": "typed_completion_action_window",
            }),
        );
        return admission;
    }
    if let Some(window) = state
        .hooks
        .completion_settlement
        .completion_action_window
        .as_mut()
    {
        window.consumed = true;
        window.attempts_remaining = 0;
        window.matched = matched;
    }
    // Admission runs before execution. Do not make the current, valid action
    // look like a text-only wrap-up violation in the common tool phase. The
    // post-tool reconciliation above owns the next-boundary transition after
    // the authoritative outcome has entered the ledger.
    admission
}

/// Third-tier observation for the parallel-batching layer. The prompt-side soft
/// nudge fires when the trailing single-tool round streak hits
/// `PARALLEL_BATCHING_NUDGE_THRESHOLD` (=6). If the model ignores the nudge
/// and produces yet another single-tool round, the streak crosses the
/// resolved `parallel_batching_force_streak` threshold (default 8, per-model
/// overrides via `ModelPolicyProfile`) and we emit advisory evidence in the
/// typed runtime lane.
pub(crate) const PARALLEL_BATCHING_FORCE_MARKER: &str = "## ⤴ Parallel Batching Observation";

/// Trailing single-tool-round streak length at which the soft prompt nudge
/// (=6) escalates into typed advisory evidence.
/// Default for the threshold; the actual value used at runtime flows through
/// `ToolPolicyConfig::effective_parallel_batching_force_streak` (and
/// per-model overrides via `ModelPolicyProfile`).
/// Must match `effective_parallel_batching_force_streak`'s zero-default.
#[cfg(test)]
pub(crate) const PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD: usize =
    astra_config::runtime_config::DEFAULT_PARALLEL_BATCHING_FORCE_STREAK as usize;

pub(crate) fn should_emit_parallel_batching_advisory(
    state: &AgenticLoopState,
    threshold: usize,
) -> bool {
    if state.stall.parallel_batching_advisory_emitted {
        return false;
    }
    // One advisory per turn: avoid stacking redundant behavior evidence.
    if state.stall.any_advisory_emitted() {
        return false;
    }
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    streak >= threshold
}

pub(crate) fn parallel_batching_advisory_message(streak: usize, original_query: &str) -> String {
    format!(
        "{PARALLEL_BATCHING_FORCE_MARKER}\n\
         Observation: the last {streak} rounds each ran exactly one tool. When \
         calls are independent, that pattern may add avoidable latency, token use, \
         and round-budget pressure. Possible next actions include answering from \
         the evidence already gathered or batching independent calls. A further \
         single-tool round remains appropriate when its input genuinely depends on \
         the prior result.\n\n\
         Original user query: {original_query}"
    )
}

/// Build a `RoundSignal` from the current loop state for the circuit breaker.
/// Uses the latest `turn_sigs` entry and checks `tool_call_records` for mutations.
fn build_circuit_breaker_signal(
    state: &AgenticLoopState,
) -> astra_turn_core::loop_circuit_breaker::RoundSignal {
    use astra_turn_core::loop_circuit_breaker::RoundSignal;

    let tool_signatures = state.stall.turn_sigs.last().cloned().unwrap_or_default();
    let tool_count = tool_signatures.len();
    if state.llm_rounds_completed == 0 {
        return RoundSignal {
            tool_signatures,
            produced_mutation: false,
            tool_count,
        };
    }

    // Check only the most recently completed round. The previous implementation
    // scanned the last `max_tools_per_turn` records, so a single mutation could
    // mask many later read-only rounds and delay stall detection.
    let latest_round = state.llm_rounds_completed - 1;
    let latest_round_records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.round == Some(latest_round))
        .collect();
    let produced_mutation = if !latest_round_records.is_empty() {
        latest_round_records
            .iter()
            .any(|record| tool_record_is_workspace_mutation(record))
    } else {
        // Legacy records may not carry round metadata; fall back to the old
        // bounded scan only when the batch is fully legacy. Partial round
        // metadata is treated as authoritative for per-round classification.
        state
            .stall
            .tool_call_records
            .iter()
            .rev()
            .take(state.max_tools_per_turn as usize)
            .any(tool_record_is_workspace_mutation)
    };
    RoundSignal {
        tool_signatures,
        produced_mutation,
        tool_count,
    }
}

pub(crate) const CACHE_WASTE_MARKER: &str = "## ⤴ Repeated Cached Tool Calls Detected";
/// Default cache-waste midloop threshold. Used in tests; production code
/// reads from `ToolPolicyConfig::effective_cache_waste_midloop_threshold()`.
#[cfg(test)]
pub(crate) const CACHE_WASTE_MIDLOOP_THRESHOLD: usize = 3;

pub(crate) fn cache_wasteful_tools(
    state: &AgenticLoopState,
    threshold: usize,
) -> Vec<(String, usize)> {
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

pub(crate) fn should_emit_cache_waste_advisory(state: &AgenticLoopState, threshold: usize) -> bool {
    if state.stall.cache_waste_advisory_emitted {
        return false;
    }
    !cache_wasteful_tools(state, threshold).is_empty()
}

pub(crate) fn cache_waste_advisory_message(
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
         Observation: cached tool results were returned this turn [{tool_list}]. \
         Cache reuse does not establish current prompt coverage or lack of task progress.\n\n\
         Recommendation: reuse earlier output when it is available and sufficient. \
         Recover missing evidence when needed. A cached result does not prove a new execution.\n\n\
         Original user query: {original_query}"
    )
}

fn update_turn_trace_collector(state: &mut AgenticLoopState, turn_result: &HostTurnResult) {
    if let Some(ref collector) = state.telemetry.turn_trace_collector {
        if let Some(identity) = turn_result
            .accum
            .context_manifest_trace
            .as_ref()
            .and_then(|trace| trace.get("request_identity"))
            .cloned()
            .and_then(|identity| {
                serde_json::from_value::<
                    astra_turn_core::context_assembly_trace::ModelRequestTraceIdentity,
                >(identity)
                .ok()
            })
        {
            collector.record_request_identity(identity);
        }
        if let Some(spt) = turn_result.accum.system_prompt_tokens {
            collector.set_system_prompt_tokens(spt);
        }
        if let Some(ref breakdown_json) = turn_result.accum.system_prompt_breakdown
            && let Ok(breakdown) = serde_json::from_value::<
                astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
            >(breakdown_json.clone())
        {
            collector.record_system_prompt(breakdown);
        }
    }
}

pub(crate) fn observe_turn_end_without_tools(
    state: &mut AgenticLoopState,
    turn_start_time: Instant,
    ttft_ms: Option<u64>,
) {
    // ── Telemetry timing ─────────────────────────────────────────
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability::TurnTiming {
            turn: session_turn_number(state),
            context_assembly_ms: 0,
            ttft_ms: ttft_ms.unwrap_or(0),
            llm_total_ms: total_ms,
            tool_execution_ms: 0,
            total_ms,
        };
        let mut session_guard = astra_core::sync_poison::recover_rwlock_write(session);
        crate::observability::on_turn_end(hub, &mut session_guard, timing);
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
        observe_turn_end_without_tools(state, prep.turn_start_time, turn_result.ttft_ms);
        state.step_recorder.end_turn(false);
        return Some(TurnExecutionControl::Return(AgenticLoopOutcome::Completed));
    }

    // First attempt: compact-and-continue instead of hard-stopping.
    // Two-tier strategy:
    //   1. Aggressive compression pipeline (clear tool results)
    //   2. If still over: persist old messages as a session artifact, keep an introspect reference
    // Only if both fail do we inject the stop directive.
    {
        let rewrite_permit = state.begin_canonical_rewrite();
        let budget = super::super::TokenBudget {
            max_prompt_tokens: state.max_turn_input_tokens,
            last_measured_tokens: measured,
            current_round_index: Some(state.current_round_index),
            now_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        // A pre-turn summary already replaced completed history; running the
        // mechanical pipeline again would rewrite its retained rounds into
        // `[Cleared]` tombstones. Artifact spill below remains an independent
        // recovery path, so it stays available regardless of the tier.
        let (mut total_freed, mut total_messages_removed, mut layer_descriptions) =
            if state.compact_tier_applied < CompactionTier::CompactHistory {
                let pipeline = super::super::CompactionEngine::aggressive_pipeline();
                let outcome = pipeline.compress_if_needed(&mut state.messages, &budget);
                (
                    outcome.total_tokens_freed,
                    outcome
                        .layer_results
                        .iter()
                        .map(|(_, r)| r.messages_removed)
                        .sum::<usize>(),
                    outcome
                        .layer_results
                        .iter()
                        .map(|(name, r)| format!("{}: ~{} tokens", name, r.estimated_tokens_freed))
                        .collect::<Vec<String>>(),
                )
            } else {
                (0, 0, Vec::new())
            };

        // Commit an artifact-backed rewrite only after durable storage succeeds.
        if measured.saturating_sub(total_freed) > state.max_turn_input_tokens {
            if let Some((freed, removed)) = spill_old_messages_to_artifact(host, state).await {
                total_freed += freed;
                total_messages_removed += removed;
                layer_descriptions.push(format!("history_artifact: ~{freed} tokens"));
            }
        }

        if total_freed > 0 {
            state.finish_canonical_rewrite(rewrite_permit);
            let pressure = measured as f64 / state.max_turn_input_tokens as f64;
            state.context_compression_triggered = true;
            state.step_recorder.record_compaction_with_kind(
                "reactive_budget",
                total_messages_removed.min(u32::MAX as usize) as u32,
                total_freed,
                pressure,
            );
            let event = CompactionEvent::new(
                CompactionKind::ReactiveBudget,
                pressure,
                total_freed,
                measured,
                state.max_turn_input_tokens,
                total_messages_removed,
                state.messages.len(),
                layer_descriptions.clone(),
            );
            host.on_compaction(event);
            if let Some(ref mut sess) = state.pipeline_session {
                sess.recovery.record_reactive_compact();
                sess.stats.record_compaction(total_freed);
            }
            state
                .compaction_effectiveness
                .record_compaction(total_freed);
            // Session 0e37eb46 regression: after compaction shreds the
            // history, the model sees a much-shorter context and often
            // misreads it as "I've been interrupted" → produces a
            // progress summary instead of continuing. Inject a short
            // directive that reframes it as "the runtime compressed
            // your history; CONTINUE the task — do NOT summarize."
            //
            // Observable: stderr line above ("♻ Context pressure…")
            // shows the compaction fired; this push_volatile adds the
            // behavioural counter-directive to the volatile lane.
            // Recoverable: if a future user wants the old behaviour,
            // the volatile is singleton per turn and never persisted.
            // Correctable: `compaction_injects_resume_directive_on_volatile_lane`
            // test locks the contract.
            state.push_volatile(
                super::host::VolatileKind::CompactResume,
                super::super::budget_messaging::COMPACT_RESUME_DIRECTIVE,
            );
            try_write_heavy_checkpoint(state);
            return Some(TurnExecutionControl::ContinueLoop);
        }
    }

    // Compaction made no progress — inject the stop directive.
    state.budget_wrapup_injected = true;
    state.hooks.completion_settlement.wrapup_origin =
        Some(super::host::BudgetWrapupOrigin::TokenRail);
    if !prep.quiet {
        host.emit_headless_line(
            HeadlessStderrStyle::Yellow,
            format!(
                "⚠ Token budget reached ({measured}/{} tokens) — wrapping up.",
                state.max_turn_input_tokens,
            ),
        );
    }
    state.push_volatile(
        super::host::VolatileKind::BudgetAdvisory,
        super::super::budget_messaging::BUDGET_REACHED_ADVISORY,
    );
    try_write_heavy_checkpoint(state);
    Some(TurnExecutionControl::ContinueLoop)
}

fn record_tool_selection(
    state: &mut AgenticLoopState,
    turn_result: &HostTurnResult,
    turn_index: usize,
) {
    // Selection is recorded after admission resolves exact callback rows.
    if turn_result.accum.has_tool_calls || !turn_result.accum.tool_calls.is_empty() {
        return;
    }
    record_tool_surface_if_unset(state, Vec::new(), turn_index);
}

fn record_tool_surface_if_unset(
    state: &mut AgenticLoopState,
    selected_tools: Vec<String>,
    turn_index: usize,
) {
    let Some(collector) = state.telemetry.turn_trace_collector.as_ref() else {
        return;
    };
    if collector.has_tool_trace() {
        return;
    }
    let tools_available = selected_tools.len() as u32;
    collector.record_tool_surface(&selected_tools, &[], tools_available, turn_index as u64);
}

/// Record the exact callback rows once the async edge ledger has resolved.
pub(crate) fn record_edge_tool_selection(
    state: &mut AgenticLoopState,
    edge_tool_round: &[astra_turn_core::sse_stream_host::EdgeToolExecResult],
    turn_index: usize,
) {
    if edge_tool_round.is_empty() {
        return;
    }
    let mut selected_tools = Vec::new();
    astra_core::canonical_names::append_unique_names(
        &mut selected_tools,
        edge_tool_round.iter().map(|result| result.tool.as_str()),
    );
    if selected_tools.is_empty() {
        return;
    }
    record_tool_surface_if_unset(state, selected_tools, turn_index);
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use astra_services::runs::{
        AtomicRunGuidanceAdmission, AtomicRunGuidanceAdmissionRequest, InMemoryRunStateStore,
    };
    use astra_services::session_journal::{
        JournalEventType, ToolCallDisposition, ToolCallRecord, TurnEventBuffer,
    };
    use async_trait::async_trait;
    use serde_json::Value;
    use tokio::sync::Mutex;

    use super::*;
    use crate::observability::ObservabilityHub;
    use crate::server::run::engine::RunEngine;
    use crate::turn::agentic_loop::host::tests::{
        MockHost, edge_tool_result, make_edge_tool, make_state, server_tool_result, text_result,
    };
    use crate::turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, AppliedUserIntent, VolatileKind,
        run_agentic_loop_with_host,
    };
    use crate::turn::run_control::{RunStatusProvider, UserIntentPoll, UserIntentProvider};
    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;

    #[tokio::test]
    async fn child_finalization_keeps_interruption_out_of_assistant_output() {
        for original_kind in [
            None,
            Some(InterruptionKind::EmptyCompletion),
            Some(InterruptionKind::UserCancelled),
            Some(InterruptionKind::BudgetExhausted),
        ] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "parent-run",
                "child-agent",
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child-agent".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent-agent".into(),
                status: crate::orchestration::AgentStatus::Running {
                    activity: "working".into(),
                },
            });
            let mut host = MockHost::new(vec![]);
            host.direct_child_owner = Some(Arc::clone(&owner));
            let mut state = make_state();
            state.current_run_id = Some("parent-run".into());
            let partial = matches!(
                original_kind,
                Some(InterruptionKind::UserCancelled | InterruptionKind::BudgetExhausted)
            );
            if partial {
                let mut response = text_result("Partial findings before interruption", 1, 1, None);
                response.accum.model_item_id = Some("partial-provider-item".into());
                capture_latest_provider_text(&mut state, &response);
                state.final_text = response.accum.full_text.clone();
                state.final_text_model_item_id = response.accum.model_item_id.clone();
                state.final_text_streamed = true;
            }
            if let Some(kind) = original_kind {
                state.interruption = Some(InterruptionRecord::new(
                    kind,
                    exhausted_provider_recovery_action(&state),
                    interruption_state_summary(
                        &state,
                        Some("provider returned no usable output".into()),
                    ),
                ));
            }
            let original = state.interruption.clone();
            super::super::finalization::finalize_and_render(&mut host, &mut state).await;
            assert_eq!(
                state.final_text,
                if partial {
                    "Partial findings before interruption"
                } else {
                    ""
                }
            );
            assert_eq!(
                state.final_text_model_item_id.as_deref(),
                partial.then_some("partial-provider-item")
            );
            let interruption = state.interruption.as_ref().unwrap();
            assert_eq!(
                interruption.kind,
                original_kind.unwrap_or(InterruptionKind::ExecutionIncomplete)
            );
            if let Some(original) = original {
                assert_eq!(interruption, &original);
            }
            assert!(owner.has_pending_direct_children());
            assert_eq!(
                state
                    .messages
                    .iter()
                    .any(|message| message["role"] == "assistant"),
                partial
            );
        }
    }

    #[tokio::test]
    async fn semantic_child_message_can_yield_empty_response_until_child_completes() {
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};

        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-agent",
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Running {
                activity: "reviewing".into(),
            },
        });
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
        ));
        let parent_address = AgentAddress::new("parent-run", "parent-agent");
        let child_address = AgentAddress::new("child-run", "child-agent");
        let parent_mailbox = router.register(parent_address.clone(), None).await.unwrap();
        let _child_mailbox = router.register(child_address.clone(), None).await.unwrap();
        let wait_started = Arc::new(tokio::sync::Notify::new());
        let mut host = MockHost::new(vec![
            text_result("provisional", 10, 2, Some(10)),
            text_result("", 10, 0, Some(10)),
            text_result("synthesized", 10, 2, Some(10)),
        ]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        host.child_wait_started = Some(Arc::clone(&wait_started));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        state.messaging.mailbox = Some(parent_mailbox);
        let run = tokio::spawn(async move {
            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
            (host, state, outcome)
        });

        tokio::time::timeout(Duration::from_secs(2), wait_started.notified())
            .await
            .expect("parent must reach child barrier");
        router
            .send(AgentMessage::new(
                child_address.clone(),
                MessageTarget::Direct {
                    address: parent_address.clone(),
                },
                MessagePayload::Progress {
                    turn_index: 1,
                    tool_calls: 0,
                    status: "reviewing".into(),
                    detail: None,
                },
            ))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wait_started.notified())
                .await
                .is_err(),
            "transient progress must not spend another parent model round"
        );
        router
            .send(AgentMessage::new(
                child_address,
                MessageTarget::Direct {
                    address: parent_address,
                },
                MessagePayload::Text {
                    content: "please decide the review scope".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), wait_started.notified())
            .await
            .expect("semantic message must wake parent before child completes");
        assert!(
            owner
                .pending_direct_children()
                .iter()
                .any(|child| !child.status.is_terminal())
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "review complete".into(),
                finish_reason: None,
            },
        });
        let (host, _, outcome) = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("parent must settle after child completion")
            .unwrap();
        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
            "{outcome:?}"
        );
        assert!(
            serde_json::to_string(&host.executed_volatile[1])
                .unwrap()
                .contains("please decide the review scope")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn admitted_wait_observes_timeout_without_bypassing_child_results() {
        for reply_pending in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let context =
                crate::server::runtime_tool_executor::tests::test_agent_tool_context(dir.path());
            let owner = Arc::clone(&context.fanout_admission);
            let replies = Arc::clone(&context.question_correlations);
            let responder = astra_messaging::AgentAddress::new("child-run", "child-agent");
            if reply_pending {
                replies
                    .reserve("test-run", "question", responder.clone())
                    .unwrap();
            }
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child-agent".into(),
                run_id: "child-run".into(),
                parent_agent_id: context.agent_id.clone(),
                status: crate::orchestration::AgentStatus::Running {
                    activity: "working".into(),
                },
            });
            let mut executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
                dir.path().into(),
                "user".into(),
                "test-session".into(),
                None,
                None,
            );
            executor.set_invocation_ledger(
                crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new(None),
            );
            executor.set_agent_tool_context(context);
            let mut state = make_state();
            state.messaging.question_correlations = Arc::clone(&replies);
            state.runtime_tool_executor = Some(Arc::new(executor));
            state.current_run_id = Some("test-run".into());
            state.current_session_id = Some("test-session".into());
            state.owner_user_id = Some("user".into());
            state.canonical_turn_chain_id = Some("test-chain".into());
            let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let wait_started = Arc::new(tokio::sync::Notify::new());
            let mut turns = vec![
                server_tool_result(
                    vec![serde_json::json!({
                        "id":"wait-call", "type":"function", "function":{
                            "name":"agent", "arguments":serde_json::json!({"action":"wait", "timeout_ms":if reply_pending {1000} else {10000}}).to_string(),
                        },
                    })],
                    vec![],
                    10,
                    5,
                    None,
                ),
                text_result(
                    if reply_pending {
                        "child still running"
                    } else {
                        "42"
                    },
                    10,
                    5,
                    None,
                ),
            ];
            if reply_pending {
                turns.push(text_result("42", 10, 5, None));
            }
            let mut host = MockHost::new(turns).with_valid_tools(&["agent"]);
            host.provider_call_counter = Some(Arc::clone(&counter));
            host.child_wait_started = Some(Arc::clone(&wait_started));
            let task = tokio::spawn(async move {
                let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
                (host, state, outcome)
            });
            tokio::time::timeout(Duration::from_secs(2), wait_started.notified())
                .await
                .expect("admitted tool must park loop");
            let parked_calls = counter.load(Ordering::SeqCst);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), wait_started.notified())
                    .await
                    .is_err()
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                parked_calls,
                "parked execution must not call provider"
            );
            if reply_pending {
                tokio::time::advance(Duration::from_secs(1)).await;
                tokio::time::timeout(Duration::from_secs(2), wait_started.notified()).await
                    .expect("the observation timeout resumes the model, then child custody parks it again");
                assert_eq!(counter.load(Ordering::SeqCst), 2);
                assert!(
                    !task.is_finished(),
                    "an unanswered question does not replace the child-result fence"
                );
                assert!(owner.has_pending_direct_children());
                assert!(
                    !replies.pending("test-run").is_empty(),
                    "timeout cannot settle question correlation"
                );
            }
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child-agent".into(),
                run_id: "child-run".into(),
                parent_agent_id: "test-agent".into(),
                status: crate::orchestration::AgentStatus::Completed {
                    result: "42".into(),
                    finish_reason: None,
                },
            });
            let (host, state, outcome) = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("child wakes parent")
                .unwrap();
            assert_eq!(
                parked_calls,
                1,
                "tool records: {}",
                serde_json::to_string(&state.stall.tool_call_records).unwrap()
            );
            assert!(
                matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
                "{outcome:?}"
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                if reply_pending { 3 } else { 2 }
            );
            assert_eq!(state.final_text, "42");
            assert_eq!(
                !replies.pending("test-run").is_empty(),
                reply_pending,
                "completion does not require manufacturing an answer"
            );
            assert_eq!(
                astra_turn_core::evaluation::count_unresolved_tool_outcome_failures(
                    &state.stall.tool_call_records
                ),
                0,
                "accepted waiting must not introduce a child execution failure"
            );
            assert!(
                host.executed_volatile.last().unwrap().iter().any(|entry| {
                    entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                        && entry.payload["children"]
                            .as_array()
                            .is_some_and(|children| {
                                children.iter().any(|child| {
                                    child["run_id"] == "child-run" && child["result"] == "42"
                                })
                            })
                }),
                "continuation must consume the canonical child result"
            );
            assert!(
                state
                    .stall
                    .tool_call_records
                    .iter()
                    .any(|record| record.name == "agent" && record.ok && record.was_executed())
            );
        }
    }

    #[tokio::test]
    async fn failed_spawn_in_same_batch_does_not_delay_failure_observation() {
        let dir = tempfile::tempdir().unwrap();
        let context =
            crate::server::runtime_tool_executor::tests::test_agent_tool_context(dir.path());
        let mut executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            dir.path().into(),
            "user".into(),
            "test-session".into(),
            None,
            None,
        );
        executor.set_invocation_ledger(
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new(None),
        );
        executor.set_agent_tool_context(context);

        let mut state = make_state();
        state.runtime_tool_executor = Some(Arc::new(executor));
        state.current_run_id = Some("test-run".into());
        state.current_session_id = Some("test-session".into());
        state.owner_user_id = Some("user".into());
        state.canonical_turn_chain_id = Some("test-chain".into());
        let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut host = MockHost::new(vec![
            server_tool_result(
                vec![
                    serde_json::json!({
                        "id":"spawn-call", "type":"function", "function":{
                            "name":"agent",
                            "arguments":serde_json::json!({
                                "action":"spawn",
                                "description":"missing prompt"
                            }).to_string(),
                        },
                    }),
                    serde_json::json!({
                        "id":"wait-call", "type":"function", "function":{
                            "name":"agent",
                            "arguments":serde_json::json!({
                                "action":"wait",
                                "timeout_ms":30_000
                            }).to_string(),
                        },
                    }),
                ],
                vec![],
                10,
                5,
                None,
            ),
            text_result("failure observed", 10, 5, None),
        ])
        .with_valid_tools(&["agent"]);
        host.provider_call_counter = Some(Arc::clone(&provider_calls));

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            run_agentic_loop_with_host(&mut host, &mut state),
        )
        .await
        .expect("failed spawn must not wait for the optional timeout")
        .unwrap();

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        assert_eq!(state.final_text, "failure observed");
        assert!(state.stall.tool_call_records.iter().any(|record| {
            record.name == "agent"
                && record
                    .authoritative_args_full()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|args| {
                        args.get("action")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some("spawn")
                && !record.ok
        }));
    }

    #[tokio::test]
    async fn restored_observation_prioritizes_retained_child_result_without_a_second_wake() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-agent",
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "retained result".into(),
                finish_reason: None,
            },
        });

        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
        assert!(!owner.has_pending_direct_children());
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.kind == super::super::host::VolatileKind::BackgroundTaskNotification
        }));

        let checkpoint = super::super::host::OriginalLoopExecutionFacts::capture(&state)
            .expect("unobserved child result must be checkpointable");
        let restored: super::super::host::OriginalLoopExecutionFacts =
            serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        state.volatile_pending = restored.pending_context;

        let receipt = astra_tools::agent_tool_contract::AgentWaitReceipt {
            parent_run_id: "parent-run".into(),
            tool_call_id: "wait-call".into(),
            timeout_ms: 30_000,
        };
        let outcome = tokio::time::timeout(
            Duration::from_millis(100),
            await_runtime_activity(&mut host, &mut state, Some(&receipt)),
        )
        .await
        .expect("retained input must not wait for a lost wake")
        .unwrap();

        assert_eq!(outcome, RuntimeActivityOutcome::InputReady);
        assert!(host.executed_messages.is_empty());
    }

    #[tokio::test]
    async fn paused_child_retains_parent_execution_until_cancelled() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "paused-child",
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "paused-child".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Paused {
                reason: "user pause".into(),
            },
        });
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        let cancellation = Arc::new(tokio_util::sync::CancellationToken::new());
        state.cancellation.token = Some(Arc::clone(&cancellation));
        let task = tokio::spawn(async move {
            let result = await_runtime_activity(&mut host, &mut state, None).await;
            (host, result)
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !task.is_finished(),
            "child pause must not retire its executing parent"
        );
        assert!(owner.has_pending_direct_children());
        cancellation.cancel();
        let (host, result) = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind, astra_core::ErrorKind::Cancelled);
        assert!(host.executed_messages.is_empty());
    }

    #[tokio::test]
    async fn paused_dependency_retains_siblings_and_resumes_the_same_obligation() {
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};
        for observation in [
            None,
            Some(astra_tools::agent_tool_contract::AgentWaitReceipt {
                parent_run_id: "parent-run".into(),
                tool_call_id: "wait-call".into(),
                timeout_ms: 10_000,
            }),
        ] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "parent-run",
                "paused-child",
            );
            for (agent_id, status) in [
                (
                    "paused-child",
                    crate::orchestration::AgentStatus::Paused {
                        reason: "user pause".into(),
                    },
                ),
                (
                    "terminal-child",
                    crate::orchestration::AgentStatus::Completed {
                        result: "retained sibling result".into(),
                        finish_reason: None,
                    },
                ),
            ] {
                owner.set_direct_child_for_test(
                    crate::orchestration::spawner::DirectChildCompletion {
                        applied_user_intents: Vec::new(),
                        agent_id: agent_id.into(),
                        run_id: format!("{agent_id}-run"),
                        parent_agent_id: "parent-agent".into(),
                        status,
                    },
                );
            }
            let mut host = MockHost::new(vec![]);
            host.direct_child_owner = Some(Arc::clone(&owner));
            let mut state = make_state();
            state.current_run_id = Some("parent-run".into());
            let router = Arc::new(AgentMailboxRouter::new(
                Arc::new(InProcessTransport::new()),
                Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
            ));
            let address = AgentAddress::new("parent-run", "parent-agent");
            let sender = AgentAddress::new("peer-run", "peer-agent");
            state.messaging.mailbox = Some(router.register(address.clone(), None).await.unwrap());
            let _sender = router.register(sender.clone(), None).await.unwrap();
            let input_seen = Arc::new(tokio::sync::Notify::new());
            let observed = Arc::clone(&input_seen);
            let task = tokio::spawn(async move {
                assert_eq!(
                    await_runtime_activity(&mut host, &mut state, observation.as_ref())
                        .await
                        .unwrap(),
                    RuntimeActivityOutcome::InputReady
                );
                // The first input-ready result represents a provider boundary
                // that adopts the staged child receipt. Keep the settlement
                // evidence, but prove that an already observed receipt does
                // not turn the next wait into a busy loop.
                state
                    .lease_volatile_pending()
                    .expect("staged child result must be leasable");
                state.commit_volatile_attempt_lease();
                assert_eq!(
                    await_runtime_activity(&mut host, &mut state, observation.as_ref())
                        .await
                        .unwrap(),
                    RuntimeActivityOutcome::InputReady
                );
                state
                    .lease_volatile_pending()
                    .expect("mailbox and child context must be leasable");
                state.commit_volatile_attempt_lease();
                assert_eq!(host.communication_events.len(), 1);
                observed.notify_one();
                let result =
                    await_runtime_activity(&mut host, &mut state, observation.as_ref()).await;
                (host, state, result)
            });
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert!(!task.is_finished());
            assert!(owner.has_retained_direct_child_result("terminal-child"));
            assert_eq!(owner.pending_direct_children().len(), 1);
            router
                .send(AgentMessage::new(
                    sender,
                    MessageTarget::Direct { address },
                    MessagePayload::Text {
                        content: "The requirements changed.".into(),
                        summary: None,
                    },
                ))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), input_seen.notified())
                .await
                .unwrap();
            assert!(
                !task.is_finished(),
                "parent input must not discard the paused child"
            );
            for status in [
                crate::orchestration::AgentStatus::Running {
                    activity: "resumed".into(),
                },
                crate::orchestration::AgentStatus::Completed {
                    result: "resumed result".into(),
                    finish_reason: None,
                },
            ] {
                owner.set_direct_child_for_test(
                    crate::orchestration::spawner::DirectChildCompletion {
                        applied_user_intents: Vec::new(),
                        agent_id: "paused-child".into(),
                        run_id: "paused-child-run".into(),
                        parent_agent_id: "parent-agent".into(),
                        status,
                    },
                );
                tokio::task::yield_now().await;
                if owner.has_pending_direct_children() {
                    assert!(!task.is_finished(), "resume is not completion");
                }
            }
            let (host, state, result) = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.unwrap(), RuntimeActivityOutcome::InputReady);
            assert!(host.executed_messages.is_empty());
            assert!(!owner.has_pending_direct_children());
            assert!(owner.has_retained_direct_child_result("terminal-child"));
            assert!(owner.has_retained_direct_child_result("paused-child"));
            let payload = serde_json::to_string(&state.volatile_pending).unwrap();
            assert!(payload.contains("terminal-child") && payload.contains("resumed result"));
        }
    }

    #[tokio::test]
    async fn pause_during_timeout_readmission_reparks_without_provider_continuation() {
        for observation_timeout in [false, true] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "parent-run",
                "child",
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent-agent".into(),
                status: crate::orchestration::AgentStatus::Running {
                    activity: "working".into(),
                },
            });
            if !observation_timeout {
                owner.set_direct_child_for_test(
                    crate::orchestration::spawner::DirectChildCompletion {
                        applied_user_intents: Vec::new(),
                        agent_id: "sibling".into(),
                        run_id: "sibling-run".into(),
                        parent_agent_id: "parent-agent".into(),
                        status: crate::orchestration::AgentStatus::Completed {
                            result: "done".into(),
                            finish_reason: None,
                        },
                    },
                );
            }
            let entered = Arc::new(tokio::sync::Notify::new());
            let grant = Arc::new(tokio::sync::Notify::new());
            let mut host = MockHost::new(vec![]).with_execution_time_budget_remaining(
                astra_turn_types::ExecutionTimeRemaining {
                    work_remaining: Duration::from_millis(15),
                    total_remaining: Duration::from_secs(2),
                },
            );
            host.direct_child_owner = Some(Arc::clone(&owner));
            host.capacity_readmission_gate = Some((Arc::clone(&entered), Arc::clone(&grant)));
            let mut state = make_state();
            state.current_run_id = Some("parent-run".into());
            let cancellation = Arc::new(tokio_util::sync::CancellationToken::new());
            state.cancellation.token = Some(Arc::clone(&cancellation));
            let task = tokio::spawn(async move {
                let receipt = astra_tools::agent_tool_contract::AgentWaitReceipt {
                    parent_run_id: "parent-run".into(),
                    tool_call_id: "wait-call".into(),
                    timeout_ms: 1,
                };
                let mut result = await_runtime_activity(
                    &mut host,
                    &mut state,
                    observation_timeout.then_some(&receipt),
                )
                .await;
                if matches!(result, Ok(RuntimeActivityOutcome::InputReady)) {
                    // Model provider adoption of the sibling before waiting again.
                    state
                        .lease_volatile_pending()
                        .expect("staged sibling result must be leasable");
                    state.commit_volatile_attempt_lease();
                    result = await_runtime_activity(&mut host, &mut state, None).await;
                }
                (host, state, result)
            });
            tokio::time::timeout(Duration::from_secs(1), entered.notified())
                .await
                .unwrap();
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent-agent".into(),
                status: crate::orchestration::AgentStatus::Paused {
                    reason: "user pause".into(),
                },
            });
            // Crossing the soft cutoff must preserve the queued acquisition.
            tokio::time::sleep(Duration::from_millis(25)).await;
            grant.notify_one();
            tokio::time::sleep(Duration::from_millis(25)).await;
            if observation_timeout {
                let (host, state, result) = tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(result.unwrap(), RuntimeActivityOutcome::ObservationTimedOut);
                assert!(owner.has_pending_direct_children());
                assert!(host.executed_messages.is_empty());
                assert!(
                    state
                        .volatile_pending
                        .iter()
                        .any(|entry| entry.payload["agent_wait"] == "timed_out")
                );
                continue;
            }
            assert!(!task.is_finished());
            assert!(owner.has_pending_direct_children());
            cancellation.cancel();
            let (host, state, result) = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.unwrap_err().kind, astra_core::ErrorKind::Cancelled);
            assert!(host.execution_capacity_releases >= 2);
            assert!(host.executed_messages.is_empty());
            assert!(
                tokio::time::timeout(Duration::from_millis(2), entered.notified())
                    .await
                    .is_err()
            );
            assert!(
                state
                    .volatile_pending
                    .iter()
                    .all(|entry| entry.payload["agent_wait"] != "timed_out")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_observation_cutoff_preserves_readmission_and_started_input() {
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
        ));
        let spawner = Arc::new(crate::orchestration::DynamicAgentSpawner::new(router));
        let mut ctx = crate::orchestration::agent_tool::tests::test_spawn_context(spawner, None);
        ctx.run_id = "agent-run".into();
        ctx.fanout_admission = ctx.spawner.fanout_parent("agent-run");
        let output = crate::orchestration::agent_tool::handle_agent_tool(
            &serde_json::json!({"action":"wait", "timeout_ms":15_000, "_tool_call_id":"wait-call"}),
            Some(&ctx),
        )
        .await;
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["status"], "wait_admitted");
        let receipt: astra_tools::agent_tool_contract::AgentWaitReceipt =
            serde_json::from_value(output["wait_request"].clone()).unwrap();
        for (phase, work_seconds) in [
            ("capacity", 40),
            ("poll", 40),
            ("apply", 40),
            ("capacity", 10),
            ("apply", 10),
        ] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "agent-run",
                "child",
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: vec![],
                agent_id: "child".into(),
                run_id: "child-run".into(),
                parent_agent_id: "agent".into(),
                status: crate::orchestration::AgentStatus::Paused {
                    reason: "user pause".into(),
                },
            });
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let mut provider = StubRunControlProvider::new(vec![UserIntentPoll {
                next_cursor: 1,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "guidance".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 0,
                    input: serde_json::json!({"content":"Use the existing result."}),
                }],
                ..UserIntentPoll::default()
            }]);
            let mut host = MockHost::new(vec![]).with_execution_time_budget_remaining(
                astra_turn_types::ExecutionTimeRemaining {
                    work_remaining: Duration::from_secs(work_seconds),
                    total_remaining: Duration::from_secs(60),
                },
            );
            let gate = Some((entered.clone(), release.clone()));
            match phase {
                "capacity" => host.capacity_readmission_gate = gate,
                "poll" => provider.poll_gate = gate,
                "apply" => provider.apply_gate = gate,
                _ => unreachable!(),
            }
            let provider = Arc::new(provider);
            let (_wake_tx, wake_rx) = tokio::sync::watch::channel(0);
            host.direct_child_owner = Some(owner.clone());
            let mut state = make_state();
            state.current_run_id = Some("agent-run".into());
            state.current_session_id = Some("session".into());
            state.owner_user_id = Some("user".into());
            state.current_run_owner_generation = Some(3);
            state.run_control = Some(provider.clone());
            state.user_intents.bind_wake(Some(wake_rx));
            let receipt = receipt.clone();
            let task = tokio::spawn(async move {
                let result = await_runtime_activity(&mut host, &mut state, Some(&receipt)).await;
                (host, state, result)
            });
            tokio::time::timeout(Duration::from_secs(1), entered.notified())
                .await
                .expect("real input wake must reach the selected capacity/IO boundary");
            tokio::time::advance(Duration::from_secs(16)).await;
            tokio::task::yield_now().await;
            if work_seconds > 15 {
                assert!(
                    !task.is_finished(),
                    "{phase}: retain admitted ownership until capacity/input settles"
                );
            }
            release.notify_one();
            let (host, state, result) = task.await.unwrap();
            assert_eq!(
                result.unwrap(),
                if work_seconds < 15 {
                    RuntimeActivityOutcome::Incomplete
                } else if phase == "capacity" {
                    RuntimeActivityOutcome::ObservationTimedOut
                } else {
                    RuntimeActivityOutcome::InputReady
                },
                "{phase}"
            );
            assert!(owner.has_pending_direct_children(), "{phase}");
            if work_seconds > 15 {
                assert!(host.cancelled_agent_ids.is_empty(), "{phase}");
                assert!(
                    state.interruption.is_none(),
                    "{phase}: {:?}",
                    state.interruption
                );
            } else {
                assert!(
                    state.interruption.is_some(),
                    "{phase}: real work cutoff remains authoritative"
                );
            }
            assert_eq!(
                provider.poll_call_count().await,
                usize::from(phase != "capacity")
            );
            assert_eq!(
                state.user_intents.user_intent_cursor(),
                usize::from(phase != "capacity" && work_seconds > 15)
            );
        }
    }

    #[tokio::test]
    async fn paused_dependency_keeps_original_authority_across_observation_cutoff() {
        tokio::time::pause();
        for observe in [false, true] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "parent-run",
                "child",
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent-agent".into(),
                status: crate::orchestration::AgentStatus::Paused {
                    reason: "user pause".into(),
                },
            });
            let started = Arc::new(tokio::sync::Notify::new());
            let mut host = MockHost::new(vec![]).with_execution_time_budget_remaining(
                astra_turn_types::ExecutionTimeRemaining {
                    work_remaining: Duration::from_secs(15),
                    total_remaining: Duration::from_secs(45),
                },
            );
            host.direct_child_owner = Some(Arc::clone(&owner));
            host.child_wait_started = Some(Arc::clone(&started));
            let mut state = make_state();
            state.current_run_id = Some("parent-run".into());
            let task = tokio::spawn(async move {
                let receipt = astra_tools::agent_tool_contract::AgentWaitReceipt {
                    parent_run_id: "parent-run".into(),
                    tool_call_id: "wait-call".into(),
                    timeout_ms: 15_000,
                };
                let result =
                    await_runtime_activity(&mut host, &mut state, observe.then_some(&receipt))
                        .await;
                (host, state, result)
            });
            started.notified().await;
            tokio::time::advance(Duration::from_secs(16)).await;
            tokio::task::yield_now().await;
            if observe {
                let (host, state, result) = task.await.unwrap();
                assert_eq!(result.unwrap(), RuntimeActivityOutcome::ObservationTimedOut);
                assert!(owner.has_pending_direct_children());
                assert!(host.executed_messages.is_empty());
                assert!(state.interruption.is_none());
                continue;
            }
            assert!(
                !task.is_finished(),
                "soft cutoff must not retire live ownership"
            );
            assert!(owner.has_pending_direct_children());
            tokio::time::advance(Duration::from_secs(29)).await;
            let (host, state, result) = task.await.unwrap();
            assert_eq!(result.unwrap(), RuntimeActivityOutcome::Incomplete);
            assert!(host.executed_messages.is_empty());
            assert!(
                state.interruption.is_some(),
                "actual authority expiry must still settle"
            );
        }
    }

    #[tokio::test]
    async fn completion_wait_preserves_authorized_budget_beyond_observation_cap() {
        tokio::time::pause();
        for remaining in [Some(Duration::from_secs(900)), None] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "parent-run",
                "child-agent",
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child-agent".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent-agent".into(),
                status: crate::orchestration::AgentStatus::Running {
                    activity: "working".into(),
                },
            });
            let wait_started = Arc::new(tokio::sync::Notify::new());
            let mut host = MockHost::new(vec![]);
            host.direct_child_owner = Some(Arc::clone(&owner));
            host.child_wait_started = Some(Arc::clone(&wait_started));
            if let Some(remaining) = remaining {
                host = host.with_execution_time_budget_remaining(
                    astra_turn_types::ExecutionTimeRemaining {
                        work_remaining: remaining.saturating_sub(Duration::from_secs(30)),
                        total_remaining: remaining,
                    },
                );
            }
            let mut state = make_state();
            state.current_run_id = Some("parent-run".into());
            let task =
                tokio::spawn(
                    async move { await_runtime_activity(&mut host, &mut state, None).await },
                );
            wait_started.notified().await;
            tokio::time::advance(Duration::from_secs(302)).await;
            tokio::task::yield_now().await;
            assert!(
                !task.is_finished(),
                "observation cap must not cancel completion wait with budget {remaining:?}"
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child-agent".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent-agent".into(),
                status: crate::orchestration::AgentStatus::Completed {
                    result: "42".into(),
                    finish_reason: None,
                },
            });
            assert_eq!(
                task.await.unwrap().unwrap(),
                RuntimeActivityOutcome::InputReady,
                "budget {remaining:?}"
            );
        }
    }

    #[tokio::test]
    async fn textless_parent_waits_for_child_before_spending_final_answer_recovery() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-agent",
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Running {
                activity: "reading workspace".into(),
            },
        });
        let mut host = MockHost::new(vec![
            text_result("", 10, 0, Some(1)),
            text_result("", 10, 0, Some(1)),
            text_result("2024, from the child's workspace evidence.", 10, 8, Some(1)),
        ]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());

        let control = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
            .await
            .unwrap();

        assert!(
            matches!(control, TurnExecutionControl::WaitForInput),
            "an outstanding child owns the next input; final-answer recovery must wait"
        );
        assert!(
            !state.hooks.completion_settlement.text_only,
            "waiting for a child must not hide coordination tools needed to answer its questions"
        );
        assert_eq!(
            state.hooks.completion_settlement.textless_response_retries,
            0
        );
        assert!(owner.has_pending_direct_children());

        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "2024, from Cargo.toml [workspace.package].".into(),
                finish_reason: None,
            },
        });
        let control = execute_turn_and_ingest_phase(&mut host, &mut state, 1, prep(false))
            .await
            .unwrap();
        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert_eq!(
            state.hooks.completion_settlement.textless_response_retries,
            1
        );
        assert!(state.hooks.completion_settlement.text_only);
        assert!(!owner.has_pending_direct_children());
        assert!(host.executed_volatile[1].iter().any(|entry| {
            entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                && entry.payload["children"][0]["result"]
                    == "2024, from Cargo.toml [workspace.package]."
        }));

        let control = execute_turn_and_ingest_phase(&mut host, &mut state, 2, prep(false))
            .await
            .unwrap();
        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.text_only_turns, vec![false, false, true]);
        super::super::finalization::finalize_and_render(&mut host, &mut state).await;
        assert!(state.interruption.is_none());
        assert_eq!(
            state.final_text,
            "2024, from the child's workspace evidence."
        );
        assert_eq!(
            host.rendered_final_text,
            vec!["2024, from the child's workspace evidence."]
        );
    }

    #[tokio::test]
    async fn observation_wait_timeout_preserves_child_and_completion_candidate() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-agent",
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Running {
                activity: "working".into(),
            },
        });
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        state.final_text = "existing candidate".into();
        state.final_text_streamed = true;
        assert_eq!(
            await_runtime_activity(
                &mut host,
                &mut state,
                Some(&astra_tools::agent_tool_contract::AgentWaitReceipt {
                    parent_run_id: "parent-run".into(),
                    tool_call_id: "wait-call".into(),
                    timeout_ms: 1,
                })
            )
            .await
            .unwrap(),
            RuntimeActivityOutcome::ObservationTimedOut
        );
        assert!(owner.has_pending_direct_children());
        assert_eq!(state.final_text, "existing candidate");
        assert!(state.final_text_streamed);
        assert!(
            serde_json::to_string(&state.volatile_pending)
                .unwrap()
                .contains("timed_out")
        );
    }

    #[tokio::test]
    async fn accepted_user_guidance_wakes_shared_child_barrier() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-agent",
        );
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-agent".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent-agent".into(),
            status: crate::orchestration::AgentStatus::Running {
                activity: "reviewing".into(),
            },
        });
        let provider = Arc::new(StubRunControlProvider::new(vec![]));
        let (wake_tx, wake_rx) = tokio::sync::watch::channel(-1);
        let wait_started = Arc::new(tokio::sync::Notify::new());
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        host.child_wait_started = Some(Arc::clone(&wait_started));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        state.current_session_id = Some("parent-session".into());
        state.owner_user_id = Some("parent-user".into());
        state.run_control = Some(provider.clone());
        state.user_intents.wake = Some(wake_rx);
        let task = tokio::spawn(async move {
            let outcome = await_runtime_activity(&mut host, &mut state, None).await;
            (host, state, outcome)
        });
        tokio::time::timeout(Duration::from_secs(2), wait_started.notified())
            .await
            .expect("parent must reach child barrier");
        provider.polls.lock().await.push_back(UserIntentPoll {
            next_cursor: 1,
            snapshot_page_fact_count: 1,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "guidance-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 0,
                input: serde_json::json!({"content": "Use GLM for review."}),
            }],
            ..UserIntentPoll::default()
        });
        wake_tx.send(0).expect("active parent receiver");
        let (host, state, outcome) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("guidance must wake parent")
            .unwrap();
        assert_eq!(outcome.unwrap(), RuntimeActivityOutcome::InputReady);
        assert_eq!(host.user_intent_applied_indices, vec![0]);
        assert_eq!(state.message, "Use GLM for review.");
        assert_eq!(provider.poll_call_count().await, 1);
        assert!(owner.has_pending_direct_children());
    }

    #[tokio::test]
    async fn shutdown_handoff_preserves_input_poll_but_user_cancellation_interrupts_it() {
        use super::super::host::{LoopEntry, RuntimeInputWait, run_agentic_loop_impl};
        for shutdown in [false, true] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "agent-run",
                "child-agent",
            );
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: "child-agent".into(),
                run_id: "child-run".into(),
                parent_agent_id: "agent".into(),
                status: crate::orchestration::AgentStatus::Running {
                    activity: "working".into(),
                },
            });
            let poll_entered = Arc::new(tokio::sync::Notify::new());
            let poll_release = Arc::new(tokio::sync::Notify::new());
            let mut provider = StubRunControlProvider::new(vec![UserIntentPoll {
                next_cursor: 1,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "guidance-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 0,
                    input: serde_json::json!({"content": "Continue with the received guidance."}),
                }],
                ..UserIntentPoll::default()
            }]);
            if shutdown {
                provider.apply_gate = Some((poll_entered.clone(), poll_release.clone()));
            } else {
                provider.poll_gate = Some((poll_entered.clone(), poll_release.clone()));
            }
            let provider = Arc::new(provider);
            let (wake_tx, wake_rx) = tokio::sync::watch::channel(-1);
            let cancellation = Arc::new(tokio_util::sync::CancellationToken::new());
            let wait_started = Arc::new(tokio::sync::Notify::new());
            let mut host = MockHost::new(vec![]);
            let handoff = tokio_util::sync::CancellationToken::new();
            host.handoff_requested = Some(handoff.clone());
            host.handoff_accepted = true;
            host.direct_child_owner = Some(Arc::clone(&owner));
            host.child_wait_started = Some(Arc::clone(&wait_started));
            let mut state = make_state();
            state.current_run_id = Some("agent-run".into());
            state.current_session_id = Some("session".into());
            state.owner_user_id = Some("user".into());
            state.run_control = Some(provider.clone());
            state.current_run_owner_generation = Some(3);
            state.step_recorder.begin_turn(0);
            state.loop_entry = LoopEntry::InputWait {
                next_index: 1,
                harness_pause_recovery_count: 0,
                wait: RuntimeInputWait::Completion,
            };
            let budget = state.run_execution_budget_snapshot();
            state.user_intents.bind_wake(Some(wake_rx));
            state.cancellation.token = Some(cancellation.clone());
            let mut task = tokio::spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(1),
                    run_agentic_loop_impl(&mut host, &mut state),
                )
                .await;
                (host, state, result)
            });
            tokio::time::timeout(Duration::from_secs(2), wait_started.notified())
                .await
                .expect("wait started");
            wake_tx.send(0).expect("active receiver");
            tokio::time::timeout(Duration::from_secs(2), poll_entered.notified())
                .await
                .expect("authoritative input transaction started");
            if shutdown {
                handoff.cancel();
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut task)
                        .await
                        .is_err(),
                    "shutdown must not abandon an in-flight durable input transaction"
                );
                poll_release.notify_one();
            } else {
                cancellation.cancel();
            }
            let (host, state, result) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the owner must observe shutdown after input settlement or explicit cancellation")
            .unwrap();
            if shutdown {
                assert!(
                    result.is_err(),
                    "settled input must freeze in the shared consumer"
                );
                assert_eq!(host.handoff_snapshots.len(), 1);
                assert_eq!(provider.released.lock().await.as_slice(), &[0]);
                assert_eq!(host.user_intent_applied_indices, vec![0]);
                assert_eq!(state.message, "Continue with the received guidance.");
            } else {
                assert_eq!(
                    result.unwrap().unwrap_err().kind,
                    astra_core::ErrorKind::Cancelled
                );
                assert!(host.handoff_snapshots.is_empty());
            }
            assert!(host.executed_messages.is_empty());
            assert_eq!(state.run_execution_budget_snapshot(), budget);
            assert!(owner.has_pending_direct_children());
        }
    }

    #[tokio::test]
    async fn action_boundary_handles_mailbox_identically_for_root_and_nested_agent() {
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};

        for (run_id, agent_id, depth) in [
            ("root-run", "root-agent", 0),
            ("nested-run", "nested-agent", 2),
        ] {
            let router = Arc::new(AgentMailboxRouter::new(
                Arc::new(InProcessTransport::new()),
                Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
            ));
            let address = AgentAddress::new(run_id, agent_id);
            let sender = AgentAddress::new("sender-run", "peer-agent");
            let mut state = make_state();
            state.recursion_depth = depth;
            state.current_run_id = Some(format!("{run_id}-first-execution"));
            state.messaging.mailbox = Some(router.register(address.clone(), None).await.unwrap());
            let _sender_mailbox = router.register(sender.clone(), None).await.unwrap();
            let mut host = MockHost::new(vec![]);

            router
                .send(AgentMessage::new(
                    sender.clone(),
                    MessageTarget::Direct {
                        address: address.clone(),
                    },
                    MessagePayload::Progress {
                        turn_index: 1,
                        tool_calls: 0,
                        status: "working".into(),
                        detail: None,
                    },
                ))
                .await
                .unwrap();
            assert!(
                !runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Action)
                    .await
                    .unwrap()
                    .model_context_changed,
                "progress must not supersede a tool action"
            );

            router
                .send(AgentMessage::new(
                    sender.clone(),
                    MessageTarget::Direct {
                        address: address.clone(),
                    },
                    MessagePayload::Text {
                        content: "Review the cancellation path before editing.".into(),
                        summary: None,
                    },
                ))
                .await
                .unwrap();
            assert!(
                runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Action)
                    .await
                    .unwrap()
                    .model_context_changed,
                "semantic input must supersede the stale tool action"
            );
            assert_eq!(host.communication_events.len(), 1);
            assert_eq!(
                host.communication_events[0].observed_by.run_id,
                format!("{run_id}-first-execution")
            );
            state.current_run_id = Some(format!("{run_id}-second-execution"));
            let message = AgentMessage::new(
                sender.clone(),
                MessageTarget::Direct {
                    address: address.clone(),
                },
                MessagePayload::Text {
                    content: "Queued follow-up".into(),
                    summary: None,
                },
            );
            let message_id = message.id.clone();
            router.send(message).await.unwrap();
            runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Action)
                .await
                .unwrap();
            let event = host.communication_events.last().unwrap();
            assert_eq!(
                event.observed_by.run_id,
                format!("{run_id}-second-execution")
            );
            assert_eq!(event.message_id, message_id);
            assert_eq!(event.from.run_id, sender.run_id);
            assert_eq!(
                event.to,
                astra_turn_types::AgentCommunicationTarget::Direct {
                    address: astra_turn_types::AgentCommunicationParty {
                        run_id: address.run_id.clone(),
                        agent_id: address.agent_id.clone()
                    },
                }
            );
            assert!(
                serde_json::to_string(&state.volatile_pending)
                    .unwrap()
                    .contains("Review the cancellation path before editing.")
            );
        }
    }

    #[tokio::test]
    async fn mailbox_messages_keep_distinct_identity_across_safe_boundaries() {
        use astra_messaging::in_process::InProcessTransport;
        use astra_messaging::router::AgentMailboxRouter;
        use astra_messaging::types::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};

        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(InProcessTransport::new()),
            Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
        ));
        let recipient = AgentAddress::new("recipient-run", "recipient");
        let sender = AgentAddress::new("sender-run", "sender");
        let mut state = make_state();
        state.messaging.mailbox = Some(router.register(recipient.clone(), None).await.unwrap());
        let _sender = router.register(sender.clone(), None).await.unwrap();
        for index in 0..65 {
            router
                .send(AgentMessage::new(
                    sender.clone(),
                    MessageTarget::Direct {
                        address: recipient.clone(),
                    },
                    MessagePayload::Text {
                        content: format!("message-{index}"),
                        summary: None,
                    },
                ))
                .await
                .unwrap();
        }
        let mut host = MockHost::new(vec![]);
        assert!(
            runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Preparation)
                .await
                .unwrap()
                .model_context_changed
        );
        assert!(
            runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Provider)
                .await
                .unwrap()
                .model_context_changed
        );
        let retained = state
            .volatile_pending
            .iter()
            .filter(|item| item.kind == VolatileKind::Mailbox)
            .collect::<Vec<_>>();
        assert_eq!(retained.len(), 65, "each message keeps its own identity");
        let ids = retained
            .iter()
            .map(|item| item.payload["message_id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 65, "no mailbox identity may be overwritten");
        let context = serde_json::to_string(&retained).unwrap();
        assert!(context.contains("message-0"));
        assert!(context.contains("message-64"));
    }

    #[tokio::test]
    async fn one_child_result_is_consumable_while_a_sibling_keeps_running() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "agent-run",
            "first-child",
        );
        for child in ["first-child", "second-child"] {
            owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: child.into(),
                run_id: format!("{child}-run"),
                parent_agent_id: "agent".into(),
                status: crate::orchestration::AgentStatus::Running {
                    activity: "working".into(),
                },
            });
        }
        let wait_started = Arc::new(tokio::sync::Notify::new());
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        host.child_wait_started = Some(Arc::clone(&wait_started));
        let mut state = make_state();
        let task = tokio::spawn(async move {
            let result = await_runtime_activity(&mut host, &mut state, None).await;
            (state, result)
        });
        tokio::time::timeout(Duration::from_secs(2), wait_started.notified())
            .await
            .expect("wait started");
        let result_text = "reviewed\n界\"".repeat(1_200);
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "first-child".into(),
            run_id: "first-child-run".into(),
            parent_agent_id: "agent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: result_text.clone(),
                finish_reason: None,
            },
        });
        let (state, result) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("first child must wake execution")
            .unwrap();
        assert_eq!(result.unwrap(), RuntimeActivityOutcome::InputReady);
        let child = &state.volatile_pending[0].payload["children"][0];
        assert_eq!(child["result"], result_text);
        assert_eq!(child["result_truncated"], false);
        assert_eq!(
            child["result_range"],
            serde_json::json!([0, result_text.len(), result_text.len()])
        );
        assert_eq!(
            child["result_sha256"],
            format!("{:x}", Sha256::digest(result_text.as_bytes()))
        );
        assert!(
            serde_json::to_vec(&state.volatile_pending[0].payload)
                .unwrap()
                .len()
                <= crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES
        );
        assert!(
            owner.has_pending_direct_children(),
            "second child still owns completion obligation"
        );
    }

    #[test]
    fn child_receipt_requires_observed_matching_successful_terminal() {
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        let record = ToolCallRecord {
            name: "agent".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(r#"{"action":"get_result","agent_id":"child@run"}"#.into()),
            result_full: Some(
                astra_turn_core::orchestration::agent_result_wire::render_wait_for_agent_status(
                    "child@run",
                    &crate::orchestration::AgentStatus::Running {
                        activity: "working".into(),
                    },
                ),
            ),
            ..Default::default()
        };
        state.push_volatile_payload(
            VolatileKind::BackgroundTaskNotification,
            serde_json::json!({
                "schema": DIRECT_CHILD_RESULT_SCHEMA,
                "parent_run_id": "parent-run",
                "children": [{
                    "agent_id":"child@run",
                    "status":"completed",
                    "result":"child completed",
                    "result_bytes": 15
                }]
            }),
        );
        assert!(!nonterminal_child_receipt_superseded(&state, &record));
        state.volatile_pending[0].payload["observed_by_provider"] = serde_json::json!(true);
        assert!(nonterminal_child_receipt_superseded(&state, &record));
        state.volatile_pending[0].payload["parent_run_id"] = serde_json::json!("other-parent");
        assert!(!nonterminal_child_receipt_superseded(&state, &record));
        state.volatile_pending[0].payload["parent_run_id"] = serde_json::json!("parent-run");
        state.volatile_pending[0].payload["children"][0]["agent_id"] =
            serde_json::json!("other@run");
        assert!(!nonterminal_child_receipt_superseded(&state, &record));
        state.volatile_pending[0].payload["children"][0]["agent_id"] =
            serde_json::json!("child@run");
        state.volatile_pending[0].payload["children"][0]["status"] = serde_json::json!("failed");
        assert!(!nonterminal_child_receipt_superseded(&state, &record));
        state.volatile_pending[0].payload["children"][0]["status"] = serde_json::json!("completed");
        let launched = ToolCallRecord {
            args_full: Some(r#"{"action":"spawn","description":"child","prompt":"work"}"#.into()),
            result_full: Some(r#"{"status":"launched","agent_id":"child@run"}"#.into()),
            ..record.clone()
        };
        assert!(nonterminal_child_receipt_superseded(&state, &launched));
        let mut failed_call = launched.clone();
        failed_call.ok = false;
        assert!(!nonterminal_child_receipt_superseded(&state, &failed_call));
        let mut rejected_call = launched.clone();
        rejected_call.disposition = Some(ToolCallDisposition::Rejected);
        assert!(!nonterminal_child_receipt_superseded(
            &state,
            &rejected_call
        ));
        let mut terminal_failure = record.clone();
        terminal_failure.result_full = Some(
            astra_turn_core::orchestration::agent_result_wire::render_wait_for_agent_status(
                "child@run",
                &crate::orchestration::AgentStatus::Interrupted {
                    partial_result: "unfinished".into(),
                    finish_reason: "deadline".into(),
                },
            ),
        );
        assert!(!nonterminal_child_receipt_superseded(
            &state,
            &terminal_failure
        ));

        let answer = ToolCallRecord {
            name: "agent".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(
                r#"{"action":"send_message","to":"child@run","message_type":"answer","request_id":"request-1"}"#
                    .into(),
            ),
            result_full: Some(r#"{"status":"queued","message_id":"answer-1"}"#.into()),
            ..Default::default()
        };
        assert!(nonterminal_child_receipt_superseded(&state, &answer));
        let mut wrong_target = answer.clone();
        wrong_target.args_full = Some(
            r#"{"action":"send_message","to":"other-child@run","message_type":"answer","request_id":"request-1"}"#
                .into(),
        );
        assert!(!nonterminal_child_receipt_superseded(&state, &wrong_target));
        let mut text_message = answer.clone();
        text_message.args_full =
            Some(r#"{"action":"send_message","to":"child@run","message_type":"text"}"#.into());
        assert!(!nonterminal_child_receipt_superseded(&state, &text_message));

        for _ in 0..2 {
            state.lease_volatile_pending().expect("provider request");
            state.commit_volatile_attempt_lease();
        }
        assert!(
            state
                .lease_volatile_pending()
                .expect("later provider request")
                .is_empty()
        );
        assert!(nonterminal_child_receipt_superseded(&state, &launched));
    }

    #[test]
    fn oversized_child_guidance_stages_a_reference_and_retains_the_complete_receipt() {
        use crate::orchestration::spawner::DirectChildCompletion;
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        let intent = super::super::host::AppliedUserIntent {
            intent_id: "change".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: 1,
            content: "界".repeat(30_000),
        };
        owner.set_direct_child_for_test(DirectChildCompletion {
            agent_id: "child".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "delivery".into(),
                finish_reason: None,
            },
            applied_user_intents: vec![intent.clone()],
        });
        assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
        let payload = &state.volatile_pending.last().unwrap().payload;
        assert!(
            serde_json::to_vec(payload).unwrap().len()
                < crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES
        );
        assert_eq!(payload["children"][0]["applied_user_intents_inline"], false);
        assert_eq!(payload["children"][0]["get_result"]["agent_id"], "child");
        assert_eq!(
            owner
                .retained_direct_child_result("child")
                .unwrap()
                .applied_user_intents,
            vec![intent]
        );
    }

    #[test]
    fn oversized_child_output_keeps_bounded_delivery_and_exact_retained_result() {
        use crate::orchestration::spawner::DirectChildCompletion;
        let budget = crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES;
        // Raw-byte overflow, JSON escaping overflow, and envelope overflow all
        // use the same existing reference path, without losing original bytes.
        for text in [
            "界".repeat(budget),
            "\"".repeat(budget / 2),
            "x".repeat(budget),
        ] {
            let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
                "parent-run",
                "child",
            );
            owner.set_direct_child_for_test(DirectChildCompletion {
                agent_id: "child".into(),
                run_id: "child-run".into(),
                parent_agent_id: "parent".into(),
                status: crate::orchestration::AgentStatus::Completed {
                    result: text.clone(),
                    finish_reason: None,
                },
                applied_user_intents: vec![],
            });
            let mut host = MockHost::new(vec![]);
            host.direct_child_owner = Some(Arc::clone(&owner));
            let mut state = make_state();
            state.current_run_id = Some("parent-run".into());
            assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
            let payload = &state.volatile_pending[0].payload;
            assert!(serde_json::to_vec(payload).unwrap().len() <= budget);
            let child = &payload["children"][0];
            assert_eq!(child["result"], "");
            assert_eq!(child["result_truncated"], true);
            assert_eq!(child["get_result"]["agent_id"], "child");
            assert_eq!(child["result_bytes"], text.len());
            assert_eq!(
                owner.retained_direct_child_result("child").unwrap().status,
                crate::orchestration::AgentStatus::Completed {
                    result: text,
                    finish_reason: None
                }
            );
        }
    }

    #[test]
    fn optional_question_notice_cannot_displace_a_later_child_result() {
        use crate::orchestration::spawner::DirectChildCompletion;
        let budget = crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES;
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-a",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        for index in 0..16 {
            state
                .messaging
                .question_correlations
                .reserve(
                    "parent-run",
                    &format!("question-{index}"),
                    astra_messaging::AgentAddress::new("run-a", "child-a"),
                )
                .unwrap();
        }
        let first = "\"".repeat(budget / 4);
        owner.set_direct_child_for_test(DirectChildCompletion {
            agent_id: "child-a".into(),
            run_id: "run-a".into(),
            parent_agent_id: "parent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: first.clone(),
                finish_reason: None,
            },
            applied_user_intents: vec![],
        });
        stage_direct_child_results(&mut host, &mut state).unwrap();
        let mut first_payload = state.volatile_pending[0].payload.clone();
        assert!(
            first_payload
                .as_object_mut()
                .unwrap()
                .remove("unobserved_questions")
                .is_some()
        );
        let mut second_child = DirectChildCompletion {
            agent_id: "child-b".into(),
            run_id: "run-b".into(),
            parent_agent_id: "parent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: String::new(),
                finish_reason: None,
            },
            applied_user_intents: vec![],
        };
        let mut empty_second = first_payload.clone();
        empty_second["children"] = serde_json::json!([second_child.notification(budget)]);
        let available =
            budget - first_payload.to_string().len() - empty_second.to_string().len() - 128;
        let second = "\"".repeat(available / 2);
        second_child.status = crate::orchestration::AgentStatus::Completed {
            result: second.clone(),
            finish_reason: None,
        };
        owner.set_direct_child_for_test(second_child);
        stage_direct_child_results(&mut host, &mut state).unwrap();
        assert_eq!(state.volatile_pending.len(), 2);
        for (entry, expected) in state.volatile_pending.iter().zip([first, second]) {
            assert_eq!(entry.payload["children"][0]["result"], expected);
            assert_ne!(entry.payload["children"][0]["result_truncated"], true);
            assert!(entry.payload.get("unobserved_questions").is_none());
        }
        assert!(
            state
                .volatile_pending
                .iter()
                .map(|entry| entry.payload.to_string().len())
                .sum::<usize>()
                <= budget
        );
    }

    #[test]
    fn staggered_child_results_share_one_total_pending_budget() {
        use crate::orchestration::spawner::DirectChildCompletion;
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-0",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        for index in 0..2 {
            owner.set_direct_child_for_test(DirectChildCompletion {
                agent_id: format!("child-{index}"),
                run_id: format!("run-{index}"),
                parent_agent_id: "parent".into(),
                status: crate::orchestration::AgentStatus::Completed {
                    result: "delivery".into(),
                    finish_reason: None,
                },
                applied_user_intents: vec![super::super::host::AppliedUserIntent {
                    intent_id: format!("change-{index}"),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::Applied,
                    event_index: index,
                    content: "界".repeat(12_000),
                }],
            });
            assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
            if index == 0 {
                state.lease_volatile_pending().unwrap();
                state.commit_volatile_attempt_lease();
            } else {
                assert_eq!(
                    state.volatile_pending[0].payload["observed_by_provider"],
                    true
                );
                assert_eq!(state.volatile_pending[0].payload["delivery_count"], 1);
            }
            assert_eq!(state.volatile_pending.len(), index + 1);
            assert!(
                state
                    .volatile_pending
                    .iter()
                    .map(|entry| serde_json::to_vec(&entry.payload).unwrap().len())
                    .sum::<usize>()
                    <= crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES
            );
        }
        for child in state
            .volatile_pending
            .iter()
            .flat_map(|entry| entry.payload["children"].as_array().unwrap())
        {
            assert_eq!(child["applied_user_intents_inline"], false);
            assert_eq!(
                owner
                    .retained_direct_child_result(child["agent_id"].as_str().unwrap())
                    .unwrap()
                    .applied_user_intents[0]
                    .content
                    .len(),
                36_000
            );
        }
    }

    #[test]
    fn foreign_child_receipt_is_rejected_without_consuming_or_relabelling() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            agent_id: "child".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "delivery".into(),
                finish_reason: None,
            },
            applied_user_intents: vec![],
        });
        state.push_volatile_payload(VolatileKind::BackgroundTaskNotification, serde_json::json!({
            "schema": DIRECT_CHILD_RESULT_SCHEMA,
            "parent_run_id": "foreign-parent",
            "children": [{"agent_id": "child", "run_id": "foreign-run", "status_fingerprint": "foreign"}],
            "delivery_count": 2, "observed_by_provider": true,
        }));
        let before = state.volatile_pending[0].payload.clone();
        assert!(stage_direct_child_results(&mut host, &mut state).is_err());
        assert_eq!(state.volatile_pending[0].payload, before);
        assert!(state.interruption.is_some());
        assert_eq!(owner.take_completed_direct_children().len(), 1);
    }

    #[test]
    fn settled_child_history_does_not_consume_the_delivery_budget() {
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "fresh",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        let settled = serde_json::json!({
            "schema": DIRECT_CHILD_RESULT_SCHEMA, "parent_run_id": "parent-run",
            "delivery_count": 2, "observed_by_provider": true,
            "children": (0..1000).map(|index| serde_json::json!({
                "agent_id": format!("settled-{index}"), "run_id": format!("run-{index}"),
                "status": "completed", "status_fingerprint": "verified-terminal-receipt",
            })).collect::<Vec<_>>(),
        });
        assert!(
            serde_json::to_vec(&settled).unwrap().len()
                > crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES
        );
        state.push_volatile_payload(VolatileKind::BackgroundTaskNotification, settled.clone());
        owner.set_direct_child_for_test(crate::orchestration::spawner::DirectChildCompletion {
            agent_id: "fresh".into(),
            run_id: "fresh-run".into(),
            parent_agent_id: "parent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "fresh delivery".into(),
                finish_reason: None,
            },
            applied_user_intents: vec![],
        });
        assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
        assert_eq!(state.volatile_pending[0].payload, settled);
        assert_eq!(
            state.volatile_pending[1].payload["children"][0]["result"],
            "fresh delivery"
        );
    }

    #[test]
    fn child_budget_includes_the_notification_envelope() {
        use crate::orchestration::spawner::DirectChildCompletion;
        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        let mut child = DirectChildCompletion {
            agent_id: "child".into(),
            run_id: "child-run".into(),
            parent_agent_id: "parent".into(),
            status: crate::orchestration::AgentStatus::Completed {
                result: "delivery".into(),
                finish_reason: None,
            },
            applied_user_intents: vec![super::super::host::AppliedUserIntent {
                intent_id: "change".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 1,
                content: String::new(),
            }],
        };
        let budget = crate::orchestration::agent_tool::MAX_FANOUT_AGGREGATE_BYTES;
        let base = serde_json::to_vec(std::slice::from_ref(&child))
            .unwrap()
            .len();
        child.applied_user_intents[0].content = "x".repeat(budget - base - 1);
        assert_eq!(
            serde_json::to_vec(std::slice::from_ref(&child))
                .unwrap()
                .len(),
            budget - 1
        );
        owner.set_direct_child_for_test(child);
        assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
        let payload = &state.volatile_pending[0].payload;
        assert!(serde_json::to_vec(payload).unwrap().len() <= budget);
        assert_eq!(payload["children"][0]["applied_user_intents_inline"], false);
    }

    #[test]
    fn corrected_child_terminal_replaces_observed_success_without_touching_siblings() {
        use crate::orchestration::AgentStatus;
        use crate::orchestration::spawner::DirectChildCompletion;

        let owner = crate::orchestration::FanoutParentAdmission::consumed_direct_child_for_test(
            "parent-run",
            "child-a",
        );
        let mut host = MockHost::new(vec![]);
        host.direct_child_owner = Some(Arc::clone(&owner));
        let mut state = make_state();
        state.current_run_id = Some("parent-run".into());
        for agent_id in ["child-a", "child-b"] {
            owner.set_direct_child_for_test(DirectChildCompletion {
                applied_user_intents: Vec::new(),
                agent_id: agent_id.into(),
                run_id: format!("run-{agent_id}"),
                parent_agent_id: "parent".into(),
                status: AgentStatus::Completed {
                    result: "done".into(),
                    finish_reason: None,
                },
            });
        }
        assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
        for _ in 0..2 {
            state.lease_volatile_pending().unwrap();
            state.commit_volatile_attempt_lease();
        }
        owner.set_direct_child_for_test(DirectChildCompletion {
            applied_user_intents: Vec::new(),
            agent_id: "child-a".into(),
            run_id: "run-child-a".into(),
            parent_agent_id: "parent".into(),
            status: AgentStatus::Failed {
                error: "corrected".into(),
                finish_reason: None,
                partial_result: String::new(),
            },
        });
        assert!(stage_direct_child_results(&mut host, &mut state).unwrap());
        let children: Vec<_> = state
            .volatile_pending
            .iter()
            .filter(|entry| entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA)
            .flat_map(|entry| entry.payload["children"].as_array().unwrap().iter())
            .collect();
        assert_eq!(children.len(), 2);
        assert!(
            children
                .iter()
                .any(|child| child["agent_id"] == "child-a" && child["status"] == "failed")
        );
        assert!(
            children
                .iter()
                .any(|child| child["agent_id"] == "child-b" && child["status"] == "completed")
        );
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.payload["children"]
                .as_array()
                .is_some_and(|children| children.iter().any(|child| child["agent_id"] == "child-b"))
                && entry.payload["observed_by_provider"] == true
                && entry.payload["delivery_count"] == 2
        }));
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.payload["children"]
                .as_array()
                .is_some_and(|children| children.iter().any(|child| child["agent_id"] == "child-a"))
                && entry.payload["observed_by_provider"] != true
        }));
    }

    #[tokio::test]
    async fn delivered_direct_child_evidence_survives_a_provider_round() {
        let mut state = make_state();
        state.push_volatile_payload(
            VolatileKind::BackgroundTaskNotification,
            serde_json::json!({
                "schema": DIRECT_CHILD_RESULT_SCHEMA,
                "parent_run_id": "parent-run",
                "children": [{"agent_id": "child", "status": "completed", "result_sha256": "sha256:test"}],
            }),
        );
        // The production host leases required context when assembling the
        // provider request; the lightweight mock only does so with an owner.
        state.volatile_pending[0].attempt_leased = true;
        let mut read = make_edge_tool("read_file", "evidence");
        read.args = serde_json::json!({"path": "/app/evidence.txt"});
        let mut host = MockHost::new(vec![edge_tool_result(vec![read], 10, 5, Some(1))])
            .with_valid_tools(&["read_file"]);
        execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
            .await
            .expect("provider round");
        assert!(
            host.executed_volatile[0]
                .iter()
                .any(|entry| { entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA })
        );
        assert!(
            state.volatile_pending.iter().any(|entry| {
                entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                    && entry.payload["observed_by_provider"] == true
                    && !entry.attempt_leased
            }),
            "pending={:?}",
            state.volatile_pending
        );
        // The lightweight mock only leases automatically when it owns a real
        // child barrier; explicitly model the next provider boundary here.
        state
            .lease_volatile_pending()
            .expect("bounded repeat provider request");
        let mut next = MockHost::new(vec![text_result("Done", 10, 5, Some(1))]);
        execute_turn_and_ingest_phase(&mut next, &mut state, 1, prep(false))
            .await
            .expect("next provider round");
        assert!(next.executed_volatile[0].iter().any(|entry| {
            entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA
                && entry.payload["observed_by_provider"] == true
        }));
        assert_eq!(
            state
                .volatile_pending
                .iter()
                .filter(|entry| entry.payload["schema"] == DIRECT_CHILD_RESULT_SCHEMA)
                .count(),
            1,
            "observed child evidence must survive the bounded repeat delivery"
        );
        assert!(
            state
                .lease_volatile_pending()
                .expect("third provider request")
                .iter()
                .all(|entry| entry.payload["schema"] != DIRECT_CHILD_RESULT_SCHEMA),
            "retained settlement evidence must not bloat later provider requests"
        );
    }

    #[tokio::test]
    async fn missing_policy_history_preserves_tool_terminal_and_ends_incomplete() {
        let mut state = make_state();
        let prior = vec![
            astra_services::session_journal::ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                round: Some(1),
                ..Default::default()
            };
            10
        ];
        let mut host = MockHost::new(vec![edge_tool_result(
            vec![make_edge_tool(
                "read_file",
                "preserved policy-boundary result",
            )],
            11,
            3,
            None,
        )])
        .with_valid_tools(&["read_file"])
        .with_stop_after_success_completion("read_file", Some("must not replace incomplete"));
        let subject = host.runtime_policy_subject(&state);
        crate::turn::runtime_policy::evaluate_tool_boundary(
            &mut state.stall.runtime_policy_evaluation,
            subject,
            &prior,
            1,
        )
        .unwrap();
        let policy_before = serde_json::to_value(&state.stall.runtime_policy_evaluation).unwrap();
        state.step_recorder.begin_turn(1);
        let budget = (
            state.max_turns,
            state.remaining_turns,
            state.charged_iterations,
        );
        let turn_result = host.execute_turn(&mut state).await.unwrap();
        let outcome = super::super::tool_phase::execute_tool_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            TurnExecutionPhase {
                llm_wall_start: Instant::now(),
                turn_result,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            super::super::tool_phase::TurnToolPhaseControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(!state.final_text.contains("must not replace incomplete"));
        assert_eq!(
            (
                state.max_turns,
                state.remaining_turns,
                state.charged_iterations
            ),
            budget
        );
        assert_eq!(
            serde_json::to_value(&state.stall.runtime_policy_evaluation).unwrap(),
            policy_before
        );
        assert!(
            host.final_policy_snapshots.is_empty(),
            "phase returns an outcome; shared loop exit renders it"
        );
        assert!(!state.stall.tool_call_records.is_empty());
        assert!(state.messages.iter().any(|message| {
            message
                .to_string()
                .contains("preserved policy-boundary result")
        }));
    }

    #[tokio::test]
    async fn restored_verification_tool_phase_preserves_results_and_ends_incomplete() {
        let mut state = make_state();
        state.stall.verification_frontier =
            super::super::verification_frontier::tests::restored_read_only_prefix();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("new contract", "./new-check"));
        state.step_recorder.begin_turn(1);
        let budget = (
            state.max_turns,
            state.remaining_turns,
            state.charged_iterations,
        );
        let mut host = MockHost::new(vec![edge_tool_result(
            vec![make_edge_tool("read_file", "preserved tool result")],
            11,
            3,
            None,
        )])
        .with_valid_tools(&["read_file"])
        .with_stop_after_success_completion("read_file", Some("must not replace incomplete"));
        // Delivery happens through admission. A changed contract discovered
        // after execution must preserve its receipt before ending the turn.
        let turn_result = host.execute_turn(&mut state).await.unwrap();
        let outcome = super::super::tool_phase::execute_tool_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            TurnExecutionPhase {
                llm_wall_start: Instant::now(),
                turn_result,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            super::super::tool_phase::TurnToolPhaseControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(state.final_text.contains("cannot claim it is complete"));
        assert!(!state.final_text.contains("must not replace incomplete"));
        assert_eq!(
            (
                state.max_turns,
                state.remaining_turns,
                state.charged_iterations
            ),
            budget
        );
        assert_eq!(state.tool_ledger_receipt.canonical_aggregate().terminal, 1);
        assert_eq!(state.stall.tool_call_records.len(), 1);
        assert!(
            state
                .tool_results
                .iter()
                .any(|result| result.to_string().contains("preserved tool result")),
            "tool results: {:?}; records: {:?}",
            state.tool_results,
            state.stall.tool_call_records
        );
        assert!(
            host.executed_messages.len() == 1,
            "only the original provider request; no follow-up request"
        );
        assert!(
            host.rendered_final_text.is_empty(),
            "phase returns an outcome; shared loop exit renders it"
        );
    }

    fn ordinary_admitted(
        calls: impl IntoIterator<Item = serde_json::Value>,
    ) -> Vec<astra_turn_core::tool::deferred_activation::CanonicalToolInvocation> {
        calls
            .into_iter()
            .map(astra_turn_core::tool::deferred_activation::CanonicalToolInvocation::ordinary)
            .collect()
    }

    fn explicit_verification_action(state: &mut AgenticLoopState) -> CompletionAction {
        if state.stall.tool_call_records.is_empty() {
            state
                .stall
                .tool_call_records
                .push(executed_record("write_file", true, None));
        }
        state.hooks.stop_hooks = vec![astra_turn_core::stop_hooks::StopHook {
            label: "required check".into(),
            command: "cargo test".into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            authoritative: true,
        }];
        CompletionAction::ExplicitVerification {
            missing_labels: vec!["required check".into()],
        }
    }

    fn admitted_logical_calls(admission: &ToolCallAdmission) -> Vec<serde_json::Value> {
        admission
            .admitted
            .iter()
            .map(|call| call.logical_target_call().clone())
            .collect()
    }

    fn install_committed_work_synthesis_wire_surface(state: &mut AgenticLoopState) {
        state
            .hooks
            .completion_settlement
            .preserve_final_synthesis_wire_surface = true;
    }

    fn attach_llm_progress_receiver(
        state: &mut AgenticLoopState,
    ) -> tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent> {
        let broadcaster = Arc::new(crate::orchestration::ProgressBroadcaster::new(16));
        let receiver = broadcaster.subscribe();
        state.messaging.progress_emitter = Some(broadcaster.for_agent_with_run_context(
            "progress-test-agent".to_string(),
            "progress-test-run".to_string(),
            "progress-test-parent".to_string(),
            None,
        ));
        receiver
    }

    fn drain_llm_progress(
        receiver: &mut tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>,
    ) -> Vec<crate::orchestration::ProgressEventType> {
        let mut events = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(event)
                    if matches!(
                        &event.event_type,
                        crate::orchestration::ProgressEventType::LlmCallStarted { .. }
                            | crate::orchestration::ProgressEventType::LlmCallCompleted { .. }
                    ) =>
                {
                    events.push(event.event_type);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    panic!("progress test receiver lagged by {skipped} events")
                }
            }
        }
        events
    }

    fn assert_no_llm_progress(
        receiver: &mut tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>,
    ) {
        assert!(
            drain_llm_progress(receiver).is_empty(),
            "pre-host rejection must not emit an orphan LLM lifecycle"
        );
    }

    fn assert_one_paired_llm_progress(
        receiver: &mut tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>,
        turn: u32,
        expected_ttft_ms: Option<u64>,
    ) {
        let events = drain_llm_progress(receiver);
        assert_eq!(events.len(), 2, "one host call must emit one exact pair");
        assert!(matches!(
            &events[0],
            crate::orchestration::ProgressEventType::LlmCallStarted { turn: actual }
                if *actual == turn
        ));
        assert!(matches!(
            &events[1],
            crate::orchestration::ProgressEventType::LlmCallCompleted {
                turn: actual,
                ttft_ms,
                ..
            } if *actual == turn && *ttft_ms == expected_ttft_ms
        ));
    }

    #[test]
    fn settlement_candidate_keeps_first_mixed_response_until_text_only_retry() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;

        let mixed = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "Verified the change and the remaining check is clear.".to_string(),
                tool_calls: vec![serde_json::json!({
                    "function": {"name": "bash", "arguments": "{}"}
                })],
                has_tool_calls: true,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,

            error_kind: None,
        };
        capture_deferred_candidate_text(&mut state, &mixed);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .deferred_candidate_text
                .as_deref(),
            Some("Verified the change and the remaining check is clear.")
        );

        let later_mixed = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "Let me inspect one more thing.".to_string(),
                tool_calls: vec![serde_json::json!({
                    "function": {"name": "cat", "arguments": "{}"}
                })],
                has_tool_calls: true,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,

            error_kind: None,
        };
        capture_deferred_candidate_text(&mut state, &later_mixed);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .deferred_candidate_text
                .as_deref(),
            Some("Verified the change and the remaining check is clear."),
            "a repeated violating response must not replace the substantive first candidate"
        );

        let compliant_retry = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "Verified the change; one requested check remains unverified."
                    .to_string(),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,

            error_kind: None,
        };
        capture_deferred_candidate_text(&mut state, &compliant_retry);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .deferred_candidate_text
                .as_deref(),
            Some("Verified the change; one requested check remains unverified."),
            "a compliant text-only retry may replace the mixed candidate"
        );
    }

    #[test]
    fn latest_provider_text_tracks_each_non_empty_response() {
        let mut state = make_state();
        let first = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "first response".to_string(),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,

            error_kind: None,
        };
        capture_latest_provider_text(&mut state, &first);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .latest_provider_text
                .as_deref(),
            Some("first response")
        );

        let second = HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: "latest response".to_string(),
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,

            error_kind: None,
        };
        capture_latest_provider_text(&mut state, &second);
        assert_eq!(
            state
                .hooks
                .completion_settlement
                .latest_provider_text
                .as_deref(),
            Some("latest response")
        );
    }

    #[test]
    fn same_boundary_observation_fanout_collapses_to_composite_overviews() {
        let mut calls = vec![
            serde_json::json!({"id":"i1","function":{"name":"introspect","arguments":"{\"facet\":\"trace\",\"question\":\"why\"}"}}),
            serde_json::json!({"id":"i2","function":{"name":"introspect","arguments":"{\"facet\":\"overview\"}"}}),
            serde_json::json!({"id":"r1","function":{"name":"reflect","arguments":"{\"facet\":\"tools\",\"question\":\"why\"}"}}),
            serde_json::json!({"id":"r2","function":{"name":"reflect","arguments":"{\"facet\":\"overview\"}"}}),
        ];

        assert_eq!(collapse_batched_observation_fanout(&mut calls), 2);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "i2");
        assert_eq!(calls[1]["id"], "r2");
        for call in calls {
            let args: serde_json::Value =
                serde_json::from_str(call["function"]["arguments"].as_str().expect("args"))
                    .expect("valid json args");
            assert_eq!(args["facet"], "overview");
            assert_eq!(args["depth"], "diagnostic");
        }
    }

    #[test]
    fn artifact_paging_is_not_collapsed_with_live_introspection() {
        let mut calls = vec![
            serde_json::json!({"id":"page","function":{"name":"introspect","arguments":"{\"artifact\":\"artifact://session/tool-result/x\",\"offset\":0}"}}),
            serde_json::json!({"id":"previous","function":{"name":"introspect","arguments":"{\"explain\":{\"target\":\"previous\"}}"}}),
            serde_json::json!({"id":"run-a","function":{"name":"introspect","arguments":"{\"explain\":{\"target\":\"run\",\"run_id\":\"a\"}}"}}),
            serde_json::json!({"id":"run-b","function":{"name":"introspect","arguments":"{\"explain\":{\"target\":\"run\",\"run_id\":\"b\"}}"}}),
            serde_json::json!({"id":"live","function":{"name":"introspect","arguments":"{\"facet\":\"overview\"}"}}),
        ];
        let original = calls.clone();
        assert_eq!(collapse_batched_observation_fanout(&mut calls), 0);
        assert_eq!(
            calls, original,
            "report selectors and their identities must survive batching"
        );
    }

    #[tokio::test]
    async fn ordinary_completion_does_not_invent_a_post_mutation_observer() {
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("write_file", "updated source")],
                20,
                10,
                Some(30),
            ),
            edge_tool_result(
                vec![make_edge_tool("bash", "wrote unrelated scratch probe")],
                20,
                10,
                Some(30),
            ),
            text_result("The change is complete.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["write_file", "bash", "read_file"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/app".into());
        state.message = "Please update the workspace and report back.".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 3);
        assert_eq!(state.final_text, "The change is complete.");
    }

    #[tokio::test]
    async fn bound_script_validation_completes_without_model_progress_phrase() {
        let mut write = make_edge_tool("write_file", "script written");
        write.args = serde_json::json!({
            "path": "/app/solution.py",
            "content": "print('ok')",
        });
        let mut validate = make_edge_tool("bash", "ok");
        validate.args = serde_json::json!({
            "command": "cd /app && python3 solution.py",
        });
        validate
            .tool_result_fields
            .as_mut()
            .expect("edge fixture carries owner metadata")
            .extend(
                astra_tools::workspace_observation::changed_receipt_with_ownership(
                    astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP,
                ),
            );
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![write], 20, 10, Some(30)),
            edge_tool_result(vec![validate], 20, 10, Some(30)),
            text_result("Ready.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["write_file", "bash"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/app".into());
        state.message = "Implement and validate the requested workspace artifact.".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("trusted tool evidence should settle the run");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 3, "no recovery round should be needed");
        assert_eq!(state.final_text, "Ready.");
        assert!(state.interruption.is_none());
    }

    fn typed_writer_record(path: &str) -> ToolCallRecord {
        let args = serde_json::json!({"path": path, "content": "updated"}).to_string();
        let mut record = executed_record("write_file", true, Some(&args));
        record.runtime_args_full = Some(args);
        record
    }

    fn full_scope_verify_record() -> ToolCallRecord {
        let args = serde_json::json!({
            "command": "ls -la /app/program.py /app/requirements.txt && python3 /app/program.py",
        })
        .to_string();
        ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(args.clone()),
            runtime_args_full: Some(args),
            result_class: Some("success".into()),
            exit_semantics: Some("success".into()),
            ..ToolCallRecord::default()
        }
    }

    #[test]
    fn live_full_scope_verify_receipt_settles_multi_file_mutation_batch_in_either_order() {
        for paths in [
            ["/app/program.py", "/app/requirements.txt"],
            ["/app/requirements.txt", "/app/program.py"],
        ] {
            let mut state = make_state();
            mark_must_mutate(&mut state);
            state.hooks.workspace_root_hint = Some("/app".into());
            state.stall.tool_call_records = vec![
                typed_writer_record(paths[0]),
                typed_writer_record(paths[1]),
                full_scope_verify_record(),
            ];
            state.hooks.completion_settlement.completion_action_window =
                Some(super::super::host::CompletionActionWindow {
                    action: CompletionAction::ExplicitVerification {
                        missing_labels: vec!["verify".into()],
                    },
                    attempts_remaining: 0,
                    mismatch_corrections_remaining: 0,
                    consumed: true,
                    matched: true,
                });

            assert_eq!(pending_completion_action(&state).unwrap(), None);
            assert_eq!(
                enforce_completion_action_window_before_text_completion(&mut state),
                CompletionActionBoundary::Settled
            );
            assert!(state.interruption.is_none());
        }
    }

    #[tokio::test]
    async fn ordinary_reporting_does_not_reinterpret_scratch_redirect_as_verification() {
        let mut scratch_bash = make_edge_tool("bash", "created scratch fixture");
        scratch_bash.args = serde_json::json!({
            "command": "cat > /tmp/test_xss.html <<'EOF'\n<script>alert(1)</script>\nEOF"
        });
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("write_file", "updated source")],
                20,
                10,
                Some(30),
            ),
            edge_tool_result(vec![scratch_bash], 20, 10, Some(30)),
            text_result("The change is complete.", 20, 10, Some(30)),
        ])
        .with_valid_tools(&["write_file", "bash", "read_file"]);
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/app".into());
        state.message = "Please update the workspace and report back.".to_string();
        state.user_intent = state.message.clone();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": state.message}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 3);
        assert_eq!(state.final_text, "The change is complete.");
    }

    #[test]
    fn owner_typed_workspace_receipts_survive_remote_root_projection() {
        // The writer/observer owner is an Edge workspace at /app while the
        // server-side scheduler is intentionally given a different local
        // root.  No host stat or task-specific path rule may be needed.
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/server-side-root".into());

        let writer_fields = astra_tools::workspace_observation::typed_workspace_tool_receipt();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            runtime_args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: writer_fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        let observer_fields =
            astra_tools::workspace_observation::typed_workspace_observation_receipt();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            runtime_args_full: Some(r#"{"path":"/app/filter.py"}"#.into()),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: observer_fields
                .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        assert_eq!(pending_completion_action(&state).unwrap(), None);

        let edge_workspace = tempfile::tempdir().expect("edge workspace");
        std::fs::write(edge_workspace.path().join("filter.py"), "ready\n").expect("edge target");
        let writer_args = serde_json::json!({"path": "filter.py", "content": "ready\n"});
        let desired_state =
            astra_tools::workspace_observation::workspace_file_state_identity(b"ready\n");
        let convergence_fields = astra_tools::workspace_observation::typed_workspace_desired_state_convergence_receipt_for(
            "write_file",
            &writer_args,
            edge_workspace.path(),
            false,
            Some(&desired_state),
        )
        .expect("owner convergence receipt");
        let observer_args = serde_json::json!({"path": "filter.py"});
        let observation_fields =
            astra_tools::workspace_observation::typed_workspace_observation_snapshot_receipt_for(
                "read_file",
                &observer_args,
                edge_workspace.path(),
                false,
                true,
            )
            .expect("owner strong observation");
        let mut convergence_state = make_state();
        mark_must_mutate(&mut convergence_state);
        convergence_state.hooks.workspace_root_hint = Some(format!(
            "/definitely-absent-scheduler-root-{}",
            uuid::Uuid::new_v4()
        ));
        convergence_state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                tool_call_id: Some("remote-write".into()),
                round: Some(1),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(writer_args.to_string()),
                runtime_args_full: Some(writer_args.to_string()),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt: convergence_fields
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
                ..ToolCallRecord::default()
            },
            ToolCallRecord {
                name: "read_file".into(),
                tool_call_id: Some("remote-read".into()),
                round: Some(2),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(observer_args.to_string()),
                runtime_args_full: Some(observer_args.to_string()),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt: observation_fields
                    .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                    .cloned(),
                ..ToolCallRecord::default()
            },
        ];

        assert_eq!(pending_completion_action(&convergence_state).unwrap(), None);
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

    fn executed_record(name: &str, ok: bool, args_full: Option<&str>) -> ToolCallRecord {
        let mut record = ToolCallRecord {
            name: name.into(),
            ok,
            args_full: args_full.map(ToString::to_string),
            disposition: Some(ToolCallDisposition::Executed),
            result_class: ok.then(|| "success".into()),
            exit_semantics: ok.then(|| "success".into()),
            ..Default::default()
        };
        // Provider-neutral fixtures model the same owner fact production
        // direct writers now emit: a successful applied write has a receipt.
        // A failed call alone does not report a workspace change.
        if ok
            && matches!(
                name,
                "write_file"
                    | "str_replace"
                    | "multi_edit"
                    | "edit_file"
                    | "create_file"
                    | "delete_file"
                    | "notebook_edit"
                    | "rollback_file_edits"
                    | "rename_symbol"
                    | "lsp"
            )
        {
            let fields = astra_tools::workspace_observation::typed_workspace_tool_receipt();
            record.workspace_mutation_observed = Some(true);
            record.workspace_mutation_scope =
                Some(astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.to_string());
            record.workspace_mutation_receipt = fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned();
        }
        record
    }

    fn validation_record(command: &str, result_class: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(serde_json::json!({"command": command}).to_string()),
            result_class: Some(result_class.into()),
            exit_semantics: Some(
                if result_class == "success" {
                    "success"
                } else {
                    "domain_negative"
                }
                .into(),
            ),
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_hook_requires_a_successful_command_result() {
        let hook = explicit_verification_hook("check", "grep pattern input.txt");
        let mut record = validation_record("grep pattern input.txt", "success");
        assert!(record_verifies_explicit_hook(&record, &hook, None));
        for class in [
            "empty_result",
            "domain_negative",
            "test_failure",
            "inconclusive",
        ] {
            record.result_class = Some(class.into());
            record.exit_semantics = Some("domain_negative".into());
            assert!(
                !record_verifies_explicit_hook(&record, &hook, None),
                "{class}"
            );
        }
        record.result_class = None;
        record.exit_semantics = Some("success".into());
        record.result_full = Some(r#"{"result_family":"agent","status":"completed"}"#.into());
        assert!(!record_verifies_explicit_hook(&record, &hook, None));
        record.result_class = Some("success".into());
        record.exit_semantics = Some("success".into());
        record.ok = false;
        assert!(!record_verifies_explicit_hook(&record, &hook, None));
    }

    #[test]
    fn explicit_contract_accepts_successful_command_after_mutation() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));

        assert_eq!(
            missing_explicit_verification_hooks(&state).unwrap(),
            Some(Vec::new())
        );
        assert!(!enforce_explicit_verification_before_text_completion(
            &mut state
        ));
        assert!(state.interruption.is_none());
    }

    #[test]
    fn explicit_contract_preserves_command_bytes_and_declared_workdir() {
        let hook = explicit_verification_hook("quality", "./quality-gate");
        let exact = serde_json::json!({"command": "./quality-gate"});
        let extra_args = serde_json::json!({"command": "./quality-gate --quick"});
        let compound = serde_json::json!({"command": "./quality-gate && echo done"});

        assert!(tool_call_verifies_explicit_hook(
            "bash",
            Some(&exact),
            &hook,
            None
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&extra_args),
            &hook,
            None
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&compound),
            &hook,
            None
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "quality",
            Some(&exact),
            &hook,
            None
        ));

        let quoted_hook = explicit_verification_hook("quoted", "printf '%s' 'a  b'");
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&serde_json::json!({
                "command": "printf '%s' 'a b'",
            })),
            &quoted_hook,
            None
        ));
        let mut root_hook = explicit_verification_hook("root", "./quality-gate");
        root_hook.working_dir = Some("/workspace".into());
        assert!(tool_call_verifies_explicit_hook(
            "bash",
            Some(&exact),
            &root_hook,
            Some("/workspace")
        ));

        let mut scoped_hook = explicit_verification_hook("quality", "./quality-gate");
        scoped_hook.working_dir = Some("/workspace/check".into());
        let scoped_exact = serde_json::json!({
            "command": "./quality-gate", "workdir": "/workspace/check"
        });
        let scoped_wrong_dir = serde_json::json!({
            "command": "./quality-gate", "workdir": "/workspace/other"
        });
        assert!(tool_call_verifies_explicit_hook(
            "bash",
            Some(&scoped_exact),
            &scoped_hook,
            None
        ));
        assert!(!tool_call_verifies_explicit_hook(
            "bash",
            Some(&scoped_wrong_dir),
            &scoped_hook,
            None
        ));
    }

    #[test]
    fn successful_unknown_verifier_is_post_mutation_evidence() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));

        assert_eq!(
            missing_explicit_verification_hooks(&state).unwrap(),
            Some(Vec::new())
        );
        assert_eq!(pending_completion_action(&state).unwrap(), None);
    }

    #[test]
    fn independent_explicit_verifiers_share_one_batch_window() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("unit", "./unit-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = pending_completion_action(&state)
            .unwrap()
            .expect("both hooks are pending");
        assert!(completion_action_window_is_batchable(&state, &action));
        let quality = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"./quality-gate\"}"}
        });
        let unit = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"./unit-gate\"}"}
        });
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action,
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let admission = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: ordinary_admitted([quality.clone(), unit.clone()]),
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            &[quality, unit],
        );
        assert_eq!(admission.admitted.len(), 2);
        assert!(admission.rejected.is_empty());
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .is_some_and(|window| window.matched)
        );
    }

    #[test]
    fn dependent_explicit_verifiers_do_not_open_batch_window() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        let mut unit = explicit_verification_hook("unit", "./unit-gate");
        unit.depends_on.push("quality".into());
        state.hooks.stop_hooks.push(unit);
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = pending_completion_action(&state)
            .unwrap()
            .expect("both hooks are pending");
        assert!(!completion_action_window_is_batchable(&state, &action));
    }

    #[test]
    fn explicit_contract_receipt_is_invalidated_by_a_later_mutation() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        assert_eq!(
            missing_explicit_verification_hooks(&state).unwrap(),
            Some(vec!["quality".to_string()])
        );
    }

    #[test]
    fn quarantined_workspace_blocks_reader_settlement_and_final_text() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let root = temp.path().to_string_lossy().into_owned();
        assert!(
            astra_tools::workspace_observation::mark_workspace_observation_unsettled(temp.path())
        );

        let mut state = make_state();
        state.hooks.workspace_root_hint = Some(root);
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));

        assert!(workspace_observation_is_quarantined(&state));

        assert_eq!(pending_completion_action(&state).unwrap(), None);
        assert!(completion_action_window_requires_followup(&state));
        assert!(reject_unsettled_execution_completion(&mut state));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn incomplete_notice_identifies_only_its_known_tool_cause() {
        for (source, names_timeout) in [("timed-out", true), ("other", false)] {
            let temp = tempfile::tempdir().unwrap();
            assert!(
                astra_tools::workspace_observation::mark_workspace_observation_unsettled(
                    temp.path()
                )
            );
            let mut state = make_state();
            state.hooks.workspace_root_hint = Some(temp.path().to_string_lossy().into_owned());
            state.stall.workspace_observation_quarantine = Some(
                astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(Some(source.into())),
            );
            let mut record = executed_record("bash", false, None);
            record.tool_call_id = Some("timed-out".into());
            record.error_kind = Some(astra_core::ErrorKind::ToolTimeout);
            state.stall.tool_call_records.push(record);
            assert!(reject_unsettled_execution_completion(&mut state));
            let message = &state.interruption.as_ref().unwrap().user_message;
            assert_eq!(
                message.contains("bash tool timed out"),
                names_timeout,
                "{message}"
            );
            assert!(message.contains("No checkpoint was saved"), "{message}");
            assert!(message.contains("inspect the current state"), "{message}");
        }
    }

    #[test]
    fn post_record_quarantine_transition_covers_weak_and_partial_facts() {
        let mut state = make_state();
        let weak_fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some("weak-call".into()),
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: weak_fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });
        let weak_records = state.stall.tool_call_records.clone();
        assert!(apply_workspace_observation_quarantine_transition(
            &mut state,
            &weak_records,
        ));
        assert_eq!(
            state
                .stall
                .workspace_observation_quarantine
                .as_ref()
                .map(|q| q.reason.as_str()),
            Some(
                astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::WEAK_PROCESS_OWNERSHIP_REASON
            )
        );

        let mut partial = make_state();
        let partial_fields = astra_tools::ToolResult::error("partial".into())
            .with_workspace_mutation_partial(vec!["/workspace/a".into()])
            .metadata
            .expect("partial receipt");
        partial.stall.tool_call_records.push(ToolCallRecord {
            tool_call_id: Some("partial-call".into()),
            name: "str_replace".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: partial_fields
                .get(astra_tools::workspace_observation::OBSERVED_FIELD)
                .and_then(serde_json::Value::as_bool),
            workspace_mutation_scope: partial_fields
                .get(astra_tools::workspace_observation::SCOPE_FIELD)
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            workspace_mutation_receipt: partial_fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            workspace_mutation_partial: Some(true),
            workspace_mutation_partial_paths: Some(vec!["/workspace/a".into()]),
            ..ToolCallRecord::default()
        });
        let partial_records = partial.stall.tool_call_records.clone();
        assert!(apply_workspace_observation_quarantine_transition(
            &mut partial,
            &partial_records,
        ));
        assert_eq!(
            partial
                .stall
                .workspace_observation_quarantine
                .as_ref()
                .map(|q| q.reason.as_str()),
            Some(
                astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::PARTIAL_MUTATION_REASON
            )
        );
        assert!(reject_unsettled_execution_completion(&mut partial));
        let message = &partial.interruption.as_ref().unwrap().user_message;
        assert!(message.contains("partial update"), "{message}");
        assert!(message.contains("Review the affected files"), "{message}");
        assert!(
            !message.contains("could not be confirmed stopped"),
            "{message}"
        );
    }

    #[test]
    fn untrusted_tool_cannot_quarantine_from_lookalike_receipt_fields() {
        let fields = astra_tools::workspace_observation::changed_receipt_with_ownership(
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP,
        );
        let mut state = make_state();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "mcp__example__write".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt: fields
                .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                .cloned(),
            ..ToolCallRecord::default()
        });

        assert!(!workspace_observation_is_quarantined(&state));
        assert!(!record_has_weak_workspace_mutation_receipt(
            &state.stall.tool_call_records[0]
        ));
    }

    #[test]
    fn partial_quarantine_requires_typed_failed_multi_path_writer_receipt() {
        let typed_receipt = serde_json::json!({
            "schema": "workspace_mutation_partial_receipt.v1",
            "source": "typed_multi_path_writer",
            "scope": astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE,
            "changed": true,
            "ownership": astra_tools::workspace_observation::TYPED_MULTI_PATH_WRITER_OWNERSHIP,
            "paths": ["/workspace/a"],
        });
        for name in ["str_replace", "lsp"] {
            let mut state = make_state();
            let record = ToolCallRecord {
                name: name.into(),
                ok: false,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                workspace_mutation_partial: Some(true),
                workspace_mutation_partial_paths: Some(vec!["/workspace/a".into()]),
                workspace_mutation_receipt: Some(typed_receipt.clone()),
                ..ToolCallRecord::default()
            };
            assert!(apply_workspace_observation_quarantine_transition(
                &mut state,
                &[record],
            ));
            assert!(workspace_observation_is_quarantined(&state));
        }
        for (name, ok, disposition, paths, receipt) in [
            (
                "lsp",
                true,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "lsp",
                false,
                astra_services::session_journal::ToolCallDisposition::Rejected,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "lsp",
                false,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(vec!["/workspace/a".to_string()]),
                None,
            ),
            (
                "mcp__example__write",
                false,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "str_replace",
                true,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "str_replace",
                false,
                astra_services::session_journal::ToolCallDisposition::Rejected,
                Some(vec!["/workspace/a".to_string()]),
                Some(typed_receipt.clone()),
            ),
            (
                "str_replace",
                false,
                astra_services::session_journal::ToolCallDisposition::Executed,
                Some(Vec::new()),
                Some(serde_json::json!({
                    "schema": "workspace_mutation_partial_receipt.v1",
                    "source": "typed_multi_path_writer",
                    "scope": astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE,
                    "changed": true,
                    "ownership": astra_tools::workspace_observation::TYPED_MULTI_PATH_WRITER_OWNERSHIP,
                    "paths": [],
                })),
            ),
        ] {
            let mut state = make_state();
            state.stall.tool_call_records.push(ToolCallRecord {
                name: name.into(),
                ok,
                disposition: Some(disposition),
                workspace_mutation_partial: Some(true),
                workspace_mutation_partial_paths: paths,
                workspace_mutation_receipt: receipt,
                ..ToolCallRecord::default()
            });
            let records = state.stall.tool_call_records.clone();
            assert!(!apply_workspace_observation_quarantine_transition(
                &mut state, &records
            ));
            assert!(!workspace_observation_is_quarantined(&state));
        }
    }

    #[test]
    fn external_scratch_after_workspace_receipt_does_not_reset_observation_watermark() {
        let mut state = make_state();
        state.hooks.workspace_root_hint = Some("/workspace".into());
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "path": "/workspace/out.txt",
                        "content": "x"
                    })
                    .to_string(),
                ),
                file_path: Some("/workspace/out.txt".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: "read_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(r#"{"path":"/workspace/out.txt"}"#.into()),
                file_path: Some("/workspace/out.txt".into()),
                ..Default::default()
            },
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "path": "/tmp/scratch.txt",
                        "content": "scratch"
                    })
                    .to_string(),
                ),
                file_path: Some("/tmp/scratch.txt".into()),
                ..Default::default()
            },
        ];

        assert_eq!(pending_completion_action(&state).unwrap(), None);
    }

    #[test]
    fn shell_text_without_a_change_receipt_does_not_invalidate_declared_checks() {
        let cases = [(
            true,
            r#"{"command":"python3 -c \"open('/workspace/a','w').write('bad')\""}#,
            ),
            (
                false,
                r#"{"command":"printf bad > /workspace/a && false"}"#,
        )];
        for (ok, command) in cases {
            let mut state = make_state();
            state
                .hooks
                .stop_hooks
                .push(explicit_verification_hook("quality", "./quality-gate"));
            state
                .stall
                .tool_call_records
                .push(executed_record("write_file", true, None));
            state.stall.tool_call_records.push(executed_record(
                "bash",
                true,
                Some(r#"{"command":"./quality-gate"}"#),
            ));
            state
                .stall
                .tool_call_records
                .push(executed_record("bash", ok, Some(command)));

            assert_eq!(
                missing_explicit_verification_hooks(&state).unwrap(),
                Some(vec![])
            );
        }
    }

    #[test]
    fn explicit_contract_reports_incomplete_after_bounded_recovery() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.verification_retries = 1;

        assert!(!enforce_explicit_verification_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(
            state
                .final_text
                .contains("Required checks did not complete successfully")
        );
    }

    #[test]
    fn explicit_contract_requires_every_declared_obligation() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("unit", "./unit-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"./quality-gate"}"#),
        ));

        assert_eq!(
            missing_explicit_verification_hooks(&state).unwrap(),
            Some(vec!["unit".to_string()])
        );
    }

    #[test]
    fn advisory_hook_does_not_create_a_terminal_gate() {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(astra_turn_core::stop_hooks::StopHook {
                authoritative: false,
                ..explicit_verification_hook("discovery", "./quality-gate")
            });
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state
            .stall
            .tool_call_records
            .push(executed_record("read_file", true, None));

        assert_eq!(missing_explicit_verification_hooks(&state).unwrap(), None);
        assert!(!enforce_explicit_verification_before_text_completion(
            &mut state
        ));
    }

    #[test]
    fn terminal_completion_action_does_not_repeat_a_successful_task_action() {
        for intent in [
            astra_config::user_profile::WorkspaceMutationIntent::Unknown,
            astra_config::user_profile::WorkspaceMutationIntent::MayMutate,
        ] {
            let mut state = make_state();
            state.turn_intent = Some(
                astra_config::user_profile::TurnIntent::default().with_workspace_mutation(intent),
            );
            state.stall.tool_call_records.push(executed_record(
                "bash",
                true,
                Some(r#"{"command":"git worktree list"}"#),
            ));

            assert_eq!(
                pending_completion_action(&state).unwrap(),
                None,
                "intent={intent:?}: a successful task-facing action already spent the fallback obligation"
            );
        }
    }

    #[test]
    fn completion_action_hint_keeps_explicit_actions_generic() {
        let explicit = completion_action_hint(&CompletionAction::ExplicitVerification {
            missing_labels: vec!["quality".into(), "unit".into()],
        });
        assert_eq!(explicit["reason_code"], "explicit_verification_missing");
        assert_eq!(
            explicit["missing_labels"],
            serde_json::json!(["quality", "unit"])
        );
        assert!(
            explicit["accepted_action_shapes"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
    }

    #[test]
    fn opaque_writer_does_not_create_a_terminal_obligation_without_receipt() {
        let mut state = make_state();
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"python3 -c \"open('/workspace/out','w').write('x')\""}"#),
        ));

        assert!(!state.task_profile.mutates_workspace);
        assert_eq!(pending_completion_action(&state).unwrap(), None);
    }

    #[test]
    fn opaque_writer_after_positive_write_keeps_the_latest_observation_barrier() {
        let mut state = make_state();
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            false,
            Some(r#"{"command":"opaque-unknown-command"}"#),
        ));

        assert!(!state.task_profile.mutates_workspace);
    }

    fn completed_explicit_check_state(extra_check: Option<&str>) -> AgenticLoopState {
        let mut state = make_state();
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("completed", "cargo test"));
        if let Some(command) = extra_check {
            state
                .hooks
                .stop_hooks
                .push(explicit_verification_hook("additional", command));
        }
        let args = serde_json::json!({"command":"cargo test"}).to_string();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(args.clone()),
            runtime_args_full: Some(args),
            result_class: Some("success".into()),
            exit_semantics: Some("success".into()),
            ..Default::default()
        });
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: CompletionAction::ExplicitVerification {
                    missing_labels: vec!["completed".into()],
                },
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        state
    }

    #[test]
    fn completed_check_cannot_skip_an_independent_declared_check() {
        let mut state = completed_explicit_check_state(Some("./quality-gate"));

        assert!(matches!(
            pending_completion_action(&state).unwrap(),
            Some(CompletionAction::ExplicitVerification { .. })
        ));
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_some()
        );
    }

    #[test]
    fn completed_declared_checks_return_to_the_canonical_work_owner() {
        let mut state = completed_explicit_check_state(Some("cargo test"));

        assert_eq!(pending_completion_action(&state).unwrap(), None);
        advance_completion_action_window_after_tool_round_for_work_state(&mut state, true);

        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[test]
    fn completion_action_matches_only_declared_verification_and_not_reads() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let action = pending_completion_action(&state)
            .unwrap()
            .expect("verification should be pending");

        let verify = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"./quality-gate\"}"}
        });
        let read = serde_json::json!({
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        });
        assert!(completion_action_match_label(&state, &action, &verify).is_some());
        assert!(!completion_action_match_label(&state, &action, &read).is_some());
    }

    #[test]
    fn completion_action_admission_consumes_any_request_but_executes_only_observation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let read = serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"cargo test\"}"}
        });
        let external_write = serde_json::json!({
            "type": "function",
            "function": {"name": "github", "arguments": "{}"}
        });
        let admission = ToolCallAdmission {
            admitted: ordinary_admitted([read.clone(), external_write.clone()]),
            rejected: Vec::new(),
            completion_action_applied: false,
        };

        let admission =
            apply_completion_action_admission(&mut state, admission, &[read, external_write]);

        assert_eq!(admission.admitted.len(), 1);
        assert_eq!(
            admission.admitted[0].logical_target_call()["function"]["name"].as_str(),
            Some("bash")
        );
        assert_eq!(admission.rejected.len(), 1);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("window remains auditable until settlement");
        assert!(window.consumed);
        assert!(window.matched);
        assert!(
            !state.hooks.completion_settlement.text_only,
            "pre-execution admission must not make its own legal action look like a wrap-up violation"
        );
    }

    #[test]
    fn non_executed_completion_mismatch_allows_one_idempotent_correction() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let wrong = serde_json::json!({
            "id": "settle-too-early",
            "type": "function",
            "function": {"name": "settle_work_item", "arguments": "{}"}
        });
        let max_before = state.max_turns;
        let remaining_before = state.remaining_turns;

        let first = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: ordinary_admitted([wrong.clone()]),
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&wrong),
        );

        assert!(first.admitted.is_empty());
        assert_eq!(first.rejected.len(), 1);
        let rejection: serde_json::Value =
            serde_json::from_str(&first.rejected[0].result).expect("structured mismatch");
        assert_eq!(rejection["error_kind"], "completion_action_mismatch");
        assert_eq!(rejection["retryable"], true);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("the executable action remains available");
        assert!(!window.consumed);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 0);
        assert_eq!(state.max_turns, max_before);
        assert_eq!(state.remaining_turns, remaining_before);

        let max_after_first = state.max_turns;
        let remaining_after_first = state.remaining_turns;
        let repeated_same_boundary =
            apply_completion_action_admission(&mut state, first, std::slice::from_ref(&wrong));
        assert!(repeated_same_boundary.completion_action_applied);
        assert_eq!(state.max_turns, max_after_first);
        assert_eq!(state.remaining_turns, remaining_after_first);
        assert!(
            !state
                .hooks
                .completion_settlement
                .completion_action_window
                .as_ref()
                .expect("same boundary is idempotent")
                .consumed
        );

        let read = serde_json::json!({
            "id": "corrected-observation",
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"cargo test\"}"}
        });
        let corrected = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: ordinary_admitted([read.clone()]),
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&read),
        );
        assert_eq!(admitted_logical_calls(&corrected), vec![read]);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("executed action remains auditable");
        assert!(window.consumed);
        assert!(window.matched);
        assert_eq!(window.attempts_remaining, 0);
        assert_eq!(state.max_turns, max_after_first);
        assert_eq!(state.remaining_turns, remaining_after_first);
    }

    #[test]
    fn second_completion_mismatch_is_terminal_without_more_headroom() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let wrong = |id: &str| {
            serde_json::json!({
                "id": id,
                "type": "function",
                "function": {"name": "settle_work_item", "arguments": "{}"}
            })
        };
        let first_call = wrong("wrong-one");
        let _ = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: ordinary_admitted([first_call.clone()]),
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&first_call),
        );
        let max_after_correction = state.max_turns;
        let remaining_after_correction = state.remaining_turns;
        let second_call = wrong("wrong-two");
        let second = apply_completion_action_admission(
            &mut state,
            ToolCallAdmission {
                admitted: ordinary_admitted([second_call.clone()]),
                rejected: Vec::new(),
                completion_action_applied: false,
            },
            std::slice::from_ref(&second_call),
        );

        let rejection: serde_json::Value =
            serde_json::from_str(&second.rejected[0].result).expect("structured mismatch");
        assert_eq!(rejection["retryable"], false);
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("terminal mismatch remains auditable");
        assert!(window.consumed);
        assert!(!window.matched);
        assert_eq!(state.max_turns, max_after_correction);
        assert_eq!(state.remaining_turns, remaining_after_correction);
    }

    #[test]
    fn matching_call_rejected_by_ordinary_admission_consumes_action_attempt() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let read = serde_json::json!({
            "id": "denied-observation",
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"cargo test\"}"}
        });
        let max_before = state.max_turns;
        let admission = ToolCallAdmission {
            admitted: Vec::new(),
            rejected: vec![RejectedToolCall::ordinary(
                read.clone(),
                r#"{"status":"rejected","error_kind":"permission_denied"}"#.into(),
            )],
            completion_action_applied: false,
        };

        let result =
            apply_completion_action_admission(&mut state, admission, std::slice::from_ref(&read));

        assert_eq!(result.rejected.len(), 1);
        assert!(result.rejected[0].result.contains("permission_denied"));
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("failed attempt remains auditable");
        assert!(window.consumed);
        assert!(!window.matched);
        assert_eq!(window.mismatch_corrections_remaining, 1);
        assert_eq!(state.max_turns, max_before);
    }

    #[test]
    fn post_mutation_action_does_not_accept_external_or_unknown_success() {
        assert!(crate::turn::tool_side_effects::tool_call_may_observe_workspace("read_file", None));
        assert!(
            crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "bash",
                Some(&serde_json::json!({"command": "git diff --stat"}))
            )
        );
        assert!(
            !crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "github",
                Some(&serde_json::json!({"action": "comment"}))
            )
        );
        assert!(
            !crate::turn::tool_side_effects::tool_call_may_observe_workspace("unknown_tool", None)
        );
    }

    #[test]
    fn completion_action_boundary_is_terminal_after_unmatched_attempt() {
        let mut state = make_state();
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 1,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: false,
            });

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::TerminalIncomplete
        );
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn completion_action_boundary_retries_once_after_plain_text_without_action() {
        let mut state = make_state();
        state.final_text = "the provider's untyped summary".to_string();
        state.hooks.completion_settlement.latest_provider_text = Some(state.final_text.clone());
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 1,
                mismatch_corrections_remaining: 1,
                consumed: false,
                matched: false,
            });
        let max_before = state.max_turns;
        let remaining_before = state.remaining_turns;

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::Retry
        );
        assert_eq!(state.max_turns, max_before);
        assert_eq!(state.remaining_turns, remaining_before);
        assert!(state.final_text.is_empty());
        assert!(state.interruption.is_none());
        assert!(
            state
                .hooks
                .completion_settlement
                .latest_provider_text
                .is_none()
        );
        let window = state
            .hooks
            .completion_settlement
            .completion_action_window
            .as_ref()
            .expect("the typed action remains available for the retry");
        assert!(!window.consumed);
        assert!(!window.matched);
        assert_eq!(window.attempts_remaining, 1);
        assert_eq!(window.mismatch_corrections_remaining, 0);
        let settlement = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::FinalAnswerSettlement)
            .expect("the retry must be projected as required runtime context");
        assert_eq!(
            settlement.payload["signal"],
            "completion_action_no_tool_retry"
        );
        assert_eq!(
            settlement.payload["execution_authority"],
            "one_matching_action"
        );

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::TerminalIncomplete
        );
        assert!(state.final_text.contains("not executed"));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn completion_action_boundary_is_terminal_when_obligation_remains() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::TerminalIncomplete
        );
        assert!(state.final_text.contains("unverified"));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[test]
    fn completion_action_boundary_clears_after_obligation_settles() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"cargo test"}"#),
        ));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 0,
                mismatch_corrections_remaining: 1,
                consumed: true,
                matched: true,
            });

        assert_eq!(
            enforce_completion_action_window_before_text_completion(&mut state),
            CompletionActionBoundary::Settled
        );
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[tokio::test]
    async fn terminal_interruption_does_not_reopen_workspace_completion_retry() {
        let mut host = MockHost::new(vec![text_result(
            "partial provider result",
            20,
            10,
            Some(30),
        )]);
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.interruption = Some(InterruptionRecord::new(
            InterruptionKind::ExecutionIncomplete,
            ResumeAction::ContinueImmediately,
            interruption_state_summary(&state, Some("provider output cap exhausted".to_string())),
        ));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("a terminal interruption should render the partial result");

        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(host.turn_count(), 1, "interruption must not reopen the LLM");
        assert_eq!(state.final_text, "partial provider result");
        assert_eq!(
            state
                .interruption
                .as_ref()
                .and_then(|record| record.error_detail.as_deref()),
            Some("provider output cap exhausted")
        );
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
    }

    #[tokio::test]
    async fn failed_probes_preserve_delivery_without_completion_rounds() {
        for attempts in [1, 2, 4] {
            let mut responses = Vec::new();
            for attempt in 0..attempts {
                let mut probe = make_edge_tool("bash", "probe failed");
                probe.request_id = format!("probe-{attempt}");
                probe.args = serde_json::json!({"command": "probe"});
                probe.status = "failed".into();
                responses.push(edge_tool_result(vec![probe], 20, 10, Some(30)));
            }
            let answer = "The requested analysis is available; the optional probe failed and was not verified.";
            responses.push(text_result(answer, 20, 10, Some(30)));
            let mut host = MockHost::new(responses).with_valid_tools(&["bash"]);
            let mut state = make_state();

            let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

            assert!(outcome.is_ok(), "{outcome:?}");
            assert_eq!(host.turn_count(), attempts + 1);
            assert_eq!(state.final_text, answer);
            assert!(state.interruption.is_none());
            assert_eq!(host.rendered_final_text, vec![answer.to_string()]);
            for request in host.executed_messages.iter().skip(1) {
                assert!(
                    request.iter().any(|message| {
                        message["role"] == "tool"
                            && message["content"]
                                .as_str()
                                .is_some_and(|content| content.contains("probe failed"))
                    }),
                    "failed tool feedback must reach the following model request"
                );
            }
            let records = &state.stall.tool_call_records;
            assert_eq!(records.len(), attempts);
            assert!(
                records
                    .iter()
                    .all(|record| !record.ok && record.was_executed())
            );
        }
    }

    #[test]
    fn policy_advisories_do_not_open_an_acceptance_tool_boundary() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.final_text = "The requested API is fully fixed.".into();
        let max_turns = state.max_turns;
        let remaining_turns = state.remaining_turns;
        state.stall.active_policy_feedback = serde_json::from_value(serde_json::json!({
            "state": "evaluated",
            "schema_version": astra_turn_core::context_feedback::RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            "recovery": null,
            "revision": 3,
            "evaluated_at_round": 8,
            "subject": {"kind": "run"},
            "entries": [{
                "signal": "round_activity",
                "stage": "observe",
                "observed_at_round": 8,
                "evidence_count": 8,
                "recommendation": "review_task_progress"
            }]
        }))
        .expect("valid policy feedback");

        // Runtime-policy signals are alerts.  Without a caller-declared
        // verification contract they must not create an actionful retry or
        // project a text-only boundary while ordinary budget remains.
        assert!(!state.hooks.completion_settlement.text_only);
        assert_eq!(state.max_turns, max_turns);
        assert_eq!(state.remaining_turns, remaining_turns);
        assert!(state.volatile_pending.iter().all(|entry| {
            entry
                .payload
                .get("schema")
                .and_then(serde_json::Value::as_str)
                != Some("acceptance_reconciliation_required.v1")
        }));
    }

    fn structured_mutating_profile() -> astra_turn_core::chat_turn_heuristics::TaskExecutionProfile
    {
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            true,
            false,
            astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
        )
    }

    fn mark_must_mutate(state: &mut AgenticLoopState) {
        state.task_profile = structured_mutating_profile();
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::MustMutate,
            ),
        );
    }

    fn recorded_tool_names(state: &AgenticLoopState) -> Vec<String> {
        state
            .telemetry
            .turn_trace_collector
            .as_ref()
            .expect("test must install a turn trace collector")
            .finalize()
            .tools
            .visible_tools
            .into_iter()
            .map(|tool| tool.tool_name)
            .collect()
    }

    fn install_tool_trace_collector(state: &mut AgenticLoopState) {
        state.telemetry.turn_trace_collector = Some(
            crate::turn::turn_trace_collector::TurnTraceCollector::new("turn-1", "session-1"),
        );
    }

    #[test]
    fn local_runtime_feedback_uses_monotonic_ingested_run_usage() {
        let mut state = make_state();
        state.total_prompt = 100;
        state.total_cache_read = 300;
        state.total_cache_creation = 4;
        state.total_completion = 20;
        let first_turn = ChatTurnSseAccum {
            has_usage: true,
            prompt_tokens: 100,
            cache_read_tokens: 300,
            cache_creation_tokens: 4,
            completion_tokens: 20,
            ..Default::default()
        };
        let first = runtime_feedback_run_usage(&state, &first_turn).expect("first run usage");

        state.total_prompt += 25;
        state.total_cache_read += 75;
        state.total_cache_creation += 1;
        state.total_completion += 5;
        let second_turn_with_admission_sidecar = ChatTurnSseAccum {
            has_usage: true,
            prompt_tokens: 25,
            cache_read_tokens: 75,
            cache_creation_tokens: 1,
            completion_tokens: 5,
            ..Default::default()
        };
        let second = runtime_feedback_run_usage(&state, &second_turn_with_admission_sidecar)
            .expect("second run usage");

        assert_eq!(first.prompt, 100);
        assert_eq!(first.cache_read, 300);
        assert_eq!(second.prompt, 125);
        assert_eq!(second.cache_read, 375);
        assert_eq!(second.cache_creation, 5);
        assert_eq!(second.completion, 25);
        assert!(second.total() > first.total());
    }

    #[test]
    fn record_edge_tool_selection_records_unique_callback_names() {
        let mut state = make_state();
        install_tool_trace_collector(&mut state);
        let callbacks = vec![
            make_edge_tool(" read_file ", "ok"),
            make_edge_tool("read_file", "ok"),
            make_edge_tool(" bash ", "ok"),
        ];
        record_edge_tool_selection(&mut state, &callbacks, 0);

        assert_eq!(recorded_tool_names(&state), vec!["read_file", "bash"]);
        assert_eq!(
            state
                .telemetry
                .turn_trace_collector
                .as_ref()
                .expect("collector")
                .finalize()
                .tools
                .tools_available,
            2,
            "the visible-name count must also floor tools_available"
        );
    }

    #[test]
    fn record_tool_selection_defers_empty_tool_bearing_trace_until_callbacks_resolve() {
        let mut state = make_state();
        install_tool_trace_collector(&mut state);
        let turn_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                has_tool_calls: true,
                tool_calls: vec![serde_json::json!({
                    "id": "call-read",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                })],
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: None,

            error_kind: None,
        };

        record_tool_selection(&mut state, &turn_result, 0);

        assert!(
            !state
                .telemetry
                .turn_trace_collector
                .as_ref()
                .expect("collector")
                .has_tool_trace(),
            "an empty provider-side tool round must leave the trace unset for callback resolution"
        );
    }

    #[test]
    fn record_tool_selection_does_not_infer_names_without_tool_evidence() {
        let mut state = make_state();
        install_tool_trace_collector(&mut state);
        let turn_result = HostTurnResult {
            accum: ChatTurnSseAccum::default(),
            ttft_ms: None,

            error_kind: None,
        };
        record_tool_selection(&mut state, &turn_result, 0);
        assert!(recorded_tool_names(&state).is_empty());
    }

    #[test]
    fn alert_dispatch_session_id_requires_real_session_identity() {
        assert_eq!(alert_dispatch_session_id(None), None);
        assert_eq!(alert_dispatch_session_id(Some("")), None);
        assert_eq!(alert_dispatch_session_id(Some("   ")), None);
        assert_eq!(
            alert_dispatch_session_id(Some("  session-123  ")).as_deref(),
            Some("session-123")
        );
    }

    #[test]
    fn spill_summary_does_not_promote_read_paths_into_prompt_facing_memory() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "tool_calls": [
                {
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"rust/astra/src/bridge/mod.rs\"}"
                    }
                },
                {
                    "function": {
                        "name": "str_replace",
                        "arguments": "{\"path\":\"crates/runtime/src/bridge/mod.rs\",\"old_str\":\"a\",\"new_str\":\"b\"}"
                    }
                }
            ]
        })];

        let summary = build_spill_summary(&messages);

        assert!(
            !summary.contains("rust/astra"),
            "read-only paths must not become prompt-facing memory: {summary}"
        );
        assert!(
            summary.contains("crates/runtime/src/bridge/mod.rs"),
            "mutated files should remain visible: {summary}"
        );
        assert!(summary.contains("- read_file"), "{summary}");
        assert!(summary.contains("- str_replace"), "{summary}");
    }

    struct StubRunControlProvider {
        polls: Mutex<VecDeque<UserIntentPoll>>,
        poll_calls: Mutex<Vec<usize>>,
        poll_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        apply_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        released: Mutex<Vec<usize>>,
        release_failures: Mutex<usize>,
        apply_outcomes: Mutex<VecDeque<crate::turn::run_control::UserIntentApplyAck>>,
        provider_authorization_calls: std::sync::atomic::AtomicUsize,
        provider_authorizations: Mutex<VecDeque<ProviderBoundaryAuthorization>>,
    }

    impl StubRunControlProvider {
        fn new(polls: Vec<UserIntentPoll>) -> Self {
            Self {
                polls: Mutex::new(VecDeque::from(polls)),
                poll_calls: Mutex::new(Vec::new()),
                poll_gate: None,
                apply_gate: None,
                released: Mutex::new(Vec::new()),
                release_failures: Mutex::new(0),
                apply_outcomes: Mutex::new(VecDeque::new()),
                provider_authorization_calls: std::sync::atomic::AtomicUsize::new(0),
                provider_authorizations: Mutex::new(VecDeque::new()),
            }
        }

        fn with_release_failures(polls: Vec<UserIntentPoll>, release_failures: usize) -> Self {
            let mut provider = Self::new(polls);
            *provider.release_failures.get_mut() = release_failures;
            provider
        }

        fn with_terminal_release(polls: Vec<UserIntentPoll>) -> Self {
            let mut provider = Self::new(polls);
            provider
                .apply_outcomes
                .get_mut()
                .push_back(crate::turn::run_control::UserIntentApplyAck::RunTerminalReturned);
            provider
        }

        fn with_provider_authorizations(
            self,
            decisions: Vec<ProviderBoundaryAuthorization>,
        ) -> Self {
            *self
                .provider_authorizations
                .try_lock()
                .expect("new provider authorization queue is uncontended") =
                VecDeque::from(decisions);
            self
        }

        async fn poll_call_count(&self) -> usize {
            self.poll_calls.lock().await.len()
        }
    }

    struct DirectErrorHost {
        error: Option<astra_core::ClassifiedError>,
        valid_tools: HashSet<String>,
        calls: usize,
        time_remaining: Option<astra_turn_types::ExecutionTimeRemaining>,
    }

    struct BoundaryDecisionRunControl {
        decision: Result<ProviderBoundaryAuthorization, String>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "harness")]
    struct PreLlmBlockingHarness;

    #[cfg(feature = "harness")]
    impl astra_harness::HarnessKernel for PreLlmBlockingHarness {
        fn snapshot(&self) -> Option<astra_harness::RuntimeSnapshot> {
            None
        }

        fn on_record(&self, record: &astra_harness::DecisionRecord) -> astra_harness::HookVerdict {
            if record.point == astra_harness::HookPoint::PreLlmRequest {
                astra_harness::HookVerdict::Block {
                    reason: "deterministic pre-LLM rejection".to_string(),
                }
            } else {
                astra_harness::HookVerdict::Continue
            }
        }
    }

    #[async_trait]
    impl RunStatusProvider for BoundaryDecisionRunControl {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<crate::turn::run_control::RunControlStatus>, String> {
            Ok(None)
        }
    }

    #[async_trait]
    impl UserIntentProvider for BoundaryDecisionRunControl {
        async fn authorize_provider_boundary(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _authority: UserIntentAdmissionAuthority,
        ) -> Result<ProviderBoundaryAuthorization, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision.clone()
        }

        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            UserIntentPoll {
                next_cursor: after_event_index,
                ..UserIntentPoll::default()
            }
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _event_indices: &[usize],
            _authority: UserIntentAdmissionAuthority,
            _invocation_identity: Option<
                crate::turn::run_control::UserIntentInvocationApplication<'_>,
            >,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    impl DirectErrorHost {
        fn new(error: astra_core::ClassifiedError) -> Self {
            Self {
                error: Some(error),
                valid_tools: HashSet::new(),
                calls: 0,
                time_remaining: None,
            }
        }

        fn turn_count(&self) -> usize {
            self.calls
        }
    }

    #[async_trait]
    impl AgenticLoopHost for DirectErrorHost {
        fn execution_time_budget_remaining(
            &self,
        ) -> Option<astra_turn_types::ExecutionTimeRemaining> {
            self.time_remaining
        }
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            self.calls = self.calls.saturating_add(1);
            Err(self.error.take().expect("test host called once"))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, _line: String) {}

        fn is_quiet(&self) -> bool {
            true
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }
    }

    #[async_trait]
    impl RunStatusProvider for StubRunControlProvider {
        async fn control_status(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<crate::turn::run_control::RunControlStatus>, String> {
            Ok(None)
        }
    }

    #[async_trait]
    impl UserIntentProvider for StubRunControlProvider {
        async fn authorize_provider_boundary(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _authority: UserIntentAdmissionAuthority,
        ) -> Result<ProviderBoundaryAuthorization, String> {
            self.provider_authorization_calls
                .fetch_add(1, Ordering::SeqCst);
            Ok(self
                .provider_authorizations
                .lock()
                .await
                .pop_front()
                .unwrap_or(ProviderBoundaryAuthorization::Authorized))
        }

        async fn poll_user_intents(
            &self,
            _user_id: &str,
            _run_id: &str,
            after_event_index: usize,
        ) -> UserIntentPoll {
            self.poll_calls.lock().await.push(after_event_index);
            if let Some((entered, release)) = &self.poll_gate {
                entered.notify_one();
                release.notified().await;
            }
            self.polls
                .lock()
                .await
                .pop_front()
                .unwrap_or(UserIntentPoll {
                    next_cursor: after_event_index,
                    snapshot_has_more: false,
                    snapshot_page_fact_count: 0,
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: None,
                })
        }

        async fn mark_user_intents_applied(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            event_indices: &[usize],
            _authority: UserIntentAdmissionAuthority,
            _invocation_identity: Option<
                crate::turn::run_control::UserIntentInvocationApplication<'_>,
            >,
        ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
            if let Some((entered, release)) = &self.apply_gate {
                entered.notify_one();
                release.notified().await;
            }
            {
                let mut outcomes = self.apply_outcomes.lock().await;
                if let Some(outcome) = outcomes.front().copied() {
                    if outcome != crate::turn::run_control::UserIntentApplyAck::RunTerminalReturned
                    {
                        outcomes.pop_front();
                    }
                    return Ok(outcome);
                }
            }
            let mut release_failures = self.release_failures.lock().await;
            if *release_failures > 0 {
                *release_failures -= 1;
                return Err("release failed".to_string());
            }
            drop(release_failures);
            self.released.lock().await.extend_from_slice(event_indices);
            Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
        }
    }

    #[test]
    fn observe_turn_end_without_tools_records_outer_session_turn() {
        let mut state = make_state();
        state.session_turn = 6;
        state.policy_model_name = Some("test-model".to_string());
        state.max_turns = 20;
        state.remaining_turns = 4;
        state.total_prompt = 10_000;
        state.total_completion = 20_000;
        state.total_cache_read = 30_000;
        state.total_cache_creation = 40_000;
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_hub = Some(Arc::new(hub));
        state.telemetry.observability_session = Some(session.clone());

        let turn_start_time = Instant::now() - Duration::from_millis(25);
        observe_turn_end_without_tools(&mut state, turn_start_time, Some(7));

        let guard = session.read().unwrap();
        assert_eq!(guard.turn_timings.len(), 1);
        assert_eq!(guard.turn_timings[0].turn, 6);
    }

    #[test]
    fn runtime_feedback_uses_typed_manifest_prompt_cache_identity() {
        let identity = astra_turn_types::PromptCacheIdentityV1::from_prefixes(
            &[serde_json::json!({"role": "system", "content": "stable"})],
            &[],
            "openai-stable-prefix-v1",
        )
        .expect("valid cache identity");
        let manifest = serde_json::json!({
            "wire": {
                "fingerprint": {
                    "prompt_cache_identity": identity.clone()
                }
            }
        });
        assert_eq!(
            prompt_cache_identity_from_manifest(Some(&manifest)),
            Some(identity)
        );

        let missing = serde_json::json!({"wire": {"fingerprint": {}}});
        let malformed = serde_json::json!({
            "wire": {"fingerprint": {"prompt_cache_identity": {"content_id": "invalid"}}}
        });
        assert_eq!(prompt_cache_identity_from_manifest(Some(&missing)), None);
        assert_eq!(prompt_cache_identity_from_manifest(Some(&malformed)), None);
    }

    #[test]
    fn runtime_feedback_uses_manifest_stable_prefix_estimate_without_inference() {
        let manifest = serde_json::json!({
            "wire": {
                "cache_estimate": {
                    "eligible_tokens": 7_919,
                    "basis": "provider_visible_stable_prefix_only"
                }
            }
        });
        assert_eq!(
            prompt_cache_eligible_tokens_from_manifest(Some(&manifest)),
            Some(7_919)
        );
        assert_eq!(
            prompt_cache_eligible_tokens_from_manifest(Some(&serde_json::json!({
                "wire": {"cache_estimate": {"eligible_tokens": "7919"}}
            }))),
            None,
            "malformed measurements stay unknown instead of being coerced"
        );
    }

    #[test]
    fn bridge_compaction_observations_update_turn_trace_and_step_audit() {
        let mut state = make_state();
        state.max_turn_input_tokens = 20_000;
        let observations = vec![
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation {
                id: "initial".to_string(),
                kind: CompactionKind::WireAssembly,
                tier: CompactionTier::CompactHistory,
                messages_before: 18,
                messages_after: 10,
                tokens_before: 15_000,
                tokens_after: 9_000,
                tokens_saved: 6_000,
                post_compaction_target_tokens: None,
                effectiveness: astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Unmeasured,
            },
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation {
                id: "context_window_retry:1:1".to_string(),
                kind: CompactionKind::WireContextRetry,
                tier: CompactionTier::AggressivePrune,
                messages_before: 12,
                messages_after: 6,
                tokens_before: 10_000,
                tokens_after: 5_000,
                tokens_saved: 5_000,
                post_compaction_target_tokens: None,
                effectiveness: astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Unmeasured,
            },
        ];

        let mut host = MockHost::new(Vec::new());
        record_context_compactions(&mut host, &mut state, &observations);

        assert!(state.context_compression_triggered);
        let compactions: Vec<_> = state
            .step_recorder
            .events()
            .iter()
            .filter(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::CompactionFired
            })
            .collect();
        assert_eq!(compactions.len(), 2);
        assert_eq!(
            compactions[0].payload.as_ref().unwrap()["results_compacted"],
            8
        );
        assert_eq!(
            compactions[0].payload.as_ref().unwrap()["tokens_saved"],
            6_000
        );
        assert_eq!(compactions[0].payload.as_ref().unwrap()["pressure"], 0.75);
        assert_eq!(
            compactions[1].payload.as_ref().unwrap()["results_compacted"],
            6
        );
        assert_eq!(state.compact_tier_applied, CompactionTier::AggressivePrune);
        assert_eq!(host.compaction_events.len(), 2);
        assert_eq!(host.compaction_events[0].kind, CompactionKind::WireAssembly);
        assert_eq!(host.compaction_events[0].tokens_freed, 6_000);
        assert_eq!(
            host.compaction_events[1].kind,
            CompactionKind::WireContextRetry
        );
    }

    // PR 5a: the turn loop must invoke host.on_turn_completed
    // exactly once per successful ingested turn, AFTER run_id is
    // populated by ingest but BEFORE tool execution / side effects.

    #[tokio::test]
    async fn model_item_identity_is_assigned_before_dispatch_and_not_reused_on_restart() {
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut state = make_state();
            let mut host = MockHost::new(vec![text_result("identical answer", 10, 5, Some(1))]);
            execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(true))
                .await
                .unwrap();
            let id = host.executed_model_item_ids[0]
                .clone()
                .expect("identity precedes host dispatch");
            Uuid::parse_str(&id).expect("physical identity is not a round index");
            assert_eq!(state.final_text_model_item_id.as_ref(), Some(&id));
            assert!(
                state
                    .messages
                    .iter()
                    .any(|message| astra_turn_types::model_item_id(message) == Some(id.as_str()))
            );
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn turn_completed_hook_fires_once_on_successful_turn() {
        let mut state = make_state();
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        let _ = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            host.turn_completed_run_ids.len(),
            1,
            "hook must fire exactly once per successful turn"
        );
    }

    #[tokio::test]
    async fn turn_completed_hook_observes_ingested_run_id() {
        // Precondition: ingest_agentic_turn_stream populates
        // state.current_run_id before the hook runs. The hook must
        // see whatever ingest left there — not some stale pre-turn
        // value, not None from before ingest.
        let mut state = make_state();
        // Pretend a previous turn set this; ingest would normally
        // overwrite, but for a turn without server-assigned run_id
        // the value flows through unchanged. The assertion below
        // is simply "whatever state.current_run_id is post-ingest,
        // the hook sees the same thing".
        state.current_run_id = Some("pre-existing-run".to_string());
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        let _ = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            host.turn_completed_run_ids,
            vec![state.current_run_id.clone()],
            "hook must observe post-ingest run_id, matching current state"
        );
    }

    #[tokio::test]
    async fn turn_completed_hook_does_not_fire_on_fatal_ingest_outcome() {
        // Even when execute_turn itself returns Ok, the SSE stream
        // may carry an error that ingest classifies as Fatal (rate
        // limit, context window, provider 500). A Fatal ingest
        // leaves state.messages / current_run_id only partially
        // updated; capturing would poison any downstream sink with
        // a corrupt prefix. The hook MUST NOT fire on Fatal.
        let mut state = make_state();
        let error_result = HostTurnResult {
            accum: ChatTurnSseAccum {
                error_message: Some("Error: simulated fatal".into()),
                has_usage: false,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(1),

            error_kind: Some(astra_core::ErrorKind::RateLimit),
        };
        let mut host = MockHost::new(vec![error_result.into()]);

        let _ = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        assert!(
            host.turn_completed_run_ids.is_empty(),
            "hook must not fire when ingest returns Fatal"
        );
    }

    #[tokio::test]
    async fn final_guidance_fence_without_input_closes_the_inner_step() {
        let mut state = make_state();
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "typed admission fence",
        )
        .with_details_json(
            serde_json::json!({
                "source": crate::turn::llm::durable::INFERENCE_LEDGER_ERROR_SOURCE,
                "scope_rejection": astra_services::InferenceScopeRejection::GuidancePending,
            })
            .to_string(),
        );
        let mut host = DirectErrorHost::new(error);
        let result =
            super::super::finalization::run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(result.is_err());
        assert_eq!(host.turn_count(), 1);
        let step = state
            .step_recorder
            .current_step()
            .expect("actual inner step began");
        assert!(step.execution.completed_at.is_some());
        use astra_pipeline::step_protocol::StepEventType;
        assert_eq!(
            state
                .step_recorder
                .events()
                .iter()
                .filter(|event| event.step_id == step.step_id()
                    && event.event_type == StepEventType::StepIncomplete)
                .count(),
            1
        );
        assert!(
            !state
                .step_recorder
                .events()
                .iter()
                .any(|event| event.step_id == "unknown")
        );
    }

    #[tokio::test]
    async fn guidance_admission_fence_reconciles_before_continuing() {
        // The host boundary fails after pre-request polling, as happens when
        // guidance races the final durable provider-attempt admission.
        for (reason, available, should_continue) in [
            (
                astra_services::InferenceScopeRejection::GuidancePending,
                true,
                true,
            ),
            (
                astra_services::InferenceScopeRejection::GuidancePending,
                false,
                false,
            ),
            (
                astra_services::InferenceScopeRejection::Unavailable,
                true,
                false,
            ),
        ] {
            let mut state = make_state();
            state.current_run_id = Some("run-guidance-race".into());
            state.current_run_owner_generation = Some(17);
            state.owner_user_id = Some("user-guidance-race".into());
            let pending = UserIntentPoll {
                next_cursor: 1,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "guidance-race".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "apply the revised objective"}),
                }],
                ..UserIntentPoll::default()
            };
            let provider = Arc::new(StubRunControlProvider::new(vec![
                UserIntentPoll::default(),
                if available {
                    pending
                } else {
                    UserIntentPoll::default()
                },
            ]));
            state.run_control = Some(provider.clone());
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "typed admission fence",
            )
            .with_details_json(
                serde_json::json!({
                    "source": crate::turn::llm::durable::INFERENCE_LEDGER_ERROR_SOURCE,
                    "scope_rejection": reason,
                })
                .to_string(),
            );
            let mut host = DirectErrorHost::new(error);
            let result = execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await;
            if should_continue {
                assert!(matches!(
                    result.unwrap(),
                    TurnExecutionControl::ContinueLoop
                ));
                assert_eq!(state.message, "apply the revised objective");
                assert_eq!(*provider.released.lock().await, vec![1]);
                assert!(state.interruption.is_none());
                assert!(state.final_text.is_empty());
                let mut resumed = MockHost::new(vec![text_result("revised result", 12, 3, None)]);
                let next = execute_turn_and_ingest_phase(
                    &mut resumed,
                    &mut state,
                    1,
                    TurnIterationPrep {
                        quiet: true,
                        turn_start_time: Instant::now(),
                    },
                )
                .await
                .expect("the same run can complete after applying guidance");
                assert!(!matches!(next, TurnExecutionControl::ContinueLoop));
                assert!(
                    resumed.executed_messages[0]
                        .iter()
                        .any(|message| message["role"] == "user"
                            && message["content"] == "apply the revised objective")
                );
                assert_eq!(
                    *provider.released.lock().await,
                    vec![1],
                    "guidance is applied once"
                );
            } else {
                assert!(result.is_err());
                assert!(provider.released.lock().await.is_empty());
            }
            assert_eq!(host.turn_count(), 1, "no stale request is retried in place");
        }
    }

    #[tokio::test]
    async fn exhausted_run_provider_recovery_uses_existing_final_window() {
        for (boundary, recovery_used, delivery, settlement_active, expected_settlement) in [
            (Some("run_work"), true, "none", false, true),
            (Some("run_work"), false, "none", false, false),
            (None, true, "none", false, false),
            (Some("run_work"), true, "text", false, false),
            (Some("run_work"), true, "tool", false, false),
            (Some("run_work"), true, "malformed", false, false),
            (Some("run_work"), true, "none", true, false),
        ] {
            let mut state = make_state();
            state.provider_adaptation.safe_recovery_attempted = recovery_used;
            state.hooks.completion_settlement.text_only = settlement_active;
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider work deadline",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {"execution_boundary": boundary},
                    "partial_full_text": if delivery == "text" { "already delivered" } else { "" },
                    "partial_reasoning": "provisional reasoning",
                    "tool_calls": match delivery {
                        "tool" => serde_json::json!([{"id": "selected-action"}]),
                        "malformed" => serde_json::Value::Null,
                        _ => serde_json::json!([]),
                    },
                })
                .to_string(),
            );
            let mut host = DirectErrorHost::new(error);
            host.time_remaining = Some(astra_turn_types::ExecutionTimeRemaining {
                work_remaining: Duration::from_secs(3),
                total_remaining: Duration::from_secs(33),
            });
            let result = execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await;
            assert_eq!(host.turn_count(), 1);
            if expected_settlement {
                assert!(matches!(result, Ok(TurnExecutionControl::ContinueLoop)));
                assert!(state.hooks.completion_settlement.text_only);
                assert!(state.budget_wrapup_injected);
                assert_eq!(
                    state.hooks.completion_settlement.wrapup_origin,
                    Some(super::super::host::BudgetWrapupOrigin::ExecutionDeadline)
                );
                assert!(
                    state
                        .hooks
                        .completion_settlement
                        .completion_action_window
                        .is_none()
                );
            } else if !recovery_used {
                assert!(matches!(result, Ok(TurnExecutionControl::ContinueLoop)));
                assert!(state.provider_adaptation.safe_recovery_pending);
                assert!(!state.budget_wrapup_injected);
            } else {
                assert!(result.is_err());
                assert!(!state.budget_wrapup_injected);
            }
        }
    }

    #[tokio::test]
    async fn turn_completed_hook_does_not_fire_when_execute_turn_errors() {
        // An empty MockHost returns BudgetExhausted on execute_turn.
        // The hook must NOT fire in the error path — we only want
        // to snapshot state after a successful response is ingested.
        let mut state = make_state();
        let mut host = MockHost::new(vec![]); // no turns queued

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;

        assert!(
            result.is_err(),
            "setup sanity: execute_turn should fail without queued results"
        );
        assert!(
            host.turn_completed_run_ids.is_empty(),
            "hook must not fire on execute_turn error"
        );
    }

    #[tokio::test]
    async fn direct_provider_rate_limit_error_records_interruption_and_cooldown() {
        let mut state = make_state();
        let mut host = DirectErrorHost::new(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::RateLimit,
            "provider returned 429",
        ));

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;

        assert!(result.is_err());
        let interruption = state
            .interruption
            .as_ref()
            .expect("direct provider 429 must be represented as an interruption");
        assert_eq!(interruption.kind, InterruptionKind::RateLimited);
        assert_eq!(state.llm_rounds_completed, 1);
        let metrics = state.rate_limit_cooldown.metrics();
        assert_eq!(metrics.total_429_errors, 1);
        assert_eq!(metrics.consecutive_errors, 1);
    }

    #[tokio::test]
    async fn text_only_authority_survives_only_an_admitted_provider_convergence() {
        let mut state = make_state();
        state.remaining_turns = 3;
        state.hooks.completion_settlement.text_only = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider action deadline",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {"phase": "semantic_progress"},
                    "partial_full_text": "",
                    "partial_reasoning": "provisional",
                    "tool_calls": []
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("action deadline should schedule bounded convergence");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.provider_adaptation.safe_recovery_pending);

        let mut rejected_state = make_state();
        rejected_state.remaining_turns = 3;
        rejected_state.hooks.completion_settlement.text_only = true;
        let mut rejected_host = DirectErrorHost::new(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider hard deadline without safe convergence evidence",
        ));
        assert!(
            execute_turn_and_ingest_phase(
                &mut rejected_host,
                &mut rejected_state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .is_err()
        );
        assert!(
            !rejected_state.hooks.completion_settlement.text_only,
            "a rejected convergence must not leak text-only authority into unrelated recovery"
        );
    }

    #[tokio::test]
    async fn empty_final_reporting_recovers_within_the_original_allowance() {
        let mut state = make_state();
        state.max_turns = 34;
        state.remaining_turns = 1;
        state.hooks.completion_settlement.text_only = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider convergence completed without visible text or a selected tool",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {
                        "scope": "provider_convergence",
                        "phase": "actionable_output"
                    },
                    "partial_full_text": "",
                    "partial_reasoning": "",
                    "tool_calls": [],
                    "provider_response": {"transport_success": true}
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("a transport-success empty final settlement should recover once");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert_eq!(state.max_turns, 34);
        assert_eq!(state.remaining_turns, 1);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.provider_adaptation.safe_recovery_attempted);
        assert!(state.provider_adaptation.safe_recovery_pending);
        assert!(state.interruption.is_none());
        let advisory = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["schema"] == "provider_safe_recovery.v1")
            .expect("bounded provider recovery advisory");
        assert_eq!(advisory.payload["execution_mode"], "text_only");
    }

    #[tokio::test]
    async fn repeated_final_text_only_empty_completion_is_typed_empty_completion() {
        let mut state = make_state();
        state.remaining_turns = 0;
        state.hooks.completion_settlement.text_only = true;
        state.provider_adaptation.safe_recovery_attempted = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider convergence completed without visible text or a selected tool",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {
                        "scope": "provider_convergence",
                        "phase": "actionable_output"
                    },
                    "partial_full_text": "",
                    "partial_reasoning": "",
                    "tool_calls": [],
                    "provider_response": {"transport_success": true}
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("clean empty exhaustion uses interrupted completion");
        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(matches!(
            state.interruption.as_ref().unwrap().resume_action,
            ResumeAction::RequiresIntervention { .. }
        ));

        let interruption = state
            .interruption
            .as_ref()
            .expect("repeated empty completion must be terminally classified");
        assert_eq!(interruption.kind, InterruptionKind::EmptyCompletion);
        assert!(!state.hooks.completion_settlement.text_only);
        assert_eq!(state.remaining_turns, 0);
    }

    #[tokio::test]
    async fn zero_budget_semantic_provider_deadline_remains_provider_deadline() {
        let mut state = make_state();
        state.remaining_turns = 0;
        state.hooks.completion_settlement.text_only = true;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider semantic progress deadline",
            )
            .with_details_json(
                serde_json::json!({
                    "deadline": {
                        "scope": "provider_inference",
                        "phase": "semantic_progress"
                    },
                    "partial_full_text": "",
                    "partial_reasoning": "provisional",
                    "tool_calls": []
                })
                .to_string(),
            ),
        );

        assert!(
            execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .is_err()
        );

        assert_eq!(
            state
                .interruption
                .as_ref()
                .expect("deadline interruption")
                .kind,
            InterruptionKind::ProviderDeadline
        );
        assert_eq!(state.remaining_turns, 0);
        assert!(!state.provider_adaptation.safe_recovery_pending);
    }

    #[tokio::test]
    async fn direct_stream_transport_with_reasoning_continues_once_without_replaying_actions() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::StreamTransport,
                "provider connection reset after provisional reasoning",
            )
            .with_details_json(
                serde_json::json!({
                    "partial_full_text": "",
                    "partial_reasoning": "working through the task",
                    "tool_calls": []
                })
                .to_string(),
            ),
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("an unambiguous transport interruption must schedule one recovery turn");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert!(state.provider_adaptation.safe_recovery_attempted);
        assert!(state.provider_adaptation.safe_recovery_pending);
        assert!(state.interruption.is_none());
        assert!(state.stall.tool_call_records.is_empty());
    }

    #[tokio::test]
    async fn direct_provider_admission_rejection_records_interruption_without_provider_cooldown() {
        let mut state = make_state();
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::RateLimit,
                "LLM provider admission rejected request before provider call",
            )
            .with_details_json(
                serde_json::json!({
                    "source": "llm_provider_admission",
                    "scope": "provider",
                    "limit": 20
                })
                .to_string(),
            ),
        );

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;

        assert!(result.is_err());
        let interruption = state
            .interruption
            .as_ref()
            .expect("admission rejection must be represented as an interruption");
        assert_eq!(interruption.kind, InterruptionKind::RateLimited);
        assert_eq!(state.llm_rounds_completed, 1);
        let metrics = state.rate_limit_cooldown.metrics();
        assert_eq!(
            metrics.total_429_errors, 0,
            "local admission rejection must not be counted as a provider 429"
        );
        assert_eq!(metrics.consecutive_errors, 0);
    }

    #[test]
    fn terminal_completion_disposition_uses_only_typed_settlement_authority() {
        let mut ordinary = make_state();
        assert_eq!(
            terminal_completion_disposition(&ordinary, false),
            TerminalCompletionDisposition::OrdinaryCompletionCandidate
        );

        ordinary.budget_wrapup_injected = true;
        ordinary.hooks.completion_settlement.text_only = true;
        ordinary.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        assert_eq!(
            terminal_completion_disposition(&ordinary, false),
            TerminalCompletionDisposition::RoundSliceTextDelivery
        );

        let mut committed_work = make_state();
        committed_work.budget_wrapup_injected = true;
        committed_work.hooks.completion_settlement.text_only = true;
        committed_work.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        install_committed_work_synthesis_wire_surface(&mut committed_work);
        assert_eq!(
            terminal_completion_disposition(&committed_work, true),
            TerminalCompletionDisposition::CommittedWorkSynthesis
        );

        let mut exact_action = make_state();
        exact_action.budget_wrapup_injected = true;
        exact_action.hooks.completion_settlement.text_only = true;
        exact_action.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        exact_action.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        exact_action
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        exact_action.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"cargo test"}"#),
        ));
        exact_action
            .hooks
            .completion_settlement
            .completion_action_window = Some(super::super::host::CompletionActionWindow {
            action: explicit_verification_action(&mut exact_action),
            attempts_remaining: 0,
            mismatch_corrections_remaining: 0,
            consumed: true,
            matched: true,
        });
        assert_eq!(
            terminal_completion_disposition(&exact_action, false),
            TerminalCompletionDisposition::SettledExactCompletionAction
        );

        let mut spent_generic_action = make_state();
        spent_generic_action.budget_wrapup_injected = true;
        spent_generic_action.hooks.completion_settlement.text_only = true;
        spent_generic_action
            .hooks
            .completion_settlement
            .wrapup_origin = Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        spent_generic_action
            .stall
            .tool_call_records
            .push(executed_record(
                "bash",
                true,
                Some(r#"{"command":"git worktree list"}"#),
            ));
        assert_eq!(
            pending_completion_action(&spent_generic_action).unwrap(),
            None,
            "the successful action must not manufacture another terminal tool allowance"
        );
        assert_eq!(
            terminal_completion_disposition(&spent_generic_action, false),
            TerminalCompletionDisposition::RoundSliceTextDelivery,
            "a generic completed action does not create additional completion debt"
        );
    }

    #[tokio::test]
    async fn reserved_final_reporting_without_unsettled_obligations_completes() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        let mut host = MockHost::new(vec![text_result(
            "Useful progress summary from the exhausted execution slice.",
            10,
            5,
            Some(1),
        )]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("forced round-slice settlement should remain a successful control transition");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert_eq!(
            state.final_text,
            "Useful progress summary from the exhausted execution slice."
        );
        assert!(!state.final_text.contains("Why stopped:"));
    }

    #[tokio::test]
    async fn committed_work_completion_synthesis_is_not_reclassified_as_round_slice_partial() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result(
            "Completed from the committed Work graph.",
            10,
            5,
            Some(1),
        )])
        .with_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("typed final synthesis should be ingested");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "the typed final-synthesis authority is one-shot"
        );
    }

    #[tokio::test]
    async fn final_work_synthesis_rechecks_run_authority_after_durable_work_read() {
        let mut state = make_state();
        state.current_run_id = Some("run-final-fence".into());
        state.current_session_id = Some("session-final-fence".into());
        state.owner_user_id = Some("user-final-fence".into());
        state.current_run_owner_generation = Some(9);
        install_committed_work_synthesis_wire_surface(&mut state);
        let provider = Arc::new(
            StubRunControlProvider::new(vec![UserIntentPoll::default(), UserIntentPoll::default()])
                .with_provider_authorizations(vec![
                    ProviderBoundaryAuthorization::Authorized,
                    ProviderBoundaryAuthorization::AuthorityLost {
                        reason: "owner lease expired during model inference".into(),
                    },
                ]),
        );
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![text_result(
            "Must not cross the lost Run boundary.",
            10,
            5,
            Some(1),
        )])
        .with_committed_work_synthesis();

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;

        let Err(error) = result else {
            panic!("lease loss after Work validation must fail closed");
        };
        assert_eq!(error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(host.committed_work_synthesis_checks, 1);
        assert_eq!(
            provider.provider_authorization_calls.load(Ordering::SeqCst),
            2,
            "the second authorization is the final Run terminal fence"
        );
        assert!(state.final_text.is_empty());
    }

    #[tokio::test]
    async fn committed_terminal_work_owns_must_mutate_goal_review_without_workspace_receipt() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );

        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result(
            "The committed Work result is ready for the user.",
            10,
            5,
            Some(1),
        )])
        .with_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("canonical Work synthesis should complete");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert_eq!(
            host.turn_count(),
            1,
            "no generic action window was reopened"
        );
        assert_eq!(
            state.final_text,
            "The committed Work result is ready for the user."
        );
    }

    #[tokio::test]
    async fn committed_work_revalidation_unavailable_interrupts_before_generic_mutation_retry() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result("Final review.", 10, 5, Some(1))])
            .with_unavailable_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("durable revalidation outage is a resumable loop outcome");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::Interrupted)
        );
    }

    #[tokio::test]
    async fn revoked_committed_work_never_falls_through_generic_must_mutate_completion() {
        let mut state = make_state();
        mark_must_mutate(&mut state);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        });

        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![text_result(
            "Stale synthesis must not be accepted.",
            10,
            5,
            Some(1),
        )]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("durable revocation is a typed incomplete outcome");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.committed_work_synthesis_checks, 1);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete)
        );
        assert!(!state.final_text.contains("Stale synthesis"));
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "canonical Ok(false) explicitly revokes the candidate"
        );
    }

    #[tokio::test]
    async fn historical_work_outage_does_not_interrupt_a_fresh_non_candidate_run() {
        let mut state = make_state();
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface
        );
        let mut host = MockHost::new(vec![text_result("Fresh run answer.", 10, 5, Some(1))])
            .with_unavailable_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("historical Work state is not authority for a fresh run");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert_eq!(host.committed_work_synthesis_checks, 0);
        assert_eq!(state.final_text, "Fresh run answer.");
    }

    #[tokio::test]
    async fn committed_work_is_revalidated_after_legitimate_inspection_before_final_prose() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("introspect", "current state")],
                10,
                5,
                Some(1),
            ),
            text_result("Final after inspection.", 10, 5, Some(1)),
        ])
        .with_valid_tools(&["introspect"])
        .with_committed_work_synthesis();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 2);
        assert!(state.interruption.is_none());
        assert_eq!(state.final_text, "Final after inspection.");
    }

    #[tokio::test]
    async fn rejected_goal_review_tool_does_not_block_durable_work_revalidation() {
        let mut state = make_state();
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        install_committed_work_synthesis_wire_surface(&mut state);
        let rejected_call = serde_json::json!({
            "id": "rejected-review",
            "type": "function",
            "function": {
                "name": "unavailable_inspector",
                "arguments": "{}"
            }
        });
        let mut host = MockHost::new(vec![
            server_tool_result(vec![rejected_call], Vec::new(), 10, 5, Some(1)),
            text_result("Final after rejected inspection.", 10, 5, Some(1)),
        ])
        .with_committed_work_synthesis();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 2);
        assert!(state.interruption.is_none());
        assert!(state.stall.tool_call_records.iter().any(|record| {
            record.name == "unavailable_inspector"
                && record.effective_disposition() == ToolCallDisposition::Rejected
        }));
        assert_eq!(state.final_text, "Final after rejected inspection.");
    }

    #[tokio::test]
    async fn explicit_verifier_runs_before_durable_work_final_synthesis() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.task_profile.verification_required = true;
        install_committed_work_synthesis_wire_surface(&mut state);
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let mut verifier = make_edge_tool("bash", "quality passed");
        verifier.args = serde_json::json!({"command": "./quality-gate"});
        verifier
            .tool_result_fields
            .get_or_insert_with(Default::default)
            .extend([
                ("result_class".into(), serde_json::json!("success")),
                ("exit_semantics".into(), serde_json::json!("success")),
            ]);
        let mut host = MockHost::new(vec![
            text_result("Premature final.", 10, 5, Some(1)),
            edge_tool_result(vec![verifier], 10, 5, Some(1)),
            text_result("Final after explicit verification.", 10, 5, Some(1)),
        ])
        .with_valid_tools(&["bash"])
        .with_committed_work_synthesis();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 3);
        assert_eq!(
            host.committed_work_synthesis_checks, 2,
            "the candidate remains live and is revalidated after verification"
        );
        assert!(state.interruption.is_none());
        assert_eq!(
            missing_explicit_verification_hooks(&state).unwrap(),
            Some(Vec::new())
        );
        assert_eq!(state.final_text, "Final after explicit verification.");
    }

    #[tokio::test]
    async fn post_verifier_final_prose_fails_closed_when_work_revalidation_is_unavailable() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.task_profile.verification_required = true;
        install_committed_work_synthesis_wire_surface(&mut state);
        state
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        let mut verifier = make_edge_tool("bash", "quality passed");
        verifier.args = serde_json::json!({"command": "./quality-gate"});
        let mut host = MockHost::new(vec![
            text_result("Premature final.", 10, 5, Some(1)),
            edge_tool_result(vec![verifier], 10, 5, Some(1)),
            text_result("Stale final after verification.", 10, 5, Some(1)),
        ])
        .with_valid_tools(&["bash"])
        .with_committed_work_synthesis_sequence([
            Ok(true),
            Err("durable Work store unavailable after verification".into()),
        ]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(outcome, Ok(AgenticLoopOutcome::Completed)));
        assert_eq!(host.turn_count(), 3);
        assert_eq!(host.committed_work_synthesis_checks, 2);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::Interrupted)
        );
        assert!(!state.final_text.contains("Stale final after verification"));
    }

    #[test]
    fn committed_work_synthesis_preserves_safety_barriers() {
        let mut paginated_fanout = make_state();
        install_committed_work_synthesis_wire_surface(&mut paginated_fanout);
        paginated_fanout.budget_wrapup_injected = true;
        paginated_fanout.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        paginated_fanout
            .hooks
            .completion_settlement
            .foreground_fanout_pagination = Some(super::super::host::ForegroundFanoutPagination {
            group_id: "fanout-1".into(),
            target_count: 2,
            pending_slots: std::collections::BTreeMap::from([(1, 1024)]),
            had_recoverable_issues: false,
        });
        assert_eq!(
            terminal_completion_disposition(&paginated_fanout, true),
            TerminalCompletionDisposition::RoundSliceIncomplete,
            "unread foreground fanout evidence remains a stronger barrier"
        );

        let mut unfinished_child = make_state();
        install_committed_work_synthesis_wire_surface(&mut unfinished_child);
        unfinished_child
            .stall
            .tool_call_records
            .push(ToolCallRecord {
                name: "agent".into(),
                ok: true,
                disposition: Some(ToolCallDisposition::Executed),
                args_full: Some(
                    serde_json::json!({
                        "action": "spawn",
                        "description": "Inspect the terminal result",
                        "prompt": "Inspect the terminal result"
                    })
                    .to_string(),
                ),
                result_full: Some(
                    serde_json::json!({
                        "status": "launched",
                        "result_family": "control_receipt",
                        "action": "spawn",
                        "success": true,
                        "parent_run_id": "parent",
                        "run_id": "child-run",
                        "agent_id": "reviewer-live"
                    })
                    .to_string(),
                ),
                ..Default::default()
            });
        assert_eq!(
            terminal_completion_disposition(&unfinished_child, true),
            TerminalCompletionDisposition::RoundSliceIncomplete,
            "an unfinished child remains stronger than Work synthesis even outside budget wrap-up"
        );
        for observation in [
            serde_json::json!({"error":"observation rejected", "agent_id":"reviewer-live"}),
            serde_json::json!({"result_family":"child_result", "status":"unknown", "agent_id":"reviewer-live"}),
        ] {
            let mut child_state = make_state();
            install_committed_work_synthesis_wire_surface(&mut child_state);
            child_state.stall.tool_call_records = unfinished_child.stall.tool_call_records.clone();
            child_state.stall.tool_call_records.push(ToolCallRecord {
                name: "agent".into(),
                ok: false,
                args_full: Some(
                    serde_json::json!({"action":"get_result","agent_id":"reviewer-live"})
                        .to_string(),
                ),
                result_full: Some(observation.to_string()),
                ..Default::default()
            });
            assert_eq!(
                terminal_completion_disposition(&child_state, true),
                TerminalCompletionDisposition::RoundSliceIncomplete
            );
            assert_eq!(
                super::super::lifecycle::unfinished_parallel_agent_ids(&child_state),
                vec!["reviewer-live".to_string()]
            );
            child_state.stall.tool_call_records.push(ToolCallRecord {
                name: "agent".into(), ok: true,
                args_full: Some(serde_json::json!({"action":"get_result","agent_id":"reviewer-live"}).to_string()),
                result_full: Some(serde_json::json!({"result_family":"child_result","status":"completed","agent_id":"reviewer-live","result":"done"}).to_string()),
                ..Default::default()
            });
            assert!(
                super::super::lifecycle::unfinished_parallel_agent_ids(&child_state).is_empty(),
                "a later authoritative terminal result must settle child ownership"
            );
        }

        let mut quarantined = make_state();
        install_committed_work_synthesis_wire_surface(&mut quarantined);
        quarantined.stall.workspace_observation_quarantine = Some(
            astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::partial_workspace_mutation(
                Some("call-with-unsettled-commit".into()),
            ),
        );
        assert!(reject_unsettled_execution_completion(&mut quarantined));
        assert_eq!(
            quarantined.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete),
            "an unsettled executor commit remains stronger than Work synthesis"
        );

        let mut verifier = make_state();
        install_committed_work_synthesis_wire_surface(&mut verifier);
        verifier
            .hooks
            .stop_hooks
            .push(explicit_verification_hook("quality", "./quality-gate"));
        verifier.task_profile.mutates_workspace = true;
        verifier.task_profile.verification_required = true;
        verifier
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        assert!(!reject_unsettled_execution_completion(&mut verifier));
        assert!(enforce_explicit_verification_before_text_completion(
            &mut verifier
        ));
        assert_eq!(verifier.hooks.completion_settlement.verification_retries, 1);
        assert!(verifier.volatile_pending.iter().any(|injection| {
            injection
                .payload
                .get("schema")
                .and_then(serde_json::Value::as_str)
                == Some("explicit_verification_required.v1")
        }));
    }

    #[tokio::test]
    async fn settled_exact_completion_action_is_not_reclassified_as_round_slice_partial() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state
            .stall
            .tool_call_records
            .push(executed_record("write_file", true, None));
        state.stall.tool_call_records.push(executed_record(
            "bash",
            true,
            Some(r#"{"command":"cargo test"}"#),
        ));
        state.hooks.completion_settlement.completion_action_window =
            Some(super::super::host::CompletionActionWindow {
                action: explicit_verification_action(&mut state),
                attempts_remaining: 0,
                mismatch_corrections_remaining: 0,
                consumed: true,
                matched: true,
            });
        let mut host = MockHost::new(vec![text_result(
            "Completed after the exact observation obligation settled.",
            10,
            5,
            Some(1),
        )]);

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("typed completion action should be ingested");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none(),
            "the settled exact action should be consumed exactly once"
        );
    }

    #[tokio::test]
    async fn committed_work_completion_does_not_promote_an_empty_provider_response() {
        let mut state = make_state();
        state.budget_wrapup_injected = true;
        state.hooks.completion_settlement.text_only = true;
        state.hooks.completion_settlement.wrapup_origin =
            Some(super::super::host::BudgetWrapupOrigin::RoundSlice);
        install_committed_work_synthesis_wire_surface(&mut state);
        let mut host = MockHost::new(vec![
            text_result("", 10, 0, Some(1)),
            text_result("Completed after the bounded retry.", 10, 5, Some(1)),
        ])
        .with_committed_work_synthesis();

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("an empty response should be handled by the bounded retry path");

        assert!(matches!(control, TurnExecutionControl::ContinueLoop));
        assert!(state.interruption.is_none());
        assert!(
            state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "an empty response must not consume typed final-synthesis authority"
        );

        let control = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            1,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("the bounded retry should accept non-empty final synthesis");

        assert!(matches!(
            control,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert!(state.interruption.is_none());
        assert!(
            !state
                .hooks
                .completion_settlement
                .preserve_final_synthesis_wire_surface,
            "the successful retry consumes typed final-synthesis authority"
        );
    }

    #[test]
    fn typed_work_settlement_goal_review_is_not_reclassified_as_contract_failure() {
        let mut state = make_state();
        state.hooks.completion_settlement.work_settlement_only = true;
        state.hooks.completion_settlement.text_only = false;
        state.final_text = "review findings".to_string();

        install_committed_work_synthesis_wire_surface(&mut state);
        super::super::tool_phase::transition_work_settlement_to_final_synthesis(&mut state);

        assert!(!enforce_typed_work_settlement_before_text_completion(
            &mut state
        ));
        assert_eq!(state.final_text, "review findings");
        assert!(
            !state
                .final_text
                .contains(WORK_SETTLEMENT_CONTRACT_FAILURE_TEXT)
        );
        assert!(!state.hooks.completion_settlement.work_settlement_only);
        assert!(!state.hooks.completion_settlement.text_only);
    }

    #[test]
    fn canonical_active_work_ownership_blocks_text_without_projection_flag() {
        let mut state = make_state();
        state.final_text = "I will settle this task next.".to_string();
        let original_max_turns = state.max_turns;
        let original_remaining_turns = state.remaining_turns;

        assert!(
            enforce_typed_work_settlement_before_text_completion_for_work_state(&mut state, true,)
        );

        assert!(state.final_text.is_empty());
        assert!(state.hooks.completion_settlement.work_settlement_only);
        assert!(!state.hooks.completion_settlement.text_only);
        assert_eq!(state.max_turns, original_max_turns);
        assert_eq!(state.remaining_turns, original_remaining_turns);
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|entry| { entry.payload["signal"] == "owned_work_attempt_unsettled" })
        );
    }

    #[test]
    fn bash_mutation_detects_compound_and_sudo_commands() {
        use crate::turn::agentic_loop::lifecycle::tool_record_is_workspace_mutation;
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
        use crate::turn::agentic_loop::lifecycle::tool_record_is_workspace_mutation;
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

    #[test]
    fn successful_validation_closes_post_mutation_obligation_without_overtrusting_opaque_bash() {
        use crate::turn::agentic_loop::lifecycle::tool_record_is_workspace_mutation;

        let mutation = ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            args_full: Some(r#"{"path":"src/lib.rs","old":"old","new":"new"}"#.into()),
            ..Default::default()
        };
        let validation = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(r#"{"command":"python3 -m pytest tests/test_knot.py"}"#.into()),
            ..Default::default()
        };
        let opaque = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(
                serde_json::json!({
                    "command": "python3 -c 'open(\"src/lib.rs\", \"w\").write(\"x\")'"
                })
                .to_string(),
            ),
            ..Default::default()
        };

        assert!(tool_record_is_workspace_mutation(&mutation));
        assert!(!tool_record_is_workspace_mutation(&validation));
        assert!(!tool_record_is_workspace_mutation(&opaque));
        assert!(
            crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "bash",
                crate::turn::agentic_loop::lifecycle::extract_tool_args(
                    validation.args_full.as_deref()
                )
                .as_ref(),
            )
        );
        assert!(
            !crate::turn::tool_side_effects::tool_call_may_observe_workspace(
                "bash",
                crate::turn::agentic_loop::lifecycle::extract_tool_args(
                    opaque.args_full.as_deref()
                )
                .as_ref(),
            )
        );
    }

    #[test]
    fn completion_obligation_uses_validation_receipt_but_not_opaque_shell_possibility() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        let mutation = ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({"path":"src/lib.rs","old":"old","new":"new"}).to_string(),
            ),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        };
        let validation = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            result_class: Some("success".into()),
            exit_semantics: Some("success".into()),
            args_full: Some(
                serde_json::json!({
                    "command": "python3 -m pytest tests/test_knot.py"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), validation];
        assert_eq!(pending_completion_action(&state).unwrap(), None);

        let opaque = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "python3 -c 'open(\"src/lib.rs\", \"w\").write(\"x\")'"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), opaque];

        let mut mutating_bash = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "sed -i 's/old/new/' src/lib.rs"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), mutating_bash.clone()];

        let compound_receipt = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "rm -rf build && git push origin main && cat dist/index.html && curl -sk https://localhost/"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![compound_receipt];

        let cython_build = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "python setup.py build_ext --inplace 2>&1 | tail -20"
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation.clone(), cython_build];

        mutating_bash.ok = false;
        state.stall.tool_call_records = vec![mutation, mutating_bash];
    }

    #[test]
    fn nested_observation_receipt_requires_canonical_bash_args() {
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        let mutation = ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            workspace_mutation_observed: Some(true),
            workspace_mutation_scope: Some(
                astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
            ),
            workspace_mutation_receipt:
                astra_tools::workspace_observation::typed_workspace_tool_receipt()
                    .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                    .cloned(),
            ..Default::default()
        };
        let validation = ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            args_full: Some(
                serde_json::json!({
                    "command": "prepare-command; value=$(curl -fsS https://service/ | tr -d '\\n'); echo \"status=$([ \\\"$value\\\" = expected ] && echo PASS || echo FAIL)\""
                })
                .to_string(),
            ),
            ..Default::default()
        };
        state.stall.tool_call_records = vec![mutation, validation];
        assert_eq!(pending_completion_action(&state).unwrap(), None);

        state.stall.tool_call_records[1].ok = false;

        // The normal bash preview is a plain (and potentially truncated)
        // command, not JSON.  It must never be promoted to authoritative
        // completion evidence when args_full is unavailable.
        let mut state = make_state();
        state.task_profile.mutates_workspace = true;
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                workspace_mutation_observed: Some(true),
                workspace_mutation_scope: Some(
                    astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE.into(),
                ),
                workspace_mutation_receipt:
                    astra_tools::workspace_observation::typed_workspace_tool_receipt()
                        .get(astra_tools::workspace_observation::RECEIPT_FIELD)
                        .cloned(),
                ..Default::default()
            },
            ToolCallRecord {
                name: "bash".into(),
                ok: true,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                args_preview: Some(
                    "prepare-command; value=$(curl -fsS https://service/); echo status=$value"
                        .into(),
                ),
                args_full: None,
                ..Default::default()
            },
        ];
    }

    #[test]
    fn circuit_breaker_signal_uses_latest_round_for_mutation_detection() {
        let mut state = make_state();
        state.llm_rounds_completed = 6;
        state.stall.turn_sigs.push(
            [astra_turn_core::stall::StallSignature::new(
                "read_file",
                br#"{"path":"a.rs"}"#,
            )]
            .into_iter()
            .collect(),
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            round: Some(2),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            round: Some(5),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(
            !signal.produced_mutation,
            "an old str_replace must not mask a later read-only round"
        );
    }

    #[test]
    fn safe_provider_recovery_schedules_exactly_one_capability_aware_attempt() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "action deadline",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "",
                "partial_reasoning": "a long provisional derivation",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        assert!(state.provider_adaptation.safe_recovery_attempted);
        assert!(state.provider_adaptation.safe_recovery_pending);
        assert!(state.volatile_pending.iter().any(|entry| {
            entry.kind == VolatileKind::BehaviorAdvisory
                && entry.payload["schema"] == "provider_safe_recovery.v1"
        }));
        assert!(
            !schedule_safe_provider_recovery(&mut state, &error),
            "the recovery boundary must be consumed exactly once"
        );

        let mut whitespace_state = make_state();
        whitespace_state.remaining_turns = 3;
        let whitespace_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "action deadline",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": " \n\t",
                "partial_reasoning": "provisional",
                "tool_calls": []
            })
            .to_string(),
        );
        assert!(
            schedule_safe_provider_recovery(&mut whitespace_state, &whitespace_error),
            "provider whitespace is not visible output and must not suppress convergence"
        );

        let mut text_only_state = make_state();
        text_only_state.remaining_turns = 3;
        text_only_state.hooks.completion_settlement.text_only = true;
        assert!(schedule_safe_provider_recovery(
            &mut text_only_state,
            &whitespace_error
        ));
        let advisory = text_only_state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["schema"] == "provider_safe_recovery.v1")
            .expect("text-only convergence advisory");
        assert_eq!(advisory.payload["execution_mode"], "text_only");
        assert!(
            advisory.payload["instruction"]
                .as_str()
                .is_some_and(|instruction| instruction.contains("do not call tools"))
        );
    }

    #[test]
    fn run_deadline_yields_to_settlement_only_without_actionable_delivery() {
        let no_response = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "LLM provider work deadline reached while waiting for provider response",
        );
        assert!(provider_deadline_has_no_actionable_delivery(&no_response));

        let no_delivery = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "LLM provider work deadline reached while consuming provider stream",
        )
        .with_details_json(
            serde_json::json!({
                "partial_full_text": "",
                "tool_calls": [],
                "partial_reasoning": "provisional reasoning"
            })
            .to_string(),
        );
        assert!(provider_deadline_has_no_actionable_delivery(&no_delivery));

        let partial_text = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "deadline after partial text",
        )
        .with_details_json(
            serde_json::json!({ "partial_full_text": "The result is", "tool_calls": [] })
                .to_string(),
        );
        assert!(!provider_deadline_has_no_actionable_delivery(&partial_text));

        let selected_tool = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "deadline after partial tool call",
        )
        .with_details_json(
            serde_json::json!({
                "partial_full_text": "",
                "tool_calls": [{ "id": "call-1", "name": "bash" }]
            })
            .to_string(),
        );
        assert!(!provider_deadline_has_no_actionable_delivery(
            &selected_tool
        ));

        let malformed = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "malformed partial data",
        )
        .with_details_json("not-json".to_string());
        assert!(!provider_deadline_has_no_actionable_delivery(&malformed));

        let transport_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "non-deadline transport failure",
        );
        assert!(!provider_deadline_has_no_actionable_delivery(
            &transport_error
        ));
    }

    #[test]
    fn terminal_empty_provider_completion_is_safe_to_recover_once() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {
                    "scope": "provider_completion",
                    "phase": "actionable_output"
                },
                "partial_full_text": "",
                "partial_reasoning": "",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(
            schedule_safe_provider_recovery(&mut state, &error),
            "a terminal provider completion with no delivery is safe to converge once"
        );
        assert!(state.provider_adaptation.safe_recovery_pending);
    }

    #[test]
    fn terminal_empty_recovery_requires_explicit_well_typed_empty_evidence() {
        let incomplete_or_malformed = [
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "tool_calls": [],
                "provider_response": {"transport_success": true}
            }),
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": "",
                "provider_response": {"transport_success": true}
            }),
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": [],
                "tool_calls": [],
                "provider_response": {"transport_success": true}
            }),
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": "",
                "tool_calls": {},
                "provider_response": {"transport_success": true}
            }),
        ];

        for details in incomplete_or_malformed {
            let mut state = make_state();
            state.max_turns = 34;
            state.remaining_turns = 0;
            state.hooks.completion_settlement.text_only = true;
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "provider convergence completed without visible text or a selected tool",
            )
            .with_details_json(details.to_string());

            assert!(
                !schedule_safe_provider_recovery(&mut state, &error),
                "missing or malformed delivery evidence must fail closed"
            );
            assert_eq!(state.max_turns, 34);
            assert_eq!(state.remaining_turns, 0);
            assert!(!state.provider_adaptation.safe_recovery_attempted);
            assert!(!state.provider_adaptation.safe_recovery_pending);
        }
    }

    #[test]
    fn terminal_whitespace_completion_gets_one_text_only_recovery() {
        let mut state = make_state();
        state.max_turns = 34;
        state.remaining_turns = 1;
        state.hooks.completion_settlement.text_only = true;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider convergence completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"scope": "provider_convergence", "phase": "actionable_output"},
                "partial_full_text": " \n\t",
                "tool_calls": [],
                "provider_response": {"transport_success": true}
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        assert_eq!(state.max_turns, 34);
        assert_eq!(state.remaining_turns, 1);
        assert!(state.hooks.completion_settlement.text_only);
        assert!(state.provider_adaptation.safe_recovery_attempted);
        assert!(state.provider_adaptation.safe_recovery_pending);
    }

    #[test]
    fn provider_completion_error_usage_preserves_unknown_without_transport_success() {
        for qualified in [
            serde_json::json!({}),
            serde_json::json!({"output_tokens":7}),
            serde_json::json!({"input_tokens":17,"cached_input_tokens":3,"cache_creation_tokens":2,"output_tokens":7}),
        ] {
            let mut state = make_state();
            state.last_measured_prompt_tokens = Some(1000);
            state.last_request_usage =
                Some(astra_turn_types::RequestTokenUsage::try_new(100, 900, 0, 20).unwrap());
            state.total_prompt = 100;
            state.total_cache_read = 900;
            state.total_completion = 20;
            state.add_qualified_usage(Some(
                astra_turn_types::CanonicalTokenUsage::new(Some(100), Some(900), Some(0), Some(20))
                    .unwrap(),
            ));
            let error =
                astra_core::ClassifiedError::new(astra_core::ErrorKind::Cancelled, "cancelled")
                    .with_details_json(
                        serde_json::json!({"qualified_usage":qualified}).to_string(),
                    );
            fold_provider_completion_error_usage(&mut state, &error);
            let usage = state.qualified_usage.unwrap();
            assert!(state.last_request_usage.is_none());
            assert!(state.last_measured_prompt_tokens.is_none());
            assert_eq!(
                usage.input_tokens(),
                qualified
                    .get("input_tokens")
                    .and_then(|value| value.as_u64())
                    .map(|value| value + 100)
            );
            assert_eq!(
                usage.cached_input_tokens(),
                qualified
                    .get("cached_input_tokens")
                    .and_then(|value| value.as_u64())
                    .map(|value| value + 900)
            );
            assert_eq!(
                usage.output_tokens(),
                qualified
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v + 20)
            );
            assert_eq!(
                state.total_prompt,
                100 + qualified
                    .get("input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
            );
            assert_eq!(
                state.total_cache_read,
                900 + qualified
                    .get("cached_input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
            );
            assert_eq!(
                state.total_cache_creation,
                qualified
                    .get("cache_creation_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
            );
            assert_eq!(
                state.total_completion,
                20 + qualified
                    .get("output_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
            );
        }
    }

    #[test]
    fn provider_completion_error_usage_is_folded_once_and_reclassified() {
        let mut state = make_state();
        state.record_local_usage_coverage(false);
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"scope": "provider_completion", "phase": "actionable_output"},
                "provider_response": {"transport_success": true},
                "last_request_usage": {"input_tokens": 10, "cached_input_tokens": 2, "cache_creation_tokens": 1},
                "usage": {
                    "input_tokens": 21,
                    "cached_input_tokens": 3,
                    "cache_creation_tokens": 2,
                    "output_tokens": 8
                }
            })
            .to_string(),
        );

        fold_provider_completion_error_usage(&mut state, &error);
        assert_eq!(state.total_prompt, 21);
        assert_eq!(state.total_cache_read, 3);
        assert_eq!(state.total_cache_creation, 2);
        assert_eq!(state.total_completion, 8);
        assert!(state.has_any_usage);
        assert_eq!(state.last_measured_prompt_tokens, Some(13));
        assert_eq!(state.token_usage_coverage().attempts, 1);
        assert_eq!(state.token_usage_coverage().provider_reported, 1);
        assert_eq!(state.token_usage_coverage().unavailable, 0);

        let partial_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "provider_response": {"transport_success": true},
                "last_request_usage": {"input_tokens": 10_000, "cached_input_tokens": 90_000},
                "last_request_input_tokens": 100_000,
                "usage": {"input_tokens": 10_000, "cached_input_tokens": 90_000, "output_tokens": 20}
            })
            .to_string(),
        );
        let mut partial_state = make_state();
        fold_provider_completion_error_usage(&mut partial_state, &partial_error);
        assert_eq!(partial_state.last_request_usage, None);
        assert_eq!(partial_state.last_measured_prompt_tokens, Some(100_000));

        let mut aggregate_only_details: serde_json::Value =
            serde_json::from_str(error.details_json.as_deref().unwrap()).unwrap();
        aggregate_only_details
            .as_object_mut()
            .unwrap()
            .remove("last_request_usage");
        let aggregate_only_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "empty response",
        )
        .with_details_json(aggregate_only_details.to_string());
        let mut aggregate_only_state = make_state();
        aggregate_only_state.last_measured_prompt_tokens = Some(999);
        aggregate_only_state.record_local_usage_coverage(false);
        fold_provider_completion_error_usage(&mut aggregate_only_state, &aggregate_only_error);
        assert_eq!(aggregate_only_state.total_prompt, 21);
        assert_eq!(aggregate_only_state.last_measured_prompt_tokens, None);

        let mut unavailable_state = make_state();
        unavailable_state.record_local_usage_coverage(false);
        let no_usage_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "provider completed without visible text or a selected tool",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"scope": "provider_completion", "phase": "actionable_output"},
                "provider_response": {"transport_success": true},
                "usage": null
            })
            .to_string(),
        );
        fold_provider_completion_error_usage(&mut unavailable_state, &no_usage_error);
        assert!(!unavailable_state.has_any_usage);
        assert_eq!(
            unavailable_state.token_usage_coverage().provider_reported,
            0
        );
        assert_eq!(unavailable_state.token_usage_coverage().unavailable, 1);

        for (usage, expected_measurement, reported) in [
            (
                serde_json::json!({"input_tokens": 21, "output_tokens": 8}),
                None,
                true,
            ),
            (
                serde_json::json!({"input_tokens": 0, "cached_input_tokens": 0, "cache_creation_tokens": 0, "output_tokens": 0}),
                Some(0),
                true,
            ),
            (serde_json::json!({}), None, false),
            (
                serde_json::json!({"input_tokens": -1, "output_tokens": 8}),
                None,
                false,
            ),
            (serde_json::Value::Null, None, false),
        ] {
            let mut state = make_state();
            state.last_measured_prompt_tokens = Some(999);
            state.record_local_usage_coverage(false);
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "empty response",
            )
            .with_details_json(
                serde_json::json!({
                    "provider_response": {"transport_success": true}, "usage": usage,
                    "last_request_usage": usage
                })
                .to_string(),
            );
            fold_provider_completion_error_usage(&mut state, &error);
            assert_eq!(state.last_measured_prompt_tokens, expected_measurement);
            assert_eq!(state.has_any_usage, reported);
            assert_eq!(
                state.token_usage_coverage().provider_reported,
                u32::from(reported)
            );
        }
    }

    #[test]
    fn executed_tool_starts_a_new_safe_provider_recovery_epoch() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "action deadline",
        )
        .with_details_json(
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "",
                "partial_reasoning": "provisional reasoning",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        assert!(state.provider_adaptation.safe_recovery_attempted);
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            ..Default::default()
        });
        assert!(advance_provider_recovery_epoch_from_new_records(
            &mut state, 0
        ));
        assert!(
            schedule_safe_provider_recovery(&mut state, &error),
            "a typed executed tool terminal creates a fresh inference-progress epoch"
        );

        let mut rejected_state = make_state();
        rejected_state.remaining_turns = 3;
        assert!(schedule_safe_provider_recovery(&mut rejected_state, &error));
        rejected_state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
            ..Default::default()
        });
        assert!(!advance_provider_recovery_epoch_from_new_records(
            &mut rejected_state,
            0
        ));
        assert!(
            !schedule_safe_provider_recovery(&mut rejected_state, &error),
            "a rejected selection must not replenish recovery"
        );

        for disposition in [
            astra_services::session_journal::ToolCallDisposition::Reused,
            astra_services::session_journal::ToolCallDisposition::Deferred,
        ] {
            let mut non_execution_state = make_state();
            non_execution_state
                .provider_adaptation
                .safe_recovery_attempted = true;
            non_execution_state
                .stall
                .tool_call_records
                .push(ToolCallRecord {
                    name: "bash".into(),
                    ok: true,
                    disposition: Some(disposition),
                    ..Default::default()
                });
            assert!(
                !advance_provider_recovery_epoch_from_new_records(&mut non_execution_state, 0),
                "{disposition:?} is not an executor-terminal progress boundary"
            );
            assert!(
                non_execution_state
                    .provider_adaptation
                    .safe_recovery_attempted
            );
        }

        let mut failed_execution_state = make_state();
        failed_execution_state
            .provider_adaptation
            .safe_recovery_attempted = true;
        failed_execution_state
            .stall
            .tool_call_records
            .push(ToolCallRecord {
                name: "bash".into(),
                ok: false,
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
                ..Default::default()
            });
        assert!(advance_provider_recovery_epoch_from_new_records(
            &mut failed_execution_state,
            0
        ));
        assert!(
            !failed_execution_state
                .provider_adaptation
                .safe_recovery_attempted
        );
    }

    #[test]
    fn safe_provider_recovery_never_supersedes_visible_or_tool_output() {
        for details in [
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "visible partial",
                "partial_reasoning": "",
                "tool_calls": []
            }),
            serde_json::json!({
                "deadline": {"phase": "semantic_progress"},
                "partial_full_text": "",
                "partial_reasoning": "thinking",
                "tool_calls": [{"function": {"name": "read_file", "arguments": "{}"}}]
            }),
        ] {
            let mut state = make_state();
            state.remaining_turns = 3;
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ProviderDeadline,
                "action deadline",
            )
            .with_details_json(details.to_string());
            assert!(!schedule_safe_provider_recovery(&mut state, &error));
            assert!(!state.provider_adaptation.safe_recovery_pending);
        }
    }

    #[test]
    fn stream_transport_with_only_provisional_reasoning_recovers_once() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "provider connection reset after thinking",
        )
        .with_details_json(
            serde_json::json!({
                "partial_full_text": "",
                "partial_reasoning": "working through the task",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(schedule_safe_provider_recovery(&mut state, &error));
        let advisory = state
            .volatile_pending
            .iter()
            .find(|entry| entry.payload["schema"] == "provider_safe_recovery.v1")
            .expect("transport recovery advisory");
        assert_eq!(
            advisory.payload["evidence"]["error_kind"],
            "stream_transport"
        );
        assert!(state.provider_adaptation.safe_recovery_pending);
        assert!(
            !schedule_safe_provider_recovery(&mut state, &error),
            "a transport failure has at most one safe logical recovery"
        );
    }

    #[test]
    fn stream_transport_without_partial_attempt_evidence_fails_closed() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "provider connection failed before delivery",
        )
        .with_details_json(
            serde_json::json!({
                "partial_full_text": "",
                "partial_reasoning": "",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(!schedule_safe_provider_recovery(&mut state, &error));
        assert!(!state.provider_adaptation.safe_recovery_pending);
    }

    #[test]
    fn stream_transport_with_malformed_partial_evidence_fails_closed() {
        let mut state = make_state();
        state.remaining_turns = 3;
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "provider connection reset after malformed partial",
        )
        .with_details_json(
            serde_json::json!({
                "partial_reasoning": "provisional reasoning",
                "tool_calls": []
            })
            .to_string(),
        );

        assert!(
            !schedule_safe_provider_recovery(&mut state, &error),
            "a transport recovery requires typed text, reasoning, and tool-call evidence"
        );
        assert!(!state.provider_adaptation.safe_recovery_pending);
    }

    #[tokio::test]
    async fn artifact_spill_carries_prior_summary_and_recovery_handles() {
        use astra_turn_core::cloud::history_summary::{
            history_summary_message, history_summary_text,
        };
        let mut host = MockHost::new(Vec::new());
        host.history_artifact_enabled = true;
        let mut state = make_state();
        state.max_turn_input_tokens = 100_000;
        state.current_session_id = Some("summary-session".into());
        state.current_run_id = Some("summary-run".into());
        state.messages = vec![history_summary_message(
            "ONLY_PRIOR_FACT\nExact transcript: context-history://prior".into(),
        )];
        for generation in 0..2 {
            for index in 0..14 {
                state.messages.push(serde_json::json!({"role":"assistant", "content":format!("generation {generation} evidence {index}: {}", "detail ".repeat(500))}));
            }
            state.messages.push(serde_json::json!({"role":"user", "content":format!("current request {generation}")}));
            let before = state.messages.clone();
            let (freed, _) = spill_old_messages_to_artifact(&mut host, &mut state)
                .await
                .unwrap();
            assert!(freed > 0);
            let summary = history_summary_text(&state.messages[0]).unwrap();
            assert!(summary.contains("ONLY_PRIOR_FACT"));
            assert!(summary.contains("context-history://prior"));
            assert_eq!(state.messages.last(), before.last());
            let stored: Vec<Value> =
                serde_json::from_str(&host.history_artifacts[generation].transcript_json).unwrap();
            assert_eq!(stored, before[..stored.len()]);
        }
    }

    #[tokio::test]
    async fn artifact_spill_failure_cancellation_and_small_summary_preserve_history() {
        for condition in [
            "failure",
            "cancel",
            "late-cancel",
            "no-benefit",
            "reasoning",
            "success",
        ] {
            let mut host = MockHost::new(Vec::new());
            host.history_artifact_enabled = true;
            host.history_artifact_error = condition == "failure";
            let mut state = make_state();
            state.max_turn_input_tokens = 10_000;
            state.current_session_id = Some("session".into());
            state.current_run_id = Some("run".into());
            for index in 0..14 {
                state.messages.push(serde_json::json!({"role":"assistant",
                    "content": format!("decision {index}: {}", "evidence ".repeat(if condition == "no-benefit" { 1 } else { 500 }))}));
            }
            state
                .messages
                .push(serde_json::json!({"role":"user","content":"current request"}));
            if condition == "reasoning" {
                state.messages[1][astra_core::model_wire::responses::CONTINUATION_KEY] = serde_json::json!({
                    "items":[{"type":"reasoning", "content":[{"type":"reasoning_text", "text":"private"}]}]
                });
            }
            let original = state.messages.clone();
            if condition == "cancel" || condition == "late-cancel" {
                let token = Arc::new(tokio_util::sync::CancellationToken::new());
                if condition == "cancel" {
                    token.cancel();
                } else {
                    host.history_artifact_cancel = Some(token.clone());
                }
                state.cancellation.token = Some(token);
            }
            let result = spill_old_messages_to_artifact(&mut host, &mut state).await;
            if condition == "success" {
                let (freed, removed) = result.unwrap();
                assert!(freed > 0 && removed > 0);
                assert_eq!(state.messages.last(), original.last());
                let stored: Vec<serde_json::Value> =
                    serde_json::from_str(&host.history_artifacts[0].transcript_json).unwrap();
                let expected: Vec<_> = original[..stored.len()]
                    .iter()
                    .map(astra_core::model_wire::responses::without_continuation)
                    .collect();
                assert_eq!(stored, expected);
                assert!(
                    !host.history_artifacts[0]
                        .transcript_json
                        .contains("reasoning_text")
                );
            } else {
                assert!(result.is_none(), "{condition}");
                assert_eq!(state.messages, original, "{condition}");
                assert_eq!(
                    host.history_artifacts.len(),
                    usize::from(condition == "late-cancel")
                );
            }
        }
    }

    #[tokio::test]
    async fn reactive_compaction_does_not_arm_wrapup_after_success() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.last_measured_prompt_tokens = Some(90_000);
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "diagnose and fix the issue"}));
        for i in 0..16 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("step {i}: {}", "x".repeat(240)),
            }));
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("follow-up {i}: {}", "y".repeat(220)),
            }));
        }

        let control = handle_token_budget(
            &mut host,
            &mut state,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("working", 90_000, 300, Some(20)),
        )
        .await;

        assert!(matches!(control, Some(TurnExecutionControl::ContinueLoop)));
        assert!(
            !state.budget_wrapup_injected,
            "successful reactive compaction should continue the turn without arming wrapup"
        );
        assert!(
            state.context_compression_triggered,
            "quiet reactive compaction must remain visible to the final context trace"
        );
        assert!(
            host.compaction_events
                .iter()
                .any(|event| event.kind == CompactionKind::ReactiveBudget),
            "quiet reactive compaction must emit the same structured callback as rendered mode"
        );
        assert!(
            state.step_recorder.events().iter().any(|event| {
                event.event_type == astra_pipeline::step_protocol::StepEventType::CompactionFired
            }),
            "quiet reactive compaction must still emit durable step audit"
        );
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|entry| entry.kind == VolatileKind::CompactResume),
            "successful reactive compaction should inject the continue-after-compact directive"
        );
    }

    #[tokio::test]
    async fn repeated_budget_pressure_retries_compaction_before_wrapup() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "diagnose and fix the issue"}));
        for i in 0..24 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("step {i}: {}", "x".repeat(240)),
            }));
            state.messages.push(serde_json::json!({
                "role": "user",
                "content": format!("follow-up {i}: {}", "y".repeat(220)),
            }));
        }

        state.last_measured_prompt_tokens = Some(90_000);
        let first = handle_token_budget(
            &mut host,
            &mut state,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("working", 90_000, 300, Some(20)),
        )
        .await;
        assert!(matches!(first, Some(TurnExecutionControl::ContinueLoop)));
        assert!(!state.budget_wrapup_injected);
        assert_eq!(state.compaction_effectiveness.attempt_count, 1);
        let first_savings = state.compaction_effectiveness.cumulative_tokens_freed;
        assert!(first_savings > 0);

        // A second provider round can add genuinely prunable evidence. Pressure
        // alone must not count the already-installed boundary as new savings.
        for i in 0..12 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("new evidence {i}: {}", "z".repeat(240)),
            }));
        }

        state.last_measured_prompt_tokens = Some(88_000);
        let second = handle_token_budget(
            &mut host,
            &mut state,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("still working", 88_000, 320, Some(20)),
        )
        .await;

        assert!(
            matches!(second, Some(TurnExecutionControl::ContinueLoop)),
            "continued context pressure should retry compaction before forcing wrapup"
        );
        assert!(
            !state.budget_wrapup_injected,
            "repeat pressure after a successful compact should not flip straight into wrapup mode"
        );
        assert_eq!(state.compaction_effectiveness.attempt_count, 2);
        assert!(state.compaction_effectiveness.cumulative_tokens_freed > first_savings);

        // Without new history, another retry cannot claim progress from gross
        // removed bytes while replacing them with an equally large boundary.
        let compacted = state.messages.clone();
        let savings = state.compaction_effectiveness.cumulative_tokens_freed;
        let third = handle_token_budget(
            &mut host,
            &mut state,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("still working", 88_000, 320, Some(20)),
        )
        .await;
        assert!(matches!(third, Some(TurnExecutionControl::ContinueLoop)));
        assert!(state.budget_wrapup_injected);
        assert_eq!(state.messages, compacted);
        assert_eq!(state.compaction_effectiveness.attempt_count, 2);
        assert_eq!(
            state.compaction_effectiveness.cumulative_tokens_freed,
            savings
        );
    }

    #[tokio::test]
    async fn prior_compactions_do_not_prevent_recovery_from_new_pressure() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.max_turn_input_tokens = 80_000;
        state.last_measured_prompt_tokens = Some(90_000);
        state.compaction_effectiveness.attempt_count = 14;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "diagnose and fix the issue"}));
        for i in 0..16 {
            state.messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("step {i}: {}", "x".repeat(240)),
            }));
        }

        let control = handle_token_budget(
            &mut host,
            &mut state,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
            &text_result("working", 90_000, 300, Some(20)),
        )
        .await;

        assert!(matches!(control, Some(TurnExecutionControl::ContinueLoop)));
        assert!(
            !state.budget_wrapup_injected,
            "previous compactions must not prevent a useful recovery"
        );
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|entry| entry.kind == VolatileKind::CompactResume),
            "successful recovery should continue the user's task"
        );
    }

    #[test]
    fn circuit_breaker_signal_ignores_round_zero_records_before_any_round_completes() {
        let mut state = make_state();
        state.llm_rounds_completed = 0;
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".into(),
            ok: true,
            round: Some(0),
            ..Default::default()
        });

        let signal = build_circuit_breaker_signal(&state);

        assert!(
            !signal.produced_mutation,
            "round 0 records are not latest completed work before any round completes"
        );
    }

    #[test]
    fn parallel_batching_suppressed_when_cascade_guard_already_fired() {
        let flags: Vec<Box<dyn Fn(&mut AgenticLoopState)>> =
            vec![Box::new(|s| s.stall.cache_waste_advisory_emitted = true)];
        for set_flag in &flags {
            let mut state = make_state();
            state.message = "explore the codebase".into();
            state.user_intent = state.message.clone();
            for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
                push_single_tool_round(&mut state);
            }
            // Precondition: would fire without the flag.
            assert!(should_emit_parallel_batching_advisory(
                &state,
                PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
            ));
            set_flag(&mut state);
            assert!(
                !should_emit_parallel_batching_advisory(
                    &state,
                    PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
                ),
                "parallel-batching must not fire when a cascade guard already active"
            );
        }
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
        state.user_intent = state.message.clone();
        for _ in 0..PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD {
            push_single_tool_round(&mut state);
        }
        assert!(should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_below_threshold() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD - 1) {
            push_single_tool_round(&mut state);
        }
        assert!(!should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_silent_when_last_round_batched() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
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
        assert!(!should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    #[test]
    fn parallel_batching_force_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "explore the codebase".into();
        state.user_intent = state.message.clone();
        for _ in 0..(PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD + 3) {
            push_single_tool_round(&mut state);
        }
        // First time would fire...
        assert!(should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
        // ...but once the flag is set, a second attempt is suppressed even
        // if the model produces yet another single-tool round.
        state.stall.parallel_batching_advisory_emitted = true;
        push_single_tool_round(&mut state);
        assert!(!should_emit_parallel_batching_advisory(
            &state,
            PARALLEL_BATCHING_FORCE_STREAK_THRESHOLD
        ));
    }

    // ─── Cascade-invariant + per-model resolver wiring ─────────────────────

    /// The runtime hard-advisory force MUST stay strictly above the
    /// prompt-layer soft nudge so the soft→hard cascade is preserved. If the
    /// resolved force ever drops to ≤ nudge, the runtime will inject a hard
    /// `user`-role advisory before the model has had any chance to
    /// self-correct on the soft prompt nudge — that inverts the intended
    /// failure-mode escalation.
    #[test]
    fn parallel_batching_force_default_above_nudge_threshold() {
        let cfg = astra_config::runtime_config::ToolPolicyConfig::default();
        let resolved = cfg.effective_parallel_batching_force_streak() as usize;
        assert!(
            resolved > crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
            "force streak {resolved} must stay strictly greater than nudge \
             threshold {} so the soft→hard cascade is preserved",
            crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD
        );
    }

    #[test]
    fn parallel_batching_force_min_tracks_runtime_nudge_plus_one() {
        assert_eq!(
            astra_config::runtime_config::MIN_PARALLEL_BATCHING_FORCE_STREAK as usize,
            crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD + 1,
            "config floor must stay exactly one round above the runtime nudge \
             threshold so the soft→hard cascade stays aligned across crates"
        );
    }

    /// The same invariant, but exercised through `resolve_for_model` with
    /// every built-in profile. Catches a regression where someone sets a
    /// per-model override below the nudge threshold or lets the config/runtime
    /// floors drift apart across crates.
    #[test]
    fn parallel_batching_force_per_model_above_nudge_threshold() {
        let cfg = astra_config::runtime_config::ToolPolicyConfig::default();
        for model in &[
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "gpt-5",
            "deepseek-v4-flash",
            "deepseek-v4-flash-anthropic",
            "MiniMax-M2.7",
            "unknown-model-id",
        ] {
            let policy = cfg.resolve_for_model(Some(*model));
            assert!(
                policy.parallel_batching_force_streak as usize
                    > crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
                "model={model} resolved force={} must stay strictly greater \
                 than nudge threshold {} so the soft→hard cascade is preserved",
                policy.parallel_batching_force_streak,
                crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
            );
        }
    }

    /// End-to-end wiring: the runtime guard MUST consume the *resolved*
    /// per-model threshold rather than the global default. This pins the
    /// chain `state.policy_model_name → resolve_for_model →
    /// EffectiveToolPolicy::parallel_batching_force_streak →
    /// should_emit_parallel_batching_advisory(_, threshold)`. A regression that
    /// re-routes the guard back to `effective_parallel_batching_force_streak`
    /// (model-blind) would silently break this.
    #[tokio::test]
    async fn parallel_batching_force_uses_resolved_per_model_threshold() {
        for (streak, should_emit) in [(8, false), (11, true)] {
            let mut state = make_state();
            state.message = "explore the codebase".into();
            state.user_intent = state.message.clone();
            state.policy_model_name = Some("claude-haiku-4-5".into());
            state.admitted_tool_policy.model_profiles =
                vec![astra_config::runtime_config::ModelPolicyProfile {
                    model_match: "haiku".into(),
                    parallel_batching_force_streak: 11,
                    ..Default::default()
                }];
            for _ in 0..streak {
                push_single_tool_round(&mut state);
            }
            let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);
            execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
                .await
                .unwrap();
            let delivered = host.executed_volatile[0].iter().any(|entry| {
                entry.kind == VolatileKind::BehaviorAdvisory
                    && entry.payload["schema"] == "runtime_advisory.v1"
                    && entry.payload["evidence"]
                        .as_str()
                        .is_some_and(|text| text.contains(PARALLEL_BATCHING_FORCE_MARKER))
            });
            assert_eq!(
                delivered, should_emit,
                "streak={streak} must use the admitted model policy"
            );
        }
    }

    /// Per-profile clamp invariant: a user that sets
    /// `parallel_batching_force_streak = 1` (or any value at/below
    /// `PARALLEL_BATCHING_NUDGE_THRESHOLD`) MUST land above the nudge
    /// threshold after `apply_profile`'s clamp, otherwise the runtime
    /// advisory arrives before the prompt-layer nudge and the intended
    /// progression is silently inverted.
    #[test]
    fn parallel_batching_force_per_profile_clamp_above_nudge() {
        for low in 1..=crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD as u32 {
            let mut cfg = astra_config::runtime_config::ToolPolicyConfig::default();
            cfg.model_profiles
                .push(astra_config::runtime_config::ModelPolicyProfile {
                    model_match: "haiku".to_string(),
                    parallel_batching_force_streak: low,
                    ..Default::default()
                });
            let policy = cfg.resolve_for_model(Some("us.anthropic.claude-haiku-4-5-20251001-v1:0"));
            assert!(
                (policy.parallel_batching_force_streak as usize)
                    > crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
                "per-profile force={low} resolved to {} but must be > nudge threshold {} \
                 to preserve the soft→hard cascade",
                policy.parallel_batching_force_streak,
                crate::prompts::PARALLEL_BATCHING_NUDGE_THRESHOLD,
            );
        }
    }

    /// Round-budget warning state must not override the resolved per-model
    /// threshold. Pacing hints can be soft; hard correction should stay tied
    /// to the explicit tool-selection policy.
    // ─── Round-budget convergence guard — REMOVED ─────────────────────────
    // The old countdown-based phase1/phase2 tests have been replaced by
    // unit tests in `astra_turn_core::loop_circuit_breaker::tests`.
    // The circuit breaker is integration-tested via the full agentic loop
    // E2E tests.

    #[derive(Clone, Copy, Debug)]
    enum TypedTerminalGateCase {
        Inference(astra_services::InferenceTerminalStatus),
        TerminalControl,
    }

    #[derive(Debug)]
    struct ProviderUsageGateCase {
        name: &'static str,
        dialect: crate::turn::token_usage::UsageDialect,
        raw_usage: serde_json::Value,
        expected: Option<crate::turn::token_usage::TokenUsage>,
    }

    fn provider_usage_gate_cases() -> Vec<ProviderUsageGateCase> {
        use crate::turn::token_usage::{TokenUsage, UsageDialect};

        vec![
            ProviderUsageGateCase {
                name: "openai_inclusive",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({
                    "prompt_tokens": 1_100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {
                        "cached_tokens": 800,
                        "cache_creation_input_tokens": 100,
                    },
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "openai_compatible_disjoint_aliases",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({
                    "prompt_tokens": 200,
                    "completion_tokens": 50,
                    "cache_read_input_tokens": 800,
                    "cache_creation_input_tokens": 100,
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "anthropic_disjoint",
                dialect: UsageDialect::AnthropicMessages,
                raw_usage: serde_json::json!({
                    "input_tokens": 200,
                    "output_tokens": 50,
                    "cache_read_input_tokens": 800,
                    "cache_creation_input_tokens": 100,
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "bedrock_disjoint",
                dialect: UsageDialect::BedrockConverse,
                raw_usage: serde_json::json!({
                    "inputTokens": 200,
                    "outputTokens": 50,
                    "cacheReadInputTokens": 800,
                    "cacheWriteInputTokens": 100,
                }),
                expected: Some(TokenUsage {
                    input_tokens: 200,
                    cached_input_tokens: 800,
                    cache_creation_tokens: 100,
                    output_tokens: 50,
                }),
            },
            ProviderUsageGateCase {
                name: "missing",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({}),
                expected: None,
            },
            ProviderUsageGateCase {
                name: "contradictory_openai_inclusive",
                dialect: UsageDialect::OpenAi,
                raw_usage: serde_json::json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {
                        "cached_tokens": 800,
                        "cache_creation_input_tokens": 100,
                    },
                }),
                expected: Some(TokenUsage {
                    input_tokens: 0,
                    cached_input_tokens: 0,
                    cache_creation_tokens: 0,
                    output_tokens: 50,
                }),
            },
        ]
    }

    #[derive(Debug, PartialEq, Eq)]
    struct UsageGateTelemetry {
        first_ttft_ms: Option<u64>,
        current_session_id: Option<String>,
        current_run_id: Option<String>,
        fresh_input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        output_tokens: u64,
        has_any_usage: bool,
        last_measured_prompt_tokens: Option<u64>,
        consecutive_context_window_errors: u32,
    }

    impl UsageGateTelemetry {
        fn from_state(state: &AgenticLoopState) -> Self {
            Self {
                first_ttft_ms: state.telemetry.first_ttft_ms,
                current_session_id: state.current_session_id.clone(),
                current_run_id: state.current_run_id.clone(),
                fresh_input_tokens: state.total_prompt,
                cache_read_tokens: state.total_cache_read,
                cache_creation_tokens: state.total_cache_creation,
                output_tokens: state.total_completion,
                has_any_usage: state.has_any_usage,
                last_measured_prompt_tokens: state.last_measured_prompt_tokens,
                consecutive_context_window_errors: state.consecutive_context_window_errors,
            }
        }

        fn total_input_tokens(&self) -> u64 {
            NormalizedPromptCacheUsage::new(
                self.fresh_input_tokens,
                self.cache_read_tokens,
                self.cache_creation_tokens,
            )
            .total_input_tokens()
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum UsageGateExecution {
        Succeeded,
        Failed(astra_core::ErrorKind),
        Delegated,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct UsageGateObservation {
        telemetry: UsageGateTelemetry,
        execution: UsageGateExecution,
    }

    fn ingest_usage_gate_snapshot(
        state: &mut AgenticLoopState,
        snap: &AgenticTurnStreamSnapshot<'_>,
    ) -> AgenticTurnIngestOutcome {
        let message = state.message.clone();
        ingest_agentic_turn_stream(
            snap,
            &message,
            AgenticTurnIngestMut {
                model_item_id: None,
                final_text_model_item_id: &mut state.final_text_model_item_id,
                first_ttft_ms: &mut state.telemetry.first_ttft_ms,
                current_session_id: &mut state.current_session_id,
                current_run_id: &mut state.current_run_id,
                final_text: &mut state.final_text,
                last_finish_reason: &mut state.last_finish_reason,
                total_prompt: &mut state.total_prompt,
                total_completion: &mut state.total_completion,
                total_cache_read: &mut state.total_cache_read,
                total_cache_creation: &mut state.total_cache_creation,
                total_tool_calls: &mut state.total_tool_calls,
                total_observation_tool_calls: &mut state.total_observation_tool_calls,
                step_recorder: &mut state.step_recorder,
                all_tools_used: &mut state.telemetry.all_tools_used,
                has_any_usage: &mut state.has_any_usage,
                messages: &mut state.messages,
                last_measured_prompt_tokens: &mut state.last_measured_prompt_tokens,
                consecutive_context_window_errors: &mut state.consecutive_context_window_errors,
            },
        )
    }

    fn exercise_usage_gate(
        usage: Option<crate::turn::token_usage::TokenUsage>,
        physical: Option<astra_turn_types::RequestTokenUsage>,
        terminal: TypedTerminalGateCase,
    ) -> UsageGateObservation {
        let usage = usage.unwrap_or_default();
        let error_kind = match terminal {
            TypedTerminalGateCase::Inference(
                astra_services::InferenceTerminalStatus::Succeeded,
            )
            | TypedTerminalGateCase::TerminalControl => None,
            TypedTerminalGateCase::Inference(astra_services::InferenceTerminalStatus::Failed) => {
                Some(astra_core::ErrorKind::ServerError)
            }
            TypedTerminalGateCase::Inference(
                astra_services::InferenceTerminalStatus::DeliveryUnknown,
            ) => Some(astra_core::ErrorKind::StreamTransport),
            TypedTerminalGateCase::Inference(
                astra_services::InferenceTerminalStatus::Cancelled,
            ) => Some(astra_core::ErrorKind::Cancelled),
        };
        let accum = ChatTurnSseAccum {
            session_id: Some("typed-usage-session".to_string()),
            run_id: Some("typed-usage-run".to_string()),
            current_request_usage: physical,
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            cache_read_tokens: usage.cached_input_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            has_usage: !usage.is_empty(),
            error_message: error_kind.map(|_| "typed provider terminal".to_string()),
            error_kind,
            ..ChatTurnSseAccum::default()
        };
        let snap = agentic_turn_stream_snapshot_with_kind(&accum, Some(17), error_kind);
        let mut state = make_state();

        let execution = match terminal {
            TypedTerminalGateCase::TerminalControl => {
                let outcome = apply_terminal_control_stream_snapshot(
                    &mut state,
                    &snap,
                    crate::turn::terminal_control::TerminalControlOutcome::Requested(
                        crate::turn::terminal_control::TerminalHandoffRequest {
                            handoff_id: "typed-handoff".to_string(),
                            kind: "agent".to_string(),
                            target: "typed-target".to_string(),
                            action: "delegate".to_string(),
                            terminal: true,
                            tool_call_id: "typed-tool-call".to_string(),
                        },
                    ),
                );
                assert!(matches!(outcome, AgenticLoopOutcome::Delegated));
                UsageGateExecution::Delegated
            }
            TypedTerminalGateCase::Inference(status) => {
                let outcome = ingest_usage_gate_snapshot(&mut state, &snap);
                match (status, outcome) {
                    (
                        astra_services::InferenceTerminalStatus::Succeeded,
                        AgenticTurnIngestOutcome::Break,
                    ) => UsageGateExecution::Succeeded,
                    (
                        astra_services::InferenceTerminalStatus::Failed
                        | astra_services::InferenceTerminalStatus::DeliveryUnknown
                        | astra_services::InferenceTerminalStatus::Cancelled,
                        AgenticTurnIngestOutcome::Fatal(error),
                    ) => UsageGateExecution::Failed(error.kind),
                    (status, outcome) => {
                        panic!("unexpected typed terminal outcome: {status:?} -> {outcome:?}")
                    }
                }
            }
        };

        UsageGateObservation {
            telemetry: UsageGateTelemetry::from_state(&state),
            execution,
        }
    }

    #[test]
    fn provider_usage_terminal_cross_product_preserves_typed_telemetry() {
        use crate::turn::token_usage::parse_usage;
        use astra_services::InferenceTerminalStatus;

        let terminal_cases = [
            TypedTerminalGateCase::Inference(InferenceTerminalStatus::Succeeded),
            TypedTerminalGateCase::Inference(InferenceTerminalStatus::Failed),
            TypedTerminalGateCase::Inference(InferenceTerminalStatus::DeliveryUnknown),
            TypedTerminalGateCase::TerminalControl,
        ];

        for provider in provider_usage_gate_cases() {
            let parsed = provider
                .raw_usage
                .as_object()
                .and_then(|usage| parse_usage(provider.dialect, usage));
            let extracted = parsed.map(|(usage, _)| usage);
            let physical = parsed.and_then(|(usage, presence)| {
                (presence.fresh_input_tokens
                    && presence.cache_read_tokens
                    && presence.cache_creation_tokens
                    && presence.output_tokens)
                    .then(|| {
                        astra_turn_types::RequestTokenUsage::try_new(
                            usage.input_tokens,
                            usage.cached_input_tokens,
                            usage.cache_creation_tokens,
                            usage.output_tokens,
                        )
                        .ok()
                    })
                    .flatten()
            });
            assert_eq!(
                extracted, provider.expected,
                "provider usage normalization: {}",
                provider.name
            );
            let expected = extracted.unwrap_or_default();
            let expected_total_input = expected
                .normalized_prompt_cache_usage()
                .total_input_tokens();

            for terminal in terminal_cases {
                let observed = exercise_usage_gate(extracted, physical, terminal);

                assert_eq!(
                    observed.telemetry.fresh_input_tokens, expected.input_tokens,
                    "fresh input: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    observed.telemetry.cache_read_tokens, expected.cached_input_tokens,
                    "cache read: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    observed.telemetry.cache_creation_tokens, expected.cache_creation_tokens,
                    "cache create: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    observed.telemetry.output_tokens, expected.output_tokens,
                    "output: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(
                    observed.telemetry.total_input_tokens(),
                    expected_total_input,
                    "fresh + read + create: {} / {terminal:?}",
                    provider.name
                );
                assert_eq!(observed.telemetry.has_any_usage, extracted.is_some());
                assert_eq!(observed.telemetry.first_ttft_ms, Some(17));
                assert_eq!(
                    observed.telemetry.current_session_id.as_deref(),
                    Some("typed-usage-session")
                );
                assert_eq!(
                    observed.telemetry.current_run_id.as_deref(),
                    Some("typed-usage-run")
                );

                let expected_execution = match terminal {
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Succeeded) => {
                        UsageGateExecution::Succeeded
                    }
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Failed) => {
                        UsageGateExecution::Failed(astra_core::ErrorKind::ServerError)
                    }
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::DeliveryUnknown) => {
                        UsageGateExecution::Failed(astra_core::ErrorKind::StreamTransport)
                    }
                    TypedTerminalGateCase::TerminalControl => UsageGateExecution::Delegated,
                    TypedTerminalGateCase::Inference(InferenceTerminalStatus::Cancelled) => {
                        unreachable!("cancelled is outside this exit-gate matrix")
                    }
                };
                assert_eq!(observed.execution, expected_execution);

                let expected_calibration = physical.map(|_| expected_total_input);
                assert_eq!(
                    observed.telemetry.last_measured_prompt_tokens, expected_calibration,
                    "prompt calibration: {} / {terminal:?}",
                    provider.name
                );
            }
        }
    }

    fn prep(quiet: bool) -> TurnIterationPrep {
        TurnIterationPrep {
            quiet,
            turn_start_time: Instant::now(),
        }
    }

    #[tokio::test]
    async fn user_intent_injects_immediately_at_loop_top() {
        let mut state = make_state();
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::MustMutate,
            ),
        );
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.current_run_id = Some("run-queued".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 2,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 1,
                input: serde_json::json!({"content": "Switch to writing tests first."}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 2);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(host.user_intent_context_indices, vec![1]);
        assert_eq!(host.user_intent_applied_indices, vec![1]);
        assert!(
            state.turn_intent.is_none(),
            "a new semantic user turn must not inherit the old typed intent"
        );
        assert!(!state.task_profile.mutates_workspace);
        assert!(!state.task_profile.verification_required);
        assert_eq!(state.message, "Switch to writing tests first.");
        assert_eq!(
            state.user_intents.applied_user_intents(),
            &[AppliedUserIntent {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 1,
                content: "Switch to writing tests first.".to_string(),
            }]
        );
        assert_eq!(
            state
                .messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str()),
            Some("Switch to writing tests first.")
        );
        assert!(
            state.volatile_pending.is_empty(),
            "real deferred user input must not be duplicated as runtime context"
        );
    }

    #[tokio::test]
    async fn action_boundary_drains_settled_page_before_observing_next_page_guidance() {
        let mut state = make_state();
        state.current_run_id = Some("run-paged-boundary".into());
        state.owner_user_id = Some("user-paged-boundary".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 256,
                snapshot_has_more: true,
                snapshot_page_fact_count: 256,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 257,
                snapshot_has_more: false,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-on-second-page".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 257,
                    input: serde_json::json!({"content": "observe before any stale action"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap(),
            "second-page guidance must invalidate the stale action snapshot"
        );
        assert_eq!(*provider.poll_calls.lock().await, vec![0, 256]);
        assert_eq!(*provider.released.lock().await, vec![257]);
        assert_eq!(state.user_intents.user_intent_cursor(), 257);
        assert_eq!(state.message, "observe before any stale action");
    }

    #[tokio::test]
    async fn action_boundary_fails_closed_when_control_snapshot_exceeds_page_cap() {
        let mut state = make_state();
        state.current_run_id = Some("run-over-page-cap".into());
        state.owner_user_id = Some("user-over-page-cap".into());
        let polls = (1..=MAX_USER_INTENT_BOUNDARY_PAGES)
            .map(|page| UserIntentPoll {
                next_cursor: page,
                snapshot_has_more: true,
                snapshot_page_fact_count: 256,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            })
            .collect();
        let provider = Arc::new(StubRunControlProvider::new(polls));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("an unbounded authoritative snapshot must block provider actions");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(error.message.contains("boundary drain limit"));
        assert_eq!(
            provider.poll_call_count().await,
            MAX_USER_INTENT_BOUNDARY_PAGES
        );
        assert_eq!(
            state.user_intents.user_intent_cursor(),
            MAX_USER_INTENT_BOUNDARY_PAGES - 1,
            "the unprocessed over-cap page must not advance the durable cursor"
        );
    }

    #[tokio::test]
    async fn middle_page_apply_failure_keeps_cursor_for_exact_retry() {
        let mut state = make_state();
        state.current_run_id = Some("run-middle-page-retry".into());
        state.owner_user_id = Some("user-middle-page-retry".into());
        let pending_page = || UserIntentPoll {
            next_cursor: 257,
            snapshot_has_more: false,
            snapshot_page_fact_count: 1,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "intent-middle-page".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 257,
                input: serde_json::json!({"content": "retry this exact page"}),
            }],
            issues: Vec::new(),
            error: None,
        };
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                UserIntentPoll {
                    next_cursor: 256,
                    snapshot_has_more: true,
                    snapshot_page_fact_count: 256,
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: None,
                },
                pending_page(),
                pending_page(),
            ],
            1,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("middle-page apply failure must fail the action boundary");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(state.user_intents.user_intent_cursor(), 256);
        assert!(provider.released.lock().await.is_empty());

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap()
        );
        assert_eq!(*provider.poll_calls.lock().await, vec![0, 256, 256]);
        assert_eq!(*provider.released.lock().await, vec![257]);
        assert_eq!(state.user_intents.user_intent_cursor(), 257);
        assert_eq!(state.message, "retry this exact page");
    }

    #[tokio::test]
    async fn settlement_boundary_forces_poll_before_empty_poll_cooldown_expires() {
        let mut state = make_state();
        state.current_run_id = Some("run-settling".into());
        state.owner_user_id = Some("user-settling".into());
        state
            .user_intents
            .note_user_intent_poll_finished(tokio::time::Instant::now(), Duration::from_secs(60));
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 9,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "intent-at-final-boundary".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 8,
                input: serde_json::json!({"content": "stop here"}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        assert!(
            inject_polled_user_intents_before_settlement(&mut host, &mut state)
                .await
                .unwrap(),
            "accepted guidance must keep the run open for another model boundary"
        );
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(*provider.released.lock().await, vec![8]);
        assert_eq!(state.message, "stop here");
    }

    #[tokio::test]
    async fn real_run_engine_drains_accepted_intent_without_a_separate_admission_gate() {
        let engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        engine
            .start_run("run-real-fence-drain", "user-real", "session-real")
            .await
            .unwrap();
        let accepted = serde_json::json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-real-fence-drain",
            "data": {
                "intent_id": "intent-real-fence-drain",
                "delivery": "guide_current_run",
                "input": {"content": "drain this accepted guidance"}
            }
        });
        assert_eq!(
            engine
                .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                    user_id: "user-real",
                    expected_session_id: "session-real",
                    run_id: "run-real-fence-drain",
                    intent_id: "intent-real-fence-drain",
                    event: &accepted,
                    process_local_execution_live: true,
                })
                .await
                .unwrap(),
            AtomicRunGuidanceAdmission::Committed { event_index: 1 }
        );

        let mut state = make_state();
        state.current_run_id = Some("run-real-fence-drain".into());
        state.current_run_owner_generation = Some(0);
        state.owner_user_id = Some("user-real".into());
        state.current_session_id = Some("session-real".into());
        state.run_control = Some(engine.clone());
        let mut host = MockHost::new(vec![]);
        assert!(
            inject_polled_user_intents_before_settlement(&mut host, &mut state)
                .await
                .unwrap(),
            "the forced boundary must ingest the accepted pre-cutoff intent"
        );
        assert_eq!(state.message, "drain this accepted guidance");

        let after_reopen = serde_json::json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-real-after-reopen",
            "data": {
                "intent_id": "intent-real-after-reopen",
                "delivery": "guide_current_run",
                "input": {"content": "continue after reopen"}
            }
        });
        assert!(matches!(
            engine
                .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                    user_id: "user-real",
                    expected_session_id: "session-real",
                    run_id: "run-real-fence-drain",
                    intent_id: "intent-real-after-reopen",
                    event: &after_reopen,
                    process_local_execution_live: true,
                })
                .await
                .unwrap(),
            AtomicRunGuidanceAdmission::Committed { .. }
        ));
        let run = engine
            .load_run("user-real", "run-real-fence-drain")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events[2]["event_type"], "user_intent_applied");
        assert_eq!(run.events[3]["event_type"], "user_intent");
    }

    #[tokio::test]
    async fn real_run_engine_rejects_stale_terminal_and_missing_provider_authority_before_host() {
        let engine = Arc::new(RunEngine::new(Arc::new(InMemoryRunStateStore::new())));
        engine
            .start_run(
                "run-provider-authority",
                "user-provider",
                "session-provider",
            )
            .await
            .unwrap();

        let mut stale = make_state();
        stale.current_run_id = Some("run-provider-authority".into());
        stale.current_run_owner_generation = Some(1);
        stale.owner_user_id = Some("user-provider".into());
        stale.current_session_id = Some("session-provider".into());
        stale.run_control = Some(engine.clone());
        let mut stale_progress = attach_llm_progress_receiver(&mut stale);
        let mut stale_host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let stale_result = execute_turn_and_ingest_phase(
            &mut stale_host,
            &mut stale,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let Err(stale_error) = stale_result else {
            panic!("rotated generation must stop before the provider");
        };
        assert_eq!(stale_error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(stale_host.turn_count(), 0);
        assert_no_llm_progress(&mut stale_progress);

        engine
            .persist_typed_cancellation_fixture(
                "user-provider",
                "session-provider",
                "run-provider-authority",
                &[astra_core::STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            )
            .await
            .unwrap();
        let mut terminal = make_state();
        terminal.current_run_id = Some("run-provider-authority".into());
        terminal.current_run_owner_generation = Some(0);
        terminal.owner_user_id = Some("user-provider".into());
        terminal.current_session_id = Some("session-provider".into());
        terminal.run_control = Some(engine.clone());
        let mut terminal_progress = attach_llm_progress_receiver(&mut terminal);
        let mut terminal_host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let terminal_result = execute_turn_and_ingest_phase(
            &mut terminal_host,
            &mut terminal,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let Err(terminal_error) = terminal_result else {
            panic!("terminal run must stop before the provider");
        };
        assert_eq!(terminal_error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(terminal_host.turn_count(), 0);
        assert_no_llm_progress(&mut terminal_progress);

        let mut missing = make_state();
        missing.current_run_id = Some("run-provider-authority".into());
        missing.current_run_owner_generation = None;
        missing.owner_user_id = Some("user-provider".into());
        missing.run_control = Some(engine);
        let mut missing_progress = attach_llm_progress_receiver(&mut missing);
        let mut missing_host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let missing_result = execute_turn_and_ingest_phase(
            &mut missing_host,
            &mut missing,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let Err(missing_error) = missing_result else {
            panic!("durable provider without a generation must fail closed");
        };
        assert_eq!(missing_error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(missing_host.turn_count(), 0);
        assert_no_llm_progress(&mut missing_progress);
    }

    #[tokio::test]
    async fn paused_provider_boundary_is_a_hold_not_a_cancellation() {
        let provider = Arc::new(BoundaryDecisionRunControl {
            decision: Ok(ProviderBoundaryAuthorization::Paused),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let pause_flag = Arc::new(AtomicBool::new(false));
        let mut state = make_state();
        state.current_run_id = Some("run-paused-boundary".into());
        state.current_run_owner_generation = Some(7);
        state.owner_user_id = Some("user-paused-boundary".into());
        state.current_session_id = Some("session-paused-boundary".into());
        state.run_control = Some(provider.clone());
        state.cancellation.pause_flag = Some(pause_flag.clone());

        assert_eq!(
            authorize_provider_boundary(&mut state).await.unwrap(),
            ProviderBoundaryGate::Paused
        );
        assert!(pause_flag.load(Ordering::Acquire));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn paused_provider_boundary_keeps_original_execution_deadline() {
        for preparation in [false, true] {
            let mut state = make_state();
            state.current_run_id = Some("run-paused-deadline".into());
            state.current_run_owner_generation = Some(7);
            state.owner_user_id = Some("user-paused-deadline".into());
            state.current_session_id = Some("session-paused-deadline".into());
            state.run_control = Some(Arc::new(BoundaryDecisionRunControl {
                decision: Ok(ProviderBoundaryAuthorization::Paused),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }));
            state.cancellation.pause_flag = Some(Arc::new(AtomicBool::new(preparation)));
            let mut host = MockHost::new(vec![]).with_execution_time_budget_remaining(
                astra_turn_types::ExecutionTimeRemaining {
                    work_remaining: Duration::ZERO,
                    total_remaining: Duration::from_millis(50),
                },
            );
            let started = tokio::time::Instant::now();
            let error = if preparation {
                super::super::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
                    .await
                    .err()
                    .unwrap()
            } else {
                execute_turn_and_ingest_phase(
                    &mut host,
                    &mut state,
                    0,
                    TurnIterationPrep {
                        quiet: true,
                        turn_start_time: Instant::now(),
                    },
                )
                .await
                .err()
                .unwrap()
            };
            assert_eq!(error.kind, astra_core::ErrorKind::BudgetExhausted);
            assert_eq!(started.elapsed(), Duration::from_millis(50));
            assert_eq!(host.turn_count(), 0);
            assert_eq!(state.llm_rounds_completed, 0);
            assert!(
                state
                    .cancellation
                    .pause_flag
                    .unwrap()
                    .load(Ordering::Acquire)
            );
        }
    }

    #[tokio::test]
    async fn paused_provider_boundary_without_retained_control_fails_before_host() {
        let provider = Arc::new(BoundaryDecisionRunControl {
            decision: Ok(ProviderBoundaryAuthorization::Paused),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut state = make_state();
        state.current_run_id = Some("run-paused-boundary".into());
        state.current_run_owner_generation = Some(7);
        state.owner_user_id = Some("user-paused-boundary".into());
        state.current_session_id = Some("session-paused-boundary".into());
        state.run_control = Some(provider.clone());
        state.cancellation.pause_flag = None;
        let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);
        let error = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            0,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .err()
        .expect("missing retained pause control must fail closed");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.turn_count(), 0);
        assert_eq!(state.llm_rounds_completed, 0);
    }

    #[tokio::test]
    async fn provider_boundary_typed_unhappy_outcomes_never_enter_host() {
        for (decision, expected_kind) in [
            (
                Ok(ProviderBoundaryAuthorization::AuthorityLost {
                    reason: "owner lease expired".to_string(),
                }),
                astra_core::ErrorKind::Cancelled,
            ),
            (
                Ok(ProviderBoundaryAuthorization::Inactive {
                    status: astra_core::STATUS_COMPLETED.to_string(),
                }),
                astra_core::ErrorKind::Cancelled,
            ),
            (
                Err("database unavailable".to_string()),
                astra_core::ErrorKind::Unknown,
            ),
        ] {
            let provider = Arc::new(BoundaryDecisionRunControl {
                decision,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut state = make_state();
            state.current_run_id = Some("run-boundary-decision".into());
            state.current_run_owner_generation = Some(7);
            state.owner_user_id = Some("user-boundary-decision".into());
            state.current_session_id = Some("session-boundary-decision".into());
            state.run_control = Some(provider.clone());
            let mut progress = attach_llm_progress_receiver(&mut state);
            let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);

            let result = execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await;
            let Err(error) = result else {
                panic!("provider authorization rejection must be fail-closed");
            };
            assert_eq!(error.kind, expected_kind);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(host.turn_count(), 0);
            assert_no_llm_progress(&mut progress);
        }
    }

    #[tokio::test]
    async fn provider_progress_is_emitted_only_for_real_host_calls_and_is_always_paired() {
        let mut work_carrier = text_result("", 3, 2, Some(7));
        work_carrier.response.accum.has_tool_calls = true;
        work_carrier.response.accum.tool_calls = vec![serde_json::json!({
            "id": "work-establishment", "type": "function",
            "function": {"name": "start_work", "arguments": "{}"}
        })];
        for response in [text_result("done", 3, 2, Some(7)), work_carrier] {
            let mut success_state = make_state();
            let mut success_progress = attach_llm_progress_receiver(&mut success_state);
            let mut success_host = MockHost::new(vec![response]);
            execute_turn_and_ingest_phase(
                &mut success_host,
                &mut success_state,
                4,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .expect("successful host boundary");
            assert_eq!(success_host.turn_count(), 1);
            assert_one_paired_llm_progress(&mut success_progress, 4, Some(7));
            assert_eq!(success_state.llm_rounds_completed, 1);
            assert_eq!(success_state.telemetry.local_usage_attempts, 1);
            assert_eq!(success_state.telemetry.local_usage_provider_reported, 1);
            assert!(success_state.current_model_item_id.is_some());
        }

        let mut error_state = make_state();
        let mut error_progress = attach_llm_progress_receiver(&mut error_state);
        let mut error_host = DirectErrorHost::new(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ServerError,
            "provider rejected request",
        ));
        let result = execute_turn_and_ingest_phase(
            &mut error_host,
            &mut error_state,
            9,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(error_host.turn_count(), 1);
        assert_one_paired_llm_progress(&mut error_progress, 9, None);
        assert_eq!(error_state.llm_rounds_completed, 1);
        assert_eq!(error_state.telemetry.local_usage_attempts, 1);
        assert_eq!(error_state.telemetry.local_usage_unavailable, 1);
    }

    #[tokio::test]
    async fn preparation_context_failure_preserves_structured_diagnostics() {
        let details = serde_json::json!({"source":"final_wire","stage":"request_preparation","execution_started":false,"budget":{"input_tokens":1234,"limit":1000}}).to_string();
        let mut host = DirectErrorHost::new(
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContextWindow,
                "protected request is too large",
            )
            .with_details_json(details.clone()),
        );
        let mut state = make_state();
        state.messages = vec![
            serde_json::json!({"role":"system","content":"protected"}),
            serde_json::json!({"role":"user","content":"current goal"}),
        ];
        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            1,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await;
        let error = result.err().expect("protected overflow remains actionable");
        assert_eq!(error.kind, astra_core::ErrorKind::ContextWindow);
        assert_eq!(error.details_json.as_deref(), Some(details.as_str()));
        assert_eq!(host.turn_count(), 1);
    }

    #[tokio::test]
    async fn hard_round_limit_rejection_emits_no_provider_progress() {
        let mut state = make_state();
        state.llm_rounds_completed = 1;
        state.stall.circuit_breaker =
            astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker::new(
                astra_turn_core::loop_circuit_breaker::BreakerConfig {
                    absolute_max_rounds: 1,
                    ..Default::default()
                },
            );
        let mut progress = attach_llm_progress_receiver(&mut state);
        let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            1,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("hard infrastructure limit is a typed pre-host return");

        assert!(matches!(
            result,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.turn_count(), 0);
        assert_no_llm_progress(&mut progress);
    }

    #[cfg(feature = "harness")]
    #[tokio::test]
    async fn harness_pre_llm_rejection_emits_no_provider_progress() {
        let mut state = make_state();
        let sink: Arc<dyn astra_harness::SnapshotSink> = astra_harness::InMemorySnapshotSink::arc();
        state.harness =
            crate::turn::harness_adapter::HarnessSlot::new(Arc::new(PreLlmBlockingHarness), sink);
        let mut progress = attach_llm_progress_receiver(&mut state);
        let mut host = MockHost::new(vec![text_result("must not run", 1, 1, None)]);

        let result = execute_turn_and_ingest_phase(
            &mut host,
            &mut state,
            3,
            TurnIterationPrep {
                quiet: true,
                turn_start_time: Instant::now(),
            },
        )
        .await
        .expect("harness block is a typed pre-host return");

        assert!(matches!(
            result,
            TurnExecutionControl::Return(AgenticLoopOutcome::Completed)
        ));
        assert_eq!(host.turn_count(), 0);
        assert_no_llm_progress(&mut progress);
    }

    #[tokio::test]
    async fn settlement_boundary_fails_closed_when_durable_poll_is_unavailable() {
        let mut state = make_state();
        state.current_run_id = Some("run-settling".into());
        state.owner_user_id = Some("user-settling".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 0,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: Vec::new(),
            issues: Vec::new(),
            error: Some("durable store unavailable".into()),
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_settlement(&mut host, &mut state)
            .await
            .expect_err("settlement cannot claim completion without closing the intent lane");

        assert_eq!(error.kind, astra_core::ErrorKind::Unknown);
        assert!(
            error
                .to_string()
                .contains("authoritative user intent boundary poll failed")
        );
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(state.user_intents.user_intent_cursor(), 0);
    }

    #[tokio::test]
    async fn action_boundary_forces_guidance_poll_before_stale_tool_execution() {
        let mut state = make_state();
        state.current_run_id = Some("run-action-fence".into());
        state.owner_user_id = Some("user-action-fence".into());
        state
            .user_intents
            .note_user_intent_poll_finished(tokio::time::Instant::now(), Duration::from_secs(60));
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 4,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "intent-during-provider".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 3,
                input: serde_json::json!({"content": "change direction before acting"}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap(),
            "new guidance must advance the execution epoch before tools are admitted"
        );
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(*provider.released.lock().await, vec![3]);
        assert_eq!(state.message, "change direction before acting");
    }

    #[tokio::test]
    async fn action_boundary_fails_closed_when_guidance_apply_ack_fails() {
        let mut state = make_state();
        state.current_run_id = Some("run-action-ack".into());
        state.owner_user_id = Some("user-action-ack".into());
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![UserIntentPoll {
                next_cursor: 4,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-action-ack".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 3,
                    input: serde_json::json!({"content": "do not execute the stale tool"}),
                }],
                issues: Vec::new(),
                error: None,
            }],
            1,
        ));
        state.run_control = Some(provider);
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("an uncommitted control epoch must not release stale actions");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(error.to_string().contains("durably acknowledged"));
    }

    #[tokio::test]
    async fn forced_action_boundary_retries_pending_apply_ack_during_backoff() {
        let mut state = make_state();
        state.current_run_id = Some("run-pending-action-ack".into());
        state.owner_user_id = Some("user-pending-action-ack".into());
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                UserIntentPoll {
                    next_cursor: 4,
                    snapshot_has_more: false,
                    snapshot_page_fact_count: 0,
                    inputs: vec![crate::turn::run_control::QueuedUserIntent {
                        intent_id: "intent-pending-action-ack".into(),
                        delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                        status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                        event_index: 3,
                        input: serde_json::json!({"content": "replace stale action"}),
                    }],
                    issues: Vec::new(),
                    error: None,
                },
                UserIntentPoll::default(),
            ],
            1,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .expect("ordinary cadence may defer the failed ACK");
        assert!(provider.released.lock().await.is_empty());

        assert!(
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .expect("forced fence must retry rather than honor cadence backoff")
        );
        assert_eq!(*provider.released.lock().await, vec![3]);
        assert_eq!(state.message, "replace stale action");
    }

    #[tokio::test]
    async fn action_boundary_discards_stale_actions_when_terminal_wins_apply_race() {
        let mut state = make_state();
        state.current_run_id = Some("run-action-terminal".into());
        state.owner_user_id = Some("user-action-terminal".into());
        let provider = Arc::new(StubRunControlProvider::with_terminal_release(vec![
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-terminal-race".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "stop"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider);
        let mut host = MockHost::new(vec![]);

        let error = inject_polled_user_intents_before_action(&mut host, &mut state)
            .await
            .expect_err("a terminal owner must prevent execution of the stale response");
        assert_eq!(error.kind, astra_core::ErrorKind::Cancelled);
    }

    #[test]
    fn superseded_provider_round_keeps_usage_and_trace_without_content_authority() {
        let mut state = make_state();
        state.last_measured_prompt_tokens = Some(999);
        state.current_run_id = Some("run-superseded".into());
        let mut result = text_result("stale answer", 120, 11, Some(7));
        result.accum.qualified_usage = Some(
            astra_turn_types::CanonicalTokenUsage::new(Some(120), None, None, Some(11)).unwrap(),
        );
        result.accum.current_request_usage = None;
        result.accum.cache_read_tokens = 30;
        result.accum.cache_creation_tokens = 5;
        result.accum.tool_calls = vec![serde_json::json!({
            "id":"stale-tool",
            "type":"function",
            "function":{"name":"read_file","arguments":"{}"}
        })];

        record_superseded_llm_round(&mut state, &result, Instant::now());

        assert_eq!(state.total_prompt, 120);
        assert_eq!(state.qualified_usage, result.accum.qualified_usage);
        assert_eq!(state.total_completion, 11);
        assert_eq!(state.total_cache_read, 30);
        assert_eq!(state.total_cache_creation, 5);
        assert!(state.has_any_usage);
        assert_eq!(state.last_measured_prompt_tokens, None);
        let round = state.recent_rounds.last().expect("physical round retained");
        assert_eq!(
            round.finish_reason.as_deref(),
            Some("superseded_by_runtime_input")
        );
        assert_eq!(round.tool_call_names, ["read_file"]);
        assert!(
            state.messages.is_empty(),
            "stale content must not be ingested"
        );
        assert!(
            state.stall.tool_call_records.is_empty(),
            "stale tools must not execute"
        );
        result.accum.current_request_usage = Some(astra_turn_types::RequestTokenUsage::default());
        record_superseded_llm_round(&mut state, &result, Instant::now());
        assert_eq!(state.last_measured_prompt_tokens, Some(0));
    }

    #[tokio::test]
    async fn guidance_preempts_a_stalled_inference_and_continues_the_run() {
        let mut state = make_state();
        state.current_run_id = Some("run-preempted-inference".into());
        state.current_run_owner_generation = Some(7);
        state.owner_user_id = Some("user-preempted-inference".into());
        let guidance = "answer the question now, then continue";
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            UserIntentPoll {
                next_cursor: 2,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-preempt".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": guidance}),
                }],
                ..UserIntentPoll::default()
            },
        ]));
        state.run_control = Some(provider.clone());
        let (wake, receive) = tokio::sync::watch::channel(-1);
        state.user_intents.guidance_wake = Some(receive);
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        state.cancellation.token = Some(parent_cancel.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let never_release = Arc::new(tokio::sync::Notify::new());
        let stale = server_tool_result(
            vec![
                serde_json::json!({"id":"stale","type":"function","function":{"name":"read_file","arguments":"{}"}}),
            ],
            vec![],
            0,
            0,
            None,
        );
        let mut host = MockHost::new(vec![stale, text_result("revised result", 5, 2, None)])
            .with_valid_tools(&["read_file"])
            .with_admission_hook();
        host.provider_response_gate = Some((started.clone(), never_release));
        let run = tokio::spawn(async move {
            let result = run_agentic_loop_with_host(&mut host, &mut state).await;
            (result, host, state)
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("first inference started");
        wake.send_replace(1);
        let (outcome, host, state) = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("guidance must not wait for the stalled provider")
            .unwrap();
        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
            "{outcome:?}"
        );
        assert!(
            !parent_cancel.is_cancelled(),
            "the parent run remains active"
        );
        assert!(host.admitted_tool_call_batches.is_empty());
        let resumed = serde_json::to_string(&host.executed_messages).unwrap();
        assert!(resumed.contains(guidance));
        assert!(!resumed.contains("unsettled completion claim"));
        assert!(!resumed.contains("unexecuted-tool-fragment"));
        assert_eq!(state.final_text, "revised result");
        assert_eq!(*provider.released.lock().await, vec![1]);
    }

    #[tokio::test]
    async fn guidance_wake_without_an_instruction_does_not_retry_stale_inference() {
        let mut state = make_state();
        state.current_run_id = Some("run-empty-guidance-wake".into());
        state.current_run_owner_generation = Some(7);
        state.owner_user_id = Some("user-empty-guidance-wake".into());
        state.run_control = Some(Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            UserIntentPoll::default(),
        ])));
        let (wake, receive) = tokio::sync::watch::channel(-1);
        state.user_intents.guidance_wake = Some(receive);
        state.cancellation.token = Some(Arc::new(tokio_util::sync::CancellationToken::new()));
        let started = Arc::new(tokio::sync::Notify::new());
        let mut host = MockHost::new(vec![text_result("stale", 0, 0, None)]);
        host.provider_response_gate = Some((started.clone(), Arc::new(tokio::sync::Notify::new())));
        let run = tokio::spawn(async move {
            let result = run_agentic_loop_with_host(&mut host, &mut state).await;
            (result, host)
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        wake.send_replace(1);
        let (result, host) = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("an empty wake must not authorize a retry"),
        };
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(host.admitted_tool_call_batches.is_empty());
    }

    #[tokio::test]
    async fn mid_provider_guidance_discards_stale_tool_and_reaches_next_request() {
        let mut state = make_state();
        state.current_run_id = Some("run-mid-provider-guidance".into());
        state.current_run_owner_generation = Some(7);
        state.owner_user_id = Some("user-mid-provider-guidance".into());
        let guidance = "answer the revised question without reading files";
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            UserIntentPoll {
                next_cursor: 2,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-mid-provider".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": guidance}),
                }],
                ..UserIntentPoll::default()
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut stale = server_tool_result(
            vec![serde_json::json!({
                "id": "stale-tool",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}
            })],
            vec![],
            120,
            11,
            Some(7),
        );
        stale.accum.cache_read_tokens = 30;
        stale.accum.cache_creation_tokens = 5;
        let mut host = MockHost::new(vec![stale, text_result("revised result", 5, 2, None)])
            .with_valid_tools(&["read_file"])
            .with_admission_hook();

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
            "{outcome:?}"
        );
        assert_eq!(host.turn_count(), 2);
        // MockHost captures the input to the host, not the serialized HTTP wire.
        assert!(
            !serde_json::to_string(&host.executed_messages[0])
                .unwrap()
                .contains(guidance)
        );
        let next_request = serde_json::to_string(&host.executed_messages[1]).unwrap();
        assert_eq!(next_request.matches(guidance).count(), 1);
        assert!(!next_request.contains("stale-tool"));
        assert!(host.admitted_tool_call_batches.is_empty());
        assert!(state.stall.tool_call_records.is_empty());
        assert_eq!(state.total_prompt, 125);
        assert_eq!(state.total_completion, 13);
        assert_eq!(state.total_cache_read, 30);
        assert_eq!(state.total_cache_creation, 5);
        let discarded = state
            .recent_rounds
            .iter()
            .find(|round| round.finish_reason.as_deref() == Some("superseded_by_runtime_input"))
            .expect("discarded physical round remains observable");
        assert_eq!(discarded.tool_call_names, ["read_file"]);
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(state.final_text, "revised result");
    }

    #[test]
    fn auxiliary_totals_do_not_change_primary_context_or_round_usage() {
        let mut state = make_state();
        // Auxiliary usage was settled by its host before primary ingestion.
        state.total_prompt = 1_000;
        state.total_completion = 3;
        let mut result = text_result("answer", 100, 10, Some(7));
        result.accum.cache_read_tokens = 900;
        result.accum.current_request_usage =
            Some(astra_turn_types::RequestTokenUsage::try_new(100, 900, 0, 10).unwrap());
        record_superseded_llm_round(&mut state, &result, Instant::now());
        assert_eq!(state.provider_input_tokens(), 2_000);
        assert_eq!(state.provider_total_tokens(), 2_013);
        assert_eq!(state.last_measured_prompt_tokens, Some(1_000));
        let round = state.recent_rounds.last().unwrap();
        assert_eq!(round.prompt_tokens, 100);
        assert_eq!(round.cache_read_tokens, 900);
        assert_eq!(round.completion_tokens, 10);
    }

    #[test]
    fn partial_pricing_evidence_still_calibrates_the_physical_input_budget() {
        let mut state = make_state();
        state.last_measured_prompt_tokens = Some(7);
        let mut result = text_result("answer", 10_000, 20, Some(7));
        result.accum.cache_read_tokens = 90_000;
        result.accum.current_request_usage = None;
        result.accum.current_request_input_tokens = Some(100_000);

        record_superseded_llm_round(&mut state, &result, Instant::now());

        assert_eq!(state.last_request_usage, None);
        assert_eq!(state.last_measured_prompt_tokens, Some(100_000));
        assert_eq!(state.total_prompt, 10_000);
        assert_eq!(state.total_cache_read, 90_000);
    }

    #[tokio::test]
    async fn nonterminal_completion_guards_continue_the_same_owned_run() {
        #[derive(Clone, Copy, Debug)]
        enum ContinuationCase {
            WorkSettlement,
            VerificationGuard,
            GuidanceArrived,
        }

        for case in [
            ContinuationCase::WorkSettlement,
            ContinuationCase::VerificationGuard,
            ContinuationCase::GuidanceArrived,
        ] {
            let mut state = make_state();
            state.current_run_id = Some(format!("run-{case:?}"));
            state.current_run_owner_generation = Some(17);
            state.owner_user_id = Some("user-settlement-guard".into());
            let polls = if matches!(case, ContinuationCase::GuidanceArrived) {
                vec![
                    UserIntentPoll::default(),
                    UserIntentPoll::default(),
                    UserIntentPoll {
                        next_cursor: 2,
                        snapshot_has_more: false,
                        snapshot_page_fact_count: 0,
                        inputs: vec![crate::turn::run_control::QueuedUserIntent {
                            intent_id: "intent-settlement-guard".into(),
                            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                            status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                            event_index: 1,
                            input: serde_json::json!({"content": "continue with this guidance"}),
                        }],
                        issues: Vec::new(),
                        error: None,
                    },
                ]
            } else {
                vec![UserIntentPoll::default(); 3]
            };
            let provider = Arc::new(StubRunControlProvider::new(polls));
            state.run_control = Some(provider.clone());

            match case {
                ContinuationCase::WorkSettlement => {
                    state.hooks.completion_settlement.work_settlement_only = true;
                }
                ContinuationCase::VerificationGuard => {
                    state.task_profile.mutates_workspace = true;
                    state.task_profile.verification_required = true;
                    state
                        .hooks
                        .stop_hooks
                        .push(explicit_verification_hook("quality", "./quality-gate"));
                    state
                        .stall
                        .tool_call_records
                        .push(executed_record("write_file", true, None));
                    state
                        .stall
                        .tool_call_records
                        .push(executed_record("read_file", true, None));
                }
                ContinuationCase::GuidanceArrived => {}
            }

            let mut host = MockHost::new(vec![text_result("candidate", 5, 2, None)]);
            let control = execute_turn_and_ingest_phase(
                &mut host,
                &mut state,
                0,
                TurnIterationPrep {
                    quiet: true,
                    turn_start_time: Instant::now(),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{case:?} must continue the same owned Run: {error}"));

            assert!(
                matches!(control, TurnExecutionControl::ContinueLoop),
                "{case:?} must be nonterminal"
            );
        }
    }

    #[tokio::test]
    async fn guidance_observed_at_settlement_continues_the_same_run() {
        let mut state = make_state();
        state.current_run_id = Some("run-continuing".into());
        state.current_run_owner_generation = Some(7);
        state.owner_user_id = Some("user-continuing".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            // The post-provider action fence observes no guidance. The next
            // poll observes guidance before the final response is committed.
            // Applying it continues the same Run without a separate gate.
            UserIntentPoll::default(),
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "intent-continue".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "include this correction"}),
                }],
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                ..UserIntentPoll::default()
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![
            text_result("first", 5, 2, None),
            text_result("final", 5, 2, None),
        ]);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(
            matches!(outcome, Ok(AgenticLoopOutcome::Completed)),
            "continuing run failed while applying guidance: {outcome:?}"
        );
        assert_eq!(state.llm_rounds_completed, 2);

        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(
            provider.provider_authorization_calls.load(Ordering::SeqCst),
            host.turn_count(),
            "each normal provider round must have exactly one bounded authority check"
        );
    }

    #[tokio::test]
    async fn active_run_guidance_keeps_runtime_work_truth_out_of_user_speech() {
        let mut state = make_state();
        state.current_run_id = Some("run-status".into());
        state.owner_user_id = Some("user-status".into());
        state
            .hooks
            .completion_settlement
            .foreground_fanout_pagination = Some(
            crate::turn::agentic_loop::host::ForegroundFanoutPagination {
                group_id: "old-group".into(),
                target_count: 1,
                pending_slots: std::collections::BTreeMap::from([(0, 1024)]),
                had_recoverable_issues: false,
            },
        );
        state.hooks.completion_settlement.text_only = true;
        install_committed_work_synthesis_wire_surface(&mut state);
        let snapshot = "<background_tasks count=\"1\"><task id=\"review-group\" kind=\"agent_fanout\" status=\"running\" active=\"2\" completed=\"1\" /></background_tasks>";
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 1,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-status".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 1,
                input: serde_json::json!({
                    "content": "现在什么情况？",
                    "astra_runtime_context": {
                        "schema": "active_work_snapshot.v1",
                        "authority": "run_control_provider",
                        "background_work_snapshot": snapshot,
                        "work_unit_observations": [{
                            "id": "review-group",
                            "kind": "agent_fanout",
                            "status": "running",
                            "revision": 7,
                            "mode": "current",
                            "wake_policy": "on_attention_or_terminal"
                        }],
                    }
                }),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider);
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.message, "现在什么情况？");
        assert!(
            state
                .hooks
                .completion_settlement
                .foreground_fanout_pagination
                .is_none()
        );
        assert!(
            !state.hooks.completion_settlement.text_only
                && !state
                    .hooks
                    .completion_settlement
                    .preserve_final_synthesis_wire_surface,
            "new semantic guidance must revoke the previous turn's synthesis authority"
        );
        assert_eq!(
            state.user_intents.applied_user_intents()[0].content,
            "现在什么情况？",
            "runtime projection must not pollute durable user speech"
        );
        let work_context = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::ActiveWorkSnapshot)
            .expect("active guidance must carry its runtime work snapshot");
        assert_eq!(
            work_context.payload["snapshots"][0]["background_work_snapshot"],
            snapshot
        );
        assert_eq!(
            work_context.kind.delivery_class(),
            astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext
        );
        assert_eq!(
            state.stall.work_unit_observations.active_work_units()[0].id,
            "review-group",
            "the same typed snapshot must gate final-answer settlement"
        );

        let unchanged = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Running,
            7,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal);
        assert_eq!(
            state.observe_work_unit(&unchanged),
            astra_core::work_unit::WorkUnitObservationOutcome::Unchanged { consecutive: 1 }
        );
        let unchanged_context = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::ActiveWorkSnapshot)
            .unwrap();
        assert_eq!(
            unchanged_context.payload["snapshots"][0]["background_work_snapshot"], snapshot,
            "an identical producer observation must not discard unrelated XML projections"
        );
        assert!(
            unchanged_context.payload["snapshots"][0]
                .get("projection_state")
                .is_none(),
            "an identical observation is not a superseding producer revision"
        );

        let completed = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Completed,
            8,
            astra_core::work_unit::WorkUnitObservationMode::Transition,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal);
        state.observe_work_unit(&completed);
        assert!(state.stall.work_unit_observations.is_empty());
        let refreshed = state
            .volatile_pending
            .iter()
            .find(|entry| entry.kind == VolatileKind::ActiveWorkSnapshot)
            .unwrap();
        assert_eq!(
            refreshed.payload["snapshots"][0]["work_unit_observations"][0]["status"],
            "completed"
        );
        assert!(
            refreshed.payload["snapshots"][0]
                .get("background_work_snapshot")
                .is_none(),
            "newer producer truth must retire the stale submission-time XML"
        );
    }

    #[test]
    fn delayed_nonterminal_snapshot_cannot_overwrite_newer_producer_revision() {
        let mut state = make_state();
        let registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        let newer = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::WaitingForInput,
            8,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnAttentionOrTerminal);
        registry.observe(&newer);
        state.attach_active_work_registry(registry);

        let mut delayed = serde_json::json!({
            "schema": "active_work_snapshot.v1",
            "background_work_snapshot": "stale display projection",
            "work_unit_observations": [{
                "id": "review-group",
                "kind": "agent_fanout",
                "status": "running",
                "revision": 7,
                "mode": "current",
                "wake_policy": "on_attention_or_terminal"
            }]
        });
        state.reconcile_active_work_context(&mut delayed);

        assert_eq!(
            delayed["work_unit_observations"][0]["status"],
            "waiting_for_input"
        );
        assert_eq!(delayed["work_unit_observations"][0]["revision"], 8);
        assert!(delayed.get("background_work_snapshot").is_none());
        assert_eq!(
            delayed["projection_state"],
            "superseded_by_newer_producer_observation"
        );
    }

    #[test]
    fn final_settlement_reconciles_terminal_registry_truth_before_rendering() {
        let mut state = make_state();
        let registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        let running = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Running,
            1,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap();
        registry.observe(&running);
        state.attach_active_work_registry(registry.clone());
        let completed = astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Completed,
            2,
            astra_core::work_unit::WorkUnitObservationMode::Transition,
        )
        .unwrap();
        registry.observe(&completed);
        state.final_text = "All reviewers stopped.".to_string();
        reconcile_unsettled_work_status(&mut state);

        assert_eq!(state.final_text, "All reviewers stopped.");
        assert!(
            state
                .stall
                .work_unit_observations
                .active_work_units()
                .is_empty(),
            "terminal producer truth must absorb the stale turn-local running projection"
        );
    }

    #[tokio::test]
    async fn durable_user_intent_continuation_replays_outbox_without_duplicate_guidance() {
        use crate::turn::run_control::{UserIntentAdmissionAuthority, UserIntentProvider};

        // Crash on either side of durable acknowledgement and after prompt
        // application. The actual run store owns all replay/disposition facts.
        for crash_boundary in 0..3 {
            let engine = Arc::new(crate::server::run::engine::RunEngine::new(Arc::new(
                astra_services::runs::InMemoryRunStateStore::new(),
            )));
            engine.start_run("run", "user", "session").await.unwrap();
            engine
                .append_event(
                    "user",
                    "session",
                    "run",
                    serde_json::json!({
                        "event_type": "user_intent",
                        "data": {
                            "intent_id": "stable-guidance",
                            "delivery": "guide_current_run",
                            "input": {"content": "preserve this guidance exactly once"}
                        }
                    }),
                )
                .await
                .unwrap();
            let mut state = make_state();
            state.current_run_id = Some("run".into());
            state.current_session_id = Some("session".into());
            state.owner_user_id = Some("user".into());
            state.current_run_owner_generation = Some(0);
            state.run_control = Some(engine.clone());
            let mut host = MockHost::new(vec![]);
            let poll = engine.poll_user_intents("user", "run", 0).await;
            assert_eq!(poll.inputs.len(), 1);
            let source_index = poll.inputs[0].event_index;
            state
                .user_intents
                .stage_pending_disposition_events(
                    &poll.inputs,
                    astra_turn_types::UserIntentStatus::Applied,
                )
                .unwrap();
            if crash_boundary == 0 {
                state
                    .user_intents
                    .note_apply_ack_failure(tokio::time::Instant::now());
            } else {
                let acknowledgement = engine
                    .mark_user_intents_applied(
                        "user",
                        "session",
                        "run",
                        &[source_index],
                        UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
                        None,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    acknowledgement,
                    crate::turn::run_control::UserIntentApplyAck::Applied
                );
                if crash_boundary == 2 {
                    let previous_run = state.current_run_id.clone();
                    let previous_turn = state.session_turn;
                    let previous_chain = state.canonical_turn_chain_id.clone();
                    let acknowledged = state.user_intents.acknowledge_apply_events(&[source_index]);
                    assert!(apply_acknowledged_user_intents(
                        &mut host,
                        &mut state,
                        &acknowledged
                    ));
                    assert_ne!(state.canonical_turn_chain_id, previous_chain);
                    assert_eq!(state.current_run_id, previous_run);
                    assert_eq!(state.session_turn, previous_turn);
                }
            }
            let wire = serde_json::to_value(state.user_intents.durable_continuation()).unwrap();
            assert!(wire.get("pending_apply_events").is_none());
            assert!(wire.get("next_apply_ack_at").is_none());
            for field in [
                "user_intent_cursor",
                "consecutive_apply_ack_failures",
                "applied_user_intents",
            ] {
                let mut missing = wire.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(
                    serde_json::from_value::<super::super::host::DurableUserIntentState>(missing)
                        .is_err()
                );
            }
            state.user_intents = super::super::host::UserIntentState::from_durable_continuation(
                serde_json::from_value(wire).unwrap(),
            );
            assert!(state.user_intents.pending_apply_event_indices().is_empty());
            inject_polled_user_intents(&mut host, &mut state)
                .await
                .unwrap();
            assert_eq!(state.user_intents.applied_user_intents().len(), 1);
            assert_eq!(
                state
                    .messages
                    .iter()
                    .filter(|message| {
                        message["content"] == "preserve this guidance exactly once"
                    })
                    .count(),
                1,
                "crash boundary {crash_boundary}"
            );
            assert!(state.user_intents.user_intent_cursor() >= source_index);
            assert!(state.user_intents.pending_apply_event_indices().is_empty());
        }
    }

    #[tokio::test]
    async fn user_intent_records_multiple_inputs_without_consecutive_user_messages() {
        let mut state = make_state();
        state.messages = vec![
            serde_json::json!({"role":"user", "content":"first queued input\n\nsecond queued input"}),
            serde_json::json!({"role":"assistant", "content":"older response"}),
            serde_json::json!({"role":"user", "content":"test query"}),
            serde_json::json!({"role":"assistant", "content":"prior response"}),
        ];
        crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
        assert_eq!(
            state
                .telemetry
                .turn_intent_context
                .as_ref()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .message_index,
            2,
        );
        state.current_run_id = Some("run-queued-many".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 3,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "first queued input"}),
                },
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-2".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 2,
                    input: serde_json::json!({"content": "second queued input"}),
                },
            ],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 3);
        assert_eq!(*provider.released.lock().await, vec![1, 2]);
        assert_eq!(state.message, "second queued input");
        let context = state.telemetry.turn_intent_context.as_ref().unwrap();
        assert_eq!(context.message, "first queued input\n\nsecond queued input");
        let source = context.source.as_ref().unwrap();
        assert_eq!(source.message_index, 4);
        assert_eq!(source.message_text, context.message);
        assert_eq!(
            context.prior_assistant_message.as_deref(),
            Some("prior response")
        );
        assert_eq!(
            source.feedback_response,
            Some(
                astra_turn_types::FeedbackResponseReference::from_canonical_prefix(
                    state.messages[..4].to_vec(),
                )
            ),
        );
        let intent = astra_config::user_profile::TurnIntent {
            assessment: Some(astra_turn_types::TurnAssessment::default()),
            ..Default::default()
        };
        assert!(super::super::lifecycle::record_current_user_turn_semantics(
            &mut state, &intent
        ));
        assert!(
            astra_turn_types::user_turn_semantics(&state.messages[4])
                .unwrap()
                .is_some()
        );
        assert!(
            astra_turn_types::user_turn_semantics(&state.messages[2])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state.user_intents.applied_user_intents(),
            &[
                AppliedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::Applied,
                    event_index: 1,
                    content: "first queued input".to_string(),
                },
                AppliedUserIntent {
                    intent_id: "input-2".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::Applied,
                    event_index: 2,
                    content: "second queued input".to_string(),
                },
            ]
        );
        assert_eq!(
            state
                .messages
                .last()
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_str()),
            Some("first queued input\n\nsecond queued input")
        );
        assert!(
            !state.messages.windows(2).any(|window| {
                window.iter().all(|message| {
                    message.get("role").and_then(|role| role.as_str()) == Some("user")
                })
            }),
            "user intent injection must keep prompt history provider-safe"
        );
    }

    #[tokio::test]
    async fn user_intent_does_not_reinject_after_cursor_advance() {
        let mut state = make_state();
        state.current_run_id = Some("run-repoll".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "only once"}),
                }],
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 3,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "runtime-update".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 2,
                    input: crate::turn::run_control::runtime_notification_input("child completed"),
                }],
                ..Default::default()
            },
            UserIntentPoll {
                next_cursor: 3,
                ..Default::default()
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        let frozen = state.telemetry.turn_intent_context.clone().unwrap();
        for _ in 0..2 {
            inject_polled_user_intents_before_action(&mut host, &mut state)
                .await
                .unwrap();
            let context = state.telemetry.turn_intent_context.as_ref().unwrap();
            assert_eq!(context.message, frozen.message);
            let source = context.source.as_ref().unwrap();
            let frozen_source = frozen.source.as_ref().unwrap();
            assert_eq!(source.message_index, frozen_source.message_index);
            assert_eq!(source.message_text, frozen_source.message_text);
            assert_eq!(source.feedback_response, frozen_source.feedback_response);
        }

        assert_eq!(host.user_intent_context_indices, vec![1]);
        assert_eq!(state.user_intents.user_intent_cursor(), 3);
        assert_eq!(*provider.released.lock().await, vec![1, 2]);
        assert_eq!(provider.poll_call_count().await, 3);
        assert_eq!(host.user_intent_applied_indices, vec![1]);
        assert_eq!(
            state
                .messages
                .iter()
                .filter(|m| m.get("content").and_then(|c| c.as_str()) == Some("only once"))
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn user_intent_empty_poll_is_throttled() {
        let mut state = make_state();
        state.current_run_id = Some("run-empty-throttle".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 0,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-1".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "arrived after quiet poll"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 1);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "empty poll should suppress immediate follow-up DB poll"
        );
        assert!(state.messages.is_empty());

        tokio::time::advance(USER_INTENT_EMPTY_POLL_INTERVAL - Duration::from_millis(1)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "poll should remain suppressed until the interval fully elapses"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 2);
        assert_eq!(state.message, "arrived after quiet poll");
        assert_eq!(*provider.released.lock().await, vec![1]);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn quiet_rounds_do_not_poll_but_notified_guidance_applies_before_the_next_provider() {
        let mut state = make_state();
        state.current_run_id = Some("run-watched-guidance".into());
        state.owner_user_id = Some("owner".into());
        state.current_session_id = Some("session".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll::default(),
            UserIntentPoll {
                next_cursor: 2,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "revised-goal".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "use the revised document"}),
                }],
                ..UserIntentPoll::default()
            },
        ]));
        state.run_control = Some(provider.clone());
        let (wake, receiver) = tokio::sync::watch::channel(-1);
        state.user_intents.bind_wake(Some(receiver));
        let mut host = MockHost::new(vec![]);
        runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Provider)
            .await
            .unwrap();
        for boundary in [
            RuntimeInputBoundary::Action,
            RuntimeInputBoundary::Provider,
            RuntimeInputBoundary::Provider,
        ] {
            runtime_input_boundary(&mut host, &mut state, boundary)
                .await
                .unwrap();
        }
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "quiet model/tool boundaries do no durable input reads"
        );
        wake.send(1).unwrap();
        let outcome = runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Provider)
            .await
            .unwrap();
        assert!(outcome.durable_guidance_applied);
        assert_eq!(state.message, "use the revised document");
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(provider.poll_call_count().await, 2);
        runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Settlement)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            2,
            "quiet settlement delegates late input to the atomic terminal transaction"
        );
    }

    #[tokio::test]
    async fn guidance_notification_during_a_snapshot_remains_ready() {
        let mut state = make_state();
        state.current_run_id = Some("run-read-race".into());
        state.owner_user_id = Some("owner".into());
        state.current_session_id = Some("session".into());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut provider = StubRunControlProvider::new(vec![UserIntentPoll::default()]);
        provider.poll_gate = Some((entered.clone(), release.clone()));
        let provider = Arc::new(provider);
        state.run_control = Some(provider.clone());
        let (wake, receiver) = tokio::sync::watch::channel(-1);
        state.user_intents.bind_wake(Some(receiver));
        let mut host = MockHost::new(vec![]);
        let (result, ()) = tokio::join!(
            runtime_input_boundary(&mut host, &mut state, RuntimeInputBoundary::Provider),
            async {
                entered.notified().await;
                wake.send(1).unwrap();
                release.notify_one();
            },
        );
        result.unwrap();
        assert_eq!(provider.poll_call_count().await, 1);
        assert_eq!(
            state.user_intents.input_wake_is_pending(),
            Some(true),
            "an input committed after the snapshot began must trigger another safe boundary"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn provider_boundary_bypasses_empty_poll_throttle_after_tool_work() {
        let mut state = make_state();
        state.current_run_id = Some("run-provider-boundary".into());
        state.owner_user_id = Some("user-provider-boundary".into());
        state.current_session_id = Some("session-provider-boundary".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![
            UserIntentPoll {
                next_cursor: 0,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: Vec::new(),
                issues: Vec::new(),
                error: None,
            },
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "stop-stale-round".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "wait before doing more work"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 1);

        // Model the next semantic boundary arriving immediately after a tool
        // completes, while the ordinary empty-poll debounce is still active.
        let applied = inject_polled_user_intents_before_provider(&mut host, &mut state)
            .await
            .unwrap();

        assert!(applied);
        assert_eq!(provider.poll_call_count().await, 2);
        assert_eq!(state.message, "wait before doing more work");
        assert_eq!(*provider.released.lock().await, vec![1]);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn user_intent_pause_retries_the_same_ack_without_repolling_or_advancing_custody() {
        for expires in [false, true] {
            let mut state = make_state();
            state.current_run_id = Some("run-paused-guidance".into());
            state.owner_user_id = Some("user-paused".into());
            let pause_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            state.cancellation.pause_flag = Some(pause_flag.clone());
            let mut provider = StubRunControlProvider::new(vec![UserIntentPoll {
                next_cursor: 2,
                snapshot_page_fact_count: 1,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "paused-input".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "read the revised document"}),
                }],
                ..UserIntentPoll::default()
            }]);
            provider
                .apply_outcomes
                .get_mut()
                .push_back(crate::turn::run_control::UserIntentApplyAck::Paused);
            let provider = Arc::new(provider);
            state.run_control = Some(provider.clone());
            let mut host = MockHost::new(vec![]);
            if expires {
                host = host.with_execution_time_budget_remaining(
                    astra_turn_types::ExecutionTimeRemaining {
                        work_remaining: Duration::ZERO,
                        total_remaining: Duration::from_millis(50),
                    },
                );
            }
            let mut boundary = Box::pin(inject_polled_user_intents_inner(
                &mut host, &mut state, true,
            ));
            assert!(futures_util::poll!(boundary.as_mut()).is_pending());
            assert!(pause_flag.load(Ordering::SeqCst));
            assert!(provider.released.lock().await.is_empty());
            assert_eq!(provider.poll_call_count().await, 1);
            if expires {
                let error = boundary.await.unwrap_err();
                assert_eq!(error.kind, astra_core::ErrorKind::BudgetExhausted);
                assert_eq!(provider.poll_call_count().await, 1);
                assert!(provider.released.lock().await.is_empty());
                assert!(host.user_intent_context_indices.is_empty());
                assert!(host.user_intent_applied_indices.is_empty());
                assert_eq!(state.user_intents.user_intent_cursor(), 0);
                assert!(
                    state
                        .user_intents
                        .pending_apply_ack_deadline()
                        .is_some_and(|deadline| deadline > tokio::time::Instant::now())
                );
                assert_eq!(host.turn_count(), 0);
                continue;
            }
            pause_flag.store(false, Ordering::SeqCst);
            assert!(boundary.await.unwrap());
            assert_eq!(provider.poll_call_count().await, 1);
            assert_eq!(*provider.released.lock().await, [1]);
            assert_eq!(host.user_intent_context_indices, [1]);
            assert_eq!(host.user_intent_applied_indices, [1]);
            assert_eq!(state.user_intents.user_intent_cursor(), 2);
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn user_intent_retries_release_without_reinjecting_after_ack_failure() {
        let mut state = make_state();
        state.messages = vec![serde_json::json!({"role":"user", "content":"test query"})];
        crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
        state.current_run_id = Some("run-release-retry".into());
        state.owner_user_id = Some("user-deferred".into());
        let pending_poll = || UserIntentPoll {
            next_cursor: 2,
            snapshot_has_more: false,
            snapshot_page_fact_count: 1,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                event_index: 1,
                input: serde_json::json!({"content": "inject once"}),
            }],
            issues: Vec::new(),
            error: None,
        };
        let provider = Arc::new(StubRunControlProvider::with_release_failures(
            vec![
                pending_poll(),
                pending_poll(),
                pending_poll(),
                pending_poll(),
            ],
            2,
        ));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        let context = state.telemetry.turn_intent_context.as_ref().unwrap();
        assert_eq!(context.message, "test query");
        let source = context.source.as_ref().unwrap();
        assert_eq!(source.message_index, 0);
        assert_eq!(source.message_text, "test query");
        assert!(
            host.user_intent_context_indices.is_empty(),
            "uncommitted guidance must not become model-visible"
        );
        assert!(
            host.user_intent_applied_indices.is_empty(),
            "live applied evidence must wait for durable acknowledgement"
        );
        assert_eq!(
            provider.poll_call_count().await,
            1,
            "failed durable acknowledgement must not create a tight retry loop"
        );
        tokio::time::advance(USER_INTENT_EMPTY_POLL_INTERVAL).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(provider.poll_call_count().await, 2);
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            2,
            "second acknowledgement failure must increase the retry delay"
        );
        tokio::time::advance(Duration::from_millis(999)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            provider.poll_call_count().await,
            3,
            "regular polling continues"
        );
        assert!(provider.released.lock().await.is_empty());
        tokio::time::advance(Duration::from_millis(1)).await;
        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 2);
        assert_eq!(
            provider.poll_call_count().await,
            4,
            "pending release acknowledgement retries after its backoff"
        );
        let context = state.telemetry.turn_intent_context.as_ref().unwrap();
        assert_eq!(context.message, "inject once");
        let source = context.source.as_ref().unwrap();
        assert_eq!(source.message_index, state.messages.len() - 1);
        assert_eq!(source.message_text, "inject once");
        assert_eq!(*provider.released.lock().await, vec![1]);
        assert_eq!(host.user_intent_applied_indices, vec![1]);
        assert_eq!(
            state
                .messages
                .iter()
                .filter(|m| m.get("content").and_then(|c| c.as_str()) == Some("inject once"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_run_does_not_publish_user_intent_applied() {
        let mut state = make_state();
        state.current_run_id = Some("run-terminal-race".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::with_terminal_release(vec![
            UserIntentPoll {
                next_cursor: 2,
                snapshot_has_more: false,
                snapshot_page_fact_count: 0,
                inputs: vec![crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-terminal".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 1,
                    input: serde_json::json!({"content": "too late"}),
                }],
                issues: Vec::new(),
                error: None,
            },
        ]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert!(host.user_intent_context_indices.is_empty());
        assert!(host.user_intent_applied_indices.is_empty());
        assert_eq!(host.user_intent_returned_indices, vec![1]);
        assert!(!state.messages.iter().any(|message| {
            message.get("content").and_then(serde_json::Value::as_str) == Some("too late")
        }));
        assert!(provider.released.lock().await.is_empty());
        assert!(
            !state
                .user_intents
                .should_retry_apply_ack(tokio::time::Instant::now())
        );
    }

    #[tokio::test]
    async fn committed_apply_outbox_rehydrates_model_context_without_second_ack() {
        let mut state = make_state();
        state.current_run_id = Some("run-recovered-apply".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 3,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![crate::turn::run_control::QueuedUserIntent {
                intent_id: "input-recovered".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 1,
                input: serde_json::json!({"content": "recover after durable apply"}),
            }],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(host.user_intent_context_indices, vec![1]);
        assert!(host.user_intent_applied_indices.is_empty());
        assert!(provider.released.lock().await.is_empty());
        assert_eq!(state.user_intents.applied_user_intents().len(), 1);
        assert!(state.messages.iter().any(|message| {
            message.get("content").and_then(serde_json::Value::as_str)
                == Some("recover after durable apply")
        }));
    }

    #[tokio::test]
    async fn malformed_user_intent_isolated_without_blocking_later_valid_input() {
        let mut state = make_state();
        state.current_run_id = Some("run-invalid".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 7,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: vec![
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-6".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 6,
                    input: serde_json::json!({"unexpected": true}),
                },
                crate::turn::run_control::QueuedUserIntent {
                    intent_id: "input-7".into(),
                    delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    status: astra_turn_types::UserIntentStatus::AcceptedRemote,
                    event_index: 7,
                    input: serde_json::json!({"content": "continue with the valid guidance"}),
                },
            ],
            issues: Vec::new(),
            error: None,
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .unwrap();

        assert_eq!(state.user_intents.user_intent_cursor(), 7);
        assert_eq!(*provider.released.lock().await, vec![7]);
        assert_eq!(state.message, "continue with the valid guidance");
        assert_eq!(state.user_intents.applied_user_intents().len(), 1);
        assert_eq!(
            state.user_intents.applied_user_intents()[0].intent_id,
            "input-7"
        );
        assert!(state.volatile_pending.is_empty());
    }

    #[tokio::test]
    async fn user_intent_poll_error_degrades_without_advancing_cursor() {
        let mut state = make_state();
        state.current_run_id = Some("run-missing".into());
        state.owner_user_id = Some("user-deferred".into());
        let provider = Arc::new(StubRunControlProvider::new(vec![UserIntentPoll {
            next_cursor: 4,
            snapshot_has_more: false,
            snapshot_page_fact_count: 0,
            inputs: Vec::new(),
            issues: Vec::new(),
            error: Some("run not found while polling user intent: run-missing".into()),
        }]));
        state.run_control = Some(provider.clone());
        let mut host = MockHost::new(vec![]);

        inject_polled_user_intents(&mut host, &mut state)
            .await
            .expect("poll errors are control-plane misses and should not fail the main turn");

        assert_eq!(state.user_intents.user_intent_cursor(), 0);
        assert!(state.messages.is_empty());
        assert!(state.volatile_pending.is_empty());
        assert!(provider.released.lock().await.is_empty());
    }

    #[test]
    fn user_intent_content_preserves_active_skills_hint() {
        let rendered = crate::turn::run_control::user_intent_content(&serde_json::json!({
            "content": "Use the release checklist.",
            "active_skills": ["release-manager", "deploy-auditor"],
        }))
        .expect("user intent should render");

        assert!(rendered.contains("Requested active skills: release-manager, deploy-auditor."));
        assert!(rendered.contains("Use the release checklist."));
    }

    #[tokio::test]
    async fn interaction_modes_deliver_same_policy_without_rewriting_history() {
        use astra_turn_core::context_feedback::{RuntimePolicySubject, RuntimeRecoveryEvidence};
        let mut delivered_payloads = Vec::new();
        for mode in [TurnInteractionMode::Auto, TurnInteractionMode::Prompt] {
            let mut state = make_state();
            state.stall.active_policy_feedback =
                crate::turn::runtime_policy::evaluate_tool_boundary_with_thresholds(
                    &mut state.stall.runtime_policy_evaluation,
                    RuntimePolicySubject::Run,
                    &[],
                    0,
                    astra_turn_core::evaluation::EvaluationThresholds::default(),
                    Some(RuntimeRecoveryEvidence {
                        error_pressure: 1,
                        timeout_pressure: 0,
                        cautioned_tools: vec![],
                        timeout_dominant_tools: vec![],
                        omitted_tools: 0,
                    }),
                )
                .unwrap()
                .unwrap();
            let history_before = state.messages.clone();
            let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))])
                .with_interaction_mode(mode);
            execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
                .await
                .unwrap();
            let delivered = host.executed_volatile.first().unwrap();
            let policy = delivered
                .iter()
                .filter(|injection| injection.kind == VolatileKind::PolicyAdvisory)
                .collect::<Vec<_>>();
            assert_eq!(policy.len(), 1);
            assert_eq!(policy[0].payload["recovery"]["error_pressure"], 1);
            assert_eq!(host.executed_messages.first(), Some(&history_before));
            delivered_payloads.push(policy[0].payload.clone());
        }
        assert_eq!(delivered_payloads[0], delivered_payloads[1]);
    }

    #[tokio::test]
    async fn context_pressure_reaches_provider_without_changing_execution_authority() {
        for (tokens, expected) in [
            (69_999, None),
            (70_000, Some("normal")),
            (89_999, Some("normal")),
            (90_000, Some("aggressive")),
        ] {
            let mut state = make_state();
            state.max_turn_input_tokens = 100_000;
            let base = super::super::host::introspect_estimated_input_tokens(&state);
            state.pinned_tool_schema_tokens = tokens - base;
            // A previous observation must disappear once live pressure drops.
            state.push_volatile_payload(
                VolatileKind::ContextPressure,
                serde_json::json!({"old": true}),
            );
            let history = state.messages.clone();
            let budget = (state.max_turns, state.remaining_turns);
            let restrictions = state.restricted_tools.clone();
            let mut host = MockHost::new(vec![text_result("done", 10, 5, None)]);
            execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
                .await
                .expect("provider boundary");
            let delivered = host.executed_volatile[0]
                .iter()
                .filter(|entry| entry.kind == VolatileKind::ContextPressure)
                .collect::<Vec<_>>();
            assert_eq!(delivered.len(), usize::from(expected.is_some()));
            if let Some(urgency) = expected {
                let entry = delivered[0];
                assert_eq!(entry.payload["evidence"]["urgency"], urgency);
                assert_eq!(
                    entry.payload["evidence"]["token_pressure"],
                    tokens as f64 / 100_000.0
                );
                assert_eq!(entry.payload["authority"], "advisory_evidence_only");
                let edge = super::super::host::volatile_injection_edge_profile(entry);
                let wire =
                    crate::turn::wire_assembly::runtime_volatile_preamble_messages(&edge).unwrap();
                assert!(!wire.is_empty());
                assert!(wire.iter().all(|message| message["role"] == "system"));
            }
            assert_eq!(host.executed_messages[0], history);
            assert_eq!((state.max_turns, state.remaining_turns), budget);
            assert_eq!(state.restricted_tools, restrictions);
        }
    }

    #[tokio::test]
    async fn converged_policy_reaches_provider() {
        let mut state = make_state();
        state.stall.active_policy_feedback = serde_json::from_value(serde_json::json!({
            "state": "evaluated",
            "schema_version": astra_turn_core::context_feedback::RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            "recovery": null,
            "revision": 3,
            "evaluated_at_round": 8,
            "subject": {"kind": "run"},
            "entries": [{
                "signal": "round_activity",
                "stage": "converge",
                "observed_at_round": 8,
                "evidence_count": 8,
                "recommendation": "review_task_progress"
            }]
        }))
        .expect("valid policy feedback");
        let mut host = MockHost::new(vec![text_result("done", 10, 5, Some(1))]);

        execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep(false))
            .await
            .expect("provider request");
        let payload = &host.executed_volatile[0]
            .iter()
            .find(|entry| entry.kind == VolatileKind::PolicyAdvisory)
            .expect("policy must reach provider")
            .payload;
        assert!(
            payload["entries"][0]["instruction"]
                .as_str()
                .unwrap()
                .contains("one decisive check or synthesize")
        );
    }

    #[test]
    fn cache_waste_advisory_fires_at_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for _ in 0..CACHE_WASTE_MIDLOOP_THRESHOLD {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_advisory_silent_below_threshold() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD - 1) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(!should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[test]
    fn cache_waste_advisory_is_one_shot_per_turn() {
        let mut state = make_state();
        state.message = "review local changes".into();
        state.user_intent = state.message.clone();
        for _ in 0..(CACHE_WASTE_MIDLOOP_THRESHOLD + 2) {
            state.turn_guard.record_cache_hit("git_diff");
        }
        assert!(should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
        state.stall.cache_waste_advisory_emitted = true;
        state.turn_guard.record_cache_hit("git_diff");
        assert!(!should_emit_cache_waste_advisory(
            &state,
            CACHE_WASTE_MIDLOOP_THRESHOLD
        ));
    }

    #[tokio::test]
    async fn pipeline_session_receives_feedback_on_successful_turn() {
        use astra_turn_core::pipeline_config::PipelineConfig;
        use astra_turn_core::pipeline_session::PipelineSession;

        let mut state = make_state();
        state.current_session_id = Some("session-1".to_string());
        state.current_run_id = Some("run-1".to_string());
        state.session_turn = 6;
        state.policy_model_name = Some("test-model".to_string());
        state.turn_event_buffer = Some(TurnEventBuffer::begin_turn(
            state.current_session_id.as_deref(),
            state.session_turn,
        ));
        state.pipeline_session = Some(PipelineSession::new(PipelineConfig::default()));

        let mut host = MockHost::new(vec![text_result("Hello", 1000, 200, Some(50))]);
        let prep = TurnIterationPrep {
            quiet: true,
            turn_start_time: Instant::now(),
        };

        let result = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep).await;
        assert!(result.is_ok());

        let sess = state.pipeline_session.as_ref().unwrap();
        assert_eq!(sess.turns_completed(), 1);
        assert_eq!(sess.stats.turns_executed, 1);

        let mut buffer = state
            .turn_event_buffer
            .take()
            .expect("pipeline feedback should be buffered");
        let events = buffer.drain();
        let feedback_event = events
            .iter()
            .find(|event| event.event_type == JournalEventType::PipelineFeedback)
            .expect("pipeline feedback event");
        assert_eq!(feedback_event.turn, Some(6));
        assert_eq!(
            feedback_event
                .producer_scope
                .as_ref()
                .map(|scope| scope.run_id.as_str()),
            Some("run-1")
        );
        assert_eq!(
            feedback_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("turn"))
                .and_then(|turn| turn.as_u64()),
            Some(6)
        );
    }

    #[tokio::test]
    async fn pipeline_session_none_does_not_panic() {
        let mut state = make_state();
        assert!(state.pipeline_session.is_none());

        let mut host = MockHost::new(vec![text_result("Hello", 500, 100, None)]);
        let prep = TurnIterationPrep {
            quiet: true,
            turn_start_time: Instant::now(),
        };

        let result = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn failed_turn_still_preserves_last_request_message_count() {
        let mut state = make_state();
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "retry me"}));

        let mut host = MockHost::new(vec![
            HostTurnResult {
                accum: ChatTurnSseAccum {
                    error_message: Some("rate limit exceeded".to_string()),
                    has_usage: true,
                    prompt_tokens: 12,
                    completion_tokens: 0,
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: None,

                error_kind: None,
            }
            .into(),
        ]);
        let prep = TurnIterationPrep {
            quiet: true,
            turn_start_time: Instant::now(),
        };

        let result = execute_turn_and_ingest_phase(&mut host, &mut state, 0, prep).await;
        assert!(result.is_err());
        assert_eq!(state.llm_rounds_completed, 1);
        assert_eq!(
            state.last_request_message_count,
            Some(1),
            "failed LLM attempts must still protect the exact request prefix for retry-time compaction"
        );
    }
}

// Spill policy tunables — keep ~40% of the tail, shed ~60%. Chosen to
// meaningfully relieve pressure in a single pass while preserving enough
// recent turns that the agent doesn't lose working context.
const SPILL_KEEP_NUMERATOR: usize = 2;
const SPILL_KEEP_DENOMINATOR: usize = 5;
const SPILL_MIN_KEEP: usize = 6;
const SPILL_MIN_TOTAL: usize = 10;
const SPILL_MIN_SPILL: usize = 4;

async fn spill_old_messages_to_artifact(
    host: &mut dyn AgenticLoopHost,
    state: &mut AgenticLoopState,
) -> Option<(u64, usize)> {
    use crate::turn::cloud::compaction::{prepare_prefix_summary, protected_history_spill_count};
    if state.messages.len() < SPILL_MIN_TOTAL || !host.context_history_artifacts_available(state) {
        return None;
    }
    let keep =
        (state.messages.len() * SPILL_KEEP_NUMERATOR / SPILL_KEEP_DENOMINATOR).max(SPILL_MIN_KEEP);
    let count =
        protected_history_spill_count(&state.messages, state.messages.len().saturating_sub(keep));
    if count < SPILL_MIN_SPILL {
        return None;
    }
    let session_id = state.current_session_id.as_deref()?;
    let run_id = state.current_run_id.as_deref()?;
    let transcript: Vec<_> = state.messages[..count]
        .iter()
        .map(astra_core::model_wire::responses::without_continuation)
        .collect();
    let transcript = serde_json::to_string(&transcript).ok()?;
    let history =
        astra_turn_types::ContextHistoryArtifactV1::new(session_id, run_id, count, transcript)
            .ok()?;
    let id = uuid::Uuid::new_v4().to_string();
    let handle = format!(
        "{}{id}",
        astra_turn_types::CONTEXT_HISTORY_ARTIFACT_URI_PREFIX
    );
    let summary = build_spill_summary(&state.messages[..count]);
    let reference = astra_turn_core::cloud::history_summary::history_summary_message(format!(
        "[Context compressed — {count} earlier messages]

{summary}

Exact transcript: {handle}
Use introspect(artifact=\"{handle}\") for earlier details."
    ));
    let candidate = prepare_prefix_summary(
        &state.messages,
        count,
        reference,
        state.pinned_tool_schema_tokens as usize,
        state.max_turn_input_tokens,
        crate::prompts::measured_prompt_tokens_from_manifest(
            state.last_llm_context_manifest_trace.as_ref(),
        ),
    )?;
    tokio::select! {
        biased;
        _ = direct_child_parent_cancelled(&state.cancellation) => return None,
        result = host.persist_context_history(state, &id, history) => {
            if let Err(error) = result {
                tracing::warn!(%error, "context history artifact persistence failed");
                return None;
            }
        }
    }
    // Cancellation may race a successful commit. The captured transcript remains
    // session-owned evidence, but must not authorize a live-history rewrite.
    if state
        .cancellation
        .token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
        || state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        || state
            .cancellation
            .execution_lease_lost
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    {
        return None;
    }
    if !host.context_history_artifacts_available(state) {
        return None;
    }
    let freed = candidate.tokens_freed();
    let removed = candidate.messages_removed;
    state.messages = candidate.messages;
    Some((freed, removed))
}

#[cfg(test)]
#[path = "argument_rejection_tests.rs"]
mod argument_rejection_tests;

#[cfg(test)]
#[path = "execution_evidence_tests.rs"]
mod execution_evidence_tests;
