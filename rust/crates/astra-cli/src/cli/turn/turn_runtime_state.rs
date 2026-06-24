//! Runtime checkpoint state carried between turns.

use crate::cli::cli_config::cli_utils::cli_user_id;
use crate::cli::session::session_state::SessionState;

fn apply_heavy_checkpoint_runtime_state(
    state: &mut SessionState,
    checkpoint: &astra_pipeline::step_protocol::StepCheckpoint,
) -> bool {
    let astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) = checkpoint else {
        return false;
    };

    state.runtime_pipeline_state = heavy.pipeline_state.clone();
    state.runtime_compaction_state = heavy.compaction_state.clone();
    state.runtime_consecutive_context_window_errors = heavy.consecutive_context_window_errors;
    true
}

fn clear_runtime_checkpoint_state(state: &mut SessionState) {
    state.runtime_pipeline_state = None;
    state.runtime_compaction_state = None;
    state.runtime_consecutive_context_window_errors = 0;
    state.runtime_idempotency_cache = None;
}

pub(crate) fn update_from_stream_result(state: &mut SessionState, result: &crate::StreamResult) {
    let Some(checkpoint) = result.last_heavy_checkpoint.as_ref() else {
        // A successful turn without a checkpoint is an authoritative completed
        // turn, so stale recovered runtime state must not bleed forward.
        clear_runtime_checkpoint_state(state);
        return;
    };

    if !apply_heavy_checkpoint_runtime_state(state, checkpoint) {
        clear_runtime_checkpoint_state(state);
    } else {
        state.runtime_idempotency_cache = None;
    }
}

pub(crate) fn update_from_turn_failure(state: &mut SessionState, failure: &crate::TurnFailure) {
    // A failed turn without a new partial checkpoint did not advance the
    // durable recovery point; keep the previous runtime state intact.
    if let Some(checkpoint) = failure.partial.last_heavy_checkpoint.as_ref() {
        apply_heavy_checkpoint_runtime_state(state, checkpoint);
        if let Some(session_id) = failure.partial.session_id.as_deref() {
            let user_id = state
                .ingestion_user_id
                .as_deref()
                .filter(|user_id| !user_id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(cli_user_id);
            state.runtime_idempotency_cache =
                astra_pipeline::step_restore::restore_session(&user_id, session_id)
                    .ok()
                    .flatten()
                    .map(|restored| restored.idempotency_cache);
        } else {
            state.runtime_idempotency_cache = None;
        }
    }
}
