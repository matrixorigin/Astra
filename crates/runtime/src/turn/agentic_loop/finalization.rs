use crate::{EventCreateRequestData, EventService};
use astra_pipeline::step_checkpoint;
use astra_pipeline::step_protocol::StepCheckpoint;
use astra_services::SessionArtifactStore;

use super::super::agentic::adaptive_runtime::record_loop_completion_feedback;
use super::host::{AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, run_agentic_loop_impl};
use super::lifecycle::{
    cancel_unfinished_child_agents, current_agentic_step, interruption_state_summary,
    resolve_cancellation_origin, session_turn_number,
};

/// Finalize the turn trace collector: record measured token budget, feed to
/// observability session, and persist to journal. Called from every exit path
/// in the agentic loop so `/context breakdown` always reflects the latest turn.
pub(crate) async fn finalize_turn_trace(state: &mut AgenticLoopState) {
    // ── Update L1a SessionFacts from this turn's tool call records ──
    update_session_facts_from_turn(state);

    let Some(collector) = state.telemetry.turn_trace_collector.take() else {
        return;
    };
    if let Some(ref session_id) = state.current_session_id {
        collector.set_session_id(session_id);
    }
    let session_turn = session_turn_number(state);
    collector.set_turn_id(format!("turn-{session_turn}"));
    let measured = state.last_measured_prompt_tokens.unwrap_or(0);
    let max = state.max_turn_input_tokens;
    let budget_pressure = if max > 0 {
        measured as f64 / max as f64
    } else {
        state.telemetry.first_budget_pressure
    };
    // Update peak pressure so the turn/eval journal events record the actual
    // final pressure, not the stale initial value from the first payload prep.
    if budget_pressure > state.telemetry.first_budget_pressure {
        state.telemetry.first_budget_pressure = budget_pressure;
    }
    collector.record_token_budget(astra_turn_core::context_assembly_trace::TokenBudgetTrace {
        max_tokens: max as u32,
        total_used: measured as u32,
        usage_source: state
            .last_measured_prompt_tokens
            .map(|_| astra_turn_types::ContextWindowUsageSource::ProviderReported)
            .unwrap_or(astra_turn_types::ContextWindowUsageSource::Estimated),
        budget_pressure,
        compression_triggered: state.context_compression_triggered,
        ..Default::default()
    });
    let trace = collector.finalize();
    if let Some(ref session) = state.telemetry.observability_session {
        let mut guard = astra_core::sync_poison::recover_rwlock_write(session);
        crate::observability::on_context_assembled(&mut guard, trace.clone());
    }
    if collector.has_data() {
        // Defer journal write to turn-commit path to prevent ghost assemblies.
        // An outer turn can issue multiple LLM requests. The latest request
        // is the only honest answer to "what context is active now?"; keeping
        // the first one made long tool turns look artificially small.
        state.telemetry.pending_context_assembly_trace =
            Some((session_turn, trace.to_json_value()));
    }
    persist_latest_context_trace_signal(state).await;
}

async fn persist_latest_context_trace_signal(state: &mut AgenticLoopState) {
    let (session_id, persistence, session) = match (
        state.current_session_id.as_deref(),
        state.telemetry.context_trace_persistence.clone(),
        state.telemetry.observability_session.clone(),
    ) {
        (Some(session_id), Some(persistence), Some(session)) if !session_id.is_empty() => {
            (session_id.to_string(), persistence, session)
        }
        _ => return,
    };
    let signal = {
        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        crate::observability::latest_context_trace_signal(&guard)
    };
    let Some(signal) = signal else {
        return;
    };

    persist_context_trace_to_workspace_if_present(
        session_id.clone(),
        persistence.user_id.clone(),
        persistence.artifact_store.clone(),
        signal.clone(),
    )
    .await;

    let mut metadata = match serde_json::to_value(&signal) {
        Ok(value) => value,
        Err(err) => {
            astra_core::agent_warn!(
                "context-trace",
                "Failed to serialize context trace signal for {}: {}",
                session_id,
                err
            );
            return;
        }
    };
    if let Some(metadata_obj) = metadata.as_object_mut() {
        if let Some(duration_ms) = signal.timing.as_ref().map(|timing| timing.total_ms) {
            metadata_obj.insert(
                "duration_ms".to_string(),
                serde_json::json!(duration_ms.min(i32::MAX as u64)),
            );
        }
        if let Some(tool_name) = signal
            .tool_surface
            .as_ref()
            .and_then(|surface| surface.visible_tools.first())
        {
            metadata_obj.insert("tool_name".to_string(), serde_json::json!(tool_name));
        }
    }

    let content = {
        let preview = signal.preview();
        if preview.is_empty() {
            "context trace signal".to_string()
        } else {
            preview
        }
    };
    let turn_id = if signal.turn_id.is_empty() {
        "latest".to_string()
    } else {
        signal.turn_id.clone()
    };
    if let Err((status, response)) = persistence
        .event_service
        .create_event(
            persistence.user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id: session_id.clone(),
                event_type: "context_trace_signal".to_string(),
                content,
                agent_id: Some(persistence.agent_id),
                agent_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                parent_event_id: None,
                parent_event_ids: Some(Vec::new()),
                causal_chain_id: Some(format!("{session_id}:context-trace:{turn_id}")),
                metadata: Some(metadata),
            },
        )
        .await
    {
        astra_core::agent_warn!(
            "context-trace",
            "Failed to persist context trace signal for {}: {} {}",
            session_id,
            status,
            response.0.detail
        );
    }
}

async fn persist_context_trace_to_workspace_if_present(
    session_id: String,
    user_id: String,
    artifact_store: astra_services::DatabaseSessionArtifactStore,
    signal: astra_services::session_workspace::ContextTraceSignal,
) {
    let workspace_session_id = session_id.clone();
    let result = tokio::task::spawn_blocking(
        move || -> std::io::Result<Option<astra_services::session_workspace::WorkspaceMetadata>> {
            let mut updated_workspace = None;
            let existed = astra_services::session_workspace::update_existing_workspace(
                &workspace_session_id,
                |workspace| {
                    workspace.last_context_trace = Some(signal);
                    workspace.updated_at = chrono::Utc::now().to_rfc3339();
                    updated_workspace = Some(workspace.clone());
                },
            )?;
            Ok(existed.then_some(updated_workspace).flatten())
        },
    )
    .await;

    match result {
        Ok(Ok(Some(workspace))) => {
            if let Err(err) = astra_services::session_workspace::persist_remote_workspace(
                &workspace,
                &user_id,
                &artifact_store,
            )
            .await
            {
                astra_core::agent_warn!(
                    "context-trace",
                    "Failed to persist remote workspace trace for {}: {}",
                    session_id,
                    err
                );
            }
        }
        Ok(Ok(None)) => {}
        Ok(Err(err)) => {
            astra_core::agent_warn!(
                "context-trace",
                "Failed to persist workspace trace for {}: {}",
                session_id,
                err
            );
        }
        Err(err) => {
            astra_core::agent_warn!(
                "context-trace",
                "Workspace trace persistence task failed for {}: {}",
                session_id,
                err
            );
        }
    }
}

fn persist_remote_composite_snapshot_index_blocking(
    state: &AgenticLoopState,
    session_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
    source: &str,
) {
    let Some(persistence) = state.telemetry.context_trace_persistence.clone() else {
        return;
    };
    let session_id = session_id.to_string();
    let session_id_for_log = session_id.clone();
    let index = index.clone();
    let future = async move {
        astra_services::session_restore::persist_remote_composite_snapshot_index(
            &session_id,
            &persistence.user_id,
            &index,
            &persistence.artifact_store,
        )
        .await
        .map(|_| ())
    };
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|runtime| runtime.block_on(future))
            .map_err(|error| error.to_string())
            .and_then(|result| result),
    };
    if let Err(error) = result {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to {source} for {session_id_for_log}: {error}"
        );
    }
}

fn checkpoint_blocked_tools(restricted_tools: &std::collections::HashSet<String>) -> Vec<String> {
    let mut blocked_tools: Vec<String> = restricted_tools.iter().cloned().collect();
    blocked_tools.sort();
    blocked_tools.dedup();
    blocked_tools
}

/// Best-effort heavy checkpoint write.
///
/// Several early-exit paths in the agentic loop (for example text-only
/// responses and explicit stop-hook boundaries) skip the main post-tool-policy checkpoint.
/// This helper ensures those paths still persist the accumulated messages so that
/// `/debug` turn inspection and session recovery have accurate per-iteration state.
fn same_recovery_state(left: &StepCheckpoint, right: &StepCheckpoint) -> bool {
    let (StepCheckpoint::Heavy(left), StepCheckpoint::Heavy(right)) = (left, right) else {
        return false;
    };
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::FinalizationRecoveryComparison,
        left.as_ref(),
    );
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::FinalizationRecoveryComparison,
        right.as_ref(),
    );
    let mut left = (**left).clone();
    let mut right = (**right).clone();
    // Wall-clock write time is artifact metadata, not recoverable execution
    // state. It must not manufacture a new immutable timeline version.
    left.light.created_at = 0;
    right.light.created_at = 0;
    match (serde_json::to_value(&left), serde_json::to_value(&right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn try_write_heavy_checkpoint(state: &mut AgenticLoopState) {
    let Some(sid) = state.current_session_id.as_ref() else {
        return;
    };
    let Some(user_id) = state.context_manifest_user_id.as_deref() else {
        astra_core::agent_warn!(
            "checkpoint",
            "Skipping local step checkpoint for session {sid}: missing user_id"
        );
        return;
    };
    // Serialize the interruption record (if any) for checkpoint persistence.
    let interruption_json = state.interruption.as_ref().map(|ir| ir.to_json());

    // Serialize approval overrides (if any) for session continuity.
    let approval_overrides_json = state
        .approval_overrides
        .as_ref()
        .and_then(|ao| ao.to_json());

    let checkpoint_blocked_tools = checkpoint_blocked_tools(&state.restricted_tools);
    let checkpoint_messages =
        astra_turn_core::runtime_scaffolding::sanitize_recoverable_runtime_messages(
            state.messages.clone(),
        );
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::FinalizationCheckpointClone,
        &checkpoint_messages,
    );
    let context_input_headroom_tokens = match (
        state.max_turn_input_tokens,
        state.last_measured_prompt_tokens,
    ) {
        (limit, Some(measured)) if limit > 0 => limit.saturating_sub(measured),
        // Do not turn a missing measurement into an apparently full budget.
        // Zero is the legacy checkpoint sentinel for an unavailable diagnostic.
        _ => 0,
    };
    let Some(mut heavy) = state
        .step_recorder
        .build_heavy_checkpoint_with_interruption(
            &checkpoint_messages,
            context_input_headroom_tokens,
            state.remaining_turns as u32,
            &checkpoint_blocked_tools,
            &state.recent_tools,
            interruption_json,
            approval_overrides_json,
            state.consecutive_context_window_errors,
        )
    else {
        return;
    };
    let persisted_activation = state
        .runtime_tool_executor
        .as_deref()
        .map(|executor| executor.activated_deferred_tool_names())
        .unwrap_or_else(|| state.activated_deferred_tool_names.clone());
    heavy.activated_deferred_tool_names =
        astra_turn_core::tool::deferred_activation::merged_activated_tool_names(
            &checkpoint_messages,
            persisted_activation,
        );
    // Persist compaction effectiveness state for enriched resume guidance.
    heavy.compaction_state = Some(state.compaction_effectiveness.to_json());
    // Persist context pipeline state for warm-start on resume (includes emergent context).
    if let Some(ref sess) = state.pipeline_session {
        heavy.pipeline_state = match serde_json::to_value(sess.snapshot_full_state()) {
            Ok(v) => Some(v),
            Err(e) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "pipeline_state serialize failed (NaN cache ratio / bad histogram?); \
                     resume will start cold — cache hit rate, feedback history, latches lost: {e}"
                );
                None
            }
        };
    }
    // Carry transport-neutral ownership uncertainty through a heavy
    // checkpoint.  The next process must not infer safety from a trimmed
    // local record window or from prose-only conversation state.
    heavy.workspace_observation_quarantine = state.stall.workspace_observation_quarantine.clone();
    let cp = StepCheckpoint::Heavy(Box::new(heavy));
    if state
        .stall
        .last_heavy_checkpoint
        .as_ref()
        .is_some_and(|previous| same_recovery_state(previous, &cp))
    {
        return;
    }

    // A delegated run does not own the parent session timeline. Its recorder
    // counter is run-local, so writing it into the parent checkpoint directory
    // can overwrite a root checkpoint or a sibling's state. Durable delegated
    // recovery belongs to the canonical run record/transcript; keep this copy
    // only for the live loop's local recovery.
    if !state.owns_session_composite_snapshot() {
        tracing::debug!(
            session_id = %sid,
            run_id = state.current_run_id.as_deref().unwrap_or_default(),
            recursion_depth = state.recursion_depth,
            delegation_chain = ?state.delegation_chain,
            self_agent_id = %state.self_agent_id,
            "retained delegated heavy checkpoint outside the parent session namespace"
        );
        state.stall.last_heavy_checkpoint = Some(cp);
        return;
    }

    let turn = session_turn_number(state);
    let snapshot = astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
        .label(format!("checkpoint-t{turn}"))
        .workspace_state(sid.clone())
        .build();
    let (_ckpt_num, snapshot, index) =
        match step_checkpoint::commit_composite_checkpoint(user_id, sid, &cp, snapshot) {
            Ok(committed) => committed,
            Err(error) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to atomically publish session checkpoint for {sid}: {error}"
                );
                // The cross-process transaction did not publish a partial index.
                // Leave local caches untouched so the next boundary retries.
                return;
            }
        };
    persist_remote_composite_snapshot_index_blocking(
        state,
        sid,
        &index,
        "persist remote composite snapshot index",
    );

    state.last_composite_snapshot = Some(snapshot);
    state.stall.last_heavy_checkpoint = Some(cp);
}

/// Producer-scoped cleanup that also runs when the loop future is cancelled.
struct UnattributedRecallRunBoundary {
    scope: Option<(String, String)>,
}

impl UnattributedRecallRunBoundary {
    fn new(scope: Option<(String, String)>) -> Self {
        Self { scope }
    }

    fn settle(&mut self) {
        let Some((session_id, producer_id)) = self.scope.take() else {
            return;
        };
        let dropped = astra_tools::memoria::MemoriaToolGateway::drain_recalls_for_producer(
            &session_id,
            &producer_id,
            None,
        )
        .len();
        if dropped > 0 {
            tracing::debug!(
                session_id,
                producer_id,
                dropped,
                "dropped unattributed memory recalls without changing their rank"
            );
        }
    }
}

impl Drop for UnattributedRecallRunBoundary {
    fn drop(&mut self) {
        self.settle();
    }
}

fn journal_writer_for_owner(
    user_id: Option<&str>,
    session_id: &str,
) -> std::io::Result<astra_services::session_journal::JournalWriter> {
    match user_id {
        Some(user_id) => {
            astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
        }
        None => astra_services::session_journal::JournalWriter::new(session_id),
    }
}

/// Run the multi-turn agentic loop using the provided host.
///
/// This is the runtime-portable entry point. The host handles all
/// CLI/server-specific behavior; the runtime handles cognitive decisions:
/// turn ingest, stall detection, tool round orchestration, post-tool policy.
pub async fn run_agentic_loop_with_host<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, astra_core::ClassifiedError> {
    // Owned before the first await so task cancellation and panic unwinding
    // settle the same producer queue as normal and error returns.
    let _recall_run_boundary = UnattributedRecallRunBoundary::new(host.memory_recall_scope(state));
    let result = run_agentic_loop_impl(host, state).await;
    let cancellation_exit = matches!(result, Ok(AgenticLoopOutcome::Cancelled))
        || matches!(
            result,
            Err(ref error) if error.kind == astra_core::ErrorKind::Cancelled
        );
    if cancellation_exit {
        let origin = resolve_cancellation_origin(state).await;
        let reason = match origin {
            astra_turn_core::orchestration_types::CancellationOrigin::User => {
                "parent turn cancelled by user"
            }
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime => {
                "parent execution cancelled by runtime"
            }
            astra_turn_core::orchestration_types::CancellationOrigin::Unverified => {
                crate::orchestration::CANCELLATION_ORIGIN_UNVERIFIED
            }
        };
        let _cancelled = cancel_unfinished_child_agents(host, state, reason, origin).await;
        if origin == astra_turn_core::orchestration_types::CancellationOrigin::User
            && !state.interruption.as_ref().is_some_and(|interruption| {
                interruption.kind == astra_turn_core::interruption::InterruptionKind::UserCancelled
            })
        {
            state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
                astra_turn_core::interruption::InterruptionKind::UserCancelled,
                astra_turn_core::interruption::ResumeAction::ContinueImmediately,
                interruption_state_summary(state, None),
            ));
        } else if origin != astra_turn_core::orchestration_types::CancellationOrigin::User
            && state.interruption.as_ref().is_some_and(|interruption| {
                interruption.kind == astra_turn_core::interruption::InterruptionKind::UserCancelled
            })
        {
            // The inner provider loop historically used UserCancelled as a
            // generic Cancelled breadcrumb. The canonical origin boundary
            // above owns that distinction; do not persist a false user fact.
            state.interruption = None;
        }
    }

    // Ensure SessionEnd fires even on error returns that skip finalize_and_render.
    #[cfg(feature = "harness")]
    if !state.harness.session_ended {
        state.harness.session_ended = true;
        super::super::harness_adapter::harness_at!(
            &state.harness,
            astra_harness::HookPoint::SessionEnd,
            state
        );
    }

    // Registry cleanup is handled by HarnessSlot::Drop (single ownership).

    // On error, best-effort flush turn observability events.
    if result.is_err() {
        if let Some(sid) = state.current_session_id.as_deref() {
            if let Some(buf) = state.turn_event_buffer.as_mut() {
                if !buf.is_empty() {
                    if let Ok(writer) =
                        journal_writer_for_owner(state.context_manifest_user_id.as_deref(), sid)
                    {
                        if let Err(error) = buf.flush_interrupted(&writer) {
                            tracing::warn!(
                                session_id = sid,
                                error = %error,
                                "failed to flush interrupted turn journal events after agentic loop error"
                            );
                        }
                    }
                }
            }
        }
    }

    // Emit structured interruption to journal if one was recorded.
    if let Some(ref interruption) = state.interruption {
        if let Some(ref sid) = state.current_session_id {
            // `JournalWriter::append` auto-prepends `SessionStart` under
            // the same file lock; the eager `ensure_session_start_event`
            // call previously here re-acquired flock + re-stat'd the
            // journal solely to recheck a condition the append path
            // rechecks atomically. See `prepend_session_start_if_needed`.
            // Best-effort flush of turn observability events on interruption.
            if let Some(buf) = state.turn_event_buffer.as_mut() {
                if !buf.is_empty() {
                    if let Ok(writer) =
                        journal_writer_for_owner(state.context_manifest_user_id.as_deref(), sid)
                    {
                        if let Err(error) = buf.flush_interrupted(&writer) {
                            tracing::warn!(
                                session_id = sid,
                                error = %error,
                                "failed to flush interrupted turn journal events"
                            );
                        }
                    }
                }
            }

            let evt = astra_services::session_journal::JournalEvent::interruption_recorded(
                Some(sid.as_str()),
                session_turn_number(state),
                interruption.to_json(),
            )
            .with_agentic_step(Some(current_agentic_step(state)));
            if let Ok(writer) =
                journal_writer_for_owner(state.context_manifest_user_id.as_deref(), sid)
            {
                if let Err(error) = writer.append(&evt) {
                    tracing::warn!(
                        session_id = sid,
                        error = %error,
                        "failed to append interruption record to session journal"
                    );
                }
            }
        }
    }

    record_loop_completion_feedback(state, &result);
    result
}

/// Render deferred final text if any is buffered, then write heavy checkpoint.
pub(crate) async fn finalize_and_render<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    // The answer itself belongs to the interactive critical path. Everything
    // below this boundary is durable/derived settlement and must never keep a
    // displayed answer looking live just because a database, workspace, or
    // checkpoint is slow.
    ensure_terminal_text(state);
    materialize_terminal_text_message(state);
    if !state.final_text.is_empty() && !state.final_text_streamed {
        host.render_final_text(&state.final_text);
        state.final_text_streamed = true;
    }
    if !state.final_text.is_empty() && !state.final_output_ready_notified {
        host.on_final_output_ready(state).await;
        state.final_output_ready_notified = true;
    }

    finalize_turn_trace(state).await;

    // Background session-memory extraction. Fire-and-forget; service
    // handles LLM vs. rule-based decision, event emission, UX broker,
    // and debounce. See `crate::session_memory::MemoryExtractionService`.
    maybe_run_memory_extraction(state);

    // Desired-state convergence authority is deliberately live-only. A
    // terminal turn (successful, interrupted, or cancelled) must not leave a
    // no-op writer snapshot consumable by a later run on the same long-lived
    // session executor. Normal write->read convergence consumes it earlier;
    // this is the abandoned-turn cleanup boundary.
    if let (Some(executor), Some(run_id), Some(turn_chain_id)) = (
        state.runtime_tool_executor.as_deref(),
        state.current_run_id.as_deref(),
        state.canonical_turn_chain_id.as_deref(),
    ) {
        executor.clear_desired_state_convergence_authority(run_id, turn_chain_id);
    }

    reset_per_turn_advisory_state(state);
    update_working_memory_for_turn_settlement(state);

    // ── Harness: SessionEnd (observe only, fire at most once) ──
    // Fire after terminal text/interruption normalization so snapshots expose
    // the real final state instead of a pre-finalization empty/completed shell.
    #[cfg(feature = "harness")]
    if !state.harness.session_ended {
        state.harness.session_ended = true;
        super::super::harness_adapter::harness_at!(
            &state.harness,
            astra_harness::HookPoint::SessionEnd,
            state
        );
    }
    #[cfg(not(feature = "harness"))]
    super::super::harness_adapter::harness_at!(
        &state.harness,
        astra_harness::HookPoint::SessionEnd,
        state
    );

    try_write_heavy_checkpoint(state);
}

fn materialize_terminal_text_message(state: &mut AgenticLoopState) {
    let final_text = state.final_text.trim();
    if final_text.is_empty() {
        return;
    }
    let current_turn_start = state
        .messages
        .iter()
        .rposition(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("user")
        })
        .unwrap_or(0);
    let already_materialized = state.messages[current_turn_start..]
        .iter()
        .rev()
        .find(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
        })
        .and_then(astra_turn_core::prompt_facing::extract_text_content)
        .is_some_and(|content| content.trim() == final_text);
    if !already_materialized {
        state.push_prompt_history_message(serde_json::json!({
            "role": "assistant",
            "content": state.final_text.clone(),
        }));
    }
}

fn update_working_memory_for_turn_settlement(state: &mut AgenticLoopState) {
    let interruption = state.interruption.clone();
    let Some(session) = state.pipeline_session.as_mut() else {
        return;
    };
    let memory = session.working_memory_mut();

    // Rebuild blocker pressure from current settlement state instead of
    // accumulating old outages/nudges across turns.
    memory.clear_blockers();

    let Some(interruption) = interruption.as_ref() else {
        memory.clear_next_action();
        return;
    };

    if interruption_requires_intervention(interruption) {
        memory.clear_next_action();
        memory.push_blocker(format!(
            "{}: {}",
            interruption.kind.label(),
            bounded_working_memory_line(&interruption.user_message)
        ));
        return;
    }

    if matches!(
        interruption.kind,
        astra_turn_core::interruption::InterruptionKind::UserCancelled
    ) {
        memory.clear_next_action();
        return;
    }

    if interruption.kind.is_resumable() {
        memory.set_next_action(format!(
            "If the user asks to continue, resume after {}: {}",
            interruption.kind.label(),
            bounded_working_memory_line(&interruption.user_message)
        ));
    } else {
        memory.clear_next_action();
    }
}

fn interruption_requires_intervention(
    interruption: &astra_turn_core::interruption::InterruptionRecord,
) -> bool {
    matches!(
        &interruption.resume_action,
        astra_turn_core::interruption::ResumeAction::RequiresIntervention { .. }
            | astra_turn_core::interruption::ResumeAction::StartNewSession
    ) || !interruption.kind.is_resumable()
}

fn bounded_working_memory_line(raw: &str) -> String {
    const MAX_CHARS: usize = 512;
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }
    let mut out = normalized
        .chars()
        .take(MAX_CHARS.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn settlement_interruption_summary(
    state: &AgenticLoopState,
    error_detail: Option<String>,
) -> astra_turn_core::interruption::InterruptionStateSummary {
    interruption_state_summary(state, error_detail)
}

const PARTIAL_ASSISTANT_RESPONSE_MARKER: &str =
    "\n\nPartial assistant response before interruption:\n";

fn ensure_terminal_text(state: &mut AgenticLoopState) {
    let latest_provider_text = state
        .hooks
        .completion_settlement
        .latest_provider_text
        .take();
    let deferred_candidate = state
        .hooks
        .completion_settlement
        .deferred_candidate_text
        .take();

    // An interruption is authoritative. Never let a candidate (or a stale
    // model answer) turn an interrupted turn into a success-shaped response.
    // Preserve useful mixed-response text as an explicitly labelled partial
    // section so callers can resume with evidence instead of losing the last
    // substantive model output.
    if let Some(interruption) = state.interruption.as_ref() {
        let interruption_text = interruption_terminal_message(interruption);
        // Finalization can be reached from more than one terminal path (for
        // example, a provider boundary followed by lifecycle settlement).
        // Treat the already-rendered interruption envelope as idempotent. If
        // we append it to itself on the second pass, users see duplicate
        // stop messages and the partial answer becomes misleading evidence.
        let already_rendered = state.final_text.starts_with(&interruption_text)
            && (state.final_text == interruption_text
                || state.final_text.contains(PARTIAL_ASSISTANT_RESPONSE_MARKER));
        if already_rendered {
            // Preserve whether the existing envelope has already crossed the
            // render boundary. Resetting this bit here makes a second
            // finalization pass render identical interruption text twice;
            // forcing it true would instead suppress a first render when an
            // earlier lifecycle path assembled but did not display the text.
            return;
        }

        // Prefer an explicitly deferred mixed response. Otherwise retain a
        // substantive provider text that ingest recorded before the typed
        // interruption was known. A pre-existing interruption envelope is
        // not a candidate and is handled by the idempotence guard above.
        let candidate = latest_provider_text
            .filter(|text| !text.trim().is_empty())
            .or_else(|| deferred_candidate.filter(|text| !text.trim().is_empty()))
            .or_else(|| {
                // Work-settlement contract text is runtime-owned outcome
                // copy, not a provider candidate. Repeating it under the
                // interruption envelope would make a typed failure look like
                // duplicated assistant prose.
                (!state.hooks.completion_settlement.work_settlement_only
                    && !state.final_text.trim().is_empty())
                .then(|| state.final_text.trim().to_string())
            });
        state.final_text = interruption_text;
        if let Some(candidate) = candidate {
            state.final_text.push_str(PARTIAL_ASSISTANT_RESPONSE_MARKER);
            state.final_text.push_str(candidate.trim());
        }
        state.final_text_streamed = false;
        return;
    }

    if let Some(candidate) = deferred_candidate
        && state.final_text.trim().is_empty()
    {
        state.final_text = candidate;
        // The candidate was normally streamed before settlement started. Do
        // not render it twice; only later annotations need a fresh render.
        state.final_text_streamed = true;
    }
    if !state.final_text.trim().is_empty() {
        // ── Truncation marker: finish_reason == "length" ─────────────
        // The model produced output but the API cut it off at max_tokens.
        // Append a visible marker so the user sees the text is incomplete.
        // Skip exploration tasks (they tolerate truncation naturally).
        if !state.task_profile.exploratory_task {
            let default_policy = crate::turn::runtime_policy::RuntimePolicy::default();
            let policy = state.budget_policy.as_ref().unwrap_or(&default_policy);
            if let Some(marker) = policy.truncation_marker(state.last_finish_reason.as_deref()) {
                state.final_text.push_str("\n\n");
                state.final_text.push_str(marker);
                state.final_text_streamed = false;
                tracing::info!(
                    finish_reason = "length",
                    "ensure_terminal_text: appended truncation marker"
                );
            }
        }
        return;
    }
    // When the model produced tool calls but no final text even after the
    // bounded text-only recovery call, preserve a typed interruption for
    // durability and render human copy. Internal reason codes and scheduler
    // counters remain observability data; they are not user-facing prose.
    if state.total_tool_calls > 0 {
        let recent_tools: Vec<String> = state
            .stall
            .tool_call_records
            .iter()
            .filter_map(|r| {
                if r.result_preview.as_deref() == Some("") {
                    return None;
                }
                if r.error.is_some() {
                    Some(format!("{}(failed)", r.name))
                } else {
                    Some(r.name.clone())
                }
            })
            .collect();
        let tool_summary = if recent_tools.is_empty() {
            String::new()
        } else {
            let mut seen = std::collections::HashSet::new();
            let unique: Vec<&str> = recent_tools
                .iter()
                .filter(|n| seen.insert(n.as_str()))
                .map(|s| s.as_str())
                .collect();
            format!(" Completed tools included: {}.", unique.join(", "))
        };
        let rounds_completed = state.max_turns.saturating_sub(state.remaining_turns);
        let detail = format!(
            "turn ended while working: {} tool call(s) completed, last_finish_reason={}, rounds_completed={}, remaining_turns={}, max_turns={}",
            state.total_tool_calls,
            state.last_finish_reason.as_deref().unwrap_or("unknown"),
            rounds_completed,
            state.remaining_turns,
            state.max_turns,
        );
        if state.interruption.is_none() {
            state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
                astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
                astra_turn_core::interruption::ResumeAction::ContinueImmediately,
                settlement_interruption_summary(state, Some(detail)),
            ));
        }
        if let Some(interruption) = state.interruption.as_ref() {
            state.final_text = interruption_terminal_message(interruption);
            state.final_text.push_str(&tool_summary);
            state.final_text_streamed = false;
        }
        return;
    }
    if state.interruption.is_none() {
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::EmptyCompletion,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            settlement_interruption_summary(
                state,
                Some("agentic loop completed without final text".to_string()),
            ),
        ));
    }
    if let Some(interruption) = state.interruption.as_ref() {
        state.final_text = interruption_terminal_message(interruption);
        state.final_text_streamed = false;
    }
}

fn interruption_terminal_message(
    interruption: &astra_turn_core::interruption::InterruptionRecord,
) -> String {
    let mut message = interruption.user_message.clone();
    append_interruption_detail(&mut message, interruption);
    message
}

fn append_interruption_detail(
    message: &mut String,
    interruption: &astra_turn_core::interruption::InterruptionRecord,
) {
    if let Some(detail) = interruption
        .error_detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
    {
        message.push_str("\n\nWhy stopped: ");
        message.push_str(detail);
    }
}

fn reset_per_turn_advisory_state(state: &mut AgenticLoopState) {
    state.stall.work_unit_observations.clear();
    if let Some(registry) = state.stall.active_work_registry.as_ref() {
        for observation in registry.active_work_observations() {
            state.stall.work_unit_observations.observe(&observation);
        }
    }
    state.stall.execution_escalation_advisory_emitted = false;
    state.stall.work_evidence_advisory_emitted = false;
    state.stall.parallel_batching_advisory_emitted = false;
    state.stall.repetition_advisory_emitted = false;
    state.stall.cache_waste_advisory_emitted = false;
    state.stall.active_policy_feedback = Default::default();
    state.stall.runtime_policy_evaluation = Default::default();
    state.hooks.completion_settlement = Default::default();
    // Hard tool restrictions are owned by capability/permission boundaries.
    // Behavioral advisories no longer add entries here, so finalization must
    // not broaden the surface by clearing the set.
    state.turn_guard.begin_fresh_user_turn();
    // Task #43 wrap-up state also belongs to the just-completed turn —
    // next user turn starts fresh. Without this reset, the lockout/abort
    // hybrid in `agentic_loop_tool_phase::execute_tool_phase` short-
    // circuits on the first round of the new turn (because
    // `budget_wrapup_injected` is still true from the previous turn),
    // which was exactly the stale-state bug the code-review called out.
    state.budget_wrapup_injected = false;
    state.budget_wrapup_ignored_rounds = 0;
    state.hooks.completion_settlement.wrapup_origin = None;
    // Defensive reset: last_finish_reason is rewritten before every LLM call
    // in execution_phase.rs, but resetting here prevents stale leakage if a
    // future early-exit path reads it before the next LLM invocation.
    state.last_finish_reason = None;
}

/// Build a synthetic JournalEvent from the current turn's tool_call_records
/// and feed it into SessionFacts. This keeps L1a ground truth up to date
/// every turn without requiring the full journal write path.
fn update_session_facts_from_turn(state: &mut super::host::AgenticLoopState) {
    use astra_services::session_journal::{JournalEvent, JournalEventType};

    // saturating_sub: max_turns=0 → immediate completion, turn_number is irrelevant
    let turn_number = session_turn_number(state);
    let mut event = JournalEvent::base_public(JournalEventType::Turn, None);
    event.turn = Some(turn_number);
    event.tokens_in = Some(
        state
            .total_prompt
            .saturating_sub(state.session_facts.estimated_tokens),
    );

    // Copy tool_call_records from this turn
    if !state.stall.tool_call_records.is_empty() {
        event.tool_calls = Some(state.stall.tool_call_records.clone());
    }

    // Check for errors this turn
    let had_error = state.error_recovery.consecutive_same_error > 0;
    if had_error {
        event.error = Some("turn had errors".to_string());
    }

    astra_turn_core::cloud_session_facts::update_from_journal_event(
        &mut state.session_facts,
        &event,
    );

    // Sync blocked tools from state
    state
        .session_facts
        .set_blocked_tools(state.restricted_tools.iter().cloned().collect());
    let _ = had_error;
}

/// Bridge between turn finalization and
/// [`crate::session_memory::MemoryExtractionService`]. Returns
/// immediately — the service owns the per-session debounce state,
/// decides whether to spawn, emits the gate/skip/extracted/errored
/// journal event inline, and runs the background worker.
fn maybe_run_memory_extraction(state: &mut AgenticLoopState) {
    let Some(svc) = state.memory_extraction_service.clone() else {
        return;
    };
    let Some(session_id) = state.current_session_id.clone() else {
        return;
    };
    let Some(_user_id) = state.context_manifest_user_id.clone() else {
        tracing::warn!(
            session_id = %session_id,
            "Skipping session-memory extraction: missing durable user_id"
        );
        return;
    };
    let turn_number = session_turn_number(state);

    let had_error = state.error_recovery.consecutive_same_error > 0;

    // Total context size the model actually sees — uncached prompt +
    // cache reads + cache creation. Using `total_prompt` alone here
    // was a semantic bug: on prompt-cache-heavy sessions 90% of the
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::MemoryExtractionHistoryClone,
        &state.messages,
    );
    let req = crate::session_memory::ExtractionRequest {
        inference_scope: astra_turn_types::InferenceInvocationScope::Session {
            session_id,
            turn: turn_number,
            round: state.current_round_index,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 0,
        },
        messages: state.messages.clone(),
        session_facts: state.session_facts.clone(),
        had_error,
        reanchors_current_objective: state
            .turn_intent
            .as_ref()
            .is_some_and(|intent| intent.reanchors_current_objective()),
    };

    match svc.maybe_spawn(req) {
        crate::session_memory::SpawnDecision::Spawned => {}
        crate::session_memory::SpawnDecision::Queued => {}
        crate::session_memory::SpawnDecision::Skipped => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::server::runtime_tool_executor::RuntimeToolExecutor;
    use crate::server::tool_transport::{ExecutorBinding, WorkspaceBinding};
    use crate::turn::agentic_loop::host::tests::{
        MockHost, edge_tool_result, make_edge_tool, make_state, text_result,
    };

    use super::*;

    fn attach_pipeline_session(state: &mut AgenticLoopState) {
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
    }

    fn mark_must_mutate(state: &mut AgenticLoopState) {
        state.task_profile =
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
                true,
                false,
                astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
            );
        state.turn_intent = Some(
            astra_config::user_profile::TurnIntent::default().with_workspace_mutation(
                astra_config::user_profile::WorkspaceMutationIntent::MustMutate,
            ),
        );
        let workspace = std::env::temp_dir();
        let mut executor = RuntimeToolExecutor::new(
            workspace.clone(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        executor.set_execution_bindings(
            WorkspaceBinding::server_sandbox(&workspace),
            ExecutorBinding::server_local(),
        );
        state.runtime_tool_executor = Some(std::sync::Arc::new(executor));
    }

    #[test]
    fn terminal_behavior_evaluation_does_not_rewrite_real_answer() {
        let mut state = make_state();
        state.message = "fix the broken build".to_string();
        state.user_intent = state.message.clone();
        state.final_text = "Done.".to_string();
        state.final_text_streamed = true;
        state.llm_rounds_completed = 40;
        state.total_prompt = 94_900;
        state
            .stall
            .tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                name: "read_file".to_string(),
                ok: false,
                error: Some("PATH_RESOLUTION_FAILED: requested path does not exist".to_string()),
                file_path: Some("obsolete/workspace/src/lib.rs".to_string()),
                ..Default::default()
            });

        ensure_terminal_text(&mut state);

        assert_eq!(state.final_text, "Done.");
        assert!(state.final_text_streamed);
    }

    #[test]
    fn terminal_text_is_materialized_once_for_canonical_history() {
        let mut state = make_state();
        state.messages = vec![serde_json::json!({
            "role": "user",
            "content": "perform the typed operation"
        })];
        state.final_text = "Operation completed.".to_string();

        materialize_terminal_text_message(&mut state);
        materialize_terminal_text_message(&mut state);

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[1]["role"], "assistant");
        assert_eq!(state.messages[1]["content"], "Operation completed.");
    }

    #[test]
    fn existing_model_answer_is_not_duplicated_during_finalization() {
        let mut state = make_state();
        state.messages = vec![
            serde_json::json!({"role": "user", "content": "answer"}),
            serde_json::json!({"role": "assistant", "content": "Done."}),
        ];
        state.final_text = "Done.".to_string();

        materialize_terminal_text_message(&mut state);

        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn earlier_equal_assistant_text_does_not_hide_the_actual_terminal_answer() {
        let mut state = make_state();
        state.messages = vec![
            serde_json::json!({"role": "user", "content": "compare both stages"}),
            serde_json::json!({"role": "assistant", "content": "Done."}),
            serde_json::json!({"role": "assistant", "content": "Intermediate update."}),
        ];
        state.final_text = "Done.".to_string();

        materialize_terminal_text_message(&mut state);

        assert_eq!(state.messages.len(), 4);
        assert_eq!(state.messages.last().unwrap()["content"], "Done.");
    }

    #[test]
    fn terminal_text_leaves_healthy_text_completion_unchanged() {
        let mut state = make_state();
        state.message = "explain the code style".to_string();
        state.user_intent = state.message.clone();
        state.final_text = "The code style is consistent.".to_string();
        state.final_text_streamed = true;

        ensure_terminal_text(&mut state);

        assert_eq!(state.final_text, "The code style is consistent.");
        assert!(state.final_text_streamed);
    }

    #[test]
    fn interruption_keeps_typed_reason_and_partial_candidate_without_success_shape() {
        let mut state = make_state();
        state.final_text = "stale completion-shaped summary".to_string();
        state.hooks.completion_settlement.deferred_candidate_text =
            Some("The build passed its final check.".to_string());
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::TokenBudgetExceeded,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 8,
                turns_completed: 12,
                remaining_turns: 0,
                error_detail: Some("prompt budget remained above the configured rail".into()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));

        ensure_terminal_text(&mut state);

        assert!(state.final_text.contains("token budget"));
        assert!(state.final_text.contains("Why stopped: prompt budget"));
        assert!(
            state
                .final_text
                .contains("Partial assistant response before interruption:")
        );
        assert!(
            state
                .final_text
                .contains("The build passed its final check.")
        );
        assert!(
            !state
                .final_text
                .starts_with("stale completion-shaped summary")
        );
        assert!(!state.final_text_streamed);
    }

    #[test]
    fn interruption_prefers_latest_provider_text_over_older_mixed_candidate() {
        let mut state = make_state();
        state.hooks.completion_settlement.deferred_candidate_text =
            Some("older mixed response".to_string());
        state.hooks.completion_settlement.latest_provider_text =
            Some("latest truthful handoff".to_string());
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::ExecutionIncomplete,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 2,
                turns_completed: 3,
                remaining_turns: 0,
                error_detail: Some("typed completion action was not satisfied".into()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));

        ensure_terminal_text(&mut state);

        assert!(state.final_text.contains("latest truthful handoff"));
        assert!(!state.final_text.contains("older mixed response"));
    }

    #[test]
    fn recovery_state_comparison_ignores_timestamp_but_detects_structured_history_change() {
        let mut state = make_state();
        state.step_recorder.begin_turn(1);
        let messages = vec![
            serde_json::json!({"role": "user", "content": "inspect the checkpoint"}),
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"src/lib.rs\"}"}
                }]
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "call-1", "content": "contents"}),
        ];
        let left = state
            .step_recorder
            .build_heavy_checkpoint(&messages, 10_000, 4, &[], &[])
            .expect("active step produces heavy checkpoint");
        let mut right = left.clone();
        right.light.created_at = right.light.created_at.saturating_add(1);
        let left = StepCheckpoint::Heavy(Box::new(left));
        let mut right = StepCheckpoint::Heavy(Box::new(right));

        assert!(same_recovery_state(&left, &right));
        let StepCheckpoint::Heavy(right_heavy) = &mut right else {
            unreachable!("test constructed a heavy checkpoint");
        };
        right_heavy
            .messages
            .push(serde_json::json!({"role": "assistant", "content": "changed"}));
        assert!(!same_recovery_state(&left, &right));
    }

    struct SessionDirGuard(std::path::PathBuf);

    impl SessionDirGuard {
        fn new(user_id: &str, session_id: &str) -> Self {
            let path = astra_pipeline::step_checkpoint::owner_session_dir_for(user_id, session_id)
                .expect("owner/session must resolve owner-bound test session directory");
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for SessionDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn finalize_and_render_surfaces_interruption_when_final_text_is_empty() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text.clear();
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 2,
                turns_completed: 4,
                remaining_turns: 0,
                error_detail: None,
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));

        finalize_and_render(&mut host, &mut state).await;

        assert!(
            state.final_text.contains("turn budget"),
            "interrupted tool-only turns must not persist an empty or success-shaped final answer"
        );
        assert!(!state.final_text.contains("budget_exhausted"));
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);
    }

    #[tokio::test]
    async fn run_loop_writes_session_start_before_interruption_on_fresh_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());

        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let sid = "11111111-2222-3333-4444-555555555555";
        state.current_session_id = Some(sid.to_string());
        state.context_manifest_model_name = Some("gpt-5".to_string());
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: false,
                tool_calls_completed: 2,
                turns_completed: 3,
                remaining_turns: 0,
                error_detail: Some("forced for test".to_string()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));

        let result = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(
            result.is_err(),
            "mock loop should error once turn results are exhausted"
        );

        let events = astra_services::session_journal::read_journal(sid).unwrap();
        let session_start_index = events.iter().position(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::SessionStart
        });
        let interruption_index = events.iter().position(|event| {
            event.event_type
                == astra_services::session_journal::JournalEventType::InterruptionRecorded
        });
        assert!(
            session_start_index.is_some(),
            "session start must be recorded on a fresh journal"
        );
        let session_start_index = session_start_index.expect("session start event");
        let interruption_index = interruption_index.expect("interruption event");
        assert!(
            session_start_index < interruption_index,
            "session start must be recorded before interruption even when trace spans precede it"
        );
    }

    #[tokio::test]
    async fn finalize_and_render_converts_blank_completion_into_empty_completion() {
        let mut host = MockHost::new(Vec::new()).with_valid_tools(&["agent", "bash", "read_file"]);
        let mut state = make_state();
        state.final_text = "   ".into();
        state.total_tool_calls = 3;
        state.remaining_turns = 2;

        finalize_and_render(&mut host, &mut state).await;

        let interruption = state
            .interruption
            .as_ref()
            .expect("blank completion should record an interruption");
        assert_eq!(
            interruption.kind,
            astra_turn_core::interruption::InterruptionKind::EmptyCompletion
        );
        assert_eq!(
            interruption.resume_restricted_tools,
            Vec::<String>::new(),
            "empty completion should preserve the user's full tool surface; settlement is guidance/state, not a tool denylist"
        );
        assert!(state.final_text.contains("final answer"));
        assert!(state.final_text.contains("Continue this session to resume"));
        assert!(!state.final_text.contains("empty_completion"));
        assert!(!state.final_text.contains("[turn_interrupted]"));
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);
    }

    #[tokio::test]
    async fn finalize_and_render_clears_stale_resume_memory_on_clean_completion() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        attach_pipeline_session(&mut state);
        state.final_text = "Done.".into();
        {
            let memory = state
                .pipeline_session
                .as_mut()
                .expect("pipeline session")
                .working_memory_mut();
            memory.push_decision("keep durable architecture decision");
            memory.push_blocker("stale network outage");
            memory.set_next_action("retry stale nudge");
        }

        finalize_and_render(&mut host, &mut state).await;

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(rendered.contains("keep durable architecture decision"));
        assert!(!rendered.contains("stale network outage"));
        assert!(!rendered.contains("retry stale nudge"));
        assert!(!rendered.contains("Next action:"));
    }

    #[tokio::test]
    async fn finalize_and_render_records_intervention_as_blocker_not_resume_action() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        attach_pipeline_session(&mut state);
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::AuthFailure,
            astra_turn_core::interruption::ResumeAction::RequiresIntervention {
                description: "refresh credentials".to_string(),
            },
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 1,
                turns_completed: 1,
                remaining_turns: 3,
                error_detail: Some("credential refresh required".to_string()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));

        finalize_and_render(&mut host, &mut state).await;

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(
            rendered.contains("Blockers:"),
            "external intervention must be prompt-visible as a blocker: {rendered}"
        );
        assert!(rendered.contains("auth_failure"));
        assert!(!rendered.contains("Next action:"));
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn heavy_checkpoint_blocked_tools_do_not_include_soft_health_avoidance_health() {
        let user_id = "test-user";
        let session_id = format!("wm-checkpoint-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.step_recorder.begin_turn(0);
        state.restricted_tools.insert("write_file".to_string());
        for _ in 0..3 {
            state.turn_guard.health.record_failure("flaky_soft_tool");
        }
        assert!(
            state
                .turn_guard
                .health
                .is_avoidance_advised("flaky_soft_tool")
        );

        try_write_heavy_checkpoint(&mut state);

        let heavy =
            astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, &session_id)
                .expect("read checkpoint")
                .expect("heavy checkpoint");
        assert_eq!(heavy.blocked_tools, vec!["write_file".to_string()]);
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn heavy_checkpoint_strips_resume_hint_and_internal_auto_route_roundtrip() {
        let user_id = "test-user";
        let session_id = format!("runtime-clean-checkpoint-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.session_turn = 10;
        state.step_recorder.begin_turn(0);
        state.messages = vec![
            serde_json::json!({"role": "user", "content": "我说过的所有话"}),
            astra_turn_types::runtime_owned_message(
                "user",
                "arbitrary resume context",
                astra_turn_types::RuntimeMessageDelivery::RequiredContext,
            ),
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "skill-auto-route-analyze-session",
                    "type": "function",
                    "function": {"name": crate::turn::skill_tool::SKILL_TOOL_NAME, "arguments": "{}"},
                }],
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "skill-auto-route-analyze-session", "content": "<skill-loaded name=\"analyze-session\"/>"}),
            serde_json::json!({"role": "assistant", "content": "你问过我所有话。"}),
        ];

        try_write_heavy_checkpoint(&mut state);

        let heavy =
            astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, &session_id)
                .expect("read checkpoint")
                .expect("heavy checkpoint");
        assert_eq!(
            heavy.messages,
            vec![
                serde_json::json!({"role": "user", "content": "我说过的所有话"}),
                serde_json::json!({"role": "assistant", "content": "你问过我所有话。"}),
            ]
        );
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn heavy_checkpoint_preserves_ordinary_tool_roundtrip_for_recovery() {
        let user_id = "test-user";
        let session_id = format!("tool-roundtrip-checkpoint-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.step_recorder.begin_turn(0);
        state.messages = vec![
            serde_json::json!({"role": "user", "content": "run tests"}),
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"},
                }],
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
        ];

        try_write_heavy_checkpoint(&mut state);

        let heavy =
            astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, &session_id)
                .expect("read checkpoint")
                .expect("heavy checkpoint");
        assert_eq!(heavy.messages, state.messages);
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn heavy_checkpoint_carries_activation_after_tool_evidence_is_compacted() {
        let user_id = "test-user";
        let session_id = format!("activation-checkpoint-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.step_recorder.begin_turn(0);
        state.activated_deferred_tool_names = vec!["github".to_string()];
        state.messages = vec![
            serde_json::json!({"role": "system", "content": "compacted", "_compact_boundary": true}),
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];

        try_write_heavy_checkpoint(&mut state);

        let heavy =
            astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, &session_id)
                .expect("read checkpoint")
                .expect("heavy checkpoint");
        assert_eq!(
            heavy.activated_deferred_tool_names,
            vec!["github"],
            "compaction may remove tool-search messages but must not erase schema materialization state"
        );
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn root_heavy_checkpoints_are_immutable_session_timeline_versions() {
        let user_id = "test-user";
        let session_id = format!("root-composite-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.session_turn = 7;
        state.step_recorder.begin_turn(0);

        try_write_heavy_checkpoint(&mut state);

        let first_snapshot = state
            .last_composite_snapshot
            .clone()
            .expect("first composite snapshot");
        let first_ref = first_snapshot
            .session_state()
            .expect("first session state ref")
            .to_string();
        let checkpoint_dir =
            astra_pipeline::step_checkpoint::owner_session_dir_for(user_id, &session_id)
                .expect("owner session dir")
                .join("step_checkpoints");
        let first_bytes = std::fs::read(checkpoint_dir.join(&first_ref)).expect("first checkpoint");
        state.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "newer state"
        }));
        try_write_heavy_checkpoint(&mut state);

        let index =
            astra_pipeline::step_checkpoint::read_composite_snapshot_index(user_id, &session_id)
                .expect("read composite index");
        assert_eq!(index.snapshots.len(), 2);
        assert_eq!(index.snapshots[0].session_id, session_id);
        assert_eq!(index.snapshots[0].turn, 7);
        assert_eq!(index.snapshots[0].snapshot_id, first_snapshot.snapshot_id);
        assert_eq!(index.snapshots[0].version, first_snapshot.version);
        assert_ne!(
            index.snapshots[0].session_state(),
            index.snapshots[1].session_state(),
            "each timeline version must own an immutable checkpoint file"
        );
        assert_eq!(
            std::fs::read(checkpoint_dir.join(first_ref)).expect("preserved first checkpoint"),
            first_bytes,
            "writing a newer turn state must not mutate the prior snapshot"
        );
        assert!(
            state.last_composite_snapshot.is_some(),
            "root loop must expose the current session composite snapshot"
        );
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn unchanged_recovery_state_does_not_create_duplicate_heavy_versions() {
        let user_id = "test-user";
        let session_id = format!("root-composite-dedup-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.session_turn = 7;
        state.step_recorder.begin_turn(0);

        try_write_heavy_checkpoint(&mut state);
        let first_snapshot = state
            .last_composite_snapshot
            .clone()
            .expect("first composite snapshot");
        try_write_heavy_checkpoint(&mut state);

        let index =
            astra_pipeline::step_checkpoint::read_composite_snapshot_index(user_id, &session_id)
                .expect("read composite index");
        assert_eq!(
            index.snapshots.len(),
            1,
            "created_at alone is not recovery state and must not create a second durable version"
        );
        assert_eq!(
            state
                .last_composite_snapshot
                .as_ref()
                .map(|snapshot| snapshot.snapshot_id.as_str()),
            Some(first_snapshot.snapshot_id.as_str())
        );
        assert_eq!(
            astra_pipeline::step_checkpoint::list_checkpoints(user_id, &session_id)
                .unwrap()
                .into_iter()
                .filter(|(_, tier)| {
                    matches!(tier, astra_pipeline::step_protocol::CheckpointTier::Heavy)
                })
                .count(),
            1
        );
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn delegated_heavy_checkpoint_does_not_promote_session_composite_snapshot() {
        let user_id = "test-user";
        let session_id = format!("delegated-composite-{}", uuid::Uuid::new_v4());
        let sessions_dir = tempfile::tempdir().expect("temp sessions dir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let _guard = SessionDirGuard::new(user_id, &session_id);
        let mut state = make_state();
        state.context_manifest_user_id = Some(user_id.to_string());
        state.current_session_id = Some(session_id.clone());
        state.current_run_id = Some("child-run".to_string());
        state.session_turn = 7;
        state.recursion_depth = 1;
        state.delegation_chain = vec!["orchestrator".to_string()];
        state.self_agent_id = "headline-agent".to_string();
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(24_000);
        state.step_recorder.begin_turn(0);

        try_write_heavy_checkpoint(&mut state);

        assert!(
            astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, &session_id)
                .expect("read checkpoint")
                .is_none(),
            "delegated state must not be written into the parent session namespace"
        );

        let index =
            astra_pipeline::step_checkpoint::read_composite_snapshot_index(user_id, &session_id)
                .expect("read composite index");
        assert!(
            index.snapshots.is_empty(),
            "delegated checkpoints must not become the parent session timeline"
        );
        assert!(
            state.last_composite_snapshot.is_none(),
            "delegated loop must not claim parent session current snapshot"
        );
        assert!(
            state.stall.last_heavy_checkpoint.is_some(),
            "delegated loop still keeps its own heavy checkpoint for local recovery"
        );
        let StepCheckpoint::Heavy(heavy) = state
            .stall
            .last_heavy_checkpoint
            .as_ref()
            .expect("delegated local checkpoint")
        else {
            panic!("delegated checkpoint must be heavy");
        };
        assert_eq!(heavy.budget_remaining_tokens, 76_000);
        assert_eq!(heavy.budget_remaining_rounds, state.remaining_turns as u32);
    }

    #[tokio::test]
    async fn finalize_and_render_surfaces_budget_interruption_detail() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text.clear();
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 5,
                turns_completed: 7,
                remaining_turns: 0,
                error_detail: Some("Infrastructure round limit reached: 240/240 rounds".into()),
                stall_signal: Some("single_tool_streak=9".to_string()),
                resume_restricted_tools: vec![],
            },
        ));
        let expected = interruption_terminal_message(
            state.interruption.as_ref().expect("budget interruption"),
        );

        finalize_and_render(&mut host, &mut state).await;

        assert_eq!(state.final_text, expected);
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);
    }

    #[tokio::test]
    async fn single_text_turn_completes() {
        let mut host = MockHost::new(vec![text_result("Hello, world!", 10, 5, Some(42))]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Hello, world!");
        assert_eq!(state.total_prompt, 10);
        assert_eq!(state.total_completion, 5);
        assert!(state.has_any_usage);
        assert_eq!(host.rendered_final_text.len(), 1);
        assert_eq!(host.rendered_final_text[0], "Hello, world!");
    }

    #[tokio::test]
    async fn mutating_profile_marks_repeated_no_change_explanation_incomplete() {
        let answer = "No workspace mutation was needed based on the evidence.";
        let mut host = MockHost::new(vec![
            text_result(answer, 10, 5, Some(42)),
            text_result(answer, 10, 5, Some(42)),
        ]);
        let mut state = make_state();
        state.message = "fix the bug".to_string();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 2);
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(astra_turn_core::interruption::InterruptionKind::ExecutionIncomplete)
        );
        assert!(
            state
                .final_text
                .contains("Why stopped: typed completion action was not satisfied"),
            "unexpected incomplete terminal: {}",
            state.final_text
        );
        assert!(state.final_text.contains(answer));
        assert_eq!(state.messages.len(), 2);
        assert_eq!(
            state.messages[0],
            serde_json::json!({
                "role": "assistant",
                "content": "No workspace mutation was needed based on the evidence."
            }),
            "the provider response must remain intact as historical evidence"
        );
        assert_eq!(
            state.messages[1],
            serde_json::json!({
                "role": "assistant",
                "content": state.final_text,
            }),
            "the typed interruption envelope must be retained after the provider response"
        );
    }

    #[tokio::test]
    async fn render_final_text_called_once_at_completion() {
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("grep", "results...")], 20, 10, Some(50)),
            text_result("Final answer", 15, 8, Some(30)),
        ])
        .with_valid_tools(&["grep"]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Final answer");
        assert_eq!(host.rendered_final_text.len(), 1);
        assert_eq!(host.rendered_final_text[0], "Final answer");
        assert_eq!(host.final_output_ready, vec!["Final answer"]);
    }

    #[tokio::test]
    async fn render_final_text_not_duplicated_across_tool_then_text() {
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("grep", "results...")], 20, 10, Some(50)),
            edge_tool_result(
                vec![make_edge_tool("grep", "more results")],
                20,
                10,
                Some(50),
            ),
            text_result("Done!", 15, 8, Some(30)),
        ])
        .with_valid_tools(&["grep"]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Done!");
        assert_eq!(host.rendered_final_text.len(), 1);
        assert_eq!(host.rendered_final_text[0], "Done!");
    }

    #[tokio::test]
    async fn finalize_turn_trace_feeds_observability_session() {
        let mut state = make_state();
        let hub = crate::observability::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_session = Some(session.clone());
        state.remaining_turns = 9; // first outer turn after prepare_turn_iteration()
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(25_000);

        let collector = crate::turn::turn_trace_collector::TurnTraceCollector::new(
            "turn-0".to_string(),
            "s1".to_string(),
        );
        collector.record_token_budget_estimate(14_000, 5_000, 0, 3_000, 200, 22_200, 100_000, 0.22);
        state.telemetry.turn_trace_collector = Some(collector);

        finalize_turn_trace(&mut state).await;

        assert!(state.telemetry.turn_trace_collector.is_none());
        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 1);
        let trace = &guard.context_traces[0];
        assert_eq!(trace.turn_id, "turn-1");
        assert_eq!(trace.token_budget.system_prompt_tokens, 14_000);
        assert_eq!(trace.token_budget.history_tokens, 5_000);
        assert_eq!(trace.token_budget.total_used, 25_000);
        assert_eq!(trace.token_budget.max_tokens, 100_000);
        assert!((trace.token_budget.budget_pressure - 0.25).abs() < 0.01);
    }

    #[tokio::test]
    async fn finalize_turn_trace_noop_when_no_collector() {
        let mut state = make_state();
        assert!(state.telemetry.turn_trace_collector.is_none());
        finalize_turn_trace(&mut state).await;
    }

    #[tokio::test]
    async fn finalize_turn_trace_updates_on_consecutive_turns() {
        let mut state = make_state();
        let hub = crate::observability::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_session = Some(session.clone());
        state.max_turn_input_tokens = 100_000;

        state.remaining_turns = 9; // outer turn 1
        session.write().unwrap().turn_number = 1;
        state.last_measured_prompt_tokens = Some(20_000);
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-0".to_string(),
                "s1".to_string(),
            ));
        finalize_turn_trace(&mut state).await;

        state.remaining_turns = 8; // outer turn 2
        session.write().unwrap().turn_number = 2;
        state.last_measured_prompt_tokens = Some(30_000);
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-1".to_string(),
                "s1".to_string(),
            ));
        finalize_turn_trace(&mut state).await;

        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 2);
        assert_eq!(guard.context_traces[0].turn_id, "turn-1");
        assert_eq!(guard.context_traces[0].token_budget.total_used, 20_000);
        assert_eq!(guard.context_traces[1].turn_id, "turn-2");
        assert_eq!(guard.context_traces[1].token_budget.total_used, 30_000);
    }

    #[tokio::test]
    async fn finalize_turn_trace_aligns_trace_turn_id_with_journal_turn() {
        let mut state = make_state();
        state.max_turns = 40;
        state.remaining_turns = 37; // outer turn 3
        let hub = crate::observability::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        session.write().unwrap().turn_number = 3;
        state.current_session_id = Some("s1".to_string());
        state.telemetry.observability_session = Some(session.clone());
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-0".to_string(),
                "s1".to_string(),
            ));
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(42_000);

        finalize_turn_trace(&mut state).await;

        let session_guard = session.read().unwrap();
        assert_eq!(session_guard.context_traces.len(), 1);
        assert_eq!(session_guard.context_traces[0].turn_id, "turn-3");
        drop(session_guard);

        // Journal write is now deferred — verify the pending trace instead.
        let (turn_num, trace_json) = state
            .telemetry
            .pending_context_assembly_trace
            .as_ref()
            .expect("pending_context_assembly_trace should be set");
        assert_eq!(*turn_num, 3);
        assert_eq!(trace_json["turn_id"], "turn-3");
    }

    #[tokio::test]
    async fn finalize_turn_trace_reports_bridge_compaction_without_budget_wrapup() {
        let mut state = make_state();
        state.max_turns = 40;
        state.remaining_turns = 39;
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(70_000);
        state.context_compression_triggered = true;
        state.budget_wrapup_injected = false;
        let hub = crate::observability::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        session.write().unwrap().turn_number = 1;
        state.current_session_id = Some("s1".to_string());
        state.telemetry.observability_session = Some(session.clone());
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-1".to_string(),
                "s1".to_string(),
            ));

        finalize_turn_trace(&mut state).await;

        let guard = session.read().unwrap();
        assert!(
            guard.context_traces[0].token_budget.compression_triggered,
            "trace must report actual context compaction independently of budget wrap-up policy"
        );
    }

    #[test]
    fn turn_finalization_preserves_compression_observability() {
        let mut state = make_state();
        state.context_compression_triggered = true;

        reset_per_turn_advisory_state(&mut state);

        assert!(
            state.context_compression_triggered,
            "turn tracing must retain whether provider-visible compaction occurred"
        );
    }

    #[tokio::test]
    async fn finalize_turn_trace_uses_outer_turn_when_internal_counter_drifts() {
        let mut state = make_state();
        state.max_turns = 40;
        state.remaining_turns = 37; // outer turn 3

        let hub = crate::observability::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        session.write().unwrap().turn_number = 31;
        state.current_session_id = Some("s1".to_string());
        state.telemetry.observability_session = Some(session);
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-0".to_string(),
                "s1".to_string(),
            ));
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(42_000);

        finalize_turn_trace(&mut state).await;

        let (turn_num, trace_json) = state
            .telemetry
            .pending_context_assembly_trace
            .as_ref()
            .expect("pending_context_assembly_trace should be set");
        assert_eq!(*turn_num, 3);
        assert_eq!(trace_json["turn_id"], "turn-3");
    }

    #[tokio::test]
    async fn finalize_turn_trace_prefers_session_turn_across_requests() {
        let mut state = make_state();
        state.max_turns = 10;
        state.remaining_turns = 9; // first internal round of a new request
        state.session_turn = 2; // second outer turn in the persisted session

        let hub = crate::observability::ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        session.write().unwrap().turn_number = 99;
        state.current_session_id = Some("s1".to_string());
        state.telemetry.observability_session = Some(session);
        state.telemetry.turn_trace_collector =
            Some(crate::turn::turn_trace_collector::TurnTraceCollector::new(
                "turn-0".to_string(),
                "s1".to_string(),
            ));
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(42_000);

        finalize_turn_trace(&mut state).await;

        let (turn_num, trace_json) = state
            .telemetry
            .pending_context_assembly_trace
            .as_ref()
            .expect("pending_context_assembly_trace should be set");
        assert_eq!(*turn_num, 2);
        assert_eq!(trace_json["turn_id"], "turn-2");
    }

    #[tokio::test]
    async fn finalize_turn_trace_keeps_latest_request_trace_within_outer_turn() {
        let mut state = make_state();
        state.max_turns = 40;
        state.remaining_turns = 39; // outer turn 1
        state.current_session_id = Some("s1".to_string());
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(12_000);

        let first = crate::turn::turn_trace_collector::TurnTraceCollector::new(
            "turn-0".to_string(),
            "s1".to_string(),
        );
        first.set_history_retained(&[]);
        first.record_token_budget_estimate(5_000, 0, 0, 2_000, 100, 7_100, 100_000, 0.071);
        state.telemetry.turn_trace_collector = Some(first);
        finalize_turn_trace(&mut state).await;

        let first_pending = state
            .telemetry
            .pending_context_assembly_trace
            .clone()
            .expect("first pending trace should be set");
        assert_eq!(first_pending.0, 1);
        assert_eq!(first_pending.1["history"]["total_turns_available"], 0);
        assert_eq!(
            first_pending.1["history"]["turns_retained"]
                .as_array()
                .expect("turns_retained should be an array")
                .len(),
            0
        );

        let second = crate::turn::turn_trace_collector::TurnTraceCollector::new(
            "turn-1".to_string(),
            "s1".to_string(),
        );
        second.set_history_retained(&[astra_turn_core::context_assembly_trace::TurnRetention {
            turn_index: 0,
            role: "assistant".to_string(),
            tokens: 123,
            has_tool_calls: true,
            content_preview: String::new(),
        }]);
        second.record_token_budget_estimate(5_000, 123, 0, 2_000, 100, 7_223, 100_000, 0.07223);
        state.telemetry.turn_trace_collector = Some(second);
        finalize_turn_trace(&mut state).await;

        let latest_pending = state
            .telemetry
            .pending_context_assembly_trace
            .as_ref()
            .expect("latest pending trace should be set");
        assert_ne!(latest_pending.1, first_pending.1);
        assert_eq!(latest_pending.0, 1);
        assert_eq!(
            latest_pending.1["history"]["turns_retained"]
                .as_array()
                .expect("turns_retained should be an array")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn session_facts_updated_from_tool_call_records() {
        let mut state = make_state();
        state.max_turns = 5;
        state.remaining_turns = 4; // turn 1
        state.session_turn = 7;

        // Simulate tool_call_records from a turn
        state.stall.tool_call_records = vec![
            astra_services::session_journal::ToolCallRecord {
                name: "read_file".to_string(),
                ok: true,
                ms: 50,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: Some("src/main.rs".to_string()),
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
            astra_services::session_journal::ToolCallRecord {
                name: "str_replace".to_string(),
                ok: true,
                ms: 30,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: Some("src/lib.rs".to_string()),
                surgically_removed: None,
                original_tool_name: None,
                ..Default::default()
            },
        ];
        state.total_prompt = 5000;

        // Before finalization, facts should be empty
        assert!(state.session_facts.active_files.is_empty());

        // Run finalization (which calls update_session_facts_from_turn)
        finalize_turn_trace(&mut state).await;

        // After finalization, facts should be populated
        assert_eq!(state.session_facts.turn, 7);
        assert_eq!(state.session_facts.active_files.len(), 2);
        assert_eq!(state.session_facts.active_files[0].path, "src/main.rs");
        assert_eq!(state.session_facts.active_files[0].last_action, "read");
        assert_eq!(state.session_facts.active_files[1].path, "src/lib.rs");
        assert_eq!(state.session_facts.active_files[1].last_action, "write");
        assert_eq!(state.session_facts.estimated_tokens, 5000);
        assert_eq!(state.session_facts.recent_tool_calls.len(), 2);
    }

    // ── finalize_and_render integrates MemoryExtractionService ─────────
    //
    // Verifies the post-wiring finalization path:
    //   * actually calls `svc.maybe_spawn` when a service is attached
    //   * writes `session-memory.md` on the rule-based fallback path
    //   * emits a `session_memory_extraction` event
    //   * is a no-op when no service is attached (test/dispatcher paths)

    /// Capturing no-op Memoria client for finalize-side integration tests.
    /// Records every `store` so assertions can verify the runner persisted
    /// L1 content without hitting a real Memoria HTTP endpoint.
    #[derive(Default)]
    struct CapturingMemoriaForFinalize {
        stored: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl crate::turn::cloud::memoria_compact::MemoriaPort for CapturingMemoriaForFinalize {
        async fn retrieve_ext(
            &self,
            _q: &str,
            _sid: Option<&str>,
            _k: usize,
            _f: bool,
        ) -> Result<Vec<crate::turn::cloud::memoria_compact::MemoriaMemory>, String> {
            Ok(Vec::new())
        }
        async fn store(
            &self,
            content: &str,
            ty: &str,
            sid: Option<&str>,
            _t: Option<&str>,
        ) -> Result<String, String> {
            self.stored.lock().unwrap().push((
                content.to_string(),
                ty.to_string(),
                sid.map(str::to_string),
            ));
            Ok("mem".to_string())
        }
        async fn purge_working(&self, _sid: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    fn attach_memory_extraction_service(
        state: &mut AgenticLoopState,
    ) -> (
        tokio::sync::mpsc::Receiver<astra_services::event_ingestion::IngestionEvent>,
        std::sync::Arc<CapturingMemoriaForFinalize>,
    ) {
        use std::sync::Arc;
        let (ingestion, rx) = astra_services::event_ingestion::IngestionSender::for_tests(256);
        let memoria = Arc::new(CapturingMemoriaForFinalize::default());
        let svc = Arc::new(
            crate::session_memory::MemoryExtractionService::new(
                Arc::new(crate::session_memory::ConstMemoryInferenceResolver(None)),
                Arc::clone(&memoria) as Arc<dyn crate::turn::cloud::memoria_compact::MemoriaPort>,
                ingestion,
                "test-user",
                Arc::new(crate::session_memory::BackgroundActivityBroker::new()),
            )
            .with_local_current_snapshot(),
        );
        state.memory_extraction_service = Some(svc);
        (rx, memoria)
    }

    #[tokio::test]
    async fn finalize_and_render_without_session_id_does_not_panic() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.current_session_id = None;
        state.error_recovery.consecutive_same_error = 1;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "no sid"}));
        let (_rx, _memoria) = attach_memory_extraction_service(&mut state);

        finalize_and_render(&mut host, &mut state).await;
        // assertion: we got here without panicking.
    }

    #[tokio::test]
    async fn finalize_and_render_does_not_classify_short_user_text() {
        let sid = format!(
            "finalize-skips-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.current_session_id = Some(sid.clone());
        state.context_manifest_user_id = Some("test-user".into());
        state.error_recovery.consecutive_same_error = 0;
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "clean turn"}));
        let (_rx, memoria) = attach_memory_extraction_service(&mut state);

        finalize_and_render(&mut host, &mut state).await;

        let pending = state
            .memory_extraction_service
            .as_ref()
            .expect("memory extraction service")
            .wait_for_pending(std::time::Duration::from_secs(2))
            .await;
        assert_eq!(pending, 0);
        assert!(
            !memoria.stored.lock().unwrap().is_empty(),
            "structured freshness, not message length, admits extraction"
        );
    }

    #[tokio::test]
    async fn finalize_and_render_without_service_is_silent_noop() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.current_session_id = Some("no-svc".to_string());
        state.total_prompt = 50_000; // would normally trigger
        assert!(state.memory_extraction_service.is_none());
        finalize_and_render(&mut host, &mut state).await;
        // Implicit: no panic, no file, no events.
    }

    #[tokio::test]
    async fn run_boundary_uses_executor_owned_scope_after_host_error() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let parent_session_id = format!("parent-session-{}", uuid::Uuid::new_v4());
        let session_id = format!("executor-session-{}", uuid::Uuid::new_v4());
        let workspace = tempfile::TempDir::new().unwrap();
        let executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        let (_, producer_id) = executor.memory_recall_scope(None);
        state.runtime_tool_executor = Some(std::sync::Arc::new(executor));
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
        astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
            &session_id,
            None,
            &producer_id,
            1,
            vec!["m1".into()],
        );
        astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
            &session_id,
            None,
            "concurrent-run",
            1,
            vec!["m2".into()],
        );
        state.current_session_id = Some(parent_session_id);
        state.current_run_id = None;

        let result = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(result.is_err(), "empty host must exercise the error exit");
        assert_eq!(
            astra_tools::memoria::MemoriaToolGateway::pending_recall_count(&session_id),
            1,
            "run boundary must use the executor session and canonical fallback producer without touching concurrent work"
        );
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
    }

    #[tokio::test]
    async fn cancelled_error_exit_cancels_unfinished_children_at_loop_boundary() {
        struct CancelledHost {
            valid_tool_names: std::collections::HashSet<String>,
            cancelled_agent_ids: Vec<String>,
            cancellation_origins: Vec<astra_turn_core::orchestration_types::CancellationOrigin>,
        }

        #[async_trait::async_trait]
        impl AgenticLoopHost for CancelledHost {
            fn emit_headless_line(
                &mut self,
                _style: astra_turn_core::headless_tool_body_preview::HeadlessStderrStyle,
                _line: String,
            ) {
            }

            fn is_quiet(&self) -> bool {
                true
            }

            fn valid_tool_names(&self) -> &std::collections::HashSet<String> {
                &self.valid_tool_names
            }

            async fn execute_turn(
                &mut self,
                _state: &mut AgenticLoopState,
            ) -> Result<crate::turn::agentic_loop::host::HostTurnResult, astra_core::ClassifiedError>
            {
                Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "user interrupted the active turn",
                ))
            }

            async fn cancel_child_agents(
                &mut self,
                agent_ids: &[String],
                _reason: &str,
                origin: astra_turn_core::orchestration_types::CancellationOrigin,
            ) -> Vec<String> {
                self.cancelled_agent_ids.extend_from_slice(agent_ids);
                self.cancellation_origins.push(origin);
                agent_ids.to_vec()
            }
        }

        let mut host = CancelledHost {
            valid_tool_names: std::collections::HashSet::new(),
            cancelled_agent_ids: Vec::new(),
            cancellation_origins: Vec::new(),
        };
        let mut state = make_state();
        state.stall.tool_call_records = vec![astra_services::session_journal::ToolCallRecord {
            name: "agent".to_string(),
            ok: true,
            ms: 0,
            args_full: Some(
                serde_json::json!({
                    "action": "spawn",
                    "description": "Review runtime"
                })
                .to_string(),
            ),
            result_full: Some(
                serde_json::json!({
                    "status": "launched",
                    "agent_id": "agent-running",
                    "description": "Review runtime"
                })
                .to_string(),
            ),
            ..Default::default()
        }];

        let result = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(matches!(
            result,
            Err(astra_core::ClassifiedError {
                kind: astra_core::ErrorKind::Cancelled,
                ..
            })
        ));
        assert_eq!(
            host.cancelled_agent_ids,
            vec!["agent-running".to_string()],
            "the shared loop exit must cancel children even when cancellation surfaces as an error"
        );
        assert_eq!(
            host.cancellation_origins,
            vec![astra_turn_core::orchestration_types::CancellationOrigin::Runtime],
            "a provider cancellation without a user marker remains runtime-origin"
        );
        assert!(
            !state.interruption.as_ref().is_some_and(|interruption| {
                interruption.kind == astra_turn_core::interruption::InterruptionKind::UserCancelled
            }),
            "runtime cancellation must not record a user interruption"
        );
    }

    #[tokio::test]
    async fn aborting_loop_future_drains_its_producer_recall_queue() {
        struct PendingHost {
            entered: Option<tokio::sync::oneshot::Sender<()>>,
            valid_tool_names: std::collections::HashSet<String>,
        }

        #[async_trait::async_trait]
        impl AgenticLoopHost for PendingHost {
            fn emit_headless_line(
                &mut self,
                _style: astra_turn_core::headless_tool_body_preview::HeadlessStderrStyle,
                _line: String,
            ) {
            }

            fn is_quiet(&self) -> bool {
                true
            }

            fn valid_tool_names(&self) -> &std::collections::HashSet<String> {
                &self.valid_tool_names
            }

            async fn execute_turn(
                &mut self,
                _state: &mut AgenticLoopState,
            ) -> Result<crate::turn::agentic_loop::host::HostTurnResult, astra_core::ClassifiedError>
            {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                std::future::pending().await
            }
        }

        let mut state = make_state();
        let session_id = format!("cancelled-run-feedback-{}", uuid::Uuid::new_v4());
        state.current_session_id = Some(session_id.clone());
        state.current_run_id = Some("cancelled-run".to_string());
        astra_tools::memoria::MemoriaToolGateway::record_recall_for_producer(
            &session_id,
            None,
            "cancelled-run",
            1,
            vec!["m1".into()],
        );

        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut host = PendingHost {
                entered: Some(entered_tx),
                valid_tool_names: std::collections::HashSet::new(),
            };
            run_agentic_loop_with_host(&mut host, &mut state).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .expect("loop must reach the cancellable host await")
            .expect("host must signal before blocking");
        task.abort();
        let join_error = task
            .await
            .expect_err("aborted loop must not complete normally");
        assert!(join_error.is_cancelled());

        assert_eq!(
            astra_tools::memoria::MemoriaToolGateway::pending_recall_count(&session_id),
            0,
            "cancelling the loop future must release its producer-owned recall queue"
        );
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(&session_id);
    }

    // I11 test removed in rebase: the branch wired a now-deleted
    // `memory_extraction_runner` module directly from
    // `finalize_and_render`. Main's refactor routes session-memory
    // extraction through a dedicated service that fires on its own
    // schedule, not synchronously from finalize. Re-add equivalent
    // coverage when the new service surfaces a synchronous hook.

    #[tokio::test]
    async fn error_triggered_l1_sets_error_state_and_builds_l1() {
        let mut state = make_state();
        state.max_turns = 5;
        state.remaining_turns = 4;
        state.current_session_id = Some("test-error-trigger".to_string());
        state
            .messages
            .push(serde_json::json!({"role": "user", "content": "fix the bug"}));
        state
            .messages
            .push(serde_json::json!({"role": "assistant", "content": "working on it"}));
        state.error_recovery.consecutive_same_error = 1;
        state.stall.tool_call_records = vec![astra_services::session_journal::ToolCallRecord {
            name: "bash".to_string(),
            ok: false,
            ms: 50,
            error: Some("compile error".to_string()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];
        state.total_prompt = 15000;

        // finalize_turn_trace will attempt L1 write to $HOME/.astra/... which may
        // fail in test (no writable session dir). That's fine — it's non-fatal.
        // We verify the facts update and L1 content generation instead.
        finalize_turn_trace(&mut state).await;

        // Error tracked in facts
        assert_eq!(state.session_facts.error_state.total_errors, 1);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // E2E: tool round guidance injection
    // Verifies that the agentic loop injects tool round warning/limit
    // messages into state.messages at the correct round thresholds.
    // Regression: CLI path was missing this entirely (found in session analysis).
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn tool_round_guidance_not_reintroduced_at_threshold() {
        // With the circuit breaker refactor, tool round directives are no
        // longer injected. The parallel-batching nudge may still fire if the
        // model produces single-tool rounds. This test verifies the loop
        // completes normally without budget pressure.
        let tool_names = [
            "read_file",
            "grep",
            "bash",
            "glob",
            "read_file",
            "grep",
            "bash",
            "glob",
            "read_file",
        ];
        let mut results = Vec::new();
        for name in &tool_names {
            results.push(edge_tool_result(
                vec![make_edge_tool(name, "tool output")],
                100,
                20,
                Some(10),
            ));
        }
        results.push(text_result("Final answer", 100, 50, Some(10)));

        let mut host =
            MockHost::new(results).with_valid_tools(&["read_file", "grep", "bash", "glob"]);
        let mut state = make_state();
        state.max_turns = 25;
        state.remaining_turns = 25;
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Tool round hard-stop directives are no longer injected (circuit breaker
        // handles stalls). Verify no "Tool Round" messages appear.
        let tool_round_guidance_found = state.messages.iter().any(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("user")
                && m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("Tool Round"))
                    .unwrap_or(false)
        });
        assert!(
            !tool_round_guidance_found,
            "tool round directives should no longer be injected (circuit breaker replaces them)",
        );
    }

    #[tokio::test]
    async fn tool_round_guidance_uses_llm_round_count_not_step_index() {
        // Regression: guidance was using turn_index (step counter, inflated by
        // progressive penalty) instead of llm_rounds_completed (actual LLM calls).
        // With progressive penalty, step 10 = only 4th LLM call, but the old code
        // would inject "round 10/6 EXCEEDED" which is nonsensical and ignored.
        //
        // This test verifies guidance fires at the 3rd LLM call regardless of
        // what the step index is.  We simulate a state where step index is already
        // high (e.g. 20) but only 2 LLM rounds have completed — guidance must NOT
        // fire yet.
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("read_file", "a")], 100, 20, Some(10)),
            edge_tool_result(vec![make_edge_tool("read_file", "b")], 100, 20, Some(10)),
            text_result("Done", 100, 50, Some(10)),
        ])
        .with_valid_tools(&["read_file"]);
        let mut state = make_state();
        // Simulate inflated step index (as if progressive penalty already fired)
        state.current_round_index = 20;
        // But llm_rounds_completed starts at 0 (only 2 LLM calls will happen)
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Only 2 LLM rounds completed — guidance threshold is 3 — must NOT fire
        let guidance_found = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.contains("Tool Round") || s.contains("Synthesize"))
                .unwrap_or(false)
        });
        assert!(
            !guidance_found,
            "guidance must NOT fire when only 2 LLM rounds completed (step index is irrelevant)"
        );
        assert_eq!(
            state.llm_rounds_completed, 3,
            "3 LLM calls should have been made"
        );
    }

    #[tokio::test]
    async fn tool_round_guidance_not_injected_before_threshold() {
        // 2 tool rounds + final text = should NOT trigger guidance (threshold=3).
        let mut host = MockHost::new(vec![
            edge_tool_result(
                vec![make_edge_tool("read_file", "content")],
                100,
                20,
                Some(10),
            ),
            edge_tool_result(vec![make_edge_tool("grep", "match")], 100, 20, Some(10)),
            text_result("Done", 100, 50, Some(10)),
        ])
        .with_valid_tools(&["read_file", "grep"]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        let guidance_found = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.contains("Tool Round") || s.contains("Synthesize"))
                .unwrap_or(false)
        });
        assert!(
            !guidance_found,
            "tool round guidance must NOT be injected before threshold"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // E2E: progressive warning penalty drains budget
    // Verifies that consecutive warnings from TurnGuard reduce remaining_turns
    // progressively (2, 4, 6, ...) so spinning loops terminate faster.
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn progressive_warning_penalty_limits_loop_duration() {
        // Simulate 20 tool rounds — without progressive penalty this would
        // run all 20. With it, the loop should terminate earlier.
        let mut results = Vec::new();
        for _ in 0..20 {
            results.push(edge_tool_result(
                vec![make_edge_tool("read_file", "same content")],
                100,
                20,
                Some(10),
            ));
        }
        // Final text in case loop completes normally
        results.push(text_result("Done", 100, 50, Some(10)));

        let mut host = MockHost::new(results).with_valid_tools(&["read_file"]);
        let mut state = make_state();
        state.max_turns = 50; // generous budget

        // Pre-seed cache hits to trigger warnings from the start
        for _ in 0..4 {
            state.turn_guard.record_cache_hit("read_file");
        }

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        // The loop should have terminated before using all 20 tool rounds.
        // Use llm_rounds_completed directly — it is the authoritative count
        // of attempted LLM calls (incremented once per execute_turn call,
        // not influenced by render/tool-message bookkeeping). Combining
        // render counts + tool-message counts is an indirect metric that
        // can pass accidentally; `llm_rounds_completed` is what the
        // progressive-penalty machinery is supposed to bound.
        let llm_rounds = state.llm_rounds_completed;

        // We don't assert exact count (depends on penalty math), but it
        // must be significantly less than 20.
        assert!(
            llm_rounds < 18,
            "progressive penalty should limit loop to fewer LLM rounds, got llm_rounds={llm_rounds}. outcome={outcome:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Regression: tool round guidance is ephemeral — only one copy lives
    // in state.messages at any time. Prior guidance must be stripped before
    // the next one is appended, otherwise every late-round call accumulates
    // a duplicate "Tool Round" user-message that wastes tokens.
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn tool_round_guidance_is_ephemeral_not_accumulated() {
        // Simulate multiple tool rounds. The blocked tool-round directive
        // must not reappear or accumulate in state.messages.
        let mut results = Vec::new();
        for _ in 0..5 {
            results.push(edge_tool_result(
                vec![make_edge_tool("read_file", "content")],
                100,
                20,
                Some(10),
            ));
        }
        results.push(text_result("Done", 100, 50, Some(10)));

        let mut host = MockHost::new(results).with_valid_tools(&["read_file"]);
        let mut state = make_state();
        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        let guidance_count = state
            .messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("user")
                    && m.get("content").and_then(|c| c.as_str()).is_some_and(|s| {
                        s.contains("## ⚡ Tool Round") || s.contains("## ⚠ Tool Round")
                    })
            })
            .count();

        assert!(
            guidance_count <= 1,
            "tool-round guidance must be ephemeral (at most 1 copy in state.messages); \
             found {guidance_count} copies"
        );
    }
}
