//! Shared prompt-history and session continuity checkpoint construction.

use std::collections::HashSet;

use astra_pipeline::step_protocol::{HeavyCheckpoint, WorkspaceObservationQuarantineV1};
use astra_pipeline::step_recorder::StepRecorder;
use serde_json::Value;

/// Borrowed continuity inputs, independent of execution-loop machinery.
pub struct TurnCheckpointInput<'a> {
    pub step_recorder: &'a StepRecorder,
    pub messages: &'a [Value],
    pub max_turn_input_tokens: u64,
    pub last_measured_prompt_tokens: Option<u64>,
    pub remaining_turns: u32,
    pub restricted_tools: &'a HashSet<String>,
    pub recent_tools: &'a [String],
    pub interruption: Option<&'a crate::interruption::InterruptionRecord>,
    pub approval_overrides: Option<&'a crate::approval_fingerprint::FingerprintedOverrides>,
    pub consecutive_context_window_errors: u32,
    pub deferred_tool_activations: &'a mut Vec<astra_turn_types::DeferredToolActivation>,
    pub pipeline_session: Option<&'a crate::pipeline_session::PipelineSession>,
    pub workspace_observation_quarantine: Option<&'a WorkspaceObservationQuarantineV1>,
}

fn checkpoint_blocked_tools(restricted_tools: &HashSet<String>) -> Vec<String> {
    let mut blocked_tools: Vec<String> = restricted_tools.iter().cloned().collect();
    blocked_tools.sort();
    blocked_tools.dedup();
    blocked_tools
}

/// Build a continuity snapshot; publication and execution authority remain with callers.
pub fn build_turn_continuity_checkpoint(input: TurnCheckpointInput<'_>) -> Option<HeavyCheckpoint> {
    // Serialize the interruption record (if any) for checkpoint persistence.
    let interruption_json = input.interruption.map(|ir| ir.to_json());

    // Serialize approval overrides (if any) for session continuity.
    let approval_overrides_json = input.approval_overrides.and_then(|ao| ao.to_json());

    let checkpoint_blocked_tools = checkpoint_blocked_tools(input.restricted_tools);
    let checkpoint_messages =
        crate::runtime_scaffolding::sanitize_recoverable_runtime_messages(input.messages.to_vec());
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::FinalizationCheckpointClone,
        &checkpoint_messages,
    );
    let context_input_headroom_tokens = match (
        input.max_turn_input_tokens,
        input.last_measured_prompt_tokens,
    ) {
        (limit, Some(measured)) if limit > 0 => limit.saturating_sub(measured),
        // Do not turn a missing measurement into an apparently full budget.
        // Zero is the legacy checkpoint sentinel for an unavailable diagnostic.
        _ => 0,
    };
    let mut heavy = input
        .step_recorder
        .build_heavy_checkpoint_with_interruption(
            &checkpoint_messages,
            context_input_headroom_tokens,
            input.remaining_turns,
            &checkpoint_blocked_tools,
            input.recent_tools,
            interruption_json,
            approval_overrides_json,
            input.consecutive_context_window_errors,
        )?;
    // Carrier authority is deliberately reconstructed only from paired
    // tool_search evidence with a schema digest. Name-only selection state is
    // neither prompt continuity nor execution authority in this protocol.
    *input.deferred_tool_activations =
        crate::tool::deferred_activation::merged_deferred_tool_activations(
            &checkpoint_messages,
            std::mem::take(input.deferred_tool_activations),
        );
    heavy.deferred_tool_activations = input.deferred_tool_activations.clone();
    // Persist context pipeline state for warm-start on resume.
    if let Some(sess) = input.pipeline_session {
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
    // local record window or from prose-only conversation input.
    heavy.workspace_observation_quarantine = input.workspace_observation_quarantine.cloned();
    Some(heavy)
}
