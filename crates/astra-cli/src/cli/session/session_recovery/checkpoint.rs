//! Checkpoint operations: build, persist, rollback, and delegation helpers.
use super::csl::{ensure_loaded_csl_state, rebuild_csl_from_history};
use super::io::{
    RecoveryCheckpointRollback, append_rollback_error, composite_index_path_for,
    read_optional_file_bytes, restore_optional_file_bytes, sync_parent_dir, workspace_path_for,
};
use super::workspace::persist_recovery_workspace_snapshot;
use crate::cli::cli_config::cli_utils::cli_user_id;
use crate::cli::session::session_projection::history_as_messages;
use crate::cli::session::session_state::SessionState;

fn recovery_user_id(state: &SessionState) -> String {
    state
        .ingestion_user_id
        .as_deref()
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(cli_user_id)
}

/// Diagnostic input headroom for crash recovery. This is deliberately not an
/// execution budget: exhausting the assembled context calls for compaction or
/// a fresh assembly, not terminating the user's task.
fn context_input_headroom_tokens_from_state(state: &SessionState) -> u64 {
    if let Some(trace) = state.latest_context_assembly_trace.as_ref() {
        let limit = u64::from(trace.token_budget.max_tokens);
        if limit > 0 {
            return limit.saturating_sub(u64::from(trace.token_budget.total_used));
        }
    }

    // `total_prompt_tokens` is cumulative session usage, not the size of the
    // latest assembled input. Mixing those clocks produces a plausible but
    // meaningless headroom value. Unknown is represented explicitly as zero.
    0
}

pub(crate) fn delegation_from_heavy_checkpoint(
    heavy: &astra_pipeline::step_protocol::HeavyCheckpoint,
    context: &str,
) -> Option<astra_turn_core::conversation_log::DelegationCompact> {
    match (&heavy.delegation_id, &heavy.delegation_pattern) {
        (Some(id), Some(pattern)) => Some(astra_turn_core::conversation_log::DelegationCompact {
            id: id.clone(),
            pattern: pattern.clone(),
            completed_sub_runs: heavy.delegation_sub_run_summaries.clone(),
        }),
        (None, None) => None,
        _ => {
            astra_core::agent_warn!(
                "checkpoint",
                "{context}: partial delegation fields in heavy checkpoint; dropping delegation restore"
            );
            None
        }
    }
}

pub(crate) fn session_state_compact_from_heavy_checkpoint(
    heavy: &astra_pipeline::step_protocol::HeavyCheckpoint,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        recent_tools: heavy.recent_tools.clone(),
        ..Default::default()
    }
}

pub(crate) fn previous_session_state_for_history_sync(
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
    csl_state: Option<astra_turn_core::conversation_log::SessionStateCompact>,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    csl_state
        .or_else(|| previous_heavy.map(session_state_compact_from_heavy_checkpoint))
        .unwrap_or_default()
}

pub(crate) async fn load_previous_recovery_state(
    state: &mut SessionState,
    user_id: &str,
    sid: &str,
) -> Result<
    (
        Option<astra_pipeline::step_protocol::HeavyCheckpoint>,
        astra_turn_core::conversation_log::SessionStateCompact,
    ),
    String,
> {
    let previous_heavy =
        astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(user_id, sid)
            .map_err(|e| format!("read latest heavy checkpoint: {e}"))?;
    let csl_state = ensure_loaded_csl_state(state, sid).await?;
    let prev_state = previous_session_state_for_history_sync(previous_heavy.as_ref(), csl_state);
    Ok((previous_heavy, prev_state))
}

/// Copy plan / durable-task fields from REPL into workspace before checkpointing.
pub(crate) fn next_step_checkpoint_number(user_id: &str, sid: &str) -> Result<u32, String> {
    let existing = astra_pipeline::step_checkpoint::list_checkpoints(user_id, sid)
        .map_err(|e| format!("list step checkpoints: {e}"))?;
    Ok(existing
        .iter()
        .map(|(n, _)| *n)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// Build heavy step checkpoint from current REPL history (OpenAI-style messages).
pub(crate) fn build_manual_heavy_step_checkpoint(
    state: &SessionState,
    sid: &str,
    _session_state: &astra_turn_core::conversation_log::SessionStateCompact,
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
) -> astra_pipeline::step_protocol::StepCheckpoint {
    use astra_pipeline::step_protocol::{
        ExecutionCursor, HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint,
        epoch_ms,
    };

    let messages = history_as_messages(&state.history);

    let now_ms = epoch_ms();
    let total_tok = state.total_prompt_tokens + state.total_completion_tokens;

    let light = LightCheckpoint {
        protocol_version: PROTOCOL_VERSION,
        cursor: ExecutionCursor::default(),
        step_id: "repl-manual".to_string(),
        task_id: state.run_id.clone().unwrap_or_else(|| "repl".to_string()),
        agent_id: sid.to_string(),
        progress: 1.0,
        total_tokens: total_tok,
        created_at: now_ms,
    };
    let preserve_interrupted_recovery = state.last_turn_interrupted;
    let previous_budget_tokens = preserve_interrupted_recovery
        .then(|| previous_heavy.map(|heavy| heavy.budget_remaining_tokens))
        .flatten();
    let previous_budget_rounds = preserve_interrupted_recovery
        .then(|| previous_heavy.map(|heavy| heavy.budget_remaining_rounds))
        .flatten();
    let interrupted_blocked_tools = if preserve_interrupted_recovery {
        if state.resume_restricted_tools.is_empty() {
            previous_heavy
                .map(|heavy| heavy.blocked_tools.clone())
                .unwrap_or_default()
        } else {
            state.resume_restricted_tools.clone()
        }
    } else {
        Vec::new()
    };
    let interrupted_approval_overrides = preserve_interrupted_recovery
        .then(|| previous_heavy.and_then(|heavy| heavy.approval_overrides.clone()))
        .flatten();
    let interrupted_interruption = preserve_interrupted_recovery
        .then(|| previous_heavy.and_then(|heavy| heavy.interruption.clone()))
        .flatten();
    let interrupted_delegation = preserve_interrupted_recovery
        .then(|| {
            previous_heavy
                .and_then(|heavy| delegation_from_heavy_checkpoint(heavy, "manual_checkpoint"))
        })
        .flatten();

    let heavy = HeavyCheckpoint {
        light,
        messages,
        budget_remaining_tokens: previous_budget_tokens
            .unwrap_or_else(|| context_input_headroom_tokens_from_state(state)),
        // A manual REPL checkpoint is outside an active agentic loop. Session
        // turn count and agentic iteration count are different clocks; using
        // `max_turns - session_turn` fabricated values such as 270 remaining
        // rounds. Zero here means "no active loop snapshot", and this legacy
        // diagnostic is never promoted into prompt or stop policy.
        budget_remaining_rounds: previous_budget_rounds.unwrap_or(0),
        blocked_tools: interrupted_blocked_tools,
        recent_tools: state.recent_tools.clone(),
        memory_context: previous_heavy.and_then(|heavy| heavy.memory_context.clone()),
        delegation_id: interrupted_delegation
            .as_ref()
            .map(|delegation| delegation.id.clone()),
        delegation_pattern: interrupted_delegation
            .as_ref()
            .map(|delegation| delegation.pattern.clone()),
        delegation_sub_run_summaries: interrupted_delegation
            .as_ref()
            .map(|delegation| delegation.completed_sub_runs.clone())
            .unwrap_or_default(),
        interruption: interrupted_interruption,
        approval_overrides: interrupted_approval_overrides,
        consecutive_context_window_errors: if preserve_interrupted_recovery {
            state.runtime_consecutive_context_window_errors
        } else {
            0
        },
        compaction_state: preserve_interrupted_recovery
            .then(|| state.runtime_compaction_state.clone())
            .flatten(),
        pipeline_state: preserve_interrupted_recovery
            .then(|| state.runtime_pipeline_state.clone())
            .flatten(),
        config_version_id: state.config_version_id.clone(),
    };
    StepCheckpoint::Heavy(Box::new(heavy))
}

/// Persist heavy JSON + composite snapshot index before any workspace/journal mutation.
pub(crate) fn persist_manual_heavy_and_composite(
    user_id: &str,
    sid: &str,
    turn: u32,
    title: &str,
    next_step: u32,
    step_cp: &astra_pipeline::step_protocol::StepCheckpoint,
) -> Result<std::path::PathBuf, String> {
    use astra_pipeline::step_checkpoint::{
        read_composite_snapshot_index, write_composite_snapshot_index, write_step_checkpoint,
    };

    let heavy_path = write_step_checkpoint(user_id, sid, next_step, step_cp)
        .map_err(|e| format!("write heavy step checkpoint: {e}"))?;
    let composite_index_path = composite_index_path_for(user_id, sid)?;
    let composite_index_backup = read_optional_file_bytes(&composite_index_path)
        .map_err(|e| format!("backup composite snapshot index: {e}"))?;

    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.to_string(), turn)
            .label(format!("manual:{title}"))
            .session_state(format!("{next_step:06}-heavy.json"))
            .workspace_state(sid.to_string())
            .build();
    let mut index = read_composite_snapshot_index(user_id, sid)
        .map_err(|e| format!("read composite snapshot index: {e}"))?;
    index
        .append(&mut snapshot)
        .map_err(|e| format!("append snapshot version: {e}"))?;
    if let Err(e) = write_composite_snapshot_index(user_id, sid, &index) {
        let mut error_message = format!("write composite snapshot index: {e}");
        append_rollback_error(
            &mut error_message,
            "composite snapshot index",
            restore_optional_file_bytes(&composite_index_path, composite_index_backup),
        );
        append_rollback_error(
            &mut error_message,
            "heavy step checkpoint",
            match std::fs::remove_file(&heavy_path) {
                Ok(()) => sync_parent_dir(&heavy_path),
                Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(remove_error) => Err(format!(
                    "remove heavy file {}: {remove_error}",
                    heavy_path.display()
                )),
            },
        );
        return Err(error_message);
    }

    Ok(heavy_path)
}

pub(crate) fn persist_recovery_checkpoint(
    state: &SessionState,
    user_id: &str,
    sid: &str,
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
) -> Result<RecoveryCheckpointRollback, String> {
    let next_step = next_step_checkpoint_number(user_id, sid)?;
    let composite_index_path = composite_index_path_for(user_id, sid)?;
    let composite_index_backup = read_optional_file_bytes(&composite_index_path)?;
    let step_cp = build_manual_heavy_step_checkpoint(state, sid, session_state, previous_heavy);
    persist_manual_heavy_and_composite(
        user_id,
        sid,
        state.turn,
        "history-sync",
        next_step,
        &step_cp,
    )?;
    Ok(RecoveryCheckpointRollback {
        step_number: next_step,
        composite_index_backup,
    })
}

pub(crate) fn rollback_recovery_checkpoint(
    user_id: &str,
    sid: &str,
    rollback: &RecoveryCheckpointRollback,
) -> Result<(), String> {
    let mut rollback_error = String::new();
    let restore_index_result = composite_index_path_for(user_id, sid).and_then(|path| {
        restore_optional_file_bytes(&path, rollback.composite_index_backup.clone())
    });
    if let Err(error) = restore_index_result {
        rollback_error = format!("restore composite snapshot index: {error}");
    }
    let delete_result = astra_pipeline::step_checkpoint::delete_step_checkpoint(
        user_id,
        sid,
        rollback.step_number,
        "heavy",
    )
    .map_err(|e| {
        format!(
            "delete heavy step checkpoint {:06}: {e}",
            rollback.step_number
        )
    });
    if let Err(error) = delete_result {
        if rollback_error.is_empty() {
            rollback_error = error;
        } else {
            rollback_error.push_str(&format!("; {error}"));
        }
    }
    if rollback_error.is_empty() {
        Ok(())
    } else {
        Err(rollback_error)
    }
}

/// Persist the current in-memory conversation state after a manual history mutation
/// (`/undo`, `/redo`, `/compact`, `/session fork`) so the next resume/fork/headless
/// continuation sees the same context the user just saw.
pub(crate) async fn sync_recovery_snapshot_after_history_edit(
    state: &mut SessionState,
) -> Result<(), String> {
    let Some(sid) = state.session_id.clone().filter(|sid| !sid.is_empty()) else {
        return Ok(());
    };

    super::super::session_projection::rebuild_continuation_anchor_from_live_state(state).await;
    let user_id = recovery_user_id(state);
    let (previous_heavy, prev_state) = load_previous_recovery_state(state, &user_id, &sid).await?;
    let session_state = super::super::session_projection::build_full_session_state_compact(
        state,
        super::super::session_projection::CslCheckpointFields,
        &prev_state,
    );
    let messages = super::super::session_projection::history_as_messages(&state.history);
    let workspace_path = workspace_path_for(&sid);
    let workspace_backup = read_optional_file_bytes(&workspace_path)?;

    let checkpoint_rollback = persist_recovery_checkpoint(
        state,
        &user_id,
        &sid,
        &session_state,
        previous_heavy.as_ref(),
    )?;
    if let Err(error) = persist_recovery_workspace_snapshot(state, &sid) {
        let mut error_message = error;
        append_rollback_error(
            &mut error_message,
            "workspace",
            restore_optional_file_bytes(&workspace_path, workspace_backup.clone()),
        );
        append_rollback_error(
            &mut error_message,
            "recovery checkpoint",
            rollback_recovery_checkpoint(&user_id, &sid, &checkpoint_rollback),
        );
        return Err(error_message);
    }
    if let Err(error) = rebuild_csl_from_history(state, &sid, &messages, &session_state).await {
        let mut error_message = error;
        append_rollback_error(
            &mut error_message,
            "workspace",
            restore_optional_file_bytes(&workspace_path, workspace_backup),
        );
        append_rollback_error(
            &mut error_message,
            "recovery checkpoint",
            rollback_recovery_checkpoint(&user_id, &sid, &checkpoint_rollback),
        );
        return Err(error_message);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_manual_heavy_step_checkpoint;
    use crate::cli::session::session_state::SessionState;

    #[test]
    fn manual_heavy_checkpoint_preserves_assistant_only_history_entries() {
        let state = SessionState {
            history: vec![
                ("".into(), "Earlier context compacted.".into()),
                ("continue".into(), "Done.".into()),
            ],
            ..Default::default()
        };

        let checkpoint = build_manual_heavy_step_checkpoint(
            &state,
            "sess-checkpoint-history",
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
            None,
        );
        let astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) = checkpoint else {
            panic!("expected heavy checkpoint");
        };

        assert_eq!(heavy.messages.len(), 3);
        assert_eq!(heavy.messages[0]["role"], "assistant");
        assert_eq!(heavy.messages[0]["content"], "Earlier context compacted.");
        assert_eq!(heavy.messages[1]["role"], "user");
        assert_eq!(heavy.messages[1]["content"], "continue");
        assert_eq!(heavy.messages[2]["role"], "assistant");
        assert_eq!(heavy.messages[2]["content"], "Done.");
    }

    #[test]
    fn manual_heavy_checkpoint_budget_remaining_uses_latest_context_trace() {
        let mut state = SessionState {
            total_prompt_tokens: 41_000,
            ..Default::default()
        };
        let mut trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace::default();
        trace.token_budget.total_used = 20_687;
        trace.token_budget.max_tokens = 800_000;
        state.latest_context_assembly_trace = Some(trace);

        let checkpoint = build_manual_heavy_step_checkpoint(
            &state,
            "sess-checkpoint-budget",
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
            None,
        );
        let astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) = checkpoint else {
            panic!("expected heavy checkpoint");
        };

        assert_eq!(heavy.budget_remaining_tokens, 779_313);
    }
}
