//! Runtime checkpoint state carried between turns.

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
    }
}

pub(crate) fn update_from_turn_failure(state: &mut SessionState, failure: &crate::TurnFailure) {
    // A failed turn without a new partial checkpoint did not advance the
    // durable recovery point; keep the previous runtime state intact.
    if let Some(checkpoint) = failure.partial.last_heavy_checkpoint.as_ref() {
        apply_heavy_checkpoint_runtime_state(state, checkpoint);
    }
}
