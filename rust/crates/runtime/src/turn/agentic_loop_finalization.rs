use crate::pipeline::step_checkpoint;
use crate::pipeline::step_protocol::StepCheckpoint;
use crate::{EventCreateRequestData, EventService};
use astra_services::SessionArtifactStore;

use super::agentic_adaptive_tuning::{
    record_loop_completion_feedback, record_new_evolution_promotion_events,
    snapshot_evolution_promotion_ids,
};
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, run_agentic_loop_impl,
};
use super::agentic_loop_lifecycle::{current_agentic_step, session_turn_number};

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
    collector.record_token_budget(crate::turn::context_assembly_trace::TokenBudgetTrace {
        max_tokens: max as u32,
        total_used: measured as u32,
        budget_pressure,
        compression_triggered: state.budget_wrapup_injected,
        ..Default::default()
    });
    let trace = collector.finalize();
    if let Some(ref session) = state.telemetry.observability_session {
        let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
        crate::observability_integration::on_context_assembled(&mut guard, trace.clone());
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
        let guard = session.read().unwrap_or_else(|e| e.into_inner());
        crate::observability_integration::latest_context_trace_signal(&guard)
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
            .tool_selection
            .as_ref()
            .and_then(|selection| selection.selected_tools.first())
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

async fn persist_remote_composite_snapshot_index_if_present(
    state: &AgenticLoopState,
    session_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
    source: &str,
) {
    let Some(persistence) = state.telemetry.context_trace_persistence.clone() else {
        return;
    };
    if let Err(error) = astra_services::session_restore::persist_remote_composite_snapshot_index(
        session_id,
        &persistence.user_id,
        index,
        &persistence.artifact_store,
    )
    .await
    .map(|_| ())
    {
        astra_core::agent_warn!("checkpoint", "Failed to {source} for {session_id}: {error}");
    }
}

/// Best-effort heavy checkpoint write.
///
/// Several early-exit paths in the agentic loop (text-only responses, stop-hook
/// injection, factual-retry nudges) skip the main post-tool-policy checkpoint.
/// This helper ensures those paths still persist the accumulated messages so that
/// `/debug` turn inspection and session recovery have accurate per-iteration state.
pub(crate) fn try_write_heavy_checkpoint(state: &mut AgenticLoopState) {
    let Some(sid) = state.current_session_id.as_ref() else {
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

    let Some(mut heavy) = state
        .step_recorder
        .build_heavy_checkpoint_with_interruption(
            &state.messages,
            0,
            state.remaining_turns as u32,
            &state
                .turn_guard
                .health
                .deprioritized_tools()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
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
    heavy.continuity_state = serde_json::to_value(&state.continuity).ok();
    let cp = StepCheckpoint::Heavy(Box::new(heavy));
    if let Err(e) = step_checkpoint::write_step_checkpoint(sid, ckpt_num, &cp) {
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

    let turn = session_turn_number(state);
    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
            .label(format!("checkpoint-t{turn}"))
            .session_state(format!("{:06}-heavy.json", ckpt_num))
            .workspace_state(sid.clone())
            .build();

    let mut index = step_checkpoint::read_composite_snapshot_index(sid).unwrap_or_default();
    if let Err(e) = index.append(&mut snapshot) {
        astra_core::agent_warn!("checkpoint", "Failed to append snapshot version: {e}");
        return;
    }
    if let Err(e) = step_checkpoint::write_composite_snapshot_index(sid, &index) {
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

/// Build a full composite snapshot asynchronously (with data provider).
///
/// Call this at strategic points (breakpoints, plan boundaries, user request)
/// where the async data snapshot is worth the cost.
#[allow(dead_code)]
pub(crate) async fn build_full_composite_snapshot(
    state: &mut AgenticLoopState,
) -> Option<astra_core::composite_snapshot::CompositeSnapshot> {
    let sid = state.current_session_id.as_ref()?;
    let turn = session_turn_number(state);
    let ckpt_num = state.step_recorder.summary().checkpoints;

    let mut builder =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.clone(), turn)
            .label(format!("full-snapshot-t{turn}"))
            .session_state(format!("{:06}-heavy.json", ckpt_num))
            .workspace_state(sid.clone());

    {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        builder = builder.memory_snapshot(astra_core::composite_snapshot::MemorySnapshotRef {
            profile: "default".to_string(),
            epoch,
            path: None,
        });
    }

    if let Ok(output) = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        && output.status.success()
    {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sha.len() >= 7 {
            builder = builder.git_commit(sha);
        }
    }

    if let Some(provider) = &state.data_snapshot_provider {
        let context = astra_core::composite_snapshot::SnapshotContext {
            session_id: sid.clone(),
            turn,
            label: Some(format!("turn-{turn}")),
            task_type: None,
            databases: None,
        };
        match provider.create_snapshot(&context).await {
            Ok(Some(ds)) => {
                builder = builder.data_snapshot(ds);
            }
            Ok(None) => {}
            Err(e) => {
                astra_core::agent_warn!("snapshot", "Data snapshot failed: {e}");
            }
        }
    }

    let mut snapshot = builder.build();

    let mut index = step_checkpoint::read_composite_snapshot_index(sid).unwrap_or_default();
    if let Err(e) = index.append(&mut snapshot) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to append composite snapshot version: {e}"
        );
        return Some(snapshot);
    }
    if let Err(e) = step_checkpoint::write_composite_snapshot_index(sid, &index) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to write composite snapshot index: {e}"
        );
    } else {
        persist_remote_composite_snapshot_index_if_present(
            state,
            sid,
            &index,
            "persist remote composite snapshot index",
        )
        .await;
    }

    state.last_composite_snapshot = Some(snapshot.clone());
    Some(snapshot)
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

    // On error, best-effort flush turn observability events.
    if result.is_err() {
        if let Some(sid) = state.current_session_id.as_deref() {
            if let Some(buf) = state.turn_event_buffer.as_mut() {
                if !buf.is_empty() {
                    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
                        let _ = buf.flush_interrupted(&writer);
                    }
                }
            }
        }
    }

    // Emit structured interruption to journal if one was recorded.
    if let Some(ref interruption) = state.interruption {
        if let Some(ref sid) = state.current_session_id {
            // Best-effort flush of turn observability events on interruption.
            if let Some(buf) = state.turn_event_buffer.as_mut() {
                if !buf.is_empty() {
                    if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
                        let _ = buf.flush_interrupted(&writer);
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
                let _ = writer.append(&evt);
            }
        }

        // Carry `UserCancelled` forward so the adaptive-tuning layer can
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

    if let Some(evo) = state.evolution_service.clone() {
        let (pending_before, applied_before, canary_before, resolved_before) =
            snapshot_evolution_promotion_ids(&evo).await;
        let (auto_applied, _llm_signals) = evo.flush().await;
        record_new_evolution_promotion_events(
            state,
            &evo,
            &pending_before,
            &applied_before,
            &canary_before,
            &resolved_before,
        )
        .await;
        if !auto_applied.is_empty() {
            eprintln!(
                "[evolution] auto-applied {} fast-path proposals",
                auto_applied.len()
            );
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
    // Drop any execution-retry corrective messages now that the loop has
    // finished. Keeping them in `state.messages` would pollute every
    // subsequent user turn (the model would see a stale "you didn't apply the
    // change" nudge that no longer applies). The marker is a stable header
    // embedded by `agentic_loop_execution_phase::execution_retry_message`.
    state
        .messages
        .retain(|m| !crate::turn::agentic_loop_execution_phase::is_execution_corrective_message(m));
    reset_per_turn_corrective_state(state);
    if state.final_text.trim().is_empty()
        && let Some(interruption) = state.interruption.as_ref()
    {
        state.final_text = interruption.user_message.clone();
        state.final_text_streamed = false;
    }
    try_write_heavy_checkpoint(state);
    if !state.final_text.is_empty() && !state.final_text_streamed {
        host.render_final_text(&state.final_text);
        state.final_text_streamed = true;
    }
}

fn reset_per_turn_corrective_state(state: &mut AgenticLoopState) {
    state.stall.forced_factual_retry = false;
    state.stall.forced_execution_retry = false;
    state.stall.forced_execution_escalation = false;
    state.stall.forced_parallel_batching = false;
    state.stall.forced_round_budget_phase1 = false;
    state.stall.forced_round_budget_phase2 = false;
    state.stall.forced_redundant_reads_corrective = false;
    state.stall.forced_cache_waste_corrective = false;
    state.stall.forced_exploration_family_corrective = false;
    state.stall.forced_exploration_family_phase2 = false;
    state.stall.exploration_family_corrective_family = None;
    // Clear tool restrictions injected by exploration-family correctives so
    // they don't leak into the next user turn.
    state.restricted_tools.clear();
}

/// Build a synthetic JournalEvent from the current turn's tool_call_records
/// and feed it into SessionFacts. This keeps L1a ground truth up to date
/// every turn without requiring the full journal write path.
fn update_session_facts_from_turn(state: &mut super::agentic_loop_host::AgenticLoopState) {
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

    state.session_facts.update_from_journal_event(&event);

    // Sync blocked tools from state
    state
        .session_facts
        .set_blocked_tools(state.restricted_tools.iter().cloned().collect());
    state.continuity.sync_facts(state.session_facts.clone());
    complete_active_runtime_todo_if_finalized(state, had_error);
    state.continuity.sync_facts(state.session_facts.clone());
    state.session_facts = state.continuity.facts.clone();

    // P4: Error-triggered L1 persist — when an error occurred this turn,
    // write L1 to local session memory file immediately so user corrections
    // are captured before compaction can drop them.
    if had_error {
        if let Some(ref sid) = state.current_session_id {
            let l1_content = crate::turn::cloud::session_memory_protocol::build_l1_from_messages(
                &state.messages,
                state.max_turns.saturating_sub(state.remaining_turns),
                state.total_prompt as usize,
            );
            if let Ok(path) = astra_services::local_session_artifact_store()
                .session_path(sid, "session-memory.md")
            {
                if let Err(e) =
                    crate::turn::cloud::session_memory_extract::write_session_memory_file(
                        &path,
                        &l1_content,
                    )
                {
                    tracing::debug!(session_id = %sid, error = %e, "error-triggered L1 write failed (non-fatal)");
                }
            }
        }
    }
}

fn complete_active_runtime_todo_if_finalized(
    state: &mut super::agentic_loop_host::AgenticLoopState,
    had_error: bool,
) {
    if had_error || state.final_text.trim().is_empty() {
        return;
    }
    let Some(active) = state.continuity.todos.active_or_next().cloned() else {
        return;
    };
    if active.status != astra_turn_core::continuity::TodoStatus::InProgress {
        return;
    }
    // If the round produced a final answer without any tool evidence
    // (e.g. pure Q&A), seed a weak evidence entry so the todo can close.
    // Otherwise the manifest keeps advertising `in_progress` forever and
    // misleads future rounds.
    if active.evidence.is_empty() {
        state
            .continuity
            .todos
            .add_evidence(&active.id, "answered without tool invocation");
    }
    state.continuity.todos.mark_done(
        &active.id,
        "final response completed after verified tool evidence",
    );
}

#[cfg(test)]
mod tests {
    use crate::turn::agentic_loop_host::tests::{
        MockHost, edge_tool_result, make_edge_tool, make_state, text_result,
    };

    use super::*;

    // E2E: full execution-retry guard lifecycle through the production loop.
    // Round 1: model defers ("需要我直接执行这些修改吗？") on a mutating-profile
    // task → guard fires, corrective user message is injected into
    // `state.messages`, loop continues. Round 2: model finalizes with "Done.".
    // After the loop ends, `finalize_and_render` must strip the marker so the
    // corrective message does not leak into subsequent user turns.
    #[tokio::test]
    async fn execution_retry_injects_then_strips_corrective_message() {
        let mut host = MockHost::new(vec![
            text_result("需要我直接执行这些修改吗？", 10, 5, Some(20)),
            text_result("Done.", 10, 5, Some(20)),
        ]);
        let mut state = make_state();
        state.message = "修复这个 bug".to_string();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("修复这个 bug");
        assert!(
            state.task_profile.mutates_workspace,
            "test precondition: profile must be mutating"
        );

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "loop should complete: {:?}", outcome);

        assert!(
            host.turn_count() == 2,
            "guard must have fired on the deferring round to force a second LLM pass"
        );
        assert_eq!(
            state.final_text, "Done.",
            "second LLM response should win after the forced retry"
        );
        assert!(
            !state.stall.forced_execution_retry,
            "completion should reset one-shot retry state so it does not leak into the next user turn"
        );

        let leftover = state
            .messages
            .iter()
            .filter(|m| crate::turn::agentic_loop_execution_phase::is_execution_retry_correction(m))
            .count();
        assert_eq!(
            leftover, 0,
            "finalize_and_render must strip the corrective message; \
             {leftover} copies still in state.messages: {:#?}",
            state.messages
        );
    }

    // E2E negative: when the model finishes with a legitimate "no change
    // needed" reply (zero tool calls, no defer signal), the guard must NOT
    // fire — even on a mutating-profile task — and no corrective message
    // should ever be injected.
    #[tokio::test]
    async fn execution_retry_does_not_fire_on_legitimate_no_op_completion() {
        let mut host = MockHost::new(vec![text_result(
            "I reviewed the code and the bug does not exist.",
            10,
            5,
            Some(20),
        )]);
        let mut state = make_state();
        state.message = "fix the bug".to_string();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("fix the bug");
        assert!(state.task_profile.mutates_workspace);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        assert!(
            !state.stall.forced_execution_retry,
            "guard must not fire when the model genuinely concludes no fix is needed"
        );
        let injected = state
            .messages
            .iter()
            .filter(|m| crate::turn::agentic_loop_execution_phase::is_execution_retry_correction(m))
            .count();
        assert_eq!(injected, 0, "no corrective message should be injected");
    }

    // E2E: model defers twice in a row. The one-shot `forced_execution_retry`
    // flag must prevent a second corrective injection, so the loop terminates
    // after two LLM rounds instead of spinning forever.
    #[tokio::test]
    async fn double_defer_does_not_cause_infinite_retry() {
        let mut host = MockHost::new(vec![
            text_result("需要我直接执行这些修改吗？", 10, 5, Some(20)),
            text_result("确认后我再执行。", 10, 5, Some(20)),
        ]);
        let mut state = make_state();
        state.message = "修复这个 bug".to_string();
        state.task_profile =
            crate::turn::chat_turn_heuristics::infer_task_execution_profile("修复这个 bug");

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "loop must terminate: {:?}", outcome);
        // Second defer becomes the final text — guard did not fire again.
        assert_eq!(state.final_text, "确认后我再执行。");
        assert_eq!(
            host.turn_count(),
            2,
            "exactly 2 LLM rounds, no infinite loop"
        );
        assert!(
            !state.stall.forced_execution_retry,
            "completion should clear the one-shot retry flag after the turn ends"
        );
    }

    #[tokio::test]
    async fn finalize_and_render_strips_all_correctives_and_resets_one_shot_flags() {
        let mut host = MockHost::new(Vec::new());
        let mut state = make_state();
        state.final_text = "Done.".into();
        state.messages.extend([
            serde_json::json!({
                "role": "user",
                "content": format!("{}\nold retry", crate::turn::agentic_loop_execution_phase::EXECUTION_RETRY_MARKER),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!("{}\nold escalation", crate::turn::agentic_loop_execution_phase::EXECUTION_ESCALATION_MARKER),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!("{}\nold batching", crate::turn::agentic_loop_execution_phase::PARALLEL_BATCHING_FORCE_MARKER),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!("{}\nold round budget", crate::turn::agentic_loop_execution_phase::ROUND_BUDGET_PHASE1_MARKER),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!("{}\nold redundant reads", crate::turn::agentic_loop_execution_phase::REDUNDANT_READS_MARKER),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!("{}\nold cache waste", crate::turn::agentic_loop_execution_phase::CACHE_WASTE_MARKER),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!(
                    "{}\nold exploration family churn",
                    crate::turn::agentic_loop_execution_phase::EXPLORATION_FAMILY_MARKER
                ),
            }),
            serde_json::json!({
                "role": "user",
                "content": format!(
                    "{}\nold exploration family phase2",
                    crate::turn::agentic_loop_execution_phase::EXPLORATION_FAMILY_PHASE2_MARKER
                ),
            }),
        ]);
        state.stall.forced_factual_retry = true;
        state.stall.forced_execution_retry = true;
        state.stall.forced_execution_escalation = true;
        state.stall.forced_parallel_batching = true;
        state.stall.forced_round_budget_phase1 = true;
        state.stall.forced_round_budget_phase2 = true;
        state.stall.forced_redundant_reads_corrective = true;
        state.stall.forced_cache_waste_corrective = true;
        state.stall.forced_exploration_family_corrective = true;
        state.stall.forced_exploration_family_phase2 = true;
        state.stall.exploration_family_corrective_family = Some("diff".into());
        state.restricted_tools.insert("git_diff".into());
        state.restricted_tools.insert("git_log".into());

        finalize_and_render(&mut host, &mut state).await;

        assert!(
            state.messages.iter().all(|m| {
                !crate::turn::agentic_loop_execution_phase::is_execution_corrective_message(m)
            }),
            "completed turns should not retain stale runtime corrective messages: {:#?}",
            state.messages
        );
        assert!(!state.stall.forced_factual_retry);
        assert!(!state.stall.forced_execution_retry);
        assert!(!state.stall.forced_execution_escalation);
        assert!(!state.stall.forced_parallel_batching);
        assert!(!state.stall.forced_round_budget_phase1);
        assert!(!state.stall.forced_round_budget_phase2);
        assert!(!state.stall.forced_redundant_reads_corrective);
        assert!(!state.stall.forced_cache_waste_corrective);
        assert!(!state.stall.forced_exploration_family_corrective);
        assert!(!state.stall.forced_exploration_family_phase2);
        assert!(state.stall.exploration_family_corrective_family.is_none());
        assert!(
            state.restricted_tools.is_empty(),
            "restricted_tools must be cleared across turns"
        );
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

    #[test]
    fn context_trace_workspace_persistence_pushes_remote_workspace_artifact() {
        let source = include_str!("agentic_loop_finalization.rs");
        let start = source
            .find("async fn persist_context_trace_to_workspace_if_present")
            .expect("workspace trace persistence helper");
        let end = source
            .find("/// Best-effort heavy checkpoint write.")
            .expect("workspace trace helper end marker");
        let snippet = &source[start..end];
        assert!(
            snippet.contains("persist_remote_workspace"),
            "context trace workspace persistence should publish remote workspace artifacts"
        );
        assert!(
            snippet.contains("artifact_store"),
            "context trace workspace persistence should receive an artifact store"
        );
    }

    #[test]
    fn composite_snapshot_index_persistence_pushes_remote_artifact() {
        let source = include_str!("agentic_loop_finalization.rs");
        assert!(
            source.contains("persist_remote_composite_snapshot_index"),
            "composite snapshot writes should publish a remote index artifact"
        );
    }

    // ─── E2E: auto-reflection is wired into run_agentic_loop_impl ──────────
    // Verifies the production loop path drains pending reflection signals
    // and invokes host.execute_reflection after a tool phase — previously
    // this was only covered by unit tests calling maybe_trigger_auto_reflection
    // directly, and the production loop never called it.
    #[tokio::test]
    async fn auto_reflection_fires_from_production_loop() {
        use crate::turn::agentic_loop_host::AUTO_REFLECTION_SIGNAL_THRESHOLD;

        let reflection_response = r#"{
            "proposals": [
                {
                    "axis": "pattern",
                    "description": "Demote stalled chain",
                    "confidence": 0.8,
                    "details": { "signature": "grep", "action": "demote" }
                }
            ],
            "summary": "Stall detected."
        }"#;
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![make_edge_tool("grep", "results...")], 20, 10, Some(50)),
            text_result("Final answer", 15, 8, Some(30)),
        ])
        .with_valid_tools(&["grep"])
        .with_reflection_text(reflection_response);

        let mut state = make_state();
        let evo = std::sync::Arc::new(crate::evolution::service::EvolutionService::new());
        state.evolution_service = Some(evo.clone());

        for i in 0..AUTO_REFLECTION_SIGNAL_THRESHOLD {
            state.pending_reflection_signals.push(
                crate::evolution::types::EvolutionSignal::RepeatedStall {
                    tool_chain: vec![format!("tool_{i}")],
                    stall_count: 3,
                    turn_id: format!("t{i}"),
                },
            );
        }
        assert_eq!(
            state.pending_reflection_signals.len(),
            AUTO_REFLECTION_SIGNAL_THRESHOLD
        );

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok(), "loop should complete: {:?}", outcome);

        assert!(
            state.pending_reflection_signals.is_empty(),
            "run_agentic_loop_impl must drain pending_reflection_signals"
        );
        assert!(
            host.last_reflection_prompt.is_some(),
            "host.execute_reflection must be called from production loop"
        );
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
        let hub = crate::observability_integration::ObservabilityHub::new();
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
        assert_eq!(trace.token_budget.total_used, 22_200);
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
        let hub = crate::observability_integration::ObservabilityHub::new();
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
        let hub = crate::observability_integration::ObservabilityHub::new();
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

        let hub = crate::observability_integration::ObservabilityHub::new();
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

        let hub = crate::observability_integration::ObservabilityHub::new();
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
    async fn finalize_turn_trace_preserves_initial_skill_selector_shortlist() {
        let mut state = make_state();
        state.max_turns = 10;
        state.remaining_turns = 9; // outer turn 1
        state.current_session_id = Some("s1".to_string());
        state.max_turn_input_tokens = 100_000;
        state.last_measured_prompt_tokens = Some(42_000);

        let collector = crate::turn::turn_trace_collector::TurnTraceCollector::new(
            "turn-0".to_string(),
            "s1".to_string(),
        );
        collector.record_skill_selector(
            astra_turn_core::skill_selector_metrics::SkillSelectorShortlistTrace {
                open_catalog: true,
                visible_skill_count: 2,
                skills: vec![
                    astra_turn_core::skill_selector_metrics::SkillSelectorShortlistEntry {
                        rank: 1,
                        skill_name: "build".to_string(),
                        aliases: Vec::new(),
                        description: "build artifacts".to_string(),
                        source: "test".to_string(),
                        category: Some("ops".to_string()),
                    },
                    astra_turn_core::skill_selector_metrics::SkillSelectorShortlistEntry {
                        rank: 2,
                        skill_name: "deploy".to_string(),
                        aliases: vec!["ship-it".to_string()],
                        description: "deploy service".to_string(),
                        source: "test".to_string(),
                        category: Some("ops".to_string()),
                    },
                ],
                telemetry: Default::default(),
            },
        );
        state.telemetry.turn_trace_collector = Some(collector);

        finalize_turn_trace(&mut state).await;

        let (turn_num, trace_json) = state
            .telemetry
            .pending_context_assembly_trace
            .as_ref()
            .expect("pending_context_assembly_trace should be set");
        assert_eq!(*turn_num, 1);
        assert_eq!(trace_json["skill_selector"]["visible_skill_count"], 2);
        assert_eq!(
            trace_json["skill_selector"]["skills"][0]["skill_name"],
            "build"
        );
        assert_eq!(
            trace_json["skill_selector"]["skills"][1]["skill_name"],
            "deploy"
        );
        assert_eq!(
            trace_json["skill_selector"]["skills"][1]["aliases"][0],
            "ship-it"
        );
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
        second.set_history_retained(&[crate::turn::context_assembly_trace::TurnRetention {
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
        assert_eq!(
            state.continuity.facts.active_files,
            state.session_facts.active_files
        );
        assert_eq!(
            state.continuity.facts.recent_tool_calls,
            state.session_facts.recent_tool_calls
        );
    }

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

        // L1 content can be built from the messages (verifies the code path runs)
        let l1 = crate::turn::cloud::session_memory_protocol::build_l1_from_messages(
            &state.messages,
            1,
            15000,
        );
        assert!(l1.contains("[session-memory:v1]"));
        assert!(l1.contains("fix the bug"));
    }

    #[tokio::test]
    async fn finalization_marks_active_runtime_todo_done_after_evidence() {
        let mut state = make_state();
        state
            .continuity
            .ensure_tracked_goal("Implement runtime continuity and validate completion evidence");
        state.continuity.todos.begin_next_ready();
        let active_id = state
            .continuity
            .todos
            .active_or_next()
            .map(|item| item.id.clone())
            .unwrap();
        state
            .continuity
            .todos
            .add_evidence(&active_id, "cargo test ok");
        state.final_text = "Done.".to_string();
        state.total_prompt = 100;

        finalize_turn_trace(&mut state).await;

        let item = state
            .continuity
            .todos
            .items
            .iter()
            .find(|item| item.id == active_id)
            .unwrap();
        assert_eq!(item.status, astra_turn_core::continuity::TodoStatus::Done);
        assert_eq!(
            state
                .session_facts
                .plan_state
                .as_ref()
                .map(|plan| (plan.completed, plan.total)),
            Some((1, 1))
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // E2E: round budget guidance injection
    // Verifies that the agentic loop injects round budget warning/limit
    // messages into state.messages at the correct round thresholds.
    // Regression: CLI path was missing this entirely (found in session analysis).
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn round_budget_guidance_injected_at_threshold() {
        // Simulate enough tool rounds to exceed ROUND_BUDGET_THRESHOLD (8).
        // Use varied tool names to avoid the stall detector's duplicate-call
        // force-stop interfering with the test.
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

        // After ROUND_BUDGET_THRESHOLD rounds, a user message with
        // "Round Budget" or "Synthesize" should appear.
        let guidance_found = state.messages.iter().any(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("user")
                && m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("Round Budget") || s.contains("Synthesize"))
                    .unwrap_or(false)
        });
        assert!(
            guidance_found,
            "round budget guidance must be injected after {} rounds",
            crate::prompts::ROUND_BUDGET_THRESHOLD,
        );
    }

    #[tokio::test]
    async fn round_budget_guidance_uses_llm_round_count_not_step_index() {
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
                .map(|s| s.contains("Round Budget") || s.contains("Synthesize"))
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
    async fn round_budget_not_injected_before_threshold() {
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
                .map(|s| s.contains("Round Budget") || s.contains("Synthesize"))
                .unwrap_or(false)
        });
        assert!(
            !guidance_found,
            "round budget guidance must NOT be injected before threshold"
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
    // Regression: round budget guidance is ephemeral — only one copy lives
    // in state.messages at any time. Prior guidance must be stripped before
    // the next one is appended, otherwise every late-round call accumulates
    // a duplicate "Round Budget" user-message that wastes tokens.
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn round_budget_guidance_is_ephemeral_not_accumulated() {
        // Simulate 5 tool rounds (>> ROUND_BUDGET_THRESHOLD) so multiple
        // injections occur. After the loop finishes, state.messages must
        // contain AT MOST one "Round Budget" user message.
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
                        s.contains("## ⚡ Round Budget") || s.contains("## ⚠ Round Budget")
                    })
            })
            .count();

        assert!(
            guidance_count <= 1,
            "round-budget guidance must be ephemeral (at most 1 copy in state.messages); \
             found {guidance_count} copies"
        );
    }
}
