//! SSE Streaming Types
//!
//! Data structures for streaming chat responses and handling turn failures.
//! These types bridge the agentic runtime with the CLI display logic.

/// Re-export of the verdict audit event type for convenience.
pub(crate) type VerdictEvent = astra_runtime::turn::agentic_verdict_audit::AgenticVerdictAuditEvent;

/// Partial data rescued from `AgenticLoopState` when a turn fails.
/// Enables enriched error logging, failure learning, and post-mortem analysis.
#[derive(Debug, Default)]
pub(crate) struct PartialTurnData {
    pub tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
    pub tools_used: Vec<String>,
    pub stall_events: Vec<(String, u32)>,
    pub verdict_events: Vec<VerdictEvent>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_count: u32,
    #[allow(dead_code)]
    pub tool_health_export: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    pub session_id: Option<String>,
    pub last_heavy_checkpoint: Option<astra_runtime::pipeline::step_protocol::StepCheckpoint>,
    /// Partial text the model generated before the turn was interrupted.
    /// Preserved in conversation history so the next turn has context.
    pub partial_text: String,
}

/// A turn failure that carries partial data for post-mortem analysis.
#[derive(Debug)]
pub(crate) struct TurnFailure {
    pub error: String,
    pub partial: PartialTurnData,
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

pub(crate) fn apply_partial_turn_data_to_error_event(
    event: &mut astra_services::session_journal::JournalEvent,
    partial: &PartialTurnData,
) {
    if !partial.tool_call_records.is_empty() {
        event.tool_calls = Some(partial.tool_call_records.clone());
    }
    if partial.prompt_tokens > 0 {
        event.tokens_in = Some(partial.prompt_tokens);
    }
    if partial.completion_tokens > 0 {
        event.tokens_out = Some(partial.completion_tokens);
    }
    if partial.tool_calls_count > 0 {
        event.tool_count = Some(partial.tool_calls_count);
    }
    if !partial.tools_used.is_empty() {
        event.tools_used = Some(partial.tools_used.clone());
    }
}

/// Result of a streaming chat turn, including token counts and tool usage data.
#[derive(Debug)]
pub(crate) struct StreamResult {
    pub(crate) session_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) full_text: String,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) tool_calls_count: u32,
    /// Tool names selected for LLM (first turn selection report).
    pub(crate) tools_selected: Vec<String>,
    /// Skill names selected by the LLM during tool selection.
    pub(crate) selected_skills: Vec<String>,
    /// Tool names with material execution across all turns.
    pub(crate) tools_used: Vec<String>,
    /// Per-tool-call audit records: name, ok, ms, error.
    pub(crate) tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
    /// Token budget used by selected dynamic tools.
    pub(crate) budget_used: u32,
    /// Token budget pressure (0.0-0.9) from compaction tier.
    pub(crate) budget_pressure: f64,
    /// Stall events that occurred during the agentic loop (stall_type, turn_number).
    pub(crate) stall_events: Vec<(String, u32)>,
    /// TurnGuard verdict events (severity, turn, injections, avoid_tools, force_stop,
    /// nudge_count, total_errors, deprioritized_count). Only non-Healthy verdicts.
    pub(crate) verdict_events: Vec<VerdictEvent>,
    /// Step Protocol recorder summary for debugging and audit.
    pub(crate) step_recorder_summary:
        Option<astra_runtime::pipeline::step_recorder::RecorderSummary>,
    /// Exported tool health entries from this turn's TurnGuard (for cross-session persistence).
    pub(crate) tool_health_export: Vec<astra_runtime::pipeline::persistence::ToolHealthEntry>,
    /// Last heavy checkpoint built during the agentic loop (for cloud persistence).
    pub(crate) last_heavy_checkpoint:
        Option<astra_runtime::pipeline::step_protocol::StepCheckpoint>,
    /// Time to first token in milliseconds.
    pub(crate) ttft_ms: Option<u64>,
    /// Context assembly time in milliseconds.
    pub(crate) context_ms: Option<u64>,
    /// Tool selection strategy used.
    pub(crate) selector_strategy: Option<String>,
    /// Tool selection time in milliseconds (subset of context_ms).
    pub(crate) selector_ms: Option<u64>,
    /// LLM tokens consumed by tool selector (0 if TF-IDF only).
    pub(crate) selector_tokens_in: u64,
    pub(crate) selector_tokens_out: u64,
    /// Memoria search time in milliseconds (subset of context_ms).
    pub(crate) memoria_ms: Option<u64>,
    /// First tool-selection confidence (0.0–1.0) from the agentic loop prep pass.
    pub(crate) selector_confidence: Option<f64>,
    /// Routing domain label for this user line (filled in REPL when writing the journal row).
    pub(crate) routing_domain_hint: Option<String>,
    /// Entity graph skipped learning: success with tools but no routing domain.
    pub(crate) entity_learn_skipped_no_domain: bool,
    /// Deferred context assembly trace: journal event is only written on turn commit.
    pub(crate) pending_context_assembly_trace: Option<(u32, serde_json::Value)>,
}

impl StreamResult {
    /// Filled by the REPL after the agentic loop returns (routing + entity-learn eligibility).
    pub(crate) fn set_repl_learning_journal_fields(
        &mut self,
        routing_domain_hint: Option<String>,
        entity_learn_skipped_no_domain: bool,
    ) {
        self.routing_domain_hint = routing_domain_hint;
        self.entity_learn_skipped_no_domain = entity_learn_skipped_no_domain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{JournalEvent, ToolCallRecord};

    fn tool_record(name: &str, result_preview: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            name: name.into(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: result_preview.map(str::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn apply_partial_turn_data_to_error_event_copies_filtered_metrics() {
        let partial = PartialTurnData {
            tool_call_records: vec![
                tool_record(
                    "bash",
                    Some("Skipped: the skill already completed this work."),
                ),
                tool_record("read_file", Some("contents")),
            ],
            tools_used: vec!["read_file".into()],
            prompt_tokens: 42,
            completion_tokens: 21,
            tool_calls_count: 1,
            ..Default::default()
        };
        let mut event = JournalEvent::turn_error(Some("s1"), 1, None, "hi", "boom", 5);

        apply_partial_turn_data_to_error_event(&mut event, &partial);

        assert_eq!(event.tool_count, Some(1));
        assert_eq!(
            event.tools_used.as_deref(),
            Some(&["read_file".to_string()][..])
        );
        assert_eq!(event.tokens_in, Some(42));
        assert_eq!(event.tokens_out, Some(21));
        assert_eq!(event.tool_calls.as_ref().map(Vec::len), Some(2));
    }
}
