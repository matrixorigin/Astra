use crate::{EventCreateRequestData, EventService};
use astra_pipeline::step_checkpoint;
use astra_pipeline::step_protocol::StepCheckpoint;
use astra_services::SessionArtifactStore;

use super::super::agentic::adaptive_runtime::record_loop_completion_feedback;
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, VolatileKind, run_agentic_loop_impl,
};
use super::lifecycle::{current_agentic_step, interruption_state_summary, session_turn_number};

fn inject_hallucination_tripwire_nudge_if_fired(state: &mut AgenticLoopState) {
    use astra_turn_core::hallucination_tripwire::{
        TripwireToolObservation, TripwireVerdict, detect,
    };

    if state.final_text.is_empty() {
        return;
    }

    let observations: Vec<TripwireToolObservation<'_>> = state
        .stall
        .tool_call_records
        .iter()
        .map(|record| {
            let result_preview = record
                .result_preview
                .as_deref()
                .or(record.result_full.as_deref())
                .or(record.error.as_deref())
                .unwrap_or_else(|| {
                    if record.output_bytes == Some(0) {
                        ""
                    } else {
                        "[tool result unavailable]"
                    }
                });
            TripwireToolObservation {
                name: record.name.as_str(),
                result_preview,
            }
        })
        .collect();

    if let TripwireVerdict::Mismatch {
        nudge,
        matched_phrases,
    } = detect(state.final_text.as_str(), observations)
    {
        state.push_volatile_payload(
            VolatileKind::HallucinationTripwire,
            serde_json::json!({
                "schema": "hallucination_tripwire.v1",
                "signal": "ungrounded_tool_outcome_claim",
                "evidence": {
                    "matched_phrases": matched_phrases,
                    "tool_observation_count": state.stall.tool_call_records.len(),
                },
                "recommendation": nudge,
                "authority": "advisory_evidence_only",
            }),
        );
    }
}

/// Finalize the turn trace collector: record measured token budget, feed to
/// observability session, and persist to journal. Called from every exit path
/// in the agentic loop so `/context breakdown` always reflects the latest turn.
pub(crate) async fn finalize_turn_trace(state: &mut AgenticLoopState) {
    // Detect phantom tool-outcome claims in the assistant's completed prose and
    // queue a correction for the next LLM call before this turn's records reset.
    inject_hallucination_tripwire_nudge_if_fired(state);

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
        budget_pressure,
        compression_triggered: state.budget_wrapup_injected,
        ..Default::default()
    });
    let trace = collector.finalize();
    if let Some(ref session) = state.telemetry.observability_session {
        let mut guard = astra_core::sync_poison::recover_rwlock_write(session);
        crate::observability::on_context_assembled(&mut guard, trace.clone());
    }
    if collector.has_data() {
        // Defer journal write to turn-commit path to prevent ghost assemblies.
        // Store only the first trace for this outer turn so the journal records
        // the initial context assembly, not a later internal iteration after
        // tool-call messages have already been appended.
        if state.telemetry.pending_context_assembly_trace.is_none() {
            state.telemetry.pending_context_assembly_trace =
                Some((session_turn, trace.to_json_value()));
        }
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
            let workspace_path =
                astra_services::session_workspace::workspace_dir_for(&workspace_session_id)
                    .join("workspace.yaml");
            if !workspace_path.is_file() {
                return Ok(None);
            }
            let mut workspace =
                astra_services::session_workspace::read_workspace(&workspace_session_id)?;
            workspace.last_context_trace = Some(signal);
            workspace.updated_at = chrono::Utc::now().to_rfc3339();
            astra_services::session_workspace::write_workspace(&workspace)?;
            Ok(Some(workspace))
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

#[cfg(test)]
mod hallucination_tripwire_integration_tests {
    use super::*;
    use astra_services::session_journal::ToolCallRecord;

    #[test]
    fn queues_next_turn_nudge_when_final_text_claims_phantom_empty_result() {
        let mut state = super::super::host::tests::make_state();
        state.final_text =
            "The str_replace edit silently returned {}, so the file stayed unchanged.".to_string();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "str_replace".to_string(),
            ok: false,
            error: Some("Error: old_str not found. Aborting edit.".to_string()),
            output_bytes: Some(42),
            ..Default::default()
        });

        inject_hallucination_tripwire_nudge_if_fired(&mut state);

        assert_eq!(state.volatile_pending.len(), 1);
        assert_eq!(
            state.volatile_pending[0].kind,
            VolatileKind::HallucinationTripwire
        );
        let payload = &state.volatile_pending[0].payload;
        assert_eq!(payload["schema"], "hallucination_tripwire.v1");
        assert_eq!(payload["signal"], "ungrounded_tool_outcome_claim");
        assert_eq!(payload["evidence"]["tool_observation_count"], 1);
        assert_eq!(
            payload["evidence"]["matched_phrases"],
            serde_json::json!(["silently returned {}"])
        );
        assert_eq!(payload["authority"], "advisory_evidence_only");
    }

    #[test]
    fn stays_silent_when_tool_record_anchors_empty_result_claim() {
        let mut state = super::super::host::tests::make_state();
        state.final_text = "The helper silently returned {}.".to_string();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "helper".to_string(),
            ok: true,
            output_bytes: Some(2),
            result_preview: Some("{}".to_string()),
            ..Default::default()
        });

        inject_hallucination_tripwire_nudge_if_fired(&mut state);

        assert!(state.volatile_pending.is_empty());
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
    let ckpt_num = state.step_recorder.summary().checkpoints;

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
    let Some(mut heavy) = state
        .step_recorder
        .build_heavy_checkpoint_with_interruption(
            &checkpoint_messages,
            0,
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
    let cp = StepCheckpoint::Heavy(Box::new(heavy));
    if let Err(e) = step_checkpoint::write_step_checkpoint(user_id, sid, ckpt_num, &cp) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to write step checkpoint {ckpt_num}: {e}"
        );
        // Disk write failed: do not commit composite snapshot state, otherwise
        // resume logic would read state pointing at a non-existent checkpoint
        // file. Leave `state.last_composite_snapshot` and the stall heavy
        // checkpoint cache untouched so the next iteration retries cleanly.
        return;
    }

    if !state.owns_session_composite_snapshot() {
        tracing::debug!(
            session_id = %sid,
            run_id = state.current_run_id.as_deref().unwrap_or_default(),
            recursion_depth = state.recursion_depth,
            delegation_chain = ?state.delegation_chain,
            self_agent_id = %state.self_agent_id,
            checkpoint = ckpt_num,
            "wrote delegated heavy checkpoint without promoting it to the session composite snapshot"
        );
        state.stall.last_heavy_checkpoint = Some(cp);
        return;
    }

    let turn = session_turn_number(state);
    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
            .label(format!("checkpoint-t{turn}"))
            .session_state(format!("{:06}-heavy.json", ckpt_num))
            .workspace_state(sid.clone())
            .build();

    let mut index = match step_checkpoint::read_composite_snapshot_index(user_id, sid) {
        Ok(index) => index,
        Err(error) => {
            astra_core::agent_warn!(
                "checkpoint",
                "Failed to read snapshot index for session {sid}: {error}"
            );
            return;
        }
    };
    if let Err(e) = index.append(&mut snapshot) {
        astra_core::agent_warn!("checkpoint", "Failed to append snapshot version: {e}");
        return;
    }
    if let Err(e) = step_checkpoint::write_composite_snapshot_index(user_id, sid, &index) {
        astra_core::agent_warn!("checkpoint", "Failed to write snapshot index: {e}");
        // Index write failed: leave snapshot state untouched so a subsequent
        // checkpoint can re-attempt without referencing a half-written index.
        return;
    }
    persist_remote_composite_snapshot_index_blocking(
        state,
        sid,
        &index,
        "persist remote composite snapshot index",
    );

    state.last_composite_snapshot = Some(snapshot);
    state.stall.last_heavy_checkpoint = Some(cp);
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
    let result = run_agentic_loop_impl(host, state).await;

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
                    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
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
                    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
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
            if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
                if let Err(error) = writer.append(&evt) {
                    tracing::warn!(
                        session_id = sid,
                        error = %error,
                        "failed to append interruption record to session journal"
                    );
                }
            }
        }

        // Carry `UserCancelled` forward so the adaptive profile layer can
        // skip scenario re-detection on the next turn. Without this gate,
        // the aborted tool history leaks into ScenarioDetector and can
        // falsely trigger an `Exploration` scenario (ratcheting the tool
        // budget). Any non-UserCancelled interruption (timeout, error,
        // stream_error, …) does NOT set the flag — only an explicit user
        // cancel invalidates the tool-history-as-evidence signal.
        if matches!(
            interruption.kind,
            astra_turn_core::interruption::InterruptionKind::UserCancelled
        ) {
            if let Some(ref session) = state.telemetry.observability_session {
                if let Ok(mut guard) = session.write() {
                    guard.previous_turn_user_cancelled = true;
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
    finalize_turn_trace(state).await;
    drop_unattributed_memory_recalls_at_turn_end(state);

    // Background session-memory extraction. Fire-and-forget; service
    // handles LLM vs. rule-based decision, event emission, UX broker,
    // and debounce. See `crate::session_memory::MemoryExtractionService`.
    maybe_run_memory_extraction(state);

    reset_per_turn_advisory_state(state);
    state.refresh_task_board_snapshot().await;
    ensure_terminal_text(state);
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
    if !state.final_text.is_empty() && !state.final_text_streamed {
        host.render_final_text(&state.final_text);
        state.final_text_streamed = true;
    }
}

fn update_working_memory_for_turn_settlement(state: &mut AgenticLoopState) {
    let task_summary = state
        .hooks
        .task_board_snapshot
        .has_paused_or_blocked_tasks()
        .then(|| state.hooks.task_board_snapshot.short_summary());
    let interruption = state.interruption.clone();
    let Some(session) = state.pipeline_session.as_mut() else {
        return;
    };
    let memory = session.working_memory_mut();

    // Rebuild blocker pressure from current settlement state instead of
    // accumulating old outages/nudges across turns.
    memory.clear_blockers();

    if let Some(summary) = task_summary {
        memory.clear_next_action();
        memory.push_blocker(format!(
            "unfinished_task_board: {}",
            bounded_working_memory_line(&summary)
        ));
        if let Some(interruption) = interruption.as_ref()
            && interruption_requires_intervention(interruption)
        {
            memory.push_blocker(format!(
                "{}: {}",
                interruption.kind.label(),
                bounded_working_memory_line(&interruption.user_message)
            ));
        }
        return;
    }

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

fn ensure_terminal_text(state: &mut AgenticLoopState) {
    if state.hooks.task_board_snapshot.has_unfinished_tasks() {
        tracing::info!(
            target: "astra::loop_guard",
            summary = %state.hooks.task_board_snapshot.short_summary(),
            "unfinished task-board state retained as settlement evidence"
        );
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
    // When the model produced tool calls (reads, edits, etc.) but no summary
    // text, the turn was doing active work — not a truly empty completion.
    // Use EmptyCompletion (semantically correct: loop ended, no final answer)
    // but provide rich context so the user sees progress, not silence.
    if state.total_tool_calls > 0 {
        // ── Build tool summary (include failed tools marked as such) ──
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
            format!(
                " Recent tools: {}. You can continue without re-reading — results are above.",
                unique.join(", ")
            )
        };
        let checkpoint_note = if state.stall.last_heavy_checkpoint.is_some() {
            " A checkpoint was saved."
        } else {
            ""
        };
        // ── Surface pre-existing interruption reason (e.g. circuit breaker) ──
        // When the loop was aborted by BudgetExhausted, GuardAbort, etc.
        // the user deserves to know *why*, not just that tools ran.
        let reason_note = match state.interruption.as_ref() {
            Some(i)
                if !matches!(
                    i.kind,
                    astra_turn_core::interruption::InterruptionKind::EmptyCompletion
                ) =>
            {
                let mut note = format!(" Interruption: {}.", i.kind.label());
                append_interruption_detail(&mut note, i);
                note
            }
            _ => String::new(),
        };
        let textless_note = if state.last_finish_reason.as_deref() == Some("tool_calls") {
            " The model was still requesting tools and did not produce final text."
        } else {
            " The loop ended without final text."
        };
        let rounds_completed = state.max_turns.saturating_sub(state.remaining_turns);
        let budget_note = if state.max_turns > 0 {
            format!(
                " Rounds: {rounds_completed}/{} completed, {} remaining.",
                state.max_turns, state.remaining_turns
            )
        } else {
            String::new()
        };
        let next_step_note = " Continue to resume from the preserved state; first summarize the evidence already gathered, then choose the next targeted action or provide the final answer.";
        // Set final_text BEFORE interruption so settlement_interruption_summary
        // sees the populated value (avoid coupling trap).
        state.final_text = format!(
            "[turn_interrupted] {} tool call(s) completed.{}{}{} Work preserved above.{}{}{}",
            state.total_tool_calls,
            reason_note,
            textless_note,
            budget_note,
            checkpoint_note,
            tool_summary,
            next_step_note,
        );
        state.final_text_streamed = false;

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
    if matches!(
        interruption.kind,
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted
            | astra_turn_core::interruption::InterruptionKind::TokenBudgetExceeded
            | astra_turn_core::interruption::InterruptionKind::CumulativeBudgetExceeded
    ) && let Some(detail) = interruption
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
    state.stall.execution_escalation_advisory_emitted = false;
    state.stall.parallel_batching_advisory_emitted = false;
    state.stall.repetition_advisory_emitted = false;
    state.stall.redundant_reads_advisory_emitted = false;
    state.stall.cache_waste_advisory_emitted = false;
    state.stall.search_fanout_advisory_emitted = false;
    state.stall.exploration_family_advisory_emitted = false;
    state.stall.stronger_exploration_family_advisory_emitted = false;
    state.stall.intent_drift_advisory_emitted = false;
    // NOTE: drift_nudge_count and last_drift_correction_round persist across turns
    state.stall.exploration_family_advisory_family = None;
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
    let Some(user_id) = state.context_manifest_user_id.clone() else {
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
    let runtime_decision_user_intent = state.runtime_decision_user_intent();
    let req = crate::session_memory::ExtractionRequest {
        user_id,
        session_id,
        messages: state.messages.clone(),
        session_facts: state.session_facts.clone(),
        had_error,
        had_user_correction: astra_turn_core::input_classifier::is_reanchor_signal(
            &runtime_decision_user_intent,
        ),
        turn_number,
    };

    match svc.maybe_spawn(req) {
        crate::session_memory::SpawnDecision::Spawned => {}
        crate::session_memory::SpawnDecision::Queued => {}
        crate::session_memory::SpawnDecision::Skipped => {}
    }
}

fn drop_unattributed_memory_recalls_at_turn_end(state: &mut AgenticLoopState) {
    let Some(session_id) = state
        .current_session_id
        .as_deref()
        .filter(|sid| !sid.is_empty())
    else {
        return;
    };
    let dropped = astra_tools::memoria::MemoriaClient::drain_recalls(session_id, None).len();
    if dropped > 0 {
        tracing::debug!(
            session_id = %session_id,
            dropped,
            "dropped unattributed memory recalls without changing their rank"
        );
    }
}

#[cfg(test)]
mod tests {
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
            state.final_text.contains("budget_exhausted"),
            "interrupted tool-only turns must not persist an empty or success-shaped final answer"
        );
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
        assert!(state.final_text.contains("[turn_interrupted]"));
        assert!(state.final_text.contains("tool call(s) completed"));
        assert!(state.final_text.contains("Rounds:"));
        assert!(state.final_text.contains("Continue to resume"));
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);
    }

    #[tokio::test]
    async fn finalize_and_render_does_not_pause_final_answer_for_in_progress_bookkeeping() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text = "Done.".into();
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "finish validation".to_string(),
                    description: None,
                    status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                    subtasks: Vec::new(),
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                    updated_at: "2025-01-01T00:00:00Z".to_string(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            ]);

        finalize_and_render(&mut host, &mut state).await;

        assert_eq!(
            state.final_text, "Done.",
            "assistant text is model output and must not be rewritten by task-board control state"
        );
        assert!(
            state.interruption.is_none(),
            "in-progress task-board bookkeeping must not turn an answered run into a paused run"
        );
        assert_eq!(host.rendered_final_text, vec!["Done.".to_string()]);
    }

    #[tokio::test]
    async fn finalize_and_render_persists_paused_task_as_blocker_not_auto_resume() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        attach_pipeline_session(&mut state);
        state.final_text = "Done.".into();
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "finish validation".to_string(),
                    description: None,
                    status: astra_tools::task_mgmt::SessionTaskStatusKind::Paused,
                    subtasks: Vec::new(),
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                    updated_at: "2025-01-01T00:00:00Z".to_string(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            ]);

        finalize_and_render(&mut host, &mut state).await;

        let rendered = state
            .pipeline_session
            .as_ref()
            .expect("pipeline session")
            .working_memory()
            .render_prompt_section();
        assert!(
            rendered.contains("Blockers:") && rendered.contains("unfinished_task_board:"),
            "paused task-board state must be preserved without auto-resume pressure: {rendered}"
        );
        assert!(
            !rendered.contains("Next action:"),
            "unfinished task-board state must not become an automatic next action: {rendered}"
        );
        assert!(rendered.contains("finish validation"));
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
            serde_json::json!({"role": "user", "content": "我说过的所有话\n\n<system-reminder>\n[session-resume:v1]\nHydrated previous session context\n</system-reminder>"}),
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
    fn root_heavy_checkpoint_promotes_session_composite_snapshot() {
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

        let index =
            astra_pipeline::step_checkpoint::read_composite_snapshot_index(user_id, &session_id)
                .expect("read composite index");
        assert_eq!(index.snapshots.len(), 1);
        assert_eq!(index.snapshots[0].session_id, session_id);
        assert_eq!(index.snapshots[0].turn, 7);
        assert!(
            state.last_composite_snapshot.is_some(),
            "root loop must expose the current session composite snapshot"
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
        state.step_recorder.begin_turn(0);

        try_write_heavy_checkpoint(&mut state);

        let heavy =
            astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, &session_id)
                .expect("read checkpoint")
                .expect("delegated heavy checkpoint");
        assert_eq!(heavy.budget_remaining_rounds, state.remaining_turns as u32);

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
    }

    #[tokio::test]
    async fn finalize_and_render_refreshes_task_board_before_settlement() {
        let manager = std::sync::Arc::new(astra_tools::task_mgmt::TaskManager::in_memory());
        manager
            .create(&serde_json::json!({"title": "finish validation"}))
            .await;
        manager
            .update(&serde_json::json!({
                "task_id": "task-1",
                "new_status": "in_progress"
            }))
            .await;
        manager
            .update(&serde_json::json!({
                "task_id": "task-1",
                "new_status": "completed"
            }))
            .await;

        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text = "Done.".into();
        state.hooks.task_board_monitor = Some(manager);
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "finish validation".to_string(),
                    description: None,
                    status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                    subtasks: Vec::new(),
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                    updated_at: "2025-01-01T00:00:00Z".to_string(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            ]);

        finalize_and_render(&mut host, &mut state).await;

        assert_eq!(
            state.final_text, "Done.",
            "finalization must refresh active tasks so completed tool-round work does not get falsely blocked"
        );
        assert!(
            !state.hooks.task_board_snapshot.has_unfinished_tasks(),
            "refreshed snapshot should observe that the task board has no active tasks"
        );
        assert_eq!(host.rendered_final_text, vec!["Done.".to_string()]);
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
    async fn finalize_and_render_empty_completion_is_independent_of_task_board_state() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text.clear();
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "finish validation".to_string(),
                    description: None,
                    status: astra_tools::task_mgmt::SessionTaskStatusKind::InProgress,
                    subtasks: Vec::new(),
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                    updated_at: "2025-01-01T00:00:00Z".to_string(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            ]);

        finalize_and_render(&mut host, &mut state).await;

        let interruption = state
            .interruption
            .as_ref()
            .expect("empty model completion should record interruption state");
        assert_eq!(
            interruption.kind,
            astra_turn_core::interruption::InterruptionKind::EmptyCompletion
        );
        assert!(matches!(
            &interruption.resume_action,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately
        ));
        assert_eq!(
            state.final_text,
            interruption_terminal_message(interruption)
        );
        assert_eq!(host.rendered_final_text, vec![state.final_text.clone()]);
    }

    #[tokio::test]
    async fn paused_task_board_does_not_override_a_real_final_answer() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text = "Current evidence supports shipping the verified fix.".into();
        state.hooks.task_board_snapshot =
            crate::turn::agentic_loop::host::TaskBoardSnapshot::from_active_tasks(&[
                astra_tools::task_mgmt::SessionTask {
                    archived_at: None,
                    id: "task-1".to_string(),
                    title: "optional follow-up".to_string(),
                    description: None,
                    status: astra_tools::task_mgmt::SessionTaskStatusKind::Paused,
                    subtasks: Vec::new(),
                    created_at: "2025-01-01T00:00:00Z".to_string(),
                    updated_at: "2025-01-01T00:00:00Z".to_string(),
                    active_form: None,
                    owner: None,
                    metadata: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                },
            ]);

        finalize_and_render(&mut host, &mut state).await;

        assert!(state.interruption.is_none());
        assert_eq!(
            state.final_text,
            "Current evidence supports shipping the verified fix."
        );
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
    async fn mutating_profile_text_completion_is_preserved_without_guard_retry() {
        let mut host = MockHost::new(vec![text_result(
            "No workspace mutation was needed based on the evidence.",
            10,
            5,
            Some(42),
        )]);
        let mut state = make_state();
        state.message = "fix the bug".to_string();
        state.user_intent = state.message.clone();
        mark_must_mutate(&mut state);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;

        assert!(outcome.is_ok());
        assert_eq!(host.turn_count(), 1);
        assert_eq!(
            state.final_text,
            "No workspace mutation was needed based on the evidence."
        );
        assert_eq!(
            state.messages,
            vec![serde_json::json!({
                "role": "assistant",
                "content": "No workspace mutation was needed based on the evidence."
            })],
            "runtime behavior signals must not add synthetic control history"
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
    async fn finalize_turn_trace_preserves_first_pending_trace_within_outer_turn() {
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
        }]);
        second.record_token_budget_estimate(5_000, 123, 0, 2_000, 100, 7_223, 100_000, 0.07223);
        state.telemetry.turn_trace_collector = Some(second);
        finalize_turn_trace(&mut state).await;

        assert_eq!(
            state.telemetry.pending_context_assembly_trace,
            Some(first_pending)
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
    impl crate::turn::cloud::memoria_compact::MemoriaClient for CapturingMemoriaForFinalize {
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
                Arc::new(crate::session_memory::ConstSelectorResolver(None)),
                Arc::clone(&memoria) as Arc<dyn crate::turn::cloud::memoria_compact::MemoriaClient>,
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
    async fn finalize_and_render_skips_low_information_turn_and_emits_typed_reason() {
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
        let (mut rx, memoria) = attach_memory_extraction_service(&mut state);

        finalize_and_render(&mut host, &mut state).await;

        // No Memoria store happened.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            memoria.stored.lock().unwrap().is_empty(),
            "a low-information turn should not create session memory"
        );

        let mut saw_low_information = false;
        while let Ok(evt) = rx.try_recv() {
            if evt.event_type != "session_memory_extraction" {
                continue;
            }
            let meta = evt.metadata.as_ref().unwrap();
            if meta["outcome"] == "skipped" && meta["reason"] == "low_information" {
                saw_low_information = true;
            }
        }
        assert!(
            saw_low_information,
            "expected a typed skipped{{low_information}} event"
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
    async fn finalize_and_render_drains_pending_recall_feedback_for_server_executor() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        let session_id = format!("server-finalize-feedback-{}", uuid::Uuid::new_v4());
        let workspace = tempfile::TempDir::new().unwrap();
        let executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        astra_tools::memoria::MemoriaClient::reset_recall_ledger(&session_id);
        astra_tools::memoria::MemoriaClient::record_recall(&session_id, 1, vec!["m1".into()]);
        state.current_session_id = Some(session_id.clone());
        state.runtime_tool_executor = Some(std::sync::Arc::new(executor));
        state.final_text = "Done.".into();

        finalize_and_render(&mut host, &mut state).await;

        assert_eq!(
            astra_tools::memoria::MemoriaClient::pending_recall_count(&session_id),
            0,
            "server finalization must drain pending recall feedback on memory-only turns"
        );
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
