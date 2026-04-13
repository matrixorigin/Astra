use crate::pipeline::step_checkpoint;
use crate::pipeline::step_protocol::StepCheckpoint;

use super::agentic_adaptive_tuning::{
    record_loop_completion_feedback, record_new_evolution_promotion_events,
    snapshot_evolution_promotion_ids,
};
use super::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, run_agentic_loop_impl,
};

/// Finalize the turn trace collector: record measured token budget, feed to
/// observability session, and persist to journal. Called from every exit path
/// in the agentic loop so `/context breakdown` always reflects the latest turn.
pub(crate) fn finalize_turn_trace(state: &mut AgenticLoopState) {
    let Some(collector) = state.telemetry.turn_trace_collector.take() else {
        return;
    };
    if let Some(ref session_id) = state.current_session_id {
        collector.set_session_id(session_id);
    }
    let session_turn = context_trace_turn_number(state);
    collector.set_turn_id(format!("turn-{session_turn}"));
    let measured = state.last_measured_prompt_tokens.unwrap_or(0);
    let max = state.max_turn_input_tokens;
    let budget_pressure = if max > 0 {
        measured as f64 / max as f64
    } else {
        state.telemetry.first_budget_pressure
    };
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
        if let Some(ref sid) = state.current_session_id
            && let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid)
        {
            let event = astra_services::session_journal::JournalEvent::context_assembly_recorded(
                Some(sid),
                session_turn,
                trace.to_json_value(),
            );
            let _ = writer.append(&event);
        }
    }
}

fn context_trace_turn_number(state: &AgenticLoopState) -> u32 {
    let outer_turn = (state.max_turns - state.remaining_turns) as u32;
    state
        .telemetry
        .observability_session
        .as_ref()
        .and_then(|s| s.read().ok().map(|g| g.turn_number))
        .filter(|turn| *turn > 0)
        .unwrap_or(outer_turn)
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
    let Some(heavy) = state.step_recorder.build_heavy_checkpoint(
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
    ) else {
        return;
    };
    let cp = StepCheckpoint::Heavy(Box::new(heavy));
    if let Err(e) = step_checkpoint::write_step_checkpoint(sid, ckpt_num, &cp) {
        astra_core::agent_warn!(
            "checkpoint",
            "Failed to write step checkpoint {ckpt_num}: {e}"
        );
    }

    let turn = (state.max_turns - state.remaining_turns) as u32;
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
    }

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
    let turn = (state.max_turns - state.remaining_turns) as u32;
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
) -> Result<AgenticLoopOutcome, String> {
    let result = run_agentic_loop_impl(host, state).await;

    if let Some(evo) = state.evolution_service.clone() {
        evo.set_runtime_promotion_signals(state.telemetry.runtime_promotion_signals.clone());
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
pub(crate) fn finalize_and_render<H: AgenticLoopHost>(host: &mut H, state: &mut AgenticLoopState) {
    finalize_turn_trace(state);
    try_write_heavy_checkpoint(state);
    if !state.final_text.is_empty() {
        host.render_final_text(&state.final_text);
    }
}
