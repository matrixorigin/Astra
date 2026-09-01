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
    if let Some(quarantine) = heavy.workspace_observation_quarantine.as_ref() {
        state.workspace_observation_quarantine = Some(quarantine.clone());
    }
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
    // Provider and tool work remains real accounting when the logical turn
    // fails. Success and failure must update the same cumulative buckets once;
    // otherwise the live summary says `0 tokens` while the TurnError journal
    // correctly retains non-zero usage.
    state.total_prompt_tokens = state
        .total_prompt_tokens
        .saturating_add(failure.partial.prompt_tokens);
    state.total_completion_tokens = state
        .total_completion_tokens
        .saturating_add(failure.partial.completion_tokens);
    state.total_cache_read_tokens = state
        .total_cache_read_tokens
        .saturating_add(failure.partial.cache_read_tokens);
    state.total_cache_creation_tokens = state
        .total_cache_creation_tokens
        .saturating_add(failure.partial.cache_creation_tokens);
    state.total_session_cost += crate::cli::slash::slash_stats::cost_for_tokens(
        failure.partial.prompt_tokens,
        failure.partial.completion_tokens,
        failure.partial.cache_read_tokens,
        failure.partial.cache_creation_tokens,
        &state.cached_pricing,
    );

    // A failed turn without a new partial checkpoint did not advance the
    // durable recovery point; keep the previous runtime state intact.
    if let Some(checkpoint) = failure.partial.last_heavy_checkpoint.as_ref() {
        apply_heavy_checkpoint_runtime_state(state, checkpoint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_turn_usage_updates_the_same_session_buckets_as_success() {
        let mut state = SessionState::default();
        state.total_prompt_tokens = 10;
        state.total_completion_tokens = 20;
        state.total_cache_read_tokens = 30;
        state.total_cache_creation_tokens = 40;
        let failure = crate::TurnFailure {
            error: "approval callback failed".into(),
            partial: crate::PartialTurnData {
                prompt_tokens: 7_603,
                completion_tokens: 315,
                cache_read_tokens: 34_688,
                cache_creation_tokens: 17,
                tool_calls_count: 3,
                ..Default::default()
            },
        };

        update_from_turn_failure(&mut state, &failure);

        assert_eq!(state.total_prompt_tokens, 7_613);
        assert_eq!(state.total_completion_tokens, 335);
        assert_eq!(state.total_cache_read_tokens, 34_718);
        assert_eq!(state.total_cache_creation_tokens, 57);
    }
}
