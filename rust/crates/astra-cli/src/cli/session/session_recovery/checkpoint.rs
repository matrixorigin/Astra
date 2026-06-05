//! Checkpoint operations: build, persist, rollback, and delegation helpers.
use super::csl::*;
use super::io::*;
use super::workspace::*;
use crate::cli::session_projection::history_as_messages;
use crate::cli::*;

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
        blocked_tools: heavy.blocked_tools.clone(),
        recent_tools: heavy.recent_tools.clone(),
        approval_overrides: heavy.approval_overrides.clone(),
        compaction_tracker: heavy.compaction_state.clone(),
        budget_remaining_tokens: heavy.budget_remaining_tokens,
        budget_remaining_rounds: heavy.budget_remaining_rounds,
        consecutive_ctx_errors: heavy.consecutive_context_window_errors,
        delegation: delegation_from_heavy_checkpoint(
            heavy,
            "session_state_compact_from_heavy_checkpoint",
        ),
        interruption: heavy.interruption.clone(),
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
    sid: &str,
) -> Result<
    (
        Option<astra_pipeline::step_protocol::HeavyCheckpoint>,
        astra_turn_core::conversation_log::SessionStateCompact,
    ),
    String,
> {
    let previous_heavy = astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(sid)
        .map_err(|e| format!("read latest heavy checkpoint: {e}"))?;
    let csl_state = ensure_loaded_csl_state(state, sid).await?;
    let prev_state = previous_session_state_for_history_sync(previous_heavy.as_ref(), csl_state);
    Ok((previous_heavy, prev_state))
}

/// Copy plan / durable-task fields from REPL into workspace before checkpointing.
pub(crate) fn next_step_checkpoint_number(sid: &str) -> Result<u32, String> {
    let existing = astra_pipeline::step_checkpoint::list_checkpoints(sid)
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
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
) -> astra_pipeline::step_protocol::StepCheckpoint {
    use astra_pipeline::step_protocol::{
        ExecutionCursor, HeavyCheckpoint, LightCheckpoint, PROTOCOL_VERSION, StepCheckpoint,
        epoch_ms,
    };

    let messages = history_as_messages(&state.history);

    let max_turns = state.cli_context.max_turns.unwrap_or(50u32);
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
    let heavy = HeavyCheckpoint {
        light,
        messages,
        budget_remaining_tokens: {
            if previous_heavy.is_some() || session_state.budget_remaining_tokens > 0 {
                session_state.budget_remaining_tokens
            } else {
                let limit = astra_core::RuntimeLimits::global().max_turn_input_tokens;
                if limit == 0 {
                    0
                } else {
                    limit.saturating_sub(state.total_prompt_tokens)
                }
            }
        },
        budget_remaining_rounds: if previous_heavy.is_some()
            || session_state.budget_remaining_rounds > 0
        {
            session_state.budget_remaining_rounds
        } else {
            max_turns.saturating_sub(state.turn)
        },
        blocked_tools: session_state.blocked_tools.clone(),
        recent_tools: state.recent_tools.clone(),
        memory_context: previous_heavy.and_then(|heavy| heavy.memory_context.clone()),
        delegation_id: session_state
            .delegation
            .as_ref()
            .map(|delegation| delegation.id.clone()),
        delegation_pattern: session_state
            .delegation
            .as_ref()
            .map(|delegation| delegation.pattern.clone()),
        delegation_sub_run_summaries: session_state
            .delegation
            .as_ref()
            .map(|delegation| delegation.completed_sub_runs.clone())
            .unwrap_or_default(),
        interruption: session_state.interruption.clone(),
        approval_overrides: session_state.approval_overrides.clone(),
        consecutive_context_window_errors: session_state.consecutive_ctx_errors,
        compaction_state: session_state.compaction_tracker.clone(),
        pipeline_state: previous_heavy.and_then(|heavy| heavy.pipeline_state.clone()),
        config_version_id: state.config_version_id.clone(),
    };
    StepCheckpoint::Heavy(Box::new(heavy))
}

/// Persist heavy JSON + composite snapshot index before any workspace/journal mutation.
pub(crate) fn persist_manual_heavy_and_composite(
    sid: &str,
    turn: u32,
    title: &str,
    next_step: u32,
    step_cp: &astra_pipeline::step_protocol::StepCheckpoint,
) -> Result<std::path::PathBuf, String> {
    use astra_pipeline::step_checkpoint::{
        read_composite_snapshot_index, write_composite_snapshot_index, write_step_checkpoint,
    };

    let heavy_path = write_step_checkpoint(sid, next_step, step_cp)
        .map_err(|e| format!("write heavy step checkpoint: {e}"))?;
    let composite_index_path = composite_index_path_for(sid);
    let composite_index_backup = read_optional_file_bytes(&composite_index_path)
        .map_err(|e| format!("backup composite snapshot index: {e}"))?;

    let mut snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(sid.to_string(), turn)
            .label(format!("manual:{title}"))
            .session_state(format!("{next_step:06}-heavy.json"))
            .workspace_state(sid.to_string())
            .build();
    let mut index = read_composite_snapshot_index(sid)
        .map_err(|e| format!("read composite snapshot index: {e}"))?;
    index
        .append(&mut snapshot)
        .map_err(|e| format!("append snapshot version: {e}"))?;
    if let Err(e) = write_composite_snapshot_index(sid, &index) {
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
    sid: &str,
    session_state: &astra_turn_core::conversation_log::SessionStateCompact,
    previous_heavy: Option<&astra_pipeline::step_protocol::HeavyCheckpoint>,
) -> Result<RecoveryCheckpointRollback, String> {
    let next_step = next_step_checkpoint_number(sid)?;
    let composite_index_backup = read_optional_file_bytes(&composite_index_path_for(sid))?;
    let step_cp = build_manual_heavy_step_checkpoint(state, sid, session_state, previous_heavy);
    persist_manual_heavy_and_composite(sid, state.turn, "history-sync", next_step, &step_cp)?;
    Ok(RecoveryCheckpointRollback {
        step_number: next_step,
        composite_index_backup,
    })
}

pub(crate) fn rollback_recovery_checkpoint(
    sid: &str,
    rollback: &RecoveryCheckpointRollback,
) -> Result<(), String> {
    let mut rollback_error = String::new();
    if let Err(error) = restore_optional_file_bytes(
        &composite_index_path_for(sid),
        rollback.composite_index_backup.clone(),
    ) {
        rollback_error = format!("restore composite snapshot index: {error}");
    }
    let delete_result =
        astra_pipeline::step_checkpoint::delete_step_checkpoint(sid, rollback.step_number, "heavy")
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
    let (previous_heavy, prev_state) = load_previous_recovery_state(state, &sid).await?;
    let session_state = super::super::session_projection::build_full_session_state_compact(
        state,
        super::super::session_projection::CslCheckpointFields::default(),
        &prev_state,
    );
    let messages = super::super::session_projection::history_as_messages(&state.history);
    let workspace_path = workspace_path_for(&sid);
    let workspace_backup = read_optional_file_bytes(&workspace_path)?;

    let checkpoint_rollback =
        persist_recovery_checkpoint(state, &sid, &session_state, previous_heavy.as_ref())?;
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
            rollback_recovery_checkpoint(&sid, &checkpoint_rollback),
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
            rollback_recovery_checkpoint(&sid, &checkpoint_rollback),
        );
        return Err(error_message);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
